//! The engine sidecar the two desktop apps start: a localhost API over the engine, the data store
//! and paper sessions. The API lives in [`api`].
//!
//! A paper session runs a strategy through the same engine [`td_engine::Session`], fill model and
//! simulated broker as a backtest; only the bar source differs: live bars from
//! [`td_data::live`] instead of stored ones. Manual orders placed over the API take the same path
//! between bars.

pub mod api;
mod error;
mod paper;
mod run;

pub use error::{ManualError, PaperError};
pub use paper::{
    FedBar, Funding, MANUAL_STRATEGY, ManualOrder, ManualOrderType, OrderSource, PaperConfig,
    PaperFill, PaperOrder, PaperSession, PaperTrade, SessionEvent, Status, TradeSource, paper_dir,
};
pub use run::{PaperHandle, WARM_UP_BARS, start, start_on, start_with_stream};
