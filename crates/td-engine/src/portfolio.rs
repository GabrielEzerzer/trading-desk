use serde::Serialize;
use td_core::{AccountCurrency, InstrumentKind, Side};

/// Cash, position and P&L for the session's one instrument, in the account currency: the quote
/// asset on a [`AccountCurrency::Quote`] account, and the base currency on a
/// [`AccountCurrency::Base`] one, where every amount in the quote currency (a fill's P&L, its fee,
/// the open position's value) is converted at the price it arose at; see
/// [`Portfolio::apply_fill`] and [`Portfolio::unrealised_pnl`]. Prices and the average entry stay
/// in the quote currency, as prices.
///
/// What `cash` means depends on the instrument:
/// - Spot: the quote balance. A buy pays `qty * price` plus the fee out of it, a sell adds the
///   proceeds. Equity is `cash + position * mark`.
/// - Perpetual: the wallet balance. Opening a position costs only the fee; realised P&L, fees and
///   funding move the balance. Equity is `cash + position * (mark - avg_entry)`.
///
/// Neither balance goes below zero. A spot buy is cut at fill time to what the cash covers (see
/// [`crate::SimBroker`]). A perpetual position is capped by the session's leverage limit when an
/// order is submitted and liquidated when the maintenance margin is breached (see
/// [`crate::Session::on_bar`]); a perpetual account left flat with a negative balance is floored
/// at zero and the shortfall recorded in [`Portfolio::shortfall_written_off`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Portfolio {
    kind: InstrumentKind,
    #[serde(skip_serializing_if = "AccountCurrency::is_quote")]
    account: AccountCurrency,
    cash: f64,
    position: f64,
    avg_entry: f64,
    realised_pnl: f64,
    fees_paid: f64,
    funding_paid: f64,
    mark_price: f64,
    shortfall_written_off: f64,
}

impl Portfolio {
    /// A quote-currency account.
    pub fn new(kind: InstrumentKind, starting_cash: f64) -> Self {
        Portfolio::new_in(kind, starting_cash, AccountCurrency::Quote)
    }

    /// An account in `account`, holding `starting_cash` of it. [`crate::Session::new`] checks
    /// that the kind allows the account currency.
    pub(crate) fn new_in(
        kind: InstrumentKind,
        starting_cash: f64,
        account: AccountCurrency,
    ) -> Self {
        Portfolio {
            kind,
            account,
            cash: starting_cash,
            position: 0.0,
            avg_entry: 0.0,
            realised_pnl: 0.0,
            fees_paid: 0.0,
            funding_paid: 0.0,
            mark_price: 0.0,
            shortfall_written_off: 0.0,
        }
    }

