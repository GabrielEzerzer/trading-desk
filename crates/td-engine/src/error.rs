use td_core::{CoreError, InstrumentKind, Millis, OrderId, Timeframe};

/// Errors from building a session or strategy and from feeding it data.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Core(#[from] CoreError),

    #[error("bar at {time} was already fed to this session")]
    DuplicateBar { time: Millis },

    #[error("bar at {got} is earlier than the previous bar at {prev}")]
    OutOfOrderBar { prev: Millis, got: Millis },

    #[error("bar open time {time} is not on a {timeframe} boundary")]
    MisalignedBar { time: Millis, timeframe: Timeframe },

    #[error("funding at {time} is outside the bar that opens at {bar_open} and the gap before it")]
    FundingOutsideBar { time: Millis, bar_open: Millis },

    #[error("funding rate {rate} at {time} is not a finite number")]
    InvalidFundingRate { time: Millis, rate: f64 },

    /// Funding given for a kind whose rules have none (spot, FX). The name is older than FX.
    #[error("funding was given for a market that has no funding")]
    FundingOnSpot,

    #[error("asks were given for a {kind} instrument, which has no separate ask price")]
    AsksWithoutQuotes { kind: InstrumentKind },

    #[error("no ask for the bar at {time}: a {kind} instrument needs one for every bar")]
    MissingAsk { time: Millis, kind: InstrumentKind },

    #[error("the ask at {time} matches no bar")]
    UnmatchedAsk { time: Millis },

    #[error("more than one ask opens at {time}")]
    DuplicateAsk { time: Millis },

    /// An ask whose open, high, low or close is below the bid's: real quotes never cross, so the
    /// series are wrong, for example swapped. Equal is a zero spread and allowed.
    #[error("the ask for the bar at {time} is below the bid")]
    CrossedQuote { time: Millis },

    #[error("bar at {time} lies wholly in the market's closed hours")]
    ClosedBar { time: Millis },

    #[error("warm-up bars must all come before the first live bar")]
    WarmUpAfterLive,

    #[error("no bars open between {from} and {to}")]
    NoBarsInRange { from: Millis, to: Millis },

    #[error("invalid session config: {0}")]
    InvalidConfig(&'static str),

    #[error("unknown strategy {0:?}")]
    UnknownStrategy(String),

    #[error("parameter {name:?}: {reason}")]
    InvalidParam { name: String, reason: String },
}

/// An order the engine refused at submission. The order is still recorded, with status
/// `Rejected`, under `id`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("order {} rejected: {reason}", id.0)]
pub struct OrderRejected {
    pub id: OrderId,
    pub reason: String,
}
