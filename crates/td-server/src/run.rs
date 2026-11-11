//! Running a paper session live: warm-up, then each completed bar from the feed, with the funding
//! that falls in it, goes to the engine session.
//!
//! Resume: [`PaperSession::open`] replays the log, then [`start`] runs the feed from the end of
//! the last bar fed. A session made by another engine revision ends at once with an error (see
//! [`PaperSession::check_resumable`]). The feed backfills every minute missed while the session was stopped over
//! REST and the session processes those bars, funding included, as if it had stayed connected.
//! Orders that were open when it stopped can fill on those bars. That keeps a resumed session
//! identical to one that never stopped, at the cost of trades it could not really have made while
//! it was down.
//!
//! A session that has not fed a live bar yet (it stopped or crashed during warm-up, or after it
//! but before its first live bar) warms up again instead: it is fed, as warm-up, the bars from the
//! end of its log up to the last closed bar, at most [`WARM_UP_BARS`] of them, and goes live from
//! the time it restarts. One restarted long after its log ends skips the bars in between, which the
//! engine allows. It holds no position or orders then, so nothing is lost, but one stopped right
//! after warm-up goes live at the restart rather than backfilling the bars it missed.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use td_core::{Candle, InstrumentId, Millis, Timeframe};
use td_data::live::{FeedEvent, Rest, SharedStream, run_feed_on, stream_url};
use td_data::time::round_to_minute;
use td_data::{DataError, Store};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::paper::{Funding, Status};
use crate::{PaperError, PaperSession};

/// How many bars of history the strategy sees before it trades: more than the longest lookback a
/// registered strategy's parameters are expected to use.
pub const WARM_UP_BARS: i64 = 500;

const HOUR: Millis = 3_600_000;
/// How far back to look for funding rates when going live, to learn the venue's funding interval.
const FUNDING_LOOKBACK: Millis = 72 * HOUR;
/// The interval assumed until two rates have been seen: both venues' default.
const DEFAULT_FUNDING_INTERVAL: Millis = 8 * HOUR;
/// When a rate due in a bar is not published yet as the bar closes, ask this many more times...
const FUNDING_RETRIES: u32 = 3;
/// ...this far apart. After that it is charged on a later bar, with its due time recorded.
const FUNDING_RETRY_DELAY: Duration = Duration::from_secs(5);

/// A running paper session. Dropping the handle without [`PaperHandle::stop`] also stops it, without
/// waiting.
pub struct PaperHandle {
    session: Arc<Mutex<PaperSession>>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
    /// Turns true once the session has ended and its final status is set.
    ended: watch::Receiver<bool>,
}

impl PaperHandle {
    /// The session, to read its state while it runs. Hold the lock briefly: the run loop takes it
    /// for every bar.
    pub fn session(&self) -> &Arc<Mutex<PaperSession>> {
        &self.session
    }

    /// Resolves once the session has ended on its own (an error) or after a stop, with its final
    /// status set.
    pub async fn finished(&self) {
        let mut ended = self.ended.clone();
        let _ = ended.wait_for(|ended| *ended).await;
    }

    /// Turns true once the session has ended, as [`PaperHandle::finished`] waits for.
    pub(crate) fn ended(&self) -> watch::Receiver<bool> {
        self.ended.clone()
    }

    /// Stops the session and waits for its loop to end. Its status is then `Stopped`, or `Error`
    /// if it had already failed.
    pub async fn stop(self) -> Arc<Mutex<PaperSession>> {
        let _ = self.stop.send(true);
        let _ = self.task.await;
        self.session
    }
}

/// Starts a session on its venue's live stream: warms it up if it has never run, then trades
/// each completed bar. Call from inside a tokio runtime.
pub fn start(session: PaperSession, rest: Arc<dyn Rest>) -> PaperHandle {
    let url = stream_url(&session.config().instrument.id);
    start_with_stream(session, rest, url)
}

/// [`start`] on another WebSocket URL serving the venue's message format, such as a test server.
/// The session opens its own connection once it has warmed up.
pub fn start_with_stream(session: PaperSession, rest: Arc<dyn Rest>, url: String) -> PaperHandle {
    let id = session.config().instrument.id.clone();
    start_on(session, rest, move || Arc::new(SharedStream::open(id, url)))
}

