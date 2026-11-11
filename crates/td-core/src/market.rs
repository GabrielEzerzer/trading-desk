//! What each [`InstrumentKind`] does as a market: one rules table, and its trading hours.

use std::ops::Range;

use crate::{InstrumentKind, Millis, days_from_civil};

/// The rules of one market type. Code that differs by market asks [`InstrumentKind::rules`]
/// instead of naming kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketRules {
    /// Positions are margined: the account holds collateral and books profit and loss, rather than
    /// paying cash for what it holds.
    pub margin: bool,
    /// A position can go below zero.
    pub can_short: bool,
    /// Open positions pay or receive funding at the venue's funding times.
    pub funding: bool,
    /// A position is liquidated when its equity falls to the maintenance margin.
    pub liquidation: bool,
    /// The kind needs an ask series beside its candles, which hold the bid.
    pub quotes: bool,
    pub hours: Hours,
}

/// When a market is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hours {
    /// Never closes.
    Always,
    /// The FX week: closed from Friday 17:00 to Sunday 17:00 New York time.
    FxWeek,
}

impl InstrumentKind {
    /// What this kind does as a market.
    pub const fn rules(self) -> MarketRules {
        match self {
            InstrumentKind::Spot => MarketRules {
                margin: false,
                can_short: false,
                funding: false,
                liquidation: false,
                quotes: false,
                hours: Hours::Always,
            },
            InstrumentKind::Perpetual => MarketRules {
                margin: true,
                can_short: true,
                funding: true,
                liquidation: true,
                quotes: false,
                hours: Hours::Always,
            },
            // FX margin rules (margin call and stop-out) are a named gap on #44, so FX has no
            // liquidation yet.
            InstrumentKind::Fx => MarketRules {
                margin: true,
                can_short: true,
                funding: false,
                liquidation: false,
                quotes: true,
                hours: Hours::FxWeek,
            },
        }
    }

    /// Whether the market is open at `t`.
    pub fn is_open(self, t: Millis) -> bool {
        match self.rules().hours {
            Hours::Always => true,
            Hours::FxWeek => {
                let day = t.div_euclid(DAY);
                // A weekend's close is on Friday evening and its open on Sunday evening in New
                // York, which is Friday 21:00 to Sunday 22:00 UTC at the widest, so only an
                // instant on a UTC Friday, Saturday or Sunday can be closed.
                let to_sunday = match weekday(day) {
                    FRIDAY => 2,
                    SATURDAY => 1,
                    SUNDAY => 0,
                    _ => return true,
                };
                !fx_weekend(day + to_sunday).contains(&t)
            }
        }
    }

    /// The closed time in `[from, to)`, as half-open spans clipped to it, in order.
    pub fn closed_spans(self, from: Millis, to: Millis) -> Vec<Range<Millis>> {
        let mut spans = Vec::new();
        if self.rules().hours == Hours::Always {
            return spans;
        }
        // The weekend that ends on the last Sunday on or before `from` is the earliest that can
        // reach into the range.
        let first = from.div_euclid(DAY);
        let mut sunday = first - weekday(first);
        loop {
            let weekend = fx_weekend(sunday);
            if weekend.start >= to {
                return spans;
            }
            let span = weekend.start.max(from)..weekend.end.min(to);
            if span.start < span.end {
                spans.push(span);
            }
            sunday += 7;
        }
    }
}

const HOUR: Millis = 3_600_000;
const DAY: Millis = 24 * HOUR;
const SUNDAY: i64 = 0;
const FRIDAY: i64 = 5;
const SATURDAY: i64 = 6;

/// The day of the week of a day count since 1970-01-01, Sunday as 0. That day was a Thursday.
fn weekday(day: i64) -> i64 {
    (day + 4).rem_euclid(7)
}

/// The FX weekend that ends on `sunday` (a day count since 1970-01-01): from Friday 17:00 to
/// Sunday 17:00 New York time, in UTC.
fn fx_weekend(sunday: i64) -> Range<Millis> {
    let five_pm = |day: i64| day * DAY + (17 + new_york_offset_hours(day)) * HOUR;
    five_pm(sunday - 2)..five_pm(sunday)
}

