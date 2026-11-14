//! Stops a trading session carries: rules fixed when the session starts, the state they keep, and
//! the decision whether a stop has fired.
//!
//! There are three rules, all required: a daily loss limit and a maximum loss, each a share of the
//! starting balance, and a cap on the open notional in the account currency. The caller reads the
//! account's equity, open profit and loss included, and the open position's notional, and passes
//! them to [`StopState::check`]; when a stop fires the caller flattens the position, cancels
//! resting orders and halts the session. A fired state stays fired: nothing in this module un-fires
//! it, and a restart restores it from its serialised form. [`StopRules::check_order`] is the
//! pre-trade side of the notional cap.
//!
//! This module only decides. It does no I/O and keeps no clock: every time is given by the caller.

use std::fmt;

use serde::{Deserialize, Serialize};
use td_core::{Millis, civil_from_days};

use crate::metrics::DAY_MS;
use crate::risk::DayRollover;

/// The stop rules of one session, fixed when it starts. Every field is required, in Rust and in
/// the serialised form.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopRules {
    /// The most the account may lose within one trading day, as a share of the starting balance,
    /// above 0 and at most 1: 0.05 is 5%. The loss is measured from the equity at the start of
    /// the day, but the limit is always this share of the starting balance, not of that equity.
    pub daily_loss_share: f64,
    /// The most the account may lose in the whole session, from the starting balance, as a share
    /// of it, above 0 and at most 1. It may be smaller than the daily limit, which then never
    /// fires first.
    pub max_loss_share: f64,
    /// The largest open notional allowed, in the account currency, above zero.
    pub open_notional_cap: f64,
    /// When one trading day ends and the next begins.
    pub rollover: DayRollover,
}

impl StopRules {
    /// Refuses a rule set the check could not judge by: a value that is not finite, a share that
    /// is not above 0 and at most 1, or a cap that is not above zero.
    pub fn validate(&self) -> Result<(), StopError> {
        for (rule, share) in [
            ("daily loss limit", self.daily_loss_share),
            ("maximum loss", self.max_loss_share),
        ] {
            if !share.is_finite() {
                return Err(StopError::NotFinite { rule, value: share });
            }
            if share <= 0.0 || share > 1.0 {
                return Err(StopError::ShareOutOfRange { rule, value: share });
            }
        }
        let cap = self.open_notional_cap;
        if !cap.is_finite() {
            return Err(StopError::NotFinite {
                rule: "open notional cap",
                value: cap,
            });
        }
        if cap <= 0.0 {
            return Err(StopError::CapNotPositive { value: cap });
        }
        Ok(())
    }

    /// Whether an order may be sent, given the open position's notional now and the notional the
    /// order would add, both in the account currency and at or above zero. An order that adds
    /// none, such as a reduce-only order, is always allowed, even when the notional now is above
    /// the cap or could not be read. Any other order is refused when it would take the open
    /// notional above the cap, or when either notional is not a finite number at or above zero.
    ///
    /// Refusing an order does not fire the stop; only [`StopState::check`] does.
    pub fn check_order(&self, open_notional: f64, added_notional: f64) -> Result<(), StopError> {
        if added_notional == 0.0 {
            return Ok(());
        }
        self.validate()?;
        valid_notional(open_notional)?;
        valid_notional(added_notional)?;
        let after = open_notional + added_notional;
        if after > self.open_notional_cap {
            return Err(StopError::OrderOverCap {
                open: open_notional,
                after,
                cap: self.open_notional_cap,
            });
        }
        Ok(())
    }
}

/// Which rule fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopRule {
    MaxLoss,
    DailyLoss,
    OpenNotional,
}

