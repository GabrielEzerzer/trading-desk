//! Paper sessions: the same results as a backtest, resume from the log, torn lines, and a live run
//! against a scripted WebSocket server on 127.0.0.1 with a fake REST source. No network.

use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::SinkExt;
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, Timeframe, Venue,
};
use td_data::live::Rest;
use td_data::{DataError, resample};
use td_engine::{
    LIQUIDATION_TAG, Order, OrderStatus, ParamValue, Params, Session, SessionConfig,
    build_strategy, run_backtest,
};
use td_server::{Funding, PaperConfig, PaperError, PaperHandle, PaperSession, Status, paper_dir};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const MIN: Millis = 60_000;
const HOUR: Millis = 60 * MIN;
/// 2024-01-01 00:00 UTC.
const T0: Millis = 1_704_067_200_000;

fn instrument(kind: InstrumentKind) -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Binance,
            symbol: "TESTUSDT".into(),
            kind,
        },
        base: "TEST".into(),
        quote: "USDT".into(),
        tick_size: 0.01,
        lot_size: 0.001,
        min_qty: 0.001,
        min_notional: 1.0,
        fees: FeeSchedule {
            maker: 0.001,
            taker: 0.002,
        },
    }
}

fn config(kind: InstrumentKind, timeframe: Timeframe) -> PaperConfig {
    PaperConfig {
        strategy: "sma_cross".into(),
        params: Params::new()
            .with("fast", ParamValue::Int(2))
            .with("slow", ParamValue::Int(5))
            .with("allow_short", ParamValue::Bool(true)),
        instrument: instrument(kind),
        timeframe,
        starting_cash: 10_000.0,
        slippage_bps: 5.0,
        fees: None,
        max_leverage: 1.0,
    }
}

/// The minute `i` minutes after `T0`, on a price wave with a 30-minute period.
fn minute(i: i64) -> Candle {
    let price = |i: i64| 100.0 + 5.0 * (i as f64 * std::f64::consts::TAU / 30.0).sin();
    let (open, close) = (price(i - 1), price(i));
    Candle {
        open_time: T0 + i * MIN,
        open,
        high: open.max(close) + 0.5,
        low: open.min(close) - 0.5,
        close,
        volume: 2.0,
        quote_volume: 2.0 * close,
    }
}

fn minutes(range: std::ops::Range<i64>) -> Vec<Candle> {
    range.map(minute).collect()
}

/// Feeds `bars` to a paper session as live bars, with the funding that falls in each.
fn feed_all(paper: &mut PaperSession, bars: &[Candle], funding: &[(Millis, f64)]) {
    let tf = paper.config().timeframe.duration_ms();
    for bar in bars {
        let in_bar: Vec<Funding> = funding
            .iter()
            .filter(|(t, _)| (bar.open_time..bar.open_time + tf).contains(t))
            .map(|&(time, rate)| Funding {
                time,
                rate,
                due: time,
            })
            .collect();
        paper.feed(*bar, in_bar, false).expect("feeds");
    }
}

/// Asserts two sessions hold the same trading state.
fn assert_same_state(a: &PaperSession, b: &PaperSession) {
    assert_eq!(a.session().bars(), b.session().bars());
    assert_eq!(a.fills(), b.fills());
    assert_eq!(a.equity(), b.equity());
    assert_eq!(a.orders(), b.orders());
    assert_eq!(a.open_orders(), b.open_orders());
    assert_eq!(a.portfolio(), b.portfolio());
    assert_eq!(a.funding(), b.funding());
}

