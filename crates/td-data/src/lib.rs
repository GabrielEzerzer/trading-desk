//! Market data: instrument metadata, history download from Binance and Bybit, the local Parquet
//! candle store, resampling to higher timeframes, and live candle feeds ([`live`]).
//!
//! The engine reads candles as [`LoadedCandles`] (the bars, and the buckets dropped as incomplete
//! or built with a few minutes missing) and funding as `Vec<(Millis, f64)>` from [`Store`].

pub mod binance;
pub mod bybit;
mod download;
mod error;
mod http;
pub mod instruments;
pub mod live;
mod resample;
mod store;
pub mod time;

pub use download::{DownloadReport, download_candles, download_funding};
pub use error::DataError;
pub use http::Http;
pub use instruments::{InstrumentList, default_fees, fetch_instruments, validate_instrument};
pub use resample::{LoadedCandles, resample};
pub use store::{Gap, MonthCoverage, Store, WriteReport, data_dir, missing_minutes};
