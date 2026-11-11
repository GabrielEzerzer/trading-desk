//! Round-trip trades built from fills.

use std::ops::Range;

use serde::{Deserialize, Serialize};
use td_core::{Candle, Fill, Liquidity, Millis, Side};

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TradeSide {
    Long,
    Short,
}

impl TradeSide {
    fn sign(self) -> f64 {
        match self {
            TradeSide::Long => 1.0,
            TradeSide::Short => -1.0,
        }
    }
}

/// One round trip: the position leaves zero, then returns to it.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub side: TradeSide,
    /// Open time of the bar of the first fill.
    pub entry_time: Millis,
    /// Open time of the bar of the fill that closed the trade, or of the last bar for an open
    /// trade.
    pub exit_time: Millis,
    /// Still open at the end of the run; its exit is the remaining position marked at the last
    /// close, with no exit fee.
    pub open: bool,
    /// The largest absolute position during the trade.
    pub qty: f64,
    /// Volume-weighted price of the fills that opened or added to the position.
    pub entry_price: f64,
    /// Volume-weighted price of the fills that reduced or closed it (and the mark, if open).
    pub exit_price: f64,
    /// `sign * exit_qty * (exit_price - entry_price)`, before fees and funding. Over a closed
    /// round trip this is the exit notional minus the entry notional (reversed for a short),
    /// which is what the portfolio's average-cost realised P&L adds up to.
    pub gross_pnl: f64,
    /// Fees of the trade's fills. A fill that flips the position splits its fee between the two
    /// trades in proportion to the quantity that closed and that opened.
    pub fees: f64,
    /// Funding charged on the trade's position: at a bar's open (including funding due in a gap
    /// before the bar) while it was held going into the bar, and later in a bar while it was held
    /// after the bar's fills.
    pub funding: f64,
    /// `gross_pnl - fees - funding`.
    pub net_pnl: f64,
    /// `net_pnl / (entry_price * qty)`: net P&L over the largest entry notional.
    pub return_on_notional: f64,
    /// Bars from the entry fill's bar to the exit fill's bar; for an open trade, to the end of the
    /// last bar. It counts bars fed, not time: missing bars in a gap are not counted, so across a
    /// gap it understates how long the position was held. For a closed trade, the time between
    /// the entry and exit fills' bars is `exit_time - entry_time`.
    pub bars_held: usize,
    /// The protective stop in force when the trade opened, if any: see [`attach_stops`]. `None`
    /// without one, and in a result saved before trades kept their stop.
    #[serde(default)]
    pub stop: Option<f64>,
    /// `net_pnl` over the initial risk, `qty * |entry_price - stop|`: the trade's result in
    /// multiples of what it risked at its stop; see [`attach_stops`]. `None` without a stop, with
    /// a stop at or beyond the entry price (no risk), or when not finite.
    #[serde(default)]
    pub r_multiple: Option<f64>,
    /// Maximum adverse excursion: the largest unrealised loss of the open position at any price
    /// it met, in the quote asset, before fees and funding; 0 if it never went against the
    /// entry. See [`build_trades`] for the prices counted. `None` in a result saved before
    /// trades had it.
    #[serde(default)]
    pub mae: Option<f64>,
    /// Maximum favourable excursion: the largest unrealised gain, measured like `mae`.
    #[serde(default)]
    pub mfe: Option<f64>,
    /// Closed by the end-of-run close-out (see [`crate::BacktestConfig::close_at_end`]) rather
    /// than by the strategy: in a walk-forward, at the end of its test window.
    #[serde(default)]
    pub force_closed: bool,
}

/// The funding a session charged on one bar, split by the position it was charged on.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BarFunding {
    /// Due at the bar's open or in the gap before it, charged on the position held going into the
    /// bar.
    pub at_open: f64,
    /// Due later in the bar, charged on the position after the bar's fills.
    pub in_bar: f64,
}

