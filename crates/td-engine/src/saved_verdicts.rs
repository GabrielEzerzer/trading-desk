//! Saved verdicts and the link of a session to the verdict it was started from (#115).
//!
//! A plan run (kind `plan`, a [`crate::PlanRun`]) is saved under the runs folder with a small
//! `verdict.json` ([`VerdictRecord`]) beside its result. [`latest_verdicts`] lists the latest
//! verdict per strategy and instrument from those records, with its checks against the strategy as
//! it is now: a verdict is current only when the engine revision, the plan and rule, and the
//! strategy's source file ([`crate::code_hash`]) are all still the ones it was judged under.
//!
//! A session, paper or live, is linked to the verdict it was started from: [`session_link`]
//! records, when the session starts, the hash of the strategy's source file, its plan and rule,
//! and that verdict, and [`LinkState`] checks it against the strategy as it is now whenever the
//! session's state is read: with the same checks the verdict list makes when it has a verdict,
//! and against what the session started with when it has none.
//!
//! Everything here reads files and nothing else. The server's `GET /verdicts` and paper sessions
//! use it, and so does the `td-live` command.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use td_core::{InstrumentId, Millis};

use crate::{
    ENGINE_REVISION, LoosenedLimit, RunId, RunKind, StrategyEntry, Verdict, VerdictRule,
    VerdictState,
};

/// The folder under the data folder that holds saved runs, one folder per [`RunId`].
pub const RUNS_DIR: &str = "runs";

/// The file beside a run's `result.json` that says what the run was made under; only its
/// `code_hash` is read here.
pub const RECORD_FILE: &str = "record.json";

/// Why saved verdicts could not be read, or a session not linked to one.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum VerdictError {
    /// The verdict named or found cannot start a session: the message says why.
    #[error("{0}")]
    Invalid(String),
    /// The runs folder could not be read, or the link could not be recorded.
    #[error("{0}")]
    Unreadable(String),
}

/// The part of a run's `record.json` read here.
#[derive(Deserialize)]
struct RecordHead {
    #[serde(default)]
    code_hash: Option<String>,
}

/// The file beside a plan run's `result.json` that `GET /verdicts` reads.
pub const VERDICT_FILE: &str = "verdict.json";

/// A plan run's `verdict.json`: what the verdict list needs, without the battery's runs. A record
/// that lacks a field (written before it existed) or does not parse is read from the run's
/// `result.json` instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerdictRecord {
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
pub fn read_record(dir: &Path) -> Option<VerdictRecord> {
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
            .and_then(|bytes| serde_json::from_slice::<RecordHead>(&bytes).ok())
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
pub fn latest_verdicts(
    runs_dir: &Path,
    strategies: &[StrategyEntry],
    strategy: Option<&str>,
) -> Result<Vec<VerdictSummary>, VerdictError> {
    let entries = match fs::read_dir(runs_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(VerdictError::Unreadable(format!(
                "{}: {e}",
                runs_dir.display()
            )));
        }
    };
    let mut latest: BTreeMap<(String, String), VerdictSummary> = BTreeMap::new();
    // Each pair's code hashes; `None` stands for every battery saved without one.
    let mut versions: BTreeMap<(String, String), BTreeSet<Option<String>>> = BTreeMap::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| VerdictError::Unreadable(format!("{}: {e}", runs_dir.display())))?;
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionLink {
    /// The hash of the strategy's source file when the session started
    /// ([`td_engine::code_hash`]); `null` for a strategy without one.
    pub code_hash: Option<String>,
    /// The strategy's test plan ([`td_engine::TestPlan`]) when the session started, as JSON, so a
    /// session saved before a field of it changed still reads.
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub plan: Value,
    /// The strategy's verdict rule ([`td_engine::VerdictRule`]) when the session started, as JSON
    /// like `plan`.
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub rule: Value,
    /// The verdict the session was started from: the one named when it was started, or else the
    /// strategy's latest verdict on the instrument as `GET /verdicts` gives it. `null` when the
    /// strategy had none there.
    pub verdict: Option<LinkedVerdict>,
}

