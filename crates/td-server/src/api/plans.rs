//! Running a strategy's test plan (#32, #35) as one job, and the verdicts it leaves.
//!
//! `POST /runs/plan` runs the test battery ([`td_engine::run_battery`]) on each instrument of the
//! strategy's plan, or on a subset of them, one after the other, as a job of kind `plan`. Before
//! the job is created the route checks the plan and the rule ([`td_engine::TestPlan::validate`],
//! [`td_engine::VerdictRule::validate`]), the subset, each instrument's trading rules and the
//! store's data for each instrument over the plan's period, so a refusal is an HTTP error, not a
//! job that fails later. The store is never filled in: a range it lacks is a 422 `data_missing`
//! naming the instrument and the missing span, where a single run's job downloads it first.
//!
//! Each instrument's data is loaded the way the other run jobs load theirs
//! ([`super::runs::load_run_data`]) over the plan's backtest configuration: the bars, a market
//! with quotes' ask bars beside them, and a perpetual's funding. Every run of the battery trades
//! on those asks, the luck test's (R3) shuffled copies too. That
//! configuration has no `warmup_from`: the engine reads no bars before the period, and the
//! walk-forward warms each test window up from the bars before it inside the period.
//!
//! Progress is one count over every instrument: the sum of each battery's own total
//! ([`td_engine::battery_runs`]), known once every instrument's data is loaded. Its message names
//! the instrument and says when the luck test, the step a cancel keeps, is running.
//!
//! A cancel stops the job at once. An instrument whose battery had finished is kept, and so is the
//! instrument whose battery the cancel stopped during the luck test (R3): the battery then returns
//! its verdict with the luck line inconclusive. An instrument stopped before R3 is dropped and the
//! later ones never start. The job ends `cancelled`, with the instruments it kept in its result.
//! A battery the cancel stopped during the luck test is saved as incomplete
//! ([`VerdictRecord::complete`]), and the verdict list prefers a complete one.
//!
//! An error on one instrument (its battery or its save) fails the job; the instruments saved
//! before it stay saved and are in the failed job's result.
//!
//! Each kept instrument is saved as its own run (kind `plan`, a [`PlanRun`]) under the runs folder,
//! with the [`super::runs::DataReport`] in `data.json`, the [`RunRecord`] in `record.json`, and a
//! small `verdict.json` ([`VerdictRecord`]) that `GET /verdicts` reads instead of the whole result.
//!
//! A verdict is current only when the engine revision, the plan and rule, and the strategy's
//! source file ([`td_engine::code_hash`]) are all still the ones it was judged under; the verdict
//! list flags each and sums them up in `stale`. It also lists the limits of the rule that are
//! looser than the kind's defaults ([`td_engine::VerdictRule::loosened`]), and counts the distinct
//! versions of the strategy's code that have been through the battery on the instrument, so many
//! edits until one version passes show.
//!
//! A paper session is linked to the verdict it was started from (#115): [`SessionLink`] records,
//! when the session starts, the hash of the strategy's source file, its plan and rule, and that
//! verdict, and [`LinkState`] checks it against the strategy as it is now whenever the session's
//! state is read: with the same checks the verdict list makes when it has a verdict, and against
//! what the session started with when it has none.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use td_core::{Instrument, InstrumentId, Millis};
use td_data::Store;
use td_engine::{
    BatterySpec, ENGINE_REVISION, LoosenedLimit, PlanRun, RunId, RunKind, SavedRun, StrategyEntry,
    TesterError, Verdict, VerdictRule, VerdictState,
};
use ts_rs::TS;

use super::AppState;
use super::data::instrument_list;
use super::error::ApiError;
use super::jobs::{JobCreated, JobCtx, JobError, JobRequest, JobResult};
use super::runs::{
    DataCutoff, RECORD_FILE, RunData, RunRecord, changed_since_accepted, check_run_data,
    code_matches, load_run_data, own_pool, save_run, tester_error,
};

/// The file beside a plan run's `result.json` that `GET /verdicts` reads.
const VERDICT_FILE: &str = "verdict.json";

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
    /// Reference instruments every run of each battery names
    /// ([`td_engine::BacktestConfig::references`]): the strategy reads their bars beside its own
    /// but trades only the plan's instrument. Each must be on the venue of every instrument run,
    /// and the store must hold their bars over the plan's period; they are not downloaded. None
    /// when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub references: Option<Vec<InstrumentId>>,
}

/// What a plan job kept: one entry per instrument whose battery finished or reached the luck test
/// before a cancel, in the plan's order. A failed plan job holds the instruments it saved before
/// the error.
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

