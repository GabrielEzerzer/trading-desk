//! Running a [`LiveSession`]: the replay's end at the venue, warm-up on a session that has not
//! traded yet, then each closed bar from the live feed, with the funding due in it, fed on the
//! run's own thread.
//!
//! On a perpetual the strategy sees the funding rates a backtest of its bars shows it
//! (`Context::funding`), as a paper session does: warm-up bars carry the venue's rates from the
//! first warm-up bar's open, uncharged, and every rate is fed at its settlement time. The one
//! difference is a rate the venue has not published when the bar it is due in closes: it comes on
//! a later bar, with a warning on the bar it was due in and another when it arrives.
//!
//! The run halts, sending and cancelling nothing more, when the venue's trading rules for the
//! instrument (tick size, quantity step, minimum quantity, minimum notional) differ from the ones
//! saved at creation or cannot be read, which is checked before anything else goes out, when the
//! broker faults, when the warm-up or a funding read fails after its bounded retries, when the
//! log or the status cannot be written, when the feed ends, when the run panics, or when the
//! owner asks it to. A fault writes the session's `faulted` marker, and a fault or a panic leaves a
//! session that is never run again. A stop leaves the position and resting orders at the venue as
//! they are. A resumed session skips warm-up and continues from the bar after its last one; the
//! feed backfills the bars missed while it was down, and they are fed as live bars.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use td_core::{Candle, InstrumentId, Millis, Timeframe};
use td_data::Store;
use td_data::live::{FeedEvent, Rest, SharedStream, run_feed_on};
use td_data::time::round_to_minute;

use crate::VenueHandle;
use crate::session::{Funding, LiveError, LiveSession, LiveStatus, State, panic_message};

/// How many bars of history the strategy sees before it trades, as a paper session.
///
/// shortcut: a copy of td-server's `WARM_UP_BARS`; move both to one place when the paper and live
/// runners share their warm-up.
const WARM_UP_BARS: i64 = 500;
/// Tries at reading the warm-up bars from the venue before the session halts.
const WARM_UP_TRIES: u32 = 3;
/// The pause after the first failed warm-up read, doubled after the second.
const WARM_UP_PAUSE: Duration = Duration::from_secs(1);
const HOUR: Millis = 3_600_000;
/// How far back the funding history is read to learn the last funding time and the interval.
const FUNDING_LOOKBACK: Millis = 72 * HOUR;
/// The interval assumed until two rates have been seen.
const DEFAULT_FUNDING_INTERVAL: Millis = 8 * HOUR;
/// Retries of a funding read that failed, or of a rate due in the bar and not yet published.
const FUNDING_RETRIES: u32 = 3;
const FUNDING_RETRY_DELAY: Duration = Duration::from_secs(5);
const FEED_ENDED: &str = "the feed ended, so no more bars can come";
/// The relative difference below which a saved trading rule and the venue's are the same.
const RULE_TOLERANCE: f64 = 1e-9;

/// What the owner asked for.
enum Signal {
    Stop,
    Halt(String),
}

enum Msg {
    Feed(FeedEvent),
    /// A signal is waiting.
    Wake,
    /// The feed's task returned or panicked: no more events will come.
    FeedEnded,
}

struct Control {
    signal: Mutex<Option<Signal>>,
    wake: mpsc::Sender<Msg>,
}

impl Control {
    /// Asks the run for `signal`; the first one asked wins.
    fn ask(&self, signal: Signal) {
        lock(&self.signal).get_or_insert(signal);
        let _ = self.wake.send(Msg::Wake);
    }

