//! The local API server that the two desktop apps talk to over localhost. For now it holds paper
//! sessions; the API that drives them comes with #16.
//!
//! A paper session runs a strategy through the same engine [`td_engine::Session`], fill model and
//! simulated broker as a backtest; only the bar source differs: live bars from
//! [`td_data::live`] instead of stored ones.

mod error;
mod paper;
mod run;

pub use error::PaperError;
pub use paper::{FedBar, Funding, PaperConfig, PaperSession, Status, paper_dir};
pub use run::{PaperHandle, WARM_UP_BARS, start, start_with_stream};
