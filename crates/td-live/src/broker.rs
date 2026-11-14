//! [`VenueBroker`], its shared [`VenueHandle`], faults and the replay record.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use td_core::{
    Candle, Fill, Instrument, Liquidity, Millis, OrderId, OrderRequest, OrderType, Side,
    TimeInForce, Timeframe,
};
use td_engine::{Broker, BrokerEvent, Order, OrderStatus, Portfolio};
use td_orders::{
    CancelOutcome, Client, ClientOrderId, Equity, Error as RouteError, ExecKind, Execution,
    Position, VenueOrder,
};

use crate::guard::SendGate;
use crate::ids::{self, InvalidPrefix, Role};

/// How far before the newest execution seen each read starts, for an execution the venue lists
/// late with an earlier time. Executions are deduplicated by id.
const LOOKBACK_MS: Millis = 60_000;
/// How many times an entry's fill is looked for before its stop waits for the next bar.
const FILL_POLLS: u32 = 5;
/// The pause between two looks at the venue for the same order.
const POLL_PAUSE: Duration = Duration::from_millis(100);
/// Failed asks about an order whose outcome is unknown before the broker faults.
const UNKNOWN_TRIES: u32 = 3;
/// Bars in a row on which the venue could not be read before the broker faults.
const UNREADABLE_BARS: u32 = 3;

const FAULTED: &str = "not sent: the broker stopped on a fault";
const STALE: &str = "not sent: it was decided on a bar that closed more than one bar length ago";
const NOT_HELD: &str = "not filled: the venue holds no order under its id";
const ENTRY_NOT_SENT: &str = "its entry was not sent";

/// What kind of disagreement stopped the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// A protective stop could not be placed or resized, and the position could not be closed at
    /// market in its place.
    ProtectionFailed,
    /// The venue could not say whether an order whose request outcome was lost exists.
    UnresolvedOrder,
    /// A trade on the instrument carries no id of this session.
    ForeignExecution,
    /// A trade carries this session's prefix but no order it is tracking.
    UnknownOrder,
    /// The venue made an execution the session cannot model, such as a liquidation or ADL.
    UnmodelledExecution,
    /// The venue's position differs from the fills reported.
    PositionMismatch,
    /// The venue lists an open order the session does not hold open.
    UnknownOpenOrder,
    /// An order the session holds open is gone from the venue without a fill to explain it.
    MissingOrder,
    /// The session's position moved without the broker's fills, such as by its own liquidation.
    SessionDiverged,
    /// The session's liquidation model would close the position on this bar; the venue did not.
    SessionLiquidation,
    /// The venue could not be read for several bars in a row.
    VenueUnreadable,
    /// The venue already held an id from before its order was decided: the prefix was reused.
    IdCollision,
    /// A replayed session made calls that differ from the record, or went past it.
    ReplayDiverged,
}

impl fmt::Display for FaultKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FaultKind::ProtectionFailed => "protection failed",
            FaultKind::UnresolvedOrder => "unresolved order",
            FaultKind::ForeignExecution => "foreign execution",
            FaultKind::UnknownOrder => "unknown order",
            FaultKind::UnmodelledExecution => "unmodelled execution",
            FaultKind::PositionMismatch => "position mismatch",
            FaultKind::UnknownOpenOrder => "unknown open order",
            FaultKind::MissingOrder => "missing order",
            FaultKind::SessionDiverged => "session diverged",
            FaultKind::SessionLiquidation => "session liquidation",
            FaultKind::VenueUnreadable => "venue unreadable",
            FaultKind::IdCollision => "id collision",
            FaultKind::ReplayDiverged => "replay diverged",
        })
    }
}

/// Why the broker stopped: what the session believed and what the venue said.
///
/// The message is built from quantities, prices, ids, statuses and the order route's own error
/// text, which holds no key, secret, signature or raw response body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct BrokerFault {
    pub kind: FaultKind,
    pub message: String,
}

/// Everything the broker did since the last [`VenueHandle::take_record`]: the calls the session
/// made and their answers, then the broker's state. Serialisable, so the owner can store it after
/// each live bar and hand it back to a restarted session ([`VenueHandle::replay`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrokerRecord {
    calls: Vec<Call>,
    book: Book,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Call {
    Submit {
        id: OrderId,
    },
    Cancel {
        id: OrderId,
        cancelled: bool,
    },
    Bar {
        open_time: Millis,
        events: Vec<BrokerEvent>,
    },
}

/// The broker's state, as a record carries it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct Book {
    orders: BTreeMap<u64, Tracked>,
    /// Cancellations decided between bars, reported on the next bar.
    pending: Vec<BrokerEvent>,
    /// The signed sum of the fills reported.
    tally: f64,
    /// Server time when the broker was built: earlier executions are not this session's to judge.
    floor: Millis,
    /// The time of the newest execution processed.
    cursor: Millis,
    /// Executions processed, by id, with their times, kept for [`LOOKBACK_MS`].
    seen: BTreeMap<String, Millis>,
    read_failures: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Tracked {
    /// The order as the session holds it.
    order: Order,
    at: At,
    /// The quantity of the venue order standing for it: the order sent, the stop placed for an
    /// entry's fill, or the close at market sent in a stop's place.
    sent: f64,
    /// The quantity reported in fills.
    reported: f64,
    /// Whether the session holds it open (it is in [`Broker::open_orders`]).
    open: bool,
    /// For an attached stop: the quantity last announced with [`BrokerEvent::Placed`].
    announced: f64,
    /// A cancel was sent and its outcome was lost.
    cancel_asked: bool,
    /// A replayed session cancelled it past the record; the cancel is sent by `go_live`.
    cancel_intent: bool,
}

