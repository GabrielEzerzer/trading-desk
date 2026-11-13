//! The trading engine: the `Strategy` trait, the event-driven simulation on candles, the fill
//! model, the broker and the portfolio, the backtest runner with its metrics and saved results,
//! the strategy tester (sweeps, out-of-sample, walk-forward and the luck test by bar permutation),
//! and the seeded resampling statistics of the test battery ([`stats`]).
//!
//! The engine is push-driven. A [`Session`] is fed one closed bar at a time with
//! [`Session::on_bar`]; a backtest ([`run_backtest`]) is a loop that feeds stored bars, and paper
//! trading feeds live bars into the same session type, so the strategy code under test is the code
//! that later trades.
//!
//! Each bar runs in this order: funding due at the bar's open, or in a gap before it, is charged on
//! the position held going into it, pending orders are checked against the new bar, a perpetual
//! position is checked for liquidation after the fills at the open and again after all of them,
//! funding due later in the bar is charged, equity is marked at the close, then the strategy sees
//! the bar. An
//! order the strategy submits while handling bar t can first fill on bar t+1, and the strategy only
//! ever sees bars up to the current one, because the session does not have later bars.
//!
//! Before the first live bar, [`Session::warm_up`] can feed bars that only give the strategy the
//! history its indicators need: the strategy sees them as usual, but cannot trade on them.

mod battery;
mod broker;
mod error;
mod fill;
pub mod metrics;
mod params;
mod permute;
mod plan;
mod portfolio;
mod registry;
mod results;
pub mod risk;
mod rng;
mod runner;
mod session;
pub mod stats;
mod strategies;
mod strategy;
mod tester;
mod trades;
mod verdict;

/// build.rs's generator for the private strategies, compiled here for its tests.
#[cfg(test)]
#[path = "../build/private_strategies.rs"]
mod private_strategies_build;

pub use battery::{
    BatteryResult, BatteryRuns, BatterySpec, Breakdown, CostStress, IntegrityReport,
    PlateauDirection, PlateauReport, RegimeResult, SideResult, StartOffset, battery_runs,
    run_battery,
};
pub use broker::{AttachedStop, Broker, BrokerEvent, Order, OrderStatus, SimBroker};
pub use error::{EngineError, OrderRejected};
pub use fill::{CandleFillModel, FillCheck, FillModel};
pub use metrics::MetricsReport;
pub use params::{ParamKind, ParamSpec, ParamValue, Params};
pub use permute::permute_bars;
pub use plan::{
    BacktestSettings, EvidenceSettings, LuckTest, MIN_PERMUTATIONS, MIN_PROFITABLE_WINDOWS,
    MinSample, Period, PlanError, Plateau, SlippageOverride, SweepPlan, TestPlan, WalkForwardPlan,
    date,
};
pub use portfolio::Portfolio;
pub use registry::{StrategyEntry, build_strategy, code_hash, find_strategy, strategies};
pub use results::{
    PlanRun, RunId, RunKind, RunSummary, SavedRun, StoreError, list, load, save, save_with_files,
};
pub use risk::{DayRollover, EquityRange};
pub use runner::{
    BacktestConfig, BacktestResult, Benchmark, ENGINE_REVISION, Rerun, config_fingerprint,
    data_fingerprint, default_slippage_bps, rerun, run, run_strategy,
};
pub use session::{
    CLOSE_OUT_TAG, EquityPoint, LIQUIDATION_TAG, MAINTENANCE_MARGIN_RATE, Session, SessionConfig,
    run_backtest,
};
pub use strategies::gotobi_calendar;
pub use strategy::{Context, Strategy};
pub use tester::{
    Direction, Headline, MAX_RUNS, Metric, Objective, OosResult, OosSpec, ParamAxis, ParamSpace,
    PermutationResult, PermutationSpec, Progress, Split, SweepMode, SweepResult, SweepRow,
    SweepSpec, TesterError, WalkForwardResult, WalkForwardSpec, Window, WindowMode, WindowResult,
    WindowSpec, out_of_sample, permutation_test, stitch, sweep, walk_forward,
};
pub use trades::{
    BarFunding, Trade, TradeSide, TradeStops, attach_stops, build_trades, build_trades_with_fills,
};
pub use verdict::{
    Comparison, Criterion, DrawdownMode, EvidenceInputs, FirmLimits, HighRiskLimits,
    IntegrityInputs, KindInputs, Layer, LoosenedLimit, MAX_DROPPED_SHARE, OosIntegrityInputs,
    Outcome, PortfolioBenchmark, PortfolioLimits, PropFirmLimits, RuleKind, TOO_GOOD_SHARPE, Unit,
    Verdict, VerdictRule, VerdictState, combine, evaluate, evaluate_evidence, evaluate_integrity,
    evaluate_oos_integrity,
};
