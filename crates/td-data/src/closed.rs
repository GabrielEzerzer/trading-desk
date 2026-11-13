//! Closures a market with trading hours has that its weekly hours do not know: for FX on OANDA,
//! about 24 December 22:00 to 25 December 22:00 UTC, from about 22:00 UTC on 31 December, and the
//! minutes of a slow Sunday open. They are recorded when a month is downloaded, and coverage and
//! the run check read them.
//!
//! A download of a kind with closed hours records, beside the month file, the spans of open time in
//! the range it asked for that the venue returned nothing for: `<YYYY-MM>.parquet.closed.json`, a
//! JSON list of `[from, to]` pairs in milliseconds, each span holding the open times `from` up to
//! but not including `to`. A download replaces what the record said about the range it fetched and
//! keeps the rest. A span that reaches the present is left out, since its minutes may only not be
//! published yet.
//!
//! When the records are read ([`Store::closures`]), spans that meet, at a month's end or across
//! closed hours, join into one stretch. A stretch holding at most [`SHORT_CLOSURE`] of open time,
//! such as a late Sunday open, counts as closed. A longer one counts as closed only if it holds at
//! most [`LONGEST_CLOSURE`] and overlaps the year-end holidays, 24 December 00:00 to 3 January
//! 00:00 UTC, where OANDA's day-long closures fall. Any other stretch is an outage, not a closure:
//! it stays missing, so its month is not complete and the run check refuses a long one. Records
//! are written only for kinds with closed hours, so a crypto outage is always missing.
//!
//! A month downloaded before records were written has none: all its missing open time counts as
//! missing, as before, and the run check keeps a wider limit for it (`max_gap` in td-server).
//!
//! A delete of a month removes its record with its data file, so the stored list and the delete
//! treat records as the store's own files, and the next download of the month fetches all of it
//! and writes the record again.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use td_core::{Hours, InstrumentId, InstrumentKind, Millis};

use crate::DataError;
use crate::store::{
    Gap, Store, bad_file, file_lock, hold, lock_across_processes, write_bytes_atomic,
};
use crate::time::{DAY, MINUTE, Month};

/// The longest stretch of open time without a candle that counts as a closure, and then only over
/// the year-end holidays: 27 hours. OANDA's holiday closures run a little past a day. Stored
/// USD/JPY, 2016 to 2025, shows Christmas closures of 1439 to 1500 minutes (2024: 24 December
/// 21:55 to 25 December 22:04, 1449 minutes; 2016: 25 December 22:00 to 26 December 23:00, 1500).
/// A holiday on a Friday joins across the weekend's closed hours to the Sunday open, which can come
/// up to 175 minutes late, so a day plus three hours covers both with a few minutes to spare.
pub const LONGEST_CLOSURE: Millis = DAY + 3 * HOUR;

/// The longest stretch that counts as a closure at any time of year: 4 hours. In the same USD/JPY
/// data, outside 24 December to 2 January, no stretch of an hour or more without a candle lasts
/// longer than 175 minutes, the Sunday open of 20 October 2019 (21:00 to 23:55 UTC); the others
/// are late Sunday opens of 60 to 129 minutes and one Thursday outage of 171 minutes on 12 May 2022.
/// Four hours keeps about an hour above the slowest Sunday open seen, while a weekday outage of a
/// day stays missing.
const SHORT_CLOSURE: Millis = 4 * HOUR;

const HOUR: Millis = 60 * MINUTE;

/// What follows `.parquet` in the name of a month's record.
pub(crate) const RECORD_SUFFIX: &str = ".closed.json";

/// The recorded closures over a range, as [`Store::closures`] reads them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Closures {
    /// Spans that count as closed, oldest first, apart from each other.
    spans: Vec<Gap>,
    /// The months, of those the range overlaps, that have a record.
    recorded: BTreeSet<Month>,
}

impl Closures {
    /// Whether `month`'s downloads recorded its closures. A month downloaded before records were
    /// written, or never downloaded, has no record.
    pub fn recorded(&self, month: Month) -> bool {
        self.recorded.contains(&month)
    }

    /// `gaps`, oldest first, with the closed spans taken out of them.
    pub fn remove(&self, gaps: Vec<Gap>) -> Vec<Gap> {
        if self.spans.is_empty() {
            return gaps;
        }
        let mut open = Vec::with_capacity(gaps.len());
        for gap in gaps {
            let mut start = gap.from;
            for span in self
                .spans
                .iter()
                .filter(|s| s.from < gap.to && s.to > gap.from)
            {
                if span.from > start {
                    open.push(Gap {
                        from: start,
                        to: span.from,
                    });
                }
                start = start.max(span.to);
            }
            if start < gap.to {
                open.push(Gap {
                    from: start,
                    to: gap.to,
                });
            }
        }
        open
    }

