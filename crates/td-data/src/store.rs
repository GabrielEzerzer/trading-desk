// The atomic rename is adapted from tipping-point engine/src/store/run.rs at 7dc6d12 (`rename_new`).

//! The local candle store: Parquet files outside the repo, one per instrument per month of 1-minute
//! candles, `<data_dir>/candles/<venue>/<kind>/<SYMBOL>/1m/<YYYY-MM>.parquet`, and one funding
//! file per perpetual, `<data_dir>/funding/<venue>/<SYMBOL>.parquet`.
//!
//! Every file holds rows sorted by time with no duplicates, and every candle passed
//! [`Candle::validate`] on its way in. Files are replaced atomically: written to a temp file in the
//! same folder, then renamed over the old one. Each read-merge-write of a file holds a lock for
//! that path, in this process and across processes, so two writers (two download jobs, or the two
//! apps' servers sharing one data folder) never lose each other's rows. The cross-process lock is
//! an OS lock on a sidecar file beside the data file, `<name>.parquet.lock`, which stays on disk.
//! Readers take no lock: the rename means they see the old file or the new one, never half of one.
//! Missing minutes are reported, never filled.
//!
//! A kind whose rules require quotes ([`td_core::MarketRules::quotes`], FX) has a bid and an ask
//! price. Its month file holds the bid candle in the candle columns and the ask candle in four more,
//! `ask_open`, `ask_high`, `ask_low` and `ask_close`, written together by [`Store::write_quotes`]
//! under one lock and one rename, so a minute is never stored on one side only. Readers select
//! columns by name, so [`Store::read_month`] reads the bids of such a file as it reads a crypto
//! file, and the listing and delete treat it as any other month file. A kind without quotes is
//! written by [`Store::write_candles`], with no ask columns.
//!
//! Deleting ([`Store::delete_months`], [`Store::delete_instrument`]) is the one way data leaves the
//! store. A delete takes the same lock a writer takes, removes the data file and any temp files a
//! crashed writer left beside it, and lets the lock go. It leaves the sidecar and the folders as
//! they are, and never follows a link (a symlink or a junction) out of the data folder.

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
use td_core::{Candle, InstrumentId, InstrumentKind, Millis, Timeframe, Venue};

use crate::DataError;
use crate::resample::{LoadedCandles, resample};
use crate::time::{MINUTE, Month, format_minute, round_to_minute};

const CANDLE_COLUMNS: [&str; 7] = [
    "open_time",
    "open",
    "high",
    "low",
    "close",
    "volume",
    "quote_volume",
];
/// The ask candle's prices beside the bid candle, in the month file of a kind with quotes. The ask
/// series has no volume of its own.
const ASK_COLUMNS: [&str; 4] = ["ask_open", "ask_high", "ask_low", "ask_close"];
/// What [`Store::read_month_asks`] reads.
const ASK_READ_COLUMNS: [&str; 5] = ["open_time", "ask_open", "ask_high", "ask_low", "ask_close"];
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

/// The missing minutes in `[from, to)` among `candles`, which are sorted by open time, for a market
/// of `kind`. Its closed hours ([`InstrumentKind::closed_spans`]) are never missing: they are cut
/// out of each gap, so an FX week with its weekend absent has no gap, and a gap that runs into the
/// weekend ends at the close. For a market that never closes every absent minute is missing.
pub fn missing_minutes(
    kind: InstrumentKind,
    candles: &[Candle],
    from: Millis,
    to: Millis,
) -> Vec<Gap> {
    missing_among(kind, candles.iter().map(|c| c.open_time), from, to)
}

/// [`missing_minutes`] among sorted open times.
fn missing_among(
    kind: InstrumentKind,
    open_times: impl IntoIterator<Item = Millis>,
    from: Millis,
    to: Millis,
) -> Vec<Gap> {
    let mut gaps = Vec::new();
    let mut push = |from: Millis, to: Millis| {
        let mut start = from;
        for closed in kind.closed_spans(from, to) {
            if start < closed.start {
                gaps.push(Gap {
                    from: start,
                    to: closed.start,
                });
            }
            start = closed.end;
        }
        if start < to {
            gaps.push(Gap { from: start, to });
        }
    };
    let mut expected = from;
    for t in open_times {
        if t < expected {
            continue;
        }
        if t >= to {
            break;
        }
        if t > expected {
            push(expected, t);
        }
        expected = t + MINUTE;
    }
    if expected < to {
        push(expected, to);
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
    /// Missing minutes between the first and last candle given, after the write, by the
    /// instrument's trading hours ([`missing_minutes`]).
    pub gaps: Vec<Gap>,
}

/// Which minutes are stored for one month.
#[derive(Debug, Clone, PartialEq)]
pub struct MonthCoverage {
    pub month: Month,
    /// Stored 1-minute candles.
    pub minutes: usize,
    /// The month's open minutes by the instrument's trading hours, which a complete month stores:
    /// every minute for a market that never closes, the minutes outside its weekends for FX.
    pub open_minutes: i64,
    pub first: Millis,
    pub last: Millis,
    /// Missing minutes between `first` and `last`, by the instrument's trading hours.
    pub gaps: Vec<Gap>,
    /// The month file's size on disk.
    pub bytes: u64,
}

/// One month file of an instrument's candles and its size on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredMonth {
    pub month: Month,
    pub bytes: u64,
}

/// What the store holds for one instrument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredInstrument {
    pub id: InstrumentId,
    /// The month files, in order; empty when only a funding file is stored.
    pub months: Vec<StoredMonth>,
    /// The funding file's size; `None` when there is none, which is always so for spot.
    pub funding_bytes: Option<u64>,
}

