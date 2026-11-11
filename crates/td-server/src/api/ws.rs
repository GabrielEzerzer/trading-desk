//! The WebSocket at `GET /ws`: one connection carries any number of topic subscriptions.
//!
//! Every message is a JSON text frame with a `type`. The client sends
//! `{"type":"subscribe","topic":"jobs"}` and `{"type":"unsubscribe","topic":"jobs"}`; the server
//! answers `{"type":"subscribed","topic":...}` or `{"type":"unsubscribed","topic":...}`, and sends
//! `{"type":"error","error":{"code","message"}}` for a message it cannot act on.
//!
//! Topics are strings, with any parameters after the name, separated by `:`:
//!
//! - `jobs`: every change to every job, as `{"type":"job","job":<JobInfo>}`. Right after
//!   `subscribed` the server sends the current state of every job, so a client that subscribes
//!   first and renders from these messages misses nothing. If a client falls so far behind that
//!   events were dropped, it gets the full state again the same way.
//! - `candles:<venue>:<symbol>:<kind>:<timeframe>`, for example `candles:binance:BTCUSDT:spot:1m`:
//!   each closed bar from the venue's live stream from now on, as
//!   `{"type":"candle","topic":...,"candle":<Candle>}`. A bar that the stream skipped is
//!   backfilled; the first bar of a timeframe above a minute comes once a whole bucket has been
//!   seen. Between them comes the bar still forming, for display, as
//!   `{"type":"candle_forming","topic":...,"candle":<Candle>}`: the bucket's closed minutes so far
//!   and the minute in progress, sent each time the venue updates that minute (about every two
//!   seconds) and opening at the bucket's start. A later one of the same open time replaces it,
//!   and so does the closed `candle` of that open time, which comes after it. When the topic
//!   starts after a bucket did, the bucket's earlier minutes are fetched from the venue first, so
//!   the forming bar covers the whole bucket; that first bucket gives no closed `candle`, so its
//!   last `candle_forming`, sent when its last minute closes, is whole. Paper sessions never see
//!   forming bars. Every subscriber of an instrument, and every paper session on it, shares one upstream
//!   connection, which closes when the last of them is gone. A subscriber that falls far behind
//!   skips bars. One connection may hold at most [`MAX_CANDLE_TOPICS`] `candles:` topics at once,
//!   since each instrument's first opens a venue connection and venues limit connections per IP;
//!   one more answers `too_many_topics`.
//! - `session:<id>`: one paper session. Right after `subscribed` comes its full state, as
//!   `{"type":"session_state","session":<SessionState>}`, then each change as
//!   `{"type":"session","id":...,"event":<SessionEvent>}`: funding charged, fills and order changes,
//!   then the bar that caused them, status changes, buckets the feed dropped and warnings. A manual
//!   order placed or cancelled between bars comes as an order change of its own, once it is in the
//!   session's log. Orders and fills carry a `source` (`strategy`, `manual` or `liquidation`). Every
//!   change after the state arrives exactly once. The full state comes again when the subscriber
//!   fell so far behind that events were dropped, and when the session is resumed, since a resume
//!   replays it from its log; each change after the latest state arrives exactly once. An unknown
//!   id answers `not_found`.
//!
//! Subscribing again to a topic already subscribed replaces the subscription: nothing of the old
//! one arrives after the new `subscribed`. Nothing of a topic arrives after its `unsubscribed`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use td_core::{Candle, InstrumentId, Timeframe};
use td_data::time::Month;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, watch};
use ts_rs::TS;

use super::AppState;
use super::auth::WS_TOKEN_PROTOCOL_PREFIX;
use super::data::parse_instrument;
use super::error::{ApiError, ErrorBody};
use super::jobs::JobInfo;
use super::live::{AbortOnDrop, CandleFeed, ChartBar};
use super::sessions::{SessionState, Sessions};
use crate::SessionEvent;

pub const JOBS_TOPIC: &str = "jobs";

/// Messages from `candles:` and `session:` subscriptions waiting to be sent on one connection.
const OUTGOING_BUFFER: usize = 1024;

/// The most `candles:` topics one connection may subscribe to at once. A desk shows a handful of
/// charts; the cap keeps a client from opening a venue connection per topic without limit.
pub const MAX_CANDLE_TOPICS: usize = 16;

