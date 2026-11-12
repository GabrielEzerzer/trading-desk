//! Downloading a range of history into the store, one month at a time so a long download keeps
//! what it has fetched if it stops part way.

use td_core::{InstrumentId, InstrumentKind, Millis, Timeframe};

use crate::DataError;
use crate::http::Http;
use crate::source::{DataSource, MinuteHistory, source};
use crate::store::{Gap, Store, missing_minutes};
use crate::time::{MINUTE, Month, now_ms};

/// What a candle download did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DownloadReport {
    /// Candles fetched per month.
    pub months: Vec<(Month, usize)>,
    /// History files the source does not have (before the symbol listed, or not yet published),
    /// as [`MinuteHistory::missing_files`] names them.
    pub missing_files: Vec<String>,
    /// Minutes in the range, up to now, that are still not stored after the download.
    pub gaps: Vec<Gap>,
}

/// Downloads the 1-minute candles of `id` with open times in `[from, to)` from its venue's source
/// ([`DataSource::minute_history`]) and merges them into the store: with their asks
/// ([`Store::write_quotes`]) when the kind's rules require quotes, else with
/// [`Store::write_candles`]. A month whose source returns candles without asks for such a kind,
/// or asks for a kind without quotes, is an error, and nothing of it is written. `progress` is
/// called with a line of text after each month.
pub fn download_candles(
    http: &Http,
    store: &Store,
    id: &InstrumentId,
    from: Millis,
    to: Millis,
    progress: &mut dyn FnMut(String),
) -> Result<DownloadReport, DataError> {
    download_candles_from(source(id.venue), http, store, id, from, to, progress)
}

/// [`download_candles`] from a given source.
pub(crate) fn download_candles_from(
    source: &dyn DataSource,
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
        let MinuteHistory {
            mut candles,
            mut asks,
            missing_files,
        } = source.minute_history(http, id, lo, hi, now)?;
        report.missing_files.extend(missing_files);
        candles.retain(|c| (lo..hi).contains(&c.open_time));
        asks.retain(|c| (lo..hi).contains(&c.open_time));
        if id.kind.rules().quotes {
            if asks.is_empty() && !candles.is_empty() {
                return Err(DataError::NoAsks {
                    instrument: id.to_string(),
                    month: month.to_string(),
                });
            }
            store.write_quotes(id, &candles, &asks)?;
        } else if !asks.is_empty() {
            return Err(DataError::NoQuotes(id.to_string()));
        } else {
            store.write_candles(id, &candles)?;
        }
        progress(format!("{id} {month}: {} candles", candles.len()));
        report.months.push((month, candles.len()));
    }
    let end = to.min(now - now.rem_euclid(MINUTE));
    if from < end {
        let stored = store.load_candles(id, Timeframe::M1, from, end, true)?;
        report.gaps = missing_minutes(id.kind, &stored.candles, from, end);
    }
    Ok(report)
}

/// Downloads the funding rates of a perpetual in `[from, to)` and merges them into the store.
/// Returns the number of rates fetched. A venue whose source has no funding is an error.
pub fn download_funding(
    http: &Http,
    store: &Store,
    id: &InstrumentId,
    from: Millis,
    to: Millis,
) -> Result<usize, DataError> {
    download_funding_from(source(id.venue), http, store, id, from, to)
}

/// [`download_funding`] from a given source.
fn download_funding_from(
    source: &dyn DataSource,
    http: &Http,
    store: &Store,
    id: &InstrumentId,
    from: Millis,
    to: Millis,
) -> Result<usize, DataError> {
    if id.kind != InstrumentKind::Perpetual {
        return Err(DataError::NotPerpetual(id.to_string()));
    }
    let rates = source.funding(http, id, from, to)?;
    store.write_funding(id, &rates)?;
    Ok(rates.len())
}

#[cfg(test)]
mod tests {
    use td_core::Venue;

    use super::*;
    use crate::source::fake::{self, DailyFiles};
    use crate::time::{DAY, parse_date};

    fn id(kind: InstrumentKind) -> InstrumentId {
        InstrumentId {
            venue: Venue::Binance,
            symbol: "EURUSD".into(),
            kind,
        }
    }

