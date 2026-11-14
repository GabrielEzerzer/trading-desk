//! A `LiveSession` against the mock venue on 127.0.0.1, fed closed minutes by a scripted stream on
//! 127.0.0.1 and a fake REST source. The strategy is a test double defined here.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::SinkExt;
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, OrderRequest, OrderType,
    Side, TimeInForce, Timeframe, Venue,
};
use td_data::DataError;
use td_data::live::{Rest, SharedStream};
use td_engine::{Context, EngineError, ParamValue, Params, Strategy};
use td_live::{
    LiveConfig, LiveError, LiveRun, LiveSession, LiveStatus, State, VenueHandle, live_dir,
};
use td_mock_bybit::{Fault, MockVenue, TEST_KEY, TEST_SECRET};
use td_orders::{Client, DemoKeys, Endpoint, Timing};
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
/// `close_on` it cancels its open orders and sells its position. `eager` instead submits a buy on
/// every bar, warm-up bars included, while it is flat with no open orders.
struct Double {
    buy_on: i64,
    close_on: i64,
    stop: f64,
    eager: bool,
    bar: i64,
}

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

/// Minutes from `start` on, [`WARM_UP`] hourly bars before it, no funding, and a clock fixed at
/// `start`.
struct FakeRest {
    start: Millis,
    warm_up_failures: Mutex<u32>,
    warm_up_reads: AtomicU32,
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

/// Pins a gap in the broker: an entry adopted at go_live does not adopt the stop the dead process
/// placed for it, so the reconcile finds an order it does not know and the session halts on a
/// fault, sending and cancelling nothing.
#[test]
fn an_entry_with_a_stop_sent_before_its_line_was_written_halts_on_resume() {
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
    drop_last_line(&rig.path("stopped", "events.jsonl"));

    let sends = rig.sends();
    let live = rig.open("stopped").expect("reopens");
    let feed = rig.feed();
    let (live, ended) = rig.run(live, &feed).wait();
    assert!(
        matches!(&ended, Err(LiveError::Faulted(f)) if f.starts_with("unknown open order")),
        "{ended:?}"
    );
    assert_eq!(rig.sends(), sends);
    assert!(matches!(live.status().state, State::Faulted { .. }));
    assert!(!feed.opened.load(Ordering::SeqCst), "no bar was fed");
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
