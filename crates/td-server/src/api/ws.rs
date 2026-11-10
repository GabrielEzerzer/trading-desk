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
//! - `candles:<venue>:<symbol>:<kind>:<timeframe>` (later, with live feeds): closed and forming
//!   bars, as `{"type":"candle",...}`.
//! - `session:<id>` (later, with paper sessions): fills, orders and equity of one paper session,
//!   as `{"type":"session",...}`.
//!
//! Subscribing to a topic that is not available yet answers `topic_unavailable`.

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};
use ts_rs::TS;

use super::AppState;
use super::auth::WS_TOKEN_PROTOCOL_PREFIX;
use super::error::{ApiError, ErrorBody};
use super::jobs::JobInfo;

pub const JOBS_TOPIC: &str = "jobs";

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

async fn connection(mut socket: WebSocket, state: Arc<AppState>) {
    let mut jobs: Option<broadcast::Receiver<JobInfo>> = None;
    let mut shutdown = state.shutdown.clone();
    loop {
        let outgoing: Vec<ServerMessage> = tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => handle(&text, &state, &mut jobs),
                Some(Ok(Message::Binary(_))) => vec![error("bad_message", "send JSON text frames")],
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
            },
            event = next_job(&mut jobs) => match event {
                Ok(job) => vec![ServerMessage::Job { job: Box::new(job) }],
                Err(RecvError::Lagged(_)) => snapshot(&state),
                Err(RecvError::Closed) => {
                    jobs = None;
                    continue;
                }
            },
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

fn handle(
    text: &str,
    state: &AppState,
    jobs: &mut Option<broadcast::Receiver<JobInfo>>,
) -> Vec<ServerMessage> {
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
            *jobs = Some(state.jobs.subscribe());
            let mut out = vec![ServerMessage::Subscribed { topic }];
            out.extend(snapshot(state));
            out
        }
        ClientMessage::Unsubscribe { topic } if topic == JOBS_TOPIC => {
            *jobs = None;
            vec![ServerMessage::Unsubscribed { topic }]
        }
        ClientMessage::Subscribe { topic } | ClientMessage::Unsubscribe { topic } => {
            let name = topic.split(':').next().unwrap_or_default();
            if matches!(name, "candles" | "session") {
                vec![error(
                    "topic_unavailable",
                    format!("the {name:?} topic is not available yet"),
                )]
            } else {
                vec![error("unknown_topic", format!("no topic {topic:?}"))]
            }
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
