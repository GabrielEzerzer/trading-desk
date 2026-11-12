//! The bar still forming on a chart feed, from scripted stream frames (closed minutes and updates of
//! the minute in progress) served on 127.0.0.1, with a fake REST source. No network.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::SinkExt;
use td_core::{Candle, InstrumentId, InstrumentKind, Millis, Timeframe, Venue};
use td_data::DataError;
use td_data::live::{ChartEvent, FeedEvent, Rest, SharedStream, run_chart_feed_on, run_feed_on};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const MIN: Millis = 60_000;
/// 2024-01-01 00:00 UTC.
const T0: Millis = 1_704_067_200_000;

fn btc() -> InstrumentId {
    InstrumentId {
        venue: Venue::Binance,
        symbol: "BTCUSDT".into(),
        kind: InstrumentKind::Spot,
    }
}

/// Minute `i` from T0 as it stands `step` updates in (0 is its first update), closing at 100 + i.
fn minute(i: i64, step: u32) -> Candle {
    let close = 100.0 + i as f64 + f64::from(step) * 0.25;
    Candle {
        open_time: T0 + i * MIN,
        open: 100.0 + i as f64 - 0.5,
        high: close + 1.0,
        low: 99.0 + i as f64 - 1.0,
        close,
        volume: 1.0 + f64::from(step),
        quote_volume: (1.0 + f64::from(step)) * close,
    }
}

/// A Binance kline frame for a minute, closed or in progress.
fn frame(c: &Candle, closed: bool) -> String {
    format!(
        r#"{{"e":"kline","E":1,"s":"BTCUSDT","k":{{"t":{},"i":"1m","o":"{}","h":"{}","l":"{}","c":"{}","v":"{}","q":"{}","x":{closed}}}}}"#,
        c.open_time, c.open, c.high, c.low, c.close, c.volume, c.quote_volume
    )
}

/// Two updates in progress, then the close, of minute `i`.
fn updates(i: i64) -> Vec<String> {
    vec![
        frame(&minute(i, 0), false),
        frame(&minute(i, 1), false),
        frame(&minute(i, 2), true),
    ]
}

/// Serves `frames` on the first connection, then holds it open in silence.
async fn serve(frames: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}/", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let mut script = Some(frames);
        let mut held = Vec::new();
        while let Ok((tcp, _)) = listener.accept().await {
            let Ok(mut socket) = tokio_tungstenite::accept_async(tcp).await else {
                continue;
            };
            for f in script.take().unwrap_or_default() {
                if socket.send(Message::text(f)).await.is_err() {
                    break;
                }
            }
            held.push(socket);
        }
    });
    url
}

/// Serves closed minutes, failing the first `failures` calls, and records each call.
struct FakeRest {
    minutes: Vec<Candle>,
    failures: Mutex<u32>,
    calls: Mutex<Vec<(Millis, Millis)>>,
}

impl FakeRest {
    fn new(minutes: Vec<Candle>) -> Arc<Self> {
        FakeRest::failing(minutes, 0)
    }

