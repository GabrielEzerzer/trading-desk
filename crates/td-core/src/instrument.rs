use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{CoreError, OrderRequest, OrderType};

/// An exchange the desk reads data from and simulates orders against.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Venue {
    Binance,
    Bybit,
}

impl fmt::Display for Venue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Venue::Binance => "binance",
            Venue::Bybit => "bybit",
        })
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstrumentKind {
    Spot,
    /// A perpetual futures contract (linear, settled in the quote asset).
    Perpetual,
    /// A spot currency pair, for example USD/JPY.
    Fx,
}

impl fmt::Display for InstrumentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            InstrumentKind::Spot => "spot",
            InstrumentKind::Perpetual => "perpetual",
            InstrumentKind::Fx => "fx",
        })
    }
}

/// Identifies an instrument across venues. The same symbol can exist as spot and as a perpetual on
/// one venue, so the kind is part of the key.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InstrumentId {
    pub venue: Venue,
    /// The venue's own symbol, for example `BTCUSDT`.
    pub symbol: String,
    pub kind: InstrumentKind,
}

impl fmt::Display for InstrumentId {
    /// Formats as `venue:symbol:kind`, for example `binance:BTCUSDT:spot`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.venue, self.symbol, self.kind)
    }
}

/// Trading fees. A fill pays `price * qty * rate + qty * per_unit`, where the rate is the maker or
/// taker fraction of notional (0.001 means 0.1%) and `per_unit` a commission per unit of the base
/// asset, in the quote currency.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FeeSchedule {
    pub maker: f64,
    pub taker: f64,
    /// Commission per unit of the base asset on every fill, in the quote currency, as FX brokers
    /// charge. Left out of saved JSON when it is zero, the default.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub per_unit: f64,
}

impl FeeSchedule {
    /// The fee on a fill of `qty` at `price` paying `rate` of notional. The per-unit term is added
    /// only when it is set, so a schedule without one gives exactly `price * qty * rate`.
    pub fn fee(&self, price: f64, qty: f64, rate: f64) -> f64 {
        let fee = price * qty * rate;
        if self.per_unit == 0.0 {
            fee
        } else {
            fee + qty * self.per_unit
        }
    }
}

fn is_zero(x: &f64) -> bool {
    *x == 0.0
}

/// Which way [`Instrument::round_price`] moves a price that is not on a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rounding {
    Down,
    Up,
    Nearest,
}

/// A tradable instrument with the venue's trading rules.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instrument {
    pub id: InstrumentId,
    /// The asset being bought or sold, for example `BTC`.
    pub base: String,
    /// The asset prices, notional and fees are in, for example `USDT`.
    pub quote: String,
    /// Smallest price increment.
    pub tick_size: f64,
    /// Smallest quantity increment.
    pub lot_size: f64,
    /// Smallest order quantity, in the base asset.
    pub min_qty: f64,
    /// Smallest order value (quantity times price), in the quote asset.
    pub min_notional: f64,
    pub fees: FeeSchedule,
}

impl Instrument {
    /// Rounds a price to a multiple of the tick size in the given direction.
    pub fn round_price(&self, price: f64, rounding: Rounding) -> f64 {
        round_to_step(price, self.tick_size, rounding)
    }

    /// Rounds a quantity down to a multiple of the lot size, so an order never exceeds what was asked.
    pub fn round_qty(&self, qty: f64) -> f64 {
        round_to_step(qty, self.lot_size, Rounding::Down)
    }

