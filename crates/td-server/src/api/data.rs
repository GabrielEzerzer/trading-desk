//! Instruments, history downloads, store coverage, and listing and deleting stored data.

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

/// The earliest `from` a Binance or Bybit download accepts: 2017-01-01 00:00 UTC, before the
/// earliest data either venue has. An earlier `from` would only ask the archive for years of files
/// that do not exist.
pub const EARLIEST_FROM: Millis = 1_483_228_800_000;

/// The earliest `from` an OANDA download accepts: 2007-01-01 00:00 UTC. The FX market hours
/// ([`InstrumentKind::is_open`]) follow the US daylight-saving rule in force since 2007, so for
/// earlier years about four weeks a year would have the weekend an hour off: real candles thrown
/// away as closed, and gaps. OANDA's practice API has minutes from 2005; the desk's FX history
/// starts in 2016.
pub const OANDA_EARLIEST_FROM: Millis = 1_167_609_600_000;

/// The earliest time anything of `venue`'s is downloaded or charted from, and the message refusing
/// a download from earlier.
pub(super) fn earliest_from(venue: Venue) -> (Millis, &'static str) {
    match venue {
        Venue::Binance | Venue::Bybit => (
            EARLIEST_FROM,
            "`from` must not be before 2017-01-01 00:00 UTC (1483228800000), before either \
             venue's earliest data",
        ),
        Venue::Oanda => (
            OANDA_EARLIEST_FROM,
            "`from` must not be before 2007-01-01 00:00 UTC (1167609600000) for oanda: the FX \
             market hours follow the US daylight-saving rule in force since 2007",
        ),
    }
}

