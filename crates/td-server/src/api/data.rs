//! Instruments, history downloads and store coverage.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use td_core::{Instrument, InstrumentId, InstrumentKind, Millis, Venue};
use td_data::time::{Month, now_ms};
use td_data::{Gap, Store};
use ts_rs::TS;

use super::AppState;
use super::error::ApiError;
use super::jobs::{JobCreated, JobCtx, JobError, JobRequest, JobResult};
use super::source::MarketSource;

#[derive(Debug, Clone, Deserialize)]
pub(super) struct InstrumentsQuery {
    venue: Venue,
    kind: InstrumentKind,
}

/// One instrument the venue lists but that failed validation, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct RejectedInstrument {
    pub symbol: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct InstrumentsResponse {
    pub instruments: Vec<Instrument>,
    pub rejected: Vec<RejectedInstrument>,
}

/// `GET /instruments?venue=binance&kind=spot`. Each list is fetched once per server run and then
/// served from memory.
pub(super) async fn instruments(
    State(state): State<Arc<AppState>>,
    query: Result<Query<InstrumentsQuery>, QueryRejection>,
) -> Result<Json<InstrumentsResponse>, ApiError> {
    let Query(InstrumentsQuery { venue, kind }) = query?;
    Ok(Json(instrument_list(&state, venue, kind).await?))
}

/// A venue's instruments of one kind, from the cache or else from the source.
pub(super) async fn instrument_list(
    state: &AppState,
    venue: Venue,
    kind: InstrumentKind,
) -> Result<InstrumentsResponse, ApiError> {
    if let Some(cached) = state.instrument_cache().get(&(venue, kind)) {
        return Ok(cached.clone());
    }
    // shortcut: two requests that miss the cache at once both fetch; harmless (the second insert
    // replaces the first with the same list), dedupe with a per-key once-cell if it ever matters.
    let source = state.source.clone();
    let list = tokio::task::spawn_blocking(move || source.instruments(venue, kind))
        .await
        .map_err(|e| ApiError::internal(format!("the instrument fetch stopped: {e}")))??;
    let response = InstrumentsResponse {
        instruments: list.instruments,
        rejected: list
            .rejected
            .into_iter()
            .map(|(symbol, reason)| RejectedInstrument { symbol, reason })
            .collect(),
    };
    state
        .instrument_cache()
        .insert((venue, kind), response.clone());
    Ok(response)
}

/// The earliest `from` a download accepts: 2017-01-01 00:00 UTC, before the earliest data either
/// venue has. An earlier `from` would only ask the archive for years of files that do not exist.
pub const EARLIEST_FROM: Millis = 1_483_228_800_000;

/// What to download: the 1-minute candles of `instrument` with open times in `[from, to)`, and
/// for a perpetual its funding rates in the same range. Anything after now is skipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct DownloadRequest {
    pub instrument: InstrumentId,
    /// Not before 2017-01-01 00:00 UTC (1483228800000); earlier is a 400.
    pub from: Millis,
    pub to: Millis,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct MonthCount {
    /// `YYYY-MM`.
    pub month: String,
    pub candles: usize,
}

/// A run of missing minutes: open times from `from` up to but not including `to`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct GapInfo {
    pub from: Millis,
    pub to: Millis,
    pub minutes: i64,
}

impl From<&Gap> for GapInfo {
    fn from(g: &Gap) -> Self {
        GapInfo {
            from: g.from,
            to: g.to,
            minutes: g.minutes(),
        }
    }
}

/// What a download did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct DownloadResult {
    /// Candles fetched in total.
    pub candles: usize,
    pub months: Vec<MonthCount>,
    /// Archive files the venue does not have (before the symbol listed, or not yet published).
    pub missing_files: Vec<String>,
    /// Minutes in the range, up to now, still not stored after the download.
    pub gaps: Vec<GapInfo>,
    /// Funding rates fetched; `null` for spot.
    pub funding_rates: Option<usize>,
}

/// `POST /data/download`: checks the request and starts a download job.
pub(super) async fn download(
    State(state): State<Arc<AppState>>,
    body: Result<Json<DownloadRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<JobCreated>), ApiError> {
    let Json(request) = body?;
    if request.from >= request.to {
        return Err(ApiError::bad_request("`to` must be after `from`"));
    }
    if request.from < EARLIEST_FROM {
        return Err(ApiError::bad_request(
            "`from` must not be before 2017-01-01 00:00 UTC (1483228800000), before either \
             venue's earliest data",
        ));
    }
    // Rejects a symbol that cannot name a store folder before any work starts.
    state
        .store
        .candle_path(&request.instrument, Month::containing(request.from))?;
    let (source, store) = (state.source.clone(), state.store.clone());
    let job_request = JobRequest::Download(request.clone());
    let job = state.jobs.submit(job_request, move |ctx| {
        run_download(source.as_ref(), &store, &request, ctx).map(JobResult::Download)
    })?;
    Ok((StatusCode::ACCEPTED, Json(JobCreated { job_id: job.id })))
}

