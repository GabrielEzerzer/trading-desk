//! Place, cancel and amend, and the resolution of a request whose answer never arrived.
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
//! Resolution, when a place, cancel or amend got no answer (timeout, dropped connection, a 5xx or
//! the venue's own server error): the venue is asked about the client id (real-time orders, order
//! history, executions), in a few rounds a settle pause apart.
//! - Place: an order or an execution for the id means it was placed. The request is sent again,
//!   with the same id, only after the original's receive window has passed and two rounds in a row
//!   found nothing; a `110072` (duplicate id) answer to that resend means the original got through,
//!   and the order is looked up, never placed twice. If the venue cannot be asked, the answer is
//!   [`Error::OutcomeUnknown`].
//! - Cancel and amend cannot create an order, and an amend sets absolute values, so while the
//!   order is still open with the old values they are sent again; a closed order or the requested
//!   values settle the outcome.

use std::time::Duration;

use serde_json::{Value, json};
use td_core::{
    CoreError, Instrument, InstrumentKind, Millis, OrderRequest, OrderType, Rounding, Side,
    TimeInForce, Venue,
};

use crate::client::{Failure, Method};
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
/// How many times a request with an unknown outcome is sent again.
const RESENDS: u32 = 2;
/// How many lookup rounds resolve one unknown outcome; two in a row must find nothing before the
/// venue counts as holding no order.
const LOOKUP_ROUNDS: u32 = 4;

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
    /// Executions but no order record yet (order history may lag): the order existed and filled.
    Executed {
        order_id: String,
    },
}

enum Lookup {
    Found(Seen),
    Absent,
    CannotAsk(String),
}

impl Client {
    /// Places `order` with client id `id`. Supported: a market order (reduce-only or not) and a
    /// reduce-only stop, which rests at the venue as a conditional market order triggered by the
    /// last price. The quantity and trigger must already be on the instrument's grid
    /// (`Instrument::round_qty`, `round_price`); the order is checked with
    /// `Instrument::validate_order` against `last_price` and refused before sending if it fails.
    ///
    /// The id is used once: a second `place` with an id this client has sent is refused, and the
    /// venue refuses an id it holds ([`Error::DuplicateClientId`]). A request whose answer never
    /// arrived is resolved as the module documentation describes; when it cannot be, the answer
    /// is [`Error::OutcomeUnknown`] and the caller must not reuse the id for a new order.
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
        let placed = |order_id: String| Placed {
            order_id,
            client_id: id.clone(),
        };
        let (mut reason, mut sent_at) = match self.call(CREATE, &body) {
            Ok(v) => match wire::from_value::<Ack>(CREATE, v) {
                Ok(ack) => return Ok(placed(ack.order_id)),
                // Accepted, but the id was unreadable: find it like any unknown outcome.
                Err(e) => (e.to_string(), self.server_now()),
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
            Err(Failure::Unknown { reason, sent_at }) => (reason, sent_at),
        };
        let unknown = |reason: String| Error::OutcomeUnknown {
            action: Action::Place,
            id: id.to_string(),
            reason,
        };
        let mut venue_reported_duplicate = false;
        for resend in 0..=RESENDS {
            self.wait_past_window(sent_at);
            match self.settled_lookup(instrument, id) {
                Lookup::Found(Seen::Order(o)) => return Ok(placed(o.order_id)),
                Lookup::Found(Seen::Executed { order_id }) => return Ok(placed(order_id)),
                Lookup::CannotAsk(why) => {
                    return Err(unknown(format!(
                        "{reason}; the venue could not be asked: {why}"
                    )));
                }
                Lookup::Absent if venue_reported_duplicate => {
                    return Err(unknown(
                        "the venue reported the id as a duplicate but lists no order for it".into(),
                    ));
                }
                Lookup::Absent if resend == RESENDS => break,
                Lookup::Absent => {}
            }
            // The venue holds no order for the id and the original can no longer be accepted.
            match self.call(CREATE, &body) {
                Ok(v) => match wire::from_value::<Ack>(CREATE, v) {
                    Ok(ack) => return Ok(placed(ack.order_id)),
                    Err(e) => (reason, sent_at) = (e.to_string(), self.server_now()),
                },
                Err(Failure::Venue {
                    code: RET_DUPLICATE_ID,
                    ..
                }) => {
                    venue_reported_duplicate = true;
                    reason = "the venue answered the resend with a duplicate id".into();
                    sent_at = 0;
                }
                Err(Failure::Venue { code, message }) => {
                    return Err(reject(instrument, code, message));
                }
                Err(Failure::NotSent(e)) => {
                    self.release_id(id);
                    return Err(e);
                }
                Err(Failure::Unknown {
                    reason: r,
                    sent_at: t,
                }) => (reason, sent_at) = (r, t),
            }
        }
        Err(unknown(format!(
            "{reason}; still unresolved after {RESENDS} resends"
        )))
    }

