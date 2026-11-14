// Adapted from tipping-point engine/src/venue/binance/rest.rs at 7dc6d12 (client setup and the
// status check), made blocking.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, RETRY_AFTER};

use crate::DataError;
use crate::time::now_ms;

/// The user agent on every request. Nothing identifying goes in headers, URLs or payloads.
const USER_AGENT: &str = "trading-desk";

/// How many times a request answered with a rate limit or a 5xx is tried again before it fails.
const RETRIES: u32 = 4;

/// The longest pause before a retry, whatever the venue asks for. With [`RETRIES`], a request
/// waits at most four minutes in all before it fails.
const LONGEST_PAUSE: Duration = Duration::from_secs(60);

/// Bybit's header giving, in milliseconds since the epoch, when its rate-limit window resets
/// (<https://bybit-exchange.github.io/docs/v5/rate-limit>).
const BYBIT_LIMIT_RESET: &str = "x-bapi-limit-reset-timestamp";

/// A blocking HTTP client for the venues' public endpoints. Cheap to clone; the connection pool is
/// shared. Every request sends `Accept-Encoding: gzip` and a gzipped body is decoded before it is
/// read: Binance's spot exchangeInfo is about 52 KB that way instead of 2.5 MB.
///
/// A GET for text or a file answered with a rate limit (HTTP 429, or a 2xx body a venue client
/// names as one, such as Bybit's `retCode` 10006) or a 5xx is tried again up to four times. Each pause
/// is the longer of a pause that starts at a second and doubles each time and the wait the venue
/// asks for (a `Retry-After` header in seconds, or Bybit's limit reset time), and at most a
/// minute: 15 s in all when the venue asks for nothing, four minutes at most. A rate limit still
/// there after the last try fails with [`DataError::RateLimited`], which says how long it waited;
/// a 5xx fails with its status.
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

    /// A client whose pauses before a retry start at a millisecond, for tests.
    #[cfg(test)]
    pub(crate) fn with_short_pauses() -> Self {
        Http {
            backoff: Duration::from_millis(1),
            ..Http::new().expect("an HTTP client")
        }
    }

    /// GETs `url` with `query` and returns the body as text; a non-2xx status is an error.
    pub(crate) fn get_text(
        &self,
        url: &str,
        query: &[(&str, String)],
    ) -> Result<String, DataError> {
        self.get_text_limited(url, query, |_| false)
    }

    /// [`Http::get_text`] for a venue that answers its rate limit with a 2xx body: a body that
    /// `limited` says is that answer is waited out and tried again as a 429 is.
    pub(crate) fn get_text_limited(
        &self,
        url: &str,
        query: &[(&str, String)],
        limited: fn(&[u8]) -> bool,
    ) -> Result<String, DataError> {
        let (status, body) = self.send(url, query, limited)?;
        let body = String::from_utf8_lossy(&body).into_owned();
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
        let (status, body) = self.send(url, &[], |_| false)?;
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
        Ok(Some(body))
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

    /// GETs `url` with `query` and returns the status and body of the answer, trying again on a
    /// rate limit (a 429, or a 2xx body `limited` names) or a 5xx as [`Http`] says.
    fn send(
        &self,
        url: &str,
        query: &[(&str, String)],
        limited: fn(&[u8]) -> bool,
    ) -> Result<(StatusCode, Vec<u8>), DataError> {
        let mut pause = self.backoff;
        let mut waited = Duration::ZERO;
        let mut tries = 0;
        loop {
            let response = self.send_once(url, query)?;
            tries += 1;
            let status = response.status();
            let asked = asked_wait(response.headers());
            let body = response
                .bytes()
                .map_err(|source| DataError::Http {
                    url: url.to_string(),
                    source,
                })?
                .to_vec();
            let rate_limited =
                status == StatusCode::TOO_MANY_REQUESTS || (status.is_success() && limited(&body));
            if !rate_limited && !status.is_server_error() {
                return Ok((status, body));
            }
            if tries > RETRIES {
                if !rate_limited {
                    return Ok((status, body));
                }
                return Err(DataError::RateLimited {
                    url: url.to_string(),
                    tries,
                    waited,
                    answer: String::from_utf8_lossy(&body).chars().take(200).collect(),
                });
            }
            let wait = asked
                .map_or(pause, |asked| asked.max(pause))
                .min(LONGEST_PAUSE);
            std::thread::sleep(wait);
            waited += wait;
            pause *= 2;
        }
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

/// The wait an answer asks for before the next request: a `Retry-After` in seconds, or the time
/// until Bybit's limit resets. A `Retry-After` given as a date, or a reset already passed by the
/// local clock, asks for none.
fn asked_wait(headers: &HeaderMap) -> Option<Duration> {
    let number = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<u64>().ok();
    number(RETRY_AFTER.as_str())
        .map(Duration::from_secs)
        .or_else(|| {
            let reset = i64::try_from(number(BYBIT_LIMIT_RESET)?).ok()?;
            let ms = reset.saturating_sub(now_ms());
            (ms > 0).then(|| Duration::from_millis(ms as u64))
        })
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

    /// A venue that never lifts its rate limit fails after the last retry, saying how many tries
    /// and how long it waited: the doubling pauses alone, 1 + 2 + 4 + 8 at the test's scale.
    #[test]
    fn a_rate_limit_that_outlasts_the_retries_fails_saying_how_long_it_waited() {
        let limited = stub::response("429 Too Many Requests", &[], b"slow down", 9);
        let (base, server) = stub::serve_each(vec![limited; RETRIES as usize + 1]);
        let url = format!("{base}/x");
        let error = Http::with_short_pauses()
            .get_text(&url, &[])
            .expect_err("the limit never lifts");
        assert_eq!(
            server.join().expect("the server").len(),
            RETRIES as usize + 1
        );
        let DataError::RateLimited {
            tries,
            waited,
            ref answer,
            ..
        } = error
        else {
            panic!("{error:?}");
        };
        assert_eq!(tries, RETRIES + 1);
        assert_eq!(waited, Duration::from_millis(15));
        assert_eq!(answer, "slow down");
        assert_eq!(
            error.to_string(),
            format!(
                "{url} still answered with its rate limit after 5 tries and 0.0 s of waiting: \
                 slow down"
            )
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
