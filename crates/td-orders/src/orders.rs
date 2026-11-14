//! Place, cancel and amend, and how each finds out what the venue did.
//!
//! Order kinds, from <https://bybit-exchange.github.io/docs/v5/order/create-order> (read
//! 2026-11-14): a market order is `orderType=Market`; setting `triggerPrice` makes any order
//! conditional, with `triggerDirection` 1 (triggers when the price rises to it) or 2 (falls to
//! it), and a rising trigger must be above the market price, a falling one below. `reduceOnly`
//! means the order "can only reduce" the position. `positionIdx` 0 is one-way mode. Amend changes
//! "unfilled or partially filled" orders, here the trigger price and quantity of a resting stop.
//! The page does not say whether a reduce-only conditional order may be placed with no position;
//! the route does not depend on it.
//!
//! Resolution. A lookup asks the venue about the client id (real-time orders, order history,
//! executions) in rounds a settle pause apart: at least [`LOOKUP_ROUNDS`], and on until the
//! receive window of the request in question has passed, so the venue can no longer accept it.
//! Empty rounds never prove that a request was not taken: the order history "may delay", and the
//! real-time list's closed-order cache can be cleared.
//! - Place: a create is sent once per id. Only a refusal before processing (the `client` module
//!   lists them: a 10006 rate limit and a timestamp refusal, each sent again there within bounds,
//!   and an IP block) frees the id. Any other answer that is not an acceptance or a `retCode`
//!   refusal (a timeout, a dropped connection, a 5xx, HTTP 429, the body codes 429, 10429 and
//!   110079, and the rest the `client` module lists) is looked up, never sent again. An order
//!   record or an execution for the id means it was placed; a record the venue rejected is
//!   [`Error::OrderRejected`]; nothing found, or a venue that cannot be asked, is
//!   [`Error::OutcomeUnknown`], never "not placed", and the id stays spent. A refusal of a create
//!   sent once is final: `110072` (duplicate id) is [`Error::DuplicateClientId`], any other code
//!   [`Error::Rejected`]. A refusal of a create sent again after a rate-limit or timestamp refusal
//!   is looked up like any unknown answer, since the venue may have applied the first send.
//! - Cancel and amend cannot create an order, and an amend sets absolute values, so they may be
//!   sent again while the order is still listed open with its old values, at most [`RESENDS`]
//!   times. The venue's acknowledgement is not taken as done: after it, a cancel is done when a
//!   lookup shows the order closed, and an amend when it shows the requested trigger and
//!   quantity. Unconfirmed within the lookup is [`Error::OutcomeUnknown`]. Once an attempt's
//!   outcome is unknown, an answer that attempt could have caused (a refusal, an order the lists
//!   do not show, a record still open after the venue said it is not) is
//!   [`Error::OutcomeUnknown`], never a final refusal or "not found". A refusal of a request sent
//!   again after a rate-limit or timestamp refusal is such an attempt too. Executions with no
//!   order record say the order traded but not whether it is still open, so they are also
//!   [`Error::OutcomeUnknown`], with the executed quantity.
//!
//! Each operation runs within [`crate::Timing::operation_budget`]. When it runs out before the
//! first request is sent, the answer is the not-sent error; once a request has been sent, it is
//! [`Error::OutcomeUnknown`] unless a lookup has settled the outcome. A request is sent again only
//! while a whole request timeout is left.

use std::time::Instant;

use serde_json::{Value, json};
use td_core::{
    CoreError, Instrument, InstrumentKind, OrderRequest, OrderType, Rounding, Side, TimeInForce,
    Venue,
};

use crate::client::{Failure, Method, pause};
use crate::wire::{self, Ack, OrderStatus, VenueOrder};
use crate::{Action, Client, ClientOrderId, Error};

pub(crate) const CATEGORY: &str = "linear";
const CREATE: &str = "/v5/order/create";
const CANCEL: &str = "/v5/order/cancel";
const AMEND: &str = "/v5/order/amend";

