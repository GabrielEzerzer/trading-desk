//! Backtests, sweeps, out-of-sample splits and walk-forward runs as background jobs, and the runs
//! saved on disk.
//!
//! A run request is checked against the store before the job is created. Missing minutes are
//! counted in the instrument's open hours only ([`missing_minutes`]), so the FX weekend is not a
//! gap, and closures recorded at download time ([`Store::closures`]) are not gaps either. A queued
//! job holds only its request, and loads its candles and funding once it has a run slot. When the
//! store lacks part of the range then, the job first downloads what is missing through the
//! [`MarketSource`], as a step of the same job reported as `downloading the data: ...`, and then
//! loads it. A failed download fails the job with the source's error, and a download that does not
//! fill the range fails it with the refusal; nothing runs on partial data. A range nothing could be
//! downloaded for (after now, or before the venue's earliest data) is refused when it is requested,
//! with a 422 `data_missing` naming the missing spans, and so is a market with bid and ask prices
//! whose stored month lacks its asks. The check reads the range and keeps nothing. The job runs on
//! a blocking thread (a tester run spreads its backtests over a rayon pool of its own), reports
//! progress per finished backtest and stops early on a cancel.
//!
//! Every finished run is saved with [`td_engine::save_with_files`] under `<data dir>/runs`, with a
//! `data.json` in the same atomic save holding the [`DataReport`]: the minutes and buckets the
//! store's data left out, and a `record.json` holding the [`RunRecord`]: the hash of the
//! strategy's code and the data cutoff the run was made under.
//!
//! With a data cutoff in force ([`DataCutoff`], `TD_DATA_CUTOFF`), every run, sweep,
//! walk-forward and test battery whose range ends after the cutoff is refused with a 422
//! `after_cutoff`, both when it is requested ([`check_run_data`]) and when its job loads the data
//! ([`load_run_data`]), the two functions every run reads candles through.

use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use td_core::{Candle, Hours, InstrumentId, InstrumentKind, Millis};
use td_data::time::{DAY, Month, format_minute, now_ms};
use td_data::{
    Closures, DataError, Gap, LoadedCandles, MonthQuotes, Store, missing_minutes, resample,
};
use td_engine::{
    BacktestConfig, ENGINE_REVISION, EngineError, ReferenceBars, RunId, RunSummary, SavedRun,
    StoreError, TesterError, config_fingerprint_with_references,
};
use ts_rs::TS;

use super::AppState;
use super::data::{GapInfo, earliest_from};
use super::error::ApiError;
use super::jobs::{JobCreated, JobCtx, JobError, JobRequest, JobResult, JobStatus};
use super::source::MarketSource;

/// The folder under the data directory that holds saved runs.
const RUNS_DIR: &str = "runs";

/// The file beside a saved run's `result.json` that holds its [`DataReport`].
const DATA_REPORT_FILE: &str = "data.json";

/// The file beside a saved run's `result.json` that holds its [`RunRecord`].
pub(super) const RECORD_FILE: &str = "record.json";

/// The environment variable that sets the data cutoff.
const DATA_CUTOFF_VAR: &str = "TD_DATA_CUTOFF";

/// A UTC date after which no run reads bars (#114), set with `TD_DATA_CUTOFF=YYYY-MM-DD` when the
/// engine starts. It keeps the latest months unseen while a strategy is worked on; the final check
/// on them is a run made with the engine started without the setting. Paper sessions and the chart
/// are not affected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataCutoff {
    date: String,
    end: Millis,
}

impl DataCutoff {
    /// Reads `YYYY-MM-DD`, a real date. Anything else is an error that says what is wanted.
    pub fn parse(text: &str) -> Result<DataCutoff, String> {
        let start = td_data::time::parse_date(text).map_err(|_| {
            format!(
                "{DATA_CUTOFF_VAR} must be a UTC date written YYYY-MM-DD, such as 2026-06-30, \
                 got {text:?}"
            )
        })?;
        Ok(DataCutoff {
            date: text.to_string(),
            end: start + DAY,
        })
    }

    /// The cutoff from `TD_DATA_CUTOFF`: `None` when it is not set, an error when it is set to
    /// anything but a date, an empty value included, so a mistyped cutoff stops the engine rather
    /// than leaving the data open.
    pub fn from_env() -> Result<Option<DataCutoff>, String> {
        match std::env::var(DATA_CUTOFF_VAR) {
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(format!("{DATA_CUTOFF_VAR} is not valid text"))
            }
            Ok(text) => DataCutoff::parse(&text).map(Some),
        }
    }

    /// The date as it was set.
    pub fn date(&self) -> &str {
        &self.date
    }

    /// Midnight UTC at the end of the date: the first moment whose bars are refused.
    pub fn end(&self) -> Millis {
        self.end
    }

    /// Refuses a configuration whose range ends after the cutoff with a 422 `after_cutoff`. A range
    /// ending exactly at the end of the cutoff date reads no bar after it.
    fn check(&self, config: &BacktestConfig) -> Result<(), ApiError> {
        if config.to <= self.end {
            return Ok(());
        }
        Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "after_cutoff",
            format!(
                "the range ends at {} UTC, after the data cutoff: the engine was started with \
                 {DATA_CUTOFF_VAR}={}, so it reads no bar after that date. End the range at {} UTC \
                 or earlier, or start the engine without {DATA_CUTOFF_VAR}",
                format_minute(config.to),
                self.date,
                format_minute(self.end)
            ),
        ))
    }
}

/// Refuses a configuration that reads past `cutoff`, when there is one.
fn within_cutoff(config: &BacktestConfig, cutoff: Option<&DataCutoff>) -> Result<(), ApiError> {
    cutoff.map_or(Ok(()), |c| c.check(config))
}

/// What a run was made under, saved beside it in `record.json`. A run saved before the file
/// existed has none, and reads as unknown.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RunRecord {
    /// The hash of the strategy's source file ([`td_engine::code_hash`]); `None` for a strategy
    /// the engine has no hash for.
    #[serde(default)]
    pub code_hash: Option<String>,
    /// The data cutoff date in force when the run was made; `None` when there was none.
    #[serde(default)]
    pub data_cutoff: Option<String>,
}

impl RunRecord {
    /// The record of a run of `strategy` made now, under `cutoff`.
    pub fn new(strategy: &str, cutoff: Option<&DataCutoff>) -> RunRecord {
        RunRecord {
            code_hash: td_engine::code_hash(strategy).map(String::from),
            data_cutoff: cutoff.map(|c| c.date.clone()),
        }
    }
}

/// Whether a saved code hash is the strategy's code in this engine: `None` when the run saved no
/// hash (saved before hashes were recorded) or the strategy is no longer registered, so the answer
/// is unknown.
pub fn code_matches(strategy: &str, saved: Option<&str>) -> Option<bool> {
    saved
        .zip(td_engine::code_hash(strategy))
        .map(|(saved, now)| saved == now)
}

/// Missing open time, or time without a funding rate, this long or longer in one stretch means the
/// store lacks that part of a run's range, and the run is refused: a day. Closures recorded at
/// download time ([`Store::closures`]), such as 25 December for FX, are closed time, not missing.
/// Minutes missing up to the start of closed hours, such as the FX weekend, and from their end are
/// one stretch, measured in the open time it holds, so the weekend inside it does not count.
/// Shorter gaps are venue outages: they and their buckets are listed in the result's
/// [`DataReport`]. A month that was never downloaded is refused.
///
/// This is the limit for a stretch that touches a month without a closure record: two days for a
/// market with hours (FX), as such a month was downloaded before records were written, so its
/// holidays, such as 25 December and 1 January, still count as missing open time. Download the
/// month again to record them and bring its limit down to a day (`gap_limit`).
// shortcut: one limit per kind for every timeframe and venue; make it relative to the timeframe if
// daily or weekly runs over long outages get refused when they should not.
pub fn max_gap(kind: InstrumentKind) -> Millis {
    match kind.rules().hours {
        Hours::Always => DAY,
        Hours::FxWeek => 2 * DAY,
    }
}

/// The limit for one stretch of missing open time, its gaps oldest first: a day when every month
/// it touches has a closure record, else [`max_gap`].
fn gap_limit(kind: InstrumentKind, closures: &Closures, stretch: &[Gap]) -> Millis {
    let (from, to) = (stretch[0].from, stretch[stretch.len() - 1].to);
    if Month::range(from, to)
        .into_iter()
        .all(|m| closures.recorded(m))
    {
        DAY
    } else {
        max_gap(kind)
    }
}

/// A bucket built although a few of its minutes are missing, within td-data's tolerance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ToleratedBucket {
    pub open_time: Millis,
    pub missing_minutes: u32,
}

/// The data a run was given: its bars at the run's timeframe, the minutes the store lacks, and the
/// buckets left out or built with minutes missing. The engine does not record these; the API
/// does, so the apps can show them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct DataReport {
    /// Bars from the start of the range (warm-up included) to its end.
    pub bars: usize,
    /// Every stretch of minutes in the range (warm-up included) the store has no candle for,
    /// oldest first, including any at the start or the end of the range, in the instrument's open
    /// hours, apart from recorded closures. Together with any next to it that only closed hours
    /// part it from, each holds less than a day of open time, or two days for FX in a month
    /// downloaded before closures were recorded; more refuses the run.
    pub gaps: Vec<GapInfo>,
    /// The spans in the range (warm-up included), oldest first, that downloads recorded the venue
    /// had nothing for and that count as closed, such as 25 December for FX: neither gaps nor
    /// dropped buckets. Empty in a run saved before closures were reported.
    #[serde(default)]
    pub closures: Vec<GapInfo>,
    /// Open times of every bucket in the range the run has no bar for: buckets with no minutes at
    /// all, and buckets left out because too many of their minutes are missing (td-data's rule: a
    /// bucket needs its first and last open minute and at most 1% of its open minutes missing). A
    /// bucket wholly inside the market's closed hours is not listed.
    pub dropped: Vec<Millis>,
    /// Buckets built with a tolerated number of minutes missing.
    pub tolerated: Vec<ToleratedBucket>,
    /// Funding rates given to the run; 0 for spot.
    pub funding_rates: usize,
}