/// The record of a fired stop. Displayed, it reads as one plain sentence.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopFired {
    pub rule: StopRule,
    /// The time given to the check that fired it.
    pub time: Millis,
    /// The equity given to that check, open profit and loss included.
    pub equity: f64,
    /// The open notional given to that check.
    pub notional: f64,
    /// The threshold crossed. For the two loss rules, the equity at or below which the rule fires:
    /// the starting balance, or the day's starting equity, less the limit. For the notional cap,
    /// the cap.
    pub threshold: f64,
}

impl fmt::Display for StopFired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let when = utc(self.time);
        let (equity, notional, threshold) = (self.equity, self.notional, self.threshold);
        match self.rule {
            StopRule::MaxLoss => write!(
                f,
                "The maximum loss stop fired at {when}: equity of {equity:.2} was at or below \
                 {threshold:.2}, the starting balance less the maximum loss. Open notional was \
                 {notional:.2}."
            ),
            StopRule::DailyLoss => write!(
                f,
                "The daily loss stop fired at {when}: equity of {equity:.2} was at or below \
                 {threshold:.2}, the day's starting equity less the daily loss limit. Open \
                 notional was {notional:.2}."
            ),
            StopRule::OpenNotional => write!(
                f,
                "The open notional stop fired at {when}: open notional of {notional:.2} was above \
                 the cap of {threshold:.2}. Equity was {equity:.2}."
            ),
        }
    }
}

/// The state the stops keep for one session. It serialises, so a restart restores it, and
/// restoring it cannot reset a limit: the rules, the starting balance and a fired stop come back
/// as they were.
///
/// The rules are kept in the state, so a session cannot check against other rules than the ones
/// it started with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopState {
    rules: StopRules,
    starting_balance: f64,
    day_start_equity: f64,
    day_ends: Millis,
    last_check: Millis,
    fired: Option<StopFired>,
}

impl StopState {
    /// Starts the stops of a session at `time`, with its starting balance: the account's equity
    /// when the session starts, which is also the first day's starting equity. Refuses rules that
    /// fail [`StopRules::validate`] and a starting balance that is not a finite number above zero.
    pub fn start(rules: StopRules, starting_balance: f64, time: Millis) -> Result<Self, StopError> {
        rules.validate()?;
        valid_balance(starting_balance)?;
        Ok(StopState {
            rules,
            starting_balance,
            day_start_equity: starting_balance,
            day_ends: rules.rollover.next_after(time),
            last_check: time,
            fired: None,
        })
    }

    pub fn rules(&self) -> &StopRules {
        &self.rules
    }

    pub fn starting_balance(&self) -> f64 {
        self.starting_balance
    }

    /// The equity the current trading day started from.
    pub fn day_start_equity(&self) -> f64 {
        self.day_start_equity
    }

    /// When the current trading day ends.
    pub fn day_ends(&self) -> Millis {
        self.day_ends
    }

    /// The stop that fired, if one has.
    pub fn fired(&self) -> Option<&StopFired> {
        self.fired.as_ref()
    }

