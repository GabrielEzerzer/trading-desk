//! Running a strategy's test plan (#32, #35) as one job, and the verdicts it leaves.
//!
//! `POST /runs/plan` runs the test battery ([`td_engine::run_battery`]) on each instrument of the
//! strategy's plan, or on a subset of them, one after the other, as a job of kind `plan`. Before
//! the job is created the route checks the plan and the rule ([`td_engine::TestPlan::validate`],
//! [`td_engine::VerdictRule::validate`]), the subset, each instrument's trading rules and the
//! store's data for each instrument over the plan's period, so a refusal is an HTTP error, not a
//! job that fails later. The store is never filled in: a range it lacks is a 422 `data_missing`
//! naming the instrument and the missing span, as for the other runs.
//!
//! Each instrument's data is loaded the way the other run jobs load theirs
//! ([`super::runs::load_run_data`]) over the plan's backtest configuration. That configuration has
//! no `warmup_from`: the engine reads no bars before the period, and the walk-forward warms each
//! test window up from the bars before it inside the period.
//!
//! Progress is one count over every instrument: the sum of each battery's own total
//! ([`td_engine::battery_runs`]), known once every instrument's data is loaded. Its message names
//! the instrument and says when the luck test, the step a cancel keeps, is running.
//!
//! A cancel stops the job at once. An instrument whose battery had finished is kept, and so is the
//! instrument whose battery the cancel stopped during the luck test (R3): the battery then returns
//! its verdict with the luck line inconclusive. An instrument stopped before R3 is dropped and the
//! later ones never start. The job ends `cancelled`, with the instruments it kept in its result.
//!
//! Each kept instrument is saved as its own run (kind `plan`, a [`PlanRun`]) under the runs folder,
//! with the [`super::runs::DataReport`] in `data.json` and a small `verdict.json` ([`VerdictRecord`]) that
//! `GET /verdicts` reads instead of the whole result.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use td_core::{Instrument, InstrumentId, Millis};
use td_data::Store;
use td_engine::{
    BatterySpec, ENGINE_REVISION, PlanRun, RunId, SavedRun, StrategyEntry, TesterError, Verdict,
    VerdictRule,
};
use ts_rs::TS;

use super::AppState;
use super::data::instrument_list;
use super::error::ApiError;
use super::jobs::{JobCreated, JobCtx, JobError, JobRequest, JobResult};
use super::runs::{
    RunData, changed_since_accepted, check_run_data, load_run_data, own_pool, save_run,
    tester_error,
};

/// The file beside a plan run's `result.json` that `GET /verdicts` reads.
pub const VERDICT_FILE: &str = "verdict.json";

/// `POST /runs/plan`: run a strategy's test plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PlanRequest {
    /// A registered strategy's name.
    pub strategy: String,
    /// Some of the plan's instruments, run in the plan's order; every one of them when left out.
    /// Each must be one of the plan's.
    #[serde(default)]
    #[ts(optional)]
    pub instruments: Option<Vec<InstrumentId>>,
}

/// What a plan job kept: one entry per instrument whose battery finished or reached the luck test
/// before a cancel, in the plan's order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PlanDone {
    pub strategy: String,
    pub instruments: Vec<PlanInstrumentDone>,
}

/// One instrument's saved battery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PlanInstrumentDone {
    /// The saved run, at `GET /runs/{run_id}`, kind `plan`.
    pub run_id: RunId,
    /// The instrument is `verdict.instrument`.
    pub verdict: Verdict,
    /// The seed of the battery's luck test and bootstraps.
    pub seed: u64,
    /// Buckets in the period with no bar; the list is the saved run's `data.dropped`.
    pub dropped_buckets: usize,
}

/// A plan run's `verdict.json`: what the verdict list needs, without the battery's runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerdictRecord {
    pub strategy: String,
    pub instrument: InstrumentId,
    pub seed: u64,
    pub engine_revision: u32,
    pub verdict: Verdict,
}

