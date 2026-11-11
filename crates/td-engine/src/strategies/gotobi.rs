//! Gotobi: sell USD/JPY ahead of the 9:55 Tokyo fixing on gotobi days, when Japanese importers buy
//! dollars to settle payments, and buy it back at 10:30 once the fixing has passed. The rules are
//! fixed to reproduce a published result (#45) over 2016-01-01 to 2026-10-31: about 775 trades,
//! +2.35 pips per trade after the spread, a t-statistic of 5.5 and 58% winners. The day is the
//! calendar in `gotobi_calendar.rs`.
//!
//! The file has the three parts of `sma_cross.rs`: the strategy, its test plan and its verdict
//! rule.

use td_core::{
    AccountCurrency, Candle, InstrumentId, InstrumentKind, Millis, OrderId, OrderType, Side,
    Timeframe, Venue,
};

use crate::strategies::gotobi_calendar::{
    OutsideHolidayTable, is_gotobi_at, is_tokyo_business_day,
};
use crate::strategies::order;
use crate::{
    BacktestSettings, Context, Direction, EngineError, EvidenceSettings, HighRiskLimits, Metric,
    MinSample, Objective, ParamAxis, ParamKind, ParamSpace, ParamSpec, ParamValue, Params, Period,
    SlippageOverride, Strategy, StrategyEntry, SweepPlan, TestPlan, VerdictRule, WalkForwardPlan,
    date,
};

// ---------------------------------------------------------------------------------------------
// 1. The strategy
// ---------------------------------------------------------------------------------------------

const MINUTE: Millis = 60_000;
const DAY: Millis = 86_400_000;
/// Tokyo is UTC+9 all year; Japan has no daylight saving time.
const TOKYO_OFFSET: Millis = 9 * 60 * MINUTE;
/// 9:55 Tokyo, the fixing, as a time of day in UTC: 00:55.
const ENTRY_UTC: Millis = 55 * MINUTE;
/// 10:30 Tokyo as a time of day in UTC: 01:30.
const EXIT_UTC: Millis = 90 * MINUTE;

/// The gotobi short on USD/JPY, in engine terms:
///
/// - **Day.** The Tokyo date of the bar is a gotobi day ([`is_gotobi_at`]), or with `control` on,
///   a Tokyo business day that is not one.
/// - **Entry.** At the close of the bar that closes at 00:55 UTC (9:55 Tokyo), when flat, a market
///   sell, which fills at the next bar's open, the bar that opens at 00:55, on the bid.
/// - **Stop.** Attached to the entry with [`Context::submit_with_stop`], `stop_pips` pips above
///   the decision bar's close. The fill price is not known when the order is sent, so the stop is
///   measured from the bid close at the decision, not from the fill. It is a buy stop, so it
///   triggers when the ask reaches it: the spread counts against it.
/// - **Exit.** At the close of the first bar that closes at or after 01:30 UTC (10:30 Tokyo) on
///   the entry's day, which is a later bar when the data has a gap there, while the position is
///   open, the stop is cancelled and a reduce-only market buy for the whole position is sent. It
///   fills at the next bar's open, the bar that opens at 01:30 when there is no gap, on the ask. A
///   stop that has already filled has ended the trade, and nothing happens at 01:30. If the buy
///   is rejected, the run fails ([`Context::fail`]) rather than hold the position without a stop.
/// - **Size.** `risk_pct` percent of the equity is lost if the stop is hit: the quantity is
///   `risk_pct / 100 * equity / to_account(stop distance)`, rounded down to the lot size by
///   [`Context::submit`]. On a dollar account the stop distance, in yen per unit, is converted to
///   dollars at the decision close ([`Context::to_account`]).
/// - **One trade a day**, never more than one position: an entry is only sent when flat with no
///   order open, and only once per Tokyo day, even if it is rejected.
///
/// The run fails ([`Context::fail`]) rather than giving a run with no trades on a bar whose Tokyo
/// date the holiday table does not cover (2016 to 2027; the last days of 2027 fail too, since
/// whether they are gotobi days depends on January 2028), on a timeframe on which no bar closes
/// at both 00:55 and 01:30 UTC (anything but 1m and 5m), and on an instrument without pips.
///
/// shortcut: a gotobi day whose decision bar (the bar that closes at 00:55) is missing from the
/// data has no trade, and an entry whose fill bar is missing fills at the next bar there is. Data
/// with gaps in the Tokyo morning then undercounts trades; fill the decision from the last bar
/// before 00:55 if gaps there turn out to matter in the Dukascopy data.
#[derive(Debug, Clone)]
pub struct Gotobi {
    pub stop_pips: f64,
    pub risk_pct: f64,
    pub control: bool,
    /// The protective stop attached to the current entry.
    stop: Option<OrderId>,
    /// When the open position is closed: 01:30 UTC on the entry's day.
    exit_at: Option<Millis>,
    /// The Tokyo date (days since 1970-01-01) of the last entry sent.
    last_entry_day: Option<i64>,
    /// The Tokyo date of the last bar and whether it is a day to trade.
    day: Option<(i64, bool)>,
}