    /// Checks the stops at `time`, with the account's equity, open profit and loss included, and
    /// the open position's notional, both in the account currency. Returns the fired stop, or
    /// `None` when no stop fires.
    ///
    /// Once a stop has fired, every later call returns the same record, whatever it is given.
    /// Until then, a time earlier than the last check, an equity that is not finite, a notional
    /// that is not a finite number at or above zero, or a restored state whose rules or starting
    /// balance are invalid, is an error, and the state is left as it was. `None` is only returned
    /// for input the check could judge.
    ///
    /// When `time` has reached the end of the trading day, the day rolls over first, and the new
    /// day starts from the equity given in this same call. Then the rules are checked, and when
    /// several fire at once the first of these is recorded:
    ///
    /// - maximum loss: the starting balance less the equity is at or above the maximum loss share
    ///   of the starting balance;
    /// - daily loss: the day's starting equity less the equity is at or above the daily loss share
    ///   of the starting balance;
    /// - open notional: the notional is above the cap.
    ///
    /// A loss limit fires on reaching the limit, since the limit is the most the account may lose;
    /// the cap fires only above it, since the cap is the most the account may hold.
    pub fn check(
        &mut self,
        time: Millis,
        equity: f64,
        notional: f64,
    ) -> Result<Option<StopFired>, StopError> {
        if let Some(fired) = self.fired {
            return Ok(Some(fired));
        }
        self.rules.validate()?;
        valid_balance(self.starting_balance)?;
        if time < self.last_check {
            return Err(StopError::TimeWentBack {
                last: self.last_check,
                got: time,
            });
        }
        if !equity.is_finite() {
            return Err(StopError::EquityNotFinite { value: equity });
        }
        valid_notional(notional)?;

        self.last_check = time;
        if time >= self.day_ends {
            // shortcut: the new day starts from the equity at the first check at or after the
            // rollover, not at the rollover itself, so a loss between the two counts toward the
            // maximum loss but toward neither day's limit. The caller checks every few seconds,
            // which keeps that gap small. Upgrade when the venue reports the equity at the
            // rollover.
            self.day_start_equity = equity;
            self.day_ends = self.rules.rollover.next_after(time);
        }

        let r = &self.rules;
        let start = self.starting_balance;
        let max_loss = r.max_loss_share * start;
        let daily_loss = r.daily_loss_share * start;
        let fired = |rule, threshold| StopFired {
            rule,
            time,
            equity,
            notional,
            threshold,
        };
        self.fired = if start - equity >= max_loss {
            Some(fired(StopRule::MaxLoss, start - max_loss))
        } else if self.day_start_equity - equity >= daily_loss {
            Some(fired(
                StopRule::DailyLoss,
                self.day_start_equity - daily_loss,
            ))
        } else if notional > r.open_notional_cap {
            Some(fired(StopRule::OpenNotional, r.open_notional_cap))
        } else {
            None
        };
        Ok(self.fired)
    }
}

/// Rules the stops refuse, input they cannot judge, and an order over the notional cap.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StopError {
    #[error("the {rule} is {value}, which is not a finite number")]
    NotFinite { rule: &'static str, value: f64 },

    #[error("the {rule} is a share of {value}; it must be above 0 and at most 1")]
    ShareOutOfRange { rule: &'static str, value: f64 },

    #[error("the open notional cap is {value}; it must be above zero")]
    CapNotPositive { value: f64 },

    #[error("the starting balance is {value}; it must be a finite number above zero")]
    InvalidStartingBalance { value: f64 },

    #[error("the equity is {value}, which is not a finite number, so the stops cannot be checked")]
    EquityNotFinite { value: f64 },

    #[error("a notional of {value} cannot be judged: it must be a finite number at or above zero")]
    InvalidNotional { value: f64 },

    #[error("the check at {got} is earlier than the last check at {last}")]
    TimeWentBack { last: Millis, got: Millis },

    #[error(
        "the order would take the open notional from {open} to {after}, above the cap of {cap}"
    )]
    OrderOverCap { open: f64, after: f64, cap: f64 },
}

fn valid_notional(value: f64) -> Result<(), StopError> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(StopError::InvalidNotional { value })
    }
}

fn valid_balance(value: f64) -> Result<(), StopError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(StopError::InvalidStartingBalance { value })
    }
}