/// A message from the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Subscribe { topic: String },
    Unsubscribe { topic: String },
}

/// A message from the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Subscribed {
        topic: String,
    },
    Unsubscribed {
        topic: String,
    },
    /// The new state of one job (`jobs` topic). Boxed: a job is much larger than the other
    /// messages.
    Job {
        job: Box<JobInfo>,
    },
    /// A closed bar (`candles:` topics).
    Candle {
        topic: String,
        candle: Candle,
    },
    /// The bar still forming (`candles:` topics), for display only: it replaces an earlier one of
    /// the same open time, and the closed `Candle` of that open time replaces it.
    CandleForming {
        topic: String,
        candle: Candle,
    },
    /// A paper session's full state (`session:` topic), first and after events were dropped.
    SessionState {
        session: Box<SessionState>,
    },
    /// A change to a paper session (`session:` topic).
    Session {
        id: String,
        event: SessionEvent,
    },
    Error {
        error: ErrorBody,
    },
}

/// `GET /ws`. The guard has already checked the token; if it came as the `td-token.` subprotocol,
/// that subprotocol is echoed back as the handshake requires.
pub(super) async fn upgrade(
    State(state): State<Arc<AppState>>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let ws = ws.map_err(|e| ApiError::new(e.status(), "bad_request", e.body_text()))?;
    let protocol = format!("{WS_TOKEN_PROTOCOL_PREFIX}{}", state.token.as_str());
    Ok(ws
        .protocols([protocol])
        .on_upgrade(move |socket| connection(socket, state)))
}

/// A `candles:` or `session:` subscription: a task forwarding its messages to the connection, and
/// the feed it keeps open. Dropping it ends both.
struct Subscription {
    /// Tells this subscription apart from an earlier one to the same topic.
    serial: u64,
    _forward: AbortOnDrop,
    _feed: Option<Arc<CandleFeed>>,
}

/// A message a forwarding task queued, with the subscription it came from.
struct Outgoing {
    topic: String,
    serial: u64,
    message: ServerMessage,
}

/// Where one subscription's forwarding task queues its messages.
struct Sink {
    topic: String,
    serial: u64,
    out: mpsc::Sender<Outgoing>,
}

impl Sink {
    /// Queues a message; false once the connection is gone.
    async fn send(&self, message: ServerMessage) -> bool {
        let outgoing = Outgoing {
            topic: self.topic.clone(),
            serial: self.serial,
            message,
        };
        self.out.send(outgoing).await.is_ok()
    }
}

/// What one connection is subscribed to.
struct Subscriptions {
    jobs: Option<broadcast::Receiver<JobInfo>>,
    topics: HashMap<String, Subscription>,
    /// Where the forwarding tasks send their messages.
    out: mpsc::Sender<Outgoing>,
    /// The serial of the latest subscription.
    serial: u64,
}

impl Subscriptions {
    /// A sink for a new subscription to `topic`.
    fn sink(&mut self, topic: &str) -> Sink {
        self.serial += 1;
        Sink {
            topic: topic.to_string(),
            serial: self.serial,
            out: self.out.clone(),
        }
    }

    /// Whether a queued message still belongs to a current subscription. One from a subscription
    /// replaced or ended since is dropped: the client has been sent the new state, or
    /// `unsubscribed`, after it was queued.
    fn is_current(&self, outgoing: &Outgoing) -> bool {
        self.topics
            .get(&outgoing.topic)
            .is_some_and(|sub| sub.serial == outgoing.serial)
    }
}

async fn connection(mut socket: WebSocket, state: Arc<AppState>) {
    let (out, mut outgoing_rx) = mpsc::channel(OUTGOING_BUFFER);
    let mut subs = Subscriptions {
        jobs: None,
        topics: HashMap::new(),
        out,
        serial: 0,
    };
    let mut shutdown = state.shutdown.clone();
    loop {
        let outgoing: Vec<ServerMessage> = tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => handle(&text, &state, &mut subs),
                Some(Ok(Message::Binary(_))) => vec![error("bad_message", "send JSON text frames")],
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
            },
            event = next_job(&mut subs.jobs) => match event {
                Ok(job) => vec![ServerMessage::Job { job: Box::new(job) }],
                Err(RecvError::Lagged(_)) => snapshot(&state),
                Err(RecvError::Closed) => {
                    subs.jobs = None;
                    continue;
                }
            },
            // The connection holds a sender, so this never ends while the loop runs.
            Some(outgoing) = outgoing_rx.recv() => {
                if !subs.is_current(&outgoing) {
                    continue;
                }
                vec![outgoing.message]
            }
            () = stopping(&mut shutdown) => {
                let close = CloseFrame {
                    code: close_code::AWAY,
                    reason: "the server is shutting down".into(),
                };
                let _ = socket.send(Message::Close(Some(close))).await;
                return;
            }
        };
        for message in outgoing {
            let Ok(text) = serde_json::to_string(&message) else {
                continue;
            };
            if socket.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
    }
}