/// Where an order stands at the venue.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum At {
    /// Not sent; its cancellation is pending.
    Refused,
    /// Sent, outcome unknown, `tries` failed asks so far.
    Unknown { tries: u32 },
    /// Submitted by a replayed session past the record; `go_live` resolves it.
    Intent,
    /// At the venue under its `o` id.
    Live,
    /// A stop replaced by a close at market under its `c` id.
    Closing,
    /// Finished at the venue.
    Done,
}

/// The broker the session owns. See the crate documentation.
pub struct VenueBroker {
    core: Arc<Mutex<Core>>,
    /// The session's open orders, refreshed after every call.
    open: Vec<Order>,
}

/// The owner's view of a [`VenueBroker`], shared with it.
#[derive(Clone)]
pub struct VenueHandle {
    core: Arc<Mutex<Core>>,
}

struct Core {
    client: Arc<Client>,
    instrument: Instrument,
    timeframe: Timeframe,
    prefix: String,
    book: Book,
    fault: Option<BrokerFault>,
    /// Calls since the last record was taken.
    log: Vec<Call>,
    /// Records still to replay; `Some` while replaying.
    replay: Option<VecDeque<BrokerRecord>>,
    /// The stop guard's gate, which every order passes before it is placed: the pre-trade
    /// notional cap, and the guard's stop of all sending. A live session always sets one.
    gate: Option<Arc<SendGate>>,
}

fn lock(core: &Mutex<Core>) -> MutexGuard<'_, Core> {
    core.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl VenueBroker {
    /// A broker for `instrument` on `timeframe`, sending through `client` with ids under
    /// `prefix` (see [`crate::ids`]; never reuse a prefix on the same account). Executions before
    /// now are not this session's.
    pub fn new(
        client: Arc<Client>,
        instrument: Instrument,
        timeframe: Timeframe,
        prefix: &str,
    ) -> Result<(VenueBroker, VenueHandle), InvalidPrefix> {
        ids::check_prefix(prefix)?;
        let now = client.server_now();
        let core = Arc::new(Mutex::new(Core {
            client,
            instrument,
            timeframe,
            prefix: prefix.to_string(),
            book: Book {
                floor: now,
                cursor: now,
                ..Book::default()
            },
            fault: None,
            log: Vec::new(),
            replay: None,
            gate: None,
        }));
        let handle = VenueHandle { core: core.clone() };
        Ok((
            VenueBroker {
                core,
                open: Vec::new(),
            },
            handle,
        ))
    }

    /// Puts every order this broker places behind `gate` (see [`SendGate`]).
    pub(crate) fn gated(self, gate: Arc<SendGate>) -> Self {
        lock(&self.core).gate = Some(gate);
        self
    }

    fn with_core<R>(&mut self, f: impl FnOnce(&mut Core) -> R) -> R {
        let mut core = lock(&self.core);
        let out = f(&mut core);
        self.open = core.open_list();
        out
    }
}

impl Broker for VenueBroker {
    fn submit(&mut self, order: Order) {
        self.with_core(|core| core.submit(order));
    }

    fn cancel(&mut self, id: OrderId) -> bool {
        self.with_core(|core| core.cancel(id))
    }

    /// The venue fills on its own prices, so `ask` is not used.
    fn on_bar(
        &mut self,
        bar: &Candle,
        _ask: &Candle,
        portfolio: &Portfolio,
        breached: &dyn Fn(&Portfolio) -> Option<f64>,
    ) -> Vec<BrokerEvent> {
        self.with_core(|core| core.on_bar(bar, portfolio, breached))
    }

    fn open_orders(&self) -> &[Order] {
        &self.open
    }
}

impl VenueHandle {
    /// The fault that stopped the broker, if any.
    pub fn fault(&self) -> Option<BrokerFault> {
        lock(&self.core).fault.clone()
    }

    /// The record of every call since the last one was taken, with the broker's state now. Take
    /// it after each live `Session::on_bar` returns, and after a manual order or cancel.
    pub fn take_record(&self) -> BrokerRecord {
        let mut core = lock(&self.core);
        BrokerRecord {
            calls: std::mem::take(&mut core.log),
            book: core.book.clone(),
        }
    }

    /// Queues `record` for a session replaying a live run, and puts a broker that has not acted
    /// yet into replay mode: each call then takes its answer from the records in order and sends
    /// nothing. A broker that has already acted live faults instead.
    pub fn replay(&self, record: BrokerRecord) {
        let mut core = lock(&self.core);
        if core.replay.is_none() && (!core.log.is_empty() || !core.book.orders.is_empty()) {
            core.raise(
                FaultKind::ReplayDiverged,
                "a record was handed to a broker that has already acted live".into(),
            );
            return;
        }
        if core.fault.is_none() {
            core.replay
                .get_or_insert_with(VecDeque::new)
                .push_back(record);
        }
    }

    /// Ends a replay: resolves the orders whose outcome the record does not hold, those whose
    /// request outcome was lost and those the session submitted past the record (adopted if the
    /// venue holds their id, otherwise sent or refused as a fresh submission would be), sends
    /// the cancels it made past the record, and reconciles the venue with the recorded state.
    /// Every record must have been replayed. Does nothing on a broker that is not replaying.
    pub fn go_live(&self) -> Result<(), BrokerFault> {
        let mut core = lock(&self.core);
        core.go_live();
        match &core.fault {
            Some(fault) => Err(fault.clone()),
            None => Ok(()),
        }
    }

    /// The venue's position on the instrument.
    pub fn venue_position(&self) -> Result<Position, RouteError> {
        let (client, instrument) = self.route();
        client.position(&instrument)
    }

    /// The account's equity at the venue.
    pub fn equity(&self) -> Result<Equity, RouteError> {
        self.route().0.equity()
    }