    fn take(&self) -> Option<Signal> {
        lock(&self.signal).take()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A running [`LiveSession`]. Dropping it asks the run to stop without waiting for it.
pub struct LiveRun {
    session: Arc<Mutex<LiveSession>>,
    handle: VenueHandle,
    control: Arc<Control>,
    thread: Option<JoinHandle<Result<(), LiveError>>>,
}

impl LiveSession {
    /// Runs the session on a thread of its own and returns at once. A session that faulted or
    /// panicked ends at once with [`LiveError::StillFaulted`] or [`LiveError::StillPanicked`],
    /// sending nothing.
    ///
    /// `rest` reads warm-up bars, missed minutes and funding rates. `stream` opens the live
    /// minute stream; it is called on the run's thread, inside the run's own async runtime, once
    /// warm-up is done. `after_bar` is called after each live bar is fed, logged and its status
    /// written, before the next bar: returning a reason halts the run with it.
    pub fn run<S, A>(self, rest: Arc<dyn Rest>, stream: S, after_bar: A) -> LiveRun
    where
        S: FnOnce() -> Arc<SharedStream> + Send + 'static,
        A: FnMut(&LiveStatus, &VenueHandle) -> Option<String> + Send + 'static,
    {
        let handle = self.handle().clone();
        let session = Arc::new(Mutex::new(self));
        let (wake, msgs) = mpsc::channel();
        let control = Arc::new(Control {
            signal: Mutex::new(None),
            wake: wake.clone(),
        });
        let thread = {
            let (session, control) = (session.clone(), control.clone());
            std::thread::spawn(move || {
                let ran = catch_unwind(AssertUnwindSafe(|| {
                    drive(&session, &control, wake, msgs, rest, stream, after_bar)
                }));
                ran.unwrap_or_else(|panic| {
                    halt(
                        &mut lock(&session),
                        LiveError::Panicked(panic_message(&*panic)),
                    )
                })
            })
        };
        LiveRun {
            session,
            handle,
            control,
            thread: Some(thread),
        }
    }
}

impl LiveRun {
    /// The session, locked; the run waits for it between bars.
    pub fn session(&self) -> MutexGuard<'_, LiveSession> {
        lock(&self.session)
    }

    pub fn status(&self) -> LiveStatus {
        self.session().status().clone()
    }

    /// The broker's handle: the venue's equity and position and the session's resting orders,
    /// readable between bars as well.
    pub fn handle(&self) -> &VenueHandle {
        &self.handle
    }

    /// Asks the run to halt with `reason` before the next bar. Nothing is sent or cancelled.
    pub fn halt(&self, reason: impl Into<String>) {
        self.control.ask(Signal::Halt(reason.into()));
    }

    /// Whether the run has ended: halted, faulted or stopped. [`LiveRun::status`] says which.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stops the run before the next bar and waits for it. The position and resting orders stay
    /// at the venue as they are, and the status says what is open. Returns the session, still
    /// locked until it is dropped, and how the run ended: `Ok` for this stop, or the error it
    /// halted on before.
    pub fn stop(self) -> (LiveSession, Result<(), LiveError>) {
        self.control.ask(Signal::Stop);
        self.wait()
    }

    /// Waits for the run to end on its own, as [`LiveRun::stop`] without asking it to.
    pub fn wait(mut self) -> (LiveSession, Result<(), LiveError>) {
        let result = match self.thread.take().map(JoinHandle::join) {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(LiveError::Panicked("the run thread panicked".into())),
            None => Ok(()),
        };
        let session = self.session.clone();
        drop(self);
        let session = Arc::into_inner(session)
            .expect("the run thread has ended and released the session")
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (session, result)
    }
}

impl Drop for LiveRun {
    fn drop(&mut self) {
        self.control.ask(Signal::Stop);
    }
}

/// Records why the run ended in the status and returns the error. A broker fault also writes
/// the `faulted` marker, first, so the session stays halted even when the status cannot be
/// written or is edited.
fn halt(s: &mut LiveSession, error: LiveError) -> Result<(), LiveError> {
    let state = match &error {
        LiveError::Faulted(fault) => State::Faulted {
            fault: fault.clone(),
        },
        LiveError::Panicked(panic) => State::Panicked {
            panic: panic.clone(),
        },
        LiveError::Halted(reason) => State::Halted {
            reason: reason.clone(),
        },
        other => State::Halted {
            reason: other.to_string(),
        },
    };
    let fault = match &error {
        LiveError::Faulted(fault) => Some(fault.clone()),
        _ => s.handle().fault().map(|fault| fault.to_string()),
    };
    if let Some(fault) = fault {
        s.mark_faulted(&fault);
    }
    // The state is set in memory whatever the write does, so the status read is the truth; a
    // write that fails leaves the old file, and the caller still gets the error.
    if let Err(e) = s.set_state(state) {
        s.warn(format!("the status could not be written: {e}"));
    }
    Err(error)
}

fn drive<S, A>(
    shared: &Mutex<LiveSession>,
    control: &Control,
    wake: mpsc::Sender<Msg>,
    msgs: mpsc::Receiver<Msg>,
    rest: Arc<dyn Rest>,
    stream: S,
    mut after_bar: A,
) -> Result<(), LiveError>
where
    S: FnOnce() -> Arc<SharedStream>,
    A: FnMut(&LiveStatus, &VenueHandle) -> Option<String>,
{
    let (id, timeframe, data_dir, next, live, handle) = {
        let mut s = lock(shared);
        // A session that faulted or panicked stays halted, in this process as after a restart.
        if let Some(refused) = s.refusal() {
            return Err(refused);
        }
        if let Err(reason) = check_rules(&s) {
            return halt(&mut s, LiveError::Halted(reason));
        }
        // Ends a replay: a fault here halts before a single bar is fed.
        if let Err(e) = s.go_live() {
            s.read_venue();
            return halt(&mut s, e);
        }
        let config = &s.config().config;
        (
            config.instrument.id.clone(),
            config.timeframe,
            s.data_dir().to_path_buf(),
            s.next_bar_time(),
            s.live_from().is_some(),
            s.handle().clone(),
        )
    };
    // A warm-up that reads the funding rates gives the clock its start from the same rates, and
    // leaves the rates after its last bar for the first live bar.
    let mut warmed = None;
    let mut after_warm_up = Vec::new();
    let start = match next {
        Some(next) if live => next,
        // Never live: warm up, or finish a warm-up the log holds part of, then go live from now.
        _ => {
            let warming = lock(shared).set_state(State::WarmingUp);
            if let Err(e) = warming {
                return halt(&mut lock(shared), e);
            }
            let end = timeframe.floor(rest.now());
            let earliest = end - WARM_UP_BARS * timeframe.duration_ms();
            let from = next.map_or(earliest, |n| n.max(earliest));
            let bars = match warm_up_bars(shared, &data_dir, &*rest, &id, timeframe, from, end) {
                Ok(bars) => bars,
                Err(reason) => return halt(&mut lock(shared), LiveError::Halted(reason)),
            };
            // As a paper session: the strategy sees every rate from where its bars begin, the end
            // of the log or the first warm-up bar, each fed at its due time with the bar it falls
            // in or the next, uncharged, as a backtest of the same bars feeds it.
            let mut rates = Vec::new();
            if let Some(seen_from) = next.or(bars.first().map(|b| b.open_time))
                && id.kind.rules().funding
            {
                let lookback = seen_from.min(end - FUNDING_LOOKBACK);
                rates = match read_funding(&*rest, &id, lookback, end) {
                    Ok(rates) => rates,
                    Err(reason) => return halt(&mut lock(shared), LiveError::Halted(reason)),
                };
                warmed = Some(FundingClock::new(&rates, end, FUNDING_RETRY_DELAY));
                rates.retain(|&(time, _)| time >= seen_from);
            }
            let mut rates = rates.into_iter().peekable();
            let mut s = lock(shared);
            for bar in bars {
                let bar_end = bar.open_time + timeframe.duration_ms();
                let due = std::iter::from_fn(|| rates.next_if(|&(time, _)| time < bar_end));
                let from = s.next_bar_time().unwrap_or(bar.open_time);
                if let Err(e) = s.feed(bar, funding_at(due, from), true) {
                    return halt(&mut s, e);
                }
            }
            after_warm_up.extend(rates);
            end
        }
    };

    let mut funding = if let Some(clock) = warmed {
        Some(clock)
    } else if id.kind.rules().funding {
        let live_from = lock(shared).live_from().unwrap_or(start);
        match read_funding(&*rest, &id, live_from - FUNDING_LOOKBACK, live_from) {
            Ok(history) => {
                let mut clock = FundingClock::new(&history, live_from, FUNDING_RETRY_DELAY);
                for f in lock(shared).funding() {
                    clock.seen(f.due);
                }
                Some(clock)
            }
            Err(reason) => return halt(&mut lock(shared), LiveError::Halted(reason)),
        }
    } else {
        None
    };

    {
        let mut s = lock(shared);
        s.read_venue();
        if let Err(e) = s.set_state(State::Running) {
            return halt(&mut s, e);
        }
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            let reason = format!("the feed's runtime could not start: {e}");
            return halt(&mut lock(shared), LiveError::Halted(reason));
        }
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    {
        let _inside = runtime.enter();
        runtime.spawn(run_feed_on(
            stream(),
            timeframe,
            Some(start),
            rest.clone(),
            tx,
        ));
    }
    runtime.spawn(async move {
        while let Some(event) = rx.recv().await {
            if wake.send(Msg::Feed(event)).is_err() {
                return;
            }
        }
        // Every sender is gone: the feed's task returned or panicked.
        let _ = wake.send(Msg::FeedEnded);
    });

    let ended = loop {
        if let Some(signal) = control.take() {
            let mut s = lock(shared);
            break match signal {
                Signal::Halt(reason) => halt(&mut s, LiveError::Halted(reason)),
                Signal::Stop => stop(&mut s),
            };
        }
        // The control keeps a sender, so this only waits.
        let event = match msgs.recv() {
            Ok(Msg::Feed(event)) => event,
            Ok(Msg::FeedEnded) => {
                break halt(&mut lock(shared), LiveError::Halted(FEED_ENDED.into()));
            }
            Ok(Msg::Wake) | Err(_) => continue,
        };
        let warning = match event {
            FeedEvent::Bar(bar) => {
                let fed = live_bar(
                    shared,
                    &*rest,
                    &id,
                    timeframe,
                    funding.as_mut(),
                    &mut after_warm_up,
                    bar,
                    &handle,
                    &mut after_bar,
                );
                if fed.is_err() {
                    break fed;
                }
                None
            }
            FeedEvent::Connected => None,
            FeedEvent::Disconnected { reason } => Some(format!("disconnected: {reason}")),
            FeedEvent::ConnectFailed { error } => Some(format!("could not connect: {error}")),
            FeedEvent::BackfillFailed { error } => {
                Some(format!("backfill failed, retrying: {error}"))
            }
            FeedEvent::BadMessage { error } => Some(error),
            FeedEvent::Dropped(open_time) => Some(format!(
                "the bar at {open_time} had too many minutes missing and was not fed"
            )),
            FeedEvent::Tolerated {
                open_time,
                missing_minutes,
            } => Some(format!(
                "the bar at {open_time} was built with {missing_minutes} of its minutes missing"
            )),
            FeedEvent::FramesDropped { count } => Some(format!(
                "{count} stream frames were dropped while the feed was behind; \
                 the closed minutes among them are backfilled"
            )),
        };
        if let Some(warning) = warning {
            lock(shared).warn(warning);
        }
    };
    runtime.shutdown_background();
    ended
}

/// Feeds one live bar with the funding due in it, and on the first live bar the rates due after
/// the last warm-up bar, then reads the venue, checks the broker's fault, writes the status and
/// calls the owner's check.
#[allow(clippy::too_many_arguments)]
fn live_bar<A>(
    shared: &Mutex<LiveSession>,
    rest: &dyn Rest,
    id: &InstrumentId,
    timeframe: Timeframe,
    clock: Option<&mut FundingClock>,
    after_warm_up: &mut Vec<(Millis, f64)>,
    bar: Candle,
    handle: &VenueHandle,
    after_bar: &mut A,
) -> Result<(), LiveError>
where
    A: FnMut(&LiveStatus, &VenueHandle) -> Option<String>,
{
    // Rates after the last warm-up bar, missing from the feed's history, go with the first live
    // bar, charged at its open on the flat position warm-up leaves.
    let from = lock(shared).next_bar_time().unwrap_or(bar.open_time);
    let mut charged = funding_at(after_warm_up.drain(..), from);
    if let Some(clock) = clock {
        let mut warnings = Vec::new();
        let due = clock.for_bar(rest, id, timeframe, &bar, from, &mut |w| warnings.push(w));
        let mut s = lock(shared);
        for warning in warnings {
            s.warn(warning);
        }
        match due {
            Ok(due) => charged.extend(due),
            Err(reason) => return halt(&mut s, LiveError::Halted(reason)),
        }
    }
    let mut s = lock(shared);
    let fed = s.feed(bar, charged, false);
    s.read_venue();
    if let Some(fault) = handle.fault() {
        return halt(&mut s, LiveError::Faulted(fault.to_string()));
    }
    if let Err(e) = fed.and_then(|()| s.save_status()) {
        return halt(&mut s, e);
    }
    let status = s.status().clone();
    drop(s);
    match after_bar(&status, handle) {
        Some(reason) => halt(&mut lock(shared), LiveError::Halted(reason)),
        None => Ok(()),
    }
}

/// Records a stop asked for by the owner: what the session leaves open at the venue.
fn stop(s: &mut LiveSession) -> Result<(), LiveError> {
    s.read_venue();
    let position = s
        .status()
        .venue
        .as_ref()
        .map_or(s.status().position, |v| v.position);
    let note = format!(
        "stopped by the owner; left at the venue as they are: a position of {position} and {} \
         resting orders",
        s.handle().resting_orders().len()
    );
    s.set_state(State::Stopped { note })
}

/// Whether the venue's trading rules for the instrument are still the ones the session saved when
/// it was created, which its orders are rounded to. The error names each rule that changed, with
/// the saved value and the venue's, or says the rules could not be read.
fn check_rules(s: &LiveSession) -> Result<(), String> {
    let saved = &s.config().config.instrument;
    let symbol = &saved.id.symbol;
    let venue = s.client().instrument_info(symbol).map_err(|e| {
        format!(
            "the venue's trading rules for {symbol} could not be read, so the session did not go \
             live and sent nothing: {e}"
        )
    })?;
    let changed: Vec<String> = [
        ("tick size", saved.tick_size, venue.tick_size),
        ("quantity step", saved.lot_size, venue.qty_step),
        ("minimum quantity", saved.min_qty, venue.min_order_qty),
        ("minimum notional", saved.min_notional, venue.min_notional),
    ]
    .into_iter()
    .filter(|&(_, saved, venue)| {
        (saved - venue).abs() > RULE_TOLERANCE * saved.abs().max(venue.abs())
    })
    .map(|(rule, saved, venue)| format!("{rule} saved as {saved}, now {venue} at the venue"))
    .collect();
    if changed.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the venue changed its trading rules for {symbol} since the session was created ({}), so \
         orders rounded to the saved rules could be refused or mis-sized; the session did not go \
         live and sent nothing",
        changed.join("; ")
    ))
}