/// What a finished run job produced: the saved run's id and its row in the run list. The whole
/// result is at `GET /jobs/{id}/result` and `GET /runs/{run_id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct RunDone {
    pub run_id: RunId,
    pub summary: RunSummary,
    /// How many buckets in the range have no bar, empty or incomplete; the list is the result's
    /// `data.dropped`, and the missing minutes are its `data.gaps`.
    pub dropped_buckets: usize,
}

/// A saved run in full.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct RunDetail {
    pub id: RunId,
    pub run: SavedRun,
    /// The data the run was given; `null` for a run saved without one (by another tool) or whose
    /// report cannot be read.
    pub data: Option<DataReport>,
    /// Why the run's data report cannot be read, when it has one that is damaged; `null` otherwise.
    pub data_error: Option<String>,
    /// Whether the run would come out the same today; set by `GET /runs/{id}`, `null` from
    /// `GET /jobs/{id}/result`.
    pub check: Option<RunCheck>,
    /// The hash of the strategy's source file when the run was made ([`td_engine::code_hash`]);
    /// `null` for a run saved before hashes were recorded.
    pub code_hash: Option<String>,
    /// Whether the strategy's source file is still the one the run was made with; `null` when that
    /// is unknown: the run saved no hash, or the strategy is no longer registered. The hash covers
    /// the strategy's own file only, not shared helpers it calls.
    pub code_matches: Option<bool>,
    /// The data cutoff date (`TD_DATA_CUTOFF`) in force when the run was made; `null` when there
    /// was none, or for a run saved before cutoffs were recorded.
    pub data_cutoff: Option<String>,
}

/// A saved run checked against the store and the engine as they are now, as
/// [`td_engine::Rerun`] reports for a rerun, without running it again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct RunCheck {
    /// Whether the store's data for the run's range hashes the same as the data the run used.
    /// False also when the store no longer has data there.
    pub data_matches: bool,
    /// Whether this engine has the revision that produced the run. When false, the same data
    /// may now give a different result because the engine changed. `null` for a sweep in which
    /// no run qualified: it holds no backtest to read the revision from.
    pub engine_matches: Option<bool>,
}

/// Candles and funding loaded for a run, with what was left out.
pub struct RunData {
    pub bars: Vec<Candle>,
    /// The ask bars beside `bars` for a kind whose rules have quotes, resampled from the same
    /// minutes, so there is one for every bar; empty for any other kind.
    pub asks: Vec<Candle>,
    pub funding: Vec<(Millis, f64)>,
    /// The bars of each reference instrument the configuration names, over the same range and at
    /// the same timeframe ([`read_references`]); empty when it names none.
    pub references: Vec<ReferenceBars>,
    pub report: DataReport,
}

/// `POST /runs/backtest`.
pub(super) async fn backtest(
    State(state): State<Arc<AppState>>,
    body: Result<Json<BacktestConfig>, JsonRejection>,
) -> Result<(StatusCode, Json<JobCreated>), ApiError> {
    let Json(config) = body?;
    // Checks the strategy, its parameters and the rules across them before any data is read.
    td_engine::build_strategy(&config.strategy, &config.params).map_err(engine_error)?;
    start(&state, JobRequest::Backtest(config)).await
}

/// `POST /runs/sweep`.
pub(super) async fn sweep(
    State(state): State<Arc<AppState>>,
    body: Result<Json<td_engine::SweepSpec>, JsonRejection>,
) -> Result<(StatusCode, Json<JobCreated>), ApiError> {
    let Json(spec) = body?;
    spec.check().map_err(tester_error)?;
    start(&state, JobRequest::Sweep(spec)).await
}

/// `POST /runs/out-of-sample`.
pub(super) async fn out_of_sample(
    State(state): State<Arc<AppState>>,
    body: Result<Json<td_engine::OosSpec>, JsonRejection>,
) -> Result<(StatusCode, Json<JobCreated>), ApiError> {
    let Json(spec) = body?;
    spec.check().map_err(tester_error)?;
    start(&state, JobRequest::Oos(spec)).await
}

/// `POST /runs/walk-forward`.
pub(super) async fn walk_forward(
    State(state): State<Arc<AppState>>,
    body: Result<Json<td_engine::WalkForwardSpec>, JsonRejection>,
) -> Result<(StatusCode, Json<JobCreated>), ApiError> {
    let Json(spec) = body?;
    spec.check().map_err(tester_error)?;
    start(&state, JobRequest::WalkForward(spec)).await
}

/// The base configuration of a run request: the backtest's own or the tester spec's base.
fn base_config(request: &JobRequest) -> Option<&BacktestConfig> {
    match request {
        JobRequest::Download(_) | JobRequest::Plan(_) => None,
        JobRequest::Backtest(c) => Some(c),
        JobRequest::Sweep(s) => Some(&s.base),
        JobRequest::Oos(s) => Some(&s.sweep.base),
        JobRequest::WalkForward(s) => Some(&s.sweep.base),
    }
}

/// Checks a request against the store and queues the run. The data is loaded by the job, once it
/// has a run slot.
async fn start(
    state: &Arc<AppState>,
    request: JobRequest,
) -> Result<(StatusCode, Json<JobCreated>), ApiError> {
    // A full queue is refused before the store is read.
    state.jobs.admit_run()?;
    let base = base_config(&request)
        .ok_or_else(|| ApiError::internal("not a run request"))?
        .clone();
    let (store, cutoff) = (state.store.clone(), state.data_cutoff.clone());
    // A shortfall the job can download is left to it; one it cannot download is refused now, as
    // is a range past the cutoff, before anything is downloaded.
    tokio::task::spawn_blocking(move || {
        let id = &base.instrument.id;
        match run_shortfall(&store, &base, cutoff.as_ref())? {
            Some(s) if download_steps(id, &s, now_ms()).is_empty() => Err(s.refusal(id)),
            _ => Ok(()),
        }
    })
    .await
    .map_err(|e| ApiError::internal(format!("the data check stopped: {e}")))??;
    let (source, store, runs_dir) = (state.source.clone(), state.store.clone(), state.runs_dir());
    let (work, cutoff) = (request.clone(), state.data_cutoff.clone());
    let job = state.jobs.submit(request, move |ctx| {
        execute(
            &work,
            source.as_ref(),
            &store,
            cutoff.as_ref(),
            &runs_dir,
            ctx,
        )
    })?;
    Ok((StatusCode::ACCEPTED, Json(JobCreated { job_id: job.id })))
}

/// The range of data a configuration reads: from its warm-up start to its end. `from`, `to` and
/// `warmup_from` must each be where a bar of the timeframe opens, so every bucket in the range is
/// whole and a bucket the range cuts is never reported as dropped; with `from` before `to`, that
/// also means the range holds at least one whole bar.
fn data_range(config: &BacktestConfig) -> Result<(Millis, Millis), ApiError> {
    if config.from >= config.to {
        return Err(invalid_config("`to` must be after `from`".into()));
    }
    let timeframe = config.timeframe;
    let edges = [
        ("from", Some(config.from)),
        ("to", Some(config.to)),
        ("warmup_from", config.warmup_from),
    ];
    for (name, t) in edges {
        if let Some(t) = t.filter(|&t| timeframe.floor(t) != t) {
            return Err(invalid_config(format!(
                "`{name}` ({} UTC) is not where a {} bar opens; the bar holding it opens at {} UTC",
                format_minute(t),
                timeframe.as_str(),
                format_minute(timeframe.floor(t))
            )));
        }
    }
    match config.warmup_from {
        Some(w) if w > config.from => Err(invalid_config(
            "`warmup_from` must not be after `from`".into(),
        )),
        w => Ok((w.unwrap_or(config.from), config.to)),
    }
}

fn invalid_config(message: String) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid_config", message)
}

/// What the store holds for a configuration's range, before judging what is missing.
struct StoreData {
    /// The bars at the configuration's timeframe, with the buckets left out.
    loaded: LoadedCandles,
    /// The ask bars beside the bars, from the same minutes; empty for a kind without quotes.
    asks: Vec<Candle>,
    /// The missing minutes, apart from recorded closures.
    gaps: Vec<Gap>,
    /// The closures recorded over the range.
    closures: Closures,
    /// A perpetual's funding rates; empty for spot.
    funding: Vec<(Millis, f64)>,
}