/// One row of `GET /verdicts`: the latest verdict of a strategy on an instrument.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct VerdictSummary {
    pub strategy: String,
    pub instrument: InstrumentId,
    /// The state and every criterion line; the runs behind it are at `GET /runs/{run_id}`.
    pub verdict: Verdict,
    /// The latest saved battery of this strategy on this instrument.
    pub run_id: RunId,
    pub created_at: Millis,
    /// The engine revision that produced the verdict.
    pub engine_revision: u32,
    /// False when another engine revision produced it: this engine may judge the same data
    /// differently, so run the plan again.
    pub engine_matches: bool,
    pub seed: u64,
    /// How many batteries have been saved for this strategy on this instrument, this one included.
    /// An instrument a cancel stopped before the luck test saved none and is not counted.
    pub runs: usize,
}

#[derive(Debug, Deserialize)]
pub(super) struct VerdictsQuery {
    strategy: Option<String>,
}

/// The seed of a battery: fixed per strategy and instrument, so running the plan again on the same
/// data and engine gives the same verdict, and running it again cannot draw a luckier luck test.
/// FNV-1a over `strategy|venue:symbol:kind`.
pub fn plan_seed(strategy: &str, instrument: &InstrumentId) -> u64 {
    format!("{strategy}|{instrument}")
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h: u64, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
}

fn invalid(code: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, code, message)
}

/// The instruments to run: the plan's, or the requested subset in the plan's order. 400
/// `instrument_not_in_plan` for one the plan does not test, `invalid_request` for an empty or
/// repeated list.
fn chosen_instruments(
    entry: &StrategyEntry,
    wanted: Option<&[InstrumentId]>,
) -> Result<Vec<InstrumentId>, ApiError> {
    let plan = &entry.plan.instruments;
    let Some(wanted) = wanted else {
        return Ok(plan.clone());
    };
    if wanted.is_empty() {
        return Err(invalid(
            "invalid_request",
            "`instruments` is empty; leave it out to run every instrument of the plan",
        ));
    }
    for (i, id) in wanted.iter().enumerate() {
        if !plan.contains(id) {
            let listed: Vec<String> = plan.iter().map(ToString::to_string).collect();
            return Err(invalid(
                "instrument_not_in_plan",
                format!(
                    "{id} is not one of {}'s plan instruments ({})",
                    entry.name,
                    listed.join(", ")
                ),
            ));
        }
        if wanted[..i].contains(id) {
            return Err(invalid("invalid_request", format!("{id} is listed twice")));
        }
    }
    Ok(plan
        .iter()
        .filter(|id| wanted.contains(id))
        .cloned()
        .collect())
}

/// `POST /runs/plan`.
pub(super) async fn start(
    State(state): State<Arc<AppState>>,
    body: Result<Json<PlanRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<JobCreated>), ApiError> {
    let Json(request) = body?;
    state.jobs.admit_run()?;
    let entry = state
        .strategies
        .iter()
        .find(|e| e.name == request.strategy)
        .cloned()
        .ok_or_else(|| {
            invalid(
                "invalid_config",
                format!("unknown strategy {:?}", request.strategy),
            )
        })?;
    entry.plan.validate(entry.name).map_err(|e| {
        invalid(
            "invalid_plan",
            format!("{}'s test plan is refused: {e}", entry.name),
        )
    })?;
    entry.rule.validate().map_err(|e| {
        invalid(
            "invalid_plan",
            format!("{}'s verdict rule is refused: {e}", entry.name),
        )
    })?;
    let ids = chosen_instruments(&entry, request.instruments.as_deref())?;

    // The trading rules of each instrument, from the venue's list as `GET /instruments` gives it
    // (cached per server run).
    let mut instruments = Vec::with_capacity(ids.len());
    for id in &ids {
        let listed = instrument_list(&state, id.venue, id.kind).await?;
        let instrument = listed
            .instruments
            .into_iter()
            .find(|i| &i.id == id)
            .ok_or_else(|| {
                invalid(
                    "invalid_config",
                    format!(
                        "{} does not list {id}, so its trading rules are unknown",
                        id.venue
                    ),
                )
            })?;
        instruments.push(instrument);
    }

    // The store must hold each instrument's period; nothing is downloaded.
    let (store, checked) = (state.store.clone(), entry.clone());
    let to_check = instruments.clone();
    tokio::task::spawn_blocking(move || {
        to_check.into_iter().try_for_each(|i| {
            check_run_data(&store, &checked.plan.backtest_config(checked.name, i))
        })
    })
    .await
    .map_err(|e| ApiError::internal(format!("the data check stopped: {e}")))??;

    let (store, runs_dir) = (state.store.clone(), state.runs_dir());
    let job_request = JobRequest::Plan(PlanRequest {
        strategy: request.strategy,
        instruments: Some(ids),
    });
    let job = state.jobs.submit(job_request, move |ctx| {
        execute(&entry, &instruments, &store, &runs_dir, ctx)
    })?;
    Ok((StatusCode::ACCEPTED, Json(JobCreated { job_id: job.id })))
}

