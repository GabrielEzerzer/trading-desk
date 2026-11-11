//! Drives the API over real HTTP and WebSocket against a server started in-process on
//! 127.0.0.1:0, with a temporary data directory and a fake market source: no network.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use td_core::{Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, Venue};
use td_data::time::{MINUTE, Month, now_ms, parse_date};
use td_data::{DataError, DownloadReport, InstrumentList, Store};
use td_server::api::auth::Token;
use td_server::api::data::EARLIEST_FROM;
use td_server::api::jobs::{JobInfo, JobResult, JobStatus};
use td_server::api::source::{LiveSource, MarketSource};
use td_server::api::ws::ServerMessage;
use td_server::api::{ApiConfig, Handshake, JOB_GRACE, Server, ts};
use tempfile::TempDir;
use tokio::net::TcpStream as AsyncTcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const WAIT: Duration = Duration::from_secs(20);
/// Candles the fake writes per month slice.
const FAKE_MINUTES: i64 = 60;

/// A venue that writes an hour of candles per month slice and three funding rates. When gated,
/// each `candles` call waits for a permit from [`FakeSource::release`].
#[derive(Default)]
struct FakeSource {
    instrument_calls: AtomicUsize,
    /// The `to` of the last `funding` call.
    funding_to: Mutex<Option<Millis>>,
    gated: bool,
    permits: Mutex<u32>,
    released: Condvar,
}

impl FakeSource {
    fn gated() -> Self {
        FakeSource {
            gated: true,
            ..Self::default()
        }
    }

    fn release(&self, n: u32) {
        *self.permits.lock().unwrap() += n;
        self.released.notify_all();
    }
}

fn instrument(venue: Venue, kind: InstrumentKind, symbol: &str) -> Instrument {
    Instrument {
        id: InstrumentId {
            venue,
            symbol: symbol.into(),
            kind,
        },
        base: "BTC".into(),
        quote: "USDT".into(),
        tick_size: 0.01,
        lot_size: 0.001,
        min_qty: 0.001,
        min_notional: 5.0,
        fees: FeeSchedule {
            maker: 0.001,
            taker: 0.001,
        },
    }
}

impl MarketSource for FakeSource {
    fn instruments(&self, venue: Venue, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
        self.instrument_calls.fetch_add(1, Ordering::SeqCst);
        Ok(InstrumentList {
            instruments: vec![instrument(venue, kind, "BTCUSDT")],
            rejected: vec![("BADUSDT".into(), "tick_size must be above zero".into())],
        })
    }

    fn candles(
        &self,
        store: &Store,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<DownloadReport, DataError> {
        if self.gated {
            let mut permits = self.permits.lock().unwrap();
            while *permits == 0 {
                permits = self.released.wait(permits).unwrap();
            }
            *permits -= 1;
        }
        let candles: Vec<Candle> = (0..FAKE_MINUTES)
            .map(|i| from + i * MINUTE)
            .filter(|t| *t < to)
            .map(|open_time| Candle {
                open_time,
                open: 100.0,
                high: 101.0,
                low: 99.0,
                close: 100.5,
                volume: 1.0,
                quote_volume: 100.0,
            })
            .collect();
        store.write_candles(id, &candles)?;
        Ok(DownloadReport {
            months: vec![(Month::containing(from), candles.len())],
            missing_files: Vec::new(),
            gaps: Vec::new(),
        })
    }

    fn funding(
        &self,
        store: &Store,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<usize, DataError> {
        *self.funding_to.lock().unwrap() = Some(to);
        let hours = 8 * 60 * MINUTE;
        let rates: Vec<(Millis, f64)> = (0..3).map(|i| (from + i * hours, 0.0001)).collect();
        store.write_funding(id, &rates)?;
        Ok(rates.len())
    }
}

/// Live data that is never there: no test in this file streams. A stream would only try a closed
/// local port.
struct NoLive;

struct NoRest;

impl td_data::live::Rest for NoRest {
    fn klines(
        &self,
        _: &InstrumentId,
        _: td_core::Timeframe,
        _: Millis,
        _: Millis,
    ) -> Result<Vec<Candle>, DataError> {
        Err(DataError::NoDataDir)
    }

    fn funding(
        &self,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        Err(DataError::NoDataDir)
    }
}

impl LiveSource for NoLive {
    fn rest(&self) -> Arc<dyn td_data::live::Rest> {
        Arc::new(NoRest)
    }

    fn stream_url(&self, _: &InstrumentId) -> String {
        "ws://127.0.0.1:9/".into()
    }
}

struct TestServer {
    server: Server,
    source: Arc<FakeSource>,
    client: reqwest::Client,
    _dir: TempDir,
    _unblock: Unblock,
}

/// Frees any gated job still waiting when the test server goes, so a failing test ends instead of
/// hanging on a blocked job thread when its runtime drops.
struct Unblock(Arc<FakeSource>);

impl Drop for Unblock {
    fn drop(&mut self) {
        self.0.release(1000);
    }
}

impl TestServer {
    async fn start(source: FakeSource) -> TestServer {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = Arc::new(source);
        let server = Server::start(ApiConfig {
            store: Store::new(dir.path()),
            source: source.clone(),
            live: Arc::new(NoLive),
            token: Token::generate().expect("rng"),
            port: 0,
        })
        .await
        .expect("starts");
        TestServer {
            server,
            _unblock: Unblock(source.clone()),
            source,
            client: reqwest::Client::new(),
            _dir: dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.server.port())
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.server.token().as_str())
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let response = request.send().await.expect("answers");
        let status = response.status();
        let text = response.text().await.expect("a body");
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
        };
        (status, body)
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let request = self
            .client
            .get(self.url(path))
            .header("authorization", self.bearer());
        self.send(request).await
    }

