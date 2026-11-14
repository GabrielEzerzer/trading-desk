//! A live session's stop rules and the `td-live` command against the mock venue on 127.0.0.1, fed
//! closed minutes by a scripted stream on 127.0.0.1 and a fake REST source. The strategy is a test
//! double defined here; the keys are the mock's own.
//!
//! shortcut: the rig (fake REST source, minute stream, client) repeats tests/live_session.rs
//! rather than sharing a module with it, to keep this change off a file another branch is
//! changing; move both onto one `tests/common` module once that branch has merged.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use clap::Parser;
use futures_util::SinkExt;
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, OrderRequest, OrderType,
    Side, TimeInForce, Timeframe, Venue,
};
use td_data::DataError;
use td_data::live::{Rest, SharedStream};
use td_engine::{
    Context, DayRollover, EngineError, ParamValue, Params, StopRule, StopRules, StopState, Strategy,
};
use td_live::cli::{Cli, Env, execute};
use td_live::{
    GuardTiming, LiveConfig, LiveError, LiveSession, LiveStatus, Outcome, State, StopHandle,
    live_dir, run_guarded,
};
use td_mock_bybit::{Fault, MockVenue, TEST_KEY, TEST_SECRET};
use td_orders::{Client, ClientOrderId, DemoKeys, Endpoint, Timing};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const SYMBOL: &str = "BTCUSDT";
const PRICE: f64 = 65000.0;
const STOP: f64 = 60000.0;
const QTY: f64 = 0.004;
const HOUR: Millis = 3_600_000;
const MIN: Millis = 60_000;
const CREATE: &str = "/v5/order/create";
const CANCEL: &str = "/v5/order/cancel";
const AMEND: &str = "/v5/order/amend";
const WALLET: &str = "/v5/account/wallet-balance";
const TICKERS: &str = "/v5/market/tickers";
const POSITIONS: &str = "/v5/position/list";
const INSTRUMENTS: &str = "/v5/market/instruments-info";
const ORDERS: &str = "/v5/order/realtime";
const DAY: Millis = 86_400_000;
const WARM_UP: i64 = 3;
/// A code the order route treats as a plain refusal; the mock answers it with its own message.
const REFUSED: i64 = 110007;
/// Strategy parameters for the command that trade nothing.
const NO_TRADES: &str = r#"{"buy_on":-1,"buy2_on":-1,"close_on":-1,"stop":false}"#;

fn instrument() -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Bybit,
            symbol: SYMBOL.into(),
            kind: InstrumentKind::Perpetual,
        },
        base: "BTC".into(),
        quote: "USDT".into(),
        tick_size: 0.1,
        lot_size: 0.001,
        min_qty: 0.002,
        min_notional: 100.0,
        fees: FeeSchedule {
            maker: 0.0002,
            taker: 0.00055,
            per_unit: 0.0,
        },
    }
}

fn client_for(mock: &MockVenue) -> Arc<Client> {
    let timing = Timing {
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_millis(1500),
        recv_window: Duration::from_millis(1000),
        settle: Duration::from_millis(50),
    };
    let keys = DemoKeys::for_loopback(TEST_KEY, TEST_SECRET).unwrap();
    Arc::new(Client::with_timing(Endpoint::Loopback(mock.port()), keys, timing).unwrap())
}

fn market(side: Side, qty: f64, reduce_only: bool) -> OrderRequest {
    OrderRequest {
        instrument: instrument().id,
        side,
        qty,
        order_type: OrderType::Market,
        tif: TimeInForce::Gtc,
        reduce_only,
        client_tag: None,
    }
}

/// The test double. On live bar `buy_on` (the first live bar is 0) it buys [`QTY`], with an
/// attached stop at [`STOP`] when `stop` is set; on `buy2_on` it buys [`QTY`] again; on `close_on`
/// it cancels its open orders and sells its position reduce-only.
struct Script {
    buy_on: i64,
    buy2_on: i64,
    close_on: i64,
    stop: bool,
    bar: i64,
}

impl Strategy for Script {
    fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
        if ctx.is_warming_up() {
            return;
        }
        if self.bar == self.buy_on {
            if self.stop {
                ctx.submit_with_stop(market(Side::Buy, QTY, false), STOP)
                    .expect("buy with stop");
            } else {
                ctx.submit(market(Side::Buy, QTY, false)).expect("buy");
            }
        }
        if self.bar == self.buy2_on {
            ctx.submit(market(Side::Buy, QTY, false))
                .expect("second buy");
        }
        if self.bar == self.close_on {
            for id in ctx.open_orders().iter().map(|o| o.id).collect::<Vec<_>>() {
                ctx.cancel(id);
            }
            let position = ctx.position();
            if position > 0.0 {
                ctx.submit(market(Side::Sell, position, true))
                    .expect("sell");
            }
        }
        self.bar += 1;
    }
}

fn build(name: &str, params: &Params) -> Result<Box<dyn Strategy>, EngineError> {
    if name != "script" {
        return td_engine::build_strategy(name, params);
    }
    Ok(Box::new(Script {
        buy_on: params.int("buy_on")?,
        buy2_on: params.int("buy2_on")?,
        close_on: params.int("close_on")?,
        stop: params.bool("stop")?,
        bar: 0,
    }))
}

fn rules(daily: f64, max: f64, cap: f64) -> StopRules {
    StopRules {
        daily_loss_share: daily,
        max_loss_share: max,
        open_notional_cap: cap,
        rollover: DayRollover::default(),
    }
}

/// Wide rules that do not fire unless a test moves the venue a long way.
fn wide() -> StopRules {
    rules(0.05, 0.10, 100_000.0)
}

fn config(buy_on: i64, stops: StopRules) -> LiveConfig {
    script(buy_on, -1, -1, stops)
}

fn script(buy_on: i64, buy2_on: i64, close_on: i64, stops: StopRules) -> LiveConfig {
    let params = Params::new()
        .with("buy_on", ParamValue::Int(buy_on))
        .with("buy2_on", ParamValue::Int(buy2_on))
        .with("close_on", ParamValue::Int(close_on))
        .with("stop", ParamValue::Bool(true));
    LiveConfig {
        strategy: "script".into(),
        params,
        instrument: instrument(),
        timeframe: Timeframe::H1,
        slippage_bps: 0.0,
        fees: None,
        max_leverage: 1.0,
        stops,
    }
}

fn flat(open_time: Millis) -> Candle {
    Candle {
        open_time,
        open: PRICE,
        high: PRICE,
        low: PRICE,
        close: PRICE,
        volume: 1.0,
        quote_volume: PRICE,
    }
}

/// Minutes from `start` on, [`WARM_UP`] hourly bars before it, no funding, and a clock fixed at
/// `start`.
struct FakeRest {
    start: Millis,
}

