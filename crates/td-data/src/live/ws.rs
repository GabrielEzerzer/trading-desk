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

/// Binance pushes a kline update every two seconds and Bybit at least once a minute, so this long
/// with no frame but heartbeat answers means the connection is dead even if TCP never said so (a
/// half-open socket after a Wi-Fi drop or NAT expiry), or that the venue is not sending the topic
/// (a refused subscription, a topic that went quiet): treat it as a disconnect, so the reader
/// reconnects and subscribes again.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// A drop mid-handshake leaves the peer silent rather than closed, so a connect (TCP, TLS and the
/// WebSocket upgrade) still unfinished after this long is given up and retried.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// A connection that lasted this long resets the reconnect backoff.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);

/// What a connection sends after it opens: a subscription, and a heartbeat text to repeat.
#[derive(Debug, Clone, Default)]
pub struct Greeting {
    pub subscribe: Option<String>,
    pub heartbeat: Option<(Duration, String)>,
    /// Tells the venue's answer to the heartbeat from other text frames. Answers do not count as
    /// traffic for the idle timeout: a connection that only answers heartbeats carries no bars.
    pub pong: Option<fn(&str) -> bool>,
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
    /// This many text frames were dropped on a full channel since the last report. Sent ahead of
    /// the next frame that fits.
    Dropped(u64),
    /// The connection ended; the reader will reconnect.
    Disconnected {
        reason: String,
    },
}

