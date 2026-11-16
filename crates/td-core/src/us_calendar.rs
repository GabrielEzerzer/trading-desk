//! The NYSE and Nasdaq regular session: 09:30 to 16:00 New York, 13:00 on early closes, on trading days only.
//! Holidays and early closes follow the exchanges' rules from 2016 on, with the one-off closures since then.

use std::ops::Range;

use crate::market::{new_york_offset_hours, weekday};
use crate::{Millis, civil_from_days, days_from_civil};

const MINUTE: Millis = 60_000;
const HOUR: Millis = 60 * MINUTE;
const DAY: Millis = 24 * HOUR;
const MONDAY: i64 = 1;
const THURSDAY: i64 = 4;
const FRIDAY: i64 = 5;
const SATURDAY: i64 = 6;
const SUNDAY: i64 = 0;

const ONE_OFF_CLOSURES: [(i64, u32, u32); 2] = [(2018, 12, 5), (2025, 1, 9)];

pub fn us_session(day: i64) -> Option<Range<Millis>> {
    let wd = weekday(day);
    if wd == SATURDAY || wd == SUNDAY || is_holiday(day) {
        return None;
    }
    let local = |minutes: i64| day * DAY + minutes * MINUTE + new_york_offset_hours(day) * HOUR;
    let close = if is_early_close(day) {
        13 * 60
    } else {
        16 * 60
    };
    Some(local(9 * 60 + 30)..local(close))
}

fn is_holiday(day: i64) -> bool {
    let (year, _, _) = civil_from_days(day);
    let date = |m: u32, d: u32| days_from_civil(year, m, d);
    let fixed = |m: u32, d: u32| observed(date(m, d));
    let mut holidays = vec![
        nth_weekday(year, 1, MONDAY, 3),
        nth_weekday(year, 2, MONDAY, 3),
        easter(year) - 2,
        last_weekday(year, 5, MONDAY),
        fixed(7, 4),
        nth_weekday(year, 9, MONDAY, 1),
        nth_weekday(year, 11, THURSDAY, 4),
        fixed(12, 25),
    ];
    if weekday(date(1, 1)) != SATURDAY {
        holidays.push(fixed(1, 1));
    }
    if year >= 2022 {
        holidays.push(fixed(6, 19));
    }
    holidays.contains(&day)
        || ONE_OFF_CLOSURES
            .iter()
            .any(|&(y, m, d)| days_from_civil(y, m, d) == day)
}

fn is_early_close(day: i64) -> bool {
    let (year, month, date) = civil_from_days(day);
    let wd = weekday(day);
    let eve = (month, date) == (7, 3) || (month, date) == (12, 24);
    (eve && wd != FRIDAY) || day == nth_weekday(year, 11, THURSDAY, 4) + 1
}

fn observed(day: i64) -> i64 {
    match weekday(day) {
        SATURDAY => day - 1,
        SUNDAY => day + 1,
        _ => day,
    }
}

fn nth_weekday(year: i64, month: u32, wd: i64, n: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    first + (wd - weekday(first)).rem_euclid(7) + 7 * (n - 1)
}

fn last_weekday(year: i64, month: u32, wd: i64) -> i64 {
    let next = if month == 12 {
        days_from_civil(year + 1, 1, 1)
    } else {
        days_from_civil(year, month + 1, 1)
    };
    let last = next - 1;
    last - (weekday(last) - wd).rem_euclid(7)
}