impl Rest for FakeRest {
    fn klines(
        &self,
        _: &InstrumentId,
        timeframe: Timeframe,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<Candle>, DataError> {
        let (first, step, to) = if timeframe == Timeframe::M1 {
            (self.start, MIN, to)
        } else {
            (self.start - WARM_UP * HOUR, HOUR, to.min(self.start))
        };
        let mut t = from.max(first).div_euclid(step) * step;
        let mut out = Vec::new();
        while t < to {
            if t >= from {
                out.push(flat(t));
            }
            t += step;
        }
        Ok(out)
    }

    fn funding(
        &self,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        Ok(Vec::new())
    }

    fn now(&self) -> Millis {
        self.start
    }
}

/// A minute stream on 127.0.0.1: frames sent with [`Feed::minute`] go to the first connection.
struct Feed {
    url: String,
    tx: mpsc::UnboundedSender<String>,
    opened: Arc<AtomicBool>,
}

impl Feed {
    fn new(runtime: &Runtime) -> Feed {
        let listener = runtime
            .block_on(TcpListener::bind("127.0.0.1:0"))
            .expect("bind");
        let url = format!("ws://{}/", listener.local_addr().expect("address"));
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        runtime.spawn(async move {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut socket) = tokio_tungstenite::accept_async(tcp).await else {
                return;
            };
            while let Some(frame) = rx.recv().await {
                if socket.send(Message::text(frame)).await.is_err() {
                    return;
                }
            }
        });
        Feed {
            url,
            tx,
            opened: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Sends the closed minute opening at `t`, which completes every bar that ends by `t`.
    fn minute(&self, t: Millis) {
        let frame = format!(
            r#"{{"topic":"kline.1.{SYMBOL}","data":[{{"start":{t},"end":{},"interval":"1","open":"{PRICE}","close":"{PRICE}","high":"{PRICE}","low":"{PRICE}","volume":"1","turnover":"{PRICE}","confirm":true,"timestamp":{t}}}],"ts":{t},"type":"snapshot"}}"#,
            t + MIN - 1
        );
        self.tx.send(frame).expect("feed server");
    }

    fn opener(&self) -> impl FnOnce() -> Arc<SharedStream> + Send + 'static {
        let (url, opened) = (self.url.clone(), self.opened.clone());
        move || {
            opened.store(true, Ordering::SeqCst);
            Arc::new(SharedStream::open(instrument().id, url))
        }
    }
}

/// A guarded run on its own thread, with the lines it wrote.
struct Guarded {
    lines: Arc<Mutex<Vec<String>>>,
    stop: StopHandle,
    thread: JoinHandle<Outcome>,
}

impl Guarded {
    fn text(&self) -> String {
        self.lines.lock().unwrap().join("\n")
    }

