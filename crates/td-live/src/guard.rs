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
//!   which reads the venue's equity, position and mark price and calls `StopState::check`. The
//!   mark is the ticker's, or the position read's own when the ticker fails. When the open
//!   notional cannot be valued at all, the loss limits are still checked on the equity alone
//!   (`StopState::check_losses`) and the check counts as a failed read.
//! - **Pre-trade.** An order that is not reduce-only passes [`SendGate::refusal`] at the one point
//!   where the broker places orders. It is judged by the position it would leave: the venue's
//!   position plus the order, once with the buys the gate let through since the bar began added
//!   and once with the sells, since each of them may or may not have filled. It is refused,
//!   before sending, when either position's notional is above the cap and above the notional
//!   before the order. So an order that only reduces the position is never refused by the cap
//!   while no earlier order of the bar is pending, and the orders of one bar are counted
//!   together.
//! - **A venue that cannot be read** is never a pass. After [`GuardTiming::unreadable`] failed
//!   checks in a row the guard halts the run, and the broker refuses every order that is not
//!   reduce-only; its protective stops still go out, so an entry already filled is still
//!   protected. The guard does not flatten on data it cannot read, since it cannot know the
//!   position.
//! - **When a stop fires**, in this order: the fired state is saved to `stops.json` (a save that
//!   fails does not hold the flatten back; it is tried again at the start of each round, before
//!   each close and when the flatten ends, and the outcome names a save that never went
//!   through); the broker stops sending and the run is asked to halt; every open order on the
//!   instrument with the session's prefix is cancelled and the venue's position is closed with a
//!   reduce-only market order for the size the venue reports, through the route directly (not the
//!   broker, whose "send nothing once faulted" rule stays as it is); then the venue is read again,
//!   and the cancel and close repeat, a pause apart and for a bounded number of rounds, until the
//!   run has finished, the position is flat and no order of the session rests. Each step is
//!   recorded in `stops.json`. A close whose outcome is unknown is settled by reading the venue on
//!   the next round, never by sending it again. Each close carries a new `f` id whose number is
//!   saved before it is sent, so a crash and a retry do not reuse an id while the disk takes the
//!   save, and a reduce-only order can never take the position past flat. The session's status
//!   becomes [`State::Ended`].
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
use td_engine::{StopFired, StopRules, StopState};
use td_orders::{CancelOutcome, Client, ClientOrderId, Error as RouteError, Position};

use crate::ids::{self, Role};
use crate::session::{
    LiveError, LiveSession, LiveStatus, STATUS_FILE, SavedConfig, State, check_id, live_dir,
    read_json, session_dir, take_lock, write_synced,
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
    /// still open at the venue ("fired, not flat"). `unsaved` is `Some` when the last save of
    /// `stops.json` failed, and says what the file does not hold.
    Fired {
        fired: StopFired,
        left: Option<String>,
        unsaved: Option<String>,
    },
    /// The owner flattened a session whose stop had not fired. `left` and `unsaved` as for
    /// `Fired`.
    Flattened {
        left: Option<String>,
        unsaved: Option<String>,
    },
}