#[test]
fn a_paper_session_fed_the_backtest_bars_matches_the_backtest() {
    for kind in [InstrumentKind::Spot, InstrumentKind::Perpetual] {
        let bars = minutes(0..300);
        let funding: Vec<(Millis, f64)> = match kind {
            InstrumentKind::Spot => Vec::new(),
            InstrumentKind::Perpetual => (0..5).map(|h| (T0 + h * HOUR, 0.0001)).collect(),
        };
        let cfg = config(kind, Timeframe::M1);
        let backtest = run_backtest(
            build_strategy(&cfg.strategy, &cfg.params).expect("builds"),
            cfg.instrument.clone(),
            cfg.timeframe,
            &bars,
            &funding,
            SessionConfig {
                starting_cash: cfg.starting_cash,
                slippage_bps: cfg.slippage_bps,
                fees: cfg.fees,
                max_leverage: cfg.max_leverage,
            },
        )
        .expect("runs");
        let dir = tempfile::tempdir().expect("temp dir");
        let mut paper = PaperSession::create(dir.path(), "same", cfg).expect("creates");
        feed_all(&mut paper, &bars, &funding);

        assert!(backtest.fills().len() > 4, "{kind}: the strategy trades");
        assert_eq!(paper.fills(), backtest.fills(), "{kind}");
        assert_eq!(paper.equity(), backtest.equity(), "{kind}");
        assert_eq!(paper.orders(), backtest.orders(), "{kind}");
        assert_eq!(paper.portfolio(), backtest.portfolio(), "{kind}");
        if kind == InstrumentKind::Perpetual {
            assert!(paper.portfolio().funding_paid() != 0.0);
        }
    }
}

#[test]
fn reopening_replays_the_log_to_the_same_state() {
    let dir = tempfile::tempdir().expect("temp dir");
    let funding: Vec<(Millis, f64)> = (0..3).map(|h| (T0 + h * HOUR, -0.0002)).collect();
    let cfg = config(InstrumentKind::Perpetual, Timeframe::M1);
    let mut paper = PaperSession::create(dir.path(), "replay", cfg).expect("creates");
    for bar in minutes(-20..0) {
        paper.feed(bar, Vec::new(), true).expect("warm-up bar");
    }
    feed_all(&mut paper, &minutes(0..150), &funding);
    let reopened = PaperSession::open(dir.path(), "replay").expect("opens");
    assert_same_state(&paper, &reopened);
    assert_eq!(reopened.live_from(), Some(T0));
    // The warm-up bars are replayed as warm-up: history, but no equity points.
    assert_eq!(reopened.session().bars().len(), 170);
    assert_eq!(reopened.equity().len(), 150);
    assert_eq!(reopened.equity()[0].time, T0);
    assert!(reopened.warnings().is_empty());
    assert!(!reopened.fills().is_empty());
}

/// Falls half a point a minute, so SMA cross goes short at about 0.95x, then the minute at 20
/// wicks to 250, more than double the entry: the short is liquidated on it.
fn falling_with_a_wick() -> Vec<Candle> {
    let price = |i: i64| 100.0 - 0.5 * i as f64;
    (0..40)
        .map(|i| {
            let (open, close) = (price(i - 1), price(i));
            Candle {
                open_time: T0 + i * MIN,
                open,
                high: if i == 20 {
                    250.0
                } else {
                    open.max(close) + 0.25
                },
                low: open.min(close) - 0.25,
                close,
                volume: 2.0,
                quote_volume: 2.0 * close,
            }
        })
        .collect()
}

/// Runs `cfg` as a backtest and as a paper session on the same bars and funding, asserts they
/// match and that reopening the paper session replays to the same state, and returns the
/// backtest.
fn paper_matches_backtest(cfg: PaperConfig, bars: &[Candle], funding: &[(Millis, f64)]) -> Session {
    let backtest = run_backtest(
        build_strategy(&cfg.strategy, &cfg.params).expect("builds"),
        cfg.instrument.clone(),
        cfg.timeframe,
        bars,
        funding,
        SessionConfig {
            starting_cash: cfg.starting_cash,
            slippage_bps: cfg.slippage_bps,
            fees: cfg.fees,
            max_leverage: cfg.max_leverage,
        },
    )
    .expect("runs");
    let dir = tempfile::tempdir().expect("temp dir");
    let mut paper = PaperSession::create(dir.path(), "same", cfg.clone()).expect("creates");
    feed_all(&mut paper, bars, funding);
    assert_eq!(paper.fills(), backtest.fills());
    assert_eq!(paper.equity(), backtest.equity());
    assert_eq!(paper.orders(), backtest.orders());
    assert_eq!(paper.portfolio(), backtest.portfolio());
    let reopened = PaperSession::open(dir.path(), "same").expect("opens");
    assert_eq!(reopened.config(), &cfg);
    assert_same_state(&paper, &reopened);
    backtest
}

