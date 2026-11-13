//! Market data: instrument metadata, history download, the local Parquet candle store,
//! resampling to higher timeframes, and live candle feeds ([`live`]). What differs between data
//! sources (Binance, Bybit, OANDA) sits behind [`DataSource`]; [`source()`] maps a venue to its source.
//!
//! The engine reads candles as [`LoadedCandles`] (the bars, and the buckets dropped as incomplete
//! or built with a few minutes missing) and funding as `Vec<(Millis, f64)>` from [`Store`].

pub mod binance;
pub mod bybit;
mod closed;
mod download;
mod error;
mod http;
pub mod instruments;
pub mod live;
pub mod oanda;
mod resample;
mod source;
mod store;
pub mod time;

pub use closed::{Closures, LONGEST_CLOSURE};
pub use download::{DownloadReport, download_candles, download_funding};
pub use error::DataError;
pub use http::Http;
pub use instruments::{
    INSTRUMENT_CACHE_AGE, InstrumentList, cached_instruments, default_fees, fetch_instruments,
    validate_instrument,
};
pub use resample::{LoadedCandles, resample};
pub use source::{DataSource, LiveStream, MinuteHistory, source};
pub use store::{
    DeleteReport, Gap, MonthCoverage, MonthQuotes, Store, StoredData, StoredInstrument,
    StoredMonth, WriteReport, data_dir, missing_minutes,
};