    /// The closed spans, oldest first, cut to `[from, to)`.
    pub fn spans_in(&self, from: Millis, to: Millis) -> Vec<Gap> {
        self.spans
            .iter()
            .map(|s| Gap {
                from: s.from.max(from),
                to: s.to.min(to),
            })
            .filter(|s| s.from < s.to)
            .collect()
    }

    /// The closed time in `[from, to)`.
    pub fn closed_in(&self, from: Millis, to: Millis) -> Millis {
        self.spans_in(from, to).iter().map(|s| s.to - s.from).sum()
    }
}

impl Store {
    fn record_path(&self, id: &InstrumentId, month: Month) -> Result<PathBuf, DataError> {
        let mut path = self.candle_path(id, month)?.into_os_string();
        path.push(RECORD_SUFFIX);
        Ok(path.into())
    }

    /// The spans of open time in `month` that its downloads found the venue had nothing for,
    /// oldest first; `None` when no download recorded them.
    pub fn closure_record(
        &self,
        id: &InstrumentId,
        month: Month,
    ) -> Result<Option<Vec<Gap>>, DataError> {
        let path = self.record_path(id, month)?;
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(DataError::io(&path)(e)),
        };
        let pairs: Vec<(Millis, Millis)> =
            serde_json::from_str(&text).map_err(|e| bad_file(&path, e.to_string()))?;
        let mut end = month.start();
        let mut spans = Vec::with_capacity(pairs.len());
        for (from, to) in pairs {
            if from < end || to <= from || to > month.end() {
                return Err(bad_file(
                    &path,
                    "its spans must be in order, apart and inside the month",
                ));
            }
            end = to;
            spans.push(Gap { from, to });
        }
        Ok(Some(spans))
    }

    /// Records that a download of `[from, to)`, inside `month`, found the venue had nothing for
    /// the spans `empty` (inside the range, oldest first), replacing what `month`'s record said
    /// about that range. Takes the month file's lock, as a write of the month does.
    pub fn record_closures(
        &self,
        id: &InstrumentId,
        month: Month,
        from: Millis,
        to: Millis,
        empty: &[Gap],
    ) -> Result<(), DataError> {
        let data = self.candle_path(id, month)?;
        let lock = file_lock(&data);
        let _held = hold(&lock);
        let _locked = lock_across_processes(&data)?;
        let mut spans: Vec<Gap> = Vec::new();
        for kept in self.closure_record(id, month)?.unwrap_or_default() {
            // What lies outside the range downloaded again.
            for (lo, hi) in [(kept.from, kept.to.min(from)), (kept.from.max(to), kept.to)] {
                if lo < hi {
                    spans.push(Gap { from: lo, to: hi });
                }
            }
        }
        spans.extend(empty.iter().filter(|g| g.from >= from && g.to <= to));
        spans.sort_by_key(|g| g.from);
        let mut merged: Vec<(Millis, Millis)> = Vec::with_capacity(spans.len());
        for g in spans {
            match merged.last_mut() {
                Some(last) if g.from <= last.1 => last.1 = last.1.max(g.to),
                _ => merged.push((g.from, g.to)),
            }
        }
        let json = serde_json::to_vec(&merged).map_err(|e| bad_file(&data, e.to_string()))?;
        write_bytes_atomic(&self.record_path(id, month)?, &json)
    }

    /// The closures recorded over `[from, to)`: each stretch of recorded spans, joined where they
    /// meet at a month's end or across closed hours, that counts as closed by the rule in this
    /// module's doc. The months either side of the range are read too, so a stretch the range cuts
    /// is judged whole. Nothing for a kind that never closes.
    pub fn closures(
        &self,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<Closures, DataError> {
        let mut closures = Closures::default();
        let months = Month::range(from, to);
        let (Some(&first), Some(&last)) = (months.first(), months.last()) else {
            return Ok(closures);
        };
        if id.kind.rules().hours == Hours::Always {
            return Ok(closures);
        }
        let before = Month::containing(first.start() - 1);
        let mut recorded = Vec::new();
        for month in std::iter::once(before)
            .chain(months.iter().copied())
            .chain(std::iter::once(last.next()))
        {
            if let Some(spans) = self.closure_record(id, month)? {
                if months.contains(&month) {
                    closures.recorded.insert(month);
                }
                recorded.extend(spans);
            }
        }
        for stretch in recorded.chunk_by(|a, b| meet(id.kind, a, b)) {
            let open: Millis = stretch.iter().map(|g| g.to - g.from).sum();
            let (start, end) = (stretch[0].from, stretch[stretch.len() - 1].to);
            if open <= SHORT_CLOSURE || (open <= LONGEST_CLOSURE && in_year_end(start, end)) {
                closures.spans.extend_from_slice(stretch);
            }
        }
        Ok(closures)
    }
}