impl StoredInstrument {
    /// The month files and the funding file together.
    pub fn bytes(&self) -> u64 {
        self.months.iter().map(|m| m.bytes).sum::<u64>() + self.funding_bytes.unwrap_or(0)
    }
}

/// Everything the store holds, from [`Store::stored`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredData {
    /// Each instrument with at least one month file or a funding file, in instrument order.
    pub instruments: Vec<StoredInstrument>,
    /// Entries under `candles/` and `funding/` that name no venue, kind, symbol or month the store
    /// writes, relative to the store root. They are left alone: nothing lists or deletes them.
    pub unrecognised: Vec<PathBuf>,
}

/// What a delete removed: data files, and temp files a crashed writer left beside them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeleteReport {
    pub files: usize,
    pub bytes: u64,
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
/// Lowercase letters are refused: venues list symbols in uppercase, and on a case-insensitive
/// file system `btcusdt` would share `BTCUSDT`'s folder while bypassing the per-file write locks
/// and the API's one-download-per-instrument check, which compare exact strings.
pub(crate) fn check_symbol(symbol: &str) -> Result<(), DataError> {
    let ok = !symbol.is_empty()
        && symbol
            .chars()
            .all(|c| (c.is_alphanumeric() && !c.is_lowercase()) || c == '-' || c == '_');
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
    /// A kind whose rules require quotes is refused: its candles are written with their asks by
    /// [`Store::write_quotes`].
    pub fn write_candles(
        &self,
        id: &InstrumentId,
        candles: &[Candle],
    ) -> Result<WriteReport, DataError> {
        if id.kind.rules().quotes {
            return Err(DataError::QuotesRequired(id.to_string()));
        }
        for c in candles {
            check_candle(id, c)?;
        }
        self.merge_months(
            id,
            candles,
            |c| c.open_time,
            |month| self.read_month(id, month),
            candle_batch,
        )
    }

    /// Merges the 1-minute bid and ask candles of a kind whose rules require quotes into the store,
    /// as [`Store::write_candles`] merges candles: under the same lock, with the same atomic rename,
    /// a minute replacing a stored one. The ask candle of each minute is stored beside its bid; its
    /// volumes are not stored and read back as 0.
    ///
    /// The batch is refused, and nothing is written, when the kind takes no quotes, when a candle
    /// is not valid or not on a minute, when the `i`-th ask does not have the `i`-th bid's open
    /// time, when an ask's open, high, low or close is below its bid's (a crossed quote), or when a
    /// minute is in the kind's closed hours.
    pub fn write_quotes(
        &self,
        id: &InstrumentId,
        bids: &[Candle],
        asks: &[Candle],
    ) -> Result<WriteReport, DataError> {
        if !id.kind.rules().quotes {
            return Err(DataError::NoQuotes(id.to_string()));
        }
        if bids.len() != asks.len() {
            return Err(DataError::QuoteMinutes {
                instrument: id.to_string(),
                reason: format!("{} bids and {} asks", bids.len(), asks.len()),
            });
        }
        let quotes: Vec<(Candle, Candle)> =
            bids.iter().copied().zip(asks.iter().copied()).collect();
        for (bid, ask) in &quotes {
            check_quote(id, bid, ask)?;
        }
        self.merge_months(
            id,
            &quotes,
            |(bid, _)| bid.open_time,
            |month| self.read_quote_month(id, month),
            quote_batch,
        )
    }

    /// Merges rows, already checked, into their month files: for each month, under its lock, reads
    /// what is stored, lets each new row replace a stored one with the same open time, and writes
    /// the month back atomically.
    fn merge_months<T: Copy>(
        &self,
        id: &InstrumentId,
        rows: &[T],
        open_time: impl Fn(&T) -> Millis,
        read: impl Fn(Month) -> Result<Vec<T>, DataError>,
        batch: impl Fn(&[T]) -> Result<RecordBatch, DataError>,
    ) -> Result<WriteReport, DataError> {
        let mut report = WriteReport::default();
        let mut by_month: BTreeMap<Month, Vec<T>> = BTreeMap::new();
        for row in rows {
            by_month
                .entry(Month::containing(open_time(row)))
                .or_default()
                .push(*row);
        }
        let (Some(first), Some(last)) = (
            rows.iter().map(&open_time).min(),
            rows.iter().map(&open_time).max(),
        ) else {
            return Ok(report);
        };
        let mut merged_span = Vec::new();
        for (month, new) in by_month {
            let path = self.candle_path(id, month)?;
            let lock = file_lock(&path);
            let _held = hold(&lock);
            let _locked = lock_across_processes(&path)?;
            let mut merged: BTreeMap<Millis, T> = read(month)?
                .into_iter()
                .map(|r| (open_time(&r), r))
                .collect();
            for row in new {
                if merged.insert(open_time(&row), row).is_some() {
                    report.duplicates += 1;
                }
            }
            let merged: Vec<T> = merged.into_values().collect();
            write_atomic(&path, &batch(&merged)?)?;
            report.months.push(month);
            merged_span.extend(
                merged
                    .iter()
                    .map(&open_time)
                    .filter(|t| (first..=last).contains(t)),
            );
        }
        report.gaps = missing_among(id.kind, merged_span, first, last + MINUTE);
        Ok(report)
    }

    /// One month of an instrument's 1-minute candles, sorted; empty when none are stored. For a
    /// kind with quotes these are the bids.
    pub fn read_month(&self, id: &InstrumentId, month: Month) -> Result<Vec<Candle>, DataError> {
        self.read_rows(id, month, &CANDLE_COLUMNS)
    }

    /// One month of the 1-minute ask candles of a kind whose rules require quotes, sorted, with the
    /// same open times as [`Store::read_month`] returns for the month; empty when none are stored.
    /// Their volumes are 0. A kind without quotes is refused, and so is a month file that has no
    /// ask columns ([`DataError::NoAskColumns`]).
    pub fn read_month_asks(
        &self,
        id: &InstrumentId,
        month: Month,
    ) -> Result<Vec<Candle>, DataError> {
        if !id.kind.rules().quotes {
            return Err(DataError::NoQuotes(id.to_string()));
        }
        let path = self.candle_path(id, month)?;
        if has_columns(&path, &ASK_COLUMNS)? == Some(false) {
            return Err(DataError::NoAskColumns { path });
        }
        self.read_rows(id, month, &ASK_READ_COLUMNS)
    }

    /// One month's stored bids and asks, paired by minute.
    fn read_quote_month(
        &self,
        id: &InstrumentId,
        month: Month,
    ) -> Result<Vec<(Candle, Candle)>, DataError> {
        let asks = self.read_month_asks(id, month)?;
        // Both come from the one file's rows, so they pair up row by row.
        Ok(self.read_month(id, month)?.into_iter().zip(asks).collect())
    }

    /// The candles of one month file, sorted, from the columns `names`: the open time, the open,
    /// high, low and close, and optionally the two volumes, which are 0 when not named. Empty when
    /// the file does not exist.
    fn read_rows(
        &self,
        id: &InstrumentId,
        month: Month,
        names: &[&str],
    ) -> Result<Vec<Candle>, DataError> {
        let path = self.candle_path(id, month)?;
        let Some(columns) = read_columns(&path, names)? else {
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
        let volume = |i: usize, row: usize| values.get(i).map_or(0.0, |v| v[row]);
        let mut candles = Vec::with_capacity(open_time.len());
        let mut previous = Millis::MIN;
        for (row, &t) in open_time.iter().enumerate() {
            let candle = Candle {
                open_time: t,
                open: values[0][row],
                high: values[1][row],
                low: values[2][row],
                close: values[3][row],
                volume: volume(4, row),
                quote_volume: volume(5, row),
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

    /// Which months and minutes are stored for an instrument, with gaps by its kind's trading hours
    /// ([`missing_minutes`]).
    pub fn coverage(&self, id: &InstrumentId) -> Result<Vec<MonthCoverage>, DataError> {
        let mut coverage = Vec::new();
        for month in self.months(id)? {
            let candles = self.read_month(id, month)?;
            let (Some(first), Some(last)) = (candles.first(), candles.last()) else {
                continue;
            };
            let closed: Millis = id
                .kind
                .closed_spans(month.start(), month.end())
                .iter()
                .map(|s| s.end - s.start)
                .sum();
            coverage.push(MonthCoverage {
                month,
                minutes: candles.len(),
                open_minutes: month.minutes() - closed / MINUTE,
                first: first.open_time,
                last: last.open_time,
                gaps: missing_minutes(id.kind, &candles, first.open_time, last.open_time + MINUTE),
                bytes: file_bytes(&self.candle_path(id, month)?)?.unwrap_or(0),
            });
        }
        Ok(coverage)
    }

    /// Bars of `timeframe` resampled from the stored 1-minute candles with open times in
    /// `[from, to)`, by the rules of [`resample`] for the instrument's kind.
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
        Ok(resample(id.kind, &minutes, timeframe, include_incomplete))
    }

    /// Ask bars of `timeframe` for a kind whose rules require quotes, resampled from the stored
    /// 1-minute asks with open times in `[from, to)` as [`Store::load_candles`] resamples the bids:
    /// the same minutes, so the same buckets are built, dropped and tolerated, and each ask bar is
    /// the exact OHLC of its minutes' asks.
    pub fn load_asks(
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
                self.read_month_asks(id, month)?
                    .into_iter()
                    .filter(|c| (from..to).contains(&c.open_time)),
            );
        }
        Ok(resample(id.kind, &minutes, timeframe, include_incomplete))
    }

    /// Merges funding rates `(funding time, rate)` into a perpetual's funding file, replacing any
    /// stored rate with the same time. Times are rounded to the nearest minute first, the rule the
    /// venue parsers apply (see [`round_to_minute`]). Returns the number of rates stored after the
    /// write.
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
        let _locked = lock_across_processes(&path)?;
        let mut merged: BTreeMap<Millis, f64> = self.read_funding(id)?.into_iter().collect();
        merged.extend(rates.iter().map(|&(t, rate)| (round_to_minute(t), rate)));
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
    ///
    /// Times are rounded to the nearest minute as they are read, so a file written before the
    /// venue parsers rounded them reads the same as one written after; the next write of the file
    /// stores them rounded. Two stamps that round to the same minute keep the later rate, with a
    /// warning.
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
            let t = round_to_minute(t);
            // Two stamps in one minute, from a file written before times were rounded: keep the
            // later, so the next write repairs the file instead of failing on it.
            if let Some(last) = rates.last_mut().filter(|(last, _)| *last == t) {
                eprintln!(
                    "warning: {}: row {row} is on the same minute as the row before; keeping \
                     the later rate",
                    path.display()
                );
                *last = (t, rate);
                continue;
            }
            rates.push((t, rate));
        }
        Ok(rates)
    }

    /// Every instrument the store holds, with each month file's size and the funding file's size.
    /// Entries that name no venue, kind, symbol or month are reported in
    /// [`StoredData::unrecognised`], not read.
    pub fn stored(&self) -> Result<StoredData, DataError> {
        let mut found: BTreeMap<InstrumentId, StoredInstrument> = BTreeMap::new();
        let mut unrecognised = Vec::new();
        for (venue, venue_dir) in self.venue_dirs("candles", &mut unrecognised)? {
            let Some(venue) = parse_name::<Venue>(&venue) else {
                unrecognised.push(self.relative(&venue_dir));
                continue;
            };
            for (kind, kind_dir) in self.named_dirs(&venue_dir, &mut unrecognised)? {
                let Some(kind) = parse_name::<InstrumentKind>(&kind) else {
                    unrecognised.push(self.relative(&kind_dir));
                    continue;
                };
                for (symbol, symbol_dir) in self.named_dirs(&kind_dir, &mut unrecognised)? {
                    if check_symbol(&symbol).is_err() {
                        unrecognised.push(self.relative(&symbol_dir));
                        continue;
                    }
                    // Months are read only from a `1m` that is a real folder; `named_dirs` reports a
                    // file or a link of that name.
                    let mut months = Vec::new();
                    for (name, dir) in self.named_dirs(&symbol_dir, &mut unrecognised)? {
                        if name != "1m" {
                            unrecognised.push(self.relative(&dir));
                            continue;
                        }
                        for name in dir_names(&dir, &mut unrecognised, &self.root)? {
                            match month_of_file(&name) {
                                Some((month, true)) => {
                                    if let Some(bytes) = file_bytes(&dir.join(&name))? {
                                        months.push(StoredMonth { month, bytes });
                                    }
                                }
                                Some((_, false)) => {} // a lock sidecar or a temp file
                                None => unrecognised.push(self.relative(&dir.join(&name))),
                            }
                        }
                    }
                    months.sort_by_key(|m| m.month);
                    if !months.is_empty() {
                        let id = InstrumentId {
                            venue,
                            symbol,
                            kind,
                        };
                        found.insert(
                            id.clone(),
                            StoredInstrument {
                                id,
                                months,
                                funding_bytes: None,
                            },
                        );
                    }
                }
            }
        }
        for (venue, venue_dir) in self.venue_dirs("funding", &mut unrecognised)? {
            let Some(venue) = parse_name::<Venue>(&venue) else {
                unrecognised.push(self.relative(&venue_dir));
                continue;
            };
            for name in dir_names(&venue_dir, &mut unrecognised, &self.root)? {
                let parsed = name
                    .split_once(".parquet")
                    .filter(|(symbol, rest)| check_symbol(symbol).is_ok() && is_store_suffix(rest));
                let Some((symbol, rest)) = parsed else {
                    unrecognised.push(self.relative(&venue_dir.join(&name)));
                    continue;
                };
                if !rest.is_empty() {
                    continue; // its lock sidecar or a temp file
                }
                let Some(bytes) = file_bytes(&venue_dir.join(&name))? else {
                    continue;
                };
                let id = InstrumentId {
                    venue,
                    symbol: symbol.to_string(),
                    kind: InstrumentKind::Perpetual,
                };
                found
                    .entry(id.clone())
                    .or_insert_with(|| StoredInstrument {
                        id,
                        months: Vec::new(),
                        funding_bytes: None,
                    })
                    .funding_bytes = Some(bytes);
            }
        }
        unrecognised.sort();
        Ok(StoredData {
            instruments: found.into_values().collect(),
            unrecognised,
        })
    }

    /// Deletes an instrument's candles for the months `from` to `to`, both included. Each month
    /// file is removed under the same lock a writer takes for it, with any temp files a crashed
    /// writer left beside it; its lock sidecar and the folders stay. Months not stored are skipped:
    /// deleting nothing is not an error, and creates nothing. When a folder on the way to the
    /// month files is missing or is a link, nothing is deleted. The funding file stays.
    pub fn delete_months(
        &self,
        id: &InstrumentId,
        from: Month,
        to: Month,
    ) -> Result<DeleteReport, DataError> {
        let dir = self.candle_dir(id)?;
        let mut report = DeleteReport::default();
        if !self.real_dir(&dir)? {
            return Ok(report);
        }
        for name in dir_names(&dir, &mut Vec::new(), &self.root)? {
            if let Some((month, true)) = month_of_file(&name)
                && (from..=to).contains(&month)
            {
                remove_locked(&dir.join(name), &mut report)?;
            }
        }
        Ok(report)
    }

    /// Deletes every month of an instrument's candles and, for a perpetual, its funding file, as
    /// [`Store::delete_months`] does.
    pub fn delete_instrument(&self, id: &InstrumentId) -> Result<DeleteReport, DataError> {
        let every_month = (
            Month {
                year: i64::MIN,
                month: 1,
            },
            Month {
                year: i64::MAX,
                month: 12,
            },
        );
        let mut report = self.delete_months(id, every_month.0, every_month.1)?;
        if id.kind == InstrumentKind::Perpetual {
            let path = self.funding_path(id)?;
            if let Some(dir) = path.parent()
                && self.real_dir(dir)?
            {
                remove_locked(&path, &mut report)?;
            }
        }
        Ok(report)
    }

    /// The venue folders under the root's `candles` or `funding`. A link or a file of that name
    /// is reported and not read, as [`Store::named_dirs`] does for everything below it, so the
    /// listing never shows data a delete refuses to touch.
    fn venue_dirs(
        &self,
        top: &str,
        unrecognised: &mut Vec<PathBuf>,
    ) -> Result<Vec<(String, PathBuf)>, DataError> {
        let dir = self.root.join(top);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if !meta.is_dir() => {
                unrecognised.push(self.relative(&dir));
                Ok(Vec::new())
            }
            _ => self.named_dirs(&dir, unrecognised),
        }
    }

    /// The subfolders of `dir` with their names, sorted; a missing `dir` has none. A file, or a
    /// folder whose name is not UTF-8, goes to `unrecognised`.
    fn named_dirs(
        &self,
        dir: &Path,
        unrecognised: &mut Vec<PathBuf>,
    ) -> Result<Vec<(String, PathBuf)>, DataError> {
        let mut dirs = Vec::new();
        for entry in read_dir_or_empty(dir)? {
            let entry = entry.map_err(DataError::io(dir))?;
            let path = entry.path();
            let is_dir = entry.file_type().map_err(DataError::io(&path))?.is_dir();
            match entry.file_name().into_string() {
                Ok(name) if is_dir => dirs.push((name, path)),
                _ => unrecognised.push(self.relative(&path)),
            }
        }
        dirs.sort();
        Ok(dirs)
    }

    fn relative(&self, path: &Path) -> PathBuf {
        relative_to(&self.root, path)
    }

    /// Whether `dir` and each folder between the store root and it are real folders, checked
    /// without following links: `false` when one is missing, or is a file or a link (a symlink or
    /// a junction), which could lead a delete out of the data folder. The root itself may be a
    /// link.
    // shortcut: checked once, before the delete lists anything, so a folder swapped for a link
    // while the delete runs would be followed; only a program that can already write to the data
    // folder can do that. Open each folder relative to its parent if that ever matters.
    fn real_dir(&self, dir: &Path) -> Result<bool, DataError> {
        let Ok(below_root) = dir.strip_prefix(&self.root) else {
            return Ok(false);
        };
        let mut path = self.root.clone();
        for part in below_root {
            path.push(part);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(DataError::io(&path)(e)),
            }
        }
        Ok(true)
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

