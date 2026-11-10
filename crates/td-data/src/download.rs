//! Downloading a range of history into the store, one month at a time so a long download keeps
//! what it has fetched if it stops part way.

use td_core::{Candle, InstrumentId, InstrumentKind, Millis, Timeframe, Venue};

use crate::binance::{self, ArchivePeriod};
use crate::http::Http;
use crate::store::{Gap, Store, missing_minutes};
use crate::time::{DAY, MINUTE, Month, now_ms};
use crate::{DataError, bybit};

/// What a candle download did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DownloadReport {
    /// Candles fetched per month.
    pub months: Vec<(Month, usize)>,
    /// Archive files the venue does not have (before the symbol listed, or not yet published).
    pub missing_files: Vec<String>,
    /// Minutes in the range, up to now, that are still not stored after the download.
    pub gaps: Vec<Gap>,
}

/// Downloads the 1-minute candles of `id` with open times in `[from, to)` and merges them into the
/// store. `progress` is called with a line of text after each month.
///
/// Binance comes from the public archive: a monthly file for each whole past month (falling back
/// to daily files if the monthly one is not published yet) and daily files for the current month,
/// which exist up to yesterday. Bybit comes from its REST API.
pub fn download_candles(
    http: &Http,
    store: &Store,
    id: &InstrumentId,
    from: Millis,
    to: Millis,
    progress: &mut dyn FnMut(String),
) -> Result<DownloadReport, DataError> {
    let now = now_ms();
    let this_month = Month::containing(now);
    let mut report = DownloadReport::default();
    for month in Month::range(from, to.min(now)) {
        let lo = from.max(month.start());
        let hi = to.min(month.end()).min(now);
        let mut candles = match id.venue {
            Venue::Binance => {
                let monthly = if month < this_month {
                    binance::fetch_archive(http, id, ArchivePeriod::Month(month))?
                } else {
                    None
                };
                match monthly {
                    Some(candles) => candles,
                    None => binance_days(http, id, lo, hi, &mut report.missing_files)?,
                }
            }
            Venue::Bybit => bybit::fetch_klines(http, id, lo, hi)?,
        };
        candles.retain(|c| (lo..hi).contains(&c.open_time));
        store.write_candles(id, &candles)?;
        progress(format!("{id} {month}: {} candles", candles.len()));
        report.months.push((month, candles.len()));
    }
    let end = to.min(now - now.rem_euclid(MINUTE));
    if from < end {
        let stored = store.load_candles(id, Timeframe::M1, from, end, true)?;
        report.gaps = missing_minutes(&stored, from, end);
    }
    Ok(report)
}

/// The daily archive files for the days that overlap `[lo, hi)`, up to yesterday.
fn binance_days(
    http: &Http,
    id: &InstrumentId,
    lo: Millis,
    hi: Millis,
    missing: &mut Vec<String>,
) -> Result<Vec<Candle>, DataError> {
    let today = now_ms().div_euclid(DAY) * DAY;
    let mut candles = Vec::new();
    let mut day = lo.div_euclid(DAY) * DAY;
    while day < hi && day < today {
        let period = ArchivePeriod::Day(day);
        match binance::fetch_archive(http, id, period)? {
            Some(rows) => candles.extend(rows),
            None => missing.push(binance::archive_file_name(&id.symbol, period)),
        }
        day += DAY;
    }
    Ok(candles)
}

/// Downloads the funding rates of a perpetual in `[from, to)` and merges them into the store.
/// Returns the number of rates fetched.
pub fn download_funding(
    http: &Http,
    store: &Store,
    id: &InstrumentId,
    from: Millis,
    to: Millis,
) -> Result<usize, DataError> {
    if id.kind != InstrumentKind::Perpetual {
        return Err(DataError::NotPerpetual(id.to_string()));
    }
    let rates = match id.venue {
        Venue::Binance => binance::fetch_funding(http, &id.symbol, from, to)?,
        Venue::Bybit => bybit::fetch_funding(http, &id.symbol, from, to)?,
    };
    store.write_funding(id, &rates)?;
    Ok(rates.len())
}
