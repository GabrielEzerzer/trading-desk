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
    let mut report = DownloadReport::default();
    for month in Month::range(from, to.min(now)) {
        let lo = from.max(month.start());
        let hi = to.min(month.end()).min(now);
        let mut candles = match id.venue {
            Venue::Binance => binance_month(
                &id.symbol,
                month,
                (lo, hi),
                now,
                &mut report.missing_files,
                |period| binance::fetch_archive(http, id, period),
            )?,
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
        report.gaps = missing_minutes(&stored.candles, from, end);
    }
    Ok(report)
}

/// One month of Binance klines from the archive, where `fetch` downloads one archive file (`None`
/// when the archive has no such file). A month before the current one comes from its monthly
/// file; when that is not published, and for the current month, the daily files for the days that
/// overlap `[lo, hi)` up to yesterday are used instead, and the names of those the archive lacks
/// are added to `missing`.
fn binance_month(
    symbol: &str,
    month: Month,
    (lo, hi): (Millis, Millis),
    now: Millis,
    missing: &mut Vec<String>,
    mut fetch: impl FnMut(ArchivePeriod) -> Result<Option<Vec<Candle>>, DataError>,
) -> Result<Vec<Candle>, DataError> {
    if month < Month::containing(now)
        && let Some(candles) = fetch(ArchivePeriod::Month(month))?
    {
        return Ok(candles);
    }
    let today = now.div_euclid(DAY) * DAY;
    let mut candles = Vec::new();
    let mut day = lo.div_euclid(DAY) * DAY;
    while day < hi && day < today {
        let period = ArchivePeriod::Day(day);
        match fetch(period)? {
            Some(rows) => candles.extend(rows),
            None => missing.push(binance::archive_file_name(symbol, period)),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads `month` through [`binance_month`] from an archive that holds the files `has` says it
    /// holds, each with one candle at the start of its period. Returns the open times read, the
    /// files asked for and the names reported missing.
    fn month_from_archive(
        month: Month,
        range: (Millis, Millis),
        now: Millis,
        has: impl Fn(ArchivePeriod) -> bool,
    ) -> (Vec<Millis>, Vec<ArchivePeriod>, Vec<String>) {
        let (mut asked, mut missing) = (Vec::new(), Vec::new());
        let candles = binance_month("BTCUSDT", month, range, now, &mut missing, |period| {
            asked.push(period);
            let open_time = match period {
                ArchivePeriod::Month(m) => m.start(),
                ArchivePeriod::Day(day) => day,
            };
            let candle = Candle {
                open_time,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 0.0,
                quote_volume: 0.0,
            };
            Ok(has(period).then(|| vec![candle]))
        })
        .expect("reads");
        let times = candles.iter().map(|c| c.open_time).collect();
        (times, asked, missing)
    }

    #[test]
    fn a_missing_monthly_file_falls_back_to_the_daily_files() {
        let jan = Month::parse("2024-01").expect("a month");
        let now = jan.end() + 40 * DAY;
        // 2024-01-10 up to 2024-01-13, from an archive with no monthly file and no file for the 11th.
        let [d10, d11, d12] = [9, 10, 11].map(|n| jan.start() + n * DAY);
        let has = |p| !matches!(p, ArchivePeriod::Month(_)) && p != ArchivePeriod::Day(d11);
        let (read, asked, missing) = month_from_archive(jan, (d10, d12 + DAY), now, has);
        assert_eq!(
            asked,
            [
                ArchivePeriod::Month(jan),
                ArchivePeriod::Day(d10),
                ArchivePeriod::Day(d11),
                ArchivePeriod::Day(d12)
            ]
        );
        assert_eq!(read, [d10, d12]);
        assert_eq!(missing, ["BTCUSDT-1m-2024-01-11.zip"]);
    }

    #[test]
    fn a_published_monthly_file_is_used_alone_and_the_current_month_reads_up_to_yesterday() {
        let jan = Month::parse("2024-01").expect("a month");
        let (read, asked, missing) =
            month_from_archive(jan, (jan.start(), jan.end()), jan.end() + DAY, |_| true);
        assert_eq!(asked, [ArchivePeriod::Month(jan)]);
        assert_eq!(read, [jan.start()]);
        assert!(missing.is_empty());
        // At noon on 2024-01-03 the month has no monthly file yet, and today has no daily file.
        let now = jan.start() + 2 * DAY + DAY / 2;
        let (read, asked, _) = month_from_archive(jan, (jan.start(), now), now, |_| true);
        let days = [jan.start(), jan.start() + DAY];
        assert_eq!(asked, days.map(ArchivePeriod::Day));
        assert_eq!(read, days);
    }
}
