//! Saved results: one directory per run under the runs directory.
//!
//! A run is stored as `<runs_dir>/<run_id>/config.json` (the backtest configuration or tester
//! spec alone, for reading by hand) and `result.json` (the whole [`SavedRun`]: its kind and the
//! result, which holds the configuration or spec, so any run can be run again). A plan run, one
//! instrument's test battery under a strategy's plan, saves the strategy, the instrument, the plan,
//! the rule and the seed as its `config.json`.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use td_core::{InstrumentId, Millis, Timeframe};

use crate::metrics::MetricsReport;
use crate::runner::hex;
use crate::{
    BacktestConfig, BacktestResult, BatteryResult, ENGINE_REVISION, EngineError, OosResult,
    SweepResult, TestPlan, VerdictRule, WalkForwardResult, config_fingerprint,
};

const CONFIG_FILE: &str = "config.json";
const RESULT_FILE: &str = "result.json";

/// Numbers each save's temporary directory within this process.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{}: {source}", path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid run id {0:?}")]
    InvalidRunId(String),

    #[error("a run with id {0} already exists")]
    RunExists(RunId),

    #[error(
        "{0:?} cannot be saved beside a run: it must be a plain file name other than {CONFIG_FILE} and {RESULT_FILE}"
    )]
    InvalidFileName(String),
}

/// A run's id: the save time in milliseconds since the epoch, zero-padded to 13 digits so ids
/// sort by time, then `-` and the first 8 hex digits of the SHA-256 of `result.json`.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
// No `#[serde(transparent)]`: serde already writes a one-field tuple struct as its field in
// JSON, and ts-rs cannot parse that attribute, so it would print a warning on every build.
pub struct RunId(String);