    /// The session's orders resting at the venue as the broker holds them.
    pub fn resting_orders(&self) -> Vec<Order> {
        lock(&self.core).open_list()
    }

    // Cancelling the session's resting orders and closing the position is the stop guard's
    // flatten (`crate::guard`), sent through the route under the `f` ids (see `crate::ids`).

    fn route(&self) -> (Arc<Client>, Instrument) {
        let core = lock(&self.core);
        (core.client.clone(), core.instrument.clone())
    }
}

/// Why an order cannot be sent, or `None` if the route takes it.
fn unsupported(request: &OrderRequest, has_stop: bool) -> Option<&'static str> {
    match request.order_type {
        OrderType::Market => None,
        OrderType::Stop { .. } if !request.reduce_only => {
            Some("not sent: the venue route takes a stop only as a reduce-only exit")
        }
        OrderType::Stop { .. } if request.tif != TimeInForce::Gtc => {
            Some("not sent: the venue route takes a stop only good till cancelled")
        }
        OrderType::Stop { .. } if has_stop => {
            Some("not sent: a stop cannot carry a stop of its own")
        }
        OrderType::Stop { .. } => None,
        OrderType::Limit { .. } => Some("not sent: the venue route does not take limit orders"),
        OrderType::StopLimit { .. } => {
            Some("not sent: the venue route does not take stop-limit orders")
        }
    }
}

fn is_stop(order: &Order) -> bool {
    matches!(order.request.order_type, OrderType::Stop { .. })
}

/// The venue refused a stop because the market is already through its trigger.
fn through_market(error: &RouteError) -> bool {
    matches!(
        error,
        RouteError::StopWouldTrigger { .. }
            | RouteError::Rejected {
                code: 110092 | 110093,
                ..
            }
    )
}

/// How a close at market sent in a stop's place failed.
enum CloseFailed {
    /// It was not taken.
    Refused(String),
    /// Its outcome is unknown.
    Unknown(String),
}

impl Core {
    fn half_lot(&self) -> f64 {
        self.instrument.lot_size / 2.0
    }

    fn id(&self, role: Role, id: OrderId) -> ClientOrderId {
        ids::client_id(&self.prefix, role, id)
    }

    /// The id of the venue order standing for `t`.
    fn venue_id(&self, t: &Tracked) -> ClientOrderId {
        let role = if t.at == At::Closing {
            Role::Close
        } else {
            Role::Order
        };
        self.id(role, t.order.id)
    }

    /// Why the gate refuses to place `request` now, priced at `last`, if it does.
    fn gated(&self, request: &OrderRequest, last: f64) -> Option<String> {
        let gate = self.gate.as_ref()?;
        gate.refusal(&self.client, &self.instrument, request, last)
    }

    /// Whether the stop guard has stopped all sending.
    fn sending_stopped(&self) -> bool {
        self.gate.as_ref().is_some_and(|g| g.stopped().is_some())
    }

    fn raise(&mut self, kind: FaultKind, message: String) {
        if self.fault.is_none() {
            self.fault = Some(BrokerFault { kind, message });
        }
    }

    fn open_list(&self) -> Vec<Order> {
        self.book
            .orders
            .values()
            .filter(|t| t.open)
            .map(|t| t.order.clone())
            .collect()
    }

    fn tracked(&mut self, id: OrderId) -> &mut Tracked {
        self.book
            .orders
            .get_mut(&id.0)
            .expect("the order is tracked")
    }

    /// Tracks `order` as one the session holds open.
    fn track(&mut self, order: Order, at: At) {
        self.book.orders.insert(
            order.id.0,
            Tracked {
                order,
                at,
                sent: 0.0,
                reported: 0.0,
                open: true,
                announced: 0.0,
                cancel_asked: false,
                cancel_intent: false,
            },
        );
    }

    /// Not sent: the session learns it is cancelled on the next bar, with its attached stop.
    fn refuse(&mut self, id: OrderId, reason: &str) {
        let t = self.tracked(id);
        t.at = At::Refused;
        let stop = t.order.attached_stop;
        self.book.pending.push(BrokerEvent::Cancelled {
            id,
            reason: reason.into(),
        });
        if let Some(stop) = stop {
            self.book.pending.push(BrokerEvent::Cancelled {
                id: stop.id,
                reason: ENTRY_NOT_SENT.into(),
            });
        }
    }

    fn is_stale(&self, order: &Order) -> bool {
        let bar = self.timeframe.duration_ms();
        self.client.server_now() > order.submitted_at + 2 * bar
    }

    // ---- submit ----

    fn submit(&mut self, order: Order) {
        if self.replay.is_some() {
            return self.replay_submit(order);
        }
        self.log.push(Call::Submit { id: order.id });
        let id = order.id;
        self.track(order.clone(), At::Live);
        if self.fault.is_some() {
            return self.refuse(id, FAULTED);
        }
        if let Some(why) = unsupported(&order.request, order.attached_stop.is_some()) {
            return self.refuse(id, why);
        }
        if self.is_stale(&order) {
            return self.send_stale(id);
        }
        self.send(id);
    }

    /// A stale order is adopted if the venue holds its id, and otherwise not sent.
    fn send_stale(&mut self, id: OrderId) {
        let cid = self.id(Role::Order, id);
        match self.client.order_by_client_id(&self.instrument, &cid) {
            Ok(Some(found)) => self.adopt(id, &found),
            Ok(None) => self.refuse(id, STALE),
            // Never sent: the next bar asks again, and refuses it if the venue holds none.
            Err(_) => self.tracked(id).at = At::Unknown { tries: 1 },
        }
    }