/// [`start`] on a stream other feeds may share. `stream` is called once the session has warmed up
/// and is ready for live bars, and must give a stream of the session's instrument.
pub fn start_on(
    session: PaperSession,
    rest: Arc<dyn Rest>,
    stream: impl FnOnce() -> Arc<SharedStream> + Send + 'static,
) -> PaperHandle {
    let session = Arc::new(Mutex::new(session));
    let (stop, mut stopped) = watch::channel(false);
    let (ended_tx, ended) = watch::channel(false);
    let shared = Arc::clone(&session);
    let task = tokio::spawn(async move {
        // The loop runs in its own task so that a panic in it is caught here and shows as an error.
        let mut driver = tokio::spawn(drive(Arc::clone(&shared), rest, Box::new(stream)));
        let status = tokio::select! {
            joined = &mut driver => match joined {
                Ok(Ok(())) => Status::Stopped,
                Ok(Err(e)) => Status::Error(e.to_string()),
                Err(e) => Status::Error(format!("the session panicked: {}", panic_message(e))),
            },
            _ = stopped.changed() => {
                // Wait for the cancelled loop to let go of the session.
                driver.abort();
                match driver.await {
                    // It panicked before the abort took effect.
                    Err(e) if e.is_panic() => {
                        Status::Error(format!("the session panicked: {}", panic_message(e)))
                    }
                    _ => Status::Stopped,
                }
            }
        };
        lock(&shared).set_status(status);
        let _ = ended_tx.send(true);
    });
    PaperHandle {
        session,
        stop,
        task,
        ended,
    }
}

fn lock(session: &Mutex<PaperSession>) -> MutexGuard<'_, PaperSession> {
    // A poisoned lock means the run loop panicked while holding it, possibly halfway through a
    // bar. The loop is over by then and the session is never fed again: the guard is taken back
    // only to set and read its status, which says the session failed.
    session.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The message a task panicked with.
fn panic_message(error: tokio::task::JoinError) -> String {
    match error.try_into_panic() {
        Ok(panic) => match panic.downcast::<String>() {
            Ok(message) => *message,
            Err(panic) => panic
                .downcast_ref::<&str>()
                .map_or_else(|| "no message".to_owned(), |m| (*m).to_owned()),
        },
        Err(error) => error.to_string(),
    }
}

/// Aborts a spawned task when dropped, so a stopped session takes its feed down with it.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Runs a blocking REST call off the async threads.
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, DataError> + Send + 'static,
) -> Result<T, PaperError> {
    tokio::task::spawn_blocking(call)
        .await
        .map_err(|e| PaperError::Task(e.to_string()))?
        .map_err(PaperError::from)
}

/// Gives the stream to feed from, once the session is ready for live bars.
type OpenStream = Box<dyn FnOnce() -> Arc<SharedStream> + Send>;

async fn drive(
    shared: Arc<Mutex<PaperSession>>,
    rest: Arc<dyn Rest>,
    stream: OpenStream,
) -> Result<(), PaperError> {
    let shared = &*shared;
    let (id, timeframe, data_dir, next, live) = {
        let s = lock(shared);
        s.check_resumable()?;
        let config = s.config();
        (
            config.instrument.id.clone(),
            config.timeframe,
            s.data_dir().to_path_buf(),
            s.next_bar_time(),
            s.live_from().is_some(),
        )
    };
    let start = match next {
        Some(next) if live => next,
        // Never live: warm up, or finish a warm-up the log holds part of, then go live from now.
        _ => {
            lock(shared).set_status(Status::WarmingUp);
            let end = timeframe.floor(rest.now());
            // From the end of the log, but never more than a full warm-up back.
            let earliest = end - WARM_UP_BARS * timeframe.duration_ms();
            let from = next.map_or(earliest, |n| n.max(earliest));
            let (store, history_rest, history_id) =
                (Store::new(data_dir), rest.clone(), id.clone());
            let bars = blocking(move || {
                warm_up_bars(&store, &*history_rest, &history_id, timeframe, from, end)
            })
            .await?;
            let mut s = lock(shared);
            for bar in bars {
                s.feed(bar, Vec::new(), true)?;
            }
            end
        }
    };

    let mut funding = if id.kind.rules().funding {
        Some(funding_clock(shared, &rest, &id, start).await)
    } else {
        None
    };

    lock(shared).set_status(Status::Reconnecting);
    let (tx, mut rx) = mpsc::channel(256);
    let feed = run_feed_on(stream(), timeframe, Some(start), rest.clone(), tx);
    let _feed = AbortOnDrop(tokio::spawn(feed));
    while let Some(event) = rx.recv().await {
        match event {
            FeedEvent::Connected => lock(shared).set_status(Status::Live),
            FeedEvent::Disconnected { .. } | FeedEvent::ConnectFailed { .. } => {
                lock(shared).set_status(Status::Reconnecting);
            }
            FeedEvent::BackfillFailed { error } => {
                let mut s = lock(shared);
                s.set_status(Status::Reconnecting);
                s.warn(format!("backfill failed, retrying: {error}"));
            }
            FeedEvent::BadMessage { error } => lock(shared).warn(error),
            FeedEvent::Dropped(open_time) => lock(shared).push_dropped(open_time),
            FeedEvent::Bar(bar) => {
                let charged = match &mut funding {
                    Some(clock) => {
                        let mut warn = |w| lock(shared).warn(w);
                        clock.for_bar(&rest, &id, timeframe, &bar, &mut warn).await
                    }
                    None => Vec::new(),
                };
                let mut s = lock(shared);
                // Bars flow again, after a failed backfill too.
                s.set_status(Status::Live);
                s.feed(bar, charged, false)?;
            }
        }
    }
    Err(PaperError::FeedEnded)
}

