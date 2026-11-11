//! Shared types for Trading Desk: venues, instruments and their market rules, timeframes, candles,
//! orders and fills.
//!
//! Conventions every crate follows:
//! - Prices and quantities are `f64`. They are rounded to the instrument's tick and lot size when an
//!   order is submitted ([`Instrument::round_price`], [`Instrument::round_qty`]).
//! - Timestamps are [`Millis`]: milliseconds since the Unix epoch, UTC.

mod candle;
mod civil;
mod error;
mod instrument;
mod market;
mod order;
mod timeframe;

pub use candle::Candle;
pub use civil::{civil_from_days, days_from_civil};
pub use error::CoreError;
pub use instrument::{FeeSchedule, Instrument, InstrumentId, InstrumentKind, Rounding, Venue};
pub use market::{AccountCurrency, Hours, MarketRules};
pub use order::{Fill, Liquidity, OrderId, OrderRequest, OrderType, Side, TimeInForce};
pub use timeframe::Timeframe;

/// Milliseconds since the Unix epoch, UTC.
pub type Millis = i64;