#[test]
fn a_paper_session_matches_the_backtest_through_a_liquidation() {
    let bars = falling_with_a_wick();
    let funding = [(T0 + 10 * MIN, 0.0001), (T0 + 20 * MIN + 30_000, 0.0001)];
    let cfg = config(InstrumentKind::Perpetual, Timeframe::M1);
    let backtest = paper_matches_backtest(cfg, &bars, &funding);
    let liquidations: Vec<&Order> = backtest
        .orders()
        .iter()
        .filter(|o| o.request.client_tag.as_deref() == Some(LIQUIDATION_TAG))
        .collect();
    assert_eq!(liquidations.len(), 1, "{:?}", backtest.orders());
    let fill = backtest
        .fills()
        .iter()
        .find(|f| f.order_id == liquidations[0].id)
        .expect("the liquidation filled");
    assert_eq!(fill.time, T0 + 20 * MIN);
    assert!(fill.price > 150.0 && fill.price < 250.0, "{}", fill.price);
    assert!(backtest.equity().iter().all(|p| p.equity >= 0.0));
}

#[test]
fn a_paper_session_carries_its_leverage_cap() {
    // At 0.5x the strategy's 0.95x entries are rejected in both, for the same reason.
    let cfg = PaperConfig {
        max_leverage: 0.5,
        ..config(InstrumentKind::Perpetual, Timeframe::M1)
    };
    let backtest = paper_matches_backtest(cfg.clone(), &minutes(0..60), &[]);
    assert!(backtest.orders().iter().any(
        |o| matches!(&o.status, OrderStatus::Rejected(reason) if reason.contains("leverage cap"))
    ));
    // A config saved before the setting existed reads as 1x.
    let mut json = serde_json::to_value(&cfg).expect("serializes");
    json.as_object_mut()
        .expect("an object")
        .remove("max_leverage");
    let old: PaperConfig = serde_json::from_value(json).expect("reads");
    assert_eq!(old.max_leverage, 1.0);
}

#[test]
fn a_torn_last_line_is_ignored_with_a_warning_and_cut_from_the_log() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = config(InstrumentKind::Spot, Timeframe::M1);
    let mut paper = PaperSession::create(dir.path(), "torn", cfg).expect("creates");
    feed_all(&mut paper, &minutes(0..3), &[]);
    drop(paper);
    let log = paper_dir(dir.path(), "torn").join("events.jsonl");
    let mut bytes = fs::read(&log).expect("reads");
    let whole = bytes.len();
    bytes.extend_from_slice(br#"{"bar":{"open_time":17040673"#);
    fs::write(&log, &bytes).expect("writes");

    let mut reopened = PaperSession::open(dir.path(), "torn").expect("opens");
    assert_eq!(reopened.session().bars(), minutes(0..3));
    assert_eq!(reopened.warnings().len(), 1, "{:?}", reopened.warnings());
    assert_eq!(
        fs::read(&log).expect("reads").len(),
        whole,
        "the torn line is cut"
    );
    // Lines appended after it read back cleanly.
    feed_all(&mut reopened, &minutes(3..5), &[]);
    drop(reopened);
    let again = PaperSession::open(dir.path(), "torn").expect("opens");
    assert_eq!(again.session().bars(), minutes(0..5));
    assert!(again.warnings().is_empty());
}

#[test]
fn a_bad_line_inside_the_log_is_an_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = config(InstrumentKind::Spot, Timeframe::M1);
    let mut paper = PaperSession::create(dir.path(), "bad", cfg).expect("creates");
    feed_all(&mut paper, &minutes(0..2), &[]);
    drop(paper);
    let log = paper_dir(dir.path(), "bad").join("events.jsonl");
    let text = fs::read_to_string(&log).expect("reads");
    fs::write(&log, format!("not json\n{text}")).expect("writes");
    assert!(matches!(
        PaperSession::open(dir.path(), "bad"),
        Err(PaperError::BadFile { .. })
    ));
}