fn easter(year: i64) -> i64 {
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let date = (h + l - 7 * m + 114) % 31 + 1;
    days_from_civil(year, month as u32, date as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i64, m: u32, d: u32) -> i64 {
        days_from_civil(y, m, d)
    }

    fn utc(y: i64, m: u32, d: u32, hour: i64, minute: i64) -> Millis {
        day(y, m, d) * DAY + hour * HOUR + minute * MINUTE
    }

    fn trading_days(year: i64) -> Vec<i64> {
        (day(year, 1, 1)..day(year + 1, 1, 1))
            .filter(|&d| us_session(d).is_some())
            .collect()
    }

    fn closed_weekdays(year: i64) -> Vec<(u32, u32)> {
        (day(year, 1, 1)..day(year + 1, 1, 1))
            .filter(|&d| weekday(d) != SATURDAY && weekday(d) != SUNDAY)
            .filter(|&d| us_session(d).is_none())
            .map(|d| {
                let (_, m, dd) = civil_from_days(d);
                (m, dd)
            })
            .collect()
    }

    fn early_closes(year: i64) -> Vec<(u32, u32)> {
        trading_days(year)
            .into_iter()
            .filter(|&d| is_early_close(d))
            .map(|d| {
                let (_, m, dd) = civil_from_days(d);
                (m, dd)
            })
            .collect()
    }

    #[test]
    fn the_2024_calendar_has_its_monday_holidays_and_early_closes() {
        assert_eq!(
            closed_weekdays(2024),
            [
                (1, 1),
                (1, 15),
                (2, 19),
                (3, 29),
                (5, 27),
                (6, 19),
                (7, 4),
                (9, 2),
                (11, 28),
                (12, 25)
            ]
        );
        assert_eq!(early_closes(2024), [(7, 3), (11, 29), (12, 24)]);
        assert_eq!(trading_days(2024).len(), 252);
    }

    #[test]
    fn sessions_are_in_new_york_time_through_daylight_saving() {
        assert_eq!(
            us_session(day(2024, 1, 2)),
            Some(utc(2024, 1, 2, 14, 30)..utc(2024, 1, 2, 21, 0))
        );
        assert_eq!(
            us_session(day(2024, 7, 2)),
            Some(utc(2024, 7, 2, 13, 30)..utc(2024, 7, 2, 20, 0))
        );
        assert_eq!(
            us_session(day(2024, 7, 3)),
            Some(utc(2024, 7, 3, 13, 30)..utc(2024, 7, 3, 17, 0))
        );
        assert_eq!(
            us_session(day(2024, 11, 29)),
            Some(utc(2024, 11, 29, 14, 30)..utc(2024, 11, 29, 18, 0))
        );
        assert_eq!(us_session(day(2024, 1, 6)), None);
        assert_eq!(us_session(day(2024, 1, 7)), None);
    }

    #[test]
    fn weekend_holidays_move_by_the_exchange_rules() {
        assert_eq!(closed_weekdays(2021).first(), Some(&(1, 1)));
        assert!(!closed_weekdays(2021).contains(&(12, 31)));
        assert!(closed_weekdays(2021).contains(&(12, 24)));
        assert!(closed_weekdays(2021).contains(&(7, 5)));
        assert!(!closed_weekdays(2021).contains(&(6, 18)));
        assert!(closed_weekdays(2022).contains(&(6, 20)));
        assert!(!closed_weekdays(2022).contains(&(1, 1)));
        assert!(closed_weekdays(2023).contains(&(1, 2)));
        assert!(closed_weekdays(2026).contains(&(7, 3)));
        assert!(closed_weekdays(2027).contains(&(6, 18)));
        assert_eq!(early_closes(2021), [(11, 26)]);
        assert_eq!(early_closes(2026), [(11, 27), (12, 24)]);
        assert_eq!(early_closes(2023), [(7, 3), (11, 24)]);
    }

    #[test]
    fn one_off_closures_and_good_friday() {
        assert!(closed_weekdays(2018).contains(&(12, 5)));
        assert!(closed_weekdays(2025).contains(&(1, 9)));
        assert!(closed_weekdays(2016).contains(&(3, 25)));
        assert!(closed_weekdays(2019).contains(&(4, 19)));
        assert!(closed_weekdays(2025).contains(&(4, 18)));
    }

    #[test]
    fn trading_days_per_year_match_the_exchange() {
        let counts: Vec<usize> = (2016..=2025).map(|y| trading_days(y).len()).collect();
        assert_eq!(counts, [252, 251, 251, 252, 253, 252, 251, 250, 252, 250]);
    }
}
