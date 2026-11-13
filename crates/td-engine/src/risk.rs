//! Risk measures for judging a strategy against limits: the equity a bar reached inside its
//! range, the leverage at the worst point, drawdowns from the starting balance and from the
//! running peak, and a daily P&L series whose day can roll over at any time, as prop firms reset
//! their daily loss limit at 17:00 in New York.
//!
//! Like [`crate::metrics`], every value that can be undefined is an `Option` and never NaN or an
//! infinity.

use serde::{Deserialize, Serialize};
use td_core::{AccountCurrency, Candle, Fill, Liquidity, Millis, civil_from_days, days_from_civil};

use crate::metrics::{DAY_MS, EquitySeries};
use crate::trades::{closing_side, fill_side};
use crate::{BarFunding, EquityPoint};

const HOUR_MS: Millis = 3_600_000;

/// `Some(x)` if `x` is finite.
fn finite(x: f64) -> Option<f64> {
    x.is_finite().then_some(x)
}

// ---------------------------------------------------------------------------------------------
// Equity inside each bar
// ---------------------------------------------------------------------------------------------

/// The lowest and highest equity one bar reached, in the account currency.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EquityRange {
    pub low: f64,
    pub high: f64,
}

/// The equity extremes of every bar of a run, and the leverage at its worst point.
#[derive(Debug, Clone, PartialEq)]
pub struct Intrabar {
    /// One per bar, in bar order, like the equity curve.
    pub ranges: Vec<EquityRange>,
    /// The highest notional over equity, `|position| * price / equity`, at any of the points
    /// [`intrabar`] evaluates, the notional converted to the account currency at that point's
    /// price. 0 when the run never held a position; `None` with no bars. Points
    /// where equity is at or below zero are skipped: the account is bankrupt there, which the
    /// drawdowns show.
    pub peak_leverage: Option<f64>,
}