/// Resolves once the server starts shutting down (or its state is gone).
async fn stopping(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stopping| *stopping).await;
}

/// The next job event, or never when not subscribed.
async fn next_job(jobs: &mut Option<broadcast::Receiver<JobInfo>>) -> Result<JobInfo, RecvError> {
    match jobs {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn handle(text: &str, state: &AppState, subs: &mut Subscriptions) -> Vec<ServerMessage> {
    let message: ClientMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            return vec![error(
                "bad_message",
                format!("cannot read the message: {e}"),
            )];
        }
    };
    match message {
        ClientMessage::Subscribe { topic } if topic == JOBS_TOPIC => {
            // Subscribe before taking the snapshot, so no change falls between the two.
            subs.jobs = Some(state.jobs.subscribe());
            let mut out = vec![ServerMessage::Subscribed { topic }];
            out.extend(snapshot(state));
            out
        }
        ClientMessage::Unsubscribe { topic } if topic == JOBS_TOPIC => {
            subs.jobs = None;
            vec![ServerMessage::Unsubscribed { topic }]
        }
        ClientMessage::Subscribe { topic } => match parse_topic(&topic, state) {
            Ok(Topic::Candles(id, timeframe)) => {
                let candle_topics = subs
                    .topics
                    .keys()
                    .filter(|t| t.starts_with("candles:"))
                    .count();
                if !subs.topics.contains_key(&topic) && candle_topics >= MAX_CANDLE_TOPICS {
                    return vec![error(
                        "too_many_topics",
                        format!(
                            "a connection may subscribe to at most {MAX_CANDLE_TOPICS} candles \
                             topics at once; unsubscribe from one first"
                        ),
                    )];
                }
                let (feed, bars) = state.streams.candles(&id, timeframe);
                let sink = subs.sink(&topic);
                let serial = sink.serial;
                let forward = forward_candles(topic.clone(), bars, sink);
                subs.topics.insert(
                    topic.clone(),
                    Subscription {
                        serial,
                        _forward: AbortOnDrop(tokio::spawn(forward)),
                        _feed: Some(feed),
                    },
                );
                vec![ServerMessage::Subscribed { topic }]
            }
            Ok(Topic::Session(id)) => {
                let Some((events, session)) = state.sessions.subscribe(&id) else {
                    return vec![error("not_found", format!("no paper session {id:?}"))];
                };
                let sink = subs.sink(&topic);
                let serial = sink.serial;
                let forward = forward_session(id, events, state.sessions.clone(), sink);
                subs.topics.insert(
                    topic.clone(),
                    Subscription {
                        serial,
                        _forward: AbortOnDrop(tokio::spawn(forward)),
                        _feed: None,
                    },
                );
                vec![
                    ServerMessage::Subscribed { topic },
                    ServerMessage::SessionState {
                        session: Box::new(session),
                    },
                ]
            }
            Err((code, message)) => vec![error(code, message)],
        },
        ClientMessage::Unsubscribe { topic } => match parse_topic(&topic, state) {
            Ok(_) => {
                subs.topics.remove(&topic);
                vec![ServerMessage::Unsubscribed { topic }]
            }
            Err((code, message)) => vec![error(code, message)],
        },
    }
}

enum Topic {
    Candles(InstrumentId, Timeframe),
    Session(String),
}