/// The job: loads every instrument's data, then runs and saves one battery after another.
fn execute(
    entry: &StrategyEntry,
    instruments: &[Instrument],
    store: &Store,
    runs_dir: &Path,
    ctx: &JobCtx,
) -> Result<JobResult, JobError> {
    ctx.checkpoint()?;
    ctx.progress(0.0, "loading the data");
    let mut loaded: Vec<RunData> = Vec::with_capacity(instruments.len());
    let mut totals = Vec::with_capacity(instruments.len());
    for instrument in instruments {
        let config = entry.plan.backtest_config(entry.name, instrument.clone());
        let data = load_run_data(store, &config).map_err(changed_since_accepted)?;
        let spec = battery_spec(entry, instrument);
        totals.push(td_engine::battery_runs(&spec, &data.bars).map_err(tester_error)?);
        loaded.push(data);
        ctx.checkpoint()?;
    }
    let grand: usize = totals.iter().map(|t| t.total).sum();
    let n = instruments.len();

    let mut done = PlanDone {
        strategy: entry.name.to_string(),
        instruments: Vec::new(),
    };
    let mut base = 0;
    for (i, ((instrument, data), runs)) in instruments.iter().zip(&loaded).zip(&totals).enumerate()
    {
        let spec = battery_spec(entry, instrument);
        let id = &instrument.id;
        let step = (grand / 200).max(1);
        let progress = |count: usize, total: usize| {
            if ctx.is_cancelled() {
                return std::ops::ControlFlow::Break(());
            }
            if count.is_multiple_of(step) || count == total {
                let phase = if count > runs.luck_from && count < total {
                    "luck test, "
                } else {
                    ""
                };
                ctx.advance((base + count) as f64 / grand.max(1) as f64, || {
                    format!("{id} ({} of {n}): {phase}{count} of {total} runs", i + 1)
                });
            }
            std::ops::ControlFlow::Continue(())
        };
        let outcome =
            own_pool(|| td_engine::run_battery(&spec, &data.bars, &data.funding, Some(&progress)))?;
        let battery = match outcome {
            Ok(battery) => battery,
            Err(TesterError::Cancelled) => return Err(cancelled(done)),
            Err(e) => {
                let e = tester_error(e);
                return Err(ApiError::new(
                    e.status,
                    &e.body.code,
                    format!("{id}: {}", e.body.message),
                )
                .into());
            }
        };
        let saved = save_plan_run(entry, instrument, battery, data, runs_dir)?;
        done.instruments.push(saved);
        // A cancel during the luck test returns the verdict without it; it is kept, and the job
        // stops here.
        if ctx.is_cancelled() {
            return Err(cancelled(done));
        }
        base += runs.total;
    }
    Ok(JobResult::Plan(done))
}

/// How a cancelled plan job ends: with the instruments it kept, if any.
fn cancelled(done: PlanDone) -> JobError {
    if done.instruments.is_empty() {
        JobError::Cancelled
    } else {
        JobError::CancelledWith(Box::new(JobResult::Plan(done)))
    }
}

fn battery_spec<'a>(entry: &'a StrategyEntry, instrument: &'a Instrument) -> BatterySpec<'a> {
    BatterySpec {
        strategy: entry.name,
        plan: &entry.plan,
        rule: &entry.rule,
        instrument,
        seed: plan_seed(entry.name, &instrument.id),
    }
}

