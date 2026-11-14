//! Drives `POST /runs/plan` and `GET /verdicts` over real HTTP and WebSocket against a server
//! started in-process on 127.0.0.1:0, with a temporary data directory seeded with synthetic
//! 1-minute candles and a small test plan: no network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, Timeframe, Venue,
};
use td_data::time::{DAY, MINUTE, Month, parse_date};
use td_data::{DataError, DownloadReport, InstrumentList, Store};
use td_engine::{
    BacktestSettings, Direction, ENGINE_REVISION, EvidenceSettings, LuckTest, Metric, Objective,
    ParamAxis, ParamSpace, Params, Period, PortfolioBenchmark, PortfolioLimits, RunId, SavedRun,
    SlippageOverride, StrategyEntry, SweepPlan, TestPlan, VerdictRule, WalkForwardPlan,
};
use td_server::api::auth::Token;
use td_server::api::jobs::{JobInfo, JobResult, JobStatus};
use td_server::api::plans::VerdictSummary;
use td_server::api::runs::{DataCutoff, RunDetail};
use td_server::api::source::{LiveSource, MarketSource};
use td_server::api::ws::ServerMessage;
use td_server::api::{ApiConfig, Server};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// A guard against hangs only: no test waits on time passing.
const WAIT: Duration = Duration::from_secs(120);
const HOUR: Millis = 60 * MINUTE;
/// The plan's period: the fewest days that hold the minimum sample with 32-day train windows (a
/// Sharpe objective needs 30 daily returns in each) and 30-day test windows: 6 windows, 180
/// out-of-sample days.
const DAYS: i64 = 212;

fn t0() -> Millis {
    parse_date("2024-01-01").unwrap()
}

fn id(kind: InstrumentKind) -> InstrumentId {
    InstrumentId {
        venue: Venue::Binance,
        symbol: "BTCUSDT".into(),
        kind,
    }
}

fn instrument(kind: InstrumentKind) -> Instrument {
    Instrument {
        id: id(kind),
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

/// An FX pair listed beside BTCUSDT, with its trading rules.
fn fx_instrument(symbol: &str) -> Instrument {
    let mut fx = instrument(InstrumentKind::Fx);
    fx.id.symbol = symbol.into();
    fx
}

/// Lists the two test instruments, and for FX the pairs of [`fx_store`], as a venue's exchange
/// info would; never downloads.
#[derive(Default)]
struct ListOnly {
    /// Lists the perpetual with fees that are not numbers, which the engine refuses when its
    /// battery starts: the route does not check fees, so the job fails on that instrument.
    nan_perpetual_fees: bool,
}

impl MarketSource for ListOnly {
    fn instruments(&self, venue: Venue, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
        assert_eq!(venue, Venue::Binance);
        let mut listed = instrument(kind);
        if self.nan_perpetual_fees && kind == InstrumentKind::Perpetual {
            listed.fees.taker = f64::NAN;
        }
        let instruments = if kind == InstrumentKind::Fx {
            FX_PAIRS.iter().map(|&(s, _)| fx_instrument(s)).collect()
        } else {
            vec![listed]
        };
        Ok(InstrumentList {
            instruments,
            rejected: Vec::new(),
        })
    }
    fn candles(
        &self,
        _: &Store,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<DownloadReport, DataError> {
        panic!("a plan run never downloads candles")
    }
    fn funding(
        &self,
        _: &Store,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<usize, DataError> {
        panic!("a plan run never downloads funding")
    }
}

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
    fn rest(&self) -> Arc<dyn td_data::live::Rest> {
        Arc::new(NoRest)
    }
    fn stream_url(&self, _: &InstrumentId) -> String {
        "ws://127.0.0.1:9/".into()
    }
}

/// A triangle wave with period 1 between -1 and 1.
fn tri(x: f64) -> f64 {
    let f = x - x.floor();
    4.0 * (f - 0.5).abs() - 1.0
}

/// 1-minute candles over the plan's period: a clean 5-day swing of about 4% either way, which a
/// moving-average cross follows with a steady profit, so the battery gets as far as the luck test.
fn minutes() -> Vec<Candle> {
    let price = |i: i64| {
        let x = i as f64;
        100.0 + 4.0 * tri(x / (5.0 * 1440.0))
    };
    (0..DAYS * 1440)
        .map(|i| {
            let (open, close) = (price(i), price(i + 1));
            Candle {
                open_time: t0() + i * MINUTE,
                open,
                high: open.max(close) + 0.02,
                low: open.min(close) - 0.02,
                close,
                volume: 1.0,
                quote_volume: close,
            }
        })
        .collect()
}

/// A store holding [`minutes`] for BTCUSDT spot and perpetual and perpetual funding every 8
/// hours, written once per test process and copied by each test: writing the minutes takes seconds
/// in a debug build. It is rebuilt by every process rather than kept between runs, because whether
/// a store folder reads back depends on the store's code as well as the minutes, and a template
/// kept from an older build (CI restores Cargo's scratch folder from its cache) read as empty.
fn template_store() -> &'static Path {
    static TEMPLATE: OnceLock<PathBuf> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let dir = fresh_template_dir(Path::new(env!("CARGO_TARGET_TMPDIR")));
        let store = Store::new(&dir);
        let candles = minutes();
        for kind in [InstrumentKind::Spot, InstrumentKind::Perpetual] {
            store.write_candles(&id(kind), &candles).expect("seeds");
        }
        let rates: Vec<(Millis, f64)> = (0..DAYS * 3)
            .map(|i| (t0() + i * 8 * HOUR, 0.00001))
            .collect();
        store
            .write_funding(&id(InstrumentKind::Perpetual), &rates)
            .expect("seeds");
        dir
    })
}

