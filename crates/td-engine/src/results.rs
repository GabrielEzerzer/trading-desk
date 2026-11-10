//! Saved backtest results: one directory per run under the runs directory.
//!
//! A run is stored as `<runs_dir>/<run_id>/config.json` (the configuration alone, for reading
//! by hand) and `result.json` (the whole [`BacktestResult`], configuration included).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use td_core::{InstrumentId, Millis, Timeframe};

use crate::BacktestResult;
use crate::metrics::MetricsReport;
use crate::runner::hex;

const CONFIG_FILE: &str = "config.json";
const RESULT_FILE: &str = "result.json";

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
}

/// A run's id: the save time in milliseconds since the epoch, zero-padded to 13 digits so ids
/// sort by time, then `-` and the first 8 hex digits of the SHA-256 of `result.json`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
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

/// One row of the run list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: RunId,
    pub created_at: Millis,
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
/// the run's id, so a run appears whole or not at all. `runs_dir` is created if needed.
pub fn save(result: &BacktestResult, runs_dir: &Path) -> Result<RunId, StoreError> {
    let config_json = to_json(&result.config, runs_dir)?;
    let result_json = to_json(result, runs_dir)?;
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
    let tmp = runs_dir.join(format!(".tmp-{id}"));
    let write = || -> Result<(), StoreError> {
        fs::create_dir(&tmp).map_err(io_err(&tmp))?;
        for (name, json) in [(CONFIG_FILE, &config_json), (RESULT_FILE, &result_json)] {
            let path = tmp.join(name);
            fs::write(&path, json).map_err(io_err(&path))?;
        }
        fs::rename(&tmp, &dest).map_err(io_err(&dest))
    };
    write().inspect_err(|_| {
        let _ = fs::remove_dir_all(&tmp);
    })?;
    Ok(id)
}

/// Loads a saved result by id.
pub fn load(runs_dir: &Path, id: &str) -> Result<BacktestResult, StoreError> {
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
    // Only the fields the list needs; serde skips the rest of the file.
    #[derive(Deserialize)]
    struct Head {
        config: crate::BacktestConfig,
        metrics: MetricsReport,
    }
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
        runs.push(RunSummary {
            created_at: id.created_at(),
            id,
            strategy: head.config.strategy,
            instrument: head.config.instrument.id,
            timeframe: head.config.timeframe,
            from: head.config.from,
            to: head.config.to,
            total_return: head.metrics.total_return,
            cagr: head.metrics.cagr,
            sharpe: head.metrics.sharpe,
            max_drawdown: head.metrics.max_drawdown,
            trades: head.metrics.trades,
        });
    }
    runs.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(runs)
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
    fn listing_a_missing_directory_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(&dir.path().join("runs")).unwrap().is_empty());
    }
}