    /// Checks an order against this instrument's trading rules. Call it after rounding: the quantity
    /// must be on the lot grid, limit and trigger prices on the tick grid, and both at or above the
    /// minimums.
    ///
    /// The notional uses the order's limit price, or its trigger for a stop, or `last_price` for a
    /// market order.
    pub fn validate_order(&self, order: &OrderRequest, last_price: f64) -> Result<(), CoreError> {
        if order.instrument != self.id {
            return Err(CoreError::InstrumentMismatch {
                order: order.instrument.clone(),
                instrument: self.id.clone(),
            });
        }
        let qty = positive("qty", order.qty)?;
        if self.round_qty(qty) != qty {
            return Err(CoreError::QtyOffLot {
                qty,
                lot_size: self.lot_size,
            });
        }
        let on_tick = |field: &'static str, price: f64| {
            let price = positive(field, price)?;
            if self.round_price(price, Rounding::Nearest) != price {
                return Err(CoreError::PriceOffTick {
                    field,
                    price,
                    tick_size: self.tick_size,
                });
            }
            Ok(price)
        };
        let price = match order.order_type {
            OrderType::Market => positive("last_price", last_price)?,
            OrderType::Limit { price } => on_tick("price", price)?,
            OrderType::Stop { trigger } => on_tick("trigger", trigger)?,
            OrderType::StopLimit { trigger, price } => {
                on_tick("trigger", trigger)?;
                on_tick("price", price)?
            }
        };
        // A rounded quantity times a price can land a hair below an exact minimum (0.57 * 100 is
        // 56.99999999999999), so the comparisons allow a relative tolerance.
        if qty < self.min_qty * (1.0 - TOLERANCE) {
            return Err(CoreError::QtyBelowMin {
                qty,
                min: self.min_qty,
            });
        }
        // Venues exempt reduce-only perpetual orders from the minimum notional, so a position that
        // has shrunk below it in value can still be closed. Whether the order really reduces a
        // position is the broker's check.
        let exempt = order.reduce_only && self.id.kind == InstrumentKind::Perpetual;
        let notional = qty * price;
        if !exempt && notional < self.min_notional * (1.0 - TOLERANCE) {
            return Err(CoreError::NotionalBelowMin {
                notional,
                min: self.min_notional,
            });
        }
        Ok(())
    }
}

const TOLERANCE: f64 = 1e-9;

fn positive(field: &'static str, value: f64) -> Result<f64, CoreError> {
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err(CoreError::InvalidNumber { field, value })
    }
}

/// Rounds `value` to a multiple of `step`.
///
/// Steps are decimal (0.01, 0.001, 5), which binary floats cannot hold exactly, so a naive
/// `value / step` can land just under a whole number (0.3 / 0.1 is 2.9999999999999996). The step
/// is scaled to an integer number of units first, a small epsilon absorbs the remaining error, and
/// the result is built from integers so it is the float nearest the decimal value. The epsilon is
/// a few units of float error in the step count, so it stays far below one step even for large
/// counts.
fn round_to_step(value: f64, step: f64, rounding: Rounding) -> f64 {
    let scale = 10f64.powi(step_decimals(step));
    let step_units = (step * scale).round();
    let steps = value * scale / step_units;
    let eps = (steps.abs() * 4.0 * f64::EPSILON).max(1e-9);
    let n = match rounding {
        Rounding::Down => (steps + eps).floor(),
        Rounding::Up => (steps - eps).ceil(),
        Rounding::Nearest => steps.round(),
    };
    n * step_units / scale
}

