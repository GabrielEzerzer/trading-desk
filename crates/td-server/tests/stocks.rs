//! Runs on daily bars of US stocks through the API, on a store seeded with one candle per trading day at the session's
//! last minute: several stocks in one account, an ETF as reference, the cutoff, the data report and the fingerprint.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::{Value, json};
use td_core::{
    Candle, Instrument, InstrumentId, InstrumentKind, Millis, OrderRequest, OrderType, Side,
    TimeInForce, Timeframe, Venue, us_bar_minute, us_session,
};
use td_data::time::{DAY, parse_date};
use td_data::{DataError, DownloadReport, Http, InstrumentList, Store};
use td_engine::{
    BacktestConfig, BacktestSettings, Context, Direction, EvidenceSettings, LuckTest, Metric,
    Objective, ParamAxis, ParamSpace, ParamValue, Params, Period, PortfolioBenchmark,
    PortfolioLimits, SavedRun, Strategy, StrategyEntry, SweepPlan, TestPlan, VerdictRule,
    WalkForwardPlan,
};
use td_server::api::auth::Token;
use td_server::api::jobs::JobStatus;
use td_server::api::runs::{DataCutoff, RunDetail, load_run_data};
use td_server::api::source::{LiveSource, MarketSource};
use td_server::api::{ApiConfig, Server};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(120);
const CUTOFF: &str = "2017-06-30";

fn day(text: &str) -> Millis {
    parse_date(text).unwrap()
}

fn trading_minutes(from: Millis, to: Millis) -> Vec<Millis> {
    (from / DAY..to / DAY)
        .filter_map(us_bar_minute)
        .map(|m| m.start)
        .collect()
}

fn tri(x: f64) -> f64 {
    let f = x - x.floor();
    4.0 * (f - 0.5).abs() - 1.0
}

fn bars(scale: f64, phase: f64, times: &[Millis]) -> Vec<Candle> {
    let price = |x: f64| {
        scale * (100.0 + 0.02 * x + 8.0 * tri(x / 80.0 + phase) + 3.0 * tri(x / 17.0 + phase))
    };
    times
        .iter()
        .enumerate()
        .map(|(i, &open_time)| {
            let (open, close) = (price(2.0 * i as f64), price(2.0 * i as f64 + 1.0));
            Candle {
                open_time,
                open,
                high: open.max(close) + 0.3 * scale,
                low: open.min(close) - 0.3 * scale,
                close,
                volume: 1_000.0,
                quote_volume: 0.0,
            }
        })
        .collect()
}

fn stock(symbol: &str) -> Instrument {
    static LISTED: OnceLock<Vec<Instrument>> = OnceLock::new();
    let listed = LISTED.get_or_init(|| {
        std::thread::spawn(|| {
            td_data::fetch_instruments(&Http::new().unwrap(), Venue::Alpaca, InstrumentKind::Stock)
                .unwrap()
                .instruments
        })
        .join()
        .unwrap()
    });
    listed
        .iter()
        .find(|i| i.id.symbol == symbol)
        .unwrap()
        .clone()
}

struct Listing;