/// The equity each bar reached inside its range, and the peak leverage, from a run's bars, their
/// asks, its equity at each bar's close, its fills and the funding charged on each bar.
///
/// Equity moves with the price by the position held: for spot and perpetuals alike it is a
/// constant plus `position * price`. On an [`AccountCurrency::Base`] account it is a constant plus
/// the open position's P&L converted at the price it is valued at, `position * (price -
/// avg_entry) / price` with the portfolio's average entry, so each point below is converted at
/// its own price; `starting_cash`, `equity` and the fills' fees are in the account currency
/// `account` already. On a market with quotes, `asks[k]` is the ask candle of bar
/// k, and every price below is on the candle the position would close on, as the session marks
/// it: the ask for a short and the bar, which holds the bid, for a long. Without quotes `asks` is
/// empty, or `None` for every bar, and every price is the bar's. The candle model does not know
/// the path inside a bar, so each bar is evaluated at the points of the path the fill model
/// assumes, as [`crate::build_trades`] does for a trade's excursions:
///
/// - the open, with the position held into the bar, from the previous close's equity moved by
///   the gap and less the funding charged at the open;
/// - each fill's price, with the position before that fill (each fill then takes its fee off),
///   moved by the spread at the open for a fill on the other side from the one the position
///   closes on, an add on a market with quotes; for a fill that reduces the position as a maker (a limit, which the model fills after the
///   stops inside the range, the order worse for the strategy) also the adverse extreme, the low
///   for a long and the high for a short;
/// - the low and high with the position after all the bar's fills, from the equity at the close,
///   because the session counts the whole range as coming after the fills (see
///   [`crate::Session::on_bar`]), and the close itself.
///
/// The low and high are the least and greatest equity among those points, with the low floored at
/// zero, since a perpetual account is liquidated or written off at zero and a spot account cannot
/// go below it. Taker fills happen at the open or at a stop's trigger, which the market reaches
/// before going further, so the position before one meets no worse price. Funding due later in a
/// bar is in the close's equity only, as the session charges it there.
///
/// `equity`, `bars` and `funding` must have one entry per bar, and so must `asks` unless it is
/// empty; `fills` must be in time order, stamped with their bar's open time, as a session records
/// them.
pub fn intrabar(
    starting_cash: f64,
    bars: &[Candle],
    asks: &[Option<Candle>],
    equity: &[EquityPoint],
    fills: &[Fill],
    funding: &[BarFunding],
    account: AccountCurrency,
) -> Intrabar {
    let mut ranges = Vec::with_capacity(bars.len());
    let mut peak: Option<f64> = (!bars.is_empty()).then_some(0.0);
    let (mut pos, mut next) = (0.0, 0);
    // The position's average entry, by the portfolio's rule; a base account values it with it.
    let mut avg = 0.0;
    // The equity `e` at price `from` moved to price `to`, with position `q` at average entry
    // `avg`.
    let shift = |e: f64, q: f64, avg: f64, from: f64, to: f64| match account {
        AccountCurrency::Quote => e + q * (to - from),
        AccountCurrency::Base => {
            // The position's P&L at price `p`, in the base currency.
            let open_pnl = |p: f64| {
                if q == 0.0 { 0.0 } else { q * (p - avg) / p }
            };
            e + open_pnl(to) - open_pnl(from)
        }
    };
    // The equity at the previous close and the price it marked the position at.
    let (mut prev_equity, mut prev_close) = (starting_cash, 0.0);
    for (k, (bar, point)) in bars.iter().zip(equity).enumerate() {
        let ask = asks.get(k).copied().flatten();
        let (mut low, mut high) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut see = |q: f64, price: f64, e: f64| {
            low = low.min(e);
            high = high.max(e);
            if q != 0.0
                && e > 0.0
                && let Some(lev) = finite(account.from_quote(q.abs() * price, price) / e)
            {
                peak = Some(peak.map_or(lev, |p| p.max(lev)));
            }
        };
        let at_open = funding.get(k).map_or(0.0, |f| f.at_open);
        // `e` is the equity at price `p` with position `pos`, as the path goes through the bar.
        let open = closing_side(bar, ask, pos).open;
        let mut e = shift(prev_equity, pos, avg, prev_close, open) - at_open;
        let mut p = open;
        see(pos, p, e);
        while next < fills.len() && fills[next].time <= bar.open_time {
            let f = &fills[next];
            next += 1;
            // A fill that reduces the position is on the side the position closes on. One that
            // adds to it is on the other side on a market with quotes, so the position before it
            // is valued one spread from the fill's price, the spread at the bar's open, as the
            // broker starts an attached stop. A fill inside its side's candle met the closing side
            // inside that side's candle, so the price is held there, as `build_trades` holds the
            // add's excursion; one filled between the bars met it on the way to the open.
            // shortcut: the spread at the fill itself is not in the candles, so a spread that
            // changed between the open and the fill still misstates the equity there by the change
            // on that position. Take the closing side's price at the fill when the fill model
            // records it.
            let c = closing_side(bar, ask, pos);
            let own = fill_side(bar, ask, f.side);
            let mut before = f.price + (c.open - own.open);
            if (own.low..=own.high).contains(&f.price) {
                before = before.clamp(c.low, c.high);
            }
            see(pos, before, shift(e, pos, avg, p, before));
            let at_fill = shift(e, pos, avg, p, f.price);
            let dq = f.side.sign() * f.qty;
            let reducing = pos != 0.0 && f64::signum(pos) != dq.signum();
            if reducing && f.liquidity == Liquidity::Maker {
                let c = closing_side(bar, ask, pos);
                let adverse = if pos > 0.0 { c.low } else { c.high };
                see(pos, adverse, shift(e, pos, avg, p, adverse));
            }
            (e, p) = (at_fill - f.fee, f.price);
            let mut new = pos + dq;
            // The portfolio's rule for float residue.
            if new.abs() <= 1e-9 * (f64::abs(pos) + f.qty) {
                new = 0.0;
            }
            avg = if new == 0.0 {
                0.0
            } else if pos == 0.0 || pos.signum() != new.signum() {
                f.price
            } else if new.abs() > pos.abs() {
                (pos.abs() * avg + f.qty * f.price) / new.abs()
            } else {
                avg
            };
            pos = new;
        }
        let close = point.equity;
        // The range is priced before the funding due later in the bar, which the session
        // charges or credits at the close: a credit must not cover a loss taken before it.
        let before = close + funding.get(k).map_or(0.0, |f| f.in_bar);
        let c = closing_side(bar, ask, pos);
        see(pos, c.low, shift(before, pos, avg, c.close, c.low));
        see(pos, c.high, shift(before, pos, avg, c.close, c.high));
        see(pos, c.close, close);
        ranges.push(EquityRange {
            low: low.max(0.0),
            high,
        });
        (prev_equity, prev_close) = (close, c.close);
    }
    Intrabar {
        ranges,
        peak_leverage: peak,
    }
}

// ---------------------------------------------------------------------------------------------
// Drawdowns
// ---------------------------------------------------------------------------------------------