pub(crate) fn entry() -> StrategyEntry {
    StrategyEntry {
        name: "gotobi",
        description: "Sells USD/JPY at 9:55 Tokyo on gotobi days and buys it back at 10:30, with \
                      a stop in pips sized to risk a set percentage of the account.",
        params: vec![
            ParamSpec {
                name: "stop_pips",
                kind: ParamKind::Float,
                default: ParamValue::Float(12.0),
                min: Some(0.1),
                max: None,
                description: "Distance from the bid close the entry is decided on to its stop, \
                              in pips.",
            },
            ParamSpec {
                name: "risk_pct",
                kind: ParamKind::Float,
                default: ParamValue::Float(0.5),
                min: Some(0.0),
                max: Some(100.0),
                description: "Percentage of equity lost if the stop is hit.",
            },
            ParamSpec {
                name: "control",
                kind: ParamKind::Bool,
                default: ParamValue::Bool(false),
                min: None,
                max: None,
                description: "Trade the same rule on every Tokyo business day that is not a \
                              gotobi day instead, to check that the edge belongs to the calendar.",
            },
        ],
        build,
        plan: plan(),
        rule: rule(),
    }
}

fn build(p: &Params) -> Result<Box<dyn Strategy>, EngineError> {
    Ok(Box::new(Gotobi {
        stop_pips: p.float("stop_pips")?,
        risk_pct: p.float("risk_pct")?,
        control: p.bool("control")?,
        stop: None,
        exit_at: None,
        last_entry_day: None,
        day: None,
    }))
}

impl Gotobi {
    /// Whether the Tokyo date of `t` is a day this strategy trades.
    fn trades_on(&self, t: Millis) -> Result<bool, OutsideHolidayTable> {
        let gotobi = is_gotobi_at(t)?;
        if self.control {
            let days = (t + TOKYO_OFFSET).div_euclid(DAY);
            Ok(!gotobi && is_tokyo_business_day(days)?)
        } else {
            Ok(gotobi)
        }
    }
}