const RET_DUPLICATE_ID: i64 = 110072;
/// "Order does not exist", "the order has been completed or cancelled", "the order has been
/// cancelled": the answers to a cancel or amend of an order that is not open.
const RET_NOT_OPEN: [i64; 3] = [110001, 110008, 110010];
/// How many times a cancel or amend is sent again while the order is still listed open with its
/// old values. A create is never sent again.
const RESENDS: u32 = 2;
/// The fewest rounds in a lookup.
const LOOKUP_ROUNDS: u32 = 3;

/// An order the venue holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub order_id: String,
    pub client_id: ClientOrderId,
}

/// How a cancel ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The order is cancelled.
    Cancelled,
    /// The order had already closed another way (filled, rejected) before the cancel took.
    AlreadyClosed(OrderStatus),
}

/// What the venue knows about a client id.
pub(crate) enum Seen {
    Order(VenueOrder),
    /// Executions but no order record yet (order history may lag): the order existed and traded
    /// `qty` in all, and whether it is still open is unknown.
    Executed {
        order_id: String,
        qty: f64,
    },
}

/// An order's state, as far as what was seen tells it.
enum State {
    Open,
    Closed(OrderStatus),
    /// Executions and no record: traded this much, open or not unknown.
    Traded(f64),
}

impl Seen {
    fn state(&self) -> State {
        match self {
            Seen::Order(o) if o.status.is_open() => State::Open,
            Seen::Order(o) => State::Closed(o.status.clone()),
            Seen::Executed { qty, .. } => State::Traded(*qty),
        }
    }

    fn closed(&self) -> bool {
        matches!(self.state(), State::Closed(_))
    }
}

/// The latest answer a lookup got.
enum Latest {
    /// No round got an answer.
    NoAnswer,
    /// The venue listed no order and no execution for the id.
    Nothing,
    Seen(Seen),
}

/// How a lookup ended: the latest answer, and how the lookup went, as "3 lookup rounds over
/// 6.1 s" with the rounds that could not ask the venue.
struct Watched {
    latest: Latest,
    span: String,
}

/// The reason for an outcome left unknown by executions with no order record.
fn traded(qty: f64) -> String {
    format!(
        "the venue shows {qty} executed for the id but no order record, so whether the order is \
         still open is unknown"
    )
}

/// `err` as it stands when no earlier attempt's outcome is unknown; otherwise an unknown outcome,
/// since the earlier attempt may have caused it.
fn unless_earlier(
    earlier: &Option<String>,
    err: Error,
    unknown: impl Fn(String) -> Error,
) -> Error {
    match earlier {
        None => err,
        Some(reason) => unknown(format!(
            "{reason}; then {err}, which the earlier attempt may have caused"
        )),
    }
}

