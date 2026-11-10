// Adapted from tipping-point engine/src/venue/binance/rest.rs at 7dc6d12 (client setup and the
// status check), made blocking.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::blocking::Client;

use crate::DataError;

/// The user agent on every request. Nothing identifying goes in headers, URLs or payloads.
const USER_AGENT: &str = "trading-desk";

/// A blocking HTTP client for the venues' public endpoints. Cheap to clone; the connection pool is
/// shared.
#[derive(Debug, Clone)]
pub struct Http {
    client: Client,
}

impl Http {
    pub fn new() -> Result<Self, DataError> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|source| DataError::Http {
                url: "(client setup)".into(),
                source,
            })?;
        Ok(Http { client })
    }

    /// GETs `url` with `query` and returns the body as text; a non-2xx status is an error.
    pub(crate) fn get_text(
        &self,
        url: &str,
        query: &[(&str, String)],
    ) -> Result<String, DataError> {
        let response = self.send(url, query)?;
        let status = response.status();
        let body = response.text().map_err(|source| DataError::Http {
            url: url.to_string(),
            source,
        })?;
        if !status.is_success() {
            return Err(DataError::Status {
                url: url.to_string(),
                status: status.as_u16(),
                body: body.chars().take(200).collect(),
            });
        }
        Ok(body)
    }

    /// GETs `url` and returns the body, or `None` when the server answers 404.
    pub(crate) fn get_bytes(&self, url: &str) -> Result<Option<Vec<u8>>, DataError> {
        let response = self.send(url, &[])?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(DataError::Status {
                url: url.to_string(),
                status: status.as_u16(),
                body: String::new(),
            });
        }
        let bytes = response.bytes().map_err(|source| DataError::Http {
            url: url.to_string(),
            source,
        })?;
        Ok(Some(bytes.to_vec()))
    }

    fn send(
        &self,
        url: &str,
        query: &[(&str, String)],
    ) -> Result<reqwest::blocking::Response, DataError> {
        self.client
            .get(url)
            .query(query)
            .send()
            .map_err(|source| DataError::Http {
                url: url.to_string(),
                source,
            })
    }
}