/// The bars of `timeframe` to warm up on, opening in `[from, end)`: from the local store as far as
/// it has them, the rest from the venue, tried [`WARM_UP_TRIES`] times.
fn warm_up_bars(
    shared: &Mutex<LiveSession>,
    data_dir: &Path,
    rest: &dyn Rest,
    id: &InstrumentId,
    timeframe: Timeframe,
    from: Millis,
    end: Millis,
) -> Result<Vec<Candle>, String> {
    let mut bars = Store::new(data_dir)
        .load_candles(id, timeframe, from, end, false)
        .map_err(|e| format!("warm-up could not read the local store: {e}"))?
        .candles;
    let rest_from = bars
        .last()
        .map_or(from, |b| b.open_time + timeframe.duration_ms());
    if rest_from < end {
        let mut tries = 0;
        let fetched = loop {
            tries += 1;
            match rest.klines(id, timeframe, rest_from, end) {
                Ok(fetched) => break fetched,
                Err(e) if tries == WARM_UP_TRIES => {
                    return Err(format!(
                        "warm-up could not read the venue in {WARM_UP_TRIES} tries, so the \
                         session did not go live: {e}"
                    ));
                }
                Err(e) => {
                    lock(shared).warn(format!("warm-up failed, retrying: {e}"));
                    std::thread::sleep(WARM_UP_PAUSE * tries);
                }
            }
        };
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

/// The funding rates in `[from, to)`, sorted, read with the same retries as a bar's funding, so
/// the strategy never sees a series with rates missing.
fn read_funding(
    rest: &dyn Rest,
    id: &InstrumentId,
    from: Millis,
    to: Millis,
) -> Result<Vec<(Millis, f64)>, String> {
    let mut error = String::new();
    for attempt in 0..=FUNDING_RETRIES {
        if attempt > 0 {
            std::thread::sleep(FUNDING_RETRY_DELAY);
        }
        match rest.funding(id, from, to) {
            Ok(mut rates) => {
                // As `FundingClock::take` keeps them.
                rates.retain(|&(time, rate)| (from..to).contains(&time) && rate.is_finite());
                rates.sort_by_key(|&(time, _)| time);
                rates.dedup_by_key(|&mut (time, _)| time);
                return Ok(rates);
            }
            Err(e) => error = e.to_string(),
        }
    }
    Err(format!(
        "funding rates could not be read after {FUNDING_RETRIES} retries: {error}"
    ))
}

/// Funding for a bar from rates `(due, rate)` due before its end and not fed yet. `from` is the
/// earliest time the engine takes with the bar: the end of the last bar fed, or the bar's own open
/// when it is the first. Each rate is given at its due time, as a backtest of the same bars gives
/// it, so a rate due in a gap before the bar keeps its time; only a rate due before `from`, which
/// should have come with an earlier bar, is given at `from`.
///
/// shortcut: a copy of td-server's `funding_at`; share one with the funding clock below.
fn funding_at(rates: impl IntoIterator<Item = (Millis, f64)>, from: Millis) -> Vec<Funding> {
    rates
        .into_iter()
        .map(|(due, rate)| Funding {
            time: due.max(from),
            rate,
            due,
        })
        .collect()
}

/// Which funding rates have been charged and when the next is due.
///
/// shortcut: a copy of td-server's `FundingClock` (crate-private there), blocking instead of async,
/// and failing after its retries instead of warning; share one when the paper runner moves onto
/// a thread like this one.
#[derive(Debug, Clone, PartialEq)]
struct FundingClock {
    /// Funding time of the last rate charged, or seen before the session went live.
    last: Millis,
    interval: Millis,
    retry_delay: Duration,
}

impl FundingClock {
    /// A clock from the rates before the session went live at `live_from`.
    fn new(history: &[(Millis, f64)], live_from: Millis, retry_delay: Duration) -> Self {
        let mut clock = FundingClock {
            last: live_from - 1,
            interval: DEFAULT_FUNDING_INTERVAL,
            retry_delay,
        };
        if let Some(&(first, _)) = history.first() {
            clock.last = first;
        }
        for &(time, _) in history {
            clock.seen(time);
        }
        clock
    }

    /// Notes a rate at `time`, charged or seen, rounded to the minute as the venue parsers round
    /// fetched rates.
    fn seen(&mut self, time: Millis) {
        let time = round_to_minute(time);
        if time > self.last {
            self.interval = time - self.last;
            self.last = time;
        }
    }

    /// Funding times are on the hour, so a bar is worth asking about when its window holds a full
    /// hour, or when a rate is overdue.
    fn should_fetch(&self, open: Millis, end: Millis) -> bool {
        let holds_an_hour = (end - 1).div_euclid(HOUR) * HOUR >= open;
        holds_an_hour || self.next_due() < end
    }

    fn next_due(&self) -> Millis {
        self.last + self.interval
    }

    /// Takes the rates fetched after the last one charged, up to the bar's end, as funding for the
    /// bar whose earliest funding time is `from` (see [`funding_at`]). A rate due before the bar is
    /// charged on it, at its open.
    fn take(&mut self, rates: &[(Millis, f64)], from: Millis, end: Millis) -> Vec<Funding> {
        let mut rates: Vec<(Millis, f64)> = rates
            .iter()
            .copied()
            .filter(|&(t, rate)| t > self.last && t < end && rate.is_finite())
            .collect();
        rates.sort_by_key(|&(t, _)| t);
        rates.dedup_by_key(|&mut (t, _)| t);
        for &(due, _) in &rates {
            self.seen(due);
        }
        funding_at(rates, from)
    }

    /// The funding to charge with `bar`, which takes funding from `from` on (see [`funding_at`]).
    /// A failed read, or a rate expected inside the bar that the venue has not published, is asked
    /// for again a few times. A read still failing after that is an error, which halts the
    /// session; a rate still unpublished is left for a later bar, with a warning on this bar and
    /// another when it comes.
    fn for_bar(
        &mut self,
        rest: &dyn Rest,
        id: &InstrumentId,
        timeframe: Timeframe,
        bar: &Candle,
        from: Millis,
        warn: &mut dyn FnMut(String),
    ) -> Result<Vec<Funding>, String> {
        let (open, end) = (bar.open_time, bar.open_time + timeframe.duration_ms());
        if !self.should_fetch(open, end) {
            return Ok(Vec::new());
        }
        let due_here = (open..end).contains(&self.next_due());
        let mut charged = Vec::new();
        let mut error = None;
        for attempt in 0..=FUNDING_RETRIES {
            if attempt > 0 {
                std::thread::sleep(self.retry_delay);
            }
            match rest.funding(id, self.last + 1, end) {
                Ok(rates) => {
                    charged.extend(self.take(&rates, from, end));
                    error = None;
                }
                Err(e) => error = Some(e.to_string()),
            }
            let missing = due_here && self.next_due() < end;
            if error.is_none() && !missing {
                break;
            }
        }
        if let Some(e) = error {
            return Err(format!(
                "funding for the bar at {open} could not be read after {FUNDING_RETRIES} \
                 retries: {e}"
            ));
        }
        if due_here && self.next_due() < end {
            // A warning, not a halt: the expected time is inferred from the last two rates, and a
            // venue that changes its funding interval must not stop the session.
            warn(format!(
                "the venue had not published the funding due at {} when the bar at {open} closed, \
                 so the strategy ran this bar without it; it will be charged and shown when the \
                 venue publishes it",
                self.next_due()
            ));
        }
        // A rate due in a gap before the bar keeps its time and is not late.
        for late in charged.iter().filter(|f| f.due < from) {
            warn(format!(
                "funding due at {} was published late and charged on the bar at {open}",
                late.due
            ));
        }
        Ok(charged)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use td_core::{InstrumentKind, Venue};
    use td_data::DataError;

    use super::*;

    const T: Millis = 1_704_067_200_000; // 2024-01-01 00:00 UTC
    const MIN: Millis = 60_000;

    /// Publishes a rate every 8 hours; the first `fail` reads fail and the first `hide` reads
    /// leave out the newest rate.
    struct Rates {
        fail: AtomicU32,
        hide: AtomicU32,
        reads: AtomicU32,
    }

    impl Rates {
        fn new(fail: u32, hide: u32) -> Self {
            Rates {
                fail: AtomicU32::new(fail),
                hide: AtomicU32::new(hide),
                reads: AtomicU32::new(0),
            }
        }
    }

    fn take_one(counter: &AtomicU32) -> bool {
        let left = counter.load(Ordering::SeqCst);
        left > 0
            && counter
                .compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
    }

    impl Rest for Rates {
        fn klines(
            &self,
            _: &InstrumentId,
            _: Timeframe,
            _: Millis,
            _: Millis,
        ) -> Result<Vec<Candle>, DataError> {
            unreachable!("funding only")
        }

        fn funding(
            &self,
            _: &InstrumentId,
            from: Millis,
            to: Millis,
        ) -> Result<Vec<(Millis, f64)>, DataError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if take_one(&self.fail) {
                return Err(DataError::Api {
                    url: "fake".into(),
                    message: "unavailable".into(),
                });
            }
            let hide = take_one(&self.hide);
            Ok((-2..=2)
                .map(|i| T + i * 8 * HOUR)
                .filter(|t| (from..to).contains(t) && !(hide && *t == T))
                .map(|t| (t, 0.0001))
                .collect())
        }
    }

    fn id() -> InstrumentId {
        InstrumentId {
            venue: Venue::Bybit,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Perpetual,
        }
    }

    fn bar(open_time: Millis) -> Candle {
        Candle {
            open_time,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
            quote_volume: 1.0,
        }
    }

    fn clock() -> FundingClock {
        FundingClock::new(
            &[(T - 16 * HOUR, 0.1), (T - 8 * HOUR, 0.1)],
            T - HOUR,
            Duration::ZERO,
        )
    }

    fn charge(rest: &Rates, open: Millis) -> Result<Vec<Funding>, String> {
        clock().for_bar(rest, &id(), Timeframe::H1, &bar(open), open, &mut |_| {})
    }

    #[test]
    fn the_rate_due_in_a_bar_is_charged_once_with_its_due_time() {
        let rest = Rates::new(0, 0);
        let mut clock = clock();
        assert_eq!(clock.next_due(), T);
        let charged = clock
            .for_bar(&rest, &id(), Timeframe::H1, &bar(T), T, &mut |_| {})
            .unwrap();
        assert_eq!(
            charged,
            [Funding {
                time: T,
                rate: 0.0001,
                due: T
            }]
        );
        let again = clock
            .for_bar(
                &rest,
                &id(),
                Timeframe::H1,
                &bar(T + HOUR),
                T + HOUR,
                &mut |_| {},
            )
            .unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn a_rate_published_late_is_retried_in_its_own_bar() {
        let rest = Rates::new(0, 2);
        assert_eq!(charge(&rest, T).unwrap().len(), 1);
        assert_eq!(rest.reads.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_failed_read_is_retried_and_then_halts() {
        let rest = Rates::new(FUNDING_RETRIES, 0);
        assert_eq!(
            charge(&rest, T).unwrap().len(),
            1,
            "the last retry reads it"
        );
        let rest = Rates::new(FUNDING_RETRIES + 1, 0);
        let error = charge(&rest, T).unwrap_err();
        assert!(error.contains("could not be read"), "{error}");
        assert_eq!(rest.reads.load(Ordering::SeqCst), FUNDING_RETRIES + 1);
    }

    #[test]
    fn a_bar_without_a_full_hour_or_a_rate_due_is_not_asked_about() {
        let rest = Rates::new(0, 0);
        let mut clock = clock();
        let m5 = clock
            .for_bar(
                &rest,
                &id(),
                Timeframe::M5,
                &bar(T - 55 * MIN),
                T - 55 * MIN,
                &mut |_| {},
            )
            .unwrap();
        assert!(m5.is_empty());
        assert_eq!(rest.reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_rate_still_unpublished_when_its_bar_closes_is_warned_about() {
        let rest = Rates::new(0, FUNDING_RETRIES + 1);
        let mut warnings = Vec::new();
        let charged = clock()
            .for_bar(&rest, &id(), Timeframe::H1, &bar(T), T, &mut |w| {
                warnings.push(w)
            })
            .unwrap();
        assert!(charged.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with(&format!(
                "the venue had not published the funding due at {T} when the bar at {T} closed"
            )),
            "{warnings:?}"
        );
    }

    /// The bar at `T` was dropped, so the rate due at `T` comes with the next bar fed. It is not
    /// late: it keeps its own time, before that bar's open, as a backtest of the same bars feeds it.
    #[test]
    fn a_rate_due_in_a_dropped_bar_comes_with_the_next_bar_at_its_own_time() {
        let rest = Rates::new(0, 0);
        let mut warnings = Vec::new();
        // The last bar fed ended at `T`.
        let charged = clock()
            .for_bar(&rest, &id(), Timeframe::H1, &bar(T + HOUR), T, &mut |w| {
                warnings.push(w)
            })
            .unwrap();
        let in_gap = Funding {
            time: T,
            rate: 0.0001,
            due: T,
        };
        assert_eq!(charged, [in_gap]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