impl Client {
    /// Places `order` with client id `id`. Supported: a market order (reduce-only or not) and a
    /// reduce-only stop, which rests at the venue as a conditional market order triggered by the
    /// last price. The quantity and trigger must already be on the instrument's grid
    /// (`Instrument::round_qty`, `round_price`); the order is checked with
    /// `Instrument::validate_order` against `last_price` and refused before sending if it fails.
    ///
    /// The create is sent once. A second `place` with an id this client has sent is refused, and
    /// the venue refuses an id it holds ([`Error::DuplicateClientId`]). An answer that leaves open
    /// whether the venue took the order is resolved by asking the venue, as the module
    /// documentation describes; when that does not settle it, the answer is
    /// [`Error::OutcomeUnknown`] and the id stays spent. The caller then treats the order as
    /// possibly live, as [`Error::OutcomeUnknown`] describes: it keeps the id across a restart,
    /// asks again later with [`Client::order_by_client_id`], counts a possible fill in its
    /// exposure, and never sends the same order again under another id.
    ///
    /// When `place` returns [`Error::RateLimited`], [`Error::TimestampRejected`] or
    /// [`Error::Transport`], the venue did not take the order as far as its answers show: every
    /// send was refused before processing, or none was made. The id is free again, and a retry
    /// of the same order goes under the same id, never a new one. If an earlier send was applied
    /// after all, the venue holds an order with that id, and a retry it refuses as a duplicate
    /// comes back as [`Error::DuplicateClientId`] (or, when the refusal answers a resend within
    /// the retry, is looked up like any unknown outcome). The caller then reads the order with
    /// [`Client::order_by_client_id`] and takes what the venue shows. The venue documents the id
    /// as "always unique" without saying whether that covers closed orders (the `ids` module); a
    /// new id it would never refuse.
    pub fn place(
        &self,
        instrument: &Instrument,
        order: &OrderRequest,
        id: &ClientOrderId,
        last_price: f64,
    ) -> Result<Placed, Error> {
        check_instrument(instrument)?;
        let body = Method::Post(order_body(instrument, order, id, last_price)?);
        self.claim_id(id)?;
        let deadline = Instant::now() + self.timing.operation_budget;
        let placed = |order_id: String| Placed {
            order_id,
            client_id: id.clone(),
        };
        // `call` reports a refusal of a create it sent more than once as unknown, so a venue
        // refusal here answers the one send.
        let (reason, sent) = match self.call(CREATE, &body, Some(deadline)) {
            Ok(v) => match wire::from_value::<Ack>(CREATE, v) {
                Ok(ack) => return Ok(placed(ack.order_id)),
                // Accepted, but the id was unreadable: find it like any unknown outcome.
                Err(e) => (e.to_string(), Instant::now()),
            },
            Err(Failure::Venue {
                code: RET_DUPLICATE_ID,
                ..
            }) => return Err(Error::DuplicateClientId { id: id.to_string() }),
            Err(Failure::Venue { code, message }) => {
                return Err(reject(instrument, code, message));
            }
            Err(Failure::NotSent(e)) => {
                self.release_id(id);
                return Err(e);
            }
            Err(Failure::Unknown { reason, sent }) => (reason, sent),
        };
        let unknown = |why: String| Error::OutcomeUnknown {
            action: Action::Place,
            id: id.to_string(),
            reason: format!("{reason}; {why}"),
        };
        let watched = self.watch(instrument, id, self.window_end(sent), deadline, |_| true);
        match watched.latest {
            Latest::Seen(Seen::Order(o)) if o.status == OrderStatus::Rejected => {
                // Every send carried the same order and id, so the record is this order's.
                Err(Error::OrderRejected {
                    id: id.to_string(),
                    // The read scrubbed the reason whole before cutting it.
                    reason: match o.reject_reason.as_str() {
                        "" | "EC_NoError" => "none given".into(),
                        _ => o.reject_reason,
                    },
                })
            }
            Latest::Seen(Seen::Order(o)) => Ok(placed(o.order_id)),
            Latest::Seen(Seen::Executed { order_id, .. }) => Ok(placed(order_id)),
            Latest::Nothing => Err(unknown(format!(
                "the venue showed no order and no execution for the id in {}",
                watched.span
            ))),
            Latest::NoAnswer => Err(unknown(format!(
                "the venue could not be asked in {}",
                watched.span
            ))),
        }
    }

