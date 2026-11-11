//! Drives the run routes over real HTTP and WebSocket against a server started in-process on
//! 127.0.0.1:0, with a temporary data directory seeded with 1-minute candles: no network.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, Timeframe, Venue,
};
use td_data::time::{DAY, MINUTE, parse_date};
use td_data::{DataError, DownloadReport, InstrumentList, Store};
use td_engine::{BacktestConfig, Params, SavedRun};
use td_server::api::auth::Token;
use td_server::api::data::GapInfo;
use td_server::api::jobs::{JobInfo, JobRequest, JobResult, JobStatus};
const MAX_RUN_JOBS: usize = 8;
use td_server::api::runs::RunDetail;
use td_server::api::source::{LiveSource, MarketSource};
use td_server::api::ws::ServerMessage;
use td_server::api::{ApiConfig, Server};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// A guard against hangs only: no test waits on time passing, and a passing test never comes
/// near it.
const WAIT: Duration = Duration::from_secs(60);
const HOUR: Millis = 60 * MINUTE;

/// The runs never download; any call is a test bug.
struct NoSource;

impl MarketSource for NoSource {
    fn instruments(&self, _: Venue, _: InstrumentKind) -> Result<InstrumentList, DataError> {
        Err(DataError::NoDataDir)
    }
    fn candles(
        &self,
        _: &Store,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<DownloadReport, DataError> {
        Err(DataError::NoDataDir)
    }
    fn funding(
        &self,
        _: &Store,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<usize, DataError> {
        Err(DataError::NoDataDir)
    }
}

/// Live data that is never there: no run streams. A stream would only try a closed local port.
struct NoLive;

struct NoRest;

impl td_data::live::Rest for NoRest {
    fn klines(
        &self,
        _: &InstrumentId,
        _: Timeframe,
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
    fn rest(&self) -> std::sync::Arc<dyn td_data::live::Rest> {
        std::sync::Arc::new(NoRest)
    }

    fn stream_url(&self, _: &InstrumentId) -> String {
        "ws://127.0.0.1:9/".into()
    }
}

fn t0() -> Millis {
    parse_date("2024-03-01").unwrap()
}

fn instrument(kind: InstrumentKind) -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Binance,
            symbol: "BTCUSDT".into(),
            kind,
        },
        base: "BTC".into(),
        quote: "USDT".into(),
        tick_size: 0.01,
        lot_size: 0.001,
        min_qty: 0.001,
        min_notional: 5.0,
        fees: FeeSchedule {
            maker: 0.0002,
            taker: 0.0005,
            per_unit: 0.0,
        },
    }
}

/// A triangle wave with period 1 between -1 and 1.
fn tri(x: f64) -> f64 {
    let f = x - x.floor();
    4.0 * (f - 0.5).abs() - 1.0
}

/// 1-minute candles over `days` days from [`t0`]: a slow trend with waves, so SMA pairs differ.
/// The minutes of `hole` are left out.
fn minutes(days: i64, hole: std::ops::Range<Millis>) -> Vec<Candle> {
    let price = |i: i64| {
        let x = i as f64;
        100.0 + 0.0002 * x + 5.0 * tri(x / 5760.0) + 2.0 * tri(x / 780.0) + 0.7 * tri(x / 97.0)
    };
    (0..days * 1440)
        .map(|i| {
            let (open, close) = (price(i), price(i + 1));
            Candle {
                open_time: t0() + i * MINUTE,
                open,
                high: open.max(close) + 0.05,
                low: open.min(close) - 0.05,
                close,
                volume: 1.0,
                quote_volume: close,
            }
        })
        .filter(|c| !hole.contains(&c.open_time))
        .collect()
}

struct TestServer {
    server: Server,
    store: Store,
    client: reqwest::Client,
    _dir: TempDir,
}