/// The protective stops open on one bar, from which a trade that opened on it takes its stop; see
/// [`attach_stops`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BarStops {
    /// The highest trigger among the reduce-only sell stops: the one nearest a long position's
    /// price, which would trigger first.
    pub sell: Option<f64>,
    /// The lowest trigger among the reduce-only buy stops, for a short.
    pub buy: Option<f64>,
}

impl BarStops {
    /// Counts a reduce-only stop on `side` with `trigger`.
    pub(crate) fn add(&mut self, side: Side, trigger: f64) {
        match side {
            Side::Sell => self.sell = Some(self.sell.map_or(trigger, |t| t.max(trigger))),
            Side::Buy => self.buy = Some(self.buy.map_or(trigger, |t| t.min(trigger))),
        }
    }
}

struct Building {
    side: TradeSide,
    /// Index of the trade's first fill.
    entry_fill: usize,
    entry_time: Millis,
    entry_bar: usize,
    max_abs: f64,
    entry_qty: f64,
    entry_notional: f64,
    exit_qty: f64,
    exit_notional: f64,
    fees: f64,
    funding: f64,
    mae: f64,
    mfe: f64,
}

impl Building {
    fn new(side: TradeSide, fill: &Fill, index: usize, bar: usize, qty: f64, fee: f64) -> Building {
        Building {
            side,
            entry_fill: index,
            entry_time: fill.time,
            entry_bar: bar,
            max_abs: qty,
            entry_qty: qty,
            entry_notional: qty * fill.price,
            exit_qty: 0.0,
            exit_notional: 0.0,
            fees: fee,
            funding: 0.0,
            mae: 0.0,
            mfe: 0.0,
        }
    }

    /// Counts the unrealised P&L of `qty` (the absolute position) at `price` against the entry
    /// price so far.
    fn excursion(&mut self, price: f64, qty: f64) {
        let pnl = self.side.sign() * qty * (price - self.entry_notional / self.entry_qty);
        self.mae = self.mae.max(-pnl);
        self.mfe = self.mfe.max(pnl);
    }

    fn finish(self, exit_time: Millis, bars_held: usize, open: bool) -> Trade {
        let entry_price = self.entry_notional / self.entry_qty;
        let exit_price = if self.exit_qty > 0.0 {
            self.exit_notional / self.exit_qty
        } else {
            0.0
        };
        let gross_pnl = self.side.sign() * self.exit_qty * (exit_price - entry_price);
        let net_pnl = gross_pnl - self.fees - self.funding;
        Trade {
            side: self.side,
            entry_time: self.entry_time,
            exit_time,
            open,
            qty: self.max_abs,
            entry_price,
            exit_price,
            gross_pnl,
            fees: self.fees,
            funding: self.funding,
            net_pnl,
            return_on_notional: net_pnl / (entry_price * self.max_abs),
            bars_held,
            stop: None,
            r_multiple: None,
            mae: Some(self.mae),
            mfe: Some(self.mfe),
            force_closed: false,
        }
    }
}

/// Builds the trade list from a run's fills, its bars and the funding charged on each bar.
///
/// A trade opens when the position leaves zero and closes when it returns to zero. A fill that
/// takes the position through zero closes the trade and opens the next one with the remainder.
/// Fills of protective stops attached to an entry are ordinary fills here: a stop that fills on
/// its entry's bar closes the trade on that bar. `bar_funding[k]` is the funding charged on bar
/// k: the part due at the open belongs to the trade held going into the bar, the rest to the trade
/// open after the bar's fills, as the session charges them. A position still open after the last
/// bar is reported as an open trade marked at the last close.
///
/// Positions follow the portfolio's rule for float residue: a position within `1e-9` of the
/// traded size of zero is flat.
///
/// The excursions (`mae`, `mfe`) are the unrealised P&L of the position, against the entry price
/// so far, at the prices it met on the fill model's path through each bar: the open, for a
/// position held into the bar; the price of each fill that reduces it, for the position before
/// that fill; and the whole bar range, for the position after the bar's fills, since the session
/// counts the whole range as coming after them (see [`crate::Session::on_bar`]). A fill that adds
/// to the position is not counted at its own price: within the bar's range the range after it
/// counts a loss at least as large, and a price slipped beyond the range is not one the market
/// traded at. A position reduced by a maker fill
/// (a limit, which fills after the stops inside the range, in the order worse for the strategy)
/// is counted at the bar's adverse extreme too. A taker fill happened at the open or at a stop's
/// trigger, which the market reached before going further, so the position before it met no worse
/// price. `stop` and `r_multiple` are left empty; [`attach_stops`] fills them.
pub fn build_trades(fills: &[Fill], bars: &[Candle], bar_funding: &[BarFunding]) -> Vec<Trade> {
    build_trades_with_fills(fills, bars, bar_funding)
        .into_iter()
        .map(|(trade, _)| trade)
        .collect()
}