    /// Cancels the order with client id `id`, and confirms it: the venue's acknowledgement is
    /// followed by lookups until the order shows closed.
    pub fn cancel(
        &self,
        instrument: &Instrument,
        id: &ClientOrderId,
    ) -> Result<CancelOutcome, Error> {
        check_instrument(instrument)?;
        let body = Method::Post(json!({
            "category": CATEGORY,
            "symbol": instrument.id.symbol,
            "orderLinkId": id.as_str(),
        }));
        let deadline = Instant::now() + self.timing.operation_budget;
        let unknown = |reason: String| Error::OutcomeUnknown {
            action: Action::Cancel,
            id: id.to_string(),
            reason,
        };
        let outcome = |status: OrderStatus| {
            if status.is_cancelled() {
                CancelOutcome::Cancelled
            } else {
                CancelOutcome::AlreadyClosed(status)
            }
        };
        let mut earlier: Option<String> = None;
        for send in 0..=RESENDS {
            if send > 0 && !self.room_to_resend(deadline) {
                break;
            }
            let reason = match self.call(CANCEL, &body, Some(deadline)) {
                Ok(_) => {
                    let until = self.window_end(Instant::now());
                    let w = self.watch(instrument, id, until, deadline, Seen::closed);
                    return match w.latest {
                        Latest::Seen(seen) => match seen.state() {
                            State::Closed(s) => Ok(outcome(s)),
                            State::Open => Err(unknown(format!(
                                "the venue acknowledged the cancel, but still listed the order \
                                 open after {}",
                                w.span
                            ))),
                            State::Traded(qty) => Err(unknown(format!(
                                "the venue acknowledged the cancel, but {}",
                                traded(qty)
                            ))),
                        },
                        Latest::Nothing => Err(unknown(format!(
                            "the venue acknowledged the cancel, but showed no order for the id \
                             in {}",
                            w.span
                        ))),
                        Latest::NoAnswer => Err(unknown(format!(
                            "the venue acknowledged the cancel, but could not be asked in {}",
                            w.span
                        ))),
                    };
                }
                Err(Failure::Venue { code, message }) if RET_NOT_OPEN.contains(&code) => {
                    let refused = Error::Rejected { code, message };
                    let w = self.watch(instrument, id, Instant::now(), deadline, Seen::closed);
                    return match w.latest {
                        Latest::Seen(seen) => match seen.state() {
                            State::Closed(s) => Ok(outcome(s)),
                            State::Open => Err(unless_earlier(&earlier, refused, unknown)),
                            State::Traded(qty) => Err(unknown(traded(qty))),
                        },
                        Latest::Nothing => Err(unless_earlier(
                            &earlier,
                            Error::OrderNotFound { id: id.to_string() },
                            unknown,
                        )),
                        Latest::NoAnswer => Err(unknown(format!(
                            "the venue answered {code} and could not then be asked in {}",
                            w.span
                        ))),
                    };
                }
                Err(Failure::Venue { code, message }) => {
                    return Err(unless_earlier(
                        &earlier,
                        Error::Rejected { code, message },
                        unknown,
                    ));
                }
                Err(Failure::NotSent(e)) => return Err(unless_earlier(&earlier, e, unknown)),
                Err(Failure::Unknown { reason, sent }) => {
                    let until = self.window_end(sent);
                    let w = self.watch(instrument, id, until, deadline, Seen::closed);
                    match w.latest {
                        Latest::Seen(seen) => match seen.state() {
                            State::Closed(s) => return Ok(outcome(s)),
                            State::Open => reason, // cancel again
                            State::Traded(qty) => {
                                return Err(unknown(format!("{reason}; {}", traded(qty))));
                            }
                        },
                        Latest::Nothing => {
                            return Err(unknown(format!(
                                "{reason}; the venue then showed no order for the id in {}",
                                w.span
                            )));
                        }
                        Latest::NoAnswer => {
                            return Err(unknown(format!(
                                "{reason}; the venue could not be asked in {}",
                                w.span
                            )));
                        }
                    }
                }
            };
            earlier = Some(reason);
        }
        Err(unknown(format!(
            "{}; the order was still listed open when the resends ran out or too little of the \
             time budget was left to send again",
            earlier.unwrap_or_default()
        )))
    }