/// Reads `url` forever, reconnecting with backoff, until the receiver is dropped.
///
/// Frames go out with `try_send`: when the channel is full the frame is dropped rather than waited
/// on, because a reader stuck on a full channel stops answering pings and loses the connection. A
/// dropped closed bar shows up later as a gap, which the feed backfills. The drops are counted and
/// reported as [`Inbound::Dropped`] once a frame fits again.
pub(crate) async fn run_reader(url: String, greeting: Greeting, tx: mpsc::Sender<Inbound>) {
    let mut backoff = INITIAL_BACKOFF;
    let mut dropped = 0;
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
        if tx.send(Inbound::Connected).await.is_err() {
            return;
        }
        let opened = Instant::now();
        let Some(reason) = read_until_closed(socket, &greeting, &tx, &mut dropped).await else {
            return;
        };
        if tx.send(Inbound::Disconnected { reason }).await.is_err() {
            return;
        }
        // A server that accepts and then drops at once keeps backing off; only a connection that
        // held starts the backoff over.
        if opened.elapsed() >= STABLE_CONNECTION {
            backoff = INITIAL_BACKOFF;
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Pumps one connection, adding the text frames it drops to `dropped` until they are reported.
/// Returns how it ended, or `None` when the receiver is gone.
async fn read_until_closed<S>(
    mut socket: WebSocketStream<S>,
    greeting: &Greeting,
    tx: &mpsc::Sender<Inbound>,
    dropped: &mut u64,
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
                return Some(format!("no data for {}s", IDLE_TIMEOUT.as_secs()));
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
        let pong = match (&message, greeting.pong) {
            (Some(Ok(Message::Text(text))), Some(is_pong)) => is_pong(text),
            _ => false,
        };
        if !pong {
            last_frame = Instant::now();
        }
        match message {
            Some(Ok(Message::Text(text))) => {
                // The count goes ahead of the frame; when it does not fit, neither does the frame.
                if *dropped > 0 && tx.try_send(Inbound::Dropped(*dropped)).is_ok() {
                    *dropped = 0;
                }
                match tx.try_send(Inbound::Text(text.to_string())) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => *dropped += 1,
                    Err(TrySendError::Closed(_)) => return None,
                }
            }
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
            pong: None,
        };
        let start = Instant::now();
        let reader =
            tokio::spawn(async move { read_until_closed(socket, &greeting, &tx, &mut 0).await });

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
        // Pings go out every 25 s; the exchange's silence ends the connection at 90 s.
        assert_eq!(sent, ["sub", "ping", "ping"]);
        let ended = timeout(Duration::from_secs(120), reader)
            .await
            .expect("still reading long after the idle timeout")
            .expect("reader task");
        assert_eq!(ended.as_deref(), Some("no data for 90s"));
        assert_eq!(start.elapsed().as_secs(), 90);
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_only_answers_heartbeats_still_times_out() {
        let (socket, mut exchange) = socket_pair().await;
        let (tx, mut rx) = mpsc::channel(16);
        let greeting = Greeting {
            subscribe: None,
            heartbeat: Some((Duration::from_secs(20), "ping".into())),
            pong: Some(|text| text == "pong"),
        };
        let start = Instant::now();
        let reader =
            tokio::spawn(async move { read_until_closed(socket, &greeting, &tx, &mut 0).await });
        // The exchange answers every ping, and sends one bar, at 10 s, but nothing else.
        let exchange = tokio::spawn(async move {
            sleep(Duration::from_secs(10)).await;
            exchange.send(Message::text("bar")).await.expect("send");
            while let Some(Ok(Message::Text(ping))) = exchange.next().await {
                assert_eq!(ping.as_str(), "ping");
                if exchange.send(Message::text("pong")).await.is_err() {
                    break;
                }
            }
        });
        let ended = timeout(Duration::from_secs(300), reader)
            .await
            .expect("the pongs kept the connection open")
            .expect("reader task");
        assert_eq!(ended.as_deref(), Some("no data for 90s"));
        // Ninety seconds after the bar. The pongs are still handed on.
        assert_eq!(start.elapsed().as_secs(), 100);
        assert_eq!(rx.recv().await, Some(Inbound::Text("bar".into())));
        assert_eq!(rx.recv().await, Some(Inbound::Text("pong".into())));
        exchange.abort();
    }

    #[tokio::test]
    async fn frames_dropped_on_a_full_channel_are_counted_and_reported_ahead_of_the_next_one() {
        let (socket, mut exchange) = socket_pair().await;
        let (tx, mut rx) = mpsc::channel(2);
        let reader = tokio::spawn(async move {
            read_until_closed(socket, &Greeting::default(), &tx, &mut 0).await
        });
        for text in ["a", "b", "c", "d"] {
            exchange.send(Message::text(text)).await.expect("send");
        }
        // The reader answers the ping after it has handled every frame before it.
        exchange
            .send(Message::Ping(vec![1].into()))
            .await
            .expect("ping");
        assert!(matches!(exchange.next().await, Some(Ok(Message::Pong(_)))));
        assert_eq!(rx.recv().await, Some(Inbound::Text("a".into())));
        assert_eq!(rx.recv().await, Some(Inbound::Text("b".into())));
        // "c" and "d" found the channel full.
        exchange.send(Message::text("e")).await.expect("send");
        assert_eq!(rx.recv().await, Some(Inbound::Dropped(2)));
        assert_eq!(rx.recv().await, Some(Inbound::Text("e".into())));
        reader.abort();
    }

    #[tokio::test]
    async fn a_server_that_drops_each_connection_at_once_is_retried_with_growing_waits() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("ws://{}/", listener.local_addr().expect("address"));
        let (tx, _rx) = mpsc::channel(16);
        let reader = tokio::spawn(run_reader(url, Greeting::default(), tx));
        let mut accepted = Vec::new();
        let accept_three = async {
            while accepted.len() < 3 {
                let (tcp, _) = listener.accept().await.expect("accept");
                // The handshake succeeds, then the connection is dropped.
                let socket = tokio_tungstenite::accept_async(tcp).await.expect("upgrade");
                accepted.push(Instant::now());
                drop(socket);
            }
        };
        timeout(Duration::from_secs(20), accept_three)
            .await
            .expect("three connections");
        reader.abort();
        // Waits of 1 s and then 2 s: a successful handshake alone does not reset the backoff.
        assert!(accepted[1] - accepted[0] >= Duration::from_secs(1));
        let second = accepted[2] - accepted[1];
        assert!(second >= Duration::from_secs(2), "waited {second:?}");
    }
}