/// Reads the 1-minute candles of the configuration's range, and for a kind with quotes their asks,
/// resamples both to its timeframe by the same rule over the same minutes, and reads a perpetual's
/// funding.
fn read_data(store: &Store, config: &BacktestConfig) -> Result<StoreData, ApiError> {
    let id = &config.instrument.id;
    let (from, to) = data_range(config)?;
    let in_range = |c: &Candle| (from..to).contains(&c.open_time);
    let (mut minutes, mut ask_minutes) = (Vec::new(), Vec::new());
    for stored in read_months(store, id, &Month::range(from, to)) {
        let (bids, asks) = stored?;
        ask_minutes.extend(asks.iter().copied().filter(in_range));
        minutes.extend(bids.iter().copied().filter(in_range));
    }
    let closures = store.closures(id, from, to)?;
    let gaps = closures.remove(missing_minutes(id.kind, &minutes, from, to));
    let loaded = resample(id.kind, &minutes, config.timeframe, false);
    // The asks share the bids' rows in the store, so these are the same minutes and resample
    // builds the same buckets from them.
    let asks = resample(id.kind, &ask_minutes, config.timeframe, false).candles;
    let funding = if id.kind.rules().funding {
        store.load_funding(id, from, to)?
    } else {
        Vec::new()
    };
    Ok(StoreData {
        loaded,
        asks,
        gaps,
        closures,
        funding,
    })
}

/// The bars of each reference instrument `config` names ([`BacktestConfig::references`]), over
/// the configuration's range: the stored 1-minute bids resampled to its timeframe, as its own bars
/// are. The range ends where the configuration's does, so the data cutoff that applies to it
/// applies to them. Refuses with 400 `invalid_config` the names the engine refuses, and with 422
/// `data_missing` a reference instrument whose stored minutes leave a stretch of its limit or more
/// of open time missing ([`long_gaps`]), or build no bar at all. Shorter gaps stay gaps: the
/// strategy sees a missing bar as missing.
///
/// shortcut: a reference instrument's missing range is refused, never downloaded as a run's own
/// range is; download it first with `POST /data/download`. Download it in the run job if runs
/// with references become common.
fn read_references(store: &Store, config: &BacktestConfig) -> Result<Vec<ReferenceBars>, ApiError> {
    if config.references.is_empty() {
        return Ok(Vec::new());
    }
    config.check_references().map_err(engine_error)?;
    let (from, to) = data_range(config)?;
    let months = Month::range(from, to);
    let mut references = Vec::with_capacity(config.references.len());
    for id in &config.references {
        let mut minutes = Vec::new();
        for stored in read_months(store, id, &months) {
            let (bids, _) = stored?;
            minutes.extend(bids.iter().filter(|c| (from..to).contains(&c.open_time)));
        }
        let closures = store.closures(id, from, to)?;
        let gaps = closures.remove(missing_minutes(id.kind, &minutes, from, to));
        let bars = resample(id.kind, &minutes, config.timeframe, false).candles;
        let mut missing = long_gaps(id.kind, &gaps, &closures);
        if missing.is_empty() && bars.is_empty() {
            missing.push(Gap { from, to });
        }
        if !missing.is_empty() {
            return Err(data_missing(format!(
                "the store has no {id} candles {}; a reference instrument is not downloaded with \
                 the run, so download it first",
                spans(&missing.iter().collect::<Vec<_>>())
            )));
        }
        references.push(ReferenceBars {
            instrument: id.clone(),
            bars,
        });
    }
    Ok(references)
}

/// Checks that the store holds a configuration's range well enough to run it, by the rules of
/// [`load_run_data`], without keeping the data: the months' 1-minute candles, and a quoted
/// market's asks, are read together ([`read_months`]) and gone through a month at a time, and only
/// the missing minutes and whether any bar can be built are kept. A 400 for a bad range, a 422
/// `after_cutoff` for a range that ends after `cutoff`, and a 422 `data_missing` where
/// `load_run_data` would refuse.
pub fn check_run_data(
    store: &Store,
    config: &BacktestConfig,
    cutoff: Option<&DataCutoff>,
) -> Result<(), ApiError> {
    match run_shortfall(store, config, cutoff)? {
        Some(shortfall) => Err(shortfall.refusal(&config.instrument.id)),
        None => Ok(()),
    }
}

/// What [`check_run_data`] finds the store lacks of a configuration's range; `None` when it can
/// run. A 400 for a bad range, a 422 `after_cutoff` for a range that ends after `cutoff`, checked
/// before the store is read, and a 422 for a month without its asks. The reference instruments
/// the configuration names are checked too, and one the store lacks is a refusal, not a
/// shortfall, since a run does not download them ([`read_references`]).
pub(super) fn run_shortfall(
    store: &Store,
    config: &BacktestConfig,
    cutoff: Option<&DataCutoff>,
) -> Result<Option<Shortfall>, ApiError> {
    let id = &config.instrument.id;
    let (from, to) = data_range(config)?;
    within_cutoff(config, cutoff)?;
    let timeframe = config.timeframe;
    // Each month's missing minutes; a gap that a month's end cuts is joined again by `refusal`.
    let mut gaps = Vec::new();
    let mut any_bar = false;
    // Minutes of a bucket that a month's end cuts, kept for the next month.
    let mut pending: Vec<Candle> = Vec::new();
    let months = Month::range(from, to);
    for (&month, stored) in months.iter().zip(read_months(store, id, &months)) {
        let (start, end) = (month.start().max(from), month.end().min(to));
        // The asks are read only to check the month has them; the load takes them from the
        // store's memory with the bids.
        let (stored, _) = stored?;
        let minutes: Vec<Candle> = stored
            .iter()
            .filter(|c| (start..end).contains(&c.open_time))
            .copied()
            .collect();
        gaps.extend(missing_minutes(id.kind, &minutes, start, end));
        if any_bar {
            continue;
        }
        pending.extend(minutes);
        // Every bucket opening before the cut ends inside this month, so it is whole in `pending`;
        // resample judges each bucket on its own minutes.
        let cut = timeframe.floor(end);
        let split = pending.partition_point(|c| c.open_time < cut);
        any_bar = !resample(id.kind, &pending[..split], timeframe, false)
            .candles
            .is_empty();
        pending.drain(..split);
    }
    any_bar = any_bar
        || !resample(id.kind, &pending, timeframe, false)
            .candles
            .is_empty();
    let funding = if id.kind.rules().funding {
        store.load_funding(id, from, to)?
    } else {
        Vec::new()
    };
    let closures = store.closures(id, from, to)?;
    let gaps = closures.remove(gaps);
    read_references(store, config)?;
    Ok(shortfall(config, &gaps, &closures, any_bar, &funding))
}

/// The stored 1-minute bids of each of `months`, in order, and for a kind whose rules require
/// quotes their asks, one per bid, from the same read of the month's file
/// ([`Store::read_quote_months`]), so a download landing between two reads cannot pair them
/// wrongly; no asks for a kind without quotes. The months are read together, from the store's
/// memory where it can, and decoded in parallel. Refuses with 422 `data_missing` a month whose file
/// has no ask prices: a run on its bids alone would trade at no spread.
fn read_months(
    store: &Store,
    id: &InstrumentId,
    months: &[Month],
) -> Vec<Result<MonthQuotes, ApiError>> {
    if !id.kind.rules().quotes {
        return store
            .read_months(id, months)
            .into_iter()
            .map(|bids| Ok((bids?, Arc::default())))
            .collect();
    }
    months
        .iter()
        .zip(store.read_quote_months(id, months))
        .map(|(month, quotes)| match quotes {
            Err(DataError::NoAskColumns { .. }) => Err(data_missing(format!(
                "the store's {id} candles for {month} have no ask prices; delete the month and \
                 download it again"
            ))),
            quotes => quotes.map_err(ApiError::from),
        })
        .collect()
}

/// The stretches of missing minutes that refuse a run of a `kind`. `gaps` are oldest first and cut
/// at the closed hours, as [`missing_minutes`] gives them, so each is open time only. A gap joins
/// the one before it when no open minute lies between them: they meet, as at the end of a month
/// the run check reads on its own, or only closed hours part them, as the FX weekend does. A
/// stretch, from its first gap's start to its last gap's end, refuses the run when its gaps add up
/// to its limit ([`gap_limit`]) or more; adding the gaps counts open time only, so a holiday on a
/// Friday joined across the weekend does not gain two days. Recorded closures are already out of
/// `gaps`.
fn long_gaps(kind: InstrumentKind, gaps: &[Gap], closures: &Closures) -> Vec<Gap> {
    gaps.chunk_by(|a, b| {
        a.to == b.from || kind.closed_spans(a.to, b.from).first() == Some(&(a.to..b.from))
    })
    .filter(|stretch| {
        stretch.iter().map(|g| g.to - g.from).sum::<Millis>() >= gap_limit(kind, closures, stretch)
    })
    .map(|stretch| Gap {
        from: stretch[0].from,
        to: stretch[stretch.len() - 1].to,
    })
    .collect()
}

/// What the store lacks of a configuration's range, by the rules of [`load_run_data`].
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Shortfall {
    /// The stretches of missing minutes that refuse the run ([`long_gaps`]), or the whole range
    /// when no bar can be built from it.
    candles: Vec<Gap>,
    /// For a perpetual, the stretches of [`max_gap`] or more without a funding rate.
    funding: Vec<Gap>,
}

impl Shortfall {
    /// The 422 `data_missing` refusing the run, naming the missing candles first.
    fn refusal(&self, id: &InstrumentId) -> ApiError {
        if self.candles.is_empty() {
            data_missing(format!(
                "the store has no {id} funding rates {}; download the range again",
                spans(&self.funding.iter().collect::<Vec<_>>())
            ))
        } else {
            data_missing(format!(
                "the store has no {id} candles {}; download them first",
                spans(&self.candles.iter().collect::<Vec<_>>())
            ))
        }
    }
}

