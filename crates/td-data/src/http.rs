// Adapted from tipping-point engine/src/venue/binance/rest.rs at 7dc6d12 (client setup and the
// status check), made blocking.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, RETRY_AFTER};

use crate::DataError;

/// The user agent on every request. Nothing identifying goes in headers, URLs or payloads.
const USER_AGENT: &str = "trading-desk";

/// How many times a request answered 429 or 5xx is tried again before its answer is returned.
const RETRIES: u32 = 4;

/// The longest pause before a retry, whatever a `Retry-After` header asks for.
const LONGEST_PAUSE: Duration = Duration::from_secs(60);

/// A blocking HTTP client for the venues' public endpoints. Cheap to clone; the connection pool is
/// shared. Every request sends `Accept-Encoding: gzip` and a gzipped body is decoded before it is
/// read: Binance's spot exchangeInfo is about 52 KB that way instead of 2.5 MB.
///
/// A GET for text or a file answered 429 or 5xx is tried again up to four times, after the pause a
/// `Retry-After` header gives in seconds, or else after a pause that starts at a second and doubles
/// each time, both at most a minute.
#[derive(Debug, Clone)]
pub struct Http {
    client: Client,
    /// The first pause before a retry when the answer gives no `Retry-After`.
    backoff: Duration,
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
        Ok(Http {
            client,
            backoff: Duration::from_secs(1),
        })
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

    /// GETs `url` with `headers` and returns the status and the body, whatever the status, without
    /// retrying (the caller decides). Mark a header that carries a secret as sensitive, so no debug
    /// output shows it. An error names `url` but not a URL it redirected to, which may carry a
    /// signature or a token.
    pub(crate) fn get_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(u16, Vec<u8>), DataError> {
        let http = |source: reqwest::Error| DataError::Http {
            url: url.to_string(),
            source: source.without_url(),
        };
        let response = self.client.get(url).headers(headers).send().map_err(http)?;
        let status = response.status().as_u16();
        let body = response.bytes().map_err(http)?;
        Ok((status, body.to_vec()))
    }

