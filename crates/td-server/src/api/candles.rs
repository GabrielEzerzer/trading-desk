//! Candle history for the charts: `GET /candles`, the closed bars a chart draws before the live
//! `candles:` topic continues it.
//!
//! Bars come from the local store first, resampled from its 1-minute candles by the store's own
//! rule ([`td_data::resample`]). The bar slots the store does not cover are fetched from the
//! venue's REST API at the timeframe itself, the way a paper session warms up, and are not written
//! to the store: only download jobs fill it. A venue with no live stream (oanda) has no such REST
//! bars, so there those slots are reported missing.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use td_core::{Candle, InstrumentId, Millis, Timeframe};
use td_data::live::Rest;
use td_data::time::{DAY, Month};
use td_data::{DataError, Store};
use ts_rs::TS;

use super::AppState;
use super::data::{earliest_from, parse_instrument};
use super::error::ApiError;

/// The most bars one response holds, and the default `limit`.
pub const MAX_CANDLES: usize = 5_000;
/// How much time one read of the store covers for weekly bars: four weeks, about a month's minutes.
const WEEKLY_STORE_READ_MS: Millis = 28 * DAY;

#[derive(Debug, Clone, Deserialize)]
pub(super) struct CandlesQuery {
    /// `venue:symbol:kind`, for example `binance:BTCUSDT:spot`.
    instrument: String,
    timeframe: Timeframe,
    from: Option<Millis>,
    to: Option<Millis>,
    limit: Option<usize>,
}

/// A run of bar slots: open times from `from` up to but not including `to`, `bars` of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct BarSpan {
    pub from: Millis,
    pub to: Millis,
    pub bars: i64,
}

/// `GET /candles`: closed bars, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct CandlesResponse {
    pub instrument: InstrumentId,
    pub timeframe: Timeframe,
    /// The bar slots the response covers: open times in `[from, to)`, each on a bar boundary. `to`
    /// is the asked `to` rounded up to a boundary, but no later than the open time of the bar
    /// still forming; `from` is later than asked when the limit cut the range, and never before
    /// the venue's earliest download date (2017-01-01, or 2007-01-01 for oanda).
    pub from: Millis,
    pub to: Millis,
    /// `from` was given and the range held more than `limit` bar slots, so the oldest were left
    /// out.
    pub truncated: bool,
    /// Closed bars, oldest first.
    pub candles: Vec<Candle>,
    /// The bars fetched from the venue rather than read from the store.
    pub from_venue: Vec<BarSpan>,
    /// Slots in `[from, to)` with no bar in the store or at the venue.
    pub missing: Vec<BarSpan>,
}

/// `GET /candles?instrument=binance:BTCUSDT:spot&timeframe=1h&from=&to=&limit=`.
pub(super) async fn candles(
    State(state): State<Arc<AppState>>,
    query: Result<Query<CandlesQuery>, QueryRejection>,
) -> Result<Json<CandlesResponse>, ApiError> {
    let Query(query) = query?;
    let id = parse_instrument(&query.instrument)?;
    let (store, rest) = (state.store.clone(), state.streams.live().rest());
    let response = tokio::task::spawn_blocking(move || read_candles(&store, &*rest, id, &query))
        .await
        .map_err(|e| ApiError::internal(format!("the candle read stopped: {e}")))??;
    Ok(Json(response))
}

/// Where one slot's bar came from, if anywhere.
type Slot = Option<(Candle, bool)>;