/// What the store's data lacks to run a configuration whose range is checked, given its missing
/// minutes as [`missing_minutes`] gives them less the recorded `closures`: a stretch of them
/// holding its limit or more of open time ([`long_gaps`]), [`max_gap`] or more without funding
/// rates for a perpetual, or no bar at all. `None` when it can run.
fn shortfall(
    config: &BacktestConfig,
    gaps: &[Gap],
    closures: &Closures,
    any_bar: bool,
    funding: &[(Millis, f64)],
) -> Option<Shortfall> {
    let id = &config.instrument.id;
    let (from, to) = (config.warmup_from.unwrap_or(config.from), config.to);
    let mut candles = long_gaps(id.kind, gaps, closures);
    if candles.is_empty() && !any_bar {
        candles.push(Gap { from, to });
    }
    let mut long = Vec::new();
    if id.kind == InstrumentKind::Perpetual {
        let times: Vec<Millis> = std::iter::once(from)
            .chain(funding.iter().map(|&(t, _)| t))
            .chain(std::iter::once(to))
            .collect();
        long = times
            .windows(2)
            .filter(|w| w[1] - w[0] >= max_gap(id.kind))
            .map(|w| Gap {
                from: w[0],
                to: w[1],
            })
            .collect();
    }
    (!candles.is_empty() || !long.is_empty()).then_some(Shortfall {
        candles,
        funding: long,
    })
}

/// One step of a run job's download.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fetch {
    /// The 1-minute candles in `[from, to)`, inside one calendar month.
    Candles(Millis, Millis),
    /// A perpetual's funding rates in `[from, to)`.
    Funding(Millis, Millis),
}

/// What a run job downloads for a shortfall: each stretch clipped to what the venue can have, from
/// its earliest data ([`earliest_from`]) up to `now`, candles a calendar month a step as
/// `/data/download` fetches them. Empty when none of it can be downloaded.
fn download_steps(id: &InstrumentId, shortfall: &Shortfall, now: Millis) -> Vec<Fetch> {
    let (earliest, _) = earliest_from(id.venue);
    let clip = |g: &Gap| Some((g.from.max(earliest), g.to.min(now))).filter(|(f, t)| f < t);
    let mut steps = Vec::new();
    for (from, to) in shortfall.candles.iter().filter_map(clip) {
        steps.extend(
            Month::range(from, to)
                .into_iter()
                .map(|m| Fetch::Candles(from.max(m.start()), to.min(m.end()))),
        );
    }
    let funding = shortfall.funding.iter().filter_map(clip);
    steps.extend(funding.map(|(from, to)| Fetch::Funding(from, to)));
    steps
}

/// Downloads `steps` into the store through `source`, each reported as `downloading the data: ...`,
/// checking for a cancel between steps. A failed step fails the job with the source's error.
fn download(
    source: &dyn MarketSource,
    store: &Store,
    id: &InstrumentId,
    steps: &[Fetch],
    ctx: &JobCtx,
) -> Result<(), JobError> {
    for (i, &step) in steps.iter().enumerate() {
        ctx.checkpoint()?;
        let fraction = i as f64 / steps.len() as f64;
        match step {
            Fetch::Candles(from, to) => {
                let month = Month::containing(from);
                ctx.progress(fraction, format!("downloading the data: {id} {month}"));
                source.candles(store, id, from, to)?;
            }
            Fetch::Funding(from, to) => {
                ctx.progress(
                    fraction,
                    format!("downloading the data: {id} funding rates"),
                );
                source.funding(store, id, from, to)?;
            }
        }
    }
    ctx.checkpoint()?;
    Ok(())
}

/// Loads a run's data, refusing with 422 `after_cutoff` when the range ends after `cutoff`, and
/// with 422 `data_missing` when the store lacks part of the range: a stretch of missing minutes
/// holding [`max_gap`] or more of open time or, for a perpetual, [`max_gap`] or more without
/// funding rates, or no bar at all.
pub fn load_run_data(
    store: &Store,
    config: &BacktestConfig,
    cutoff: Option<&DataCutoff>,
) -> Result<RunData, ApiError> {
    load_or_shortfall(store, config, cutoff)?.map_err(|s| s.refusal(&config.instrument.id))
}

/// [`load_run_data`], answering what the store lacks instead of refusing. A range past `cutoff`
/// is refused before the store is read, so the job downloads nothing for it.
fn load_or_shortfall(
    store: &Store,
    config: &BacktestConfig,
    cutoff: Option<&DataCutoff>,
) -> Result<Result<RunData, Shortfall>, ApiError> {
    let (from, to) = data_range(config)?;
    within_cutoff(config, cutoff)?;
    let StoreData {
        loaded,
        asks,
        gaps,
        closures,
        funding,
    } = read_data(store, config)?;
    let any_bar = !loaded.candles.is_empty();
    if let Some(s) = shortfall(config, &gaps, &closures, any_bar, &funding) {
        return Ok(Err(s));
    }
    let references = read_references(store, config)?;
    // Every bucket in the range with no bar: td-data's `dropped` lists only the incomplete ones,
    // not those with no minute at all. Bars are sorted and inside the range, which starts and
    // ends on bar boundaries. A bucket wholly in closed hours or recorded closures has no bar to
    // miss.
    let step = config.timeframe.duration_ms();
    let kind = config.instrument.id.kind;
    let mut bar_times = loaded.candles.iter().map(|c| c.open_time).peekable();
    let dropped: Vec<Millis> = (0..(to - from) / step)
        .map(|i| from + i * step)
        .filter(|&t| bar_times.next_if_eq(&t).is_none())
        .filter(|&t| {
            !closures
                .remove(missing_minutes(kind, &[], t, t + step))
                .is_empty()
        })
        .collect();
    let report = DataReport {
        bars: loaded.candles.len(),
        gaps: gaps.iter().map(GapInfo::from).collect(),
        closures: closures
            .spans_in(from, to)
            .iter()
            .map(GapInfo::from)
            .collect(),
        dropped,
        tolerated: loaded
            .tolerated
            .into_iter()
            .map(|(open_time, missing_minutes)| ToleratedBucket {
                open_time,
                missing_minutes,
            })
            .collect(),
        funding_rates: funding.len(),
    };
    Ok(Ok(RunData {
        bars: loaded.candles,
        asks,
        funding,
        references,
        report,
    }))
}

/// A refusal at load time of a run whose data passed the check when it was accepted: the store
/// changed while the run waited for a slot.
pub(super) fn changed_since_accepted(e: ApiError) -> ApiError {
    if e.body.code != "data_missing" {
        return e;
    }
    let message = format!(
        "the store's data changed after the run was accepted, so it was not run: {}",
        e.body.message
    );
    ApiError::new(e.status, "data_missing", message)
}

/// A refusal at load time after the job downloaded what the store lacked: the venue did not have
/// it all.
fn not_filled(e: ApiError) -> ApiError {
    if e.body.code != "data_missing" {
        return e;
    }
    let message = format!(
        "the download did not fill the range, so it was not run: {}",
        e.body.message
    );
    ApiError::new(e.status, "data_missing", message)
}

fn data_missing(message: String) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "data_missing", message)
}

fn span(from: Millis, to: Millis) -> String {
    format!("from {} to {} UTC", format_minute(from), format_minute(to))
}

/// The first few spans, for a message.
fn spans(gaps: &[&Gap]) -> String {
    const SHOWN: usize = 3;
    let mut text = gaps
        .iter()
        .take(SHOWN)
        .map(|g| span(g.from, g.to))
        .collect::<Vec<_>>()
        .join(", ");
    if gaps.len() > SHOWN {
        text.push_str(&format!(" and {} more spans", gaps.len() - SHOWN));
    }
    text
}