/// [`build_trades`], with the range of `fills` each trade is made of. The ranges follow one
/// another; a fill that takes the position through zero ends one range and starts the next.
pub fn build_trades_with_fills(
    fills: &[Fill],
    bars: &[Candle],
    bar_funding: &[BarFunding],
) -> Vec<(Trade, Range<usize>)> {
    let mut trades = Vec::new();
    let mut current: Option<Building> = None;
    let mut pos = 0.0;
    let mut next = 0;
    for (k, bar) in bars.iter().enumerate() {
        let funding = bar_funding.get(k).copied().unwrap_or_default();
        if let Some(t) = current.as_mut() {
            t.funding += funding.at_open;
            // Held into the bar: the market moved from the previous close to the open.
            t.excursion(bar.open, f64::abs(pos));
        }
        // Fills are stamped with the open time of the bar they happened in.
        while next < fills.len() && fills[next].time <= bar.open_time {
            let fill = &fills[next];
            next += 1;
            let qty = fill.qty;
            let dq = fill.side.sign() * qty;
            let mut new = pos + dq;
            if new.abs() <= 1e-9 * (f64::abs(pos) + qty) {
                new = 0.0;
            }
            let reducing = pos != 0.0 && f64::signum(pos) != dq.signum();
            if reducing {
                let t = current.as_mut().expect("a non-zero position has a trade");
                t.excursion(fill.price, f64::abs(pos));
                if fill.liquidity == Liquidity::Maker {
                    let adverse = if pos > 0.0 { bar.low } else { bar.high };
                    t.excursion(adverse, f64::abs(pos));
                }
                let closed = qty.min(f64::abs(pos));
                t.exit_qty += closed;
                t.exit_notional += closed * fill.price;
                let flipped = new != 0.0 && new.signum() != f64::signum(pos);
                let closing_fee = if flipped {
                    fill.fee * closed / qty
                } else {
                    fill.fee
                };
                t.fees += closing_fee;
                if new == 0.0 || flipped {
                    let done = current.take().expect("checked above");
                    let held = k - done.entry_bar;
                    let range = done.entry_fill..next;
                    trades.push((done.finish(fill.time, held, false), range));
                }
                if flipped {
                    let side = if new > 0.0 {
                        TradeSide::Long
                    } else {
                        TradeSide::Short
                    };
                    let rest = qty - closed;
                    current = Some(Building::new(
                        side,
                        fill,
                        next - 1,
                        k,
                        rest,
                        fill.fee - closing_fee,
                    ));
                }
            } else if let Some(t) = current.as_mut() {
                t.entry_qty += qty;
                t.entry_notional += qty * fill.price;
                t.fees += fill.fee;
                t.max_abs = t.max_abs.max(new.abs());
            } else {
                let side = if dq > 0.0 {
                    TradeSide::Long
                } else {
                    TradeSide::Short
                };
                current = Some(Building::new(side, fill, next - 1, k, qty, fill.fee));
            }
            pos = new;
        }
        if let Some(t) = current.as_mut() {
            t.funding += funding.in_bar;
            t.excursion(bar.low, f64::abs(pos));
            t.excursion(bar.high, f64::abs(pos));
        }
    }
    if let (Some(mut t), Some(last)) = (current, bars.last()) {
        let remaining = f64::abs(pos);
        t.exit_qty += remaining;
        t.exit_notional += remaining * last.close;
        let held = bars.len() - t.entry_bar;
        let range = t.entry_fill..next;
        trades.push((t.finish(last.open_time, held, true), range));
    }
    trades
}

