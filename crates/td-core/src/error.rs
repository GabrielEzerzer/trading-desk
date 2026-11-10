use crate::{InstrumentId, Millis};

/// Errors from parsing and validating the core types.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CoreError {
    #[error("unknown timeframe {0:?}")]
    UnknownTimeframe(String),

    #[error("invalid candle at {open_time}: {reason}")]
    InvalidCandle {
        open_time: Millis,
        reason: &'static str,
    },

    #[error("order is for {order}, but the instrument is {instrument}")]
    InstrumentMismatch {
        order: InstrumentId,
        instrument: InstrumentId,
    },

    #[error("{field} must be a finite number above zero, got {value}")]
    InvalidNumber { field: &'static str, value: f64 },

    #[error("quantity {qty} is not a multiple of the lot size {lot_size}")]
    QtyOffLot { qty: f64, lot_size: f64 },

    #[error("{field} {price} is not a multiple of the tick size {tick_size}")]
    PriceOffTick {
        field: &'static str,
        price: f64,
        tick_size: f64,
    },

    #[error("quantity {qty} is below the minimum {min}")]
    QtyBelowMin { qty: f64, min: f64 },

    #[error("notional {notional} is below the minimum {min}")]
    NotionalBelowMin { notional: f64, min: f64 },
}
