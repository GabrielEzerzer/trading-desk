//! Live streams shared across the server: one upstream connection per instrument, used by every
//! `candles:` topic and paper session on that instrument, and closed once none of them is left.
//!
//! Each user holds an `Arc` of the instrument's [`SharedStream`] and the registry only a weak
//! reference, so the connection closes when the last feed on it ends. A `candles:` topic has one
//! feed per instrument and timeframe, however many WebSocket clients subscribe to it; it ends when
//! the last of them unsubscribes or disconnects.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use td_core::{Candle, InstrumentId, Timeframe};
use td_data::live::{ChartEvent, FeedEvent, SharedStream, run_chart_feed_on};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use super::source::LiveSource;

/// Messages (closed and forming bars) buffered per `candles:` subscriber; one that falls further
/// behind skips messages.
const CANDLE_BUFFER: usize = 256;

/// What a `candles:` topic sends: a closed bar, or the bar still forming, which is for display
/// only and never reaches a paper session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ChartBar {
    Closed(Candle),
    Forming(Candle),
}

/// Aborts a spawned task when dropped.
pub(crate) struct AbortOnDrop(pub(crate) JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Maps of weak references stay consistent even if a holder panicked.
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The streams and `candles:` feeds open right now.
pub(crate) struct Streams {
    live: Arc<dyn LiveSource>,
    streams: Mutex<HashMap<InstrumentId, Weak<SharedStream>>>,
    candles: Mutex<HashMap<(InstrumentId, Timeframe), Weak<CandleFeed>>>,
}

/// The feed behind one `candles:` topic. Dropping the last handle stops it.
pub(crate) struct CandleFeed {
    bars: broadcast::Sender<ChartBar>,
    _task: AbortOnDrop,
}

impl Streams {
    pub(crate) fn new(live: Arc<dyn LiveSource>) -> Self {
        Streams {
            live,
            streams: Mutex::new(HashMap::new()),
            candles: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn live(&self) -> &Arc<dyn LiveSource> {
        &self.live
    }

    /// The instrument's stream: the one already open, or a new connection.
    pub(crate) fn stream(&self, id: &InstrumentId) -> Arc<SharedStream> {
        let mut streams = lock(&self.streams);
        streams.retain(|_, stream| stream.strong_count() > 0);
        if let Some(stream) = streams.get(id).and_then(Weak::upgrade) {
            return stream;
        }
        let stream = Arc::new(SharedStream::open(id.clone(), self.live.stream_url(id)));
        streams.insert(id.clone(), Arc::downgrade(&stream));
        stream
    }

    /// Subscribes to the bars of `id` at `timeframe`, closed and forming, starting the feed if no
    /// one else is subscribed. Keep the returned handle for as long as the subscription lasts.
    pub(crate) fn candles(
        &self,
        id: &InstrumentId,
        timeframe: Timeframe,
    ) -> (Arc<CandleFeed>, broadcast::Receiver<ChartBar>) {
        let mut feeds = lock(&self.candles);
        feeds.retain(|_, feed| feed.strong_count() > 0);
        let key = (id.clone(), timeframe);
        if let Some(feed) = feeds.get(&key).and_then(Weak::upgrade) {
            let bars = feed.bars.subscribe();
            return (feed, bars);
        }
        let (bars, rx) = broadcast::channel(CANDLE_BUFFER);
        let (feed_tx, mut feed_rx) = mpsc::channel(CANDLE_BUFFER);
        // Bars from whatever minute closes first: the topic streams what happens from now on.
        let run = run_chart_feed_on(self.stream(id), timeframe, None, self.live.rest(), feed_tx);
        let out = bars.clone();
        let task = tokio::spawn(async move {
            let _feed = AbortOnDrop(tokio::spawn(run));
            while let Some(event) = feed_rx.recv().await {
                let bar = match event {
                    ChartEvent::Feed(FeedEvent::Bar(bar)) => ChartBar::Closed(bar),
                    ChartEvent::Forming(bar) => ChartBar::Forming(bar),
                    ChartEvent::Feed(_) => continue,
                };
                let _ = out.send(bar);
            }
        });
        let feed = Arc::new(CandleFeed {
            bars,
            _task: AbortOnDrop(task),
        });
        feeds.insert(key, Arc::downgrade(&feed));
        (feed, rx)
    }
}