impl MarketSource for Listing {
    fn instruments(&self, venue: Venue, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
        td_data::fetch_instruments(&Http::new()?, venue, kind)
    }
    fn candles(
        &self,
        _: &Store,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<DownloadReport, DataError> {
        Ok(DownloadReport::default())
    }
    fn funding(
        &self,
        _: &Store,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<usize, DataError> {
        panic!("these runs never download funding")
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

struct Rotation {
    days: usize,
}

impl Strategy for Rotation {
    fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
        if ctx.is_warming_up() {
            return;
        }
        let spy = stock("SPY").id;
        let closes: Vec<f64> = ctx
            .reference(&spy)
            .unwrap_or_default()
            .iter()
            .rev()
            .take(self.days)
            .filter_map(|b| b.map(|b| b.close))
            .collect();
        if closes.len() < self.days {
            return;
        }
        let mean = closes.iter().sum::<f64>() / closes.len() as f64;
        let hold = closes[0] > mean;
        let ids: Vec<InstrumentId> = ctx.traded().map(|i| i.id.clone()).collect();
        let idle = |ctx: &Context, id: &InstrumentId| {
            ctx.open_orders_of(id).is_some_and(|o| o.is_empty()) && ctx.bar_of(id).is_some()
        };
        if hold {
            let wanted: Vec<InstrumentId> = ids
                .into_iter()
                .filter(|id| ctx.position_of(id) == Some(0.0) && idle(ctx, id))
                .collect();
            let budget = ctx.cash() * 0.95 / wanted.len().max(1) as f64;
            for id in wanted {
                let close = ctx.bar_of(&id).unwrap().close;
                let qty = (budget / close).floor();
                if qty >= 1.0 {
                    let _ = ctx.submit(market(id, Side::Buy, qty));
                }
            }
        } else {
            for id in ids {
                let held = ctx.position_of(&id).unwrap_or(0.0);
                if held > 0.0 && idle(ctx, &id) {
                    let _ = ctx.submit(market(id, Side::Sell, held));
                }
            }
        }
    }
}

fn market(instrument: InstrumentId, side: Side, qty: f64) -> OrderRequest {
    OrderRequest {
        instrument,
        side,
        qty,
        order_type: OrderType::Market,
        tif: TimeInForce::Gtc,
        reduce_only: false,
        client_tag: None,
    }
}

fn stock_plan_entry() -> StrategyEntry {
    let defaults = EvidenceSettings::default();
    let axis = |name: &str, from: f64| ParamAxis {
        name: name.into(),
        space: ParamSpace::Range {
            from,
            to: from + 1.0,
            step: 1.0,
        },
    };
    StrategyEntry {
        plan: TestPlan {
            instruments: vec![stock("AAPL").id],
            timeframe: Timeframe::D1,
            period: Period {
                from: day("2016-01-01"),
                to: day("2017-07-01"),
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
                train_days: 60,
                test_days: 45,
                mode: td_engine::WindowMode::Rolling,
            },
            out_of_sample: None,
            evidence: EvidenceSettings {
                luck: LuckTest {
                    permutations: 49,
                    ..defaults.luck
                },
                ..defaults
            },
        },
        rule: VerdictRule::Portfolio {
            benchmark: PortfolioBenchmark::Cash,
            limits: PortfolioLimits::default(),
        },
        ..td_engine::find_strategy("sma_cross").unwrap()
    }
}

struct TestServer {
    server: Server,
    store: Store,
    client: reqwest::Client,
    _dir: TempDir,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.server.jobs().cancel_all();
    }
}

impl TestServer {
    async fn seeded() -> TestServer {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let times = trading_minutes(day("2016-01-01"), day("2018-01-01"));
        for (symbol, scale, phase) in [("AAPL", 1.0, 0.0), ("MSFT", 2.0, 0.3), ("SPY", 4.0, 0.1)] {
            store
                .write_candles(&stock(symbol).id, &bars(scale, phase, &times))
                .unwrap();
        }
        let mut holed = bars(1.5, 0.6, &times);
        holed.remove(40);
        store.write_candles(&stock("IBM").id, &holed).unwrap();
        let mut entries = td_engine::strategies();
        entries.retain(|e| e.name != "sma_cross");
        entries.push(stock_plan_entry());
        let server = Server::start_with_strategies(
            ApiConfig {
                store: store.clone(),
                source: Arc::new(Listing),
                live: Arc::new(NoLive),
                token: Token::generate().unwrap(),
                port: 0,
                data_cutoff: Some(DataCutoff::parse(CUTOFF).unwrap()),
            },
            entries,
        )
        .await
        .unwrap();
        TestServer {
            server,
            store,
            client: reqwest::Client::new(),
            _dir: dir,
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let response = request
            .header(
                "authorization",
                format!("Bearer {}", self.server.token().as_str()),
            )
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap()
        };
        (status, body)
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.server.port())
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(self.client.get(self.url(path))).await
    }

    async fn post(&self, path: &str, body: &Value) -> (StatusCode, Value) {
        let request = self
            .client
            .post(self.url(path))
            .header("content-type", "application/json")
            .body(body.to_string());
        self.send(request).await
    }

    async fn finished(&self, path: &str, body: &Value) -> (Value, u64) {
        let (status, created) = self.post(path, body).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{path}: {created}");
        let job = created["job_id"].as_u64().unwrap();
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            let (_, state) = self.get(&format!("/jobs/{job}")).await;
            if ["done", "failed", "cancelled"]
                .contains(&state["status"].as_str().unwrap_or_default())
            {
                return (state, job);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("job {job} never finished");
    }

    async fn saved_run(&self, path: &str, body: &Value) -> RunDetail {
        let (state, job) = self.finished(path, body).await;
        assert_eq!(state["status"], "done", "{state}");
        let (status, detail) = self.get(&format!("/jobs/{job}/result")).await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        serde_json::from_value(detail).unwrap()
    }
}

fn config() -> BacktestConfig {
    BacktestConfig {
        strategy: "sma_cross".into(),
        params: Params::new()
            .with("fast", ParamValue::Int(5))
            .with("slow", ParamValue::Int(20)),
        instrument: stock("AAPL"),
        timeframe: Timeframe::D1,
        from: day("2016-01-01"),
        to: day("2017-01-01"),
        starting_cash: 10_000.0,
        slippage_bps: 5.0,
        fee_override: None,
        warmup_from: None,
        close_at_end: false,
        max_leverage: 1.0,
        account_currency: td_core::AccountCurrency::Quote,
        references: vec![stock("SPY").id],
        series: Vec::new(),
        also_traded: vec![stock("MSFT")],
        start_when_all_listed: false,
    }
}

fn sweep() -> Value {
    let base = BacktestConfig {
        params: Params::new(),
        ..config()
    };
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

#[tokio::test(flavor = "multi_thread")]
async fn a_backtest_on_daily_stock_bars_shares_an_account_reads_the_etf_and_keeps_to_the_cutoff() {
    let t = TestServer::seeded().await;
    let body = serde_json::to_value(config()).unwrap();
    let detail = t.saved_run("/runs/backtest", &body).await;
    assert_eq!(detail.data_cutoff.as_deref(), Some(CUTOFF));
    let data = detail.data.as_ref().expect("a data report");
    assert_eq!(data.bars, 252, "the trading days of 2016");
    assert!(data.gaps.is_empty(), "{:?}", data.gaps);
    assert!(data.dropped.is_empty() && data.closures.is_empty());
    let SavedRun::Backtest(result) = &detail.run else {
        panic!("a saved backtest");
    };
    assert_eq!(result.config.also_traded, vec![stock("MSFT")]);
    assert_eq!(result.config.references, vec![stock("SPY").id]);
    assert!(result.metrics.trades > 0, "the test needs trades");
    for fill in &result.fills {
        assert_eq!(fill.time % DAY, 0, "{fill:?}");
        assert!(
            us_session(fill.time / DAY).is_some(),
            "a fill on a closed day: {fill:?}"
        );
    }

    let run = format!("/runs/{}", detail.id);
    let (_, reopened) = t.get(&run).await;
    assert_eq!(
        reopened["check"],
        json!({ "data_matches": true, "engine_matches": true })
    );

    let mut past = body.clone();
    past["to"] = json!(day("2017-09-01"));
    let (status, answer) = t.post("/runs/backtest", &past).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"]["code"], "after_cutoff");

    let mut hourly = body.clone();
    hourly["timeframe"] = json!("1h");
    let (status, answer) = t.post("/runs/backtest", &hourly).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("1d or longer"),
        "{answer}"
    );

    let mut holed = body.clone();
    holed["instrument"] = serde_json::to_value(stock("IBM")).unwrap();
    holed["also_traded"] = json!([]);
    let (state, _) = t.finished("/runs/backtest", &holed).await;
    assert_eq!(state["status"], "failed", "{state}");
    assert_eq!(state["error"]["code"], "data_missing", "{state}");

    let msft = stock("MSFT").id;
    let month = td_data::time::Month::containing(day("2016-06-01"));
    let mut changed = t.store.read_month(&msft, month).unwrap()[3];
    changed.close *= 1.01;
    changed.high = changed.high.max(changed.close);
    t.store.write_candles(&msft, &[changed]).unwrap();
    let (_, reopened) = t.get(&run).await;
    assert_eq!(
        reopened["check"],
        json!({ "data_matches": false, "engine_matches": true })
    );
}

struct BuysBoth {
    bar: usize,
    rejected: Arc<Mutex<Vec<String>>>,
}

impl Strategy for BuysBoth {
    fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
        self.bar += 1;
        let (aapl, msft) = (stock("AAPL").id, stock("MSFT").id);
        let all_in = |ctx: &Context, id: &InstrumentId| {
            (ctx.cash() * 0.98 / ctx.bar_of(id).unwrap().close).floor()
        };
        let note = |r: Result<_, td_engine::OrderRejected>| {
            if let Err(e) = r {
                self.rejected.lock().unwrap().push(e.to_string());
            }
        };
        match self.bar {
            1 => {
                let qty = all_in(ctx, &aapl);
                note(ctx.submit(market(aapl, Side::Buy, qty)));
                let qty = all_in(ctx, &msft);
                note(ctx.submit(market(msft, Side::Buy, qty)));
            }
            10 => {
                let held = ctx.position_of(&aapl).unwrap();
                note(ctx.submit(market(aapl, Side::Sell, held)));
            }
            11 => {
                let qty = all_in(ctx, &msft);
                note(ctx.submit(market(msft.clone(), Side::Sell, 1.0)));
                note(ctx.submit(market(msft, Side::Buy, qty)));
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stocks_of_one_account_buy_from_its_one_balance() {
    let t = TestServer::seeded().await;
    let config = config();
    let data = load_run_data(&t.store, &config, None).unwrap();
    let rejected = Arc::new(Mutex::new(Vec::new()));
    let result = td_engine::run_strategy_with_traded(
        &config,
        Box::new(BuysBoth {
            bar: 0,
            rejected: rejected.clone(),
        }),
        &data.bars,
        &data.asks,
        &data.funding,
        &data.references,
        &data.series,
        &data.also_traded,
    )
    .unwrap();
    let rejected = rejected.lock().unwrap().clone();
    assert_eq!(rejected.len(), 2, "{rejected:?}");
    assert!(
        rejected[0].contains("cannot buy for more than its cash"),
        "{rejected:?}"
    );
    assert!(
        rejected[1].contains("cannot sell more than it holds"),
        "{rejected:?}"
    );

    let (aapl, msft) = (stock("AAPL").id, stock("MSFT").id);
    let fills: Vec<(&InstrumentId, Side)> = result
        .fills
        .iter()
        .map(|f| (&f.instrument, f.side))
        .collect();
    assert_eq!(
        fills,
        [(&aapl, Side::Buy), (&aapl, Side::Sell), (&msft, Side::Buy)]
    );
    let mut cash = config.starting_cash;
    for fill in &result.fills {
        cash -= fill.side.sign() * fill.qty * fill.price + fill.fee;
        assert!(
            cash >= -1e-6,
            "the account went below zero: {cash} after {fill:?}"
        );
        assert_eq!(fill.qty, fill.qty.floor(), "whole shares: {fill:?}");
    }
    let decided = data.bars[10].open_time;
    let next = trading_minutes(decided + DAY, decided + 10 * DAY)[0] / DAY * DAY;
    assert_eq!(result.fills[2].time, next, "the next trading day's open");

    let rotated = td_engine::run_strategy_with_traded(
        &config,
        Box::new(Rotation { days: 20 }),
        &data.bars,
        &data.asks,
        &data.funding,
        &data.references,
        &data.series,
        &data.also_traded,
    )
    .unwrap();
    for id in [&aapl, &msft] {
        assert!(
            rotated
                .fills
                .iter()
                .any(|f| &f.instrument == id && f.side == Side::Buy),
            "the ETF's mean never let it buy {id}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sweeps_and_walk_forward_runs_trade_daily_stock_bars_in_one_account() {
    let t = TestServer::seeded().await;
    let sweep = sweep();
    let SavedRun::Sweep(swept) = t.saved_run("/runs/sweep", &sweep).await.run else {
        panic!("a sweep");
    };
    assert_eq!(swept.rows.len(), 4);
    assert_eq!(swept.spec.base.also_traded, vec![stock("MSFT")]);
    let wf = json!({
        "sweep": sweep,
        "windows": { "train_bars": 120, "test_bars": 40, "step_bars": 40, "mode": "rolling" },
    });
    let detail = t.saved_run("/runs/walk-forward", &wf).await;
    let SavedRun::WalkForward(windows) = &detail.run else {
        panic!("a walk-forward run");
    };
    assert_eq!(windows.spec.sweep.base.references, vec![stock("SPY").id]);
    assert!(!windows.windows.is_empty());
    let (_, reopened) = t.get(&format!("/runs/{}", detail.id)).await;
    assert_eq!(reopened["check"]["data_matches"], true, "{reopened}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_runs_its_battery_on_several_stocks_with_an_etf_as_reference() {
    let t = TestServer::seeded().await;
    let body = json!({
        "strategy": "sma_cross",
        "instruments": [stock("AAPL").id],
        "references": [stock("SPY").id],
        "also_traded": [stock("MSFT").id],
    });
    let (state, job) = t.finished("/runs/plan", &body).await;
    assert_eq!(state["status"], "done", "{state}");
    let (_, info) = t.get(&format!("/jobs/{job}")).await;
    let run_id = info["result"]["instruments"][0]["run_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{info}"))
        .to_string();
    let (status, detail) = t.get(&format!("/runs/{run_id}")).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["check"]["data_matches"], true, "{detail}");
    assert_eq!(detail["data_cutoff"], CUTOFF);
    let detail: RunDetail = serde_json::from_value(detail).unwrap();
    let SavedRun::Plan(run) = &detail.run else {
        panic!("a plan run");
    };
    assert_eq!(run.config.also_traded, vec![stock("MSFT")]);
    assert_eq!(run.config.references, vec![stock("SPY").id]);
    let sweep = run.battery.sweep.as_ref().expect("a sweep");
    let traded = sweep
        .rows
        .iter()
        .filter_map(|r| r.metrics.as_ref())
        .any(|m| m.trades > 0);
    assert!(traded, "the battery's sweep traded");
    let wf = run.battery.walk_forward.as_ref();
    assert!(
        wf.is_some_and(|w| w.windows.len() >= 4),
        "{:?}",
        run.battery.walk_forward_error
    );
    assert!(run.battery.daily_bootstrap.is_some(), "the bootstraps ran");
    assert_eq!(
        JobStatus::Done,
        serde_json::from_value(state["status"].clone()).unwrap()
    );
}
