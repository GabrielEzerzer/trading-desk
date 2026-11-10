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
    },
}

/// Decides whether and at what price an order fills on a bar. The broker calls it only for bars
/// after the one the order was decided on, and handles fees, ordering and order state.
pub trait FillModel: Send {
    /// `triggered` says whether a stop-limit was triggered on an earlier bar.
    fn check(&self, order: &Order, triggered: bool, bar: &Candle, inst: &Instrument) -> FillCheck;
}

/// Fills orders from OHLC bars alone.
///
/// - Market: the bar's open, moved against the order by `slippage_bps`. Taker.
/// - Limit buy: fills if the low reaches the limit, at the limit, or at the open when the bar
///   opens below it. Sells mirror this. Maker.
/// - Stop buy: triggers if the high reaches the trigger and fills at `max(open, trigger)` plus
///   slippage. Sells mirror this. Taker.
/// - Stop-limit: triggers like a stop. If the limit is marketable at the trigger point it fills
///   there with slippage, never past the limit, as a taker. Otherwise it rests as a limit from
///   the next bar on: the bar's path after the trigger is unknown, so it is assumed not to come
///   back to the limit within the same bar.
/// - IOC: only the bar's open is reachable, since the order never rests; it fills as a taker or
///   not at all.
///
/// Slipped prices are rounded to the tick against the order (buys up, sells down).
///
/// shortcut: a limit order that is already marketable when it arrives is charged the maker fee,
/// where a venue would charge taker; upgrade with a trade-level fill model.
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
    fn check(&self, order: &Order, triggered: bool, bar: &Candle, inst: &Instrument) -> FillCheck {
        let req = &order.request;
        let buy = req.side == Side::Buy;
        let ioc = req.tif == TimeInForce::Ioc;
        let open = bar.open;
        let (high, low) = if ioc {
            (open, open)
        } else {
            (bar.high, bar.low)
        };
        let resting = if ioc {
            Liquidity::Taker
        } else {
            Liquidity::Maker
        };
        let limit = |price: f64| {
            let reached = if buy { low <= price } else { high >= price };
            let fill = if buy {
                open.min(price)
            } else {
                open.max(price)
            };
            if reached {
                FillCheck::Fill {
                    price: fill,
                    liquidity: resting,
                }
            } else {
                FillCheck::NoFill
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
            },
            OrderType::Limit { price } => limit(price),
            OrderType::Stop { trigger } if reaches(trigger) => FillCheck::Fill {
                price: self.slip(trigger_point(trigger), req.side, inst),
                liquidity: Liquidity::Taker,
            },
            OrderType::Stop { .. } => FillCheck::NoFill,
            OrderType::StopLimit { price, .. } if triggered => limit(price),
            OrderType::StopLimit { trigger, price } if reaches(trigger) => {
                let point = trigger_point(trigger);
                let marketable = if buy { point <= price } else { point >= price };
                if !marketable {
                    return FillCheck::Triggered;
                }
                let slipped = self.slip(point, req.side, inst);
                FillCheck::Fill {
                    price: if buy {
                        slipped.min(price)
                    } else {
                        slipped.max(price)
                    },
                    liquidity: Liquidity::Taker,
                }
            }
            OrderType::StopLimit { .. } => FillCheck::NoFill,
        }
    }
}
