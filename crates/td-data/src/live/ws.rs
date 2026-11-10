// Adapted from tipping-point engine/src/market/ws.rs at 7dc6d12.

//! The WebSocket reader: one connection to a venue's stream, kept alive and reconnected with
//! capped backoff, handing text frames to a bounded channel without ever blocking on it.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::time::{Instant, interval_at, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, connect_async};

pub(crate) const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Binance pushes a kline update every two seconds and Bybit answers a ping every 20 seconds, so
/// this long with no frame at all means the connection is dead even if TCP never said so (a
/// half-open socket after a Wi-Fi drop or NAT expiry): treat it as a disconnect.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// A drop mid-handshake leaves the peer silent rather than closed, so a connect (TCP, TLS and the
/// WebSocket upgrade) still unfinished after this long is given up and retried.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// What a connection sends after it opens: a subscription, and a heartbeat text to repeat.
#[derive(Debug, Clone, Default)]
pub(crate) struct Greeting {
    pub subscribe: Option<String>,
    pub heartbeat: Option<(Duration, String)>,
}

/// What the reader sends down its channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Inbound {
    Connected,
    /// A connection attempt failed; the reader will back off and retry.
    ConnectFailed {
        error: String,
    },
    Text(String),
    /// The connection ended; the reader will reconnect.
    Disconnected {
        reason: String,
    },
}

/// Reads `url` forever, reconnecting with backoff, until the receiver is dropped.
///
/// Frames go out with `try_send`: when the channel is full the frame is dropped rather than waited
/// on, because a reader stuck on a full channel stops answering pings and loses the connection. A
/// dropped closed bar shows up later as a gap, which the feed backfills.
pub(crate) async fn run_reader(url: String, greeting: Greeting, tx: mpsc::Sender<Inbound>) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let connected = match timeout(CONNECT_TIMEOUT, connect_async(&url)).await {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(_) => Err(format!(
                "no handshake within {}s",
                CONNECT_TIMEOUT.as_secs()
            )),
        };
        let socket = match connected {
            Ok((socket, _response)) => socket,
            Err(error) => {
                if tx.send(Inbound::ConnectFailed { error }).await.is_err() {
                    return;
                }
                sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        backoff = INITIAL_BACKOFF;
        if tx.send(Inbound::Connected).await.is_err() {
            return;
        }
        let Some(reason) = read_until_closed(socket, &greeting, &tx).await else {
            return;
        };
        if tx.send(Inbound::Disconnected { reason }).await.is_err() {
            return;
        }
        sleep(backoff).await;
    }
}

/// Pumps one connection. Returns how it ended, or `None` when the receiver is gone.
async fn read_until_closed<S>(
    mut socket: WebSocketStream<S>,
    greeting: &Greeting,
    tx: &mpsc::Sender<Inbound>,
) -> Option<String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if let Some(subscribe) = &greeting.subscribe
        && let Err(error) = socket.send(Message::text(subscribe.clone())).await
    {
        return Some(format!("subscribe failed: {error}"));
    }
    // With no heartbeat the ticker still runs, once a day, and its branch is disabled.
    let every = greeting
        .heartbeat
        .as_ref()
        .map_or(Duration::from_secs(86_400), |(every, _)| *every);
    let mut heartbeat = interval_at(Instant::now() + every, every);
    let mut last_frame = Instant::now();
    loop {
        let message = tokio::select! {
            message = socket.next() => message,
            () = sleep_until(last_frame + IDLE_TIMEOUT) => {
                return Some(format!("no frames for {}s", IDLE_TIMEOUT.as_secs()));
            }
            _ = heartbeat.tick(), if greeting.heartbeat.is_some() => {
                if let Some((_, text)) = &greeting.heartbeat
                    && let Err(error) = socket.send(Message::text(text.clone())).await
                {
                    return Some(format!("heartbeat failed: {error}"));
                }
                continue;
            }
        };
        last_frame = Instant::now();
        match message {
            Some(Ok(Message::Text(text))) => match tx.try_send(Inbound::Text(text.to_string())) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Closed(_)) => return None,
            },
            Some(Ok(Message::Ping(payload))) => {
                if let Err(error) = socket.send(Message::Pong(payload)).await {
                    return Some(format!("pong failed: {error}"));
                }
            }
            Some(Ok(Message::Close(frame))) => return Some(format!("server closed: {frame:?}")),
            Some(Ok(Message::Binary(_) | Message::Pong(_) | Message::Frame(_))) => {}
            Some(Err(error)) => return Some(format!("socket error: {error}")),
            None => return Some("stream ended".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{DuplexStream, duplex};
    use tokio_tungstenite::tungstenite::protocol::Role;

    use super::*;

    /// Our end of a socket and the exchange's end, joined in memory: no network.
    async fn socket_pair() -> (WebSocketStream<DuplexStream>, WebSocketStream<DuplexStream>) {
        let (ours, theirs) = duplex(1 << 16);
        (
            WebSocketStream::from_raw_socket(ours, Role::Client, None).await,
            WebSocketStream::from_raw_socket(theirs, Role::Server, None).await,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn sends_the_subscription_and_heartbeats_and_silence_still_ends_the_connection() {
        let (socket, mut exchange) = socket_pair().await;
        let (tx, _rx) = mpsc::channel(16);
        let greeting = Greeting {
            subscribe: Some("sub".into()),
            heartbeat: Some((Duration::from_secs(25), "ping".into())),
        };
        let start = Instant::now();
        let reader = tokio::spawn(async move { read_until_closed(socket, &greeting, &tx).await });

        let mut sent = Vec::new();
        // The exchange never answers: heartbeats go out, but they are not frames received.
        while let Ok(Some(Ok(Message::Text(text)))) =
            timeout(Duration::from_secs(120), exchange.next()).await
        {
            sent.push(text.to_string());
            if sent.len() == 3 {
                break;
            }
        }
        // Pings at 25 s and 50 s; the exchange's silence ends the connection at 60 s.
        assert_eq!(sent, ["sub", "ping", "ping"]);
        let ended = timeout(Duration::from_secs(120), reader)
            .await
            .expect("still reading long after the idle timeout")
            .expect("reader task");
        assert_eq!(ended.as_deref(), Some("no frames for 60s"));
        assert_eq!(start.elapsed().as_secs(), 60);
    }
}
