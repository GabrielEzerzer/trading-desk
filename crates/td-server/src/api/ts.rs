//! The TypeScript declarations of every type the API sends or takes, for the GUI. Written to
//! `gui/shared/src/api-types.ts` by `cargo run -p td-server --bin gen-api-types`; CI fails when
//! the committed file is out of date.

use std::path::PathBuf;

use ts_rs::{Config, TS};

use super::Handshake;
use super::HealthResponse;
use super::StrategyInfo;
use super::candles::{BarSpan, CandlesResponse};
use super::data::{
    CoverageResponse, DeleteRequest, DeleteResult, DownloadRequest, DownloadResult,
    FundingCoverage, GapInfo, InstrumentsResponse, MonthCount, MonthCoverageInfo,
    RejectedInstrument, StoredInstrumentInfo, StoredMonthInfo, StoredResponse,
};
use super::error::{ErrorBody, ErrorResponse};
use super::jobs::{JobCreated, JobInfo, JobKind, JobRequest, JobResult, JobStatus};
use super::runs::{DataReport, RunCheck, RunDetail, RunDone, ToleratedBucket};
use super::sessions::{PositionInfo, SessionState, SessionSummary, StartSessionRequest};
use super::ws::{ClientMessage, ServerMessage};
use crate::{
    Funding, ManualOrder, ManualOrderType, OrderSource, PaperConfig, PaperFill, PaperOrder,
    PaperTrade, SessionEvent, Status, TradeSource,
};

/// Where the declarations live, from the repository root.
pub const OUTPUT: &str = "gui/shared/src/api-types.ts";

/// The output file's absolute path.
pub fn output_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(OUTPUT)
}

/// The whole file. 64-bit integers (timestamps, job ids) are `number`: JSON.parse reads them as
/// numbers, and every value the API sends fits in 2^53.
pub fn render() -> String {
    let cfg = Config::new().with_large_int("number");
    let mut out = String::from(
        "// Generated from the Rust API types by `cargo run -p td-server --bin gen-api-types`.\n\
         // Do not edit by hand; CI fails when this file is out of date.\n",
    );
    macro_rules! declare {
        ($($ty:ty),* $(,)?) => {$(
            out.push('\n');
            if let Some(docs) = <$ty as TS>::docs() {
                out.push_str(&docs);
            }
            out.push_str("export ");
            out.push_str(&<$ty as TS>::decl(&cfg));
            out.push('\n');
        )*};
    }
    declare!(
        // td-core and td-engine
        td_core::Venue,
        td_core::InstrumentKind,
        td_core::InstrumentId,
        td_core::FeeSchedule,
        td_core::Instrument,
        td_core::Timeframe,
        td_core::Candle,
        td_core::Side,
        td_core::OrderType,
        td_core::TimeInForce,
        td_core::OrderId,
        td_core::OrderRequest,
        td_core::Liquidity,
        td_core::Fill,
        td_core::AccountCurrency,
        td_engine::ParamKind,
        td_engine::ParamValue,
        td_engine::ParamSpec,
        td_engine::Params,
        td_engine::BacktestConfig,
        td_engine::BacktestResult,
        td_engine::Benchmark,
        td_engine::EquityPoint,
        td_engine::EquityRange,
        td_engine::Order,
        td_engine::AttachedStop,
        td_engine::OrderStatus,
        td_engine::Trade,
        td_engine::TradeSide,
        td_engine::MetricsReport,
        td_engine::ParamSpace,
        td_engine::ParamAxis,
        td_engine::SweepMode,
        td_engine::Metric,
        td_engine::Direction,
        td_engine::Objective,
        td_engine::SweepSpec,
        td_engine::Headline,
        td_engine::SweepRow,
        td_engine::SweepResult,
        td_engine::Split,
        td_engine::OosSpec,
        td_engine::OosResult,
        td_engine::WindowMode,
        td_engine::WindowSpec,
        td_engine::WalkForwardSpec,
        td_engine::WindowResult,
        td_engine::WalkForwardResult,
        td_engine::RunId,
        td_engine::RunKind,
        td_engine::SavedRun,
        td_engine::RunSummary,
        td_engine::TestPlan,
        td_engine::Period,
        td_engine::BacktestSettings,
        td_engine::SlippageOverride,
        td_engine::SweepPlan,
        td_engine::WalkForwardPlan,
        td_engine::EvidenceSettings,
        td_engine::MinSample,
        td_engine::LuckTest,
        td_engine::Plateau,
        td_engine::RuleKind,
        td_engine::VerdictRule,
        td_engine::FirmLimits,
        td_engine::DrawdownMode,
        td_engine::PropFirmLimits,
        td_engine::PortfolioBenchmark,
        td_engine::PortfolioLimits,
        td_engine::HighRiskLimits,
        td_engine::Layer,
        td_engine::Comparison,
        td_engine::Unit,
        td_engine::Outcome,
        td_engine::Criterion,
        td_engine::VerdictState,
        td_engine::Verdict,
        // the API
        Handshake,
        ErrorResponse,
        ErrorBody,
        HealthResponse,
        InstrumentsResponse,
        RejectedInstrument,
        DownloadRequest,
        DownloadResult,
        MonthCount,
        GapInfo,
        CoverageResponse,
        MonthCoverageInfo,
        FundingCoverage,
        StoredResponse,
        StoredInstrumentInfo,
        StoredMonthInfo,
        DeleteRequest,
        DeleteResult,
        CandlesResponse,
        BarSpan,
        StrategyInfo,
        JobCreated,
        JobInfo,
        JobKind,
        JobStatus,
        JobRequest,
        JobResult,
        RunDone,
        RunDetail,
        RunCheck,
        DataReport,
        ToleratedBucket,
        PaperConfig,
        Funding,
        Status,
        StartSessionRequest,
        SessionSummary,
        PositionInfo,
        OrderSource,
        PaperOrder,
        PaperFill,
        TradeSource,
        PaperTrade,
        ManualOrder,
        ManualOrderType,
        SessionState,
        SessionEvent,
        ClientMessage,
        ServerMessage,
    );
    out
}
