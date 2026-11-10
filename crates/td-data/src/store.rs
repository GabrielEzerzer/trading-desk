// The atomic rename is adapted from tipping-point engine/src/store/run.rs at 7dc6d12 (`rename_new`).

//! The local candle store: Parquet files outside the repo, one per instrument per month of 1-minute
//! candles, `<data_dir>/candles/<venue>/<kind>/<SYMBOL>/1m/<YYYY-MM>.parquet`, and one funding
//! file per perpetual, `<data_dir>/funding/<venue>/<SYMBOL>.parquet`.
//!
//! Every file holds rows sorted by time with no duplicates, and every candle passed
//! [`Candle::validate`] on its way in. Files are replaced atomically: written to a temp file in the
//! same folder, then renamed over the old one. Each read-merge-write of a file holds a lock for
//! that path, so two writers in one process (two download jobs, say) never lose each other's rows.
//! Missing minutes are reported, never filled.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::sleep;
use std::time::Duration;

use arrow::array::{Array, ArrayRef, Float64Array, Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use td_core::{Candle, InstrumentId, InstrumentKind, Millis, Timeframe};

use crate::DataError;
use crate::resample::{LoadedCandles, resample};
use crate::time::{MINUTE, Month};

const CANDLE_COLUMNS: [&str; 7] = [
    "open_time",
    "open",
    "high",
    "low",
    "close",
    "volume",
    "quote_volume",
];
const FUNDING_COLUMNS: [&str; 2] = ["time", "rate"];

/// A run of missing minutes: open times `from`, `from + 1m`, ... up to but not including `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    pub from: Millis,
    pub to: Millis,
}

impl Gap {
    pub fn minutes(&self) -> i64 {
        (self.to - self.from) / MINUTE
    }
}

/// The missing minutes in `[from, to)` among `candles`, which are sorted by open time.
pub fn missing_minutes(candles: &[Candle], from: Millis, to: Millis) -> Vec<Gap> {
    let mut gaps = Vec::new();
    let mut expected = from;
    for c in candles {
        if c.open_time < expected {
            continue;
        }
        if c.open_time >= to {
            break;
        }
        if c.open_time > expected {
            gaps.push(Gap {
                from: expected,
                to: c.open_time,
            });
        }
        expected = c.open_time + MINUTE;
    }
    if expected < to {
        gaps.push(Gap { from: expected, to });
    }
    gaps
}

/// What one write did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WriteReport {
    /// The month files written.
    pub months: Vec<Month>,
    /// Candles given that replaced a stored candle or an earlier one in the same batch with the
    /// same open time.
    pub duplicates: usize,
    /// Missing minutes between the first and last candle given, after the write.
    pub gaps: Vec<Gap>,
}

/// Which minutes are stored for one month.
#[derive(Debug, Clone, PartialEq)]
pub struct MonthCoverage {
    pub month: Month,
    /// Stored 1-minute candles.
    pub minutes: usize,
    pub first: Millis,
    pub last: Millis,
    /// Missing minutes between `first` and `last`.
    pub gaps: Vec<Gap>,
}

/// The data directory: `TD_DATA_DIR` if set, else `%LOCALAPPDATA%\trading-desk` on Windows and
/// `$XDG_DATA_HOME/trading-desk` or `~/.local/share/trading-desk` elsewhere.
pub fn data_dir() -> Result<PathBuf, DataError> {
    data_dir_from(cfg!(windows), |name| std::env::var_os(name))
}

fn data_dir_from(
    windows: bool,
    var: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, DataError> {
    let var = |name: &str| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = var("TD_DATA_DIR") {
        return Ok(dir);
    }
    let base = if windows {
        var("LOCALAPPDATA")
    } else {
        var("XDG_DATA_HOME").or_else(|| var("HOME").map(|home| home.join(".local").join("share")))
    };
    base.map(|b| b.join("trading-desk"))
        .ok_or(DataError::NoDataDir)
}

/// Checks that a symbol is safe as a folder or file name: letters (any script), digits, `-`, `_`.
pub(crate) fn check_symbol(symbol: &str) -> Result<(), DataError> {
    let ok = !symbol.is_empty()
        && symbol
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(DataError::InvalidSymbol(symbol.to_string()))
    }
}