/// Hours to add to New York local time to get UTC, for a time after 02:00 on local `day`: 4 in US
/// daylight saving, which runs from 02:00 on the second Sunday of March to 02:00 on the first
/// Sunday of November, and 5 outside it.
///
/// shortcut: this is the US rule since 2007. Before 2007 daylight saving ran from the first Sunday
/// of April to the last Sunday of October, so FX weekends in the weeks between the two rules
/// (mid-March to early April, and late October to early November) come out an hour off for data
/// before 2007. Upgrade to a per-year rule when the desk backtests FX before 2007.
fn new_york_offset_hours(day: i64) -> i64 {
    let (year, _, _) = crate::civil_from_days(day);
    let first_sunday = |month: u32| {
        let first = days_from_civil(year, month, 1);
        first + (7 - weekday(first)) % 7
    };
    let starts = first_sunday(3) + 7;
    let ends = first_sunday(11);
    if (starts..ends).contains(&day) { 4 } else { 5 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use InstrumentKind::{Fx, Perpetual, Spot};

    const MINUTE: Millis = 60_000;

    /// `hour:minute` UTC on a date.
    fn utc(year: i64, month: u32, day: u32, hour: i64, minute: i64) -> Millis {
        days_from_civil(year, month, day) * DAY + hour * HOUR + minute * MINUTE
    }

    #[test]
    fn rules_table() {
        let spot = Spot.rules();
        assert!(!spot.margin);
        assert!(!spot.can_short);
        assert!(!spot.funding);
        assert!(!spot.liquidation);
        assert!(!spot.quotes);
        assert_eq!(spot.hours, Hours::Always);

        let perp = Perpetual.rules();
        assert!(perp.margin);
        assert!(perp.can_short);
        assert!(perp.funding);
        assert!(perp.liquidation);
        assert!(!perp.quotes);
        assert_eq!(perp.hours, Hours::Always);

        let fx = Fx.rules();
        assert!(fx.margin);
        assert!(fx.can_short);
        assert!(!fx.funding);
        assert!(!fx.liquidation);
        assert!(fx.quotes);
        assert_eq!(fx.hours, Hours::FxWeek);
    }

    /// Checks one weekend: `close` is Friday 17:00 New York in UTC and `open` Sunday 17:00.
    fn assert_weekend(close: Millis, open: Millis) {
        assert!(Fx.is_open(close - MINUTE), "Friday 16:59 is open");
        assert!(!Fx.is_open(close), "Friday 17:00 is closed");
        assert!(!Fx.is_open(open - MINUTE), "Sunday 16:59 is closed");
        assert!(Fx.is_open(open), "Sunday 17:00 is open");
        assert_eq!(Fx.closed_spans(close - DAY, open + DAY), vec![close..open]);
    }

    #[test]
    fn fx_week_in_winter() {
        // Friday 2024-01-12 and Sunday 2024-01-14 are in standard time, UTC-5: 17:00 is 22:00 UTC.
        assert_weekend(utc(2024, 1, 12, 22, 0), utc(2024, 1, 14, 22, 0));
    }

    #[test]
    fn fx_week_in_summer() {
        // Friday 2024-07-12 and Sunday 2024-07-14 are in daylight saving, UTC-4: 17:00 is
        // 21:00 UTC.
        assert_weekend(utc(2024, 7, 12, 21, 0), utc(2024, 7, 14, 21, 0));
    }

    #[test]
    fn fx_week_when_daylight_saving_starts() {
        // Daylight saving started at 02:00 on Sunday 2024-03-10, the second Sunday of March. The
        // Friday before, 2024-03-08, is still UTC-5 (22:00 UTC); that Sunday's 17:00 is already
        // UTC-4 (21:00 UTC). The weekend is an hour shorter.
        assert_weekend(utc(2024, 3, 8, 22, 0), utc(2024, 3, 10, 21, 0));
    }

    #[test]
    fn fx_week_when_daylight_saving_ends() {
        // Daylight saving ended at 02:00 on Sunday 2024-11-03, the first Sunday of November. The
        // Friday before, 2024-11-01, is still UTC-4 (21:00 UTC); that Sunday's 17:00 is UTC-5
        // (22:00 UTC). The weekend is an hour longer.
        assert_weekend(utc(2024, 11, 1, 21, 0), utc(2024, 11, 3, 22, 0));
    }

    #[test]
    fn closed_spans_are_clipped_to_the_range() {
        // Monday 2024-01-08 00:00 UTC to Monday 2024-01-15 00:00 UTC holds one whole weekend.
        let close = utc(2024, 1, 12, 22, 0);
        let open = utc(2024, 1, 14, 22, 0);
        let week = utc(2024, 1, 8, 0, 0)..utc(2024, 1, 15, 0, 0);
        assert_eq!(Fx.closed_spans(week.start, week.end), vec![close..open]);

        // From Saturday noon, inside the weekend, the span starts at the range's start.
        let saturday = utc(2024, 1, 13, 12, 0);
        assert_eq!(Fx.closed_spans(saturday, week.end), vec![saturday..open]);
        // From Sunday noon UTC too, which is still inside the weekend that ends that evening.
        let sunday = utc(2024, 1, 14, 12, 0);
        assert_eq!(Fx.closed_spans(sunday, week.end), vec![sunday..open]);
        // And a range that ends inside it stops there.
        assert_eq!(Fx.closed_spans(week.start, saturday), vec![close..saturday]);

        // Monday to Friday noon has no closed time.
        assert!(
            Fx.closed_spans(week.start, utc(2024, 1, 12, 12, 0))
                .is_empty()
        );
        // Nor has an empty or reversed range.
        assert!(Fx.closed_spans(saturday, saturday).is_empty());
        assert!(Fx.closed_spans(week.end, week.start).is_empty());

        // Two weeks hold two weekends, in order.
        let next = utc(2024, 1, 19, 22, 0)..utc(2024, 1, 21, 22, 0);
        assert_eq!(
            Fx.closed_spans(week.start, week.end + 7 * DAY),
            vec![close..open, next]
        );

        // Crypto never closes.
        for kind in [Spot, Perpetual] {
            assert!(kind.closed_spans(week.start, week.end).is_empty());
            assert!(kind.is_open(saturday));
        }
    }

    #[test]
    fn is_open_agrees_with_closed_spans_minute_by_minute() {
        // A week either side of each daylight saving switch in 2024, and one in summer.
        for start in [
            utc(2024, 3, 4, 0, 0),
            utc(2024, 7, 8, 0, 0),
            utc(2024, 10, 28, 0, 0),
        ] {
            let end = start + 14 * DAY;
            let spans = Fx.closed_spans(start, end);
            assert_eq!(spans.len(), 2);
            let mut t = start;
            while t < end {
                let closed = spans.iter().any(|span| span.contains(&t));
                assert_eq!(Fx.is_open(t), !closed, "at {t}");
                t += MINUTE;
            }
        }
    }
}