/// What to download: the 1-minute candles of `instrument` with open times in `[from, to)`, and
/// for a perpetual its funding rates in the same range. Anything after now is skipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct DownloadRequest {
    pub instrument: InstrumentId,
    /// Not before 2017-01-01 00:00 UTC (1483228800000) for binance and bybit, or before
    /// 2007-01-01 00:00 UTC (1167609600000) for oanda; earlier is a 400.
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
    let (earliest, refusal) = earliest_from(request.instrument.venue);
    if request.from < earliest {
        return Err(ApiError::bad_request(refusal));
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
    // Held for the whole download, so a delete of the instrument is refused until it ends; this
    // waits while a delete of it runs.
    let _hold = store.hold_for_download(id)?;
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
    /// The month's open minutes by the instrument's trading hours, which a complete month stores:
    /// every minute of the month for a market that never closes, the minutes outside its weekends
    /// for FX.
    pub month_minutes: i64,
    /// Open time of the first and last stored candle.
    pub first: Millis,
    pub last: Millis,
    /// Missing minutes between `first` and `last`.
    pub gaps: Vec<GapInfo>,
    /// The month file's size on disk.
    pub bytes: u64,
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

pub fn read_coverage(store: &Store, id: InstrumentId) -> Result<CoverageResponse, ApiError> {
    let months = store
        .coverage(&id)?
        .iter()
        .map(|m| MonthCoverageInfo {
            month: m.month.to_string(),
            minutes: m.minutes,
            month_minutes: m.open_minutes,
            first: m.first,
            last: m.last,
            gaps: m.gaps.iter().map(GapInfo::from).collect(),
            bytes: m.bytes,
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

/// One month file of an instrument's candles and its size on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct StoredMonthInfo {
    /// `YYYY-MM`.
    pub month: String,
    pub bytes: u64,
}

/// What the store holds for one instrument.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct StoredInstrumentInfo {
    pub instrument: InstrumentId,
    /// The month files, oldest first; empty when only a funding file is stored.
    pub months: Vec<StoredMonthInfo>,
    /// The funding file's size; `null` when there is none, which is always so for spot.
    pub funding_bytes: Option<u64>,
    /// The month files and the funding file together.
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct StoredResponse {
    /// The data folder, as an absolute path. It also holds saved runs and paper sessions, which
    /// are not market data.
    pub data_dir: String,
    /// Every instrument with at least one month file or a funding file, in instrument order.
    pub instruments: Vec<StoredInstrumentInfo>,
    /// Paths in the data folder, relative to it, that name no venue, kind, symbol or month the
    /// store writes. They are neither counted nor deleted.
    pub unrecognised: Vec<String>,
}

/// `GET /data/stored`: every instrument the store holds, with the size of each file.
pub(super) async fn stored(
    State(state): State<Arc<AppState>>,
) -> Result<Json<StoredResponse>, ApiError> {
    let store = state.store.clone();
    let stored = tokio::task::spawn_blocking(move || store.stored())
        .await
        .map_err(|e| ApiError::internal(format!("the listing stopped: {e}")))??;
    let data_dir = std::path::absolute(state.store.root())
        .unwrap_or_else(|_| state.store.root().to_path_buf())
        .to_string_lossy()
        .into_owned();
    Ok(Json(StoredResponse {
        data_dir,
        instruments: stored
            .instruments
            .iter()
            .map(|i| StoredInstrumentInfo {
                instrument: i.id.clone(),
                months: i
                    .months
                    .iter()
                    .map(|m| StoredMonthInfo {
                        month: m.month.to_string(),
                        bytes: m.bytes,
                    })
                    .collect(),
                funding_bytes: i.funding_bytes,
                bytes: i.bytes(),
            })
            .collect(),
        unrecognised: stored
            .unrecognised
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect(),
    }))
}

/// What to delete: the candles of `instrument` for the months `from` to `to`, both included, or,
/// with neither given, every month and a perpetual's funding file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct DeleteRequest {
    pub instrument: InstrumentId,
    /// `YYYY-MM`; give both `from` and `to`, or neither.
    #[serde(default)]
    #[ts(optional)]
    pub from: Option<String>,
    /// `YYYY-MM`, included.
    #[serde(default)]
    #[ts(optional)]
    pub to: Option<String>,
}

/// What a delete removed: data files, and temp files a crashed download left beside them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct DeleteResult {
    pub files: usize,
    pub bytes: u64,
}

/// `POST /data/delete`: deletes stored data now and answers what it removed. Nothing stored is
/// not an error. A 409 `conflict` while a download of the instrument is queued in this server, or
/// runs here or in another process on the same data folder, or another delete of it runs: the
/// delete would race the files it is writing.
pub(super) async fn delete(
    State(state): State<Arc<AppState>>,
    body: Result<Json<DeleteRequest>, JsonRejection>,
) -> Result<Json<DeleteResult>, ApiError> {
    let Json(request) = body?;
    let id = request.instrument;
    let month = |text: &str| {
        Month::parse(text).ok_or_else(|| {
            ApiError::bad_request(format!(
                "{text:?} is not a month: use YYYY-MM, like 2024-01"
            ))
        })
    };
    let range = match (request.from.as_deref(), request.to.as_deref()) {
        (None, None) => None,
        (Some(from), Some(to)) => {
            let (from, to) = (month(from)?, month(to)?);
            if from > to {
                return Err(ApiError::bad_request("`to` must not be before `from`"));
            }
            Some((from, to))
        }
        _ => {
            return Err(ApiError::bad_request(
                "give both `from` and `to`, or neither to delete the whole instrument",
            ));
        }
    };
    // A queued download holds nothing yet, so this server's own queue is checked first; the store
    // refuses the delete while a running download, here or in another process, holds the
    // instrument.
    if let Some(job) = state.jobs.download_in_progress(&id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            format!("{id} has a download in progress (job {job}); wait for it or cancel it"),
        ));
    }
    let store = state.store.clone();
    let report = tokio::task::spawn_blocking(move || match range {
        Some((from, to)) => store.delete_months(&id, from, to),
        None => store.delete_instrument(&id),
    })
    .await
    .map_err(|e| ApiError::internal(format!("the delete stopped: {e}")))??;
    Ok(Json(DeleteResult {
        files: report.files,
        bytes: report.bytes,
    }))
}

/// `POST /data/delete-all`: deletes every instrument's candles and funding, and answers what it
/// removed. Saved runs, paper sessions and anything else in the data folder stay. A 409 `conflict`,
/// deleting nothing, while a download or another delete holds any stored instrument.
pub(super) async fn delete_all(
    State(state): State<Arc<AppState>>,
) -> Result<Json<DeleteResult>, ApiError> {
    let store = state.store.clone();
    let report = tokio::task::spawn_blocking(move || store.delete_all())
        .await
        .map_err(|e| ApiError::internal(format!("the delete stopped: {e}")))??;
    Ok(Json(DeleteResult {
        files: report.files,
        bytes: report.bytes,
    }))
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
        for text in ["bybit:ETHUSDT:perpetual", "oanda:USDJPY:fx"] {
            let id = parse_instrument(text).expect("parses");
            assert_eq!(id.to_string(), text);
        }
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
