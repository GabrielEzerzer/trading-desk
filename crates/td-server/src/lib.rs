//! The engine sidecar the two desktop apps start: a localhost API over the engine, the data store
//! and paper sessions. The API lives in [`api`].
//!
//! A paper session runs a strategy through the same engine [`td_engine::Session`], fill model and
//! simulated broker as a backtest; only the bar source differs: live bars from
//! [`td_data::live`] instead of stored ones.

pub mod api;
mod error;
mod paper;
mod run;

pub use error::PaperError;
pub use paper::{FedBar, Funding, PaperConfig, PaperSession, Status, paper_dir};
pub use run::{PaperHandle, WARM_UP_BARS, start, start_with_stream};