/// The bars of `timeframe` to warm up on, opening in `[from, end)`: from the local store as far as
/// it has them, the rest over REST at the timeframe itself (one request rather than hundreds of
/// pages of minutes).
fn warm_up_bars(
    store: &Store,
    rest: &dyn Rest,
    id: &InstrumentId,
    timeframe: Timeframe,
    from: Millis,
    end: Millis,
) -> Result<Vec<Candle>, DataError> {
    let mut bars = store.load_candles(id, timeframe, from, end, false)?.candles;
    let rest_from = bars
        .last()
        .map_or(from, |b| b.open_time + timeframe.duration_ms());
    if rest_from < end {
        let fetched = rest.klines(id, timeframe, rest_from, end)?;
        bars.extend(
            fetched
                .into_iter()
                .filter(|b| (rest_from..end).contains(&b.open_time)),
        );
    }
    // Bars are fed as they are; one that fails validation would stop the session, so skip it.
    bars.retain(|b| b.validate().is_ok());
    Ok(bars)
}

/// Sets up funding tracking for a perpetual going live at `start`: the venue's recent rates give
/// the last funding time before the session went live and the interval between rates, and the
/// rates already charged (on a resume) move it on.
async fn funding_clock(
    shared: &Mutex<PaperSession>,
    rest: &Arc<dyn Rest>,
    id: &InstrumentId,
    start: Millis,
) -> FundingClock {
    let live_from = lock(shared).live_from().unwrap_or(start);
    let (history_rest, history_id) = (rest.clone(), id.clone());
    let history = blocking(move || {
        history_rest.funding(&history_id, live_from - FUNDING_LOOKBACK, live_from)
    })
    .await;
    let mut s = lock(shared);
    let history = history.unwrap_or_else(|e| {
        s.warn(format!("could not read recent funding rates: {e}"));
        Vec::new()
    });
    let mut clock = FundingClock::new(&history, live_from);
    for f in s.funding() {
        clock.seen(f.due);
    }
    clock
}

/// Which funding rates have been charged and when the next is due.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FundingClock {
    /// Funding time of the last rate charged, or seen before the session went live.
    last: Millis,
    interval: Millis,
    retry_delay: Duration,
}

impl FundingClock {
    /// A clock from the rates before the session went live at `live_from`.
    pub(crate) fn new(history: &[(Millis, f64)], live_from: Millis) -> Self {
        let mut clock = FundingClock {
            last: live_from - 1,
            interval: DEFAULT_FUNDING_INTERVAL,
            retry_delay: FUNDING_RETRY_DELAY,
        };
        if let Some(&(first, _)) = history.first() {
            clock.last = first;
        }
        for &(time, _) in history {
            clock.seen(time);
        }
        clock
    }

    /// Notes a rate at `time`, charged or seen. The time is rounded to the minute, as the venue
    /// parsers round fetched rates, so a rate logged with a raw stamp a few milliseconds early is
    /// not charged again when the same rate is fetched on the minute after a resume.
    fn seen(&mut self, time: Millis) {
        let time = round_to_minute(time);
        if time > self.last {
            self.interval = time - self.last;
            self.last = time;
        }
    }

