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

    /// `place` was given an id this client has already sent and not freed. The id stays spent
    /// whatever the earlier send's result was. When that result was [`Error::OutcomeUnknown`],
    /// the order may be live: read it with [`crate::Client::order_by_client_id`], and never send
    /// it again under a new id. A new id is only for a new order.
    #[error(
        "client order id {id} was already sent by this client; read that order with \
         order_by_client_id, and use a new id only for a new order"
    )]
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

    /// A create whose answer left its outcome open, found on the venue with status `Rejected`.
    /// Every send of the create carried the same order and id, so the record is this order's.
    #[error("the venue lists order {id} as rejected (reason {reason})")]
    OrderRejected { id: String, reason: String },

    /// The venue may or may not have applied the request, and asking it did not settle which.
    ///
    /// After a `place`, the order may be live and the id stays spent. The caller:
    /// - keeps the unresolved order and its original id, across a restart too;
    /// - keeps asking the venue about that id (`Client::order_by_client_id`, the executions)
    ///   until it shows the order or the order's fate is otherwise settled;
    /// - counts the order's possible fill in its exposure meanwhile;
    /// - never takes an empty lookup as permission to send the same order again under another
    ///   id: the venue's lists can lag, and an empty answer does not prove the order was not
    ///   placed.
    ///
    /// A timeout or a spent time budget does not withdraw the request; the venue may still apply
    /// it. After a `cancel` or `amend_stop`, read the order before acting on it.
    #[error("the outcome of the {action} request for {id} is unknown: {reason}")]
    OutcomeUnknown {
        action: Action,
        id: String,
        reason: String,
    },

    /// From `place`, the venue did not take the order and the id is free again; a retry goes
    /// under the same id ([`crate::Client::place`]).
    #[error("rate limited on {endpoint}{}", if *.ip_block { " (IP blocked; wait at least 10 minutes)" } else { "" })]
    RateLimited {
        endpoint: &'static str,
        ip_block: bool,
    },

    /// From `place`, the venue did not take the order and the id is free again; a retry goes
    /// under the same id ([`crate::Client::place`]).
    #[error("the venue rejected the request timestamp after a clock resync: {code} {message}")]
    TimestampRejected { code: i64, message: String },

    #[error("the venue's server time is more than a day from the local clock")]
    ServerTimeOutOfRange,

    #[error(
        "{endpoint} answered with more than {} bytes",
        crate::client::MAX_BODY_BYTES
    )]
    ResponseTooLarge { endpoint: &'static str },

    /// From `place`, the venue did not take the order and the id is free again; a retry goes
    /// under the same id ([`crate::Client::place`]).
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