/// A minute of bid and ask: each candle valid and on a minute, the same open time, the ask's open,
/// high, low and close not below the bid's, and the market open.
fn check_quote(id: &InstrumentId, bid: &Candle, ask: &Candle) -> Result<(), DataError> {
    check_candle(id, bid)?;
    check_candle(id, ask)?;
    let instrument = || id.to_string();
    if bid.open_time != ask.open_time {
        return Err(DataError::QuoteMinutes {
            instrument: instrument(),
            reason: format!(
                "the bid at {} UTC is paired with the ask at {} UTC",
                format_minute(bid.open_time),
                format_minute(ask.open_time)
            ),
        });
    }
    // The ask is at or above the bid at every instant, so on each of open, high, low and close:
    // the engine refuses a bar crossed on any of them, so the store does too.
    for (field, b, a) in [
        ("open", bid.open, ask.open),
        ("high", bid.high, ask.high),
        ("low", bid.low, ask.low),
        ("close", bid.close, ask.close),
    ] {
        if a < b {
            return Err(DataError::CrossedQuote {
                instrument: instrument(),
                open_time: bid.open_time,
                field,
                bid: b,
                ask: a,
            });
        }
    }
    if !id.kind.is_open(bid.open_time) {
        return Err(DataError::ClosedMinute {
            instrument: instrument(),
            open_time: bid.open_time,
        });
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

/// The bid candle's columns and the ask candle's prices beside them.
fn quote_batch(quotes: &[(Candle, Candle)]) -> Result<RecordBatch, DataError> {
    let (bids, asks): (Vec<Candle>, Vec<Candle>) = quotes.iter().copied().unzip();
    let float = |f: fn(&Candle) -> f64| -> ArrayRef {
        Arc::new(Float64Array::from_iter_values(asks.iter().map(f)))
    };
    let mut names = CANDLE_COLUMNS.to_vec();
    names.extend(ASK_COLUMNS);
    let mut columns = candle_batch(&bids)?.columns().to_vec();
    columns.extend([
        float(|c| c.open),
        float(|c| c.high),
        float(|c| c.low),
        float(|c| c.close),
    ]);
    record_batch(&names, columns)
}

/// The lock that serialises read-merge-write of one store file among this process's threads.
/// Callers take it, then [`lock_across_processes`], and hold both from the read through the
/// rename. The OS lock alone would be enough, but this keeps waiting threads on a cheap mutex
/// instead of each holding an open lock file in the OS lock queue.
// shortcut: keyed by the path as built from the store root, so two `Store`s that spell one folder
// differently (relative and absolute) do not share this mutex, and entries are never removed (one
// per file written, a few hundred bytes each); canonicalise and evict if either ever matters. The
// OS lock taken next still serialises such writers, since it is keyed by the file itself.
fn file_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut locks = hold(LOCKS.get_or_init(Mutex::default));
    locks.entry(path.to_path_buf()).or_default().clone()
}

/// Takes the exclusive OS lock that serialises read-merge-write of the store file at `path` across
/// processes, waiting for any other holder, and returns the open lock file; the lock lasts until it
/// is dropped. Callers take it after [`file_lock`] and hold it from the read through the rename.
///
/// The lock is on a sidecar file, `<file name>.lock` beside `path`, not on the data file: on Windows
/// a lock or open handle on the data file would make the rename over it fail. The sidecar is never
/// deleted, since a writer could otherwise lock a file another writer is about to remove and both
/// would proceed. It is an OS lock (`flock` on Unix, `LockFileEx` on Windows) bound to the open
/// handle, so the OS releases it when the holder exits or is killed: a crash cannot leave a file
/// locked, as a create-exclusive marker file would.
fn lock_across_processes(path: &Path) -> Result<File, DataError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(DataError::io(dir))?;
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".lock");
    let lock_path = dir.join(name);
    let file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(DataError::io(&lock_path))?;
    file.lock().map_err(DataError::io(&lock_path))?;
    Ok(file)
}