    /// Sends a tracked order and protects an entry's fill.
    fn send(&mut self, id: OrderId) {
        let t = &self.book.orders[&id.0];
        let (request, last) = (t.order.request.clone(), t.order.decision_close);
        if let Some(why) = self.gated(&request, last) {
            return self.refuse(id, &format!("not sent: {why}"));
        }
        let cid = self.id(Role::Order, id);
        match self.client.place(&self.instrument, &request, &cid, last) {
            Ok(_) => {
                let t = self.tracked(id);
                t.at = At::Live;
                t.sent = request.qty;
            }
            Err(RouteError::DuplicateClientId { .. }) => {
                match self.client.order_by_client_id(&self.instrument, &cid) {
                    Ok(Some(found)) => self.adopt(id, &found),
                    _ => self.tracked(id).at = At::Unknown { tries: 0 },
                }
            }
            Err(RouteError::OutcomeUnknown { .. }) => {
                let t = self.tracked(id);
                t.at = At::Unknown { tries: 0 };
                t.sent = request.qty;
            }
            Err(e) if through_market(&e) && is_stop(&self.book.orders[&id.0].order) => {
                // The market is already through a standalone stop: the venue would trigger it at
                // once, so it is closed at market as the simulated broker fills it at the open.
                match self.close_at_market(id, request.qty, last) {
                    Ok(()) => {}
                    Err(CloseFailed::Refused(why)) => {
                        self.refuse(id, &format!("not sent: {e}; closing at market failed: {why}"))
                    }
                    Err(CloseFailed::Unknown(why)) => self.raise(
                        FaultKind::ProtectionFailed,
                        format!("stop {cid} was replaced by a close at market whose outcome is unknown: {why}"),
                    ),
                }
                return;
            }
            Err(e) => return self.refuse(id, &format!("not sent: {e}")),
        }
        if self.book.orders[&id.0].at == At::Live
            && self.book.orders[&id.0].order.attached_stop.is_some()
        {
            self.confirm_and_protect(id);
        }
    }

    /// Takes an order the venue already holds, unless it was created before the order was
    /// decided, which means another session used the prefix.
    fn adopt(&mut self, id: OrderId, found: &VenueOrder) {
        let submitted_at = self.book.orders[&id.0].order.submitted_at;
        if found.created < submitted_at {
            self.raise(
                FaultKind::IdCollision,
                format!(
                    "the venue holds {} from {}, before order {} was decided at {submitted_at}",
                    found.client_id, found.created, id.0
                ),
            );
            return self.refuse(id, NOT_HELD);
        }
        let t = self.tracked(id);
        t.at = At::Live;
        t.sent = found.qty;
        // Its executions may predate the broker; read back to them.
        self.book.cursor = self.book.cursor.min(found.created);
    }

    /// Waits briefly for an entry's fill and places its stop for the quantity filled. If the fill
    /// does not show in time, the stop is placed when the fill is first seen on a bar.
    fn confirm_and_protect(&mut self, entry: OrderId) {
        let cid = self.id(Role::Order, entry);
        let mut filled = None;
        for poll in 0..FILL_POLLS {
            if poll > 0 {
                std::thread::sleep(POLL_PAUSE);
            }
            if let Ok(Some(found)) = self.client.order_by_client_id(&self.instrument, &cid)
                && found.filled_qty > 0.0
            {
                filled = Some((found.filled_qty, found.avg_price));
                if !found.status.is_open() {
                    break;
                }
            }
        }
        if let Some((qty, price)) = filled {
            let last = price.unwrap_or(self.book.orders[&entry.0].order.decision_close);
            self.protect(entry, qty, last);
        }
    }

    /// Makes the entry's attached stop cover `qty`: places it the first time, resizes it after a
    /// further fill. A stop the venue will not take is replaced by a close at market; when that
    /// fails too, or a resize fails, the broker faults.
    fn protect(&mut self, entry: OrderId, qty: f64, last: f64) {
        // The stop guard is flattening: it cancels and closes everything itself.
        if self.sending_stopped() {
            return;
        }
        let e = &self.book.orders[&entry.0];
        let Some(stop) = e.order.attached_stop else {
            return;
        };
        let request = stop.request(&e.order.request, qty);
        let (submitted_at, decision_close) = (e.order.submitted_at, e.order.decision_close);
        let cid = self.id(Role::Order, stop.id);
        let half = self.half_lot();
        match self.book.orders.get(&stop.id.0).map(|t| (t.at, t.sent)) {
            None => {
                let order = Order {
                    id: stop.id,
                    request: request.clone(),
                    submitted_at,
                    decision_close,
                    attached_stop: None,
                    status: OrderStatus::Open,
                };
                self.track(order, At::Live);
                let t = self.tracked(stop.id);
                // The session holds it as attached until the bar that announces it.
                t.open = false;
                t.sent = qty;
                let why = match self.client.place(&self.instrument, &request, &cid, last) {
                    Ok(_) | Err(RouteError::DuplicateClientId { .. }) => return,
                    Err(e) => e.to_string(),
                };
                if let Err(CloseFailed::Refused(e) | CloseFailed::Unknown(e)) =
                    self.close_at_market(stop.id, qty, last)
                {
                    self.raise(
                        FaultKind::ProtectionFailed,
                        format!(
                            "stop {cid} for {qty} was not placed ({why}) and closing at market failed ({e})"
                        ),
                    );
                }
            }
            Some((At::Live, sent)) if qty > sent + half => {
                match self
                    .client
                    .amend_stop(&self.instrument, &cid, None, Some(qty))
                {
                    Ok(()) => {
                        let t = self.tracked(stop.id);
                        t.sent = qty;
                        t.order.request.qty = qty;
                    }
                    Err(e) => self.raise(
                        FaultKind::ProtectionFailed,
                        format!("stop {cid} covers {sent} but its entry filled {qty}; resizing it failed: {e}"),
                    ),
                }
            }
            Some((At::Closing, sent)) if qty > sent + half => self.raise(
                FaultKind::ProtectionFailed,
                format!("entry {} filled {qty} after its stop was replaced by a close at market for {sent}", entry.0),
            ),
            Some(_) => {}
        }
    }

