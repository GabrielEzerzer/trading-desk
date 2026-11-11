//! The bar still forming on the `candles:` WebSocket topic: real HTTP and WebSocket against a
//! server started in-process on 127.0.0.1:0, a scripted venue stream on 127.0.0.1 that sends
//! updates of the minute in progress as well as closed minutes, and a fake REST source. No network.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, Timeframe, Venue,
};
use td_data::live::Rest;
use td_data::{DataError, DownloadReport, InstrumentList, Store, resample};
use td_server::api::auth::Token;
use td_server::api::source::{LiveSource, MarketSource};
use td_server::api::ws::ServerMessage;
use td_server::api::{ApiConfig, JOB_GRACE, Server};
use td_server::{MANUAL_STRATEGY, SessionEvent};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const WAIT: Duration = Duration::from_secs(20);
const MIN: Millis = 60_000;
/// 2024-01-01 00:00 UTC.
const T0: Millis = 1_704_067_200_000;
const INSTRUMENT: &str = "binance:TESTUSDT:spot";

fn instrument() -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Binance,
            symbol: "TESTUSDT".into(),
            kind: InstrumentKind::Spot,
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

/// The minute `i` minutes after `T0` as it stands `step` updates in; step 2 is its close.
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
        r#"{{"e":"kline","E":1,"s":"TESTUSDT","k":{{"t":{},"i":"1m","o":"{}","h":"{}","l":"{}","c":"{}","v":"{}","q":"{}","x":{closed}}}}}"#,
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

