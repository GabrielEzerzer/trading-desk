use td_core::{CoreError, Millis, OrderId, Timeframe};

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

    #[error("funding at {time} is outside the bar that opens at {bar_open}")]
    FundingOutsideBar { time: Millis, bar_open: Millis },

    #[error("funding rate {rate} at {time} is not a finite number")]
    InvalidFundingRate { time: Millis, rate: f64 },

    #[error("funding was given for a spot instrument")]
    FundingOnSpot,

    #[error("warm-up bars must all come before the first live bar")]
    WarmUpAfterLive,

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