    #[test]
    fn a_download_runs_month_by_month_through_the_source_into_the_store() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = id(InstrumentKind::Spot);
        let feb = parse_date("2024-02-01").expect("a date");
        // Noon on 30 January up to 3 February, from a source that has no file for 1 February.
        let (from, to) = (feb - 2 * DAY + DAY / 2, feb + 2 * DAY);
        let source = DailyFiles::new(vec![feb]);
        let mut lines = Vec::new();
        let http = Http::new().expect("an HTTP client");
        let report = download_candles_from(&source, &http, &store, &id, from, to, &mut |line| {
            lines.push(line)
        })
        .expect("downloads");

        // One call per month, each kept inside its month.
        assert_eq!(
            *source.asked.lock().expect("lock"),
            [(from, feb), (feb, to)]
        );
        let per_day = DAY / MINUTE;
        let [jan, february] = [Month::containing(from), Month::containing(feb)];
        // The morning of 30 January, which the source sent with the rest of its file, is left out.
        let counts = [
            (jan, (per_day * 3 / 2) as usize),
            (february, per_day as usize),
        ];
        assert_eq!(report.months, counts);
        assert_eq!(report.missing_files, ["2024-02-01.csv.gz"]);
        assert_eq!(
            report.gaps,
            [Gap {
                from: feb,
                to: feb + DAY
            }]
        );
        assert_eq!(lines.len(), 2);

        let stored = store
            .load_candles(&id, Timeframe::M1, from - DAY, to + DAY, true)
            .expect("loads")
            .candles;
        assert_eq!(stored.len(), (per_day * 5 / 2) as usize);
        assert_eq!(stored.first().map(|c| c.open_time), Some(from));
        assert_eq!(stored.last().map(|c| c.open_time), Some(to - MINUTE));
    }

    #[test]
    fn an_fx_download_stores_the_bids_and_the_asks() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = id(InstrumentKind::Fx);
        // Monday 8 January 2024 to the Monday after: one FX week, with its weekend closed.
        let from = parse_date("2024-01-08").expect("a date");
        let to = from + 7 * DAY;
        let source = DailyFiles::fx(true);
        let http = Http::new().expect("an HTTP client");
        let report = download_candles_from(&source, &http, &store, &id, from, to, &mut |_| {})
            .expect("downloads");
        let open_minutes = (5 * DAY / MINUTE) as usize;
        let jan = Month::containing(from);
        assert_eq!(report.months, [(jan, open_minutes)]);
        assert_eq!(report.gaps, [], "the weekend is closed, not missing");

        let bids = store.read_month(&id, jan).expect("bids");
        let asks = store.read_month_asks(&id, jan).expect("asks");
        assert_eq!(bids.len(), open_minutes);
        let times =
            |candles: &[td_core::Candle]| candles.iter().map(|c| c.open_time).collect::<Vec<_>>();
        assert_eq!(times(&asks), times(&bids));
        assert!(asks.iter().all(|a| *a == fake::ask(a.open_time)));
        assert!(bids.iter().all(|b| *b == fake::candle(b.open_time)));
    }

    #[test]
    fn an_fx_source_that_returns_no_asks_is_refused_and_stores_nothing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = id(InstrumentKind::Fx);
        let from = parse_date("2024-01-08").expect("a date");
        let source = DailyFiles::fx(false);
        let http = Http::new().expect("an HTTP client");
        let result =
            download_candles_from(&source, &http, &store, &id, from, from + DAY, &mut |_| {});
        match result {
            Err(e @ DataError::NoAsks { .. }) => assert_eq!(
                e.to_string(),
                "binance:EURUSD:fx 2024-01: the source returned bids without asks"
            ),
            other => panic!("expected NoAsks, got {other:?}"),
        }
        assert_eq!(store.months(&id).expect("lists"), []);
    }

    #[test]
    fn funding_from_a_source_without_funding_is_an_error_and_stores_nothing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = id(InstrumentKind::Perpetual);
        let http = Http::new().expect("an HTTP client");
        let source = DailyFiles::new(Vec::new());
        let result = download_funding_from(&source, &http, &store, &id, 0, DAY);
        assert!(
            matches!(result, Err(DataError::NoFunding(Venue::Binance))),
            "{result:?}"
        );
        assert!(
            store
                .load_funding(&id, i64::MIN, i64::MAX)
                .expect("loads")
                .is_empty()
        );
    }
}
