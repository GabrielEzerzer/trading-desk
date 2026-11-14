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
    BacktestConfig, Context, DayRollover, EngineError, ParamValue, Params, Session, SessionConfig,
    StopRules, Strategy, run_strategy,
};
use td_live::{
    GuardTiming, LiveConfig, LiveError, LiveRun, LiveSession, LiveStatus, Outcome, State,
    StopHandle, VenueHandle, live_dir, run_guarded,
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
/// on that bar. On every live bar from `add_from` on it buys [`QTY`] more. `eager` instead submits
/// a buy on every bar, warm-up bars included, while it is flat with no open orders.
struct Double {
    buy_on: i64,
    close_on: i64,
    panic_on: i64,
    fail_on: i64,
    add_from: i64,
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
        if self.add_from >= 0 && self.bar >= self.add_from {
            ctx.submit(market(Side::Buy, QTY, false)).expect("add");
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
        add_from: params.int("add_from")?,
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
        .with("add_from", ParamValue::Int(-1))
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
        // Wide enough never to fire or refuse here; tests/stops.rs tests the stops.
        stops: StopRules {
            daily_loss_share: 1.0,
            max_loss_share: 1.0,
            open_notional_cap: 1e12,
            rollover: DayRollover::default(),
        },
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

/// Minutes from `start` on, [`WARM_UP`] hourly bars before it less those opening at a time in
/// `missing`, the funding `rates`, and a clock fixed at `start`. Funding reads after the first
/// `funding_works_for` fail.
struct FakeRest {
    start: Millis,
    missing: Mutex<Vec<Millis>>,
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
        let missing = self.missing.lock().unwrap();
        let mut t = from.max(first).div_euclid(step) * step;
        let mut out = Vec::new();
        while t < to {
            if t >= from && (step == MIN || !missing.contains(&t)) {
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
                missing: Mutex::new(Vec::new()),
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
        LiveSession::create(self.data(), id, config, None, self.client.clone(), build)
    }

    fn open(&self, id: &str) -> Result<LiveSession, LiveError> {
        LiveSession::open(self.data(), id, self.client.clone(), build)
    }

    fn run(&self, live: LiveSession, feed: &Feed) -> LiveRun {
        td_live::run_unguarded(live, self.rest.clone(), feed.opener(), |_, _| None)
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
        // Whether the run had ended is read before its status, so a status written just before
        // it ended, such as a halt, is still read once after the end.
        let finished = run.is_finished();
        let status = run.status();
        if done(&status) {
            return;
        }
        assert!(!finished, "the run ended before {what}: {status:?}");
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

/// The broker's rule for a venue it cannot read ("Unreadable venue" in the crate header), seen
/// through a running session: no order that could add to the position goes out, the status warns
/// and the session runs on, and the third bar in a row without a full read halts it for good.
#[test]
fn a_venue_that_cannot_be_read_gets_no_adding_order_and_halts_the_session_on_the_third_bar() {
    let rig = Rig::new();
    let mut cfg = config(0, 99, true);
    cfg.params.set("add_from", ParamValue::Int(2));
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(rig.create("blind", cfg).expect("creates"), &feed);

    // Bar 0 bought with a stop; by the end of bar 1 both are at the venue.
    wait_for_bar(&run, rig.bar(1));
    assert_eq!(rig.mock.position(SYMBOL), QTY);
    let orders = rig.mock.orders();
    assert_eq!(orders.len(), 2, "{orders:?}");
    assert_eq!(orders.iter().filter(|o| o.reduce_only).count(), 1);

    // From bar 2 on the strategy buys more on every bar, and the venue cannot be read.
    for path in [
        "/v5/execution/list",
        "/v5/order/realtime",
        "/v5/position/list",
        WALLET,
    ] {
        rig.mock.inject_always(path, Fault::DropBeforeApply);
    }
    let sent = || [CREATE, CANCEL, AMEND].map(|p| rig.mock.request_count(p));
    let before = sent();

    // One bar without a read: a warning, still running, nothing sent, cancelled or amended.
    feed.minute(rig.bar(3));
    wait_for_bar(&run, rig.bar(2));
    let status = run.status();
    assert_eq!(status.state, State::Running);
    assert!(
        status
            .warnings
            .iter()
            .any(|w| w.contains("the venue could not be read")),
        "{status:?}"
    );
    assert_eq!(sent(), before, "creates, cancels and amends");

    // Two more: the third halts the run on the broker's fault.
    feed.minute(rig.bar(5));
    wait_until(&run, "the fault", |s| {
        matches!(s.state, State::Faulted { .. })
    });
    let (live, ended) = run.wait();
    let Err(error) = &ended else {
        panic!("not faulted: {ended:?}");
    };
    let LiveError::Faulted(fault) = error else {
        panic!("not faulted: {error}");
    };
    assert!(
        fault.starts_with("venue unreadable: 3 bars in a row without a full read"),
        "{fault}"
    );
    assert!(error.to_string().contains("broker fault"), "{error}");
    assert!(error.to_string().contains(fault.as_str()), "{error}");
    assert_eq!(live.status().last_bar, Some(rig.bar(4)));

    // The venue received no order, cancel or amend while it could not be read, so nothing added
    // to the position and the stop was left as it was.
    assert_eq!(sent(), before, "creates, cancels and amends");
    assert_eq!(rig.mock.orders().len(), 2);
    assert_eq!(rig.mock.position(SYMBOL), QTY);
    // The adds the strategy decided were cancelled with the broker's reason, never sent.
    let refused = live
        .session()
        .orders()
        .iter()
        .filter(|o| !o.request.reduce_only && o.attached_stop.is_none())
        .filter(|o| matches!(&o.status, td_engine::OrderStatus::Cancelled(r) if r.contains("could not be read")))
        .count();
    assert_eq!(refused, 2, "bars 2 and 3: {:?}", live.session().orders());

    assert_eq!(
        rig.saved_status("blind").state,
        State::Faulted {
            fault: fault.clone()
        }
    );
    let marker = fs::read_to_string(rig.path("blind", "faulted")).expect("faulted marker");
    assert!(marker.contains("without a full read"), "{marker}");
    drop(live);
    let refused = rig.open("blind").err().expect("refused");
    assert!(
        matches!(&refused, LiveError::StillFaulted(f) if f == fault),
        "{refused}"
    );
}

/// The other half of the broker's rule for a venue it cannot read, seen through a running session:
/// a reduce-only order the strategy decides still goes out, so a position can be closed while the
/// venue's reads fail.
#[test]
fn a_reduce_only_order_still_goes_out_while_the_venue_cannot_be_read() {
    let rig = Rig::new();
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(
        rig.create("close", config(0, 3, false)).expect("creates"),
        &feed,
    );

    // Bar 0 bought; by the end of bar 1 the venue holds the position.
    wait_for_bar(&run, rig.bar(1));
    assert_eq!(rig.mock.position(SYMBOL), QTY);
    assert_eq!(rig.mock.orders().len(), 1);

    // From bar 2 on the venue cannot be read; bar 3 sells the position reduce-only.
    for path in [
        "/v5/execution/list",
        "/v5/order/realtime",
        "/v5/position/list",
        WALLET,
    ] {
        rig.mock.inject_always(path, Fault::DropBeforeApply);
    }
    feed.minute(rig.bar(4));
    wait_for_bar(&run, rig.bar(3));
    let status = run.status();
    let (_live, ended) = run.stop();
    ended.expect("stopped cleanly");

    assert_eq!(status.state, State::Running);
    assert!(
        status
            .warnings
            .iter()
            .any(|w| w.contains("the venue could not be read")),
        "{status:?}"
    );
    let orders = rig.mock.orders();
    assert_eq!(orders.len(), 2, "{orders:?}");
    assert_eq!(orders[1].side, Side::Sell);
    assert!(orders[1].reduce_only);
    assert_eq!(orders[1].qty, QTY);
    assert_eq!(rig.mock.position(SYMBOL), 0.0);
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
    let run = td_live::run_unguarded(
        live,
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
        references: Vec::new(),
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

/// With the last warm-up bar missing at the venue, a rate inside it is shown with the first live
/// bar, at its own time, as a backtest of the same bars shows it.
#[test]
fn a_rate_inside_a_missing_last_warm_up_bar_reaches_the_first_live_bar() {
    let rig = Rig::new();
    rig.rest.missing.lock().unwrap().push(rig.bar(-1));
    // Every half hour from inside the first warm-up bar to inside the second live bar, two of
    // them at the missing bar's open and inside it.
    let funding: Vec<(Millis, f64)> = (1..10)
        .map(|k| (rig.bar(-3) + k * 30 * MIN, 0.0001 * k as f64))
        .collect();
    *rig.rest.rates.lock().unwrap() = funding.clone();
    let cfg = config(-1, -1, false);
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(rig.create("gap", cfg.clone()).expect("creates"), &feed);
    wait_for_bar(&run, rig.bar(1));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert!(live.status().warnings.is_empty(), "{:?}", live.status());

    let in_live = seen_live(&rig, "gap", &cfg);
    let bars = live.session().bars();
    let opens: Vec<Millis> = bars.iter().map(|b| b.open_time).collect();
    assert_eq!(
        opens,
        [rig.bar(-3), rig.bar(-2), rig.bar(0), rig.bar(1)],
        "the missing bar is not fed"
    );
    assert_eq!(in_live, seen_in_backtest(&cfg, bars, rig.bar(0), &funding));
    let first_live = in_live.iter().find(|s| !s.1).expect("a live bar");
    // On the first live bar: every rate from the first warm-up bar's open to its own end.
    assert_eq!(
        (first_live.0, &first_live.2[..]),
        (rig.bar(0), &funding[..7])
    );
    assert_eq!(rig.sends(), 0);
}

/// A warm-up the log holds part of, left by a process that ended, is finished on resume from the
/// end of the log: the rates the log holds are not shown again, those after it are shown once,
/// and the strategy sees what a backtest of the same bars shows it.
#[test]
fn a_resumed_part_finished_warm_up_shows_each_rate_once() {
    let rig = Rig::new();
    // Every half hour from inside the first warm-up bar to inside the second live bar.
    let funding: Vec<(Millis, f64)> = (1..10)
        .map(|k| (rig.bar(-3) + k * 30 * MIN, 0.0001 * k as f64))
        .collect();
    *rig.rest.rates.lock().unwrap() = funding.clone();
    let cfg = config(-1, -1, false);
    // Held until the run stops: a feed dropped earlier closes its socket, and the disconnect is a
    // warning.
    let first_feed = rig.feed();
    let run = rig.run(
        rig.create("part", cfg.clone()).expect("creates"),
        &first_feed,
    );
    wait_until(&run, "running", |s| s.state == State::Running);
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    drop(first_feed);
    drop(live);
    // The process ended after the first warm-up bar's line: the later warm-up lines are not synced.
    let path = rig.path("part", "events.jsonl");
    let text = fs::read_to_string(&path).expect("log");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), WARM_UP as usize);
    fs::write(&path, format!("{}\n", lines[0])).expect("cut the log");
    rewrite_state(&rig, "part", State::WarmingUp);

    let live = rig.open("part").expect("reopens");
    assert_eq!(live.session().bars().len(), 1);
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(live, &feed);
    wait_for_bar(&run, rig.bar(1));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert!(live.status().warnings.is_empty(), "{:?}", live.status());

    let in_live = seen_live(&rig, "part", &cfg);
    let bars = live.session().bars();
    assert_eq!(bars.len(), WARM_UP as usize + 2);
    assert_eq!(in_live, seen_in_backtest(&cfg, bars, rig.bar(0), &funding));
    let logged: Vec<Millis> = rig
        .log("part")
        .iter()
        .filter(|l| l["sending"] != true)
        .flat_map(|l| l["funding"].as_array().cloned().unwrap_or_default())
        .map(|f| f["time"].as_i64().expect("time"))
        .collect();
    let times: Vec<Millis> = funding.iter().map(|f| f.0).collect();
    assert_eq!(logged, times, "each rate is logged once");
    assert_eq!(rig.sends(), 0);
}

/// Warm-up does not feed a bar without the funding rates it would show: a funding read that fails
/// after its retries halts the run before any bar is fed, and it does not go live.
#[test]
fn a_funding_read_that_fails_during_warm_up_halts_before_a_bar_and_sends_nothing() {
    let rig = Rig::new();
    eight_hourly(&rig);
    rig.rest.funding_works_for.store(0, Ordering::SeqCst);
    let feed = rig.feed();
    feed.minute(rig.bar(1));
    let live = rig
        .create("unfunded-warm", config(0, 99, false))
        .expect("creates");
    let (live, ended) = rig.run(live, &feed).wait();
    let Err(LiveError::Halted(reason)) = ended else {
        panic!("not halted: {ended:?}");
    };
    assert!(
        reason.contains("funding rates could not be read after 3 retries"),
        "{reason}"
    );
    assert_eq!(rig.rest.funding_reads.load(Ordering::SeqCst), 1 + 3);
    assert_eq!(live.status().state, State::Halted { reason });
    assert_eq!(live.status().last_bar, None);
    assert!(live.session().bars().is_empty(), "no warm-up bar was fed");
    assert!(rig.log("unfunded-warm").is_empty());
    assert!(!feed.opened.load(Ordering::SeqCst), "it did not go live");
    assert_eq!(rig.sends(), 0);
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
    let run = td_live::run_unguarded(
        live,
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
fn a_feed_that_ends_halts_a_guarded_run_with_exit_code_one() {
    let rig = Rig::new();
    let feed = rig.feed();
    let url = feed.url.clone();
    let live = rig
        .create("guarded", config(0, 99, false))
        .expect("creates");
    let rest = rig.rest.clone();
    let guarded = std::thread::spawn(move || {
        let mut lines = Vec::new();
        let outcome = run_guarded(
            live,
            rest,
            move || Arc::new(SharedStream::open_on(&PANICKY, instrument().id, url)),
            GuardTiming::default(),
            &StopHandle::new(),
            &mut |line| lines.push(line.to_string()),
        );
        (outcome, lines)
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while rig.saved_status("guarded").state != State::Running {
        assert!(Instant::now() < deadline, "the run did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    feed.text("boom".into());
    while !guarded.is_finished() {
        assert!(Instant::now() < deadline, "the run did not halt");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (outcome, lines) = guarded.join().expect("the guarded run");
    let Outcome::Halted(reason) = &outcome else {
        panic!(
            "not halted: {outcome:?}
{}",
            lines.join(
                "
"
            )
        );
    };
    assert!(reason.contains("the feed ended"), "{reason}");
    assert_eq!(outcome.exit_code(), 1);
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

// ---- the link to what a session was started from, and `td-live status` ----

/// The registered strategy the link tests start: it trades nothing on the rig's flat bars.
const LINKED: &str = "sma_cross";
const LINKED_PARAMS: &str = r#"{"fast":10,"slow":30}"#;

impl Rig {
    /// An `Env` for the command on this rig. Its stop is asked for before the command runs, so a
    /// `start` or `resume` creates or opens the session and stops it cleanly.
    fn env(&self, feed: &Feed) -> td_live::cli::Env {
        let client = self.client.clone();
        let (url, data) = (feed.url.clone(), self.data().to_path_buf());
        let stop = StopHandle::new();
        stop.stop();
        td_live::cli::Env {
            connect: Box::new(move || Ok(client.clone())),
            rest: self.rest.clone(),
            stream: Arc::new(move |id| Arc::new(SharedStream::open(id, url.clone()))),
            build,
            data_dir: Box::new(move || Ok(data.clone())),
            timing: GuardTiming {
                interval: Duration::from_millis(100),
                pause: Duration::from_millis(50),
                rounds: 6,
                unreadable: 4,
            },
            stop,
        }
    }

    /// Runs `td-live` with `args` on this rig and returns its exit code and output.
    fn td_live(&self, feed: &Feed, args: &[&str]) -> (u8, String) {
        use clap::Parser;
        let cli = td_live::cli::Cli::try_parse_from(
            std::iter::once("td-live").chain(args.iter().copied()),
        )
        .expect("parses");
        let mut lines = Vec::new();
        let code = td_live::cli::execute(cli, &self.env(feed), &mut |l| lines.push(l.to_string()));
        (code, lines.join("\n"))
    }

    /// `td-live start` of [`LINKED`] as session `id`, with `extra` arguments.
    fn start_linked(&self, feed: &Feed, id: &str, extra: &[&str]) -> (u8, String) {
        let mut args = vec![
            "start",
            "--id",
            id,
            "--strategy",
            LINKED,
            "--params",
            LINKED_PARAMS,
            "--symbol",
            SYMBOL,
            "--timeframe",
            "1h",
            "--daily-loss",
            "1",
            "--max-loss",
            "1",
            "--notional-cap",
            "1000000000000",
        ];
        args.extend_from_slice(extra);
        self.td_live(feed, &args)
    }

    fn config_json(&self, id: &str) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(self.path(id, "config.json")).expect("config"))
            .expect("config reads")
    }

    fn write_config_json(&self, id: &str, config: &serde_json::Value) {
        fs::write(
            self.path(id, "config.json"),
            serde_json::to_vec_pretty(config).expect("json"),
        )
        .expect("write");
    }

    /// Saves a verdict of [`LINKED`] on `on`, as its plan and rule are now, as run `run_id` in the
    /// runs folder, judged with `references`.
    fn save_verdict(&self, run_id: &str, on: &InstrumentId, references: Vec<InstrumentId>) {
        let entry = td_engine::find_strategy(LINKED).expect("registered");
        let record = td_engine::VerdictRecord {
            strategy: LINKED.into(),
            instrument: on.clone(),
            seed: 1,
            engine_revision: td_engine::ENGINE_REVISION,
            verdict: td_engine::Verdict::new(on.clone(), Vec::new()),
            plan: serde_json::to_value(&entry.plan).unwrap(),
            rule: serde_json::to_value(entry.rule).unwrap(),
            complete: true,
            code_hash: td_engine::code_hash(LINKED).map(String::from),
            references,
        };
        let dir = self.data().join(td_engine::RUNS_DIR).join(run_id);
        fs::create_dir_all(&dir).expect("run folder");
        fs::write(
            dir.join(td_engine::VERDICT_FILE),
            serde_json::to_vec(&record).expect("json"),
        )
        .expect("verdict");
    }
}

const OLDER: &str = "1700000000000-0000000a";
const NEWER: &str = "1700000000001-0000000b";

/// The status lines that start with `link`.
fn link_status(rig: &Rig, feed: &Feed, id: &str) -> String {
    let (code, text) = rig.td_live(feed, &["status", "--id", id]);
    assert_eq!(code, 0, "{text}");
    text.lines()
        .filter(|l| l.starts_with("link"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn start_links_the_session_to_the_latest_verdict_or_the_one_named_and_status_checks_it() {
    let rig = Rig::new();
    let feed = rig.feed();
    let on = instrument().id;
    rig.save_verdict(OLDER, &on, Vec::new());
    rig.save_verdict(NEWER, &on, Vec::new());

    let (code, text) = rig.start_linked(&feed, "latest", &[]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("created session latest"), "{text}");
    let link = &rig.config_json("latest")["link"];
    assert_eq!(link["verdict"]["run_id"], NEWER);
    assert_eq!(
        link["code_hash"].as_str(),
        td_engine::code_hash(LINKED),
        "the hash of the strategy's file"
    );
    let entry = td_engine::find_strategy(LINKED).unwrap();
    assert_eq!(link["plan"], serde_json::to_value(&entry.plan).unwrap());
    assert_eq!(link["rule"], serde_json::to_value(entry.rule).unwrap());

    // The session started with the code, plan and rule the verdict judged, on the engine that
    // judged it. The verdict's engine and code are this build's. The strategy's plan does not
    // test this venue's perpetual, so both plan checks fail and the link is stale, as the verdict
    // list would show it.
    let status = link_status(&rig, &feed, "latest");
    assert!(
        status.contains(&format!(
            "link: started from verdict {NEWER} of {LINKED} on {on}"
        )),
        "{status}"
    );
    assert!(
        status.contains(
            "link checks of what the session started with against the verdict: plan and rule \
             does not match, code matches, engine revision matches\n"
        ),
        "{status}"
    );
    assert!(
        status.contains(
            "link checks of the verdict against the strategy as this td-live build has it: \
             engine revision matches, plan and rule does not match, code matches"
        ),
        "{status}"
    );
    assert!(
        status.contains(
            "link: stale; what the session started with, or the code, plan or engine of the \
             strategy as this td-live build has it, may not be what that verdict judged; a \
             running session keeps the code it was started with until it is restarted"
        ),
        "{status}"
    );

    // A session started with other code than the verdict judged shows it from its saved link,
    // whatever this build's code is.
    let mut config = rig.config_json("latest");
    config["link"]["code_hash"] = "0000000000000000".into();
    rig.write_config_json("latest", &config);
    let status = link_status(&rig, &feed, "latest");
    assert!(
        status.contains(
            "against the verdict: plan and rule does not match, code does not match, engine revision \
             matches\nlink checks of the \
             verdict against the strategy as this td-live build has it: engine revision matches, \
             plan and rule does not match, code matches"
        ),
        "{status}"
    );

    // An older engine revision shows in its own check.
    let mut config = rig.config_json("latest");
    config["link"]["verdict"]["engine_revision"] = (td_engine::ENGINE_REVISION - 1).into();
    rig.write_config_json("latest", &config);
    let status = link_status(&rig, &feed, "latest");
    assert!(
        status.contains("engine revision does not match, plan and rule does not match"),
        "{status}"
    );
    // The session was made by this engine, not the one the verdict was judged by.
    assert!(
        status.contains(
            "against the verdict: plan and rule does not match, code does not match, engine revision \
             does not match\n"
        ),
        "{status}"
    );

    // A verdict named with --verdict is the one linked, in a data folder of its own.
    let other = tempfile::tempdir().expect("temp dir");
    let runs = other.path().join(td_engine::RUNS_DIR);
    fs::create_dir_all(&runs).unwrap();
    for run in [OLDER, NEWER] {
        let from = rig.data().join(td_engine::RUNS_DIR).join(run);
        fs::create_dir_all(runs.join(run)).unwrap();
        fs::copy(
            from.join(td_engine::VERDICT_FILE),
            runs.join(run).join(td_engine::VERDICT_FILE),
        )
        .unwrap();
    }
    let data = other.path().to_str().unwrap();
    let (code, text) = rig.start_linked(&feed, "named", &["--verdict", OLDER, "--data-dir", data]);
    assert_eq!(code, 0, "{text}");
    let config: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(live_dir(other.path(), "named").join("config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(config["link"]["verdict"]["run_id"], OLDER);
}

#[test]
fn a_link_without_a_verdict_is_current_until_the_code_or_plan_it_started_with_changes() {
    let rig = Rig::new();
    let feed = rig.feed();
    let (code, text) = rig.start_linked(&feed, "unjudged", &[]);
    assert_eq!(code, 0, "{text}");
    let started = rig.config_json("unjudged");
    assert!(started["link"]["verdict"].is_null(), "{started}");

    let status = link_status(&rig, &feed, "unjudged");
    let on = instrument().id;
    assert_eq!(
        status,
        format!(
            "link: started with no saved verdict of {LINKED} on {on}; the strategy's code hash was {}\n\
             link checks of what the session started with against the strategy as this td-live \
             build has it: plan and rule matches, code matches, engine revision matches\n\
             link: current",
            td_engine::code_hash(LINKED).unwrap()
        )
    );

    // As if the strategy's file had been edited since the session started.
    let mut edited = started.clone();
    edited["link"]["code_hash"] = "0000000000000000".into();
    rig.write_config_json("unjudged", &edited);
    let status = link_status(&rig, &feed, "unjudged");
    assert!(
        status.contains(
            "plan and rule matches, code does not match, engine revision matches\nlink: stale; \
             the code, plan or engine of the strategy as this td-live build has it may not be \
             what the session started with; a running session keeps the code it was started \
             with until it is restarted"
        ),
        "{status}"
    );

    // As if the session had been made by another engine revision than this build's; the link
    // saves none, so the one in its config is checked.
    let mut other_engine = started.clone();
    other_engine["engine_revision"] = (td_engine::ENGINE_REVISION + 1).into();
    rig.write_config_json("unjudged", &other_engine);
    let status = link_status(&rig, &feed, "unjudged");
    assert!(
        status.contains(
            "plan and rule matches, code matches, engine revision does not match\nlink: stale; \
             the code, plan or engine"
        ),
        "{status}"
    );

    // As if its plan had been changed since.
    let mut replanned = started.clone();
    let from = replanned["link"]["plan"]["period"]["from"]
        .as_i64()
        .unwrap();
    replanned["link"]["plan"]["period"]["from"] = (from - 86_400_000).into();
    rig.write_config_json("unjudged", &replanned);
    let status = link_status(&rig, &feed, "unjudged");
    assert!(
        status.contains("plan and rule does not match, code matches, engine revision matches"),
        "{status}"
    );
    assert!(
        status.contains("\nlink: stale; the code, plan or engine"),
        "{status}"
    );
}

#[test]
fn a_session_saved_without_a_link_loads_resumes_and_shows_status() {
    let rig = Rig::new();
    let feed = rig.feed();
    // The test double is not registered, so its session has no link, and its config.json is the
    // one a session saved before links were recorded holds: the same fields, with no `link`.
    drop(rig.create("old", config(-1, -1, false)).expect("creates"));
    let config = rig.config_json("old");
    assert!(config.get("link").is_none(), "{config}");
    let saved: td_live::SavedConfig = serde_json::from_value(config).expect("reads");
    assert_eq!(saved.link, None);

    let status = link_status(&rig, &feed, "old");
    assert_eq!(
        status,
        "link: none recorded; the session was created before links were recorded, or its \
         strategy is not registered"
    );
    drop(rig.open("old").expect("opens"));
    let (code, text) = rig.td_live(&feed, &["resume", "--id", "old"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("resumed session old"), "{text}");
}

/// A live session feeds its strategy no reference instruments, so it is never started from a
/// verdict judged with them, named or latest; nor from a run that is not a saved verdict of the
/// strategy on the instrument. Each refusal comes before the session's folder is made and before
/// any request reaches the venue.
#[test]
fn start_refuses_a_verdict_it_cannot_run_before_writing_or_asking_the_venue_anything() {
    let rig = Rig::new();
    let feed = rig.feed();
    let on = instrument().id;
    let reference = InstrumentId {
        symbol: "ETHUSDT".into(),
        ..on.clone()
    };
    rig.save_verdict(OLDER, &on, Vec::new());
    rig.save_verdict(NEWER, &on, vec![reference.clone()]);
    let another = "1700000000002-0000000c";
    rig.save_verdict(another, &reference, Vec::new());

    let requests = rig.mock.requests().len();
    let refused = |extra: &[&str], why: &[&str]| {
        let (code, text) = rig.start_linked(&feed, "refused", extra);
        assert_eq!(code, 1, "{extra:?}: {text}");
        assert!(text.starts_with("error: "), "{text}");
        for part in why {
            assert!(text.contains(part), "{extra:?}: {part:?} not in {text}");
        }
        assert_eq!(rig.mock.requests().len(), requests, "{extra:?}: {text}");
        assert!(!rig.data().join("live").exists(), "{extra:?}");
    };
    let feeds_none = format!(
        "run {NEWER} was judged with the reference instruments {reference}, and a live session \
         cannot feed reference instruments yet"
    );
    // The latest verdict was judged with a reference.
    refused(
        &[],
        &[
            &feeds_none,
            &format!("it is the strategy's latest verdict on {on}"),
            "name a verdict judged without them with --verdict",
        ],
    );
    // So is the verdict named.
    refused(&["--verdict", NEWER], &[&feeds_none]);
    // A verdict of another instrument, a run that is not a verdict, and an id that is not one.
    refused(
        &["--verdict", another],
        &[&format!(
            "run {another} is the verdict of {LINKED} on {reference}, not of {LINKED} on {on}"
        )],
    );
    refused(
        &["--verdict", "1700000000003-0000000d"],
        &["run 1700000000003-0000000d is not a saved verdict"],
    );
    refused(
        &["--verdict", "../../etc"],
        &["\"../../etc\" is not a run id"],
    );
    refused(&["--verdict", r"..\runs"], &["is not a run id"]);

    // A verdict judged without references still starts the session.
    let (code, text) = rig.start_linked(&feed, "plain", &["--verdict", OLDER]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(rig.config_json("plain")["link"]["verdict"]["run_id"], OLDER);
}

#[test]
fn a_verdict_named_for_a_strategy_that_is_not_registered_is_refused_before_anything_is_sent() {
    let rig = Rig::new();
    let feed = rig.feed();
    let requests = rig.mock.requests().len();
    let (code, text) = rig.td_live(
        &feed,
        &[
            "start",
            "--id",
            "double",
            "--strategy",
            "double",
            "--params",
            "{}",
            "--symbol",
            SYMBOL,
            "--timeframe",
            "1h",
            "--daily-loss",
            "1",
            "--max-loss",
            "1",
            "--notional-cap",
            "1",
            "--verdict",
            OLDER,
        ],
    );
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("double is not a registered strategy, so it has none"),
        "{text}"
    );
    assert_eq!(rig.mock.requests().len(), requests);
    assert!(!rig.data().join("live").exists());
}

/// A symbol the venue lists written another way is refused before a session is written or an
/// order sent, so the link and the session name one instrument. `Client::instrument_info` refuses
/// the answer first; td-live's own check stands behind it.
#[test]
fn start_refuses_a_symbol_the_venue_lists_written_another_way() {
    let rig = Rig::new();
    let feed = rig.feed();
    rig.mock.list_as("btcusdt", SYMBOL);
    let (code, text) = rig.td_live(
        &feed,
        &[
            "start",
            "--id",
            "lower",
            "--strategy",
            LINKED,
            "--params",
            LINKED_PARAMS,
            "--symbol",
            "btcusdt",
            "--timeframe",
            "1h",
            "--daily-loss",
            "1",
            "--max-loss",
            "1",
            "--notional-cap",
            "1000000000000",
        ],
    );
    assert_eq!(code, 1, "{text}");
    assert!(!rig.data().join("live").exists(), "{text}");
    let sent = rig.mock.requests();
    assert!(
        !sent
            .iter()
            .any(|r| [CREATE, CANCEL, AMEND].contains(&r.path.as_str())),
        "{text}"
    );
    assert!(text.contains("another symbol"), "{text}");
}

/// Text from the data folder reaches the owner's terminal without control characters, so a file
/// another person or program wrote cannot drive it: a run record's text in an error, and the link
/// `status` shows from a session's config.
#[test]
fn text_from_the_data_folder_reaches_the_terminal_without_control_characters() {
    let rig = Rig::new();
    let feed = rig.feed();
    let marked = InstrumentId {
        symbol: "ETH\u{1b}[2JUSDT".into(),
        ..instrument().id
    };
    rig.save_verdict(NEWER, &marked, Vec::new());
    let (code, text) = rig.start_linked(&feed, "refused", &["--verdict", NEWER]);
    assert_eq!(code, 1, "{text}");
    assert!(!text.chars().any(char::is_control), "{text:?}");
    assert!(
        text.contains(" on bybit:ETH [2JUSDT:perpetual, not of "),
        "{text}"
    );

    rig.save_verdict(OLDER, &instrument().id, Vec::new());
    let (code, text) = rig.start_linked(&feed, "marked", &[]);
    assert_eq!(code, 0, "{text}");
    let mut config = rig.config_json("marked");
    config["strategy"] = "sma\u{1b}[2Jcross".into();
    config["instrument"]["id"]["symbol"] = "BTC\u{7}USDT".into();
    config["link"]["code_hash"] = "\u{1b}]0;title\u{7}".into();
    config["link"]["verdict"]["run_id"] = "1700000000000-\u{1b}[31m".into();
    rig.write_config_json("marked", &config);
    let (code, text) = rig.td_live(&feed, &["status", "--id", "marked"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "{text:?}"
    );
    assert!(
        text.contains(
            "link: started from verdict 1700000000000- [31m of sma [2Jcross on \
             bybit:BTC USDT:perpetual: "
        ),
        "{text}"
    );
    assert!(
        text.contains("the strategy's code hash was  ]0;title \n"),
        "{text}"
    );
}

/// `status` reads a session's config only from a plain folder and file, as `compare` and a resume
/// do.
#[test]
fn status_refuses_a_link_in_place_of_the_session_folder_or_its_config() {
    let rig = Rig::new();
    let feed = rig.feed();
    drop(rig.create("real", config(0, 99, false)).expect("creates"));
    let refused = |id: &str| {
        let (code, text) = rig.td_live(&feed, &["status", "--id", id]);
        assert_eq!(code, 1, "{text}");
        assert!(text.starts_with("error: "), "nothing before it: {text}");
        assert!(text.contains("is a link or reparse point"), "{text}");
    };
    match link_dir(
        &live_dir(rig.data(), "real"),
        &live_dir(rig.data(), "linked"),
    ) {
        Ok(()) => refused("linked"),
        Err(why) => println!("skipped the folder link: it could not be made: {why}"),
    }

    let saved = rig.path("real", "config.json");
    let outside = rig.data().join("elsewhere");
    fs::create_dir_all(&outside).expect("folder");
    let target = outside.join("config.json");
    fs::rename(&saved, &target).expect("move the config out");
    match link_file(&target, &saved) {
        Ok(()) => refused("real"),
        Err(why) => println!("skipped the config link: it could not be made: {why}"),
    }
}

/// The status and stop files `status` prints are read only from a plain folder and plain files:
/// a link in place of the session folder, `status.json` or `stops.json` is refused before either
/// is read.
#[test]
fn the_saved_files_status_prints_refuse_a_link_in_place_of_the_folder_or_either_file() {
    let rig = Rig::new();
    drop(rig.create("real", config(0, 99, false)).expect("creates"));
    let refused = |id: &str, what: &str| {
        let read = td_live::saved_files(rig.data(), id);
        assert!(
            matches!(read, Err(LiveError::Link(_))),
            "{what}: {:?}",
            read.map(|_| "read")
        );
    };
    match link_dir(
        &live_dir(rig.data(), "real"),
        &live_dir(rig.data(), "linked"),
    ) {
        Ok(()) => refused("linked", "the folder"),
        Err(why) => println!("skipped the folder link: it could not be made: {why}"),
    }
    for file in ["status.json", "stops.json"] {
        let saved = rig.path("real", file);
        let outside = rig.data().join("elsewhere");
        fs::create_dir_all(&outside).expect("folder");
        let target = outside.join(file);
        fs::rename(&saved, &target).expect("move the file out");
        match link_file(&target, &saved) {
            Ok(()) => refused("real", file),
            Err(why) => println!("skipped the {file} link: it could not be made: {why}"),
        }
        let _ = fs::remove_file(&saved).or_else(|_| fs::remove_dir(&saved));
        fs::rename(&target, &saved).expect("move the file back");
    }
}

// ---- `td-live compare` ----

/// Runs session `id` on live bars 0 to 2: it buys on bar 0 and adds on every bar after. The venue
/// fills bar 0's buy 10 above the bar and refuses every order after it.
fn compared_session(rig: &Rig, feed: &Feed, id: &str) {
    let mut cfg = config(0, -1, false);
    cfg.params.set("add_from", ParamValue::Int(1));
    rig.mock.set_price(SYMBOL, PRICE + 10.0);
    feed.minute(rig.bar(1));
    let run = rig.run(rig.create(id, cfg).expect("creates"), feed);
    wait_for_bar(&run, rig.bar(0));
    rig.mock.inject_always(CREATE, Fault::Answer(10001));
    feed.minute(rig.bar(3));
    wait_for_bar(&run, rig.bar(2));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert_eq!(live.session().fills().len(), 1);
}

/// Writes `lines` as session `id`'s log.
fn write_log(rig: &Rig, id: &str, lines: &[serde_json::Value]) {
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(rig.path(id, "events.jsonl"), text).expect("log");
}

/// A session against the mock beside a backtest over its own bars ([`compared_session`]), while
/// the backtest also fills bar 1's add. `compare` reads the folder only: it runs with a connect
/// that fails and sends nothing.
#[test]
fn compare_pairs_the_venue_fills_with_a_backtest_over_the_session_bars() {
    let rig = Rig::new();
    let feed = rig.feed();
    compared_session(&rig, &feed, "cmp");

    let mut env = rig.env(&feed);
    env.connect = Box::new(|| Err("no keys in this test".into()));
    let requests = rig.mock.requests().len();
    let cli =
        <td_live::cli::Cli as clap::Parser>::try_parse_from(["td-live", "compare", "--id", "cmp"])
            .expect("parses");
    let mut lines = Vec::new();
    let code = td_live::cli::execute(cli, &env, &mut |l| lines.push(l.to_string()));
    let text = lines.join("\n");
    assert_eq!(code, 0, "{text}");
    assert_eq!(
        rig.mock.requests().len(),
        requests,
        "compare asked the venue nothing"
    );

    let at = |t: Millis| td_data::time::format_minute(t);
    // By hand. The venue: 0.004 at 65010, fee 65010 x 0.004 x 0.00055 = 0.143022, read on bar 1.
    // The backtest: 0.004 at bar 1's open of 65000, fee 260 x 0.00055 = 0.143; and bar 1's add at
    // bar 2's open, fee 0.143. Bar 2's add has no bar after it to fill on.
    assert!(
        text.contains(&format!(
            "compared session cmp with a backtest of double on {} 1h over the session's own bars: \
             3 live bars from {} to {}, after {WARM_UP} warm-up bars",
            instrument().id,
            at(rig.bar(0)),
            at(rig.bar(2)),
        )),
        "{text}"
    );
    assert!(
        text.contains("what is compared: the fills the venue reported"),
        "{text}"
    );
    assert!(text.contains("which the backtest cannot model"), "{text}");
    assert!(
        text.contains(&format!(
            "matched fills: 1\n  buy 0.004, decided on the bar opening at {}: session 65010 at \
             {}, backtest 65000 at {}; time difference 0 ms, price difference +10, quantity \
             difference 0, fee difference +0.000022",
            at(rig.bar(0)),
            at(rig.bar(1)),
            at(rig.bar(1)),
        )),
        "{text}"
    );
    assert!(
        text.contains("session fills with no backtest partner: 0\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "backtest fills with no session partner: 1\n  buy 0.004 at 65000 at {}, fee 0.143",
            at(rig.bar(2))
        )),
        "{text}"
    );
    assert!(
        text.contains(
            "fees: session 0.143022, backtest 0.286; funding paid: session 0, backtest 0"
        ),
        "{text}"
    );
    // The session loses its fee and 10 x 0.004 = 0.04 to the bar's close of 65000: -0.183022. The
    // backtest loses its two fees: -0.286. -0.183022 / -0.286 = 0.63993...
    assert!(
        text.ends_with(
            "profit and loss: session -0.183022, backtest -0.286; the session's over the \
             backtest's: 0.6399"
        ),
        "{text}"
    );

    // A strategy whose code or parameters now make other orders than the session made is not
    // compared: here it no longer adds on bar 1, as the record says it did.
    let mut config = rig.config_json("cmp");
    config["params"]["add_from"] = 5.into();
    rig.write_config_json("cmp", &config);
    let (code, text) = rig.td_live(&feed, &["compare", "--id", "cmp"]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("does not replay with the strategy as it is now"),
        "{text}"
    );
}

/// A session id that is not one is refused before any path is made from it, and a session
/// that has fed no live bar has nothing to compare.
#[test]
fn compare_refuses_a_bad_id_and_a_session_with_no_live_bar() {
    let rig = Rig::new();
    let feed = rig.feed();
    let (code, text) = rig.td_live(&feed, &["compare", "--id", "../x"]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("invalid session id \"../x\""), "{text}");
    drop(rig.create("empty", config(-1, -1, false)).expect("creates"));
    let (code, text) = rig.td_live(&feed, &["compare", "--id", "empty"]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("has not fed a live bar yet"), "{text}");
}

/// A log that ends on a `sending` line, as a session that stopped while sending, or is still
/// sending, leaves it: the orders the strategy makes on that bar past the record count as
/// unfilled. A strategy that now makes no send or cancel there does not replay, since the
/// session wrote that line because it was about to make one.
#[test]
fn compare_counts_the_orders_of_a_bar_the_log_ends_part_way_through_as_unfilled() {
    let rig = Rig::new();
    let feed = rig.feed();
    compared_session(&rig, &feed, "cut");
    drop_last_line(&rig.path("cut", "events.jsonl"));
    let log = rig.log("cut");
    let last = log.last().expect("a line");
    assert_eq!(
        (&last["sending"], &last["bar"]["open_time"]),
        (&true.into(), &rig.bar(2).into())
    );

    let at = |t: Millis| td_data::time::format_minute(t);
    let (code, text) = rig.td_live(&feed, &["compare", "--id", "cut"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains(&format!(
            "3 live bars from {} to {}, after {WARM_UP} warm-up bars",
            at(rig.bar(0)),
            at(rig.bar(2))
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "the log ends part way through the bar opening at {} (the session stopped, or is \
             still sending), and the 1 orders the strategy made on it after the record ended are \
             counted as unfilled\nmatched fills: 1\n",
            at(rig.bar(2))
        )),
        "{text}"
    );
    assert!(
        text.contains("session fills with no backtest partner: 0\n"),
        "{text}"
    );
    assert!(
        text.contains("backtest fills with no session partner: 1\n"),
        "{text}"
    );

    // The log cut after bar 0's sending line, with a strategy that no longer buys there.
    let first_sending = log.iter().position(|l| l["sending"] == true).unwrap();
    write_log(&rig, "cut", &log[..=first_sending]);
    let mut config = rig.config_json("cut");
    config["params"]["buy_on"] = 5.into();
    rig.write_config_json("cut", &config);
    let (code, text) = rig.td_live(&feed, &["compare", "--id", "cut"]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains(&format!(
            "it ends on the sending line of the bar opening at {}, which the session writes \
             before it sends or cancels, and the replay sent and cancelled nothing on that bar",
            at(rig.bar(0))
        )),
        "{text}"
    );
}

/// A recorded call too long to print in full is cut short where the replay error shows it, so the
/// error keeps its end, which says where the replay differs from the record.
#[test]
fn compare_keeps_the_end_of_a_replay_error_whose_recorded_call_is_long() {
    let rig = Rig::new();
    let feed = rig.feed();
    compared_session(&rig, &feed, "long");
    let mut log = rig.log("long");
    // A bar call that reports events, with them repeated, under a time that is not its bar's.
    let (at, call) = log
        .iter()
        .enumerate()
        .find_map(|(i, line)| {
            let calls = line["record"]["calls"].as_array()?;
            calls
                .iter()
                .position(|c| c["Bar"]["events"].as_array().is_some_and(|e| !e.is_empty()))
                .map(|c| (i, c))
        })
        .expect("a bar call with events");
    let bar = log[at]["bar"]["open_time"].as_i64().unwrap();
    let recorded = &mut log[at]["record"]["calls"][call]["Bar"];
    let events = recorded["events"].as_array().unwrap().clone();
    recorded["events"] = events
        .iter()
        .cycle()
        .take(20 * events.len())
        .cloned()
        .collect();
    recorded["open_time"] = (bar + 1).into();
    write_log(&rig, "long", &log);

    let (code, text) = rig.td_live(&feed, &["compare", "--id", "long"]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("does not replay with the strategy as it is now: the record has Some(Bar"),
        "{text}"
    );
    assert!(
        text.ends_with(&format!("… where the session fed the bar opening at {bar}")),
        "{text}"
    );
}

/// `compare` reads a faulted session's folder as any other: the fault halted the session after
/// its bars were logged, and its fills are compared.
#[test]
fn compare_works_on_a_faulted_session() {
    let rig = Rig::new();
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(
        rig.create("fault", config(0, 99, false)).expect("creates"),
        &feed,
    );
    wait_for_bar(&run, rig.bar(1));
    rig.mock.set_position(SYMBOL, 0.01, PRICE);
    feed.minute(rig.bar(4));
    let (live, ended) = run.wait();
    assert!(matches!(ended, Err(LiveError::Faulted(_))), "{ended:?}");
    drop(live);
    assert!(matches!(
        rig.saved_status("fault").state,
        State::Faulted { .. }
    ));

    let at = |t: Millis| td_data::time::format_minute(t);
    let (code, text) = rig.td_live(&feed, &["compare", "--id", "fault"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains(&format!(
            "3 live bars from {} to {}",
            at(rig.bar(0)),
            at(rig.bar(2))
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "matched fills: 1\n  buy 0.004, decided on the bar opening at {}: session 65000 at \
             {}, backtest 65000 at {}; time difference 0 ms, price difference 0",
            at(rig.bar(0)),
            at(rig.bar(1)),
            at(rig.bar(1)),
        )),
        "{text}"
    );
}

/// The report counts the funding rates the session was charged, and how many of them on a later
/// bar than the one they were due in, which the backtest charges on that earlier bar. A rate fed
/// with a bar's `sending` line and its own line is one rate.
#[test]
fn compare_counts_the_funding_the_session_was_charged_late() {
    let rig = Rig::new();
    eight_hourly(&rig);
    let feed = rig.feed();
    feed.minute(rig.bar(2));
    let run = rig.run(
        rig.create("funded", config(1, 99, false)).expect("creates"),
        &feed,
    );
    wait_for_bar(&run, rig.bar(1));
    let (live, ended) = run.stop();
    ended.expect("stopped cleanly");
    assert_eq!(live.funding().len(), 1);
    drop(live);
    let log = rig.log("funded");
    let with_rate = |l: &&serde_json::Value| l["funding"].as_array().is_some_and(|f| !f.is_empty());
    assert_eq!(
        log.iter().filter(with_rate).count(),
        2,
        "a sending line and its bar's"
    );

    let charged = |late: usize| {
        format!(
            "funding rates the session was charged: 1, {late} of them on a later bar than the \
             one they were due in. The backtest is given each rate at its due time, so each of \
             those is applied on another bar there. That can change funding, equity and the \
             result without any difference in how orders were filled; how much depends on the \
             position and the price when each rate is applied."
        )
    };
    let (code, text) = rig.td_live(&feed, &["compare", "--id", "funded"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains(&charged(0)), "{text}");

    // As if the venue had published the rate due at bar 0 only after bar 0 closed.
    let late: Vec<serde_json::Value> = log
        .into_iter()
        .map(|mut l| {
            if let Some(rates) = l.get_mut("funding").and_then(|f| f.as_array_mut()) {
                for rate in rates {
                    rate["due"] = rig.bar(0).into();
                }
            }
            l
        })
        .collect();
    write_log(&rig, "funded", &late);
    let (code, text) = rig.td_live(&feed, &["compare", "--id", "funded"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains(&charged(1)), "{text}");
}

/// The strategy name and symbol `compare` prints come from config.json, and reach the terminal
/// without control characters.
#[test]
fn compare_prints_the_saved_config_without_control_characters() {
    let rig = Rig::new();
    let feed = rig.feed();
    feed.minute(rig.bar(1));
    let run = rig.run(
        rig.create("marked", config(99, 99, false))
            .expect("creates"),
        &feed,
    );
    wait_for_bar(&run, rig.bar(0));
    run.stop().1.expect("stopped cleanly");
    // The double orders nothing here, so the replay never meets the symbol it orders on.
    let mut config = rig.config_json("marked");
    config["instrument"]["id"]["symbol"] = "BTC\u{1b}[2JUSDT".into();
    rig.write_config_json("marked", &config);
    let (code, text) = rig.td_live(&feed, &["compare", "--id", "marked"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "{text:?}"
    );
    assert!(
        text.contains(" with a backtest of double on bybit:BTC [2JUSDT:perpetual 1h "),
        "{text}"
    );
}