/// The fall from the starting balance to the lowest equity, as a fraction of the starting
/// balance: `max(0, (start - min(lows)) / start)`. Unlike a drawdown from the running peak, gains
/// never raise the level it is measured from; prop firms call it a static drawdown. Pass the
/// closes, or each bar's low ([`EquityRange::low`]) to measure it inside bars. `None` with no
/// values, a start at or below zero, or a result that is not finite.
pub fn static_drawdown(start: f64, lows: impl IntoIterator<Item = f64>) -> Option<f64> {
    let min = lows.into_iter().reduce(f64::min)?;
    if start <= 0.0 {
        return None;
    }
    finite(((start - min) / start).max(0.0))
}

/// The largest fall from the running peak, measured inside bars.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrailingDrawdown {
    /// `1 - low / peak` at its deepest, as [`crate::metrics::max_drawdown`] measures it on
    /// closes.
    pub depth: f64,
    /// `peak - low` at its largest, in the account currency: the fall below the high-water mark that
    /// trailing prop-firm limits measure. Not necessarily at the same point as `depth`.
    pub amount: f64,
}

/// The largest fall from the running peak to a later or same-bar low, where the peak starts at
/// `start` and rises with each bar's high. A bar's own high counts before its low: the path
/// inside a bar is unknown, and like the fill model this takes the order worse for the strategy,
/// so a bar that rose and fell back counts as a drawdown. With a range per close (`low == high ==
/// close`) this is the drawdown on closes. `None` with no ranges, a start at or below zero, or a
/// result that is not finite; 0 for a curve that never falls.
pub fn trailing_drawdown(start: f64, ranges: &[EquityRange]) -> Option<TrailingDrawdown> {
    if ranges.is_empty() || start <= 0.0 {
        return None;
    }
    let (mut peak, mut depth, mut amount) = (start, 0.0f64, 0.0f64);
    for r in ranges {
        peak = peak.max(r.high);
        depth = depth.max(1.0 - r.low / peak);
        amount = amount.max(peak - r.low);
    }
    Some(TrailingDrawdown {
        depth: finite(depth)?,
        amount: finite(amount)?,
    })
}

// ---------------------------------------------------------------------------------------------
// Daily P&L
// ---------------------------------------------------------------------------------------------

/// When one trading day ends and the next begins, for the daily P&L and daily loss limits.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DayRollover {
    /// At the same time every day: `offset_minutes` after 00:00 UTC, taken modulo a day, so 0
    /// is midnight UTC (the day the metrics use) and both 1380 and -60 are 23:00 UTC.
    Utc { offset_minutes: i32 },
    /// 17:00 in New York, where most prop firms reset their daily limits: 21:00 UTC while US
    /// daylight saving time is in force, from the second Sunday of March to the first Sunday of
    /// November (the rules since 2007), and 22:00 UTC otherwise.
    NewYork1700,
}

impl Default for DayRollover {
    /// Midnight UTC.
    fn default() -> Self {
        DayRollover::Utc { offset_minutes: 0 }
    }
}

impl DayRollover {
    /// The rollover on the UTC calendar date `day` (days since the epoch). Always within that
    /// date, so rollovers rise strictly from one date to the next.
    fn on_date(self, day: i64) -> Millis {
        let into_day = match self {
            DayRollover::Utc { offset_minutes } => {
                (i64::from(offset_minutes) * 60_000).rem_euclid(DAY_MS)
            }
            // 17:00 New York falls on the same UTC date at 21:00 or 22:00 UTC, and daylight saving
            // changes at 02:00 local, so the regime at 17:00 is the one of the whole date.
            DayRollover::NewYork1700 if us_daylight_saving(day) => 21 * HOUR_MS,
            DayRollover::NewYork1700 => 22 * HOUR_MS,
        };
        day * DAY_MS + into_day
    }

    /// The first rollover strictly after `t`: the end of the trading day that holds `t` as a
    /// start, or that holds `t + 1` as a bar close.
    pub fn next_after(self, t: Millis) -> Millis {
        let day = t.div_euclid(DAY_MS);
        let today = self.on_date(day);
        if today > t {
            today
        } else {
            self.on_date(day + 1)
        }
    }
}

/// The first Sunday on or after a date. 1970-01-01, day 0, was a Thursday.
fn sunday_on_or_after(days: i64) -> i64 {
    let weekday = (days + 4).rem_euclid(7); // 0 is Sunday
    days + (7 - weekday) % 7
}

