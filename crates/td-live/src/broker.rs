//! [`VenueBroker`], its shared [`VenueHandle`], faults and the replay record.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use td_core::{
    Candle, Fill, Instrument, InstrumentId, Liquidity, Millis, OrderId, OrderRequest, OrderType,
    Side, TimeInForce, Timeframe,
};
use td_engine::{Broker, BrokerEvent, Order, OrderStatus, Portfolio};
use td_orders::{
    CancelOutcome, Client, ClientOrderId, Equity, Error as RouteError, ExecKind, Execution,
    Position, VenueOrder,
};

use crate::ids::{self, InvalidPrefix, Role};

/// How far before the newest execution seen each read starts, for an execution the venue lists
/// late with an earlier time. Executions are deduplicated by id.
const LOOKBACK_MS: Millis = 60_000;
/// How many times an entry's fill is looked for before its stop is placed for the quantity sent.
const FILL_POLLS: u32 = 5;
/// The pause between two looks at the venue for the same order.
const POLL_PAUSE: Duration = Duration::from_millis(100);
/// Failed asks about an order whose outcome is unknown before the broker faults.
const UNKNOWN_TRIES: u32 = 3;
/// Bars in a row on which the venue could not be read before the broker faults.
const UNREADABLE_BARS: u32 = 3;
/// The longest piece of venue text a fault message, a cancel reason or a record keeps, as the
/// order route caps the venue's own messages.
const TEXT_MAX_LEN: usize = 200;
/// The longest execution id the broker keeps; a longer one is a fault.
const EXEC_ID_MAX_LEN: usize = 64;

const FAULTED: &str = "not sent: the broker stopped on a fault";
const STALE: &str = "not sent: it was decided on a bar that closed more than one bar length ago";
const NOT_HELD: &str = "not filled: the venue holds no order under its id";
const UNREADABLE: &str =
    "not sent: the venue could not be read on the last bar, so only reduce-only orders go out";
const ENTRY_NOT_SENT: &str = "its entry was not sent";
const ENTRY_CLOSED: &str = "its entry closed unfilled at the venue";

/// What kind of disagreement stopped the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// A replayed session made calls that differ from the record, went past it, or was handed a
    /// record made for another session or holding values no live run makes.
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
/// text, which holds no key, secret, signature or raw response body. Every piece of venue text in
/// it is stripped of control characters and capped in length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct BrokerFault {
    pub kind: FaultKind,
    pub message: String,
}

