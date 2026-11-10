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

    #[error("quantity {qty} is below the minimum {min}")]
    QtyBelowMin { qty: f64, min: f64 },

    #[error("notional {notional} is below the minimum {min}")]
    NotionalBelowMin { notional: f64, min: f64 },
}