    /// Waits until a line contains `needle`, or fails after 20 s.
    fn wait_line(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.text().contains(needle) {
            assert!(
                !self.thread.is_finished(),
                "the run ended before a line with {needle:?}:\n{}",
                self.text()
            );
            assert!(
                Instant::now() < deadline,
                "no line with {needle:?} within 20 s:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for the run to end on its own, or fails after 30 s.
    fn finish(self) -> (Outcome, String) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.thread.is_finished() {
            assert!(
                Instant::now() < deadline,
                "the run did not end within 30 s:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let text = self.text();
        (self.thread.join().expect("the guard thread"), text)
    }

    fn stop(self) -> (Outcome, String) {
        self.stop.stop();
        self.finish()
    }
}

struct Rig {
    mock: MockVenue,
    client: Arc<Client>,
    dir: tempfile::TempDir,
    rest: Arc<FakeRest>,
    runtime: Runtime,
    start: Millis,
}

impl Rig {
    fn new() -> Rig {
        let mock = MockVenue::start();
        mock.add_instrument(&instrument(), PRICE);
        let start = Timeframe::H1.floor(mock.now()) - HOUR;
        Rig {
            client: client_for(&mock),
            mock,
            dir: tempfile::tempdir().expect("temp dir"),
            rest: Arc::new(FakeRest { start }),
            runtime: Runtime::new().expect("runtime"),
            start,
        }
    }

    fn data(&self) -> &Path {
        self.dir.path()
    }

    fn create(&self, id: &str, config: LiveConfig) -> Result<LiveSession, LiveError> {
        LiveSession::create(self.data(), id, config, self.client.clone(), build)
    }

    fn open(&self, id: &str) -> Result<LiveSession, LiveError> {
        LiveSession::open(self.data(), id, self.client.clone(), build)
    }

    fn feed(&self) -> Feed {
        Feed::new(&self.runtime)
    }

    fn bar(&self, i: i64) -> Millis {
        self.start + i * HOUR
    }

    fn guard(&self, live: LiveSession, feed: &Feed, timing: GuardTiming) -> Guarded {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let stop = StopHandle::new();
        let (rest, opener, out, handle) = (
            self.rest.clone(),
            feed.opener(),
            lines.clone(),
            stop.clone(),
        );
        let thread = std::thread::spawn(move || {
            run_guarded(live, rest, opener, timing, &handle, &mut |line| {
                out.lock().unwrap().push(line.to_string())
            })
        });
        Guarded {
            lines,
            stop,
            thread,
        }
    }

    /// An `Env` for the command on this rig, with `timing`.
    fn env(&self, feed: &Feed, timing: GuardTiming) -> Env {
        let client = self.client.clone();
        let (url, data) = (feed.url.clone(), self.data().to_path_buf());
        let opened = feed.opened.clone();
        Env {
            connect: Box::new(move || Ok(client.clone())),
            rest: self.rest.clone(),
            stream: Arc::new(move |id| {
                opened.store(true, Ordering::SeqCst);
                Arc::new(SharedStream::open(id, url.clone()))
            }),
            build,
            data_dir: Box::new(move || Ok(data.clone())),
            timing,
            stop: StopHandle::new(),
        }
    }

    /// Order requests that change something at the venue.
    fn sends(&self) -> usize {
        [CREATE, CANCEL, AMEND]
            .iter()
            .map(|p| self.mock.request_count(p))
            .sum()
    }

    fn path(&self, id: &str, file: &str) -> PathBuf {
        live_dir(self.data(), id).join(file)
    }

    fn json(&self, id: &str, file: &str) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(self.path(id, file)).expect(file)).expect(file)
    }

    fn write_json(&self, id: &str, file: &str, value: &serde_json::Value) {
        fs::write(
            self.path(id, file),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .expect(file);
    }

    fn saved_status(&self, id: &str) -> LiveStatus {
        serde_json::from_value(self.json(id, "status.json")).expect("status reads")
    }

    /// The flatten orders the mock holds: client ids with the `f` role.
    fn flatten_orders(&self) -> Vec<td_mock_bybit::MockOrder> {
        self.mock
            .orders()
            .into_iter()
            .filter(|o| {
                o.link_id
                    .rsplit('-')
                    .next()
                    .is_some_and(|r| r.starts_with('f'))
            })
            .collect()
    }

    fn resting(&self) -> usize {
        self.mock
            .orders()
            .iter()
            .filter(|o| ["New", "Untriggered", "PartiallyFilled"].contains(&o.status.as_str()))
            .count()
    }

    /// Waits until `done`, or fails after 20 s.
    fn wait(&self, what: &str, done: impl Fn(&Rig) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(self) {
            assert!(Instant::now() < deadline, "no {what} within 20 s");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Creates `id` buying on bar 0 with a stop, runs it guarded until the entry is filled and its
    /// stop rests at the venue, and returns the run.
    fn entered(&self, id: &str, stops: StopRules, feed: &Feed, timing: GuardTiming) -> Guarded {
        let live = self.create(id, config(0, stops)).expect("creates");
        let run = self.guard(live, feed, timing);
        run.wait_line("check: no stop fired");
        feed.minute(self.bar(1));
        self.wait("entry and its stop", |r| {
            r.mock.position(SYMBOL) == QTY && r.resting() == 1
        });
        run
    }

    /// Lowers the wallet by `loss`, as a realised loss would.
    fn lose(&self, loss: f64) {
        self.mock.set_wallet(self.mock.wallet_balance() - loss);
    }
}

/// Checks on a quick timer, with short flatten pauses.
fn quick() -> GuardTiming {
    GuardTiming {
        interval: Duration::from_millis(100),
        pause: Duration::from_millis(50),
        rounds: 6,
        unreadable: 4,
    }
}

/// Checks only after bars: a timer too slow to fire within a test.
fn bars_only() -> GuardTiming {
    GuardTiming {
        interval: Duration::from_secs(600),
        ..quick()
    }
}

/// Asserts a fired session ended flat at the venue and can never run again.
fn assert_ended_flat(rig: &Rig, id: &str, rule: StopRule) {
    assert_eq!(rig.mock.position(SYMBOL), 0.0, "flat at the venue");
    assert_eq!(rig.resting(), 0, "no order rests");
    let stops = rig.json(id, "stops.json");
    assert_eq!(
        stops["stops"]["fired"]["rule"],
        serde_json::to_value(rule).unwrap(),
        "{stops}"
    );
    assert_eq!(stops["flatten"]["result"]["kind"], "flat", "{stops}");
    assert!(
        matches!(rig.saved_status(id).state, State::Ended { .. }),
        "{:?}",
        rig.saved_status(id).state
    );
    assert!(matches!(rig.open(id), Err(LiveError::StopFired(_))));
}

/// The flatten steps in the order they were recorded.
fn steps(rig: &Rig, id: &str) -> Vec<String> {
    rig.json(id, "stops.json")["flatten"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn creation_is_refused_without_valid_rules_and_nothing_is_written_or_read() {
    let rig = Rig::new();
    for (name, bad) in [
        ("zero daily", rules(0.0, 0.1, 1000.0)),
        ("max above 1", rules(0.05, 1.5, 1000.0)),
        ("no cap", rules(0.05, 0.1, 0.0)),
        ("nan cap", rules(0.05, 0.1, f64::NAN)),
    ] {
        let err = rig.create("bad", config(0, bad)).err().expect(name);
        assert!(matches!(err, LiveError::InvalidStops(_)), "{name}: {err}");
    }
    assert!(!live_dir(rig.data(), "bad").exists());
    assert_eq!(rig.mock.request_count(WALLET), 0, "the venue is not read");
}

#[test]
fn the_rules_round_trip_and_stay_as_created() {
    let rig = Rig::new();
    let rules = rules(0.03, 0.08, 5000.0);
    drop(rig.create("trip", config(0, rules)).expect("creates"));
    let reopened = rig.open("trip").expect("reopens");
    assert_eq!(reopened.config().config.stops, rules);
    let stops = rig.json("trip", "stops.json");
    assert_eq!(
        stops["stops"]["rules"],
        serde_json::to_value(rules).unwrap()
    );
    assert_eq!(stops["stops"]["starting_balance"], 100_000.0);
    assert_eq!(stops["stops"]["fired"], serde_json::Value::Null);
}

#[test]
fn open_refuses_a_missing_or_edited_stop_file_and_an_edited_config() {
    let rig = Rig::new();
    type Edit = fn(&Rig, &str);
    let edits: [(&str, Edit); 8] = [
        ("missing", |rig, id| {
            fs::remove_file(rig.path(id, "stops.json")).unwrap()
        }),
        ("unreadable", |rig, id| {
            fs::write(rig.path(id, "stops.json"), "{").unwrap()
        }),
        ("stop file balance", |rig, id| {
            let mut v = rig.json(id, "stops.json");
            v["stops"]["starting_balance"] = 50_000.0.into();
            rig.write_json(id, "stops.json", &v);
        }),
        ("config balance", |rig, id| {
            let mut v = rig.json(id, "config.json");
            v["starting_balance"] = 1_000_000.0.into();
            rig.write_json(id, "config.json", &v);
        }),
        ("config rule", |rig, id| {
            let mut v = rig.json(id, "config.json");
            v["stops"]["max_loss_share"] = 0.9.into();
            rig.write_json(id, "config.json", &v);
        }),
        // No stop fired, yet the flatten record shows a flatten ran.
        ("flatten id claimed", |rig, id| {
            let mut v = rig.json(id, "stops.json");
            v["flatten"]["last_id"] = 1.into();
            rig.write_json(id, "stops.json", &v);
        }),
        ("flatten result", |rig, id| {
            let mut v = rig.json(id, "stops.json");
            v["flatten"]["result"] = serde_json::json!({ "kind": "flat" });
            rig.write_json(id, "stops.json", &v);
        }),
        // The day starting at the maximum loss line of 90 000, with no stop fired.
        ("day start at the line", |rig, id| {
            let mut v = rig.json(id, "stops.json");
            v["stops"]["day_start_equity"] = 90_000.0.into();
            rig.write_json(id, "stops.json", &v);
        }),
    ];
    for (i, (name, edit)) in edits.into_iter().enumerate() {
        let id = format!("edit-{i}");
        drop(rig.create(&id, config(0, wide())).expect("creates"));
        edit(&rig, &id);
        let err = rig.open(&id).err().expect(name);
        assert!(matches!(err, LiveError::StopFile { .. }), "{name}: {err}");
    }
}

fn a_loss_fire(rule: StopRule, stops: StopRules) {
    let rig = Rig::new();
    let feed = rig.feed();
    let run = rig.entered("loss", stops, &feed, bars_only());
    let sends = rig.sends();
    rig.lose(6000.0);
    feed.minute(rig.bar(2));
    let (outcome, text) = run.finish();

    assert!(
        matches!(&outcome, Outcome::Fired { fired, left: None, unsaved: None } if fired.rule == rule),
        "{outcome:?}\n{text}"
    );
    assert_eq!(outcome.exit_code(), 2);
    assert_ended_flat(&rig, "loss", rule);
    // The stop was cancelled and the position closed with one reduce-only order: two sends.
    assert_eq!(rig.sends(), sends + 2, "{text}");
    let closes = rig.flatten_orders();
    assert_eq!(closes.len(), 1, "{closes:?}");
    assert!(closes[0].reduce_only && closes[0].side == Side::Sell);
    assert!(
        rig.mock
            .orders()
            .iter()
            .any(|o| o.trigger == Some(STOP) && o.status == "Deactivated")
    );
    // The fired state was saved before anything was sent.
    let steps = steps(&rig, "loss");
    assert!(
        steps[0].contains("saved before anything is sent"),
        "{steps:?}"
    );
    assert!(steps.iter().any(|s| s.contains("cancelled")), "{steps:?}");
    assert!(
        steps.iter().any(|s| s.contains("reduce-only market")),
        "{steps:?}"
    );
    // The run halted: the halt and the fire are in the output, and the status says ended.
    assert!(text.contains("stop fired"), "{text}");
}

#[test]
fn a_daily_loss_fire_saves_halts_cancels_closes_and_ends_flat() {
    a_loss_fire(StopRule::DailyLoss, rules(0.05, 0.5, 100_000.0));
}

#[test]
fn a_max_loss_fire_saves_halts_cancels_closes_and_ends_flat() {
    a_loss_fire(StopRule::MaxLoss, rules(0.5, 0.05, 100_000.0));
}

#[test]
fn the_timer_fires_a_stop_with_no_bar_arriving() {
    let rig = Rig::new();
    let feed = rig.feed();
    let run = rig.entered("timer", wide(), &feed, quick());
    run.wait_line(&format!("bar fed: {}", rig.bar(0)));
    rig.lose(11_000.0);
    let (outcome, text) = run.finish();
    assert!(
        matches!(&outcome, Outcome::Fired { fired, left: None, unsaved: None } if fired.rule == StopRule::MaxLoss),
        "{outcome:?}\n{text}"
    );
    assert_ended_flat(&rig, "timer", StopRule::MaxLoss);
    assert_eq!(rig.saved_status("timer").last_bar, Some(rig.bar(0)));
}

#[test]
fn the_cap_refuses_an_entry_before_sending_and_lets_a_reduce_only_close_through() {
    let rig = Rig::new();
    let feed = rig.feed();
    // One buy is 260 of notional: the second would take it to 520, above the cap of 400.
    let live = rig
        .create("cap", script(0, 1, 2, rules(0.05, 0.1, 400.0)))
        .expect("creates");
    let run = rig.guard(live, &feed, bars_only());
    run.wait_line("check: no stop fired");
    feed.minute(rig.bar(1));
    rig.wait("entry", |r| r.mock.position(SYMBOL) == QTY);
    feed.minute(rig.bar(4));
    run.wait_line(&format!("bar fed: {}", rig.bar(3)));
    let (outcome, text) = run.stop();
    assert_eq!(outcome, Outcome::Stopped, "{text}");

    let entries: Vec<_> = rig
        .mock
        .orders()
        .into_iter()
        .filter(|o| !o.reduce_only)
        .collect();
    assert_eq!(entries.len(), 1, "the second buy was not sent: {entries:?}");
    assert_eq!(
        rig.mock.position(SYMBOL),
        0.0,
        "the reduce-only close went through"
    );
    let log = fs::read_to_string(rig.path("cap", "events.jsonl")).unwrap();
    assert!(log.contains("above the cap of 400"), "{log}");
    assert!(text.contains("limits are not enforced"), "{text}");
}

#[test]
fn the_cap_fires_when_the_mark_price_drifts_up() {
    let rig = Rig::new();
    let feed = rig.feed();
    let run = rig.entered("drift", rules(0.05, 0.1, 300.0), &feed, quick());
    // 0.004 at 80 000 is 320 of open notional.
    rig.mock.set_price(SYMBOL, 80_000.0);
    let (outcome, text) = run.finish();
    assert!(
        matches!(&outcome, Outcome::Fired { fired, left: None, unsaved: None } if fired.rule == StopRule::OpenNotional),
        "{outcome:?}\n{text}"
    );
    assert_ended_flat(&rig, "drift", StopRule::OpenNotional);
}

#[test]
fn a_close_whose_answer_is_dropped_after_it_applied_is_settled_by_reading() {
    let rig = Rig::new();
    let feed = rig.feed();
    let run = rig.entered("dropped", wide(), &feed, quick());
    rig.mock.inject(CREATE, Fault::DropAfterApply);
    rig.lose(11_000.0);
    let (outcome, text) = run.finish();
    assert!(
        matches!(outcome, Outcome::Fired { left: None, .. }),
        "{outcome:?}\n{text}"
    );
    assert_eq!(rig.flatten_orders().len(), 1, "exactly one close: {text}");
    assert_ended_flat(&rig, "dropped", StopRule::MaxLoss);
}

/// Writes a fired max-loss stop into `id`'s stop file, as a guard that died right after saving it
/// would have left it, with `last_id` flatten ids claimed.
fn crash_after_fire(rig: &Rig, id: &str, last_id: u64) {
    let config = rig.json(id, "config.json");
    let rules: StopRules = serde_json::from_value(config["stops"].clone()).unwrap();
    let starting = config["starting_balance"].as_f64().unwrap();
    let created = config["created_at"].as_i64().unwrap();
    let mut state = StopState::start(rules, starting, created).unwrap();
    let fired = state.check(rig.mock.now(), 1.0, 0.0).unwrap();
    assert!(fired.is_some());
    let file = serde_json::json!({
        "stops": state,
        "flatten": {
            "last_id": last_id,
            "steps": ["fired, saved before anything is sent"],
            "result": null,
        },
    });
    rig.write_json(id, "stops.json", &file);
}

/// Runs the command with `args` on this rig and returns its exit code and output.
fn td_live(rig: &Rig, feed: &Feed, args: &[&str]) -> (u8, String) {
    let cli = Cli::try_parse_from(std::iter::once("td-live").chain(args.iter().copied())).unwrap();
    let mut lines = Vec::new();
    let code = execute(cli, &rig.env(feed, quick()), &mut |l| {
        lines.push(l.to_string())
    });
    (code, lines.join("\n"))
}

fn resume(rig: &Rig, id: &str, feed: &Feed) -> (u8, String) {
    td_live(rig, feed, &["resume", "--id", id])
}

/// The command line that starts session `id` in `data` with the script strategy and `params`.
fn start_cli(id: &str, data: &Path, params: &str) -> Cli {
    Cli::try_parse_from([
        "td-live",
        "start",
        "--id",
        id,
        "--strategy",
        "script",
        "--params",
        params,
        "--symbol",
        SYMBOL,
        "--timeframe",
        "1h",
        "--daily-loss",
        "0.05",
        "--max-loss",
        "0.1",
        "--notional-cap",
        "1000",
        "--data-dir",
        data.to_str().unwrap(),
    ])
    .unwrap()
}

/// Asserts that neither the key, the secret nor the signature of any request the mock saw is in
/// `text` or in a file of session `id`'s folder.
fn assert_no_credential(rig: &Rig, id: &str, text: &str) {
    let mut seen = text.to_string();
    for entry in fs::read_dir(live_dir(rig.data(), id)).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            seen.push_str(&String::from_utf8_lossy(&fs::read(&path).unwrap()));
        }
    }
    assert!(!seen.contains(TEST_KEY), "the key leaked");
    assert!(!seen.contains(TEST_SECRET), "the secret leaked");
    let signatures: Vec<String> = rig
        .mock
        .requests()
        .into_iter()
        .flat_map(|r| r.headers)
        .filter(|(name, value)| name == "x-bapi-sign" && !value.is_empty())
        .map(|(_, value)| value)
        .collect();
    assert!(!signatures.is_empty(), "the mock saw no signed request");
    for signature in &signatures {
        assert!(!seen.contains(signature.as_str()), "a signature leaked");
    }
}

#[test]
fn a_crash_after_the_fire_was_saved_finishes_the_flatten_on_resume_without_a_double_close() {
    let rig = Rig::new();
    let feed = rig.feed();
    let (outcome, _) = rig.entered("crash", wide(), &feed, bars_only()).stop();
    assert_eq!(outcome, Outcome::Stopped);
    crash_after_fire(&rig, "crash", 0);

    let feed = rig.feed();
    let (code, text) = resume(&rig, "crash", &feed);
    assert_eq!(code, 2, "{text}");
    assert_eq!(rig.flatten_orders().len(), 1, "{text}");
    assert_ended_flat(&rig, "crash", StopRule::MaxLoss);

    // A second resume finds the flatten finished and sends nothing.
    let sends = rig.sends();
    let (code, text) = resume(&rig, "crash", &feed);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("stays halted"), "{text}");
    assert_eq!(rig.sends(), sends);
    assert!(!feed.opened.load(Ordering::SeqCst), "no run started");
}

#[test]
fn a_crash_after_the_close_was_sent_does_not_close_twice() {
    let rig = Rig::new();
    let feed = rig.feed();
    let (_, _) = rig.entered("sent", wide(), &feed, bars_only()).stop();
    // The guard claimed f1, sent the close, and died before recording it. The close has filled
    // only half the position and still rests.
    crash_after_fire(&rig, "sent", 1);
    let prefix = rig.json("sent", "config.json")["prefix"]
        .as_str()
        .unwrap()
        .to_string();
    let f1 = format!("{prefix}-f1");
    let mut close = market(Side::Sell, QTY, true);
    close.tif = TimeInForce::Ioc;
    rig.mock.cap_next_fill(0.002);
    rig.client
        .place(
            &instrument(),
            &close,
            &ClientOrderId::new(f1.clone()).unwrap(),
            PRICE,
        )
        .expect("the close the crashed guard sent");
    assert!((rig.mock.position(SYMBOL) - 0.002).abs() < 1e-9);
    let creates = rig.mock.request_count(CREATE);

    let (code, text) = resume(&rig, "sent", &feed);
    assert_eq!(code, 2, "{text}");
    // One more close, under the next id, for what the venue still held; f1's rest was cancelled.
    assert_eq!(
        rig.mock.request_count(CREATE),
        creates + 1,
        "one close for what was left: {text}"
    );
    let closes = rig.flatten_orders();
    assert_eq!(closes.len(), 2, "{closes:?}");
    let first = closes.iter().find(|o| o.link_id == f1).expect("f1");
    assert_ne!(
        first.status, "PartiallyFilled",
        "f1 still rests: {closes:?}"
    );
    let second = closes
        .iter()
        .find(|o| o.link_id == format!("{prefix}-f2"))
        .expect("the next close is f2");
    assert!(
        second.reduce_only && (second.qty - 0.002).abs() < 1e-9,
        "sized from the venue's position: {second:?}"
    );
    assert_eq!(rig.json("sent", "stops.json")["flatten"]["last_id"], 2);
    assert_ended_flat(&rig, "sent", StopRule::MaxLoss);
}

#[test]
fn a_venue_unreadable_for_the_bound_halts_the_run_without_sending() {
    let rig = Rig::new();
    let feed = rig.feed();
    let run = rig.entered("blind", wide(), &feed, quick());
    run.wait_line(&format!("bar fed: {}", rig.bar(0)));
    let sends = rig.sends();
    rig.mock.inject_always(WALLET, Fault::ServerError);
    run.wait_line("check failed (4 of 4");
    run.wait_line("halt:");
    rig.wait("halted status", |r| {
        matches!(r.saved_status("blind").state, State::Halted { .. })
    });
    rig.mock.clear_faults();
    let (outcome, text) = run.stop();
    assert!(
        matches!(&outcome, Outcome::Halted(reason) if reason.contains("could not be read")),
        "{outcome:?}\n{text}"
    );
    assert_eq!(outcome.exit_code(), 1);
    assert_eq!(rig.sends(), sends, "nothing sent: {text}");
    assert_eq!(rig.mock.position(SYMBOL), QTY, "nothing closed");
    assert_eq!(rig.resting(), 1, "the stop still rests");
}

#[test]
fn a_flatten_that_runs_out_of_rounds_records_fired_not_flat() {
    let rig = Rig::new();
    let feed = rig.feed();
    let run = rig.entered("stuck", wide(), &feed, quick());
    rig.mock.inject_always(CREATE, Fault::Answer(10014));
    rig.lose(11_000.0);
    let (outcome, text) = run.finish();
    assert!(
        matches!(&outcome, Outcome::Fired { left: Some(left), .. } if left.contains("0.004")),
        "{outcome:?}\n{text}"
    );
    assert_eq!(outcome.exit_code(), 3);
    let stops = rig.json("stuck", "stops.json");
    assert_eq!(stops["flatten"]["result"]["kind"], "not_flat", "{stops}");
    assert!(
        matches!(rig.saved_status("stuck").state, State::Ended { reason } if reason.contains("fired, not flat"))
    );
    // A fired session that did not end flat finishes its flatten on resume.
    rig.mock.clear_faults();
    let (code, text) = resume(&rig, "stuck", &feed);
    assert_eq!(code, 2, "{text}");
    assert_ended_flat(&rig, "stuck", StopRule::MaxLoss);
}

#[test]
fn resume_fires_before_any_bar_when_equity_fell_while_the_command_was_down() {
    let rig = Rig::new();
    let feed = rig.feed();
    let (outcome, _) = rig.entered("down", wide(), &feed, bars_only()).stop();
    assert_eq!(outcome, Outcome::Stopped);
    let last_bar = rig.saved_status("down").last_bar;
    rig.lose(11_000.0);

    let feed = rig.feed();
    let live = rig.open("down").expect("opens");
    let (outcome, text) = rig.guard(live, &feed, bars_only()).finish();
    assert!(
        matches!(&outcome, Outcome::Fired { fired, left: None, unsaved: None } if fired.rule == StopRule::MaxLoss),
        "{outcome:?}\n{text}"
    );
    assert!(!feed.opened.load(Ordering::SeqCst), "no bar was fed");
    assert_eq!(rig.saved_status("down").last_bar, last_bar);
    assert_ended_flat(&rig, "down", StopRule::MaxLoss);
}

#[test]
fn both_files_edited_to_ten_times_the_balance_still_end_halted() {
    let rig = Rig::new();
    let feed = rig.feed();
    let (_, _) = rig.entered("edited", wide(), &feed, bars_only()).stop();
    let mut config = rig.json("edited", "config.json");
    config["starting_balance"] = 1_000_000.0.into();
    rig.write_json("edited", "config.json", &config);
    let mut stops = rig.json("edited", "stops.json");
    stops["stops"]["starting_balance"] = 1_000_000.0.into();
    stops["stops"]["day_start_equity"] = 1_000_000.0.into();
    rig.write_json("edited", "stops.json", &stops);

    let live = rig.open("edited").expect("the edits agree, so it opens");
    let (outcome, text) = rig.guard(live, &rig.feed(), bars_only()).finish();
    assert!(
        matches!(&outcome, Outcome::Fired { fired, left: None, unsaved: None } if fired.rule == StopRule::MaxLoss),
        "the venue's real equity is far below the edited line: {outcome:?}\n{text}"
    );
    assert_ended_flat(&rig, "edited", StopRule::MaxLoss);
}

#[test]
fn the_command_starts_a_session_and_no_key_reaches_a_file_or_the_output() {
    let rig = Rig::new();
    rig.mock.set_echo_credentials(true);
    let feed = rig.feed();
    // A start whose instrument read is refused with a message that echoes the key and signature
    // fails and prints the error without them.
    rig.mock.inject(INSTRUMENTS, Fault::Answer(REFUSED));
    let mut refused = Vec::new();
    let code = execute(
        start_cli("refused", rig.data(), NO_TRADES),
        &rig.env(&feed, quick()),
        &mut |l| refused.push(l.to_string()),
    );
    let refused = refused.join("\n");
    assert_eq!(code, 1, "{refused}");
    assert!(
        refused.contains("error:") && refused.contains("message withheld"),
        "{refused}"
    );
    assert!(!live_dir(rig.data(), "refused").exists());

    // Enough failures allowed that the held fault below cannot halt the run.
    let env = rig.env(
        &feed,
        GuardTiming {
            unreadable: 100,
            ..quick()
        },
    );
    let stop = env.stop.clone();
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let out = lines.clone();
    let data = rig.data().to_path_buf();
    let thread = std::thread::spawn(move || {
        // It buys on the first live bar, without a stop.
        let cli = start_cli(
            "cmd",
            &data,
            r#"{"buy_on":0,"buy2_on":-1,"close_on":-1,"stop":false}"#,
        );
        execute(cli, &env, &mut |l| out.lock().unwrap().push(l.to_string()))
    });
    let text = || lines.lock().unwrap().join("\n");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !text().contains("check: no stop fired") {
        assert!(
            Instant::now() < deadline && !thread.is_finished(),
            "{}",
            text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Once the run has made its own first venue read, a refused read whose message echoes the
    // key and signature, then a fire.
    rig.wait("the run", |r| {
        matches!(r.saved_status("cmd").state, State::Running)
    });
    // The entry is refused with a message that echoes the key and signature; the broker records
    // the refusal in the log.
    rig.mock.inject(CREATE, Fault::Answer(REFUSED));
    feed.minute(rig.bar(1));
    rig.wait("the refusal in the log", |r| {
        fs::read_to_string(r.path("cmd", "events.jsonl"))
            .is_ok_and(|log| log.contains("message withheld"))
    });
    rig.mock.inject_always(WALLET, Fault::Answer(10014));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !text().contains("check failed") {
        assert!(
            Instant::now() < deadline && !thread.is_finished(),
            "{}",
            text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    rig.mock.clear_faults();
    rig.lose(11_000.0);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !thread.is_finished() {
        assert!(Instant::now() < deadline, "{}", text());
        std::thread::sleep(Duration::from_millis(20));
    }
    let code = thread.join().unwrap();
    drop(stop);
    let text = text();
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("created session cmd"), "{text}");
    assert_ended_flat(&rig, "cmd", StopRule::MaxLoss);
    assert_no_credential(&rig, "cmd", &format!("{refused}\n{text}"));
}

#[test]
fn status_reads_the_files_and_flatten_ends_a_stopped_session() {
    let rig = Rig::new();
    let feed = rig.feed();
    let (outcome, _) = rig.entered("own", wide(), &feed, bars_only()).stop();
    assert_eq!(outcome, Outcome::Stopped);
    let env = rig.env(&feed, quick());

    let requests = rig.mock.requests().len();
    let mut lines = Vec::new();
    let cli = Cli::try_parse_from(["td-live", "status", "--id", "own"]).unwrap();
    assert_eq!(execute(cli, &env, &mut |l| lines.push(l.to_string())), 0);
    assert_eq!(
        rig.mock.requests().len(),
        requests,
        "status touches no venue"
    );
    let text = lines.join("\n");
    assert!(
        text.contains("\"stopped\"") || text.contains("Stopped"),
        "{text}"
    );
    assert!(text.contains("starting_balance"), "{text}");

    let mut lines = Vec::new();
    let cli = Cli::try_parse_from(["td-live", "flatten", "--id", "own"]).unwrap();
    assert_eq!(execute(cli, &env, &mut |l| lines.push(l.to_string())), 0);
    assert_eq!(rig.mock.position(SYMBOL), 0.0);
    assert_eq!(rig.resting(), 0);
    assert!(matches!(rig.saved_status("own").state, State::Ended { .. }));
    assert!(matches!(rig.open("own"), Err(LiveError::Ended(_))));
}

#[test]
fn a_flatten_and_a_resume_whose_requests_are_refused_with_the_key_write_no_credential() {
    let rig = Rig::new();
    rig.mock.set_echo_credentials(true);
    let feed = rig.feed();
    let (_, _) = rig.entered("keys", wide(), &feed, bars_only()).stop();
    crash_after_fire(&rig, "keys", 0);
    // The resumed flatten's first cancel and first close are refused with messages that echo the
    // key and signature.
    rig.mock.inject(CANCEL, Fault::Answer(REFUSED));
    rig.mock.inject(CREATE, Fault::Answer(REFUSED));
    let (code, resumed) = resume(&rig, "keys", &feed);
    assert_eq!(code, 2, "{resumed}");
    assert_ended_flat(&rig, "keys", StopRule::MaxLoss);
    // A flatten whose first position read is refused the same way.
    rig.mock.inject(POSITIONS, Fault::Answer(REFUSED));
    let (code, flattened) = td_live(&rig, &feed, &["flatten", "--id", "keys"]);
    assert_eq!(code, 2, "{flattened}");

    let steps = steps(&rig, "keys");
    let refusals = steps
        .iter()
        .filter(|s| s.contains("message withheld"))
        .count();
    assert!(
        refusals >= 3,
        "a cancel, a close and a read refused: {steps:?}"
    );
    assert!(resumed.contains("message withheld"), "{resumed}");
    assert!(flattened.contains("message withheld"), "{flattened}");
    assert_no_credential(&rig, "keys", &format!("{resumed}\n{flattened}"));
}

#[test]
fn a_protective_stop_is_placed_when_the_venue_turns_unreadable_during_fill_confirmation() {
    let rig = Rig::new();
    let feed = rig.feed();
    let live = rig.create("confirm", config(0, wide())).expect("creates");
    // A timer slow enough that the entry goes out between the third and fourth failed check.
    let timing = GuardTiming {
        interval: Duration::from_secs(1),
        ..quick()
    };
    let run = rig.guard(live, &feed, timing);
    run.wait_line("check: no stop fired");
    rig.mock.inject_always(WALLET, Fault::ServerError);
    run.wait_line("check failed (3 of 4");
    let creates = rig.mock.request_count(CREATE);
    // The entry fills half and rests, so the broker polls for its fill five times; each poll is
    // then held, so the confirmation lasts past the fourth check.
    rig.mock.cap_next_fill(0.002);
    feed.minute(rig.bar(1));
    rig.wait("the entry", |r| r.mock.request_count(CREATE) > creates);
    rig.mock
        .inject_always(ORDERS, Fault::Hang(Duration::from_millis(400)));
    run.wait_line("check failed (4 of 4");
    let protected = |r: &Rig| r.mock.orders().iter().any(|o| o.trigger == Some(STOP));
    assert!(
        !protected(&rig),
        "the stop was placed before the fourth check, so this run did not test the case:\n{}",
        run.text()
    );
    rig.wait("the protective stop", protected);
    rig.wait("halted status", |r| {
        matches!(r.saved_status("confirm").state, State::Halted { .. })
    });
    rig.mock.clear_faults();
    let (outcome, text) = run.stop();
    assert!(
        matches!(&outcome, Outcome::Halted(reason) if reason.contains("could not be read")),
        "{outcome:?}\n{text}"
    );
    let stop = rig
        .mock
        .orders()
        .into_iter()
        .find(|o| o.trigger == Some(STOP))
        .unwrap();
    assert!(
        stop.reduce_only && (stop.qty - 0.002).abs() < 1e-9,
        "{stop:?}"
    );
}

#[test]
fn a_fire_flattens_in_full_when_the_stop_file_cannot_be_saved() {
    let rig = Rig::new();
    let feed = rig.feed();
    let run = rig.entered("nodisk", wide(), &feed, quick());
    run.wait_line(&format!("bar fed: {}", rig.bar(0)));
    // A folder in the way of the stop file's temporary file: every save fails.
    fs::create_dir(rig.path("nodisk", "stops.json.tmp")).expect("folder");
    rig.lose(11_000.0);
    let (outcome, text) = run.finish();
    let Outcome::Fired {
        fired,
        left: None,
        unsaved: Some(unsaved),
    } = &outcome
    else {
        panic!("{outcome:?}\n{text}");
    };
    assert_eq!(fired.rule, StopRule::MaxLoss);
    assert!(
        unsaved.contains("stops.json could not be saved")
            && unsaved.contains("does not record the fired stop"),
        "{unsaved}"
    );
    assert_eq!(outcome.exit_code(), 3);
    assert!(text.contains("the flatten goes ahead"), "{text}");
    assert_eq!(rig.flatten_orders().len(), 1, "exactly one close: {text}");
    assert_eq!(rig.mock.position(SYMBOL), 0.0, "flat at the venue");
    assert_eq!(rig.resting(), 0, "no order rests");
    // The status was still written; it says what the stop file is missing.
    let State::Ended { reason } = rig.saved_status("nodisk").state else {
        panic!("{:?}", rig.saved_status("nodisk").state);
    };
    assert!(reason.contains("could not be saved"), "{reason}");
    assert!(matches!(rig.open("nodisk"), Err(LiveError::Ended(_))));
}

#[test]
fn a_new_day_s_baseline_that_cannot_be_saved_is_reported_once_and_saved_on_a_later_check() {
    let rig = Rig::new();
    drop(rig.create("day", config(-1, wide())).expect("creates"));
    // Two days since the last check, so the first check starts a new day.
    let mut stops = rig.json("day", "stops.json");
    for field in ["last_check", "day_ends"] {
        stops["stops"][field] = (stops["stops"][field].as_i64().unwrap() - 2 * DAY).into();
    }
    rig.write_json("day", "stops.json", &stops);
    fs::create_dir(rig.path("day", "stops.json.tmp")).expect("folder");

    let feed = rig.feed();
    let run = rig.guard(rig.open("day").expect("opens"), &feed, quick());
    run.wait_line("the new day's starting equity could not be saved");
    let wallet = rig.mock.request_count(WALLET);
    rig.wait("three more checks", |r| {
        r.mock.request_count(WALLET) >= wallet + 3
    });
    fs::remove_dir(rig.path("day", "stops.json.tmp")).unwrap();
    run.wait_line("saved after an earlier failed save");
    let (outcome, text) = run.stop();
    assert_eq!(outcome, Outcome::Stopped, "{text}");
    assert_eq!(text.matches("could not be saved").count(), 1, "{text}");
    let saved = rig.json("day", "stops.json");
    assert!(
        saved["stops"]["day_ends"].as_i64().unwrap() > rig.mock.now(),
        "{saved}"
    );
}

#[test]
fn the_loss_limits_fire_and_flatten_while_the_ticker_cannot_be_read() {
    let rig = Rig::new();
    let feed = rig.feed();
    let run = rig.entered("ticker", wide(), &feed, quick());
    run.wait_line(&format!("bar fed: {}", rig.bar(0)));
    rig.mock.inject_always(TICKERS, Fault::ServerError);
    rig.lose(11_000.0);
    let (outcome, text) = run.finish();
    assert!(
        matches!(&outcome, Outcome::Fired { fired, left: None, unsaved: None } if fired.rule == StopRule::MaxLoss),
        "{outcome:?}\n{text}"
    );
    assert_ended_flat(&rig, "ticker", StopRule::MaxLoss);
}

#[test]
fn the_cap_fires_on_the_position_s_mark_while_the_ticker_cannot_be_read() {
    let rig = Rig::new();
    let feed = rig.feed();
    let run = rig.entered("blind-drift", rules(0.05, 0.1, 300.0), &feed, quick());
    rig.mock.inject_always(TICKERS, Fault::ServerError);
    // 0.004 at 80 000 is 320 of open notional, read from the position's own mark.
    rig.mock.set_price(SYMBOL, 80_000.0);
    let (outcome, text) = run.finish();
    assert!(
        matches!(&outcome, Outcome::Fired { fired, left: None, unsaved: None } if fired.rule == StopRule::OpenNotional),
        "{outcome:?}\n{text}"
    );
    assert_ended_flat(&rig, "blind-drift", StopRule::OpenNotional);
}

#[test]
fn the_loss_limits_fire_while_the_open_notional_cannot_be_valued() {
    let rig = Rig::new();
    let feed = rig.feed();
    let run = rig.entered("unvalued", wide(), &feed, quick());
    run.wait_line(&format!("bar fed: {}", rig.bar(0)));
    rig.mock.inject_always(TICKERS, Fault::ServerError);
    rig.mock.inject_always(POSITIONS, Fault::ServerError);
    rig.lose(11_000.0);
    // "stop fired:" and not "stop fired", which the earlier "no stop fired" line holds too.
    run.wait_line("stop fired: ");
    // The flatten reads the position, so the reads come back once the stop has fired.
    rig.mock.clear_faults();
    let (outcome, text) = run.finish();
    assert!(
        matches!(&outcome, Outcome::Fired { fired, left: None, unsaved: None } if fired.rule == StopRule::MaxLoss),
        "{outcome:?}\n{text}"
    );
    assert!(
        text.contains("Open notional could not be valued."),
        "{text}"
    );
    assert_ended_flat(&rig, "unvalued", StopRule::MaxLoss);
}

#[test]
fn a_stale_temporary_stop_file_is_replaced_when_the_fired_stop_is_saved() {
    let rig = Rig::new();
    let feed = rig.feed();
    let run = rig.entered("stale", wide(), &feed, quick());
    run.wait_line(&format!("bar fed: {}", rig.bar(0)));
    fs::write(rig.path("stale", "stops.json.tmp"), "left by a crash").unwrap();
    rig.lose(11_000.0);
    let (outcome, text) = run.finish();
    assert!(
        matches!(
            &outcome,
            Outcome::Fired {
                left: None,
                unsaved: None,
                ..
            }
        ),
        "{outcome:?}\n{text}"
    );
    assert_ended_flat(&rig, "stale", StopRule::MaxLoss);
    assert!(!rig.path("stale", "stops.json.tmp").exists());
}

/// Makes `link` point at `target`: a file symlink, or where that is not allowed, a symlink or
/// junction to `target`'s folder, which is a reparse point too. Returns why none could be made.
fn link_to(target: &Path, link: &Path) -> Result<(), String> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link).map_err(|e| e.to_string());
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_file(target, link).is_ok() {
            return Ok(());
        }
        let folder = target.parent().expect("a folder");
        if std::os::windows::fs::symlink_dir(folder, link).is_ok() {
            return Ok(());
        }
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(folder)
            .output()
            .map_err(|e| e.to_string())?;
        if made.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&made.stderr).into_owned())
        }
    }
}

#[test]
fn a_link_in_place_of_the_stop_file_is_refused() {
    let rig = Rig::new();
    drop(rig.create("linked", config(-1, wide())).expect("creates"));
    let stops = rig.path("linked", "stops.json");
    let elsewhere = rig.data().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::rename(&stops, elsewhere.join("stops.json")).unwrap();
    if let Err(e) = link_to(&elsewhere.join("stops.json"), &stops) {
        println!("skipped: no link could be made here: {e}");
        return;
    }
    assert!(matches!(rig.open("linked"), Err(LiveError::Link(_))));
    let (code, text) = td_live(&rig, &rig.feed(), &["flatten", "--id", "linked"]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("link or reparse point"), "{text}");
}

#[test]
fn start_is_refused_while_another_session_on_the_data_folder_has_not_ended() {
    let rig = Rig::new();
    drop(rig.create("first", config(-1, wide())).expect("creates"));
    let requests = rig.mock.requests().len();
    let env = rig.env(&rig.feed(), quick());
    let stop = env.stop.clone();
    let data = rig.data().to_path_buf();
    let start = std::thread::spawn(move || {
        let mut lines = Vec::new();
        let code = execute(start_cli("second", &data, NO_TRADES), &env, &mut |l| {
            lines.push(l.to_string())
        });
        (code, lines.join("\n"))
    });
    // A start that is not refused runs until it is stopped.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !start.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let refused = start.is_finished();
    stop.stop();
    let (code, text) = start.join().expect("the start");
    assert!(refused, "the second start was not refused:\n{text}");
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("session first") && text.contains("td-live flatten --id first"),
        "{text}"
    );
    assert!(!live_dir(rig.data(), "second").exists());
    assert_eq!(rig.mock.requests().len(), requests, "the venue is not read");
}

#[test]
fn flatten_ends_a_session_whose_stop_file_is_damaged_or_missing() {
    let rig = Rig::new();
    type Damage = fn(&Path);
    let cases: [(&str, Damage); 2] = [
        ("damaged", |path| fs::write(path, "{").unwrap()),
        ("missing", |path| fs::remove_file(path).unwrap()),
    ];
    for (id, damage) in cases {
        let feed = rig.feed();
        let (outcome, _) = rig.entered(id, wide(), &feed, bars_only()).stop();
        assert_eq!(outcome, Outcome::Stopped, "{id}");
        damage(&rig.path(id, "stops.json"));
        let (code, text) = td_live(&rig, &feed, &["flatten", "--id", id]);
        assert_eq!(code, 0, "{id}: {text}");
        assert!(
            text.contains("the stop file could not be read"),
            "{id}: {text}"
        );
        assert_eq!(rig.mock.position(SYMBOL), 0.0, "{id}");
        assert_eq!(rig.resting(), 0, "{id}");
        assert!(
            matches!(rig.saved_status(id).state, State::Ended { .. }),
            "{id}"
        );
        assert!(
            steps(&rig, id)
                .iter()
                .any(|s| s.contains("could not be read")),
            "{id}"
        );
        assert!(matches!(rig.open(id), Err(LiveError::Ended(_))), "{id}");
    }
}

#[test]
fn a_flatten_leaves_an_order_of_another_session_alone() {
    let rig = Rig::new();
    let feed = rig.feed();
    let (_, _) = rig.entered("mine", wide(), &feed, bars_only()).stop();
    // A reduce-only stop far below the market under another session's prefix.
    let mut theirs = market(Side::Sell, 0.002, true);
    theirs.order_type = OrderType::Stop { trigger: 50_000.0 };
    rig.client
        .place(
            &instrument(),
            &theirs,
            &ClientOrderId::new("othersession-o1".to_string()).unwrap(),
            PRICE,
        )
        .expect("another session's order");
    let (code, text) = td_live(&rig, &feed, &["flatten", "--id", "mine"]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(rig.mock.position(SYMBOL), 0.0, "{text}");
    let orders = rig.mock.orders();
    let other = orders
        .iter()
        .find(|o| o.link_id == "othersession-o1")
        .unwrap();
    assert_eq!(other.status, "Untriggered", "{orders:?}");
    let own = orders.iter().find(|o| o.trigger == Some(STOP)).unwrap();
    assert_eq!(own.status, "Deactivated", "{orders:?}");
}

#[test]
fn flatten_ends_a_faulted_session() {
    let rig = Rig::new();
    let feed = rig.feed();
    let (_, _) = rig.entered("faulty", wide(), &feed, bars_only()).stop();
    fs::write(
        rig.path("faulty", "faulted"),
        "a broker fault the test wrote",
    )
    .unwrap();
    assert!(matches!(
        rig.open("faulty"),
        Err(LiveError::StillFaulted(_))
    ));
    let (code, text) = td_live(&rig, &feed, &["flatten", "--id", "faulty"]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(rig.mock.position(SYMBOL), 0.0, "{text}");
    assert_eq!(rig.resting(), 0, "{text}");
    assert!(matches!(
        rig.saved_status("faulty").state,
        State::Ended { .. }
    ));
}
