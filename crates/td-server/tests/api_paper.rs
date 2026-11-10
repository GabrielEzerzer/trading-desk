//! Paper sessions and live candles over the API: real HTTP and WebSocket against a server started
//! in-process on 127.0.0.1:0, a temporary data directory, a scripted venue stream on 127.0.0.1 and
//! a fake REST source. No network.

use std::collections::HashMap;
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
use td_engine::{ParamValue, Params};
use td_server::api::auth::Token;
use td_server::api::sessions::SessionState;
use td_server::api::source::{LiveSource, MarketSource};
use td_server::api::ws::{MAX_CANDLE_TOPICS, ServerMessage};
use td_server::api::{ApiConfig, JOB_GRACE, Server};
use td_server::{Funding, PaperConfig, PaperSession, SessionEvent};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const WAIT: Duration = Duration::from_secs(20);
const MIN: Millis = 60_000;
const HOUR: Millis = 60 * MIN;
/// 2024-01-01 00:00 UTC.
const T0: Millis = 1_704_067_200_000;
const INSTRUMENT: &str = "binance:TESTUSDT:perpetual";

fn instrument_id() -> InstrumentId {
    InstrumentId {
        venue: Venue::Binance,
        symbol: "TESTUSDT".into(),
        kind: InstrumentKind::Perpetual,
    }
}

