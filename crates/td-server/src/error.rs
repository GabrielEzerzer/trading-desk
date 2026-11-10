use std::io;
use std::path::PathBuf;

use td_data::DataError;
use td_engine::EngineError;

/// Errors from creating, resuming and running a paper session.
#[derive(Debug, thiserror::Error)]
pub enum PaperError {
    #[error(transparent)]
    Engine(#[from] EngineError),

    #[error(transparent)]
    Data(#[from] DataError),

    #[error("{}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{}: {reason}", .path.display())]
    BadFile { path: PathBuf, reason: String },

    #[error("session id {0:?} must be 1 to 64 letters, digits, '-' or '_'")]
    InvalidId(String),

    #[error("a paper session already exists in {}", .0.display())]
    Exists(PathBuf),

    #[error(
        "the session was created with engine revision {created} and this engine is revision \
         {current}, so it cannot be resumed: start a new session"
    )]
    EngineChanged { created: u32, current: u32 },

    #[error("the live feed stopped")]
    FeedEnded,

    #[error("a background task failed: {0}")]
    Task(String),

    #[error("the session's log could not be written, so it takes no more bars or orders: {0}")]
    LogFailed(String),
}

/// Why a manual order or cancel was refused.
#[derive(Debug, thiserror::Error)]
pub enum ManualError {
    #[error("the session has not fed its first live bar yet; orders can be placed once it has")]
    NotLive,

    /// The engine's reason, or a request it cannot take.
    #[error("{0}")]
    Rejected(String),

    #[error("no order {0} in this session")]
    NoSuchOrder(u64),

    #[error("order {0} was placed by the strategy; only manual orders can be cancelled")]
    NotManual(u64),

    #[error("order {id} is {state}, so it cannot be cancelled")]
    Done { id: u64, state: &'static str },

    #[error(transparent)]
    Paper(#[from] PaperError),
}

impl PaperError {
    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |source| PaperError::Io { path, source }
    }
}