impl RunId {
    /// Checks the format, so an id from outside cannot name a path outside the runs directory.
    pub fn parse(s: &str) -> Result<RunId, StoreError> {
        let b = s.as_bytes();
        let ok = b.len() == 22
            && b[..13].iter().all(u8::is_ascii_digit)
            && b[13] == b'-'
            && b[14..]
                .iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c));
        if ok {
            Ok(RunId(s.to_string()))
        } else {
            Err(StoreError::InvalidRunId(s.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The save time encoded in the id.
    pub fn created_at(&self) -> Millis {
        self.0[..13].parse().expect("checked by RunId::parse")
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a saved run is.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Backtest,
    Sweep,
    Oos,
    WalkForward,
    /// One instrument's test battery under a strategy's plan.
    Plan,
}

/// A result as saved, tagged with its kind: `{"kind": "...", "result": {...}}`.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "result", rename_all = "snake_case")]
pub enum SavedRun {
    Backtest(Box<BacktestResult>),
    Sweep(Box<SweepResult>),
    Oos(Box<OosResult>),
    WalkForward(Box<WalkForwardResult>),
    Plan(Box<PlanRun>),
}

/// One instrument's test battery (#35) under a strategy's plan, as saved: what was tested, the
/// engine that tested it and everything the battery produced, its verdict and seed included.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanRun {
    pub strategy: String,
    /// The plan's backtest on the instrument ([`TestPlan::backtest_config`]): the period, the
    /// fixed parameters, the starting cash, the slippage and the instrument's trading rules.
    pub config: BacktestConfig,
    /// The plan and the rule as they were when the battery ran.
    pub plan: TestPlan,
    pub rule: VerdictRule,
    /// The td-engine crate version that produced the run.
    pub engine_version: String,
    /// The [`ENGINE_REVISION`] that produced the run. A verdict from another revision may differ
    /// from what this engine would give on the same data.
    pub engine_revision: u32,
    /// The fingerprint of the data the battery was given, over `config`'s range; see
    /// [`config_fingerprint`].
    pub data_fingerprint: String,
    pub battery: BatteryResult,
}

impl PlanRun {
    /// Wraps the battery's result of `spec` with what it was run on, stamped with this engine's
    /// revision. `bars`, `asks` and `funding` are the data the battery was given.
    pub fn new(
        spec: &crate::BatterySpec,
        battery: BatteryResult,
        bars: &[td_core::Candle],
        asks: &[td_core::Candle],
        funding: &[(Millis, f64)],
    ) -> Result<PlanRun, EngineError> {
        let config = spec
            .plan
            .backtest_config(spec.strategy, spec.instrument.clone());
        Ok(PlanRun {
            strategy: spec.strategy.to_string(),
            data_fingerprint: config_fingerprint(&config, bars, asks, funding)?,
            config,
            plan: spec.plan.clone(),
            rule: *spec.rule,
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            engine_revision: ENGINE_REVISION,
            battery,
        })
    }
}

/// A plan run's `config.json`: what was tested, without the results.
#[derive(Serialize)]
struct PlanConfig<'a> {
    strategy: &'a str,
    instrument: &'a td_core::Instrument,
    plan: &'a TestPlan,
    rule: &'a VerdictRule,
    seed: u64,
}

impl SavedRun {
    pub fn kind(&self) -> RunKind {
        match self {
            SavedRun::Backtest(_) => RunKind::Backtest,
            SavedRun::Sweep(_) => RunKind::Sweep,
            SavedRun::Oos(_) => RunKind::Oos,
            SavedRun::WalkForward(_) => RunKind::WalkForward,
            SavedRun::Plan(_) => RunKind::Plan,
        }
    }

    /// The backtest configuration the run is built on: the backtest's own, or the tester spec's
    /// base. Its range is the whole range the run used.
    pub fn base_config(&self) -> &BacktestConfig {
        match self {
            SavedRun::Backtest(r) => &r.config,
            SavedRun::Sweep(r) => &r.spec.base,
            SavedRun::Oos(r) => &r.spec.sweep.base,
            SavedRun::WalkForward(r) => &r.spec.sweep.base,
            SavedRun::Plan(r) => &r.config,
        }
    }

    /// The fingerprint of the data the run used: [`crate::config_fingerprint`] over
    /// [`SavedRun::base_config`] gives the same on the same data.
    pub fn data_fingerprint(&self) -> &str {
        match self {
            SavedRun::Backtest(r) => &r.data_fingerprint,
            SavedRun::Sweep(r) => &r.data_fingerprint,
            SavedRun::Oos(r) => &r.data_fingerprint,
            SavedRun::WalkForward(r) => &r.data_fingerprint,
            SavedRun::Plan(r) => &r.data_fingerprint,
        }
    }

    /// The [`crate::ENGINE_REVISION`] that produced the run, read from a backtest it holds; `None`
    /// for a sweep in which no run qualified, which holds none.
    pub fn engine_revision(&self) -> Option<u32> {
        match self {
            SavedRun::Backtest(r) => Some(r.engine_revision),
            SavedRun::Sweep(r) => r.best.as_ref().map(|b| b.engine_revision),
            SavedRun::Oos(r) => Some(r.out_of_sample.engine_revision),
            SavedRun::WalkForward(r) => r.windows.first().map(|w| w.out_of_sample.engine_revision),
            SavedRun::Plan(r) => Some(r.engine_revision),
        }
    }

    /// The run's row in the run list, as [`list`] would give it under `id`.
    pub fn summary(&self, id: RunId) -> RunSummary {
        let metrics = match self {
            SavedRun::Backtest(r) => Some(&r.metrics),
            SavedRun::Sweep(r) => r.best.as_ref().map(|b| &b.metrics),
            SavedRun::Oos(r) => Some(&r.out_of_sample.metrics),
            SavedRun::WalkForward(r) => Some(&r.metrics),
            SavedRun::Plan(r) => r.battery.walk_forward.as_ref().map(|w| &w.metrics),
        };
        summary_row(id, self.kind(), self.base_config(), metrics)
    }
}

/// One row of the run list. The strategy and range are the base configuration's. The metrics
/// are the backtest's, the sweep's best run's (none if no run qualified), the out-of-sample
/// segment's, or the stitched walk-forward curve's (for a plan run, its battery's walk-forward,
/// none when that did not run).
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: RunId,
    pub created_at: Millis,
    pub kind: RunKind,
    pub strategy: String,
    pub instrument: InstrumentId,
    pub timeframe: Timeframe,
    pub from: Millis,
    pub to: Millis,
    pub total_return: Option<f64>,
    pub cagr: Option<f64>,
    pub sharpe: Option<f64>,
    pub max_drawdown: Option<f64>,
    pub trades: usize,
}