    /// Sends a reduce-only market order for `qty` under order `id`'s `c` id, against the side its
    /// stop would close.
    fn close_at_market(&mut self, id: OrderId, qty: f64, last: f64) -> Result<(), CloseFailed> {
        if self.sending_stopped() {
            return Err(CloseFailed::Refused(
                "the stop guard has stopped all sending".into(),
            ));
        }
        let stop = &self.book.orders[&id.0].order;
        let request = OrderRequest {
            instrument: stop.request.instrument.clone(),
            side: stop.request.side,
            qty,
            order_type: OrderType::Market,
            tif: TimeInForce::Ioc,
            reduce_only: true,
            client_tag: None,
        };
        let cid = self.id(Role::Close, id);
        match self.client.place(&self.instrument, &request, &cid, last) {
            Ok(_) | Err(RouteError::DuplicateClientId { .. }) => {
                let t = self.tracked(id);
                t.at = At::Closing;
                t.sent = qty;
                Ok(())
            }
            Err(e @ RouteError::OutcomeUnknown { .. }) => Err(CloseFailed::Unknown(e.to_string())),
            Err(e) => Err(CloseFailed::Refused(e.to_string())),
        }
    }

    // ---- cancel ----

    fn cancel(&mut self, id: OrderId) -> bool {
        if self.replay.is_some() {
            return self.replay_cancel(id);
        }
        let cancelled = self.cancel_live(id);
        self.log.push(Call::Cancel { id, cancelled });
        cancelled
    }

    fn cancel_live(&mut self, id: OrderId) -> bool {
        let Some(t) = self.book.orders.get(&id.0).filter(|t| t.open) else {
            return false;
        };
        match t.at {
            At::Refused => {
                // Not sent: its pending cancellation (and its stop's) is replaced by this one.
                let stop = t.order.attached_stop.map(|s| s.id);
                self.book.pending.retain(|e| match e {
                    BrokerEvent::Cancelled { id: c, .. } => *c != id && Some(*c) != stop,
                    _ => true,
                });
                let t = self.tracked(id);
                t.at = At::Done;
                t.open = false;
                true
            }
            At::Live if self.fault.is_none() => {
                let cid = self.id(Role::Order, id);
                match self.client.cancel(&self.instrument, &cid) {
                    Ok(CancelOutcome::Cancelled) => {
                        let t = self.tracked(id);
                        t.at = At::Done;
                        t.open = false;
                        true
                    }
                    // Its fill arrives through the executions.
                    Ok(CancelOutcome::AlreadyClosed(_)) => false,
                    Err(RouteError::OutcomeUnknown { .. }) => {
                        self.tracked(id).cancel_asked = true;
                        false
                    }
                    Err(_) => false,
                }
            }
            _ => false,
        }
    }

    // ---- on_bar ----

    fn on_bar(
        &mut self,
        bar: &Candle,
        portfolio: &Portfolio,
        breached: &dyn Fn(&Portfolio) -> Option<f64>,
    ) -> Vec<BrokerEvent> {
        if self.replay.is_some() {
            return self.replay_bar(bar);
        }
        let events = self.live_bar(bar, portfolio, breached);
        self.log.push(Call::Bar {
            open_time: bar.open_time,
            events: events.clone(),
        });
        events
    }

    fn live_bar(
        &mut self,
        bar: &Candle,
        portfolio: &Portfolio,
        breached: &dyn Fn(&Portfolio) -> Option<f64>,
    ) -> Vec<BrokerEvent> {
        if self.fault.is_none() && (portfolio.position() - self.book.tally).abs() > self.half_lot()
        {
            self.raise(
                FaultKind::SessionDiverged,
                format!(
                    "the session holds {} but the venue's fills reported add up to {}",
                    portfolio.position(),
                    self.book.tally
                ),
            );
        }
        let mut fills = Vec::new();
        if self.fault.is_none() {
            match self.read_bar(bar, &mut fills) {
                Ok(()) => self.book.read_failures = 0,
                Err(e) => {
                    self.book.read_failures += 1;
                    if self.book.read_failures >= UNREADABLE_BARS {
                        self.raise(
                            FaultKind::VenueUnreadable,
                            format!("{UNREADABLE_BARS} bars in a row without a full read: {e}"),
                        );
                    }
                }
            }
        }
        if self.fault.is_none() {
            let mut after = portfolio.clone();
            for event in &fills {
                if let BrokerEvent::Filled(f) = event {
                    after.apply_fill(f.side, f.qty, f.price, f.fee);
                }
            }
            if let Some(price) = breached(&after) {
                self.raise(
                    FaultKind::SessionLiquidation,
                    format!(
                        "the session's model liquidates its position of {} at {price} on the bar \
                         opening at {}, which the venue did not do",
                        after.position(),
                        bar.open_time
                    ),
                );
            }
        }
        let mut events = std::mem::take(&mut self.book.pending);
        events.extend(fills);
        for event in &events {
            let id = match event {
                BrokerEvent::Cancelled { id, .. } => *id,
                BrokerEvent::Filled(f) => f.order_id,
                BrokerEvent::Placed(_) => continue,
            };
            if let Some(t) = self.book.orders.get_mut(&id.0) {
                t.open = false;
                if matches!(event, BrokerEvent::Cancelled { .. }) {
                    t.at = At::Done;
                }
            }
        }
        self.book
            .orders
            .retain(|_, t| t.open || !matches!(t.at, At::Done | At::Refused));
        events
    }

