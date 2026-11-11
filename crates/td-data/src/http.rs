// Adapted from tipping-point engine/src/venue/binance/rest.rs at 7dc6d12 (client setup and the
// status check), made blocking.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::HeaderMap;

use crate::DataError;

/// The user agent on every request. Nothing identifying goes in headers, URLs or payloads.
const USER_AGENT: &str = "trading-desk";

/// A blocking HTTP client for the venues' public endpoints. Cheap to clone; the connection pool is
/// shared. Every request sends `Accept-Encoding: gzip` and a gzipped body is decoded before it is
/// read: Binance's spot exchangeInfo is about 52 KB that way instead of 2.5 MB.
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

    /// GETs `url` with `headers` and returns the status and the body, whatever the status. Mark a
    /// header that carries a secret as sensitive, so no debug output shows it.
    pub(crate) fn get_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(u16, Vec<u8>), DataError> {
        let http = |source| DataError::Http {
            url: url.to_string(),
            source,
        };
        let response = self.client.get(url).headers(headers).send().map_err(http)?;
        let status = response.status().as_u16();
        let body = response.bytes().map_err(http)?;
        Ok((status, body.to_vec()))
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

/// A one-shot HTTP server on 127.0.0.1 for tests: it answers one request with a fixed response and
/// hands back the request it received.
#[cfg(test)]
pub(crate) mod stub {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    /// Serves one request at the returned base URL (`http://127.0.0.1:<port>`) with `headers` and
    /// `body`, then closes. Joining the handle gives the request line and headers, lower-cased.
    pub(crate) fn serve_once(
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        let mut head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("a connection");
            let mut reader = BufReader::new(stream.try_clone().expect("a clone"));
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("reads");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                request.push_str(&line.to_lowercase());
            }
            stream.write_all(head.as_bytes()).expect("writes");
            stream.write_all(&body).expect("writes");
            request
        });
        (base, handle)
    }

    /// `text` gzipped.
    pub(crate) fn gzip(text: &str) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(text.as_bytes()).expect("compresses");
        encoder.finish().expect("compresses")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_asks_for_gzip_and_a_gzipped_body_is_decoded() {
        let text = r#"{"symbols":[]}"#;
        let (base, server) = stub::serve_once(
            &[
                ("Content-Type", "application/json"),
                ("Content-Encoding", "gzip"),
            ],
            stub::gzip(text),
        );
        let http = Http::new().expect("an HTTP client");
        let body = http.get_text(&format!("{base}/info"), &[]).expect("a body");
        assert_eq!(body, text);
        let request = server.join().expect("the server");
        let accepts = request
            .lines()
            .find_map(|l| l.strip_prefix("accept-encoding:"))
            .unwrap_or_default();
        assert!(accepts.contains("gzip"), "{request}");
        // Nothing identifying: the user agent stays plain.
        assert!(
            request.lines().any(|l| l == "user-agent: trading-desk"),
            "{request}"
        );
    }
}
