//! The Tokyo gotobi calendar: the days of the month ending in 5 or 0, when Japanese companies
//! settle payments and importers buy dollars before the 9:55 Tokyo fixing.
//!
//! A gotobi day is the 5th, 10th, 15th, 20th, 25th or 30th of a month. February has no 30th, so it
//! has five. Month-end (the 31st, or the last business day of the month) is not a gotobi day here:
//! Ito and Yamada (NBER working paper 22820, 2016) list "the 5th, 10th, 15th, 20th, 25th, and 30th"
//! and model the end-of-month trading day as a separate calendar effect. A nominal day that is not
//! a Tokyo business day moves back to the nearest earlier business day, which can be in the
//! previous month. When two nominal days move back to the same date, as April 30 and May 5 did in
//! the 2019 Golden Week, that date is one gotobi day.
//!
//! The holiday table covers 2016 to 2027. Any date outside it is an [`OutsideHolidayTable`] error,
//! never a silent "not a holiday".

use std::collections::HashSet;
use std::sync::OnceLock;

use td_core::{Millis, civil_from_days, days_from_civil};

/// The first and last years of `japan_holidays.csv`.
pub const HOLIDAY_TABLE_YEARS: (i64, i64) = (2016, 2027);

const HOLIDAYS_CSV: &str = include_str!("japan_holidays.csv");
const NOMINAL_DAYS: [u32; 6] = [5, 10, 15, 20, 25, 30];
const DAY_MS: Millis = 86_400_000;
/// Tokyo is UTC+9 all year; Japan has no daylight saving time.
const TOKYO_OFFSET_MS: Millis = 9 * 3_600_000;

/// A date the holiday table does not cover, so whether it is a business day is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{year:04}-{month:02}-{day:02} is outside the Japanese holiday table (2016 to 2027)")]
pub struct OutsideHolidayTable {
    pub year: i64,
    pub month: u32,
    pub day: u32,
}

/// One gotobi day: its Tokyo calendar date and the UTC instant of 00:00 Tokyo time on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GotobiDay {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    /// 15:00 UTC on the previous calendar day.
    pub tokyo_midnight: Millis,
}

/// The holiday table as day counts since 1970-01-01.
fn holidays() -> &'static HashSet<i64> {
    static HOLIDAYS: OnceLock<HashSet<i64>> = OnceLock::new();
    HOLIDAYS.get_or_init(|| {
        HOLIDAYS_CSV
            .lines()
            .filter(|line| !line.starts_with('#') && !line.starts_with("date,") && !line.is_empty())
            .map(|line| {
                let date = line.split(',').next().unwrap_or_default();
                let mut parts = date.split('-').map(|p| p.parse::<i64>());
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(Ok(y)), Some(Ok(m)), Some(Ok(d))) => {
                        days_from_civil(y, m as u32, d as u32)
                    }
                    _ => panic!("bad row in japan_holidays.csv: {line}"),
                }
            })
            .collect()
    })
}

fn check_in_table(days: i64) -> Result<(i64, u32, u32), OutsideHolidayTable> {
    let (year, month, day) = civil_from_days(days);
    if (HOLIDAY_TABLE_YEARS.0..=HOLIDAY_TABLE_YEARS.1).contains(&year) {
        Ok((year, month, day))
    } else {
        Err(OutsideHolidayTable { year, month, day })
    }
}

/// Whether a date (days since 1970-01-01) is a Japanese national holiday, including substitute
/// holidays, citizens' holidays and the one-off holidays of 2019 to 2021.
pub fn is_national_holiday(days: i64) -> Result<bool, OutsideHolidayTable> {
    check_in_table(days)?;
    Ok(holidays().contains(&days))
}

