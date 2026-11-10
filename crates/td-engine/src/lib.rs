//! The trading engine: the `Strategy` trait, the event-driven simulation on candles, the fill
//! model, the broker and the portfolio.
//!
//! The engine is push-driven. A [`Session`] is fed one closed bar at a time with
//! [`Session::on_bar`]; a backtest ([`run_backtest`]) is a loop that feeds stored bars, and paper
//! trading feeds live bars into the same session type, so the strategy code under test is the code
//! that later trades.
//!
//! Each bar runs in this order: funding due at the bar's open is charged on the position held going
//! into it, pending orders are checked against the new bar, funding due later in the bar is
//! charged, equity is marked at the close, then the strategy sees the bar. An order the strategy
//! submits while handling bar t can first fill on bar t+1, and the strategy only ever sees bars up
//! to the current one, because the session does not have later bars.
//!
//! Before the first live bar, [`Session::warm_up`] can feed bars that only give the strategy the
//! history its indicators need: the strategy sees them as usual, but cannot trade on them.

mod broker;
mod error;
mod fill;
mod params;
mod portfolio;
mod registry;
mod session;
mod strategies;
mod strategy;

pub use broker::{AttachedStop, Broker, BrokerEvent, Order, OrderStatus, SimBroker};
pub use error::{EngineError, OrderRejected};
pub use fill::{CandleFillModel, FillCheck, FillModel};
pub use params::{ParamKind, ParamSpec, ParamValue, Params};
pub use portfolio::Portfolio;
pub use registry::{StrategyEntry, build_strategy, find_strategy, strategies};
pub use session::{EquityPoint, Session, SessionConfig, run_backtest};
pub use strategy::{Context, Strategy};
