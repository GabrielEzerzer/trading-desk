//! The stop guard: the session's loss limits and open-notional cap, checked against the venue and
//! enforced by the engine, and the flatten that follows a fired stop.
//!
//! [`run_guarded`] is how a live session runs with its stops. It owns no strategy state: it holds
//! the session's stop state ([`td_engine::StopState`], restored from `stops.json`), the order
//! route, and the broker's [`SendGate`].
//!
//! - **Before trading.** On every start and resume, one check runs before the first bar is fed
//!   and before the broker may send anything. A venue that cannot be read then refuses the run; a
//!   stop that fires then is flattened at once and the session never trades.
//! - **While running.** The check runs after every live bar (the run's `after_bar` hook) and on a
//!   timer between bars ([`GuardTiming::interval`]) from the guard's own thread, which does not
//!   wait for bars, so it keeps checking if the feed stalls or ends. Both go through one function,
//!   which reads the venue's equity, position and mark price and calls `StopState::check`.
//! - **Pre-trade.** An order that is not reduce-only passes [`SendGate::refusal`] at the one point
//!   where the broker places orders: refused, before sending, when the open notional plus the
//!   order's notional at its last known price would pass the cap. A reduce-only order is never
//!   refused by the cap.
//! - **A venue that cannot be read** is never a pass. After [`GuardTiming::unreadable`] failed
//!   checks in a row the guard stops the broker sending and halts the run. It does not flatten on
//!   data it cannot read, since it cannot know the position.
//! - **When a stop fires**, in this order: the fired state is saved to `stops.json`; the broker
//!   stops sending and the run is asked to halt; every open order on the instrument with the
//!   session's prefix is cancelled and the venue's position is closed with a reduce-only market
//!   order for the size the venue reports, through the route directly (not the broker, whose
//!   "send nothing once faulted" rule stays as it is); then the venue is read again, and the
//!   cancel and close repeat, a pause apart and for a bounded number of rounds, until the run has
//!   finished, the position is flat and no order of the session rests. Each step is recorded in
//!   `stops.json`. A close whose outcome is unknown is settled by reading the venue on the next
//!   round, never by sending it again. Each close carries a new `f` id whose number is saved
//!   before it is sent, so a crash and a retry never reuse an id, and a reduce-only order can
//!   never take the position past flat. The session's status becomes [`State::Ended`].
//! - **After the run ends** for another reason (a halt, a fault, a feed that ended), the guard
//!   keeps checking while the process lives and the venue position is not known to be flat.
//!
//! A session whose stop has fired never trades again: `LiveSession::open` refuses it, and
//! [`flatten_session`] finishes a flatten a crash or the round bound left unfinished.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use td_core::{Instrument, Millis, OrderId, OrderRequest, OrderType, Side, TimeInForce};
use td_data::live::{Rest, SharedStream};
use td_engine::{StopFired, StopRules};
use td_orders::{CancelOutcome, Client, ClientOrderId, Error as RouteError, Position};

use crate::ids::{self, Role};
use crate::session::{
    LiveError, LiveSession, LiveStatus, STATUS_FILE, SavedConfig, State, check_id, live_dir,
    read_json, take_lock, write_synced,
};
use crate::stop_file::{FlattenResult, StopRecord};

/// The guard's pace and bounds. [`GuardTiming::default`] is what the command uses; tests shorten
/// it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuardTiming {
    /// The pause between two timer checks. Default 15 s.
    pub interval: Duration,
    /// The pause between two flatten rounds. Default 2 s.
    pub pause: Duration,
    /// Flatten rounds before the guard gives up and records "fired, not flat". Default 10.
    pub rounds: u32,
    /// Failed checks in a row before the guard halts the run. Default 4, about a minute at the
    /// default interval.
    pub unreadable: u32,
}

impl Default for GuardTiming {
    fn default() -> Self {
        GuardTiming {
            interval: Duration::from_secs(15),
            pause: Duration::from_secs(2),
            rounds: 10,
            unreadable: 4,
        }
    }
}