/// The job: loads the request's data, downloading first what the store lacks, runs the request on
/// it, saves the result, and answers its id and summary.
fn execute(
    request: &JobRequest,
    source: &dyn MarketSource,
    store: &Store,
    cutoff: Option<&DataCutoff>,
    runs_dir: &Path,
    ctx: &JobCtx,
) -> Result<JobResult, JobError> {
    ctx.checkpoint()?;
    let base = base_config(request).ok_or_else(|| ApiError::internal("a download is not a run"))?;
    let id = &base.instrument.id;
    ctx.progress(0.0, "loading the data");
    // A run whose data is all stored takes no hold: the hold waits for a running delete, and such
    // a run should not.
    let data = match load_or_shortfall(store, base, cutoff)? {
        Ok(data) => data,
        Err(_) => {
            // The download takes a download slot and waits for any other download of the
            // instrument, a download job's or another run's, so the same months are never fetched
            // twice at once. The turn and the hold last from here through the read, so a delete
            // cannot remove a month between the download and the read. What is missing is looked
            // at again under them, as another download or a delete may have landed since the
            // first look.
            let _turn = ctx.download_turn(id)?;
            let _hold = store.hold_for_download(id)?;
            ctx.progress(0.0, "loading the data");
            match load_or_shortfall(store, base, cutoff)? {
                Ok(data) => data,
                Err(shortfall) => {
                    let steps = download_steps(id, &shortfall, now_ms());
                    if steps.is_empty() {
                        return Err(changed_since_accepted(shortfall.refusal(id)).into());
                    }
                    download(source, store, id, &steps, ctx)?;
                    ctx.progress(0.0, "loading the data");
                    load_run_data(store, base, cutoff).map_err(not_filled)?
                }
            }
        }
    };
    let RunData {
        bars,
        asks,
        funding,
        references,
        report,
    } = data;
    ctx.checkpoint()?;
    let record = RunRecord::new(&base.strategy, cutoff);
    let progress = |done: usize, total: usize| report_progress(ctx, done, total);
    let saved = match request {
        JobRequest::Download(_) | JobRequest::Plan(_) => {
            return Err(ApiError::internal("not a single run").into());
        }
        JobRequest::Backtest(config) => {
            ctx.progress(0.0, "running the backtest");
            let result =
                td_engine::run_with_references(config, &bars, &asks, &funding, &references)
                    .map_err(engine_error)?;
            SavedRun::Backtest(Box::new(result))
        }
        JobRequest::Sweep(spec) => {
            let result = own_pool(|| {
                td_engine::sweep_with_references(
                    spec,
                    &bars,
                    &asks,
                    &funding,
                    &references,
                    Some(&progress),
                )
            })?;
            SavedRun::Sweep(Box::new(result.map_err(tester_job_error)?))
        }
        JobRequest::Oos(spec) => {
            let result = own_pool(|| {
                td_engine::out_of_sample_with_references(
                    spec,
                    &bars,
                    &asks,
                    &funding,
                    &references,
                    Some(&progress),
                )
            })?;
            SavedRun::Oos(Box::new(result.map_err(tester_job_error)?))
        }
        JobRequest::WalkForward(spec) => {
            let result = own_pool(|| {
                td_engine::walk_forward_with_references(
                    spec,
                    &bars,
                    &asks,
                    &funding,
                    &references,
                    Some(&progress),
                )
            })?;
            SavedRun::WalkForward(Box::new(result.map_err(tester_job_error)?))
        }
    };
    // A cancel that came in during the last steps still wins: nothing is saved.
    ctx.checkpoint()?;
    ctx.progress(1.0, "saving the result");
    let id = save_run(&saved, &report, &record, &[], runs_dir)?;
    let done = RunDone {
        summary: saved.summary(id.clone()),
        run_id: id,
        dropped_buckets: report.dropped.len(),
    };
    Ok(match request {
        JobRequest::Sweep(_) => JobResult::Sweep(done),
        JobRequest::Oos(_) => JobResult::Oos(done),
        JobRequest::WalkForward(_) => JobResult::WalkForward(done),
        _ => JobResult::Backtest(done),
    })
}

/// Runs a tester call on a rayon pool of its own, one thread per core, dropped when the call
/// returns. On the shared global pool, a run's backtests would queue behind every backtest another
/// run has already split off, so a second run would make no progress, and not see its cancel,
/// until the first finished. With a pool each, two runs share the cores through the operating
/// system, and a cancel stops a run after the backtest each of its threads has in hand.
pub(super) fn own_pool<T: Send>(work: impl FnOnce() -> T + Send) -> Result<T, JobError> {
    let pool = rayon::ThreadPoolBuilder::new()
        .build()
        .map_err(|e| ApiError::internal(format!("cannot start the run's threads: {e}")))?;
    Ok(pool.install(work))
}

/// Progress from the tester's worker threads. Publishes about every half percent, not on every
/// backtest, so a sweep of many short runs does not flood the `jobs` topic; checks for a cancel on
/// every call.
fn report_progress(ctx: &JobCtx, done: usize, total: usize) -> ControlFlow<()> {
    if ctx.is_cancelled() {
        return ControlFlow::Break(());
    }
    let step = (total / 200).max(1);
    if done.is_multiple_of(step) || done == total {
        // Worker threads can report their counts out of order; `advance` keeps the shown
        // progress from moving backwards.
        ctx.advance(done as f64 / total.max(1) as f64, || {
            format!("{done} of {total} backtests")
        });
    }
    ControlFlow::Continue(())
}

/// Saves a run with its data report, its record and any `extra` files, all in one atomic save.
pub fn save_run(
    saved: &SavedRun,
    report: &DataReport,
    record: &RunRecord,
    extra: &[(&str, &[u8])],
    runs_dir: &Path,
) -> Result<RunId, JobError> {
    let json = serde_json::to_vec_pretty(report)
        .map_err(|e| ApiError::internal(format!("cannot write the data report: {e}")))?;
    let record_json = serde_json::to_vec_pretty(record)
        .map_err(|e| ApiError::internal(format!("cannot write the run record: {e}")))?;
    let mut files = vec![
        (DATA_REPORT_FILE, json.as_slice()),
        (RECORD_FILE, record_json.as_slice()),
    ];
    files.extend_from_slice(extra);
    // Two identical results saved in the same millisecond get the same id; the retry lands in a
    // later millisecond.
    let mut attempt = td_engine::save_with_files(saved, runs_dir, &files);
    for _ in 0..3 {
        let Err(StoreError::RunExists(_)) = attempt else {
            break;
        };
        std::thread::sleep(std::time::Duration::from_millis(2));
        attempt = td_engine::save_with_files(saved, runs_dir, &files);
    }
    Ok(attempt.map_err(store_error)?)
}

/// Reads a saved run with its data report and record. A record that is missing (a run saved
/// before records existed, or by another tool) or damaged reads as unknown.
pub fn read_run(runs_dir: &Path, id: &str) -> Result<RunDetail, ApiError> {
    let run = td_engine::load(runs_dir, id).map_err(|e| match e {
        StoreError::InvalidRunId(_) => ApiError::not_found(format!("no run {id}")),
        StoreError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            ApiError::not_found(format!("no run {id}"))
        }
        e => store_error(e),
    })?;
    let id = RunId::parse(id).map_err(store_error)?;
    let path = runs_dir.join(id.as_str()).join(DATA_REPORT_FILE);
    let (data, data_error) = match fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, None),
        Err(e) => (None, Some(format!("cannot read {DATA_REPORT_FILE}: {e}"))),
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(report) => (Some(report), None),
            Err(e) => (None, Some(format!("{DATA_REPORT_FILE} is damaged: {e}"))),
        },
    };
    let record: RunRecord = fs::read(runs_dir.join(id.as_str()).join(RECORD_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let code_matches = code_matches(&run.base_config().strategy, record.code_hash.as_deref());
    Ok(RunDetail {
        id,
        run,
        data,
        data_error,
        check: None,
        code_hash: record.code_hash,
        code_matches,
        data_cutoff: record.data_cutoff,
    })
}

/// Compares a saved run with what the store and engine would give now.
pub fn check_run(store: &Store, run: &SavedRun) -> RunCheck {
    let base = run.base_config();
    let data_matches = read_data(store, base)
        .ok()
        .and_then(|d| {
            let references = read_references(store, base).ok()?;
            config_fingerprint_with_references(
                base,
                &d.loaded.candles,
                &d.asks,
                &d.funding,
                &references,
            )
            .ok()
        })
        .is_some_and(|fingerprint| fingerprint == run.data_fingerprint());
    RunCheck {
        data_matches,
        engine_matches: run.engine_revision().map(|r| r == ENGINE_REVISION),
    }
}

/// `GET /runs`: the saved runs, newest first.
pub(super) async fn list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<RunSummary>>, ApiError> {
    let runs_dir = state.runs_dir();
    let runs = tokio::task::spawn_blocking(move || td_engine::list(&runs_dir))
        .await
        .map_err(|e| ApiError::internal(format!("the run list stopped: {e}")))?
        .map_err(store_error)?;
    Ok(Json(runs))
}

/// `GET /runs/{id}`: one saved run, checked against today's data and engine.
pub(super) async fn get(
    State(state): State<Arc<AppState>>,
    path: Result<UrlPath<String>, PathRejection>,
) -> Result<Json<RunDetail>, ApiError> {
    let UrlPath(id) = path.map_err(|_| ApiError::not_found("no such run"))?;
    let (runs_dir, store) = (state.runs_dir(), state.store.clone());
    let detail = tokio::task::spawn_blocking(move || {
        let mut detail = read_run(&runs_dir, &id)?;
        detail.check = Some(check_run(&store, &detail.run));
        Ok::<_, ApiError>(detail)
    })
    .await
    .map_err(|e| ApiError::internal(format!("the run read stopped: {e}")))??;
    Ok(Json(detail))
}

/// `GET /jobs/{id}/result`: the saved run of a finished run job.
pub(super) async fn job_result(
    State(state): State<Arc<AppState>>,
    path: Result<UrlPath<u64>, PathRejection>,
) -> Result<Json<RunDetail>, ApiError> {
    let id = super::job_id(path)?;
    let job = state
        .jobs
        .get(id)
        .ok_or_else(|| ApiError::not_found(format!("no job {id}")))?;
    // `job_not_done` means wait and ask again; `no_result` means the job has finished without a
    // saved run, so asking again will not help.
    let no_result = |why: String| ApiError::new(StatusCode::CONFLICT, "no_result", why);
    let run_id = match job.status {
        JobStatus::Queued | JobStatus::Running => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "job_not_done",
                format!("job {id} is {:?}, not done yet", job.status).to_lowercase(),
            ));
        }
        _ if matches!(job.result, Some(JobResult::Plan(_))) => {
            return Err(no_result(format!(
                "job {id} ran a test plan, which saves one run per instrument; their ids are in \
                 GET /jobs/{id} (`result.instruments`), each at GET /runs/{{run_id}}"
            )));
        }
        JobStatus::Failed => {
            let why = job.error.map(|e| e.message).unwrap_or_default();
            return Err(no_result(format!(
                "job {id} failed, so it saved no run: {why}"
            )));
        }
        JobStatus::Cancelled => {
            return Err(no_result(format!(
                "job {id} was cancelled, so it saved no run"
            )));
        }
        JobStatus::Done => job
            .result
            .as_ref()
            .and_then(JobResult::run)
            .map(|r| r.run_id.clone())
            .ok_or_else(|| {
                no_result(format!(
                    "job {id} is not a run and saved none; its result is in GET /jobs/{id}"
                ))
            })?,
    };
    let runs_dir = state.runs_dir();
    let detail = tokio::task::spawn_blocking(move || read_run(&runs_dir, run_id.as_str()))
        .await
        .map_err(|e| ApiError::internal(format!("the run read stopped: {e}")))??;
    Ok(Json(detail))
}