/// The candle and funding store under one data directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }

    /// The store in [`data_dir`].
    pub fn open_default() -> Result<Self, DataError> {
        data_dir().map(Store::new)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn candle_dir(&self, id: &InstrumentId) -> Result<PathBuf, DataError> {
        check_symbol(&id.symbol)?;
        Ok(self
            .root
            .join("candles")
            .join(id.venue.to_string())
            .join(id.kind.to_string())
            .join(&id.symbol)
            .join("1m"))
    }

    /// The file that holds one month of an instrument's 1-minute candles.
    pub fn candle_path(&self, id: &InstrumentId, month: Month) -> Result<PathBuf, DataError> {
        Ok(self.candle_dir(id)?.join(format!("{month}.parquet")))
    }

    /// The file that holds a perpetual's funding rates.
    pub fn funding_path(&self, id: &InstrumentId) -> Result<PathBuf, DataError> {
        check_symbol(&id.symbol)?;
        if id.kind != InstrumentKind::Perpetual {
            return Err(DataError::NotPerpetual(id.to_string()));
        }
        Ok(self
            .root
            .join("funding")
            .join(id.venue.to_string())
            .join(format!("{}.parquet", id.symbol)))
    }

    /// Merges 1-minute candles into the store. Every candle must be valid and start on a minute;
    /// if any is not, nothing is written. A candle replaces a stored one with the same open time.
    pub fn write_candles(
        &self,
        id: &InstrumentId,
        candles: &[Candle],
    ) -> Result<WriteReport, DataError> {
        let mut report = WriteReport::default();
        let mut by_month: BTreeMap<Month, Vec<Candle>> = BTreeMap::new();
        for c in candles {
            check_candle(id, c)?;
            by_month
                .entry(Month::containing(c.open_time))
                .or_default()
                .push(*c);
        }
        let (Some(first), Some(last)) = (
            candles.iter().map(|c| c.open_time).min(),
            candles.iter().map(|c| c.open_time).max(),
        ) else {
            return Ok(report);
        };
        let mut merged_span = Vec::new();
        for (month, new) in by_month {
            let path = self.candle_path(id, month)?;
            let lock = file_lock(&path);
            let _held = hold(&lock);
            let mut merged: BTreeMap<Millis, Candle> = self
                .read_month(id, month)?
                .into_iter()
                .map(|c| (c.open_time, c))
                .collect();
            for c in new {
                if merged.insert(c.open_time, c).is_some() {
                    report.duplicates += 1;
                }
            }
            let merged: Vec<Candle> = merged.into_values().collect();
            write_atomic(&path, &candle_batch(&merged)?)?;
            report.months.push(month);
            merged_span.extend(
                merged
                    .into_iter()
                    .filter(|c| (first..=last).contains(&c.open_time)),
            );
        }
        report.gaps = missing_minutes(&merged_span, first, last + MINUTE);
        Ok(report)
    }

    /// One month of an instrument's 1-minute candles, sorted; empty when none are stored.
    pub fn read_month(&self, id: &InstrumentId, month: Month) -> Result<Vec<Candle>, DataError> {
        let path = self.candle_path(id, month)?;
        let Some(columns) = read_columns(&path, &CANDLE_COLUMNS)? else {
            return Ok(Vec::new());
        };
        let [open_time, rest @ ..] = &columns[..] else {
            return Ok(Vec::new());
        };
        let open_time = int_column(&path, open_time)?;
        let values = rest
            .iter()
            .map(|c| float_column(&path, c))
            .collect::<Result<Vec<_>, _>>()?;
        let mut candles = Vec::with_capacity(open_time.len());
        let mut previous = Millis::MIN;
        for (row, &t) in open_time.iter().enumerate() {
            let candle = Candle {
                open_time: t,
                open: values[0][row],
                high: values[1][row],
                low: values[2][row],
                close: values[3][row],
                volume: values[4][row],
                quote_volume: values[5][row],
            };
            check_candle(id, &candle)?;
            if t <= previous || Month::containing(t) != month {
                return Err(bad_file(
                    &path,
                    format!("row {row} is out of order or outside {month}"),
                ));
            }
            previous = t;
            candles.push(candle);
        }
        Ok(candles)
    }

    /// The months stored for an instrument, in order.
    pub fn months(&self, id: &InstrumentId) -> Result<Vec<Month>, DataError> {
        let dir = self.candle_dir(id)?;
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(DataError::io(&dir)(e)),
        };
        let mut months = Vec::new();
        for entry in entries {
            let name = entry.map_err(DataError::io(&dir))?.file_name();
            let month = name
                .to_str()
                .and_then(|n| n.strip_suffix(".parquet"))
                .and_then(Month::parse);
            months.extend(month);
        }
        months.sort();
        Ok(months)
    }

    /// Which months and minutes are stored for an instrument.
    pub fn coverage(&self, id: &InstrumentId) -> Result<Vec<MonthCoverage>, DataError> {
        let mut coverage = Vec::new();
        for month in self.months(id)? {
            let candles = self.read_month(id, month)?;
            let (Some(first), Some(last)) = (candles.first(), candles.last()) else {
                continue;
            };
            coverage.push(MonthCoverage {
                month,
                minutes: candles.len(),
                first: first.open_time,
                last: last.open_time,
                gaps: missing_minutes(&candles, first.open_time, last.open_time + MINUTE),
            });
        }
        Ok(coverage)
    }

    /// Bars of `timeframe` resampled from the stored 1-minute candles with open times in
    /// `[from, to)`, by the rules of [`resample`].
    ///
    /// Minutes outside the range are not read, so a bucket that `from` or `to` falls inside is
    /// missing minutes, and it is dropped or kept by the same rule as a bucket with minutes missing
    /// from the store.
    pub fn load_candles(
        &self,
        id: &InstrumentId,
        timeframe: Timeframe,
        from: Millis,
        to: Millis,
        include_incomplete: bool,
    ) -> Result<LoadedCandles, DataError> {
        let mut minutes = Vec::new();
        for month in Month::range(from, to) {
            minutes.extend(
                self.read_month(id, month)?
                    .into_iter()
                    .filter(|c| (from..to).contains(&c.open_time)),
            );
        }
        Ok(resample(&minutes, timeframe, include_incomplete))
    }

    /// Merges funding rates `(funding time, rate)` into a perpetual's funding file, replacing any
    /// stored rate with the same time. Returns the number of rates stored after the write.
    pub fn write_funding(
        &self,
        id: &InstrumentId,
        rates: &[(Millis, f64)],
    ) -> Result<usize, DataError> {
        let path = self.funding_path(id)?;
        if let Some(&(t, rate)) = rates.iter().find(|(_, rate)| !rate.is_finite()) {
            return Err(DataError::parse(
                format!("{id} funding at {t}"),
                format!("rate {rate} is not finite"),
            ));
        }
        let lock = file_lock(&path);
        let _held = hold(&lock);
        let mut merged: BTreeMap<Millis, f64> = self.read_funding(id)?.into_iter().collect();
        merged.extend(rates.iter().copied());
        let (times, values): (Vec<Millis>, Vec<f64>) = merged.into_iter().unzip();
        let count = times.len();
        let batch = record_batch(
            &FUNDING_COLUMNS,
            vec![
                Arc::new(Int64Array::from(times)),
                Arc::new(Float64Array::from(values)),
            ],
        )?;
        write_atomic(&path, &batch)?;
        Ok(count)
    }

    /// A perpetual's funding rates `(funding time, rate)` in `[from, to)`, oldest first.
    pub fn load_funding(
        &self,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        let mut rates = self.read_funding(id)?;
        rates.retain(|&(t, _)| (from..to).contains(&t));
        Ok(rates)
    }

    /// A perpetual's funding file, sorted; empty when none is stored.
    fn read_funding(&self, id: &InstrumentId) -> Result<Vec<(Millis, f64)>, DataError> {
        let path = self.funding_path(id)?;
        let Some(columns) = read_columns(&path, &FUNDING_COLUMNS)? else {
            return Ok(Vec::new());
        };
        let times = int_column(&path, &columns[0])?;
        let values = float_column(&path, &columns[1])?;
        let mut rates = Vec::with_capacity(times.len());
        let mut previous = Millis::MIN;
        for (row, (&t, &rate)) in times.iter().zip(values).enumerate() {
            if !rate.is_finite() {
                return Err(bad_file(
                    &path,
                    format!("row {row} has a rate that is not finite: {rate}"),
                ));
            }
            if t <= previous {
                return Err(bad_file(&path, format!("row {row} is out of order")));
            }
            previous = t;
            rates.push((t, rate));
        }
        Ok(rates)
    }
}