/// Removes the store file at `path` under its lock, taken as a writer takes it, with any temp files
/// a crashed writer left beside it, and adds them to `report`; the lock sidecar stays as it is.
/// While the lock is held no writer is part way through this file, in this process or another, so
/// every temp file of it is a leftover. When `path` is not a regular file (nothing, or a link),
/// there is nothing to delete and no lock is taken.
fn remove_locked(path: &Path, report: &mut DeleteReport) -> Result<(), DataError> {
    if file_bytes(path)?.is_none() {
        return Ok(());
    }
    let lock = file_lock(path);
    let _held = hold(&lock);
    let _locked = lock_across_processes(path)?;
    for file in std::iter::once(path.to_path_buf()).chain(temp_files(path)?) {
        let Some(bytes) = file_bytes(&file)? else {
            continue;
        };
        match retry_denied(|| fs::remove_file(&file)) {
            Ok(()) => {
                report.files += 1;
                report.bytes += bytes;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                let DeleteReport { files, bytes } = *report;
                let stopped = format!(
                    "{e} (the delete stopped; files removed before it: {files}, {bytes} bytes)"
                );
                return Err(DataError::io(&file)(io::Error::new(e.kind(), stopped)));
            }
        }
    }
    Ok(())
}

/// The temp files [`write_atomic`] names after the store file at `path` that are beside it.
fn temp_files(path: &Path) -> Result<Vec<PathBuf>, DataError> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return Ok(Vec::new());
    };
    let prefix = format!("{name}.");
    Ok(dir_names(dir, &mut Vec::new(), dir)?
        .into_iter()
        .filter(|n| n.starts_with(&prefix) && n.ends_with(".tmp"))
        .map(|n| dir.join(n))
        .collect())
}