/// Engine errors from what the client asked for are 400 `invalid_config`; quote errors, which mean
/// the stored bids and asks cannot be run, are 422 `data_missing`; the rest are internal.
fn engine_error(e: EngineError) -> ApiError {
    match e {
        EngineError::MissingAsk { .. }
        | EngineError::UnmatchedAsk { .. }
        | EngineError::DuplicateAsk { .. }
        | EngineError::CrossedQuote { .. }
        | EngineError::ClosedBar { .. }
        | EngineError::AsksWithoutQuotes { .. } => data_missing(format!(
            "the store's bids and asks cannot be run: {e}; delete the months and download them \
             again"
        )),
        EngineError::InvalidConfig(_)
        | EngineError::InvalidReferences(_)
        | EngineError::UnknownStrategy(_)
        | EngineError::InvalidParam { .. }
        | EngineError::NoBarsInRange { .. }
        | EngineError::FundingOnSpot
        | EngineError::StrategyFailed(_) => {
            ApiError::new(StatusCode::BAD_REQUEST, "invalid_config", e.to_string())
        }
        e => ApiError::internal(e.to_string()),
    }
}

/// Tester errors from what the client asked for are 400s: `invalid_spec` for the spec and
/// `no_qualifying_run` when no parameter set made enough trades.
pub(super) fn tester_error(e: TesterError) -> ApiError {
    match e {
        TesterError::Engine(e) => engine_error(e),
        TesterError::InvalidSpace { .. }
        | TesterError::TooManyRuns { .. }
        | TesterError::InvalidSpec(_) => {
            ApiError::new(StatusCode::BAD_REQUEST, "invalid_spec", e.to_string())
        }
        TesterError::NoQualifyingRun { .. } => {
            ApiError::new(StatusCode::BAD_REQUEST, "no_qualifying_run", e.to_string())
        }
        TesterError::Cancelled => ApiError::internal(e.to_string()),
    }
}

fn tester_job_error(e: TesterError) -> JobError {
    match e {
        TesterError::Cancelled => JobError::Cancelled,
        e => tester_error(e).into(),
    }
}

fn store_error(e: StoreError) -> ApiError {
    ApiError::internal(e.to_string())
}

impl AppState {
    pub(super) fn runs_dir(&self) -> PathBuf {
        self.store.root().join(RUNS_DIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use td_core::{FeeSchedule, Instrument, InstrumentId, Timeframe, Venue};
    use td_data::time::{MINUTE, parse_date};
    use td_engine::Params;

    fn config(kind: InstrumentKind, from: Millis, to: Millis) -> BacktestConfig {
        BacktestConfig {
            strategy: "sma_cross".into(),
            params: Params::new(),
            instrument: Instrument {
                id: InstrumentId {
                    venue: Venue::Bybit,
                    symbol: "BTCUSDT".into(),
                    kind,
                },
                base: "BTC".into(),
                quote: "USDT".into(),
                tick_size: 0.01,
                lot_size: 0.001,
                min_qty: 0.001,
                min_notional: 5.0,
                fees: FeeSchedule {
                    maker: 0.0,
                    taker: 0.0,
                    per_unit: 0.0,
                },
            },
            timeframe: Timeframe::H1,
            from,
            to,
            starting_cash: 1000.0,
            slippage_bps: 0.0,
            fee_override: None,
            warmup_from: None,
            close_at_end: false,
            max_leverage: 1.0,
            account_currency: td_core::AccountCurrency::Quote,
            references: Vec::new(),
        }
    }

    /// Loads a configuration's data as a job does, after asserting that the check made when the
    /// run is requested gives the same answer.
    fn checked(store: &Store, config: &BacktestConfig) -> Result<RunData, ApiError> {
        let loaded = load_run_data(store, config, None);
        assert_eq!(
            check_run_data(store, config, None).err(),
            loaded.as_ref().err().cloned(),
            "the check and the load disagree"
        );
        loaded
    }

    fn minutes(from: Millis, to: Millis) -> Vec<Candle> {
        (0..(to - from) / MINUTE)
            .map(|i| Candle {
                open_time: from + i * MINUTE,
                open: 100.0,
                high: 101.0,
                low: 99.0,
                close: 100.0,
                volume: 1.0,
                quote_volume: 100.0,
            })
            .collect()
    }

    #[test]
    fn short_gaps_drop_buckets_and_long_ones_or_missing_funding_refuse_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let t0 = parse_date("2024-03-01").unwrap();
        let spot = config(InstrumentKind::Spot, t0, t0 + 4 * DAY);
        let id = &spot.instrument.id;
        // Four days with a 30-minute outage on day two and day four missing entirely.
        let mut stored = minutes(t0, t0 + 3 * DAY);
        stored
            .retain(|c| !(t0 + DAY + 600 * MINUTE..t0 + DAY + 630 * MINUTE).contains(&c.open_time));
        store.write_candles(id, &stored).unwrap();

        let e = checked(&store, &spot).err().expect("day four is missing");
        assert_eq!(
            (e.status.as_u16(), e.body.code.as_str()),
            (422, "data_missing")
        );
        assert!(
            e.body
                .message
                .contains("from 2024-03-04 00:00 to 2024-03-05 00:00 UTC"),
            "{}",
            e.body.message
        );

        let three_days = config(InstrumentKind::Spot, t0, t0 + 3 * DAY);
        let data = checked(&store, &three_days).unwrap();
        assert_eq!(data.report.dropped, vec![t0 + DAY + 10 * 3_600_000]);
        assert_eq!(data.report.bars, 3 * 24 - 1);
        assert_eq!(data.report.funding_rates, 0);

        // The same minutes as a perpetual: no funding stored at all is refused, every 8 hours is not.
        let mut perp = three_days.clone();
        perp.instrument.id.kind = InstrumentKind::Perpetual;
        store.write_candles(&perp.instrument.id, &stored).unwrap();
        let e = checked(&store, &perp).err().expect("no funding");
        assert!(e.body.message.contains("funding"), "{}", e.body.message);
        let rates: Vec<(Millis, f64)> = (0..9).map(|i| (t0 + i * 8 * 3_600_000, 0.0001)).collect();
        store.write_funding(&perp.instrument.id, &rates).unwrap();
        assert_eq!(checked(&store, &perp).unwrap().report.funding_rates, 9);

        // A range with no candles at all, even a short one, is refused.
        let empty = config(InstrumentKind::Spot, t0 + 5 * DAY, t0 + 5 * DAY + 3_600_000);
        assert_eq!(
            checked(&store, &empty).err().unwrap().body.code,
            "data_missing"
        );
    }

    const HOUR: Millis = 60 * MINUTE;

    /// A store with three days of minutes from 2024-03-01, less the hour from 10:00 on day two,
    /// and an H1 spot configuration over those three days.
    fn store_with_an_hour_missing() -> (tempfile::TempDir, Store, BacktestConfig, Millis) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let t0 = parse_date("2024-03-01").unwrap();
        let outage = t0 + DAY + 10 * HOUR;
        let mut stored = minutes(t0, t0 + 3 * DAY);
        stored.retain(|c| !(outage..outage + HOUR).contains(&c.open_time));
        let spot = config(InstrumentKind::Spot, t0, t0 + 3 * DAY);
        store.write_candles(&spot.instrument.id, &stored).unwrap();
        (dir, store, spot, outage)
    }

    #[test]
    fn an_outage_filling_whole_buckets_is_reported_as_dropped() {
        let (_dir, store, spot, outage) = store_with_an_hour_missing();
        let data = checked(&store, &spot).unwrap();
        assert_eq!(data.report.dropped, vec![outage]);
        assert_eq!(data.report.bars, 3 * 24 - 1);
        assert_eq!(data.report.gaps, vec![span_of(outage, outage + HOUR)]);
    }

    #[test]
    fn on_m1_every_missing_minute_is_a_dropped_bucket() {
        let (_dir, store, mut m1, outage) = store_with_an_hour_missing();
        m1.timeframe = Timeframe::M1;
        let data = checked(&store, &m1).unwrap();
        let expected: Vec<Millis> = (0..60).map(|i| outage + i * MINUTE).collect();
        assert_eq!(data.report.dropped, expected);
        assert_eq!(data.report.bars, 3 * 1440 - 60);
        assert_eq!(data.report.gaps, vec![span_of(outage, outage + HOUR)]);
    }

