use td_core::{Candle, Instrument, Liquidity, OrderType, Rounding, Side, TimeInForce};

use crate::Order;

/// What a fill model decided for one order on one bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FillCheck {
    NoFill,
    /// A stop-limit's trigger was reached but its limit did not fill; it now rests as a limit.
    Triggered,
    Fill {
        price: f64,
        liquidity: Liquidity,
        /// The market price the order met, before slippage and before capping at a limit. When it
        /// is the bar's open, the broker takes the fill as happening at the open, before any fill
        /// inside the bar's range, and an attached stop is checked against the rest of the bar
        /// from this price. A resting limit the bar opens through has the open here and its limit
        /// in `price`, and fills as a maker: the broker takes it as filled between the bars, before
        /// the fills at the open, and checks its attached stop from the limit, with the market
        /// trading on from there to the open.
        market: f64,
    },
}

/// Decides whether and at what price an order fills on a bar. The broker calls it only for bars
/// after the one the order was decided on, and handles fees, ordering and order state. For an
/// attached stop it also calls it on its entry's fill bar, with the bar's open replaced by the
/// price the entry met the market at: its market price, or its limit when it filled between the
/// bars, with the range stretched to include that limit. For a reduce-only limit that waits for
/// the open, it calls it with the order's time in force set to IOC, to price it as a limit arriving
/// at the open.
pub trait FillModel: Send {
    /// `first` says whether this is the first bar the order can fill on. `triggered` says whether
    /// a stop-limit was triggered on an earlier bar.
    fn check(
        &self,
        order: &Order,
        first: bool,
        triggered: bool,
        bar: &Candle,
        inst: &Instrument,
    ) -> FillCheck;
}

/// Fills orders from OHLC bars alone.
///
/// - Market: the bar's open, moved against the order by `slippage_bps`. Taker.
/// - Limit buy: rests from the close it was decided on, and fills at the limit if the low reaches
///   it. When a bar opens below it, the market passed the limit between the bars, where the
///   resting order filled: it fills at the limit, not the better open, as of the open. Sells
///   mirror this. Maker.
/// - Limit already marketable at the close it was decided on (a buy at or above that close, a
///   sell at or below it): on its first bar it takes liquidity like a market order, at the open
///   plus slippage but never past its limit, as a taker. If that bar opens past the limit, it
///   rests as an ordinary limit instead.
/// - Stop buy: triggers if the high reaches the trigger and fills at `max(open, trigger)` plus
///   slippage. Sells mirror this. Taker.
/// - Stop-limit: triggers like a stop. If the limit is marketable at the trigger point it fills
///   there with slippage, never past the limit, as a taker. Otherwise it rests as a limit from
///   the next bar on: the bar's path after the trigger is unknown, so it is assumed not to come
///   back to the limit within the same bar. From then on the limit rests like any other.
/// - IOC: only the bar's open is reachable, since the order never rests. A limit marketable there
///   fills like a marketable limit, at the open plus slippage but never past its limit, as a
///   taker; otherwise it does not fill.
///
/// Slipped prices are rounded to the tick against the order (buys up, sells down).
///
/// shortcut: a resting limit a bar opens through fills at its own price, which assumes the market
/// traded through it between the bars, as on a venue that trades around the clock with no gap.
/// The broker keeps to that assumption for what follows from the fill: it comes before the fills
/// at the open, and an attached stop between the limit and the open fills at its trigger on the
/// way. A reduce-only limit with nothing to reduce between the bars waits for the open instead
/// (see [`crate::SimBroker`]). After a real jump (a halt or an outage that reopens past the limit) a venue would fill the
/// limit at the reopening price, which is better, so this errs against the strategy. Stops the bar
/// opens through still fill at the open, which errs against the strategy the other way. Upgrade
/// with a trade-level fill model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandleFillModel {
    pub slippage_bps: f64,
}

impl CandleFillModel {
    /// Moves `price` against the order by the slippage and rounds it to the tick against the
    /// order: buys up, sells down.
    pub(crate) fn slip(&self, price: f64, side: Side, inst: &Instrument) -> f64 {
        let factor = self.slippage_bps / 10_000.0;
        match side {
            Side::Buy => inst.round_price(price * (1.0 + factor), Rounding::Up),
            Side::Sell => inst.round_price(price * (1.0 - factor), Rounding::Down),
        }
    }
}

impl FillModel for CandleFillModel {
    fn check(
        &self,
        order: &Order,
        first: bool,
        triggered: bool,
        bar: &Candle,
        inst: &Instrument,
    ) -> FillCheck {
        let req = &order.request;
        let buy = req.side == Side::Buy;
        let ioc = req.tif == TimeInForce::Ioc;
        let open = bar.open;
        let (high, low) = if ioc {
            (open, open)
        } else {
            (bar.high, bar.low)
        };
        // A resting limit fills at its own price, as a maker, whether the bar reaches it inside its
        // range or opens through it. Through the open, it filled between the bars, so as of the
        // open. An IOC order never rests, so it never fills here: its range is only the open, and
        // a limit marketable there is taken below as a taker.
        let limit = |price: f64| {
            let reached = if buy { low <= price } else { high >= price };
            let through = if buy { open <= price } else { open >= price };
            if reached {
                FillCheck::Fill {
                    price,
                    liquidity: Liquidity::Maker,
                    market: if through { open } else { price },
                }
            } else {
                FillCheck::NoFill
            }
        };
        // Never trade past a limit: a buy pays at most it, a sell gets at least it.
        let cap = |price: f64, limit: f64| {
            if buy {
                price.min(limit)
            } else {
                price.max(limit)
            }
        };
        let reaches = |trigger: f64| if buy { high >= trigger } else { low <= trigger };
        let trigger_point = |trigger: f64| {
            if buy {
                open.max(trigger)
            } else {
                open.min(trigger)
            }
        };
        match req.order_type {
            OrderType::Market => FillCheck::Fill {
                price: self.slip(open, req.side, inst),
                liquidity: Liquidity::Taker,
                market: open,
            },
            OrderType::Limit { price } => {
                let marketable_at = |level: f64| if buy { price >= level } else { price <= level };
                // An IOC order arrives at the open; any other limit at the decision close, and it
                // rests from there unless it was marketable then.
                let arrives_marketable = ioc || (first && marketable_at(order.decision_close));
                if arrives_marketable && marketable_at(open) {
                    // Sent through the market, so it takes liquidity as soon as it arrives.
                    FillCheck::Fill {
                        price: cap(self.slip(open, req.side, inst), price),
                        liquidity: Liquidity::Taker,
                        market: open,
                    }
                } else {
                    limit(price)
                }
            }
            OrderType::Stop { trigger } if reaches(trigger) => FillCheck::Fill {
                price: self.slip(trigger_point(trigger), req.side, inst),
                liquidity: Liquidity::Taker,
                market: trigger_point(trigger),
            },
            OrderType::Stop { .. } => FillCheck::NoFill,
            OrderType::StopLimit { price, .. } if triggered => limit(price),
            OrderType::StopLimit { trigger, price } if reaches(trigger) => {
                let point = trigger_point(trigger);
                let marketable = if buy { point <= price } else { point >= price };
                if !marketable {
                    return FillCheck::Triggered;
                }
                FillCheck::Fill {
                    price: cap(self.slip(point, req.side, inst), price),
                    liquidity: Liquidity::Taker,
                    market: point,
                }
            }
            OrderType::StopLimit { .. } => FillCheck::NoFill,
        }
    }
}
