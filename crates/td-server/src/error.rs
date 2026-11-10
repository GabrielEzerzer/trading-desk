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

    #[error("the live feed stopped")]
    FeedEnded,

    #[error("a background task failed: {0}")]
    Task(String),
}

impl PaperError {
    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |source| PaperError::Io { path, source }
    }
}
