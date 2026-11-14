use td_core::{Candle, Fill, Instrument, Liquidity, OrderType, Rounding, Side, TimeInForce};

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
/// the open, it calls it with the order's time in force set to IOC, and a triggered stop-limit
/// turned into its limit, to price it as a limit arriving at the open.
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
///   it, in full, even when the low only touches the limit: there is no queue, so a fill that a
///   real book would give only as far as the order's place in it allows is taken whole (see
///   [`touch_only`], which the battery counts). When a bar opens below it, the market passed the limit between the bars, where the
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
/// (see [`crate::SimBroker`]). After a real jump (a halt, an outage, or a market's close such as
/// the FX weekend, that reopens past the limit) a venue would fill the limit at the reopening
/// price, which is better, so this errs against the strategy. Stops the bar
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

/// Whether a resting limit's fill was touch-only: its side's bar reached the limit without trading
/// at least one tick through it, a buy whose low is above `price - tick` or a sell whose high is
/// below `price + tick`. [`CandleFillModel`] fills these in full on the touch alone; a rule that
/// asked for a trade-through would not fill them. `None` for a taker fill, which is not a resting
/// limit's.
///
/// It reads the fill as the model makes it: only a resting limit fills as a maker, always at its
/// own limit and as of the bar it filled on. So `fill.price` is the limit, and `bar` must be the
/// bar on the fill's side that opened at `fill.time`: the ask on a market with quotes for a buy,
/// the bid bar otherwise. Being worked out from the fill and its bar, not marked on the order,
/// nothing a strategy sends can change it.
pub(crate) fn touch_only(fill: &Fill, bar: &Candle, inst: &Instrument) -> Option<bool> {
    if fill.liquidity != Liquidity::Maker {
        return None;
    }
    // A hair under a tick, so a low exactly one tick below the limit reads as a trade-through
    // whichever way the float subtraction rounds.
    let through = inst.tick_size * (1.0 - 1e-6);
    Some(match fill.side {
        Side::Buy => bar.low > fill.price - through,
        Side::Sell => bar.high < fill.price + through,
    })
}

#[cfg(test)]
mod tests {
    use td_core::{FeeSchedule, InstrumentId, InstrumentKind, OrderId, OrderRequest, Venue};

    use super::*;
    use crate::OrderStatus;

    fn inst() -> Instrument {
        Instrument {
            id: InstrumentId {
                venue: Venue::Binance,
                symbol: "TESTUSDT".into(),
                kind: InstrumentKind::Perpetual,
            },
            base: "TEST".into(),
            quote: "USDT".into(),
            tick_size: 0.01,
            lot_size: 0.001,
            min_qty: 0.001,
            min_notional: 0.0,
            fees: FeeSchedule {
                maker: 0.0,
                taker: 0.0,
                per_unit: 0.0,
            },
        }
    }

    /// A limit at `price`, decided on a close of 100.
    fn limit(side: Side, price: f64) -> Order {
        Order {
            id: OrderId(1),
            request: OrderRequest {
                instrument: inst().id,
                side,
                qty: 1.0,
                order_type: OrderType::Limit { price },
                tif: TimeInForce::Gtc,
                reduce_only: false,
                client_tag: None,
            },
            submitted_at: 0,
            decision_close: 100.0,
            attached_stop: None,
            status: OrderStatus::Open,
        }
    }

    fn bar(open: f64, high: f64, low: f64) -> Candle {
        Candle {
            open_time: 60_000,
            open,
            high,
            low,
            close: open,
            volume: 1.0,
            quote_volume: open,
        }
    }

    /// The model's fill of `order` on `bar`, its first bar when `first`, made into the fill the
    /// broker would record, and whether that fill was touch-only.
    fn touch_only_on(order: &Order, first: bool, bar: &Candle) -> Option<bool> {
        let model = CandleFillModel { slippage_bps: 0.0 };
        let FillCheck::Fill {
            price, liquidity, ..
        } = model.check(order, first, false, bar, &inst())
        else {
            panic!("the limit should fill on {bar:?}");
        };
        let fill = Fill {
            order_id: order.id,
            instrument: inst().id,
            side: order.request.side,
            time: bar.open_time,
            price,
            qty: 1.0,
            fee: 0.0,
            liquidity,
            close_out: false,
        };
        touch_only(&fill, bar, &inst())
    }

    #[test]
    fn a_buy_limit_the_low_only_touches_is_touch_only() {
        let order = limit(Side::Buy, 99.03);
        assert_eq!(
            touch_only_on(&order, false, &bar(100.0, 100.5, 99.03)),
            Some(true)
        );
        // One tick below, which float subtraction may land either side of, and further.
        assert_eq!(
            touch_only_on(&order, false, &bar(100.0, 100.5, 99.02)),
            Some(false)
        );
        assert_eq!(
            touch_only_on(&order, false, &bar(100.0, 100.5, 98.5)),
            Some(false)
        );
        // Opened through it: filled between the bars, and traded through.
        assert_eq!(
            touch_only_on(&order, false, &bar(99.0, 99.5, 98.9)),
            Some(false)
        );
    }

    #[test]
    fn a_sell_limit_the_high_only_touches_is_touch_only() {
        let order = limit(Side::Sell, 101.07);
        assert_eq!(
            touch_only_on(&order, false, &bar(100.0, 101.07, 99.5)),
            Some(true)
        );
        assert_eq!(
            touch_only_on(&order, false, &bar(100.0, 101.08, 99.5)),
            Some(false)
        );
        assert_eq!(
            touch_only_on(&order, false, &bar(100.0, 102.0, 99.5)),
            Some(false)
        );
        assert_eq!(
            touch_only_on(&order, false, &bar(101.2, 101.5, 101.1)),
            Some(false)
        );
    }

    #[test]
    fn a_limit_that_takes_liquidity_is_not_a_resting_limit_fill() {
        // Marketable at its decision close, so on its first bar it fills at the open as a taker.
        let order = limit(Side::Buy, 101.0);
        assert_eq!(touch_only_on(&order, true, &bar(100.0, 100.5, 99.5)), None);
    }
}