/// A candle must be valid and open on a minute.
fn check_candle(id: &InstrumentId, c: &Candle) -> Result<(), DataError> {
    c.validate().map_err(|source| DataError::Candle {
        instrument: id.to_string(),
        source,
    })?;
    if c.open_time.rem_euclid(MINUTE) != 0 {
        return Err(DataError::parse(
            format!("{id} candle at {}", c.open_time),
            "the open time is not on a minute",
        ));
    }
    Ok(())
}

fn bad_file(path: &Path, reason: impl Into<String>) -> DataError {
    DataError::BadFile {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

fn record_batch(names: &[&str], columns: Vec<ArrayRef>) -> Result<RecordBatch, DataError> {
    let fields: Vec<Field> = names
        .iter()
        .zip(&columns)
        .map(|(name, column)| Field::new(*name, column.data_type().clone(), false))
        .collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
        .map_err(|e| DataError::parse("a record batch", e))
}

fn candle_batch(candles: &[Candle]) -> Result<RecordBatch, DataError> {
    let float = |f: fn(&Candle) -> f64| -> ArrayRef {
        Arc::new(Float64Array::from_iter_values(candles.iter().map(f)))
    };
    record_batch(
        &CANDLE_COLUMNS,
        vec![
            Arc::new(Int64Array::from_iter_values(
                candles.iter().map(|c| c.open_time),
            )),
            float(|c| c.open),
            float(|c| c.high),
            float(|c| c.low),
            float(|c| c.close),
            float(|c| c.volume),
            float(|c| c.quote_volume),
        ],
    )
}

/// The lock that serialises read-merge-write of one store file within this process. Callers hold
/// it from the read through the rename.
// shortcut: keyed by the path as built from the store root, so two `Store`s that spell one folder
// differently (relative and absolute) do not share locks, and entries are never removed (one per
// file written, a few hundred bytes each); canonicalise and evict if either ever matters. Other
// processes writing the same data directory are not covered.
fn file_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut locks = hold(LOCKS.get_or_init(Mutex::default));
    locks.entry(path.to_path_buf()).or_default().clone()
}

/// Locks `mutex`, carrying on past a panic in another holder: the data behind these locks is a
/// map of locks or nothing at all, so it cannot be left half-changed.
fn hold<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Writes `batch` to a temp file beside `path`, then renames it over `path`. The temp name holds
/// the process id and a per-process counter, so no two writes share one.
fn write_atomic(path: &Path, batch: &RecordBatch) -> Result<(), DataError> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(DataError::io(dir))?;
    let mut temp_name = path.file_name().unwrap_or_default().to_owned();
    let write = WRITES.fetch_add(1, Ordering::Relaxed);
    temp_name.push(format!(".{}.{write}.tmp", std::process::id()));
    let temp = dir.join(temp_name);
    let parquet_error = |source| DataError::Parquet {
        path: temp.clone(),
        source,
    };
    let result = (|| {
        let file = File::create(&temp).map_err(DataError::io(&temp))?;
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::default()))
            .build();
        let mut writer =
            ArrowWriter::try_new(file, batch.schema(), Some(props)).map_err(parquet_error)?;
        writer.write(batch).map_err(parquet_error)?;
        let file = writer.into_inner().map_err(parquet_error)?;
        file.sync_all().map_err(DataError::io(&temp))?;
        drop(file);
        rename_replacing(&temp, path).map_err(DataError::io(path))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// `fs::rename`, retried briefly when Windows refuses it because another process (an indexer or a
/// virus scanner) holds one of the files open.
fn rename_replacing(from: &Path, to: &Path) -> io::Result<()> {
    let mut wait = Duration::from_millis(25);
    for _ in 0..6 {
        match fs::rename(from, to) {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                sleep(wait);
                wait *= 2;
            }
            done => return done,
        }
    }
    fs::rename(from, to)
}