/// The bar of the closed minutes `closed`, then minute `open` as it stands `step` updates in.
fn bar(open_time: Millis, closed: std::ops::Range<i64>, open: Option<(i64, u32)>) -> Candle {
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

/// Lists the one test instrument for any venue and kind; downloads are not used here.
struct Venues;

impl MarketSource for Venues {
    fn instruments(&self, _: Venue, _: InstrumentKind) -> Result<InstrumentList, DataError> {
        Ok(InstrumentList {
            instruments: vec![instrument()],
            rejected: Vec::new(),
        })
    }

    fn candles(
        &self,
        _: &Store,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<DownloadReport, DataError> {
        Err(DataError::NoDataDir)
    }

    fn funding(
        &self,
        _: &Store,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<usize, DataError> {
        Err(DataError::NoDataDir)
    }
}

/// Serves the closed minutes before `T0` and the first two after it (and bars resampled from
/// them), as of 00:02:30.
struct FakeRest {
    minutes: Vec<Candle>,
}

impl Rest for FakeRest {
    fn klines(
        &self,
        _: &InstrumentId,
        timeframe: Timeframe,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<Candle>, DataError> {
        let bars = if timeframe == Timeframe::M1 {
            self.minutes.clone()
        } else {
            resample(InstrumentKind::Spot, &self.minutes, timeframe, false).candles
        };
        Ok(bars
            .into_iter()
            .filter(|c| (from..to).contains(&c.open_time))
            .collect())
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
        T0 + 2 * MIN + 30_000
    }
}

/// A scripted venue stream on 127.0.0.1: it sends each pushed frame to every open connection.
#[derive(Clone)]
struct Exchange {
    url: String,
    push: broadcast::Sender<String>,
    open: Arc<AtomicUsize>,
}

impl Exchange {
    async fn start() -> Exchange {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}/", listener.local_addr().expect("address"));
        let venue = Exchange {
            url,
            push: broadcast::channel(1024).0,
            open: Arc::new(AtomicUsize::new(0)),
        };
        let serving = venue.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let Ok(mut socket) = tokio_tungstenite::accept_async(tcp).await else {
                    continue;
                };
                let mut frames = serving.push.subscribe();
                serving.open.fetch_add(1, Ordering::SeqCst);
                let open = serving.open.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            frame = frames.recv() => match frame {
                                Ok(f) => if socket.send(Message::text(f)).await.is_err() { break },
                                Err(_) => break,
                            },
                            incoming = socket.next() => match incoming {
                                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                                Some(Ok(_)) => {}
                            },
                        }
                    }
                    open.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        venue
    }

    fn send(&self, frames: &[String]) {
        for f in frames {
            self.push.send(f.clone()).expect("a connection is open");
        }
    }

    async fn until_open(&self, n: usize) {
        let deadline = Instant::now() + WAIT;
        while self.open.load(Ordering::SeqCst) != n {
            assert!(Instant::now() < deadline, "connections never reached {n}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

struct Live {
    venue: Exchange,
    rest: Arc<FakeRest>,
}

impl LiveSource for Live {
    fn rest(&self) -> Arc<dyn Rest> {
        self.rest.clone()
    }

    fn stream_url(&self, _: &InstrumentId) -> String {
        self.venue.url.clone()
    }
}

async fn start(dir: &TempDir, venue: &Exchange) -> Server {
    let mut minutes: Vec<Candle> = (-60..0).map(|i| minute(i, 2)).collect();
    minutes.extend([minute(0, 2), minute(1, 2)]);
    let live = Live {
        venue: venue.clone(),
        rest: Arc::new(FakeRest { minutes }),
    };
    Server::start(ApiConfig {
        store: Store::new(dir.path()),
        source: Arc::new(Venues),
        live: Arc::new(live),
        token: Token::generate().expect("rng"),
        port: 0,
    })
    .await
    .expect("starts")
}

async fn shutdown(server: Server) {
    tokio::time::timeout(JOB_GRACE, server.shutdown())
        .await
        .expect("shuts down within the grace period")
        .expect("shuts down cleanly");
}

struct Ws(WebSocketStream<MaybeTlsStream<TcpStream>>);

impl Ws {
    async fn open(server: &Server) -> Ws {
        let mut request = format!("ws://127.0.0.1:{}/ws", server.port())
            .into_client_request()
            .expect("a request");
        let bearer = format!("Bearer {}", server.token().as_str());
        request
            .headers_mut()
            .insert("authorization", bearer.parse().expect("a header"));
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("upgrades");
        Ws(ws)
    }

    async fn subscribe(&mut self, topic: &str) {
        let message = json!({ "type": "subscribe", "topic": topic }).to_string();
        self.0
            .send(Message::Text(message.into()))
            .await
            .expect("sends");
        assert_eq!(
            self.next().await.1,
            ServerMessage::Subscribed {
                topic: topic.into()
            }
        );
    }

    /// The next message, as its raw JSON and read.
    async fn next(&mut self) -> (Value, ServerMessage) {
        loop {
            let frame = tokio::time::timeout(WAIT, self.0.next())
                .await
                .expect("a message in time");
            match frame {
                Some(Ok(Message::Text(text))) => {
                    let raw = serde_json::from_str(&text).expect("JSON");
                    return (raw, serde_json::from_str(&text).expect("a server message"));
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => panic!("the socket closed"),
                Some(Ok(_)) => {}
            }
        }
    }

    /// Messages until the closed candle of `topic` opening at `open_time`.
    async fn until_candle(
        &mut self,
        topic: &str,
        open_time: Millis,
    ) -> Vec<(Value, ServerMessage)> {
        let mut messages = Vec::new();
        loop {
            let message = self.next().await;
            let done = matches!(&message.1, ServerMessage::Candle { topic: t, candle } if t == topic && candle.open_time == open_time);
            messages.push(message);
            if done {
                return messages;
            }
        }
    }
}

/// The forming bars of `topic` among `messages`.
fn forming(messages: &[(Value, ServerMessage)], topic: &str) -> Vec<Candle> {
    messages
        .iter()
        .filter_map(|(_, m)| match m {
            ServerMessage::CandleForming { topic: t, candle } if t == topic => Some(*candle),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_candles_topic_sends_the_bar_forming_then_the_closed_bar_that_replaces_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let venue = Exchange::start().await;
    let server = start(&dir, &venue).await;
    let mut ws = Ws::open(&server).await;
    let m1 = format!("candles:{INSTRUMENT}:1m");
    let m5 = format!("candles:{INSTRUMENT}:5m");
    ws.subscribe(&m1).await;
    ws.subscribe(&m5).await;
    venue.until_open(1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Joined during 00:02: the 5m bar fetches 00:00 and 00:01 first.
    let frames: Vec<String> = (2..10).flat_map(updates).collect();
    venue.send(&frames);
    let messages = ws.until_candle(&m5, T0 + 5 * MIN).await;

    // The message's wire shape.
    let (raw, _) = messages
        .iter()
        .find(|(_, m)| matches!(m, ServerMessage::CandleForming { topic, .. } if *topic == m5))
        .expect("a forming bar");
    assert_eq!(raw["type"], "candle_forming");
    assert_eq!(raw["topic"], m5.as_str());
    assert_eq!(raw["candle"]["open_time"], T0);
    assert_eq!(
        raw.as_object().map(|o| o.len()),
        Some(3),
        "type, topic and candle: {raw}"
    );

    let five = forming(&messages, &m5);
    assert_eq!(
        five[0],
        bar(T0, 0..2, Some((2, 0))),
        "the bucket from its start"
    );
    assert!(
        five.contains(&bar(T0, 0..5, None)),
        "the first bucket ends whole"
    );
    assert_eq!(five.last(), Some(&bar(T0 + 5 * MIN, 5..9, Some((9, 1)))));
    // The closed bar comes last and replaces the forming one.
    assert_eq!(
        messages.last().map(|(_, m)| m.clone()),
        Some(ServerMessage::Candle {
            topic: m5.clone(),
            candle: bar(T0 + 5 * MIN, 5..10, None)
        })
    );

    // Each minute: its two updates as forming bars, then its closed bar.
    let one: Vec<&ServerMessage> = messages
        .iter()
        .map(|(_, m)| m)
        .filter(|m| matches!(m, ServerMessage::Candle { topic, .. } | ServerMessage::CandleForming { topic, .. } if *topic == m1))
        .collect();
    for (k, i) in (2..10).enumerate() {
        assert_eq!(
            one[3 * k..3 * k + 3],
            [
                &ServerMessage::CandleForming {
                    topic: m1.clone(),
                    candle: minute(i, 0)
                },
                &ServerMessage::CandleForming {
                    topic: m1.clone(),
                    candle: minute(i, 1)
                },
                &ServerMessage::Candle {
                    topic: m1.clone(),
                    candle: minute(i, 2)
                },
            ],
            "minute {i}"
        );
    }
    shutdown(server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paper_session_beside_a_chart_sees_closed_bars_only() {
    let dir = tempfile::tempdir().expect("temp dir");
    let venue = Exchange::start().await;
    let server = start(&dir, &venue).await;
    let client = reqwest::Client::new();
    let body = json!({
        "strategy": MANUAL_STRATEGY,
        "params": {},
        "instrument": { "venue": "binance", "symbol": "TESTUSDT", "kind": "spot" },
        "timeframe": "1m",
        "starting_cash": 10_000.0,
        "slippage_bps": 0.0,
    });
    let response = client
        .post(format!("http://127.0.0.1:{}/sessions", server.port()))
        .header(
            "authorization",
            format!("Bearer {}", server.token().as_str()),
        )
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("answers");
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Value =
        serde_json::from_str(&response.text().await.expect("a body")).expect("JSON");
    let id = created["id"].as_str().expect("an id").to_string();
    // Live once its feed is connected, so it takes every frame sent from now on.
    let deadline = Instant::now() + WAIT;
    loop {
        let text = client
            .get(format!("http://127.0.0.1:{}/sessions/{id}", server.port()))
            .header(
                "authorization",
                format!("Bearer {}", server.token().as_str()),
            )
            .send()
            .await
            .expect("answers")
            .text()
            .await
            .expect("a body");
        let state: Value = serde_json::from_str(&text).expect("JSON");
        if state["status"]["status"] == "live" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the session never went live: {state}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut ws = Ws::open(&server).await;
    let chart = format!("candles:{INSTRUMENT}:1m");
    ws.subscribe(&chart).await;
    ws.subscribe(&format!("session:{id}")).await;
    venue.until_open(1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    venue.send(&(2..5).flat_map(updates).collect::<Vec<_>>());
    let mut forming_bars = 0;
    let mut session_bars = Vec::new();
    while session_bars.last().map(|b: &Candle| b.open_time) != Some(T0 + 4 * MIN) {
        match ws.next().await.1 {
            ServerMessage::CandleForming { .. } => forming_bars += 1,
            ServerMessage::Session {
                event: SessionEvent::Bar { bar, .. },
                ..
            } => session_bars.push(bar),
            _ => {}
        }
    }
    assert_eq!(forming_bars, 6, "the chart got every update in progress");
    // The session's live bars are the closed minutes, each once, never a minute in progress.
    let live: Vec<Candle> = session_bars
        .into_iter()
        .filter(|b| b.open_time >= T0 + 2 * MIN)
        .collect();
    assert_eq!(live, [minute(2, 2), minute(3, 2), minute(4, 2)]);
    shutdown(server).await;
}