    /// The bar's reads: executions, unknown outcomes, closed orders, then the reconcile.
    fn read_bar(&mut self, bar: &Candle, events: &mut Vec<BrokerEvent>) -> Result<(), RouteError> {
        self.intake(bar, events)?;
        self.resolve_unknown();
        for attempt in 0..2 {
            if self.fault.is_some() {
                return Ok(());
            }
            let open = self.client.open_orders(&self.instrument)?;
            let position = self.client.position(&self.instrument)?.size;
            let Some((kind, message)) = self.reconcile(&open, position, events)? else {
                return Ok(());
            };
            if attempt == 0 {
                // An execution may have landed between the reads: read once more.
                self.intake(bar, events)?;
            } else {
                self.raise(kind, message);
            }
        }
        Ok(())
    }

    /// Reads the executions not yet processed and reports them as one fill per order, followed by
    /// the stop each entry's fill places.
    fn intake(&mut self, bar: &Candle, events: &mut Vec<BrokerEvent>) -> Result<(), RouteError> {
        let since = self.book.cursor - LOOKBACK_MS;
        let executions = self.client.executions_since(&self.instrument, since)?;
        let mut groups: Vec<(OrderId, Vec<Execution>)> = Vec::new();
        for e in executions {
            if self.book.seen.contains_key(&e.exec_id) {
                continue;
            }
            let ours = ids::parse(&self.prefix, &e.client_id);
            let tracked = match ours {
                Some((Role::Order | Role::Close, id)) => {
                    self.book.orders.get(&id.0).map(|t| (id, t.at))
                }
                _ => None,
            };
            if e.time < self.book.floor && tracked.is_none() {
                continue;
            }
            self.book.seen.insert(e.exec_id.clone(), e.time);
            self.book.cursor = self.book.cursor.max(e.time);
            match e.kind {
                ExecKind::Funding => continue,
                ExecKind::Trade => {}
                kind => {
                    self.raise(
                        FaultKind::UnmodelledExecution,
                        format!(
                            "execution {} of {} is a {kind:?}, which the session cannot model",
                            e.exec_id, e.qty
                        ),
                    );
                    break;
                }
            }
            let reportable = match (ours, tracked) {
                (
                    Some((Role::Order, _)),
                    Some((id, At::Live | At::Unknown { .. } | At::Intent | At::Closing)),
                ) => Some(id),
                (Some((Role::Close, _)), Some((id, At::Closing))) => Some(id),
                _ => None,
            };
            let Some(id) = reportable else {
                let (kind, whose) = if ours.is_some() {
                    (
                        FaultKind::UnknownOrder,
                        "this session's prefix but no order it holds",
                    )
                } else {
                    (FaultKind::ForeignExecution, "no id of this session")
                };
                self.raise(
                    kind,
                    format!(
                        "trade {} of {} at {} carries client id '{}': {whose}",
                        e.exec_id, e.qty, e.price, e.client_id
                    ),
                );
                break;
            };
            match groups.iter_mut().find(|(g, _)| *g == id) {
                Some((_, list)) => list.push(e),
                None => groups.push((id, vec![e])),
            }
        }
        // On a fault the fills read before it are still reported: they happened.
        let horizon = self.book.cursor - LOOKBACK_MS;
        self.book.seen.retain(|_, time| *time >= horizon);
        for (id, list) in groups {
            let qty: f64 = list.iter().map(|e| e.qty).sum();
            let price = if list.iter().all(|e| e.price == list[0].price) {
                list[0].price
            } else {
                list.iter().map(|e| e.price * e.qty).sum::<f64>() / qty
            };
            let fee = list.iter().map(|e| e.fee).sum();
            let liquidity = if list.iter().any(|e| e.liquidity == Liquidity::Taker) {
                Liquidity::Taker
            } else {
                Liquidity::Maker
            };
            let t = self.tracked(id);
            if matches!(t.at, At::Unknown { .. } | At::Intent) {
                t.at = At::Live;
            }
            t.reported += qty;
            let side = t.order.request.side;
            let filled = t.reported;
            let instrument = t.order.request.instrument.clone();
            let has_stop = t.order.attached_stop;
            self.book.tally += side.sign() * qty;
            events.push(BrokerEvent::Filled(Fill {
                order_id: id,
                instrument,
                side,
                time: bar.open_time,
                price,
                qty,
                fee,
                liquidity,
            }));
            if let Some(stop) = has_stop {
                self.protect(id, filled, price);
                if let Some(s) = self.book.orders.get_mut(&stop.id.0)
                    && s.announced != s.sent
                {
                    s.announced = s.sent;
                    s.order.request.qty = s.sent;
                    s.open = true;
                    events.push(BrokerEvent::Placed(s.order.clone()));
                }
            }
        }
        Ok(())
    }

    /// Asks the venue about each order whose outcome is unknown: adopted when it holds the id,
    /// refused when two asks find nothing, and a fault after [`UNKNOWN_TRIES`] failed asks.
    fn resolve_unknown(&mut self) {
        let unknown: Vec<(OrderId, u32)> = self
            .book
            .orders
            .values()
            .filter_map(|t| match t.at {
                At::Unknown { tries } => Some((t.order.id, tries)),
                _ => None,
            })
            .collect();
        for (id, tries) in unknown {
            match self.find(id) {
                Ok(Some(found)) => self.adopt(id, &found),
                Ok(None) => self.refuse(id, NOT_HELD),
                Err(e) if tries + 1 >= UNKNOWN_TRIES => self.raise(
                    FaultKind::UnresolvedOrder,
                    format!(
                        "the outcome of {} is still unknown after {UNKNOWN_TRIES} asks: {e}",
                        self.id(Role::Order, id)
                    ),
                ),
                Err(_) => self.tracked(id).at = At::Unknown { tries: tries + 1 },
            }
        }
    }

