//! Live feeds from recorded stream frames served by a scripted WebSocket server on 127.0.0.1, with
//! a fake REST source for backfills. No network.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use td_core::{Candle, InstrumentId, InstrumentKind, Millis, Timeframe, Venue};
use td_data::live::{
    FeedEvent, FeedSpec, Rest, SharedStream, parse_message, run_feed, run_feed_on,
};
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
async fn aborting_a_feed_closes_its_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}/", listener.local_addr().expect("address"));
    let (tx, mut rx) = mpsc::channel(64);
    let spec = FeedSpec {
        url,
        ..FeedSpec::new(btc(Venue::Binance), Timeframe::M1, Some(T0))
    };
    let feed = tokio::spawn(run_feed(spec, FakeRest::new(Vec::new(), 0), tx));
    let (tcp, _) = listener.accept().await.expect("accept");
    let mut socket = tokio_tungstenite::accept_async(tcp).await.expect("upgrade");
    assert_eq!(rx.recv().await, Some(FeedEvent::Connected));
    feed.abort();
    // The reader goes with the feed: the connection closes now, not at the reader's idle timeout.
    let ended = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the connection is still open after the feed was aborted");
    assert!(
        matches!(ended, None | Some(Err(_) | Ok(Message::Close(_)))),
        "{ended:?}"
    );
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

    assert_eq!(
        bars(&events),
        resample(InstrumentKind::Spot, &all, Timeframe::M5, false).candles
    );
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

/// The next bar a feed sends, or fails after 20 s.
async fn next_bar(rx: &mut mpsc::Receiver<FeedEvent>) -> Candle {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("a bar in time");
        match event {
            Some(FeedEvent::Bar(bar)) => return bar,
            Some(_) => {}
            None => panic!("the feed ended"),
        }
    }
}

#[tokio::test]
async fn feeds_of_two_timeframes_share_one_connection_which_closes_when_both_are_gone() {
    let all = minutes();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}/", listener.local_addr().expect("address"));
    let stream = Arc::new(SharedStream::open(btc(Venue::Binance), url));
    let (tcp, _) = listener.accept().await.expect("accept");
    let mut socket = tokio_tungstenite::accept_async(tcp).await.expect("upgrade");

    // One feed starts before the connection is up, one after; both learn it is connected.
    let rest = FakeRest::new(all.clone(), 0);
    let (m1_tx, mut m1) = mpsc::channel(64);
    let m1_feed = tokio::spawn(run_feed_on(
        stream.clone(),
        Timeframe::M1,
        Some(T0),
        rest.clone(),
        m1_tx,
    ));
    assert_eq!(m1.recv().await, Some(FeedEvent::Connected));
    let (m5_tx, mut m5) = mpsc::channel(64);
    let m5_feed = tokio::spawn(run_feed_on(
        stream.clone(),
        Timeframe::M5,
        Some(T0),
        rest.clone(),
        m5_tx,
    ));
    assert_eq!(m5.recv().await, Some(FeedEvent::Connected));
    drop(stream);

    for minute in &all {
        let frame = format!(
            r#"{{"e":"kline","E":1,"s":"BTCUSDT","k":{{"t":{},"i":"1m","o":"{}","h":"{}","l":"{}","c":"{}","v":"{}","q":"{}","x":true}}}}"#,
            minute.open_time,
            minute.open,
            minute.high,
            minute.low,
            minute.close,
            minute.volume,
            minute.quote_volume
        );
        socket.send(Message::text(frame)).await.expect("sends");
    }
    let mut m1_bars = Vec::new();
    for _ in 0..all.len() {
        m1_bars.push(next_bar(&mut m1).await);
    }
    assert_eq!(m1_bars, all);
    assert_eq!(
        vec![next_bar(&mut m5).await],
        resample(InstrumentKind::Spot, &all, Timeframe::M5, false).candles
    );
    assert!(rest.calls().is_empty(), "nothing was missed");

    // The connection outlives one feed and closes with the last.
    m1_feed.abort();
    let held = tokio::time::timeout(Duration::from_millis(300), socket.next()).await;
    assert!(held.is_err(), "closed while a feed still ran: {held:?}");
    m5_feed.abort();
    let ended = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the connection is still open after both feeds were aborted");
    assert!(
        matches!(ended, None | Some(Err(_) | Ok(Message::Close(_)))),
        "{ended:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn frames_that_arrive_before_the_first_feed_runs_are_not_lost() {
    let all = minutes();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}/", listener.local_addr().expect("address"));
    let stream = Arc::new(SharedStream::open(btc(Venue::Binance), url));
    let (tcp, _) = listener.accept().await.expect("accept");
    let mut socket = tokio_tungstenite::accept_async(tcp).await.expect("upgrade");
    let frame = |c: &Candle| {
        format!(
            r#"{{"e":"kline","E":1,"s":"BTCUSDT","k":{{"t":{},"i":"1m","o":"{}","h":"{}","l":"{}","c":"{}","v":"{}","q":"{}","x":true}}}}"#,
            c.open_time, c.open, c.high, c.low, c.close, c.volume, c.quote_volume
        )
    };
    // The venue sends four minutes before any feed runs on the stream, as it can between opening
    // a stream and spawning the feed on it...
    for minute in &all[..4] {
        socket
            .send(Message::text(frame(minute)))
            .await
            .expect("sends");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rest = FakeRest::new(all.clone(), 0);
    let (tx, mut rx) = mpsc::channel(64);
    let feed = tokio::spawn(run_feed_on(
        stream,
        Timeframe::M1,
        Some(T0),
        rest.clone(),
        tx,
    ));
    // ...and the fifth after.
    socket
        .send(Message::text(frame(&all[4])))
        .await
        .expect("sends");

    let mut got = Vec::new();
    for _ in 0..all.len() {
        got.push(next_bar(&mut rx).await);
    }
    assert_eq!(got, all);
    assert!(
        rest.calls().is_empty(),
        "frames were lost: {:?}",
        rest.calls()
    );
    feed.abort();
}

