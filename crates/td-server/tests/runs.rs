//! Drives the run routes over real HTTP and WebSocket against a server started in-process on
//! 127.0.0.1:0, with a temporary data directory seeded with 1-minute candles: no network.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, Rounding, Side,
    Timeframe, Venue,
};
use td_data::time::{DAY, MINUTE, Month, parse_date};
use td_data::{DataError, DownloadReport, InstrumentList, Store};
use td_engine::{BacktestConfig, BacktestResult, Params, SavedRun};
use td_server::api::auth::Token;
use td_server::api::data::GapInfo;
use td_server::api::jobs::{JobInfo, JobRequest, JobResult, JobStatus};
const MAX_RUN_JOBS: usize = 8;
use td_server::api::runs::{DataCutoff, RunDetail, check_run_data, load_run_data};
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

/// What [`Filling`] does when a run asks it for candles.
#[derive(Clone, Copy)]
enum Fill {
    /// Writes the minutes of [`minutes`] in the range asked for.
    Minutes,
    /// Writes nothing, as a venue that has nothing there.
    Nothing,
    /// Fails, as a source with no OANDA token does.
    Fails,
}

/// A source for runs that download: it records each candle range asked of it.
struct Filling {
    fill: Fill,
    asked: Mutex<Vec<(Millis, Millis)>>,
}

impl Filling {
    fn new(fill: Fill) -> Arc<Filling> {
        Arc::new(Filling {
            fill,
            asked: Mutex::new(Vec::new()),
        })
    }
}