/// Whether US daylight saving time is in force at 17:00 New York time on this date.
///
/// shortcut: the US rule since 2007, as td-core's FX week uses; days before 2007 between the two
/// rules (mid-March to early April, late October to early November) roll over an hour off.
/// Upgrade to a per-year rule, shared with the FX week, when the desk tests data before 2007.
fn us_daylight_saving(days: i64) -> bool {
    let (year, _, _) = civil_from_days(days);
    let start = sunday_on_or_after(days_from_civil(year, 3, 1)) + 7;
    let end = sunday_on_or_after(days_from_civil(year, 11, 1));
    (start..end).contains(&days)
}

/// One trading day of an equity series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DailyPnl {
    /// The rollover that ends the day.
    pub end: Millis,
    /// The equity at the day's start: the end of the day before, or the starting equity.
    pub start_equity: f64,
    /// The last equity at or before the day's end.
    pub end_equity: f64,
    /// The lowest equity of the day: the lowest bar low ([`EquityRange::low`]) of the bars that
    /// close in it, or the lowest close without ranges. The start equity for a day with no bar
    /// close.
    pub low_equity: f64,
}

impl DailyPnl {
    /// `end_equity - start_equity`.
    pub fn pnl(&self) -> f64 {
        self.end_equity - self.start_equity
    }
}

/// The P&L of each trading day an equity series spans, with days ending at `rollover`.
///
/// The days run from the one holding the series' first time (the first bar's open, which starts a
/// day when it falls exactly on a rollover) to the one holding its last time. A value is in the
/// day its time falls in, and a value stamped exactly on a rollover ends the day before, so a bar
/// is in the day its close falls in, and so is its low, even when the bar began the day before. A
/// day with no value has a P&L of zero. With the default rollover, midnight UTC, the end
/// equities are [`crate::metrics::daily_equity`] after its first value.
///
/// `ranges`, when given, has one entry per bar: `ranges[k]` for the bar that closes at
/// `series.times[k + 1]`. The result is empty for a series of fewer than two values, mismatched
/// lengths or ranges of the wrong length.
pub fn daily_pnl(
    series: &EquitySeries,
    ranges: Option<&[EquityRange]>,
    rollover: DayRollover,
) -> Vec<DailyPnl> {
    let (times, values) = (&series.times, &series.values);
    let n = values.len();
    if n < 2 || times.len() != n || ranges.is_some_and(|r| r.len() + 1 != n) {
        return Vec::new();
    }
    let last = times[n - 1];
    let mut days = Vec::new();
    let (mut i, mut start) = (0, values[0]);
    let mut end = rollover.next_after(times[0]);
    loop {
        let mut low = f64::INFINITY;
        while i + 1 < n && times[i + 1] <= end {
            i += 1;
            low = low.min(values[i]);
            if let Some(r) = ranges {
                low = low.min(r[i - 1].low);
            }
        }
        days.push(DailyPnl {
            end,
            start_equity: start,
            end_equity: values[i],
            low_equity: if low.is_finite() { low } else { start },
        });
        start = values[i];
        if end >= last {
            return days;
        }
        end = rollover.next_after(end);
    }
}

/// The largest loss over one day, from the day's start to its end, as a fraction of `base`
/// (usually the starting balance, which prop firms size their limits from); 0 if no day lost.
/// `None` with no days, a base at or below zero, or a result that is not finite.
pub fn worst_daily_loss(days: &[DailyPnl], base: f64) -> Option<f64> {
    worst_loss(days, base, |d| d.end_equity)
}

/// The largest fall inside one day, from the day's start to its lowest equity
/// ([`DailyPnl::low_equity`], inside bars when the days were built with ranges), as a fraction of
/// `base`, as [`worst_daily_loss`] measures it. Prop firms breach a daily limit the moment equity
/// touches it, not only at the day's end.
pub fn worst_daily_loss_intraday(days: &[DailyPnl], base: f64) -> Option<f64> {
    worst_loss(days, base, |d| d.low_equity)
}

fn worst_loss(days: &[DailyPnl], base: f64, to: impl Fn(&DailyPnl) -> f64) -> Option<f64> {
    if days.is_empty() || base <= 0.0 {
        return None;
    }
    let worst = days
        .iter()
        .map(|d| d.start_equity - to(d))
        .fold(0.0, f64::max);
    finite(worst / base)
}

#[cfg(test)]
mod tests {
    use td_core::{InstrumentId, InstrumentKind, OrderId, Side, Venue};

    use super::*;

