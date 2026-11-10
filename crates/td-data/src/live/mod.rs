//! Live candle feeds from Binance's and Bybit's public WebSocket streams.
//!
//! A feed reads a venue's 1-minute kline stream, keeps only closed bars (Binance `k.x`, Bybit
//! `confirm`), checks each with [`Candle::validate`], skips duplicates, and backfills over REST any
//! minutes the stream skipped, after a reconnect or a dropped frame, before it takes a newer one.
//! It turns the minutes into bars of the session's timeframe and emits a bar once its bucket is
//! settled, by the same resampling rule as the store ([`crate::resample`]).
//!
//! One connection can serve many feeds: a [`SharedStream`] reads an instrument's stream once and
//! every feed run on it with [`run_feed_on`] builds its own bars, from its own start, out of the
//! same frames.
//!
//! The feed never asks the clock: a gap is noticed when the next closed minute arrives, so after a
//! reconnect the missing minutes are backfilled when the first closed minute comes in, at most a
//! minute later. By then REST has published every minute before it.

mod bars;
mod parse;
mod ws;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use td_core::{Candle, InstrumentId, InstrumentKind, Millis, Timeframe, Venue};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc};

pub use bars::{BarBuilder, Placement};
pub use parse::{KlineUpdate, parse_binance, parse_bybit, parse_message};

use crate::time::{MINUTE, now_ms};
use crate::{DataError, Http, LoadedCandles, binance, bybit};
use ws::{Greeting, INITIAL_BACKOFF, Inbound, MAX_BACKOFF};

/// How many frames may wait for the feed while it backfills; more are dropped and recovered as a
/// gap.
const FRAME_BUFFER: usize = 1024;
/// Bybit closes a public connection that sends nothing for a while; it asks for a ping every 20 s.
const BYBIT_PING_EVERY: Duration = Duration::from_secs(20);