impl MarketSource for Filling {
    fn instruments(&self, _: Venue, _: InstrumentKind) -> Result<InstrumentList, DataError> {
        Err(DataError::NoDataDir)
    }
    fn candles(
        &self,
        store: &Store,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<DownloadReport, DataError> {
        self.asked.lock().unwrap().push((from, to));
        let candles: Vec<Candle> = match self.fill {
            Fill::Minutes => minutes(40, 0..0)
                .into_iter()
                .filter(|c| (from..to).contains(&c.open_time))
                .collect(),
            Fill::Nothing => Vec::new(),
            Fill::Fails => return Err(DataError::MissingOandaToken),
        };
        store.write_candles(id, &candles)?;
        Ok(DownloadReport {
            months: vec![(Month::containing(from), candles.len())],
            ..DownloadReport::default()
        })
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
        TestServer::seeded_with(days, None).await
    }

    /// [`TestServer::seeded`], on a server started with a data cutoff when one is given.
    async fn seeded_with(days: i64, cutoff: Option<&str>) -> TestServer {
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
        TestServer::on_with(dir, store, cutoff).await
    }

    /// A server on a store already seeded.
    async fn on(dir: TempDir, store: Store) -> TestServer {
        TestServer::serve(dir, store, Arc::new(NoSource), None).await
    }

    /// [`TestServer::on`], started with `TD_DATA_CUTOFF` set to `cutoff` when one is given.
    async fn on_with(dir: TempDir, store: Store, cutoff: Option<&str>) -> TestServer {
        TestServer::serve(dir, store, Arc::new(NoSource), cutoff).await
    }

    /// A server on a store already seeded, downloading through `source`.
    async fn with_source(dir: TempDir, store: Store, source: Arc<dyn MarketSource>) -> TestServer {
        TestServer::serve(dir, store, source, None).await
    }

    /// A server on a store already seeded, downloading through `source`, with the data cutoff
    /// `cutoff` when one is given.
    async fn serve(
        dir: TempDir,
        store: Store,
        source: Arc<dyn MarketSource>,
        cutoff: Option<&str>,
    ) -> TestServer {
        let server = Server::start(ApiConfig {
            store: store.clone(),
            source,
            live: std::sync::Arc::new(NoLive),
            token: Token::generate().expect("rng"),
            port: 0,
            data_cutoff: cutoff.map(|c| DataCutoff::parse(c).expect("a cutoff")),
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
        account_currency: td_core::AccountCurrency::Quote,
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
async fn a_range_nothing_can_be_downloaded_for_is_422_data_missing_and_starts_no_job() {
    let t = TestServer::seeded(3).await;
    // Days after now, which no venue has yet.
    let future = parse_date("2100-01-01").unwrap();
    let mut beyond = serde_json::to_value(BacktestConfig {
        from: future,
        to: future + 2 * DAY,
        ..config(InstrumentKind::Spot, 3)
    })
    .unwrap();
    let (status, body) = t.post("/runs/backtest", &beyond).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "data_missing");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("binance:BTCUSDT:spot")
            && message.contains("from 2100-01-01 00:00 to 2100-01-03 00:00 UTC"),
        "{message}"
    );
    // An instrument with nothing stored, before the venue's earliest data.
    let early = parse_date("2016-01-01").unwrap();
    beyond["instrument"]["id"]["symbol"] = json!("ETHUSDT");
    beyond["from"] = json!(early);
    beyond["to"] = json!(early + 5 * HOUR);
    let (status, body) = t.post("/runs/backtest", &beyond).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "data_missing");
    let mut sweep = sweep_spec(4, (3, 4), (10, 11));
    sweep["base"]["from"] = json!(future);
    sweep["base"]["to"] = json!(future + 4 * DAY);
    let (status, body) = t.post("/runs/sweep", &sweep).await;
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

/// With a data cutoff in force, every run kind whose range ends after the cutoff date is a 422
/// that names the cutoff and starts no job, though the store holds the bars; a run ending exactly
/// at the end of the cutoff date runs, and its record says the cutoff was in force and which code
/// it ran. `GET /health` says the cutoff is in force, and says nothing on a server without one.
#[tokio::test(flavor = "multi_thread")]
async fn a_data_cutoff_refuses_runs_past_it_and_is_recorded_on_the_runs_it_allows() {
    let t = TestServer::seeded_with(7, Some("2024-03-05")).await;
    let sweep = sweep_spec(6, (3, 4), (10, 11));
    let past = [
        (
            "/runs/backtest",
            serde_json::to_value(config(InstrumentKind::Spot, 6)).unwrap(),
        ),
        ("/runs/sweep", sweep.clone()),
        (
            "/runs/out-of-sample",
            json!({ "sweep": sweep, "split": { "fraction": 0.7 } }),
        ),
        (
            "/runs/walk-forward",
            json!({
                "sweep": sweep,
                "windows": { "train_bars": 72, "test_bars": 24, "step_bars": 24, "mode": "rolling" },
            }),
        ),
    ];
    for (path, body) in &past {
        let (status, body) = t.post(path, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}: {body}");
        assert_eq!(body["error"]["code"], "after_cutoff", "{path}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("ends at 2024-03-07 00:00 UTC")
                && message.contains("TD_DATA_CUTOFF=2024-03-05")
                && message.contains("End the range at 2024-03-06 00:00 UTC or earlier"),
            "{path}: {message}"
        );
    }
    let (_, jobs) = t.get("/jobs").await;
    assert_eq!(jobs, json!([]), "no job was created");

    let within = serde_json::to_value(config(InstrumentKind::Spot, 5)).unwrap();
    let detail = saved_run(&t, "/runs/backtest", &within).await;
    assert_eq!(detail.data_cutoff.as_deref(), Some("2024-03-05"));
    assert_eq!(
        detail.code_hash.as_deref(),
        td_engine::code_hash("sma_cross")
    );
    assert!(detail.code_hash.is_some());
    assert_eq!(detail.code_matches, Some(true));
    let (status, reopened) = t.get(&format!("/runs/{}", detail.id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reopened["data_cutoff"], "2024-03-05");
    assert_eq!(reopened["code_matches"], true);

    let (status, health) = t.get("/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["data_cutoff"], "2024-03-05");
    let open = TestServer::seeded(1).await;
    let (_, health) = open.get("/health").await;
    assert_eq!(health["data_cutoff"], Value::Null, "{health}");
}

/// A run saved before runs recorded their code, or by code that has changed since, says so on
/// `GET /runs/{id}`: no record is unknown (null), never a match, and a different hash is false.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_run_says_whether_the_code_it_ran_still_matches() {
    let t = TestServer::seeded(3).await;
    let body = serde_json::to_value(config(InstrumentKind::Spot, 3)).unwrap();
    let detail = saved_run(&t, "/runs/backtest", &body).await;
    assert_eq!(detail.data_cutoff, None, "no cutoff in force");
    let folder = t.store.root().join("runs").join(detail.id.as_str());
    let record = folder.join("record.json");
    let saved: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    assert_eq!(
        saved["code_hash"].as_str(),
        td_engine::code_hash("sma_cross")
    );

    std::fs::write(&record, r#"{ "code_hash": "0000000000000000" }"#).unwrap();
    let (_, changed) = t.get(&format!("/runs/{}", detail.id)).await;
    assert_eq!(changed["code_matches"], false, "{changed}");
    assert_eq!(changed["code_hash"], "0000000000000000");

    std::fs::remove_file(&record).unwrap();
    let (status, old) = t.get(&format!("/runs/{}", detail.id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(old["code_matches"], Value::Null, "{old}");
    assert_eq!(old["code_hash"], Value::Null);
    assert_eq!(old["data_cutoff"], Value::Null);
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
    let states = ws.follow(job).await;
    let (_, held) = t.get(&format!("/jobs/{}", busy[1])).await;
    assert_eq!(held["status"], "running", "{held}");
    cancel(&t, busy[1]).await;
    // The run finds the month gone and goes to download it, which this source refuses.
    assert!(
        states
            .iter()
            .any(|s| s.message == "downloading the data: binance:BTCUSDT:spot 2024-03"),
        "{states:?}"
    );
    let last = states.last().unwrap();
    assert_eq!(last.status, JobStatus::Failed, "{last:?}");
    let error = last.error.as_ref().expect("an error");
    assert_eq!(error.message, DataError::NoDataDir.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_on_a_range_the_store_lacks_downloads_it_first_then_runs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::new(dir.path());
    let id = instrument(InstrumentKind::Spot).id;
    store.write_candles(&id, &minutes(3, 0..0)).expect("seeds");
    let source = Filling::new(Fill::Minutes);
    let t = TestServer::with_source(dir, store, source.clone()).await;
    let mut ws = t.websocket().await;

    // Five days, two of them not stored: accepted, as the job downloads them.
    let body = serde_json::to_value(config(InstrumentKind::Spot, 5)).unwrap();
    let job = t.start("/runs/backtest", &body).await;
    let states = ws.follow(job).await;
    let messages: Vec<&str> = states.iter().map(|s| s.message.as_str()).collect();
    let downloading = "downloading the data: binance:BTCUSDT:spot 2024-03";
    let at = messages.iter().position(|m| *m == downloading);
    let ran = messages.iter().position(|m| *m == "running the backtest");
    assert!(at.is_some() && at < ran, "{messages:?}");
    let last = states.last().unwrap();
    assert_eq!(last.status, JobStatus::Done, "{last:?}");
    assert_eq!(
        *source.asked.lock().unwrap(),
        [(t0() + 3 * DAY, t0() + 5 * DAY)]
    );
    let data = t.result(job).await.data.expect("a data report");
    assert_eq!((data.bars, data.gaps.len()), (5 * 24, 0));

    // The range is stored now: a second run downloads nothing.
    let again = t.start("/runs/backtest", &body).await;
    assert_eq!(
        ws.follow(again).await.last().unwrap().status,
        JobStatus::Done
    );
    assert_eq!(source.asked.lock().unwrap().len(), 1);
}

/// With a data cutoff and nothing stored, a run ending after the cutoff is refused as past it
/// when it is requested, and nothing is downloaded for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_past_the_cutoff_is_refused_before_anything_is_downloaded() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::new(dir.path());
    let source = Filling::new(Fill::Minutes);
    let t = TestServer::serve(dir, store, source.clone(), Some("2024-03-05")).await;
    let body = serde_json::to_value(config(InstrumentKind::Spot, 7)).unwrap();
    let (status, body) = t.post("/runs/backtest", &body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "after_cutoff");
    let (_, jobs) = t.get("/jobs").await;
    assert_eq!(jobs, json!([]), "no job was created");
    assert_eq!(*source.asked.lock().unwrap(), []);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_or_short_download_fails_the_run_and_nothing_is_run() {
    for (fill, code, message) in [
        (
            Fill::Fails,
            "bad_request",
            DataError::MissingOandaToken.to_string(),
        ),
        (
            Fill::Nothing,
            "data_missing",
            "the download did not fill the range, so it was not run: the store has no \
             binance:BTCUSDT:spot candles from 2024-03-04 00:00 to 2024-03-06 00:00 UTC; download \
             them first"
                .to_string(),
        ),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::new(dir.path());
        let id = instrument(InstrumentKind::Spot).id;
        store.write_candles(&id, &minutes(3, 0..0)).expect("seeds");
        let t = TestServer::with_source(dir, store, Filling::new(fill)).await;
        let mut ws = t.websocket().await;
        let body = serde_json::to_value(config(InstrumentKind::Spot, 5)).unwrap();
        let job = t.start("/runs/backtest", &body).await;
        let states = ws.follow(job).await;
        assert!(
            states.iter().all(|s| s.message != "running the backtest"),
            "{states:?}"
        );
        let last = states.last().unwrap();
        assert_eq!(last.status, JobStatus::Failed, "{last:?}");
        let error = last.error.as_ref().expect("an error");
        assert_eq!((error.code.as_str(), &error.message), (code, &message));
        let (_, runs) = t.get("/runs").await;
        assert_eq!(runs, json!([]), "a failed run is not saved");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_fx_run_whose_stored_month_has_no_asks_is_422_data_missing_and_starts_no_job() {
    let t = TestServer::seeded(3).await;
    let (spot, fx) = (
        instrument(InstrumentKind::Spot).id,
        instrument(InstrumentKind::Fx).id,
    );
    // The spot month file copied into the FX folder: bids with no ask columns.
    let march = Month::containing(t0());
    let path = t.store.candle_path(&fx, march).expect("a path");
    std::fs::create_dir_all(path.parent().expect("a folder")).expect("makes the folder");
    std::fs::copy(t.store.candle_path(&spot, march).expect("a path"), &path).expect("copies");

    let body = serde_json::to_value(config(InstrumentKind::Fx, 3)).unwrap();
    let (status, body) = t.post("/runs/backtest", &body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "data_missing");
    assert_eq!(
        body["error"]["message"],
        "the store's binance:BTCUSDT:fx candles for 2024-03 have no ask prices; delete the month \
         and download it again"
    );
    let (_, jobs) = t.get("/jobs").await;
    assert_eq!(jobs, json!([]), "no job was created");
}

#[test]
fn an_fx_week_with_its_weekend_closed_passes_the_data_check_with_no_gap_or_dropped_bucket() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::new(dir.path());
    // Monday 8 January 2024 to the Monday after, without the FX weekend (Friday 22:00 UTC to
    // Sunday 22:00 UTC).
    let monday = parse_date("2024-01-08").unwrap();
    let bids: Vec<Candle> = (0..7 * DAY / MINUTE)
        .map(|i| monday + i * MINUTE)
        .filter(|&t| InstrumentKind::Fx.is_open(t))
        .map(|t| {
            let price = 150.0 + tri((t - monday) as f64 / (97.0 * MINUTE as f64));
            Candle {
                open_time: t,
                open: price,
                high: price + 0.02,
                low: price - 0.02,
                close: price + 0.01,
                volume: 1.0,
                quote_volume: price,
            }
        })
        .collect();
    let asks: Vec<Candle> = bids
        .iter()
        .map(|b| Candle {
            open: b.open + 0.01,
            high: b.high + 0.01,
            low: b.low + 0.01,
            close: b.close + 0.01,
            ..*b
        })
        .collect();
    let fx = config(InstrumentKind::Fx, 7);
    let fx = BacktestConfig {
        from: monday,
        to: monday + 7 * DAY,
        ..fx
    };
    store
        .write_quotes(&fx.instrument.id, &bids, &asks)
        .expect("seeds");

    check_run_data(&store, &fx, None).expect("the weekend is closed, not missing");
    let data = load_run_data(&store, &fx, None).expect("loads");
    assert_eq!(data.report.gaps, []);
    assert_eq!(data.report.dropped, [] as [Millis; 0]);
    assert_eq!(data.report.bars, 5 * 24, "five open days of hourly bars");

    // The same minutes as spot, which never closes: the weekend is two days missing.
    let spot = BacktestConfig {
        instrument: instrument(InstrumentKind::Spot),
        ..fx.clone()
    };
    store
        .write_candles(&spot.instrument.id, &bids)
        .expect("seeds");
    match check_run_data(&store, &spot, None) {
        Err(e) => assert_eq!(e.body.code, "data_missing", "{e:?}"),
        Ok(()) => panic!("spot with the weekend missing must be refused"),
    }
}

/// Monday 8 January 2024. The FX tests run from it for [`FX_DAYS`] days, across the weekend of 12
/// to 14 January, closed from Friday 22:00 to Sunday 22:00 UTC.
fn fx_t0() -> Millis {
    parse_date("2024-01-08").unwrap()
}

const FX_DAYS: i64 = 9;

/// How far the ask is above the bid, on every price of every minute: five ticks.
const SPREAD: f64 = 0.05;

fn fx_instrument(symbol: &str) -> Instrument {
    let mut fx = instrument(InstrumentKind::Fx);
    fx.id.symbol = symbol.into();
    fx
}

/// The bid minute at `t`: waves around 100.
fn fx_bid(t: Millis) -> Candle {
    let price = |i: i64| {
        let x = i as f64;
        100.0 + 2.0 * tri(x / 780.0) + 0.7 * tri(x / 97.0)
    };
    let i = (t - fx_t0()) / MINUTE;
    let (open, close) = (price(i), price(i + 1));
    Candle {
        open_time: t,
        open,
        high: open.max(close) + 0.02,
        low: open.min(close) - 0.02,
        close,
        volume: 1.0,
        quote_volume: close,
    }
}

/// The ask minute at `t`, `spread` above the bid, with no volume, as the store reads it back.
fn fx_ask(t: Millis, spread: f64) -> Candle {
    let bid = fx_bid(t);
    Candle {
        open: bid.open + spread,
        high: bid.high + spread,
        low: bid.low + spread,
        close: bid.close + spread,
        volume: 0.0,
        quote_volume: 0.0,
        ..bid
    }
}

/// The open minutes of `[from, to)`: the FX weekend is absent.
fn open_minutes(from: Millis, to: Millis) -> Vec<Millis> {
    (from..to)
        .step_by(MINUTE as usize)
        .filter(|&t| InstrumentKind::Fx.is_open(t))
        .collect()
}

/// Writes the bids and asks of `times` for `symbol`, the asks `spread` above the bids.
fn write_fx(store: &Store, symbol: &str, times: &[Millis], spread: f64) {
    let bids: Vec<Candle> = times.iter().map(|&t| fx_bid(t)).collect();
    let asks: Vec<Candle> = times.iter().map(|&t| fx_ask(t, spread)).collect();
    store
        .write_quotes(&fx_instrument(symbol).id, &bids, &asks)
        .expect("seeds");
}

/// Hourly bars from complete hours of minutes, built by hand: the first open, the highest high,
/// the lowest low, the last close and the summed volumes.
fn hourly(minutes: &[Candle]) -> Vec<Candle> {
    minutes
        .chunk_by(|a, b| a.open_time / HOUR == b.open_time / HOUR)
        .map(|hour| {
            let mut bar = hour[0];
            for m in &hour[1..] {
                bar.high = bar.high.max(m.high);
                bar.low = bar.low.min(m.low);
                bar.close = m.close;
                bar.volume += m.volume;
                bar.quote_volume += m.quote_volume;
            }
            bar
        })
        .collect()
}

/// A server whose store holds [`FX_DAYS`] days of FX minutes from [`fx_t0`], weekend absent:
/// `EURUSD` with its asks [`SPREAD`] above the bids, and `EURUSDZ` with the same bids and asks
/// equal to them.
async fn fx_server() -> TestServer {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::new(dir.path());
    let times = open_minutes(fx_t0(), fx_t0() + FX_DAYS * DAY);
    write_fx(&store, "EURUSD", &times, SPREAD);
    write_fx(&store, "EURUSDZ", &times, 0.0);
    TestServer::on(dir, store).await
}

fn fx_config(symbol: &str, timeframe: Timeframe) -> BacktestConfig {
    BacktestConfig {
        instrument: fx_instrument(symbol),
        timeframe,
        from: fx_t0(),
        to: fx_t0() + FX_DAYS * DAY,
        slippage_bps: 0.0,
        ..config(InstrumentKind::Fx, FX_DAYS)
    }
}

/// An hourly sweep over fast x slow on an FX symbol, with shorts: four backtests.
fn fx_sweep(symbol: &str) -> Value {
    let mut base = serde_json::to_value(fx_config(symbol, Timeframe::H1)).unwrap();
    base["params"] = json!({ "allow_short": true });
    json!({
        "base": base,
        "axes": [
            { "name": "fast", "space": { "kind": "range", "from": 3, "to": 4, "step": 1 } },
            { "name": "slow", "space": { "kind": "range", "from": 10, "to": 11, "step": 1 } },
        ],
        "mode": "grid",
        "objective": { "metric": "total_return", "direction": "maximize" },
        "min_trades": 0,
    })
}

/// Starts a run through the API, waits for it and returns what it saved.
async fn saved_run(t: &TestServer, path: &str, body: &Value) -> RunDetail {
    let job = t.start(path, body).await;
    let state = finished(t, job).await;
    assert_eq!(state["status"], "done", "{state}");
    t.result(job).await
}

async fn fx_backtest(t: &TestServer, config: &BacktestConfig) -> (RunDetail, BacktestResult) {
    let detail = saved_run(t, "/runs/backtest", &serde_json::to_value(config).unwrap()).await;
    let SavedRun::Backtest(result) = &detail.run else {
        panic!("a saved backtest");
    };
    let result = (**result).clone();
    (detail, result)
}

/// Asserts every fill of `result` is at its own side's open of the bar it fills in: a buy at the
/// ask open rounded up to the tick, a sell at the bid open rounded down. Returns how many buys and
/// sells there are.
fn assert_fills_on_their_side(result: &BacktestResult, spread: f64) -> (usize, usize) {
    let inst = &result.config.instrument;
    let (mut buys, mut sells) = (0, 0);
    for fill in &result.fills {
        let expected = match fill.side {
            Side::Buy => {
                buys += 1;
                inst.round_price(fx_ask(fill.time, spread).open, Rounding::Up)
            }
            Side::Sell => {
                sells += 1;
                inst.round_price(fx_bid(fill.time).open, Rounding::Down)
            }
        };
        assert_eq!(fill.price, expected, "{fill:?}");
    }
    (buys, sells)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_fx_backtest_across_a_weekend_buys_at_the_ask_and_sells_at_the_bid_on_1m_and_1h() {
    let t = fx_server().await;
    let times = open_minutes(fx_t0(), fx_t0() + FX_DAYS * DAY);
    let bids: Vec<Candle> = times.iter().map(|&t| fx_bid(t)).collect();
    let asks: Vec<Candle> = times.iter().map(|&t| fx_ask(t, SPREAD)).collect();

    // On 1m the bars are the stored minutes themselves.
    let (detail, m1) = fx_backtest(&t, &fx_config("EURUSD", Timeframe::M1)).await;
    let data = detail.data.expect("a data report");
    assert_eq!(
        (data.gaps.len(), data.dropped.len()),
        (0, 0),
        "the weekend is closed, not missing"
    );
    assert_eq!(data.bars, times.len());
    let (buys, sells) = assert_fills_on_their_side(&m1, SPREAD);
    assert!(buys > 0 && sells > 0, "{buys} buys and {sells} sells");
    // One of each by hand: a buy is five ticks above the bid open of its minute, a sell on it.
    let buy = m1.fills.iter().find(|f| f.side == Side::Buy).unwrap();
    let bid_open = fx_bid(buy.time).open;
    assert!(
        buy.price >= bid_open + SPREAD && buy.price < bid_open + SPREAD + 0.01,
        "{buy:?} against the bid open {bid_open}"
    );
    let sell = m1.fills.iter().find(|f| f.side == Side::Sell).unwrap();
    let bid_open = fx_bid(sell.time).open;
    assert!(
        sell.price <= bid_open && sell.price > bid_open - 0.01,
        "{sell:?} against the bid open {bid_open}"
    );
    assert_eq!(
        m1.data_fingerprint,
        td_engine::data_fingerprint(&bids, &asks, &[])
    );

    // On 1h the asks are resampled over the same minutes as the bids.
    let (detail, h1) = fx_backtest(&t, &fx_config("EURUSD", Timeframe::H1)).await;
    let (hourly_bids, hourly_asks) = (hourly(&bids), hourly(&asks));
    assert_eq!(
        hourly_bids.len(),
        (FX_DAYS * 24 - 48) as usize,
        "one weekend closed"
    );
    assert_eq!(detail.data.expect("a data report").bars, hourly_bids.len());
    let (buys, sells) = assert_fills_on_their_side(&h1, SPREAD);
    assert!(buys > 0 && sells > 0, "{buys} buys and {sells} sells");
    // The fingerprint covers every bar and ask bar used, so it pins each resampled ask bar's OHLC.
    assert_eq!(
        h1.data_fingerprint,
        td_engine::data_fingerprint(&hourly_bids, &hourly_asks, &[])
    );
    assert_ne!(
        h1.data_fingerprint,
        td_engine::data_fingerprint(&hourly_bids, &[], &[]),
        "the asks are in the fingerprint"
    );
    let rerun = td_engine::rerun(&h1, &hourly_bids, &hourly_asks, &[]).unwrap();
    assert!(rerun.fingerprint_matches && rerun.engine_matches);
    assert_eq!(rerun.result, h1);

    // Both saved runs reopen as matching today's data and engine.
    let (_, list) = t.get("/runs").await;
    assert_eq!(list.as_array().map(Vec::len), Some(2));
    for row in list.as_array().unwrap() {
        let (status, reopened) = t
            .get(&format!("/runs/{}", row["id"].as_str().unwrap()))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            reopened["check"],
            json!({ "data_matches": true, "engine_matches": true })
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fx_sweeps_out_of_sample_and_walk_forward_runs_trade_on_the_asks() {
    let t = fx_server().await;

    // Every row pays the spread: the same grid on the same bids with no spread does better.
    let sweep = |detail: RunDetail| match detail.run {
        SavedRun::Sweep(result) => result,
        other => panic!("a sweep: {:?}", other.kind()),
    };
    let spread = sweep(saved_run(&t, "/runs/sweep", &fx_sweep("EURUSD")).await);
    let none = sweep(saved_run(&t, "/runs/sweep", &fx_sweep("EURUSDZ")).await);
    assert_eq!(spread.rows.len(), 4);
    for (row, free) in spread.rows.iter().zip(&none.rows) {
        assert_eq!(row.params, free.params);
        let (m, f) = (
            row.metrics.as_ref().unwrap(),
            free.metrics.as_ref().unwrap(),
        );
        assert!(m.trades > 0, "{row:?}");
        assert!(
            m.total_return.unwrap() < f.total_return.unwrap(),
            "{row:?} against {free:?}"
        );
    }
    let best = spread.best.as_ref().expect("a best run");
    assert_fills_on_their_side(best, SPREAD);

    let oos = json!({ "sweep": fx_sweep("EURUSD"), "split": { "fraction": 0.7 } });
    let SavedRun::Oos(oos) = saved_run(&t, "/runs/out-of-sample", &oos).await.run else {
        panic!("an out-of-sample run");
    };
    assert_fills_on_their_side(&oos.out_of_sample, SPREAD);

    let wf = json!({
        "sweep": fx_sweep("EURUSD"),
        "windows": { "train_bars": 72, "test_bars": 24, "step_bars": 24, "mode": "rolling" },
    });
    let SavedRun::WalkForward(wf) = saved_run(&t, "/runs/walk-forward", &wf).await.run else {
        panic!("a walk-forward run");
    };
    // 168 hourly bars hold four windows.
    assert_eq!(wf.windows.len(), 4);
    let (mut buys, mut sells) = (0, 0);
    for window in &wf.windows {
        let (b, s) = assert_fills_on_their_side(&window.out_of_sample, SPREAD);
        (buys, sells) = (buys + b, sells + s);
    }
    assert!(buys > 0 && sells > 0, "{buys} buys and {sells} sells");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_fx_run_goes_over_a_holiday_but_two_days_missing_or_a_spot_day_is_refused() {
    // FX_DAYS days of FX minutes from Monday 8 January 2024. EURUSD lacks 25 hours of open time
    // from Tuesday 22:00 UTC, as OANDA's minutes lack a holiday such as 25 to 26 December 2016.
    // USDJPY lacks the hour before the weekend's close, Friday 22:00 UTC, and the 25 hours from
    // its open, Sunday 22:00 UTC: 26 hours of open time, three days and two hours from end to end.
    // GBPUSD lacks two days from Tuesday 00:00 UTC.
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::new(dir.path());
    let (from, to) = (fx_t0(), fx_t0() + FX_DAYS * DAY);
    let holiday = from + DAY + 22 * HOUR..from + 2 * DAY + 23 * HOUR;
    let (close, reopen) = (from + 4 * DAY + 22 * HOUR, from + 6 * DAY + 22 * HOUR);
    let two_days = from + DAY..from + 3 * DAY;
    let without = |hole: std::ops::Range<Millis>| -> Vec<Millis> {
        let open = open_minutes(from, to).into_iter();
        open.filter(|t| !hole.contains(t)).collect()
    };
    write_fx(&store, "EURUSD", &without(holiday.clone()), SPREAD);
    write_fx(
        &store,
        "USDJPY",
        &without(close - HOUR..reopen + 25 * HOUR),
        SPREAD,
    );
    write_fx(&store, "GBPUSD", &without(two_days), SPREAD);
    // Three days of spot minutes from 1 March 2024, the second day missing.
    let spot = config(InstrumentKind::Spot, 3);
    store
        .write_candles(&spot.instrument.id, &minutes(3, t0() + DAY..t0() + 2 * DAY))
        .expect("seeds");
    let t = TestServer::on(dir, store).await;

    // The holiday is run through, and listed in the run's data report with its hours.
    let (detail, _) = fx_backtest(&t, &fx_config("EURUSD", Timeframe::H1)).await;
    let data = detail.data.expect("a data report");
    let gap = GapInfo {
        from: holiday.start,
        to: holiday.end,
        minutes: 25 * 60,
    };
    assert_eq!(data.gaps, [gap]);
    assert_eq!(data.dropped.len(), 25);
    // USDJPY's two gaps hold under two days of open time together, as the weekend between them
    // does not count: run, and listed apart. The Friday hour is a bucket dropped as well.
    let (detail, _) = fx_backtest(&t, &fx_config("USDJPY", Timeframe::H1)).await;
    let data = detail.data.expect("a data report");
    let gaps = [
        GapInfo {
            from: close - HOUR,
            to: close,
            minutes: 60,
        },
        GapInfo {
            from: reopen,
            to: reopen + 25 * HOUR,
            minutes: 25 * 60,
        },
    ];
    assert_eq!(data.gaps, gaps);
    assert_eq!(data.dropped.len(), 26);

    // Two days missing for FX, and a day for spot, are refused by the check: a run job would
    // download them first.
    let refused = [
        (
            fx_config("GBPUSD", Timeframe::H1),
            "the store has no binance:GBPUSD:fx candles from 2024-01-09 00:00 to 2024-01-11 \
             00:00 UTC; download them first",
        ),
        (
            spot,
            "the store has no binance:BTCUSDT:spot candles from 2024-03-02 00:00 to 2024-03-03 \
             00:00 UTC; download them first",
        ),
    ];
    for (config, message) in refused {
        let e = check_run_data(&t.store, &config, None).expect_err("refused");
        assert_eq!(e.body.code, "data_missing");
        assert_eq!(e.body.message, message);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fx_minutes_missing_either_side_of_a_weekend_are_judged_together_and_refused() {
    // The weekend of 12 to 14 January 2024 is closed from Friday 22:00 to Sunday 22:00 UTC.
    // EURUSD is stored from Monday 8 January to Wednesday 10 January 12:00 UTC, and USDJPY from
    // Tuesday 16 January 12:00 to Wednesday 17 January 12:00 UTC.
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::new(dir.path());
    let wednesday = fx_t0() + 2 * DAY + 12 * HOUR;
    let tuesday = fx_t0() + 8 * DAY + 12 * HOUR;
    let next_wednesday = fx_t0() + 9 * DAY + 12 * HOUR;
    write_fx(&store, "EURUSD", &open_minutes(fx_t0(), wednesday), SPREAD);
    write_fx(
        &store,
        "USDJPY",
        &open_minutes(tuesday, next_wednesday),
        SPREAD,
    );
    let t = TestServer::on(dir, store).await;

    let refused = [
        // To the next Wednesday 12:00: five days of open time missing from Wednesday 12:00, two
        // days and 10 hours before the weekend and two days and 14 hours after it.
        (
            "EURUSD",
            fx_t0(),
            "the store has no binance:EURUSD:fx candles from 2024-01-10 12:00 to 2024-01-17 \
             12:00 UTC; download them first",
        ),
        // From Thursday 11 January, five days and 12 hours before the stored minutes: three and
        // a half days of open time missing, 46 hours before the weekend and 38 after it, each
        // under two days on its own.
        (
            "USDJPY",
            fx_t0() + 3 * DAY,
            "the store has no binance:USDJPY:fx candles from 2024-01-11 00:00 to 2024-01-16 \
             12:00 UTC; download them first",
        ),
    ];
    for (symbol, from, message) in refused {
        let config = BacktestConfig {
            from,
            to: next_wednesday,
            ..fx_config(symbol, Timeframe::H1)
        };
        let e = check_run_data(&t.store, &config, None).expect_err("refused");
        assert_eq!(e.body.code, "data_missing", "{symbol}");
        assert_eq!(e.body.message, message);
    }
}

/// Sets the ask high of the minute at `open_time` in its stored month file to `high`, past the
/// store's checks.
fn plant_ask_high(store: &Store, id: &InstrumentId, open_time: Millis, high: f64) {
    use arrow::array::{Array, ArrayRef, Float64Array, Int64Array};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let path = store
        .candle_path(id, Month::containing(open_time))
        .expect("a path");
    let file = std::fs::File::open(&path).expect("the month file");
    let batches: Vec<RecordBatch> = ParquetRecordBatchReaderBuilder::try_new(file)
        .expect("parquet")
        .build()
        .expect("a reader")
        .collect::<Result<_, _>>()
        .expect("reads");
    let mut planted = 0;
    let batches: Vec<RecordBatch> = batches
        .into_iter()
        .map(|batch| {
            let column = |name: &str| batch.column_by_name(name).expect(name).clone();
            let times = column("open_time");
            let times = times.as_any().downcast_ref::<Int64Array>().expect("i64");
            let highs = column("ask_high");
            let highs = highs.as_any().downcast_ref::<Float64Array>().expect("f64");
            let highs: Float64Array = times
                .values()
                .iter()
                .zip(highs.values())
                .map(|(&t, &h)| {
                    planted += usize::from(t == open_time);
                    if t == open_time { high } else { h }
                })
                .collect();
            let mut columns = batch.columns().to_vec();
            let index = batch.schema().index_of("ask_high").expect("ask_high");
            columns[index] = std::sync::Arc::new(highs) as ArrayRef;
            RecordBatch::try_new(batch.schema(), columns).expect("a batch")
        })
        .collect();
    assert_eq!(planted, 1);
    let file = std::fs::File::create(&path).expect("rewrites");
    let mut writer = ArrowWriter::try_new(file, batches[0].schema(), None).expect("a writer");
    for batch in &batches {
        writer.write(batch).expect("writes");
    }
    writer.close().expect("closes");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crossed_stored_quote_fails_the_run_as_data_missing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::new(dir.path());
    let times = open_minutes(fx_t0(), fx_t0() + FX_DAYS * DAY);
    let bids: Vec<Candle> = times.iter().map(|&t| fx_bid(t)).collect();
    let mut asks: Vec<Candle> = times.iter().map(|&t| fx_ask(t, SPREAD)).collect();
    // One minute's ask high below its bid high. The store refuses that, so it is planted in the
    // month file after the write, as a month written before the store checked the high would hold
    // it; the engine refuses the bar on 1m. (On 1h the other minutes' asks lift the hour's ask
    // high back over its bid high.)
    let crossed = fx_t0() + DAY + 10 * HOUR + 30 * MINUTE;
    let i = times.iter().position(|&t| t == crossed).unwrap();
    // The bid's open, low and close, a zero spread the store allows, and a high a tick under the
    // bid's, which is still above the open and the close.
    asks[i] = Candle {
        volume: 0.0,
        quote_volume: 0.0,
        ..bids[i]
    };
    let id = fx_instrument("EURUSD").id;
    store.write_quotes(&id, &bids, &asks).expect("writes");
    plant_ask_high(&store, &id, crossed, bids[i].high - 0.01);
    let t = TestServer::on(dir, store).await;

    let config = fx_config("EURUSD", Timeframe::M1);
    let job = t
        .start("/runs/backtest", &serde_json::to_value(&config).unwrap())
        .await;
    let state = finished(&t, job).await;
    assert_eq!(state["status"], "failed", "{state}");
    assert_eq!(state["error"]["code"], "data_missing", "{state}");
    assert_eq!(
        state["error"]["message"],
        format!(
            "the store's bids and asks cannot be run: the ask for the bar at {crossed} is below \
             the bid; delete the months and download them again"
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn coverage_expects_the_open_minutes_so_a_complete_fx_month_is_whole() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::new(dir.path());
    let (jan, feb) = (
        parse_date("2024-01-01").unwrap(),
        parse_date("2024-02-01").unwrap(),
    );
    let times = open_minutes(jan, feb);
    // Four FX weekends lie wholly in January 2024, ending on the 7th, 14th, 21st and 28th, each 48
    // hours: 31 days less 192 hours are open.
    let open = (31 * 24 - 192) * 60;
    assert_eq!(times.len(), open as usize);
    write_fx(&store, "EURUSD", &times, SPREAD);
    // The same month less the open hour from 10:00 on Wednesday 10 January.
    let hole = parse_date("2024-01-10").unwrap() + 10 * HOUR;
    let holed: Vec<Millis> = times
        .iter()
        .copied()
        .filter(|t| !(hole..hole + HOUR).contains(t))
        .collect();
    write_fx(&store, "EURUSDH", &holed, SPREAD);
    // A whole crypto month: March 2024.
    let spot = instrument(InstrumentKind::Spot).id;
    store
        .write_candles(&spot, &minutes(31, 0..0))
        .expect("seeds");
    let t = &TestServer::on(dir, store).await;

    let month = |instrument: &'static str| async move {
        let (status, body) = t
            .get(&format!("/data/coverage?instrument={instrument}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["months"].as_array().map(Vec::len), Some(1), "{body}");
        let m = &body["months"][0];
        (
            m["minutes"].as_i64().unwrap(),
            m["month_minutes"].as_i64().unwrap(),
            m["gaps"].as_array().unwrap().len(),
        )
    };
    assert_eq!(month("binance:EURUSD:fx").await, (open, open, 0), "100%");
    assert_eq!(
        month("binance:EURUSDH:fx").await,
        (open - 60, open, 1),
        "short by the hour"
    );
    assert_eq!(
        month("binance:BTCUSDT:spot").await,
        (31 * 1440, 31 * 1440, 0),
        "crypto expects every minute"
    );
}