    /// Changes the trigger price, the quantity or both of the resting stop with client id `id`, in
    /// place, and confirms it: the venue's acknowledgement is followed by lookups until the order
    /// shows the requested values. Each given value must be on the instrument's grid; a quantity
    /// must be at least the minimum.
    pub fn amend_stop(
        &self,
        instrument: &Instrument,
        id: &ClientOrderId,
        trigger: Option<f64>,
        qty: Option<f64>,
    ) -> Result<(), Error> {
        check_instrument(instrument)?;
        let mut body = json!({
            "category": CATEGORY,
            "symbol": instrument.id.symbol,
            "orderLinkId": id.as_str(),
        });
        if trigger.is_none() && qty.is_none() {
            return Err(Error::Unsupported {
                what: "an amend that changes nothing",
            });
        }
        if let Some(t) = trigger {
            check_price(instrument, "trigger", t)?;
            body["triggerPrice"] = Value::String(wire::decimal(t, instrument.tick_size)?);
        }
        if let Some(q) = qty {
            check_qty(instrument, q)?;
            body["qty"] = Value::String(wire::decimal(q, instrument.lot_size)?);
        }
        let body = Method::Post(body);
        let deadline = Instant::now() + self.timing.operation_budget;
        let unknown = |reason: String| Error::OutcomeUnknown {
            action: Action::Amend,
            id: id.to_string(),
            reason,
        };
        let closed = |status: OrderStatus| Error::OrderClosed {
            id: id.to_string(),
            status: status.to_string(),
        };
        let amended = |o: &VenueOrder| {
            trigger.is_none_or(|t| o.trigger_price == Some(t)) && qty.is_none_or(|q| o.qty == q)
        };
        // The lookup after an amend is settled by the requested values or a closed order.
        let settled = |seen: &Seen| match seen {
            Seen::Order(o) => !o.status.is_open() || amended(o),
            Seen::Executed { .. } => false,
        };
        let mut earlier: Option<String> = None;
        for send in 0..=RESENDS {
            if send > 0 && !self.room_to_resend(deadline) {
                break;
            }
            let reason = match self.call(AMEND, &body, Some(deadline)) {
                Ok(_) => {
                    let until = self.window_end(Instant::now());
                    let w = self.watch(instrument, id, until, deadline, settled);
                    return match w.latest {
                        Latest::Seen(Seen::Order(o)) if o.status.is_open() && amended(&o) => Ok(()),
                        Latest::Seen(Seen::Order(o)) if !o.status.is_open() => {
                            Err(closed(o.status))
                        }
                        Latest::Seen(Seen::Order(_)) => Err(unknown(format!(
                            "the venue acknowledged the amend, but the order still showed other \
                             values after {}",
                            w.span
                        ))),
                        Latest::Seen(Seen::Executed { qty, .. }) => Err(unknown(format!(
                            "the venue acknowledged the amend, but {}",
                            traded(qty)
                        ))),
                        Latest::Nothing => Err(unknown(format!(
                            "the venue acknowledged the amend, but showed no order for the id \
                             in {}",
                            w.span
                        ))),
                        Latest::NoAnswer => Err(unknown(format!(
                            "the venue acknowledged the amend, but could not be asked in {}",
                            w.span
                        ))),
                    };
                }
                Err(Failure::Venue { code, message }) if RET_NOT_OPEN.contains(&code) => {
                    let refused = Error::Rejected { code, message };
                    let w = self.watch(instrument, id, Instant::now(), deadline, Seen::closed);
                    return Err(match w.latest {
                        Latest::Seen(seen) => match seen.state() {
                            State::Closed(s) => closed(s),
                            State::Open => unless_earlier(&earlier, refused, unknown),
                            State::Traded(qty) => unknown(traded(qty)),
                        },
                        Latest::Nothing => unless_earlier(
                            &earlier,
                            Error::OrderNotFound { id: id.to_string() },
                            unknown,
                        ),
                        Latest::NoAnswer => unknown(format!(
                            "the venue answered {code} and could not then be asked in {}",
                            w.span
                        )),
                    });
                }
                Err(Failure::Venue { code, message }) => {
                    return Err(unless_earlier(
                        &earlier,
                        Error::Rejected { code, message },
                        unknown,
                    ));
                }
                Err(Failure::NotSent(e)) => return Err(unless_earlier(&earlier, e, unknown)),
                Err(Failure::Unknown { reason, sent }) => {
                    let until = self.window_end(sent);
                    let w = self.watch(instrument, id, until, deadline, settled);
                    match w.latest {
                        Latest::Seen(Seen::Order(o)) if o.status.is_open() && amended(&o) => {
                            return Ok(());
                        }
                        Latest::Seen(Seen::Order(o)) if !o.status.is_open() => {
                            return Err(closed(o.status));
                        }
                        Latest::Seen(Seen::Order(_)) => reason, // still the old values: amend again
                        Latest::Seen(Seen::Executed { qty, .. }) => {
                            return Err(unknown(format!("{reason}; {}", traded(qty))));
                        }
                        Latest::Nothing => {
                            return Err(unknown(format!(
                                "{reason}; the venue then showed no order for the id in {}",
                                w.span
                            )));
                        }
                        Latest::NoAnswer => {
                            return Err(unknown(format!(
                                "{reason}; the venue could not be asked in {}",
                                w.span
                            )));
                        }
                    }
                }
            };
            earlier = Some(reason);
        }
        Err(unknown(format!(
            "{}; the order still had its old values when the resends ran out or too little of the \
             time budget was left to send again",
            earlier.unwrap_or_default()
        )))
    }