    #[test]
    fn a_missing_head_and_tail_of_the_range_are_reported() {
        let (_dir, store, spot, outage) = store_with_an_hour_missing();
        // Two hours before the first stored minute to five hours past the last.
        let wider = config(
            InstrumentKind::Spot,
            spot.from - 2 * HOUR,
            spot.to + 5 * HOUR,
        );
        let data = checked(&store, &wider).unwrap();
        let mut expected = vec![spot.from - 2 * HOUR, spot.from - HOUR, outage];
        expected.extend((0..5).map(|i| spot.to + i * HOUR));
        assert_eq!(data.report.dropped, expected);
        assert_eq!(
            data.report.gaps,
            vec![
                span_of(wider.from, spot.from),
                span_of(outage, outage + HOUR),
                span_of(spot.to, wider.to),
            ]
        );
    }

    #[test]
    fn the_check_joins_a_gap_across_a_month_end_and_carries_a_week_across_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let from = parse_date("2024-02-19").unwrap(); // a Monday
        let march = parse_date("2024-03-01").unwrap();
        let spot = config(InstrumentKind::Spot, from, from + 21 * DAY);
        let with_hole = |hours: i64| {
            let hole = march - hours * HOUR..march + hours * HOUR;
            let mut stored = minutes(from, from + 21 * DAY);
            stored.retain(|c| !hole.contains(&c.open_time));
            stored
        };
        // 13 hours missing on each side of the month's end: short in each month, a day together.
        store
            .write_candles(&spot.instrument.id, &with_hole(13))
            .unwrap();
        let e = checked(&store, &spot).err().expect("refused");
        assert_eq!(e.body.code, "data_missing");
        assert!(
            e.body
                .message
                .contains("from 2024-02-29 11:00 to 2024-03-01 13:00 UTC"),
            "{}",
            e.body.message
        );

        // 11 hours on each side is under a day: run, and reported as one gap.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store
            .write_candles(&spot.instrument.id, &with_hole(11))
            .unwrap();
        let data = checked(&store, &spot).unwrap();
        assert_eq!(
            data.report.gaps,
            vec![span_of(march - 11 * HOUR, march + 11 * HOUR)]
        );