/// Reads the named columns of a Parquet file, one array per column across all row groups. `None`
/// when the file does not exist.
fn read_columns(path: &Path, names: &[&str]) -> Result<Option<Vec<ArrayRef>>, DataError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(DataError::io(path)(e)),
    };
    let parquet_error = |source| DataError::Parquet {
        path: path.to_path_buf(),
        source,
    };
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(parquet_error)?
        .build()
        .map_err(parquet_error)?;
    let mut parts: Vec<Vec<ArrayRef>> = vec![Vec::new(); names.len()];
    for batch in reader {
        let batch = batch.map_err(|e| bad_file(path, e.to_string()))?;
        for (name, part) in names.iter().zip(&mut parts) {
            let column = batch
                .column_by_name(name)
                .ok_or_else(|| bad_file(path, format!("no column {name:?}")))?;
            if column.null_count() > 0 {
                return Err(bad_file(path, format!("column {name:?} has nulls")));
            }
            part.push(column.clone());
        }
    }
    let mut columns = Vec::with_capacity(names.len());
    for (name, part) in names.iter().zip(parts) {
        let refs: Vec<&dyn Array> = part.iter().map(|a| a.as_ref()).collect();
        let column = if refs.is_empty() {
            let data_type = if *name == names[0] {
                DataType::Int64
            } else {
                DataType::Float64
            };
            arrow::array::new_empty_array(&data_type)
        } else {
            arrow::compute::concat(&refs).map_err(|e| bad_file(path, e.to_string()))?
        };
        columns.push(column);
    }
    Ok(Some(columns))
}