/// The names of the regular files in `dir`, sorted; a missing `dir` has none. Anything else (a
/// subfolder, a link, or a file whose name is not UTF-8) goes to `unrecognised`, as a path
/// relative to `root`.
fn dir_names(
    dir: &Path,
    unrecognised: &mut Vec<PathBuf>,
    root: &Path,
) -> Result<Vec<String>, DataError> {
    let mut names = Vec::new();
    for entry in read_dir_or_empty(dir)? {
        let entry = entry.map_err(DataError::io(dir))?;
        let is_file = entry
            .file_type()
            .map_err(DataError::io(entry.path()))?
            .is_file();
        match entry.file_name().into_string() {
            Ok(name) if is_file => names.push(name),
            _ => unrecognised.push(relative_to(root, &entry.path())),
        }
    }
    names.sort();
    Ok(names)
}

/// The entries of `dir`; none when it does not exist.
fn read_dir_or_empty(
    dir: &Path,
) -> Result<impl Iterator<Item = io::Result<fs::DirEntry>>, DataError> {
    match fs::read_dir(dir) {
        Ok(entries) => Ok(Some(entries).into_iter().flatten()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None.into_iter().flatten()),
        Err(e) => Err(DataError::io(dir)(e)),
    }
}

fn relative_to(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

/// The size of the regular file at `path`, not following a link; `None` when there is none, or a
/// folder or a link is there instead.
fn file_bytes(path: &Path) -> Result<Option<u64>, DataError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.is_file().then_some(meta.len())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(DataError::io(path)(e)),
    }
}