/// Saves a result and returns its new id.
///
/// Both files are written into a temporary directory inside `runs_dir`, which is then renamed to
/// the run's id, so a run appears whole or not at all. `runs_dir` is created if needed. A run with
/// the same id already saved, by this call's time or by another save racing it, is
/// [`StoreError::RunExists`]; saving again in a later millisecond gives a new id.
pub fn save(run: &SavedRun, runs_dir: &Path) -> Result<RunId, StoreError> {
    save_with_files(run, runs_dir, &[])
}

/// [`save`], with `files` (name and contents) written into the run's directory in the same atomic
/// save, for what a caller keeps beside a run. Each name must be a plain file name other than the
/// run's own files.
pub fn save_with_files(
    run: &SavedRun,
    runs_dir: &Path,
    files: &[(&str, &[u8])],
) -> Result<RunId, StoreError> {
    for &(name, _) in files {
        let plain = Path::new(name).file_name().is_some_and(|n| n == name);
        // Case-insensitive, as Windows and macOS file systems are: `Result.json` would replace
        // the run's own `result.json`.
        let own = [CONFIG_FILE, RESULT_FILE]
            .iter()
            .any(|f| f.eq_ignore_ascii_case(name));
        if !plain || own {
            return Err(StoreError::InvalidFileName(name.to_string()));
        }
    }
    let config_json = match run {
        SavedRun::Backtest(r) => to_json(&r.config, runs_dir)?,
        SavedRun::Sweep(r) => to_json(&r.spec, runs_dir)?,
        SavedRun::Oos(r) => to_json(&r.spec, runs_dir)?,
        SavedRun::WalkForward(r) => to_json(&r.spec, runs_dir)?,
        SavedRun::Plan(r) => to_json(
            &PlanConfig {
                strategy: &r.strategy,
                instrument: &r.config.instrument,
                plan: &r.plan,
                rule: &r.rule,
                seed: r.battery.seed,
            },
            runs_dir,
        )?,
    };
    let result_json = to_json(run, runs_dir)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as Millis);
    let hash = hex(&Sha256::digest(result_json.as_bytes()));
    let id = RunId(format!("{now:013}-{}", &hash[..8]));

    fs::create_dir_all(runs_dir).map_err(io_err(runs_dir))?;
    let dest = runs_dir.join(id.as_str());
    if dest.exists() {
        return Err(StoreError::RunExists(id));
    }
    // Unique to this call, so two saves of the same run in the same millisecond (one id) never
    // share a temporary directory, and each removes only its own.
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = runs_dir.join(format!(".tmp-{id}-{}-{n}", std::process::id()));
    match fs::create_dir(&tmp) {
        Ok(()) => {}
        // A directory left by a process that had this pid; a later save gets a new name.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(StoreError::RunExists(id));
        }
        Err(e) => return Err(io_err(&tmp)(e)),
    }
    let write = || -> Result<(), StoreError> {
        let own = [
            (CONFIG_FILE, config_json.as_bytes()),
            (RESULT_FILE, result_json.as_bytes()),
        ];
        for &(name, contents) in own.iter().chain(files) {
            let path = tmp.join(name);
            fs::write(&path, contents).map_err(io_err(&path))?;
        }
        // Another save of the same run can land between the check above and here; the rename
        // then fails, as no file system renames a directory over a non-empty one.
        fs::rename(&tmp, &dest).map_err(|e| {
            if dest.exists() {
                StoreError::RunExists(id.clone())
            } else {
                io_err(&dest)(e)
            }
        })
    };
    write().inspect_err(|_| {
        let _ = fs::remove_dir_all(&tmp);
    })?;
    Ok(id)
}

/// Loads a saved result by id.
pub fn load(runs_dir: &Path, id: &str) -> Result<SavedRun, StoreError> {
    let id = RunId::parse(id)?;
    read_json(&runs_dir.join(id.as_str()).join(RESULT_FILE))
}