/// Whether a date (days since 1970-01-01) is a Tokyo business day: not a Saturday or Sunday, not a
/// national holiday, and not one of the bank holidays December 31, January 2 and January 3.
/// Japanese banks are closed from December 31 to January 3 by law, and the national holiday list
/// holds only January 1 of those four days.
pub fn is_tokyo_business_day(days: i64) -> Result<bool, OutsideHolidayTable> {
    let (_, month, day) = check_in_table(days)?;
    // 1970-01-01 was a Thursday; 0 is Monday, 5 Saturday, 6 Sunday.
    let weekend = (days + 3).rem_euclid(7) >= 5;
    let bank_holiday = matches!((month, day), (12, 31) | (1, 2) | (1, 3));
    Ok(!weekend && !bank_holiday && !holidays().contains(&days))
}

/// The gotobi days whose date lies in `from..=to` (days since 1970-01-01), each once, in order.
///
/// A nominal day after `to` can move back into the range, so this also looks at the days after
/// `to` up to the next business day; any date it has to look at outside the holiday table is an
/// error, including those days after `to`.
pub fn gotobi_days(from: i64, to: i64) -> Result<Vec<GotobiDay>, OutsideHolidayTable> {
    let mut out: Vec<GotobiDay> = Vec::new();
    if from > to {
        return Ok(out);
    }
    check_in_table(from)?;
    check_in_table(to)?;
    // A nominal day on or after the first business day after `to` moves back to a date after `to`.
    let mut stop = to + 1;
    while !is_tokyo_business_day(stop)? {
        stop += 1;
    }
    let (mut year, mut month, _) = civil_from_days(from);
    loop {
        let (next_year, next_month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        let next_month_start = days_from_civil(next_year, next_month, 1);
        for nominal in NOMINAL_DAYS {
            let nominal = days_from_civil(year, month, nominal);
            if nominal >= next_month_start {
                continue; // February 30
            }
            if nominal >= stop {
                return Ok(out);
            }
            if nominal < from {
                continue;
            }
            let mut date = nominal;
            while date >= from && !is_tokyo_business_day(date)? {
                date -= 1;
            }
            if date < from
                || out
                    .last()
                    .is_some_and(|last| last.tokyo_midnight == midnight(date))
            {
                continue;
            }
            let (y, m, d) = civil_from_days(date);
            out.push(GotobiDay {
                year: y,
                month: m,
                day: d,
                tokyo_midnight: midnight(date),
            });
        }
        (year, month) = (next_year, next_month);
    }
}

fn midnight(days: i64) -> Millis {
    days * DAY_MS - TOKYO_OFFSET_MS
}

/// Whether the Tokyo calendar date (UTC+9) of `t` is a gotobi day.
pub fn is_gotobi_at(t: Millis) -> Result<bool, OutsideHolidayTable> {
    let days = (t + TOKYO_OFFSET_MS).div_euclid(DAY_MS);
    Ok(!gotobi_days(days, days)?.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(year: i64, month: u32, day: u32) -> i64 {
        days_from_civil(year, month, day)
    }

    fn dates(days: &[GotobiDay]) -> Vec<(i64, u32, u32)> {
        days.iter().map(|g| (g.year, g.month, g.day)).collect()
    }

    /// The gotobi days dated in one month.
    fn month(year: i64, month: u32) -> Vec<(i64, u32, u32)> {
        let (ny, nm) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        dates(&gotobi_days(d(year, month, 1), d(ny, nm, 1) - 1).unwrap())
    }

    #[test]
    fn holiday_table_matches_hand_checked_dates() {
        let holiday = |y, m, dd| is_national_holiday(d(y, m, dd)).unwrap();
        assert!(holiday(2016, 1, 1));
        // A substitute holiday: Vernal Equinox Day 2016 fell on Sunday March 20.
        assert!(holiday(2016, 3, 20));
        assert!(holiday(2016, 3, 21));
        assert!(!holiday(2016, 3, 22));
        // 2019: the ten-day Golden Week for the enthronement, Saturday April 27 to Monday May 6,
        // and the enthronement ceremony on October 22.
        assert!(holiday(2019, 4, 29));
        assert!(holiday(2019, 4, 30));
        for day in 1..=6 {
            assert!(holiday(2019, 5, day));
        }
        assert!(holiday(2019, 10, 22));
        // Sports Day and Mountain Day moved for the Olympics in 2020 and 2021.
        assert!(holiday(2020, 7, 24));
        assert!(holiday(2020, 8, 10));
        assert!(!holiday(2020, 10, 12));
        assert!(holiday(2021, 7, 23));
        assert!(holiday(2021, 8, 9));
        assert!(!holiday(2021, 10, 11));
        // The last year of the table.
        assert!(holiday(2027, 3, 22));
        // December 31 and January 2 and 3 are bank holidays, not national holidays.
        assert!(!holiday(2018, 12, 31));
        assert!(!holiday(2019, 1, 2));
    }

    #[test]
    fn business_days_exclude_weekends_holidays_and_the_year_end_bank_holidays() {
        let business = |y, m, dd| is_tokyo_business_day(d(y, m, dd)).unwrap();
        assert!(business(2026, 11, 10)); // Tuesday
        assert!(!business(2026, 11, 14)); // Saturday
        assert!(!business(2026, 11, 15)); // Sunday
        assert!(!business(2026, 11, 3)); // Culture Day, a Tuesday
        assert!(business(2018, 12, 28)); // Friday
        assert!(!business(2018, 12, 31)); // Monday
        assert!(!business(2019, 1, 2)); // Wednesday
        assert!(!business(2019, 1, 3)); // Thursday
        assert!(business(2019, 1, 4)); // Friday
    }

    #[test]
    fn weekend_days_move_back_to_friday() {
        // 2016-11-05 was a Saturday and 2017-11-05 a Sunday.
        assert_eq!(month(2016, 11)[0], (2016, 11, 4));
        assert_eq!(month(2017, 11)[0], (2017, 11, 2)); // Friday November 3 is Culture Day
        assert_eq!(month(2022, 11)[0], (2022, 11, 4)); // a Saturday 5th
        assert_eq!(month(2023, 11)[0], (2023, 11, 2)); // a Sunday 5th, Culture Day on Friday
        assert_eq!(month(2025, 10)[0], (2025, 10, 3)); // a Sunday 5th
    }

    #[test]
    fn a_holiday_after_a_weekend_moves_back_several_days() {
        // 2021-07-25 was a Sunday after Marine Day and Sports Day on July 22 and 23.
        assert_eq!(month(2021, 7)[4], (2021, 7, 21));
        // A query that ends before the nominal day still finds it, and so does a single day.
        assert_eq!(
            dates(&gotobi_days(d(2021, 7, 16), d(2021, 7, 21)).unwrap()),
            [(2021, 7, 20), (2021, 7, 21)]
        );
        assert!(is_gotobi_at(midnight(d(2021, 7, 21))).unwrap());
        // 2019: April 30 and May 5 fall inside the Golden Week and both move back to Friday
        // April 26, which counts once.
        assert_eq!(
            month(2019, 4),
            [
                (2019, 4, 5),
                (2019, 4, 10),
                (2019, 4, 15),
                (2019, 4, 19),
                (2019, 4, 25),
                (2019, 4, 26)
            ]
        );
        assert_eq!(
            month(2019, 5),
            [
                (2019, 5, 10),
                (2019, 5, 15),
                (2019, 5, 20),
                (2019, 5, 24),
                (2019, 5, 30)
            ]
        );
    }

    #[test]
    fn year_end_days_move_back_into_december() {
        // 2020-01-05 was a Sunday. January 4 is a Saturday, January 1 to 3 and December 31 are
        // closed, so it moves back to Monday 2019-12-30, the December 30 gotobi day itself.
        assert_eq!(
            dates(&gotobi_days(d(2019, 12, 26), d(2020, 1, 9)).unwrap()),
            [(2019, 12, 30)]
        );
        assert_eq!(month(2020, 1)[0], (2020, 1, 10));
        // A December query alone still sees January 5 2025 (a Sunday) moving into it.
        assert_eq!(month(2024, 12).last(), Some(&(2024, 12, 30)));
        assert_eq!(month(2024, 12).len(), 6);
        // 2022-01-05 was a Wednesday and stays.
        assert_eq!(month(2022, 1)[0], (2022, 1, 5));
    }

    #[test]
    fn february_has_five_and_month_end_is_not_a_gotobi_day() {
        // 2020 is a leap year and February 15 was a Saturday.
        assert_eq!(
            month(2020, 2),
            [
                (2020, 2, 5),
                (2020, 2, 10),
                (2020, 2, 14),
                (2020, 2, 20),
                (2020, 2, 25)
            ]
        );
        assert_eq!(month(2026, 2).len(), 5);
        assert_eq!(month(2026, 2).last(), Some(&(2026, 2, 25)));
        // The last business day of February and a 31st are not gotobi days.
        assert!(!is_gotobi_at(midnight(d(2026, 2, 27))).unwrap());
        assert!(!is_gotobi_at(midnight(d(2026, 3, 31))).unwrap());
    }

    #[test]
    fn a_day_moving_back_across_a_month_boundary_lands_in_the_previous_month() {
        // 2021-05-05 was Children's Day, a Wednesday, after Constitution Day and Greenery Day and
        // the weekend: it moves back to Friday April 30, also a gotobi day, and counts once.
        assert_eq!(month(2021, 5)[0], (2021, 5, 10));
        assert_eq!(month(2021, 4).last(), Some(&(2021, 4, 30)));
        assert_eq!(month(2021, 4).len(), 6);
    }

    #[test]
    fn the_timestamp_helper_uses_the_tokyo_date() {
        // 2026-11-10 is a Tuesday and a gotobi day; 2026-11-09 is not.
        let fixing = d(2026, 11, 10) * DAY_MS + 55 * 60_000; // 00:55 UTC, 9:55 Tokyo
        assert!(is_gotobi_at(fixing).unwrap());
        // 15:00 UTC on November 9 is 00:00 Tokyo on November 10.
        let tokyo_midnight = d(2026, 11, 9) * DAY_MS + 15 * 3_600_000;
        assert_eq!(tokyo_midnight, month_day_midnight(2026, 11, 10));
        assert!(!is_gotobi_at(tokyo_midnight - 1).unwrap());
        assert!(is_gotobi_at(tokyo_midnight).unwrap());
    }

    fn month_day_midnight(year: i64, month: u32, day: u32) -> Millis {
        gotobi_days(d(year, month, day), d(year, month, day)).unwrap()[0].tokyo_midnight
    }

    #[test]
    fn dates_outside_the_table_are_refused() {
        let dec31 = OutsideHolidayTable {
            year: 2015,
            month: 12,
            day: 31,
        };
        assert_eq!(is_national_holiday(d(2015, 12, 31)), Err(dec31));
        assert_eq!(is_tokyo_business_day(d(2015, 12, 31)), Err(dec31));
        assert!(is_national_holiday(d(2028, 1, 1)).is_err());
        assert!(gotobi_days(d(2015, 12, 1), d(2016, 1, 31)).is_err());
        // The last days of 2027 depend on January 2028, which the table does not hold.
        assert!(gotobi_days(d(2027, 12, 1), d(2027, 12, 31)).is_err());
        assert!(is_gotobi_at(d(2028, 1, 5) * DAY_MS).is_err());
        // A single day outside the table is refused even when no nominal day falls on it.
        assert_eq!(is_gotobi_at(midnight(d(2015, 12, 31))), Err(dec31));
    }

    #[test]
    fn the_published_span_has_a_pinned_count() {
        // The published test reports 775 trades over this span, an independent rebuild 774.
        let all = gotobi_days(d(2016, 1, 1), d(2026, 10, 31)).unwrap();
        assert_eq!(all.len(), 765);
        assert!(
            all.windows(2)
                .all(|w| w[0].tokyo_midnight < w[1].tokyo_midnight)
        );
    }
}