/// Feeds on sources other than the venues' own, through `SharedStream::open_on`.
mod fake_sources {
    use td_core::FeeSchedule;
    use td_data::live::{Greeting, KlineUpdate, LiveStream};
    use td_data::{DataSource, Http, InstrumentList, MinuteHistory};

    use super::*;

    /// A source whose stream greets with `hello` and sends one bar per frame as
    /// `open_time,open,high,low,close,volume,quote_volume,closed`, and `ack` frames with no bar.
    struct CsvStream;

    /// A source with history only: no funding and no live stream.
    struct HistoryOnly;

    impl DataSource for CsvStream {
        fn venue(&self) -> Venue {
            Venue::Binance
        }
        fn kinds(&self) -> &'static [InstrumentKind] {
            &[InstrumentKind::Spot]
        }
        fn instruments(&self, _: &Http, _: InstrumentKind) -> Result<InstrumentList, DataError> {
            Ok(InstrumentList::default())
        }
        fn default_fees(&self, _: InstrumentKind) -> Result<FeeSchedule, DataError> {
            Ok(FeeSchedule {
                maker: 0.0,
                taker: 0.0,
            })
        }
        fn minute_history(
            &self,
            _: &Http,
            _: &InstrumentId,
            _: Millis,
            _: Millis,
            _: Millis,
        ) -> Result<MinuteHistory, DataError> {
            Ok(MinuteHistory::default())
        }
        fn live(&self) -> Result<&dyn LiveStream, DataError> {
            Ok(self)
        }
    }

    impl LiveStream for CsvStream {
        fn url(&self, _: &InstrumentId) -> String {
            "ws://127.0.0.1:9/".into()
        }
        fn greeting(&self, _: &InstrumentId) -> Greeting {
            Greeting {
                subscribe: Some("hello".into()),
                heartbeat: None,
            }
        }
        fn parse(&self, text: &str) -> Result<Vec<KlineUpdate>, DataError> {
            if text == "ack" {
                return Ok(Vec::new());
            }
            let bad = |reason: &str| DataError::Parse {
                what: "a fake frame".into(),
                reason: reason.into(),
            };
            let fields: Vec<&str> = text.split(',').collect();
            let [t, o, h, l, c, v, q, closed] = fields[..] else {
                return Err(bad("not 8 fields"));
            };
            let num = |s: &str| s.parse::<f64>().map_err(|_| bad(s));
            let candle = Candle {
                open_time: t.parse().map_err(|_| bad(t))?,
                open: num(o)?,
                high: num(h)?,
                low: num(l)?,
                close: num(c)?,
                volume: num(v)?,
                quote_volume: num(q)?,
            };
            Ok(vec![KlineUpdate {
                candle,
                closed: closed == "1",
            }])
        }
        fn klines(
            &self,
            _: &Http,
            _: &InstrumentId,
            _: Timeframe,
            _: Millis,
            _: Millis,
        ) -> Result<Vec<Candle>, DataError> {
            Ok(Vec::new())
        }
    }

    impl DataSource for HistoryOnly {
        fn venue(&self) -> Venue {
            Venue::Binance
        }
        fn kinds(&self) -> &'static [InstrumentKind] {
            &[InstrumentKind::Spot]
        }
        fn instruments(&self, _: &Http, _: InstrumentKind) -> Result<InstrumentList, DataError> {
            Ok(InstrumentList::default())
        }
        fn default_fees(&self, _: InstrumentKind) -> Result<FeeSchedule, DataError> {
            Ok(FeeSchedule {
                maker: 0.0,
                taker: 0.0,
            })
        }
        fn minute_history(
            &self,
            _: &Http,
            _: &InstrumentId,
            _: Millis,
            _: Millis,
            _: Millis,
        ) -> Result<MinuteHistory, DataError> {
            Ok(MinuteHistory::default())
        }
    }

    fn csv(c: &Candle, closed: bool) -> String {
        format!(
            "{},{},{},{},{},{},{},{}",
            c.open_time,
            c.open,
            c.high,
            c.low,
            c.close,
            c.volume,
            c.quote_volume,
            u8::from(closed)
        )
    }

    #[tokio::test]
    async fn a_feed_reads_a_fake_sources_stream_through_its_greeting_and_parser() {
        let all = minutes();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}/", listener.local_addr().expect("address"));
        let stream = Arc::new(SharedStream::open_on(&CsvStream, btc(Venue::Binance), url));
        let rest = FakeRest::new(all.clone(), 0);
        let (tx, mut rx) = mpsc::channel(64);
        let feed = tokio::spawn(run_feed_on(
            stream,
            Timeframe::M1,
            Some(T0),
            rest.clone(),
            tx,
        ));
        let (tcp, _) = listener.accept().await.expect("accept");
        let mut socket = tokio_tungstenite::accept_async(tcp).await.expect("upgrade");

        // The source's greeting comes first.
        let hello = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("a greeting in time")
            .expect("a frame")
            .expect("reads");
        assert_eq!(hello.to_text().expect("text"), "hello");
        // Minute 0 in progress, a frame with no bar, then minutes 0, 1, 2 and 4 closed: minute 3
        // is skipped and backfilled.
        let mut script = vec![csv(&all[0], false), "ack".to_string()];
        script.extend([0, 1, 2, 4].map(|i| csv(&all[i], true)));
        for frame in script {
            socket.send(Message::text(frame)).await.expect("sends");
        }
        let mut got = Vec::new();
        for _ in 0..all.len() {
            got.push(next_bar(&mut rx).await);
        }
        assert_eq!(got, all);
        assert_eq!(rest.calls(), [(T0 + 3 * MIN, T0 + 4 * MIN)]);
        feed.abort();
    }

    #[tokio::test]
    async fn a_source_without_a_live_stream_says_so_instead_of_connecting() {
        let id = btc(Venue::Binance);
        let stream = Arc::new(SharedStream::open_on(&HistoryOnly, id, String::new()));
        let (tx, mut rx) = mpsc::channel(64);
        let feed = tokio::spawn(run_feed_on(
            stream,
            Timeframe::M1,
            None,
            FakeRest::new(Vec::new(), 0),
            tx,
        ));
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("an event in time");
        let error = DataError::NoLiveStream(Venue::Binance).to_string();
        assert_eq!(error, "binance has no live stream");
        assert_eq!(event, Some(FeedEvent::ConnectFailed { error }));
        feed.abort();
    }

    #[tokio::test]
    async fn a_stream_for_a_kind_the_venue_does_not_list_says_so_instead_of_connecting() {
        let id = InstrumentId {
            kind: InstrumentKind::Fx,
            ..btc(Venue::Binance)
        };
        let url = td_data::live::stream_url(&id);
        assert_eq!(url, "");
        let stream = Arc::new(SharedStream::open(id, url));
        let (tx, mut rx) = mpsc::channel(64);
        let feed = tokio::spawn(run_feed_on(
            stream,
            Timeframe::M1,
            None,
            FakeRest::new(Vec::new(), 0),
            tx,
        ));
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("an event in time");
        let error = "binance does not list fx instruments".to_string();
        assert_eq!(event, Some(FeedEvent::ConnectFailed { error }));
        feed.abort();
    }
}