/// A plan run's `verdict.json`: what the verdict list needs, without the battery's runs. A record
/// that lacks a field (written before it existed) or does not parse is read from the run's
/// `result.json` instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct VerdictRecord {
    pub strategy: String,
    pub instrument: InstrumentId,
    pub seed: u64,
    pub engine_revision: u32,
    pub verdict: Verdict,
    /// The plan ([`td_engine::TestPlan`]) and the rule ([`td_engine::VerdictRule`]) the battery
    /// ran under. Both are compiled in, and editing one does not change [`ENGINE_REVISION`], so the
    /// verdict list compares them with the registered ones. They are kept as JSON so that a record
    /// written before a field of either was added, renamed or removed still reads, as stale.
    pub plan: Value,
    pub rule: Value,
    /// False when a cancel stopped the luck test (R3), so the verdict lacks it.
    pub complete: bool,
    /// The hash of the strategy's source file the battery ran with ([`td_engine::code_hash`]);
    /// `None` in a record written before hashes were recorded.
    #[serde(default)]
    pub code_hash: Option<String>,
    /// The reference instruments the battery's runs named; empty, and left out, when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<InstrumentId>,
}

/// One row of `GET /verdicts`: the latest verdict of a strategy on an instrument, the newest
/// complete one when the pair has any.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct VerdictSummary {
    pub strategy: String,
    pub instrument: InstrumentId,
    /// The state and every criterion line; the runs behind it are at `GET /runs/{run_id}`.
    pub verdict: Verdict,
    /// The newest complete battery of this strategy on this instrument, or the newest battery when
    /// none is complete.
    pub run_id: RunId,
    pub created_at: Millis,
    /// The engine revision that produced the verdict.
    pub engine_revision: u32,
    /// False when another engine revision produced it: this engine may judge the same data
    /// differently, so run the plan again.
    pub engine_matches: bool,
    /// False when the strategy is no longer registered, its plan or rule has changed since the
    /// battery ran, or the plan no longer tests the instrument: run the plan again.
    pub plan_matches: bool,
    /// Whether the strategy's source file is still the one the battery ran with: false when it has
    /// been edited since, `null` when that is unknown (a battery saved before code hashes were
    /// recorded, or a strategy no longer registered). The hash covers the strategy's own file only,
    /// not shared helpers it calls, so a change to a shared helper leaves this true.
    pub code_matches: Option<bool>,
    /// True unless `engine_matches`, `plan_matches` and `code_matches` are all true: the verdict
    /// may not be what this engine, plan, rule and code would give, so run the plan again.
    pub stale: bool,
    /// Every limit of the rule the verdict was judged under that is looser than its kind's default,
    /// with both values; empty when none is, or when the saved rule no longer reads as a rule
    /// (`plan_matches` is then false). Tightened limits are not listed.
    pub loosened: Vec<LoosenedLimit>,
    /// False when a cancel stopped the battery's luck test, so its luck line is inconclusive.
    pub complete: bool,
    pub seed: u64,
    /// How many batteries have been saved for this strategy on this instrument, this one included.
    /// An instrument a cancel stopped before the luck test saved none and is not counted. Keyed by
    /// strategy name: renaming a strategy starts the count again.
    pub runs: usize,
    /// How many distinct versions of the strategy's source file have been through the battery on
    /// this instrument, this one included; batteries saved before code hashes were recorded count
    /// together as one version. Keyed by strategy name like `runs`.
    pub code_versions: usize,
    /// The reference instruments the battery's runs named; left out when none. A verdict with
    /// references and one without share a row, the newer winning as above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<InstrumentId>,
}

#[derive(Debug, Deserialize)]
pub(super) struct VerdictsQuery {
    strategy: Option<String>,
}

/// The seed of a battery: fixed per strategy and instrument, so running the plan again on the same
/// data and engine gives the same verdict, and running it again cannot draw a luckier luck test.
/// FNV-1a over `strategy|venue:symbol:kind`, kept to its low 53 bits so it stays exact as a
/// JavaScript number in the GUI.
fn plan_seed(strategy: &str, instrument: &InstrumentId) -> u64 {
    let hash = format!("{strategy}|{instrument}")
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h: u64, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    hash & ((1 << 53) - 1)
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
    let references = request.references.clone().unwrap_or_default();

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

    // The store must hold each instrument's period, and the period must end by the data cutoff;
    // nothing is downloaded.
    let (store, checked) = (state.store.clone(), entry.clone());
    let (to_check, cutoff) = (instruments.clone(), state.data_cutoff.clone());
    let named = references.clone();
    tokio::task::spawn_blocking(move || {
        to_check.into_iter().try_for_each(|i| {
            let mut config = checked.plan.backtest_config(checked.name, i);
            config.references = named.clone();
            check_run_data(&store, &config, cutoff.as_ref())
        })
    })
    .await
    .map_err(|e| ApiError::internal(format!("the data check stopped: {e}")))??;

    let (store, runs_dir) = (state.store.clone(), state.runs_dir());
    let job_request = JobRequest::Plan(PlanRequest {
        strategy: request.strategy,
        instruments: Some(ids),
        references: request.references,
    });
    let cutoff = state.data_cutoff.clone();
    let job = state.jobs.submit(job_request, move |ctx| {
        execute(
            &entry,
            &instruments,
            &references,
            &store,
            cutoff.as_ref(),
            &runs_dir,
            ctx,
        )
    })?;
    Ok((StatusCode::ACCEPTED, Json(JobCreated { job_id: job.id })))
}

