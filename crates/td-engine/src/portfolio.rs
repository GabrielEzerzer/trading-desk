use serde::Serialize;
use td_core::{InstrumentKind, Side};

/// Cash, position and P&L for the session's one instrument, in the quote asset.
///
/// What `cash` means depends on the instrument:
/// - Spot: the quote balance. A buy pays `qty * price` plus the fee out of it, a sell adds the
///   proceeds. Equity is `cash + position * mark`.
/// - Perpetual: the wallet balance. Opening a position costs only the fee; realised P&L, fees and
///   funding move the balance. Equity is `cash + position * (mark - avg_entry)`.
///
/// shortcut: perpetual positions are only marked to market and charged funding; there is no
/// margin, leverage limit or liquidation. Upgrade when strategies trade with real leverage.
///
/// shortcut: spot buys are checked against the cash when they are submitted, a market buy at the
/// decision bar's close and a stop at its trigger, so a fill above that price (a gap up at the
/// next open, or slippage) can still overdraw the cash slightly. Upgrade by checking again in the
/// broker at fill time if a strategy depends on never overdrawing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Portfolio {
    kind: InstrumentKind,
    cash: f64,
    position: f64,
    avg_entry: f64,
    realised_pnl: f64,
    fees_paid: f64,
    funding_paid: f64,
    mark_price: f64,
}

impl Portfolio {
    pub fn new(kind: InstrumentKind, starting_cash: f64) -> Self {
        Portfolio {
            kind,
            cash: starting_cash,
            position: 0.0,
            avg_entry: 0.0,
            realised_pnl: 0.0,
            fees_paid: 0.0,
            funding_paid: 0.0,
            mark_price: 0.0,
        }
    }

    pub fn cash(&self) -> f64 {
        self.cash
    }

    /// Signed quantity in the base asset: positive long, negative short.
    pub fn position(&self) -> f64 {
        self.position
    }

    /// Average entry price of the open position; 0 when flat.
    pub fn avg_entry(&self) -> f64 {
        self.avg_entry
    }

    /// P&L of closed quantity against the average entry price, before fees and funding.
    pub fn realised_pnl(&self) -> f64 {
        self.realised_pnl
    }

    /// P&L of the open position at the last mark price.
    pub fn unrealised_pnl(&self) -> f64 {
        self.position * (self.mark_price - self.avg_entry)
    }

    pub fn fees_paid(&self) -> f64 {
        self.fees_paid
    }

    /// Net funding paid; negative when the position received funding.
    pub fn funding_paid(&self) -> f64 {
        self.funding_paid
    }

    /// The price the position was last marked at: the close of the latest live bar. Warm-up bars
    /// do not mark it.
    pub fn mark_price(&self) -> f64 {
        self.mark_price
    }

    pub fn equity(&self) -> f64 {
        match self.kind {
            InstrumentKind::Spot => self.cash + self.position * self.mark_price,
            InstrumentKind::Perpetual => self.cash + self.unrealised_pnl(),
        }
    }

    pub(crate) fn apply_fill(&mut self, side: Side, qty: f64, price: f64, fee: f64) {
        let dq = side.sign() * qty;
        let pos = self.position;
        self.cash -= fee;
        self.fees_paid += fee;
        if self.kind == InstrumentKind::Spot {
            self.cash -= dq * price;
        }
        if pos != 0.0 && pos.signum() != dq.signum() {
            let closed = qty.min(pos.abs());
            let pnl = closed * (price - self.avg_entry) * pos.signum();
            self.realised_pnl += pnl;
            if self.kind == InstrumentKind::Perpetual {
                self.cash += pnl;
            }
        }
        let mut new = pos + dq;
        // Closing 0.3 that was bought as 0.1 + 0.2 leaves 5.6e-17 behind; that is flat.
        if new.abs() <= 1e-9 * (pos.abs() + qty) {
            new = 0.0;
        }
        self.avg_entry = if new == 0.0 {
            0.0
        } else if pos == 0.0 || pos.signum() != new.signum() {
            // Opened from flat, or flipped through zero: the remainder was all bought at `price`.
            price
        } else if new.abs() > pos.abs() {
            (pos.abs() * self.avg_entry + qty * price) / new.abs()
        } else {
            self.avg_entry
        };
        self.position = new;
    }

    /// Charges one funding payment of `position * price * rate`: longs pay a positive rate and
    /// shorts receive it. Returns the amount paid.
    pub(crate) fn charge_funding(&mut self, rate: f64, price: f64) -> f64 {
        let payment = self.position * price * rate;
        self.cash -= payment;
        self.funding_paid += payment;
        payment
    }

    pub(crate) fn mark(&mut self, price: f64) {
        self.mark_price = price;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn perpetual_average_entry_and_realised_pnl() {
        let mut p = Portfolio::new(InstrumentKind::Perpetual, 1000.0);
        p.apply_fill(Side::Buy, 1.0, 100.0, 0.0);
        p.apply_fill(Side::Buy, 1.0, 110.0, 0.0);
        assert!(close(p.avg_entry(), 105.0));
        p.apply_fill(Side::Sell, 0.5, 120.0, 0.0);
        assert!(close(p.realised_pnl(), 7.5));
        assert!(close(p.avg_entry(), 105.0));
        // Flip from long 1.5 to short 0.5 at 90: closes 1.5 at a loss of 15 each.
        p.apply_fill(Side::Sell, 2.0, 90.0, 0.0);
        assert!(close(p.realised_pnl(), 7.5 - 22.5));
        assert!(close(p.position(), -0.5));
        assert!(close(p.avg_entry(), 90.0));
        assert!(close(p.cash(), 1000.0 - 15.0));
        p.mark(80.0);
        assert!(close(p.equity(), 985.0 + 5.0));
    }

    #[test]
    fn spot_flat_after_float_residue() {
        let mut p = Portfolio::new(InstrumentKind::Spot, 1000.0);
        p.apply_fill(Side::Buy, 0.1, 10.0, 0.0);
        p.apply_fill(Side::Buy, 0.2, 10.0, 0.0);
        p.apply_fill(Side::Sell, 0.3, 10.0, 0.0);
        assert_eq!(p.position(), 0.0);
        assert_eq!(p.avg_entry(), 0.0);
    }
}