    /// Cancels the order with client id `id`.
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
        let unknown = |reason: String| Error::OutcomeUnknown {
            action: Action::Cancel,
            id: id.to_string(),
            reason,
        };
        let mut reason = String::new();
        for _ in 0..=RESENDS {
            match self.call(CANCEL, &body) {
                Ok(_) => return Ok(CancelOutcome::Cancelled),
                Err(Failure::Venue { code, message }) if RET_NOT_OPEN.contains(&code) => {
                    return match self.settled_lookup(instrument, id) {
                        Lookup::Found(seen) => match closed_status(&seen) {
                            Some(status) => Ok(CancelOutcome::AlreadyClosed(status)),
                            None => Err(Error::Rejected { code, message }),
                        },
                        Lookup::Absent => Err(Error::OrderNotFound { id: id.to_string() }),
                        Lookup::CannotAsk(why) => Err(unknown(format!(
                            "the venue answered {code} and could not then be asked: {why}"
                        ))),
                    };
                }
                Err(Failure::Venue { code, message }) => {
                    return Err(Error::Rejected { code, message });
                }
                Err(Failure::NotSent(e)) => return Err(e),
                Err(Failure::Unknown { reason: r, .. }) => reason = r,
            }
            match self.settled_lookup(instrument, id) {
                Lookup::Found(seen) => match closed_status(&seen) {
                    Some(s) if s.is_cancelled() => return Ok(CancelOutcome::Cancelled),
                    Some(s) => return Ok(CancelOutcome::AlreadyClosed(s)),
                    None => {} // still open: cancel again
                },
                Lookup::Absent => return Err(Error::OrderNotFound { id: id.to_string() }),
                Lookup::CannotAsk(why) => {
                    return Err(unknown(format!(
                        "{reason}; the venue could not be asked: {why}"
                    )));
                }
            }
        }
        Err(unknown(format!(
            "{reason}; the order was still open after {RESENDS} resends"
        )))
    }

    /// Changes the trigger price, the quantity or both of the resting stop with client id `id`, in
    /// place. Each given value must be on the instrument's grid; a quantity must be at least the
    /// minimum.
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
        let unknown = |reason: String| Error::OutcomeUnknown {
            action: Action::Amend,
            id: id.to_string(),
            reason,
        };
        let closed = |status: OrderStatus| Error::OrderClosed {
            id: id.to_string(),
            status: status.to_string(),
        };
        let mut reason = String::new();
        for _ in 0..=RESENDS {
            match self.call(AMEND, &body) {
                Ok(_) => return Ok(()),
                Err(Failure::Venue { code, message }) if RET_NOT_OPEN.contains(&code) => {
                    return match self.settled_lookup(instrument, id) {
                        Lookup::Found(seen) => match closed_status(&seen) {
                            Some(status) => Err(closed(status)),
                            None => Err(Error::Rejected { code, message }),
                        },
                        Lookup::Absent => Err(Error::OrderNotFound { id: id.to_string() }),
                        Lookup::CannotAsk(why) => Err(unknown(format!(
                            "the venue answered {code} and could not then be asked: {why}"
                        ))),
                    };
                }
                Err(Failure::Venue { code, message }) => {
                    return Err(Error::Rejected { code, message });
                }
                Err(Failure::NotSent(e)) => return Err(e),
                Err(Failure::Unknown { reason: r, .. }) => reason = r,
            }
            match self.settled_lookup(instrument, id) {
                Lookup::Found(Seen::Order(o)) if o.status.is_open() => {
                    let done = trigger.is_none_or(|t| o.trigger_price == Some(t))
                        && qty.is_none_or(|q| o.qty == q);
                    if done {
                        return Ok(());
                    }
                    // Still the old values: amend again.
                }
                Lookup::Found(seen) => {
                    return Err(closed(closed_status(&seen).unwrap_or(OrderStatus::Filled)));
                }
                Lookup::Absent => return Err(Error::OrderNotFound { id: id.to_string() }),
                Lookup::CannotAsk(why) => {
                    return Err(unknown(format!(
                        "{reason}; the venue could not be asked: {why}"
                    )));
                }
            }
        }
        Err(unknown(format!(
            "{reason}; the order still had its old values after {RESENDS} resends"
        )))
    }

    /// Sleeps until the venue would refuse a request signed at `sent_at` as expired, plus the
    /// settle margin for clock error.
    fn wait_past_window(&self, sent_at: Millis) {
        let until = sent_at
            + self.timing.recv_window.as_millis() as Millis
            + self.timing.settle.as_millis() as Millis;
        let wait = until - self.server_now();
        if wait > 0 {
            std::thread::sleep(Duration::from_millis(wait as u64));
        }
    }

    /// Asks the venue about `id` in up to [`LOOKUP_ROUNDS`] rounds, a settle pause apart. Anything
    /// found settles it; nothing found counts only when two rounds in a row say so.
    fn settled_lookup(&self, instrument: &Instrument, id: &ClientOrderId) -> Lookup {
        let mut absent = 0;
        let mut last_error = String::from("no round finished");
        for round in 0..LOOKUP_ROUNDS {
            if round > 0 {
                std::thread::sleep(self.timing.settle);
            }
            match self.find(instrument, id) {
                Ok(Some(seen)) => return Lookup::Found(seen),
                Ok(None) => {
                    absent += 1;
                    if absent >= 2 {
                        return Lookup::Absent;
                    }
                }
                Err(e) => {
                    absent = 0;
                    last_error = e.to_string();
                }
            }
        }
        Lookup::CannotAsk(last_error)
    }

    /// One round: the real-time orders (open and recent closed ones), the order history, then the
    /// executions for the id.
    fn find(&self, instrument: &Instrument, id: &ClientOrderId) -> Result<Option<Seen>, Error> {
        if let Some(order) = self.order_by_client_id_with(instrument, id, true)? {
            return Ok(Some(Seen::Order(order)));
        }
        let execs = self.executions_for_id(instrument, id)?;
        Ok(execs.into_iter().next().map(|e| Seen::Executed {
            order_id: e.order_id,
        }))
    }
}

/// The closed status of what was seen, `None` while the order is open.
fn closed_status(seen: &Seen) -> Option<OrderStatus> {
    match seen {
        Seen::Order(o) if o.status.is_open() => None,
        Seen::Order(o) => Some(o.status.clone()),
        Seen::Executed { .. } => Some(OrderStatus::Filled),
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