    /// Funding times are on the hour on both venues, so a bar is worth asking about when its
    /// window holds a full hour, or when a rate is overdue.
    fn should_fetch(&self, open: Millis, end: Millis) -> bool {
        let holds_an_hour = (end - 1).div_euclid(HOUR) * HOUR >= open;
        holds_an_hour || self.next_due() < end
    }

    fn next_due(&self) -> Millis {
        self.last + self.interval
    }

    /// Takes the rates fetched after the last one charged, up to the bar's end, as funding for the
    /// bar opening at `open`. A rate due before the bar is charged on it, at its open.
    fn take(&mut self, rates: &[(Millis, f64)], open: Millis, end: Millis) -> Vec<Funding> {
        let mut rates: Vec<(Millis, f64)> = rates
            .iter()
            .copied()
            .filter(|&(t, rate)| t > self.last && t < end && rate.is_finite())
            .collect();
        rates.sort_by_key(|&(t, _)| t);
        rates.dedup_by_key(|&mut (t, _)| t);
        rates
            .into_iter()
            .map(|(due, rate)| {
                self.seen(due);
                Funding {
                    time: due.max(open),
                    rate,
                    due,
                }
            })
            .collect()
    }

    /// The funding to charge with `bar`. A rate expected inside the bar that the venue has not
    /// published is asked for again a few times, then left for a later bar.
    async fn for_bar(
        &mut self,
        rest: &Arc<dyn Rest>,
        id: &InstrumentId,
        timeframe: Timeframe,
        bar: &Candle,
        warn: &mut (dyn FnMut(String) + Send),
    ) -> Vec<Funding> {
        let (open, end) = (bar.open_time, bar.open_time + timeframe.duration_ms());
        if !self.should_fetch(open, end) {
            return Vec::new();
        }
        // Retry only for a rate due in this bar; one overdue from an earlier bar was retried then.
        let due_here = (open..end).contains(&self.next_due());
        let mut charged = Vec::new();
        let mut error = None;
        for attempt in 0..=FUNDING_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(self.retry_delay).await;
            }
            let (rest, id, from) = (rest.clone(), id.clone(), self.last + 1);
            match blocking(move || rest.funding(&id, from, end)).await {
                Ok(rates) => {
                    charged.extend(self.take(&rates, open, end));
                    error = None;
                }
                Err(e) => error = Some(e),
            }
            let missing = due_here && self.next_due() < end;
            if !missing {
                break;
            }
        }
        if let Some(e) = error {
            warn(format!(
                "funding for the bar at {open} could not be read: {e}"
            ));
        }
        for late in charged.iter().filter(|f| f.due < open) {
            warn(format!(
                "funding due at {} was published late and charged on the bar at {open}",
                late.due
            ));
        }
        charged
    }
}

#[cfg(test)]
mod tests {
    use td_core::InstrumentKind;

    use super::*;

    const T: Millis = 1_704_067_200_000; // 2024-01-01 00:00 UTC
    const MIN: Millis = 60_000;

    #[test]
    fn the_interval_comes_from_the_last_two_rates() {
        let clock = FundingClock::new(&[(T - 16 * HOUR, 0.1), (T - 12 * HOUR, 0.1)], T);
        assert_eq!((clock.last, clock.interval), (T - 12 * HOUR, 4 * HOUR));
        let clock = FundingClock::new(&[], T);
        assert_eq!((clock.last, clock.interval), (T - 1, 8 * HOUR));
    }

    #[test]
    fn bars_holding_an_hour_or_an_overdue_rate_are_asked_about() {
        // The rate due at T has been seen; the next is due at T + 8h.
        let clock = FundingClock::new(&[(T - 8 * HOUR, 0.1), (T, 0.1)], T + MIN);
        assert!(clock.should_fetch(T + HOUR, T + HOUR + MIN));
        assert!(!clock.should_fetch(T + MIN, T + 2 * MIN));
        // The rate due at T has not been seen, so every bar after it asks.
        let overdue = FundingClock::new(&[(T - 16 * HOUR, 0.1), (T - 8 * HOUR, 0.1)], T + MIN);
        assert!(overdue.should_fetch(T + MIN, T + 2 * MIN));
    }

