use std::io;
use std::path::PathBuf;

use td_core::CoreError;

/// Errors from downloading, parsing and storing market data.
#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("{url}: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("{url} answered {status}: {body}")]
    Status {
        url: String,
        status: u16,
        body: String,
    },

    #[error("{url} returned an error: {message}")]
    Api { url: String, message: String },

    #[error("cannot parse {what}: {reason}")]
    Parse { what: String, reason: String },

    #[error("{file}: its SHA-256 is {actual}, but the published .CHECKSUM says {expected}")]
    ChecksumMismatch {
        file: String,
        expected: String,
        actual: String,
    },

    #[error("{file}: {reason}")]
    Archive { file: String, reason: String },

    #[error("{}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{}: {source}", .path.display())]
    Parquet {
        path: PathBuf,
        #[source]
        source: parquet::errors::ParquetError,
    },

    #[error("{}: {reason}", .path.display())]
    BadFile { path: PathBuf, reason: String },

    #[error("{instrument}: {source}")]
    Candle {
        instrument: String,
        #[source]
        source: CoreError,
    },

    #[error("instrument {symbol}: {reason}")]
    InvalidInstrument { symbol: String, reason: String },

    #[error("symbol {0:?} cannot name a folder: use letters, digits, '-' and '_' only")]
    InvalidSymbol(String),

    #[error("{0} is not a perpetual, so it has no funding")]
    NotPerpetual(String),

    #[error("{0:?} is not a date in the form YYYY-MM-DD")]
    InvalidDate(String),

    #[error("the range {from} to {to} is empty: --to must be after --from")]
    EmptyRange { from: String, to: String },

    #[error("no data directory: set TD_DATA_DIR")]
    NoDataDir,
}

impl DataError {
    pub(crate) fn parse(what: impl Into<String>, reason: impl ToString) -> Self {
        DataError::Parse {
            what: what.into(),
            reason: reason.to_string(),
        }
    }

    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |source| DataError::Io { path, source }
    }
}
