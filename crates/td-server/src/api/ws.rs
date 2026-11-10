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
//!   seen. Every subscriber of an instrument, and every paper session on it, shares one upstream
//!   connection, which closes when the last of them is gone. A subscriber that falls far behind
//!   skips bars.
//! - `session:<id>`: one paper session. Right after `subscribed` comes its full state, as
//!   `{"type":"session_state","session":<SessionState>}`, then each change as
//!   `{"type":"session","id":...,"event":<SessionEvent>}`: funding charged, fills and order changes,
//!   then the bar that caused them, and status changes. Every change after the state arrives
//!   exactly once; a subscriber that falls so far behind that events were dropped gets the full
//!   state again. An unknown id answers `not_found`.
//!
//! Subscribing again to a topic already subscribed replaces the subscription.

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
use super::live::{AbortOnDrop, CandleFeed};
use super::sessions::{SessionState, Sessions};
use crate::SessionEvent;

pub const JOBS_TOPIC: &str = "jobs";

/// Messages from `candles:` and `session:` subscriptions waiting to be sent on one connection.
const OUTGOING_BUFFER: usize = 1024;

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
    _forward: AbortOnDrop,
    _feed: Option<Arc<CandleFeed>>,
}

/// What one connection is subscribed to.
struct Subscriptions {
    jobs: Option<broadcast::Receiver<JobInfo>>,
    topics: HashMap<String, Subscription>,
    /// Where the forwarding tasks send their messages.
    out: mpsc::Sender<ServerMessage>,
}

async fn connection(mut socket: WebSocket, state: Arc<AppState>) {
    let (out, mut outgoing_rx) = mpsc::channel(OUTGOING_BUFFER);
    let mut subs = Subscriptions {
        jobs: None,
        topics: HashMap::new(),
        out,
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
            Some(message) = outgoing_rx.recv() => vec![message],
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
                let (feed, bars) = state.streams.candles(&id, timeframe);
                let forward = forward_candles(topic.clone(), bars, subs.out.clone());
                subs.topics.insert(
                    topic.clone(),
                    Subscription {
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
                let forward = forward_session(id, events, state.sessions.clone(), subs.out.clone());
                subs.topics.insert(
                    topic.clone(),
                    Subscription {
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

/// Sends a `candles:` topic's bars to the connection. A subscriber that fell behind skips bars.
async fn forward_candles(
    topic: String,
    mut bars: broadcast::Receiver<Candle>,
    out: mpsc::Sender<ServerMessage>,
) {
    loop {
        let candle = match bars.recv().await {
            Ok(candle) => candle,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let topic = topic.clone();
        if out
            .send(ServerMessage::Candle { topic, candle })
            .await
            .is_err()
        {
            return;
        }
    }
}

/// Sends a session's events to the connection. One that fell behind subscribes again and gets the
/// full state, so it still sees every change exactly once.
async fn forward_session(
    id: String,
    mut events: broadcast::Receiver<SessionEvent>,
    sessions: Arc<Sessions>,
    out: mpsc::Sender<ServerMessage>,
) {
    loop {
        let message = match events.recv().await {
            Ok(event) => ServerMessage::Session {
                id: id.clone(),
                event,
            },
            Err(RecvError::Lagged(_)) => {
                let Some((again, session)) = sessions.subscribe(&id) else {
                    return;
                };
                events = again;
                ServerMessage::SessionState {
                    session: Box::new(session),
                }
            }
            Err(RecvError::Closed) => return,
        };
        if out.send(message).await.is_err() {
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