/// The verdict a session was started from, with what its checks compare.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub plan: Value,
    /// The rule the battery ran under, as its verdict record holds it.
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub rule: Value,
}

/// A session's [`SessionLink`] with its checks against the strategy as it is now, made when the
/// session's state is read. With a verdict, each check compares the verdict with this engine and
/// the strategy now, and is the one `GET /verdicts` makes for that verdict. Without one, the plan
/// and code checks compare what the session started with (the link's `plan`, `rule` and
/// `code_hash`) with the strategy now, and there is no engine check. A check that cannot be made is
/// `null`, and counts as not matching for `stale`.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
/// `verdict`, or from the strategy's latest verdict on the instrument when it is `None`.
/// [`VerdictError::Invalid`] for a run that is not a saved verdict of this strategy on this
/// instrument, or one judged with reference instruments, which a session cannot feed its strategy:
/// it would be a different test from the one the verdict judged. That holds for the latest verdict
/// too, so a session of a strategy last judged with references does not start unless the caller
/// names a verdict judged without them; `name_one` says how, as in "name a verdict judged without
/// them {name_one}".
///
/// `verdict` is checked with [`RunId::parse`] before it is joined to `runs_dir`, since it names a
/// path and may come from outside.
pub fn session_link(
    runs_dir: &Path,
    entry: &StrategyEntry,
    instrument: &InstrumentId,
    verdict: Option<&str>,
    name_one: &str,
) -> Result<SessionLink, VerdictError> {
    let bad = VerdictError::Invalid;
    let record = match verdict {
        Some(id) => {
            let checked = RunId::parse(id).map_err(|_| bad(format!("{id:?} is not a run id")))?;
            let record = read_record(&runs_dir.join(checked.as_str()))
                .ok_or_else(|| bad(format!("run {id} is not a saved verdict")))?;
            if record.strategy != entry.name || record.instrument != *instrument {
                return Err(bad(format!(
                    "run {id} is the verdict of {} on {}, not of {} on {instrument}",
                    record.strategy, record.instrument, entry.name
                )));
            }
            if !record.references.is_empty() {
                return Err(bad(cannot_feed(&checked, &record.references)));
            }
            Some((checked, record))
        }
        None => {
            let latest = latest_verdicts(runs_dir, std::slice::from_ref(entry), Some(entry.name))?
                .into_iter()
                .find(|row| row.instrument == *instrument);
            match latest {
                // Judged from the row, so a record that cannot be read again does not let the
                // session start unlinked.
                Some(row) if !row.references.is_empty() => {
                    return Err(bad(format!(
                        "{}; it is the strategy's latest verdict on {instrument}, so name a verdict judged without them {name_one}, or run a test without them first",
                        cannot_feed(&row.run_id, &row.references)
                    )));
                }
                Some(row) => read_record(&runs_dir.join(row.run_id.as_str()))
                    .map(|record| (row.run_id, record)),
                None => None,
            }
        }
    };
    let json = |value: Result<Value, serde_json::Error>| {
        value
            .map_err(|e| VerdictError::Unreadable(format!("cannot record the session's link: {e}")))
    };
    Ok(SessionLink {
        code_hash: crate::code_hash(entry.name).map(String::from),
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

/// Why a session cannot be linked to verdict `id`, judged with `references`.
///
/// shortcut: a paper session feeds no reference instruments, so it cannot run the test a verdict
/// with references judged; feed their live bars beside the session's own when a strategy that
/// reads them is paper traded.
fn cannot_feed(id: &RunId, references: &[InstrumentId]) -> String {
    let names: Vec<String> = references.iter().map(ToString::to_string).collect();
    format!(
        "run {id} was judged with the reference instruments {}, and a paper session cannot feed reference instruments yet",
        names.join(", ")
    )
}

/// Whether a saved code hash is the strategy's code in this engine: `None` when the run saved no
/// hash (saved before hashes were recorded) or the strategy is no longer registered, so the answer
/// is unknown.
pub fn code_matches(strategy: &str, saved: Option<&str>) -> Option<bool> {
    saved
        .zip(crate::code_hash(strategy))
        .map(|(saved, now)| saved == now)
}