    /// The venue order under order `id`'s `o` id; `None` only when two looks a pause apart both
    /// find nothing.
    fn find(&self, id: OrderId) -> Result<Option<VenueOrder>, RouteError> {
        let cid = self.id(Role::Order, id);
        for look in 0..2 {
            if look > 0 {
                std::thread::sleep(POLL_PAUSE);
            }
            if let Some(found) = self.client.order_by_client_id(&self.instrument, &cid)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// Settles the orders the venue no longer lists and compares the venue with what the session
    /// holds. Returns the first disagreement.
    fn reconcile(
        &mut self,
        open: &[VenueOrder],
        position: f64,
        events: &mut Vec<BrokerEvent>,
    ) -> Result<Option<(FaultKind, String)>, RouteError> {
        if let Some(problem) = self.unknown_open(open) {
            return Ok(Some(problem));
        }
        let half = self.half_lot();
        let standing: Vec<OrderId> = self
            .book
            .orders
            .values()
            .filter(|t| matches!(t.at, At::Live | At::Closing))
            .map(|t| t.order.id)
            .collect();
        for id in standing {
            let t = &self.book.orders[&id.0];
            let cid = self.venue_id(t);
            if open.iter().any(|o| o.client_id == cid.as_str()) {
                continue;
            }
            if t.reported >= t.sent - half {
                self.tracked(id).at = At::Done;
                continue;
            }
            let (reported, cancel_asked, stop, side) = (
                t.reported,
                t.cancel_asked,
                is_stop(&t.order),
                t.order.request.side,
            );
            let Some(found) = self.client.order_by_client_id(&self.instrument, &cid)? else {
                return Ok(Some((
                    FaultKind::MissingOrder,
                    format!("{cid} is neither open nor in the venue's history"),
                )));
            };
            if found.status.is_open() {
                continue; // the lists disagree for a moment
            }
            if found.filled_qty > reported + half {
                return Ok(Some((
                    FaultKind::PositionMismatch,
                    format!(
                        "{cid} filled {} at the venue but {reported} was reported",
                        found.filled_qty
                    ),
                )));
            }
            if reported > 0.0 {
                // Part filled, the rest closed: the session already holds it filled.
                self.tracked(id).at = At::Done;
                continue;
            }
            // Closed with nothing filled.
            let nothing_to_reduce = match side {
                Side::Sell => self.book.tally <= half,
                Side::Buy => self.book.tally >= -half,
            };
            let reason = if cancel_asked {
                "cancelled at the venue".to_string()
            } else if !stop {
                format!("the venue closed it unfilled ({})", found.status)
            } else if nothing_to_reduce {
                format!(
                    "the venue closed it ({}): the position it would reduce is gone",
                    found.status
                )
            } else {
                return Ok(Some((
                    FaultKind::MissingOrder,
                    format!(
                        "stop {cid} is {} at the venue with a position of {} still open",
                        found.status, self.book.tally
                    ),
                )));
            };
            self.tracked(id).at = At::Done;
            if self.book.orders[&id.0].open {
                events.push(BrokerEvent::Cancelled { id, reason });
            }
        }
        Ok(self.position_check(position, self.book.tally))
    }

    /// A venue open order the session does not hold open.
    fn unknown_open(&self, open: &[VenueOrder]) -> Option<(FaultKind, String)> {
        for o in open {
            let known =
                match ids::parse(&self.prefix, &o.client_id) {
                    Some((Role::Order, id)) => self.book.orders.get(&id.0).is_some_and(|t| {
                        matches!(t.at, At::Live | At::Unknown { .. } | At::Intent)
                    }),
                    Some((Role::Close, id)) => self
                        .book
                        .orders
                        .get(&id.0)
                        .is_some_and(|t| t.at == At::Closing),
                    _ => false,
                };
            if !known {
                return Some((
                    FaultKind::UnknownOpenOrder,
                    format!(
                        "the venue lists open order '{}' ({:?} {} at trigger {:?}, {}) that this session does not hold open",
                        o.client_id, o.side, o.qty, o.trigger_price, o.status
                    ),
                ));
            }
        }
        None
    }

    fn position_check(&self, position: f64, expected: f64) -> Option<(FaultKind, String)> {
        ((position - expected).abs() > self.half_lot()).then(|| {
            (
                FaultKind::PositionMismatch,
                format!(
                    "the venue holds a position of {position} but this session's fills add up to {expected}"
                ),
            )
        })
    }

    // ---- replay ----

    /// Applies the state of every record whose calls are all replayed, checking that the open
    /// orders the replay arrived at are the recorded ones.
    fn flush_records(&mut self) {
        while let Some(queue) = &mut self.replay {
            if !queue.front().is_some_and(|r| r.calls.is_empty()) {
                return;
            }
            let record = queue.pop_front().expect("checked above");
            let derived: Vec<OrderId> = self.open_list().iter().map(|o| o.id).collect();
            let recorded: Vec<OrderId> = record
                .book
                .orders
                .values()
                .filter(|t| t.open)
                .map(|t| t.order.id)
                .collect();
            if derived != recorded {
                return self.diverged(format!(
                    "the replay holds orders {derived:?} open where the record holds {recorded:?}"
                ));
            }
            self.book = record.book;
        }
    }

    /// The next recorded call, `None` past the last record.
    fn next_call(&mut self) -> Option<Call> {
        self.flush_records();
        let queue = self.replay.as_mut()?;
        let record = queue.front_mut()?;
        Some(record.calls.remove(0))
    }

    /// Stops the replay on a fault; the broker then refuses everything as any faulted broker.
    fn diverged(&mut self, message: String) {
        self.raise(FaultKind::ReplayDiverged, message);
        self.replay = None;
    }

    fn replay_submit(&mut self, order: Order) {
        let id = order.id;
        match self.next_call() {
            Some(Call::Submit { id: recorded }) if recorded == id => {
                self.track(order, At::Live);
                self.flush_records();
            }
            None if self.replay.is_some() => {
                // Logged so the next record holds it for a later replay.
                self.log.push(Call::Submit { id });
                self.track(order, At::Intent);
            }
            None => self.submit(order),
            Some(other) => {
                self.diverged(format!(
                    "the record has {other:?} where the session submitted order {}",
                    id.0
                ));
                self.submit(order);
            }
        }
    }

    fn replay_cancel(&mut self, id: OrderId) -> bool {
        match self.next_call() {
            Some(Call::Cancel {
                id: recorded,
                cancelled,
            }) if recorded == id => {
                if cancelled && let Some(t) = self.book.orders.get_mut(&id.0) {
                    t.open = false;
                }
                self.flush_records();
                cancelled
            }
            None if self.replay.is_some() => {
                let Some(t) = self.book.orders.get_mut(&id.0).filter(|t| t.open) else {
                    return false;
                };
                t.open = false;
                if t.at == At::Intent {
                    t.at = At::Done;
                } else {
                    t.cancel_intent = true;
                }
                self.log.push(Call::Cancel {
                    id,
                    cancelled: true,
                });
                true
            }
            None => self.cancel(id),
            Some(other) => {
                self.diverged(format!(
                    "the record has {other:?} where the session cancelled order {}",
                    id.0
                ));
                false
            }
        }
    }

    fn replay_bar(&mut self, bar: &Candle) -> Vec<BrokerEvent> {
        match self.next_call() {
            Some(Call::Bar { open_time, events }) if open_time == bar.open_time => {
                for event in &events {
                    match event {
                        BrokerEvent::Filled(f) => {
                            if let Some(t) = self.book.orders.get_mut(&f.order_id.0) {
                                t.open = false;
                            }
                        }
                        BrokerEvent::Cancelled { id, .. } => {
                            if let Some(t) = self.book.orders.get_mut(&id.0) {
                                t.open = false;
                            }
                        }
                        BrokerEvent::Placed(order) => {
                            self.track(order.clone(), At::Live);
                        }
                    }
                }
                self.flush_records();
                events
            }
            next => {
                let what = match next {
                    Some(call) => format!("the record has {call:?}"),
                    None => "the record has ended".to_string(),
                };
                self.diverged(format!(
                    "{what} where the session fed the bar opening at {}",
                    bar.open_time
                ));
                Vec::new()
            }
        }
    }

    fn go_live(&mut self) {
        if self.replay.is_none() {
            return;
        }
        self.flush_records();
        let left: usize = self
            .replay
            .take()
            .map_or(0, |q| q.iter().map(|r| r.calls.len()).sum());
        if left > 0 {
            return self.diverged(format!("{left} recorded calls were not replayed"));
        }
        if self.fault.is_some() {
            return;
        }
        let intents: Vec<OrderId> = self
            .book
            .orders
            .values()
            .filter(|t| matches!(t.at, At::Intent | At::Unknown { .. }))
            .map(|t| t.order.id)
            .collect();
        for id in intents {
            match self.find(id) {
                Ok(Some(found)) => self.adopt(id, &found),
                Ok(None) => {
                    let order = self.book.orders[&id.0].order.clone();
                    if let Some(why) = unsupported(&order.request, order.attached_stop.is_some()) {
                        self.refuse(id, why);
                    } else if self.is_stale(&order) {
                        self.refuse(id, STALE);
                    } else {
                        // shortcut: this resend does not wait out the receive window of a request
                        // the crashed run may have sent under the same id; the venue still refuses
                        // a duplicate id it holds. Wait the window first if a crash between send
                        // and record turns out to happen.
                        self.send(id);
                    }
                }
                Err(_) => self.tracked(id).at = At::Unknown { tries: 1 },
            }
        }
        let cancels: Vec<OrderId> = self
            .book
            .orders
            .values()
            .filter(|t| t.cancel_intent)
            .map(|t| t.order.id)
            .collect();
        for id in cancels {
            self.tracked(id).cancel_intent = false;
            if self.book.orders[&id.0].at != At::Live {
                continue;
            }
            let cid = self.id(Role::Order, id);
            match self.client.cancel(&self.instrument, &cid) {
                Ok(CancelOutcome::Cancelled) => self.tracked(id).at = At::Done,
                // Its fill arrives through the executions on the next bar.
                Ok(CancelOutcome::AlreadyClosed(_)) => {}
                Err(_) => self.tracked(id).cancel_asked = true,
            }
        }
        // A venue that cannot be read now is reconciled on the next bar, as any bar's reads.
        if self.verify().is_err() {
            self.book.read_failures += 1;
        }
    }

    /// The reconcile at the end of a replay: the venue's open orders must be the session's, and
    /// its position the recorded fills plus this session's executions not yet reported.
    fn verify(&mut self) -> Result<(), RouteError> {
        let executions = self
            .client
            .executions_since(&self.instrument, self.book.cursor - LOOKBACK_MS)?;
        let mut expected = self.book.tally;
        for e in executions {
            if self.book.seen.contains_key(&e.exec_id) || e.kind != ExecKind::Trade {
                continue;
            }
            if let Some((Role::Order | Role::Close, id)) = ids::parse(&self.prefix, &e.client_id)
                && let Some(t) = self.book.orders.get(&id.0)
            {
                expected += t.order.request.side.sign() * e.qty;
            }
        }
        let open = self.client.open_orders(&self.instrument)?;
        let position = self.client.position(&self.instrument)?.size;
        if let Some((kind, message)) = self
            .unknown_open(&open)
            .or_else(|| self.position_check(position, expected))
        {
            self.raise(kind, message);
        }
        Ok(())
    }
}