impl Outcome {
    /// The command's exit code: 0 for a stop by the owner or a flatten that ended flat, 1 for a
    /// halt, 2 for a fired stop flattened, 3 for anything left open at the venue or a stop file
    /// that could not be saved at the end of a flatten.
    pub fn exit_code(&self) -> u8 {
        match self {
            Outcome::Stopped
            | Outcome::Flattened {
                left: None,
                unsaved: None,
            } => 0,
            Outcome::Halted(_) => 1,
            Outcome::Fired {
                left: None,
                unsaved: None,
                ..
            } => 2,
            Outcome::Fired { .. } | Outcome::Flattened { .. } => 3,
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
/// the pre-trade notional cap and the guard's two levels of stopping: a halt on a venue it cannot
/// read, after which only reduce-only orders go out, and a fired stop, after which nothing does.
pub(crate) struct SendGate {
    rules: StopRules,
    /// Set when a stop fires: the broker sends nothing more, its protective stops included.
    stopped: Mutex<Option<String>>,
    /// Set when the venue could not be read for the bound: orders that are not reduce-only are
    /// refused, and the broker's protective stops and closes still go out.
    halted: Mutex<Option<String>>,
    /// The summed quantity of the buys (zero or more) and of the sells (zero or less) that are not
    /// reduce-only let through since the current bar began. Held while an order is judged, so two
    /// orders are judged one after the other.
    passed: Mutex<(f64, f64)>,
}

impl SendGate {
    pub fn new(rules: StopRules) -> SendGate {
        SendGate {
            rules,
            stopped: Mutex::new(None),
            halted: Mutex::new(None),
            passed: Mutex::new((0.0, 0.0)),
        }
    }

    /// Stops all sending through the broker; the first reason given is kept.
    pub fn stop(&self, reason: &str) {
        lock(&self.stopped).get_or_insert_with(|| reason.to_string());
    }

    /// Whether a fired stop has stopped all sending, and why.
    pub fn stopped(&self) -> Option<String> {
        lock(&self.stopped).clone()
    }

    /// Refuses every order that is not reduce-only from now on; the first reason given is kept.
    pub fn halt(&self, reason: &str) {
        lock(&self.halted).get_or_insert_with(|| reason.to_string());
    }

    /// A new bar has begun: the orders let through on the bars before are in the venue's
    /// position by now, or never filled.
    pub fn new_bar(&self) {
        *lock(&self.passed) = (0.0, 0.0);
    }

    /// Why an order may not go out now whatever its size, or `None`.
    fn blocked(&self, reduce_only: bool) -> Option<String> {
        if let Some(reason) = self.stopped() {
            return Some(format!("the stop guard stopped all sending: {reason}"));
        }
        if reduce_only {
            return None;
        }
        lock(&self.halted)
            .as_ref()
            .map(|reason| format!("the stop guard refuses new exposure: {reason}"))
    }

    /// Why `request` may not be placed now, or `None` when it may. Every order is refused once a
    /// stop has fired, and an order that is not reduce-only once the guard has halted.
    ///
    /// An order that is not reduce-only is judged by the position it would leave: the venue's
    /// signed position plus the order's signed quantity, once with the buys this gate let through
    /// since the bar began added and once with its sells added (each may or may not have filled),
    /// valued at the position's mark price (or at `last`, the order's last known price, when the
    /// venue gives no mark). It is refused when either notional is above the cap and above the
    /// notional before it, or when the position cannot be read. So an order that shrinks the
    /// position is never refused by the cap while no earlier order of the bar is pending, whether
    /// or not it is reduce-only.
    pub fn refusal(
        &self,
        client: &Client,
        instrument: &Instrument,
        request: &OrderRequest,
        last: f64,
    ) -> Option<String> {
        if let Some(reason) = self.blocked(request.reduce_only) {
            return Some(reason);
        }
        if request.reduce_only {
            return None;
        }
        let mut passed = lock(&self.passed);
        let position = match client.position(instrument) {
            Ok(position) => position,
            Err(e) => {
                return Some(format!(
                    "the open notional could not be read for the notional cap: {e}"
                ));
            }
        };
        // The guard may have fired or halted while the position was read.
        if let Some(reason) = self.blocked(false) {
            return Some(reason);
        }
        let signed = match request.side {
            Side::Buy => request.qty.abs(),
            Side::Sell => -request.qty.abs(),
        };
        // Each order let through earlier in the bar is in the venue's position already or still
        // pending, so the real starting position lies between the position with the bar's sells
        // added and the position with its buys added. The positions an order passes from form one
        // unbroken range, so passing at both ends covers every mix of fills.
        // shortcut: near the cap this can refuse an order that would fit, in either direction,
        // until the next bar clears the sums; it never lets through one that takes the position
        // past the cap. Upgrade to reading the orders' fills back if strategies place several
        // orders a bar near the cap.
        let price = valuation_price(&position, last);
        let check = |before: f64| {
            self.rules
                .check_order(before.abs() * price, (before + signed).abs() * price)
        };
        let (buys, sells) = &mut *passed;
        match check(position.size + *buys).and_then(|()| check(position.size + *sells)) {
            Ok(()) if signed >= 0.0 => {
                *buys += signed;
                None
            }
            Ok(()) => {
                *sells += signed;
                None
            }
            Err(e) => Some(e.to_string()),
        }
    }
}

/// The price a position is valued at for the cap: its mark price, or `fallback` when the venue
/// gives no mark.
fn valuation_price(position: &Position, fallback: f64) -> f64 {
    if position.mark_price > 0.0 {
        position.mark_price
    } else {
        fallback
    }
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
    /// The check could not be made in full; `failures` in a row so far.
    Unreadable {
        failures: u32,
        reason: String,
    },
}

/// What the venue said for one check.
struct VenueNow {
    time: Millis,
    equity: f64,
    /// The open notional and whether the position is flat, or why the notional could not be
    /// valued.
    open: Result<(f64, bool), String>,
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
    /// Why the last save of `record` failed, while `stops.json` does not hold its last change.
    unsaved: Option<String>,
}

impl Guard {
    fn event(&self, line: String) {
        let _ = lock(&self.events).send(line);
        self.wake.set(|f| f.poke = true);
    }

    fn half_lot(&self) -> f64 {
        self.instrument.lot_size / 2.0
    }

    /// Reads the venue for a check: the settle coin's equity with open P&L included, and the open
    /// notional. Only an equity that cannot be read fails the read.
    fn read_venue(&self) -> Result<VenueNow, String> {
        let equity = self
            .client
            .equity()
            .map_err(|e| format!("the equity could not be read: {e}"))?
            .equity;
        Ok(VenueNow {
            time: self.client.server_now(),
            equity,
            open: self.open_notional(),
        })
    }

    /// The open notional and whether the position is flat: the venue position's size at the
    /// ticker's mark price, or at the position read's own mark price when the ticker fails or
    /// gives no usable mark.
    fn open_notional(&self) -> Result<(f64, bool), String> {
        let position = self
            .client
            .position(&self.instrument)
            .map_err(|e| format!("the position could not be read: {e}"))?;
        if position.size.abs() < self.half_lot() {
            return Ok((0.0, true));
        }
        let usable = |mark: f64| mark.is_finite() && mark > 0.0;
        let mark = match self.client.ticker(&self.instrument.id.symbol) {
            Ok(ticker) if usable(ticker.mark_price) => ticker.mark_price,
            _ if usable(position.mark_price) => position.mark_price,
            ticker => {
                let ticker = match ticker {
                    Ok(t) => format!("the ticker gave a mark price of {}", t.mark_price),
                    Err(e) => format!("the ticker could not be read ({e})"),
                };
                return Err(format!(
                    "{ticker} and the position read gave a mark price of {}",
                    position.mark_price
                ));
            }
        };
        Ok((position.size.abs() * mark, false))
    }

    /// Saves the record, and on success forgets an earlier failed save.
    fn save(&self, inner: &mut Inner) -> Result<(), String> {
        match inner.record.save(&self.dir) {
            Ok(()) => {
                if inner.unsaved.take().is_some() {
                    self.event("the stop state was saved after an earlier failed save".into());
                }
                Ok(())
            }
            Err(e) => {
                let e = e.to_string();
                inner.unsaved = Some(e.clone());
                Err(e)
            }
        }
    }

    /// The one stop check: reads the venue and calls `StopState::check` with the time, the
    /// venue's equity and the open notional.
    ///
    /// Losses are measured from the equity this reads from the venue (the settle coin's equity,
    /// open P&L included), never from a figure the strategy or the session's own bookkeeping
    /// computes. The only saved number the limits rest on is the starting balance, which
    /// `StopState::restore` checks against `config.json`. Open notional is the venue position's
    /// size times the venue's mark price. When the notional cannot be valued, the loss limits are
    /// still checked on the equity alone (`StopState::check_losses`), and the check counts as a
    /// failed read.
    ///
    /// The state is saved whenever it changes (a new day's baseline, a fired stop) before this
    /// returns. A fired stop is acted on whether or not its save succeeded; a new day's baseline
    /// that could not be saved is kept in memory and reported once. A failed save is tried again
    /// at the start of each later check. An equity read that fails or a check the state refuses
    /// is not a pass: it counts toward the failures in a row.
    fn check(&self) -> Check {
        let mut inner = lock(&self.inner);
        if let Some(fired) = inner.record.state.fired() {
            return Check::Fired(*fired);
        }
        if inner.unsaved.is_some() {
            let _ = self.save(&mut inner);
        }
        let now = match self.read_venue() {
            Ok(now) => now,
            Err(reason) => return self.failed(&mut inner, reason),
        };
        let time = now.time.max(inner.last_time);
        let day_ends = inner.record.state.day_ends();
        let checked = match &now.open {
            Ok((notional, _)) => inner.record.state.check(time, now.equity, *notional),
            Err(_) => inner.record.state.check_losses(time, now.equity),
        };
        let fired = match checked {
            Ok(fired) => fired,
            Err(e) => return self.failed(&mut inner, e.to_string()),
        };
        inner.last_time = time;
        if let Some(fired) = fired {
            inner
                .record
                .flatten
                .step(format!("fired, saved before anything is sent: {fired}"));
            if let Err(e) = self.save(&mut inner) {
                // A fired stop is acted on whether or not it reached the disk: the account is
                // better flat than exposed.
                if let Some(step) = inner.record.flatten.steps.last_mut() {
                    *step = format!(
                        "fired: {fired} The save before the flatten failed and the flatten went \
                         ahead: {e}"
                    );
                }
                self.event(format!(
                    "the fired stop could not be saved, and the flatten goes ahead: {e}"
                ));
            }
            return Check::Fired(fired);
        }
        if inner.record.state.day_ends() != day_ends {
            let reported = inner.unsaved.is_some();
            if let Err(e) = self.save(&mut inner)
                && !reported
            {
                self.event(format!(
                    "the new day's starting equity could not be saved; the guard keeps it and \
                     tries the save again at each check: {e}"
                ));
            }
        }
        let (notional, flat) = match now.open {
            Ok(open) => open,
            Err(reason) => {
                return self.failed(
                    &mut inner,
                    format!(
                        "the loss limits were checked, but the open notional could not be \
                         valued: {reason}"
                    ),
                );
            }
        };
        inner.failures = 0;
        if !inner.shown_clear {
            inner.shown_clear = true;
            self.event(format!(
                "check: no stop fired (equity {:.2}, open notional {notional:.2})",
                now.equity
            ));
        }
        Check::Clear {
            flat,
            equity: now.equity,
            notional,
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

    /// The reason a check gives the run to halt, or `None` to go on. A fired stop also stops the
    /// broker sending anything; the last allowed failure stops it sending anything that is not
    /// reduce-only, so an entry already filled still gets its protective stop.
    fn halt_reason(&self, check: &Check) -> Option<String> {
        match check {
            Check::Fired(fired) => {
                let reason = format!("a stop fired: {fired}");
                self.gate.stop(&reason);
                Some(reason)
            }
            Check::Unreadable { failures, reason } if *failures >= self.timing.unreadable => {
                // shortcut: the guard halts on a venue it cannot read but does not flatten, since
                // it cannot know the position; the position and the limits are then unwatched
                // until the venue answers again or the owner runs `flatten`. Upgrade to a
                // flatten on reads that come back, or to a second route, if outages turn out long.
                let reason = format!(
                    "the venue could not be read for {failures} checks in a row, so the session \
                     halted and sends no order that could add to the position: {reason}"
                );
                self.gate.halt(&reason);
                Some(reason)
            }
            _ => None,
        }
    }
}

/// Whether `stops.json` holds the record a flatten works on.
struct Disk<'a> {
    dir: &'a Path,
    /// Why the last save failed, while the disk does not hold the record's last change.
    unsaved: Option<String>,
    /// Whether a save of the record has gone through, so the disk holds the fired stop (or, for
    /// the owner's flatten, the record it loaded).
    saved_once: bool,
}

impl Disk<'_> {
    fn save(&mut self, record: &StopRecord) -> Result<(), String> {
        match record.save(self.dir) {
            Ok(()) => {
                self.unsaved = None;
                self.saved_once = true;
                Ok(())
            }
            Err(e) => {
                let e = e.to_string();
                self.unsaved = Some(e.clone());
                Err(e)
            }
        }
    }
}

/// The route-side flatten, shared by the guard and by [`flatten_session`].
struct Flattener<'a> {
    client: &'a Client,
    instrument: &'a Instrument,
    prefix: &'a str,
    timing: GuardTiming,
}

impl Flattener<'_> {
    /// Cancels the session's resting orders and closes the venue's position, round by round,
    /// until the run has finished, the position is flat and no order of the session rests, or the
    /// rounds run out. Each step is recorded in `record` and shown with `out`. A save that failed
    /// is tried again at the start of each round; a failed save never holds a round back.
    ///
    /// `done(wait)` says whether the run has finished; with `wait` it first waits for it to. It is
    /// asked before each round's reads, so an order the run had in flight when it was halted shows
    /// in them. A round that finds the venue clean while the run is still ending waits for it
    /// (the run has been halted and sending has stopped, so it is finishing at most one route
    /// call, which the route's timeouts bound) and reads again.
    fn run(
        &self,
        record: &mut StopRecord,
        disk: &mut Disk,
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
            if disk.unsaved.is_some() && disk.save(record).is_ok() {
                note(
                    record,
                    format!("round {round}: the stop file was saved after an earlier failed save"),
                );
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
            let read = match self.client.position(self.instrument) {
                Ok(read) => read,
                Err(e) => {
                    note(
                        record,
                        format!("round {round}: the position could not be read: {e}"),
                    );
                    continue;
                }
            };
            let position = read.size;
            let flat = position.abs() < half;
            if flat && ours.is_empty() {
                if finished {
                    note(
                        record,
                        format!(
                            "round {round}: the venue position is flat and no order of the \
                             session rests"
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
                let line = self.close(record, disk, &read);
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

    /// Sends one reduce-only market order for the size of the venue's position `read`, under a new
    /// `f` id whose number is saved first. When that save fails the order is sent all the same,
    /// under the id it claimed. It is priced at the ticker's last price, or at the position read's
    /// mark price when the ticker gives none. Returns what happened.
    fn close(&self, record: &mut StopRecord, disk: &mut Disk, read: &Position) -> String {
        let usable = |price: f64| price.is_finite() && price > 0.0;
        let last = match self.client.ticker(&self.instrument.id.symbol) {
            Ok(t) if usable(t.last_price) => t.last_price,
            _ if usable(read.mark_price) => read.mark_price,
            Ok(t) => {
                return format!(
                    "not closed: the venue gave a last price of {} and a mark price of {}",
                    t.last_price, read.mark_price
                );
            }
            Err(e) => {
                return format!(
                    "not closed: the last price could not be read ({e}) and the position read \
                     gave a mark price of {}",
                    read.mark_price
                );
            }
        };
        record.flatten.last_id += 1;
        let unsaved = disk.save(record).err().map_or_else(String::new, |e| {
            format!(" (the flatten id could not be saved first, and the close went ahead: {e})")
        });
        let cid = ids::client_id(self.prefix, Role::Flatten, OrderId(record.flatten.last_id));
        let size = read.size;
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
        let line = match self.client.place(self.instrument, &request, &cid, last) {
            Ok(_) => format!(
                "sent {cid}, a reduce-only market {:?} of {qty}",
                request.side
            ),
            Err(e @ RouteError::OutcomeUnknown { .. }) => format!(
                "the outcome of {cid} is unknown, so the venue is read again before anything \
                 else is sent: {e}"
            ),
            Err(e) => format!("{cid} was not taken: {e}"),
        };
        format!("{line}{unsaved}")
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
            unsaved: None,
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
    // Why the run ended, once it has, and the session it gave back. Set when `run` is taken.
    let mut ended: Option<(String, LiveSession)> = None;
    let why_ended = |ended: Option<(String, LiveSession)>| {
        ended.map_or_else(|| "the run ended".to_string(), |(reason, _)| reason)
    };
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
            let Some(run) = run.take() else {
                out("stopped: the loss limits are not enforced while td-live is not running");
                return Outcome::Halted(why_ended(ended.take()));
            };
            let (_session, result) = run.stop();
            out(
                "stopped: the position and its resting orders stay at the venue, and the loss \
                 limits are not enforced while td-live is not running; `td-live flatten` closes \
                 the position",
            );
            return match result {
                Ok(()) => Outcome::Stopped,
                Err(e) => Outcome::Halted(e.to_string()),
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
                return Outcome::Halted(why_ended(ended.take()));
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
    let (mut record, unsaved) = {
        let inner = lock(&guard.inner);
        (inner.record.clone(), inner.unsaved.clone())
    };
    let mut disk = Disk {
        dir: &guard.dir,
        saved_once: unsaved.is_none(),
        unsaved,
    };
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
        timing: guard.timing,
    };
    for line in rx.try_iter() {
        out(&line);
    }
    let result = flattener.run(&mut record, &mut disk, &mut done, &mut |l| out(&l));
    drop(done);
    for line in rx.try_iter().chain(ran) {
        out(&line);
    }
    let outcome = finish(&mut record, fired_outcome(fired), result, &mut disk);
    *lock(&guard.inner) = Inner {
        record,
        failures: 0,
        last_time: Millis::MIN,
        shown_clear: false,
        unsaved: disk.unsaved,
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
    Outcome::Fired {
        fired,
        left: None,
        unsaved: None,
    }
}

/// Records `result`, saves the record a last time, and returns `outcome` with what was left open
/// at the venue and what the stop file does not hold, if anything.
fn finish(
    record: &mut StopRecord,
    outcome: Outcome,
    result: FlattenResult,
    disk: &mut Disk,
) -> Outcome {
    let left = match &result {
        FlattenResult::Flat => None,
        FlattenResult::NotFlat { left } => Some(left.clone()),
    };
    record.flatten.result = Some(result);
    let fired = match outcome {
        Outcome::Fired { fired, .. } => Some(fired),
        _ => None,
    };
    let unsaved = disk.save(record).err().map(|e| {
        let missing = match (disk.saved_once, fired) {
            (true, _) => "how the flatten ended",
            (false, Some(_)) => "the fired stop",
            (false, None) => "this flatten",
        };
        format!("stops.json could not be saved, so it does not record {missing}: {e}")
    });
    match fired {
        Some(fired) => Outcome::Fired {
            fired,
            left,
            unsaved,
        },
        None => Outcome::Flattened { left, unsaved },
    }
}

/// The status reason of an ended session.
fn ended_reason(outcome: &Outcome) -> String {
    let done = |not_flat: &str, left: &Option<String>, unsaved: &Option<String>| {
        let done = match left {
            None => {
                "the session's orders were cancelled and the venue position is flat".to_string()
            }
            Some(left) => format!("{not_flat}: still open at the venue: {left}"),
        };
        match unsaved {
            None => done,
            Some(unsaved) => format!("{done}; {unsaved}"),
        }
    };
    match outcome {
        Outcome::Fired {
            fired,
            left,
            unsaved,
        } => format!("{fired} {}", done("fired, not flat", left, unsaved)),
        Outcome::Flattened { left, unsaved } => {
            format!(
                "flattened by the owner: {}",
                done("not flat", left, unsaved)
            )
        }
        Outcome::Stopped => "stopped".into(),
        Outcome::Halted(reason) => reason.clone(),
    }
}

/// Cancels the resting orders of the session `id` and closes its venue position, reduce-only,
/// with the same rounds as a fired stop, and ends the session: its status becomes
/// [`State::Ended`]. For a session that is not running: halted, faulted, stopped, or one whose
/// stop fired and whose flatten did not finish. Nothing else is done at the venue.
///
/// The session's lock is held throughout, so it cannot run meanwhile. The stop state's flatten
/// counter keeps every close under a new id. A stop file that is missing, unreadable or refused
/// does not block the flatten: it is replaced by a fresh record that says so, whose flatten ids
/// count on from the venue's clock in milliseconds, far above any count the lost record could
/// have reached. A link in its place is refused. A status file that cannot be read is replaced.
pub fn flatten_session(
    data_dir: &Path,
    id: &str,
    client: &Client,
    timing: GuardTiming,
    out: &mut dyn FnMut(&str),
) -> Result<Outcome, LiveError> {
    check_id(id)?;
    let dir = session_dir(data_dir, id)?;
    let config_path = dir.join(crate::session::CONFIG_FILE);
    if !config_path.exists() {
        return Err(LiveError::NotFound(dir));
    }
    let saved: SavedConfig = read_json(&config_path)?;
    let _lock = take_lock(&dir)?;
    let (mut record, saved_once) = match StopRecord::load(&dir, &saved) {
        Ok(record) => (record, true),
        Err(LiveError::StopFile { reason, .. }) => {
            let now = client.server_now();
            let mut record = StopRecord::new(StopState::start(
                saved.config.stops,
                saved.starting_balance,
                now,
            )?);
            record.flatten.last_id = u64::try_from(now).unwrap_or(0);
            let line = format!(
                "the stop file could not be read ({reason}), so this flatten keeps a fresh \
                 record; its flatten ids count on from the venue's clock"
            );
            out(&format!("flatten: {line}"));
            record.flatten.step(line);
            (record, false)
        }
        Err(e) => return Err(e),
    };
    let outcome = match record.state.fired() {
        Some(fired) => fired_outcome(*fired),
        None => Outcome::Flattened {
            left: None,
            unsaved: None,
        },
    };
    let mut disk = Disk {
        dir: &dir,
        unsaved: None,
        saved_once,
    };
    let flattener = Flattener {
        client,
        instrument: &saved.config.instrument,
        prefix: &saved.prefix,
        timing,
    };
    let result = flattener.run(&mut record, &mut disk, &mut |_| true, &mut |l| out(&l));
    let outcome = finish(&mut record, outcome, result, &mut disk);
    let status_path = dir.join(STATUS_FILE);
    let mut status: LiveStatus = read_json(&status_path).unwrap_or_else(|e| {
        out(&format!(
            "the status could not be read, so a fresh one is written: {e}"
        ));
        LiveStatus::not_started(None)
    });
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
    use std::time::Duration;

    use td_core::{FeeSchedule, InstrumentId, InstrumentKind, Venue};
    use td_engine::DayRollover;
    use td_mock_bybit::{MockVenue, TEST_KEY, TEST_SECRET};
    use td_orders::{DemoKeys, Endpoint, Timing};

    use super::*;

    const SYMBOL: &str = "BTCUSDT";
    const PRICE: f64 = 65000.0;
    /// 260 of notional at [`PRICE`].
    const QTY: f64 = 0.004;

    #[test]
    fn exit_codes_tell_a_stop_a_halt_a_fire_and_what_is_left_apart() {
        let fired = StopFired {
            rule: td_engine::StopRule::MaxLoss,
            time: 0,
            equity: 1.0,
            notional: Some(0.0),
            threshold: 2.0,
        };
        let some = || Some("x".to_string());
        assert_eq!(Outcome::Stopped.exit_code(), 0);
        let flattened = |left, unsaved| Outcome::Flattened { left, unsaved };
        assert_eq!(flattened(None, None).exit_code(), 0);
        assert_eq!(Outcome::Halted("x".into()).exit_code(), 1);
        let fire = |left, unsaved| Outcome::Fired {
            fired,
            left,
            unsaved,
        };
        assert_eq!(fire(None, None).exit_code(), 2);
        // Something left at the venue, or a stop file that does not hold the end, even when flat.
        assert_eq!(fire(some(), None).exit_code(), 3);
        assert_eq!(fire(None, some()).exit_code(), 3);
        assert_eq!(flattened(some(), None).exit_code(), 3);
        assert_eq!(flattened(None, some()).exit_code(), 3);
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
        assert_eq!(valuation_price(&p, 11.0), 12.0);
        p.mark_price = 0.0;
        assert_eq!(valuation_price(&p, 11.0), 11.0);
    }

    fn instrument() -> Instrument {
        Instrument {
            id: InstrumentId {
                venue: Venue::Bybit,
                symbol: SYMBOL.into(),
                kind: InstrumentKind::Perpetual,
            },
            base: "BTC".into(),
            quote: "USDT".into(),
            tick_size: 0.1,
            lot_size: 0.001,
            min_qty: 0.002,
            min_notional: 100.0,
            fees: FeeSchedule {
                maker: 0.0002,
                taker: 0.00055,
                per_unit: 0.0,
            },
        }
    }

    /// The mock with the instrument at [`PRICE`] and a position of `size`, a client for it, and a
    /// gate with a cap of 400.
    fn gate_at(size: f64) -> (MockVenue, Client, SendGate) {
        let mock = MockVenue::start();
        mock.add_instrument(&instrument(), PRICE);
        if size != 0.0 {
            mock.set_position(SYMBOL, size, PRICE);
        }
        let timing = Timing {
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_millis(1500),
            recv_window: Duration::from_millis(1000),
            settle: Duration::from_millis(50),
        };
        let keys = DemoKeys::for_loopback(TEST_KEY, TEST_SECRET).unwrap();
        let client = Client::with_timing(Endpoint::Loopback(mock.port()), keys, timing).unwrap();
        let gate = SendGate::new(StopRules {
            daily_loss_share: 0.05,
            max_loss_share: 0.1,
            open_notional_cap: 400.0,
            rollover: DayRollover::default(),
        });
        (mock, client, gate)
    }

    fn market(side: Side, qty: f64, reduce_only: bool) -> OrderRequest {
        OrderRequest {
            instrument: instrument().id,
            side,
            qty,
            order_type: OrderType::Market,
            tif: TimeInForce::Gtc,
            reduce_only,
            client_tag: None,
        }
    }

    #[test]
    fn the_cap_judges_an_order_by_the_position_it_leaves() {
        // A long of 260, above half the cap of 400, so an order counted as added notional would
        // pass the cap whatever its side.
        let refused = |side, qty| {
            let (_mock, client, gate) = gate_at(QTY);
            gate.refusal(&client, &instrument(), &market(side, qty, false), PRICE)
        };
        assert_eq!(
            refused(Side::Sell, QTY),
            None,
            "an exit that is not reduce-only"
        );
        assert_eq!(refused(Side::Sell, 0.002), None, "a partial exit");
        assert_eq!(
            refused(Side::Sell, 2.0 * QTY),
            None,
            "a flip to a short of 260"
        );
        let over = refused(Side::Sell, 3.0 * QTY).expect("a flip to a short of 520");
        assert!(over.contains("above the cap of 400"), "{over}");
        assert!(refused(Side::Buy, QTY).is_some(), "an add to 520");
        // A short is judged the same way.
        let (_mock, client, gate) = gate_at(-QTY);
        let buy = |qty| {
            gate.refusal(
                &client,
                &instrument(),
                &market(Side::Buy, qty, false),
                PRICE,
            )
        };
        assert_eq!(buy(QTY), None);
    }

    #[test]
    fn two_orders_let_through_in_one_bar_are_counted_together() {
        let (mock, client, gate) = gate_at(0.0);
        let buy = || {
            gate.refusal(
                &client,
                &instrument(),
                &market(Side::Buy, QTY, false),
                PRICE,
            )
        };
        // Nothing reaches the venue here, so its position stays flat throughout.
        assert_eq!(buy(), None);
        let second = buy().expect("260 + 260 is above the cap of 400");
        assert!(second.contains("from 260 to 520"), "{second}");
        // An exit from what was let through still passes. The first buy may fill before the sell,
        // so another buy is refused until the next bar, judged from 260: the refused order is not
        // counted.
        let sell = gate.refusal(
            &client,
            &instrument(),
            &market(Side::Sell, QTY, false),
            PRICE,
        );
        assert_eq!(sell, None);
        let third = buy().expect("the first buy may fill before the sell");
        assert!(third.contains("from 260 to 520"), "{third}");
        gate.new_bar();
        assert_eq!(buy(), None, "the next bar starts from the venue's position");
        assert_eq!(mock.position(SYMBOL), 0.0);
    }

    #[test]
    fn an_order_that_filled_earlier_in_the_bar_cannot_hide_a_flip_past_the_cap() {
        let order = |gate: &SendGate, client: &Client, side, qty| {
            gate.refusal(client, &instrument(), &market(side, qty, false), PRICE)
        };
        // Long 260; an exit passes and fills, then a buy of 520 would leave a long of 520.
        let (mock, client, gate) = gate_at(QTY);
        assert_eq!(order(&gate, &client, Side::Sell, QTY), None);
        mock.set_position(SYMBOL, 0.0, PRICE);
        let buy = order(&gate, &client, Side::Buy, 2.0 * QTY).expect("a long of 520");
        assert!(buy.contains("from 0 to 520"), "{buy}");
        // Flat; a buy passes and fills, then a sell of 780 would leave a short of 520.
        let (mock, client, gate) = gate_at(0.0);
        assert_eq!(order(&gate, &client, Side::Buy, QTY), None);
        mock.set_position(SYMBOL, QTY, PRICE);
        let sell = order(&gate, &client, Side::Sell, 3.0 * QTY).expect("a short of 520");
        assert!(sell.contains("from 260 to 520"), "{sell}");
    }

    #[test]
    fn a_pending_buy_cannot_hide_behind_a_filled_sell_of_the_same_bar() {
        let order = |gate: &SendGate, client: &Client, side, qty| {
            gate.refusal(client, &instrument(), &market(side, qty, false), PRICE)
        };
        // Flat; a sell of 260 passes and fills, a buy of 520 passes and is still pending, then a
        // buy of 260 would leave a long of 520 once both fill.
        let (mock, client, gate) = gate_at(0.0);
        assert_eq!(order(&gate, &client, Side::Sell, QTY), None);
        mock.set_position(SYMBOL, -QTY, PRICE);
        assert_eq!(order(&gate, &client, Side::Buy, 2.0 * QTY), None);
        let buy = order(&gate, &client, Side::Buy, QTY).expect("a long of 520");
        assert!(buy.contains("from 260 to 520"), "{buy}");
    }

    #[test]
    fn a_halt_refuses_new_exposure_and_a_fired_stop_refuses_everything() {
        let (_mock, client, gate) = gate_at(QTY);
        let refused = |request: OrderRequest| gate.refusal(&client, &instrument(), &request, PRICE);
        gate.halt("the venue could not be read");
        let entry = refused(market(Side::Buy, 0.002, false)).expect("an entry");
        assert!(entry.contains("refuses new exposure"), "{entry}");
        assert!(refused(market(Side::Sell, QTY, false)).is_some());
        assert_eq!(refused(market(Side::Sell, QTY, true)), None, "reduce-only");
        assert_eq!(
            gate.stopped(),
            None,
            "the broker's protective stops still go out"
        );
        gate.stop("a stop fired");
        let close = refused(market(Side::Sell, QTY, true)).expect("reduce-only too");
        assert!(
            close.contains("stopped all sending: a stop fired"),
            "{close}"
        );
    }
}