/// The job: loads every instrument's data, then runs and saves one battery after another.
fn execute(
    entry: &StrategyEntry,
    instruments: &[Instrument],
    references: &[InstrumentId],
    store: &Store,
    cutoff: Option<&DataCutoff>,
    runs_dir: &Path,
    ctx: &JobCtx,
) -> Result<JobResult, JobError> {
    ctx.checkpoint()?;
    ctx.progress(0.0, "loading the data");
    let mut loaded: Vec<RunData> = Vec::with_capacity(instruments.len());
    let mut totals = Vec::with_capacity(instruments.len());
    for instrument in instruments {
        let mut config = entry.plan.backtest_config(entry.name, instrument.clone());
        config.references = references.to_vec();
        let data = load_run_data(store, &config, cutoff).map_err(changed_since_accepted)?;
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
        // The asks are empty for a kind without quotes; for one with quotes every run of the
        // battery trades on them.
        let outcome = own_pool(|| {
            td_engine::run_battery_with_references(
                &spec,
                &data.bars,
                &data.asks,
                &data.funding,
                &data.references,
                Some(&progress),
            )
        });
        let battery = match outcome {
            Ok(Ok(battery)) => battery,
            Ok(Err(TesterError::Cancelled)) => return Err(cancelled(done)),
            Ok(Err(e)) => {
                let e = tester_error(e);
                let e = ApiError::new(e.status, &e.body.code, format!("{id}: {}", e.body.message));
                return Err(failed(done, e.into()));
            }
            Err(e) => return Err(failed(done, e)),
        };
        match save_plan_run(entry, instrument, battery, data, cutoff, runs_dir) {
            Ok(saved) => done.instruments.push(saved),
            Err(e) => return Err(failed(done, e)),
        }
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

/// How a failed plan job ends: with the instruments it saved before the error, if any.
fn failed(done: PlanDone, e: JobError) -> JobError {
    match e {
        JobError::Failed(error) if !done.instruments.is_empty() => {
            JobError::FailedWith(error, Box::new(JobResult::Plan(done)))
        }
        e => e,
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
    cutoff: Option<&DataCutoff>,
    runs_dir: &Path,
) -> Result<PlanInstrumentDone, JobError> {
    let run_record = RunRecord::new(entry.name, cutoff);
    let record = VerdictRecord {
        strategy: entry.name.to_string(),
        instrument: instrument.id.clone(),
        seed: battery.seed,
        engine_revision: ENGINE_REVISION,
        verdict: battery.verdict.clone(),
        plan: to_json(&entry.plan)?,
        rule: to_json(&entry.rule)?,
        // Only a cancel during the luck test leaves a battery incomplete.
        complete: !battery.luck_cancelled,
        code_hash: run_record.code_hash.clone(),
        references: data
            .references
            .iter()
            .map(|r| r.instrument.clone())
            .collect(),
    };
    let run = PlanRun::with_references(
        &battery_spec(entry, instrument),
        battery,
        &data.bars,
        &data.asks,
        &data.funding,
        &data.references,
    )
    .map_err(|e| ApiError::internal(e.to_string()))?;
    let record_json = serde_json::to_vec_pretty(&record)
        .map_err(|e| ApiError::internal(format!("cannot write the verdict record: {e}")))?;
    let saved = SavedRun::Plan(Box::new(run));
    let run_id = save_run(
        &saved,
        &data.report,
        &run_record,
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

fn to_json<T: Serialize>(value: &T) -> Result<Value, JobError> {
    serde_json::to_value(value)
        .map_err(|e| ApiError::internal(format!("cannot write the verdict record: {e}")).into())
}

/// A plan run's `config.json`, enough to tell it from the other kinds' without reading the result.
#[derive(Deserialize)]
#[allow(dead_code)] // read only to check that the fields are there
struct PlanConfigHead {
    plan: serde::de::IgnoredAny,
    rule: serde::de::IgnoredAny,
}

/// The beginning of a plan run's `result.json` (a [`SavedRun::Plan`]): the fields of a
/// [`VerdictRecord`], with the plan and the rule as JSON for the same reason as there.
#[derive(Deserialize)]
struct PlanResultHead {
    kind: RunKind,
    result: PlanResultFields,
}

#[derive(Deserialize)]
struct PlanResultFields {
    strategy: String,
    config: ConfigHead,
    plan: Value,
    rule: Value,
    engine_revision: u32,
    battery: BatteryHead,
}

#[derive(Deserialize)]
struct ConfigHead {
    instrument: InstrumentHead,
    #[serde(default)]
    references: Vec<InstrumentId>,
}

#[derive(Deserialize)]
struct InstrumentHead {
    id: InstrumentId,
}

#[derive(Deserialize)]
struct BatteryHead {
    verdict: Verdict,
    seed: u64,
    #[serde(default)]
    luck_cancelled: bool,
}

/// The verdict record of the run in `dir`, if it is a plan run. A run saved without a
/// `verdict.json` (by another tool), or whose `verdict.json` does not parse (damaged, or written
/// before a field existed), is read from its `result.json`, complete unless its battery says a
/// cancel stopped the luck test (a result saved before that flag existed reads as complete).
/// `None` for other kinds and for a run that cannot be read, so one damaged run does not hide the
/// rest.
fn read_record(dir: &Path) -> Option<VerdictRecord> {
    match fs::read(dir.join(VERDICT_FILE)) {
        Ok(bytes) => {
            if let Ok(record) = serde_json::from_slice(&bytes) {
                return Some(record);
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    let config = fs::read(dir.join("config.json")).ok()?;
    serde_json::from_slice::<PlanConfigHead>(&config).ok()?;
    let result = fs::read(dir.join("result.json")).ok()?;
    let head: PlanResultHead = serde_json::from_slice(&result).ok()?;
    if head.kind != RunKind::Plan {
        return None;
    }
    let run = head.result;
    Some(VerdictRecord {
        strategy: run.strategy,
        instrument: run.config.instrument.id,
        seed: run.battery.seed,
        engine_revision: run.engine_revision,
        verdict: run.battery.verdict,
        plan: run.plan,
        rule: run.rule,
        complete: !run.battery.luck_cancelled,
        // The result does not hold the hash; record.json beside it does when it was saved with one.
        code_hash: fs::read(dir.join(RECORD_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RunRecord>(&bytes).ok())
            .and_then(|r| r.code_hash),
        references: run.config.references,
    })
}

/// The part of a plan, as JSON, that the battery on `instrument` reads: the plan with its
/// per-instrument fields, `instruments` and `backtest.slippage`, cut down to `instrument`'s
/// entry, and without `out_of_sample`, the single split that only plan validation reads. A
/// battery reads no other instrument's entry, and the seed is the strategy's and the
/// instrument's, so adding, removing or reordering another instrument, changing another
/// instrument's slippage or changing the split leaves this instrument's verdict current. A field that is missing or not
/// shaped as expected is left as it is, so the plan compares unequal to the registered one.
fn instrument_view(mut plan: Value, instrument: &Value) -> Value {
    if let Some(fields) = plan.as_object_mut() {
        fields.remove("out_of_sample");
        if let Some(instruments) = fields.get_mut("instruments") {
            *instruments = Value::Array(vec![instrument.clone()]);
        }
        if let Some(Value::Array(slippage)) = fields
            .get_mut("backtest")
            .and_then(|b| b.get_mut("slippage"))
        {
            slippage.retain(|o| o.get("instrument") == Some(instrument));
        }
    }
    plan
}

/// Whether a battery of `strategy` on `instrument`, saved with `rule` and the plan whose
/// [`instrument_view`] is `saved_view`, ran under `entry`'s current plan and rule, as far as they
/// concern that instrument, and the plan still tests it.
fn ran_under(
    entry: &StrategyEntry,
    strategy: &str,
    instrument: &InstrumentId,
    rule: &Value,
    saved_view: &Value,
) -> bool {
    entry.name == strategy
        && entry.plan.instruments.contains(instrument)
        && same_plan(entry, instrument, rule, saved_view)
}

/// Whether `rule` and the plan whose [`instrument_view`] on `instrument` is `saved_view` are
/// `entry`'s current rule and plan, as far as they concern that instrument. JSON numbers compare
/// exactly: serde_json's `float_roundtrip` reads each saved float back to the value it was
/// written from.
fn same_plan(
    entry: &StrategyEntry,
    instrument: &InstrumentId,
    rule: &Value,
    saved_view: &Value,
) -> bool {
    let (Ok(plan), Ok(current_rule), Ok(id)) = (
        serde_json::to_value(&entry.plan),
        serde_json::to_value(entry.rule),
        serde_json::to_value(instrument),
    ) else {
        return false;
    };
    &current_rule == rule && &instrument_view(plan, &id) == saved_view
}

/// The latest verdict per strategy and instrument (the newest complete one, or the newest when none
/// is complete), with how many batteries and code versions each pair has saved, ordered by strategy
/// and then instrument. Only `strategy`'s when it is given. Each verdict's plan and rule are
/// compared with those of the registered `strategies`, and its code hash with the registered
/// strategy's ([`td_engine::code_hash`]).
fn latest_verdicts(
    runs_dir: &Path,
    strategies: &[StrategyEntry],
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
    // Each pair's code hashes; `None` stands for every battery saved without one.
    let mut versions: BTreeMap<(String, String), BTreeSet<Option<String>>> = BTreeMap::new();
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
        let Some(mut record) = read_record(&entry.path()) else {
            continue;
        };
        if strategy.is_some_and(|s| s != record.strategy) {
            continue;
        }
        let key = (record.strategy.clone(), record.instrument.to_string());
        let runs = latest.get(&key).map_or(0, |s| s.runs) + 1;
        let saved_view = serde_json::to_value(&record.instrument)
            .ok()
            .map(|id| instrument_view(record.plan.take(), &id));
        let plan_matches = saved_view.is_some_and(|view| {
            strategies
                .iter()
                .any(|e| ran_under(e, &record.strategy, &record.instrument, &record.rule, &view))
        });
        let engine_matches = record.engine_revision == ENGINE_REVISION;
        let code_matches = code_matches(&record.strategy, record.code_hash.as_deref());
        let loosened = serde_json::from_value::<VerdictRule>(record.rule.clone())
            .map(|rule| rule.loosened())
            .unwrap_or_default();
        versions
            .entry(key.clone())
            .or_default()
            .insert(record.code_hash.clone());
        let row = VerdictSummary {
            strategy: record.strategy,
            instrument: record.instrument,
            verdict: record.verdict,
            created_at: id.created_at(),
            run_id: id,
            engine_revision: record.engine_revision,
            engine_matches,
            plan_matches,
            code_matches,
            stale: !(engine_matches && plan_matches && code_matches == Some(true)),
            loosened,
            complete: record.complete,
            seed: record.seed,
            runs,
            code_versions: 0,
            references: record.references,
        };
        // A complete run beats an incomplete one; between two of the same, the newer wins.
        match latest.get_mut(&key) {
            Some(seen) if (seen.complete, &seen.run_id) > (row.complete, &row.run_id) => {
                seen.runs = runs;
            }
            _ => {
                latest.insert(key, row);
            }
        }
    }
    Ok(latest
        .into_iter()
        .map(|(key, mut row)| {
            row.code_versions = versions.get(&key).map_or(0, BTreeSet::len);
            row
        })
        .collect())
}

/// What a session was started from: the strategy's code, plan and rule when it started, and the
/// verdict of the strategy on the session's instrument it was started from. Fixed when the session
/// starts and saved with its config; [`LinkState`] shows it with its checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SessionLink {
    /// The hash of the strategy's source file when the session started
    /// ([`td_engine::code_hash`]); `null` for a strategy without one.
    pub code_hash: Option<String>,
    /// The strategy's test plan ([`td_engine::TestPlan`]) when the session started, as JSON, so a
    /// session saved before a field of it changed still reads.
    #[ts(type = "unknown")]
    pub plan: Value,
    /// The strategy's verdict rule ([`td_engine::VerdictRule`]) when the session started, as JSON
    /// like `plan`.
    #[ts(type = "unknown")]
    pub rule: Value,
    /// The verdict the session was started from: the one named when it was started, or else the
    /// strategy's latest verdict on the instrument as `GET /verdicts` gives it. `null` when the
    /// strategy had none there.
    pub verdict: Option<LinkedVerdict>,
}

/// The verdict a session was started from, with what its checks compare.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct LinkedVerdict {
    /// The saved plan run, at `GET /runs/{run_id}`.
    pub run_id: RunId,
    pub state: VerdictState,
    /// False when a cancel stopped the battery's luck test.
    pub complete: bool,
    /// The engine revision that produced the verdict.
    pub engine_revision: u32,
    /// The hash of the strategy's source file the battery ran with; `null` for a battery saved
    /// before code hashes were recorded.
    pub code_hash: Option<String>,
    /// The plan the battery ran under, as its verdict record holds it.
    #[ts(type = "unknown")]
    pub plan: Value,
    /// The rule the battery ran under, as its verdict record holds it.
    #[ts(type = "unknown")]
    pub rule: Value,
}

/// A session's [`SessionLink`] with its checks against the strategy as it is now, made when the
/// session's state is read. With a verdict, each check compares the verdict with this engine and
/// the strategy now, and is the one `GET /verdicts` makes for that verdict. Without one, the plan
/// and code checks compare what the session started with (the link's `plan`, `rule` and
/// `code_hash`) with the strategy now, and there is no engine check. A check that cannot be made is
/// `null`, and counts as not matching for `stale`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct LinkState {
    #[serde(flatten)]
    pub link: SessionLink,
    /// With a verdict, whether this engine has the verdict's engine revision; `null` without one.
    pub engine_matches: Option<bool>,
    /// With a verdict, whether the strategy's plan and rule are still the ones the verdict was
    /// judged under, as far as they concern the session's instrument, and the plan still tests
    /// it; false when the strategy is no longer registered. Without one, whether they are still
    /// the ones the session started with, as far as they concern the session's instrument; `null`
    /// when the strategy is no longer registered.
    pub plan_matches: Option<bool>,
    /// With a verdict, whether the strategy's source file is still the one the verdict was judged
    /// with; without one, whether it is still the one the session started with. `null` when that
    /// is unknown: no hash was saved, or the strategy is no longer registered. The session replays
    /// and trades with the current file, so this compares the code it runs now. The hash covers
    /// the strategy's own file only, not shared helpers it calls.
    pub code_matches: Option<bool>,
    /// True unless each check made is true: all three with a verdict, `plan_matches` and
    /// `code_matches` without one. With a verdict: the code, plan or engine the session runs on
    /// may no longer be the one that verdict judged. Without one: the code or plan may have
    /// changed since the session started.
    pub stale: bool,
}

impl LinkState {
    /// `link`'s checks for a session of `strategy` on `instrument`, against `strategies`, the
    /// server's registered strategies that `GET /verdicts` also reads.
    pub fn of(
        link: &SessionLink,
        strategy: &str,
        instrument: &InstrumentId,
        strategies: &[StrategyEntry],
    ) -> LinkState {
        let entry = strategies.iter().find(|e| e.name == strategy);
        let view = |plan: &Value| {
            serde_json::to_value(instrument)
                .ok()
                .map(|id| instrument_view(plan.clone(), &id))
        };
        let Some(verdict) = &link.verdict else {
            let plan_matches = entry.map(|e| {
                view(&link.plan).is_some_and(|v| same_plan(e, instrument, &link.rule, &v))
            });
            let code_matches = code_matches(strategy, link.code_hash.as_deref());
            return LinkState {
                link: link.clone(),
                engine_matches: None,
                plan_matches,
                code_matches,
                stale: !(plan_matches == Some(true) && code_matches == Some(true)),
            };
        };
        let engine_matches = verdict.engine_revision == ENGINE_REVISION;
        let plan_matches = view(&verdict.plan).is_some_and(|v| {
            entry.is_some_and(|e| ran_under(e, strategy, instrument, &verdict.rule, &v))
        });
        let code_matches = code_matches(strategy, verdict.code_hash.as_deref());
        LinkState {
            link: link.clone(),
            engine_matches: Some(engine_matches),
            plan_matches: Some(plan_matches),
            code_matches,
            stale: !(engine_matches && plan_matches && code_matches == Some(true)),
        }
    }
}

/// The link of a session of `entry` on `instrument` started now, from the verdict saved as run
/// `verdict`, or from the strategy's latest verdict on the instrument when it is `None`. 400
/// `invalid_verdict` for a run that is not a saved verdict of this strategy on this instrument, or
/// one judged with reference instruments, which a paper session cannot feed its strategy: it would
/// be a different test from the one the verdict judged. When the latest verdict is such a one, the
/// session is linked to no verdict.
pub(crate) fn session_link(
    runs_dir: &Path,
    entry: &StrategyEntry,
    instrument: &InstrumentId,
    verdict: Option<&RunId>,
) -> Result<SessionLink, ApiError> {
    let bad = |message: String| invalid("invalid_verdict", message);
    let record = match verdict {
        Some(id) => {
            // A run id from the request has not had its format checked, and it names a path.
            let checked =
                RunId::parse(id.as_str()).map_err(|_| bad(format!("{id:?} is not a run id")))?;
            let record = read_record(&runs_dir.join(checked.as_str()))
                .ok_or_else(|| bad(format!("run {id} is not a saved verdict")))?;
            if record.strategy != entry.name || record.instrument != *instrument {
                return Err(bad(format!(
                    "run {id} is the verdict of {} on {}, not of {} on {instrument}",
                    record.strategy, record.instrument, entry.name
                )));
            }
            if !record.references.is_empty() {
                return Err(bad(paper_cannot_feed(id, &record.references)));
            }
            Some((checked, record))
        }
        None => latest_verdicts(runs_dir, std::slice::from_ref(entry), Some(entry.name))?
            .into_iter()
            .find(|row| row.instrument == *instrument)
            .and_then(|row| {
                let record = read_record(&runs_dir.join(row.run_id.as_str()))?;
                Some((row.run_id, record))
            })
            .filter(|(_, record)| record.references.is_empty()),
    };
    let json = |value: Result<Value, serde_json::Error>| {
        value.map_err(|e| ApiError::internal(format!("cannot record the session's link: {e}")))
    };
    Ok(SessionLink {
        code_hash: td_engine::code_hash(entry.name).map(String::from),
        plan: json(serde_json::to_value(&entry.plan))?,
        rule: json(serde_json::to_value(entry.rule))?,
        verdict: record.map(|(run_id, record)| LinkedVerdict {
            run_id,
            state: record.verdict.state,
            complete: record.complete,
            engine_revision: record.engine_revision,
            code_hash: record.code_hash,
            plan: record.plan,
            rule: record.rule,
        }),
    })
}

/// Why a paper session cannot be linked to verdict `id`, judged with `references`.
///
/// shortcut: a paper session feeds no reference instruments, so it cannot run the test a verdict
/// with references judged; feed their live bars beside the session's own when a strategy that
/// reads them is paper traded.
fn paper_cannot_feed(id: &RunId, references: &[InstrumentId]) -> String {
    let names: Vec<String> = references.iter().map(ToString::to_string).collect();
    format!(
        "run {id} was judged with the reference instruments {}, and a paper session cannot feed reference instruments yet",
        names.join(", ")
    )
}

/// `GET /verdicts?strategy=`.
pub(super) async fn verdicts(
    State(state): State<Arc<AppState>>,
    query: Result<Query<VerdictsQuery>, QueryRejection>,
) -> Result<Json<Vec<VerdictSummary>>, ApiError> {
    let Query(VerdictsQuery { strategy }) = query?;
    let (runs_dir, strategies) = (state.runs_dir(), state.strategies.clone());
    let rows = tokio::task::spawn_blocking(move || {
        latest_verdicts(&runs_dir, &strategies, strategy.as_deref())
    })
    .await
    .map_err(|e| ApiError::internal(format!("the verdict list stopped: {e}")))??;
    Ok(Json(rows))
}

#[cfg(test)]
mod tests {
    use td_core::{InstrumentKind, Venue};

    use super::*;

    fn id(symbol: &str, kind: InstrumentKind) -> InstrumentId {
        InstrumentId {
            venue: Venue::Binance,
            symbol: symbol.into(),
            kind,
        }
    }

    /// A verdict of `entry` on `instrument` as it is now, saved as run `run_id` under `runs_dir`.
    fn save_verdict(runs_dir: &Path, run_id: &str, entry: &StrategyEntry, on: &InstrumentId) {
        save_verdict_with(runs_dir, run_id, entry, on, Vec::new());
    }

    /// [`save_verdict`] for a battery whose runs named `references`.
    fn save_verdict_with(
        runs_dir: &Path,
        run_id: &str,
        entry: &StrategyEntry,
        on: &InstrumentId,
        references: Vec<InstrumentId>,
    ) {
        let record = VerdictRecord {
            strategy: entry.name.into(),
            instrument: on.clone(),
            seed: 1,
            engine_revision: ENGINE_REVISION,
            verdict: Verdict::new(on.clone(), Vec::new()),
            plan: serde_json::to_value(&entry.plan).unwrap(),
            rule: serde_json::to_value(entry.rule).unwrap(),
            complete: true,
            code_hash: td_engine::code_hash(entry.name).map(String::from),
            references,
        };
        let dir = runs_dir.join(run_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(VERDICT_FILE), serde_json::to_vec(&record).unwrap()).unwrap();
    }

    #[test]
    fn a_session_is_linked_to_the_verdict_it_names_or_else_the_latest() {
        let dir = tempfile::tempdir().unwrap();
        let entry = td_engine::find_strategy("sma_cross").unwrap();
        let on = entry.plan.instruments[0].clone();
        let (older, newer) = ("1700000000000-0000000a", "1700000000001-0000000b");
        save_verdict(dir.path(), older, &entry, &on);
        save_verdict(dir.path(), newer, &entry, &on);

        let latest = session_link(dir.path(), &entry, &on, None).unwrap();
        assert_eq!(
            latest.code_hash.as_deref(),
            td_engine::code_hash("sma_cross")
        );
        assert_eq!(latest.plan, serde_json::to_value(&entry.plan).unwrap());
        assert_eq!(latest.verdict.as_ref().unwrap().run_id.as_str(), newer);

        let named = RunId::parse(older).unwrap();
        let link = session_link(dir.path(), &entry, &on, Some(&named)).unwrap();
        assert_eq!(link.verdict.unwrap().run_id, named);

        // Another instrument has no verdict to link to.
        let other = id("NOPEUSDT", InstrumentKind::Spot);
        let none = session_link(dir.path(), &entry, &other, None).unwrap();
        assert_eq!(none.verdict, None);
        // A verdict of another instrument, a run that is not a verdict and an id that is not one
        // are refused.
        let refused = |run: &str| {
            let run: RunId = serde_json::from_value(Value::String(run.into())).unwrap();
            session_link(dir.path(), &entry, &other, Some(&run)).unwrap_err()
        };
        for run in [newer, "1700000000002-0000000c", "../../etc"] {
            assert_eq!(refused(run).body.code, "invalid_verdict", "{run}");
        }
    }

    /// A paper session feeds no reference instruments, so it is never linked to a verdict judged
    /// with them: one named is refused, and a latest one leaves the session without a verdict.
    #[test]
    fn a_session_is_never_linked_to_a_verdict_judged_with_reference_instruments() {
        let dir = tempfile::tempdir().unwrap();
        let entry = td_engine::find_strategy("sma_cross").unwrap();
        let on = entry.plan.instruments[0].clone();
        let reference = InstrumentId {
            symbol: "OTHERUSDT".into(),
            ..on.clone()
        };
        let (older, newer) = ("1700000000000-0000000a", "1700000000001-0000000b");
        save_verdict(dir.path(), older, &entry, &on);
        save_verdict_with(dir.path(), newer, &entry, &on, vec![reference.clone()]);

        let named = RunId::parse(newer).unwrap();
        let refused = session_link(dir.path(), &entry, &on, Some(&named)).unwrap_err();
        assert_eq!(refused.body.code, "invalid_verdict");
        assert!(
            refused
                .body
                .message
                .contains("cannot feed reference instruments"),
            "{}",
            refused.body.message
        );
        assert_eq!(
            session_link(dir.path(), &entry, &on, None).unwrap().verdict,
            None
        );
        // A verdict judged without them still links.
        let plain = RunId::parse(older).unwrap();
        let link = session_link(dir.path(), &entry, &on, Some(&plain)).unwrap();
        assert_eq!(link.verdict.unwrap().run_id, plain);

        // The verdict list says which references the latest verdict was judged with.
        let rows = latest_verdicts(dir.path(), std::slice::from_ref(&entry), None).unwrap();
        assert_eq!(rows[0].references, vec![reference]);
        let row = serde_json::to_value(&rows[0]).unwrap();
        assert!(row.get("references").is_some());
    }

    #[test]
    fn a_session_link_goes_stale_when_the_code_plan_or_engine_no_longer_matches_its_verdict() {
        let entry = td_engine::find_strategy("sma_cross").unwrap();
        let on = entry.plan.instruments[0].clone();
        let current = LinkedVerdict {
            run_id: RunId::parse("1700000000000-0000000a").unwrap(),
            state: VerdictState::Trade,
            complete: true,
            engine_revision: ENGINE_REVISION,
            code_hash: td_engine::code_hash("sma_cross").map(String::from),
            plan: serde_json::to_value(&entry.plan).unwrap(),
            rule: serde_json::to_value(entry.rule).unwrap(),
        };
        let link = |verdict: Option<LinkedVerdict>| SessionLink {
            code_hash: td_engine::code_hash("sma_cross").map(String::from),
            plan: current.plan.clone(),
            rule: current.rule.clone(),
            verdict,
        };
        let registered = std::slice::from_ref(&entry);
        let check = |verdict: LinkedVerdict| {
            LinkState::of(&link(Some(verdict)), "sma_cross", &on, registered)
        };

        let fresh = check(current.clone());
        let flags = (fresh.engine_matches, fresh.plan_matches, fresh.code_matches);
        assert_eq!(flags, (Some(true), Some(true), Some(true)));
        assert!(!fresh.stale);

        let edited = check(LinkedVerdict {
            code_hash: Some("0000000000000000".into()),
            ..current.clone()
        });
        assert_eq!((edited.code_matches, edited.stale), (Some(false), true));

        let mut plan = current.plan.clone();
        plan["period"]["from"] = (entry.plan.period.from - 86_400_000).into();
        let replanned = check(LinkedVerdict {
            plan,
            ..current.clone()
        });
        assert_eq!(
            (replanned.plan_matches, replanned.stale),
            (Some(false), true)
        );

        let old_engine = check(LinkedVerdict {
            engine_revision: ENGINE_REVISION - 1,
            ..current.clone()
        });
        assert_eq!(
            (old_engine.engine_matches, old_engine.stale),
            (Some(false), true)
        );

        let unhashed = check(LinkedVerdict {
            code_hash: None,
            ..current.clone()
        });
        assert_eq!((unhashed.code_matches, unhashed.stale), (None, true));

        // A strategy the server no longer registers fails the plan check, as on `GET /verdicts`.
        let unregistered = LinkState::of(&link(Some(current.clone())), "sma_cross", &on, &[]);
        assert_eq!(
            (unregistered.plan_matches, unregistered.stale),
            (Some(false), true)
        );
    }

    #[test]
    fn a_session_link_without_a_verdict_goes_stale_when_the_code_or_plan_changes_after_it_starts() {
        let entry = td_engine::find_strategy("sma_cross").unwrap();
        let registered = std::slice::from_ref(&entry);
        // An instrument the plan does not test, so it has no verdict.
        let on = id("NOPEUSDT", InstrumentKind::Spot);
        let started = SessionLink {
            code_hash: td_engine::code_hash("sma_cross").map(String::from),
            plan: serde_json::to_value(&entry.plan).unwrap(),
            rule: serde_json::to_value(entry.rule).unwrap(),
            verdict: None,
        };
        let checks = |state: &LinkState| {
            let flags = (state.engine_matches, state.plan_matches, state.code_matches);
            (flags, state.stale)
        };

        let unchanged = LinkState::of(&started, "sma_cross", &on, registered);
        assert_eq!(checks(&unchanged), ((None, Some(true), Some(true)), false));

        let edited = SessionLink {
            code_hash: Some("0000000000000000".into()),
            ..started.clone()
        };
        let edited = LinkState::of(&edited, "sma_cross", &on, registered);
        assert_eq!(checks(&edited), ((None, Some(true), Some(false)), true));

        let mut plan = started.plan.clone();
        plan["period"]["from"] = (entry.plan.period.from - 86_400_000).into();
        let replanned = SessionLink {
            plan,
            ..started.clone()
        };
        let replanned = LinkState::of(&replanned, "sma_cross", &on, registered);
        assert_eq!(checks(&replanned), ((None, Some(false), Some(true)), true));

        // When the server no longer registers the strategy its plan cannot be checked: `null`,
        // and stale, not "not stale".
        let unregistered = LinkState::of(&started, "sma_cross", &on, &[]);
        assert_eq!(checks(&unregistered), ((None, None, Some(true)), true));
    }

    #[test]
    fn the_seed_is_stable_and_exact_as_a_javascript_number() {
        let spot = id("BTCUSDT", InstrumentKind::Spot);
        let cases = [
            plan_seed("sma_cross", &spot),
            plan_seed("sma_cross", &id("BTCUSDT", InstrumentKind::Perpetual)),
            plan_seed("donchian_breakout", &id("ETHUSDT", InstrumentKind::Spot)),
        ];
        // Each full 64-bit hash here is above 2^53; the seed keeps its low 53 bits.
        assert!(cases.iter().all(|&s| s < 1 << 53), "{cases:?}");
        assert_eq!(cases[0], 1_706_792_435_730_898);
        assert_eq!(plan_seed("sma_cross", &spot), cases[0]);
        assert_ne!(cases[0], cases[1]);
    }
}
