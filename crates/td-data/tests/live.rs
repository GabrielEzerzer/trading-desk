//! Live feeds from recorded stream frames served by a scripted WebSocket server on 127.0.0.1, with
//! a fake REST source for backfills. No network.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::SinkExt;
use td_core::{Candle, InstrumentId, InstrumentKind, Millis, Timeframe, Venue};
use td_data::live::{FeedEvent, FeedSpec, Rest, parse_message, run_feed};
use td_data::{DataError, binance, bybit, resample};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const MIN: Millis = 60_000;
/// 2024-01-01 00:00 UTC, the first minute in the fixtures.
const T0: Millis = 1_704_067_200_000;

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn frames(name: &str) -> Vec<String> {
    fixture(name).lines().map(str::to_string).collect()
}

/// The five recorded minutes from 00:00, from the REST fixture.
fn minutes() -> Vec<Candle> {
    binance::parse_klines(&fixture("binance_klines.json")).expect("parses")
}

fn btc(venue: Venue) -> InstrumentId {
    InstrumentId {
        venue,
        symbol: "BTCUSDT".into(),
        kind: InstrumentKind::Spot,
    }
}

/// Serves one script of frames per connection, then closes it. After the last script it accepts
/// connections and holds them open in silence.
async fn serve(scripts: Vec<Vec<String>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}/", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let mut scripts = scripts.into_iter();
        let mut held = Vec::new();
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut socket) = tokio_tungstenite::accept_async(tcp).await else {
                continue;
            };
            match scripts.next() {
                Some(script) => {
                    for frame in script {
                        if socket.send(Message::text(frame)).await.is_err() {
                            break;
                        }
                    }
                    // Dropped without a close frame: a connection lost mid-stream.
                    drop(socket);
                }
                None => held.push(socket),
            }
        }
    });
    url
}

/// Serves REST minutes from a list, failing the first `failures` calls, and records each call.
struct FakeRest {
    minutes: Vec<Candle>,
    failures: Mutex<u32>,
    calls: Mutex<Vec<(Millis, Millis)>>,
}

impl FakeRest {
    fn new(minutes: Vec<Candle>, failures: u32) -> Arc<Self> {
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
        assert_eq!(timeframe, Timeframe::M1, "feeds backfill minutes");
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

/// Runs a feed and collects its events until `bars` bars have come, or fails after 20 s.
async fn collect(spec: FeedSpec, rest: Arc<FakeRest>, bars: usize) -> Vec<FeedEvent> {
    let (tx, mut rx) = mpsc::channel(64);
    let feed = tokio::spawn(run_feed(spec, rest, tx));
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while events
        .iter()
        .filter(|e| matches!(e, FeedEvent::Bar(_)))
        .count()
        < bars
    {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(event)) => events.push(event),
            other => panic!("feed stopped or stalled ({other:?}); events so far: {events:?}"),
        }
    }
    feed.abort();
    events
}

fn bars(events: &[FeedEvent]) -> Vec<Candle> {
    events
        .iter()
        .filter_map(|e| match e {
            FeedEvent::Bar(bar) => Some(*bar),
            _ => None,
        })
        .collect()
}

#[test]
fn recorded_frames_parse_into_updates_with_the_closed_flag() {
    for (venue, file, expected_frames) in [
        (Venue::Binance, "binance_kline_stream.jsonl", 10),
        (Venue::Bybit, "bybit_kline_stream.jsonl", 10),
    ] {
        let updates: Vec<_> = frames(file)
            .iter()
            .flat_map(|f| parse_message(venue, f).expect("parses"))
            .collect();
        assert_eq!(updates.len(), expected_frames, "{venue}");
        let closed: Vec<Millis> = updates
            .iter()
            .filter(|u| u.closed)
            .map(|u| (u.candle.open_time - T0) / MIN)
            .collect();
        // Minute 1 closes twice (a duplicate) and minute 3 never comes.
        assert_eq!(closed, [0, 1, 1, 2, 4], "{venue}");
        let first_close = updates.iter().find(|u| u.closed).expect("a closed bar");
        assert_eq!(first_close.candle, minutes()[0], "{venue}");
    }
}

#[test]
fn a_refused_bybit_subscription_is_an_error() {
    let refused = r#"{"success":false,"ret_msg":"Invalid symbol :[kline.1.NOPE]","conn_id":"x","op":"subscribe"}"#;
    assert!(matches!(
        parse_message(Venue::Bybit, refused),
        Err(DataError::Api { .. })
    ));
}

#[test]
fn binance_rest_klines_parse() {
    let klines = minutes();
    assert_eq!(klines.len(), 5);
    assert_eq!(klines[0].open_time, T0);
    assert_eq!(klines[0].quote_volume, 636_760.295_441_92);
    // The REST page and Bybit's kline page are the same minutes in different shapes.
    let mut page = bybit::parse_kline_page(&fixture("bybit_kline_page.json")).expect("parses");
    page.reverse();
    assert_eq!(
        klines.iter().map(|c| c.close).collect::<Vec<_>>(),
        page.iter().map(|c| c.close).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn recorded_streams_give_closed_bars_once_in_order_with_the_skipped_minute_backfilled() {
    for (venue, file) in [
        (Venue::Binance, "binance_kline_stream.jsonl"),
        (Venue::Bybit, "bybit_kline_stream.jsonl"),
    ] {
        let url = serve(vec![frames(file)]).await;
        let rest = FakeRest::new(minutes(), 0);
        let spec = FeedSpec {
            url,
            ..FeedSpec::new(btc(venue), Timeframe::M1, Some(T0))
        };
        let events = collect(spec, Arc::clone(&rest), 5).await;
        assert_eq!(bars(&events), minutes(), "{venue}");
        assert_eq!(rest.calls(), [(T0 + 3 * MIN, T0 + 4 * MIN)], "{venue}");
        assert_eq!(events[0], FeedEvent::Connected, "{venue}");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, FeedEvent::BadMessage { .. })),
            "{venue}: {events:?}"
        );
    }
}

#[tokio::test]
async fn a_dropped_connection_is_reconnected_and_the_missed_minutes_backfilled() {
    let all = minutes();
    let closed = |i: usize| {
        let c = all[i];
        format!(
            r#"{{"e":"kline","E":1,"s":"BTCUSDT","k":{{"t":{},"i":"1m","o":"{}","h":"{}","l":"{}","c":"{}","v":"{}","q":"{}","x":true}}}}"#,
            c.open_time, c.open, c.high, c.low, c.close, c.volume, c.quote_volume
        )
    };
    // The first connection carries minutes 1 and 2 and drops; the second repeats 2, then 4.
    let url = serve(vec![vec![closed(1), closed(2)], vec![closed(2), closed(4)]]).await;
    // The first backfill (minute 0, before the stream's first bar) fails once and is retried.
    let rest = FakeRest::new(all.clone(), 1);
    let spec = FeedSpec {
        url,
        ..FeedSpec::new(btc(Venue::Binance), Timeframe::M5, Some(T0))
    };
    let events = collect(spec, Arc::clone(&rest), 1).await;

    assert_eq!(bars(&events), resample(&all, Timeframe::M5, false).candles);
    assert_eq!(
        rest.calls(),
        [(T0, T0 + MIN), (T0, T0 + MIN), (T0 + 3 * MIN, T0 + 4 * MIN)]
    );
    let connects = events
        .iter()
        .filter(|e| **e == FeedEvent::Connected)
        .count();
    assert_eq!(connects, 2, "{events:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, FeedEvent::Disconnected { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, FeedEvent::BackfillFailed { .. }))
    );
}