/// Whether `[from, to)` overlaps the year-end holidays, 24 December 00:00 to 3 January 00:00 UTC,
/// of any year.
fn in_year_end(from: Millis, to: Millis) -> bool {
    let year = Month::containing(from).year;
    (year - 1..=year).any(|y| {
        let start = Month { year: y, month: 12 }.start() + 23 * DAY;
        let end = Month {
            year: y + 1,
            month: 1,
        }
        .start()
            + 2 * DAY;
        from < end && to > start
    })
}

/// Whether span `b`, after `a`, starts where `a` ends or only closed hours part them.
fn meet(kind: InstrumentKind, a: &Gap, b: &Gap) -> bool {
    a.to == b.from || kind.closed_spans(a.to, b.from).first() == Some(&(a.to..b.from))
}

#[cfg(test)]
mod tests {
    use td_core::Venue;

    use super::*;
    use crate::source::fake;
    use crate::time::{MINUTE, parse_date};

    fn fx() -> InstrumentId {
        InstrumentId {
            venue: Venue::Oanda,
            symbol: "USDJPY".into(),
            kind: InstrumentKind::Fx,
        }
    }

    #[test]
    fn a_record_replaces_the_range_downloaded_again_and_keeps_the_rest() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let dec = Month::parse("2024-12").expect("a month");
        let day = |d: i64| dec.start() + (d - 1) * DAY;
        assert_eq!(store.closure_record(&fx(), dec).expect("reads"), None);
        let christmas = Gap {
            from: day(24) + 22 * HOUR,
            to: day(25) + 22 * HOUR,
        };
        let early = Gap {
            from: day(2),
            to: day(2) + HOUR,
        };
        store
            .record_closures(&fx(), dec, dec.start(), dec.end(), &[early, christmas])
            .expect("records");
        // The first half again, where the venue now has the early hour.
        store
            .record_closures(&fx(), dec, dec.start(), day(16), &[])
            .expect("records");
        assert_eq!(
            store.closure_record(&fx(), dec).expect("reads"),
            Some(vec![christmas])
        );
        // A span the range cuts keeps its part outside, and joins the new part again.
        store
            .record_closures(
                &fx(),
                dec,
                day(25),
                dec.end(),
                &[Gap {
                    from: day(25),
                    to: christmas.to,
                }],
            )
            .expect("records");
        assert_eq!(
            store.closure_record(&fx(), dec).expect("reads"),
            Some(vec![christmas])
        );
        // The listing does not take the record for a stray file.
        assert_eq!(
            store.stored().expect("lists").unrecognised,
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn spans_joined_across_a_month_end_count_as_closed_only_up_to_the_longest_closure() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let jan = parse_date("2025-01-01").expect("a date"); // a Wednesday
        let (dec, january) = (Month::containing(jan - 1), Month::containing(jan));
        // 31 December 22:00 to 1 January 22:00: two hours in December, 22 in January.
        let tail = Gap {
            from: jan - 2 * HOUR,
            to: jan,
        };
        let head = Gap {
            from: jan,
            to: jan + 22 * HOUR,
        };
        store
            .record_closures(&fx(), dec, dec.start(), dec.end(), &[tail])
            .expect("records");
        store
            .record_closures(&fx(), january, jan, january.end(), &[head])
            .expect("records");
        // Read from January alone, the December part still counts towards the stretch.
        let closures = store.closures(&fx(), jan, january.end()).expect("reads");
        assert!(closures.recorded(january) && !closures.recorded(dec));
        assert_eq!(closures.closed_in(jan, january.end()), 22 * HOUR);
        let gap = Gap {
            from: jan - 5 * HOUR,
            to: jan + 23 * HOUR,
        };
        assert_eq!(
            closures.remove(vec![gap]),
            [
                Gap {
                    from: gap.from,
                    to: tail.from
                },
                Gap {
                    from: head.to,
                    to: gap.to
                }
            ]
        );

        // Four hours more in January make the stretch 28 hours: an outage, not closed.
        let longer = Gap {
            from: jan,
            to: jan + 26 * HOUR,
        };
        store
            .record_closures(&fx(), january, jan, january.end(), &[longer])
            .expect("records");
        let closures = store.closures(&fx(), jan, january.end()).expect("reads");
        assert_eq!(closures.closed_in(jan, january.end()), 0);
        assert_eq!(closures.remove(vec![gap]), [gap]);
    }