    /// GETs `url` with `query`, trying again on a 429 or a 5xx as [`Http`] says.
    fn send(
        &self,
        url: &str,
        query: &[(&str, String)],
    ) -> Result<reqwest::blocking::Response, DataError> {
        let mut pause = self.backoff;
        for _ in 0..RETRIES {
            let response = self.send_once(url, query)?;
            let status = response.status();
            if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
                return Ok(response);
            }
            // A Retry-After given as a date rather than seconds falls back to the doubling pause.
            let asked = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok())
                .map(Duration::from_secs);
            std::thread::sleep(asked.unwrap_or(pause).min(LONGEST_PAUSE));
            pause *= 2;
        }
        self.send_once(url, query)
    }

    fn send_once(
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
        serve_once_with("200 OK", headers, body)
    }

    /// [`serve_once`] answering with `status`, such as `"302 Found"`.
    pub(crate) fn serve_once_with(
        status: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> (String, JoinHandle<String>) {
        let (base, server) = serve_each(vec![response(status, headers, &body, body.len())]);
        let handle = std::thread::spawn(move || server.join().expect("the server").remove(0));
        (base, handle)
    }

    /// The bytes of a response with `status`, `headers` and `body`, saying its body is
    /// `content_length` bytes long.
    pub(crate) fn response(
        status: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        content_length: usize,
    ) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {content_length}\r\nConnection: close\r\n"
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    /// Serves one request per entry of `responses`, in turn, with that entry's bytes as written,
    /// closing the connection after each. Joining the handle gives each request line and headers,
    /// lower-cased.
    pub(crate) fn serve_each(responses: Vec<Vec<u8>>) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for bytes in responses {
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
                stream.write_all(&bytes).expect("writes");
                requests.push(request);
            }
            requests
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

    /// The client follows a redirect, but not with a secret: a redirect to another host arrives
    /// without the `Authorization` header the first request carried, as reqwest drops it.
    #[test]
    fn a_redirect_to_another_host_does_not_carry_the_authorization_header() {
        let (target, landed) = stub::serve_once(&[], b"moved here".to_vec());
        // `localhost` rather than `127.0.0.1`: another host name and another port.
        let location = target.replace("127.0.0.1", "localhost") + "/elsewhere";
        let (base, first) =
            stub::serve_once_with("302 Found", &[("Location", location.as_str())], Vec::new());
        let mut headers = HeaderMap::new();
        let mut secret = reqwest::header::HeaderValue::from_static("Bearer not-a-real-token");
        secret.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, secret);
        let http = Http::new().expect("an HTTP client");
        let (status, body) = http
            .get_with_headers(&format!("{base}/start"), headers)
            .expect("an answer");
        assert_eq!((status, body), (200, b"moved here".to_vec()));
        let first = first.join().expect("the first server");
        assert!(
            first
                .lines()
                .any(|l| l == "authorization: bearer not-a-real-token"),
            "{first}"
        );
        let landed = landed.join().expect("the redirect's server");
        assert!(landed.starts_with("get /elsewhere "), "{landed}");
        assert!(!landed.contains("authorization"), "{landed}");
        assert!(!landed.contains("not-a-real-token"), "{landed}");
    }

    #[test]
    fn a_429_or_a_5xx_is_tried_again_after_the_pause_retry_after_asks_for() {
        let (base, server) = stub::serve_each(vec![
            stub::response("429 Too Many Requests", &[("Retry-After", "1")], b"", 0),
            stub::response("503 Service Unavailable", &[], b"", 0),
            stub::response("200 OK", &[], b"ok", 2),
        ]);
        let http = Http {
            backoff: Duration::from_millis(1),
            ..Http::new().expect("an HTTP client")
        };
        let start = std::time::Instant::now();
        let body = http.get_text(&format!("{base}/x"), &[]).expect("a body");
        assert_eq!(body, "ok");
        assert_eq!(server.join().expect("the server").len(), 3);
        assert!(
            start.elapsed() >= Duration::from_secs(1),
            "Retry-After was not honoured"
        );
    }

    #[test]
    fn retries_stop_after_a_bound_and_the_last_answer_is_the_error() {
        let failure = stub::response("500 Internal Server Error", &[], b"down", 4);
        let (base, server) = stub::serve_each(vec![failure; RETRIES as usize + 1]);
        let http = Http {
            backoff: Duration::from_millis(1),
            ..Http::new().expect("an HTTP client")
        };
        let result = http.get_bytes(&format!("{base}/file.zip"));
        assert!(
            matches!(result, Err(DataError::Status { status: 500, .. })),
            "{result:?}"
        );
        assert_eq!(
            server.join().expect("the server").len(),
            RETRIES as usize + 1
        );
    }

    /// A body cut short after a redirect fails without the redirect's URL in the error, so a URL
    /// the venue redirects to, which may carry a signature or a token, is never shown or logged.
    #[test]
    fn a_body_cut_short_after_a_redirect_is_an_error_without_the_redirects_url() {
        let (target, landed) = stub::serve_each(vec![stub::response("200 OK", &[], b"cut", 100)]);
        let location = target.replace("127.0.0.1", "localhost") + "/signed?token=not-a-real-one";
        let (base, first) =
            stub::serve_once_with("302 Found", &[("Location", location.as_str())], Vec::new());
        let http = Http::new().expect("an HTTP client");
        let start = format!("{base}/start");
        let error = http
            .get_with_headers(&start, HeaderMap::new())
            .expect_err("the body is cut short");
        first.join().expect("the first server");
        landed.join().expect("the redirect's server");
        let text = error.to_string();
        assert!(matches!(error, DataError::Http { .. }), "{error:?}");
        assert!(text.starts_with(&start), "{text}");
        assert!(
            !text.contains("signed") && !text.contains("not-a-real-one"),
            "{text}"
        );
    }
}
