//! The trading engine: the `Strategy` trait, the event-driven simulation on candles, the fill
//! model, the broker and the portfolio, the backtest runner with its metrics and saved results,
//! and the strategy tester (sweeps, out-of-sample and walk-forward).
//!
//! The engine is push-driven. A [`Session`] is fed one closed bar at a time with
//! [`Session::on_bar`]; a backtest ([`run_backtest`]) is a loop that feeds stored bars, and paper
//! trading feeds live bars into the same session type, so the strategy code under test is the code
//! that later trades.
//!
//! Each bar runs in this order: pending orders are checked against the new bar, funding is
//! charged, equity is marked at the close, then the strategy sees the bar. An order the strategy
//! submits while handling bar t can first fill on bar t+1, and the strategy only ever sees bars up
//! to the current one, because the session does not have later bars.

mod broker;
mod error;
mod fill;
pub mod metrics;
mod params;
mod portfolio;
mod registry;
mod results;
mod runner;
mod session;
mod strategies;
mod strategy;
mod tester;
mod trades;

pub use broker::{Broker, BrokerEvent, Order, OrderStatus, SimBroker};
pub use error::{EngineError, OrderRejected};
pub use fill::{CandleFillModel, FillCheck, FillModel};
pub use metrics::MetricsReport;
pub use params::{ParamKind, ParamSpec, ParamValue, Params};
pub use portfolio::Portfolio;
pub use registry::{StrategyEntry, build_strategy, find_strategy, strategies};
pub use results::{RunId, RunKind, RunSummary, SavedRun, StoreError, list, load, save};
pub use runner::{BacktestConfig, BacktestResult, Benchmark, Rerun, data_fingerprint, rerun, run};
pub use session::{EquityPoint, Session, SessionConfig, run_backtest};
pub use strategy::{Context, Strategy};
pub use tester::{
    Direction, Headline, MAX_RUNS, Metric, Objective, OosResult, OosSpec, ParamAxis, ParamSpace,
    Progress, Split, SweepMode, SweepResult, SweepRow, SweepSpec, TesterError, WalkForwardResult,
    WalkForwardSpec, Window, WindowMode, WindowResult, WindowSpec, out_of_sample, stitch, sweep,
    walk_forward,
};
pub use trades::{Trade, TradeSide, build_trades};