/// How a guarded run, or a flatten, ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The owner stopped the run while it was running; what it holds stays at the venue.
    Stopped,
    /// The run halted or faulted, or could not start; the reason says why.
    Halted(String),
    /// A stop fired. `left` is `None` when the flatten ended flat, and otherwise says what is
    /// still open at the venue ("fired, not flat").
    Fired {
        fired: StopFired,
        left: Option<String>,
    },
    /// The owner flattened a session whose stop had not fired. `left` as for `Fired`.
    Flattened { left: Option<String> },
}

impl Outcome {
    /// The command's exit code: 0 for a stop by the owner or a flatten that ended flat, 1 for a
    /// halt, 2 for a fired stop flattened, 3 for anything left open at the venue.
    pub fn exit_code(&self) -> u8 {
        match self {
            Outcome::Stopped | Outcome::Flattened { left: None } => 0,
            Outcome::Halted(_) => 1,
            Outcome::Fired { left: None, .. } => 2,
            Outcome::Fired { left: Some(_), .. } | Outcome::Flattened { left: Some(_) } => 3,
        }
    }
}

/// Asks a guarded run to stop, as Ctrl-C does. Clone it to hand to a signal handler.
#[derive(Clone, Default)]
pub struct StopHandle {
    wake: Arc<Wake>,
}

impl StopHandle {
    pub fn new() -> StopHandle {
        StopHandle::default()
    }

    /// Asks the guarded run to stop. A running session is stopped cleanly and what it holds stays
    /// at the venue; a flatten already under way finishes first.
    pub fn stop(&self) {
        self.wake.set(|f| f.stop = true);
    }
}

#[derive(Default)]
struct Wake {
    flags: Mutex<Flags>,
    cv: Condvar,
}

#[derive(Default, Clone, Copy)]
struct Flags {
    stop: bool,
    poke: bool,
}

impl Wake {
    fn set(&self, f: impl FnOnce(&mut Flags)) {
        f(&mut lock(&self.flags));
        self.cv.notify_all();
    }