fn instrument() -> Instrument {
    Instrument {
        id: instrument_id(),
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

fn params() -> Params {
    Params::new()
        .with("fast", ParamValue::Int(2))
        .with("slow", ParamValue::Int(5))
        .with("allow_short", ParamValue::Bool(true))
}

fn start_body() -> String {
    json!({
        "strategy": "sma_cross",
        "params": { "fast": 2, "slow": 5, "allow_short": true },
        "instrument": { "venue": "binance", "symbol": "TESTUSDT", "kind": "perpetual" },
        "timeframe": "1m",
        "starting_cash": 10_000.0,
        "slippage_bps": 5.0,
    })
    .to_string()
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

/// The funding rates the venue publishes: one before the session, to give the interval, and one
/// due in the first live bar.
const FUNDING: [(Millis, f64); 2] = [(T0 - 8 * HOUR, 0.0001), (T0, 0.0003)];

/// A closed Binance kline frame for a minute.
fn frame(c: &Candle) -> String {
    format!(
        r#"{{"e":"kline","E":1,"s":"TESTUSDT","k":{{"t":{},"i":"1m","o":"{}","h":"{}","l":"{}","c":"{}","v":"{}","q":"{}","x":true}}}}"#,
        c.open_time, c.open, c.high, c.low, c.close, c.volume, c.quote_volume
    )
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

/// Serves minutes (and bars resampled from them) and the funding rates, as of 00:00:30.
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
            resample(&self.minutes, timeframe, false).candles
        };
        Ok(bars
            .into_iter()
            .filter(|c| (from..to).contains(&c.open_time))
            .collect())
    }

    fn funding(
        &self,
        _: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        Ok(FUNDING
            .into_iter()
            .filter(|(t, _)| (from..to).contains(t))
            .collect())
    }

    fn now(&self) -> Millis {
        T0 + 30_000
    }
}

/// A scripted venue stream on 127.0.0.1: it sends each pushed frame to every open connection and
/// counts connections.
#[derive(Clone)]
struct Exchange {
    url: String,
    push: broadcast::Sender<String>,
    accepted: Arc<AtomicUsize>,
    open: Arc<AtomicUsize>,
}

impl Exchange {
    async fn start() -> Exchange {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}/", listener.local_addr().expect("address"));
        let venue = Exchange {
            url,
            push: broadcast::channel(1024).0,
            accepted: Arc::new(AtomicUsize::new(0)),
            open: Arc::new(AtomicUsize::new(0)),
        };
        let serving = venue.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let Ok(mut socket) = tokio_tungstenite::accept_async(tcp).await else {
                    continue;
                };
                let mut frames = serving.push.subscribe();
                serving.accepted.fetch_add(1, Ordering::SeqCst);
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

    fn send(&self, bars: &[Candle]) {
        for bar in bars {
            self.push.send(frame(bar)).expect("a connection is open");
        }
    }

    async fn until_open(&self, n: usize) {
        let deadline = Instant::now() + WAIT;
        while self.open.load(Ordering::SeqCst) != n {
            assert!(
                Instant::now() < deadline,
                "{} connections open, expected {n}",
                self.open.load(Ordering::SeqCst)
            );
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

    /// Every instrument streams from the one scripted venue; only the test instrument's frames are
    /// ever sent.
    fn stream_url(&self, _: &InstrumentId) -> String {
        self.venue.url.clone()
    }
}

struct Api {
    server: Server,
    client: reqwest::Client,
}

impl Api {
    async fn start(dir: &TempDir, venue: &Exchange) -> Api {
        let live = Live {
            venue: venue.clone(),
            rest: Arc::new(FakeRest {
                minutes: minutes(-60..60),
            }),
        };
        let server = Server::start(ApiConfig {
            store: Store::new(dir.path()),
            source: Arc::new(Venues),
            live: Arc::new(live),
            token: Token::generate().expect("rng"),
            port: 0,
        })
        .await
        .expect("starts");
        Api {
            server,
            client: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.server.port())
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let response = request.send().await.expect("answers");
        let status = response.status();
        let text = response.text().await.expect("a body");
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        (status, body)
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.server.token().as_str())
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let request = self
            .client
            .get(self.url(path))
            .header("authorization", self.bearer());
        self.send(request).await
    }

    async fn post(&self, path: &str, body: &str) -> (StatusCode, Value) {
        let request = self
            .client
            .post(self.url(path))
            .header("authorization", self.bearer())
            .header("content-type", "application/json")
            .body(body.to_string());
        self.send(request).await
    }

    async fn start_session(&self) -> String {
        let (status, created) = self.post("/sessions", &start_body()).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        created["id"].as_str().expect("an id").to_string()
    }

    /// Polls a session's state until `done` holds, returning it.
    async fn until(&self, id: &str, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let (status, state) = self.get(&format!("/sessions/{id}")).await;
            assert_eq!(status, StatusCode::OK, "{state}");
            assert_ne!(state["status"]["status"], "error", "{}", state["status"]);
            if done(&state) {
                return state;
            }
            assert!(Instant::now() < deadline, "session never {what}: {state}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn until_live(&self, id: &str) -> Value {
        self.until(id, "went live", |s| s["status"]["status"] == "live")
            .await
    }

    async fn until_bar(&self, id: &str, open_time: Millis) -> Value {
        self.until(id, "reached the bar", |s| {
            s["last_bar"]["open_time"].as_i64() >= Some(open_time)
        })
        .await
    }

    async fn websocket(&self) -> Ws {
        let mut request = format!("ws://127.0.0.1:{}/ws", self.server.port())
            .into_client_request()
            .expect("a request");
        request
            .headers_mut()
            .insert("authorization", self.bearer().parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("upgrades");
        Ws(ws)
    }

    async fn shutdown(self) {
        tokio::time::timeout(JOB_GRACE, self.server.shutdown())
            .await
            .expect("shuts down within the grace period")
            .expect("shuts down cleanly");
    }
}

struct Ws(WebSocketStream<MaybeTlsStream<TcpStream>>);

impl Ws {
    async fn send(&mut self, message: Value) {
        self.0
            .send(Message::Text(message.to_string().into()))
            .await
            .expect("sends");
    }

    async fn subscribe(&mut self, topic: &str) {
        self.send(json!({ "type": "subscribe", "topic": topic }))
            .await;
        assert_eq!(
            self.next().await,
            ServerMessage::Subscribed {
                topic: topic.into()
            }
        );
    }

    async fn unsubscribe(&mut self, topic: &str) {
        self.send(json!({ "type": "unsubscribe", "topic": topic }))
            .await;
        assert_eq!(
            self.next().await,
            ServerMessage::Unsubscribed {
                topic: topic.into()
            }
        );
    }

    async fn next(&mut self) -> ServerMessage {
        loop {
            let frame = tokio::time::timeout(WAIT, self.0.next())
                .await
                .expect("a message in time");
            match frame {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text).expect("a server message");
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => panic!("the socket closed"),
                Some(Ok(_)) => {}
            }
        }
    }

    /// Reads a `session:` subscription, whose last bar so far is `last`, until it has sent the live
    /// bar at `open_time`. Returns how many full states came, the minute of the latest one's last
    /// bar (0 for none) and the live bars sent after it. Fails on a bar that is not the minute
    /// after the one before it, counting the state's last bar.
    async fn session_until(
        &mut self,
        mut last: Option<Millis>,
        open_time: Millis,
    ) -> (usize, i64, Vec<Candle>) {
        let (mut states, mut from, mut bars) = (0, 0, Vec::new());
        loop {
            match self.next().await {
                ServerMessage::SessionState { session } => {
                    states += 1;
                    bars.clear();
                    last = session.last_bar.map(|b| b.open_time);
                    from = last.map_or(0, |t| (t - T0) / MIN);
                    if last >= Some(open_time) {
                        return (states, from, bars);
                    }
                }
                ServerMessage::Session {
                    event:
                        SessionEvent::Bar {
                            bar,
                            warm_up: false,
                            ..
                        },
                    ..
                } => {
                    assert_eq!(
                        Some(bar.open_time - MIN),
                        last,
                        "a bar out of turn: {bar:?} after {last:?}"
                    );
                    last = Some(bar.open_time);
                    bars.push(bar);
                    if bar.open_time >= open_time {
                        return (states, from, bars);
                    }
                }
                ServerMessage::Session { .. } => {}
                other => panic!("expected a session message, got {other:?}"),
            }
        }
    }

    /// Candles of `topic` until one opens at `open_time`.
    async fn candles_until(&mut self, topic: &str, open_time: Millis) -> Vec<Candle> {
        let mut candles = Vec::new();
        loop {
            match self.next().await {
                ServerMessage::Candle { topic: t, candle } if t == topic => {
                    candles.push(candle);
                    if candle.open_time >= open_time {
                        return candles;
                    }
                }
                other => panic!("expected a candle of {topic}, got {other:?}"),
            }
        }
    }
}

fn config() -> PaperConfig {
    PaperConfig {
        strategy: "sma_cross".into(),
        params: params(),
        instrument: instrument(),
        timeframe: Timeframe::M1,
        starting_cash: 10_000.0,
        slippage_bps: 5.0,
        fees: None,
        max_leverage: 1.0,
    }
}

/// A library session warmed up on the hour before `T0` and fed `live` minutes with the funding
/// due in each, as the server's session should have been.
fn reference(live: &[Candle]) -> (TempDir, PaperSession) {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut paper = PaperSession::create(dir.path(), "ref", config()).expect("creates");
    for bar in minutes(-60..0) {
        paper.feed(bar, Vec::new(), true).expect("warm-up bar");
    }
    for bar in live {
        let funding = FUNDING
            .iter()
            .filter(|(t, _)| (bar.open_time..bar.open_time + MIN).contains(t))
            .map(|&(time, rate)| Funding {
                time,
                rate,
                due: time,
            })
            .collect();
        paper.feed(*bar, funding, false).expect("feeds");
    }
    (dir, paper)
}

/// Asserts the API's state of a session is the reference's, apart from its id and status.
fn assert_same_state(api: &Value, reference: &PaperSession) {
    let mut expected = serde_json::to_value(SessionState::of(reference)).expect("serializes");
    let mut actual = api.clone();
    for state in [&mut expected, &mut actual] {
        let object = state.as_object_mut().expect("an object");
        object.remove("id");
        object.remove("status");
    }
    assert_eq!(actual, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_streams_its_events_matches_the_library_and_stops() {
    let dir = tempfile::tempdir().expect("temp dir");
    let venue = Exchange::start().await;
    let api = Api::start(&dir, &venue).await;
    let id = api.start_session().await;
    let mut ws = api.websocket().await;
    ws.subscribe(&format!("session:{id}")).await;
    let ServerMessage::SessionState { session: first } = ws.next().await else {
        panic!("the state comes right after subscribed");
    };
    assert_eq!(first.id, id);
    assert_eq!(first.config, config());

    api.until_live(&id).await;
    venue.until_open(1).await;
    // A candles subscriber on the session's instrument shares its connection.
    let topic = format!("candles:{INSTRUMENT}:1m");
    let mut chart = api.websocket().await;
    chart.subscribe(&topic).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    venue.send(&minutes(0..30));
    let charted = chart.candles_until(&topic, T0 + 29 * MIN).await;
    assert_eq!(charted.last(), Some(&minute(29)));
    assert_eq!(venue.accepted.load(Ordering::SeqCst), 1);

    // Every change after the first state, in order: a bar comes after the events it caused.
    let mut events = Vec::new();
    loop {
        match ws.next().await {
            ServerMessage::Session { id: from, event } => {
                assert_eq!(from, id);
                let last = matches!(&event, SessionEvent::Bar { bar, .. } if bar.open_time == T0 + 29 * MIN);
                events.push(event);
                if last {
                    break;
                }
            }
            other => panic!("expected a session event, got {other:?}"),
        }
    }
    let state = api.until_bar(&id, T0 + 29 * MIN).await;
    let (_dir, reference) = reference(&minutes(0..30));
    assert!(reference.fills().len() > 2, "the strategy trades");
    assert_same_state(&state, &reference);

    let live_bars: Vec<Candle> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Bar {
                bar,
                warm_up: false,
                equity,
            } => {
                assert!(equity.is_some());
                Some(*bar)
            }
            _ => None,
        })
        .collect();
    assert_eq!(live_bars, minutes(0..30));
    let fills: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Fill { fill } => Some(fill.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(fills, reference.fills(), "each fill once");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Order { .. }))
    );
    let funding_at = events
        .iter()
        .position(|e| matches!(e, SessionEvent::Funding { funding } if funding.due == T0))
        .expect("the funding due at T0 is charged");
    let first_bar = events
        .iter()
        .position(|e| matches!(e, SessionEvent::Bar { warm_up: false, .. }))
        .unwrap();
    assert!(funding_at < first_bar, "funding comes before its bar");

    let (status, list) = api.get("/sessions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    assert_eq!(list[0]["id"], id.as_str());
    assert_eq!(list[0]["strategy"], "sma_cross");
    assert_eq!(list[0]["instrument"]["symbol"], "TESTUSDT");
    assert_eq!(list[0]["status"]["status"], "live");
    assert_eq!(list[0]["equity"], state["equity"]);

    // The chart leaves; the session still needs the connection.
    chart.unsubscribe(&topic).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(venue.open.load(Ordering::SeqCst), 1);

    let (status, stopped) = api.post(&format!("/sessions/{id}/stop"), "").await;
    assert_eq!(status, StatusCode::OK, "{stopped}");
    assert_eq!(stopped["status"]["status"], "stopped");
    match ws.next().await {
        ServerMessage::Session {
            event: SessionEvent::Status { status },
            ..
        } => assert_eq!(status, td_server::Status::Stopped),
        other => panic!("expected the stop, got {other:?}"),
    }
    venue.until_open(0).await;
    let (status, body) = api.post(&format!("/sessions/{id}/stop"), "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "not_running");
    let (_, after) = api.get(&format!("/sessions/{id}")).await;
    assert_same_state(&after, &reference);
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_running_when_the_server_stopped_resumes_after_a_restart() {
    let dir = tempfile::tempdir().expect("temp dir");
    let venue = Exchange::start().await;
    let api = Api::start(&dir, &venue).await;
    let id = api.start_session().await;
    api.until_live(&id).await;
    venue.until_open(1).await;
    venue.send(&minutes(0..20));
    api.until_bar(&id, T0 + 19 * MIN).await;
    // Shutting down stops the session cleanly and closes its connection.
    api.shutdown().await;
    venue.until_open(0).await;

    let api = Api::start(&dir, &venue).await;
    let (_, list) = api.get("/sessions").await;
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    assert_eq!(list[0]["id"], id.as_str());
    assert_eq!(list[0]["status"]["status"], "stopped");
    let (_, replayed) = api.get(&format!("/sessions/{id}")).await;
    assert_same_state(&replayed, &reference(&minutes(0..20)).1);

    // A subscriber that stays connected through the resume, a stop and another resume.
    let topic = format!("session:{id}");
    let mut ws = api.websocket().await;
    ws.subscribe(&topic).await;
    let ServerMessage::SessionState { session: replayed } = ws.next().await else {
        panic!("the state comes right after subscribed");
    };
    assert_eq!(replayed.last_bar, Some(minute(19)));

    let (status, resumed) = api.post(&format!("/sessions/{id}/resume"), "").await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    let (status, body) = api.post(&format!("/sessions/{id}/resume"), "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "already_running");
    // The resumed session was replayed from its log: the subscriber gets its state first.
    match ws.next().await {
        ServerMessage::SessionState { session } => {
            assert_eq!(session.last_bar, Some(minute(19)));
        }
        other => panic!("expected the resumed session's state, got {other:?}"),
    }
    api.until_live(&id).await;
    venue.until_open(1).await;
    // Minutes 20 to 24 went by while it was stopped: the feed backfills them.
    venue.send(&minutes(25..40));
    let (states, _, bars) = ws.session_until(Some(T0 + 19 * MIN), T0 + 39 * MIN).await;
    assert_eq!((states, bars), (0, minutes(20..40)), "each live bar once");
    let state = api.until_bar(&id, T0 + 39 * MIN).await;
    assert_same_state(&state, &reference(&minutes(0..40)).1);

    let (status, stopped) = api.post(&format!("/sessions/{id}/stop"), "").await;
    assert_eq!(status, StatusCode::OK, "{stopped}");
    venue.until_open(0).await;
    let (status, resumed) = api.post(&format!("/sessions/{id}/resume"), "").await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    api.until_live(&id).await;
    venue.until_open(1).await;
    venue.send(&minutes(45..50));
    // The stop shows, then the state of the session resumed again, then minutes 40 to 49 once.
    let (states, from, bars) = ws.session_until(Some(T0 + 39 * MIN), T0 + 49 * MIN).await;
    assert_eq!(states, 1);
    assert!(
        from >= 39,
        "the state is the resumed session's, at minute {from}"
    );
    assert_eq!(
        bars,
        minutes(from + 1..50),
        "each live bar after the state once"
    );
    let state = api.until_bar(&id, T0 + 49 * MIN).await;
    assert_same_state(&state, &reference(&minutes(0..50)).1);
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_an_old_subscription_queued_arrives_after_a_resubscribe_or_an_unsubscribe() {
    const CYCLES: usize = 100;
    let dir = tempfile::tempdir().expect("temp dir");
    let venue = Exchange::start().await;
    let api = Api::start(&dir, &venue).await;
    let id = api.start_session().await;
    api.until_live(&id).await;
    venue.until_open(1).await;
    let session = format!("session:{id}");
    let candles = format!("candles:{INSTRUMENT}:1m");
    let mut ws = api.websocket().await;
    // Bars keep coming while the client leaves and rejoins both topics over and over without
    // reading, as a React app in StrictMode does on mount: a bar every other round.
    for round in 0..CYCLES {
        if round % 2 == 0 {
            venue.send(&[minute(round as i64 / 2)]);
        }
        // Each round leaves and rejoins both, and subscribes to each again on top.
        for topic in [&session, &candles] {
            for kind in ["unsubscribe", "subscribe", "subscribe"] {
                ws.send(json!({ "type": kind, "topic": topic })).await;
            }
        }
    }

    // Every message, checked: nothing of a topic between its unsubscribed and its next
    // subscribed, and after each session state, each later bar once and in turn.
    let mut subscribed: HashMap<String, usize> = HashMap::new();
    let mut open: HashMap<String, bool> = HashMap::new();
    let mut last_bar: Option<Millis> = None;
    let mut last_candle: Option<Millis> = None;
    loop {
        let message = ws.next().await;
        match &message {
            ServerMessage::Subscribed { topic } => {
                *subscribed.entry(topic.clone()).or_default() += 1;
                open.insert(topic.clone(), true);
                if *topic == candles {
                    last_candle = None;
                }
            }
            ServerMessage::Unsubscribed { topic } => {
                open.insert(topic.clone(), false);
            }
            ServerMessage::SessionState { session: state } => {
                assert_eq!(
                    open.get(&session),
                    Some(&true),
                    "a state after unsubscribed"
                );
                last_bar = state.last_bar.map(|b| b.open_time);
            }
            ServerMessage::Session { event, .. } => {
                assert_eq!(
                    open.get(&session),
                    Some(&true),
                    "{event:?} after unsubscribed"
                );
                if let SessionEvent::Bar {
                    bar,
                    warm_up: false,
                    ..
                } = event
                {
                    assert_eq!(
                        Some(bar.open_time - MIN),
                        last_bar,
                        "a bar out of turn: {bar:?}"
                    );
                    last_bar = Some(bar.open_time);
                }
            }
            ServerMessage::Candle { topic, candle } => {
                assert_eq!(*topic, candles);
                assert_eq!(
                    open.get(&candles),
                    Some(&true),
                    "{candle:?} after unsubscribed"
                );
                assert!(
                    last_candle < Some(candle.open_time),
                    "{candle:?} again or out of order after {last_candle:?}"
                );
                last_candle = Some(candle.open_time);
            }
            other => panic!("unexpected {other:?}"),
        }
        let rejoined = |topic: &str| subscribed.get(topic) == Some(&(2 * CYCLES));
        if rejoined(&session) && rejoined(&candles) && last_bar >= Some(T0 + 49 * MIN) {
            break;
        }
    }
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_holds_a_limited_number_of_candle_topics() {
    let dir = tempfile::tempdir().expect("temp dir");
    let venue = Exchange::start().await;
    let api = Api::start(&dir, &venue).await;
    let topic = |i: usize| format!("candles:binance:T{i}USDT:spot:1m");
    let mut ws = api.websocket().await;
    for i in 0..MAX_CANDLE_TOPICS {
        ws.subscribe(&topic(i)).await;
    }
    ws.send(json!({ "type": "subscribe", "topic": topic(MAX_CANDLE_TOPICS) }))
        .await;
    match ws.next().await {
        ServerMessage::Error { error } => {
            assert_eq!(error.code, "too_many_topics");
            assert!(error.message.contains("unsubscribe"), "{}", error.message);
        }
        other => panic!("expected too_many_topics, got {other:?}"),
    }
    // Subscribing again to one it holds replaces it; leaving one makes room for another.
    ws.subscribe(&topic(0)).await;
    ws.unsubscribe(&topic(0)).await;
    ws.subscribe(&topic(MAX_CANDLE_TOPICS)).await;
    // Other topics do not count.
    ws.subscribe("jobs").await;
    venue.until_open(MAX_CANDLE_TOPICS).await;
    drop(ws);
    venue.until_open(0).await;
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn candle_subscribers_share_one_upstream_connection_that_closes_when_they_leave() {
    let dir = tempfile::tempdir().expect("temp dir");
    let venue = Exchange::start().await;
    let api = Api::start(&dir, &venue).await;
    let topic = format!("candles:{INSTRUMENT}:1m");
    let mut first = api.websocket().await;
    let mut second = api.websocket().await;
    first.subscribe(&topic).await;
    second.subscribe(&topic).await;
    // Another timeframe of the same instrument uses the same connection.
    second.subscribe(&format!("candles:{INSTRUMENT}:5m")).await;
    venue.until_open(1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    venue.send(&minutes(0..4));
    let a = first.candles_until(&topic, T0 + 3 * MIN).await;
    let b = second.candles_until(&topic, T0 + 3 * MIN).await;
    assert_eq!(a, b);
    assert_eq!(a.last(), Some(&minute(3)));
    assert_eq!(venue.accepted.load(Ordering::SeqCst), 1);

    first.unsubscribe(&topic).await;
    second.unsubscribe(&topic).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(venue.open.load(Ordering::SeqCst), 1, "the 5m topic is left");
    // The last subscriber disconnects without unsubscribing.
    drop(second);
    venue.until_open(0).await;
    assert_eq!(venue.accepted.load(Ordering::SeqCst), 1);
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_sessions_are_404_and_bad_starts_are_400() {
    let dir = tempfile::tempdir().expect("temp dir");
    let venue = Exchange::start().await;
    let api = Api::start(&dir, &venue).await;
    for (status, body) in [
        api.get("/sessions/nope").await,
        api.post("/sessions/nope/stop", "").await,
        api.post("/sessions/nope/resume", "").await,
    ] {
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "not_found");
    }
    let mut ws = api.websocket().await;
    ws.send(json!({ "type": "subscribe", "topic": "session:nope" }))
        .await;
    match ws.next().await {
        ServerMessage::Error { error } => assert_eq!(error.code, "not_found"),
        other => panic!("expected not_found, got {other:?}"),
    }

    let mut body: Value = serde_json::from_str(&start_body()).unwrap();
    let bad = |change: &dyn Fn(&mut Value)| {
        let mut b = body.clone();
        change(&mut b);
        b.to_string()
    };
    let cases = [
        bad(&|b| b["strategy"] = json!("nope")),
        bad(&|b| b["instrument"]["symbol"] = json!("OTHERUSDT")),
        bad(&|b| b["starting_cash"] = json!(-1.0)),
        bad(&|b| b["params"]["fast"] = json!("two")),
        "{not json".to_string(),
    ];
    for case in cases {
        let (status, answer) = api.post("/sessions", &case).await;
        assert!(status.is_client_error(), "{case}: {status} {answer}");
        assert!(answer["error"]["message"].is_string(), "{answer}");
    }
    body["timeframe"] = json!("2m");
    let (status, _) = api.post("/sessions", &body.to_string()).await;
    assert!(status.is_client_error());
    assert_eq!(api.get("/sessions").await.1, json!([]), "nothing started");
    assert_eq!(venue.accepted.load(Ordering::SeqCst), 0);
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn session_routes_and_the_websocket_still_need_the_token() {
    let dir = tempfile::tempdir().expect("temp dir");
    let venue = Exchange::start().await;
    let api = Api::start(&dir, &venue).await;
    let id = api.start_session().await;
    let requests = [
        api.client.get(api.url("/sessions")),
        api.client.post(api.url("/sessions")).body(start_body()),
        api.client.get(api.url(&format!("/sessions/{id}"))),
        api.client.post(api.url(&format!("/sessions/{id}/stop"))),
        api.client.post(api.url(&format!("/sessions/{id}/resume"))),
    ];
    for request in requests {
        let (status, body) = api.send(request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(body["error"]["code"], "unauthorized");
    }
    // Topic subscriptions only travel over an upgraded socket, and the upgrade needs the token.
    let request = format!("ws://127.0.0.1:{}/ws", api.server.port())
        .into_client_request()
        .unwrap();
    match tokio_tungstenite::connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), 401);
        }
        other => panic!("expected a 401, got {:?}", other.map(|(_, r)| r.status())),
    }
    let (_, list) = api.get("/sessions").await;
    assert_eq!(list.as_array().map(Vec::len), Some(1), "no second session");
    assert_ne!(
        list[0]["status"]["status"], "stopped",
        "the refused stop did nothing"
    );
    api.shutdown().await;
}