    /// The currency every amount here is in.
    pub fn account(&self) -> AccountCurrency {
        self.account
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

    /// P&L of closed quantity against the average entry price, before fees and funding. On a base
    /// account each closing fill's P&L is converted at that fill's price.
    pub fn realised_pnl(&self) -> f64 {
        self.realised_pnl
    }

    /// P&L of the open position at the last mark price. On a base account it is converted at the
    /// mark: closing the position there would realise the quote amount at that price.
    pub fn unrealised_pnl(&self) -> f64 {
        let pnl = self.position * (self.mark_price - self.avg_entry);
        // Flat before the first mark has a mark of zero to divide by.
        if self.position == 0.0 {
            return pnl;
        }
        self.account.from_quote(pnl, self.mark_price)
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

    /// Losses beyond the account's equity that were written off: a perpetual account left flat
    /// with a negative balance (a liquidation that filled past its price on a gap, or a stop that
    /// filled beyond the liquidation price) has its balance set to zero, as a venue's insurance
    /// fund would cover the rest.
    pub fn shortfall_written_off(&self) -> f64 {
        self.shortfall_written_off
    }

    pub fn equity(&self) -> f64 {
        if self.kind.rules().margin {
            self.cash + self.unrealised_pnl()
        } else {
            self.cash + self.position * self.mark_price
        }
    }

    /// Applies a fill of `qty` at `price`, paying `fee`, which is in the account currency (a
    /// broker converts it at the fill's price, see [`crate::SimBroker`]). The P&L of the quantity
    /// it closes arises in the quote currency and is converted at `price`.
    ///
    /// shortcut: a spot account is always in the quote currency ([`crate::Session::new`] refuses
    /// a base account there), so the spot cash movement `qty * price` is not converted.
    pub(crate) fn apply_fill(&mut self, side: Side, qty: f64, price: f64, fee: f64) {
        let dq = side.sign() * qty;
        let pos = self.position;
        self.cash -= fee;
        self.fees_paid += fee;
        let margin = self.kind.rules().margin;
        if !margin {
            self.cash -= dq * price;
        }
        if pos != 0.0 && pos.signum() != dq.signum() {
            let closed = qty.min(pos.abs());
            let pnl = self
                .account
                .from_quote(closed * (price - self.avg_entry) * pos.signum(), price);
            self.realised_pnl += pnl;
            if margin {
                self.cash += pnl;
            }
        }
        (self.position, self.avg_entry) = next_position(pos, self.avg_entry, dq, price);
    }

    /// Charges one funding payment of `position * price * rate`: longs pay a positive rate and
    /// shorts receive it. Returns the amount paid.
    ///
    /// Not converted: funding is charged only on a kind whose rules have it (perpetuals), and
    /// [`crate::Session::new`] refuses a base account on those, so it is always a quote account.
    pub(crate) fn charge_funding(&mut self, rate: f64, price: f64) -> f64 {
        let payment = self.position * price * rate;
        self.cash -= payment;
        self.funding_paid += payment;
        payment
    }

    /// Sets the balance to exactly zero, for a liquidation that keeps the rest of the margin.
    pub(crate) fn clear_cash(&mut self) {
        self.cash = 0.0;
    }

    /// Floors a flat perpetual account's negative balance at zero and records the shortfall.
    pub(crate) fn write_off_shortfall(&mut self) {
        if self.kind == InstrumentKind::Perpetual && self.position == 0.0 && self.cash < 0.0 {
            self.shortfall_written_off -= self.cash;
            self.cash = 0.0;
        }
    }

    pub(crate) fn mark(&mut self, price: f64) {
        self.mark_price = price;
    }
}

/// The position and average entry after a fill of signed quantity `dq` at `price`, from a
/// position of `pos` entered at an average of `avg`. The one rule for both, which the trade list
/// and the intrabar risk measures replay so they agree with the portfolio.
pub(crate) fn next_position(pos: f64, avg: f64, dq: f64, price: f64) -> (f64, f64) {
    let qty = dq.abs();
    let mut new = pos + dq;
    // Closing 0.3 that was bought as 0.1 + 0.2 leaves 5.6e-17 behind; that is flat.
    if new.abs() <= 1e-9 * (pos.abs() + qty) {
        new = 0.0;
    }
    let avg = if new == 0.0 {
        0.0
    } else if pos == 0.0 || pos.signum() != new.signum() {
        // Opened from flat, or flipped through zero: the remainder was all bought at `price`.
        price
    } else if new.abs() > pos.abs() {
        (pos.abs() * avg + qty * price) / new.abs()
    } else {
        avg
    };
    (new, avg)
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
    fn fx_books_profit_and_loss_into_cash_like_any_margined_market() {
        let mut p = Portfolio::new(InstrumentKind::Fx, 1000.0);
        p.apply_fill(Side::Sell, 1.0, 110.0, 0.0);
        // Margined: opening a position moves no cash.
        assert!(close(p.cash(), 1000.0));
        p.mark(105.0);
        assert!(close(p.equity(), 1005.0));
        p.apply_fill(Side::Buy, 1.0, 100.0, 0.0);
        assert!(close(p.cash(), 1010.0));
        assert!(close(p.equity(), 1010.0));
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