/// The download job: one calendar month at a time, checking for a cancel between months, then
/// the funding rates of a perpetual. The range ends at now at the latest.
pub fn run_download(
    source: &dyn MarketSource,
    store: &Store,
    request: &DownloadRequest,
    ctx: &JobCtx,
) -> Result<DownloadResult, JobError> {
    let id = &request.instrument;
    let (from, to) = (request.from, request.to.min(now_ms()));
    let months = Month::range(from, to);
    let perpetual = id.kind == InstrumentKind::Perpetual;
    let steps = (months.len() + usize::from(perpetual)).max(1) as f64;
    let mut result = DownloadResult {
        candles: 0,
        months: Vec::new(),
        missing_files: Vec::new(),
        gaps: Vec::new(),
        funding_rates: None,
    };
    for (i, month) in months.iter().enumerate() {
        ctx.checkpoint()?;
        let report = source.candles(store, id, from.max(month.start()), to.min(month.end()))?;
        let count: usize = report.months.iter().map(|(_, n)| n).sum();
        result.candles += count;
        result.months.push(MonthCount {
            month: month.to_string(),
            candles: count,
        });
        result.missing_files.extend(report.missing_files);
        result.gaps.extend(report.gaps.iter().map(GapInfo::from));
        ctx.progress(
            (i + 1) as f64 / steps,
            format!("{id} {month}: {count} candles"),
        );
    }
    if perpetual {
        ctx.checkpoint()?;
        let rates = if from < to {
            source.funding(store, id, from, to)?
        } else {
            0
        };
        result.funding_rates = Some(rates);
        ctx.progress(1.0, format!("{id}: {rates} funding rates"));
    }
    Ok(result)
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct CoverageQuery {
    /// `venue:symbol:kind`, for example `binance:BTCUSDT:spot`.
    instrument: String,
}

/// Which minutes the store holds for one month.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct MonthCoverageInfo {
    /// `YYYY-MM`.
    pub month: String,
    /// Stored 1-minute candles.
    pub minutes: usize,
    /// Minutes in the whole month.
    pub month_minutes: i64,
    /// Open time of the first and last stored candle.
    pub first: Millis,
    pub last: Millis,
    /// Missing minutes between `first` and `last`.
    pub gaps: Vec<GapInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct FundingCoverage {
    pub rates: usize,
    pub first: Option<Millis>,
    pub last: Option<Millis>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct CoverageResponse {
    pub instrument: InstrumentId,
    pub months: Vec<MonthCoverageInfo>,
    /// `null` for spot.
    pub funding: Option<FundingCoverage>,
}

/// `GET /data/coverage?instrument=binance:BTCUSDT:spot`.
pub(super) async fn coverage(
    State(state): State<Arc<AppState>>,
    query: Result<Query<CoverageQuery>, QueryRejection>,
) -> Result<Json<CoverageResponse>, ApiError> {
    let Query(CoverageQuery { instrument }) = query?;
    let id = parse_instrument(&instrument)?;
    let store = state.store.clone();
    let response = tokio::task::spawn_blocking(move || read_coverage(&store, id))
        .await
        .map_err(|e| ApiError::internal(format!("the coverage read stopped: {e}")))??;
    Ok(Json(response))
}

fn read_coverage(store: &Store, id: InstrumentId) -> Result<CoverageResponse, ApiError> {
    let months = store
        .coverage(&id)?
        .iter()
        .map(|m| MonthCoverageInfo {
            month: m.month.to_string(),
            minutes: m.minutes,
            month_minutes: m.month.minutes(),
            first: m.first,
            last: m.last,
            gaps: m.gaps.iter().map(GapInfo::from).collect(),
        })
        .collect();
    let funding = if id.kind == InstrumentKind::Perpetual {
        let rates = store.load_funding(&id, Millis::MIN, Millis::MAX)?;
        Some(FundingCoverage {
            rates: rates.len(),
            first: rates.first().map(|r| r.0),
            last: rates.last().map(|r| r.0),
        })
    } else {
        None
    };
    Ok(CoverageResponse {
        instrument: id,
        months,
        funding,
    })
}

/// Reads `venue:symbol:kind`, the form `InstrumentId` displays as.
pub fn parse_instrument(text: &str) -> Result<InstrumentId, ApiError> {
    let bad = || {
        ApiError::bad_request(format!(
            "{text:?} is not an instrument: use venue:symbol:kind, for example binance:BTCUSDT:spot"
        ))
    };
    let mut parts = text.split(':');
    let (Some(venue), Some(symbol), Some(kind), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad());
    };
    let named = |s: &str| serde_json::Value::String(s.to_string());
    let venue: Venue = serde_json::from_value(named(venue)).map_err(|_| bad())?;
    let kind: InstrumentKind = serde_json::from_value(named(kind)).map_err(|_| bad())?;
    if symbol.is_empty() {
        return Err(bad());
    }
    Ok(InstrumentId {
        venue,
        symbol: symbol.to_string(),
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruments_parse_from_their_display_form() {
        let id = parse_instrument("bybit:ETHUSDT:perpetual").expect("parses");
        assert_eq!(id.to_string(), "bybit:ETHUSDT:perpetual");
        for bad in [
            "",
            "binance:BTCUSDT",
            "binance::spot",
            "kraken:BTCUSDT:spot",
            "binance:BTCUSDT:future",
            "binance:BTC:USDT:spot",
        ] {
            assert!(parse_instrument(bad).is_err(), "{bad}");
        }
    }
}