fn int_column<'a>(path: &Path, column: &'a ArrayRef) -> Result<&'a [i64], DataError> {
    column
        .as_any()
        .downcast_ref::<Int64Array>()
        .map(|a| a.values().as_ref())
        .ok_or_else(|| {
            bad_file(
                path,
                format!("a column is {}, not Int64", column.data_type()),
            )
        })
}

fn float_column<'a>(path: &Path, column: &'a ArrayRef) -> Result<&'a [f64], DataError> {
    column
        .as_any()
        .downcast_ref::<Float64Array>()
        .map(|a| a.values().as_ref())
        .ok_or_else(|| {
            bad_file(
                path,
                format!("a column is {}, not Float64", column.data_type()),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_prefers_td_data_dir_then_the_platform_folder() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        let dir = |windows, pairs| data_dir_from(windows, env(pairs)).ok();
        assert_eq!(
            dir(true, &[("TD_DATA_DIR", "D:/td"), ("LOCALAPPDATA", "C:/L")]),
            Some(PathBuf::from("D:/td"))
        );
        assert_eq!(
            dir(true, &[("TD_DATA_DIR", ""), ("LOCALAPPDATA", "C:/L")]),
            Some(PathBuf::from("C:/L").join("trading-desk"))
        );
        assert_eq!(
            dir(false, &[("XDG_DATA_HOME", "/x"), ("HOME", "/h")]),
            Some(PathBuf::from("/x").join("trading-desk"))
        );
        assert_eq!(
            dir(false, &[("HOME", "/h")]),
            Some(PathBuf::from("/h/.local/share/trading-desk"))
        );
        assert_eq!(dir(false, &[]), None);
    }

    #[test]
    fn symbols_that_could_escape_the_store_are_refused() {
        for ok in [
            "BTCUSDT",
            "1000PEPEUSDT",
            "BTC-26DEC25",
            "BTCUSDT_261225",
            "币安人生USDT",
        ] {
            assert!(check_symbol(ok).is_ok(), "{ok}");
        }
        for bad in ["", "..", "BTC/USDT", "..\\x", "a b", "C:"] {
            assert!(check_symbol(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn two_threads_writing_one_month_keep_every_minute_both_wrote() {
        const WRITES: i64 = 40;
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = InstrumentId {
            venue: td_core::Venue::Binance,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Spot,
        };
        let jan = Month::containing(1_704_067_200_000); // 2024-01-01
        let candle = |open_time| Candle {
            open_time,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
            volume: 1.0,
            quote_volume: 100.0,
        };
        // One thread writes the even minutes and the other the odd ones, one write per minute, so
        // each write reads, merges and rewrites the same month file the other is rewriting.
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for parity in 0..2 {
                let (store, id, start) = (&store, &id, &start);
                scope.spawn(move || {
                    start.wait();
                    for i in 0..WRITES {
                        let t = jan.start() + (2 * i + parity) * MINUTE;
                        store.write_candles(id, &[candle(t)]).expect("writes");
                    }
                });
            }
        });
        let stored: Vec<Millis> = store
            .read_month(&id, jan)
            .expect("the file stays readable")
            .iter()
            .map(|c| c.open_time)
            .collect();
        let expected: Vec<Millis> = (0..2 * WRITES).map(|i| jan.start() + i * MINUTE).collect();
        assert_eq!(stored, expected);
        let leftovers: Vec<_> =
            fs::read_dir(store.candle_path(&id, jan).unwrap().parent().unwrap())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .filter(|name| name.to_string_lossy().ends_with(".tmp"))
                .collect();
        assert_eq!(
            leftovers,
            Vec::<OsString>::new(),
            "no temp files left behind"
        );
    }

    #[test]
    fn funding_files_out_of_order_or_not_finite_are_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = InstrumentId {
            venue: td_core::Venue::Bybit,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Perpetual,
        };
        let path = store.funding_path(&id).expect("a path");
        for (times, rates) in [
            (vec![2, 1], vec![0.0001, 0.0002]),
            (vec![1, 1], vec![0.0001, 0.0002]),
            (vec![1, 2], vec![0.0001, f64::NAN]),
            (vec![1, 2], vec![f64::INFINITY, 0.0002]),
        ] {
            let columns: Vec<ArrayRef> = vec![
                Arc::new(Int64Array::from(times.clone())),
                Arc::new(Float64Array::from(rates.clone())),
            ];
            write_atomic(
                &path,
                &record_batch(&FUNDING_COLUMNS, columns).expect("a batch"),
            )
            .expect("writes");
            let read = store.load_funding(&id, Millis::MIN, Millis::MAX);
            assert!(
                matches!(read, Err(DataError::BadFile { .. })),
                "{times:?} {rates:?}: {read:?}"
            );
        }
    }
}
