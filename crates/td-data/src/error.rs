use std::io;
use std::path::PathBuf;

use td_core::{CoreError, InstrumentKind, Millis, Venue};

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

    /// The venue still answered with its rate limit after every retry ([`crate::Http`]).
    #[error(
        "{url} still answered with its rate limit after {tries} tries and {:.1} s of waiting: {answer}",
        .waited.as_secs_f64()
    )]
    RateLimited {
        url: String,
        tries: u32,
        waited: std::time::Duration,
        answer: String,
    },

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

    #[error("{0} has bid and ask prices, so its candles are written with their asks")]
    QuotesRequired(String),

    #[error("{0} has no ask prices, so it takes no asks")]
    NoQuotes(String),

    #[error("{instrument}: the bids and asks do not cover the same minutes: {reason}")]
    QuoteMinutes { instrument: String, reason: String },

    #[error(
        "{instrument} at {} UTC: the ask {field} {ask} is below the bid {field} {bid}",
        crate::time::format_minute(*.open_time)
    )]
    CrossedQuote {
        instrument: String,
        open_time: Millis,
        field: &'static str,
        bid: f64,
        ask: f64,
    },

    #[error(
        "{instrument} at {} UTC: the market is closed then, so no candle may open there",
        crate::time::format_minute(*.open_time)
    )]
    ClosedMinute {
        instrument: String,
        open_time: Millis,
    },

    #[error(
        "{}: the month has no ask prices; delete it and download it again",
        .path.display()
    )]
    NoAskColumns { path: PathBuf },

    #[error("{instrument} {month}: the source returned bids without asks")]
    NoAsks { instrument: String, month: String },

    #[error("{venue} does not list {kind} instruments")]
    KindNotListed { venue: Venue, kind: InstrumentKind },

    #[error("{0} publishes no funding rates")]
    NoFunding(Venue),

    #[error("{0} has no live stream")]
    NoLiveStream(Venue),

    #[error("{0:?} is not a date in the form YYYY-MM-DD")]
    InvalidDate(String),

    #[error("the range {from} to {to} is empty: --to must be after --from")]
    EmptyRange { from: String, to: String },

    #[error(
        "{0} is being downloaded or deleted right now, in this app or another program on the same data folder; delete it again once that has finished"
    )]
    InstrumentBusy(String),

    #[error("no data directory: set TD_DATA_DIR")]
    NoDataDir,

    /// Names the variable, never a value.
    #[error(
        "oanda's history needs an API token: set OANDA_API_TOKEN to the token of an OANDA \
         practice account"
    )]
    MissingOandaToken,

    /// OANDA answered 401 or 403. `message` is OANDA's `errorMessage` alone, with any copy of the
    /// token taken out; nothing of the request is kept.
    #[error(
        "{url} was refused ({status}: {message}): the token in OANDA_API_TOKEN is not valid, or \
         is not a practice account's token"
    )]
    OandaRefused {
        url: String,
        status: u16,
        message: String,
    },

    #[error(
        "alpaca's market data needs API keys: set APCA_API_KEY_ID and APCA_API_SECRET_KEY to the \
         keys of an Alpaca account"
    )]
    MissingAlpacaKeys,

    #[error(
        "{url} was refused ({status}: {message}): the keys in APCA_API_KEY_ID and \
         APCA_API_SECRET_KEY are not valid, or their plan cannot read this data"
    )]
    AlpacaRefused {
        url: String,
        status: u16,
        message: String,
    },

    #[error(
        "{instrument} is stored with price adjustment {stored}, but its source gives {wanted}: \
         delete its stored data and download it again"
    )]
    AdjustmentMismatch {
        instrument: String,
        stored: String,
        wanted: String,
    },

    #[error(
        "{instrument}'s adjusted prices changed for {date} since they were stored, after a split \
         or dividend: delete its stored data and download it again so every bar shares one basis"
    )]
    AdjustmentChanged { instrument: String, date: String },
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
