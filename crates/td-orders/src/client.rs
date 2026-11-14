//! The signed request loop: clock, timeouts, rate limits, the time budget, and the classification
//! of every answer into accepted, refused, not sent, or unknown.
//!
//! What the venue documents, read 2026-11-14:
//! - <https://bybit-exchange.github.io/docs/v5/rate-limit>: per account and endpoint, a rolling
//!   one-second window. Each answer carries `X-Bapi-Limit-Status` (requests left), `X-Bapi-Limit` and
//!   `X-Bapi-Limit-Reset-Timestamp`. Over the limit the body says `retCode` 10006, "Too many
//!   visits!". Per IP, 600 requests in 5 seconds; over it the answer is HTTP 403 "access too
//!   frequent" and the IP is blocked for at least 10 minutes; `retCode` 10018 is the same limit.
//!   HTTP 429 is "system level frequency protection. Please retry when encounter this".
//! - <https://bybit-exchange.github.io/docs/v5/guide>: the venue accepts a request only when
//!   `server_time - recv_window <= timestamp < server_time + 1000`, and answers 10002 otherwise.
//! - <https://bybit-exchange.github.io/docs/v5/error>: `retCode` 429, "The trading service is
//!   experiencing a high server load. Please retry if you encounter this issue"; 10429, "System
//!   level frequency protection" (listed with the WebSocket order-entry codes); 110079, "The order
//!   is processing and can not be operated, please try again later"; 40004, "the order is modified
//!   during the process of replacing, please check the order status again"; 10014, "Invalid
//!   duplicate request"; 10000 and 10016, a server timeout and a server error.
//!
//! Only three kinds of answer say that the venue refused a request before processing it, and each
//! rests on one documented statement:
//! - 10006 rests on the rate-limit page: the request is over the account's per-request limit for
//!   the endpoint. The `retCode` alone is taken as that answer; `X-Bapi-Limit-Status` is read only
//!   for the wait. It is sent again after the reset the venue gives, at most
//!   [`RATE_LIMIT_RETRIES`] times and only when the reset is within [`LONGEST_RATE_WAIT`] and the
//!   time budget.
//! - 10002 rests on the guide's rule: a request whose timestamp is outside that window is not
//!   accepted. The clock offset is measured again and the request sent once more.
//! - HTTP 403 and 10018 rest on the rate-limit page's IP block: the IP's requests are refused. They
//!   are returned at once; waiting ten minutes is the caller's decision.
//!
//! Every other answer that is neither an acceptance nor a `retCode` refusal leaves open whether an
//! order request was taken, so it goes back as unknown at once and is never sent again here: a
//! timeout, a dropped connection, an unreadable or oversized answer, HTTP 429, any other status
//! outside 2xx (a redirect included: a server may apply a POST and then redirect), the body codes
//! 429, 10429 and 110079 (the venue asks for a retry but does not say the request was not taken),
//! 40004, 10014, and the server errors 10000 and 10016. The caller then asks the venue (`orders`).
//! So does any `retCode` refusal of an order request that was sent more than once: if the venue
//! was wrong to refuse an earlier send, that send may have been applied, and a duplicate-id or
//! other refusal of the next one may follow from it.
//! A read changes nothing, so a read answered 429 (HTTP or body) or 10429 waits and goes again
//! within the same bounds, and a read whose answer never arrived is tried again ([`Client::read`]).
//!
//! The client measures its clock offset from `GET /v5/market/time` on the same host when it is
//! built and on a 10002. A server time more than a day from the local clock is refused. Numbers the
//! venue sends (server time, rate-limit resets) go through checked or saturating arithmetic, and no
//! answer body is read past [`MAX_BODY_BYTES`].
//!
//! A `place`, `cancel` or `amend_stop` passes its deadline to every request it makes: each
//! request's timeout, the clock resync, each rate-limit wait and each pause is cut to what is left,
//! and nothing is sent once it has passed. An order request is sent again only while a whole
//! request timeout is left, so its answer, or the lookup after it, fits in the budget; the first
//! send of an operation is cut to what is left instead.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::blocking::Client as Http;
use reqwest::blocking::Response;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use td_core::Millis;

use crate::guard::{Endpoint, same_origin};
use crate::wire::{self, ServerTime};
use crate::{ClientOrderId, DemoKeys, Error};