    async fn post(&self, path: &str, body: &str) -> (StatusCode, Value) {
        let request = self
            .client
            .post(self.url(path))
            .header("authorization", self.bearer())
            .header("content-type", "application/json")
            .body(body.to_string());
        self.send(request).await
    }

    /// Polls `GET /jobs/{id}` until the job's status is `status`, returning the job.
    async fn until_status(&self, id: u64, status: &str) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let (_, job) = self.get(&format!("/jobs/{id}")).await;
            if job["status"] == status {
                return job;
            }
            assert!(
                Instant::now() < deadline,
                "job {id} never reached {status}: {job}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Opens `/ws` with the token as the `td-token.` subprotocol, as a browser would, and checks
    /// the server echoed the subprotocol back.
    async fn websocket(&self) -> Ws {
        let mut request = format!("ws://127.0.0.1:{}/ws", self.server.port())
            .into_client_request()
            .expect("a request");
        let protocol = format!("td-token.{}", self.server.token().as_str());
        request
            .headers_mut()
            .insert("sec-websocket-protocol", protocol.parse().unwrap());
        let (ws, response) = tokio_tungstenite::connect_async(request)
            .await
            .expect("upgrades");
        assert_eq!(
            response.headers().get("sec-websocket-protocol").unwrap(),
            protocol.as_str()
        );
        Ws(ws)
    }
}

struct Ws(WebSocketStream<MaybeTlsStream<AsyncTcpStream>>);

impl Ws {
    async fn send(&mut self, message: Value) {
        self.0
            .send(Message::Text(message.to_string().into()))
            .await
            .expect("sends");
    }

    /// The next server message, or `None` once the server closed the connection.
    async fn next(&mut self) -> Option<ServerMessage> {
        loop {
            let frame = tokio::time::timeout(WAIT, self.0.next())
                .await
                .expect("a message in time");
            match frame {
                Some(Ok(Message::Text(text))) => {
                    return Some(serde_json::from_str(&text).expect("a server message"));
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
                Some(Ok(_)) => continue,
            }
        }
    }

    /// Reads job messages until job `id` reaches `status`, returning it.
    async fn until_job(&mut self, id: u64, status: JobStatus) -> JobInfo {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if let Some(ServerMessage::Job { job }) = self.next().await
                && job.id == id
                && job.status == status
            {
                return *job;
            }
        }
        panic!("job {id} never reached {status:?}");
    }
}

fn day(text: &str) -> Millis {
    parse_date(text).expect("a date")
}

#[tokio::test(flavor = "multi_thread")]
async fn health_answers_the_version_with_the_token() {
    let t = TestServer::start(FakeSource::default()).await;
    let (status, body) = t.get("/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "version": env!("CARGO_PKG_VERSION") }));
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_without_the_token_or_with_a_wrong_one_are_401() {
    let t = TestServer::start(FakeSource::default()).await;
    let wrong = format!("Bearer {}", Token::generate().unwrap().as_str());
    for path in [
        "/health",
        "/jobs",
        "/strategies",
        "/data/stored",
        "/no-such-route",
        "/ws",
    ] {
        let (status, body) = t.send(t.client.get(t.url(path))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} without a token");
        assert_eq!(body["error"]["code"], "unauthorized");
        let request = t.client.get(t.url(path)).header("authorization", &wrong);
        let (status, _) = t.send(request).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{path} with a wrong token"
        );
    }
    for path in ["/data/download", "/data/delete"] {
        let (status, _) = t.send(t.client.post(t.url(path)).body("{}")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} without a token");
    }

    // A WebSocket upgrade with a wrong subprotocol token, and with none, is refused.
    for protocol in [Some(format!("td-token.{}", "0".repeat(64))), None] {
        let mut request = format!("ws://127.0.0.1:{}/ws", t.server.port())
            .into_client_request()
            .unwrap();
        if let Some(p) = &protocol {
            request
                .headers_mut()
                .insert("sec-websocket-protocol", p.parse().unwrap());
        }
        match tokio_tungstenite::connect_async(request).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), 401);
            }
            other => panic!("expected a 401, got {:?}", other.map(|(_, r)| r.status())),
        }
    }
}