impl Strategy for Gotobi {
    fn on_start(&mut self, ctx: &mut Context) {
        let tf = ctx.timeframe();
        let bar = tf.duration_ms();
        if ENTRY_UTC % bar != 0 || EXIT_UTC % bar != 0 {
            ctx.fail(format!(
                "gotobi decides at 00:55 and 01:30 UTC, and no {tf} bar closes at both: run it on \
                 1m or 5m bars"
            ));
        } else if ctx.instrument().pip_size().is_none() {
            let id = ctx.instrument().id.clone();
            ctx.fail(format!(
                "gotobi sets its stop in pips, and {id} has none: run it on an FX pair"
            ));
        }
    }

    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let day = (bar.open_time + TOKYO_OFFSET).div_euclid(DAY);
        let trade_day = match self.day {
            Some((d, trades)) if d == day => trades,
            _ => match self.trades_on(bar.open_time) {
                Ok(trades) => {
                    self.day = Some((day, trades));
                    trades
                }
                Err(e) => {
                    ctx.fail(format!(
                        "gotobi needs the Tokyo holiday calendar for every bar: {e}"
                    ));
                    return;
                }
            },
        };
        // Send nothing while warming up: it would only be rejected.
        if ctx.is_warming_up() {
            return;
        }
        let close = bar.open_time + ctx.timeframe().duration_ms();
        let position = ctx.position();
        if position != 0.0 {
            if self.exit_at.is_some_and(|at| close >= at) {
                self.exit_at = None;
                // A reduce-only stop left resting would stay open once the position is closed.
                if let Some(stop) = self.stop.take() {
                    ctx.cancel(stop);
                }
                let side = if position > 0.0 {
                    Side::Sell
                } else {
                    Side::Buy
                };
                let exit = order(ctx, side, position.abs(), OrderType::Market, true);
                if let Err(e) = ctx.submit(exit) {
                    ctx.fail(format!(
                        "gotobi's exit at 01:30 UTC was rejected after its stop was cancelled, \
                         which would leave the position without a stop: {e}"
                    ));
                }
            }
            return;
        }
        // Flat: the last trade, if any, has ended, at its exit or its stop.
        self.stop = None;
        self.exit_at = None;
        if !trade_day
            || close.rem_euclid(DAY) != ENTRY_UTC
            || self.last_entry_day == Some(day)
            || !ctx.open_orders().is_empty()
        {
            return;
        }
        let Some(pip) = ctx.instrument().pip_size() else {
            return; // refused in on_start
        };
        self.last_entry_day = Some(day);
        let distance = self.stop_pips * pip;
        // The distance is a price, in the quote currency; the risk is in the account's.
        let qty = self.risk_pct / 100.0 * ctx.equity() / ctx.to_account(distance);
        let entry = order(ctx, Side::Sell, qty, OrderType::Market, false);
        // A rejected entry (too small, or over the leverage cap) means no trade today.
        if let Ok((_, stop)) = ctx.submit_with_stop(entry, bar.close + distance) {
            self.stop = Some(stop);
            self.exit_at = Some(close - ENTRY_UTC + EXIT_UTC);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 2. The test plan
// ---------------------------------------------------------------------------------------------

/// Dukascopy USD/JPY on 5-minute bars over the published period, on a dollar account.
///
/// - **Timeframe.** 5m, the coarsest on which bars close at both 00:55 and 01:30 UTC.
/// - **Account.** Dollars, the pair's base currency, as the published test reports; 100,000 of
///   starting cash. The 30x cap is the retail FX limit on majors in Japan and the EU; at 0.5% risk
///   on a 12-pip stop a trade is about 6x, so the cap only refuses entries from much larger risk
///   settings. Slippage is 0 because the bid and ask bars already carry the cost of crossing the
///   spread, which is the cost the published result is stated after. The cost stress (R4)
///   multiplies fees and slippage, which are both 0 here, and not the spread, so it has nothing to
///   stress on this plan; a comparison with the published figure after a commission needs a run
///   with the commission set.
/// - **Sweep.** Only `stop_pips`, from 6 to 24 in steps of 3, so 12 has two neighbours on each
///   side for the plateau test (R5) and a stop twice as tight or twice as wide is tried.
///   `risk_pct` is fixed at its default: it only scales the size, and sweeping it would have the
///   sweep pick leverage, not anything about the calendar. `control` is fixed off, since the
///   control run is a separate run with it on. The objective is the Sharpe ratio, as in the other
///   plans; with the size fixed by the risk it ranks the stops by return per unit of variation.
///   A set needs 50 trades to rank: a train window holds about 140, so a stop that loses most of
///   them to the 5m noise still ranks, but a broken one that hardly trades does not.
/// - **Walk-forward.** About 70 trades a year, so a train window of 520 days of bars (two years of
///   FX weeks) holds about 140 trades to choose a stop on, and a test window of 130 days (half a
///   year) about 35, enough that the share of profitable windows (R1) is not decided by one or
///   two trades. The period then holds 17 windows and about 8.5 years out of sample.
/// - **Out-of-sample split.** None: the walk-forward already gives the out-of-sample curve, and a
///   single split is one more draw, reported only.
/// - **Evidence.** The defaults, with the minimum sample raised to 200 out-of-sample trades. The
///   published trade outcomes have a standard deviation of about 12 pips (2.35 pips at t = 5.5
///   over 775 trades), so a mean of about 2 pips sits two standard errors above zero only from
///   about 140 trades on; below 200 the sample is too small to tell an edge of that size from
///   none.
fn plan() -> TestPlan {
    let usdjpy = || InstrumentId {
        venue: Venue::Dukascopy,
        symbol: "USDJPY".into(),
        kind: InstrumentKind::Fx,
    };
    TestPlan {
        instruments: vec![usdjpy()],
        timeframe: Timeframe::M5,
        period: Period {
            from: date(2016, 1, 1),
            to: date(2026, 10, 31),
        },
        backtest: BacktestSettings {
            starting_cash: 100_000.0,
            max_leverage: 30.0,
            slippage: vec![SlippageOverride {
                instrument: usdjpy(),
                bps: 0.0,
            }],
            account_currency: AccountCurrency::Base,
        },
        sweep: SweepPlan {
            fixed: Params::new()
                .with("risk_pct", ParamValue::Float(0.5))
                .with("control", ParamValue::Bool(false)),
            axes: vec![ParamAxis {
                name: "stop_pips".into(),
                space: ParamSpace::Range {
                    from: 6.0,
                    to: 24.0,
                    step: 3.0,
                },
            }],
            objective: Objective {
                metric: Metric::Sharpe,
                direction: Direction::Maximize,
            },
            min_trades: 50,
        },
        walk_forward: WalkForwardPlan {
            train_days: 520,
            test_days: 130,
        },
        out_of_sample: None,
        evidence: EvidenceSettings {
            min_sample: MinSample {
                trades: 200,
                ..EvidenceSettings::default().min_sample
            },
            ..EvidenceSettings::default()
        },
    }
}

// ---------------------------------------------------------------------------------------------
// 3. The verdict rule
// ---------------------------------------------------------------------------------------------

/// Judged as `HighRisk`, the kind judged per trade: the published claim is a mean per trade, and
/// this kind is the one whose criteria are the mean net P&L per trade and the bootstrap's lower
/// bound on it. The rule passes when, out of sample and after the spread:
///
/// - the mean net P&L per trade is above zero, and so is the bootstrap's lower bound on it, which
///   is the margin of error. Every trade risks the same share of the account, so the mean in
///   dollars has the sign of the mean in pips, and its lower bound above zero is the claim that
///   the pips per trade are above zero by more than the sampling error;
/// - the default profit factor of 1.3, risk of ruin at most 5% of a 50% drawdown, and a positive
///   return without the single best trade, so one lucky day cannot carry the result.
///
/// The lower bound above zero is what says the effect is real. The profit factor of 1.3 is a
/// second bar, on the effect's size. A profit factor is 1 plus the mean over the gross loss per
/// trade. With trade outcomes whose standard deviation is about 12 pips, the gross loss per trade
/// is about 0.4 of that, 4.8 pips, when there is no edge; holding it there, a profit factor of 1.3
/// asks for a mean of about 1.4 pips, roughly 60% of the published 2.35, which would itself give
/// about 1.5. That is a rough figure from a simple model, not a measurement, and the source
/// publishes no profit factor to check it against. A real but smaller effect therefore ends as
/// not passing, on purpose, because the published edge also has to pay a commission that this
/// plan does not charge. The published numbers (775 trades, +2.35 pips, t = 5.5, 58% winners) are
/// what a run is read against, not pass marks.
fn rule() -> VerdictRule {
    VerdictRule::HighRisk {
        limits: HighRiskLimits {
            min_trades: 200,
            ..HighRiskLimits::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strategy(control: bool) -> Gotobi {
        let p = Params::new()
            .with("control", ParamValue::Bool(control))
            .resolve(&entry().params)
            .unwrap();
        Gotobi {
            stop_pips: p.float("stop_pips").unwrap(),
            risk_pct: p.float("risk_pct").unwrap(),
            control,
            stop: None,
            exit_at: None,
            last_entry_day: None,
            day: None,
        }
    }

    /// 00:55 UTC on a date, 9:55 Tokyo.
    fn fixing(year: i64, month: u32, day: u32) -> Millis {
        date(year, month, day) + ENTRY_UTC
    }

    /// The days each mode trades, including the weekend days an FX session cannot be fed.
    #[test]
    fn the_control_trades_only_business_days_that_are_not_gotobi_days() {
        let (gotobi, control) = (strategy(false), strategy(true));
        let trades = |s: &Gotobi, t| s.trades_on(t).unwrap();
        // Tuesday 2026-11-10 is a gotobi day; Monday 2026-11-09 a business day that is not.
        assert!(trades(&gotobi, fixing(2026, 11, 10)));
        assert!(!trades(&control, fixing(2026, 11, 10)));
        assert!(!trades(&gotobi, fixing(2026, 11, 9)));
        assert!(trades(&control, fixing(2026, 11, 9)));
        // Saturday 2026-11-14 and Sunday 2026-11-15, and Sunday 2025-10-05, a nominal gotobi day
        // that moves back to the Friday.
        for t in [
            fixing(2026, 11, 14),
            fixing(2026, 11, 15),
            fixing(2025, 10, 5),
        ] {
            assert!(!trades(&gotobi, t));
            assert!(!trades(&control, t));
        }
        // Culture Day, Tuesday 2026-11-03, and the bank holiday on Monday 2018-12-31.
        for t in [fixing(2026, 11, 3), fixing(2018, 12, 31)] {
            assert!(!trades(&gotobi, t));
            assert!(!trades(&control, t));
        }
        // Outside the table both modes are an error, not a day without a trade.
        assert!(gotobi.trades_on(fixing(2028, 1, 5)).is_err());
        assert!(control.trades_on(fixing(2028, 1, 5)).is_err());
    }
}