    /// When the venue refuses, as expired, a request stamped at local instant `sent`, plus the
    /// settle margin for clock error. Measured on the local clock, so a clock resync during the
    /// lookup cannot move it.
    fn window_end(&self, sent: Instant) -> Instant {
        sent + self.timing.recv_window + self.timing.settle
    }

    /// Asks the venue about `id` in rounds a settle pause apart, until `done` accepts what it
    /// sees, or at least [`LOOKUP_ROUNDS`] rounds have run and `until` has passed, or the next
    /// pause would pass `deadline`. The latest answer wins: a round that could not ask the venue
    /// does not replace what an earlier round saw.
    fn watch(
        &self,
        instrument: &Instrument,
        id: &ClientOrderId,
        until: Instant,
        deadline: Instant,
        done: impl Fn(&Seen) -> bool,
    ) -> Watched {
        let started = Instant::now();
        let (mut rounds, mut failed) = (0, 0);
        let mut last_error = String::new();
        let mut latest = Latest::NoAnswer;
        loop {
            rounds += 1;
            match self.find(instrument, id, deadline) {
                Ok(Some(seen)) => {
                    let stop = done(&seen);
                    latest = Latest::Seen(seen);
                    if stop {
                        break;
                    }
                }
                Ok(None) => latest = Latest::Nothing,
                Err(e) => {
                    failed += 1;
                    last_error = e.to_string();
                }
            }
            if rounds >= LOOKUP_ROUNDS && Instant::now() >= until {
                break;
            }
            if !pause(self.timing.settle, Some(deadline)) {
                break;
            }
        }
        let mut span = format!(
            "{rounds} lookup rounds over {:.1} s",
            started.elapsed().as_secs_f64()
        );
        if failed > 0 {
            span +=
                &format!(", {failed} of which could not ask the venue (the last: {last_error})");
        }
        Watched { latest, span }
    }

    /// One round: the real-time orders (open and recent closed ones), the order history, then the
    /// executions for the id, each request sent once within `deadline`.
    fn find(
        &self,
        instrument: &Instrument,
        id: &ClientOrderId,
        deadline: Instant,
    ) -> Result<Option<Seen>, Error> {
        if let Some(order) = self.order_by_client_id_with(instrument, id, Some(deadline))? {
            return Ok(Some(Seen::Order(order)));
        }
        let execs = self.executions_for_id(instrument, id, deadline)?;
        Ok(execs.first().map(|e| Seen::Executed {
            order_id: e.order_id.clone(),
            qty: execs.iter().map(|e| e.qty).sum(),
        }))
    }
}