#[test]
fn ids_are_checked_and_a_session_is_never_overwritten() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = config(InstrumentKind::Spot, Timeframe::M1);
    for bad in ["", "../up", "a b", &"x".repeat(65)] {
        assert!(matches!(
            PaperSession::create(dir.path(), bad, cfg.clone()),
            Err(PaperError::InvalidId(_))
        ));
    }
    PaperSession::create(dir.path(), "once", cfg.clone()).expect("creates");
    assert!(matches!(
        PaperSession::create(dir.path(), "once", cfg.clone()),
        Err(PaperError::Exists(_))
    ));
    let unknown = PaperConfig {
        strategy: "nope".into(),
        ..cfg
    };
    assert!(PaperSession::create(dir.path(), "other", unknown).is_err());
    assert!(!paper_dir(dir.path(), "other").exists());
}

/// Serves REST data from lists, as of `now`.
struct FakeRest {
    minutes: Vec<Candle>,
    /// Bars of the session's timeframe, for the warm-up.
    history: Vec<Candle>,
    funding: Vec<(Millis, f64)>,
    now: Millis,
    /// How many more requests for minutes fail.
    failures: Mutex<u32>,
    calls: Mutex<Vec<(Timeframe, Millis, Millis)>>,
}

impl FakeRest {
    fn new(minutes: Vec<Candle>, history: Vec<Candle>, now: Millis) -> Self {
        FakeRest {
            minutes,
            history,
            funding: Vec::new(),
            now,
            failures: Mutex::new(0),
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl Rest for FakeRest {
    fn klines(
        &self,
        _id: &InstrumentId,
        timeframe: Timeframe,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<Candle>, DataError> {
        self.calls.lock().expect("lock").push((timeframe, from, to));
        let source = if timeframe == Timeframe::M1 {
            let mut failures = self.failures.lock().expect("lock");
            if *failures > 0 {
                *failures -= 1;
                return Err(DataError::Api {
                    url: "fake".into(),
                    message: "unavailable".into(),
                });
            }
            &self.minutes
        } else {
            &self.history
        };
        Ok(source
            .iter()
            .copied()
            .filter(|c| (from..to).contains(&c.open_time))
            .collect())
    }

    fn funding(
        &self,
        _id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        Ok(self
            .funding
            .iter()
            .copied()
            .filter(|(t, _)| (from..to).contains(t))
            .collect())
    }

    fn now(&self) -> Millis {
        self.now
    }
}

/// A source whose clock panics, to make the run loop panic.
struct PanickingRest;

impl Rest for PanickingRest {
    fn klines(
        &self,
        _id: &InstrumentId,
        _timeframe: Timeframe,
        _from: Millis,
        _to: Millis,
    ) -> Result<Vec<Candle>, DataError> {
        Ok(Vec::new())
    }

    fn funding(
        &self,
        _id: &InstrumentId,
        _from: Millis,
        _to: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        Ok(Vec::new())
    }

    fn now(&self) -> Millis {
        panic!("the clock broke");
    }
}

/// A source whose clock blocks for a while and then panics, so that a stop can arrive while the
/// run loop is about to panic and cannot be aborted.
struct SlowPanickingRest;

impl Rest for SlowPanickingRest {
    fn klines(
        &self,
        _id: &InstrumentId,
        _timeframe: Timeframe,
        _from: Millis,
        _to: Millis,
    ) -> Result<Vec<Candle>, DataError> {
        Ok(Vec::new())
    }

    fn funding(
        &self,
        _id: &InstrumentId,
        _from: Millis,
        _to: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        Ok(Vec::new())
    }

    fn now(&self) -> Millis {
        std::thread::sleep(Duration::from_millis(500));
        panic!("the clock broke late");
    }
}

/// A closed Binance kline frame for a minute.
fn frame(c: &Candle) -> String {
    format!(
        r#"{{"e":"kline","E":1,"s":"TESTUSDT","k":{{"t":{},"i":"1m","o":"{}","h":"{}","l":"{}","c":"{}","v":"{}","q":"{}","x":true}}}}"#,
        c.open_time, c.open, c.high, c.low, c.close, c.volume, c.quote_volume
    )
}

/// Serves `frames` on the first connection and holds it open; later connections are held silent.
async fn serve(frames: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}/", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let mut frames = Some(frames);
        let mut held = Vec::new();
        while let Ok((tcp, _)) = listener.accept().await {
            let Ok(mut socket) = tokio_tungstenite::accept_async(tcp).await else {
                continue;
            };
            for f in frames.take().unwrap_or_default() {
                if socket.send(Message::text(f)).await.is_err() {
                    break;
                }
            }
            held.push(socket);
        }
    });
    url
}

/// Waits until the session's last bar opens at `open_time`, or fails after 20 s.
async fn wait_for_bar(handle: &PaperHandle, open_time: Millis) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        {
            let s = handle.session().lock().expect("lock");
            if let Status::Error(e) = s.status() {
                panic!("session failed: {e}");
            }
            if s.last_bar().is_some_and(|b| b.open_time >= open_time) {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no bar at {open_time} within 20 s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn unwrap_session(shared: Arc<Mutex<PaperSession>>) -> PaperSession {
    match Arc::try_unwrap(shared) {
        Ok(mutex) => mutex.into_inner().expect("not poisoned"),
        Err(_) => panic!("the run loop still holds the session"),
    }
}

/// Runs a session live, stops it across the funding time at 02:00, resumes it, and checks it ends
/// in the same state as a session that saw every bar in one go.
async fn live_session_stops_and_resumes(kind: InstrumentKind) {
    let tf = Timeframe::M5;
    let cfg = config(kind, tf);
    let history = resample(&minutes(-60..0), tf, false).candles;
    // A rate every hour, the ones before the session only telling it the interval.
    let funding: Vec<(Millis, f64)> = match kind {
        InstrumentKind::Spot => Vec::new(),
        InstrumentKind::Perpetual => (-3..=3)
            .map(|h| (T0 + h * HOUR, 0.0001 * (1 + h.rem_euclid(3)) as f64))
            .collect(),
    };
    let rest = Arc::new(FakeRest {
        funding: funding.clone(),
        ..FakeRest::new(minutes(0..180), history.clone(), T0 + 30_000)
    });
    let dir = tempfile::tempdir().expect("temp dir");

    // First run: the stream starts at minute 2 and skips minute 7; REST fills 0, 1 and 7.
    let first: Vec<String> = minutes(2..70)
        .iter()
        .filter(|m| m.open_time != T0 + 7 * MIN)
        .map(frame)
        .collect();
    let paper = PaperSession::create(dir.path(), "live", cfg.clone()).expect("creates");
    let handle = td_server::start_with_stream(paper, rest.clone(), serve(first).await);
    wait_for_bar(&handle, T0 + 65 * MIN).await;
    assert_eq!(
        *handle.session().lock().expect("lock").status(),
        Status::Live
    );
    let stopped = unwrap_session(handle.stop().await);
    assert_eq!(*stopped.status(), Status::Stopped);
    assert_eq!(stopped.session().bars().len(), history.len() + 14);

    // The log replays to the state the session stopped in.
    let reopened = PaperSession::open(dir.path(), "live").expect("opens");
    assert_same_state(&stopped, &reopened);
    let log =
        fs::read_to_string(paper_dir(dir.path(), "live").join("events.jsonl")).expect("reads");
    assert_eq!(
        log.lines()
            .filter(|l| l.contains(r#""warm_up":true"#))
            .count(),
        history.len()
    );

    // Second run, after minutes 70 to 149 went by while stopped, the funding time at 02:00
    // among them: REST backfills them.
    let second: Vec<String> = minutes(150..180).iter().map(frame).collect();
    let handle = td_server::start_with_stream(reopened, rest.clone(), serve(second).await);
    wait_for_bar(&handle, T0 + 175 * MIN).await;
    let resumed = unwrap_session(handle.stop().await);

    // The same as a session that saw every bar in one go.
    let ref_dir = tempfile::tempdir().expect("temp dir");
    let mut reference = PaperSession::create(ref_dir.path(), "ref", cfg).expect("creates");
    for bar in &history {
        reference.feed(*bar, Vec::new(), true).expect("warm-up bar");
    }
    feed_all(
        &mut reference,
        &resample(&minutes(0..180), tf, false).candles,
        &funding,
    );
    assert!(!reference.fills().is_empty(), "the strategy trades live");
    assert_same_state(&resumed, &reference);
    assert_eq!(
        resumed.portfolio().funding_paid(),
        reference.portfolio().funding_paid()
    );
    if kind == InstrumentKind::Perpetual {
        let due: Vec<Millis> = resumed.funding().iter().map(|f| f.due).collect();
        assert_eq!(due, [T0, T0 + HOUR, T0 + 2 * HOUR]);
        assert!(reference.portfolio().funding_paid() != 0.0);
    }

    let calls = rest.calls.lock().expect("lock").clone();
    assert_eq!(
        calls,
        [
            (tf, T0 - 500 * tf.duration_ms(), T0),
            (Timeframe::M1, T0, T0 + 2 * MIN),
            (Timeframe::M1, T0 + 7 * MIN, T0 + 8 * MIN),
            (Timeframe::M1, T0 + 70 * MIN, T0 + 150 * MIN),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_spot_session_warms_up_trades_stops_and_resumes_with_the_missed_minutes_backfilled()
{
    live_session_stops_and_resumes(InstrumentKind::Spot).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_perpetual_session_resumed_across_a_funding_time_charges_each_rate_once() {
    live_session_stops_and_resumes(InstrumentKind::Perpetual).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_crashed_during_warm_up_finishes_it_and_goes_live_from_the_restart() {
    let tf = Timeframe::M5;
    let cfg = config(InstrumentKind::Spot, tf);
    let history = resample(&minutes(-60..20), tf, false).candles;
    let dir = tempfile::tempdir().expect("temp dir");
    // The process died after writing five of the warm-up lines.
    let mut paper = PaperSession::create(dir.path(), "crash", cfg.clone()).expect("creates");
    for bar in &history[..5] {
        paper.feed(*bar, Vec::new(), true).expect("warm-up bar");
    }
    drop(paper);

    // It restarts at 00:20:30, with every minute since well before the crash on REST.
    let rest = Arc::new(FakeRest::new(
        minutes(-60..60),
        history.clone(),
        T0 + 20 * MIN + 30_000,
    ));
    let reopened = PaperSession::open(dir.path(), "crash").expect("opens");
    assert_eq!(reopened.live_from(), None);
    let stream: Vec<String> = minutes(20..40).iter().map(frame).collect();
    let handle = td_server::start_with_stream(reopened, rest.clone(), serve(stream).await);
    wait_for_bar(&handle, T0 + 35 * MIN).await;
    let resumed = unwrap_session(handle.stop().await);

    // None of the bars before the restart were traded: the first live bar is the one after it.
    assert_eq!(resumed.live_from(), Some(T0 + 20 * MIN));
    assert_eq!(resumed.equity()[0].time, T0 + 20 * MIN);
    let ref_dir = tempfile::tempdir().expect("temp dir");
    let mut reference = PaperSession::create(ref_dir.path(), "ref", cfg).expect("creates");
    for bar in &history {
        reference.feed(*bar, Vec::new(), true).expect("warm-up bar");
    }
    feed_all(
        &mut reference,
        &resample(&minutes(20..40), tf, false).candles,
        &[],
    );
    assert_same_state(&resumed, &reference);
    // The rest of the warm-up came from where the log ended; nothing was backfilled as live.
    let calls = rest.calls.lock().expect("lock").clone();
    assert_eq!(calls, [(tf, history[5].open_time, T0 + 20 * MIN)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_warm_up_restarted_long_after_it_crashed_is_fed_one_warm_up_of_bars_at_most() {
    let tf = Timeframe::M5;
    let cfg = config(InstrumentKind::Spot, tf);
    const DAY: i64 = 1440;
    let history = resample(&minutes(-60..30 * DAY + 20), tf, false).candles;
    let dir = tempfile::tempdir().expect("temp dir");
    // The process died after writing five of the warm-up lines...
    let mut paper = PaperSession::create(dir.path(), "late", cfg).expect("creates");
    for bar in &history[..5] {
        paper.feed(*bar, Vec::new(), true).expect("warm-up bar");
    }
    drop(paper);

    // ...and restarts 30 days later, at 00:20:30.
    let restart = T0 + (30 * DAY + 20) * MIN;
    let rest = Arc::new(FakeRest::new(
        minutes(30 * DAY..30 * DAY + 60),
        history.clone(),
        restart + 30_000,
    ));
    let reopened = PaperSession::open(dir.path(), "late").expect("opens");
    let stream: Vec<String> = minutes(30 * DAY + 20..30 * DAY + 40)
        .iter()
        .map(frame)
        .collect();
    let handle = td_server::start_with_stream(reopened, rest.clone(), serve(stream).await);
    wait_for_bar(&handle, restart + 15 * MIN).await;
    let resumed = unwrap_session(handle.stop().await);

    // Only the last WARM_UP_BARS bars before the restart were asked for and fed.
    let earliest = restart - td_server::WARM_UP_BARS * tf.duration_ms();
    let calls = rest.calls.lock().expect("lock").clone();
    assert_eq!(calls, [(tf, earliest, restart)]);
    let log =
        fs::read_to_string(paper_dir(dir.path(), "late").join("events.jsonl")).expect("reads");
    let warm_up_lines = log
        .lines()
        .filter(|l| l.contains(r#""warm_up":true"#))
        .count();
    assert_eq!(warm_up_lines, 5 + td_server::WARM_UP_BARS as usize);
    // It went live from the restart and traded the bars after it.
    assert_eq!(resumed.live_from(), Some(restart));
    assert_eq!(resumed.equity()[0].time, restart);
    let first_after_gap = resumed
        .session()
        .bars()
        .iter()
        .find(|b| b.open_time > history[4].open_time)
        .expect("bars after the crash");
    assert_eq!(first_after_gap.open_time, earliest);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_is_live_again_once_a_failed_backfill_succeeds() {
    let tf = Timeframe::M5;
    let history = resample(&minutes(-60..0), tf, false).candles;
    // The backfill of minutes 0 and 1 fails once and succeeds on its retry.
    let rest = Arc::new(FakeRest {
        failures: Mutex::new(1),
        ..FakeRest::new(minutes(0..10), history, T0 + 30_000)
    });
    let dir = tempfile::tempdir().expect("temp dir");
    let paper = PaperSession::create(dir.path(), "retry", config(InstrumentKind::Spot, tf))
        .expect("creates");
    let stream: Vec<String> = minutes(2..10).iter().map(frame).collect();
    let handle = td_server::start_with_stream(paper, rest, serve(stream).await);
    wait_for_bar(&handle, T0 + 5 * MIN).await;
    {
        let s = handle.session().lock().expect("lock");
        assert!(
            s.warnings().iter().any(|w| w.contains("backfill failed")),
            "{:?}",
            s.warnings()
        );
        assert_eq!(*s.status(), Status::Live);
    }
    handle.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panic_in_the_run_loop_shows_as_an_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paper = PaperSession::create(
        dir.path(),
        "panic",
        config(InstrumentKind::Spot, Timeframe::M5),
    )
    .expect("creates");
    let handle =
        td_server::start_with_stream(paper, Arc::new(PanickingRest), serve(Vec::new()).await);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = handle.session().lock().expect("lock").status().clone();
        if let Status::Error(message) = status {
            assert!(message.contains("the clock broke"), "{message}");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "still {status:?} after the loop panicked"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panic_that_races_a_stop_still_shows_as_an_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paper = PaperSession::create(
        dir.path(),
        "race",
        config(InstrumentKind::Spot, Timeframe::M5),
    )
    .expect("creates");
    let handle =
        td_server::start_with_stream(paper, Arc::new(SlowPanickingRest), serve(Vec::new()).await);
    // Stop while the loop is blocked in the clock, before it panics.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stopped = handle.stop().await;
    let status = stopped.lock().expect("lock").status().clone();
    match status {
        Status::Error(message) => assert!(message.contains("the clock broke late"), "{message}"),
        other => panic!("a panic during the stop shows as {other:?}"),
    }
}