    fn failing(minutes: Vec<Candle>, failures: u32) -> Arc<Self> {
        Arc::new(FakeRest {
            minutes,
            failures: Mutex::new(failures),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(Millis, Millis)> {
        self.calls.lock().expect("lock").clone()
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
        assert_eq!(timeframe, Timeframe::M1, "feeds fetch minutes");
        self.calls.lock().expect("lock").push((from, to));
        let mut failures = self.failures.lock().expect("lock");
        if *failures > 0 {
            *failures -= 1;
            return Err(DataError::Api {
                url: "fake".into(),
                message: "unavailable".into(),
            });
        }
        Ok(self
            .minutes
            .iter()
            .copied()
            .filter(|m| (from..to).contains(&m.open_time))
            .collect())
    }

    fn funding(
        &self,
        _id: &InstrumentId,
        _from: Millis,
        _to: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        Ok(Vec::new())
    }
}

/// Receives until `done` holds for an event, or fails after 20 s.
async fn until<T: std::fmt::Debug>(
    rx: &mut mpsc::Receiver<T>,
    done: impl Fn(&T) -> bool,
) -> Vec<T> {
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = rx.recv().await {
            let stop = done(&event);
            events.push(event);
            if stop {
                return;
            }
        }
        panic!("the feed ended");
    })
    .await
    .unwrap_or_else(|_| panic!("timed out after {events:?}"));
    events
}

/// The bar of minutes `from..to` (each at its closing update), with the minute `to` as it stands
/// `step` updates in, opening at `open_time`.
fn joined(open_time: Millis, closed: std::ops::Range<i64>, open: Option<(i64, u32)>) -> Candle {
    let minutes: Vec<Candle> = closed
        .map(|i| minute(i, 2))
        .chain(open.map(|(i, step)| minute(i, step)))
        .collect();
    let mut bar = Candle {
        open_time,
        ..minutes[0]
    };
    for m in &minutes[1..] {
        bar.high = bar.high.max(m.high);
        bar.low = bar.low.min(m.low);
        bar.close = m.close;
        bar.volume += m.volume;
        bar.quote_volume += m.quote_volume;
    }
    bar
}

#[tokio::test]
async fn a_one_minute_chart_gets_each_update_in_progress_then_the_closed_bar() {
    let url = serve(updates(0)).await;
    let rest = FakeRest::new(Vec::new());
    let (tx, mut rx) = mpsc::channel(64);
    let stream = Arc::new(SharedStream::open(btc(), url));
    let _feed = tokio::spawn(run_chart_feed_on(
        stream,
        Timeframe::M1,
        None,
        rest.clone(),
        tx,
    ));
    let events = until(&mut rx, |e| {
        matches!(e, ChartEvent::Feed(FeedEvent::Bar(_)))
    })
    .await;
    assert_eq!(
        events,
        [
            ChartEvent::Feed(FeedEvent::Connected),
            ChartEvent::Forming(minute(0, 0)),
            ChartEvent::Forming(minute(0, 1)),
            ChartEvent::Feed(FeedEvent::Bar(minute(0, 2))),
        ]
    );
    assert_eq!(rest.calls(), [], "a minute bar has no earlier minutes");
}

#[tokio::test]
async fn a_five_minute_chart_joined_late_fetches_the_bucket_start_and_ends_the_bucket_whole() {
    // Joined during 00:02. 00:00 and 00:01 come from REST; the bucket gives no closed bar, so its
    // last forming bar is whole. The next bucket is seen from its start and closes as normal.
    let frames: Vec<String> = (2..10).flat_map(updates).collect();
    let url = serve(frames).await;
    let rest = FakeRest::new(vec![minute(0, 2), minute(1, 2)]);
    let (tx, mut rx) = mpsc::channel(256);
    let stream = Arc::new(SharedStream::open(btc(), url));
    let _feed = tokio::spawn(run_chart_feed_on(
        stream,
        Timeframe::M5,
        None,
        rest.clone(),
        tx,
    ));
    let events = until(&mut rx, |e| {
        matches!(e, ChartEvent::Feed(FeedEvent::Bar(_)))
    })
    .await;

    assert_eq!(
        rest.calls(),
        [(T0, T0 + 2 * MIN)],
        "the bucket's start, once"
    );
    let forming: Vec<Candle> = events
        .iter()
        .filter_map(|e| match e {
            ChartEvent::Forming(bar) => Some(*bar),
            _ => None,
        })
        .collect();
    // Two updates per minute of the first bucket (00:02 to 00:04), the whole bucket at its end,
    // then two per minute of the second.
    assert_eq!(forming.len(), 3 * 2 + 1 + 5 * 2, "{forming:?}");
    assert_eq!(forming[0], joined(T0, 0..2, Some((2, 0))));
    assert_eq!(forming[3], joined(T0, 0..3, Some((3, 1))));
    assert_eq!(
        forming[6],
        joined(T0, 0..5, None),
        "the first bucket, whole"
    );
    assert_eq!(forming[7], joined(T0 + 5 * MIN, 5..5, Some((5, 0))));
    assert_eq!(forming[16], joined(T0 + 5 * MIN, 5..9, Some((9, 1))));
    assert!(forming[..7].iter().all(|b| b.open_time == T0));
    assert!(forming[7..].iter().all(|b| b.open_time == T0 + 5 * MIN));

    // The closed events are what a plain feed reports, with the whole first bucket's forming bar
    // before its drop and the second bucket's closed bar last.
    let closed: Vec<&ChartEvent> = events
        .iter()
        .filter(|e| !matches!(e, ChartEvent::Forming(_)))
        .collect();
    assert_eq!(
        closed,
        [
            &ChartEvent::Feed(FeedEvent::Connected),
            &ChartEvent::Feed(FeedEvent::Dropped(T0)),
            &ChartEvent::Feed(FeedEvent::Bar(joined(T0 + 5 * MIN, 5..10, None))),
        ]
    );
    let whole = events
        .iter()
        .position(|e| *e == ChartEvent::Forming(joined(T0, 0..5, None)))
        .expect("the whole first bucket");
    assert_eq!(events[whole + 1], ChartEvent::Feed(FeedEvent::Dropped(T0)));
}

/// Runs a five-minute chart feed on `frames` until its first closed bar.
async fn five_minute_chart(frames: Vec<String>, rest: Arc<FakeRest>) -> Vec<ChartEvent> {
    let url = serve(frames).await;
    let (tx, mut rx) = mpsc::channel(256);
    let stream = Arc::new(SharedStream::open(btc(), url));
    let _feed = tokio::spawn(run_chart_feed_on(stream, Timeframe::M5, None, rest, tx));
    until(&mut rx, |e| {
        matches!(e, ChartEvent::Feed(FeedEvent::Bar(_)))
    })
    .await
}

#[tokio::test]
async fn a_bucket_joined_late_ends_whole_when_its_last_minute_closes_after_the_next_one_starts() {
    // The stream sends 00:05's first update before 00:04's closed one.
    let mut frames: Vec<String> = (2..4).flat_map(updates).collect();
    frames.push(frame(&minute(4, 0), false));
    frames.push(frame(&minute(5, 0), false));
    frames.push(frame(&minute(4, 2), true));
    frames.extend(updates(5).into_iter().skip(1));
    frames.extend((6..10).flat_map(updates));
    let events = five_minute_chart(frames, FakeRest::new(vec![minute(0, 2), minute(1, 2)])).await;

    let next = events
        .iter()
        .position(|e| *e == ChartEvent::Forming(joined(T0 + 5 * MIN, 5..5, Some((5, 0)))))
        .expect("the next bucket starts");
    assert_eq!(
        events[next + 1..next + 3],
        [
            ChartEvent::Forming(joined(T0, 0..5, None)),
            ChartEvent::Feed(FeedEvent::Dropped(T0)),
        ],
        "the first bucket, whole, after the next one started: {events:?}"
    );
}

#[tokio::test]
async fn a_failed_fetch_of_a_joined_buckets_start_is_tried_again_when_the_bucket_ends() {
    let frames: Vec<String> = (2..10).flat_map(updates).collect();
    let rest = FakeRest::failing(vec![minute(0, 2), minute(1, 2)], 1);
    let events = five_minute_chart(frames, rest.clone()).await;

    assert_eq!(
        rest.calls(),
        [(T0, T0 + 2 * MIN), (T0, T0 + 2 * MIN)],
        "once when joined, once more when the bucket ends"
    );
    let forming: Vec<Candle> = events
        .iter()
        .filter_map(|e| match e {
            ChartEvent::Forming(bar) => Some(*bar),
            _ => None,
        })
        .collect();
    assert_eq!(
        forming[0],
        joined(T0, 2..2, Some((2, 0))),
        "without its start"
    );
    assert_eq!(
        forming[6],
        joined(T0, 0..5, None),
        "the first bucket, whole"
    );
}

#[tokio::test]
async fn a_bucket_whose_last_minute_comes_through_a_backfill_retries_its_start_and_ends_whole() {
    // Joined during 00:02, the fetch of 00:00 and 00:01 fails; 00:04's closed frame is lost, so
    // it comes from the backfill before 00:05's close.
    let mut frames: Vec<String> = (2..4).flat_map(updates).collect();
    frames.extend(updates(4).into_iter().take(2));
    frames.extend((5..10).flat_map(updates));
    let rest = FakeRest::failing(vec![minute(0, 2), minute(1, 2), minute(4, 2)], 1);
    let events = five_minute_chart(frames, rest.clone()).await;

    assert_eq!(
        rest.calls(),
        [
            (T0, T0 + 2 * MIN),
            (T0 + 4 * MIN, T0 + 5 * MIN),
            (T0, T0 + 2 * MIN)
        ],
        "the start, the backfill, then the start once more"
    );
    let whole = events
        .iter()
        .position(|e| *e == ChartEvent::Forming(joined(T0, 0..5, None)))
        .unwrap_or_else(|| panic!("the first bucket, whole: {events:?}"));
    assert_eq!(events[whole + 1], ChartEvent::Feed(FeedEvent::Dropped(T0)));
}

#[tokio::test]
async fn a_plain_feed_ignores_updates_in_progress_and_never_fetches_for_them() {
    let frames: Vec<String> = (2..10).flat_map(updates).collect();
    let url = serve(frames).await;
    let rest = FakeRest::new(vec![minute(0, 2), minute(1, 2)]);
    let (tx, mut rx) = mpsc::channel(256);
    let stream = Arc::new(SharedStream::open(btc(), url));
    let _feed = tokio::spawn(run_feed_on(stream, Timeframe::M5, None, rest.clone(), tx));
    let events = until(&mut rx, |e| matches!(e, FeedEvent::Bar(_))).await;
    assert_eq!(
        events,
        [
            FeedEvent::Connected,
            FeedEvent::Dropped(T0),
            FeedEvent::Bar(joined(T0 + 5 * MIN, 5..10, None)),
        ]
    );
    assert_eq!(rest.calls(), []);
}