/// Gives each trade the protective stop in force when it opened, and its R multiple.
///
/// The rule: a trade takes its stop from the bar of its first fill (`stops[k]` for the bar
/// `bars[k]` that opens at its `entry_time`). It is the nearest reduce-only stop on the trade's
/// closing side, a sell stop for a long and a buy stop for a short, among those placed on that
/// bar, filled on it, or open once the strategy has handled it (see [`crate::Session::bar_stops`]).
/// So a stop attached to the entry ([`crate::Context::submit_with_stop`]) counts, since it is
/// placed the moment the entry fills, even when it fills on the same bar; so do a reduce-only stop
/// resting from before and one the strategy submits on the entry bar's close in reaction to the
/// fill. The nearest is the one that would trigger first: the highest sell stop, the lowest buy
/// stop. A stop moved later does not change the trade's stop. Stop-limits count by their trigger.
///
/// The initial risk is `qty * (entry_price - stop)` for a long and `qty * (stop - entry_price)`
/// for a short, with the trade's largest position and volume-weighted entry price, before fees;
/// the R multiple is `net_pnl` over it, so fees and funding count against the result. A stop at or
/// beyond the entry price risks nothing: the trade then has a stop but no R multiple.
pub fn attach_stops(trades: &mut [Trade], bars: &[Candle], stops: &[BarStops]) {
    for t in trades {
        let Ok(k) = bars.binary_search_by_key(&t.entry_time, |b| b.open_time) else {
            continue;
        };
        let at = stops.get(k).copied().unwrap_or_default();
        t.stop = match t.side {
            TradeSide::Long => at.sell,
            TradeSide::Short => at.buy,
        };
        t.r_multiple = t.stop.and_then(|stop| {
            let risk = t.side.sign() * t.qty * (t.entry_price - stop);
            let r = t.net_pnl / risk;
            (risk > 0.0 && r.is_finite()).then_some(r)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use td_core::{InstrumentId, InstrumentKind, Liquidity, OrderId, Side, Venue};

    const MIN: Millis = 60_000;

    #[track_caller]
    fn approx(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    fn fill(bar: i64, side: Side, qty: f64, price: f64, fee: f64) -> Fill {
        Fill {
            order_id: OrderId(1),
            instrument: InstrumentId {
                venue: Venue::Binance,
                symbol: "X".into(),
                kind: InstrumentKind::Perpetual,
            },
            side,
            time: bar * MIN,
            price,
            qty,
            fee,
            liquidity: Liquidity::Taker,
        }
    }

    fn in_bar(funding: &[f64]) -> Vec<BarFunding> {
        funding
            .iter()
            .map(|&in_bar| BarFunding {
                at_open: 0.0,
                in_bar,
            })
            .collect()
    }

    fn bars(closes: &[f64]) -> Vec<Candle> {
        closes
            .iter()
            .enumerate()
            .map(|(i, &c)| Candle {
                open_time: i as i64 * MIN,
                open: c,
                high: c,
                low: c,
                close: c,
                volume: 1.0,
                quote_volume: c,
            })
            .collect()
    }

    #[test]
    fn scale_in_flip_close_and_open_trade_at_the_end() {
        let fills = [
            // Long 1 @ 100, add 1 @ 110: entry VWAP 105, max position 2.
            fill(1, Side::Buy, 1.0, 100.0, 0.10),
            fill(2, Side::Buy, 1.0, 110.0, 0.11),
            // Sell 3 @ 120: closes the long 2 and opens a short 1. Fee 0.36 splits 0.24 / 0.12.
            fill(4, Side::Sell, 3.0, 120.0, 0.36),
            // Buy 1 @ 115 closes the short.
            fill(6, Side::Buy, 1.0, 115.0, 0.115),
            // Sell 2 @ 118 opens a short still open after the last bar (close 116).
            fill(7, Side::Sell, 2.0, 118.0, 0.236),
        ];
        let b = bars(
            &[100.0; 9]
                .iter()
                .copied()
                .chain([116.0])
                .collect::<Vec<_>>(),
        );
        // Funding later in each bar, charged on the position after that bar's fills.
        let mut funding = in_bar(&[0.0, 0.5, 0.25, 0.0, 0.1, 0.2, 0.0, 0.0, 0.0, 0.05]);
        // Funding at the open, charged on the position held going into the bar: the long on bar
        // 4, where it is closed; the short on bar 6, where it is closed; nothing on bar 7, which
        // starts flat.
        funding[4].at_open = 0.4;
        funding[6].at_open = 0.07;
        funding[7].at_open = 1.0;
        let t = build_trades(&fills, &b, &funding);
        assert_eq!(t.len(), 3);

        // Long: gross 2 * (120 - 105) = 30, fees 0.45, funding bars 1-3 = 0.75 and the open of
        // bar 4 = 0.4.
        assert_eq!(t[0].side, TradeSide::Long);
        assert_eq!((t[0].entry_time, t[0].exit_time), (MIN, 4 * MIN));
        assert!(!t[0].open);
        approx(t[0].qty, 2.0);
        approx(t[0].entry_price, 105.0);
        approx(t[0].exit_price, 120.0);
        approx(t[0].gross_pnl, 30.0);
        approx(t[0].fees, 0.45);
        approx(t[0].funding, 1.15);
        approx(t[0].net_pnl, 28.4);
        approx(t[0].return_on_notional, 28.4 / 210.0);
        assert_eq!(t[0].bars_held, 3);

        // Short from the flip: gross 1 * (120 - 115) = 5, fees 0.12 + 0.115, funding bars 4-5
        // and the open of bar 6.
        assert_eq!(t[1].side, TradeSide::Short);
        assert_eq!((t[1].entry_time, t[1].exit_time), (4 * MIN, 6 * MIN));
        approx(t[1].qty, 1.0);
        approx(t[1].gross_pnl, 5.0);
        approx(t[1].fees, 0.235);
        approx(t[1].funding, 0.37);
        approx(t[1].net_pnl, 4.395);
        approx(t[1].return_on_notional, 4.395 / 120.0);
        assert_eq!(t[1].bars_held, 2);

        // Open short marked at 116: gross 2 * (118 - 116) = 4, fee 0.236, funding 0.05. Held
        // from bar 7 through the end of bar 9.
        assert_eq!(t[2].side, TradeSide::Short);
        assert!(t[2].open);
        assert_eq!((t[2].entry_time, t[2].exit_time), (7 * MIN, 9 * MIN));
        approx(t[2].exit_price, 116.0);
        approx(t[2].gross_pnl, 4.0);
        approx(t[2].net_pnl, 4.0 - 0.236 - 0.05);
        assert_eq!(t[2].bars_held, 3);

        // The flip at bar 4 ends the long's fills and starts the short's.
        let ranges: Vec<Range<usize>> = build_trades_with_fills(&fills, &b, &funding)
            .into_iter()
            .map(|(_, range)| range)
            .collect();
        assert_eq!(ranges, [0..3, 2..4, 4..5]);
    }

    #[test]
    fn partial_exits_are_volume_weighted() {
        // Buy 2 @ 100, sell 1 @ 110, sell 1 @ 130: exit VWAP 120, gross 40.
        let fills = [
            fill(0, Side::Buy, 2.0, 100.0, 0.0),
            fill(1, Side::Sell, 1.0, 110.0, 0.0),
            fill(2, Side::Sell, 1.0, 130.0, 0.0),
        ];
        let t = build_trades(&fills, &bars(&[100.0; 3]), &in_bar(&[0.0; 3]));
        assert_eq!(t.len(), 1);
        approx(t[0].exit_price, 120.0);
        approx(t[0].gross_pnl, 40.0);
        assert_eq!(t[0].bars_held, 2);
    }

    #[test]
    fn float_residue_closes_the_trade() {
        let fills = [
            fill(0, Side::Buy, 0.1, 10.0, 0.0),
            fill(0, Side::Buy, 0.2, 10.0, 0.0),
            fill(1, Side::Sell, 0.3, 11.0, 0.0),
        ];
        let t = build_trades(&fills, &bars(&[10.0, 11.0]), &in_bar(&[0.0; 2]));
        assert_eq!(t.len(), 1);
        assert!(!t[0].open);
        approx(t[0].gross_pnl, 0.3);
    }

    #[test]
    fn no_fills_no_trades() {
        assert!(build_trades(&[], &bars(&[1.0, 2.0]), &[]).is_empty());
    }

    fn candle(i: i64, open: f64, high: f64, low: f64, close: f64) -> Candle {
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

    fn maker(f: Fill) -> Fill {
        Fill {
            liquidity: Liquidity::Maker,
            ..f
        }
    }

    /// Long 2 @ 100, sold 2 @ 107 on bar 2, whose low is 95.
    fn long_trade(exit: Fill) -> Vec<Trade> {
        let b = [
            candle(0, 100.0, 104.0, 97.0, 101.0),
            candle(1, 102.0, 108.0, 99.0, 106.0),
            candle(2, 105.0, 109.0, 95.0, 107.0),
        ];
        let fills = [fill(0, Side::Buy, 2.0, 100.0, 0.0), exit];
        build_trades(&fills, &b, &in_bar(&[0.0; 3]))
    }

    #[test]
    fn excursions_follow_the_fill_models_path() {
        // Bar 0, after the entry: low 97 is 2 * -3 = -6, high 104 is +8. Bar 1: open 102 is +4,
        // low 99 is -2, high 108 is +16. Bar 2: open 105 is +10, the exit at 107 is +14. A limit
        // (maker) exit fills after the stops inside the range, so the position also met the low
        // 95 first: 2 * -5 = -10.
        let t = long_trade(maker(fill(2, Side::Sell, 2.0, 107.0, 0.0)));
        assert_eq!(t.len(), 1);
        approx(t[0].mae.unwrap(), 10.0);
        approx(t[0].mfe.unwrap(), 16.0);

        // A market or stop (taker) exit met no worse price than its own, so bar 2's low does
        // not count once the position is closed: the worst stays bar 0's low.
        let t = long_trade(fill(2, Side::Sell, 2.0, 107.0, 0.0));
        approx(t[0].mae.unwrap(), 6.0);
        approx(t[0].mfe.unwrap(), 16.0);
    }

    #[test]
    fn excursions_of_a_short_scaled_into_use_the_entry_price_so_far() {
        let b = [
            candle(0, 50.0, 52.0, 48.0, 50.0),
            candle(1, 51.0, 55.0, 49.0, 54.0),
            candle(2, 54.0, 54.0, 45.0, 46.0),
        ];
        let fills = [
            // Short 1 @ 50: bar 0's high 52 is -2, its low 48 is +2.
            fill(0, Side::Sell, 1.0, 50.0, 0.0),
            // Bar 1: the open, 51, is -1 on the short 1. After the add at 51 the entry is 50.5 on
            // 2: the high 55 is 2 * -4.5 = -9, the low 49 is 2 * 1.5 = +3.
            fill(1, Side::Sell, 1.0, 51.0, 0.0),
            // Bar 2: closed at the open, 54, which is -7. The low 45 comes after the close.
            fill(2, Side::Buy, 2.0, 54.0, 0.0),
        ];
        let t = build_trades(&fills, &b, &in_bar(&[0.0; 3]));
        assert_eq!(t.len(), 1);
        approx(t[0].entry_price, 50.5);
        approx(t[0].mae.unwrap(), 9.0);
        approx(t[0].mfe.unwrap(), 3.0);
    }

    #[test]
    fn a_trade_that_never_goes_against_its_entry_has_zero_mae() {
        let b = [
            candle(0, 10.0, 12.0, 10.0, 11.0),
            candle(1, 11.0, 13.0, 11.0, 13.0),
        ];
        let t = build_trades(
            &[fill(0, Side::Buy, 1.0, 10.0, 0.0)],
            &b,
            &in_bar(&[0.0; 2]),
        );
        assert!(t[0].open);
        approx(t[0].mae.unwrap(), 0.0);
        approx(t[0].mfe.unwrap(), 3.0);
    }

    #[test]
    fn bar_stops_keep_the_nearest_trigger_per_side() {
        let mut s = BarStops::default();
        s.add(Side::Sell, 90.0);
        s.add(Side::Sell, 95.0);
        s.add(Side::Sell, 92.0);
        s.add(Side::Buy, 110.0);
        s.add(Side::Buy, 105.0);
        s.add(Side::Buy, 108.0);
        assert_eq!(s.sell, Some(95.0));
        assert_eq!(s.buy, Some(105.0));
    }

    #[test]
    fn r_multiple_is_net_pnl_over_the_risk_to_the_stop() {
        let b = [
            candle(0, 100.0, 104.0, 97.0, 101.0),
            candle(1, 102.0, 108.0, 99.0, 106.0),
            candle(2, 105.0, 109.0, 95.0, 107.0),
        ];
        let stop = |sell: Option<f64>, buy: Option<f64>| BarStops { sell, buy };

        // Long 2 @ 100 sold @ 107 with a fee of 1 at each end: net 14 - 2 = 12. A sell stop at
        // 95 risks 2 * 5 = 10, so R = 1.2. The entry bar's stops count, not later bars'.
        let fills = [
            fill(0, Side::Buy, 2.0, 100.0, 1.0),
            fill(2, Side::Sell, 2.0, 107.0, 1.0),
        ];
        let mut t = build_trades(&fills, &b, &in_bar(&[0.0; 3]));
        let stops = [
            stop(Some(95.0), Some(120.0)),
            stop(Some(99.0), None),
            stop(None, None),
        ];
        attach_stops(&mut t, &b, &stops);
        assert_eq!(t[0].stop, Some(95.0));
        approx(t[0].r_multiple.unwrap(), 1.2);

        // A long takes a sell stop only.
        let mut t = build_trades(&fills, &b, &in_bar(&[0.0; 3]));
        attach_stops(&mut t, &b, &[stop(None, Some(90.0))]);
        assert_eq!((t[0].stop, t[0].r_multiple), (None, None));

        // A stop at the entry price risks nothing: a stop, but no R.
        let mut t = build_trades(&fills, &b, &in_bar(&[0.0; 3]));
        attach_stops(&mut t, &b, &[stop(Some(100.0), None)]);
        assert_eq!((t[0].stop, t[0].r_multiple), (Some(100.0), None));

        // Short 2 @ 100 (entry bar 1), bought back @ 107: net -14. A buy stop at 103 risks
        // 2 * 3 = 6, so R = -14 / 6.
        let fills = [
            fill(1, Side::Sell, 2.0, 100.0, 0.0),
            fill(2, Side::Buy, 2.0, 107.0, 0.0),
        ];
        let mut t = build_trades(&fills, &b, &in_bar(&[0.0; 3]));
        attach_stops(
            &mut t,
            &b,
            &[stop(None, None), stop(Some(90.0), Some(103.0))],
        );
        assert_eq!(t[0].stop, Some(103.0));
        approx(t[0].r_multiple.unwrap(), -14.0 / 6.0);
    }
}
