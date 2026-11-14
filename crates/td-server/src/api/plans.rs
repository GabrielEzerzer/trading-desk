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
//!
//! The verdict records, the verdict list and the link live in `td_engine`, which reads them from
//! the runs folder, so the `td-live` command links its sessions the same way without the server;
//! this module re-exports their types and maps their errors to HTTP ones.

use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use td_core::{Instrument, InstrumentId};
use td_data::Store;
use td_engine::{
    BatterySpec, ENGINE_REVISION, PlanRun, RunId, SavedRun, StrategyEntry, TesterError,
    VERDICT_FILE, Verdict, VerdictError, VerdictRecord,
};
pub use td_engine::{LinkState, LinkedVerdict, SessionLink, VerdictSummary};
use ts_rs::TS;

use super::AppState;
use super::data::instrument_list;
use super::error::ApiError;
use super::jobs::{JobCreated, JobCtx, JobError, JobRequest, JobResult};
use super::runs::{
    DataCutoff, RunData, RunRecord, changed_since_accepted, check_run_data, load_run_data,
    own_pool, save_run, tester_error,
};

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

/// A verdict error as an HTTP one: 400 `invalid_verdict` for a verdict that cannot start a
/// session, 500 for a runs folder that cannot be read.
fn verdict_error(e: VerdictError) -> ApiError {
    match e {
        VerdictError::Invalid(message) => invalid("invalid_verdict", message),
        VerdictError::Unreadable(message) => ApiError::internal(message),
    }
}

/// [`td_engine::latest_verdicts`] with its error as an HTTP one.
fn latest_verdicts(
    runs_dir: &Path,
    strategies: &[StrategyEntry],
    strategy: Option<&str>,
) -> Result<Vec<VerdictSummary>, ApiError> {
    td_engine::latest_verdicts(runs_dir, strategies, strategy).map_err(verdict_error)
}

/// The link of a paper session that feeds its strategy `references` ([`td_engine::session_link`]):
/// 400 `invalid_verdict` for a run that is not a saved verdict of this strategy on this instrument,
/// or one judged with other reference instruments, named or latest.
pub(crate) fn session_link(
    runs_dir: &Path,
    entry: &StrategyEntry,
    instrument: &InstrumentId,
    verdict: Option<&RunId>,
    references: &[InstrumentId],
) -> Result<SessionLink, ApiError> {
    td_engine::session_link(
        runs_dir,
        entry,
        instrument,
        verdict.map(RunId::as_str),
        Some(references),
        "in `verdict_run_id`",
    )
    .map_err(verdict_error)
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
    use std::fs;

    use td_core::{InstrumentKind, Venue};
    use td_engine::VerdictState;

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

        let latest = session_link(dir.path(), &entry, &on, None, &[]).unwrap();
        assert_eq!(
            latest.code_hash.as_deref(),
            td_engine::code_hash("sma_cross")
        );
        assert_eq!(latest.plan, serde_json::to_value(&entry.plan).unwrap());
        assert_eq!(latest.verdict.as_ref().unwrap().run_id.as_str(), newer);

        let named = RunId::parse(older).unwrap();
        let link = session_link(dir.path(), &entry, &on, Some(&named), &[]).unwrap();
        assert_eq!(link.verdict.unwrap().run_id, named);

        // Another instrument has no verdict to link to.
        let other = id("NOPEUSDT", InstrumentKind::Spot);
        let none = session_link(dir.path(), &entry, &other, None, &[]).unwrap();
        assert_eq!(none.verdict, None);
        // A verdict of another instrument, a run that is not a verdict and an id that is not one
        // are refused.
        let refused = |run: &str| {
            let run: RunId = serde_json::from_value(Value::String(run.into())).unwrap();
            session_link(dir.path(), &entry, &other, Some(&run), &[]).unwrap_err()
        };
        for run in [newer, "1700000000002-0000000c", "../../etc"] {
            assert_eq!(refused(run).body.code, "invalid_verdict", "{run}");
        }
    }

    /// A session is linked to a verdict judged with reference instruments only when it feeds
    /// exactly those: one named is refused without them, with the missing instruments in the
    /// message, and so is a session left to the latest verdict when that is such a one, since it
    /// would run without the bars its strategy was judged on.
    #[test]
    fn a_session_is_linked_to_a_verdict_judged_with_references_only_when_it_feeds_them() {
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
        let refused = session_link(dir.path(), &entry, &on, Some(&named), &[]).unwrap_err();
        assert_eq!(refused.body.code, "invalid_verdict");
        assert!(
            refused
                .body
                .message
                .contains(&format!("the session does not feed {reference}")),
            "{}",
            refused.body.message
        );
        let latest = session_link(dir.path(), &entry, &on, None, &[]).unwrap_err();
        assert_eq!(latest.body.code, "invalid_verdict");
        assert!(
            latest.body.message.contains("latest verdict"),
            "{}",
            latest.body.message
        );
        // Fed the same references, both link.
        let fed = std::slice::from_ref(&reference);
        let link = session_link(dir.path(), &entry, &on, Some(&named), fed).unwrap();
        assert_eq!(link.verdict.unwrap().run_id, named);
        let link = session_link(dir.path(), &entry, &on, None, fed).unwrap();
        assert_eq!(link.verdict.unwrap().run_id, named);
        // A verdict judged without them links a session without them, and refuses one with them.
        let plain = RunId::parse(older).unwrap();
        let link = session_link(dir.path(), &entry, &on, Some(&plain), &[]).unwrap();
        assert_eq!(link.verdict.unwrap().run_id, plain);
        let extra = session_link(dir.path(), &entry, &on, Some(&plain), fed).unwrap_err();
        assert!(
            extra
                .body
                .message
                .contains(&format!("also feeds {reference}")),
            "{}",
            extra.body.message
        );

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