/// Stops every unfinished job when a test ends, including by a failed assertion: dropping the
/// runtime waits for blocking tasks, so a job that holds a slot until cancelled would otherwise
/// hang the suite instead of failing the test.
impl Drop for TestServer {
    fn drop(&mut self) {
        self.server.jobs().cancel_all();
    }
}

impl TestServer {
    /// A server whose store holds `days` days of minutes for BTCUSDT spot and perpetual, with a
    /// 30-minute outage at 10:00 on the second day and perpetual funding every 8 hours.
    async fn seeded(days: i64) -> TestServer {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::new(dir.path());
        let outage = t0() + DAY + 10 * HOUR..t0() + DAY + 10 * HOUR + 30 * MINUTE;
        let candles = minutes(days, outage);
        for kind in [InstrumentKind::Spot, InstrumentKind::Perpetual] {
            store
                .write_candles(&instrument(kind).id, &candles)
                .expect("seeds");
        }
        let rates: Vec<(Millis, f64)> = (0..days * 3)
            .map(|i| {
                (
                    t0() + i * 8 * HOUR,
                    if i % 2 == 0 { 0.0001 } else { -0.00005 },
                )
            })
            .collect();
        store
            .write_funding(&instrument(InstrumentKind::Perpetual).id, &rates)
            .expect("seeds");
        let server = Server::start(ApiConfig {
            store: store.clone(),
            source: std::sync::Arc::new(NoSource),
            live: std::sync::Arc::new(NoLive),
            token: Token::generate().expect("rng"),
            port: 0,
        })
        .await
        .expect("starts");
        TestServer {
            server,
            store,
            client: reqwest::Client::new(),
            _dir: dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.server.port())
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

    fn bearer(&self) -> String {
        format!("Bearer {}", self.server.token().as_str())
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let request = self
            .client
            .get(self.url(path))
            .header("authorization", self.bearer());
        self.send(request).await
    }

    async fn post(&self, path: &str, body: &Value) -> (StatusCode, Value) {
        let request = self
            .client
            .post(self.url(path))
            .header("authorization", self.bearer())
            .header("content-type", "application/json")
            .body(body.to_string());
        self.send(request).await
    }

    /// Starts a run and returns its job id.
    async fn start(&self, path: &str, body: &Value) -> u64 {
        let (status, created) = self.post(path, body).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{path}: {created}");
        created["job_id"].as_u64().expect("a job id")
    }

    async fn result(&self, job: u64) -> RunDetail {
        let (status, body) = self.get(&format!("/jobs/{job}/result")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_value(body).expect("a run detail")
    }

    async fn websocket(&self) -> Ws {
        let mut request = format!("ws://127.0.0.1:{}/ws", self.server.port())
            .into_client_request()
            .expect("a request");
        let protocol = format!("td-token.{}", self.server.token().as_str());
        request
            .headers_mut()
            .insert("sec-websocket-protocol", protocol.parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("upgrades");
        let mut ws = Ws(ws);
        ws.0.send(Message::Text(
            json!({ "type": "subscribe", "topic": "jobs" })
                .to_string()
                .into(),
        ))
        .await
        .expect("sends");
        assert!(matches!(
            ws.next().await,
            Some(ServerMessage::Subscribed { .. })
        ));
        ws
    }
}

struct Ws(WebSocketStream<MaybeTlsStream<TcpStream>>);

impl Ws {
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

    /// Every state of job `id` the topic sends until it finishes, in order.
    async fn follow(&mut self, id: u64) -> Vec<JobInfo> {
        let deadline = Instant::now() + WAIT;
        let mut states = Vec::new();
        while Instant::now() < deadline {
            if let Some(ServerMessage::Job { job }) = self.next().await
                && job.id == id
            {
                let finished = job.status.is_finished();
                states.push(*job);
                if finished {
                    return states;
                }
            }
        }
        panic!("job {id} never finished: {states:?}");
    }
}

fn config(kind: InstrumentKind, days: i64) -> BacktestConfig {
    BacktestConfig {
        strategy: "sma_cross".into(),
        params: Params::new()
            .with("fast", td_engine::ParamValue::Int(5))
            .with("slow", td_engine::ParamValue::Int(20)),
        instrument: instrument(kind),
        timeframe: Timeframe::H1,
        from: t0(),
        to: t0() + days * DAY,
        starting_cash: 10_000.0,
        slippage_bps: 2.0,
        fee_override: None,
        warmup_from: None,
        close_at_end: false,
        max_leverage: 1.0,
    }
}

/// A sweep over fast x slow on the perpetual, with shorts, ranked by total return: the seeded
/// ranges are days long, too short for a Sharpe ratio or a CAGR.
fn sweep_spec(days: i64, fast: (i64, i64), slow: (i64, i64)) -> Value {
    let mut base = serde_json::to_value(config(InstrumentKind::Perpetual, days)).unwrap();
    base["params"] = json!({ "allow_short": true });
    json!({
        "base": base,
        "axes": [
            { "name": "fast", "space": { "kind": "range", "from": fast.0, "to": fast.1, "step": 1 } },
            { "name": "slow", "space": { "kind": "range", "from": slow.0, "to": slow.1, "step": 1 } },
        ],
        "mode": "grid",
        "objective": { "metric": "total_return", "direction": "maximize" },
        "min_trades": 0,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_backtest_through_the_api_equals_td_engine_run_on_the_same_candles() {
    let t = TestServer::seeded(10).await;
    let mut ws = t.websocket().await;
    // Two days of warm-up before the backtest proper; the outage on day two falls in it.
    let config = BacktestConfig {
        from: t0() + 2 * DAY,
        warmup_from: Some(t0()),
        ..config(InstrumentKind::Perpetual, 10)
    };
    let job = t
        .start("/runs/backtest", &serde_json::to_value(&config).unwrap())
        .await;
    let states = ws.follow(job).await;
    assert!(states.iter().any(|j| j.status == JobStatus::Running));
    let done = states.last().unwrap();
    assert_eq!(done.status, JobStatus::Done, "{:?}", done.error);
    let Some(JobResult::Backtest(finished)) = &done.result else {
        panic!("a backtest result: {done:?}");
    };
    // The outage on day two drops the 10:00 bucket.
    assert_eq!(finished.dropped_buckets, 1);

    let detail = t.result(job).await;
    assert_eq!(detail.id, finished.run_id);
    assert_eq!(detail.check, None);
    let data = detail.data.as_ref().expect("a data report");
    assert_eq!(data.dropped, vec![t0() + DAY + 10 * HOUR]);
    let outage = t0() + DAY + 10 * HOUR;
    assert_eq!(
        data.gaps,
        vec![GapInfo {
            from: outage,
            to: outage + 30 * MINUTE,
            minutes: 30,
        }]
    );
    assert_eq!(data.bars, 10 * 24 - 1);
    assert_eq!(data.funding_rates, 30);

    // The direct run reads from the warm-up start, as the API must.
    let id = &config.instrument.id;
    let loaded = t
        .store
        .load_candles(id, Timeframe::H1, t0(), config.to, false)
        .unwrap();
    assert_eq!(loaded.dropped, data.dropped);
    let funding = t.store.load_funding(id, t0(), config.to).unwrap();
    let direct = td_engine::run(&config, &loaded.candles, &[], &funding).unwrap();
    assert!(direct.metrics.trades > 0, "the test needs trades");
    let SavedRun::Backtest(through_api) = &detail.run else {
        panic!("a saved backtest");
    };
    assert_eq!(**through_api, direct);
    assert_eq!(
        serde_json::to_string(&**through_api).unwrap(),
        serde_json::to_string(&direct).unwrap()
    );

    // The run is listed and reopens as matching today's data and engine.
    let (status, list) = t.get("/runs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    assert_eq!(list[0], serde_json::to_value(&finished.summary).unwrap());
    assert_eq!(list[0]["kind"], "backtest");
    assert_eq!(list[0]["strategy"], "sma_cross");
    assert_eq!(list[0]["from"], config.from);
    let (status, reopened) = t.get(&format!("/runs/{}", finished.run_id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        reopened["check"],
        json!({ "data_matches": true, "engine_matches": true })
    );
    assert_eq!(reopened["data"]["dropped"], json!([t0() + DAY + 10 * HOUR]));
    assert_eq!(reopened["data_error"], Value::Null);

    // A damaged data report reads as an error, not as a run saved without one.
    let report = t
        .store
        .root()
        .join("runs")
        .join(finished.run_id.as_str())
        .join("data.json");
    std::fs::write(&report, "{ not json").unwrap();
    let (status, damaged) = t.get(&format!("/runs/{}", finished.run_id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(damaged["data"], Value::Null);
    assert!(damaged["data_error"].is_string(), "{damaged}");
    std::fs::remove_file(&report).unwrap();
    let (_, absent) = t.get(&format!("/runs/{}", finished.run_id)).await;
    assert_eq!(
        (&absent["data"], &absent["data_error"]),
        (&Value::Null, &Value::Null)
    );

    // A changed candle in the range makes the data no longer match.
    let mut changed = t
        .store
        .read_month(id, td_data::time::Month::containing(t0()))
        .unwrap()[100];
    changed.close += 1.0;
    changed.high += 1.0;
    t.store.write_candles(id, &[changed]).unwrap();
    let (_, reopened) = t.get(&format!("/runs/{}", finished.run_id)).await;
    assert_eq!(
        reopened["check"],
        json!({ "data_matches": false, "engine_matches": true })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_out_of_sample_and_walk_forward_jobs_report_progress_and_save_their_runs() {
    let t = TestServer::seeded(20).await;
    let mut ws = t.websocket().await;
    let days = 20;
    // 6 x 8 = 48 backtests.
    let sweep = sweep_spec(days, (3, 8), (13, 20));
    let job = t.start("/runs/sweep", &sweep).await;
    let states = ws.follow(job).await;
    assert_eq!(states.last().unwrap().status, JobStatus::Done);
    let partial: Vec<f64> = states
        .iter()
        .map(|j| j.progress)
        .filter(|p| *p > 0.0 && *p < 1.0)
        .collect();
    assert!(!partial.is_empty(), "progress between 0 and 1 on the topic");
    assert!(partial.windows(2).all(|w| w[0] <= w[1]), "{partial:?}");
    let detail = t.result(job).await;
    let SavedRun::Sweep(result) = &detail.run else {
        panic!("a sweep: {:?}", detail.run.kind());
    };
    assert_eq!(result.rows.len(), 48);
    assert!(result.best.is_some());
    assert_eq!(detail.data.as_ref().unwrap().dropped.len(), 1);

    let oos = json!({ "sweep": sweep, "split": { "fraction": 0.7 } });
    let job = t.start("/runs/out-of-sample", &oos).await;
    let done = ws.follow(job).await.pop().unwrap();
    assert_eq!(done.status, JobStatus::Done, "{:?}", done.error);
    assert!(matches!(done.result, Some(JobResult::Oos(_))));
    let detail = t.result(job).await;
    let SavedRun::Oos(result) = &detail.run else {
        panic!("an out-of-sample run");
    };
    assert_eq!(result.in_sample.rows.len(), 48);

    let wf = json!({
        "sweep": sweep,
        "windows": { "train_bars": 200, "test_bars": 60, "step_bars": 60, "mode": "rolling" },
    });
    let job = t.start("/runs/walk-forward", &wf).await;
    let done = ws.follow(job).await.pop().unwrap();
    assert_eq!(done.status, JobStatus::Done, "{:?}", done.error);
    assert_eq!(done.kind, td_server::api::jobs::JobKind::WalkForward);
    let detail = t.result(job).await;
    let SavedRun::WalkForward(result) = &detail.run else {
        panic!("a walk-forward run");
    };
    // 20 days of hourly bars, less the dropped one: 479 bars hold four windows.
    assert_eq!(result.windows.len(), 4);

    let (_, list) = t.get("/runs").await;
    let kinds: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["walk_forward", "oos", "sweep"], "newest first");
    for row in list.as_array().unwrap() {
        let (status, reopened) = t
            .get(&format!("/runs/{}", row["id"].as_str().unwrap()))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            reopened["check"],
            json!({ "data_matches": true, "engine_matches": true }),
            "{}",
            row["kind"]
        );
        assert_eq!(reopened["run"]["kind"], row["kind"]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_range_the_store_lacks_is_422_data_missing_and_starts_no_job() {
    let t = TestServer::seeded(3).await;
    let mut beyond = serde_json::to_value(config(InstrumentKind::Spot, 5)).unwrap();
    let (status, body) = t.post("/runs/backtest", &beyond).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "data_missing");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("binance:BTCUSDT:spot")
            && message.contains("from 2024-03-04 00:00 to 2024-03-06 00:00 UTC"),
        "{message}"
    );
    // An instrument with nothing stored.
    beyond["instrument"]["id"]["symbol"] = json!("ETHUSDT");
    beyond["to"] = json!(t0() + 3_600_000 * 5);
    let (status, body) = t.post("/runs/backtest", &beyond).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "data_missing");
    let (status, body) = t
        .post("/runs/sweep", &sweep_spec(4, (3, 4), (10, 11)))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "data_missing");

    // Specs that are wrong whatever the data are 400s.
    let mut bad = sweep_spec(3, (3, 4), (10, 11));
    bad["axes"][1]["name"] = json!("fast");
    let (status, body) = t.post("/runs/sweep", &bad).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_spec");
    let mut unknown = serde_json::to_value(config(InstrumentKind::Spot, 3)).unwrap();
    unknown["strategy"] = json!("nope");
    let (status, body) = t.post("/runs/backtest", &unknown).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "invalid_config");
    let mut unknown_sweep = sweep_spec(3, (3, 4), (10, 11));
    unknown_sweep["base"]["strategy"] = json!("nope");
    let (status, body) = t.post("/runs/sweep", &unknown_sweep).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "invalid_config");
    let mut crossed = serde_json::to_value(config(InstrumentKind::Spot, 3)).unwrap();
    crossed["params"] = json!({ "fast": 30, "slow": 10 });
    let (status, body) = t.post("/runs/backtest", &crossed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_config");
    let (status, body) = t.post("/runs/walk-forward", &json!({ "sweep": 1 })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "bad_request");

    let (_, jobs) = t.get("/jobs").await;
    assert_eq!(jobs, json!([]), "no job was created");
}

/// Holds every thread of rayon's global pool until dropped.
struct BusyGlobalPool(Arc<(Mutex<bool>, Condvar)>);

impl BusyGlobalPool {
    async fn hold() -> BusyGlobalPool {
        let threads = rayon::current_num_threads();
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        for _ in 0..threads {
            let (started, release) = (started.clone(), release.clone());
            rayon::spawn(move || {
                started.fetch_add(1, Ordering::SeqCst);
                let (released, wake) = &*release;
                let mut released = released.lock().unwrap_or_else(|e| e.into_inner());
                while !*released {
                    released = wake.wait(released).unwrap_or_else(|e| e.into_inner());
                }
            });
        }
        let held = BusyGlobalPool(release);
        let deadline = Instant::now() + WAIT;
        while started.load(Ordering::SeqCst) < threads {
            assert!(
                Instant::now() < deadline,
                "the global pool never took the holds"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        held
    }
}

impl Drop for BusyGlobalPool {
    fn drop(&mut self) {
        let (released, wake) = &*self.0;
        *released.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_sweep_stops_well_before_its_last_backtest() {
    // Every thread of rayon's global pool stays busy throughout, standing in for any other work in
    // the process: another run's backtests, or another test's. A run must neither wait for it to
    // start its backtests nor be held past a cancel by it.
    let _busy = BusyGlobalPool::hold().await;
    let t = TestServer::seeded(30).await;
    let mut ws = t.websocket().await;
    // 100 x 200 = 20 000 backtests of 720 bars, far more than run before the cancel lands.
    let job = t
        .start("/runs/sweep", &sweep_spec(30, (1, 100), (101, 300)))
        .await;

    // It reports progress while it runs, and its result is not there yet.
    loop {
        let Some(ServerMessage::Job { job: state }) = ws.next().await else {
            panic!("the topic closed");
        };
        if state.id == job && state.progress > 0.0 {
            assert_eq!(state.status, JobStatus::Running);
            break;
        }
    }
    let (status, body) = t.get(&format!("/jobs/{job}/result")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "job_not_done");

    let (status, _) = t.post(&format!("/jobs/{job}/cancel"), &json!(null)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let last = ws.follow(job).await.pop().unwrap();
    assert_eq!(last.status, JobStatus::Cancelled);
    assert!(last.result.is_none());
    // Counted, not timed: fewer than half its backtests ran.
    assert!(last.progress < 0.5, "stopped at {}", last.progress);

    let (_, runs) = t.get("/runs").await;
    assert_eq!(runs, json!([]), "a cancelled run is not saved");
    let (status, body) = t.get(&format!("/jobs/{job}/result")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "no_result", "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_run_job_answers_no_result_not_job_not_done() {
    let t = TestServer::seeded(3).await;
    let mut ws = t.websocket().await;
    // No parameter set can make a million trades, so the out-of-sample run fails.
    let mut sweep = sweep_spec(3, (3, 4), (10, 11));
    sweep["min_trades"] = json!(1_000_000);
    let job = t
        .start(
            "/runs/out-of-sample",
            &json!({ "sweep": sweep, "split": { "fraction": 0.7 } }),
        )
        .await;
    let last = ws.follow(job).await.pop().unwrap();
    assert_eq!(last.status, JobStatus::Failed);
    let (status, body) = t.get(&format!("/jobs/{job}/result")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "no_result", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("failed"),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_run_routes_need_the_token_and_unknown_ids_are_404() {
    let t = TestServer::seeded(1).await;
    let gets = ["/runs", "/runs/1762790000123-0a1b2c3d", "/jobs/1/result"];
    for path in gets {
        let (status, body) = t.send(t.client.get(t.url(path))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(body["error"]["code"], "unauthorized");
    }
    let posts = [
        "/runs/backtest",
        "/runs/sweep",
        "/runs/out-of-sample",
        "/runs/walk-forward",
    ];
    for path in posts {
        let request = t.client.post(t.url(path)).body("{}");
        let (status, _) = t.send(request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
    }

    for path in [
        "/runs/1762790000123-0a1b2c3d",
        "/runs/..%2F..%2Fetc",
        "/runs/not-a-run",
        "/jobs/7/result",
    ] {
        let (status, body) = t.get(path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
    let (status, list) = t.get("/runs").await;
    assert_eq!((status, list), (StatusCode::OK, json!([])));
}

/// Fills both run slots with jobs that hold them, doing no work, until each is cancelled, and
/// waits until both are running. They are run jobs, so they also take two places in the queue.
async fn hold_the_run_slots(t: &TestServer, ws: &mut Ws) -> [u64; 2] {
    let hold = || {
        let request = JobRequest::Backtest(config(InstrumentKind::Spot, 10));
        let job = t.server.jobs().submit(request, |ctx| {
            loop {
                ctx.checkpoint()?;
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        job.expect("accepted").id
    };
    let busy = [hold(), hold()];
    let mut running = Vec::new();
    while running.len() < busy.len() {
        let Some(ServerMessage::Job { job }) = ws.next().await else {
            panic!("the topic closed");
        };
        if busy.contains(&job.id) && job.status == JobStatus::Running && !running.contains(&job.id)
        {
            running.push(job.id);
        }
    }
    busy
}

/// Job `job` once it has finished.
async fn finished(t: &TestServer, job: u64) -> Value {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        let (_, state) = t.get(&format!("/jobs/{job}")).await;
        if ["done", "failed", "cancelled"].contains(&state["status"].as_str().unwrap_or_default()) {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("job {job} never finished");
}

async fn cancel(t: &TestServer, job: u64) -> Value {
    let (status, body) = t.post(&format!("/jobs/{job}/cancel"), &json!(null)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    body
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_beyond_the_queue_limit_are_429_and_a_cancelled_queued_run_makes_room() {
    let t = TestServer::seeded(30).await;
    let mut ws = t.websocket().await;
    let busy = hold_the_run_slots(&t, &mut ws).await;
    let backtest = serde_json::to_value(config(InstrumentKind::Spot, 10)).unwrap();
    let mut queued = Vec::new();
    for _ in busy.len()..MAX_RUN_JOBS {
        queued.push(t.start("/runs/backtest", &backtest).await);
    }
    for path in ["/runs/backtest", "/runs/sweep"] {
        let body = if path == "/runs/sweep" {
            sweep_spec(3, (3, 4), (10, 11))
        } else {
            backtest.clone()
        };
        let (status, body) = t.post(path, &body).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{path}: {body}");
        assert_eq!(body["error"]["code"], "too_many_runs");
    }
    let (_, jobs) = t.get("/jobs").await;
    assert_eq!(jobs.as_array().map(Vec::len), Some(MAX_RUN_JOBS));

    // A queued run is cancelled at once, and its place is free for another.
    let cancelled = cancel(&t, queued[0]).await;
    assert_eq!(cancelled["status"], "cancelled");
    queued[0] = t.start("/runs/backtest", &backtest).await;
    for &job in &queued {
        let (_, state) = t.get(&format!("/jobs/{job}")).await;
        assert_eq!(state["status"], "queued", "{state}");
    }

    for &job in queued.iter().chain(&busy) {
        cancel(&t, job).await;
    }
    for job in busy {
        assert_eq!(finished(&t, job).await["status"], "cancelled");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_run_loads_its_data_only_when_it_starts() {
    let t = TestServer::seeded(30).await;
    let mut ws = t.websocket().await;
    let busy = hold_the_run_slots(&t, &mut ws).await;
    let job = t
        .start(
            "/runs/backtest",
            &serde_json::to_value(config(InstrumentKind::Spot, 10)).unwrap(),
        )
        .await;
    let (_, state) = t.get(&format!("/jobs/{job}")).await;
    assert_eq!(state["status"], "queued", "{state}");

    // The spot candles go while the run waits; a run holding data loaded on request would not
    // notice.
    let id = instrument(InstrumentKind::Spot).id;
    let file = t
        .store
        .candle_path(&id, td_data::time::Month::containing(t0()))
        .unwrap();
    std::fs::remove_file(file).unwrap();
    // One slot frees, and the queued run starts on it while the other is still held.
    cancel(&t, busy[0]).await;
    let last = ws.follow(job).await.pop().unwrap();
    let (_, held) = t.get(&format!("/jobs/{}", busy[1])).await;
    assert_eq!(held["status"], "running", "{held}");
    cancel(&t, busy[1]).await;
    assert_eq!(last.status, JobStatus::Failed, "{last:?}");
    let error = last.error.expect("an error");
    assert_eq!(error.code, "data_missing");
    assert!(
        error.message.contains("changed after the run was accepted")
            && error.message.contains("binance:BTCUSDT:spot"),
        "{}",
        error.message
    );
}