/// Saves one instrument's battery with its data report and verdict record, in one atomic save.
fn save_plan_run(
    entry: &StrategyEntry,
    instrument: &Instrument,
    battery: td_engine::BatteryResult,
    data: &RunData,
    runs_dir: &Path,
) -> Result<PlanInstrumentDone, JobError> {
    let record = VerdictRecord {
        strategy: entry.name.to_string(),
        instrument: instrument.id.clone(),
        seed: battery.seed,
        engine_revision: ENGINE_REVISION,
        verdict: battery.verdict.clone(),
    };
    let run = PlanRun::new(
        entry.name,
        &entry.plan,
        &entry.rule,
        instrument.clone(),
        battery,
        &data.bars,
        &data.funding,
    )
    .map_err(|e| ApiError::internal(e.to_string()))?;
    let record_json = serde_json::to_vec_pretty(&record)
        .map_err(|e| ApiError::internal(format!("cannot write the verdict record: {e}")))?;
    let saved = SavedRun::Plan(Box::new(run));
    let run_id = save_run(
        &saved,
        &data.report,
        &[(VERDICT_FILE, record_json.as_slice())],
        runs_dir,
    )?;
    Ok(PlanInstrumentDone {
        run_id,
        verdict: record.verdict,
        seed: record.seed,
        dropped_buckets: data.report.dropped.len(),
    })
}

/// A plan run's `config.json`, enough to tell it from the other kinds' without reading the result.
#[derive(Deserialize)]
#[allow(dead_code)] // read only to check that the fields are there
struct PlanConfigHead {
    plan: serde::de::IgnoredAny,
    rule: VerdictRule,
}

/// The verdict record of the run in `dir`, if it is a plan run. A run saved without a
/// `verdict.json` (by another tool) is read from its `result.json`. `None` for other kinds and for
/// a run that cannot be read, so one damaged run does not hide the rest.
fn read_record(dir: &Path) -> Option<VerdictRecord> {
    match fs::read(dir.join(VERDICT_FILE)) {
        Ok(bytes) => return serde_json::from_slice(&bytes).ok(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    let config = fs::read(dir.join("config.json")).ok()?;
    serde_json::from_slice::<PlanConfigHead>(&config).ok()?;
    let runs_dir = dir.parent()?;
    let id = dir.file_name()?.to_str()?;
    match td_engine::load(runs_dir, id).ok()? {
        SavedRun::Plan(run) => Some(VerdictRecord {
            strategy: run.strategy,
            instrument: run.config.instrument.id,
            seed: run.battery.seed,
            engine_revision: run.engine_revision,
            verdict: run.battery.verdict,
        }),
        _ => None,
    }
}

/// The latest verdict per strategy and instrument, with how many batteries each pair has saved,
/// ordered by strategy and then instrument. Only `strategy`'s when it is given.
pub fn latest_verdicts(
    runs_dir: &Path,
    strategy: Option<&str>,
) -> Result<Vec<VerdictSummary>, ApiError> {
    let entries = match fs::read_dir(runs_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(ApiError::internal(format!("{}: {e}", runs_dir.display())));
        }
    };
    let mut latest: BTreeMap<(String, String), VerdictSummary> = BTreeMap::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| ApiError::internal(format!("{}: {e}", runs_dir.display())))?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|n| RunId::parse(n).ok())
        else {
            continue;
        };
        let Some(record) = read_record(&entry.path()) else {
            continue;
        };
        if strategy.is_some_and(|s| s != record.strategy) {
            continue;
        }
        let key = (record.strategy.clone(), record.instrument.to_string());
        let runs = latest.get(&key).map_or(0, |s| s.runs) + 1;
        let row = VerdictSummary {
            strategy: record.strategy,
            instrument: record.instrument,
            verdict: record.verdict,
            created_at: id.created_at(),
            run_id: id,
            engine_revision: record.engine_revision,
            engine_matches: record.engine_revision == ENGINE_REVISION,
            seed: record.seed,
            runs,
        };
        match latest.get_mut(&key) {
            Some(seen) if seen.run_id > row.run_id => seen.runs = runs,
            _ => {
                latest.insert(key, row);
            }
        }
    }
    Ok(latest.into_values().collect())
}

/// `GET /verdicts?strategy=`.
pub(super) async fn verdicts(
    State(state): State<Arc<AppState>>,
    query: Result<Query<VerdictsQuery>, QueryRejection>,
) -> Result<Json<Vec<VerdictSummary>>, ApiError> {
    let Query(VerdictsQuery { strategy }) = query?;
    let runs_dir = state.runs_dir();
    let rows = tokio::task::spawn_blocking(move || latest_verdicts(&runs_dir, strategy.as_deref()))
        .await
        .map_err(|e| ApiError::internal(format!("the verdict list stopped: {e}")))??;
    Ok(Json(rows))
}