    #[test]
    fn a_december_with_its_christmas_closure_recorded_has_whole_coverage() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let dec = Month::parse("2024-12").expect("a month");
        // OANDA's 2024 Christmas closure as stored USD/JPY has it: 24 December 21:55 to
        // 25 December 22:04 UTC, 1449 minutes, longer than a day.
        let christmas = Gap {
            from: parse_date("2024-12-24").expect("a date") + 21 * HOUR + 55 * MINUTE,
            to: parse_date("2024-12-25").expect("a date") + 22 * HOUR + 4 * MINUTE,
        };
        let open: Vec<Millis> = (dec.start()..dec.end())
            .step_by(MINUTE as usize)
            .filter(|&t| fx().kind.is_open(t) && !(christmas.from..christmas.to).contains(&t))
            .collect();
        let bids: Vec<_> = open.iter().map(|&t| fake::candle(t)).collect();
        let asks: Vec<_> = open.iter().map(|&t| fake::ask(t)).collect();
        store.write_quotes(&fx(), &bids, &asks).expect("writes");

        // Before the record, as for a month downloaded before records were written: a gap.
        let coverage = store.coverage(&fx()).expect("covers");
        assert_eq!(coverage[0].gaps, [christmas]);
        assert!(coverage[0].minutes < coverage[0].open_minutes as usize);

        store
            .record_closures(&fx(), dec, dec.start(), dec.end(), &[christmas])
            .expect("records");
        let coverage = store.coverage(&fx()).expect("covers");
        assert_eq!(coverage[0].gaps, []);
        assert_eq!(coverage[0].minutes, coverage[0].open_minutes as usize);
    }

    /// The coverage of `month` stored with every open minute but those of `hole`, which a
    /// download recorded as empty.
    fn coverage_without(store: &Store, month: &str, hole: Gap) -> crate::MonthCoverage {
        let month = Month::parse(month).expect("a month");
        let open: Vec<Millis> = (month.start()..month.end())
            .step_by(MINUTE as usize)
            .filter(|&t| fx().kind.is_open(t) && !(hole.from..hole.to).contains(&t))
            .collect();
        let bids: Vec<_> = open.iter().map(|&t| fake::candle(t)).collect();
        let asks: Vec<_> = open.iter().map(|&t| fake::ask(t)).collect();
        store.write_quotes(&fx(), &bids, &asks).expect("writes");
        store
            .record_closures(&fx(), month, month.start(), month.end(), &[hole])
            .expect("records");
        let coverage = store.coverage(&fx()).expect("covers");
        coverage
            .into_iter()
            .find(|c| c.month == month)
            .expect("the month")
    }

    #[test]
    fn a_long_recorded_span_outside_the_year_end_is_an_outage() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        // 25 hours from Tuesday 11 March 2025: short of the 27-hour ceiling, but no holiday.
        let tuesday = parse_date("2025-03-11").expect("a date");
        let outage = Gap {
            from: tuesday,
            to: tuesday + 25 * HOUR,
        };
        let march = coverage_without(&store, "2025-03", outage);
        assert_eq!(march.gaps, [outage]);
        assert!(march.minutes < march.open_minutes as usize);
    }

    #[test]
    fn a_late_sunday_open_counts_as_closed_at_any_time_of_year() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        // The first three open hours of Sunday 15 June 2025.
        let sunday = parse_date("2025-06-15").expect("a date");
        let open = (sunday..sunday + DAY)
            .step_by(MINUTE as usize)
            .find(|&t| fx().kind.is_open(t))
            .expect("opens on Sunday");
        let late = Gap {
            from: open,
            to: open + 3 * HOUR,
        };
        let june = coverage_without(&store, "2025-06", late);
        assert_eq!(june.gaps, []);
        assert_eq!(june.minutes, june.open_minutes as usize);
    }

    #[test]
    fn the_year_end_window_runs_from_24_december_to_the_end_of_2_january() {
        let dec24 = parse_date("2024-12-24").expect("a date");
        let jan3 = parse_date("2025-01-03").expect("a date");
        assert!(!in_year_end(dec24 - HOUR, dec24));
        assert!(in_year_end(dec24 - HOUR, dec24 + MINUTE));
        assert!(in_year_end(jan3 - MINUTE, jan3 + HOUR));
        assert!(!in_year_end(jan3, jan3 + HOUR));
        assert!(!in_year_end(jan3, jan3 + 300 * DAY));
    }

    #[test]
    fn a_damaged_record_is_an_error_not_a_closure() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let dec = Month::parse("2024-12").expect("a month");
        let path = store.record_path(&fx(), dec).expect("a path");
        fs::create_dir_all(path.parent().expect("a folder")).expect("makes it");
        for text in ["not json", "[[5, 1]]", "[[0, 60000]]"] {
            fs::write(&path, text).expect("writes");
            let read = store.closure_record(&fx(), dec);
            assert!(
                matches!(read, Err(DataError::BadFile { .. })),
                "{text}: {read:?}"
            );
        }
    }
}
