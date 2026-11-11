//! Backtests, sweeps, out-of-sample splits and walk-forward runs as background jobs, and the runs
//! saved on disk.
//!
//! A run request is checked against the store before the job is created, so a range the store
//! does not cover is a 422 `data_missing` naming the missing spans, not a job that fails later. The
//! check streams the range a month at a time and keeps nothing: a queued job holds only its
//! request, and loads its candles and funding once it has a run slot. If the store changed in
//! between so that the run would now be refused, the job fails with that refusal. The job runs on
//! a blocking thread (a tester run spreads its backtests over a rayon pool of its own), reports
//! progress per finished backtest and stops early on a cancel.
//!
//! Every finished run is saved with [`td_engine::save_with_files`] under `<data dir>/runs`, with a
//! `data.json` in the same atomic save holding the [`DataReport`]: the minutes and buckets the
//! store's data left out.

use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use td_core::{Candle, InstrumentKind, Millis};
use td_data::time::{DAY, Month, format_minute};
use td_data::{Gap, LoadedCandles, Store, missing_minutes, resample};
use td_engine::{
    BacktestConfig, ENGINE_REVISION, EngineError, RunId, RunSummary, SavedRun, StoreError,
    TesterError, config_fingerprint,
};
use ts_rs::TS;

use super::AppState;
use super::data::GapInfo;
use super::error::ApiError;
use super::jobs::{JobCreated, JobCtx, JobError, JobRequest, JobResult, JobStatus};

/// The folder under the data directory that holds saved runs.
pub const RUNS_DIR: &str = "runs";

/// The file beside a saved run's `result.json` that holds its [`DataReport`].
const DATA_REPORT_FILE: &str = "data.json";

/// A stretch of missing minutes, or of time without a funding rate, this long or longer means the
/// store lacks that part of a run's range, and the run is refused. Shorter gaps are venue outages:
/// they and their buckets are listed in the result's [`DataReport`].
// shortcut: one fixed limit for every timeframe and venue; make it relative to the timeframe if
// daily or weekly runs over long outages get refused when they should not.
pub const MAX_GAP: Millis = DAY;

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
    /// oldest first, including any at the start or the end of the range. Each is shorter than a
    /// day; a longer one refuses the run.
    pub gaps: Vec<GapInfo>,
    /// Open times of every bucket in the range the run has no bar for: buckets with no minutes at
    /// all, and buckets left out because too many of their minutes are missing (td-data's rule: a
    /// bucket needs its first and last minute and at most 1% missing).
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
    pub funding: Vec<(Millis, f64)>,
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
        JobRequest::Download(_) => None,
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
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || check_run_data(&store, &base))
        .await
        .map_err(|e| ApiError::internal(format!("the data check stopped: {e}")))??;
    let (store, runs_dir) = (state.store.clone(), state.runs_dir());
    let work = request.clone();
    let job = state
        .jobs
        .submit(request, move |ctx| execute(&work, &store, &runs_dir, ctx))?;
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
    /// The missing minutes.
    gaps: Vec<Gap>,
    /// A perpetual's funding rates; empty for spot.
    funding: Vec<(Millis, f64)>,
}

/// Reads the 1-minute candles of the configuration's range, resamples them to its timeframe, and
/// reads a perpetual's funding.
fn read_data(store: &Store, config: &BacktestConfig) -> Result<StoreData, ApiError> {
    let id = &config.instrument.id;
    let (from, to) = data_range(config)?;
    let mut minutes = Vec::new();
    for month in Month::range(from, to) {
        minutes.extend(
            store
                .read_month(id, month)?
                .into_iter()
                .filter(|c| (from..to).contains(&c.open_time)),
        );
    }
    let gaps = missing_minutes(&minutes, from, to);
    let loaded = resample(&minutes, config.timeframe, false);
    let funding = if id.kind.rules().funding {
        store.load_funding(id, from, to)?
    } else {
        Vec::new()
    };
    Ok(StoreData {
        loaded,
        gaps,
        funding,
    })
}