/// A time as `2024-03-10 14:05:00 UTC`.
fn utc(t: Millis) -> String {
    let (y, m, d) = civil_from_days(t.div_euclid(DAY_MS));
    let secs = t.rem_euclid(DAY_MS) / 1000;
    let (h, min, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02} UTC")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const HOUR: Millis = 3_600_000;
    const DAY: Millis = DAY_MS;

    // Shares that are exact in binary, so the thresholds are exact: on a starting balance of
    // 10,000 the daily limit is 625, the maximum loss 1,250 and the cap 50,000.
    const START: f64 = 10_000.0;

    fn rules() -> StopRules {
        StopRules {
            daily_loss_share: 0.0625,
            max_loss_share: 0.125,
            open_notional_cap: 50_000.0,
            rollover: DayRollover::default(),
        }
    }

    fn state(rules: StopRules) -> StopState {
        StopState::start(rules, START, 5 * DAY + HOUR).unwrap()
    }

    /// The rule that fires on a fresh state checked once at `equity` and `notional`.
    fn once(rules: StopRules, equity: f64, notional: f64) -> Option<StopRule> {
        state(rules)
            .check(5 * DAY + 2 * HOUR, equity, notional)
            .unwrap()
            .map(|f| f.rule)
    }

    #[test]
    fn the_daily_loss_fires_from_its_limit() {
        // A daily limit above the maximum loss would fire first; make the maximum loss the whole
        // balance so only the daily limit is under test.
        let r = StopRules {
            max_loss_share: 1.0,
            ..rules()
        };
        assert_eq!(once(r, 9_375.01, 0.0), None);
        assert_eq!(once(r, 9_375.0, 0.0), Some(StopRule::DailyLoss));
        assert_eq!(once(r, 9_374.99, 0.0), Some(StopRule::DailyLoss));
        let fired = state(r).check(5 * DAY + 2 * HOUR, 9_375.0, 0.0).unwrap();
        assert_eq!(fired.unwrap().threshold, 9_375.0);
    }

    #[test]
    fn the_maximum_loss_fires_from_its_limit() {
        let r = StopRules {
            daily_loss_share: 1.0,
            ..rules()
        };
        assert_eq!(once(r, 8_750.01, 0.0), None);
        assert_eq!(once(r, 8_750.0, 0.0), Some(StopRule::MaxLoss));
        assert_eq!(once(r, 8_749.99, 0.0), Some(StopRule::MaxLoss));
        let fired = state(r).check(5 * DAY + 2 * HOUR, 8_750.0, 0.0).unwrap();
        assert_eq!(fired.unwrap().threshold, 8_750.0);
    }

    #[test]
    fn the_notional_cap_fires_only_above_it() {
        assert_eq!(once(rules(), START, 49_999.99), None);
        assert_eq!(once(rules(), START, 50_000.0), None);
        assert_eq!(
            once(rules(), START, 50_000.01),
            Some(StopRule::OpenNotional)
        );
    }

    #[test]
    fn the_maximum_loss_comes_first_then_the_daily_loss_then_the_cap() {
        // All three fire.
        assert_eq!(once(rules(), 8_000.0, 60_000.0), Some(StopRule::MaxLoss));
        // 700 down: the daily limit and the cap fire, the maximum loss does not.
        assert_eq!(once(rules(), 9_300.0, 60_000.0), Some(StopRule::DailyLoss));
    }

    #[test]
    fn the_day_rolls_over_at_midnight_utc_and_moves_only_the_daily_baseline() {
        let mut s = state(rules());
        assert_eq!(s.day_ends(), 6 * DAY);
        assert_eq!(s.check(5 * DAY + 20 * HOUR, 9_500.0, 0.0), Ok(None));
        // One millisecond before midnight is still the first day.
        assert_eq!(s.check(6 * DAY - 1, 9_500.0, 0.0), Ok(None));
        assert_eq!(s.day_start_equity(), START);
        // At midnight the new day starts from the equity given then.
        assert_eq!(s.check(6 * DAY, 9_500.0, 0.0), Ok(None));
        assert_eq!(s.day_start_equity(), 9_500.0);
        assert_eq!(s.day_ends(), 7 * DAY);
        assert_eq!(s.starting_balance(), START);
        // 600 down on the day: under the limit, which stays 625 (a share of the starting balance,
        // not of the day's 9,500, which would be 593.75).
        assert_eq!(s.check(6 * DAY + HOUR, 8_900.0, 0.0), Ok(None));
        // 625 down on the day, 1,125 down overall: the daily limit fires, the maximum loss does
        // not.
        let fired = s.check(6 * DAY + 2 * HOUR, 8_875.0, 0.0).unwrap().unwrap();
        assert_eq!(fired.rule, StopRule::DailyLoss);
        assert_eq!(fired.threshold, 8_875.0);
    }

    #[test]
    fn the_maximum_loss_counts_across_days() {
        let mut s = state(rules());
        // 600 lost on each of two days, then 50 more on the third: 1,250 in all.
        assert_eq!(s.check(5 * DAY + 2 * HOUR, 9_400.0, 0.0), Ok(None));
        assert_eq!(s.check(6 * DAY, 9_400.0, 0.0), Ok(None));
        assert_eq!(s.check(6 * DAY + HOUR, 8_800.0, 0.0), Ok(None));
        // A gap of more than a day rolls over once, to the day that holds the check.
        assert_eq!(s.check(8 * DAY + HOUR, 8_800.0, 0.0), Ok(None));
        assert_eq!(s.day_ends(), 9 * DAY);
        let fired = s.check(8 * DAY + 2 * HOUR, 8_750.0, 0.0).unwrap().unwrap();
        assert_eq!(fired.rule, StopRule::MaxLoss);
    }

    #[test]
    fn the_new_york_rollover_follows_daylight_saving() {
        // 2024-03-01 00:00 UTC. Saturday 9 March rolls over at 22:00 UTC; Sunday 10 March, the
        // first day of daylight saving, at 21:00 UTC.
        let mar1: Millis = 1_709_251_200_000;
        let date = |d: i64| mar1 + d * DAY;
        let r = StopRules {
            rollover: DayRollover::NewYork1700,
            ..rules()
        };
        let mut s = StopState::start(r, START, date(8) + 12 * HOUR).unwrap();
        assert_eq!(s.day_ends(), date(8) + 22 * HOUR);
        assert_eq!(s.check(date(8) + 22 * HOUR - 1, 9_800.0, 0.0), Ok(None));
        assert_eq!(s.day_start_equity(), START);
        assert_eq!(s.check(date(8) + 22 * HOUR, 9_800.0, 0.0), Ok(None));
        assert_eq!(s.day_start_equity(), 9_800.0);
        // The day across the change is 23 hours long.
        assert_eq!(s.day_ends(), date(9) + 21 * HOUR);
        assert_eq!(s.check(date(9) + 21 * HOUR - 1, 9_600.0, 0.0), Ok(None));
        assert_eq!(s.day_start_equity(), 9_800.0);
        assert_eq!(s.check(date(9) + 21 * HOUR, 9_600.0, 0.0), Ok(None));
        assert_eq!(s.day_start_equity(), 9_600.0);
        assert_eq!(s.day_ends(), date(10) + 21 * HOUR);
        assert_eq!(s.starting_balance(), START);
    }

    #[test]
    fn a_fired_stop_stays_fired() {
        let mut s = state(rules());
        let fired = s.check(5 * DAY + 2 * HOUR, 9_300.0, 0.0).unwrap().unwrap();
        // Equity back up, flat, a new day: the same record.
        assert_eq!(s.check(9 * DAY, 20_000.0, 0.0), Ok(Some(fired)));
        // Input the check could not judge does not un-fire it either.
        assert_eq!(s.check(0, f64::NAN, -1.0), Ok(Some(fired)));
        assert_eq!(s.fired(), Some(&fired));
    }

    #[test]
    fn a_restored_state_makes_the_same_decisions() {
        let mut s = state(rules());
        assert_eq!(s.check(5 * DAY + 20 * HOUR, 9_700.0, 10_000.0), Ok(None));
        assert_eq!(s.check(6 * DAY + HOUR, 9_600.0, 10_000.0), Ok(None));
        let json = serde_json::to_string(&s).unwrap();
        let mut restored: StopState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, s);
        for (t, e, n) in [
            (6 * DAY + 2 * HOUR, 9_100.0, 10_000.0),
            (6 * DAY + 3 * HOUR, 8_975.0, 10_000.0),
            (6 * DAY + 4 * HOUR, 12_000.0, 0.0),
        ] {
            assert_eq!(restored.check(t, e, n), s.check(t, e, n));
        }
        // The 625 the day started from 9,600 hit at 8,975.
        assert_eq!(s.fired().unwrap().rule, StopRule::DailyLoss);
        // A fired state comes back fired.
        let again: StopState = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(again.fired(), s.fired());
        // An earlier check than the restored state's last one is refused.
        let mut restored: StopState = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            restored.check(6 * DAY, 9_600.0, 0.0),
            Err(StopError::TimeWentBack { .. })
        ));
    }

    #[test]
    fn every_rule_is_required_when_loaded() {
        let full = serde_json::to_value(rules()).unwrap();
        for field in [
            "daily_loss_share",
            "max_loss_share",
            "open_notional_cap",
            "rollover",
        ] {
            let mut v = full.clone();
            v.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<StopRules>(v).is_err(), "{field}");
        }
        assert_eq!(serde_json::from_value::<StopRules>(full).unwrap(), rules());
    }

    #[test]
    fn invalid_rules_are_refused_with_a_reason() {
        // The kind of refusal and the rule it names, for rules with these three values.
        let refusal = |daily, max, cap| {
            let r = StopRules {
                daily_loss_share: daily,
                max_loss_share: max,
                open_notional_cap: cap,
                ..rules()
            };
            match r.validate() {
                Err(StopError::NotFinite { rule, .. }) => ("not finite", rule),
                Err(StopError::ShareOutOfRange { rule, .. }) => ("out of range", rule),
                Err(StopError::CapNotPositive { .. }) => ("not positive", "open notional cap"),
                other => panic!("unexpected {other:?}"),
            }
        };
        let (d, m, c) = (0.0625, 0.125, 50_000.0);
        let (daily, max, cap) = ("daily loss limit", "maximum loss", "open notional cap");
        for (args, want) in [
            ((f64::NAN, m, c), ("not finite", daily)),
            ((d, f64::INFINITY, c), ("not finite", max)),
            ((d, m, f64::NAN), ("not finite", cap)),
            ((d, m, f64::INFINITY), ("not finite", cap)),
            ((0.0, m, c), ("out of range", daily)),
            ((-0.1, m, c), ("out of range", daily)),
            ((1.5, m, c), ("out of range", daily)),
            ((d, 0.0, c), ("out of range", max)),
            ((d, 1.0001, c), ("out of range", max)),
            ((d, m, 0.0), ("not positive", cap)),
            ((d, m, -1.0), ("not positive", cap)),
        ] {
            assert_eq!(refusal(args.0, args.1, args.2), want, "{args:?}");
        }
        // A daily limit above the maximum loss, and shares of exactly 1, are allowed.
        let r = StopRules {
            daily_loss_share: 1.0,
            max_loss_share: 0.5,
            ..rules()
        };
        assert_eq!(r.validate(), Ok(()));
        // Starting refuses invalid rules and a starting balance that is not above zero.
        let bad = StopRules {
            max_loss_share: 2.0,
            ..rules()
        };
        assert!(StopState::start(bad, START, 0).is_err());
        for balance in [0.0, -5.0, f64::NAN, f64::INFINITY] {
            assert!(matches!(
                StopState::start(rules(), balance, 0),
                Err(StopError::InvalidStartingBalance { .. })
            ));
        }
        // A restored state with rules that fail validation cannot be checked.
        let mut v = serde_json::to_value(state(rules())).unwrap();
        v["rules"]["daily_loss_share"] = serde_json::json!(0.0);
        let mut tampered: StopState = serde_json::from_value(v).unwrap();
        assert!(tampered.check(6 * DAY, START, 0.0).is_err());
    }

    #[test]
    fn input_the_check_cannot_judge_is_an_error_and_changes_nothing() {
        let mut s = state(rules());
        let before = s.clone();
        let t = 7 * DAY; // past the day's end, so a pass would also roll the day over
        for (equity, notional) in [
            (f64::NAN, 0.0),
            (f64::INFINITY, 0.0),
            (f64::NEG_INFINITY, 0.0),
            (START, f64::NAN),
            (START, f64::INFINITY),
            (START, -1.0),
        ] {
            assert!(s.check(t, equity, notional).is_err(), "{equity} {notional}");
            assert_eq!(s, before);
        }
        assert_eq!(
            s.check(5 * DAY, START, 0.0),
            Err(StopError::TimeWentBack {
                last: 5 * DAY + HOUR,
                got: 5 * DAY
            })
        );
        assert_eq!(s, before);
        // The same time as the last check is not earlier.
        assert_eq!(s.check(5 * DAY + HOUR, START, 0.0), Ok(None));
    }

    #[test]
    fn an_order_over_the_cap_is_refused_without_firing_the_stop() {
        let r = rules();
        assert_eq!(r.check_order(30_000.0, 19_999.0), Ok(()));
        assert_eq!(r.check_order(30_000.0, 20_000.0), Ok(()));
        assert_eq!(
            r.check_order(30_000.0, 20_001.0),
            Err(StopError::OrderOverCap {
                open: 30_000.0,
                after: 50_001.0,
                cap: 50_000.0
            })
        );
        // An order that adds nothing, such as a reduce-only order, is always allowed.
        assert_eq!(r.check_order(80_000.0, 0.0), Ok(()));
        assert_eq!(r.check_order(f64::NAN, 0.0), Ok(()));
        // Otherwise a notional that cannot be judged refuses the order.
        assert!(r.check_order(f64::NAN, 1.0).is_err());
        assert!(r.check_order(0.0, f64::INFINITY).is_err());
        assert!(r.check_order(0.0, -1.0).is_err());
        // The refusal leaves the stops alone.
        let mut s = state(r);
        assert!(s.rules().check_order(40_000.0, 20_000.0).is_err());
        assert_eq!(s.check(5 * DAY + 2 * HOUR, START, 40_000.0), Ok(None));
    }

    #[test]
    fn a_fired_stop_reads_as_a_sentence() {
        let fired = StopFired {
            rule: StopRule::DailyLoss,
            time: 1_710_079_500_000, // 2024-03-10 14:05:00 UTC
            equity: 9_300.0,
            notional: 1_234.5,
            threshold: 9_375.0,
        };
        assert_eq!(
            fired.to_string(),
            "The daily loss stop fired at 2024-03-10 14:05:00 UTC: equity of 9300.00 was at or \
             below 9375.00, the day's starting equity less the daily loss limit. Open notional \
             was 1234.50."
        );
        let cap = StopFired {
            rule: StopRule::OpenNotional,
            notional: 60_000.0,
            threshold: 50_000.0,
            ..fired
        };
        assert_eq!(
            cap.to_string(),
            "The open notional stop fired at 2024-03-10 14:05:00 UTC: open notional of 60000.00 \
             was above the cap of 50000.00. Equity was 9300.00."
        );
    }

    proptest! {
        #[test]
        fn a_fired_state_never_returns_not_fired(
            checks in prop::collection::vec(
                (
                    -3 * DAY..3 * DAY,
                    prop_oneof![any::<f64>(), -1e6..1e6f64],
                    prop_oneof![any::<f64>(), 0.0..1e6f64],
                ),
                1..20,
            )
        ) {
            let mut s = state(rules());
            let fired = s.check(5 * DAY + 2 * HOUR, 8_000.0, 0.0).unwrap().unwrap();
            let mut t = 5 * DAY + 2 * HOUR;
            for (dt, equity, notional) in checks {
                t += dt;
                prop_assert_eq!(s.check(t, equity, notional), Ok(Some(fired)));
            }
        }
    }
}