/// Lists saved runs, newest first. A missing runs directory is an empty list. Entries that are
/// not run directories (temporary directories of a save in progress, stray files) are skipped,
/// and so is a run whose `result.json` cannot be read, so one damaged run does not hide the rest.
///
/// shortcut: reads each run's whole `result.json` to get the headline metrics; add a small
/// summary file per run if listing gets slow with many long runs.
pub fn list(runs_dir: &Path) -> Result<Vec<RunSummary>, StoreError> {
    let entries = match fs::read_dir(runs_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(runs_dir)(e)),
    };
    let mut runs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err(runs_dir))?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|n| RunId::parse(n).ok())
        else {
            continue;
        };
        let Ok(head) = read_json::<Head>(&entry.path().join(RESULT_FILE)) else {
            continue;
        };
        let (kind, config, metrics) = match head {
            Head::Backtest { config, metrics } => (RunKind::Backtest, config, Some(metrics)),
            Head::Sweep { spec, best } => (RunKind::Sweep, spec.base, best.map(|b| b.metrics)),
            Head::Oos {
                spec,
                out_of_sample,
            } => (RunKind::Oos, spec.sweep.base, Some(out_of_sample.metrics)),
            Head::WalkForward { spec, metrics } => {
                (RunKind::WalkForward, spec.sweep.base, Some(metrics))
            }
            Head::Plan { config, battery } => (
                RunKind::Plan,
                config,
                battery.walk_forward.map(|w| w.metrics),
            ),
        };
        runs.push(summary_row(id, kind, &config, metrics.as_ref()));
    }
    runs.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(runs)
}

fn summary_row(
    id: RunId,
    kind: RunKind,
    config: &BacktestConfig,
    m: Option<&MetricsReport>,
) -> RunSummary {
    RunSummary {
        created_at: id.created_at(),
        id,
        kind,
        strategy: config.strategy.clone(),
        instrument: config.instrument.id.clone(),
        timeframe: config.timeframe,
        from: config.from,
        to: config.to,
        total_return: m.and_then(|m| m.total_return),
        cagr: m.and_then(|m| m.cagr),
        sharpe: m.and_then(|m| m.sharpe),
        max_drawdown: m.and_then(|m| m.max_drawdown),
        trades: m.map_or(0, |m| m.trades),
    }
}

// Only the fields the run list needs; serde skips the rest of the file.
#[derive(Deserialize)]
#[serde(tag = "kind", content = "result", rename_all = "snake_case")]
enum Head {
    Backtest {
        config: BacktestConfig,
        metrics: MetricsReport,
    },
    Sweep {
        spec: SweepHead,
        best: Option<MetricsHead>,
    },
    Oos {
        spec: TesterHead,
        out_of_sample: MetricsHead,
    },
    WalkForward {
        spec: TesterHead,
        metrics: MetricsReport,
    },
    Plan {
        config: BacktestConfig,
        battery: BatteryHead,
    },
}

#[derive(Deserialize)]
struct BatteryHead {
    walk_forward: Option<MetricsHead>,
}

#[derive(Deserialize)]
struct SweepHead {
    base: BacktestConfig,
}

#[derive(Deserialize)]
struct TesterHead {
    sweep: SweepHead,
}

#[derive(Deserialize)]
struct MetricsHead {
    metrics: MetricsReport,
}

fn to_json<T: Serialize>(value: &T, path: &Path) -> Result<String, StoreError> {
    serde_json::to_string_pretty(value).map_err(|source| StoreError::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, StoreError> {
    let text = fs::read_to_string(path).map_err(io_err(path))?;
    serde_json::from_str(&text).map_err(|source| StoreError::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn io_err(path: &Path) -> impl Fn(io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_are_checked() {
        let id = RunId::parse("1762790000123-0a1b2c3d").unwrap();
        assert_eq!(id.created_at(), 1_762_790_000_123);
        for bad in [
            "",
            "../../etc/passwd",
            "1762790000123-0A1B2C3D",
            "1762790000123_0a1b2c3d",
            "1762790000123-0a1b2c3",
            "176279000012x-0a1b2c3d",
        ] {
            assert!(RunId::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_run_id_is_written_as_its_string() {
        let id = RunId::parse("1762790000123-0a1b2c3d").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""1762790000123-0a1b2c3d""#);
        assert_eq!(serde_json::from_str::<RunId>(&json).unwrap(), id);
    }

    #[test]
    fn listing_a_missing_directory_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(&dir.path().join("runs")).unwrap().is_empty());
    }
}
