//! Time series that are not price bars (#227): numbers with a time of their own at which each
//! became known, such as a figure published on some days, that a strategy reads beside its bars.
//!
//! A series is one file in the data folder, `<data_dir>/series/<name>.json` ([`read_series`]): a
//! JSON array of rows `{"known_at": <ms>, "value": <number>}`, with an optional `"period": <ms>`,
//! the start of the period the value describes. `known_at` is the time the value became public,
//! not the period it describes, and it alone decides when a strategy may see the row. Rows must be
//! strictly increasing in `known_at`.
//!
//! A run names its series in [`BacktestConfig::series`] and is given their rows as [`Series`]. The
//! session holds every row back until the close of the first bar at or after its `known_at`
//! (see [`crate::Session`]), so on a bar the strategy reads ([`crate::Context::series`]) only the
//! rows known by that bar's close; a later row is not in anything the strategy can reach.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use serde::{Deserialize, Serialize};
use td_core::Millis;

use crate::{BacktestConfig, EngineError};

/// The folder of the data folder that holds the series files.
pub const SERIES_DIR: &str = "series";

/// The longest series name accepted.
const MAX_NAME: usize = 64;

/// Names Windows keeps for devices whatever the extension, so `nul.json` is not a file there.
const DEVICE_NAMES: [&str; 4] = ["con", "prn", "aux", "nul"];

/// One value of a series.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesRow {
    /// When the value became public, in milliseconds since the Unix epoch, UTC. A strategy sees
    /// the row from the bar whose close is at or after this time.
    pub known_at: Millis,
    pub value: f64,
    /// The start of the period the value describes, when the file gives one. Only read by the
    /// strategy: what it may see is decided by `known_at` alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<Millis>,
}

/// The rows of one series a run names, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: String,
    pub rows: Vec<SeriesRow>,
}

/// Refuses a series name that is not a plain one (exported as `check_series_name`): 1 to 64 characters of lowercase ASCII letters,
/// digits, `_` and `-`, starting with a letter or a digit, and not a name Windows keeps for a
/// device. The name becomes a file name in the series folder, so a separator, a dot, a drive
/// prefix or anything else that could leave the folder is refused.
pub fn check_name(name: &str) -> Result<(), EngineError> {
    let refuse = |why: &str| Err(EngineError::InvalidSeries(format!("{name:?} {why}")));
    let plain = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    if name.is_empty() || name.len() > MAX_NAME {
        return refuse("is not a series name: give 1 to 64 characters");
    }
    if !name.starts_with(plain) || !name.chars().all(|c| plain(c) || c == '_' || c == '-') {
        return refuse(
            "is not a series name: use lowercase letters, digits, `_` and `-`, starting with a \
             letter or a digit",
        );
    }
    let device = DEVICE_NAMES.contains(&name)
        || ["com", "lpt"].iter().any(|p| {
            name.strip_prefix(p)
                .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
        });
    if device {
        return refuse("is a name Windows keeps for a device, so it cannot be a series name");
    }
    Ok(())
}

/// Refuses a name [`check_name`] refuses and one named twice.
pub(crate) fn check_names(names: &[String]) -> Result<(), EngineError> {
    for (i, name) in names.iter().enumerate() {
        check_name(name)?;
        if names[..i].contains(name) {
            return Err(EngineError::InvalidSeries(format!(
                "series {name} is named twice"
            )));
        }
    }
    Ok(())
}

