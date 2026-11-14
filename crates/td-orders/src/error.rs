use std::fmt;

use td_core::CoreError;

/// Which request an unknown outcome belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Place,
    Cancel,
    Amend,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Action::Place => "place",
            Action::Cancel => "cancel",
            Action::Amend => "amend",
        })
    }
}

/// Errors from the order route.
///
/// No variant holds the API key, the secret, a signature or a raw response body: a venue message
/// is kept only after [`crate::DemoKeys`] has checked it carries none of them, and a body that
/// does not parse is described by where it failed, not by what it held.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Error {
    #[error("refused base URL: {reason}")]
    HostRefused { reason: &'static str },

    #[error("{name} is not set")]
    MissingKey { name: &'static str },

    #[error("{name} is empty or holds characters that cannot go in a header")]
    InvalidKey { name: &'static str },

    #[error("keys made for the loopback mock cannot be used with the demo service")]
    LoopbackKeysOnDemo,

    #[error("keys for the demo service cannot be sent to a loopback port")]
    DemoKeysOnLoopback,

    #[error("invalid timing: {reason}")]
    InvalidTiming { reason: &'static str },

    #[error("invalid client order id: {reason}")]
    InvalidClientId { reason: &'static str },

    #[error("client order id {id} was already sent by this client; use a new id")]
    ClientIdAlreadyUsed { id: String },

    #[error("order refused before sending: {0}")]
    Filter(#[from] CoreError),

    #[error("not supported by this route: {what}")]
    Unsupported { what: &'static str },

    #[error("a stop at {trigger} would trigger at once against the last price {last_price}")]
    StopWouldTrigger { trigger: f64, last_price: f64 },

    #[error("the account is in hedge mode on {symbol}; this route sends one-way orders only")]
    HedgeMode { symbol: String },

    #[error("the venue rejected the request: {code} {message}")]
    Rejected { code: i64, message: String },

    #[error("the venue already holds an order with client order id {id}")]
    DuplicateClientId { id: String },

    #[error("the venue holds no order with client order id {id}")]
    OrderNotFound { id: String },

    #[error("order {id} is no longer open (status {status})")]
    OrderClosed { id: String, status: String },

    /// An order whose request's answer was lost, found on the venue with status `Rejected` before
    /// any resend. Found after a resend, it is [`Error::OutcomeUnknown`] instead.
    #[error("the venue lists order {id} as rejected (reason {reason})")]
    OrderRejected { id: String, reason: String },

    #[error("the outcome of the {action} request for {id} is unknown: {reason}")]
    OutcomeUnknown {
        action: Action,
        id: String,
        reason: String,
    },

    #[error("rate limited on {endpoint}{}", if *.ip_block { " (IP blocked; wait at least 10 minutes)" } else { "" })]
    RateLimited {
        endpoint: &'static str,
        ip_block: bool,
    },

    #[error("the venue rejected the request timestamp after a clock resync: {code} {message}")]
    TimestampRejected { code: i64, message: String },

    #[error("the venue's server time is more than a day from the local clock")]
    ServerTimeOutOfRange,

    #[error(
        "{endpoint} answered with more than {} bytes",
        crate::client::MAX_BODY_BYTES
    )]
    ResponseTooLarge { endpoint: &'static str },

    #[error("request to {endpoint} failed: {reason}")]
    Transport {
        endpoint: &'static str,
        reason: String,
    },

    #[error("{endpoint} answered HTTP {status}")]
    HttpStatus { endpoint: &'static str, status: u16 },

    #[error("unexpected response from {endpoint}: {reason}")]
    BadResponse {
        endpoint: &'static str,
        reason: String,
    },
}
