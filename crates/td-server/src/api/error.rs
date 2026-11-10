//! The one error shape every route answers with: `{"error": {"code", "message"}}` and a status.

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use td_data::DataError;
use ts_rs::TS;

/// The body of every error response.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize, TS)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

/// What went wrong: a stable machine-readable `code` and a sentence for people.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize, TS)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

/// An error a handler returns; it becomes a JSON error response.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: StatusCode,
    pub body: ErrorBody,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            body: ErrorBody {
                code: code.to_string(),
                message: message.into(),
            },
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorResponse { error: self.body };
        (self.status, Json(body)).into_response()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        ApiError::new(rejection.status(), "bad_request", rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        ApiError::bad_request(rejection.body_text())
    }
}

/// Data errors caused by what the client asked for are 400s; the venue failing is a 502; the rest
/// (disk, corrupt files, no data directory) are 500s.
impl From<DataError> for ApiError {
    fn from(e: DataError) -> Self {
        let message = e.to_string();
        match e {
            DataError::InvalidSymbol(_)
            | DataError::InvalidDate(_)
            | DataError::EmptyRange { .. }
            | DataError::NotPerpetual(_)
            | DataError::KindNotListed { .. }
            | DataError::NoFunding(_)
            | DataError::NoLiveStream(_)
            | DataError::InvalidInstrument { .. } => ApiError::bad_request(message),
            DataError::Http { .. } | DataError::Status { .. } | DataError::Api { .. } => {
                ApiError::new(StatusCode::BAD_GATEWAY, "venue", message)
            }
            _ => ApiError::internal(message),
        }
    }
}