/// Reads a `candles:` or `session:` topic; anything else is an error code and message.
fn parse_topic(topic: &str, state: &AppState) -> Result<Topic, (&'static str, String)> {
    if let Some(id) = topic.strip_prefix("session:") {
        return Ok(Topic::Session(id.to_string()));
    }
    let Some(rest) = topic.strip_prefix("candles:") else {
        return Err(("unknown_topic", format!("no topic {topic:?}")));
    };
    let bad = || {
        (
            "bad_topic",
            format!(
                "{topic:?} is not a candles topic: use candles:venue:symbol:kind:timeframe, for \
                 example candles:binance:BTCUSDT:spot:1m"
            ),
        )
    };
    let (instrument, timeframe) = rest.rsplit_once(':').ok_or_else(bad)?;
    let id = parse_instrument(instrument).map_err(|_| bad())?;
    let timeframe: Timeframe = timeframe.parse().map_err(|_| bad())?;
    // The symbol goes into a URL or a subscription: refuse one that could not name a store folder.
    state
        .store
        .candle_path(&id, Month::containing(0))
        .map_err(|e| ("bad_topic", e.to_string()))?;
    Ok(Topic::Candles(id, timeframe))
}

/// Sends a `candles:` topic's bars, closed and forming, to the connection. A subscriber that fell
/// behind skips bars.
async fn forward_candles(topic: String, mut bars: broadcast::Receiver<ChartBar>, sink: Sink) {
    loop {
        let bar = match bars.recv().await {
            Ok(bar) => bar,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let topic = topic.clone();
        let message = match bar {
            ChartBar::Closed(candle) => ServerMessage::Candle { topic, candle },
            ChartBar::Forming(candle) => ServerMessage::CandleForming { topic, candle },
        };
        if !sink.send(message).await {
            return;
        }
    }
}

/// Sends a session's events to the connection. When it fell behind and events were dropped, or the
/// session was resumed and its old channel closed, it subscribes again and sends the full state, so
/// every change after the latest state still arrives exactly once.
async fn forward_session(
    id: String,
    mut events: broadcast::Receiver<SessionEvent>,
    sessions: Arc<Sessions>,
    sink: Sink,
) {
    loop {
        let message = match events.recv().await {
            Ok(event) => ServerMessage::Session {
                id: id.clone(),
                event,
            },
            Err(RecvError::Lagged(_) | RecvError::Closed) => {
                let Some((again, session)) = sessions.subscribe(&id) else {
                    return;
                };
                events = again;
                ServerMessage::SessionState {
                    session: Box::new(session),
                }
            }
        };
        if !sink.send(message).await {
            return;
        }
    }
}

fn snapshot(state: &AppState) -> Vec<ServerMessage> {
    state
        .jobs
        .list()
        .into_iter()
        .map(|job| ServerMessage::Job { job: Box::new(job) })
        .collect()
}

fn error(code: &str, message: impl Into<String>) -> ServerMessage {
    ServerMessage::Error {
        error: ErrorBody {
            code: code.to_string(),
            message: message.into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::sessions::EVENT_BUFFER;
    use crate::api::sessions::tests::one_session;

    /// The next message a forwarding task queued, or fails after 5 s.
    async fn next(rx: &mut mpsc::Receiver<Outgoing>) -> ServerMessage {
        let outgoing = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a message in time")
            .expect("the task is running");
        outgoing.message
    }

    fn warning(message: &str) -> SessionEvent {
        SessionEvent::Warning {
            message: message.into(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_subscriber_that_fell_behind_gets_the_full_state_then_the_changes_after_it() {
        let (_dir, sessions) = one_session();
        let (events, state) = sessions.subscribe("s").expect("a session");
        let tx = sessions.events("s").expect("a session");
        // More events than the channel holds, before the subscriber reads any.
        for i in 0..EVENT_BUFFER + 10 {
            tx.send(warning(&format!("burst {i}"))).expect("subscribed");
        }
        let (out, mut rx) = mpsc::channel(16);
        let sink = Sink {
            topic: "session:s".into(),
            serial: 1,
            out,
        };
        let forward = AbortOnDrop(tokio::spawn(forward_session(
            "s".into(),
            events,
            sessions.clone(),
            sink,
        )));
        match next(&mut rx).await {
            ServerMessage::SessionState { session } => assert_eq!(*session, state),
            other => panic!("expected the full state, got {other:?}"),
        }
        tx.send(warning("after")).expect("subscribed again");
        assert_eq!(
            next(&mut rx).await,
            ServerMessage::Session {
                id: "s".into(),
                event: warning("after"),
            }
        );
        drop(forward);
    }
}