    /// Waits until a stop or a poke, or `deadline`, and returns whether a stop was asked. A poke is
    /// consumed.
    fn wait(&self, deadline: Instant) -> bool {
        let mut flags = lock(&self.flags);
        while !flags.stop && !flags.poke {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            flags = self
                .cv
                .wait_timeout(flags, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        flags.poke = false;
        flags.stop
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The broker's gate: every order it places passes here first. It holds the session's rules for
/// the pre-trade notional cap, and the guard's stop of all sending.
pub(crate) struct SendGate {
    rules: StopRules,
    stopped: Mutex<Option<String>>,
}

impl SendGate {
    pub fn new(rules: StopRules) -> SendGate {
        SendGate {
            rules,
            stopped: Mutex::new(None),
        }
    }

    /// Stops all sending through the broker; the first reason given is kept.
    pub fn stop(&self, reason: &str) {
        lock(&self.stopped).get_or_insert_with(|| reason.to_string());
    }

    pub fn stopped(&self) -> Option<String> {
        lock(&self.stopped).clone()
    }

    /// Why `request` may not be placed now, or `None` when it may. Every order is refused once
    /// sending has stopped. An order that is not reduce-only is refused when the venue's open
    /// notional (its position at the mark price) plus the order's quantity at `last`, its last
    /// known price, would pass the cap, or when the position cannot be read. A reduce-only order
    /// is never refused by the cap.
    pub fn refusal(
        &self,
        client: &Client,
        instrument: &Instrument,
        request: &OrderRequest,
        last: f64,
    ) -> Option<String> {
        if let Some(reason) = self.stopped() {
            return Some(format!("the stop guard stopped all sending: {reason}"));
        }
        if request.reduce_only {
            return None;
        }
        let position = match client.position(instrument) {
            Ok(position) => position,
            Err(e) => {
                return Some(format!(
                    "the open notional could not be read for the notional cap: {e}"
                ));
            }
        };
        let open = position_notional(&position, last);
        self.rules
            .check_order(open, request.qty.abs() * last)
            .err()
            .map(|e| e.to_string())
    }
}

/// The position's notional at its mark price, or at `fallback` when the venue gives no mark.
fn position_notional(position: &Position, fallback: f64) -> f64 {
    let price = if position.mark_price > 0.0 {
        position.mark_price
    } else {
        fallback
    };
    position.size.abs() * price
}

/// One check's answer.
#[derive(Debug, Clone, PartialEq)]
enum Check {
    /// No stop fired; `flat` is whether the venue position is flat.
    Clear {
        flat: bool,
        equity: f64,
        notional: f64,
    },
    Fired(StopFired),
    /// The check could not be made; `failures` in a row so far.
    Unreadable {
        failures: u32,
        reason: String,
    },
}

/// What the venue said for one check.
struct VenueNow {
    time: Millis,
    equity: f64,
    notional: f64,
    flat: bool,
}

/// The guard's shared side: the check, used from the run's thread after each bar and from the
/// guard's own thread on its timer.
struct Guard {
    dir: PathBuf,
    client: Arc<Client>,
    instrument: Instrument,
    gate: Arc<SendGate>,
    timing: GuardTiming,
    inner: Mutex<Inner>,
    wake: Arc<Wake>,
    events: Mutex<mpsc::Sender<String>>,
}

struct Inner {
    record: StopRecord,
    failures: u32,
    /// The time of the last check that passed, so a clock resync cannot move time back.
    last_time: Millis,
    /// Whether the last check shown was clear, so a clear check is shown only on a change.
    shown_clear: bool,
}

impl Guard {
    fn event(&self, line: String) {
        let _ = lock(&self.events).send(line);
        self.wake.set(|f| f.poke = true);
    }

    fn half_lot(&self) -> f64 {
        self.instrument.lot_size / 2.0
    }

    /// Reads the venue for a check: the settle coin's equity with open P&L included, the position,
    /// and, when there is one, the mark price.
    fn read_venue(&self) -> Result<VenueNow, String> {
        let equity = self.client.equity().map_err(|e| e.to_string())?.equity;
        let position = self
            .client
            .position(&self.instrument)
            .map_err(|e| e.to_string())?;
        let flat = position.size.abs() < self.half_lot();
        let notional = if flat {
            0.0
        } else {
            let mark = self
                .client
                .ticker(&self.instrument.id.symbol)
                .map_err(|e| e.to_string())?
                .mark_price;
            if !(mark.is_finite() && mark > 0.0) {
                return Err(format!("the venue gave a mark price of {mark}"));
            }
            position.size.abs() * mark
        };
        Ok(VenueNow {
            time: self.client.server_now(),
            equity,
            notional,
            flat,
        })
    }

    /// The one stop check: reads the venue and calls `StopState::check` with the time, the
    /// venue's equity and the open notional.
    ///
    /// Losses are measured from the equity this reads from the venue (the settle coin's equity,
    /// open P&L included), never from a figure the strategy or the session's own bookkeeping
    /// computes. The only saved number the limits rest on is the starting balance, which
    /// `StopState::restore` checks against `config.json`. Open notional is the venue position's
    /// size times the venue's mark price.
    ///
    /// The state is saved whenever it changes (a new day's baseline, a fired stop) before this
    /// returns, and a fired stop is saved before anything is sent. A read that fails, a check the
    /// state refuses, or a save that fails is not a pass: it counts toward the failures in a row.
    fn check(&self) -> Check {
        let mut inner = lock(&self.inner);
        if let Some(fired) = inner.record.state.fired() {
            return Check::Fired(*fired);
        }
        let now = match self.read_venue() {
            Ok(now) => now,
            Err(reason) => return self.failed(&mut inner, reason),
        };
        let time = now.time.max(inner.last_time);
        let day_ends = inner.record.state.day_ends();
        let fired = match inner.record.state.check(time, now.equity, now.notional) {
            Ok(fired) => fired,
            Err(e) => return self.failed(&mut inner, e.to_string()),
        };
        if fired.is_some() || inner.record.state.day_ends() != day_ends {
            if let Some(fired) = fired {
                inner
                    .record
                    .flatten
                    .step(format!("fired, saved before anything is sent: {fired}"));
            }
            if let Err(e) = inner.record.save(&self.dir) {
                let reason = format!("the stop state could not be saved: {e}");
                if let Some(fired) = fired {
                    // A fired stop is acted on whether or not it reached the disk: the account
                    // is better flat than exposed.
                    self.event(format!("{reason}; the flatten goes ahead"));
                    return Check::Fired(fired);
                }
                return self.failed(&mut inner, reason);
            }
        }
        inner.failures = 0;
        inner.last_time = time;
        if let Some(fired) = fired {
            return Check::Fired(fired);
        }
        if !inner.shown_clear {
            inner.shown_clear = true;
            self.event(format!(
                "check: no stop fired (equity {:.2}, open notional {:.2})",
                now.equity, now.notional
            ));
        }
        Check::Clear {
            flat: now.flat,
            equity: now.equity,
            notional: now.notional,
        }
    }

    fn failed(&self, inner: &mut Inner, reason: String) -> Check {
        inner.failures += 1;
        inner.shown_clear = false;
        let failures = inner.failures;
        self.event(format!(
            "check failed ({failures} of {} in a row allowed): {reason}",
            self.timing.unreadable
        ));
        Check::Unreadable { failures, reason }
    }

    /// The reason a check gives the run to halt, or `None` to go on. A fired stop or the last
    /// allowed failure also stops the broker sending.
    fn halt_reason(&self, check: &Check) -> Option<String> {
        let reason = match check {
            Check::Fired(fired) => format!("a stop fired: {fired}"),
            Check::Unreadable { failures, reason } if *failures >= self.timing.unreadable => {
                // shortcut: the guard halts on a venue it cannot read but does not flatten, since
                // it cannot know the position; the position and the limits are then unwatched
                // until the venue answers again or the owner runs `flatten`. Upgrade to a
                // flatten on reads that come back, or to a second route, if outages turn out long.
                format!(
                    "the venue could not be read for {failures} checks in a row, so the session \
                     halted without sending anything: {reason}"
                )
            }
            _ => return None,
        };
        self.gate.stop(&reason);
        Some(reason)
    }
}

/// The route-side flatten, shared by the guard and by [`flatten_session`].
struct Flattener<'a> {
    client: &'a Client,
    instrument: &'a Instrument,
    prefix: &'a str,
    dir: &'a Path,
    timing: GuardTiming,
}

impl Flattener<'_> {
    /// Cancels the session's resting orders and closes the venue's position, round by round,
    /// until the run has finished, the position is flat and no order of the session rests, or the
    /// rounds run out. Each step is recorded in `record` and shown with `out`.
    ///
    /// `done(wait)` says whether the run has finished; with `wait` it first waits for it to. It is
    /// asked before each round's reads, so an order the run had in flight when it was halted shows
    /// in them. A round that finds the venue clean while the run is still ending waits for it
    /// (the run has been halted and sending has stopped, so it is finishing at most one route
    /// call, which the route's timeouts bound) and reads again.
    fn run(
        &self,
        record: &mut StopRecord,
        done: &mut dyn FnMut(bool) -> bool,
        out: &mut dyn FnMut(String),
    ) -> FlattenResult {
        let half = self.instrument.lot_size / 2.0;
        let mut left = String::from("the venue could not be read");
        let mut note = |record: &mut StopRecord, line: String| {
            out(format!("flatten: {line}"));
            record.flatten.step(line);
        };
        for round in 1..=self.timing.rounds {
            if round > 1 {
                std::thread::sleep(self.timing.pause);
            }
            let finished = done(false);
            let open = match self.client.open_orders(self.instrument) {
                Ok(open) => open,
                Err(e) => {
                    note(
                        record,
                        format!("round {round}: the open orders could not be read: {e}"),
                    );
                    continue;
                }
            };
            let ours: Vec<_> = open
                .into_iter()
                .filter(|o| ids::parse(self.prefix, &o.client_id).is_some())
                .collect();
            let position = match self.client.position(self.instrument) {
                Ok(p) => p.size,
                Err(e) => {
                    note(
                        record,
                        format!("round {round}: the position could not be read: {e}"),
                    );
                    continue;
                }
            };
            let flat = position.abs() < half;
            if flat && ours.is_empty() {
                if finished {
                    note(
                        record,
                        format!(
                            "round {round}: the venue position is flat and no order of the                              session rests"
                        ),
                    );
                    return FlattenResult::Flat;
                }
                note(
                    record,
                    format!("round {round}: the venue is clean; waiting for the run to end"),
                );
                done(true);
                continue;
            }
            left = format!(
                "a position of {position} and {} resting orders of the session{}",
                ours.len(),
                if finished {
                    ""
                } else {
                    ", and the run had not finished"
                }
            );
            for order in &ours {
                let line = match ClientOrderId::new(order.client_id.clone()) {
                    Ok(cid) => match self.client.cancel(self.instrument, &cid) {
                        Ok(CancelOutcome::Cancelled) => format!("cancelled {cid}"),
                        Ok(CancelOutcome::AlreadyClosed(status)) => {
                            format!("{cid} had already closed ({status:?})")
                        }
                        Err(e) => format!("cancelling {cid} failed: {e}"),
                    },
                    Err(e) => format!("{} cannot be cancelled by id: {e}", order.client_id),
                };
                note(record, format!("round {round}: {line}"));
            }
            if !flat {
                let line = self.close(record, position);
                note(record, format!("round {round}: {line}"));
            }
        }
        note(
            record,
            format!(
                "fired, not flat after {} rounds: {left}",
                self.timing.rounds
            ),
        );
        FlattenResult::NotFlat { left }
    }

    /// Sends one reduce-only market order for the venue's position `size`, under a new `f` id
    /// whose number is saved first. Returns what happened.
    fn close(&self, record: &mut StopRecord, size: f64) -> String {
        let last = match self.client.ticker(&self.instrument.id.symbol) {
            Ok(t) if t.last_price.is_finite() && t.last_price > 0.0 => t.last_price,
            Ok(t) => {
                return format!(
                    "not closed: the venue gave a last price of {}",
                    t.last_price
                );
            }
            Err(e) => return format!("not closed: the last price could not be read: {e}"),
        };
        record.flatten.last_id += 1;
        if let Err(e) = record.save(self.dir) {
            return format!("not closed: the flatten id could not be saved first: {e}");
        }
        let cid = ids::client_id(self.prefix, Role::Flatten, OrderId(record.flatten.last_id));
        let qty = self.instrument.round_qty(size.abs());
        let request = OrderRequest {
            instrument: self.instrument.id.clone(),
            side: if size > 0.0 { Side::Sell } else { Side::Buy },
            qty,
            order_type: OrderType::Market,
            tif: TimeInForce::Ioc,
            reduce_only: true,
            client_tag: None,
        };
        match self.client.place(self.instrument, &request, &cid, last) {
            Ok(_) => format!(
                "sent {cid}, a reduce-only market {:?} of {qty}",
                request.side
            ),
            Err(e @ RouteError::OutcomeUnknown { .. }) => format!(
                "the outcome of {cid} is unknown, so the venue is read again before anything \
                 else is sent: {e}"
            ),
            Err(e) => format!("{cid} was not taken: {e}"),
        }
    }
}

/// Runs `live` with its stops until it ends, and returns how. See the module documentation.
///
/// `rest` and `stream` are the run's feed, as `LiveSession::run` takes them. `out` gets one line
/// per event: a bar fed, the orders the session holds, a check whose result changed, a fired stop
/// and each flatten step, a halt. `stop` ends the run as Ctrl-C does.
pub fn run_guarded<S>(
    live: LiveSession,
    rest: Arc<dyn Rest>,
    stream: S,
    timing: GuardTiming,
    stop: &StopHandle,
    out: &mut dyn FnMut(&str),
) -> Outcome
where
    S: FnOnce() -> Arc<SharedStream> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let guard = Arc::new(Guard {
        dir: live.dir().to_path_buf(),
        client: live.client(),
        instrument: live.config().config.instrument.clone(),
        gate: live.gate.clone(),
        timing,
        inner: Mutex::new(Inner {
            record: live.stops.clone(),
            failures: 0,
            last_time: Millis::MIN,
            shown_clear: false,
        }),
        wake: stop.wake.clone(),
        events: Mutex::new(tx),
    });
    let prefix = live.config().prefix.clone();
    let show = |rx: &mpsc::Receiver<String>, out: &mut dyn FnMut(&str)| {
        for line in rx.try_iter() {
            out(&line);
        }
    };

    // Before trading: one check, before the first bar and before the broker may send anything.
    let first = guard.check();
    show(&rx, out);
    let mut live = live;
    match first {
        Check::Fired(fired) => {
            let reason = guard.halt_reason(&Check::Fired(fired)).unwrap_or_default();
            out(&format!("halt: {reason}"));
            return fire(&guard, &prefix, fired, None, Some(live), &rx, out);
        }
        Check::Unreadable { reason, .. } => {
            let reason = format!(
                "the venue could not be read before trading, so the session did not run: {reason}"
            );
            let _ = live.set_state(State::Halted {
                reason: reason.clone(),
            });
            out(&format!("halt: {reason}"));
            return Outcome::Halted(reason);
        }
        Check::Clear { .. } => {}
    }

    let after_bar = {
        let guard = guard.clone();
        let mut known: Vec<u64> = Vec::new();
        let mut position = f64::NAN;
        move |status: &LiveStatus, _: &crate::VenueHandle| {
            if let Some(bar) = status.last_bar {
                guard.event(format!("bar fed: {bar}"));
            }
            for order in &status.open_orders {
                if !known.contains(&order.id.0) {
                    known.push(order.id.0);
                    let r = &order.request;
                    guard.event(format!(
                        "order {} held: {:?} {} {:?}{}",
                        order.id.0,
                        r.side,
                        r.qty,
                        r.order_type,
                        if r.reduce_only { ", reduce-only" } else { "" }
                    ));
                }
            }
            if status.position != position {
                position = status.position;
                guard.event(format!("position: {position}"));
            }
            let check = guard.check();
            let reason = guard.halt_reason(&check);
            if reason.is_some() {
                guard.wake.set(|f| f.poke = true);
            }
            reason
        }
    };
    let mut run = Some(live.run(rest, stream, after_bar));
    // Why the run ended, once it has, and the session it gave back.
    let mut ended: Option<(String, LiveSession)> = None;
    let mut halted = false;
    let mut next_check = Instant::now() + timing.interval;
    loop {
        let stopping = guard.wake.wait(next_check);
        show(&rx, out);
        // Bound first: an `if let` on the lock would hold it through `fire`, which takes it.
        let fired = lock(&guard.inner).record.state.fired().copied();
        if let Some(fired) = fired {
            let session = ended.take().map(|(_, s)| s);
            return fire(&guard, &prefix, fired, run, session, &rx, out);
        }
        if run.as_ref().is_some_and(|r| r.is_finished()) {
            let (session, result) = run.take().expect("checked").wait();
            let reason = match result {
                Ok(()) => "the run ended".to_string(),
                Err(e) => e.to_string(),
            };
            out(&format!("halt: {reason}"));
            ended = Some((reason, session));
        }
        if stopping {
            return match (run.take(), ended.take()) {
                (Some(run), _) => {
                    let (_session, result) = run.stop();
                    out(
                        "stopped: the position and its resting orders stay at the venue, and the \
                         loss limits are not enforced while td-live is not running; `td-live \
                         flatten` closes the position",
                    );
                    match result {
                        Ok(()) => Outcome::Stopped,
                        Err(e) => Outcome::Halted(e.to_string()),
                    }
                }
                (None, Some((reason, _))) => {
                    out("stopped: the loss limits are not enforced while td-live is not running");
                    Outcome::Halted(reason)
                }
                (None, None) => Outcome::Halted("stopped".into()),
            };
        }
        if Instant::now() < next_check {
            continue;
        }
        next_check = Instant::now() + timing.interval;
        let check = guard.check();
        show(&rx, out);
        match check {
            Check::Fired(_) => {
                guard.halt_reason(&check);
                guard.wake.set(|f| f.poke = true);
            }
            Check::Unreadable { .. } => {
                if let Some(reason) = guard.halt_reason(&check)
                    && let Some(run) = &run
                    && !halted
                {
                    halted = true;
                    run.halt(reason);
                }
            }
            Check::Clear { flat: true, .. } if run.is_none() => {
                let reason = ended
                    .take()
                    .map_or_else(|| "the run ended".into(), |(r, _)| r);
                return Outcome::Halted(reason);
            }
            Check::Clear { .. } => {}
        }
    }
}

/// A fired stop: stops sending, halts the run, flattens, and records how it ended.
fn fire(
    guard: &Guard,
    prefix: &str,
    fired: StopFired,
    run: Option<crate::LiveRun>,
    session: Option<LiveSession>,
    rx: &mpsc::Receiver<String>,
    out: &mut dyn FnMut(&str),
) -> Outcome {
    let reason = format!("a stop fired: {fired}");
    guard.gate.stop(&reason);
    if let Some(run) = &run {
        run.halt(reason.clone());
    }
    out(&format!("stop fired: {fired}"));
    let mut record = lock(&guard.inner).record.clone();
    let (mut run, mut session) = (run, session);
    let mut ran = Vec::new();
    let mut done = |wait: bool| {
        if run.as_ref().is_some_and(|r| wait || r.is_finished()) {
            let (s, result) = run.take().expect("checked").wait();
            if let Err(e) = result {
                ran.push(format!("the run ended: {e}"));
            }
            session = Some(s);
        }
        run.is_none()
    };
    let flattener = Flattener {
        client: &guard.client,
        instrument: &guard.instrument,
        prefix,
        dir: &guard.dir,
        timing: guard.timing,
    };
    for line in rx.try_iter() {
        out(&line);
    }
    let result = flattener.run(&mut record, &mut done, &mut |l| out(&l));
    drop(done);
    for line in rx.try_iter().chain(ran) {
        out(&line);
    }
    let outcome = finish(&mut record, fired_outcome(fired), result, &guard.dir);
    if let Err(e) = record.save(&guard.dir) {
        out(&format!("the stop state could not be saved: {e}"));
    }
    *lock(&guard.inner) = Inner {
        record,
        failures: 0,
        last_time: Millis::MIN,
        shown_clear: false,
    };
    if let Some(session) = &mut session
        && let Err(e) = session.set_state(State::Ended {
            reason: ended_reason(&outcome),
        })
    {
        out(&format!("the status could not be written: {e}"));
    }
    out(&ended_reason(&outcome));
    outcome
}

fn fired_outcome(fired: StopFired) -> Outcome {
    Outcome::Fired { fired, left: None }
}

/// Records `result` and returns `outcome` with what was left open, if anything.
fn finish(
    record: &mut StopRecord,
    outcome: Outcome,
    result: FlattenResult,
    _dir: &Path,
) -> Outcome {
    let left = match &result {
        FlattenResult::Flat => None,
        FlattenResult::NotFlat { left } => Some(left.clone()),
    };
    record.flatten.result = Some(result);
    match outcome {
        Outcome::Fired { fired, .. } => Outcome::Fired { fired, left },
        _ => Outcome::Flattened { left },
    }
}

/// The status reason of an ended session.
fn ended_reason(outcome: &Outcome) -> String {
    let done = |left: &Option<String>| match left {
        None => "the session's orders were cancelled and the venue position is flat".to_string(),
        Some(left) => format!("fired, not flat: still open at the venue: {left}"),
    };
    match outcome {
        Outcome::Fired { fired, left } => format!("{fired} {}", done(left)),
        Outcome::Flattened { left } => format!("flattened by the owner: {}", done(left)),
        Outcome::Stopped => "stopped".into(),
        Outcome::Halted(reason) => reason.clone(),
    }
}

/// Cancels the resting orders of the session `id` and closes its venue position, reduce-only,
/// with the same rounds as a fired stop, and ends the session: its status becomes
/// [`State::Ended`]. For a session that is not running: halted, faulted, stopped, or one whose
/// stop fired and whose flatten did not finish. Nothing else is done at the venue.
///
/// The session's lock is held throughout, so it cannot run meanwhile. The stop state must load
/// as for an open: its flatten counter keeps every close under a new id.
pub fn flatten_session(
    data_dir: &Path,
    id: &str,
    client: &Client,
    timing: GuardTiming,
    out: &mut dyn FnMut(&str),
) -> Result<Outcome, LiveError> {
    check_id(id)?;
    let dir = live_dir(data_dir, id);
    let config_path = dir.join(crate::session::CONFIG_FILE);
    if !config_path.exists() {
        return Err(LiveError::NotFound(dir));
    }
    let saved: SavedConfig = read_json(&config_path)?;
    let _lock = take_lock(&dir)?;
    let mut record = StopRecord::load(&dir, &saved)?;
    let outcome = match record.state.fired() {
        Some(fired) => fired_outcome(*fired),
        None => Outcome::Flattened { left: None },
    };
    let flattener = Flattener {
        client,
        instrument: &saved.config.instrument,
        prefix: &saved.prefix,
        dir: &dir,
        timing,
    };
    let result = flattener.run(&mut record, &mut |_| true, &mut |l| out(&l));
    let outcome = finish(&mut record, outcome, result, &dir);
    record.save(&dir)?;
    let status_path = dir.join(STATUS_FILE);
    let mut status: LiveStatus = read_json(&status_path)?;
    status.state = State::Ended {
        reason: ended_reason(&outcome),
    };
    let json = serde_json::to_vec_pretty(&status).map_err(|e| LiveError::BadFile {
        path: status_path.clone(),
        reason: e.to_string(),
    })?;
    write_synced(&dir, STATUS_FILE, &json)?;
    out(&ended_reason(&outcome));
    Ok(outcome)
}

/// Whether the session `id` has a fired stop whose flatten has not finished flat. Reads its files
/// only; takes no lock and touches no venue.
pub(crate) fn flatten_unfinished(data_dir: &Path, id: &str) -> Result<bool, LiveError> {
    check_id(id)?;
    let dir = live_dir(data_dir, id);
    let saved: SavedConfig = read_json(&dir.join(crate::session::CONFIG_FILE))?;
    Ok(StopRecord::load(&dir, &saved)?.flatten_unfinished())
}

/// The saved status and stop state of the session `id`, as their files hold them, for the
/// `status` command. Touches no venue and takes no lock.
pub fn saved_files(
    data_dir: &Path,
    id: &str,
) -> Result<(serde_json::Value, serde_json::Value), LiveError> {
    check_id(id)?;
    let dir = live_dir(data_dir, id);
    if !dir.join(crate::session::CONFIG_FILE).exists() {
        return Err(LiveError::NotFound(dir));
    }
    let status = read_json(&dir.join(STATUS_FILE))?;
    let stops = read_json(&dir.join(crate::stop_file::STOP_FILE))?;
    Ok((status, stops))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_tell_a_stop_a_halt_a_fire_and_what_is_left_apart() {
        let fired = StopFired {
            rule: td_engine::StopRule::MaxLoss,
            time: 0,
            equity: 1.0,
            notional: 0.0,
            threshold: 2.0,
        };
        assert_eq!(Outcome::Stopped.exit_code(), 0);
        assert_eq!(Outcome::Flattened { left: None }.exit_code(), 0);
        assert_eq!(Outcome::Halted("x".into()).exit_code(), 1);
        assert_eq!(Outcome::Fired { fired, left: None }.exit_code(), 2);
        let left = Some("a position".to_string());
        assert_eq!(
            Outcome::Fired {
                fired,
                left: left.clone()
            }
            .exit_code(),
            3
        );
        assert_eq!(Outcome::Flattened { left }.exit_code(), 3);
    }

    #[test]
    fn a_position_is_valued_at_its_mark_and_at_the_last_price_without_one() {
        let mut p = Position {
            symbol: "X".into(),
            size: -2.0,
            avg_price: 10.0,
            unrealised_pnl: 0.0,
            mark_price: 12.0,
        };
        assert_eq!(position_notional(&p, 11.0), 24.0);
        p.mark_price = 0.0;
        assert_eq!(position_notional(&p, 11.0), 22.0);
    }
}