/// The number of decimal places in `step`: 2 for 0.01, 0 for 5. Capped at 15.
fn step_decimals(step: f64) -> i32 {
    (0..15)
        .find(|&d| {
            let scaled = step * 10f64.powi(d);
            let whole = scaled.round();
            whole >= 1.0 && (scaled - whole).abs() <= 1e-9 * whole
        })
        .unwrap_or(15)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Side, TimeInForce};

    fn btc_perp() -> Instrument {
        Instrument {
            id: InstrumentId {
                venue: Venue::Binance,
                symbol: "BTCUSDT".into(),
                kind: InstrumentKind::Perpetual,
            },
            base: "BTC".into(),
            quote: "USDT".into(),
            tick_size: 0.1,
            lot_size: 0.001,
            min_qty: 0.001,
            min_notional: 5.0,
            fees: FeeSchedule {
                maker: 0.0002,
                taker: 0.0005,
                per_unit: 0.0,
            },
        }
    }

    fn with_steps(tick_size: f64, lot_size: f64) -> Instrument {
        Instrument {
            tick_size,
            lot_size,
            ..btc_perp()
        }
    }

    fn order(qty: f64, order_type: OrderType) -> OrderRequest {
        OrderRequest {
            instrument: btc_perp().id,
            side: Side::Buy,
            qty,
            order_type,
            tif: TimeInForce::Gtc,
            reduce_only: false,
            client_tag: None,
        }
    }

    #[test]
    fn price_rounds_to_tick_in_each_direction() {
        let i = btc_perp();
        assert_eq!(i.round_price(65000.17, Rounding::Down), 65000.1);
        assert_eq!(i.round_price(65000.17, Rounding::Up), 65000.2);
        assert_eq!(i.round_price(65000.17, Rounding::Nearest), 65000.2);
        assert_eq!(i.round_price(65000.12, Rounding::Nearest), 65000.1);
    }

    #[test]
    fn price_already_on_tick_is_unchanged() {
        let i = btc_perp();
        for rounding in [Rounding::Down, Rounding::Up, Rounding::Nearest] {
            assert_eq!(i.round_price(65000.3, rounding), 65000.3);
        }
    }

    #[test]
    fn whole_number_ticks() {
        let i = with_steps(5.0, 1.0);
        assert_eq!(i.round_price(1003.0, Rounding::Down), 1000.0);
        assert_eq!(i.round_price(1003.0, Rounding::Up), 1005.0);
        assert_eq!(i.round_qty(7.9), 7.0);
    }

    #[test]
    fn qty_rounds_down_to_lot() {
        let i = btc_perp();
        assert_eq!(i.round_qty(0.0129), 0.012);
        assert_eq!(i.round_qty(0.0005), 0.0);
    }

    #[test]
    fn float_error_does_not_drop_a_lot() {
        // 0.3 / 0.1 is 2.9999999999999996 and 1.15 / 0.05 is 22.999999999999996 in f64.
        assert_eq!(with_steps(0.1, 0.1).round_qty(0.3), 0.3);
        assert_eq!(with_steps(0.1, 0.05).round_qty(1.15), 1.15);
        assert_eq!(with_steps(0.1, 0.1).round_price(0.3, Rounding::Down), 0.3);
        assert_eq!(with_steps(0.1, 0.1).round_price(0.3, Rounding::Up), 0.3);
    }

    #[test]
    fn large_step_counts_round_in_the_asked_direction() {
        // About 1e13 steps of 1e-8, and 1e10 steps of 1.
        let cases = [(99999.999999995, 1e-8), (9999999999.995, 1.0)];
        for (value, step) in cases {
            let down = round_to_step(value, step, Rounding::Down);
            let up = round_to_step(value, step, Rounding::Up);
            assert!(down <= value, "{down} > {value}");
            assert!(up >= value, "{up} < {value}");
        }
        assert_eq!(
            round_to_step(99999.999999995, 1e-8, Rounding::Down),
            99999.99999999
        );
        assert_eq!(round_to_step(99999.999999995, 1e-8, Rounding::Up), 100000.0);
        assert_eq!(
            round_to_step(9999999999.995, 1.0, Rounding::Down),
            9999999999.0
        );
        assert_eq!(round_to_step(9999999999.995, 1.0, Rounding::Up), 1e10);
    }

    #[test]
    fn rounded_values_are_the_nearest_float_to_the_decimal() {
        // A naive 3.0 * 0.1 gives 0.30000000000000004.
        assert_eq!(with_steps(0.1, 0.1).round_qty(0.31), 0.3);
        assert_eq!(with_steps(1e-8, 1e-8).round_qty(0.123456789), 0.12345678);
    }

    #[test]
    fn valid_orders_pass() {
        let i = btc_perp();
        assert_eq!(
            i.validate_order(&order(0.001, OrderType::Market), 65000.0),
            Ok(())
        );
        let limit = OrderType::Limit { price: 64000.0 };
        assert_eq!(i.validate_order(&order(0.01, limit), f64::NAN), Ok(()));
    }

    #[test]
    fn qty_below_min_is_rejected() {
        let i = with_steps(0.1, 0.0001);
        let err = i.validate_order(&order(0.0005, OrderType::Market), 65000.0);
        assert!(matches!(err, Err(CoreError::QtyBelowMin { .. })));
    }

    #[test]
    fn qty_off_the_lot_grid_is_rejected() {
        let i = btc_perp();
        let err = i.validate_order(&order(0.0015, OrderType::Market), 65000.0);
        assert!(matches!(err, Err(CoreError::QtyOffLot { .. })));
        // 3.0 * 0.1 is 0.30000000000000004; it must be rounded first.
        let i = with_steps(0.1, 0.1);
        let err = i.validate_order(&order(3.0 * 0.1, OrderType::Market), 65000.0);
        assert!(matches!(err, Err(CoreError::QtyOffLot { .. })));
        let rounded = i.round_qty(3.0 * 0.1);
        assert_eq!(
            i.validate_order(&order(rounded, OrderType::Market), 65000.0),
            Ok(())
        );
    }

    #[test]
    fn prices_off_the_tick_grid_are_rejected() {
        let i = btc_perp();
        let limit = OrderType::Limit { price: 65000.05 };
        let err = i.validate_order(&order(0.01, limit), 65000.0);
        assert!(matches!(
            err,
            Err(CoreError::PriceOffTick { field: "price", .. })
        ));
        let stop = OrderType::Stop { trigger: 65000.05 };
        let err = i.validate_order(&order(0.01, stop), 65000.0);
        assert!(matches!(
            err,
            Err(CoreError::PriceOffTick {
                field: "trigger",
                ..
            })
        ));
        let stop_limit = OrderType::StopLimit {
            trigger: 65000.1,
            price: 65000.05,
        };
        let err = i.validate_order(&order(0.01, stop_limit), 65000.0);
        assert!(matches!(
            err,
            Err(CoreError::PriceOffTick { field: "price", .. })
        ));
        // A last price for a market order is a market observation, not checked against the grid.
        assert_eq!(
            i.validate_order(&order(0.01, OrderType::Market), 65000.05),
            Ok(())
        );
    }

    #[test]
    fn notional_below_min_is_rejected() {
        let i = btc_perp();
        let limit = OrderType::Limit { price: 4000.0 };
        let err = i.validate_order(&order(0.001, limit), 65000.0);
        assert!(matches!(err, Err(CoreError::NotionalBelowMin { .. })));
    }

    #[test]
    fn reduce_only_perpetual_order_is_exempt_from_min_notional() {
        let i = btc_perp();
        let limit = OrderType::Limit { price: 4000.0 };
        let mut o = order(0.001, limit);
        o.reduce_only = true;
        assert!(i.validate_order(&o, 65000.0).is_ok());

        let spot = Instrument {
            id: InstrumentId {
                kind: InstrumentKind::Spot,
                ..i.id.clone()
            },
            ..i
        };
        o.instrument = spot.id.clone();
        let err = spot.validate_order(&o, 65000.0);
        assert!(matches!(err, Err(CoreError::NotionalBelowMin { .. })));
    }

    #[test]
    fn notional_a_hair_under_the_minimum_from_float_error_passes() {
        // 0.57 * 100.0 is 56.99999999999999 in f64.
        let i = Instrument {
            min_notional: 57.0,
            min_qty: 0.01,
            ..with_steps(0.01, 0.01)
        };
        let limit = OrderType::Limit { price: 100.0 };
        assert_eq!(i.validate_order(&order(0.57, limit), 0.0), Ok(()));
    }

    #[test]
    fn non_finite_and_non_positive_values_are_rejected() {
        let i = btc_perp();
        for qty in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            let err = i.validate_order(&order(qty, OrderType::Market), 65000.0);
            assert!(matches!(
                err,
                Err(CoreError::InvalidNumber { field: "qty", .. })
            ));
        }
        let err = i.validate_order(&order(1.0, OrderType::Market), f64::NAN);
        assert!(matches!(
            err,
            Err(CoreError::InvalidNumber {
                field: "last_price",
                ..
            })
        ));
        let stop_limit = OrderType::StopLimit {
            trigger: f64::INFINITY,
            price: 65000.0,
        };
        let err = i.validate_order(&order(1.0, stop_limit), 65000.0);
        assert!(matches!(
            err,
            Err(CoreError::InvalidNumber {
                field: "trigger",
                ..
            })
        ));
    }

    #[test]
    fn order_for_another_instrument_is_rejected() {
        let i = btc_perp();
        let mut o = order(1.0, OrderType::Market);
        o.instrument.kind = InstrumentKind::Spot;
        let err = i.validate_order(&o, 65000.0);
        assert!(matches!(err, Err(CoreError::InstrumentMismatch { .. })));
    }

    #[test]
    fn instrument_id_displays_as_venue_symbol_kind() {
        assert_eq!(btc_perp().id.to_string(), "binance:BTCUSDT:perpetual");
    }

    #[test]
    fn kinds_serialise_and_display_as_their_names() {
        for (kind, name) in [
            (InstrumentKind::Spot, "spot"),
            (InstrumentKind::Perpetual, "perpetual"),
            (InstrumentKind::Fx, "fx"),
        ] {
            assert_eq!(kind.to_string(), name);
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<InstrumentKind>(&json).unwrap(), kind);
        }
        assert!(serde_json::from_str::<InstrumentKind>("\"forex\"").is_err());
    }
}