/// The month a file in an instrument's `1m` folder belongs to, and whether it is the month file
/// itself rather than its lock sidecar or a temp file; `None` for any other name.
fn month_of_file(name: &str) -> Option<(Month, bool)> {
    let (month, rest) = name.split_once(".parquet")?;
    let month = Month::parse(month).filter(|_| is_store_suffix(rest))?;
    Some((month, rest.is_empty()))
}

/// Whether `rest`, what follows `.parquet` in a file name, is one the store writes: nothing (the
/// data file), `.lock` (its sidecar) or `.<pid>.<n>.tmp` (a temp file).
fn is_store_suffix(rest: &str) -> bool {
    rest.is_empty() || rest == ".lock" || (rest.starts_with('.') && rest.ends_with(".tmp"))
}

/// A venue or kind from the folder name it is stored under, which is its serde name.
fn parse_name<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
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

/// `fs::rename`, retried briefly as [`retry_denied`] does.
fn rename_replacing(from: &Path, to: &Path) -> io::Result<()> {
    retry_denied(|| fs::rename(from, to))
}

/// Runs `op`, retried briefly when Windows refuses it because another process (an indexer or a
/// virus scanner) holds one of the files open.
fn retry_denied<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut wait = Duration::from_millis(25);
    for _ in 0..6 {
        match op() {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                sleep(wait);
                wait *= 2;
            }
            done => return done,
        }
    }
    op()
}

