//! Round-trip trades built from fills.

use serde::{Deserialize, Serialize};
use td_core::{Candle, Fill, Millis};

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
    /// last bar.
    pub bars_held: usize,
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

struct Building {
    side: TradeSide,
    entry_time: Millis,
    entry_bar: usize,
    max_abs: f64,
    entry_qty: f64,
    entry_notional: f64,
    exit_qty: f64,
    exit_notional: f64,
    fees: f64,
    funding: f64,
}

impl Building {
    fn new(side: TradeSide, fill: &Fill, bar: usize, qty: f64, fee: f64) -> Building {
        Building {
            side,
            entry_time: fill.time,
            entry_bar: bar,
            max_abs: qty,
            entry_qty: qty,
            entry_notional: qty * fill.price,
            exit_qty: 0.0,
            exit_notional: 0.0,
            fees: fee,
            funding: 0.0,
        }
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
pub fn build_trades(fills: &[Fill], bars: &[Candle], bar_funding: &[BarFunding]) -> Vec<Trade> {
    let mut trades = Vec::new();
    let mut current: Option<Building> = None;
    let mut pos = 0.0;
    let mut next = 0;
    for (k, bar) in bars.iter().enumerate() {
        let funding = bar_funding.get(k).copied().unwrap_or_default();
        if let Some(t) = current.as_mut() {
            t.funding += funding.at_open;
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
                    trades.push(done.finish(fill.time, held, false));
                }
                if flipped {
                    let side = if new > 0.0 {
                        TradeSide::Long
                    } else {
                        TradeSide::Short
                    };
                    let rest = qty - closed;
                    current = Some(Building::new(side, fill, k, rest, fill.fee - closing_fee));
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
                current = Some(Building::new(side, fill, k, qty, fill.fee));
            }
            pos = new;
        }
        if let Some(t) = current.as_mut() {
            t.funding += funding.in_bar;
        }
    }
    if let (Some(mut t), Some(last)) = (current, bars.last()) {
        let remaining = f64::abs(pos);
        t.exit_qty += remaining;
        t.exit_notional += remaining * last.close;
        let held = bars.len() - t.entry_bar;
        trades.push(t.finish(last.open_time, held, true));
    }
    trades
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
}
