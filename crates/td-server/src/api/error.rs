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

/// Data errors caused by what the client asked for, or by an OANDA token that is missing or
/// refused, are 400s; the venue failing, or a source giving quotes the store refuses during a
/// download, is a 502; a stored month of a quoted market without its asks is a 422
/// `data_missing`; the rest (disk, corrupt files, no data directory) are 500s.
impl From<DataError> for ApiError {
    fn from(e: DataError) -> Self {
        let message = e.to_string();
        match e {
            // A stored month without its asks, read for a run or merged into by a download: it
            // cannot be used until it is deleted and downloaded again.
            DataError::NoAskColumns { .. } => {
                ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "data_missing", message)
            }
            // Only writes give these, and the server writes only what a download's source gave.
            DataError::QuotesRequired(_)
            | DataError::NoQuotes(_)
            | DataError::QuoteMinutes { .. }
            | DataError::CrossedQuote { .. }
            | DataError::ClosedMinute { .. }
            | DataError::NoAsks { .. } => ApiError::new(StatusCode::BAD_GATEWAY, "venue", message),
            DataError::InvalidSymbol(_)
            | DataError::InvalidDate(_)
            | DataError::EmptyRange { .. }
            | DataError::NotPerpetual(_)
            | DataError::KindNotListed { .. }
            | DataError::NoFunding(_)
            | DataError::NoLiveStream(_)
            | DataError::InvalidInstrument { .. }
            | DataError::MissingOandaToken
            | DataError::OandaRefused { .. } => ApiError::bad_request(message),
            DataError::Http { .. } | DataError::Status { .. } | DataError::Api { .. } => {
                ApiError::new(StatusCode::BAD_GATEWAY, "venue", message)
            }
            _ => ApiError::internal(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn quote_errors_map_to_data_missing_for_the_store_and_venue_for_a_download() {
        let instrument = || "binance:EURUSD:fx".to_string();
        let cases = [
            (
                DataError::NoAskColumns {
                    path: PathBuf::from("2024-01.parquet"),
                },
                422,
                "data_missing",
            ),
            (DataError::QuotesRequired(instrument()), 502, "venue"),
            (DataError::NoQuotes(instrument()), 502, "venue"),
            (
                DataError::QuoteMinutes {
                    instrument: instrument(),
                    reason: "1 bids and 0 asks".into(),
                },
                502,
                "venue",
            ),
            (
                DataError::CrossedQuote {
                    instrument: instrument(),
                    open_time: 0,
                    field: "open",
                    bid: 1.0,
                    ask: 0.9,
                },
                502,
                "venue",
            ),
            (
                DataError::ClosedMinute {
                    instrument: instrument(),
                    open_time: 0,
                },
                502,
                "venue",
            ),
            (
                DataError::NoAsks {
                    instrument: instrument(),
                    month: "2024-01".into(),
                },
                502,
                "venue",
            ),
        ];
        for (e, status, code) in cases {
            let message = e.to_string();
            let api = ApiError::from(e);
            assert_eq!(
                (
                    api.status.as_u16(),
                    api.body.code.as_str(),
                    api.body.message
                ),
                (status, code, message)
            );
        }
    }
}