        // The one weekly bar of the week the month's end falls in is whole only across both months.
        let mut weekly = config(InstrumentKind::Spot, from + 7 * DAY, from + 14 * DAY);
        weekly.timeframe = Timeframe::W1;
        store
            .write_candles(&weekly.instrument.id, &minutes(from, from + 21 * DAY))
            .unwrap();
        assert_eq!(checked(&store, &weekly).unwrap().report.bars, 1);
    }

    #[test]
    fn fx_gaps_join_across_a_month_end_and_a_weekend_and_count_their_open_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        // Monday 26 February to Wednesday 6 March 2024. February ends at Friday 00:00 UTC, and
        // that weekend is closed from Friday 22:00 to Sunday 22:00 UTC.
        let from = parse_date("2024-02-26").unwrap();
        let thursday = parse_date("2024-02-29").unwrap();
        let monday = parse_date("2024-03-04").unwrap();
        let with_hole = |symbol: &str, hole: std::ops::Range<Millis>| {
            let mut fx = config(InstrumentKind::Fx, from, from + 9 * DAY);
            fx.instrument.id.symbol = symbol.into();
            let bids: Vec<Candle> = minutes(fx.from, fx.to)
                .into_iter()
                .filter(|c| InstrumentKind::Fx.is_open(c.open_time))
                .filter(|c| !hole.contains(&c.open_time))
                .collect();
            let asks: Vec<Candle> = bids
                .iter()
                .map(|b| Candle {
                    open: 100.5,
                    high: 101.5,
                    low: 99.5,
                    close: 100.5,
                    ..*b
                })
                .collect();
            store.write_quotes(&fx.instrument.id, &bids, &asks).unwrap();
            fx
        };
        // Thursday 12:00 to Monday 12:00: 12 hours of open time before the month's end, 22 after
        // it and 14 after the weekend, two days in all. The check reads them a month at a time,
        // as three gaps; the load as two.
        let two_days = with_hole("EURUSD", thursday + 12 * HOUR..monday + 12 * HOUR);
        let e = checked(&store, &two_days).err().expect("refused");
        assert_eq!(e.body.code, "data_missing");
        assert!(
            e.body
                .message
                .contains("from 2024-02-29 12:00 to 2024-03-04 12:00 UTC"),
            "{}",
            e.body.message
        );

        // From 13:00 it is an hour under two days of open time, though four days less an hour
        // from end to end: run, and its two gaps reported apart.
        let under = with_hole("GBPUSD", thursday + 13 * HOUR..monday + 12 * HOUR);
        let data = checked(&store, &under).unwrap();
        let (close, reopen) = (thursday + DAY + 22 * HOUR, thursday + 3 * DAY + 22 * HOUR);
        assert_eq!(
            data.report.gaps,
            vec![
                span_of(thursday + 13 * HOUR, close),
                span_of(reopen, monday + 12 * HOUR)
            ]
        );
    }

    /// December 2024 of FX minutes for `symbol` with `hole` left out, and an H1 FX configuration
    /// over the month.
    fn december_without(
        store: &Store,
        symbol: &str,
        hole: std::ops::Range<Millis>,
    ) -> BacktestConfig {
        let dec = Month::parse("2024-12").unwrap();
        let mut fx = config(InstrumentKind::Fx, dec.start(), dec.end());
        fx.instrument.id.symbol = symbol.into();
        let bids: Vec<Candle> = minutes(fx.from, fx.to)
            .into_iter()
            .filter(|c| InstrumentKind::Fx.is_open(c.open_time) && !hole.contains(&c.open_time))
            .collect();
        let asks: Vec<Candle> = bids
            .iter()
            .map(|b| Candle {
                open: 100.5,
                high: 101.5,
                low: 99.5,
                close: 100.5,
                ..*b
            })
            .collect();
        store.write_quotes(&fx.instrument.id, &bids, &asks).unwrap();
        fx
    }

    #[test]
    fn a_recorded_closure_is_no_gap_and_a_recorded_month_refuses_a_day_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let dec = Month::parse("2024-12").unwrap();
        let record = |fx: &BacktestConfig, hole: &std::ops::Range<Millis>| {
            let empty = Gap {
                from: hole.start,
                to: hole.end,
            };
            let id = &fx.instrument.id;
            store
                .record_closures(id, dec, dec.start(), dec.end(), &[empty])
                .unwrap();
        };
        // OANDA's Christmas closure, Tuesday 24 December 22:00 to Wednesday 22:00 UTC.
        let christmas = parse_date("2024-12-24").unwrap() + 22 * HOUR
            ..parse_date("2024-12-25").unwrap() + 22 * HOUR;
        let fx = december_without(&store, "EURUSD", christmas.clone());
        // Without a record, as for a month downloaded before records were written: run as
        // before, with the closure listed as a gap and its buckets as dropped.
        let data = checked(&store, &fx).unwrap();
        assert_eq!(
            data.report.gaps,
            vec![span_of(christmas.start, christmas.end)]
        );
        assert_eq!(data.report.dropped.len(), 24);
        assert_eq!(data.report.closures, vec![]);
        // Recorded at download time, it is closed: no gap, no dropped bucket, and listed as
        // closed.
        record(&fx, &christmas);
        let data = checked(&store, &fx).unwrap();
        assert_eq!(data.report.gaps, vec![]);
        assert_eq!(
            data.report.closures,
            vec![span_of(christmas.start, christmas.end)]
        );
        assert_eq!(data.report.dropped, Vec::<Millis>::new());

        // 36 hours missing from Tuesday 10 December: under the two days a month without a record
        // allows, so it runs; recorded, it is too long to be a closure, and the month's limit is
        // a day, so it is refused.
        let tuesday = parse_date("2024-12-10").unwrap();
        let hole = tuesday..tuesday + 36 * HOUR;
        let fx = december_without(&store, "GBPUSD", hole.clone());
        let data = checked(&store, &fx).unwrap();
        assert_eq!(data.report.gaps, vec![span_of(hole.start, hole.end)]);
        record(&fx, &hole);
        let e = checked(&store, &fx).err().expect("refused");
        assert_eq!(e.body.code, "data_missing");
        assert!(
            e.body
                .message
                .contains("from 2024-12-10 00:00 to 2024-12-11 12:00 UTC"),
            "{}",
            e.body.message
        );

        // Two days missing in open hours, with a record: refused.
        let two_days = tuesday..tuesday + 2 * DAY;
        let fx = december_without(&store, "USDJPY", two_days.clone());
        record(&fx, &two_days);
        let e = checked(&store, &fx).err().expect("refused");
        assert!(
            e.body
                .message
                .contains("from 2024-12-10 00:00 to 2024-12-12 00:00 UTC"),
            "{}",
            e.body.message
        );
    }

    #[test]
    fn a_shortfall_downloads_only_what_the_venue_can_have_a_month_a_step() {
        let id = InstrumentId {
            venue: Venue::Bybit,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Perpetual,
        };
        let jan = parse_date("2024-01-01").unwrap();
        let (earliest, _) = earliest_from(Venue::Bybit);
        let shortfall = Shortfall {
            candles: vec![
                Gap {
                    from: earliest - DAY,
                    to: earliest + HOUR,
                },
                Gap {
                    from: jan - DAY,
                    to: jan + 3 * DAY,
                },
            ],
            funding: vec![Gap {
                from: jan,
                to: jan + 2 * DAY,
            }],
        };
        assert_eq!(
            download_steps(&id, &shortfall, jan + DAY),
            [
                Fetch::Candles(earliest, earliest + HOUR),
                Fetch::Candles(jan - DAY, jan),
                Fetch::Candles(jan, jan + DAY),
                Fetch::Funding(jan, jan + DAY),
            ]
        );
        assert_eq!(download_steps(&id, &shortfall, earliest - DAY), []);
    }

    #[test]
    fn minutes_that_build_no_bar_are_refused_by_the_check_and_the_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let t0 = parse_date("2024-03-01").unwrap();
        let mut daily = config(InstrumentKind::Spot, t0, t0 + 3 * DAY);
        daily.timeframe = Timeframe::D1;
        // Each day lacks its first minute, so no daily bucket is built and no gap is long.
        let mut stored = minutes(t0, t0 + 3 * DAY);
        stored.retain(|c| (c.open_time - t0) % DAY != 0);
        store.write_candles(&daily.instrument.id, &stored).unwrap();
        let e = checked(&store, &daily).err().expect("no bar");
        assert_eq!(e.body.code, "data_missing");
    }

    #[test]
    fn engine_quote_errors_are_422_data_missing() {
        let kind = InstrumentKind::Fx;
        for e in [
            EngineError::MissingAsk { time: 0, kind },
            EngineError::UnmatchedAsk { time: 0 },
            EngineError::DuplicateAsk { time: 0 },
            EngineError::CrossedQuote { time: 0 },
            EngineError::ClosedBar { time: 0 },
            EngineError::AsksWithoutQuotes {
                kind: InstrumentKind::Spot,
            },
        ] {
            let api = engine_error(e.clone());
            assert_eq!(
                (api.status.as_u16(), api.body.code.as_str()),
                (422, "data_missing"),
                "{e:?}"
            );
            assert!(api.body.message.contains(&e.to_string()), "{api:?}");
        }
    }

    #[test]
    fn fx_data_carries_one_ask_bar_per_bar_and_spot_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let monday = parse_date("2024-01-08").unwrap();
        let fx = config(InstrumentKind::Fx, monday, monday + 7 * DAY);
        let bids: Vec<Candle> = minutes(monday, monday + 7 * DAY)
            .into_iter()
            .filter(|c| InstrumentKind::Fx.is_open(c.open_time))
            .collect();
        let asks: Vec<Candle> = bids
            .iter()
            .map(|b| Candle {
                open: 100.5,
                high: 101.5,
                low: 99.5,
                close: 100.5,
                ..*b
            })
            .collect();
        store.write_quotes(&fx.instrument.id, &bids, &asks).unwrap();
        let data = checked(&store, &fx).unwrap();
        assert_eq!(data.asks.len(), data.bars.len());
        assert_eq!(data.bars.len(), 5 * 24);
        for (bar, ask) in data.bars.iter().zip(&data.asks) {
            assert_eq!(ask.open_time, bar.open_time);
            assert_eq!(
                (ask.open, ask.high, ask.low, ask.close),
                (100.5, 101.5, 99.5, 100.5)
            );
        }

        let spot = config(InstrumentKind::Spot, monday, monday + 3 * DAY);
        store
            .write_candles(&spot.instrument.id, &minutes(monday, monday + 3 * DAY))
            .unwrap();
        assert_eq!(checked(&store, &spot).unwrap().asks, []);
    }

    fn span_of(from: Millis, to: Millis) -> GapInfo {
        GapInfo::from(&Gap { from, to })
    }

    /// A strategy that stops the run over what was asked for answers 400 `invalid_config` with
    /// its reason, from a backtest and from the tester alike.
    #[test]
    fn a_strategy_that_stops_the_run_is_400_invalid_config_with_its_reason() {
        let t0 = parse_date("2024-03-01").unwrap();
        // gotobi decides at 00:55 UTC, where no 15m bar closes.
        let mut gotobi = config(InstrumentKind::Spot, t0, t0 + HOUR);
        gotobi.strategy = "gotobi".into();
        gotobi.timeframe = Timeframe::M15;
        let bars: Vec<Candle> = minutes(t0, t0 + HOUR).into_iter().step_by(15).collect();
        let failed = td_engine::run(&gotobi, &bars, &[], &[]).expect_err("gotobi refuses 15m bars");
        assert!(matches!(failed, EngineError::StrategyFailed(_)), "{failed}");
        for e in [
            engine_error(failed.clone()),
            tester_error(TesterError::Engine(failed)),
        ] {
            assert_eq!(
                (e.status.as_u16(), e.body.code.as_str()),
                (400, "invalid_config"),
                "{}",
                e.body.message
            );
            assert!(
                e.body.message.contains("no 15m bar closes at both"),
                "{}",
                e.body.message
            );
        }
    }

    #[test]
    fn range_edges_off_a_bar_boundary_are_400_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        // 2024-03-04 is a Monday; three whole weeks of minutes from it.
        let monday = parse_date("2024-03-04").unwrap();
        let mut weekly = config(InstrumentKind::Spot, monday, monday + 21 * DAY);
        weekly.timeframe = Timeframe::W1;
        store
            .write_candles(&weekly.instrument.id, &minutes(monday, monday + 21 * DAY))
            .unwrap();
        let data = checked(&store, &weekly).unwrap();
        assert_eq!((data.report.bars, data.report.dropped.len()), (3, 0));

        let refused = |c: &BacktestConfig, field: &str| {
            let e = checked(&store, c).err().expect("refused");
            assert_eq!(
                (e.status.as_u16(), e.body.code.as_str()),
                (400, "invalid_config"),
                "{}",
                e.body.message
            );
            assert!(e.body.message.contains(field), "{}", e.body.message);
        };
        // From a Wednesday on complete data.
        let mut from_wednesday = weekly.clone();
        from_wednesday.from = monday + 2 * DAY;
        refused(&from_wednesday, "`from`");
        let mut to_wednesday = weekly.clone();
        to_wednesday.to = monday + 16 * DAY;
        refused(&to_wednesday, "`to`");
        let mut warmup = config(InstrumentKind::Spot, monday + DAY, monday + 2 * DAY);
        warmup.warmup_from = Some(monday + 30 * MINUTE);
        refused(&warmup, "`warmup_from`");
        // Shorter than one bar: no whole bar, so a config error, not missing data.
        let short = config(
            InstrumentKind::Spot,
            monday + HOUR,
            monday + HOUR + 30 * MINUTE,
        );
        refused(&short, "`to`");
    }

    #[test]
    fn a_cutoff_must_be_a_real_utc_date() {
        let cutoff = DataCutoff::parse("2024-03-02").unwrap();
        assert_eq!(cutoff.date(), "2024-03-02");
        assert_eq!(cutoff.end(), parse_date("2024-03-03").unwrap());
        for bad in [
            "",
            "2024-3-2",
            "02/03/2024",
            "2024-02-30",
            "2024-03-02T00:00",
            " 2024-03-02",
        ] {
            let e = DataCutoff::parse(bad).unwrap_err();
            assert!(e.contains("TD_DATA_CUTOFF must be a UTC date"), "{e}");
            assert!(e.contains(&format!("{bad:?}")), "{e}");
        }
    }

    /// The check made when a run is requested and the load its job makes both refuse a range
    /// that ends after the cutoff, so neither path reads past it; a range ending at the end of the
    /// cutoff date, or with no cutoff, loads.
    #[test]
    fn the_check_and_the_load_refuse_a_range_past_the_cutoff() {
        let (_dir, store, spot, _) = store_with_an_hour_missing();
        let cutoff = DataCutoff::parse("2024-03-02").unwrap();
        let refused = [
            check_run_data(&store, &spot, Some(&cutoff)).unwrap_err(),
            load_run_data(&store, &spot, Some(&cutoff)).err().unwrap(),
        ];
        for e in refused {
            assert_eq!(
                (e.status.as_u16(), e.body.code.as_str()),
                (422, "after_cutoff"),
                "{}",
                e.body.message
            );
            assert!(
                e.body.message.contains(
                    "the range ends at 2024-03-04 00:00 UTC, after the data cutoff: the engine \
                     was started with TD_DATA_CUTOFF=2024-03-02"
                ),
                "{}",
                e.body.message
            );
            assert!(
                e.body
                    .message
                    .contains("End the range at 2024-03-03 00:00 UTC"),
                "{}",
                e.body.message
            );
        }
        let mut within = spot.clone();
        within.to = cutoff.end();
        assert_eq!(check_run_data(&store, &within, Some(&cutoff)), Ok(()));
        assert!(load_run_data(&store, &within, Some(&cutoff)).is_ok());
        assert!(load_run_data(&store, &spot, None).is_ok());
    }

    /// On an empty store a range past the cutoff is refused as past the cutoff, not answered as a
    /// shortfall, both by the check a request makes and by the job's first load, so neither the
    /// request nor the job reaches a download step.
    #[test]
    fn a_range_past_the_cutoff_is_no_shortfall_to_download() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let t0 = parse_date("2024-03-01").unwrap();
        let spot = config(InstrumentKind::Spot, t0, t0 + 3 * DAY);
        let cutoff = DataCutoff::parse("2024-03-02").unwrap();
        let shortfall = run_shortfall(&store, &spot, None).unwrap();
        assert!(
            shortfall
                .is_some_and(|s| !download_steps(&spot.instrument.id, &s, now_ms()).is_empty())
        );
        let refused = [
            run_shortfall(&store, &spot, Some(&cutoff)).unwrap_err(),
            load_or_shortfall(&store, &spot, Some(&cutoff))
                .err()
                .unwrap(),
        ];
        for e in refused {
            assert_eq!(e.body.code, "after_cutoff", "{}", e.body.message);
        }
    }
}
