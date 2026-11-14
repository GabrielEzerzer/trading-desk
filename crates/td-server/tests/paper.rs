//! Paper sessions: the same results as a backtest, resume from the log, torn lines, and a live run
//! against a scripted WebSocket server on 127.0.0.1 with a fake REST source. No network.

use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::SinkExt;
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, OrderId, OrderRequest,
    OrderType, Side, TimeInForce, Timeframe, Venue,
};
use td_data::live::Rest;
use td_data::{DataError, resample};
use td_engine::{
    BacktestConfig, Context, ENGINE_REVISION, LIQUIDATION_TAG, Order, OrderStatus, ParamValue,
    Params, Session, SessionConfig, Strategy, build_strategy, run_backtest, run_strategy,
};
use td_server::{
    FedBar, Funding, MANUAL_STRATEGY, ManualError, ManualOrder, ManualOrderType, OrderSource,
    PaperConfig, PaperError, PaperHandle, PaperSession, SessionEvent, Status, TradeSource,
    paper_dir,
};
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
            per_unit: 0.0,
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
        let funding: Vec<(Millis, f64)> = if kind.rules().funding {
            (0..5).map(|h| (T0 + h * HOUR, 0.0001)).collect()
        } else {
            Vec::new()
        };
        let cfg = config(kind, Timeframe::M1);
        let backtest = run_backtest(
            build_strategy(&cfg.strategy, &cfg.params).expect("builds"),
            cfg.instrument.clone(),
            cfg.timeframe,
            &bars,
            &[],
            &funding,
            SessionConfig {
                starting_cash: cfg.starting_cash,
                slippage_bps: cfg.slippage_bps,
                fees: cfg.fees,
                max_leverage: cfg.max_leverage,
                account_currency: td_core::AccountCurrency::Quote,
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
        &[],
        funding,
        SessionConfig {
            starting_cash: cfg.starting_cash,
            slippage_bps: cfg.slippage_bps,
            fees: cfg.fees,
            max_leverage: cfg.max_leverage,
            account_currency: td_core::AccountCurrency::Quote,
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

/// Rewrites a saved session's config as an older engine would have saved it: with `revision`
/// recorded, or with none, as before the revision was recorded.
fn save_as_engine(dir: &std::path::Path, id: &str, revision: Option<u32>) {
    let path = paper_dir(dir, id).join("config.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("reads")).expect("parses");
    let object = json.as_object_mut().expect("an object");
    match revision {
        Some(r) => object.insert("engine_revision".into(), r.into()),
        None => object.remove("engine_revision"),
    };
    fs::write(&path, serde_json::to_vec(&json).expect("serializes")).expect("writes");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_made_by_another_engine_opens_with_a_warning_and_will_not_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = config(InstrumentKind::Spot, Timeframe::M1);

    // A session made now records this engine's revision and reopens without a warning.
    let mut paper = PaperSession::create(dir.path(), "now", cfg.clone()).expect("creates");
    feed_all(&mut paper, &minutes(0..3), &[]);
    drop(paper);
    let path = paper_dir(dir.path(), "now").join("config.json");
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("reads")).expect("parses");
    assert_eq!(saved["engine_revision"], ENGINE_REVISION);
    let now = PaperSession::open(dir.path(), "now").expect("opens");
    assert_eq!(now.engine_revision(), ENGINE_REVISION);
    assert!(now.warnings().is_empty(), "{:?}", now.warnings());
    now.check_resumable().expect("resumable");

    let older = ENGINE_REVISION - 1;
    for (id, recorded, read_as) in [("unrecorded", None, 1), ("older", Some(older), older)] {
        let mut paper = PaperSession::create(dir.path(), id, cfg.clone()).expect("creates");
        feed_all(&mut paper, &minutes(0..3), &[]);
        drop(paper);
        save_as_engine(dir.path(), id, recorded);

        // It still opens, replayed by this engine, with a warning saying so.
        let old = PaperSession::open(dir.path(), id).expect("still opens");
        assert_eq!(old.session().bars(), minutes(0..3));
        assert_eq!(old.engine_revision(), read_as);
        let expected = format!(
            "created with engine revision {read_as} and its numbers are replayed with engine \
             revision {ENGINE_REVISION}"
        );
        assert_eq!(old.warnings().len(), 1, "{:?}", old.warnings());
        assert!(
            old.warnings()[0].contains(&expected),
            "{:?}",
            old.warnings()
        );
        assert!(matches!(
            old.check_resumable(),
            Err(PaperError::EngineChanged { created, current })
                if created == read_as && current == ENGINE_REVISION
        ));

        // Started anyway, it ends at once with that error, before it asks the venue for anything:
        // this source's clock panics if it is asked.
        let handle =
            td_server::start_with_stream(old, Arc::new(PanickingRest), serve(Vec::new()).await);
        tokio::time::timeout(Duration::from_secs(10), handle.finished())
            .await
            .expect("ends at once");
        let status = handle.session().lock().expect("lock").status().clone();
        match status {
            Status::Error(message) => {
                assert!(message.contains("start a new session"), "{message}");
            }
            other => panic!("expected the start refused, got {other:?}"),
        }
        handle.stop().await;
    }
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
    /// How many more requests for warm-up bars fail.
    warm_up_failures: Mutex<u32>,
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
            warm_up_failures: Mutex::new(0),
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
        let failures = if timeframe == Timeframe::M1 {
            &self.failures
        } else {
            &self.warm_up_failures
        };
        let source = {
            let mut failures = failures.lock().expect("lock");
            if *failures > 0 {
                *failures -= 1;
                return Err(DataError::Api {
                    url: "fake".into(),
                    message: "unavailable".into(),
                });
            }
            if timeframe == Timeframe::M1 {
                &self.minutes
            } else {
                &self.history
            }
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
    let history = resample(InstrumentKind::Spot, &minutes(-60..0), tf, false).candles;
    // A rate every hour, the ones before the session only telling it the interval.
    let funding: Vec<(Millis, f64)> = if kind.rules().funding {
        (-3..=3)
            .map(|h| (T0 + h * HOUR, 0.0001 * (1 + h.rem_euclid(3)) as f64))
            .collect()
    } else {
        Vec::new()
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
    // Each warm-up line carries the venue's rates due in its bar, from the first warm-up bar's
    // open on, so the strategy sees them and a replay feeds them again: here the rate due at the
    // first warm-up bar's open, an hour before the session went live. They are not charged.
    let warm_up_funding: Vec<(Millis, Vec<Funding>)> = log
        .lines()
        .map(|l| serde_json::from_str::<FedBar>(l).expect("a bar line"))
        .filter(|fed| fed.warm_up && !fed.funding.is_empty())
        .map(|fed| (fed.bar.open_time, fed.funding))
        .collect();
    if kind == InstrumentKind::Perpetual {
        let due = T0 - HOUR;
        let rate = funding.iter().find(|f| f.0 == due).expect("a rate").1;
        assert_eq!(history[0].open_time, due);
        let fed = Funding {
            time: due,
            rate,
            due,
        };
        assert_eq!(warm_up_funding, [(due, vec![fed])]);
    } else {
        assert!(warm_up_funding.is_empty());
    }

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
        let funding = warm_up_funding
            .iter()
            .find(|(open, _)| *open == bar.open_time)
            .map_or_else(Vec::new, |(_, funding)| funding.clone());
        reference.feed(*bar, funding, true).expect("warm-up bar");
    }
    feed_all(
        &mut reference,
        &resample(InstrumentKind::Spot, &minutes(0..180), tf, false).candles,
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

/// A rate due after the last warm-up bar the venue gave, before the session went live, comes with
/// the first live bar, at its own time, so the strategy still sees it as a backtest shows it.
/// Nothing is held then, so it costs nothing, and the funding clock does not give it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_rate_due_after_the_last_warm_up_bar_comes_with_the_first_live_bar() {
    let tf = Timeframe::M5;
    // The venue's bars stop an hour before the session goes live at T0.
    let history = resample(InstrumentKind::Spot, &minutes(-120..-60), tf, false).candles;
    let (before, after, live) = ((T0 - 2 * HOUR, 0.0001), (T0 - HOUR, 0.0003), (T0, 0.0002));
    let rest = Arc::new(FakeRest {
        funding: vec![before, after, live],
        ..FakeRest::new(minutes(0..30), history, T0 + 30_000)
    });
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = config(InstrumentKind::Perpetual, tf);
    let paper = PaperSession::create(dir.path(), "gap", cfg).expect("creates");
    let stream: Vec<String> = minutes(0..20).iter().map(frame).collect();
    let handle = td_server::start_with_stream(paper, rest, serve(stream).await);
    wait_for_bar(&handle, T0 + 10 * MIN).await;
    let session = unwrap_session(handle.stop().await);
    assert_eq!(session.live_from(), Some(T0));
    let charged = |(due, rate): (Millis, f64)| Funding {
        time: due,
        rate,
        due,
    };
    assert_eq!(session.funding(), [charged(after), charged(live)]);
    assert_eq!(session.portfolio().funding_paid(), 0.0);
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

/// What a strategy saw in the paper session `id`: each line of its log fed to a session running
/// [`SeesFunding`] as the paper session fed it and as a reopen replays it, warm-up lines as warm-up
/// and each rate at its `time`.
fn seen_in_paper(dir: &std::path::Path, id: &str, cfg: &PaperConfig) -> Seen {
    let log = fs::read_to_string(paper_dir(dir, id).join("events.jsonl")).expect("reads");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let money = SessionConfig {
        starting_cash: cfg.starting_cash,
        slippage_bps: cfg.slippage_bps,
        fees: cfg.fees,
        max_leverage: cfg.max_leverage,
        account_currency: td_core::AccountCurrency::Quote,
    };
    let strategy = Box::new(SeesFunding(seen.clone()));
    let mut session =
        Session::new(strategy, cfg.instrument.clone(), cfg.timeframe, money).expect("session");
    for line in log.lines() {
        let fed: FedBar = serde_json::from_str(line).expect("a bar line");
        let funding: Vec<(Millis, f64)> = fed.funding.iter().map(|f| (f.time, f.rate)).collect();
        let fed = if fed.warm_up {
            session.warm_up(fed.bar, &funding)
        } else {
            session.on_bar(fed.bar, &funding)
        };
        fed.expect("feeds");
    }
    seen.lock().expect("lock").clone()
}

/// What a strategy sees in a backtest of `bars` given the venue's `funding`, warmed up on the bars
/// before `live_from` and trading from it.
fn seen_in_backtest(
    cfg: &PaperConfig,
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
        starting_cash: cfg.starting_cash,
        slippage_bps: cfg.slippage_bps,
        fee_override: cfg.fees,
        max_leverage: cfg.max_leverage,
        warmup_from: Some(bars[0].open_time),
        close_at_end: false,
        account_currency: td_core::AccountCurrency::Quote,
        references: Vec::new(),
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let strategy = Box::new(SeesFunding(seen.clone()));
    run_strategy(&config, strategy, bars, &[], funding).expect("runs");
    seen.lock().expect("lock").clone()
}

/// A paper session shows its strategy, on every bar, the funding a backtest of the same bars shows
/// it, times and rates: a rate due where warm-up bars are missing, and one due after the last
/// warm-up bar before the session went live, keep their settlement times.
#[tokio::test(flavor = "multi_thread")]
async fn a_paper_session_shows_its_strategy_the_funding_a_backtest_of_its_bars_shows() {
    let tf = Timeframe::M5;
    // The venue's bars run from three hours before the session goes live at T0 to an hour and a
    // half before it, without the two bars from 2h05m to 1h55m before it.
    let missing = [T0 - 2 * HOUR - 5 * MIN, T0 - 2 * HOUR];
    let history: Vec<Candle> = resample(InstrumentKind::Spot, &minutes(-180..-90), tf, false)
        .candles
        .into_iter()
        .filter(|b| !missing.contains(&b.open_time))
        .collect();
    // Before the first warm-up bar, so never seen; at its open; inside the missing bars; inside
    // the gap before the first live bar; at that bar's open.
    let funding = vec![
        (T0 - 4 * HOUR, 0.0005),
        (T0 - 3 * HOUR, 0.0001),
        (T0 - 2 * HOUR, -0.0002),
        (T0 - HOUR, 0.0003),
        (T0, 0.0004),
    ];
    let rest = Arc::new(FakeRest {
        funding: funding.clone(),
        ..FakeRest::new(minutes(0..30), history, T0 + 30_000)
    });
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = config(InstrumentKind::Perpetual, tf);
    let paper = PaperSession::create(dir.path(), "seen", cfg.clone()).expect("creates");
    let stream: Vec<String> = minutes(0..20).iter().map(frame).collect();
    let handle = td_server::start_with_stream(paper, rest, serve(stream).await);
    wait_for_bar(&handle, T0 + 10 * MIN).await;
    let session = unwrap_session(handle.stop().await);
    assert_eq!(session.live_from(), Some(T0));

    let in_paper = seen_in_paper(dir.path(), "seen", &cfg);
    let bars = session.session().bars();
    let opens: Vec<Millis> = in_paper.iter().map(|s| s.0).collect();
    assert_eq!(opens, bars.iter().map(|b| b.open_time).collect::<Vec<_>>());
    assert_eq!(in_paper, seen_in_backtest(&cfg, bars, T0, &funding));
    // On the first live bar: every rate from the first warm-up bar's open on, at its own time.
    let first_live = in_paper.iter().find(|s| !s.1).expect("a live bar");
    assert_eq!((first_live.0, &first_live.2[..]), (T0, &funding[1..]));
}

/// A session that stopped part-way through its warm-up, with funding in the warm-up lines it
/// wrote, finishes the warm-up from the end of its log: no rate is fed twice or left out, and its
/// strategy sees on every bar what it sees in a session that never stopped.
#[tokio::test(flavor = "multi_thread")]
async fn a_warm_up_resumed_from_a_log_with_funding_feeds_each_rate_once() {
    let tf = Timeframe::M5;
    let cfg = config(InstrumentKind::Perpetual, tf);
    // Three hours of warm-up bars before T0, without the two from 2h to 1h55m before it, just
    // after where the log will end.
    let missing = [T0 - 2 * HOUR, T0 - 2 * HOUR + 5 * MIN];
    let history: Vec<Candle> = resample(InstrumentKind::Spot, &minutes(-180..0), tf, false)
        .candles
        .into_iter()
        .filter(|b| !missing.contains(&b.open_time))
        .collect();
    let funding: Vec<(Millis, f64)> = (-4..=0)
        .map(|h| (T0 + h * HOUR, 0.0001 * (5 + h) as f64))
        .collect();
    let rest = || {
        Arc::new(FakeRest {
            funding: funding.clone(),
            ..FakeRest::new(minutes(0..30), history.clone(), T0 + 30_000)
        })
    };
    let run = |paper: PaperSession| {
        let rest = rest();
        async move {
            let stream: Vec<String> = minutes(0..20).iter().map(frame).collect();
            let handle = td_server::start_with_stream(paper, rest, serve(stream).await);
            wait_for_bar(&handle, T0 + 10 * MIN).await;
            unwrap_session(handle.stop().await)
        }
    };

    // A session that never stopped.
    let whole_dir = tempfile::tempdir().expect("temp dir");
    let whole = PaperSession::create(whole_dir.path(), "whole", cfg.clone()).expect("creates");
    run(whole).await;
    let whole_log = fs::read_to_string(paper_dir(whole_dir.path(), "whole").join("events.jsonl"))
        .expect("reads");

    // The same session, stopped after writing its first twelve warm-up lines, up to the bar at
    // 2h05m before T0: the first of them holds the rate due 3h before, and the rate due 2h before
    // falls at the end of the last.
    let dir = tempfile::tempdir().expect("temp dir");
    drop(PaperSession::create(dir.path(), "cut", cfg.clone()).expect("creates"));
    let cut: String = whole_log
        .lines()
        .take(12)
        .map(|l| format!("{l}\n"))
        .collect();
    let last: FedBar = serde_json::from_str(cut.lines().last().expect("lines")).expect("a line");
    assert_eq!(last.bar.open_time, T0 - 2 * HOUR - 5 * MIN);
    fs::write(paper_dir(dir.path(), "cut").join("events.jsonl"), cut).expect("writes");
    let reopened = PaperSession::open(dir.path(), "cut").expect("opens");
    assert_eq!(reopened.live_from(), None);
    let resumed = run(reopened).await;
    assert_eq!(resumed.live_from(), Some(T0));

    // Every rate from the first warm-up bar's open on was fed once, at its own time.
    let log = fs::read_to_string(paper_dir(dir.path(), "cut").join("events.jsonl")).expect("reads");
    let fed: Vec<(Millis, Millis)> = log
        .lines()
        .flat_map(|l| {
            serde_json::from_str::<FedBar>(l)
                .expect("a bar line")
                .funding
        })
        .map(|f| (f.time, f.due))
        .collect();
    let due: Vec<(Millis, Millis)> = funding[1..].iter().map(|&(t, _)| (t, t)).collect();
    assert_eq!(fed, due);

    // Bar for bar, the strategy saw what it saw in the session that never stopped.
    let until =
        |seen: Seen| -> Seen { seen.into_iter().filter(|s| s.0 <= T0 + 10 * MIN).collect() };
    let in_resumed = until(seen_in_paper(dir.path(), "cut", &cfg));
    let in_whole = until(seen_in_paper(whole_dir.path(), "whole", &cfg));
    assert_eq!(in_resumed.len(), history.len() + 3);
    assert_eq!(in_resumed, in_whole);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_crashed_during_warm_up_finishes_it_and_goes_live_from_the_restart() {
    let tf = Timeframe::M5;
    let cfg = config(InstrumentKind::Spot, tf);
    let history = resample(InstrumentKind::Spot, &minutes(-60..20), tf, false).candles;
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
        &resample(InstrumentKind::Spot, &minutes(20..40), tf, false).candles,
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
    let history = resample(
        InstrumentKind::Spot,
        &minutes(-60..30 * DAY + 20),
        tf,
        false,
    )
    .candles;
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
    let history = resample(InstrumentKind::Spot, &minutes(-60..0), tf, false).candles;
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
async fn a_failed_warm_up_request_is_retried_with_a_warning() {
    let tf = Timeframe::M5;
    let history = resample(InstrumentKind::Spot, &minutes(-60..0), tf, false).candles;
    let rest = Arc::new(FakeRest {
        warm_up_failures: Mutex::new(1),
        ..FakeRest::new(minutes(0..10), history.clone(), T0 + 30_000)
    });
    let dir = tempfile::tempdir().expect("temp dir");
    let paper = PaperSession::create(dir.path(), "warm", config(InstrumentKind::Spot, tf))
        .expect("creates");
    let stream: Vec<String> = minutes(0..10).iter().map(frame).collect();
    let handle = td_server::start_with_stream(paper, rest.clone(), serve(stream).await);
    wait_for_bar(&handle, T0 + 5 * MIN).await;
    let resumed = unwrap_session(handle.stop().await);
    assert!(
        resumed
            .warnings()
            .iter()
            .any(|w| w.starts_with("warm-up failed, retrying:")),
        "{:?}",
        resumed.warnings()
    );
    assert_eq!(resumed.live_from(), Some(T0));
    assert_eq!(resumed.session().bars().len(), history.len() + 2);
    let warm_up = (tf, T0 - 500 * tf.duration_ms(), T0);
    assert_eq!(rest.calls.lock().expect("lock")[..2], [warm_up, warm_up]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_bar_built_with_a_tolerated_minute_missing_is_noted_on_the_session() {
    // A four-hour bucket may miss two of its 240 minutes; REST has all but minute 7.
    let tf = Timeframe::H4;
    let all: Vec<Candle> = minutes(0..240)
        .into_iter()
        .filter(|m| m.open_time != T0 + 7 * MIN)
        .collect();
    let rest = Arc::new(FakeRest::new(all, Vec::new(), T0 + 30_000));
    let dir = tempfile::tempdir().expect("temp dir");
    let paper =
        PaperSession::create(dir.path(), "gap", config(InstrumentKind::Spot, tf)).expect("creates");
    // The stream's first closed minute is the next bucket's, so the whole bucket is backfilled.
    let handle = td_server::start_with_stream(paper, rest, serve(vec![frame(&minute(240))]).await);
    wait_for_bar(&handle, T0).await;
    // The note follows its bar.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while handle.session().lock().expect("lock").warnings().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no warning in 20 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let session = unwrap_session(handle.stop().await);
    assert_eq!(
        session.warnings(),
        [format!(
            "the bar at {T0} was built with 1 of its minutes missing"
        )]
    );
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

fn manual_config(kind: InstrumentKind) -> PaperConfig {
    PaperConfig {
        strategy: MANUAL_STRATEGY.into(),
        params: Params::new(),
        ..config(kind, Timeframe::M1)
    }
}

fn market(side: Side, qty: f64) -> ManualOrder {
    ManualOrder {
        side,
        qty,
        order_type: ManualOrderType::Market,
        limit_price: None,
        reduce_only: None,
    }
}

fn limit(side: Side, qty: f64, price: f64) -> ManualOrder {
    ManualOrder {
        order_type: ManualOrderType::Limit,
        limit_price: Some(price),
        ..market(side, qty)
    }
}

/// Something done by hand between bars.
#[derive(Clone)]
enum Hand {
    Place(ManualOrder),
    Cancel(u64),
}

/// What the manual tests do by hand, after the minute each is paired with closes: a market buy
/// with its quantity off the lot; a limit sell, off the tick, above every high until the next
/// peak; a limit buy that is never reached, then its cancel.
fn script() -> Vec<(i64, Hand)> {
    vec![
        (4, Hand::Place(market(Side::Buy, 10.0004))),
        (9, Hand::Place(limit(Side::Sell, 10.0, 105.299))),
        (20, Hand::Place(limit(Side::Buy, 10.0, 90.0))),
        (25, Hand::Cancel(3)),
    ]
}

/// The close of the minute `i` minutes after `T0`.
fn close_of(i: i64) -> Millis {
    T0 + (i + 1) * MIN
}

/// Does `hand`, placing an order as accepted at `at`.
fn by_hand(
    paper: &mut PaperSession,
    hand: &Hand,
    at: Millis,
) -> Result<td_server::PaperOrder, ManualError> {
    match hand {
        Hand::Place(order) => paper.place(order, at),
        Hand::Cancel(id) => paper.cancel(OrderId(*id)),
    }
}

/// Feeds the minutes in `live` as live bars, doing what `script` says after each.
fn feed_by_hand(paper: &mut PaperSession, live: std::ops::Range<i64>, script: &[(i64, Hand)]) {
    for i in live {
        paper.feed(minute(i), Vec::new(), false).expect("feeds");
        for (_, hand) in script.iter().filter(|(m, _)| *m == i) {
            by_hand(paper, hand, close_of(i)).expect("accepted");
        }
    }
}

/// A session warmed up on the 20 minutes before `T0`.
fn warmed_up(dir: &std::path::Path, id: &str, cfg: PaperConfig) -> PaperSession {
    let mut paper = PaperSession::create(dir, id, cfg).expect("creates");
    for bar in minutes(-20..0) {
        paper.feed(bar, Vec::new(), true).expect("warm-up bar");
    }
    paper
}

/// Submits and cancels from its bar callbacks what a script does by hand after those bars: the
/// reference for manual orders.
struct Scripted {
    instrument: InstrumentId,
    script: Vec<(i64, Hand)>,
}

impl Strategy for Scripted {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        for (minute, hand) in &self.script {
            if T0 + minute * MIN != bar.open_time {
                continue;
            }
            match hand {
                Hand::Place(order) => {
                    let _ = ctx.submit(OrderRequest {
                        instrument: self.instrument.clone(),
                        side: order.side,
                        qty: order.qty,
                        order_type: match order.limit_price {
                            Some(price) => OrderType::Limit { price },
                            None => OrderType::Market,
                        },
                        tif: TimeInForce::Gtc,
                        reduce_only: order.reduce_only.unwrap_or(false),
                        client_tag: None,
                    });
                }
                Hand::Cancel(id) => {
                    ctx.cancel(OrderId(*id));
                }
            }
        }
    }
}

/// Asserts two sessions hold the same trading state, with the same sources.
fn assert_same_marked_state(a: &PaperSession, b: &PaperSession) {
    assert_same_state(a, b);
    assert_eq!(a.fills_with_source(), b.fills_with_source());
    assert_eq!(a.open_orders_with_source(), b.open_orders_with_source());
    assert_eq!(a.trades(), b.trades());
}

#[test]
fn manual_orders_take_the_strategy_orders_path_and_fill_by_the_engines_rules() {
    for kind in [InstrumentKind::Spot, InstrumentKind::Perpetual] {
        let cfg = manual_config(kind);
        let dir = tempfile::tempdir().expect("temp dir");
        let mut paper = PaperSession::create(dir.path(), "hand", cfg.clone()).expect("creates");
        // Refused until the first live bar, warm-up bars included.
        let early = paper.place(&market(Side::Buy, 1.0), T0);
        assert!(matches!(early, Err(ManualError::NotLive)), "{early:?}");
        for bar in minutes(-20..0) {
            paper.feed(bar, Vec::new(), true).expect("warm-up bar");
        }
        let early = paper.place(&market(Side::Buy, 1.0), T0);
        assert!(matches!(early, Err(ManualError::NotLive)), "{early:?}");
        feed_by_hand(&mut paper, 0..60, &script());

        // A strategy submitting the same orders from the same bars' callbacks trades the same.
        let scripted = Scripted {
            instrument: cfg.instrument.id.clone(),
            script: script(),
        };
        let money = SessionConfig {
            starting_cash: cfg.starting_cash,
            slippage_bps: cfg.slippage_bps,
            fees: cfg.fees,
            max_leverage: cfg.max_leverage,
            account_currency: td_core::AccountCurrency::Quote,
        };
        let mut reference = Session::new(
            Box::new(scripted),
            cfg.instrument.clone(),
            cfg.timeframe,
            money,
        )
        .expect("session");
        for bar in minutes(-20..0) {
            reference.warm_up(bar, &[]).expect("warm-up bar");
        }
        for bar in minutes(0..60) {
            reference.on_bar(bar, &[]).expect("live bar");
        }
        assert_eq!(paper.fills(), reference.fills(), "{kind}");
        assert_eq!(paper.equity(), reference.equity(), "{kind}");
        assert_eq!(paper.portfolio(), reference.portfolio(), "{kind}");
        // The same orders, apart from who cancelled the limit buy.
        let mut expected = reference.orders().to_vec();
        assert_eq!(
            expected[2].status,
            OrderStatus::Cancelled("cancelled by the strategy".into())
        );
        expected[2].status = OrderStatus::Cancelled("cancelled manually".into());
        assert_eq!(paper.orders(), expected, "{kind}");

        // The market buy, rounded down to the lot, filled at the open of the next bar.
        let (orders, fills) = (paper.orders(), paper.fills());
        assert_eq!(fills.len(), 2, "{kind}: {fills:?}");
        assert_eq!(orders[0].request.qty, 10.0);
        assert_eq!(
            (fills[0].order_id, fills[0].time),
            (OrderId(1), T0 + 5 * MIN)
        );
        assert_eq!(fills[0].liquidity, td_core::Liquidity::Taker);
        // The limit sell, rounded up to the tick, rested until the bar at minute 37 reached it
        // and filled at its price.
        let OrderType::Limit { price } = orders[1].request.order_type else {
            panic!("a limit order");
        };
        assert!((price - 105.3).abs() < 1e-9, "{price}");
        assert_eq!(
            (fills[1].order_id, fills[1].time, fills[1].price),
            (OrderId(2), T0 + 37 * MIN, price)
        );
        assert_eq!(fills[1].liquidity, td_core::Liquidity::Maker);
        assert!(paper.open_orders().is_empty());
        assert_eq!(paper.portfolio().position(), 0.0);

        // Every order, fill and trade is marked manual.
        assert!(
            paper
                .fills_with_source()
                .iter()
                .all(|f| f.source == OrderSource::Manual)
        );
        let trades = paper.trades();
        assert_eq!(trades.len(), 1, "{kind}: {trades:?}");
        assert_eq!(trades[0].source, TradeSource::Manual);
        assert!(!trades[0].trade.open);
    }
}

#[test]
fn a_refused_manual_order_records_nothing_and_says_why() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut paper = PaperSession::create(
        dir.path(),
        "refused",
        manual_config(InstrumentKind::Perpetual),
    )
    .expect("creates");
    paper.feed(minute(0), Vec::new(), false).expect("live bar");
    let log = paper_dir(dir.path(), "refused").join("events.jsonl");
    let lines = || fs::read_to_string(&log).expect("reads").lines().count();
    let refused =
        |paper: &mut PaperSession, order: ManualOrder| match paper.place(&order, close_of(0)) {
            Err(ManualError::Rejected(reason)) => reason,
            other => panic!("expected a rejection, got {other:?}"),
        };

    // Rounded down to the lot it is zero.
    let reason = refused(&mut paper, market(Side::Buy, 0.0004));
    assert!(reason.contains("qty"), "{reason}");
    // At 1x and a close of 100, 101 TEST is worth more than the equity.
    let reason = refused(&mut paper, market(Side::Buy, 101.0));
    assert!(reason.contains("leverage cap"), "{reason}");
    let priced_market = ManualOrder {
        limit_price: Some(100.0),
        ..market(Side::Buy, 1.0)
    };
    assert!(refused(&mut paper, priced_market).contains("limit_price"));
    let unpriced_limit = ManualOrder {
        limit_price: None,
        ..limit(Side::Buy, 1.0, 1.0)
    };
    assert!(refused(&mut paper, unpriced_limit).contains("limit_price"));
    assert!(paper.orders().is_empty());
    assert_eq!(lines(), 1, "only the bar is in the log");

    // The next order accepted takes the first id.
    let placed = paper
        .place(&market(Side::Buy, 1.0), close_of(0))
        .expect("accepted");
    assert_eq!(
        (placed.order.id, placed.source),
        (OrderId(1), OrderSource::Manual)
    );
    assert_eq!(placed.order.status, OrderStatus::Open);
    assert_eq!(lines(), 2);

    // A manual session takes no parameters.
    let with_params = PaperConfig {
        params: Params::new().with("fast", ParamValue::Int(2)),
        ..manual_config(InstrumentKind::Perpetual)
    };
    assert!(PaperSession::create(dir.path(), "params", with_params).is_err());
}

#[test]
fn a_manual_order_is_refused_while_the_session_is_more_than_one_bar_behind() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut paper = PaperSession::create(
        dir.path(),
        "behind",
        manual_config(InstrumentKind::Perpetual),
    )
    .expect("creates");
    for i in 0..=10 {
        paper.feed(minute(i), Vec::new(), false).expect("live bar");
    }
    let log = paper_dir(dir.path(), "behind").join("events.jsonl");
    let lines = || fs::read_to_string(&log).expect("reads").lines().count();
    let order = market(Side::Buy, 1.0);

    // The last bar fed, 00:10, is the last closed bar from 00:11, and one bar behind it from
    // 00:12: both are accepted.
    for (at, id) in [
        (close_of(10), 1),
        (T0 + 12 * MIN, 2),
        (T0 + 13 * MIN - 1, 3),
    ] {
        let placed = paper.place(&order, at).expect("accepted");
        assert_eq!(placed.order.id, OrderId(id), "{at}");
    }
    assert_eq!(lines(), 14);

    // From 00:13 it is two bars behind, catching up: refused, and nothing changes.
    for at in [T0 + 13 * MIN, T0 + 13 * MIN + 30_000, T0 + 60 * MIN] {
        let refused = paper.place(&order, at);
        assert!(
            matches!(refused, Err(ManualError::CatchingUp)),
            "{at}: {refused:?}"
        );
    }
    assert_eq!(paper.orders().len(), 3);
    assert_eq!(lines(), 14, "a refused order is not in the log");

    // Cancels are still taken.
    let cancelled = paper.cancel(OrderId(3)).expect("cancels");
    assert_eq!(
        cancelled.order.status,
        OrderStatus::Cancelled("cancelled manually".into())
    );

    // Fed up to 00:12, it takes orders again at 00:13.
    for i in 11..=12 {
        paper.feed(minute(i), Vec::new(), false).expect("live bar");
    }
    let placed = paper.place(&order, T0 + 13 * MIN).expect("accepted");
    assert_eq!(placed.order.id, OrderId(4));
}

#[test]
fn manual_orders_on_a_strategy_session_are_marked_manual_and_only_they_can_be_cancelled() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = config(InstrumentKind::Perpetual, Timeframe::M1);
    let mut paper = PaperSession::create(dir.path(), "mixed", cfg).expect("creates");
    let (events, mut rx) = tokio::sync::broadcast::channel(4096);
    paper.set_events(events);
    feed_all(&mut paper, &minutes(0..40), &[]);
    while rx.try_recv().is_ok() {}
    let position = paper.portfolio().position();
    assert!(position != 0.0, "the strategy holds a position");
    let (side, far) = if position > 0.0 {
        (Side::Sell, limit(Side::Sell, 0.1, 200.0))
    } else {
        (Side::Buy, limit(Side::Buy, 0.1, 50.0))
    };

    // The strategy's position closed by hand, reported once its line is in the log.
    let close = ManualOrder {
        reduce_only: Some(true),
        ..market(side, position.abs())
    };
    let close = paper.place(&close, close_of(39)).expect("accepted");
    assert_eq!(close.source, OrderSource::Manual);
    assert_eq!(
        rx.try_recv(),
        Ok(SessionEvent::Order {
            order: close.clone()
        })
    );
    let log =
        fs::read_to_string(paper_dir(dir.path(), "mixed").join("events.jsonl")).expect("reads");
    let last = log.lines().last().expect("lines");
    assert!(last.starts_with(r#"{"manual_order":{"id":"#), "{last}");

    // A resting limit far from the price, cancelled by hand.
    let resting = paper.place(&far, close_of(39)).expect("accepted");
    assert!(rx.try_recv().is_ok(), "the order is reported");
    let cancelled = paper.cancel(resting.order.id).expect("cancels");
    assert_eq!(
        cancelled.order.status,
        OrderStatus::Cancelled("cancelled manually".into())
    );
    assert_eq!(
        rx.try_recv(),
        Ok(SessionEvent::Order {
            order: cancelled.clone()
        })
    );
    let again = paper.cancel(resting.order.id);
    assert!(
        matches!(
            again,
            Err(ManualError::Done {
                state: "cancelled",
                ..
            })
        ),
        "{again:?}"
    );
    let strategys = paper.cancel(OrderId(1));
    assert!(
        matches!(strategys, Err(ManualError::NotManual(1))),
        "{strategys:?}"
    );
    for unknown in [0, 999] {
        let none = paper.cancel(OrderId(unknown));
        assert!(matches!(none, Err(ManualError::NoSuchOrder(_))), "{none:?}");
    }

    // The close fills at the next open and closes the strategy's trade.
    feed_all(&mut paper, &minutes(40..41), &[]);
    let filled = paper.cancel(close.order.id);
    assert!(
        matches!(
            filled,
            Err(ManualError::Done {
                state: "filled",
                ..
            })
        ),
        "{filled:?}"
    );
    feed_all(&mut paper, &minutes(41..80), &[]);
    for fill in paper.fills_with_source() {
        let expected = if fill.fill.order_id == close.order.id {
            OrderSource::Manual
        } else {
            OrderSource::Strategy
        };
        assert_eq!(fill.source, expected, "{fill:?}");
    }
    let sources: Vec<TradeSource> = paper.trades().iter().map(|t| t.source).collect();
    assert_eq!(
        sources.iter().filter(|&&s| s == TradeSource::Mixed).count(),
        1,
        "{sources:?}"
    );
    assert!(
        sources
            .iter()
            .filter(|&&s| s == TradeSource::Strategy)
            .count()
            > 1,
        "{sources:?}"
    );
    assert!(!sources.contains(&TradeSource::Manual), "{sources:?}");

    // The log replays to the same state, sources included.
    let reopened = PaperSession::open(dir.path(), "mixed").expect("opens");
    assert_same_marked_state(&paper, &reopened);
    assert!(reopened.warnings().is_empty(), "{:?}", reopened.warnings());
}

#[test]
fn reopening_replays_manual_orders_and_cancels_to_the_uninterrupted_state() {
    for kind in [InstrumentKind::Spot, InstrumentKind::Perpetual] {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut whole = warmed_up(dir.path(), "whole", manual_config(kind));
        feed_by_hand(&mut whole, 0..60, &script());

        // Stopped at minute 30, after the cancel and before the limit sell fills.
        let mut first = warmed_up(dir.path(), "split", manual_config(kind));
        feed_by_hand(&mut first, 0..30, &script());
        let mut reopened = PaperSession::open(dir.path(), "split").expect("opens");
        assert_same_marked_state(&first, &reopened);
        assert_eq!(reopened.orders().len(), 3, "{kind}");
        assert_eq!(reopened.open_orders().len(), 1, "{kind}");
        drop(first);
        feed_by_hand(&mut reopened, 30..60, &script());
        assert_same_marked_state(&whole, &reopened);
        drop(reopened);

        let again = PaperSession::open(dir.path(), "split").expect("opens");
        assert_same_marked_state(&whole, &again);
        assert!(
            again.warnings().is_empty(),
            "{kind}: {:?}",
            again.warnings()
        );
        assert_eq!(again.fills().len(), 2, "{kind}");
    }
}

#[test]
fn a_log_written_before_manual_orders_existed_still_opens() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = config(InstrumentKind::Perpetual, Timeframe::M1);
    let mut current = PaperSession::create(dir.path(), "current", cfg.clone()).expect("creates");
    PaperSession::create(dir.path(), "old", cfg).expect("creates");
    // The lines as sessions wrote them before: a bar, the funding fed with it if any, and
    // whether it was a warm-up bar if it was.
    let mut log = String::new();
    for i in -20..60 {
        let c = minute(i);
        log.push_str(&format!(
            r#"{{"bar":{{"open_time":{},"open":{:?},"high":{:?},"low":{:?},"close":{:?},"volume":{:?},"quote_volume":{:?}}}"#,
            c.open_time, c.open, c.high, c.low, c.close, c.volume, c.quote_volume
        ));
        let mut funding = Vec::new();
        if i == 0 {
            log.push_str(&format!(
                r#","funding":[{{"time":{T0},"rate":0.0001,"due":{T0}}}]"#
            ));
            funding.push(Funding {
                time: T0,
                rate: 0.0001,
                due: T0,
            });
        }
        if i < 0 {
            log.push_str(r#","warm_up":true"#);
        }
        log.push_str("}\n");
        current.feed(c, funding, i < 0).expect("feeds");
    }
    let path = paper_dir(dir.path(), "old").join("events.jsonl");
    fs::write(&path, log).expect("writes");

    let old = PaperSession::open(dir.path(), "old").expect("opens");
    assert_same_marked_state(&current, &old);
    assert!(old.warnings().is_empty(), "{:?}", old.warnings());
    assert!(!old.fills().is_empty());
    assert!(
        old.fills_with_source()
            .iter()
            .all(|f| f.source == OrderSource::Strategy)
    );
}

/// An hour bar `i` hours after `T0` that opens and closes at `open` and dips to `low`.
fn hour(i: i64, open: f64, low: f64) -> Candle {
    Candle {
        open_time: T0 + i * HOUR,
        open,
        high: open + 1.0,
        low,
        close: open,
        volume: 2.0,
        quote_volume: 2.0 * open,
    }
}

/// What the hourly manual test does, after the hour each step is paired with, at the time given.
/// After 00:00, at 01:50 while the 01:00 bar is in progress: a market buy, and a limit buy at 95,
/// which the 01:00 bar dips to. After 04:00, at 06:30 while the feed is one bar behind, the most
/// an order is accepted at (the 05:00 bar closed but is not fed yet): a market sell, and a
/// marketable limit buy that is cancelled before it takes effect.
fn hourly_script() -> Vec<(i64, Millis, Hand)> {
    let at = |h: i64, m: i64| T0 + h * HOUR + m * MIN;
    vec![
        (0, at(1, 50), Hand::Place(market(Side::Buy, 1.0))),
        (0, at(1, 50), Hand::Place(limit(Side::Buy, 1.0, 95.0))),
        (4, at(6, 30), Hand::Place(market(Side::Sell, 1.0))),
        (4, at(6, 30), Hand::Place(limit(Side::Buy, 1.0, 200.0))),
        (4, at(6, 30), Hand::Cancel(4)),
    ]
}

/// The hour bars the hourly manual test feeds: none after 01:00 dips to 95.
fn hourly_bars() -> Vec<Candle> {
    let mut bars = vec![hour(0, 100.0, 99.0), hour(1, 101.0, 94.0)];
    bars.extend((2..10).map(|i| hour(i, 100.0 + i as f64, 99.0 + i as f64)));
    bars
}

/// Feeds the hour bars in `live` as live bars, doing what the hourly script says after each.
fn feed_hours(paper: &mut PaperSession, live: std::ops::Range<usize>) {
    let bars = hourly_bars();
    for i in live {
        paper.feed(bars[i], Vec::new(), false).expect("feeds");
        for (_, at, hand) in hourly_script().iter().filter(|(h, ..)| *h == i as i64) {
            by_hand(paper, hand, *at).expect("accepted");
        }
    }
}

#[test]
fn a_manual_order_never_fills_on_a_bar_from_before_it_was_placed_and_replays_so() {
    let cfg = PaperConfig {
        timeframe: Timeframe::H1,
        ..manual_config(InstrumentKind::Perpetual)
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let mut whole = PaperSession::create(dir.path(), "whole", cfg.clone()).expect("creates");
    feed_hours(&mut whole, 0..10);
    let fills: Vec<(u64, Millis)> = whole
        .fills()
        .iter()
        .map(|f| (f.order_id.0, f.time))
        .collect();
    // The buy placed at 01:50 fills at the 02:00 open, not the 01:00 one; the limit at 95, which
    // only the 01:00 bar reached, never fills; the sell placed at 06:30 fills at the 07:00 open,
    // not on the 05:00 and 06:00 bars fed after it.
    assert_eq!(fills, [(1, T0 + 2 * HOUR), (3, T0 + 7 * HOUR)]);
    let open = whole.fills()[0].price;
    assert!(
        open > 102.0 && open < 102.1,
        "the 02:00 open with slippage: {open}"
    );
    let orders = whole.orders();
    assert_eq!(
        orders.iter().map(|o| o.submitted_at).collect::<Vec<_>>(),
        [T0 + HOUR, T0 + HOUR, T0 + 6 * HOUR, T0 + 6 * HOUR]
    );
    assert_eq!(orders[1].status, OrderStatus::Open);
    assert_eq!(
        orders[3].status,
        OrderStatus::Cancelled("cancelled manually".into())
    );

    // Stopped after 04:00, with both orders placed at 06:30 not yet in effect, and reopened: the
    // replay places them as they were, and the rest goes as in the session that ran through.
    let mut first = PaperSession::create(dir.path(), "split", cfg).expect("creates");
    feed_hours(&mut first, 0..5);
    let mut reopened = PaperSession::open(dir.path(), "split").expect("opens");
    assert_same_marked_state(&first, &reopened);
    drop(first);
    feed_hours(&mut reopened, 5..10);
    assert_same_marked_state(&whole, &reopened);
    drop(reopened);
    let again = PaperSession::open(dir.path(), "split").expect("opens");
    assert_same_marked_state(&whole, &again);
    assert!(again.warnings().is_empty(), "{:?}", again.warnings());
}

#[test]
fn a_manual_order_that_would_replay_under_another_id_is_refused_or_skipped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = manual_config(InstrumentKind::Perpetual);
    let mut paper = PaperSession::create(dir.path(), "edited", cfg).expect("creates");
    feed_by_hand(
        &mut paper,
        0..10,
        &[(4, Hand::Place(market(Side::Buy, 1.0)))],
    );
    assert_eq!(paper.fills().len(), 1);
    drop(paper);
    let path = paper_dir(dir.path(), "edited").join("events.jsonl");
    let log = fs::read_to_string(&path).expect("reads");
    let edited = log.replace(r#"{"manual_order":{"id":1,"#, r#"{"manual_order":{"id":2,"#);
    assert_ne!(edited, log, "the order's line is in the log");
    fs::write(&path, edited).expect("writes");

    // Made by this engine, the log no longer matches what the session did: it does not open.
    match PaperSession::open(dir.path(), "edited") {
        Err(PaperError::BadFile { reason, .. }) => {
            assert!(
                reason.contains("manual order 2 would replay as order 1"),
                "{reason}"
            );
        }
        Err(other) => panic!("expected a bad file, got {other}"),
        Ok(_) => panic!("a log that does not replay opened"),
    }

    // Made by an older engine, it opens to be looked at, with the order skipped and never placed.
    save_as_engine(dir.path(), "edited", Some(ENGINE_REVISION - 1));
    let old = PaperSession::open(dir.path(), "edited").expect("opens");
    assert!(old.orders().is_empty(), "{:?}", old.orders());
    assert!(old.fills().is_empty(), "{:?}", old.fills());
    assert_eq!(old.session().bars(), minutes(0..10));
    assert_eq!(old.warnings().len(), 2, "{:?}", old.warnings());
    assert!(
        old.warnings()[1].contains("manual order 2 would replay as order 1; skipped"),
        "{:?}",
        old.warnings()
    );
}