/// Everything the broker did since the last [`VenueHandle::take_record`]: the calls the session
/// made and their answers, then the broker's state and its fault. Serialisable, so the owner can
/// store it after each live bar and hand it back to a restarted session
/// ([`VenueHandle::replay`]). It names the session prefix, instrument and timeframe it was made
/// under, and only a broker with the same three replays it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrokerRecord {
    prefix: String,
    instrument: InstrumentId,
    timeframe: Timeframe,
    calls: Vec<Call>,
    book: Book,
    /// The fault that stopped the broker, if any; a replay restores it.
    fault: Option<BrokerFault>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Call {
    /// The order as the session submitted it; a replayed session must submit the same.
    Submit {
        order: Order,
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

impl Call {
    /// The call in a few words, for a fault message.
    fn describe(&self) -> String {
        match self {
            Call::Submit { order } => format!("a submit of order {}", order.id.0),
            Call::Cancel { id, .. } => format!("a cancel of order {}", id.0),
            Call::Bar { open_time, .. } => format!("the bar opening at {open_time}"),
        }
    }
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
    /// A cancel was sent: its outcome was lost, or it took and the next read settles any fill
    /// made before it.
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
    /// The fault was raised during the current call, so a stop for a fill the venue confirmed in
    /// it may still be placed (see the crate documentation).
    fault_fresh: bool,
    /// The broker has made a live call or gone live: a record handed to it now is a fault.
    acted_live: bool,
    /// Calls since the last record was taken.
    log: Vec<Call>,
    /// Records still to replay; `Some` while replaying.
    replay: Option<VecDeque<BrokerRecord>>,
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
        // shortcut: the session's leverage cap reads the session's own modelled equity (its
        // starting cash plus the fills reported), not the venue's. The caller starts the session
        // with the venue's equity as its starting cash; read the venue's equity on each bar if
        // the two drift apart in practice.
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
            fault_fresh: false,
            acted_live: false,
            log: Vec::new(),
            replay: None,
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

    fn with_core<R>(&mut self, f: impl FnOnce(&mut Core) -> R) -> R {
        let mut core = lock(&self.core);
        core.fault_fresh = false;
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

    /// The record of every call since the last one was taken, with the broker's state and fault
    /// now. Take it after each live `Session::on_bar` returns, and after a manual order or cancel.
    pub fn take_record(&self) -> BrokerRecord {
        let mut core = lock(&self.core);
        BrokerRecord {
            prefix: core.prefix.clone(),
            instrument: core.instrument.id.clone(),
            timeframe: core.timeframe,
            calls: std::mem::take(&mut core.log),
            book: core.book.clone(),
            fault: core.fault.clone(),
        }
    }

    /// Queues `record` for a session replaying a live run, and puts a broker that has not acted
    /// yet into replay mode: each call then takes its answer from the records in order and sends
    /// nothing. A broker that has already acted live or gone live faults instead, and so does a
    /// record made under another session prefix, instrument or timeframe, or one holding values
    /// no live run makes.
    pub fn replay(&self, record: BrokerRecord) {
        let mut core = lock(&self.core);
        if core.acted_live {
            return core.raise(
                FaultKind::ReplayDiverged,
                "a record was handed to a broker that has already acted live".into(),
            );
        }
        if core.fault.is_some() && core.replay.is_none() {
            return; // the replay already diverged
        }
        if let Err(why) = core.check_record(&record) {
            return core.diverged(why);
        }
        core.replay
            .get_or_insert_with(VecDeque::new)
            .push_back(record);
    }

    /// Ends a replay: resolves the orders whose outcome the record does not hold, those whose
    /// request outcome was lost and those the session submitted past the record (adopted with
    /// any protective stop the venue already holds for them, otherwise sent or refused as a
    /// fresh submission would be), sends the cancels it made past the record, and reconciles the
    /// venue with the recorded state. Every record must have been replayed. A fault the records
    /// carry is restored, and then nothing is sent and the fault is returned. Does nothing more on
    /// a broker that is not replaying.
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

    // The owner's "cancel this session's resting orders and close its position" action belongs
    // here, sending under the reserved `x` ids (see `crate::ids`). It is not built yet.

    fn route(&self) -> (Arc<Client>, Instrument) {
        let core = lock(&self.core);
        (core.client.clone(), core.instrument.clone())
    }
}

/// Venue text as a fault message, a cancel reason or a record may hold it: without control
/// characters and at most [`TEXT_MAX_LEN`] characters.
fn venue_text(text: impl fmt::Display) -> String {
    text.to_string()
        .chars()
        .filter(|c| !c.is_control())
        .take(TEXT_MAX_LEN)
        .collect()
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

/// The order the session holds for `entry`'s attached stop covering `qty`, if it carries one.
fn stop_order(entry: &Order, qty: f64) -> Option<Order> {
    let stop = entry.attached_stop?;
    Some(Order {
        id: stop.id,
        request: stop.request(&entry.request, qty),
        submitted_at: entry.submitted_at,
        decision_close: entry.decision_close,
        attached_stop: None,
        status: OrderStatus::Open,
    })
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
    /// It was not taken, or not sent because the broker faulted.
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

    fn raise(&mut self, kind: FaultKind, message: String) {
        if self.fault.is_none() {
            self.fault = Some(BrokerFault { kind, message });
            self.fault_fresh = true;
        }
    }

    // ---- the only three places that send to the venue ----

    /// Places an order, unless the broker has faulted. The one exception is a reduce-only stop
    /// protecting a fill the venue confirmed (`protects_fill`), in the call that raised the fault.
    /// `None` when it is not sent.
    fn venue_place(
        &mut self,
        request: &OrderRequest,
        cid: &ClientOrderId,
        last: f64,
        protects_fill: bool,
    ) -> Option<Result<(), RouteError>> {
        let protective = protects_fill
            && request.reduce_only
            && matches!(request.order_type, OrderType::Stop { .. });
        (self.fault.is_none() || (protective && self.fault_fresh)).then(|| {
            self.client
                .place(&self.instrument, request, cid, last)
                .map(|_| ())
        })
    }

    /// Cancels an order, unless the broker has faulted; `None` when it is not sent.
    fn venue_cancel(&self, cid: &ClientOrderId) -> Option<Result<CancelOutcome, RouteError>> {
        self.fault
            .is_none()
            .then(|| self.client.cancel(&self.instrument, cid))
    }

    /// Resizes a resting stop, unless the broker has faulted; `None` when it is not sent.
    fn venue_amend(&self, cid: &ClientOrderId, qty: f64) -> Option<Result<(), RouteError>> {
        self.fault.is_none().then(|| {
            self.client
                .amend_stop(&self.instrument, cid, None, Some(qty))
        })
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
        self.client.server_now() > order.submitted_at.saturating_add(2 * bar)
    }

    // ---- submit ----

    fn submit(&mut self, order: Order) {
        if self.replay.is_some() {
            return self.replay_submit(order);
        }
        self.acted_live = true;
        self.log.push(Call::Submit {
            order: order.clone(),
        });
        let id = order.id;
        self.track(order, At::Live);
        let order = &self.book.orders[&id.0].order;
        if self.fault.is_some() {
            return self.refuse(id, FAULTED);
        }
        if let Some(why) = unsupported(&order.request, order.attached_stop.is_some()) {
            return self.refuse(id, why);
        }
        if self.is_stale(order) {
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

    /// Sends a tracked order and protects an entry's fill. While the venue could not be read on
    /// the last bar, only a reduce-only order goes out.
    fn send(&mut self, id: OrderId) {
        let order = &self.book.orders[&id.0].order;
        let (request, last) = (order.request.clone(), order.decision_close);
        let (stop, has_stop) = (is_stop(order), order.attached_stop.is_some());
        if self.book.read_failures > 0 && !request.reduce_only {
            return self.refuse(id, UNREADABLE);
        }
        let cid = self.id(Role::Order, id);
        let Some(placed) = self.venue_place(&request, &cid, last, false) else {
            return self.refuse(id, FAULTED);
        };
        match placed {
            Ok(()) => {
                let t = self.tracked(id);
                t.at = At::Live;
                t.sent = request.qty;
                if has_stop {
                    self.confirm_and_protect(id);
                }
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
                // An entry that may have filled does not wait a bar for its stop: the venue is
                // asked at once, and the entry is adopted and protected if it holds the id.
                if has_stop && let Ok(Some(found)) = self.find(id) {
                    self.adopt(id, &found);
                }
            }
            Err(e) if through_market(&e) && stop => {
                // The market is already through a standalone stop: the venue would trigger it at
                // once, so it is closed at market as the simulated broker fills it at the open.
                match self.close_at_market(id, request.qty, last) {
                    Ok(()) => {}
                    Err(CloseFailed::Refused(why)) => self.refuse(
                        id,
                        &format!(
                            "not sent: {}; closing at market failed: {why}",
                            venue_text(&e)
                        ),
                    ),
                    Err(CloseFailed::Unknown(why)) => self.raise(
                        FaultKind::ProtectionFailed,
                        format!("stop {cid} was replaced by a close at market whose outcome is unknown: {why}"),
                    ),
                }
            }
            Err(e) => self.refuse(id, &format!("not sent: {}", venue_text(&e))),
        }
    }

    /// Takes an order the venue already holds, unless it was created before the order was
    /// decided, which means another session used the prefix. An entry's attached stop is then
    /// taken from the venue if it holds it, and otherwise placed as for an entry just sent.
    fn adopt(&mut self, id: OrderId, found: &VenueOrder) {
        let submitted_at = self.book.orders[&id.0].order.submitted_at;
        if found.created < submitted_at {
            self.raise(
                FaultKind::IdCollision,
                format!(
                    "the venue holds {} from {}, before order {} was decided at {submitted_at}",
                    venue_text(&found.client_id),
                    found.created,
                    id.0
                ),
            );
            return self.refuse(id, NOT_HELD);
        }
        let t = self.tracked(id);
        t.at = At::Live;
        t.sent = found.qty;
        let stop = t.order.attached_stop;
        if let Some(stop) = stop.filter(|s| !self.book.orders.contains_key(&s.id.0))
            && self.adopt_stop(id, stop.id)
        {
            self.confirm_and_protect(id);
        }
    }

    /// Takes the stop the venue already holds under the id of adopted entry `entry`'s attached
    /// stop, placed before a restart. Returns whether the entry still needs protecting: true when
    /// the venue holds no such stop (or cannot say; a placement under the same id cannot double
    /// it) or holds it open, so it is resized to the fill as any stop. A stop the venue already
    /// closed is settled by the reads, as any stop found closed. One that is not the stop the
    /// session intends is a fault.
    fn adopt_stop(&mut self, entry: OrderId, stop: OrderId) -> bool {
        let cid = self.id(Role::Order, stop);
        let Ok(Some(found)) = self.client.order_by_client_id(&self.instrument, &cid) else {
            return true;
        };
        let e = &self.book.orders[&entry.0].order;
        let order = stop_order(e, found.qty).expect("the entry carries the stop");
        let trigger = match order.request.order_type {
            OrderType::Stop { trigger } => trigger,
            _ => unreachable!("an attached stop is a stop order"),
        };
        let intended = found.side == order.request.side
            && found.reduce_only
            && found.qty.is_finite()
            && found.qty > 0.0
            && found
                .trigger_price
                .is_some_and(|t| (t - trigger).abs() <= self.instrument.tick_size / 2.0);
        if !intended {
            self.raise(
                FaultKind::UnknownOpenOrder,
                format!(
                    "the venue holds {} ({:?} {}, reduce-only {}, trigger {:?}, {}) where entry {} \
                     carries a reduce-only {:?} stop at {trigger}",
                    venue_text(&found.client_id),
                    found.side,
                    found.qty,
                    found.reduce_only,
                    found.trigger_price,
                    venue_text(&found.status),
                    entry.0,
                    order.request.side
                ),
            );
            return false;
        }
        self.track(order, At::Live);
        let t = self.tracked(stop);
        // The session holds it as attached until the bar that announces it.
        t.open = false;
        t.sent = found.qty;
        found.status.is_open()
    }

    /// Waits briefly for an entry's fill and places its stop for the quantity filled. When the
    /// fill cannot be confirmed in time, the stop is placed at once for the quantity sent and
    /// resized when the fill is first seen on a bar. An entry the venue closed unfilled gets none;
    /// the next bar's reads report it.
    fn confirm_and_protect(&mut self, entry: OrderId) {
        let cid = self.id(Role::Order, entry);
        let mut filled = None;
        let mut closed = false;
        for poll in 0..FILL_POLLS {
            if poll > 0 {
                std::thread::sleep(POLL_PAUSE);
            }
            if let Ok(Some(found)) = self.client.order_by_client_id(&self.instrument, &cid) {
                if found.filled_qty > 0.0 {
                    filled = Some((found.filled_qty, found.avg_price));
                }
                if !found.status.is_open() {
                    closed = true;
                    break;
                }
            }
        }
        let e = &self.book.orders[&entry.0];
        let (sent, decision_close) = (e.sent, e.order.decision_close);
        match filled {
            Some((qty, price)) => self.protect(entry, qty, price.unwrap_or(decision_close), true),
            None if closed => {}
            None => self.protect(entry, sent, decision_close, false),
        }
    }

    /// Makes the entry's attached stop cover `qty`: places it the first time, resizes it when the
    /// fill turns out different. A stop the venue will not take is replaced by a close at market;
    /// when that fails too, or a resize fails, the broker faults. `confirmed` says `qty` is a fill
    /// the venue confirmed, whose stop may still go out in the call that raised a fault.
    fn protect(&mut self, entry: OrderId, qty: f64, last: f64, confirmed: bool) {
        let Some(order) = stop_order(&self.book.orders[&entry.0].order, qty) else {
            return;
        };
        let id = order.id;
        let cid = self.id(Role::Order, id);
        let half = self.half_lot();
        match self.book.orders.get(&id.0).map(|t| (t.at, t.sent)) {
            None => {
                let request = order.request.clone();
                self.track(order, At::Live);
                let t = self.tracked(id);
                // The session holds it as attached until the bar that announces it.
                t.open = false;
                t.sent = qty;
                let why = match self.venue_place(&request, &cid, last, confirmed) {
                    Some(Ok(()) | Err(RouteError::DuplicateClientId { .. })) => return,
                    Some(Err(e)) => venue_text(&e),
                    None => FAULTED.to_string(),
                };
                if let Err(CloseFailed::Refused(e) | CloseFailed::Unknown(e)) =
                    self.close_at_market(id, qty, last)
                {
                    self.raise(
                        FaultKind::ProtectionFailed,
                        format!(
                            "stop {cid} for {qty} was not placed ({why}) and closing at market failed ({e})"
                        ),
                    );
                }
            }
            Some((At::Live, sent)) if (qty - sent).abs() > half => {
                self.resize(id, sent, qty, last, confirmed)
            }
            Some((At::Closing, sent)) if qty > sent + half => self.raise(
                FaultKind::ProtectionFailed,
                format!("entry {} filled {qty} after its stop was replaced by a close at market for {sent}", entry.0),
            ),
            Some(_) => {}
        }
    }

    /// Resizes resting stop `id` from `sent` to `qty`. When that fails and the stop covers less
    /// than the fill, one more reduce-only stop at the same trigger is placed for the rest under
    /// the stop's `s` id; the broker faults either way.
    fn resize(&mut self, id: OrderId, sent: f64, qty: f64, last: f64, confirmed: bool) {
        let cid = self.id(Role::Order, id);
        let why = match self.venue_amend(&cid, qty) {
            Some(Ok(())) => {
                let t = self.tracked(id);
                t.sent = qty;
                t.order.request.qty = qty;
                return;
            }
            Some(Err(e)) => venue_text(&e),
            None => FAULTED.to_string(),
        };
        if qty < sent {
            return self.raise(
                FaultKind::ProtectionFailed,
                format!("stop {cid} covers {sent} but its entry filled only {qty}; resizing it down failed ({why})"),
            );
        }
        let rest = self.instrument.round_qty(qty - sent);
        let cover = self.id(Role::Cover, id);
        let request = OrderRequest {
            qty: rest,
            ..self.book.orders[&id.0].order.request.clone()
        };
        let covered = match self.venue_place(&request, &cover, last, confirmed) {
            Some(Ok(()) | Err(RouteError::DuplicateClientId { .. })) => {
                format!("{cover} now covers the other {rest}")
            }
            Some(Err(e)) => format!(
                "placing {cover} for the other {rest} failed too ({}), so {rest} is not covered",
                venue_text(&e)
            ),
            None => format!("{rest} is not covered"),
        };
        self.raise(
            FaultKind::ProtectionFailed,
            format!("stop {cid} covers {sent} but its entry filled {qty}; resizing it failed ({why}); {covered}"),
        );
    }

    /// Sends a reduce-only market order for `qty` under order `id`'s `c` id, against the side its
    /// stop would close. Never sent once the broker has faulted.
    fn close_at_market(&mut self, id: OrderId, qty: f64, last: f64) -> Result<(), CloseFailed> {
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
        match self.venue_place(&request, &cid, last, false) {
            Some(Ok(()) | Err(RouteError::DuplicateClientId { .. })) => {
                let t = self.tracked(id);
                t.at = At::Closing;
                t.sent = qty;
                Ok(())
            }
            Some(Err(e @ RouteError::OutcomeUnknown { .. })) => {
                Err(CloseFailed::Unknown(venue_text(&e)))
            }
            Some(Err(e)) => Err(CloseFailed::Refused(venue_text(&e))),
            None => Err(CloseFailed::Refused(FAULTED.into())),
        }
    }

    // ---- cancel ----

    fn cancel(&mut self, id: OrderId) -> bool {
        if self.replay.is_some() {
            return self.replay_cancel(id);
        }
        self.acted_live = true;
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
                self.drop_refused(id);
                true
            }
            At::Live => {
                let cid = self.id(Role::Order, id);
                match self.venue_cancel(&cid) {
                    Some(Ok(CancelOutcome::Cancelled)) => {
                        // The next read settles it, and reports any fill made before the cancel
                        // took.
                        let t = self.tracked(id);
                        t.open = false;
                        t.cancel_asked = true;
                        true
                    }
                    Some(Err(RouteError::OutcomeUnknown { .. })) => {
                        self.tracked(id).cancel_asked = true;
                        false
                    }
                    // Already closed: its fill arrives through the executions. Not sent on a
                    // fault.
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// Cancels an order that was not sent: its pending cancellation, and its stop's, give way to
    /// this one.
    fn drop_refused(&mut self, id: OrderId) {
        let stop = self.book.orders[&id.0].order.attached_stop.map(|s| s.id);
        self.book.pending.retain(|e| match e {
            BrokerEvent::Cancelled { id: c, .. } => *c != id && Some(*c) != stop,
            _ => true,
        });
        let t = self.tracked(id);
        t.at = At::Done;
        t.open = false;
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
        self.acted_live = true;
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
                    self.book.read_failures = self.book.read_failures.saturating_add(1);
                    if self.book.read_failures >= UNREADABLE_BARS {
                        self.raise(
                            FaultKind::VenueUnreadable,
                            format!(
                                "{UNREADABLE_BARS} bars in a row without a full read: {}",
                                venue_text(&e)
                            ),
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
        let since = self.book.cursor.saturating_sub(LOOKBACK_MS);
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
            if e.exec_id.len() > EXEC_ID_MAX_LEN {
                self.raise(
                    FaultKind::UnmodelledExecution,
                    format!(
                        "execution '{}' carries an id longer than {EXEC_ID_MAX_LEN} characters",
                        venue_text(&e.exec_id)
                    ),
                );
                break;
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
                            "execution {} of {} is a {}, which the session cannot model",
                            venue_text(&e.exec_id),
                            e.qty,
                            venue_text(format!("{kind:?}"))
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
                        venue_text(&e.exec_id),
                        e.qty,
                        e.price,
                        venue_text(&e.client_id)
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
        let horizon = self.book.cursor.saturating_sub(LOOKBACK_MS);
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
                self.protect(id, filled, price, true);
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
            let tries = tries.saturating_add(1);
            match self.find(id) {
                Ok(Some(found)) => self.adopt(id, &found),
                Ok(None) => self.refuse(id, NOT_HELD),
                Err(e) if tries >= UNKNOWN_TRIES => self.raise(
                    FaultKind::UnresolvedOrder,
                    format!(
                        "the outcome of {} is still unknown after {UNKNOWN_TRIES} asks: {}",
                        self.id(Role::Order, id),
                        venue_text(&e)
                    ),
                ),
                Err(_) => self.tracked(id).at = At::Unknown { tries },
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
            let (reported, cancel_asked, stop, side, attached, held) = (
                t.reported,
                t.cancel_asked,
                is_stop(&t.order),
                t.order.request.side,
                t.order.attached_stop,
                t.open,
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
            let status = venue_text(&found.status);
            let reason = if cancel_asked {
                "cancelled at the venue".to_string()
            } else if !stop {
                format!("the venue closed it unfilled ({status})")
            } else if nothing_to_reduce {
                format!("the venue closed it ({status}): the position it would reduce is gone")
            } else {
                return Ok(Some((
                    FaultKind::MissingOrder,
                    format!(
                        "stop {cid} is {status} at the venue with a position of {} still open",
                        self.book.tally
                    ),
                )));
            };
            // Its attached stop goes with it. One already placed because the entry's fill could
            // not be confirmed must not rest without a position.
            if let Some(s) = attached {
                if self.book.orders.contains_key(&s.id.0) {
                    return Ok(Some((
                        FaultKind::UnknownOpenOrder,
                        format!(
                            "entry {cid} closed unfilled ({status}) after its stop {} was placed",
                            self.id(Role::Order, s.id)
                        ),
                    )));
                }
                if held {
                    events.push(BrokerEvent::Cancelled {
                        id: s.id,
                        reason: ENTRY_CLOSED.into(),
                    });
                }
            }
            self.tracked(id).at = At::Done;
            if held {
                events.push(BrokerEvent::Cancelled { id, reason });
            }
        }
        Ok(self.position_check(position, self.book.tally))
    }

    /// A venue open order the session does not hold open. An order of this session still counts
    /// as held while it works on after a fill the session was given, and an entry's attached stop
    /// while it waits for the bar that announces it.
    fn unknown_open(&self, open: &[VenueOrder]) -> Option<(FaultKind, String)> {
        for o in open {
            let known = match ids::parse(&self.prefix, &o.client_id) {
                Some((Role::Order, id)) => self.book.orders.get(&id.0).is_some_and(|t| {
                    matches!(t.at, At::Live | At::Unknown { .. } | At::Intent)
                        && (t.open || t.reported > 0.0 || self.awaits_announcement(id))
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
                        venue_text(&o.client_id),
                        o.side,
                        o.qty,
                        o.trigger_price,
                        venue_text(&o.status)
                    ),
                ));
            }
        }
        None
    }

    /// Whether order `id` is an entry's attached stop placed at the venue and not yet announced.
    fn awaits_announcement(&self, id: OrderId) -> bool {
        self.book
            .orders
            .get(&id.0)
            .is_some_and(|t| !t.open && t.announced == 0.0)
            && self
                .book
                .orders
                .values()
                .any(|e| e.order.attached_stop.is_some_and(|s| s.id == id))
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

    /// Why `record` cannot be replayed on this broker, if it cannot: it was made for another
    /// session, or it holds a value no live run makes.
    fn check_record(&self, record: &BrokerRecord) -> Result<(), String> {
        if record.prefix != self.prefix {
            return Err("the record was made under another session prefix".into());
        }
        if record.instrument != self.instrument.id {
            return Err(format!(
                "the record was made for another instrument than {}",
                self.instrument.id
            ));
        }
        if record.timeframe != self.timeframe {
            return Err(format!(
                "the record was made for another timeframe than {}",
                self.timeframe
            ));
        }
        let book = &record.book;
        if !book.tally.is_finite() {
            return Err("the record's position tally is not a finite number".into());
        }
        if book.cursor < book.floor {
            return Err("the record's execution cursor is before its floor".into());
        }
        let quantity = |q: f64| q.is_finite() && q >= 0.0;
        if let Some(key) = book.orders.iter().find_map(|(key, t)| {
            (![t.sent, t.reported, t.announced].into_iter().all(quantity)).then_some(key)
        }) {
            return Err(format!(
                "the record holds a quantity for order {key} that is negative or not finite"
            ));
        }
        if book.seen.keys().any(|id| id.len() > EXEC_ID_MAX_LEN) {
            return Err(format!(
                "the record holds an execution id longer than {EXEC_ID_MAX_LEN} characters"
            ));
        }
        let mut events = record
            .calls
            .iter()
            .filter_map(|call| match call {
                Call::Bar { events, .. } => Some(events),
                _ => None,
            })
            .flatten()
            .chain(&book.pending);
        let unsound = events.any(|event| match event {
            BrokerEvent::Filled(f) => {
                !(quantity(f.qty) && f.qty > 0.0 && f.price.is_finite() && f.price > 0.0)
                    || !f.fee.is_finite()
            }
            BrokerEvent::Placed(o) => !(quantity(o.request.qty) && o.request.qty > 0.0),
            BrokerEvent::Cancelled { .. } => false,
        });
        if unsound {
            return Err(
                "the record holds a fill or stop whose quantity, price or fee no live run makes"
                    .into(),
            );
        }
        Ok(())
    }

    /// Takes the state of every record whose calls are all replayed.
    fn flush_records(&mut self) {
        while let Some(queue) = &mut self.replay {
            if !queue.front().is_some_and(|r| r.calls.is_empty()) {
                return;
            }
            let record = queue.pop_front().expect("checked above");
            if let Err(why) = self.take_book(record) {
                return self.diverged(why);
            }
        }
    }

    /// Takes from a fully replayed record where each order stands at the venue, keeping the
    /// replayed session's own orders. The replay must hold open exactly the orders the record
    /// holds open, and every order the record tracks must be one the replay holds (or the
    /// attached stop of one), unchanged. A fault the record carries is restored.
    fn take_book(&mut self, record: BrokerRecord) -> Result<(), String> {
        let derived = self.open_list();
        let recorded: Vec<Order> = record
            .book
            .orders
            .values()
            .filter(|t| t.open)
            .map(|t| t.order.clone())
            .collect();
        if derived != recorded {
            let ids = |orders: &[Order]| orders.iter().map(|o| o.id.0).collect::<Vec<_>>();
            return Err(format!(
                "the replay holds orders {:?} open where the record holds {:?}, or holds them differently",
                ids(&derived),
                ids(&recorded)
            ));
        }
        let mut orders = BTreeMap::new();
        for (key, rec) in record.book.orders {
            let own =
                match self.book.orders.get(&key) {
                    Some(t) => Some(t.order.clone()),
                    None => self.book.orders.values().find_map(|e| {
                        stop_order(&e.order, rec.sent).filter(|stop| stop.id.0 == key)
                    }),
                };
            match own {
                Some(order) if order == rec.order => {
                    orders.insert(key, Tracked { order, ..rec });
                }
                _ => {
                    return Err(format!(
                        "the record tracks order {key}, which the replay does not hold as recorded"
                    ));
                }
            }
        }
        self.book = Book {
            orders,
            ..record.book
        };
        if let Some(fault) = record.fault {
            self.fault.get_or_insert(fault);
            self.fault_fresh = false;
        }
        Ok(())
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
        match self.next_call() {
            Some(Call::Submit { order: recorded }) if recorded == order => {
                self.track(order, At::Live);
                self.flush_records();
            }
            None if self.replay.is_some() => {
                // Logged so the next record holds it for a later replay.
                self.log.push(Call::Submit {
                    order: order.clone(),
                });
                let id = order.id;
                self.track(order, At::Intent);
                if self.fault.is_some() {
                    self.refuse(id, FAULTED);
                }
            }
            None => self.submit(order),
            Some(other) => {
                self.diverged(format!(
                    "the record has {} where the session submitted order {}, or submitted it differently",
                    other.describe(),
                    order.id.0
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
                let cancelled = self.cancel_past_record(id);
                self.log.push(Call::Cancel { id, cancelled });
                cancelled
            }
            None => self.cancel(id),
            Some(other) => {
                self.diverged(format!(
                    "the record has {} where the session cancelled order {}",
                    other.describe(),
                    id.0
                ));
                false
            }
        }
    }

    /// A cancel past the record, answered as the live path would answer it. A cancel that must
    /// reach the venue is sent by `go_live`.
    fn cancel_past_record(&mut self, id: OrderId) -> bool {
        let Some(t) = self.book.orders.get(&id.0).filter(|t| t.open) else {
            return false;
        };
        match t.at {
            At::Refused => self.drop_refused(id),
            At::Intent => {
                let t = self.tracked(id);
                t.at = At::Done;
                t.open = false;
            }
            At::Live if self.fault.is_none() => {
                let t = self.tracked(id);
                t.open = false;
                t.cancel_intent = true;
            }
            // Its outcome is unknown, it is being closed at market, or the broker faulted.
            _ => return false,
        }
        true
    }

    fn replay_bar(&mut self, bar: &Candle) -> Vec<BrokerEvent> {
        match self.next_call() {
            Some(Call::Bar { open_time, events }) if open_time == bar.open_time => {
                for event in &events {
                    if let Err(why) = self.replay_event(event) {
                        self.diverged(why);
                        return Vec::new();
                    }
                }
                self.flush_records();
                events
            }
            next => {
                let what = match next {
                    Some(call) => format!("the record has {}", call.describe()),
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

    /// Applies one recorded event to the replay's orders. A fill must be of an order the replay
    /// holds, a cancellation of one it holds or of the attached stop of one, and an announced stop
    /// the one its entry carries.
    fn replay_event(&mut self, event: &BrokerEvent) -> Result<(), String> {
        match event {
            BrokerEvent::Filled(f) => match self.book.orders.get_mut(&f.order_id.0) {
                Some(t) => t.open = false,
                None => {
                    return Err(format!(
                        "the record reports a fill of order {}, which the replay does not hold",
                        f.order_id.0
                    ));
                }
            },
            BrokerEvent::Cancelled { id, .. } => {
                let attached = self
                    .book
                    .orders
                    .values()
                    .any(|e| e.order.attached_stop.is_some_and(|s| s.id == *id));
                match self.book.orders.get_mut(&id.0) {
                    Some(t) => t.open = false,
                    None if attached => {}
                    None => {
                        return Err(format!(
                            "the record cancels order {}, which the replay does not hold",
                            id.0
                        ));
                    }
                }
            }
            BrokerEvent::Placed(order) => {
                let carried = self.book.orders.values().find_map(|e| {
                    stop_order(&e.order, order.request.qty).filter(|stop| stop.id == order.id)
                });
                if carried.as_ref() != Some(order) {
                    return Err(format!(
                        "the record announces stop {}, which no entry the replay holds carries",
                        order.id.0
                    ));
                }
                self.track(order.clone(), At::Live);
            }
        }
        Ok(())
    }

    fn go_live(&mut self) {
        self.acted_live = true;
        self.fault_fresh = false;
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
        // A fault the records carry, or one the replay raised, stands: nothing goes out.
        if self.fault.is_some() {
            return;
        }
        let intents: Vec<OrderId> = self
            .book
            .orders
            .values()
            .filter(|t| t.open && matches!(t.at, At::Intent | At::Unknown { .. }))
            .map(|t| t.order.id)
            .collect();
        for id in intents {
            match self.find(id) {
                Ok(Some(found)) => self.adopt(id, &found),
                Ok(None) => {
                    let order = &self.book.orders[&id.0].order;
                    if let Some(why) = unsupported(&order.request, order.attached_stop.is_some()) {
                        self.refuse(id, why);
                    } else if self.is_stale(order) {
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
            match self.venue_cancel(&cid) {
                // The next read settles it, and reports any fill made before the cancel took.
                Some(Ok(CancelOutcome::Cancelled)) => self.tracked(id).cancel_asked = true,
                // Already closed: its fill arrives through the executions on the next bar. Not
                // sent on a fault raised above.
                Some(Ok(CancelOutcome::AlreadyClosed(_))) | None => {}
                Some(Err(e)) => self.raise(
                    FaultKind::UnknownOpenOrder,
                    format!(
                        "cancelling {cid}, which the replayed session cancelled, failed: {}",
                        venue_text(&e)
                    ),
                ),
            }
        }
        // A venue that cannot be read now is reconciled on the next bar, as any bar's reads.
        if self.fault.is_none() && self.verify().is_err() {
            self.book.read_failures = self.book.read_failures.saturating_add(1);
        }
    }

    /// The reconcile at the end of a replay: the venue's open orders must be the session's, and
    /// its position the recorded fills plus this session's executions not yet reported.
    fn verify(&mut self) -> Result<(), RouteError> {
        let executions = self.client.executions_since(
            &self.instrument,
            self.book.cursor.saturating_sub(LOOKBACK_MS),
        )?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venue_text_drops_control_characters_and_caps_the_length() {
        assert_eq!(venue_text("t-o1\u{7}\r\n\u{1b}[2J"), "t-o1[2J");
        let long = format!("{}\n", "x".repeat(500));
        assert_eq!(venue_text(&long), "x".repeat(TEXT_MAX_LEN));
    }
}