    #[test]
    fn a_late_rate_is_charged_at_the_later_bar_open_with_its_due_time() {
        let mut clock = FundingClock::new(&[(T - 8 * HOUR, 0.1)], T);
        let late = clock.take(&[(T - 8 * HOUR, 0.1), (T, 0.2)], T + 5 * MIN, T + 6 * MIN);
        assert_eq!(
            late,
            [Funding {
                time: T + 5 * MIN,
                rate: 0.2,
                due: T
            }]
        );
        assert_eq!(clock.last, T);
        assert!(clock.take(&[(T, 0.2)], T + 6 * MIN, T + 7 * MIN).is_empty());
    }

    #[test]
    fn a_rate_logged_a_few_ms_early_is_not_charged_again_when_fetched_on_the_minute() {
        // A resumed session's log holds the rate due at T with the venue's raw stamp, 5 ms early.
        let mut clock = FundingClock::new(&[(T - 16 * HOUR, 0.1), (T - 8 * HOUR, 0.1)], T - HOUR);
        clock.seen(T - 5);
        assert_eq!((clock.last, clock.interval), (T, 8 * HOUR));
        assert!(clock.take(&[(T, 0.1)], T + MIN, T + 2 * MIN).is_empty());
    }

    /// Funding with one rate due at `T`, published from the `publish_on`th request on.
    struct SlowFunding {
        publish_on: u32,
        calls: Mutex<u32>,
    }

    impl Rest for SlowFunding {
        fn klines(
            &self,
            _: &InstrumentId,
            _: Timeframe,
            _: Millis,
            _: Millis,
        ) -> Result<Vec<Candle>, DataError> {
            Ok(Vec::new())
        }

        fn funding(
            &self,
            _: &InstrumentId,
            from: Millis,
            to: Millis,
        ) -> Result<Vec<(Millis, f64)>, DataError> {
            let mut calls = self.calls.lock().expect("lock");
            *calls += 1;
            let published = *calls >= self.publish_on;
            Ok([(T, 0.0001)]
                .into_iter()
                .filter(|&(t, _)| published && (from..to).contains(&t))
                .collect())
        }
    }

    fn bar(open_time: Millis) -> Candle {
        Candle {
            open_time,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 0.0,
            quote_volume: 0.0,
        }
    }

    /// Feeds bars at `T`, `T + 1m`, ... to a fresh clock against funding published on the
    /// `publish_on`th request, and returns the funding charged with each bar, the requests made and
    /// the warnings.
    async fn charge(publish_on: u32, bars: i64) -> (Vec<Vec<Funding>>, u32, Vec<String>) {
        let id = InstrumentId {
            venue: td_core::Venue::Binance,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Perpetual,
        };
        let slow = Arc::new(SlowFunding {
            publish_on,
            calls: Mutex::new(0),
        });
        let rest: Arc<dyn Rest> = slow.clone();
        let mut clock = FundingClock::new(&[(T - 16 * HOUR, 0.1), (T - 8 * HOUR, 0.1)], T);
        clock.retry_delay = Duration::ZERO;
        let mut charged = Vec::new();
        let mut warnings = Vec::new();
        for i in 0..bars {
            let mut warn = |w: String| warnings.push(w);
            let bar = bar(T + i * MIN);
            charged.push(
                clock
                    .for_bar(&rest, &id, Timeframe::M1, &bar, &mut warn)
                    .await,
            );
        }
        let calls = *slow.calls.lock().expect("lock");
        (charged, calls, warnings)
    }

    #[tokio::test]
    async fn a_rate_published_late_is_retried_in_its_own_bar() {
        let (charged, calls, warnings) = charge(3, 2).await;
        assert!(warnings.is_empty(), "{warnings:?}");
        let on_time = Funding {
            time: T,
            rate: 0.0001,
            due: T,
        };
        assert_eq!(charged, [vec![on_time], vec![]]);
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn a_rate_still_unpublished_after_the_retries_is_charged_on_a_later_bar() {
        // Four requests in its own bar find nothing; the next bar's one request finds it.
        let (charged, calls, warnings) = charge(5, 3).await;
        assert_eq!(warnings.len(), 1, "the late charge is noted");
        let late = Funding {
            time: T + MIN,
            rate: 0.0001,
            due: T,
        };
        assert_eq!(charged, [vec![], vec![late], vec![]]);
        assert_eq!(calls, 5);
    }
}