/// The series `name` from `<data_dir>/series/<name>.json`, with only the rows known at or before
/// `until` when it is given (the end of the data cutoff date): reading stops at the first row
/// known after it, so nothing later is read into a run, or checked.
///
/// Refuses a name [`check_name`] refuses with [`EngineError::InvalidSeries`], before any path is
/// made from it. Refuses with [`EngineError::SeriesData`] a missing file, a link (a symlink, or on
/// Windows any reparse point) in place of the series folder or the file, a file that is not a JSON
/// array of rows, and rows that are not strictly increasing in `known_at` or hold a value that is
/// not a finite number, naming the row (counted from 1).
///
/// shortcut: the links are checked before the file is opened, so a link put in place between the
/// check and the read is followed, as the live session files are; open the file without following
/// links if series folders are ever shared.
pub fn read_series(
    data_dir: &Path,
    name: &str,
    until: Option<Millis>,
) -> Result<Series, EngineError> {
    check_name(name)?;
    let dir = data_dir.join(SERIES_DIR);
    let path = dir.join(format!("{name}.json"));
    let bad = |why: String| EngineError::SeriesData(format!("series {name}: {why}"));
    for p in [&dir, &path] {
        match fs::symlink_metadata(p) {
            Ok(meta) if is_link(&meta) => {
                return Err(bad(format!(
                    "{} is a link; the series folder and its files must be plain ones",
                    p.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(bad(format!(
                    "there is no file {}; put the series there",
                    path.display()
                )));
            }
            Err(e) => return Err(bad(format!("cannot read {}: {e}", p.display()))),
        }
    }
    let bytes =
        fs::read(&path).map_err(|e| bad(format!("cannot read {}: {e}", path.display())))?;
    parse(name, &bytes, until)
}

/// The rows of series `name` in `bytes`, a JSON array, up to the first row known after `until`.
fn parse(name: &str, bytes: &[u8], until: Option<Millis>) -> Result<Series, EngineError> {
    let bad = |why: String| EngineError::SeriesData(format!("series {name}: {why}"));
    let items: Vec<serde_json::Value> = serde_json::from_slice(bytes)
        .map_err(|e| bad(format!("the file is not a JSON array of rows: {e}")))?;
    let mut rows = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        let row: SeriesRow = serde_json::from_value(item)
            .map_err(|e| bad(format!("row {} is not a row: {e}", i + 1)))?;
        if until.is_some_and(|until| row.known_at > until) {
            break;
        }
        rows.push(row);
    }
    check_rows(name, &rows)?;
    Ok(Series {
        name: name.to_string(),
        rows,
    })
}

/// Refuses rows that are not strictly increasing in `known_at`, and a value that is not a finite
/// number, naming the row, counted from 1.
fn check_rows(name: &str, rows: &[SeriesRow]) -> Result<(), EngineError> {
    for (i, row) in rows.iter().enumerate() {
        let why = if !row.value.is_finite() {
            "its value is not a finite number".to_string()
        } else if i > 0 && row.known_at <= rows[i - 1].known_at {
            format!(
                "it is known at {}, not after the row before it, known at {}; rows must be \
                 strictly increasing in known_at",
                row.known_at,
                rows[i - 1].known_at
            )
        } else {
            continue;
        };
        return Err(EngineError::SeriesData(format!(
            "series {name}: row {}: {why}",
            i + 1
        )));
    }
    Ok(())
}

/// The series `given` for `config`, in the order it names them, once each passes: refuses names
/// [`BacktestConfig::check_series`] refuses, a named series not given, one given that the
/// configuration does not name or given twice, rows [`read_series`] would refuse, and a series
/// with no row known at or before `first_close`, the close of the first bar the strategy sees,
/// warm-up included, so the strategy never starts without a value. Empty when the configuration
/// names none and none are given.
///
/// The end of the range is not checked: a series can be irregular, so a value missing later on
/// cannot be told from one that was never published.
pub(crate) fn named<'a>(
    config: &BacktestConfig,
    first_close: Millis,
    given: &'a [Series],
) -> Result<Vec<&'a Series>, EngineError> {
    config.check_series()?;
    let refuse = |why: String| Err(EngineError::InvalidSeries(why));
    for (i, s) in given.iter().enumerate() {
        if !config.series.contains(&s.name) {
            return refuse(format!(
                "rows were given for series {}, which the run does not name",
                s.name
            ));
        }
        if given[..i].iter().any(|g| g.name == s.name) {
            return refuse(format!("rows were given twice for series {}", s.name));
        }
    }
    let mut named = Vec::with_capacity(config.series.len());
    for name in &config.series {
        let Some(s) = given.iter().find(|s| &s.name == name) else {
            return Err(EngineError::SeriesData(format!(
                "series {name}: no rows were given for it"
            )));
        };
        check_rows(name, &s.rows)?;
        if s.rows.first().is_none_or(|r| r.known_at > first_close) {
            return Err(EngineError::SeriesData(format!(
                "series {name} has no row known at or before {first_close}, the close of the \
                 first bar the strategy sees, so it does not cover the start of the range"
            )));
        }
        named.push(s);
    }
    Ok(named)
}

/// How many of `rows` are known at or before `at`.
pub(crate) fn known_by(rows: &[SeriesRow], at: Millis) -> usize {
    rows.partition_point(|r| r.known_at <= at)
}

/// A symbolic link, or on Windows any reparse point (a junction among them).
fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}