/// The user agent on every request. Nothing identifying goes in headers, URLs or payloads.
const USER_AGENT: &str = "trading-desk";

/// How many times a request refused with 10006, or a read answered 429 or 10429, is sent again.
const RATE_LIMIT_RETRIES: u32 = 3;
/// The longest wait for a rate-limit reset; a later reset is returned as an error.
const LONGEST_RATE_WAIT: Duration = Duration::from_secs(3);
/// How many times a read whose answer never arrived is tried in all.
const READ_ATTEMPTS: u32 = 3;
/// The most of an answer body that is read, after decompression. The largest answer the route
/// asks for (a page of 100 executions) is tens of kilobytes.
pub(crate) const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;
/// The furthest the venue's server time may be from the local clock.
const MAX_CLOCK_OFFSET_MS: i64 = 24 * 60 * 60 * 1000;

const RET_TIMESTAMP: i64 = 10002;
const RET_RATE_LIMIT: i64 = 10006;
const RET_IP_RATE_LIMIT: i64 = 10018;
const RET_SERVER_TIMEOUT: i64 = 10000;
const RET_SERVER_ERROR: i64 = 10016;
/// Load and frequency protection answered in the body.
const RET_HIGH_LOAD: i64 = 429;
const RET_FREQUENCY_PROTECTION: i64 = 10429;
/// "The order is processing and can not be operated, please try again later."
const RET_PROCESSING: i64 = 110079;
/// "The order is modified during the process of replacing, please check the order status again."
const RET_CHECK_STATUS: i64 = 40004;
/// "Invalid duplicate request."
const RET_DUPLICATE_REQUEST: i64 = 10014;

/// The route's timeouts, pauses and time budget. [`Timing::default`] is what the route uses
/// against the venue; tests against the mock shorten it. [`Client::with_timing`] refuses values
/// outside the ranges below, so no caller can set a request timeout in minutes or a budget over
/// two minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Time to open a connection: 50 ms to 30 s. Default 5 s.
    pub connect_timeout: Duration,
    /// Time for a whole request and its answer, connection included: 50 ms to 30 s. Default 10 s.
    pub request_timeout: Duration,
    /// The `X-BAPI-RECV-WINDOW` sent with each request: 100 ms to 60 s. Default 5 s, the venue's
    /// own default. The venue accepts a request signed at time `t` only until `t` plus this
    /// window, so the lookup after a request of unknown outcome goes on until it has passed.
    pub recv_window: Duration,
    /// The pause between lookup rounds, and the margin added to the receive window for clock
    /// error: 10 ms to 10 s. Default 1 s.
    pub settle: Duration,
    /// The longest one `place`, `cancel` or `amend_stop` may take, its lookups included: 1 s to
    /// 120 s. Default 30 s: room for a request that times out (10 s), a lookup that spans the
    /// receive window and its margin (6 s), and a resend of a cancel or amend with its own
    /// confirmation. Reads have no budget.
    pub operation_budget: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(10),
            recv_window: Duration::from_secs(5),
            settle: Duration::from_secs(1),
            operation_budget: Duration::from_secs(30),
        }
    }
}

impl Timing {
    fn check(&self) -> Result<(), Error> {
        let within = |d: Duration, lo: u64, hi: u64| {
            d >= Duration::from_millis(lo) && d <= Duration::from_millis(hi)
        };
        let ok = within(self.connect_timeout, 50, 30_000)
            && within(self.request_timeout, 50, 30_000)
            && within(self.recv_window, 100, 60_000)
            && within(self.settle, 10, 10_000)
            && within(self.operation_budget, 1_000, 120_000);
        if ok {
            Ok(())
        } else {
            Err(Error::InvalidTiming {
                reason: "a timeout, receive window, pause or budget is outside its allowed range",
            })
        }
    }
}