/// This process's template folder under `scratch`, emptied. Only this process's own folder is
/// deleted: another test process sharing the target dir (two worktrees with one
/// `CARGO_TARGET_DIR`) may be writing or copying its own template at the same moment.
// shortcut: folders left by finished processes stay, a few MB each; `cargo clean` removes them.
fn fresh_template_dir(scratch: &Path) -> PathBuf {
    let dir = scratch.join(format!("plans-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn the_template_folder_of_another_process_is_left_alone() {
    let scratch = tempfile::tempdir().expect("temp dir");
    let other = scratch.path().join("plans-store-0");
    std::fs::create_dir_all(&other).unwrap();
    let own = scratch
        .path()
        .join(format!("plans-store-{}", std::process::id()));
    std::fs::create_dir_all(own.join("candles")).unwrap();
    assert_eq!(fresh_template_dir(scratch.path()), own);
    assert!(!own.exists(), "this process's old template is removed");
    assert!(other.exists(), "another process's template stays");
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// `sma_cross` under a small plan on BTCUSDT spot and perpetual: H4 bars, a 2 x 2 grid and
/// `permutations` shuffled copies in the luck test, judged as a portfolio against cash.
fn entry(permutations: u32) -> StrategyEntry {
    let mut entry = td_engine::find_strategy("sma_cross").unwrap();
    let axis = |name: &str, from: f64| ParamAxis {
        name: name.into(),
        space: ParamSpace::Range {
            from,
            to: from + 1.0,
            step: 1.0,
        },
    };
    let defaults = EvidenceSettings::default();
    entry.plan = TestPlan {
        instruments: vec![id(InstrumentKind::Spot), id(InstrumentKind::Perpetual)],
        timeframe: Timeframe::H4,
        period: Period {
            from: t0(),
            to: t0() + DAYS * DAY,
        },
        backtest: BacktestSettings {
            starting_cash: 10_000.0,
            max_leverage: 1.0,
            slippage: Vec::new(),
            account_currency: td_core::AccountCurrency::Quote,
            fee_override: None,
        },
        sweep: SweepPlan {
            fixed: Params::new(),
            axes: vec![axis("fast", 3.0), axis("slow", 10.0)],
            objective: Objective {
                metric: Metric::Sharpe,
                direction: Direction::Maximize,
            },
            min_trades: 1,
        },
        walk_forward: WalkForwardPlan {
            train_days: 32,
            test_days: 30,
        },
        out_of_sample: None,
        evidence: EvidenceSettings {
            luck: LuckTest {
                permutations,
                ..defaults.luck
            },
            ..defaults
        },
    };
    entry.rule = VerdictRule::Portfolio {
        benchmark: PortfolioBenchmark::Cash,
        limits: PortfolioLimits::default(),
    };
    entry
}

struct TestServer {
    server: Server,
    store: Store,
    client: reqwest::Client,
    /// Shared with a server [`TestServer::restarted`] on the same store.
    dir: Arc<TempDir>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.server.jobs().cancel_all();
    }
}

impl TestServer {
    /// A server running `entries`, whose store is a copy of [`template_store`].
    async fn seeded(entries: Vec<StrategyEntry>) -> TestServer {
        TestServer::seeded_listing(entries, ListOnly::default()).await
    }

    /// [`TestServer::seeded`] with the venue list `listing`.
    async fn seeded_listing(entries: Vec<StrategyEntry>, listing: ListOnly) -> TestServer {
        TestServer::seeded_with(entries, listing, None).await
    }

    /// [`TestServer::seeded`], started with `TD_DATA_CUTOFF` set to `cutoff`.
    async fn seeded_with_cutoff(entries: Vec<StrategyEntry>, cutoff: &str) -> TestServer {
        TestServer::seeded_with(entries, ListOnly::default(), Some(cutoff)).await
    }

    async fn seeded_with(
        entries: Vec<StrategyEntry>,
        listing: ListOnly,
        cutoff: Option<&str>,
    ) -> TestServer {
        let dir = tempfile::tempdir().expect("temp dir");
        let template = tokio::task::spawn_blocking(template_store).await.unwrap();
        copy_dir(template, dir.path());
        TestServer::start(Arc::new(dir), entries, listing, cutoff).await
    }

    /// Another server on this one's store, running `entries`, as after a rebuild that changed the
    /// compiled-in strategies.
    async fn restarted(&self, entries: Vec<StrategyEntry>) -> TestServer {
        TestServer::start(self.dir.clone(), entries, ListOnly::default(), None).await
    }

    async fn start(
        dir: Arc<TempDir>,
        entries: Vec<StrategyEntry>,
        listing: ListOnly,
        cutoff: Option<&str>,
    ) -> TestServer {
        let store = Store::new(dir.path());
        let server = Server::start_with_strategies(
            ApiConfig {
                store: store.clone(),
                source: Arc::new(listing),
                live: Arc::new(NoLive),
                token: Token::generate().expect("rng"),
                port: 0,
                data_cutoff: cutoff.map(|c| DataCutoff::parse(c).expect("a cutoff")),
            },
            entries,
        )
        .await
        .expect("starts");
        TestServer {
            server,
            store,
            client: reqwest::Client::new(),
            dir,
        }
    }

    fn runs_dir(&self) -> PathBuf {
        self.store.root().join("runs")
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

    async fn start_plan(&self, body: &Value) -> u64 {
        let (status, created) = self.post("/runs/plan", body).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{created}");
        created["job_id"].as_u64().expect("a job id")
    }

    async fn verdicts(&self, query: &str) -> Vec<VerdictSummary> {
        let (status, body) = self.get(&format!("/verdicts{query}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_value(body).expect("verdict rows")
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

    /// The next state of job `id`.
    async fn next_of(&mut self, id: u64) -> JobInfo {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            match self.next().await {
                Some(ServerMessage::Job { job }) if job.id == id => return *job,
                Some(_) => {}
                None => panic!("the topic closed"),
            }
        }
        panic!("no state of job {id}");
    }

    /// Every state of job `id` until it finishes, in order.
    async fn follow(&mut self, id: u64) -> Vec<JobInfo> {
        let mut states = Vec::new();
        loop {
            let job = self.next_of(id).await;
            let finished = job.status.is_finished();
            states.push(job);
            if finished {
                return states;
            }
        }
    }

    /// The final state of each of `ids`, in that order, however their states interleave.
    async fn finish_all(&mut self, ids: &[u64]) -> Vec<JobInfo> {
        let mut finished: BTreeMap<u64, JobInfo> = BTreeMap::new();
        while finished.len() < ids.len() {
            match self.next().await {
                Some(ServerMessage::Job { job }) if ids.contains(&job.id) => {
                    if job.status.is_finished() {
                        finished.insert(job.id, *job);
                    }
                }
                Some(_) => {}
                None => panic!("the topic closed"),
            }
        }
        ids.iter().map(|id| finished.remove(id).unwrap()).collect()
    }

    /// Follows job `id` until a state whose message contains every one of `parts`; panics if the
    /// job finishes first.
    async fn until_message(&mut self, id: u64, parts: &[&str]) -> JobInfo {
        loop {
            let job = self.next_of(id).await;
            if parts.iter().all(|p| job.message.contains(p)) {
                return job;
            }
            assert!(
                !job.status.is_finished(),
                "job {id} finished before a message with {parts:?}: {job:?}"
            );
        }
    }
}

fn plan_done(job: &JobInfo) -> &td_server::api::plans::PlanDone {
    match &job.result {
        Some(JobResult::Plan(done)) => done,
        other => panic!("not a plan result: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_runs_every_instrument_saves_its_verdicts_and_counts_the_runs() {
    let t = TestServer::seeded(vec![entry(49)]).await;
    let mut ws = t.websocket().await;
    let job = t.start_plan(&json!({ "strategy": "sma_cross" })).await;
    let states = ws.follow(job).await;
    let done = states.last().unwrap();
    assert_eq!(done.status, JobStatus::Done, "{:?}", done.error);
    assert_eq!(done.kind, td_server::api::jobs::JobKind::Plan);

    // One progress count over both instruments: every report of the second instrument is past
    // every report of the first, and none moves backwards.
    let running: Vec<&JobInfo> = states
        .iter()
        .filter(|j| j.status == JobStatus::Running && j.progress > 0.0)
        .collect();
    assert!(running.windows(2).all(|w| w[0].progress <= w[1].progress));
    let of = |n: &str| -> Vec<f64> {
        running
            .iter()
            .filter(|j| j.message.contains(n))
            .map(|j| j.progress)
            .collect()
    };
    let (first, second) = (of("(1 of 2)"), of("(2 of 2)"));
    assert!(!first.is_empty() && !second.is_empty(), "{running:?}");
    assert!(first.iter().all(|&p| p <= 0.6) && second.iter().all(|&p| p >= 0.4));
    assert!(
        running.iter().any(|j| j.message.contains("luck test")),
        "the luck test is named while it runs"
    );

    // One verdict per instrument, in the plan's order, each saved as a plan run.
    let result = plan_done(done);
    assert_eq!(result.strategy, "sma_cross");
    let instruments: Vec<&InstrumentId> = result
        .instruments
        .iter()
        .map(|i| &i.verdict.instrument)
        .collect();
    assert_eq!(
        instruments,
        [&id(InstrumentKind::Spot), &id(InstrumentKind::Perpetual)]
    );
    for kept in &result.instruments {
        let (status, body) = t.get(&format!("/runs/{}", kept.run_id)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail: RunDetail = serde_json::from_value(body).unwrap();
        let SavedRun::Plan(run) = &detail.run else {
            panic!("a plan run: {:?}", detail.run.kind());
        };
        assert_eq!(run.battery.verdict, kept.verdict);
        assert_eq!(run.battery.seed, kept.seed);
        assert_eq!(run.engine_revision, ENGINE_REVISION);
        assert_eq!(run.config.instrument.id, kept.verdict.instrument);
        // The battery ran to the end: the luck test has a p-value.
        assert!(
            run.battery.permutation.is_some(),
            "{:?}",
            run.battery.luck_note
        );
        let check = detail.check.unwrap();
        assert!(check.data_matches);
        assert_eq!(check.engine_matches, Some(true));
        assert_eq!(detail.data.unwrap().dropped.len(), 0);
    }
    // The job result route points to the per-instrument runs.
    let (status, body) = t.get(&format!("/jobs/{job}/result")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("per instrument")
    );
    // Plan runs are listed with the other runs.
    let (_, list) = t.get("/runs").await;
    assert!(list.as_array().unwrap().iter().all(|r| r["kind"] == "plan"));

    // Rows are ordered by strategy, then instrument as text: the perpetual before spot.
    let rows = t.verdicts("").await;
    assert_eq!(rows.len(), 2);
    for (row, kept) in rows.iter().zip(result.instruments.iter().rev()) {
        assert_eq!((row.run_id.clone(), row.runs), (kept.run_id.clone(), 1));
        assert_eq!(row.verdict, kept.verdict);
        assert!(row.engine_matches);
        assert!(row.plan_matches && row.complete, "{row:?}");
        assert_eq!(row.engine_revision, ENGINE_REVISION);
        assert_eq!(row.created_at, row.run_id.created_at());
    }

    // A second run on the spot pair alone: its count goes up, the perpetual's does not, and the
    // same seed on the same data gives the same verdict.
    let job = t
        .start_plan(&json!({ "strategy": "sma_cross", "instruments": [id(InstrumentKind::Spot)] }))
        .await;
    let done = ws.follow(job).await.pop().unwrap();
    assert_eq!(done.status, JobStatus::Done, "{:?}", done.error);
    let again = &plan_done(&done).instruments;
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].verdict, result.instruments[0].verdict);
    let rows = t.verdicts("").await;
    let counts: Vec<(InstrumentKind, usize)> =
        rows.iter().map(|r| (r.instrument.kind, r.runs)).collect();
    assert_eq!(
        counts,
        [(InstrumentKind::Perpetual, 1), (InstrumentKind::Spot, 2)]
    );
    assert_eq!(
        rows[1].run_id, again[0].run_id,
        "the latest run of the pair"
    );
    assert_eq!(rows[0].run_id, result.instruments[1].run_id);
    assert_eq!(t.verdicts("?strategy=sma_cross").await, rows);
    assert!(t.verdicts("?strategy=donchian_breakout").await.is_empty());

    // A result saved by an older engine revision (by another tool, so without a verdict file) is
    // read from its result, counted, and marked; as the latest, it is the pair's row.
    let runs_dir = t.store.root().join("runs");
    let SavedRun::Plan(mut old) = td_engine::load(&runs_dir, again[0].run_id.as_str()).unwrap()
    else {
        panic!("a plan run");
    };
    old.engine_revision = ENGINE_REVISION - 1;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let old_id = td_engine::save(&SavedRun::Plan(old), &runs_dir).unwrap();
    let rows = t.verdicts("").await;
    assert_eq!(rows[1].run_id, old_id);
    assert_eq!(rows[1].runs, 3);
    assert_eq!(rows[1].engine_revision, ENGINE_REVISION - 1);
    assert!(!rows[1].engine_matches);
    assert!(rows[1].plan_matches && rows[1].complete, "{:?}", rows[1]);
    assert!(rows[0].engine_matches);
    let (_, detail) = t.get(&format!("/runs/{old_id}")).await;
    assert_eq!(detail["check"]["engine_matches"], json!(false));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_keeps_finished_instruments_and_one_stopped_in_its_luck_test() {
    // The default 199 copies make the luck test most of each battery, so the cancel lands in it.
    let t = TestServer::seeded(vec![entry(199)]).await;
    let mut ws = t.websocket().await;

    // Cancelled before any luck test: nothing is kept. The cancel is sent once the job reports
    // that it is loading the data, before any battery has started, so it lands before R3 by the
    // whole data load and the first battery's runs before its luck test, not by a few runs.
    let job = t.start_plan(&json!({ "strategy": "sma_cross" })).await;
    let state = ws.until_message(job, &["loading the data"]).await;
    assert_eq!(state.status, JobStatus::Running);
    let (status, body) = t.post(&format!("/jobs/{job}/cancel"), &json!(null)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let done = ws.follow(job).await.pop().unwrap();
    assert_eq!(done.status, JobStatus::Cancelled);
    assert_eq!(done.result, None);
    assert!(t.verdicts("").await.is_empty());

    // Cancelled during the second instrument's luck test: the first is kept whole and the second
    // with its luck line inconclusive.
    let job = t.start_plan(&json!({ "strategy": "sma_cross" })).await;
    ws.until_message(job, &["(2 of 2)", "luck test"]).await;
    let (status, body) = t.post(&format!("/jobs/{job}/cancel"), &json!(null)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let done = ws.follow(job).await.pop().unwrap();
    assert_eq!(done.status, JobStatus::Cancelled);
    let kept = &plan_done(&done).instruments;
    assert_eq!(kept.len(), 2, "{kept:?}");
    let luck = |v: &td_engine::Verdict| {
        v.criteria
            .iter()
            .find(|c| c.id == "luck_p")
            .cloned()
            .expect("a luck line")
    };
    assert_ne!(
        luck(&kept[0].verdict).outcome,
        td_engine::Outcome::Inconclusive
    );
    let stopped = luck(&kept[1].verdict);
    assert_eq!(stopped.outcome, td_engine::Outcome::Inconclusive);
    assert!(
        stopped
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("cancelled"),
        "{stopped:?}"
    );
    let rows = t.verdicts("").await;
    let ids: Vec<_> = rows.iter().map(|r| r.run_id.clone()).collect();
    assert_eq!(ids, [kept[1].run_id.clone(), kept[0].run_id.clone()]);
    assert!(rows.iter().all(|r| r.runs == 1));
    // The perpetual has no complete battery, so its stopped one is its row, marked incomplete.
    let complete: Vec<bool> = rows.iter().map(|r| r.complete).collect();
    assert_eq!(complete, [false, true]);

    // Read from `result.json` alone, the stopped battery is still incomplete.
    let record = t
        .runs_dir()
        .join(kept[1].run_id.as_str())
        .join("verdict.json");
    std::fs::remove_file(&record).unwrap();
    let rows = t.verdicts("").await;
    let complete: Vec<bool> = rows.iter().map(|r| r.complete).collect();
    assert_eq!(complete, [false, true]);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_requests_are_400_and_missing_data_is_refused_naming_the_span() {
    let mut invalid = entry(49);
    invalid.name = "donchian_breakout";
    invalid.plan.evidence.luck.permutations = 10;
    let t = TestServer::seeded(vec![entry(49), invalid]).await;
    // The perpetual lacks February 2024.
    let february = Month::parse("2024-02").unwrap();
    t.store
        .delete_months(&id(InstrumentKind::Perpetual), february, february)
        .unwrap();

    let other = InstrumentId {
        venue: Venue::Binance,
        symbol: "ETHUSDT".into(),
        kind: InstrumentKind::Spot,
    };
    let refused = [
        (
            json!({ "strategy": "sma_cross", "instruments": [other] }),
            "instrument_not_in_plan",
        ),
        (
            json!({ "strategy": "sma_cross", "instruments": [] }),
            "invalid_request",
        ),
        (
            json!({ "strategy": "sma_cross", "instruments": [id(InstrumentKind::Spot), id(InstrumentKind::Spot)] }),
            "invalid_request",
        ),
        (json!({ "strategy": "nope" }), "invalid_config"),
        (json!({ "strategy": "donchian_breakout" }), "invalid_plan"),
    ];
    for (body, code) in refused {
        let (status, answer) = t.post("/runs/plan", &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {answer}");
        assert_eq!(answer["error"]["code"], code, "{body}: {answer}");
    }
    let (_, answer) = t
        .post(
            "/runs/plan",
            &json!({ "strategy": "sma_cross", "instruments": [other] }),
        )
        .await;
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("binance:ETHUSDT:spot"),
        "{answer}"
    );

    // The whole plan reads the perpetual's hole: refused before any job, naming it and the span.
    let (status, answer) = t
        .post("/runs/plan", &json!({ "strategy": "sma_cross" }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"]["code"], "data_missing");
    let message = answer["error"]["message"].as_str().unwrap();
    assert!(message.contains("binance:BTCUSDT:perpetual"), "{message}");
    assert!(
        message.contains("from 2024-02-01 00:00 to 2024-03-01 00:00 UTC"),
        "{message}"
    );
    // So does a subset that holds the perpetual.
    let (status, answer) = t
        .post(
            "/runs/plan",
            &json!({ "strategy": "sma_cross", "instruments": [id(InstrumentKind::Perpetual)] }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"]["code"], "data_missing");
    let message = answer["error"]["message"].as_str().unwrap();
    assert!(message.contains("binance:BTCUSDT:perpetual"), "{message}");
    let (_, jobs) = t.get("/jobs").await;
    assert_eq!(jobs, json!([]), "no job was created");

    // The spot instrument alone has all its data.
    let job = t
        .start_plan(&json!({ "strategy": "sma_cross", "instruments": [id(InstrumentKind::Spot)] }))
        .await;
    let (_, state) = t.get(&format!("/jobs/{job}")).await;
    assert_eq!(state["kind"], "plan");
    assert_eq!(
        state["request"]["instruments"],
        json!([id(InstrumentKind::Spot)])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_rule_and_an_instrument_the_venue_does_not_list_are_400() {
    // A rule whose limit is not a number: 400 invalid_plan naming the rule.
    let mut bad_rule = entry(49);
    bad_rule.rule = VerdictRule::Portfolio {
        benchmark: PortfolioBenchmark::Cash,
        limits: PortfolioLimits {
            min_sharpe: f64::NAN,
            ..PortfolioLimits::default()
        },
    };
    let t = TestServer::seeded(vec![bad_rule]).await;
    let (status, answer) = t
        .post("/runs/plan", &json!({ "strategy": "sma_cross" }))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(answer["error"]["code"], "invalid_plan", "{answer}");
    let message = answer["error"]["message"].as_str().unwrap();
    assert!(message.contains("verdict rule"), "{message}");

    // A plan instrument the venue's list (faked, offline) does not hold: 400 invalid_config.
    let unlisted = InstrumentId {
        venue: Venue::Binance,
        symbol: "ETHUSDT".into(),
        kind: InstrumentKind::Spot,
    };
    let mut on_unlisted = entry(49);
    on_unlisted.plan.instruments = vec![id(InstrumentKind::Spot), unlisted];
    let t = TestServer::seeded(vec![on_unlisted]).await;
    let (status, answer) = t
        .post("/runs/plan", &json!({ "strategy": "sma_cross" }))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(answer["error"]["code"], "invalid_config", "{answer}");
    let message = answer["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("does not list binance:ETHUSDT:spot"),
        "{message}"
    );
    let (_, jobs) = t.get("/jobs").await;
    assert_eq!(jobs, json!([]), "no job was created");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_plan_jobs_on_one_strategy_at_once_both_save_and_count() {
    let t = TestServer::seeded(vec![entry(49)]).await;
    let mut ws = t.websocket().await;
    let spot = json!({ "strategy": "sma_cross", "instruments": [id(InstrumentKind::Spot)] });
    let jobs = [t.start_plan(&spot).await, t.start_plan(&spot).await];
    let done = ws.finish_all(&jobs).await;
    let mut saved = Vec::new();
    for job in &done {
        assert_eq!(job.status, JobStatus::Done, "{:?}", job.error);
        saved.push(plan_done(job).instruments[0].run_id.clone());
    }
    // The same seed on the same data gives the same result; the two saves still get two runs.
    assert_ne!(saved[0], saved[1]);
    let rows = t.verdicts("").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].runs, 2);
    assert_eq!(&rows[0].run_id, saved.iter().max().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failure_partway_keeps_the_instruments_saved_before_it() {
    // The perpetual, second in the plan, is listed with fees the engine refuses.
    let listing = ListOnly {
        nan_perpetual_fees: true,
    };
    let t = TestServer::seeded_listing(vec![entry(49)], listing).await;
    let mut ws = t.websocket().await;
    let job = t.start_plan(&json!({ "strategy": "sma_cross" })).await;
    let done = ws.follow(job).await.pop().unwrap();
    assert_eq!(done.status, JobStatus::Failed);
    let error = done.error.as_ref().expect("an error");
    assert_eq!(error.code, "invalid_config", "{error:?}");
    assert!(
        error.message.contains("binance:BTCUSDT:perpetual"),
        "{error:?}"
    );

    // The spot instrument was saved before the error, and the failed job says so.
    let kept = &plan_done(&done).instruments;
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0].verdict.instrument, id(InstrumentKind::Spot));
    let (status, body) = t.get(&format!("/runs/{}", kept[0].run_id)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = t.verdicts("").await;
    let ids: Vec<_> = rows.iter().map(|r| r.run_id.clone()).collect();
    assert_eq!(ids, [kept[0].run_id.clone()]);
    // The job result route points to the saved run, not to "no run".
    let (status, body) = t.get(&format!("/jobs/{job}/result")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("per instrument"), "{message}");
    assert!(!message.contains("  "), "{message}");
}

/// Rewrites the `verdict.json` of run `id`.
fn rewrite_record(runs_dir: &Path, id: &RunId, change: impl FnOnce(&mut Value)) {
    let path = runs_dir.join(id.as_str()).join("verdict.json");
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    change(&mut record);
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_verdict_list_flags_a_changed_plan_and_prefers_a_complete_battery() {
    let t = TestServer::seeded(vec![entry(49)]).await;
    let mut ws = t.websocket().await;
    let spot = json!({ "strategy": "sma_cross", "instruments": [id(InstrumentKind::Spot)] });
    let job = t.start_plan(&spot).await;
    let done = ws.follow(job).await.pop().unwrap();
    assert_eq!(done.status, JobStatus::Done, "{:?}", done.error);
    let first = plan_done(&done).instruments[0].run_id.clone();
    let row = |rows: Vec<VerdictSummary>| -> VerdictSummary {
        assert_eq!(rows.len(), 1, "{rows:?}");
        rows.into_iter().next().unwrap()
    };
    let r = row(t.verdicts("").await);
    assert_eq!(r.run_id, first);
    assert!(r.plan_matches && r.complete, "{r:?}");

    // The plans and rules are compiled in: a server built with other ones sees the saved verdict
    // as stale. The same entry still matches, and so does one that changes only what the spot
    // battery does not read: the other instruments and their slippage.
    let same = t.restarted(vec![entry(49)]).await;
    assert!(row(same.verdicts("").await).plan_matches);
    let eth = InstrumentId {
        symbol: "ETHUSDT".into(),
        ..id(InstrumentKind::Spot)
    };
    let slippage = |kind: InstrumentKind, bps: f64| SlippageOverride {
        instrument: id(kind),
        bps,
    };
    let mut another_instrument = entry(49);
    another_instrument.plan.instruments.push(eth.clone());
    let mut reordered = entry(49);
    reordered.plan.instruments.reverse();
    let mut other_slippage = entry(49);
    other_slippage.plan.backtest.slippage = vec![slippage(InstrumentKind::Perpetual, 7.5)];
    let mut everything_else = entry(49);
    everything_else.plan.instruments = vec![eth, id(InstrumentKind::Spot)];
    everything_else.plan.backtest.slippage = vec![SlippageOverride {
        instrument: everything_else.plan.instruments[0].clone(),
        bps: 3.0,
    }];
    let mut other_split = entry(49);
    other_split.plan.out_of_sample = Some(td_engine::Split::Fraction(0.3));
    for current in [
        another_instrument,
        reordered,
        other_slippage,
        everything_else,
        other_split,
    ] {
        let changed = t.restarted(vec![current.clone()]).await;
        let r = row(changed.verdicts("").await);
        assert!(r.plan_matches, "{:?}: {r:?}", current.plan);
    }
    let mut other_rule = entry(49);
    other_rule.rule = VerdictRule::Portfolio {
        benchmark: PortfolioBenchmark::Cash,
        limits: PortfolioLimits {
            min_sharpe: 1.5,
            ..PortfolioLimits::default()
        },
    };
    let mut without_spot = entry(49);
    without_spot.plan.instruments = vec![id(InstrumentKind::Perpetual)];
    let mut spot_slippage = entry(49);
    spot_slippage.plan.backtest.slippage = vec![slippage(InstrumentKind::Spot, 7.5)];
    let stale = [
        vec![entry(50)],
        vec![other_rule],
        vec![without_spot],
        vec![spot_slippage],
        Vec::new(),
    ];
    for entries in stale {
        let changed = t.restarted(entries).await;
        let r = row(changed.verdicts("").await);
        assert_eq!(r.run_id, first);
        assert!(!r.plan_matches, "{r:?}");
        assert!(r.engine_matches);
    }

    // A damaged verdict record does not hide the run: it is read from its result.
    let runs_dir = t.runs_dir();
    let record = runs_dir.join(first.as_str()).join("verdict.json");
    let intact = std::fs::read(&record).unwrap();
    std::fs::write(&record, b"{\"strategy\": \"sma_cro").unwrap();
    let r = row(t.verdicts("").await);
    assert_eq!((r.run_id.clone(), r.runs), (first.clone(), 1));
    assert!(r.plan_matches && r.complete, "{r:?}");
    // So is one written before the plan, the rule and the completeness were recorded.
    std::fs::write(&record, &intact).unwrap();
    rewrite_record(&runs_dir, &first, |v| {
        let v = v.as_object_mut().unwrap();
        for field in ["plan", "rule", "complete"] {
            v.remove(field);
        }
    });
    let r = row(t.verdicts("").await);
    assert_eq!((r.run_id.clone(), r.runs), (first.clone(), 1));
    assert!(r.plan_matches && r.complete, "{r:?}");
    std::fs::write(&record, &intact).unwrap();

    // A record whose plan no longer reads as this build's plan type (it has a field since removed
    // or renamed) still shows, as stale, whether it is read from the record or from the result.
    rewrite_record(&runs_dir, &first, |v| v["plan"]["retired"] = json!(1));
    let r = row(t.verdicts("").await);
    assert_eq!((r.run_id.clone(), r.runs), (first.clone(), 1));
    assert!(!r.plan_matches && r.complete, "{r:?}");
    std::fs::write(&record, &intact).unwrap();
    let result = runs_dir.join(first.as_str()).join("result.json");
    let intact_result = std::fs::read(&result).unwrap();
    let mut saved: Value = serde_json::from_slice(&intact_result).unwrap();
    saved["result"]["plan"]["retired"] = json!(1);
    std::fs::write(&result, serde_json::to_vec(&saved).unwrap()).unwrap();
    rewrite_record(&runs_dir, &first, |v| {
        v.as_object_mut().unwrap().remove("complete");
    });
    let r = row(t.verdicts("").await);
    assert_eq!((r.run_id.clone(), r.runs), (first.clone(), 1));
    assert!(!r.plan_matches && r.complete, "{r:?}");
    std::fs::write(&result, &intact_result).unwrap();
    std::fs::write(&record, &intact).unwrap();

    // A newer battery whose luck test a cancel stopped is counted but does not replace the
    // complete one as the pair's row.
    let copy = |runs_dir: &Path| {
        let SavedRun::Plan(run) = td_engine::load(runs_dir, first.as_str()).unwrap() else {
            panic!("a plan run");
        };
        std::thread::sleep(Duration::from_millis(5));
        let id = td_engine::save(&SavedRun::Plan(run), runs_dir).unwrap();
        std::fs::copy(&record, runs_dir.join(id.as_str()).join("verdict.json")).unwrap();
        id
    };
    let stopped = copy(&runs_dir);
    assert!(stopped > first);
    rewrite_record(&runs_dir, &stopped, |v| v["complete"] = json!(false));
    let r = row(t.verdicts("").await);
    assert_eq!((r.run_id.clone(), r.runs), (first.clone(), 2));
    assert!(r.complete);

    // With no complete battery left, the newest one is the row.
    let newest = copy(&runs_dir);
    for id in [&first, &newest] {
        rewrite_record(&runs_dir, id, |v| v["complete"] = json!(false));
    }
    let r = row(t.verdicts("").await);
    assert_eq!((r.run_id.clone(), r.runs), (newest, 3));
    assert!(!r.complete);
}

/// A battery whose test period ends after the data cutoff is refused before any job starts,
/// naming the cutoff; one whose period ends at the end of the cutoff date starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_battery_whose_period_ends_after_the_data_cutoff_is_refused() {
    // The plan's period runs from 2024-01-01 to 2024-07-31 00:00.
    let t = TestServer::seeded_with_cutoff(vec![entry(49)], "2024-06-30").await;
    let (status, body) = t
        .post("/runs/plan", &json!({ "strategy": "sma_cross" }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "after_cutoff");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("ends at 2024-07-31 00:00 UTC")
            && message.contains("TD_DATA_CUTOFF=2024-06-30"),
        "{message}"
    );
    let (_, jobs) = t.get("/jobs").await;
    assert_eq!(jobs, json!([]), "no job was created");

    let t = TestServer::seeded_with_cutoff(vec![entry(49)], "2024-07-30").await;
    let job = t.start_plan(&json!({ "strategy": "sma_cross" })).await;
    let (status, body) = t.post(&format!("/jobs/{job}/cancel"), &json!(null)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
}

/// Each verdict row says whether the strategy's code still matches the code the battery ran,
/// counts that in `stale`, lists the limits its rule loosened from the kind's defaults (and not
/// the ones it tightened), and counts the distinct code versions that went through the battery.
#[tokio::test(flavor = "multi_thread")]
async fn a_verdict_shows_its_code_its_loosened_limits_and_how_many_code_versions_were_tested() {
    let mut loose = entry(49);
    loose.rule = VerdictRule::Portfolio {
        benchmark: PortfolioBenchmark::Cash,
        limits: PortfolioLimits {
            min_sharpe: 0.5,
            max_drawdown: 0.2,
            ..PortfolioLimits::default()
        },
    };
    let t = TestServer::seeded(vec![loose]).await;
    let mut ws = t.websocket().await;
    let spot = json!({ "strategy": "sma_cross", "instruments": [id(InstrumentKind::Spot)] });
    let job = t.start_plan(&spot).await;
    let done = ws.follow(job).await.pop().unwrap();
    assert_eq!(done.status, JobStatus::Done, "{:?}", done.error);
    let first = plan_done(&done).instruments[0].run_id.clone();
    let row = |rows: Vec<VerdictSummary>| -> VerdictSummary {
        assert_eq!(rows.len(), 1, "{rows:?}");
        rows.into_iter().next().unwrap()
    };
    let r = row(t.verdicts("").await);
    assert_eq!(r.code_matches, Some(true), "{r:?}");
    assert!(!r.stale && r.plan_matches && r.engine_matches, "{r:?}");
    assert_eq!((r.runs, r.code_versions), (1, 1));
    let default_sharpe = PortfolioLimits::default().min_sharpe;
    assert!(default_sharpe > 0.5);
    assert_eq!(
        r.loosened,
        vec![td_engine::LoosenedLimit {
            name: "min_sharpe".into(),
            value: 0.5,
            default: default_sharpe,
        }],
        "the tightened max_drawdown is not listed"
    );
    let (_, body) = t.get("/verdicts").await;
    assert_eq!(body[0]["loosened"][0]["name"], "min_sharpe");
    assert_eq!(body[0]["code_matches"], true);

    // The code changed since the battery ran.
    let runs_dir = t.runs_dir();
    let record = runs_dir.join(first.as_str()).join("verdict.json");
    rewrite_record(&runs_dir, &first, |v| {
        v["code_hash"] = json!("0000000000000000")
    });
    let r = row(t.verdicts("").await);
    assert_eq!(r.code_matches, Some(false), "{r:?}");
    assert!(r.stale && r.plan_matches && r.engine_matches, "{r:?}");
    assert_eq!(r.code_versions, 1);

    // A battery saved before runs recorded their code is unknown, not a match, and counts as one
    // more version.
    std::thread::sleep(Duration::from_millis(5));
    let SavedRun::Plan(run) = td_engine::load(&runs_dir, first.as_str()).unwrap() else {
        panic!("a plan run");
    };
    let old = td_engine::save(&SavedRun::Plan(run), &runs_dir).unwrap();
    std::fs::copy(&record, runs_dir.join(old.as_str()).join("verdict.json")).unwrap();
    rewrite_record(&runs_dir, &old, |v| {
        v.as_object_mut().unwrap().remove("code_hash");
    });
    let r = row(t.verdicts("").await);
    assert_eq!(r.run_id, old);
    assert_eq!(r.code_matches, None, "{r:?}");
    assert!(r.stale);
    assert_eq!((r.runs, r.code_versions), (2, 2));

    // Read from the result when the verdict record is damaged, the code comes from the run's
    // record.
    std::fs::write(runs_dir.join(old.as_str()).join("verdict.json"), b"{").unwrap();
    std::fs::write(
        runs_dir.join(old.as_str()).join("record.json"),
        serde_json::to_vec(&json!({ "code_hash": td_engine::code_hash("sma_cross") })).unwrap(),
    )
    .unwrap();
    let r = row(t.verdicts("").await);
    assert_eq!(r.run_id, old);
    assert_eq!(r.code_matches, Some(true), "{r:?}");
    assert_eq!(r.code_versions, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_plan_routes_need_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start_with_strategies(
        ApiConfig {
            store: Store::new(dir.path()),
            source: Arc::new(ListOnly::default()),
            live: Arc::new(NoLive),
            token: Token::generate().unwrap(),
            port: 0,
            data_cutoff: None,
        },
        vec![entry(49)],
    )
    .await
    .unwrap();
    let client = reqwest::Client::new();
    let url = |p: &str| format!("http://127.0.0.1:{}{p}", server.port());
    let requests = [
        client.get(url("/verdicts")),
        client.get(url("/verdicts?strategy=sma_cross")),
        client
            .post(url("/runs/plan"))
            .header("content-type", "application/json")
            .body(json!({ "strategy": "sma_cross" }).to_string()),
    ];
    for request in requests {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = client
        .get(url("/verdicts"))
        .header(
            "authorization",
            format!("Bearer {}", server.token().as_str()),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(body, json!([]));
    server.jobs().cancel_all();
}

/// The FX plan's period in days from Monday [`t0`]: long enough that its open hours, without the
/// weekends, hold the plan's six walk-forward windows of H4 bars.
const FX_DAYS: i64 = 301;

/// The FX pairs of [`fx_store`] and how far each one's ask is above its bid on every price of
/// every minute: the same bids, `EURUSD` at five ticks and `EURUSDZ` at none.
const FX_PAIRS: [(&str, f64); 2] = [("EURUSD", 0.05), ("EURUSDZ", 0.0)];

/// A store holding the bids and asks of [`FX_PAIRS`] over [`FX_DAYS`] days from [`t0`], weekends
/// absent: the bids are the swing of [`minutes`] laid over the open minutes alone, so a weekend
/// does not break it.
fn fx_store(dir: &Path) -> Store {
    let store = Store::new(dir);
    let price = |i: usize| 100.0 + 4.0 * tri(i as f64 / (5.0 * 1440.0));
    let bids: Vec<Candle> = (0..FX_DAYS * 1440)
        .map(|i| t0() + i * MINUTE)
        .filter(|&t| InstrumentKind::Fx.is_open(t))
        .enumerate()
        .map(|(i, open_time)| {
            let (open, close) = (price(i), price(i + 1));
            Candle {
                open_time,
                open,
                high: open.max(close) + 0.02,
                low: open.min(close) - 0.02,
                close,
                volume: 1.0,
                quote_volume: close,
            }
        })
        .collect();
    for (symbol, spread) in FX_PAIRS {
        let asks: Vec<Candle> = bids
            .iter()
            .map(|b| Candle {
                open: b.open + spread,
                high: b.high + spread,
                low: b.low + spread,
                close: b.close + spread,
                volume: 0.0,
                quote_volume: 0.0,
                ..*b
            })
            .collect();
        store
            .write_quotes(&fx_instrument(symbol).id, &bids, &asks)
            .expect("seeds");
    }
    store
}

/// [`entry`]'s plan on the pairs of [`fx_store`] over [`FX_DAYS`].
fn fx_entry() -> StrategyEntry {
    let mut entry = entry(49);
    entry.plan.instruments = FX_PAIRS.iter().map(|&(s, _)| fx_instrument(s).id).collect();
    entry.plan.period.to = t0() + FX_DAYS * DAY;
    entry
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_on_fx_trades_on_the_stored_asks_in_every_step() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().to_path_buf();
    tokio::task::spawn_blocking(move || fx_store(&path))
        .await
        .unwrap();
    let t = TestServer::start(Arc::new(dir), vec![fx_entry()], ListOnly::default(), None).await;
    let mut ws = t.websocket().await;
    let job = t.start_plan(&json!({ "strategy": "sma_cross" })).await;
    let done = ws.follow(job).await.pop().unwrap();
    assert_eq!(done.status, JobStatus::Done, "{:?}", done.error);
    let kept = &plan_done(&done).instruments;
    let pairs: Vec<&str> = kept
        .iter()
        .map(|k| k.verdict.instrument.symbol.as_str())
        .collect();
    assert_eq!(pairs, ["EURUSD", "EURUSDZ"]);

    let mut sweeps = Vec::new();
    for kept in kept {
        let luck = kept
            .verdict
            .criteria
            .iter()
            .find(|c| c.id == "luck_p")
            .expect("a luck line");
        let (status, body) = t.get(&format!("/runs/{}", kept.run_id)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail: RunDetail = serde_json::from_value(body).unwrap();
        let SavedRun::Plan(run) = detail.run else {
            panic!("a plan run: {:?}", detail.run.kind());
        };
        assert_eq!(run.battery.verdict, kept.verdict);
        // The battery ran the luck test too, as on any market, and its line holds its p.
        let permutation = run.battery.permutation.as_ref().expect("the luck test ran");
        assert_eq!(luck.value, Some(permutation.p_value));
        assert!(!run.battery.luck_cancelled);
        // The saved fingerprint covers the asks the store holds today.
        let check = detail.check.unwrap();
        assert!(check.data_matches);
        assert_eq!(check.engine_matches, Some(true));
        sweeps.push(run.battery.sweep.expect("a sweep"));
    }

    // Every parameter set pays the spread: on the same bids with no spread it does better.
    let (spread, none) = (&sweeps[0], &sweeps[1]);
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

    // A skipped luck test is not a cancelled one: both verdicts are complete.
    let rows = t.verdicts("").await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|r| r.complete && r.plan_matches),
        "{rows:?}"
    );
}

/// A plan request naming a reference instrument gives every run of the battery that instrument's
/// stored bars: the saved plan run's configuration names it and its data still match, and the
/// verdict list says which references the verdict was judged with. A plan naming a reference the
/// store lacks is a 422 that starts no job.
#[tokio::test(flavor = "multi_thread")]
async fn a_plan_naming_a_reference_instrument_judges_with_it_and_records_it() {
    let t = TestServer::seeded(vec![entry(49)]).await;
    let other = InstrumentId {
        venue: Venue::Binance,
        symbol: "ETHUSDT".into(),
        kind: InstrumentKind::Spot,
    };
    let stored = tokio::task::spawn_blocking(minutes).await.unwrap();
    t.store.write_candles(&other, &stored).unwrap();

    let absent = InstrumentId {
        symbol: "SOLUSDT".into(),
        ..other.clone()
    };
    let (status, answer) = t
        .post(
            "/runs/plan",
            &json!({ "strategy": "sma_cross", "instruments": [id(InstrumentKind::Spot)], "references": [absent] }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"]["code"], "data_missing");
    let (_, jobs) = t.get("/jobs").await;
    assert_eq!(jobs, json!([]), "no job was created");

    let mut ws = t.websocket().await;
    let job = t
        .start_plan(&json!({
            "strategy": "sma_cross",
            "instruments": [id(InstrumentKind::Spot)],
            "references": [other],
        }))
        .await;
    let done = ws.follow(job).await.pop().unwrap();
    assert_eq!(done.status, JobStatus::Done, "{:?}", done.error);
    let kept = &plan_done(&done).instruments[0];
    let (status, body) = t.get(&format!("/runs/{}", kept.run_id)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["check"],
        json!({ "data_matches": true, "engine_matches": true })
    );
    let detail: RunDetail = serde_json::from_value(body).unwrap();
    let SavedRun::Plan(run) = &detail.run else {
        panic!("a plan run");
    };
    assert_eq!(run.config.references, vec![other.clone()]);
    let rows = t.verdicts("").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].run_id, kept.run_id);
    assert_eq!(rows[0].references, vec![other]);
}
