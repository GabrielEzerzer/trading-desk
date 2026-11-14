//! A `LiveSession` against the mock venue on 127.0.0.1, fed closed minutes by a scripted stream on
//! 127.0.0.1 and a fake REST source. The strategy is a test double defined here.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::SinkExt;
use td_core::{
    AccountCurrency, Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis,
    OrderRequest, OrderType, Side, TimeInForce, Timeframe, Venue,
};
use td_data::live::{Greeting, KlineUpdate, Rest, SharedStream};
use td_data::{DataError, DataSource, Http, InstrumentList, LiveStream, MinuteHistory};
use td_engine::{
    BacktestConfig, Context, EngineError, ParamValue, Params, Session, SessionConfig, Strategy,
    run_strategy,
};
use td_live::{
    LiveConfig, LiveError, LiveRun, LiveSession, LiveStatus, State, VenueHandle, live_dir,
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
const INSTRUMENTS: &str = "/v5/market/instruments-info";
/// Warm-up bars the fake REST source has before the first live bar.
const WARM_UP: i64 = 3;

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
        operation_budget: Duration::from_secs(5),
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

/// The test double. On its live bar `buy_on` (warm-up bars do not count, the first live bar is 0)
/// it buys [`QTY`], with an attached stop at `stop` when that is above zero. On live bar
/// `close_on` it cancels its open orders and sells its position. On live bar `panic_on` it
/// panics, and on `fail_on` it fails the bar ([`Context::fail`]), each after anything else it does
/// on that bar. `eager` instead submits a buy on every bar, warm-up bars included, while it is
/// flat with no open orders.
struct Double {
    buy_on: i64,
    close_on: i64,
    panic_on: i64,
    fail_on: i64,
    stop: f64,
    eager: bool,
    bar: i64,
}

const PANIC: &str = "the double panicked on purpose";
const FAILED: &str = "the double failed the bar on purpose";

impl Strategy for Double {
    fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
        if self.eager {
            if ctx.position() == 0.0 && ctx.open_orders().is_empty() {
                let _ = ctx.submit(market(Side::Buy, QTY, false));
            }
            return;
        }
        if ctx.is_warming_up() {
            return;
        }
        if self.bar == self.buy_on {
            if self.stop > 0.0 {
                ctx.submit_with_stop(market(Side::Buy, QTY, false), self.stop)
                    .expect("buy with stop");
            } else {
                ctx.submit(market(Side::Buy, QTY, false)).expect("buy");
            }
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
        if self.bar == self.panic_on {
            panic!("{PANIC}");
        }
        if self.bar == self.fail_on {
            ctx.fail(FAILED);
        }
        self.bar += 1;
    }
}

fn build(name: &str, params: &Params) -> Result<Box<dyn Strategy>, EngineError> {
    if name != "double" {
        return td_engine::build_strategy(name, params);
    }
    Ok(Box::new(Double {
        buy_on: params.int("buy_on")?,
        close_on: params.int("close_on")?,
        panic_on: params.int("panic_on")?,
        fail_on: params.int("fail_on")?,
        stop: params.float("stop")?,
        eager: params.bool("eager")?,
        bar: 0,
    }))
}

/// Buys on live bar `buy_on` (with a stop at [`STOP`] if `stop`) and closes on `close_on`.
fn config(buy_on: i64, close_on: i64, stop: bool) -> LiveConfig {
    let params = Params::new()
        .with("buy_on", ParamValue::Int(buy_on))
        .with("close_on", ParamValue::Int(close_on))
        .with("panic_on", ParamValue::Int(-1))
        .with("fail_on", ParamValue::Int(-1))
        .with("stop", ParamValue::Float(if stop { STOP } else { 0.0 }))
        .with("eager", ParamValue::Bool(false));
    LiveConfig {
        strategy: "double".into(),
        params,
        instrument: instrument(),
        timeframe: Timeframe::H1,
        slippage_bps: 0.0,
        fees: None,
        max_leverage: 1.0,
    }
}

fn eager_config() -> LiveConfig {
    let mut config = config(-1, -1, false);
    config.params.set("eager", ParamValue::Bool(true));
    config
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

/// Minutes from `start` on, [`WARM_UP`] hourly bars before it, the funding `rates`, and a clock
/// fixed at `start`. Funding reads after the first `funding_works_for` fail.
struct FakeRest {
    start: Millis,
    warm_up_failures: Mutex<u32>,
    warm_up_reads: AtomicU32,
    rates: Mutex<Vec<(Millis, f64)>>,
    funding_reads: AtomicU32,
    funding_works_for: AtomicU32,
}

impl Rest for FakeRest {
    fn klines(
        &self,
        _: &InstrumentId,
        timeframe: Timeframe,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<Candle>, DataError> {
        let (first, step) = if timeframe == Timeframe::M1 {
            (self.start, MIN)
        } else {
            self.warm_up_reads.fetch_add(1, Ordering::SeqCst);
            let mut failures = self.warm_up_failures.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(DataError::Api {
                    url: "fake".into(),
                    message: "unavailable".into(),
                });
            }
            (self.start - WARM_UP * HOUR, HOUR)
        };
        let to = if timeframe == Timeframe::M1 {
            to
        } else {
            to.min(self.start)
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
        from: Millis,
        to: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        let read = self.funding_reads.fetch_add(1, Ordering::SeqCst);
        if read >= self.funding_works_for.load(Ordering::SeqCst) {
            return Err(DataError::Api {
                url: "fake".into(),
                message: "unavailable".into(),
            });
        }
        let rates = self.rates.lock().unwrap();
        Ok(rates
            .iter()
            .copied()
            .filter(|&(t, _)| from <= t && t < to)
            .collect())
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
            // Later connections are held open and silent.
            tokio::spawn(async move {
                let mut held = Vec::new();
                while let Ok((tcp, _)) = listener.accept().await {
                    if let Ok(socket) = tokio_tungstenite::accept_async(tcp).await {
                        held.push(socket);
                    }
                }
            });
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

    /// Sends the closed minute opening at `t`: the feed backfills the minutes before it from the
    /// fake REST source, which completes every bar that ends by `t`.
    fn minute(&self, t: Millis) {
        let frame = format!(
            r#"{{"topic":"kline.1.{SYMBOL}","data":[{{"start":{t},"end":{},"interval":"1","open":"{PRICE}","close":"{PRICE}","high":"{PRICE}","low":"{PRICE}","volume":"1","turnover":"{PRICE}","confirm":true,"timestamp":{t}}}],"ts":{t},"type":"snapshot"}}"#,
            t + MIN - 1
        );
        self.text(frame);
    }

    /// Sends one text frame as it is.
    fn text(&self, frame: String) {
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

struct Rig {
    mock: MockVenue,
    client: Arc<Client>,
    dir: tempfile::TempDir,
    rest: Arc<FakeRest>,
    runtime: Runtime,
    /// Open time of the first live bar: the last closed hour, so orders decided on it are fresh.
    start: Millis,
}

impl Rig {
    fn new() -> Rig {
        let mock = MockVenue::start();
        mock.add_instrument(&instrument(), PRICE);
        let start = Timeframe::H1.floor(mock.now()) - HOUR;
        Rig::at(mock, start)
    }

    fn at(mock: MockVenue, start: Millis) -> Rig {
        let client = client_for(&mock);
        Rig {
            mock,
            client,
            dir: tempfile::tempdir().expect("temp dir"),
            rest: Arc::new(FakeRest {
                start,
                warm_up_failures: Mutex::new(0),
                warm_up_reads: AtomicU32::new(0),
                rates: Mutex::new(Vec::new()),
                funding_reads: AtomicU32::new(0),
                funding_works_for: AtomicU32::new(u32::MAX),
            }),
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

    fn run(&self, live: LiveSession, feed: &Feed) -> LiveRun {
        live.run(self.rest.clone(), feed.opener(), |_, _| None)
    }

    fn feed(&self) -> Feed {
        Feed::new(&self.runtime)
    }

    /// The open time of live bar `i`.
    fn bar(&self, i: i64) -> Millis {
        self.start + i * HOUR
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

    fn log(&self, id: &str) -> Vec<serde_json::Value> {
        fs::read_to_string(self.path(id, "events.jsonl"))
            .expect("log")
            .lines()
            .map(|l| serde_json::from_str(l).expect("line"))
            .collect()
    }

    fn saved_status(&self, id: &str) -> LiveStatus {
        serde_json::from_str(&fs::read_to_string(self.path(id, "status.json")).expect("status"))
            .expect("status reads")
    }
}

/// Waits until the run's status passes `done`, or fails after 20 s.
fn wait_until(run: &LiveRun, what: &str, done: impl Fn(&LiveStatus) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = run.status();
        if done(&status) {
            return;
        }
        assert!(
            !run.is_finished(),
            "the run ended before {what}: {status:?}"
        );
        assert!(
            Instant::now() < deadline,
            "no {what} within 20 s: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_bar(run: &LiveRun, open_time: Millis) {
    wait_until(run, &format!("bar at {open_time}"), |s| {
        s.last_bar.is_some_and(|t| t >= open_time)
    });
}

#[test]
fn a_round_trip_trades_at_the_venue_and_its_log_status_and_fills_match_the_venue() {
    let rig = Rig::new();
    let live = rig.create("trip", config(0, 2, true)).expect("creates");
    assert_eq!(
        live.config().starting_balance,
        100_000.0,
        "the venue's equity"
    );
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(live, &feed);

    // Bar 0 bought with a stop; by the end of bar 1 the stop rests at the venue.
    wait_for_bar(&run, rig.bar(1));
    assert_eq!(rig.mock.position(SYMBOL), QTY);
    let resting: Vec<_> = rig
        .mock
        .orders()
        .into_iter()
        .filter(|o| o.status == "Untriggered")
        .collect();
    assert_eq!(resting.len(), 1, "{resting:?}");
    assert_eq!(resting[0].trigger, Some(STOP));
    assert!(resting[0].reduce_only);
    assert_eq!(run.handle().resting_orders().len(), 1);
    let status = run.status();
    assert_eq!(status.state, State::Running);
    assert_eq!(status.position, QTY);
    assert_eq!(status.venue.as_ref().map(|v| v.position), Some(QTY));

    // Bar 2 cancels the stop and sells; bar 3 reports the sale.
    feed.minute(rig.bar(4));
    wait_for_bar(&run, rig.bar(3));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");

    assert_eq!(rig.mock.position(SYMBOL), 0.0);
    assert!(rig.mock.orders().iter().all(|o| o.status != "Untriggered"));
    let executions = rig.mock.executions();
    let fills = live.session().fills();
    assert_eq!(fills.len(), 2);
    assert_eq!(executions.len(), 2);
    for (fill, execution) in fills.iter().zip(&executions) {
        assert_eq!(fill.qty, execution.qty);
        assert_eq!(fill.price, execution.price);
        assert_eq!(fill.fee, execution.fee);
    }
    assert_eq!(fills[0].side, Side::Buy);
    assert_eq!(fills[1].side, Side::Sell);

    let status = rig.saved_status("trip");
    assert_eq!(&status, live.status());
    assert!(
        matches!(&status.state, State::Stopped { note } if note.contains("stopped by the owner"))
    );
    assert_eq!(status.last_bar, Some(rig.bar(3)));
    assert_eq!(status.position, 0.0);
    assert!(status.open_orders.is_empty());
    assert_eq!(status.venue.as_ref().map(|v| v.position), Some(0.0));

    // Three warm-up lines, then bars 0 to 3, bars 0 and 2 each with a sending line first.
    let log = rig.log("trip");
    let count = |key: &str| log.iter().filter(|l| l.get(key).is_some()).count();
    assert_eq!(log.len(), 3 + 4 + 2);
    assert_eq!(count("warm_up"), 3);
    assert_eq!(count("sending"), 2);
    assert_eq!(count("record"), 6);

    // Reopening replays the log to the same state, without a request to the venue.
    let (orders, fills) = (live.session().orders().to_vec(), fills.to_vec());
    drop(live);
    let requests = rig.mock.requests().len();
    let reopened = rig.open("trip").expect("reopens");
    assert_eq!(reopened.session().orders(), orders);
    assert_eq!(reopened.session().fills(), fills);
    assert_eq!(rig.mock.requests().len(), requests);
}

/// Cuts the log's last line, as a process that died before writing it would have left it.
fn drop_last_line(path: &Path) {
    let bytes = fs::read(path).expect("log");
    let body = &bytes[..bytes.len() - 1];
    let keep = body.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    fs::write(path, &bytes[..keep]).expect("cut");
}

#[test]
fn a_restart_with_a_position_open_replays_without_sending_and_ends_as_an_uninterrupted_run() {
    let rig = Rig::new();
    let cfg = config(0, 3, true);
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(rig.create("restart", cfg.clone()).expect("creates"), &feed);
    wait_for_bar(&run, rig.bar(1));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");

    // The stop left the position and its stop at the venue as they were, and says so.
    let State::Stopped { note } = &live.status().state else {
        panic!("not stopped: {:?}", live.status());
    };
    assert!(
        note.contains(&format!("a position of {QTY} and 1 resting orders")),
        "{note}"
    );
    drop(live);
    assert_eq!(rig.mock.position(SYMBOL), QTY);
    assert_eq!(rig.mock.request_count(CANCEL), 0);

    // Reopening replays the log without a single request to the venue.
    let requests = rig.mock.requests().len();
    let live = rig.open("restart").expect("reopens");
    assert_eq!(rig.mock.requests().len(), requests);
    assert_eq!(live.session().portfolio().position(), QTY);

    // Resumed: go_live reconciles, and bars 2 to 4 trade on (bar 3 closes, bar 4 reports it).
    let feed = rig.feed();
    feed.minute(rig.bar(5));
    let run = rig.run(live, &feed);
    wait_for_bar(&run, rig.bar(4));
    let (resumed, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert_eq!(rig.mock.position(SYMBOL), 0.0);

    // The same as a session that ran bars 0 to 4 in one go, on a venue of its own.
    let mock = MockVenue::start();
    mock.add_instrument(&instrument(), PRICE);
    let other = Rig::at(mock, rig.start);
    let feed = other.feed();
    feed.minute(rig.bar(5));
    let run = other.run(other.create("reference", cfg).expect("creates"), &feed);
    wait_for_bar(&run, rig.bar(4));
    let (reference, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert_eq!(reference.session().fills().len(), 2);
    assert_eq!(resumed.session().orders(), reference.session().orders());
    assert_eq!(resumed.session().fills(), reference.session().fills());
    assert_eq!(
        resumed.session().portfolio().position(),
        reference.session().portfolio().position()
    );
    for path in [CREATE, CANCEL, AMEND] {
        assert_eq!(
            rig.mock.request_count(path),
            other.mock.request_count(path),
            "{path}"
        );
    }
}

#[test]
fn an_order_sent_before_its_line_was_written_is_not_sent_twice_on_resume() {
    let rig = Rig::new();
    let feed = rig.feed();
    feed.minute(rig.bar(1));
    let run = rig.run(
        rig.create("crash", config(0, 99, false)).expect("creates"),
        &feed,
    );
    wait_for_bar(&run, rig.bar(0));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    drop(live);
    assert_eq!(rig.mock.request_count(CREATE), 1);

    // The process died after the buy went out and before bar 0's line was written: the log ends
    // with the line written before the send.
    drop_last_line(&rig.path("crash", "events.jsonl"));
    assert_eq!(rig.log("crash").last().expect("a line")["sending"], true);

    let requests = rig.mock.requests().len();
    let live = rig.open("crash").expect("reopens");
    assert_eq!(
        rig.mock.requests().len(),
        requests,
        "replay touches nothing"
    );
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(live, &feed);
    wait_for_bar(&run, rig.bar(1));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");

    assert_eq!(rig.mock.request_count(CREATE), 1, "adopted, not sent again");
    assert_eq!(rig.mock.position(SYMBOL), QTY);
    assert_eq!(live.session().portfolio().position(), QTY);
    assert_eq!(live.session().fills().len(), 1);
    assert_eq!(live.session().fills()[0].qty, rig.mock.executions()[0].qty);
    drop(live);

    // The resume completed bar 0's line, so the log replays whole again.
    let reopened = rig.open("crash").expect("reopens");
    assert_eq!(reopened.session().portfolio().position(), QTY);
    let sending = rig.log("crash");
    assert_eq!(
        sending
            .iter()
            .filter(|l| l.get("sending").is_some())
            .count(),
        1
    );
}

/// The dead process placed the entry's stop as well: the resume adopts the entry and that stop
/// from the venue, sends, cancels and resizes nothing, and trades on with the stop resting.
#[test]
fn an_entry_with_a_stop_sent_before_its_line_was_written_is_adopted_with_its_stop_on_resume() {
    let rig = Rig::new();
    let feed = rig.feed();
    feed.minute(rig.bar(1));
    let run = rig.run(
        rig.create("stopped", config(0, 99, true)).expect("creates"),
        &feed,
    );
    wait_for_bar(&run, rig.bar(0));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    drop(live);
    assert_eq!(rig.mock.request_count(CREATE), 2, "the entry and its stop");
    drop_last_line(&rig.path("stopped", "events.jsonl"));
    assert_eq!(rig.log("stopped").last().expect("a line")["sending"], true);

    let sends = rig.sends();
    let live = rig.open("stopped").expect("reopens");
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(live, &feed);
    wait_for_bar(&run, rig.bar(1));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");

    assert_eq!(rig.sends(), sends, "nothing sent, cancelled or resized");
    assert_eq!(rig.mock.position(SYMBOL), QTY);
    assert_eq!(live.session().portfolio().position(), QTY);
    let resting = live.handle().resting_orders();
    assert_eq!(resting.len(), 1, "{resting:?}");
    assert_eq!(resting[0].request.qty, QTY);
    assert!(resting[0].request.reduce_only);
    let open: Vec<_> = rig
        .mock
        .orders()
        .into_iter()
        .filter(|o| o.status == "Untriggered")
        .collect();
    assert_eq!(open.len(), 1, "{open:?}");
}

#[test]
fn a_venue_position_changed_while_down_halts_the_resume_before_a_bar() {
    let rig = Rig::new();
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(
        rig.create("moved", config(0, 99, false)).expect("creates"),
        &feed,
    );
    wait_for_bar(&run, rig.bar(1));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    drop(live);

    rig.mock.set_position(SYMBOL, 0.01, PRICE);
    let live = rig
        .open("moved")
        .expect("reopens: the replay does not read the venue");
    let sends = rig.sends();
    let feed = rig.feed();
    let (live, ended) = rig.run(live, &feed).wait();
    let Err(LiveError::Faulted(fault)) = ended else {
        panic!("not faulted: {ended:?}");
    };
    assert!(fault.starts_with("position mismatch"), "{fault}");
    assert_eq!(rig.sends(), sends, "nothing sent or cancelled");
    assert_eq!(
        live.status().state,
        State::Faulted {
            fault: fault.clone()
        }
    );
    assert_eq!(live.status().last_bar, Some(rig.bar(1)));
    assert!(!feed.opened.load(Ordering::SeqCst), "no bar was fed");
    drop(live);
    let refused = rig.open("moved").err().expect("refused");
    assert!(
        matches!(&refused, LiveError::StillFaulted(f) if *f == fault),
        "{refused}"
    );
}

#[test]
fn a_fault_during_the_run_halts_it_and_the_session_stays_halted() {
    let rig = Rig::new();
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(
        rig.create("fault", config(0, 99, false)).expect("creates"),
        &feed,
    );
    wait_for_bar(&run, rig.bar(1));

    rig.mock.set_position(SYMBOL, 0.01, PRICE);
    let sends = rig.sends();
    feed.minute(rig.bar(4));
    let (live, ended) = run.wait();
    let Err(error) = &ended else {
        panic!("not faulted: {ended:?}");
    };
    let LiveError::Faulted(fault) = error else {
        panic!("not faulted: {error}");
    };
    assert!(fault.starts_with("position mismatch"), "{fault}");
    assert!(error.to_string().contains("broker fault"), "{error}");
    // Bar 2 found the disagreement; bar 3 was not fed.
    assert_eq!(live.status().last_bar, Some(rig.bar(2)));
    assert_eq!(rig.sends(), sends, "nothing sent or cancelled");
    assert_eq!(
        rig.saved_status("fault").state,
        State::Faulted {
            fault: fault.clone()
        }
    );
    drop(live);

    let refused = rig.open("fault").err().expect("refused");
    assert!(matches!(&refused, LiveError::StillFaulted(f) if f == fault));
    assert!(refused.to_string().contains("stays halted"), "{refused}");
}

#[test]
fn a_fresh_start_is_refused_before_anything_is_written_unless_the_account_is_flat_and_readable() {
    let rig = Rig::new();
    rig.mock.set_position(SYMBOL, 0.01, PRICE);
    let refused = rig
        .create("busy", config(0, 99, false))
        .err()
        .expect("refused");
    assert!(matches!(refused, LiveError::NotFlat { .. }), "{refused}");
    assert!(
        refused
            .to_string()
            .contains("a fresh session must start flat")
    );
    assert!(!live_dir(rig.data(), "busy").exists());

    rig.mock.set_position(SYMBOL, 0.0, 0.0);
    rig.mock.inject_always(WALLET, Fault::ServerError);
    let refused = rig
        .create("busy", config(0, 99, false))
        .err()
        .expect("refused");
    assert!(
        matches!(refused, LiveError::VenueUnreadable(_)),
        "{refused}"
    );
    assert!(!live_dir(rig.data(), "busy").exists());
    assert_eq!(rig.sends(), 0);

    rig.mock.clear_faults();
    rig.create("busy", config(0, 99, false))
        .expect("flat and readable");
}

#[test]
fn a_second_run_of_the_same_session_is_refused_by_the_lock() {
    let rig = Rig::new();
    let live = rig.create("once", config(0, 99, false)).expect("creates");
    let locked = rig.open("once").err().expect("refused");
    assert!(matches!(locked, LiveError::Locked(_)), "{locked}");

    let feed = rig.feed();
    let run = rig.run(live, &feed);
    wait_until(&run, "running", |s| s.state == State::Running);
    let locked = rig.open("once").err().expect("refused");
    assert!(locked.to_string().contains("already running"), "{locked}");
    let exists = rig
        .create("once", config(0, 99, false))
        .err()
        .expect("refused");
    assert!(matches!(exists, LiveError::Exists(_)), "{exists}");

    // Dropping the run stops it, leaving what is open at the venue, and lets the session go.
    drop(run);
    let deadline = Instant::now() + Duration::from_secs(20);
    let reopened = loop {
        match rig.open("once") {
            Ok(live) => break live,
            Err(LiveError::Locked(_)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("does not reopen: {e}"),
        }
    };
    assert!(matches!(
        &reopened.status().state,
        State::Stopped { note } if note.contains("stopped by the owner")
    ));
}

#[test]
fn a_warm_up_that_cannot_read_the_venue_halts_after_its_tries_and_sends_nothing() {
    let rig = Rig::new();
    *rig.rest.warm_up_failures.lock().unwrap() = 99;
    let feed = rig.feed();
    let live = rig.create("cold", config(0, 99, false)).expect("creates");
    let (live, ended) = rig.run(live, &feed).wait();
    let Err(LiveError::Halted(reason)) = ended else {
        panic!("not halted: {ended:?}");
    };
    assert!(
        reason.contains("warm-up could not read the venue in 3 tries"),
        "{reason}"
    );
    assert_eq!(rig.rest.warm_up_reads.load(Ordering::SeqCst), 3);
    assert_eq!(live.status().state, State::Halted { reason });
    assert_eq!(live.status().last_bar, None);
    assert!(!feed.opened.load(Ordering::SeqCst), "it did not go live");
    assert_eq!(rig.sends(), 0);
}

#[test]
fn nothing_is_sent_during_warm_up() {
    let rig = Rig::new();
    let feed = rig.feed();
    let run = rig.run(rig.create("eager", eager_config()).expect("creates"), &feed);
    wait_until(&run, "running", |s| s.state == State::Running);
    // The double tried to buy on each warm-up bar.
    assert_eq!(run.session().session().bars().len(), WARM_UP as usize);
    assert_eq!(rig.sends(), 0);

    feed.minute(rig.bar(1));
    wait_for_bar(&run, rig.bar(0));
    let (_live, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert_eq!(
        rig.mock.request_count(CREATE),
        1,
        "the first live bar trades"
    );
}

/// Runs a session that buys on its first live bar and stops it after that bar, its position open.
fn traded(rig: &Rig, id: &str) {
    let feed = rig.feed();
    feed.minute(rig.bar(1));
    let run = rig.run(
        rig.create(id, config(0, 99, false)).expect("creates"),
        &feed,
    );
    wait_for_bar(&run, rig.bar(0));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    drop(live);
    assert_eq!(rig.mock.position(SYMBOL), QTY);
}

#[test]
fn a_resume_halts_before_sending_when_the_venue_changed_the_instruments_quantity_step() {
    let rig = Rig::new();
    traded(&rig, "rules");
    // The saved rules no longer match the venue's: its quantity step is 0.001.
    let path = rig.path("rules", "config.json");
    let mut saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    saved["instrument"]["lot_size"] = 0.002.into();
    fs::write(&path, serde_json::to_vec(&saved).expect("json")).expect("write");

    let requests = rig.mock.requests().len();
    let live = rig.open("rules").expect("reopens");
    assert_eq!(rig.mock.requests().len(), requests, "open asks nothing");
    let sends = rig.sends();
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(live, &feed);
    wait_until(&run, "a halt", |s| matches!(s.state, State::Halted { .. }));
    let (live, ended) = run.wait();
    let Err(LiveError::Halted(reason)) = ended else {
        panic!("not halted: {ended:?}");
    };
    assert!(
        reason.contains("quantity step saved as 0.002, now 0.001 at the venue"),
        "{reason}"
    );
    assert!(!reason.contains("tick size"), "{reason}");
    // The session had traded, so the reason says what is left at the venue.
    assert!(
        reason.contains("position and resting orders stay at the venue as they are"),
        "{reason}"
    );
    assert_eq!(live.status().state, State::Halted { reason });
    assert_eq!(rig.sends(), sends, "nothing sent, cancelled or resized");
    assert_eq!(rig.mock.request_count(CREATE), 1);
    assert!(!feed.opened.load(Ordering::SeqCst), "no bar was fed");
    assert_eq!(live.status().last_bar, Some(rig.bar(0)));
    assert_eq!(rig.mock.position(SYMBOL), QTY);
}

#[test]
fn a_resume_whose_instrument_rules_cannot_be_read_halts_before_sending() {
    let rig = Rig::new();
    traded(&rig, "unread");
    rig.mock.inject_always(INSTRUMENTS, Fault::ServerError);
    let live = rig.open("unread").expect("reopens");
    let sends = rig.sends();
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(live, &feed);
    wait_until(&run, "a halt", |s| matches!(s.state, State::Halted { .. }));
    let (live, ended) = run.wait();
    let Err(LiveError::Halted(reason)) = ended else {
        panic!("not halted: {ended:?}");
    };
    assert!(
        reason.contains("rules for BTCUSDT could not be read"),
        "{reason}"
    );
    assert_eq!(live.status().state, State::Halted { reason });
    assert_eq!(rig.sends(), sends);
    assert!(!feed.opened.load(Ordering::SeqCst), "no bar was fed");
}

#[test]
fn a_resume_whose_instrument_rules_are_unchanged_reads_them_once_and_trades_on() {
    let rig = Rig::new();
    traded(&rig, "same");
    let reads = rig.mock.request_count(INSTRUMENTS);
    let live = rig.open("same").expect("reopens");
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(live, &feed);
    wait_for_bar(&run, rig.bar(1));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert_eq!(rig.mock.request_count(INSTRUMENTS), reads + 1);
    assert!(feed.opened.load(Ordering::SeqCst), "it went live");
    assert_eq!(live.session().portfolio().position(), QTY);
    assert_eq!(rig.mock.position(SYMBOL), QTY);
}

#[test]
fn a_session_saved_at_another_engine_revision_is_refused() {
    let rig = Rig::new();
    drop(rig.create("old", config(0, 99, false)).expect("creates"));
    let path = rig.path("old", "config.json");
    let mut saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    saved["engine_revision"] = 1.into();
    fs::write(&path, serde_json::to_vec(&saved).expect("json")).expect("write");

    let requests = rig.mock.requests().len();
    let refused = rig.open("old").err().expect("refused");
    assert!(
        matches!(refused, LiveError::EngineChanged { created: 1, .. }),
        "{refused}"
    );
    assert!(refused.to_string().contains("cannot be resumed"));
    assert_eq!(rig.mock.requests().len(), requests);
}

#[test]
fn the_owner_reads_the_venue_after_each_bar_and_can_halt_the_run() {
    let rig = Rig::new();
    let feed = rig.feed();
    feed.minute(rig.bar(4));
    let reads = Arc::new(Mutex::new(Vec::new()));
    let (seen, last) = (reads.clone(), rig.bar(1));
    let live = rig.create("owner", config(0, 99, true)).expect("creates");
    let run = live.run(
        rig.rest.clone(),
        feed.opener(),
        move |status: &LiveStatus, handle: &VenueHandle| {
            let position = handle.venue_position().expect("position").size;
            let equity = handle.equity().expect("equity").equity;
            let resting = handle.resting_orders().len();
            seen.lock().unwrap().push((position, equity, resting));
            (status.last_bar == Some(last)).then(|| "the owner's limit".to_string())
        },
    );
    let (live, ended) = run.wait();
    assert!(
        matches!(&ended, Err(LiveError::Halted(r)) if r == "the owner's limit"),
        "{ended:?}"
    );
    assert_eq!(
        live.status().last_bar,
        Some(rig.bar(1)),
        "bar 2 was not fed"
    );
    let reads = reads.lock().unwrap().clone();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[1].0, QTY);
    assert!(reads[1].1 > 0.0);
    assert_eq!(reads[1].2, 1, "the stop rests");
    assert_eq!(
        rig.saved_status("owner").state,
        State::Halted {
            reason: "the owner's limit".into()
        }
    );
    drop(live);

    // A halt is not a fault: the session resumes, and a halt asked between bars ends it again.
    let live = rig.open("owner").expect("a halted session resumes");
    let feed = rig.feed();
    let run = rig.run(live, &feed);
    wait_until(&run, "running", |s| s.state == State::Running);
    run.halt("asked between bars");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !run.is_finished() {
        assert!(Instant::now() < deadline, "the run did not halt");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (live, ended) = run.wait();
    assert!(matches!(&ended, Err(LiveError::Halted(r)) if r == "asked between bars"));
    assert_eq!(live.status().last_bar, Some(rig.bar(1)));
    assert_eq!(rig.mock.position(SYMBOL), QTY, "a halt leaves the position");
}

#[test]
fn no_key_or_secret_is_written_to_the_saved_files() {
    let rig = Rig::new();
    rig.mock.set_echo_credentials(true);
    let live = rig.create("keys", config(0, 2, true)).expect("creates");
    // Every equity read now fails with the key and signature in the venue's message.
    rig.mock.inject_always(WALLET, Fault::TimestampReject);
    let feed = rig.feed();
    feed.minute(rig.bar(3));
    let run = rig.run(live, &feed);
    wait_for_bar(&run, rig.bar(2));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    let warnings = &live.status().warnings;
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("the venue could not be read")),
        "{warnings:?}"
    );
    drop(live);
    for file in ["config.json", "events.jsonl", "status.json"] {
        let text = fs::read_to_string(rig.path("keys", file)).expect("file");
        assert!(!text.contains(TEST_KEY), "{file} holds the key");
        assert!(!text.contains(TEST_SECRET), "{file} holds the secret");
    }
}

/// Rewrites the saved status with `state`, as a hand edit would.
fn rewrite_state(rig: &Rig, id: &str, state: State) {
    let mut status = rig.saved_status(id);
    status.state = state;
    let json = serde_json::to_vec_pretty(&status).expect("json");
    fs::write(rig.path(id, "status.json"), json).expect("write");
}

fn stopped(note: &str) -> State {
    State::Stopped { note: note.into() }
}

#[test]
fn a_faulted_session_stays_refused_after_its_status_is_rewritten() {
    let rig = Rig::new();
    let feed = rig.feed();
    feed.minute(rig.bar(1));
    let run = rig.run(
        rig.create("marked", config(0, 99, false)).expect("creates"),
        &feed,
    );
    wait_for_bar(&run, rig.bar(0));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    drop(live);

    rig.mock.set_position(SYMBOL, 0.01, PRICE);
    let live = rig.open("marked").expect("reopens");
    let (live, ended) = rig.run(live, &rig.feed()).wait();
    let Err(LiveError::Faulted(fault)) = ended else {
        panic!("not faulted: {ended:?}");
    };
    let marker = fs::read_to_string(rig.path("marked", "faulted")).expect("the marker");
    assert!(marker.contains("position mismatch"), "{marker}");

    // The session handed back cannot be run again in this process either.
    let sends = rig.sends();
    let feed = rig.feed();
    let (live, again) = rig.run(live, &feed).wait();
    assert!(
        matches!(&again, Err(LiveError::StillFaulted(f)) if *f == fault),
        "{again:?}"
    );
    assert_eq!(rig.sends(), sends);
    assert!(!feed.opened.load(Ordering::SeqCst));
    drop(live);

    // A status edited to say it stopped does not lift the marker.
    rig.mock.set_position(SYMBOL, QTY, PRICE);
    rewrite_state(&rig, "marked", stopped("edited"));
    let refused = rig.open("marked").err().expect("refused");
    assert!(
        matches!(&refused, LiveError::StillFaulted(f) if f.contains("position mismatch")),
        "{refused}"
    );
}

#[test]
fn a_torn_last_line_is_cut_and_the_session_resumes_whole() {
    let rig = Rig::new();
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(
        rig.create("torn", config(0, 99, false)).expect("creates"),
        &feed,
    );
    wait_for_bar(&run, rig.bar(1));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    drop(live);

    // The process died part way through writing bar 2's line.
    let path = rig.path("torn", "events.jsonl");
    let whole = fs::read(&path).expect("log");
    let text = String::from_utf8(whole.clone()).expect("text");
    let next = text
        .lines()
        .last()
        .expect("a line")
        .replace(&rig.bar(1).to_string(), &rig.bar(2).to_string());
    let mut torn = whole.clone();
    torn.extend_from_slice(&next.as_bytes()[..next.len() / 2]);
    fs::write(&path, &torn).expect("tear");

    let live = rig.open("torn").expect("reopens");
    assert_eq!(
        fs::read(&path).expect("log"),
        whole,
        "the torn line was cut"
    );
    assert!(
        live.status()
            .warnings
            .iter()
            .any(|w| w.contains("cut a torn last line")),
        "{:?}",
        live.status().warnings
    );
    assert_eq!(live.status().last_bar, Some(rig.bar(1)));
    assert_eq!(live.session().portfolio().position(), QTY);

    let feed = rig.feed();
    feed.minute(rig.bar(3));
    let run = rig.run(live, &feed);
    wait_for_bar(&run, rig.bar(2));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert_eq!(rig.mock.request_count(CREATE), 1);
    drop(live);
    let reopened = rig.open("torn").expect("the log replays whole");
    assert_eq!(reopened.status().last_bar, Some(rig.bar(2)));
}

/// Runs a session that buys on bar 0 through bar 1 and stops it.
fn two_bars(rig: &Rig, id: &str) {
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(
        rig.create(id, config(0, 99, false)).expect("creates"),
        &feed,
    );
    wait_for_bar(&run, rig.bar(1));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    drop(live);
}

#[test]
fn a_line_that_does_not_read_is_refused_before_a_torn_tail_is_cut() {
    let rig = Rig::new();
    two_bars(&rig, "garbled");
    let path = rig.path("garbled", "events.jsonl");
    let text = fs::read_to_string(&path).expect("log");
    let mut lines: Vec<&str> = text.lines().collect();
    lines[3] = "not a line of the log";
    let edited = format!("{}\n{{\"torn", lines.join("\n"));
    fs::write(&path, &edited).expect("write");

    let requests = rig.mock.requests().len();
    let refused = rig.open("garbled").err().expect("refused");
    assert!(
        matches!(&refused, LiveError::BadLog { reason, .. } if reason.starts_with("line 4")),
        "{refused}"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("log"),
        edited,
        "left as it is"
    );
    assert_eq!(rig.mock.requests().len(), requests);
}

#[test]
fn a_sending_line_followed_by_another_bar_is_refused() {
    let rig = Rig::new();
    two_bars(&rig, "spliced");
    let path = rig.path("spliced", "events.jsonl");
    let text = fs::read_to_string(&path).expect("log");
    let lines: Vec<&str> = text.lines().collect();
    let sending = lines
        .iter()
        .position(|l| l.contains("\"sending\":true"))
        .expect("bar 0's sending line");
    // Bar 0's own line is gone, so bar 1's follows the sending line.
    let spliced: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != sending + 1)
        .map(|(_, l)| *l)
        .collect();
    fs::write(&path, format!("{}\n", spliced.join("\n"))).expect("write");

    let refused = rig.open("spliced").err().expect("refused");
    assert!(
        matches!(&refused, LiveError::BadLog { reason, .. }
            if reason.contains("a sending line is not followed by its bar")),
        "{refused}"
    );
}

#[test]
fn a_strategy_that_panics_after_its_first_send_halts_and_open_refuses_without_unwinding() {
    let rig = Rig::new();
    let mut cfg = config(1, 99, false);
    cfg.params.set("panic_on", ParamValue::Int(1));
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let (live, ended) = rig
        .run(rig.create("panics", cfg).expect("creates"), &feed)
        .wait();
    assert!(
        matches!(&ended, Err(LiveError::Panicked(p)) if p.contains(PANIC)),
        "{ended:?}"
    );
    assert_eq!(rig.mock.request_count(CREATE), 1, "the buy went out first");
    assert!(
        matches!(&live.status().state, State::Panicked { panic } if panic.contains(PANIC)),
        "{:?}",
        live.status().state
    );
    assert_eq!(rig.saved_status("panics").state, live.status().state);

    let feed = rig.feed();
    let (live, again) = rig.run(live, &feed).wait();
    assert!(
        matches!(again, Err(LiveError::StillPanicked(_))),
        "{again:?}"
    );
    assert!(!feed.opened.load(Ordering::SeqCst));
    drop(live);

    let refused = rig.open("panics").err().expect("refused");
    assert!(
        matches!(&refused, LiveError::StillPanicked(p) if p.contains(PANIC)),
        "{refused}"
    );
    assert!(refused.to_string().contains("panicked"), "{refused}");

    // With the status edited, the replay runs into the same panic, which open returns.
    rewrite_state(&rig, "panics", stopped("edited"));
    let refused = rig.open("panics").err().expect("refused");
    assert!(
        matches!(&refused, LiveError::BadLog { reason, .. }
            if reason.contains("panicked") && reason.contains(PANIC)),
        "{refused}"
    );
    assert_eq!(rig.mock.request_count(CREATE), 1);
}

#[test]
fn a_bar_the_strategy_fails_is_logged_halts_and_replays_on_resume() {
    let rig = Rig::new();
    let mut cfg = config(0, 99, false);
    cfg.params.set("fail_on", ParamValue::Int(1));
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let (live, ended) = rig
        .run(rig.create("fails", cfg).expect("creates"), &feed)
        .wait();
    assert!(
        ended
            .as_ref()
            .is_err_and(|e| e.to_string().contains(FAILED)),
        "{ended:?}"
    );
    assert!(
        matches!(&live.status().state, State::Halted { reason } if reason.contains(FAILED)),
        "{:?}",
        live.status().state
    );
    assert_eq!(
        live.status().last_bar,
        Some(rig.bar(1)),
        "bar 1 was fed in full"
    );
    assert_eq!(rig.saved_status("fails").state, live.status().state);
    drop(live);
    let records = rig
        .log("fails")
        .iter()
        .filter(|l| l["record"].is_object() && l["sending"] != true)
        .count();
    assert_eq!(records, 2, "bars 0 and 1 are logged");

    let live = rig.open("fails").expect("a halt resumes");
    assert_eq!(live.status().last_bar, Some(rig.bar(1)));
    assert_eq!(live.session().portfolio().position(), QTY);
    let feed = rig.feed();
    feed.minute(rig.bar(3));
    let run = rig.run(live, &feed);
    wait_for_bar(&run, rig.bar(2));
    let (_live, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert_eq!(rig.mock.request_count(CREATE), 1);
}

/// Funding every 8 hours, the first live one due at the open of live bar 1.
fn eight_hourly(rig: &Rig) {
    let rates = (0..9)
        .map(|k| (rig.bar(1) - k * 8 * HOUR, 0.0001))
        .collect();
    *rig.rest.rates.lock().unwrap() = rates;
}

#[test]
fn a_funding_rate_is_charged_once_across_a_restart() {
    let rig = Rig::new();
    eight_hourly(&rig);
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(
        rig.create("funded", config(0, 99, false)).expect("creates"),
        &feed,
    );
    wait_for_bar(&run, rig.bar(1));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert_eq!(live.funding().len(), 1);
    assert_eq!(live.funding()[0].due, rig.bar(1));
    drop(live);

    let live = rig.open("funded").expect("reopens");
    assert_eq!(live.funding().len(), 1, "replayed from the log");
    let feed = rig.feed();
    feed.minute(rig.bar(4));
    let run = rig.run(live, &feed);
    wait_for_bar(&run, rig.bar(3));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert_eq!(live.funding().len(), 1, "not charged again");
    let charged: usize = rig
        .log("funded")
        .iter()
        .map(|l| l["funding"].as_array().map_or(0, Vec::len))
        .sum();
    assert_eq!(charged, 1);
}

/// What a strategy saw on each bar it was called on: the bar's open time, whether it was a warm-up
/// bar, and [`Context::funding`].
type Seen = Vec<(Millis, bool, Vec<(Millis, f64)>)>;

/// Never trades; records what [`Context::funding`] gives it on each bar.
struct SeesFunding(Arc<Mutex<Seen>>);

impl Strategy for SeesFunding {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let seen = (bar.open_time, ctx.is_warming_up(), ctx.funding().to_vec());
        self.0.lock().expect("lock").push(seen);
    }
}

/// What a strategy saw in the live session `id`: each bar of its log fed to a session running
/// [`SeesFunding`] as the live session fed it and as a reopen replays it, warm-up bars as warm-up
/// and each rate at its `time`.
fn seen_live(rig: &Rig, id: &str, cfg: &LiveConfig) -> Seen {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let money = SessionConfig {
        starting_cash: 100_000.0,
        slippage_bps: cfg.slippage_bps,
        fees: cfg.fees,
        max_leverage: cfg.max_leverage,
        account_currency: AccountCurrency::Quote,
    };
    let strategy = Box::new(SeesFunding(seen.clone()));
    let mut session =
        Session::new(strategy, cfg.instrument.clone(), cfg.timeframe, money).expect("session");
    for line in rig.log(id) {
        // A `sending` line's bar is fed by the line after it.
        if line["sending"] == true {
            continue;
        }
        let bar: Candle = serde_json::from_value(line["bar"].clone()).expect("a bar");
        let funding: Vec<(Millis, f64)> = line["funding"].as_array().map_or(Vec::new(), |rates| {
            rates
                .iter()
                .map(|f| {
                    (
                        f["time"].as_i64().expect("time"),
                        f["rate"].as_f64().expect("rate"),
                    )
                })
                .collect()
        });
        let fed = if line["warm_up"] == true {
            session.warm_up(bar, &funding)
        } else {
            session.on_bar(bar, &funding)
        };
        fed.expect("feeds");
    }
    seen.lock().expect("lock").clone()
}

/// What a strategy sees in a backtest of `bars` given the venue's `funding`, warmed up on the bars
/// before `live_from` and trading from it.
fn seen_in_backtest(
    cfg: &LiveConfig,
    bars: &[Candle],
    live_from: Millis,
    funding: &[(Millis, f64)],
) -> Seen {
    let config = BacktestConfig {
        strategy: "sees_funding".into(),
        params: Params::new(),
        instrument: cfg.instrument.clone(),
        timeframe: cfg.timeframe,
        from: live_from,
        to: bars.last().expect("bars").open_time + cfg.timeframe.duration_ms(),
        starting_cash: 100_000.0,
        slippage_bps: cfg.slippage_bps,
        fee_override: cfg.fees,
        max_leverage: cfg.max_leverage,
        warmup_from: Some(bars[0].open_time),
        close_at_end: false,
        account_currency: AccountCurrency::Quote,
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let strategy = Box::new(SeesFunding(seen.clone()));
    run_strategy(&config, strategy, bars, &[], funding).expect("runs");
    seen.lock().expect("lock").clone()
}

/// A live session shows its strategy, on every bar, warm-up bars included, the funding a backtest
/// of the same bars shows it, times and rates.
#[test]
fn a_live_session_shows_its_strategy_the_funding_a_backtest_of_its_bars_shows() {
    let rig = Rig::new();
    // Before the first warm-up bar, so never seen; at its open; inside a warm-up bar; at the
    // first live bar's open; at the second's and the third's.
    let funding = vec![
        (rig.bar(-4), 0.0005),
        (rig.bar(-3), 0.0001),
        (rig.bar(-2) + 30 * MIN, -0.0002),
        (rig.bar(0), 0.0003),
        (rig.bar(1), 0.0004),
        (rig.bar(2), -0.0001),
    ];
    *rig.rest.rates.lock().unwrap() = funding.clone();
    let cfg = config(-1, -1, false);
    let feed = rig.feed();
    feed.minute(rig.bar(3));
    let run = rig.run(rig.create("seen", cfg.clone()).expect("creates"), &feed);
    wait_for_bar(&run, rig.bar(2));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert!(live.status().warnings.is_empty(), "{:?}", live.status());

    let in_live = seen_live(&rig, "seen", &cfg);
    let bars = live.session().bars();
    let opens: Vec<Millis> = in_live.iter().map(|s| s.0).collect();
    assert_eq!(opens, bars.iter().map(|b| b.open_time).collect::<Vec<_>>());
    assert_eq!(in_live.len(), WARM_UP as usize + 3);
    assert_eq!(in_live, seen_in_backtest(&cfg, bars, rig.bar(0), &funding));
    // On the first live bar: every rate from the first warm-up bar's open on, at its own time.
    let first_live = in_live.iter().find(|s| !s.1).expect("a live bar");
    assert_eq!(
        (first_live.0, &first_live.2[..]),
        (rig.bar(0), &funding[1..4])
    );
}

#[test]
fn a_funding_read_that_keeps_failing_halts_the_run() {
    let rig = Rig::new();
    eight_hourly(&rig);
    // The history read before going live works; every read for a bar fails.
    rig.rest.funding_works_for.store(1, Ordering::SeqCst);
    let feed = rig.feed();
    feed.minute(rig.bar(1));
    let live = rig
        .create("unfunded", config(0, 99, false))
        .expect("creates");
    let (live, ended) = rig.run(live, &feed).wait();
    let Err(LiveError::Halted(reason)) = ended else {
        panic!("not halted: {ended:?}");
    };
    let expected = format!("funding for the bar at {} could not be read", rig.bar(0));
    assert!(reason.contains(&expected), "{reason}");
    assert_eq!(rig.rest.funding_reads.load(Ordering::SeqCst), 1 + 4);
    assert_eq!(live.status().last_bar, None, "bar 0 was not fed");
    assert_eq!(rig.sends(), 0);
}

#[test]
fn a_fresh_start_is_refused_when_the_account_holds_only_open_orders() {
    let rig = Rig::new();
    let stop = OrderRequest {
        order_type: OrderType::Stop { trigger: STOP },
        ..market(Side::Sell, QTY, true)
    };
    let id = ClientOrderId::new("manual-1").expect("id");
    rig.client
        .place(&instrument(), &stop, &id, PRICE)
        .expect("placed");
    assert_eq!(rig.mock.position(SYMBOL), 0.0);

    let refused = rig
        .create("orders", config(0, 99, false))
        .err()
        .expect("refused");
    assert!(
        matches!(refused, LiveError::NotFlat { open_orders: 1, .. }),
        "{refused}"
    );
    assert!(!live_dir(rig.data(), "orders").exists());
}

/// Makes `link` point at the folder `target`: a symlink, or on Windows without the right to make
/// one, a junction. Returns why neither could be made.
fn link_dir(target: &Path, link: &Path) -> Result<(), String> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link).map_err(|e| e.to_string());
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return Ok(());
        }
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .map_err(|e| e.to_string())?;
        if made.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&made.stderr).into_owned())
        }
    }
}

fn link_file(target: &Path, link: &Path) -> Result<(), String> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link).map_err(|e| e.to_string());
    #[cfg(windows)]
    return std::os::windows::fs::symlink_file(target, link).map_err(|e| e.to_string());
}

#[test]
fn a_link_in_place_of_the_session_folder_or_its_log_is_refused() {
    let rig = Rig::new();
    drop(rig.create("real", config(0, 99, false)).expect("creates"));

    match link_dir(
        &live_dir(rig.data(), "real"),
        &live_dir(rig.data(), "linked"),
    ) {
        Ok(()) => {
            let refused = rig.open("linked").err().expect("refused");
            assert!(matches!(refused, LiveError::Link(_)), "{refused}");
            let refused = rig
                .create("linked", config(0, 99, false))
                .err()
                .expect("refused");
            assert!(matches!(refused, LiveError::Exists(_)), "{refused}");
        }
        Err(why) => println!("skipped the folder link: it could not be made: {why}"),
    }

    let log = rig.path("real", "events.jsonl");
    let outside = rig.data().join("elsewhere");
    fs::create_dir_all(&outside).expect("folder");
    let target = outside.join("events.jsonl");
    fs::rename(&log, &target).expect("move the log out");
    match link_file(&target, &log) {
        Ok(()) => {
            let refused = rig.open("real").err().expect("refused");
            assert!(matches!(refused, LiveError::Link(_)), "{refused}");
        }
        Err(why) => println!("skipped the log link: it could not be made: {why}"),
    }
}

/// A Bybit source whose stream parser panics on the frame `boom`, which ends the feed's task.
struct Panicky;

static PANICKY: Panicky = Panicky;

fn bybit() -> &'static dyn DataSource {
    td_data::source(Venue::Bybit)
}

impl DataSource for Panicky {
    fn venue(&self) -> Venue {
        bybit().venue()
    }

    fn kinds(&self) -> &'static [InstrumentKind] {
        bybit().kinds()
    }

    fn instruments(&self, http: &Http, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
        bybit().instruments(http, kind)
    }

    fn default_fees(&self, kind: InstrumentKind) -> Result<FeeSchedule, DataError> {
        bybit().default_fees(kind)
    }

    fn minute_history(
        &self,
        http: &Http,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
        now: Millis,
    ) -> Result<MinuteHistory, DataError> {
        bybit().minute_history(http, id, from, to, now)
    }

    fn live(&self) -> Result<&dyn LiveStream, DataError> {
        Ok(self)
    }
}

impl LiveStream for Panicky {
    fn url(&self, id: &InstrumentId) -> String {
        bybit().live().expect("a stream").url(id)
    }

    fn greeting(&self, id: &InstrumentId) -> Greeting {
        bybit().live().expect("a stream").greeting(id)
    }

    fn parse(&self, text: &str) -> Result<Vec<KlineUpdate>, DataError> {
        assert_ne!(text, "boom", "the stream's parser broke on purpose");
        bybit().live()?.parse(text)
    }

    fn klines(
        &self,
        http: &Http,
        id: &InstrumentId,
        timeframe: Timeframe,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<Candle>, DataError> {
        bybit().live()?.klines(http, id, timeframe, from, to)
    }
}

#[test]
fn a_feed_that_ends_halts_the_run() {
    let rig = Rig::new();
    let feed = rig.feed();
    let url = feed.url.clone();
    let live = rig.create("ended", config(0, 99, false)).expect("creates");
    let run = live.run(
        rig.rest.clone(),
        move || Arc::new(SharedStream::open_on(&PANICKY, instrument().id, url)),
        |_, _| None,
    );
    wait_until(&run, "running", |s| s.state == State::Running);
    feed.text("boom".into());
    let deadline = Instant::now() + Duration::from_secs(20);
    while !run.is_finished() {
        assert!(Instant::now() < deadline, "the run did not halt");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (live, ended) = run.wait();
    let Err(LiveError::Halted(reason)) = ended else {
        panic!("not halted: {ended:?}");
    };
    assert!(reason.contains("the feed ended"), "{reason}");
    assert_eq!(rig.saved_status("ended").state, live.status().state);
    drop(live);
    rig.open("ended").expect("a halt resumes");
}

#[test]
fn a_status_that_cannot_be_written_still_halts_the_run_in_memory() {
    let rig = Rig::new();
    let live = rig
        .create("unwritable", config(0, 99, false))
        .expect("creates");
    // A folder in the way of the status's temporary file.
    fs::create_dir(rig.path("unwritable", "status.json.tmp")).expect("folder");
    let feed = rig.feed();
    let (live, ended) = rig.run(live, &feed).wait();
    let error = ended.expect_err("halted");
    assert!(error.to_string().contains("status.json.tmp"), "{error}");
    assert!(
        matches!(&live.status().state,
            State::Halted { reason } if reason.contains("status.json.tmp")),
        "{:?}",
        live.status()
    );
    assert!(
        live.status()
            .warnings
            .iter()
            .any(|w| w.contains("the status could not be written")),
        "{:?}",
        live.status().warnings
    );
    assert!(!feed.opened.load(Ordering::SeqCst));
    assert_eq!(rig.rest.warm_up_reads.load(Ordering::SeqCst), 0);
}

#[test]
fn a_status_left_running_by_a_process_that_died_reads_as_stopped() {
    let rig = Rig::new();
    two_bars(&rig, "died");
    rewrite_state(&rig, "died", State::Running);
    let live = rig.open("died").expect("reopens");
    assert!(
        matches!(&live.status().state,
            State::Stopped { note } if note.contains("ended without stopping")),
        "{:?}",
        live.status().state
    );
    assert_eq!(
        rig.saved_status("died").state,
        live.status().state,
        "saved too"
    );
}

#[test]
fn a_folder_with_its_config_and_no_status_opens_as_not_started_only_while_its_log_is_empty() {
    let rig = Rig::new();
    drop(rig.create("half", config(0, 99, false)).expect("creates"));
    fs::remove_file(rig.path("half", "status.json")).expect("remove");
    fs::remove_file(rig.path("half", "events.jsonl")).expect("remove");
    let live = rig.open("half").expect("opens");
    assert_eq!(live.status().state, stopped("created; not started"));
    drop(live);

    two_bars(&rig, "bars");
    fs::remove_file(rig.path("bars", "status.json")).expect("remove");
    let refused = rig.open("bars").err().expect("refused");
    assert!(matches!(refused, LiveError::BadFile { .. }), "{refused}");
}

#[test]
fn a_config_whose_prefix_was_edited_is_refused_without_asking_the_venue() {
    let rig = Rig::new();
    drop(rig.create("prefix", config(0, 99, false)).expect("creates"));
    let path = rig.path("prefix", "config.json");
    let mut saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    saved["prefix"] = "zzz".into();
    fs::write(&path, serde_json::to_vec(&saved).expect("json")).expect("write");

    let requests = rig.mock.requests().len();
    let refused = rig.open("prefix").err().expect("refused");
    assert!(
        matches!(&refused, LiveError::BadFile { reason, .. } if reason.contains("prefix")),
        "{refused}"
    );
    assert_eq!(rig.mock.requests().len(), requests);
}

#[test]
fn warnings_are_trimmed_when_a_status_is_loaded() {
    let rig = Rig::new();
    drop(rig.create("noisy", config(0, 99, false)).expect("creates"));
    let mut status = rig.saved_status("noisy");
    status.warnings = (0..150).map(|i| format!("warning {i}")).collect();
    status.warnings.push("x".repeat(10_000));
    fs::write(
        rig.path("noisy", "status.json"),
        serde_json::to_vec(&status).expect("json"),
    )
    .expect("write");

    let live = rig.open("noisy").expect("opens");
    let warnings = &live.status().warnings;
    assert_eq!(warnings.len(), 100);
    assert_eq!(warnings[0], "warning 51");
    assert!(warnings.iter().all(|w| w.chars().count() <= 1001));
}