    #[track_caller]
    fn approx(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() <= 1e-9 * expected.abs().max(1.0),
            "expected {expected}, got {actual}"
        );
    }

    const MIN: Millis = 60_000;

    fn bar(i: i64, open: f64, high: f64, low: f64, close: f64) -> Candle {
        Candle {
            open_time: i * MIN,
            open,
            high,
            low,
            close,
            volume: 1.0,
            quote_volume: close,
        }
    }

    fn fill(i: i64, side: Side, qty: f64, price: f64, fee: f64, liquidity: Liquidity) -> Fill {
        Fill {
            order_id: OrderId(1),
            instrument: InstrumentId {
                venue: Venue::Binance,
                symbol: "X".into(),
                kind: InstrumentKind::Perpetual,
            },
            side,
            time: i * MIN,
            price,
            qty,
            fee,
            liquidity,
        }
    }

    fn points(equity: &[f64]) -> Vec<EquityPoint> {
        equity
            .iter()
            .enumerate()
            .map(|(i, &equity)| EquityPoint {
                time: i as i64 * MIN,
                equity,
            })
            .collect()
    }

    // A perpetual account of 1000, worked by hand:
    // bar 0: buys 2 at 100 (taker, fee 0.2) on a bar 100..104 / 97, closing at 102.
    //   Equity after the fill 999.8; at the close 999.8 + 2 * 2 = 1003.8. Range with 2 long:
    //   low 1003.8 + 2 * (97 - 102) = 993.8, high 1003.8 + 2 * (104 - 102) = 1007.8.
    // bar 1: opens at 99, high 106, low 95, close 105; a maker sell limit of 2 at 106 (fee 0.1)
    //   closes the long inside the range.
    //   At the open: 1003.8 + 2 * (99 - 102) - 0.5 funding = 997.3.
    //   At the fill: 997.3 + 2 * (106 - 99) = 1011.3; a maker exit also counts the low with the
    //   long: 997.3 + 2 * (95 - 99) = 989.3. After the fill 1011.2, flat; the close is 1010.7
    //   after 0.5 of funding later in the bar, and flat the range adds nothing.
    //   Low 989.3, high 1011.3.
    // bar 2: flat all bar at 1010.7.
    // Long 1 from 100 into a bar that dips to 90 and closes at 100, with 10 of funding credited
    // later in the bar: the close is 1010, but the dip came before the credit, so the low is
    // 1000 + 1 * (90 - 100) = 990, not 1010 - 10 = 1000.
    #[test]
    fn a_funding_credit_at_the_close_does_not_hide_an_earlier_dip() {
        let bars = [
            bar(0, 100.0, 100.0, 100.0, 100.0),
            bar(1, 100.0, 100.0, 90.0, 100.0),
        ];
        let fills = [fill(0, Side::Buy, 1.0, 100.0, 0.0, Liquidity::Taker)];
        let funding = [
            BarFunding::default(),
            BarFunding {
                at_open: 0.0,
                in_bar: -10.0,
            },
        ];
        let x = intrabar(
            1000.0,
            &bars,
            &[],
            &points(&[1000.0, 1010.0]),
            &fills,
            &funding,
            AccountCurrency::Quote,
        );
        approx(x.ranges[1].low, 990.0);
        approx(x.ranges[1].high, 1010.0);
    }

    #[test]
    fn extremes_follow_the_fill_models_path_through_each_bar() {
        let bars = [
            bar(0, 100.0, 104.0, 97.0, 102.0),
            bar(1, 99.0, 106.0, 95.0, 105.0),
            bar(2, 105.0, 107.0, 101.0, 103.0),
        ];
        let fills = [
            fill(0, Side::Buy, 2.0, 100.0, 0.2, Liquidity::Taker),
            fill(1, Side::Sell, 2.0, 106.0, 0.1, Liquidity::Maker),
        ];
        let funding = [
            BarFunding::default(),
            BarFunding {
                at_open: 0.5,
                in_bar: 0.5,
            },
            BarFunding::default(),
        ];
        let equity = points(&[1003.8, 1010.7, 1010.7]);
        let x = intrabar(
            1000.0,
            &bars,
            &[],
            &equity,
            &fills,
            &funding,
            AccountCurrency::Quote,
        );
        let r = &x.ranges;
        assert_eq!(r.len(), 3);
        approx(r[0].low, 993.8);
        approx(r[0].high, 1007.8);
        approx(r[1].low, 989.3);
        approx(r[1].high, 1011.3);
        assert_eq!((r[2].low, r[2].high), (1010.7, 1010.7));
        // Leverage: notional over equity, highest where the long is worth most against the
        // least equity: 2 * 104 / 1007.8 on bar 0 (0.20639) against 2 * 99 / 997.3 at bar 1's
        // open (0.19854) and 2 * 106 / 1011.3 at its fill (0.20963).
        approx(x.peak_leverage.unwrap(), 2.0 * 106.0 / 1011.3);
    }

    #[test]
    fn a_taker_exit_caps_the_adverse_side_at_its_fill_and_a_short_is_mirrored() {
        // Short 1 entered before; bar 0 holds it from 1000 at a close of 100. Bar 1 opens at 101,
        // a buy stop fills at 103 (taker, no fee) and the bar goes on up to 110: the short never
        // saw 110. At the open 1000 - 1 = 999, at the stop 1000 - 3 = 997, then flat at 997.
        let bars = [
            bar(0, 100.0, 100.0, 100.0, 100.0),
            bar(1, 101.0, 110.0, 99.0, 104.0),
        ];
        let fills = [
            fill(0, Side::Sell, 1.0, 100.0, 0.0, Liquidity::Taker),
            fill(1, Side::Buy, 1.0, 103.0, 0.0, Liquidity::Taker),
        ];
        let equity = points(&[1000.0, 997.0]);
        let x = intrabar(
            1000.0,
            &bars,
            &[],
            &equity,
            &fills,
            &[],
            AccountCurrency::Quote,
        );
        assert_eq!(
            x.ranges[1],
            EquityRange {
                low: 997.0,
                high: 999.0
            }
        );
        // The same exit as a maker fill counts the bar's high with the short: 999 - 9 = 990.
        let maker = [
            fills[0].clone(),
            Fill {
                liquidity: Liquidity::Maker,
                ..fills[1].clone()
            },
        ];
        let x = intrabar(
            1000.0,
            &bars,
            &[],
            &equity,
            &maker,
            &[],
            AccountCurrency::Quote,
        );
        assert_eq!(
            x.ranges[1],
            EquityRange {
                low: 990.0,
                high: 999.0
            }
        );
    }

    #[test]
    fn a_flat_run_has_flat_ranges_and_zero_leverage() {
        let bars = [bar(0, 10.0, 12.0, 8.0, 11.0)];
        let x = intrabar(
            500.0,
            &bars,
            &[],
            &points(&[500.0]),
            &[],
            &[],
            AccountCurrency::Quote,
        );
        assert_eq!(
            x.ranges,
            vec![EquityRange {
                low: 500.0,
                high: 500.0
            }]
        );
        assert_eq!(x.peak_leverage, Some(0.0));
        assert_eq!(
            intrabar(500.0, &[], &[], &[], &[], &[], AccountCurrency::Quote).peak_leverage,
            None
        );
    }

    #[test]
    fn the_low_is_floored_at_zero() {
        // Long 20 from 100 with 1000 closes at 60 with 200; the bar's low of 0 would take it to
        // 200 - 20 * 60 = -1000, but an account cannot go below zero.
        let bars = [bar(0, 100.0, 100.0, 0.0, 60.0)];
        let fills = [fill(0, Side::Buy, 20.0, 100.0, 0.0, Liquidity::Taker)];
        let x = intrabar(
            1000.0,
            &bars,
            &[],
            &points(&[200.0]),
            &fills,
            &[],
            AccountCurrency::Quote,
        );
        assert_eq!(
            x.ranges[0],
            EquityRange {
                low: 0.0,
                high: 1000.0
            }
        );
    }

    fn range(low: f64, high: f64) -> EquityRange {
        EquityRange { low, high }
    }

    #[test]
    fn static_drawdown_is_measured_from_the_start_only() {
        // Up to 150 first does not raise the level: the low of 90 is 10% under the start.
        approx(static_drawdown(100.0, [150.0, 90.0, 120.0]).unwrap(), 0.1);
        // Never under the start: 0.
        assert_eq!(static_drawdown(100.0, [100.0, 130.0]), Some(0.0));
        assert_eq!(static_drawdown(100.0, []), None);
        assert_eq!(static_drawdown(0.0, [1.0]), None);
    }

    #[test]
    fn trailing_drawdown_counts_a_bars_own_high_before_its_low() {
        // From 100: bar 0 reaches 120 and falls to 108 inside the bar (10% of 120, 12 below it);
        // bar 1 falls to 99 (17.5% of 120, 21 below); bar 2 rises to 200 and closes at 190.
        let r = [range(108.0, 120.0), range(99.0, 110.0), range(190.0, 200.0)];
        let d = trailing_drawdown(100.0, &r).unwrap();
        approx(d.depth, 1.0 - 99.0 / 120.0);
        approx(d.amount, 21.0);
        // The amount and the depth can peak at different points: 10 below 50 is 20%, 30 below
        // 300 is 10%.
        let r = [range(40.0, 50.0), range(270.0, 300.0)];
        let d = trailing_drawdown(50.0, &r).unwrap();
        approx(d.depth, 0.2);
        approx(d.amount, 30.0);
        // On closes, low == high: the drawdown on closes.
        let closes = [range(110.0, 110.0), range(99.0, 99.0)];
        approx(trailing_drawdown(100.0, &closes).unwrap().depth, 0.1);
        assert_eq!(trailing_drawdown(100.0, &[]), None);
        assert_eq!(trailing_drawdown(-1.0, &closes), None);
    }

    const DAY: Millis = DAY_MS;
    const HOUR: Millis = HOUR_MS;

    #[test]
    fn utc_rollovers_take_the_offset_modulo_a_day() {
        let at = |m: i32| DayRollover::Utc { offset_minutes: m };
        assert_eq!(DayRollover::default().next_after(5 * DAY + HOUR), 6 * DAY);
        // A time exactly on a rollover ends the day before: the next one is a day later.
        assert_eq!(DayRollover::default().next_after(5 * DAY), 6 * DAY);
        assert_eq!(at(17 * 60).next_after(5 * DAY + HOUR), 5 * DAY + 17 * HOUR);
        assert_eq!(
            at(17 * 60).next_after(5 * DAY + 18 * HOUR),
            6 * DAY + 17 * HOUR
        );
        assert_eq!(at(-60).next_after(5 * DAY), 5 * DAY + 23 * HOUR);
        assert_eq!(at(1380).next_after(5 * DAY), 5 * DAY + 23 * HOUR);
        assert_eq!(at(1440).next_after(5 * DAY + HOUR), 6 * DAY);
    }

    #[test]
    fn new_york_rollover_follows_us_daylight_saving() {
        // 2024: daylight saving from Sunday 10 March to Sunday 3 November. 2024-03-01 00:00 UTC
        // is 1_709_251_200_000 ms (1 January 2024 plus 31 + 29 days).
        let mar1 = 1_709_251_200_000;
        let date = |d: i64| mar1 + d * DAY;
        let ny = DayRollover::NewYork1700;
        // Saturday 9 March: 22:00 UTC. Sunday 10 March: 21:00 UTC.
        assert_eq!(ny.next_after(date(8)), date(8) + 22 * HOUR);
        assert_eq!(ny.next_after(date(9)), date(9) + 21 * HOUR);
        // Between the two rollovers of 9 and 10 March the day is 23 hours long.
        assert_eq!(ny.next_after(date(8) + 22 * HOUR), date(9) + 21 * HOUR);
        // Saturday 2 November (246 days after 1 March): 21:00. Sunday 3 November: 22:00.
        assert_eq!(ny.next_after(date(246)), date(246) + 21 * HOUR);
        assert_eq!(ny.next_after(date(247)), date(247) + 22 * HOUR);
        // 2025 starts on 9 March and 2007, the first year of these rules, on 11 March.
        assert!(!us_daylight_saving(days_from_civil(2025, 3, 8)));
        assert!(us_daylight_saving(days_from_civil(2025, 3, 9)));
        assert!(!us_daylight_saving(days_from_civil(2007, 3, 10)));
        assert!(us_daylight_saving(days_from_civil(2007, 3, 11)));
        assert!(us_daylight_saving(days_from_civil(2007, 11, 3)));
        assert!(!us_daylight_saving(days_from_civil(2007, 11, 4)));
        assert_eq!(days_from_civil(2024, 3, 1) * DAY, mar1);
    }

    /// From 12:00 on day 0: closes at 18:00, 00:00 on day 1, 12:00 and 20:00 on day 1, none on
    /// day 2, 06:00 and 23:00 on day 3.
    fn series() -> EquitySeries {
        EquitySeries {
            times: vec![
                12 * HOUR,
                18 * HOUR,
                DAY,
                DAY + 12 * HOUR,
                DAY + 20 * HOUR,
                3 * DAY + 6 * HOUR,
                3 * DAY + 23 * HOUR,
            ],
            values: vec![100.0, 104.0, 110.0, 105.0, 99.0, 120.0, 108.9],
        }
    }

    #[test]
    fn daily_pnl_on_utc_days_matches_the_daily_equity() {
        let s = series();
        let days = daily_pnl(&s, None, DayRollover::default());
        let ends: Vec<f64> = days.iter().map(|d| d.end_equity).collect();
        let daily = crate::metrics::daily_equity(&s.times, &s.values);
        assert_eq!(ends, daily[1..]);
        let starts: Vec<f64> = days.iter().map(|d| d.start_equity).collect();
        assert_eq!(starts, daily[..daily.len() - 1]);
        assert_eq!(
            days.iter().map(|d| d.end).collect::<Vec<_>>(),
            vec![DAY, 2 * DAY, 3 * DAY, 4 * DAY]
        );
        // Day 0: 100 -> 110 (the 00:00 close ends it), low 104. Day 1: 110 -> 99, low 99.
        // Day 2: no close, carried at 99. Day 3: 99 -> 108.9, low 108.9.
        let lows: Vec<f64> = days.iter().map(|d| d.low_equity).collect();
        assert_eq!(lows, vec![104.0, 99.0, 99.0, 108.9]);
        approx(days[1].pnl(), -11.0);
        assert_eq!(days[2].pnl(), 0.0);
        // Worst day: -11 on 100 of starting balance.
        approx(worst_daily_loss(&days, 100.0).unwrap(), 0.11);
        approx(worst_daily_loss_intraday(&days, 100.0).unwrap(), 0.11);
    }

    #[test]
    fn a_later_rollover_moves_closes_into_other_days() {
        // Days end at 19:00 UTC: (.., 19:00 day 0] holds 18:00; (19:00 day 0, 19:00 day 1] holds
        // 00:00 and 12:00; (19:00 day 1, 19:00 day 2] holds 20:00; the next holds 06:00 on day 3;
        // and the last, ending 19:00 on day 4, holds 23:00 on day 3.
        let s = series();
        let days = daily_pnl(
            &s,
            None,
            DayRollover::Utc {
                offset_minutes: 19 * 60,
            },
        );
        let ends: Vec<f64> = days.iter().map(|d| d.end_equity).collect();
        assert_eq!(ends, vec![104.0, 105.0, 99.0, 120.0, 108.9]);
        let lows: Vec<f64> = days.iter().map(|d| d.low_equity).collect();
        assert_eq!(lows, vec![104.0, 105.0, 99.0, 120.0, 108.9]);
        // The worst day is now 120 -> 108.9, a fall that sat inside a rising day on UTC days,
        // where the worst was 110 -> 99.
        approx(worst_daily_loss(&days, 100.0).unwrap(), 0.111);
    }

    #[test]
    fn ranges_lower_the_days_low_but_not_its_end() {
        let s = series();
        // Bar 4 (closing at 20:00 on day 1 at 99) dipped to 90; bar 1 (closing at 18:00 on day 0
        // at 104) to 95.
        let mut r: Vec<EquityRange> = s.values[1..].iter().map(|&v| range(v, v)).collect();
        r[0].low = 95.0;
        r[3].low = 90.0;
        let days = daily_pnl(&s, Some(&r), DayRollover::default());
        let lows: Vec<f64> = days.iter().map(|d| d.low_equity).collect();
        assert_eq!(lows, vec![95.0, 90.0, 99.0, 108.9]);
        // Day 1 started at 110 and touched 90: 20% of 100 inside the day, 11% by its end.
        approx(worst_daily_loss_intraday(&days, 100.0).unwrap(), 0.2);
        approx(worst_daily_loss(&days, 100.0).unwrap(), 0.11);
        // Wrong lengths give no days.
        assert!(daily_pnl(&s, Some(&r[1..]), DayRollover::default()).is_empty());
    }

    #[test]
    fn daily_loss_edge_cases() {
        let up = EquitySeries {
            times: vec![0, HOUR, 2 * HOUR],
            values: vec![100.0, 101.0, 102.0],
        };
        let days = daily_pnl(&up, None, DayRollover::default());
        assert_eq!(days.len(), 1);
        assert_eq!(worst_daily_loss(&days, 100.0), Some(0.0));
        assert_eq!(worst_daily_loss(&[], 100.0), None);
        assert_eq!(worst_daily_loss(&days, 0.0), None);
        let one = EquitySeries {
            times: vec![0],
            values: vec![100.0],
        };
        assert!(daily_pnl(&one, None, DayRollover::default()).is_empty());
    }
}