/// Reads the bars from the store, fills what it lacks from the venue, and reports both. Blocks.
fn read_candles(
    store: &Store,
    rest: &dyn Rest,
    id: InstrumentId,
    query: &CandlesQuery,
) -> Result<CandlesResponse, ApiError> {
    let timeframe = query.timeframe;
    let limit = match query.limit {
        Some(0) => return Err(ApiError::bad_request("`limit` must be at least 1")),
        Some(n) => n.min(MAX_CANDLES),
        None => MAX_CANDLES,
    };
    let now = rest.now();
    let to = query.to.unwrap_or(now);
    if query.from.is_some_and(|from| from >= to) {
        return Err(ApiError::bad_request("`to` must be after `from`"));
    }
    // Refuses a symbol that cannot name a store folder before the venue is asked about it.
    store.candle_path(&id, Month::containing(now))?;

    let dur = timeframe.duration_ms();
    let ceil = |t: Millis| {
        let floor = timeframe.floor(t);
        if floor == t { t } else { floor + dur }
    };
    // Clamped first, so the arithmetic below cannot overflow. The bar open at `now` is not closed.
    let (earliest, _) = earliest_from(id.venue);
    let clamp = |t: Millis| t.clamp(earliest, now.max(earliest));
    let end = ceil(clamp(to)).min(timeframe.floor(now));
    let window = end - limit as i64 * dur;
    let asked = ceil(clamp(query.from.unwrap_or(earliest)));
    let from = asked.max(window).min(end);
    let truncated = query.from.is_some() && asked < window;

    let slot_of = |t: Millis| {
        ((from..end).contains(&t) && timeframe.floor(t) == t).then(|| ((t - from) / dur) as usize)
    };
    let mut slots: Vec<Slot> = vec![None; ((end - from) / dur) as usize];
    // The stored minutes are read about a month at a time, so a daily or weekly chart over years
    // never holds years of minutes at once, and every piece ends on a bar boundary. Up to 1d a
    // piece ends at the next month start, which is a whole day, so it reads one month file; weeks
    // do not divide months, so 1w reads four weeks at a time.
    let mut start = from;
    while start < end {
        let stop = if dur <= DAY {
            Month::containing(start).end()
        } else {
            start + WEEKLY_STORE_READ_MS
        }
        .min(end);
        for bar in store
            .load_candles(&id, timeframe, start, stop, false)?
            .candles
        {
            if let Some(i) = slot_of(bar.open_time) {
                slots[i] = Some((bar, false));
            }
        }
        start = stop;
    }
    // One request over every slot the store lacks: at most `limit` bars, a few pages at most.
    if let (Some(first), Some(last)) = (
        slots.iter().position(Option::is_none),
        slots.iter().rposition(Option::is_none),
    ) {
        let (span_from, span_to) = (from + first as i64 * dur, from + (last as i64 + 1) * dur);
        let fetched = match rest.klines(&id, timeframe, span_from, span_to) {
            // A venue with no live stream (oanda) has no REST bars either: its slots stay missing.
            Err(DataError::NoLiveStream(_)) => Vec::new(),
            fetched => fetched.map_err(|e| venue_error(&id, timeframe, e))?,
        };
        // A bar that fails validation is skipped, as in a session's warm-up.
        for bar in fetched.into_iter().filter(|b| b.validate().is_ok()) {
            if let Some(i) = slot_of(bar.open_time)
                && slots[i].is_none()
            {
                slots[i] = Some((bar, true));
            }
        }
    }

    let mut response = CandlesResponse {
        instrument: id,
        timeframe,
        from,
        to: end,
        truncated,
        candles: Vec::with_capacity(slots.len()),
        from_venue: Vec::new(),
        missing: Vec::new(),
    };
    for (i, slot) in slots.into_iter().enumerate() {
        let open_time = from + i as i64 * dur;
        match slot {
            Some((bar, venue)) => {
                response.candles.push(bar);
                if venue {
                    extend(&mut response.from_venue, open_time, dur);
                }
            }
            None => extend(&mut response.missing, open_time, dur),
        }
    }
    Ok(response)
}

/// The venue refusing the request (an HTTP 4xx, such as Binance's answer for a symbol it does not
/// list) is the client's mistake, a 400 with the venue's message. Any other failure is a 502: a
/// 5xx, the network, a timeout, a body that does not parse, and the venue's rate limit (429, and
/// 418 for Binance's ban after ignoring it), which says nothing about the request.
fn venue_error(id: &InstrumentId, timeframe: Timeframe, e: DataError) -> ApiError {
    let message = format!("fetching {id} {timeframe} bars from the venue failed: {e}");
    match e {
        DataError::Status { status, .. }
            if (400..500).contains(&status) && !matches!(status, 418 | 429) =>
        {
            ApiError::bad_request(message)
        }
        _ => ApiError::new(StatusCode::BAD_GATEWAY, "venue", message),
    }
}

/// Adds the slot opening at `open_time` to the last span if it follows on, else starts a new one.
fn extend(spans: &mut Vec<BarSpan>, open_time: Millis, dur: Millis) {
    match spans.last_mut() {
        Some(span) if span.to == open_time => {
            span.to += dur;
            span.bars += 1;
        }
        _ => spans.push(BarSpan {
            from: open_time,
            to: open_time + dur,
            bars: 1,
        }),
    }
}