/// The venue's REST data a live feed and a paper session need. [`VenueRest`] is the real one; tests
/// use a fake.
///
/// The calls block, so async code runs them with `tokio::task::spawn_blocking`.
pub trait Rest: Send + Sync {
    /// Closed bars of `timeframe` with open times in `[from, to)`, oldest first.
    fn klines(
        &self,
        id: &InstrumentId,
        timeframe: Timeframe,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<Candle>, DataError>;

    /// A perpetual's funding rates `(funding time, rate)` in `[from, to)`, oldest first.
    fn funding(
        &self,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError>;

    /// The current time, as the venue's data should be read at. A fake sets its own.
    fn now(&self) -> Millis {
        now_ms()
    }
}

/// Binance's and Bybit's public REST APIs.
///
/// shortcut: a new HTTP client per call, so the blocking client is always made and dropped on a
/// blocking thread, never inside the async runtime (where reqwest's blocking client panics on drop).
/// Calls are a few per hour; reuse a client if that changes.
#[derive(Debug, Clone, Copy, Default)]
pub struct VenueRest;

impl Rest for VenueRest {
    fn klines(
        &self,
        id: &InstrumentId,
        timeframe: Timeframe,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<Candle>, DataError> {
        let http = Http::new()?;
        match id.venue {
            Venue::Binance => binance::fetch_klines(&http, id, timeframe, from, to),
            Venue::Bybit => bybit::fetch_klines(&http, id, timeframe, from, to),
        }
    }

    fn funding(
        &self,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        if id.kind != InstrumentKind::Perpetual {
            return Err(DataError::NotPerpetual(id.to_string()));
        }
        let http = Http::new()?;
        match id.venue {
            Venue::Binance => binance::fetch_funding(&http, &id.symbol, from, to),
            Venue::Bybit => bybit::fetch_funding(&http, &id.symbol, from, to),
        }
    }
}

/// The public kline stream for an instrument's 1-minute bars.
pub fn stream_url(id: &InstrumentId) -> String {
    let lower = id.symbol.to_lowercase();
    match (id.venue, id.kind) {
        (Venue::Binance, InstrumentKind::Spot) => {
            format!("wss://stream.binance.com:9443/ws/{lower}@kline_1m")
        }
        // USD-M serves market streams under `/market`; the old `/ws/<stream>` path still accepts
        // the connection but sent no frames when checked on 2026-11-10.
        (Venue::Binance, InstrumentKind::Perpetual) => {
            format!("wss://fstream.binance.com/market/ws/{lower}@kline_1m")
        }
        (Venue::Bybit, InstrumentKind::Spot) => "wss://stream.bybit.com/v5/public/spot".into(),
        (Venue::Bybit, InstrumentKind::Perpetual) => {
            "wss://stream.bybit.com/v5/public/linear".into()
        }
    }
}

/// What a connection sends once it opens: Bybit needs a subscription and a ping every 20 s;
/// Binance's stream is named in the URL and the server does the pinging.
fn greeting(id: &InstrumentId) -> Greeting {
    match id.venue {
        Venue::Binance => Greeting::default(),
        Venue::Bybit => Greeting {
            subscribe: Some(format!(
                r#"{{"op":"subscribe","args":["kline.1.{}"]}}"#,
                id.symbol
            )),
            heartbeat: Some((BYBIT_PING_EVERY, r#"{"op":"ping"}"#.into())),
        },
    }
}

/// Picks the process-wide TLS crypto provider once, so that a second provider compiled in by
/// another dependency cannot make rustls panic on its first connection.
// Adapted from tipping-point engine/src/market/live.rs at 7dc6d12 (`install_crypto_provider`).
pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Which stream to read and from which minute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedSpec {
    pub id: InstrumentId,
    pub timeframe: Timeframe,
    /// The open time of the first minute wanted, on a timeframe boundary: everything from here on
    /// is backfilled if the stream does not carry it. `None` starts with whatever closes first.
    pub start: Option<Millis>,
    pub url: String,
}

impl FeedSpec {
    /// A feed of the venue's own stream for `id`.
    pub fn new(id: InstrumentId, timeframe: Timeframe, start: Option<Millis>) -> Self {
        let url = stream_url(&id);
        FeedSpec {
            id,
            timeframe,
            start,
            url,
        }
    }
}

/// What a feed reports.
#[derive(Debug, Clone, PartialEq)]
pub enum FeedEvent {
    /// The stream is connected.
    Connected,
    /// The stream dropped; the feed is reconnecting.
    Disconnected { reason: String },
    /// A connection attempt failed; the feed will retry after a backoff.
    ConnectFailed { error: String },
    /// A completed bar of the feed's timeframe. Bars come in order, each later than the last.
    Bar(Candle),
    /// The bucket opening at this time had too many minutes missing and gives no bar.
    Dropped(Millis),
    /// A REST backfill failed; the feed retries it after a backoff and emits nothing newer
    /// meanwhile.
    BackfillFailed { error: String },
    /// A frame that did not read, or a kline that failed validation. It is skipped.
    BadMessage { error: String },
}

/// Runs a feed on its own connection until `tx`'s receiver is dropped.
pub async fn run_feed(spec: FeedSpec, rest: Arc<dyn Rest>, tx: mpsc::Sender<FeedEvent>) {
    let stream = Arc::new(SharedStream::open(spec.id, spec.url));
    run_feed_on(stream, spec.timeframe, spec.start, rest, tx).await;
}

/// One connection to an instrument's 1-minute stream, read once and handed to any number of feeds
/// ([`run_feed_on`]). It reconnects on its own, and closes when the last handle is dropped. Open it
/// from inside a tokio runtime.
pub struct SharedStream {
    id: InstrumentId,
    frames: broadcast::Sender<Inbound>,
    /// Whether the connection is up, so a feed that joins later still learns it is connected.
    connected: Arc<AtomicBool>,
    _reader: AbortOnDrop,
}

impl SharedStream {
    /// Connects to `url`, which serves `id`'s venue's message format.
    pub fn open(id: InstrumentId, url: String) -> SharedStream {
        install_crypto_provider();
        let (frames, _) = broadcast::channel(FRAME_BUFFER);
        let connected = Arc::new(AtomicBool::new(false));
        let (reader_tx, mut reader_rx) = mpsc::channel(FRAME_BUFFER);
        let (out, flag, greeting) = (frames.clone(), connected.clone(), greeting(&id));
        let reader = tokio::spawn(async move {
            let _reader = AbortOnDrop(tokio::spawn(ws::run_reader(url, greeting, reader_tx)));
            while let Some(inbound) = reader_rx.recv().await {
                match inbound {
                    Inbound::Connected => flag.store(true, Ordering::SeqCst),
                    Inbound::Disconnected { .. } | Inbound::ConnectFailed { .. } => {
                        flag.store(false, Ordering::SeqCst);
                    }
                    Inbound::Text(_) => {}
                }
                // No feed listening is fine; one that falls behind loses frames, recovered as a gap.
                let _ = out.send(inbound);
            }
        });
        SharedStream {
            id,
            frames,
            connected,
            _reader: AbortOnDrop(reader),
        }
    }

    pub fn id(&self) -> &InstrumentId {
        &self.id
    }
}

/// Runs a feed of `timeframe` bars from `start` (see [`FeedSpec::start`]) on a shared stream,
/// until `tx`'s receiver is dropped. The feed keeps the stream open while it runs.
pub async fn run_feed_on(
    stream: Arc<SharedStream>,
    timeframe: Timeframe,
    start: Option<Millis>,
    rest: Arc<dyn Rest>,
    tx: mpsc::Sender<FeedEvent>,
) {
    let mut frames = stream.frames.subscribe();
    let mut feed = Feed {
        builder: BarBuilder::new(timeframe, start),
        id: stream.id.clone(),
        rest,
        tx,
    };
    // Subscribed first, so a connect that happens now is seen at worst twice, never missed.
    if stream.connected.load(Ordering::SeqCst) && feed.send(FeedEvent::Connected).await.is_err() {
        return;
    }
    loop {
        let inbound = match frames.recv().await {
            Ok(inbound) => inbound,
            // The frames it missed show up as a gap at the next closed minute, which is backfilled.
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        };
        if feed.handle(inbound).await.is_err() {
            break;
        }
    }
}

/// Aborts a spawned task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The receiver of a feed's events is gone.
struct Gone;

struct Feed {
    id: InstrumentId,
    builder: BarBuilder,
    rest: Arc<dyn Rest>,
    tx: mpsc::Sender<FeedEvent>,
}

impl Feed {
    async fn handle(&mut self, inbound: Inbound) -> Result<(), Gone> {
        match inbound {
            Inbound::Connected => self.send(FeedEvent::Connected).await,
            Inbound::ConnectFailed { error } => self.send(FeedEvent::ConnectFailed { error }).await,
            Inbound::Disconnected { reason } => self.send(FeedEvent::Disconnected { reason }).await,
            Inbound::Text(text) => match parse_message(self.id.venue, &text) {
                Err(e) => {
                    let error = e.to_string();
                    self.send(FeedEvent::BadMessage { error }).await
                }
                Ok(updates) => {
                    for update in updates.into_iter().filter(|u| u.closed) {
                        self.closed_minute(update.candle).await?;
                    }
                    Ok(())
                }
            },
        }
    }

    /// Takes a closed minute from the stream, backfilling any minutes before it first.
    async fn closed_minute(&mut self, minute: Candle) -> Result<(), Gone> {
        if let Err(error) = check_minute(&minute) {
            return self.send(FeedEvent::BadMessage { error }).await;
        }
        match self.builder.place(minute.open_time) {
            Placement::Stale => return Ok(()),
            Placement::Gap { from, to } => self.backfill(from, to).await?,
            Placement::Next => {}
        }
        let built = self.builder.push(minute);
        self.emit(built).await
    }

    /// Fetches the minutes `[from, to)` over REST and takes them, retrying with backoff until the
    /// venue answers. Minutes the venue does not have are treated as missing.
    async fn backfill(&mut self, from: Millis, to: Millis) -> Result<(), Gone> {
        let mut backoff = INITIAL_BACKOFF;
        let minutes = loop {
            let rest = Arc::clone(&self.rest);
            let id = self.id.clone();
            let fetched =
                tokio::task::spawn_blocking(move || rest.klines(&id, Timeframe::M1, from, to))
                    .await;
            let error = match fetched {
                Ok(Ok(minutes)) => break minutes,
                Ok(Err(e)) => e.to_string(),
                Err(e) => format!("the backfill task failed: {e}"),
            };
            self.send(FeedEvent::BackfillFailed { error }).await?;
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        };
        let mut minutes: Vec<Candle> = minutes
            .into_iter()
            .filter(|m| (from..to).contains(&m.open_time))
            .collect();
        minutes.sort_by_key(|m| m.open_time);
        for minute in minutes {
            if let Err(error) = check_minute(&minute) {
                self.send(FeedEvent::BadMessage { error }).await?;
                continue;
            }
            if let Placement::Gap { to: next, .. } = self.builder.place(minute.open_time) {
                // The venue has no bar for these minutes either.
                let built = self.builder.settle(next);
                self.emit(built).await?;
            }
            let built = self.builder.push(minute);
            self.emit(built).await?;
        }
        let built = self.builder.settle(to);
        self.emit(built).await
    }

    /// Sends the bars and dropped buckets in time order.
    async fn emit(&self, built: LoadedCandles) -> Result<(), Gone> {
        let mut events: Vec<(Millis, FeedEvent)> = built
            .candles
            .into_iter()
            .map(|bar| (bar.open_time, FeedEvent::Bar(bar)))
            .chain(
                built
                    .dropped
                    .into_iter()
                    .map(|t| (t, FeedEvent::Dropped(t))),
            )
            .collect();
        events.sort_by_key(|(t, _)| *t);
        for (_, event) in events {
            self.send(event).await?;
        }
        Ok(())
    }

    async fn send(&self, event: FeedEvent) -> Result<(), Gone> {
        self.tx.send(event).await.map_err(|_| Gone)
    }
}

/// A minute must be a valid bar that opens on a minute.
fn check_minute(minute: &Candle) -> Result<(), String> {
    minute.validate().map_err(|e| e.to_string())?;
    if minute.open_time.rem_euclid(MINUTE) != 0 {
        return Err(format!("open time {} is not on a minute", minute.open_time));
    }
    Ok(())
}