/// Checks that the store holds a configuration's range well enough to run it, by the rules of
/// [`load_run_data`], without keeping the data: the 1-minute candles are read a month at a time,
/// and only the long gaps and whether any bar can be built are kept. A 400 for a bad range, a 422
/// `data_missing` where `load_run_data` would refuse.
pub fn check_run_data(store: &Store, config: &BacktestConfig) -> Result<(), ApiError> {
    let id = &config.instrument.id;
    let (from, to) = data_range(config)?;
    let timeframe = config.timeframe;
    let mut long = Vec::new();
    // The latest gap, which may go on into the next month.
    let mut open: Option<Gap> = None;
    let mut any_bar = false;
    // Minutes of a bucket that a month's end cuts, kept for the next month.
    let mut pending: Vec<Candle> = Vec::new();
    for month in Month::range(from, to) {
        let (start, end) = (month.start().max(from), month.end().min(to));
        let minutes: Vec<Candle> = store
            .read_month(id, month)?
            .into_iter()
            .filter(|c| (start..end).contains(&c.open_time))
            .collect();
        for gap in missing_minutes(&minutes, start, end) {
            match open.as_mut() {
                Some(o) if o.to == gap.from => o.to = gap.to,
                _ => long.extend(open.replace(gap).filter(is_long)),
            }
        }
        if any_bar {
            continue;
        }
        pending.extend(minutes);
        // Every bucket opening before the cut ends inside this month, so it is whole in `pending`;
        // resample judges each bucket on its own minutes.
        let cut = timeframe.floor(end);
        let split = pending.partition_point(|c| c.open_time < cut);
        any_bar = !resample(&pending[..split], timeframe, false)
            .candles
            .is_empty();
        pending.drain(..split);
    }
    long.extend(open.filter(is_long));
    any_bar = any_bar || !resample(&pending, timeframe, false).candles.is_empty();
    let funding = if id.kind.rules().funding {
        store.load_funding(id, from, to)?
    } else {
        Vec::new()
    };
    match refusal(config, &long, any_bar, &funding) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Whether a gap is long enough to refuse a run.
fn is_long(gap: &Gap) -> bool {
    gap.to - gap.from >= MAX_GAP
}

/// Why the store's data cannot run a configuration whose range is checked: a stretch of
/// [`MAX_GAP`] or more without candles or, for a perpetual, without funding rates, or no bar at
/// all. `None` when it can run.
fn refusal(
    config: &BacktestConfig,
    long_gaps: &[Gap],
    any_bar: bool,
    funding: &[(Millis, f64)],
) -> Option<ApiError> {
    let id = &config.instrument.id;
    let (from, to) = (config.warmup_from.unwrap_or(config.from), config.to);
    if !long_gaps.is_empty() || !any_bar {
        let spans = if long_gaps.is_empty() {
            span(from, to)
        } else {
            spans(&long_gaps.iter().collect::<Vec<_>>())
        };
        return Some(data_missing(format!(
            "the store has no {id} candles {spans}; download them first"
        )));
    }
    if id.kind == InstrumentKind::Perpetual {
        let times: Vec<Millis> = std::iter::once(from)
            .chain(funding.iter().map(|&(t, _)| t))
            .chain(std::iter::once(to))
            .collect();
        let long: Vec<Gap> = times
            .windows(2)
            .filter(|w| w[1] - w[0] >= MAX_GAP)
            .map(|w| Gap {
                from: w[0],
                to: w[1],
            })
            .collect();
        if !long.is_empty() {
            return Some(data_missing(format!(
                "the store has no {id} funding rates {}; download the range again",
                spans(&long.iter().collect::<Vec<_>>())
            )));
        }
    }
    None
}

/// Loads a run's data, refusing with 422 `data_missing` when the store lacks part of the range:
/// a stretch of [`MAX_GAP`] or more without candles or, for a perpetual, without funding rates,
/// or no bar at all.
pub fn load_run_data(store: &Store, config: &BacktestConfig) -> Result<RunData, ApiError> {
    let (from, to) = data_range(config)?;
    let StoreData {
        loaded,
        gaps,
        funding,
    } = read_data(store, config)?;
    let long: Vec<Gap> = gaps.iter().copied().filter(is_long).collect();
    if let Some(e) = refusal(config, &long, !loaded.candles.is_empty(), &funding) {
        return Err(e);
    }
    // Every bucket in the range with no bar: td-data's `dropped` lists only the incomplete ones,
    // not those with no minute at all. Bars are sorted and inside the range, which starts and
    // ends on bar boundaries.
    let step = config.timeframe.duration_ms();
    let mut bar_times = loaded.candles.iter().map(|c| c.open_time).peekable();
    let dropped: Vec<Millis> = (0..(to - from) / step)
        .map(|i| from + i * step)
        .filter(|&t| bar_times.next_if_eq(&t).is_none())
        .collect();
    let report = DataReport {
        bars: loaded.candles.len(),
        gaps: gaps.iter().map(GapInfo::from).collect(),
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
    Ok(RunData {
        bars: loaded.candles,
        funding,
        report,
    })
}

/// A refusal at load time of a run whose data passed the check when it was accepted: the store
/// changed while the run waited for a slot.
fn changed_since_accepted(e: ApiError) -> ApiError {
    if e.body.code != "data_missing" {
        return e;
    }
    let message = format!(
        "the store's data changed after the run was accepted, so it was not run: {}",
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

/// The job: loads the request's data, runs the request on it, saves the result, and answers its id
/// and summary.
fn execute(
    request: &JobRequest,
    store: &Store,
    runs_dir: &Path,
    ctx: &JobCtx,
) -> Result<JobResult, JobError> {
    ctx.checkpoint()?;
    let base = base_config(request).ok_or_else(|| ApiError::internal("a download is not a run"))?;
    ctx.progress(0.0, "loading the data");
    let RunData {
        bars,
        funding,
        report,
    } = load_run_data(store, base).map_err(changed_since_accepted)?;
    ctx.checkpoint()?;
    let progress = |done: usize, total: usize| report_progress(ctx, done, total);
    let saved = match request {
        JobRequest::Download(_) => {
            return Err(ApiError::internal("a download is not a run").into());
        }
        JobRequest::Backtest(config) => {
            ctx.progress(0.0, "running the backtest");
            let result = td_engine::run(config, &bars, &funding).map_err(engine_error)?;
            SavedRun::Backtest(Box::new(result))
        }
        JobRequest::Sweep(spec) => {
            let result = own_pool(|| td_engine::sweep(spec, &bars, &funding, Some(&progress)))?;
            SavedRun::Sweep(Box::new(result.map_err(tester_job_error)?))
        }
        JobRequest::Oos(spec) => {
            let result =
                own_pool(|| td_engine::out_of_sample(spec, &bars, &funding, Some(&progress)))?;
            SavedRun::Oos(Box::new(result.map_err(tester_job_error)?))
        }
        JobRequest::WalkForward(spec) => {
            let result =
                own_pool(|| td_engine::walk_forward(spec, &bars, &funding, Some(&progress)))?;
            SavedRun::WalkForward(Box::new(result.map_err(tester_job_error)?))
        }
    };
    // A cancel that came in during the last steps still wins: nothing is saved.
    ctx.checkpoint()?;
    ctx.progress(1.0, "saving the result");
    let id = save_run(&saved, &report, runs_dir)?;
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
fn own_pool<T: Send>(work: impl FnOnce() -> T + Send) -> Result<T, JobError> {
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

/// Saves a run with its data report, both in one atomic save.
fn save_run(saved: &SavedRun, report: &DataReport, runs_dir: &Path) -> Result<RunId, JobError> {
    let json = serde_json::to_vec_pretty(report)
        .map_err(|e| ApiError::internal(format!("cannot write the data report: {e}")))?;
    let files = [(DATA_REPORT_FILE, json.as_slice())];
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

/// Reads a saved run and its data report.
fn read_run(runs_dir: &Path, id: &str) -> Result<RunDetail, ApiError> {
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
    Ok(RunDetail {
        id,
        run,
        data,
        data_error,
        check: None,
    })
}

/// Compares a saved run with what the store and engine would give now.
fn check_run(store: &Store, run: &SavedRun) -> RunCheck {
    let base = run.base_config();
    let data_matches = read_data(store, base)
        .ok()
        .and_then(|d| config_fingerprint(base, &d.loaded.candles, &d.funding).ok())
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

/// Engine errors from what the client asked for are 400 `invalid_config`; the rest are internal.
fn engine_error(e: EngineError) -> ApiError {
    match e {
        EngineError::InvalidConfig(_)
        | EngineError::UnknownStrategy(_)
        | EngineError::InvalidParam { .. }
        | EngineError::NoBarsInRange { .. }
        | EngineError::FundingOnSpot => {
            ApiError::new(StatusCode::BAD_REQUEST, "invalid_config", e.to_string())
        }
        e => ApiError::internal(e.to_string()),
    }
}

/// Tester errors from what the client asked for are 400s: `invalid_spec` for the spec and
/// `no_qualifying_run` when no parameter set made enough trades.
fn tester_error(e: TesterError) -> ApiError {
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
        }
    }

    /// Loads a configuration's data as a job does, after asserting that the check made when the
    /// run is requested gives the same answer.
    fn checked(store: &Store, config: &BacktestConfig) -> Result<RunData, ApiError> {
        let loaded = load_run_data(store, config);
        assert_eq!(
            check_run_data(store, config).err(),
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

    fn span_of(from: Millis, to: Millis) -> GapInfo {
        GapInfo::from(&Gap { from, to })
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
}