/// The route trades the venue's linear perpetuals only.
pub(crate) fn check_instrument(instrument: &Instrument) -> Result<(), Error> {
    if instrument.id.venue != Venue::Bybit || instrument.id.kind != InstrumentKind::Perpetual {
        return Err(Error::Unsupported {
            what: "an instrument that is not a perpetual on this venue",
        });
    }
    Ok(())
}

/// A venue rejection, with the one-way order refused by a hedge-mode account named as such.
fn reject(instrument: &Instrument, code: i64, message: String) -> Error {
    if code == 10001 && message.to_ascii_lowercase().contains("position idx") {
        return Error::HedgeMode {
            symbol: instrument.id.symbol.clone(),
        };
    }
    Error::Rejected { code, message }
}

/// The create-order body, after the engine's own checks.
fn order_body(
    instrument: &Instrument,
    order: &OrderRequest,
    id: &ClientOrderId,
    last_price: f64,
) -> Result<Value, Error> {
    let trigger = match order.order_type {
        OrderType::Market => None,
        OrderType::Stop { trigger } => Some(trigger),
        OrderType::Limit { .. } | OrderType::StopLimit { .. } => {
            return Err(Error::Unsupported {
                what: "limit orders",
            });
        }
    };
    instrument.validate_order(order, last_price)?;
    let side = match order.side {
        Side::Buy => "Buy",
        Side::Sell => "Sell",
    };
    let mut body = json!({
        "category": CATEGORY,
        "symbol": instrument.id.symbol,
        "side": side,
        "orderType": "Market",
        "qty": wire::decimal(order.qty, instrument.lot_size)?,
        "positionIdx": 0,
        "orderLinkId": id.as_str(),
        "reduceOnly": order.reduce_only,
    });
    if let Some(trigger) = trigger {
        if !order.reduce_only {
            return Err(Error::Unsupported {
                what: "a stop that is not reduce-only",
            });
        }
        if order.tif != TimeInForce::Gtc {
            return Err(Error::Unsupported {
                what: "a resting stop that is not good till cancelled",
            });
        }
        // A sell stop protects a long and triggers on a fall; a buy stop protects a short and
        // triggers on a rise. The venue refuses a trigger on the wrong side of the market.
        let (direction, wrong_side) = match order.side {
            Side::Sell => (2, trigger >= last_price),
            Side::Buy => (1, trigger <= last_price),
        };
        if wrong_side {
            return Err(Error::StopWouldTrigger {
                trigger,
                last_price,
            });
        }
        body["triggerPrice"] = Value::String(wire::decimal(trigger, instrument.tick_size)?);
        body["triggerDirection"] = json!(direction);
        body["triggerBy"] = json!("LastPrice");
    }
    Ok(body)
}

fn check_price(instrument: &Instrument, field: &'static str, price: f64) -> Result<(), Error> {
    if !(price.is_finite() && price > 0.0) {
        return Err(CoreError::InvalidNumber {
            field,
            value: price,
        }
        .into());
    }
    if instrument.round_price(price, Rounding::Nearest) != price {
        return Err(CoreError::PriceOffTick {
            field,
            price,
            tick_size: instrument.tick_size,
        }
        .into());
    }
    Ok(())
}

fn check_qty(instrument: &Instrument, qty: f64) -> Result<(), Error> {
    if !(qty.is_finite() && qty > 0.0) {
        return Err(CoreError::InvalidNumber {
            field: "qty",
            value: qty,
        }
        .into());
    }
    if instrument.round_qty(qty) != qty {
        return Err(CoreError::QtyOffLot {
            qty,
            lot_size: instrument.lot_size,
        }
        .into());
    }
    // The same relative tolerance as Instrument::validate_order.
    if qty < instrument.min_qty * (1.0 - 1e-9) {
        return Err(CoreError::QtyBelowMin {
            qty,
            min: instrument.min_qty,
        }
        .into());
    }
    Ok(())
}
