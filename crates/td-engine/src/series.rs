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
//! session holds every row back until the first bar whose close is at or after its `known_at`
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

/// Refuses a series name that is not a plain one (exported as `check_series_name`): 1 to 64
/// characters of lowercase ASCII letters, digits, `_` and `-`, starting with a letter or a digit,
/// and not a name Windows keeps for a device. The name becomes a file name in the series folder, so a separator, a dot, a drive
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
    let bytes = fs::read(&path).map_err(|e| bad(format!("cannot read {}: {e}", path.display())))?;
    parse(name, &bytes, until)
}

/// The rows of series `name` in `bytes`, a JSON array, up to the first row known after `until`.
fn parse(name: &str, bytes: &[u8], until: Option<Millis>) -> Result<Series, EngineError> {
    let bad = |why: String| EngineError::SeriesData(format!("series {name}: {why}"));
    let items: Vec<serde_json::Value> = serde_json::from_slice(bytes)
        .map_err(|e| bad(format!("the file is not a JSON array of rows: {e}")))?;
    let mut rows = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        // Only its time is looked at before the cutoff decides, so nothing else of a later row is
        // read.
        let known_at = item.get("known_at").and_then(serde_json::Value::as_i64);
        if let (Some(until), Some(known_at)) = (until, known_at)
            && known_at > until
        {
            break;
        }
        let row: SeriesRow = serde_json::from_value(item)
            .map_err(|e| bad(format!("row {} is not a row: {e}", i + 1)))?;
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use td_core::{
        Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Timeframe, Venue,
    };

    use super::*;
    use crate::runner::config_fingerprint_with_references;
    use crate::strategies::test_only::EXAMPLE_SERIES;
    use crate::{Context, Strategy, run_strategy_with_references, run_with_references};

    const HOUR: Millis = 3_600_000;
    const DAY: Millis = 24 * HOUR;

    /// The repo's `mock/` folder, which holds `series/example_series.json`.
    fn mock_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../mock")
    }

    fn example() -> Series {
        read_series(&mock_dir(), EXAMPLE_SERIES, None).unwrap()
    }

    fn row(known_at: Millis, value: f64) -> SeriesRow {
        SeriesRow {
            known_at,
            value,
            period: None,
        }
    }

    fn series(name: &str, rows: Vec<SeriesRow>) -> Series {
        Series {
            name: name.into(),
            rows,
        }
    }

    fn instrument() -> Instrument {
        Instrument {
            id: InstrumentId {
                venue: Venue::Binance,
                symbol: "TESTUSDT".into(),
                kind: InstrumentKind::Perpetual,
            },
            base: "TEST".into(),
            quote: "USDT".into(),
            tick_size: 0.01,
            lot_size: 0.001,
            min_qty: 0.001,
            min_notional: 1.0,
            fees: FeeSchedule {
                maker: 0.001,
                taker: 0.002,
                per_unit: 0.0,
            },
        }
    }

    fn candle(open_time: Millis, open: f64, close: f64) -> Candle {
        Candle {
            open_time,
            open,
            high: open.max(close) * 1.001,
            low: open.min(close) * 0.999,
            close,
            volume: 10.0,
            quote_volume: 10.0 * close,
        }
    }

    /// Hourly bars opening at 0h to `n - 1` h.
    fn hourly(n: i64) -> Vec<Candle> {
        (0..n)
            .map(|i| candle(i * HOUR, 100.0, 100.0 + i as f64))
            .collect()
    }

    /// A run of `example_series` on hourly bars from `from` to `to`, naming `names`.
    fn config(from: Millis, to: Millis, names: &[&str]) -> BacktestConfig {
        let entry = crate::find_strategy(EXAMPLE_SERIES).unwrap();
        BacktestConfig {
            timeframe: Timeframe::H1,
            from,
            to,
            series: names.iter().map(|n| n.to_string()).collect(),
            ..entry.plan.backtest_config(EXAMPLE_SERIES, instrument())
        }
    }

    /// Four-hour bars over the example strategy's plan, 250 days from 0, that rise while the
    /// latest row of `rows` known at their open is above 50 and fall otherwise, with a little
    /// noise: the example strategy at level 50 has an edge on them, and on a shuffled copy it does
    /// not.
    fn trending_bars(rows: &[SeriesRow]) -> Vec<Candle> {
        let mut rng = crate::rng::SplitMix64(7);
        let mut close = 100.0;
        (0..250 * 6)
            .map(|i| {
                let open_time = i * 4 * HOUR;
                let value = rows[known_by(rows, open_time) - 1].value;
                let drift = if value > 50.0 { 0.002 } else { -0.002 };
                let open = close;
                close *= 1.0 + drift + (rng.unit() - 0.5) * 0.004;
                candle(open_time, open, close)
            })
            .collect()
    }

    /// The `known_at` of each row of series `s` it was shown on each bar, by the bar's open time.
    type Shown = Arc<Mutex<Vec<(Millis, Option<Vec<Millis>>)>>>;

    /// Records the rows of series `s` it is shown on each bar.
    struct SeesSeries(Shown);

    impl Strategy for SeesSeries {
        fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
            let shown = ctx
                .series("s")
                .map(|rows| rows.iter().map(|r| r.known_at).collect());
            self.0.lock().unwrap().push((bar.open_time, shown));
        }
    }

    fn shown(
        config: &BacktestConfig,
        bars: &[Candle],
        given: &[Series],
    ) -> Vec<(Millis, Vec<Millis>)> {
        let seen = Shown::default();
        let strategy = Box::new(SeesSeries(seen.clone()));
        run_strategy_with_references(config, strategy, bars, &[], &[], &[], given).unwrap();
        let seen = seen.lock().unwrap().clone();
        seen.into_iter()
            .map(|(t, rows)| (t, rows.unwrap()))
            .collect()
    }

    fn write(dir: &Path, name: &str, text: &str) {
        let folder = dir.join(SERIES_DIR);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join(format!("{name}.json")), text).unwrap();
    }

    fn refused(dir: &Path, name: &str) -> String {
        read_series(dir, name, None).unwrap_err().to_string()
    }

    #[test]
    fn the_example_fixture_reads_with_its_periods() {
        let s = example();
        assert_eq!(s.name, EXAMPLE_SERIES);
        assert_eq!(s.rows.len(), 26);
        assert_eq!(s.rows[0], row(0, 42.0));
        assert_eq!(s.rows[1].period, Some(0));
        assert_eq!(s.rows[25].known_at, 250 * DAY);
    }

    /// A row known exactly at a bar's close is shown on that bar; one known 1 ms later waits for
    /// the next bar. Nothing is shown before it is known, warm-up bars included.
    #[test]
    fn a_row_is_shown_from_the_bar_whose_close_is_at_or_after_its_known_at() {
        let given = [series(
            "s",
            vec![row(0, 1.0), row(2 * HOUR, 2.0), row(3 * HOUR + 1, 3.0)],
        )];
        let config = BacktestConfig {
            warmup_from: Some(0),
            ..config(HOUR, 5 * HOUR, &["s"])
        };
        let want = vec![
            (0, vec![0]),
            (HOUR, vec![0, 2 * HOUR]),
            (2 * HOUR, vec![0, 2 * HOUR]),
            (3 * HOUR, vec![0, 2 * HOUR, 3 * HOUR + 1]),
            (4 * HOUR, vec![0, 2 * HOUR, 3 * HOUR + 1]),
        ];
        assert_eq!(shown(&config, &hourly(5), &given), want);
        // A run that names none shows none.
        let seen = Shown::default();
        let strategy = Box::new(SeesSeries(seen.clone()));
        run_strategy_with_references(
            &super::tests::config(0, 2 * HOUR, &[]),
            strategy,
            &hourly(2),
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert!(seen.lock().unwrap().iter().all(|(_, rows)| rows.is_none()));
    }

    /// The first value must be known by the close of the first bar the strategy sees, a warm-up
    /// bar when there is one; the end of the range is not checked.
    #[test]
    fn a_series_must_cover_the_close_of_the_first_bar_the_strategy_sees() {
        let bars = hourly(5);
        let covers = |first: Millis, config: &BacktestConfig| {
            let given = [series("s", vec![row(first, 1.0)])];
            run_strategy_with_references(
                config,
                Box::new(SeesSeries(Shown::default())),
                &bars,
                &[],
                &[],
                &[],
                &given,
            )
        };
        let plain = config(HOUR, 5 * HOUR, &["s"]);
        assert!(covers(2 * HOUR, &plain).is_ok());
        let late = covers(2 * HOUR + 1, &plain).unwrap_err();
        assert!(matches!(late, EngineError::SeriesData(_)), "{late}");
        assert!(
            late.to_string()
                .contains("does not cover the start of the range"),
            "{late}"
        );
        // With warm-up from 0h the first close is 1h, so the same row is now too late.
        let warm = BacktestConfig {
            warmup_from: Some(0),
            ..plain.clone()
        };
        assert!(covers(HOUR, &warm).is_ok());
        let err = covers(2 * HOUR, &warm).unwrap_err();
        assert!(
            err.to_string().contains("does not cover the start"),
            "{err}"
        );
        // A series that stops long before the range ends is not refused.
        assert!(covers(0, &plain).is_ok());
    }

    #[test]
    fn a_series_named_and_not_given_or_given_and_not_named_is_refused() {
        let bars = hourly(3);
        let run = |names: &[&str], given: &[Series]| {
            run_with_references(&config(0, 3 * HOUR, names), &bars, &[], &[], &[], given)
        };
        let missing = run(&[EXAMPLE_SERIES], &[]).unwrap_err();
        assert!(matches!(missing, EngineError::SeriesData(_)), "{missing}");
        assert!(
            missing.to_string().contains("no rows were given"),
            "{missing}"
        );
        let extra = run(&[], &[example()]).unwrap_err();
        assert!(matches!(extra, EngineError::InvalidSeries(_)), "{extra}");
        let twice = run(&[EXAMPLE_SERIES], &[example(), example()]).unwrap_err();
        assert!(twice.to_string().contains("twice"), "{twice}");
        let named_twice = run(&[EXAMPLE_SERIES, EXAMPLE_SERIES], &[example()]).unwrap_err();
        assert!(
            named_twice.to_string().contains("named twice"),
            "{named_twice}"
        );
        // The example strategy run without its series fails rather than trading blind.
        let blind = BacktestConfig {
            series: Vec::new(),
            ..config(0, 3 * HOUR, &[])
        };
        let err = run_with_references(&blind, &bars, &[], &[], &[], &[]).unwrap_err();
        assert!(matches!(err, EngineError::StrategyFailed(_)), "{err}");
        // Rows given in memory are checked as a file's are.
        let nan = series(EXAMPLE_SERIES, vec![row(0, 1.0), row(HOUR, f64::NAN)]);
        let err = run(&[EXAMPLE_SERIES], &[nan]).unwrap_err();
        assert!(
            err.to_string()
                .contains("row 2: its value is not a finite number"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_series(dir.path(), "absent", None).unwrap_err();
        assert!(matches!(err, EngineError::SeriesData(_)), "{err}");
        assert!(err.to_string().contains("there is no file"), "{err}");
    }

    #[test]
    fn a_malformed_file_is_refused_naming_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "object",
                r#"{"known_at": 0, "value": 1}"#,
                "is not a JSON array of rows",
            ),
            ("text", "not json", "is not a JSON array of rows"),
            (
                "field",
                r#"[{"known_at": 0, "value": 1, "when": 2}]"#,
                "row 1 is not a row",
            ),
            (
                "string",
                r#"[{"known_at": 0, "value": 1}, {"known_at": 1, "value": "x"}]"#,
                "row 2 is not a row",
            ),
            ("nokey", r#"[{"known_at": 0}]"#, "row 1 is not a row"),
            (
                "order",
                r#"[{"known_at": 0, "value": 1}, {"known_at": 10, "value": 2}, {"known_at": 5, "value": 3}]"#,
                "row 3: it is known at 5, not after the row before it, known at 10",
            ),
            (
                "same",
                r#"[{"known_at": 0, "value": 1}, {"known_at": 0, "value": 2}]"#,
                "row 2: it is known at 0",
            ),
        ];
        for (name, text, why) in cases {
            write(dir.path(), name, text);
            let err = refused(dir.path(), name);
            assert!(err.contains(&format!("series {name}: ")), "{name}: {err}");
            assert!(err.contains(why), "{name}: {err}");
        }
        write(dir.path(), "empty", "[]");
        assert!(
            read_series(dir.path(), "empty", None)
                .unwrap()
                .rows
                .is_empty()
        );
    }

    #[test]
    fn a_name_that_could_leave_the_folder_is_refused_before_any_path_is_made() {
        for name in [
            "../example_series",
            "..",
            "a/b",
            r"a\b",
            "c:x",
            ".hidden",
            "x.json",
            "",
            "Upper",
            "-dash",
            "has space",
            "nul",
            "com1",
            "lpt9",
            &"a".repeat(65),
        ] {
            let err = read_series(&mock_dir(), name, None).unwrap_err();
            assert!(
                matches!(err, EngineError::InvalidSeries(_)),
                "{name:?}: {err}"
            );
        }
        for name in ["a", "example_series", "x-1", "9", "com10", &"a".repeat(64)] {
            check_name(name).unwrap();
        }
        // The configuration's names are checked the same way.
        let err = config(0, HOUR, &["../x"]).check_series().unwrap_err();
        assert!(matches!(err, EngineError::InvalidSeries(_)), "{err}");
    }

    #[cfg(unix)]
    fn link(target: &Path, at: &Path, _dir: bool) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, at)
    }

    #[cfg(windows)]
    fn link(target: &Path, at: &Path, dir: bool) -> std::io::Result<()> {
        if dir {
            std::os::windows::fs::symlink_dir(target, at)
        } else {
            std::os::windows::fs::symlink_file(target, at)
        }
    }

    /// A link in place of the file or of the series folder is refused. Windows lets a process
    /// make a link only with developer mode or as an administrator, so there the test is skipped
    /// without one; CI runs it on Linux.
    #[test]
    fn a_link_in_place_of_the_file_or_the_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        write(&real, "inside", "[]");
        let data = dir.path().join("data");
        fs::create_dir_all(data.join(SERIES_DIR)).unwrap();
        let file = data.join(SERIES_DIR).join("linked.json");
        if let Err(e) = link(&real.join(SERIES_DIR).join("inside.json"), &file, false) {
            eprintln!("skipped: cannot make a link here: {e}");
            return;
        }
        let err = refused(&data, "linked");
        assert!(err.contains("is a link"), "{err}");
        let other = dir.path().join("other");
        fs::create_dir_all(&other).unwrap();
        link(&real.join(SERIES_DIR), &other.join(SERIES_DIR), true).unwrap();
        let err = refused(&other, "inside");
        assert!(err.contains("is a link"), "{err}");
        // The same file read in place is fine.
        assert!(read_series(&real, "inside", None).is_ok());
    }

    /// Rows known after `until` are not read: one that would be refused is not even looked at.
    #[test]
    fn rows_known_after_the_cutoff_are_not_read() {
        let mock = mock_dir();
        let upto = |until| {
            read_series(&mock, EXAMPLE_SERIES, Some(until))
                .unwrap()
                .rows
                .len()
        };
        assert_eq!(upto(10 * DAY), 2);
        assert_eq!(upto(10 * DAY - 1), 1);
        assert_eq!(upto(1000 * DAY), 26);

        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "late",
            r#"[{"known_at": 0, "value": 1}, {"known_at": 10, "value": 2}, {"known_at": 20, "value": "x"}]"#,
        );
        assert_eq!(
            read_series(dir.path(), "late", Some(10))
                .unwrap()
                .rows
                .len(),
            2
        );
        let err = refused(dir.path(), "late");
        assert!(err.contains("row 3"), "{err}");
    }

    /// The fingerprint covers the rows the run could see: a value changed among them changes it,
    /// one known after the run's last close does not, and a run that names no series keeps the
    /// fingerprint, configuration and JSON it had before series existed (the golden fixture in
    /// `tests/backtest.rs` pins that fingerprint).
    #[test]
    fn the_fingerprint_covers_the_series_rows_the_run_could_see() {
        let bars = hourly(5);
        let config = config(0, 5 * HOUR, &["s"]);
        let rows = vec![row(0, 1.0), row(2 * HOUR, 2.0), row(9 * HOUR, 3.0)];
        let print = |rows: Vec<SeriesRow>| {
            config_fingerprint_with_references(&config, &bars, &[], &[], &[], &[series("s", rows)])
                .unwrap()
        };
        let base = print(rows.clone());
        let mut changed = rows.clone();
        changed[1].value = 2.5;
        assert_ne!(print(changed), base);
        let mut moved = rows.clone();
        moved[1].known_at += 1;
        assert_ne!(print(moved), base);
        let mut period = rows.clone();
        period[1].period = Some(HOUR);
        assert_ne!(print(period), base);
        let mut later = rows.clone();
        later[2].value = 30.0;
        assert_eq!(print(later), base);
        // The result carries the same fingerprint.
        let result = run_strategy_with_references(
            &config,
            Box::new(SeesSeries(Shown::default())),
            &bars,
            &[],
            &[],
            &[],
            &[series("s", rows)],
        )
        .unwrap();
        assert_eq!(result.data_fingerprint, base);

        let none = BacktestConfig {
            series: Vec::new(),
            ..config.clone()
        };
        let plain = crate::config_fingerprint(&none, &bars, &[], &[]).unwrap();
        assert_ne!(plain, base);
        let json = serde_json::to_value(&none).unwrap();
        assert!(json.get("series").is_none(), "{json}");
        let named = serde_json::to_value(&config).unwrap();
        assert_eq!(named["series"], serde_json::json!(["s"]));
    }

    /// Every copy of the luck test is given the series as it is, in place in time: each copy's
    /// best is the run on the bars `permute_bars` gives for that copy's seed with the same rows.
    /// Shuffled or shifted rows would make the example strategy trade differently.
    #[test]
    fn the_luck_test_leaves_each_series_in_place_in_time() {
        let given = vec![example()];
        let bars: Vec<Candle> = trending_bars(&given[0].rows)
            .into_iter()
            .take(100 * 6)
            .collect();
        let base = BacktestConfig {
            timeframe: Timeframe::H4,
            close_at_end: true,
            ..config(0, 100 * DAY, &[EXAMPLE_SERIES])
        };
        let spec = crate::PermutationSpec {
            sweep: crate::SweepSpec {
                base: base.clone(),
                axes: Vec::new(),
                mode: crate::SweepMode::Grid,
                objective: crate::Objective {
                    metric: crate::Metric::TotalReturn,
                    direction: crate::Direction::Maximize,
                },
                min_trades: 0,
            },
            permutations: 6,
            seed: 11,
        };
        let result =
            crate::permutation_test_with_references(&spec, &bars, &[], &[], &[], &given, None)
                .unwrap();
        for (k, got) in result.permuted_best.iter().enumerate() {
            let seed = crate::rng::derive_seed(spec.seed, k as u64);
            let copy = crate::permute_bars(&bars, seed).unwrap();
            let want = run_with_references(&base, &copy, &[], &[], &[], &given)
                .unwrap()
                .metrics
                .total_return;
            assert_eq!(*got, want, "copy {k}");
        }
        assert!(result.permuted_best.windows(2).any(|w| w[0] != w[1]));
        // The fingerprint covers the series.
        let mut other = given.clone();
        other[0].rows[3].value += 1.0;
        let again =
            crate::permutation_test_with_references(&spec, &bars, &[], &[], &[], &other, None)
                .unwrap();
        assert_ne!(again.data_fingerprint, result.data_fingerprint);
    }

    /// The battery gives every run the series: the example strategy fails any run without it, so
    /// a battery that reaches every step (the sweep, the walk-forward, R5's neighbours, the cost
    /// stress, the start offsets and the luck test) fed it to each. Its record names it.
    #[test]
    fn the_battery_feeds_the_series_to_every_run_and_its_record_names_it() {
        let entry = crate::find_strategy(EXAMPLE_SERIES).unwrap();
        let mut plan = entry.plan.clone();
        plan.evidence.luck.permutations = 4;
        plan.evidence.min_sample.trades = 1;
        // Neighbours at the other levels trade on other rows, so the plateau would decide the
        // verdict before the later steps run.
        plan.evidence.plateau.min_median_ratio = f64::NEG_INFINITY;
        let given = vec![example()];
        let bars = trending_bars(&given[0].rows);
        let instrument = instrument();
        let battery = |rule: &crate::VerdictRule, given: &[Series]| {
            let spec = crate::BatterySpec {
                strategy: EXAMPLE_SERIES,
                plan: &plan,
                rule,
                instrument: &instrument,
                seed: 5,
            };
            crate::run_battery_with_references(&spec, &bars, &[], &[], &[], given, None)
        };
        let r = battery(&entry.rule, &given).unwrap();
        let wf = r
            .walk_forward
            .as_ref()
            .unwrap_or_else(|| panic!("{:?}", r.walk_forward_error));
        assert_eq!(wf.spec.sweep.base.series, vec![EXAMPLE_SERIES.to_string()]);
        assert!(r.sweep.is_some() && r.plateau.is_some());
        assert!(r.cost_stress.is_some(), "{:#?}", r.verdict.criteria);
        assert!(!r.start_offsets.is_empty());
        for offset in &r.start_offsets {
            assert!(offset.oos_return.is_some(), "{offset:?}");
        }
        assert!(r.permutation.is_some(), "{:?}", r.luck_note);

        let spec = crate::BatterySpec {
            strategy: EXAMPLE_SERIES,
            plan: &plan,
            rule: &entry.rule,
            instrument: &instrument,
            seed: 5,
        };
        let record =
            crate::PlanRun::with_references(&spec, r, &bars, &[], &[], &[], &given).unwrap();
        assert_eq!(record.config.series, vec![EXAMPLE_SERIES.to_string()]);
        assert_eq!(
            record.data_fingerprint,
            config_fingerprint_with_references(&record.config, &bars, &[], &[], &[], &given)
                .unwrap()
        );

        // A prop-firm rule sized from the starting cash makes each window's test run again for
        // its exposures, with the series too.
        let firm = crate::VerdictRule::PropFirm {
            firm: crate::FirmLimits {
                daily_loss: 0.06,
                max_drawdown: 0.20,
                drawdown: crate::DrawdownMode::Trailing,
                profit_target: 0.08,
                second_target: None,
                min_trading_days: 0,
                best_day_share: None,
                day_target_share: None,
                trading_day: None,
                trailing_floor_lock: None,
                max_days_without_trading: None,
                day_rollover: None,
            },
            limits: crate::PropFirmLimits {
                sizes_from_starting_cash: true,
                ..crate::PropFirmLimits::default()
            },
            attempt: None,
        };
        let sized = battery(&firm, &given).unwrap();
        assert!(
            sized.walk_forward.is_some(),
            "{:?}",
            sized.walk_forward_error
        );

        // Without the series the battery stops on its first run.
        let blind = battery(&entry.rule, &[]).unwrap_err();
        assert!(
            blind.to_string().contains("no series example_series"),
            "{blind}"
        );
    }
}