/// A blocking client for the venue's demo-trading v5 REST API, USDT perpetuals (`linear`) only.
/// Call it from a thread that may block. It is `Send + Sync`; one client per session keeps the
/// clock offset, rate-limit state and the set of client ids it has sent.
///
/// A `place`, `cancel` or `amend_stop` returns within about its time budget
/// ([`Timing::operation_budget`], 30 s with [`Timing::default`]): before each request, clock
/// resync, rate-limit wait and pause it takes what is left of the budget, cuts the request's
/// timeout (10 s by default, connection included) or the wait to it, and sends nothing once it
/// has run out; an order request is not sent again with less than a request timeout left. What
/// can run past the budget is the time to sign a request and parse an answer.
/// Reads are not under the budget: each request in a read is bounded by the request timeout and
/// is tried at most three times, each try with up to three rate-limit waits of at most 3 s and
/// one clock resync.
pub struct Client {
    endpoint: Endpoint,
    base: Url,
    http: Http,
    keys: DemoKeys,
    pub(crate) timing: Timing,
    /// Server time minus local time, in milliseconds.
    offset_ms: Mutex<i64>,
    /// Per endpoint path, the server time its limit resets when the last answer said no requests
    /// were left.
    limits: Mutex<HashMap<&'static str, Millis>>,
    /// Every client id this client has sent.
    sent: Mutex<HashSet<String>>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("endpoint", &self.endpoint)
            .field("timing", &self.timing)
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

pub(crate) enum Method {
    Get(Vec<(&'static str, String)>),
    /// A GET of a public market endpoint, sent without the key or a signature.
    Public(Vec<(&'static str, String)>),
    Post(serde_json::Value),
}

/// How a signed request ended, short of success.
pub(crate) enum Failure {
    /// The venue did not take the request: it was never sent, or the venue refused it before
    /// processing it (the module documentation lists those answers). Sending it again cannot
    /// duplicate anything.
    NotSent(Error),
    /// The request may or may not have been applied. `sent` is the local instant it was stamped,
    /// so a lookup's window is measured on the local clock, which a clock resync cannot move.
    Unknown { reason: String, sent: Instant },
    /// The venue answered with a non-zero `retCode` that refuses the request. The message has
    /// been scrubbed.
    Venue { code: i64, message: String },
}

impl Client {
    /// Builds a client with [`Timing::default`] and measures the clock offset.
    pub fn new(endpoint: Endpoint, keys: DemoKeys) -> Result<Self, Error> {
        Self::with_timing(endpoint, keys, Timing::default())
    }

    /// Builds a client with the given timing (checked against [`Timing`]'s ranges) and measures
    /// the clock offset against the same host's server time.
    pub fn with_timing(endpoint: Endpoint, keys: DemoKeys, timing: Timing) -> Result<Self, Error> {
        let base = endpoint.base_url()?;
        match endpoint {
            Endpoint::Demo if keys.loopback_only() => return Err(Error::LoopbackKeysOnDemo),
            // Whatever listens on a local port would get the real key and a valid signature.
            Endpoint::Loopback(_) if !keys.loopback_only() => {
                return Err(Error::DemoKeysOnLoopback);
            }
            _ => {}
        }
        timing.check()?;
        let http = Http::builder()
            .user_agent(USER_AGENT)
            // A redirect could carry the signed headers to another host.
            .redirect(Policy::none())
            // A system proxy would see loopback traffic in the clear and choose the route.
            .no_proxy()
            // No automatic resend of any kind: whether to send again is decided here.
            .retry(reqwest::retry::never())
            .connect_timeout(timing.connect_timeout)
            .timeout(timing.request_timeout)
            .build()
            .map_err(|_| Error::Transport {
                endpoint: "(client setup)",
                reason: "the HTTP client could not be built".into(),
            })?;
        let client = Client {
            endpoint,
            base,
            http,
            keys,
            timing,
            offset_ms: Mutex::new(0),
            limits: Mutex::new(HashMap::new()),
            sent: Mutex::new(HashSet::new()),
        };
        client.sync_clock(timing.request_timeout)?;
        Ok(client)
    }

    /// The venue's current time by this client's clock, in milliseconds.
    pub fn server_now(&self) -> Millis {
        local_now().saturating_add(*self.offset_ms.lock().unwrap())
    }

    /// Measures the offset between the venue's server time and the local clock, with `timeout`
    /// for the request. A server time more than a day away is refused and the offset left as it
    /// was.
    fn sync_clock(&self, timeout: Duration) -> Result<(), Error> {
        const PATH: &str = "/v5/market/time";
        let url = self.url(PATH)?;
        let before = local_now();
        let response = self
            .http
            .get(url)
            .timeout(timeout)
            .send()
            .map_err(|e| transport(PATH, &e))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Error::HttpStatus {
                endpoint: PATH,
                status,
            });
        }
        let body = read_capped(response).map_err(|e| match e {
            BodyFailure::TooLarge => Error::ResponseTooLarge { endpoint: PATH },
            BodyFailure::Failed(reason) => Error::Transport {
                endpoint: PATH,
                reason: reason.into(),
            },
        })?;
        let after = local_now();
        let env = wire::envelope(PATH, &body)?;
        if env.ret_code != 0 {
            return Err(Error::Rejected {
                code: env.ret_code,
                message: self.keys.scrub(&env.ret_msg, ""),
            });
        }
        let time: ServerTime = wire::from_value(PATH, env.result)?;
        let nanos: i128 = time.time_nano.parse().map_err(|_| Error::BadResponse {
            endpoint: PATH,
            reason: "timeNano is not a number".into(),
        })?;
        let local = before + (after - before) / 2;
        let offset = i64::try_from(nanos / 1_000_000)
            .ok()
            .and_then(|server| server.checked_sub(local))
            .filter(|offset| offset.abs() <= MAX_CLOCK_OFFSET_MS)
            .ok_or(Error::ServerTimeOutOfRange)?;
        *self.offset_ms.lock().unwrap() = offset;
        Ok(())
    }

    /// A venue message with any credential withheld, as every error in the crate holds it.
    pub(crate) fn scrub(&self, message: &str) -> String {
        self.keys.scrub(message, "")
    }

    fn url(&self, path: &'static str) -> Result<Url, Error> {
        let url = self.base.join(path).map_err(|_| Error::HostRefused {
            reason: "the request path does not join the base URL",
        })?;
        if !same_origin(&self.base, &url) {
            return Err(Error::HostRefused {
                reason: "a request URL left the base URL's origin",
            });
        }
        Ok(url)
    }

    /// Whether a whole request timeout is left before `deadline`, as an order request sent again
    /// needs.
    pub(crate) fn room_to_resend(&self, deadline: Instant) -> bool {
        deadline.saturating_duration_since(Instant::now()) >= self.timing.request_timeout
    }

    /// The timeout for the next request: the request timeout, cut to what is left before
    /// `deadline`. `None` once the deadline has passed.
    fn timeout_within(&self, deadline: Option<Instant>) -> Option<Duration> {
        match deadline {
            None => Some(self.timing.request_timeout),
            Some(end) => {
                let left = end.saturating_duration_since(Instant::now());
                (!left.is_zero()).then(|| left.min(self.timing.request_timeout))
            }
        }
    }

    /// Sends one request and classifies the answer as the module documentation describes. A
    /// 10006 and one timestamp refusal are sent again here, and so is a read answered 429 or
    /// 10429; every other answer goes back to the caller. With a `deadline`, each request's
    /// timeout and each wait is cut to what is left of it, nothing is sent once it has passed,
    /// and an order request is not sent again with less than a request timeout left.
    pub(crate) fn call(
        &self,
        path: &'static str,
        method: &Method,
        deadline: Option<Instant>,
    ) -> Result<serde_json::Value, Failure> {
        let mut resynced = false;
        let mut rate_retries = 0;
        let mut sends = 0u32;
        let signed = !matches!(method, Method::Public(_));
        let order_request = matches!(method, Method::Post(_));
        let out_of_time = || {
            Failure::NotSent(Error::Transport {
                endpoint: path,
                reason: "too little of the time budget was left to send the request".into(),
            })
        };
        let limited = |ip_block| {
            Failure::NotSent(Error::RateLimited {
                endpoint: path,
                ip_block,
            })
        };
        loop {
            self.wait_for_limit(path, deadline);
            let Some(timeout) = self.timeout_within(deadline) else {
                return Err(out_of_time());
            };
            // Every earlier send was refused before processing, so not sending frees the id.
            if order_request && sends > 0 && deadline.is_some_and(|d| !self.room_to_resend(d)) {
                return Err(out_of_time());
            }
            let timestamp = self.server_now();
            let stamped = Instant::now();
            let unknown = |reason: String| Failure::Unknown {
                reason,
                sent: stamped,
            };
            let mut url = self.url(path).map_err(Failure::NotSent)?;
            let (payload, body) = match method {
                Method::Get(query) | Method::Public(query) => {
                    if !query.is_empty() {
                        url.query_pairs_mut()
                            .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
                    }
                    (url.query().unwrap_or("").to_string(), None)
                }
                Method::Post(json) => {
                    let text = json.to_string();
                    (text.clone(), Some(text))
                }
            };
            let recv_window = self.timing.recv_window.as_millis() as u64;
            let (signature, headers) = if signed {
                let signature = self.keys.sign(timestamp, recv_window, &payload);
                let headers = self
                    .headers(timestamp, recv_window, &signature, body.is_some())
                    .map_err(Failure::NotSent)?;
                (signature, headers)
            } else {
                (String::new(), HeaderMap::new())
            };
            let request = match body {
                None => self.http.get(url),
                Some(body) => self.http.post(url).body(body),
            };
            sends += 1;
            let response = match request.headers(headers).timeout(timeout).send() {
                Ok(r) => r,
                Err(e) => return Err(unknown(transport_reason(&e).into())),
            };
            let status = response.status().as_u16();
            self.note_limit(path, response.headers());
            let reset = header_i64(response.headers(), "x-bapi-limit-reset-timestamp");
            let body = match read_capped(response) {
                Ok(b) => b,
                // A read changes nothing, so its error goes back as is; an order request's answer
                // was unreadable, so whether it was applied is unknown.
                Err(BodyFailure::TooLarge) if !order_request => {
                    return Err(Failure::NotSent(Error::ResponseTooLarge { endpoint: path }));
                }
                Err(BodyFailure::TooLarge) => {
                    return Err(unknown(format!(
                        "the answer was longer than {MAX_BODY_BYTES} bytes"
                    )));
                }
                Err(BodyFailure::Failed(reason)) => return Err(unknown(reason.into())),
            };
            match status {
                200..=299 => {}
                403 => return Err(limited(true)),
                429 if !order_request => {
                    rate_retries += 1;
                    if rate_retries > RATE_LIMIT_RETRIES
                        || !pause(self.timing.settle * rate_retries, deadline)
                    {
                        return Err(limited(false));
                    }
                    continue;
                }
                // A 5xx, or any other status to an order request (HTTP 429 and redirects
                // included): nothing the venue documents says the request was not taken.
                _ if status >= 500 || order_request => {
                    return Err(unknown(format!("the venue answered HTTP {status}")));
                }
                _ => {
                    return Err(Failure::NotSent(Error::HttpStatus {
                        endpoint: path,
                        status,
                    }));
                }
            }
            let env = match wire::envelope(path, &body) {
                Ok(env) => env,
                Err(_) => return Err(unknown("the answer was not the documented JSON".into())),
            };
            match env.ret_code {
                0 => return Ok(env.result),
                // Refused before processing (the rate-limit page): wait for the reset the venue
                // gives and send again.
                RET_RATE_LIMIT => {
                    rate_retries += 1;
                    let wait = reset
                        .map(|r| Duration::from_millis(millis_until(r, self.server_now())))
                        .unwrap_or(self.timing.settle);
                    if rate_retries > RATE_LIMIT_RETRIES
                        || wait > LONGEST_RATE_WAIT
                        || !pause(wait, deadline)
                    {
                        return Err(limited(false));
                    }
                }
                RET_HIGH_LOAD | RET_FREQUENCY_PROTECTION if !order_request => {
                    rate_retries += 1;
                    if rate_retries > RATE_LIMIT_RETRIES
                        || !pause(self.timing.settle * rate_retries, deadline)
                    {
                        return Err(limited(false));
                    }
                }
                RET_HIGH_LOAD | RET_FREQUENCY_PROTECTION | RET_PROCESSING if order_request => {
                    return Err(unknown(format!(
                        "the venue answered {}, which asks for a retry but does not say the \
                         request was not taken",
                        env.ret_code
                    )));
                }
                RET_CHECK_STATUS | RET_DUPLICATE_REQUEST => {
                    return Err(unknown(format!(
                        "the venue answered {}, which leaves the outcome open",
                        env.ret_code
                    )));
                }
                RET_IP_RATE_LIMIT => return Err(limited(true)),
                // Refused before processing (the guide's timestamp rule): measure the clock again
                // and send once more.
                RET_TIMESTAMP if !resynced => {
                    resynced = true;
                    let Some(timeout) = self.timeout_within(deadline) else {
                        return Err(out_of_time());
                    };
                    // The request was refused before processing, so a failed resync leaves it
                    // not sent. It is reported on the request's own endpoint as the documented
                    // not-sent error, whatever the time endpoint answered.
                    self.sync_clock(timeout).map_err(|e| {
                        Failure::NotSent(Error::Transport {
                            endpoint: path,
                            reason: format!(
                                "the venue refused the request timestamp, and the clock resync \
                                 failed: {e}"
                            ),
                        })
                    })?;
                }
                RET_TIMESTAMP => {
                    return Err(Failure::NotSent(Error::TimestampRejected {
                        code: env.ret_code,
                        message: self.keys.scrub(&env.ret_msg, &signature),
                    }));
                }
                RET_SERVER_TIMEOUT | RET_SERVER_ERROR => {
                    return Err(unknown(format!(
                        "the venue answered server error {}",
                        env.ret_code
                    )));
                }
                // An earlier send the venue refused may have been applied after all, and this
                // refusal may follow from it.
                code if order_request && sends > 1 => {
                    return Err(unknown(format!(
                        "the venue refused a send that followed a rate-limit or timestamp \
                         refusal with {code} {}, which the earlier send may have caused",
                        self.keys.scrub(&env.ret_msg, &signature)
                    )));
                }
                code => {
                    return Err(Failure::Venue {
                        code,
                        message: self.keys.scrub(&env.ret_msg, &signature),
                    });
                }
            }
        }
    }

    /// A read: like [`Client::call`] with GET, tried again when its answer never arrived, since a
    /// read changes nothing.
    pub(crate) fn get(
        &self,
        path: &'static str,
        query: Vec<(&'static str, String)>,
    ) -> Result<serde_json::Value, Error> {
        self.read(path, Method::Get(query))
    }

    /// A read of a public market endpoint: like [`Client::get`], without the key or a signature,
    /// through the same host checks and HTTP client.
    pub(crate) fn get_public(
        &self,
        path: &'static str,
        query: Vec<(&'static str, String)>,
    ) -> Result<serde_json::Value, Error> {
        self.read(path, Method::Public(query))
    }

    fn read(&self, path: &'static str, method: Method) -> Result<serde_json::Value, Error> {
        let mut last = String::new();
        for attempt in 0..READ_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(self.timing.settle);
            }
            match self.call(path, &method, None) {
                Ok(v) => return Ok(v),
                Err(Failure::Unknown { reason, .. }) => last = reason,
                Err(failure) => return Err(failure_error(failure)),
            }
        }
        Err(Error::Transport {
            endpoint: path,
            reason: last,
        })
    }

    /// One read with no retry, within `deadline`, for the lookups of an order operation (they
    /// have their own rounds).
    pub(crate) fn get_once(
        &self,
        path: &'static str,
        query: Vec<(&'static str, String)>,
        deadline: Instant,
    ) -> Result<serde_json::Value, Error> {
        self.call(path, &Method::Get(query), Some(deadline))
            .map_err(|f| match f {
                Failure::Unknown { reason, .. } => Error::Transport {
                    endpoint: path,
                    reason,
                },
                other => failure_error(other),
            })
    }

    fn headers(
        &self,
        timestamp: Millis,
        recv_window: u64,
        signature: &str,
        json: bool,
    ) -> Result<HeaderMap, Error> {
        let value = |s: &str, sensitive: bool| {
            let mut v = HeaderValue::from_str(s).map_err(|_| Error::InvalidKey {
                name: crate::keys::KEY_VAR,
            })?;
            v.set_sensitive(sensitive);
            Ok::<_, Error>(v)
        };
        let mut h = HeaderMap::new();
        h.insert("X-BAPI-API-KEY", value(self.keys.key(), true)?);
        h.insert("X-BAPI-TIMESTAMP", value(&timestamp.to_string(), false)?);
        h.insert(
            "X-BAPI-RECV-WINDOW",
            value(&recv_window.to_string(), false)?,
        );
        h.insert("X-BAPI-SIGN", value(signature, true)?);
        if json {
            h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        Ok(h)
    }

    fn note_limit(&self, path: &'static str, headers: &HeaderMap) {
        let left = header_i64(headers, "x-bapi-limit-status");
        let reset = header_i64(headers, "x-bapi-limit-reset-timestamp");
        let mut limits = self.limits.lock().unwrap();
        match (left, reset) {
            (Some(0), Some(reset)) => {
                limits.insert(path, reset);
            }
            _ => {
                limits.remove(path);
            }
        }
    }

    /// Waits, at most [`LONGEST_RATE_WAIT`] and never past `deadline`, when the last answer from
    /// `path` said no requests were left until a reset still to come.
    fn wait_for_limit(&self, path: &'static str, deadline: Option<Instant>) {
        let reset = self.limits.lock().unwrap().get(path).copied();
        if let Some(reset) = reset {
            let mut wait = Duration::from_millis(millis_until(reset, self.server_now()))
                .min(LONGEST_RATE_WAIT);
            if let Some(end) = deadline {
                wait = wait.min(end.saturating_duration_since(Instant::now()));
            }
            std::thread::sleep(wait);
        }
    }

    /// Records that `id` is being sent. An id this client has sent before is refused.
    pub(crate) fn claim_id(&self, id: &ClientOrderId) -> Result<(), Error> {
        if self.sent.lock().unwrap().insert(id.as_str().to_string()) {
            Ok(())
        } else {
            Err(Error::ClientIdAlreadyUsed { id: id.to_string() })
        }
    }

    /// Frees `id` after the venue has certainly not taken a request carrying it.
    pub(crate) fn release_id(&self, id: &ClientOrderId) {
        self.sent.lock().unwrap().remove(id.as_str());
    }
}

/// Sleeps for `wait` and says so, or says no at once when `deadline` would pass first.
pub(crate) fn pause(wait: Duration, deadline: Option<Instant>) -> bool {
    if deadline.is_some_and(|end| Instant::now() + wait >= end) {
        return false;
    }
    std::thread::sleep(wait);
    true
}

/// The public error for a failure the caller gets as is.
pub(crate) fn failure_error(failure: Failure) -> Error {
    match failure {
        Failure::NotSent(e) => e,
        Failure::Venue { code, message } => Error::Rejected { code, message },
        Failure::Unknown { reason, .. } => Error::Transport {
            endpoint: "(request)",
            reason,
        },
    }
}

fn header_i64(headers: &HeaderMap, name: &str) -> Option<i64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

/// Milliseconds from `now` until `at`, zero when `at` has passed, for any `at` the venue sends.
fn millis_until(at: Millis, now: Millis) -> u64 {
    u64::try_from(at.saturating_sub(now)).unwrap_or(0)
}

enum BodyFailure {
    TooLarge,
    Failed(&'static str),
}

/// Reads an answer body, decompressed, up to [`MAX_BODY_BYTES`].
fn read_capped(response: Response) -> Result<Vec<u8>, BodyFailure> {
    let mut body = Vec::new();
    response
        .take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|e| {
            BodyFailure::Failed(
                match e.get_ref().and_then(|i| i.downcast_ref::<reqwest::Error>()) {
                    Some(inner) => transport_reason(inner),
                    None if e.kind() == std::io::ErrorKind::TimedOut => {
                        "timed out before the answer arrived"
                    }
                    None => "the connection failed before the answer arrived",
                },
            )
        })?;
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err(BodyFailure::TooLarge);
    }
    Ok(body)
}

fn local_now() -> Millis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as Millis)
        .unwrap_or(0)
}

/// A fixed description of a transport failure. reqwest's own text is not kept: it is safe today,
/// but a fixed set of phrases cannot start quoting a URL or a header later.
fn transport_reason(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timed out before the answer arrived"
    } else if e.is_connect() {
        "could not connect"
    } else if e.is_redirect() {
        "the venue answered with a redirect, which this client never follows"
    } else {
        "the connection failed before the answer arrived"
    }
}

fn transport(endpoint: &'static str, e: &reqwest::Error) -> Error {
    Error::Transport {
        endpoint,
        reason: transport_reason(e).into(),
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn keys_that_are_not_loopback_only_never_reach_a_local_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let keys = DemoKeys::from_values("not-a-real-key", "not-a-real-secret", false).unwrap();
        let err = Client::new(Endpoint::Loopback(port), keys).unwrap_err();
        assert_eq!(err, Error::DemoKeysOnLoopback);
        // The refusal comes before the clock sync, the first request a client makes.
        assert!(listener.accept().is_err(), "the local port was contacted");
    }

    #[test]
    fn a_reset_time_from_the_venue_cannot_overflow_the_wait() {
        assert_eq!(millis_until(i64::MIN, 1_700_000_000_000), 0);
        assert_eq!(millis_until(i64::MAX, -1), u64::try_from(i64::MAX).unwrap());
        assert_eq!(millis_until(1_000, 400), 600);
    }
}