/// Whether the Parquet file at `path` has every column in `names`, from its footer; `None` when the
/// file does not exist.
fn has_columns(path: &Path, names: &[&str]) -> Result<Option<bool>, DataError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(DataError::io(path)(e)),
    };
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|source| DataError::Parquet {
            path: path.to_path_buf(),
            source,
        })?;
    let schema = builder.schema();
    Ok(Some(
        names
            .iter()
            .all(|name| schema.column_with_name(name).is_some()),
    ))
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
        for bad in [
            "", "..", "BTC/USDT", "..\\x", "a b", "C:", "btcusdt", "BTCusdt",
        ] {
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

    /// Set in a child process started by [`spawn_child`] to the role it plays in its test.
    const CHILD_ROLE: &str = "TD_STORE_TEST_CHILD_ROLE";
    /// Set in a child process to the folder it shares with its parent test.
    const CHILD_DIR: &str = "TD_STORE_TEST_CHILD_DIR";

    /// The folder shared with the parent, when this process is a child playing `role`.
    fn child_dir(role: &str) -> Option<PathBuf> {
        if std::env::var(CHILD_ROLE).ok()? != role {
            return None;
        }
        std::env::var_os(CHILD_DIR).map(PathBuf::from)
    }

    /// Starts this test binary again as a second OS process that runs only the test `name`, as a
    /// child playing `role` with the folder `dir`.
    fn spawn_child(name: &str, role: &str, dir: &Path) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD_ROLE, role)
            .env(CHILD_DIR, dir)
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("the child process starts")
    }

    /// Waits up to 60 s for `path` to exist.
    fn wait_for(path: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn two_processes_writing_one_month_keep_every_minute_both_wrote() {
        const NAME: &str =
            "store::tests::two_processes_writing_one_month_keep_every_minute_both_wrote";
        const ROLE: &str = "odd minutes";
        const WRITES: i64 = 60;
        let id = InstrumentId {
            venue: td_core::Venue::Binance,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Spot,
        };
        let jan = Month::containing(1_704_067_200_000); // 2024-01-01
        // One process writes the even minutes and the other the odd ones, one write per minute, so
        // each write reads, merges and rewrites the same month file the other is rewriting.
        let write_minutes = |root: &Path, parity: i64| {
            let store = Store::new(root);
            for i in 0..WRITES {
                let candle = Candle {
                    open_time: jan.start() + (2 * i + parity) * MINUTE,
                    open: 100.0,
                    high: 101.0,
                    low: 99.0,
                    close: 100.5,
                    volume: 1.0,
                    quote_volume: 100.0,
                };
                store.write_candles(&id, &[candle]).expect("writes");
            }
        };
        if let Some(dir) = child_dir(ROLE) {
            fs::write(dir.join("ready"), b"").expect("signals ready");
            wait_for(&dir.join("go"));
            write_minutes(&dir.join("store"), 1);
            return;
        }

        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("store");
        let mut child = spawn_child(NAME, ROLE, dir.path());
        wait_for(&dir.path().join("ready"));
        fs::write(dir.path().join("go"), b"").expect("starts the child");
        write_minutes(&root, 0);
        let status = child.wait().expect("the child finishes");
        assert!(status.success(), "the child process failed: {status}");

        let store = Store::new(&root);
        let stored: Vec<Millis> = store
            .read_month(&id, jan)
            .expect("the file stays readable")
            .iter()
            .map(|c| c.open_time)
            .collect();
        let expected: Vec<Millis> = (0..2 * WRITES).map(|i| jan.start() + i * MINUTE).collect();
        assert_eq!(stored, expected);
        let path = store.candle_path(&id, jan).expect("a path");
        assert!(
            path.with_extension("parquet.lock").exists(),
            "the lock file stays"
        );
        assert_eq!(
            store.months(&id).expect("lists"),
            [jan],
            "lock files are not months"
        );
        assert_eq!(store.coverage(&id).expect("covers").len(), 1);
    }

    #[test]
    fn a_killed_writer_leaves_no_file_locked() {
        const NAME: &str = "store::tests::a_killed_writer_leaves_no_file_locked";
        const ROLE: &str = "lock holder";
        if let Some(dir) = child_dir(ROLE) {
            let _locked = lock_across_processes(&dir.join("2024-01.parquet")).expect("locks");
            fs::write(dir.join("ready"), b"").expect("signals ready");
            sleep(Duration::from_secs(60)); // killed long before this ends
            return;
        }

        let dir = tempfile::tempdir().expect("a temp dir");
        let mut child = spawn_child(NAME, ROLE, dir.path());
        wait_for(&dir.path().join("ready"));
        let lock_file = File::options()
            .write(true)
            .open(dir.path().join("2024-01.parquet.lock"))
            .expect("the child made the lock file");
        let held = lock_file.try_lock();
        let _ = child.kill();
        child.wait().expect("the child is gone");
        assert!(
            matches!(held, Err(fs::TryLockError::WouldBlock)),
            "the lock is held while its holder lives: {held:?}"
        );
        // Windows releases a dead process's locks soon after it exits, not always at once.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match lock_file.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("the killed holder's lock was not released: {e}"),
            }
        }
    }

    fn spot_id() -> InstrumentId {
        InstrumentId {
            venue: td_core::Venue::Binance,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Spot,
        }
    }

    fn flat_candle(open_time: Millis) -> Candle {
        Candle {
            open_time,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
            volume: 1.0,
            quote_volume: 100.0,
        }
    }

    #[test]
    fn a_delete_waits_for_the_lock_a_writer_holds() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = spot_id();
        let jan = Month::containing(1_704_067_200_000); // 2024-01-01
        store
            .write_candles(&id, &[flat_candle(jan.start())])
            .expect("writes");
        let path = store.candle_path(&id, jan).expect("a path");
        // Hold the lock as a writer does, from another process's point of view: the OS lock only.
        let held = lock_across_processes(&path).expect("locks");
        std::thread::scope(|scope| {
            let delete = scope.spawn(|| store.delete_months(&id, jan, jan));
            sleep(Duration::from_millis(300));
            assert!(!delete.is_finished(), "the delete waits for the lock");
            assert!(path.exists(), "nothing is removed while the lock is held");
            drop(held);
            let report = delete.join().expect("no panic").expect("deletes");
            assert_eq!(report.files, 1);
        });
        assert!(!path.exists());
        let sidecar = fs::metadata(path.with_extension("parquet.lock")).expect("the sidecar stays");
        assert_eq!(sidecar.len(), 0, "and the delete wrote nothing into it");
    }

    #[test]
    fn a_writer_and_a_delete_on_one_month_leave_it_readable_and_whole() {
        const WRITES: i64 = 60;
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = spot_id();
        let jan = Month::containing(1_704_067_200_000); // 2024-01-01
        let start = std::sync::Barrier::new(2);
        let deletes = std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                start.wait();
                for i in 0..WRITES {
                    store
                        .write_candles(&id, &[flat_candle(jan.start() + i * MINUTE)])
                        .expect("every write succeeds");
                }
            });
            start.wait();
            let mut deletes = 0;
            // Until the writer is done or has failed: a failed write must end the test, not leave
            // this loop running.
            while !writer.is_finished() {
                let report = store.delete_months(&id, jan, jan).expect("deletes");
                deletes += report.files;
                // Between deletes, the file always reads whole.
                let stored = store.read_month(&id, jan).expect("stays readable");
                assert!(stored.windows(2).all(|w| w[0].open_time < w[1].open_time));
            }
            writer.join().expect("every write succeeds");
            deletes
        });
        assert!(deletes > 0, "the delete ran while the writer did");
        // Every write merged into what was there, so what is left is the minutes written since the
        // last delete: an unbroken run ending at the last minute written, or nothing when the last
        // delete came after the last write.
        let stored: Vec<Millis> = store
            .read_month(&id, jan)
            .expect("the file stays readable")
            .iter()
            .map(|c| c.open_time)
            .collect();
        if let Some(&first) = stored.first() {
            let last = jan.start() + (WRITES - 1) * MINUTE;
            let expected: Vec<Millis> = (first..=last).step_by(MINUTE as usize).collect();
            assert_eq!(stored, expected);
        }
        let folder = store
            .candle_path(&id, jan)
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let leftovers: Vec<_> = read_dir_or_empty(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert_eq!(leftovers, Vec::<OsString>::new(), "no temp files left");
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

    #[test]
    fn funding_files_with_offset_stamps_read_back_rounded_to_the_minute() {
        // A file written before funding times were rounded at parsing: 08:00 stamped 8 ms late
        // and 16:00 stamped 5 ms early. They read back on the minute, and a new write of the same
        // funding replaces them instead of sitting beside them.
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = InstrumentId {
            venue: td_core::Venue::Binance,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Perpetual,
        };
        let path = store.funding_path(&id).expect("a path");
        let t = 1_704_096_000_000;
        let h8 = 8 * 3_600_000;
        let columns: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![t + 8, t + h8 - 5])),
            Arc::new(Float64Array::from(vec![0.0001, 0.0002])),
        ];
        write_atomic(
            &path,
            &record_batch(&FUNDING_COLUMNS, columns).expect("a batch"),
        )
        .expect("writes");
        assert_eq!(
            store.load_funding(&id, t, t + 2 * h8).expect("loads"),
            [(t, 0.0001), (t + h8, 0.0002)]
        );
        // Loading from the hour itself finds the rate stamped just after it.
        assert_eq!(
            store.load_funding(&id, t + h8, t + 2 * h8).expect("loads"),
            [(t + h8, 0.0002)]
        );
        assert_eq!(
            store
                .write_funding(&id, &[(t + h8, 0.0003), (t + 2 * h8 + 3, 0.0004)])
                .expect("writes"),
            3
        );
        assert_eq!(
            store.load_funding(&id, t, t + 3 * h8).expect("loads"),
            [(t, 0.0001), (t + h8, 0.0003), (t + 2 * h8, 0.0004)]
        );
        // The file itself now holds the times on the minute.
        assert_eq!(raw_funding_times(&store, &id), [t, t + h8, t + 2 * h8]);
    }

    fn funding_id() -> InstrumentId {
        InstrumentId {
            venue: td_core::Venue::Binance,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Perpetual,
        }
    }

    /// The funding times as stored in the file, without the rounding applied on reading.
    fn raw_funding_times(store: &Store, id: &InstrumentId) -> Vec<Millis> {
        let path = store.funding_path(id).expect("a path");
        let columns = read_columns(&path, &FUNDING_COLUMNS)
            .expect("reads")
            .expect("a file");
        int_column(&path, &columns[0]).expect("times").to_vec()
    }

    #[test]
    fn funding_is_written_with_its_times_rounded_to_the_minute() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = funding_id();
        let t = 1_704_096_000_000;
        store
            .write_funding(&id, &[(t + 8, 0.0001), (t + 8 * 3_600_000 - 5, 0.0002)])
            .expect("writes");
        assert_eq!(raw_funding_times(&store, &id), [t, t + 8 * 3_600_000]);
    }

    #[test]
    fn a_funding_file_with_two_stamps_in_one_minute_keeps_the_later_and_is_repaired_on_write() {
        // Written before times were rounded: 00:00 stamped 5 ms early, then again 8 ms late.
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = funding_id();
        let path = store.funding_path(&id).expect("a path");
        let t = 1_704_096_000_000;
        let h8 = 8 * 3_600_000;
        let columns: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![t - 5, t + 8])),
            Arc::new(Float64Array::from(vec![0.0001, 0.0002])),
        ];
        write_atomic(
            &path,
            &record_batch(&FUNDING_COLUMNS, columns).expect("a batch"),
        )
        .expect("writes");
        assert_eq!(
            store.load_funding(&id, t - h8, t + h8).expect("loads"),
            [(t, 0.0002)]
        );
        assert_eq!(
            store
                .write_funding(&id, &[(t + h8, 0.0003)])
                .expect("writes"),
            2
        );
        assert_eq!(raw_funding_times(&store, &id), [t, t + h8]);
    }
}