#[test]
fn a_foreign_host_header_is_403_even_with_the_token() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let t = runtime.block_on(TestServer::start(FakeSource::default()));
    let port = t.server.port();
    let token = t.server.token().as_str().to_string();
    let status_for = |host: &str| {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "GET /health HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response.split(' ').nth(1).unwrap_or_default().to_string();
        (status, response)
    };
    for host in [
        "rebound.example".to_string(),
        format!("rebound.example:{port}"),
        "127.0.0.1:1".to_string(),
    ] {
        let (status, response) = status_for(&host);
        assert_eq!(status, "403", "{host}");
        assert!(response.contains("forbidden_host"), "{response}");
    }
    assert_eq!(status_for(&format!("localhost:{port}")).0, "200");
    assert_eq!(status_for(&format!("127.0.0.1:{port}")).0, "200");
    // No CORS headers: other origins cannot read responses.
    assert!(
        !status_for(&format!("127.0.0.1:{port}"))
            .1
            .to_ascii_lowercase()
            .contains("access-control-allow")
    );
    runtime.block_on(t.server.shutdown()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn instruments_come_from_the_source_once_per_list_and_then_from_memory() {
    let t = TestServer::start(FakeSource::default()).await;
    let (status, body) = t.get("/instruments?venue=binance&kind=spot").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["instruments"][0]["id"]["symbol"], "BTCUSDT");
    assert_eq!(body["instruments"][0]["id"]["venue"], "binance");
    assert_eq!(body["rejected"][0]["symbol"], "BADUSDT");
    let (_, again) = t.get("/instruments?venue=binance&kind=spot").await;
    assert_eq!(again, body);
    assert_eq!(t.source.instrument_calls.load(Ordering::SeqCst), 1);
    let (status, body) = t.get("/instruments?venue=bybit&kind=perpetual").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["instruments"][0]["id"]["kind"], "perpetual");
    assert_eq!(t.source.instrument_calls.load(Ordering::SeqCst), 2);

    for bad in [
        "/instruments?venue=kraken&kind=spot",
        "/instruments?venue=binance",
    ] {
        let (status, body) = t.get(bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(body["error"]["code"], "bad_request");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn strategies_list_their_parameter_specs() {
    let t = TestServer::start(FakeSource::default()).await;
    let (status, body) = t.get("/strategies").await;
    assert_eq!(status, StatusCode::OK);
    let list = body.as_array().expect("a list");
    assert_eq!(list.len(), td_engine::strategies().len());
    let sma = list
        .iter()
        .find(|s| s["name"] == "sma_cross")
        .expect("sma_cross is registered");
    let fast = sma["params"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "fast")
        .expect("a fast parameter");
    assert_eq!(fast["kind"], "int");
    assert!(fast["default"].is_number());
}

#[tokio::test(flavor = "multi_thread")]
async fn strategies_list_their_kind_plan_and_rule() {
    let t = TestServer::start(FakeSource::default()).await;
    let (status, body) = t.get("/strategies").await;
    assert_eq!(status, StatusCode::OK);
    let list = body.as_array().expect("a list");
    for entry in td_engine::strategies() {
        let s = list
            .iter()
            .find(|s| s["name"] == entry.name)
            .unwrap_or_else(|| panic!("{} is listed", entry.name));
        assert_eq!(s["plan"], serde_json::to_value(&entry.plan).unwrap());
        assert_eq!(s["rule"], serde_json::to_value(entry.rule).unwrap());
        assert_eq!(s["kind"], s["rule"]["kind"]);
    }
    let sma = list.iter().find(|s| s["name"] == "sma_cross").unwrap();
    assert_eq!(sma["kind"], "portfolio");
    assert_eq!(sma["rule"]["benchmark"], "buy_and_hold");
    assert_eq!(sma["rule"]["limits"]["min_sharpe"], 1.0);
    assert_eq!(sma["plan"]["timeframe"], "1h");
    assert_eq!(sma["plan"]["instruments"][0]["symbol"], "BTCUSDT");
    assert_eq!(sma["plan"]["walk_forward"]["train_days"], 180);
    assert_eq!(sma["plan"]["evidence"]["luck"]["permutations"], 199);
    assert_eq!(sma["plan"]["evidence"]["min_sample"]["trades"], 30);
    let donchian = list
        .iter()
        .find(|s| s["name"] == "donchian_breakout")
        .unwrap();
    assert_eq!(donchian["kind"], "prop_firm");
    assert_eq!(donchian["rule"]["firm"]["daily_loss"], 0.05);
    assert_eq!(donchian["rule"]["firm"]["drawdown"], "static");
    assert_eq!(donchian["rule"]["limits"]["share_of_firm_limits"], 0.5);
    assert_eq!(donchian["plan"]["out_of_sample"], Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_job_reaches_done_on_the_jobs_topic_and_shows_in_coverage() {
    let t = TestServer::start(FakeSource::default()).await;
    let mut ws = t.websocket().await;
    ws.send(json!({ "type": "subscribe", "topic": "jobs" }))
        .await;
    assert_eq!(
        ws.next().await,
        Some(ServerMessage::Subscribed {
            topic: "jobs".into()
        })
    );

    let (from, to) = (day("2024-01-15"), day("2024-02-10"));
    let body = json!({
        "instrument": { "venue": "bybit", "symbol": "BTCUSDT", "kind": "perpetual" },
        "from": from,
        "to": to,
    });
    let (status, created) = t.post("/data/download", &body.to_string()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = created["job_id"].as_u64().expect("a job id");

    let job = ws.until_job(id, JobStatus::Done).await;
    assert_eq!(job.progress, 1.0);
    assert!(job.finished_at.is_some() && job.started_at.is_some());
    let Some(JobResult::Download(result)) = &job.result else {
        panic!("a download result: {job:?}");
    };
    assert_eq!(result.candles, 2 * FAKE_MINUTES as usize);
    let months: Vec<&str> = result.months.iter().map(|m| m.month.as_str()).collect();
    assert_eq!(months, ["2024-01", "2024-02"]);
    assert_eq!(result.funding_rates, Some(3));

    let (status, fetched) = t.get(&format!("/jobs/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["status"], "done");
    assert_eq!(fetched["kind"], "download");
    assert_eq!(fetched["request"]["kind"], "download");
    assert_eq!(fetched["result"]["candles"], 2 * FAKE_MINUTES);
    let (_, list) = t.get("/jobs").await;
    assert_eq!(list.as_array().map(Vec::len), Some(1));

    let (status, coverage) = t
        .get("/data/coverage?instrument=bybit:BTCUSDT:perpetual")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(coverage["months"][0]["month"], "2024-01");
    assert_eq!(coverage["months"][0]["minutes"], FAKE_MINUTES);
    assert_eq!(coverage["months"][0]["first"], from);
    assert_eq!(coverage["months"][0]["month_minutes"], 31 * 1440);
    assert_eq!(coverage["months"][1]["first"], day("2024-02-01"));
    assert_eq!(coverage["funding"]["rates"], 3);
    assert_eq!(coverage["funding"]["first"], from);

    let (_, empty) = t
        .get("/data/coverage?instrument=binance:ETHUSDT:spot")
        .await;
    assert_eq!(empty["months"], json!([]));
    assert_eq!(empty["funding"], Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_download_stops_at_its_next_checkpoint_when_cancelled() {
    let t = TestServer::start(FakeSource::gated()).await;
    let mut ws = t.websocket().await;
    ws.send(json!({ "type": "subscribe", "topic": "jobs" }))
        .await;
    assert!(matches!(
        ws.next().await,
        Some(ServerMessage::Subscribed { .. })
    ));

    let body = json!({
        "instrument": { "venue": "binance", "symbol": "BTCUSDT", "kind": "spot" },
        "from": day("2024-01-01"),
        "to": day("2024-04-01"),
    });
    let (_, created) = t.post("/data/download", &body.to_string()).await;
    let id = created["job_id"].as_u64().unwrap();
    ws.until_job(id, JobStatus::Running).await;

    let (status, cancelled) = t.post(&format!("/jobs/{id}/cancel"), "").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(cancelled["cancel_requested"], true);
    assert_eq!(cancelled["status"], "running");

    // The first month finishes, then the job sees the cancel before the second.
    t.source.release(3);
    let job = ws.until_job(id, JobStatus::Cancelled).await;
    assert!(job.result.is_none());
    assert!(job.progress < 1.0);
    assert!(job.finished_at.is_some());

    let (status, body) = t.post(&format!("/jobs/{id}/cancel"), "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "job_finished");
    let (status, _) = t.post("/jobs/999/cancel", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = t.get("/jobs/999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_requests_get_json_errors_with_matching_statuses() {
    let t = TestServer::start(FakeSource::default()).await;
    let download = |symbol: &str, from: Millis, to: Millis| {
        json!({
            "instrument": { "venue": "binance", "symbol": symbol, "kind": "spot" },
            "from": from,
            "to": to,
        })
        .to_string()
    };
    let jan = day("2024-01-01");
    let cases = [
        (download("BTCUSDT", jan, jan), StatusCode::BAD_REQUEST),
        (download("BTCUSDT", -5, jan), StatusCode::BAD_REQUEST),
        (download("BTCUSDT", 0, jan), StatusCode::BAD_REQUEST),
        (
            download("BTCUSDT", EARLIEST_FROM - MINUTE, jan),
            StatusCode::BAD_REQUEST,
        ),
        (
            download("../escape", jan, jan + MINUTE),
            StatusCode::BAD_REQUEST,
        ),
        ("{not json".to_string(), StatusCode::BAD_REQUEST),
        (
            r#"{"from": 1}"#.to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ];
    for (body, expected) in cases {
        let (status, answer) = t.post("/data/download", &body).await;
        assert_eq!(status, expected, "{body}");
        assert!(answer["error"]["message"].is_string(), "{answer}");
    }
    assert_eq!(t.get("/jobs").await.1, json!([]), "no job was started");

    for (path, expected) in [
        (
            "/data/coverage?instrument=binance:BTCUSDT",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/data/coverage?instrument=binance:a.b:spot",
            StatusCode::BAD_REQUEST,
        ),
        ("/data/coverage", StatusCode::BAD_REQUEST),
        ("/jobs/abc", StatusCode::NOT_FOUND),
        ("/no-such-route", StatusCode::NOT_FOUND),
    ] {
        let (status, body) = t.get(path).await;
        assert_eq!(status, expected, "{path}");
        assert!(body["error"]["code"].is_string(), "{path}: {body}");
    }
    let (status, body) = t.post("/health", "").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["error"]["code"], "method_not_allowed");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_websocket_refuses_bad_topics_and_closes_on_shutdown() {
    let t = TestServer::start(FakeSource::default()).await;
    // The bearer header works for the upgrade too, without a subprotocol.
    let mut request = format!("ws://127.0.0.1:{}/ws", t.server.port())
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", t.bearer().parse().unwrap());
    let (socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert!(response.headers().get("sec-websocket-protocol").is_none());
    let mut ws = Ws(socket);

    let error_code = |m: Option<ServerMessage>| match m {
        Some(ServerMessage::Error { error }) => error.code,
        other => panic!("expected an error, got {other:?}"),
    };
    for bad in [
        "candles:binance:BTCUSDT:spot",
        "candles:binance:BTCUSDT:spot:2m",
        "candles:kraken:BTCUSDT:spot:1m",
        "candles:binance:btcusdt:spot:1m",
    ] {
        ws.send(json!({ "type": "subscribe", "topic": bad })).await;
        assert_eq!(error_code(ws.next().await), "bad_topic", "{bad}");
    }
    ws.send(json!({ "type": "subscribe", "topic": "session:1" }))
        .await;
    assert_eq!(error_code(ws.next().await), "not_found");
    ws.send(json!({ "type": "subscribe", "topic": "weather" }))
        .await;
    assert_eq!(error_code(ws.next().await), "unknown_topic");
    ws.send(json!({ "type": "shout" })).await;
    assert_eq!(error_code(ws.next().await), "bad_message");

    ws.send(json!({ "type": "subscribe", "topic": "jobs" }))
        .await;
    assert!(matches!(
        ws.next().await,
        Some(ServerMessage::Subscribed { .. })
    ));
    ws.send(json!({ "type": "unsubscribe", "topic": "jobs" }))
        .await;
    assert_eq!(
        ws.next().await,
        Some(ServerMessage::Unsubscribed {
            topic: "jobs".into()
        })
    );

    tokio::time::timeout(WAIT, t.server.shutdown())
        .await
        .expect("shuts down in time")
        .expect("shuts down cleanly");
    assert_eq!(ws.next().await, None, "the server closed the socket");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_download_of_an_instrument_with_one_unfinished_is_409() {
    let t = TestServer::start(FakeSource::gated()).await;
    let download = |symbol: &str, from: &str, to: &str| {
        json!({
            "instrument": { "venue": "binance", "symbol": symbol, "kind": "spot" },
            "from": day(from),
            "to": day(to),
        })
        .to_string()
    };
    let (status, created) = t
        .post(
            "/data/download",
            &download("BTCUSDT", "2024-01-01", "2024-02-01"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let first = created["job_id"].as_u64().unwrap();
    t.until_status(first, "running").await;

    let (status, body) = t
        .post(
            "/data/download",
            &download("BTCUSDT", "2024-01-15", "2024-03-01"),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "conflict");
    // Another instrument is not held up.
    let (status, created) = t
        .post(
            "/data/download",
            &download("ETHUSDT", "2024-01-01", "2024-02-01"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let other = created["job_id"].as_u64().unwrap();

    t.source.release(2);
    t.until_status(first, "done").await;
    t.until_status(other, "done").await;
    // Once the first has finished, the instrument can be downloaded again.
    let (status, created) = t
        .post(
            "/data/download",
            &download("BTCUSDT", "2024-01-15", "2024-03-01"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    t.source.release(2);
    t.until_status(created["job_id"].as_u64().unwrap(), "done")
        .await;
    assert_eq!(t.get("/jobs").await.1.as_array().map(Vec::len), Some(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_reaching_past_now_asks_for_funding_only_up_to_now() {
    let t = TestServer::start(FakeSource::default()).await;
    let now = now_ms();
    let from = (now - 1440 * MINUTE).div_euclid(MINUTE) * MINUTE;
    let body = json!({
        "instrument": { "venue": "bybit", "symbol": "BTCUSDT", "kind": "perpetual" },
        "from": from,
        "to": now + 30 * 1440 * MINUTE,
    });
    let (status, created) = t.post("/data/download", &body.to_string()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    t.until_status(created["job_id"].as_u64().unwrap(), "done")
        .await;
    let funding_to = t
        .source
        .funding_to
        .lock()
        .unwrap()
        .expect("funding was fetched");
    assert!(
        (now..=now_ms()).contains(&funding_to),
        "funding asked up to {funding_to}, now is {now}"
    );
}

#[test]
fn the_subprotocol_token_opens_only_the_websocket_route() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let t = runtime.block_on(TestServer::start(FakeSource::default()));
    let port = t.server.port();
    let token = t.server.token().as_str().to_string();
    // A WebSocket upgrade request to /jobs with the token only as the subprotocol, no bearer.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET /jobs HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Protocol: td-token.{token}\r\n\r\n"
    )
    .unwrap();
    let mut status_line = String::new();
    BufReader::new(stream).read_line(&mut status_line).unwrap();
    assert!(status_line.starts_with("HTTP/1.1 401"), "{status_line}");
    drop(t);
}

/// A client that sends half a request and stalls must not keep the binary alive once its stdin
/// closes: it waits [`JOB_GRACE`] for open requests, then exits anyway.
#[test]
fn the_binary_exits_after_the_grace_period_when_a_request_never_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_td-server"))
        .args(["--port", "0"])
        .env("TD_DATA_DIR", dir.path())
        .env_remove("TD_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("starts");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .expect("a handshake line");
    let handshake: Handshake = serde_json::from_str(line.trim_end()).expect("JSON");

    // Headers begun but never ended, and no token.
    let mut slow = TcpStream::connect(("127.0.0.1", handshake.port)).unwrap();
    write!(
        slow,
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n",
        handshake.port
    )
    .unwrap();
    slow.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    drop(child.stdin.take());
    let closed = Instant::now();
    let limit = JOB_GRACE + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if closed.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("still running {limit:?} after stdin closed");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status}");
    drop(slow);
}

#[test]
fn the_binary_prints_the_handshake_and_stops_when_stdin_closes() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_td-server"))
        .args(["--port", "0"])
        .env("TD_DATA_DIR", dir.path())
        .env_remove("TD_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("starts");
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).expect("a handshake line");
    let handshake: Handshake = serde_json::from_str(line.trim_end()).expect("JSON");
    assert_ne!(handshake.port, 0);
    assert_eq!(handshake.token.len(), 64);
    assert!(handshake.token.bytes().all(|b| b.is_ascii_hexdigit()));
    let keys: Vec<String> = serde_json::from_str::<serde_json::Map<String, Value>>(&line)
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(keys, ["port", "token"]);

    // The handshake is the API's: the token opens /health on that port.
    let mut stream = TcpStream::connect(("127.0.0.1", handshake.port)).unwrap();
    write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
        handshake.port, handshake.token
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");

    drop(child.stdin.take());
    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "still running after stdin closed"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status}");
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "", "stdout carries only the handshake");
}

#[test]
fn the_committed_typescript_types_are_up_to_date() {
    let committed = std::fs::read_to_string(ts::output_path())
        .expect("gui/shared/src/api-types.ts exists")
        .replace("\r\n", "\n");
    assert!(
        committed == ts::render(),
        "gui/shared/src/api-types.ts is stale: run `cargo run -p td-server --bin gen-api-types`"
    );
}

/// Downloads January and February 2024 of a Bybit perpetual through the fake: an hour of candles
/// per month and three funding rates.
async fn download_two_months(t: &TestServer, symbol: &str) {
    let body = json!({
        "instrument": { "venue": "bybit", "symbol": symbol, "kind": "perpetual" },
        "from": day("2024-01-01"),
        "to": day("2024-02-10"),
    });
    let (status, created) = t.post("/data/download", &body.to_string()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{created}");
    t.until_status(created["job_id"].as_u64().unwrap(), "done")
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stored_data_lists_with_sizes_and_deletes_by_month_range_or_whole_instrument() {
    let t = TestServer::start(FakeSource::default()).await;
    let (status, empty) = t.get("/data/stored").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty, json!({ "instruments": [], "unrecognised": [] }));
    download_two_months(&t, "BTCUSDT").await;
    download_two_months(&t, "ETHUSDT").await;

    let (status, stored) = t.get("/data/stored").await;
    assert_eq!(status, StatusCode::OK);
    let btc = &stored["instruments"][0];
    assert_eq!(
        btc["instrument"],
        json!({ "venue": "bybit", "symbol": "BTCUSDT", "kind": "perpetual" })
    );
    assert_eq!(stored["instruments"][1]["instrument"]["symbol"], "ETHUSDT");
    let months: Vec<&str> = btc["months"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["month"].as_str().unwrap())
        .collect();
    assert_eq!(months, ["2024-01", "2024-02"]);
    let month_bytes: Vec<u64> = btc["months"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["bytes"].as_u64().unwrap())
        .collect();
    let funding_bytes = btc["funding_bytes"].as_u64().expect("a funding file");
    assert!(month_bytes.iter().all(|b| *b > 0) && funding_bytes > 0);
    assert_eq!(
        btc["bytes"].as_u64(),
        Some(month_bytes.iter().sum::<u64>() + funding_bytes)
    );
    // Coverage reports the same bytes per month.
    let (_, coverage) = t
        .get("/data/coverage?instrument=bybit:BTCUSDT:perpetual")
        .await;
    let covered: Vec<u64> = coverage["months"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["bytes"].as_u64().unwrap())
        .collect();
    assert_eq!(covered, month_bytes);

    // A month range removes those months only; the funding file stays.
    let instrument = json!({ "venue": "bybit", "symbol": "BTCUSDT", "kind": "perpetual" });
    let body = json!({ "instrument": instrument, "from": "2023-11", "to": "2024-01" });
    let (status, removed) = t.post("/data/delete", &body.to_string()).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed, json!({ "files": 1, "bytes": month_bytes[0] }));
    let (_, coverage) = t
        .get("/data/coverage?instrument=bybit:BTCUSDT:perpetual")
        .await;
    assert_eq!(coverage["months"].as_array().map(Vec::len), Some(1));
    assert_eq!(coverage["months"][0]["month"], "2024-02");
    assert_eq!(coverage["funding"]["rates"], 3);

    // With no range, the rest of the instrument goes, funding included.
    let body = json!({ "instrument": instrument });
    let (status, removed) = t.post("/data/delete", &body.to_string()).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(
        removed,
        json!({ "files": 2, "bytes": month_bytes[1] + funding_bytes })
    );
    let (_, stored) = t.get("/data/stored").await;
    let left: Vec<&str> = stored["instruments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["instrument"]["symbol"].as_str().unwrap())
        .collect();
    assert_eq!(left, ["ETHUSDT"], "the other instrument is untouched");
    assert_eq!(
        stored["unrecognised"],
        json!([]),
        "lock files are not strays"
    );
    // Deleting what is not there is not an error.
    let (status, removed) = t.post("/data/delete", &body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(removed, json!({ "files": 0, "bytes": 0 }));
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_delete_requests_are_400_and_delete_nothing() {
    let t = TestServer::start(FakeSource::default()).await;
    download_two_months(&t, "BTCUSDT").await;
    let (_, before) = t.get("/data/stored").await;
    let delete = |symbol: &str, range: Value| {
        let mut body = json!({
            "instrument": { "venue": "bybit", "symbol": symbol, "kind": "perpetual" },
        });
        body.as_object_mut()
            .unwrap()
            .extend(range.as_object().unwrap().clone());
        body.to_string()
    };
    let cases = [
        delete("BTCUSDT", json!({ "from": "2024-01" })),
        delete("BTCUSDT", json!({ "to": "2024-01" })),
        delete("BTCUSDT", json!({ "from": "2024-02", "to": "2024-01" })),
        delete("BTCUSDT", json!({ "from": "2024-1", "to": "2024-02" })),
        delete("BTCUSDT", json!({ "from": "2024-01-01", "to": "2024-02" })),
        delete("BTCUSDT", json!({ "from": "2024-13", "to": "2024-13" })),
        delete("../BTCUSDT", json!({})),
        delete("..", json!({ "from": "2024-01", "to": "2024-02" })),
        delete("btcusdt", json!({})),
        "{not json".to_string(),
    ];
    for body in cases {
        let (status, answer) = t.post("/data/delete", &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {answer}");
        assert!(answer["error"]["message"].is_string(), "{answer}");
    }
    let (status, _) = t.post("/data/delete", r#"{"from": "2024-01"}"#).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(t.get("/data/stored").await.1, before, "nothing was deleted");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delete_while_the_instrument_downloads_is_409() {
    let t = TestServer::start(FakeSource::gated()).await;
    let body = json!({
        "instrument": { "venue": "binance", "symbol": "BTCUSDT", "kind": "spot" },
        "from": day("2024-01-01"),
        "to": day("2024-03-01"),
    });
    let (status, created) = t.post("/data/download", &body.to_string()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let job = created["job_id"].as_u64().unwrap();
    t.source.release(1);
    // January is stored and the job waits on February.
    let deadline = Instant::now() + WAIT;
    while t.get("/data/stored").await.1["instruments"] == json!([]) {
        assert!(Instant::now() < deadline, "January was never stored");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(t.get(&format!("/jobs/{job}")).await.1["status"], "running");

    let delete = |symbol: &str| {
        json!({ "instrument": { "venue": "binance", "symbol": symbol, "kind": "spot" } })
            .to_string()
    };
    let (status, answer) = t.post("/data/delete", &delete("BTCUSDT")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(answer["error"]["code"], "conflict");
    let range = json!({
        "instrument": { "venue": "binance", "symbol": "BTCUSDT", "kind": "spot" },
        "from": "2024-01",
        "to": "2024-01",
    });
    let (status, _) = t.post("/data/delete", &range.to_string()).await;
    assert_eq!(status, StatusCode::CONFLICT, "a month range too");
    let (_, coverage) = t
        .get("/data/coverage?instrument=binance:BTCUSDT:spot")
        .await;
    assert_eq!(coverage["months"][0]["month"], "2024-01", "nothing deleted");
    // Another instrument is not held up.
    let (status, _) = t.post("/data/delete", &delete("ETHUSDT")).await;
    assert_eq!(status, StatusCode::OK);

    t.source.release(1);
    t.until_status(job, "done").await;
    let (status, removed) = t.post("/data/delete", &delete("BTCUSDT")).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["files"], 2);
}
