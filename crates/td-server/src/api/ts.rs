//! The TypeScript declarations of every type the API sends or takes, for the GUI. Written to
//! `gui/shared/src/api-types.ts` by `cargo run -p td-server --bin gen-api-types`; CI fails when
//! the committed file is out of date.

use std::path::PathBuf;

use ts_rs::{Config, TS};

use super::Handshake;
use super::HealthResponse;
use super::StrategyInfo;
use super::data::{
    CoverageResponse, DownloadRequest, DownloadResult, FundingCoverage, GapInfo,
    InstrumentsResponse, MonthCount, MonthCoverageInfo, RejectedInstrument,
};
use super::error::{ErrorBody, ErrorResponse};
use super::jobs::{JobCreated, JobInfo, JobKind, JobRequest, JobResult, JobStatus};
use super::ws::{ClientMessage, ServerMessage};

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
        td_engine::ParamKind,
        td_engine::ParamValue,
        td_engine::ParamSpec,
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
        StrategyInfo,
        JobCreated,
        JobInfo,
        JobKind,
        JobStatus,
        JobRequest,
        JobResult,
        ClientMessage,
        ServerMessage,
    );
    out
}
