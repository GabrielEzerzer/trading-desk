// Adapted from tipping-point engine/src/store/run.rs at 7dc6d12 (the civil-date functions) and
// engine/src/venue/archive.rs at 7dc6d12 (date parsing).

//! UTC calendar arithmetic on [`Millis`]: dates, months and minute counts. No time zones, no
//! date crate.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use td_core::Millis;

use crate::DataError;

pub const MINUTE: Millis = 60_000;
pub const DAY: Millis = 24 * 60 * MINUTE;

/// Days since 1970-01-01 of a proleptic Gregorian date: Howard Hinnant's `days_from_civil`.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let (month, day) = (i64::from(month), i64::from(day));
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The (year, month, day) of a day count since 1970-01-01: Howard Hinnant's `civil_from_days`.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    // Both are in range by construction: month 1..=12, day 1..=31.
    (year, month as u32, day as u32)
}

/// Reads `YYYY-MM-DD` as midnight UTC that starts the day.
pub fn parse_date(text: &str) -> Result<Millis, DataError> {
    let bad = || DataError::InvalidDate(text.to_string());
    let mut parts = text.split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad());
    };
    let digits =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    if !digits(y, 4) || !digits(m, 2) || !digits(d, 2) {
        return Err(bad());
    }
    let (year, month, day) = (
        y.parse::<i64>().map_err(|_| bad())?,
        m.parse::<u32>().map_err(|_| bad())?,
        d.parse::<u32>().map_err(|_| bad())?,
    );
    // A real date comes back from its day count unchanged; 2024-02-30 would come back as 03-01.
    let days = days_from_civil(year, month, day);
    if !(1..=12).contains(&month) || civil_from_days(days) != (year, month, day) {
        return Err(bad());
    }
    Ok(days * DAY)
}

/// The whole minute nearest `t`; half a minute rounds up.
///
/// Funding times are read through this, at parsing and when stored files are read, because a
/// venue can stamp a rate a few milliseconds after the hour it belongs to. The engine charges
/// funding at a bar's open only when the time is at or before the open, so a stamp a few
/// milliseconds late would be charged on the position after that bar's fills instead of the one
/// held into it.
pub fn round_to_minute(t: Millis) -> Millis {
    (t + MINUTE / 2).div_euclid(MINUTE) * MINUTE
}

/// How far a stamp can be from the minute it rounds to (see [`round_to_minute`]). A venue is asked
/// for funding stamped this far either side of a range, so a rate stamped just before the range
/// starts, which rounds into it, is not lost between two adjacent fetches.
pub(crate) const ROUNDING_PAD: Millis = MINUTE / 2;

/// Funding rates fetched as the venue stamps them, in any order, as `(funding time, rate)` on the
/// minute in `[from, to)`, oldest first. Two stamps on one minute keep the later, as
/// [`crate::Store`] does when it reads a file.
pub(crate) fn funding_on_the_minute(
    mut stamped: Vec<(Millis, f64)>,
    from: Millis,
    to: Millis,
) -> Vec<(Millis, f64)> {
    stamped.sort_by_key(|&(t, _)| t);
    let mut rates: Vec<(Millis, f64)> = Vec::with_capacity(stamped.len());
    for (t, rate) in stamped {
        let t = round_to_minute(t);
        if !(from..to).contains(&t) {
            continue;
        }
        match rates.last_mut() {
            Some(last) if last.0 == t => *last = (t, rate),
            _ => rates.push((t, rate)),
        }
    }
    rates
}

/// Formats as `YYYY-MM-DD HH:MM` UTC.
pub fn format_minute(t: Millis) -> String {
    let (year, month, day) = civil_from_days(t.div_euclid(DAY));
    let minutes = t.rem_euclid(DAY) / MINUTE;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        minutes / 60,
        minutes % 60
    )
}

/// Formats as `YYYY-MM-DD` UTC.
pub fn format_date(t: Millis) -> String {
    let (year, month, day) = civil_from_days(t.div_euclid(DAY));
    format!("{year:04}-{month:02}-{day:02}")
}

/// The current time.
pub fn now_ms() -> Millis {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        Millis::try_from(d.as_millis()).unwrap_or(Millis::MAX)
    })
}

/// A UTC calendar month: the unit of one store file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Month {
    pub year: i64,
    pub month: u32,
}

impl Month {
    /// The month that contains `t`.
    pub fn containing(t: Millis) -> Month {
        let (year, month, _) = civil_from_days(t.div_euclid(DAY));
        Month { year, month }
    }

    /// Reads `YYYY-MM`.
    pub fn parse(text: &str) -> Option<Month> {
        let start = parse_date(&format!("{text}-01")).ok()?;
        Some(Month::containing(start))
    }

    /// Midnight UTC on the first day.
    pub fn start(self) -> Millis {
        days_from_civil(self.year, self.month, 1) * DAY
    }

    /// The start of the next month.
    pub fn end(self) -> Millis {
        self.next().start()
    }

    pub fn next(self) -> Month {
        if self.month == 12 {
            Month {
                year: self.year + 1,
                month: 1,
            }
        } else {
            Month {
                year: self.year,
                month: self.month + 1,
            }
        }
    }

    /// The number of minutes in the month.
    pub fn minutes(self) -> i64 {
        (self.end() - self.start()) / MINUTE
    }

    /// The months that overlap `[from, to)`, in order.
    pub fn range(from: Millis, to: Millis) -> Vec<Month> {
        let mut months = Vec::new();
        if from >= to {
            return months;
        }
        let mut month = Month::containing(from);
        while month.start() < to {
            months.push(month);
            month = month.next();
        }
        months
    }
}

impl fmt::Display for Month {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}", self.year, self.month)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_from_civil_inverts_civil_from_days() {
        for days in [-719_468, -1, 0, 1, 11_016, 19_723, 20_746, 2_932_896] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
    }

    #[test]
    fn dates_parse_to_midnight_utc() {
        assert_eq!(parse_date("2024-01-01").ok(), Some(1_704_067_200_000));
        assert_eq!(parse_date("2025-01-01").ok(), Some(1_735_689_600_000));
        for bad in [
            "2024-02-30",
            "2024-13-01",
            "2024-1-01",
            "2024-01-01x",
            "",
            "2024/01/01",
        ] {
            assert!(parse_date(bad).is_err(), "{bad}");
        }
        assert_eq!(format_date(1_704_067_200_000), "2024-01-01");
        assert_eq!(format_minute(1_704_067_260_000), "2024-01-01 00:01");
    }

    #[test]
    fn months_know_their_bounds_and_ranges() {
        let feb = Month::parse("2024-02").expect("a month");
        assert_eq!(feb.to_string(), "2024-02");
        assert_eq!(feb.minutes(), 29 * 1440);
        assert_eq!(Month::containing(feb.end() - 1), feb);
        let dec = Month {
            year: 2023,
            month: 12,
        };
        assert_eq!(dec.next().to_string(), "2024-01");
        let months = Month::range(dec.start() + DAY, feb.start() + 1);
        let names: Vec<String> = months.iter().map(Month::to_string).collect();
        assert_eq!(names, ["2023-12", "2024-01", "2024-02"]);
        assert!(Month::range(feb.start(), feb.start()).is_empty());
    }
}
