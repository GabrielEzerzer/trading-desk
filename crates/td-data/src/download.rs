//! Downloading a range of history into the store, one month at a time so a long download keeps
//! what it has fetched if it stops part way. A month already stored whole is not fetched again.

use td_core::{Hours, InstrumentId, InstrumentKind, Millis, Timeframe};

use crate::DataError;
use crate::http::Http;
use crate::source::{DataSource, MinuteHistory, source};
use crate::store::{Gap, Store, missing_minutes};
use crate::time::{MINUTE, Month, now_ms};

/// What a candle download did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DownloadReport {
    /// Candles fetched per month: none for a month already stored whole.
    pub months: Vec<(Month, usize)>,
    /// History files the source does not have (before the symbol listed, or not yet published),
    /// as [`MinuteHistory::missing_files`] names them.
    pub missing_files: Vec<String>,
    /// Minutes in the range, up to now, that are still not stored after the download, apart from
    /// the closures recorded ([`Store::closures`]).
    pub gaps: Vec<Gap>,
}

/// Downloads the 1-minute candles of `id` with open times in `[from, to)` from its venue's source
/// ([`DataSource::minute_history`]) and merges them into the store: with their asks
/// ([`Store::write_quotes`]) when the kind's rules require quotes, else with
/// [`Store::write_candles`]. A month whose source returns candles without asks for such a kind,
/// or asks for a kind without quotes, is an error, and nothing of it is written. `progress` is
/// called with a line of text after each month.
///
/// A month whose part of the range is already stored with no minute missing, apart from recorded
/// closures, is skipped. For a kind with closed hours, the open time the source returned nothing
/// for is recorded beside the month ([`Store::record_closures`]), and a month with no record yet is
/// fetched whole, up to now, so its first record covers all of it.
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
        // shortcut: a crypto month with an outage in it is never whole, so it is fetched again on
        // every download that covers it; record crypto outages too if that gets slow.
        let stored = store.read_month(id, month)?;
        if !stored.is_empty()
            && store
                .closures(id, lo, hi)?
                .remove(missing_minutes(id.kind, &stored, lo, hi))
                .is_empty()
        {
            progress(format!("{id} {month}: already stored"));
            report.months.push((month, 0));
            continue;
        }
        let closes = id.kind.rules().hours != Hours::Always;
        // A record makes the run check hold the month to a day missing, so it must not cover only
        // the part this download asked for while the month's other closures go unrecorded.
        let (lo, hi) = if closes && store.closure_record(id, month)?.is_none() {
            (month.start(), month.end().min(now))
        } else {
            (lo, hi)
        };
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
        // A month that holds nothing has no record either: the listing and a delete of all market
        // data would not see it.
        if closes && !(candles.is_empty() && stored.is_empty()) {
            candles.sort_by_key(|c| c.open_time);
            let mut empty = missing_minutes(id.kind, &candles, lo, hi);
            // Minutes up to now may only not be published yet.
            if hi == now && empty.last().is_some_and(|g| g.to == hi) {
                empty.pop();
            }
            store.record_closures(id, month, lo, hi, &empty)?;
        }
        progress(format!("{id} {month}: {} candles", candles.len()));
        report.months.push((month, candles.len()));
    }
    let end = to.min(now - now.rem_euclid(MINUTE));
    if from < end {
        let stored = store.load_candles(id, Timeframe::M1, from, end, true)?;
        report.gaps = store.closures(id, from, end)?.remove(missing_minutes(
            id.kind,
            &stored.candles,
            from,
            end,
        ));
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
        // January 2024, with its weekends closed.
        let jan = Month::parse("2024-01").expect("a month");
        let (from, to) = (jan.start(), jan.end());
        let source = DailyFiles::fx(true);
        let http = Http::new().expect("an HTTP client");
        let report = download_candles_from(&source, &http, &store, &id, from, to, &mut |_| {})
            .expect("downloads");
        let open_minutes = (from..to)
            .step_by(MINUTE as usize)
            .filter(|&t| id.kind.is_open(t))
            .count();
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
    fn a_month_stored_whole_is_not_fetched_again() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = id(InstrumentKind::Spot);
        let feb = parse_date("2024-02-01").expect("a date");
        let (from, to) = (feb - 2 * DAY, feb + 2 * DAY);
        let http = Http::new().expect("an HTTP client");
        // 2 February is missing the first time, so February is not whole.
        let first = DailyFiles::new(vec![feb + DAY]);
        download_candles_from(&first, &http, &store, &id, from, to, &mut |_| {})
            .expect("downloads");
        let again = DailyFiles::new(Vec::new());
        let mut lines = Vec::new();
        let report = download_candles_from(&again, &http, &store, &id, from, to, &mut |line| {
            lines.push(line)
        })
        .expect("downloads");
        // January is whole, so only February is asked for.
        assert_eq!(*again.asked.lock().expect("lock"), [(feb, to)]);
        let jan = Month::containing(from);
        assert_eq!(report.months[0], (jan, 0));
        assert_eq!(lines[0], format!("{id} {jan}: already stored"));
        assert_eq!(report.gaps, []);
        // Now both are whole: nothing is asked for.
        let third = DailyFiles::new(Vec::new());
        download_candles_from(&third, &http, &store, &id, from, to, &mut |_| {})
            .expect("downloads");
        assert_eq!(*third.asked.lock().expect("lock"), []);
    }

    #[test]
    fn an_fx_day_the_source_has_nothing_for_is_recorded_as_closed() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = id(InstrumentKind::Fx);
        let dec = Month::parse("2024-12").expect("a month");
        let christmas = parse_date("2024-12-25").expect("a date"); // a Wednesday
        let source = DailyFiles {
            missing_days: vec![christmas],
            ..DailyFiles::fx(true)
        };
        let http = Http::new().expect("an HTTP client");
        let report = download_candles_from(
            &source,
            &http,
            &store,
            &id,
            dec.start(),
            dec.end(),
            &mut |_| {},
        )
        .expect("downloads");
        let closed = Gap {
            from: christmas,
            to: christmas + DAY,
        };
        assert_eq!(
            store.closure_record(&id, dec).expect("reads"),
            Some(vec![closed])
        );
        assert_eq!(report.gaps, [], "a recorded day is closed, not missing");
        // The month is whole now, so it is not fetched again.
        let again = DailyFiles::fx(true);
        download_candles_from(
            &again,
            &http,
            &store,
            &id,
            dec.start(),
            dec.end(),
            &mut |_| {},
        )
        .expect("downloads");
        assert_eq!(*again.asked.lock().expect("lock"), []);
    }

    #[test]
    fn the_first_fx_download_of_part_of_a_month_fetches_and_records_the_whole_month() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = id(InstrumentKind::Fx);
        let dec = Month::parse("2024-12").expect("a month");
        let christmas = parse_date("2024-12-25").expect("a date");
        let source = DailyFiles {
            missing_days: vec![christmas],
            ..DailyFiles::fx(true)
        };
        let http = Http::new().expect("an HTTP client");
        // Two days in the middle of a month with no record yet.
        let tuesday = parse_date("2024-12-10").expect("a date");
        download_candles_from(
            &source,
            &http,
            &store,
            &id,
            tuesday,
            tuesday + 2 * DAY,
            &mut |_| {},
        )
        .expect("downloads");
        assert_eq!(
            *source.asked.lock().expect("lock"),
            [(dec.start(), dec.end())]
        );
        // So the record covers Christmas too, and the month is whole.
        let closed = Gap {
            from: christmas,
            to: christmas + DAY,
        };
        assert_eq!(
            store.closure_record(&id, dec).expect("reads"),
            Some(vec![closed])
        );
        let coverage = store.coverage(&id).expect("covers");
        assert_eq!(coverage[0].gaps, []);

        // A delete removes the record with the month, so the next download of part of it fetches
        // the whole month again.
        store.delete_months(&id, dec, dec).expect("deletes");
        assert_eq!(store.closure_record(&id, dec).expect("reads"), None);
        let again = DailyFiles::fx(true);
        download_candles_from(
            &again,
            &http,
            &store,
            &id,
            tuesday,
            tuesday + 2 * DAY,
            &mut |_| {},
        )
        .expect("downloads");
        assert_eq!(
            *again.asked.lock().expect("lock"),
            [(dec.start(), dec.end())]
        );
    }

    #[test]
    fn an_fx_month_the_source_has_nothing_for_gets_no_record() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = id(InstrumentKind::Fx);
        let feb = Month::parse("2024-02").expect("a month");
        let source = DailyFiles {
            missing_days: (0..29).map(|d| feb.start() + d * DAY).collect(),
            ..DailyFiles::fx(true)
        };
        let http = Http::new().expect("an HTTP client");
        download_candles_from(
            &source,
            &http,
            &store,
            &id,
            feb.start(),
            feb.end(),
            &mut |_| {},
        )
        .expect("downloads");
        // Nothing is stored, so the listing and a delete would not see a record either.
        assert_eq!(store.read_month(&id, feb).expect("reads"), []);
        assert_eq!(store.closure_record(&id, feb).expect("reads"), None);
    }

    #[test]
    fn an_fx_hole_longer_than_a_day_stays_missing_and_is_fetched_again() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let id = id(InstrumentKind::Fx);
        let dec = Month::parse("2024-12").expect("a month");
        let tuesday = parse_date("2024-12-10").expect("a date");
        let source = DailyFiles {
            missing_days: vec![tuesday, tuesday + DAY],
            ..DailyFiles::fx(true)
        };
        let http = Http::new().expect("an HTTP client");
        let report = download_candles_from(
            &source,
            &http,
            &store,
            &id,
            dec.start(),
            dec.end(),
            &mut |_| {},
        )
        .expect("downloads");
        let hole = Gap {
            from: tuesday,
            to: tuesday + 2 * DAY,
        };
        assert_eq!(report.gaps, [hole]);
        let again = DailyFiles::fx(true);
        let report = download_candles_from(
            &again,
            &http,
            &store,
            &id,
            dec.start(),
            dec.end(),
            &mut |_| {},
        )
        .expect("downloads");
        assert_eq!(
            *again.asked.lock().expect("lock"),
            [(dec.start(), dec.end())]
        );
        assert_eq!(report.gaps, []);
        assert_eq!(store.closure_record(&id, dec).expect("reads"), Some(vec![]));
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
