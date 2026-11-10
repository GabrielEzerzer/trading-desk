use serde::Serialize;
use td_core::{
    Candle, FeeSchedule, Fill, Instrument, Liquidity, Millis, OrderId, OrderRequest, OrderType,
    Side, TimeInForce,
};

use crate::{FillCheck, FillModel};

/// An order the session accepted or rejected, with its request as rounded at submission.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Order {
    pub id: OrderId,
    pub request: OrderRequest,
    /// Open time of the bar the order was decided on. It can first fill on a later bar.
    pub submitted_at: Millis,
    /// Close of the bar the order was decided on: the last price before it was sent.
    pub decision_close: f64,
    /// The protective stop to place when this order fills, if one was attached.
    pub attached_stop: Option<AttachedStop>,
    pub status: OrderStatus,
}

/// A protective stop attached to an entry with [`crate::Context::submit_with_stop`]. When the
/// entry fills, the broker places a reduce-only stop on the other side, for the filled quantity,
/// under `id`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct AttachedStop {
    pub id: OrderId,
    pub trigger: f64,
}

impl AttachedStop {
    /// The stop's request for an entry that filled `qty`.
    pub(crate) fn request(&self, entry: &OrderRequest, qty: f64) -> OrderRequest {
        OrderRequest {
            instrument: entry.instrument.clone(),
            side: match entry.side {
                Side::Buy => Side::Sell,
                Side::Sell => Side::Buy,
            },
            qty,
            order_type: OrderType::Stop {
                trigger: self.trigger,
            },
            tif: TimeInForce::Gtc,
            reduce_only: true,
            client_tag: None,
        }
    }
}

/// Why an attached stop is cancelled along with its entry.
pub(crate) const ENTRY_CANCELLED: &str = "its entry was cancelled before it filled";

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum OrderStatus {
    /// A protective stop waiting for its entry to fill.
    Attached,
    Open,
    Filled,
    Cancelled(String),
    Rejected(String),
}

/// What happened to open orders on a bar.
#[derive(Debug, Clone, PartialEq)]
pub enum BrokerEvent {
    Filled(Fill),
    Cancelled {
        id: OrderId,
        reason: String,
    },
    /// An entry filled and its attached stop was placed. The stop is an open order from here on.
    Placed(Order),
}

/// Where orders go: the simulated broker in backtests and paper trading, and later an exchange
/// testnet or a real account.
pub trait Broker: Send {
    /// Takes an order the session has already rounded and validated.
    fn submit(&mut self, order: Order);
    /// Cancels an open order. Returns false if no open order has this id.
    fn cancel(&mut self, id: OrderId) -> bool;
    /// Processes a new closed bar and returns the fills, cancellations and placed stops it caused,
    /// in the order they happened. `position` is the signed position before the bar.
    fn on_bar(&mut self, bar: &Candle, position: f64) -> Vec<BrokerEvent>;
    fn open_orders(&self) -> &[Order];
}

/// A broker that fills orders with a [`FillModel`] and charges fees from a [`FeeSchedule`].
///
/// When one bar fills several orders, the order of the fills matters for reduce-only orders (the
/// second of two exits finds less position to close). The bar's path is unknown, so fills are
/// applied in the order that is worse for the strategy: market orders first (they fill at the
/// open), then stops and stop-limits triggered on this bar, then limit orders, then by order id.
/// A protective stop therefore beats a take-profit limit in the same bar.
///
/// When an entry with an attached stop fills, the stop is placed at once and checked against the
/// same bar before the bar's remaining fills. The path after the fill is unknown, so the whole bar
/// range counts as coming after it: the stop fills at its trigger if the bar reaches it, or at the
/// market price the entry met if that was already through the trigger, as when the bar opens past
/// the stop. Slippage applies as for any stop.
pub struct SimBroker<M: FillModel> {
    instrument: Instrument,
    fees: FeeSchedule,
    model: M,
    open: Vec<Order>,
    /// Stop-limit orders whose trigger was reached on an earlier bar.
    triggered: Vec<OrderId>,
    /// Open time of the previous bar. An order decided on it is seeing its first bar.
    last_bar: Millis,
}

impl<M: FillModel> SimBroker<M> {
    pub fn new(instrument: Instrument, fees: FeeSchedule, model: M) -> Self {
        SimBroker {
            instrument,
            fees,
            model,
            open: Vec::new(),
            triggered: Vec::new(),
            last_bar: Millis::MIN,
        }
    }

    /// Fills `order` at `price` on `bar` and moves `position` by the fill. A reduce-only order is
    /// cut to the position it can close, and `None` means it had nothing to close.
    fn fill(
        &self,
        order: &Order,
        price: f64,
        liquidity: Liquidity,
        bar: &Candle,
        position: &mut f64,
    ) -> Option<Fill> {
        let req = &order.request;
        let mut qty = req.qty;
        if req.reduce_only {
            // How much of the position this order's side can close.
            let reducible = match req.side {
                Side::Buy => -*position,
                Side::Sell => *position,
            };
            qty = self.instrument.round_qty(qty.min(reducible.max(0.0)));
            if qty <= 0.0 {
                return None;
            }
        }
        let rate = match liquidity {
            Liquidity::Maker => self.fees.maker,
            Liquidity::Taker => self.fees.taker,
        };
        *position += req.side.sign() * qty;
        Some(Fill {
            order_id: order.id,
            instrument: req.instrument.clone(),
            side: req.side,
            // The candle model knows only the bar a fill happened in, not the moment.
            time: bar.open_time,
            price,
            qty,
            fee: price * qty * rate,
            liquidity,
        })
    }
}

/// Records the cancellation of `order`, and of its attached stop, which was never placed.
fn cancel(events: &mut Vec<BrokerEvent>, order: &Order, reason: &str) {
    events.push(BrokerEvent::Cancelled {
        id: order.id,
        reason: reason.into(),
    });
    if let Some(stop) = order.attached_stop {
        events.push(BrokerEvent::Cancelled {
            id: stop.id,
            reason: ENTRY_CANCELLED.into(),
        });
    }
}

impl<M: FillModel> Broker for SimBroker<M> {
    fn submit(&mut self, order: Order) {
        self.open.push(order);
    }

    fn cancel(&mut self, id: OrderId) -> bool {
        let before = self.open.len();
        self.open.retain(|o| o.id != id);
        self.triggered.retain(|t| *t != id);
        self.open.len() != before
    }

    fn on_bar(&mut self, bar: &Candle, mut position: f64) -> Vec<BrokerEvent> {
        let mut candidates = Vec::new();
        let mut newly_triggered = Vec::new();
        let mut ioc = Vec::new();
        for (index, order) in self.open.iter().enumerate() {
            // The engine's no-lookahead rule: never fill on the bar the order was decided on.
            if order.submitted_at >= bar.open_time {
                continue;
            }
            let first = order.submitted_at >= self.last_bar;
            let was_triggered = self.triggered.contains(&order.id);
            let priority = match order.request.order_type {
                OrderType::Market => 0,
                OrderType::Stop { .. } => 1,
                OrderType::StopLimit { .. } if !was_triggered => 1,
                OrderType::Limit { .. } | OrderType::StopLimit { .. } => 2,
            };
            match self
                .model
                .check(order, first, was_triggered, bar, &self.instrument)
            {
                FillCheck::Fill {
                    price,
                    liquidity,
                    market,
                } => candidates.push((priority, order.id, index, price, liquidity, market)),
                FillCheck::Triggered => newly_triggered.push(order.id),
                FillCheck::NoFill => {}
            }
            if order.request.tif == TimeInForce::Ioc {
                ioc.push(index);
            }
        }
        candidates.sort_by_key(|&(priority, id, ..)| (priority, id));

        const NOTHING_TO_REDUCE: &str = "reduce-only order would not reduce the position";
        let mut events = Vec::new();
        let mut done = Vec::new();
        let mut placed = Vec::new();
        for (_, id, index, price, liquidity, market) in candidates {
            let order = &self.open[index];
            done.push(id);
            let Some(fill) = self.fill(order, price, liquidity, bar, &mut position) else {
                cancel(&mut events, order, NOTHING_TO_REDUCE);
                continue;
            };
            let qty = fill.qty;
            events.push(BrokerEvent::Filled(fill));
            let Some(attached) = order.attached_stop else {
                continue;
            };
            let stop = Order {
                id: attached.id,
                request: attached.request(&order.request, qty),
                submitted_at: order.submitted_at,
                decision_close: order.decision_close,
                attached_stop: None,
                status: OrderStatus::Open,
            };
            events.push(BrokerEvent::Placed(stop.clone()));
            // The rest of this bar, from the entry's fill on.
            let rest = Candle {
                open: market,
                ..*bar
            };
            match self
                .model
                .check(&stop, true, false, &rest, &self.instrument)
            {
                FillCheck::Fill {
                    price, liquidity, ..
                } => match self.fill(&stop, price, liquidity, bar, &mut position) {
                    Some(fill) => events.push(BrokerEvent::Filled(fill)),
                    None => cancel(&mut events, &stop, NOTHING_TO_REDUCE),
                },
                FillCheck::Triggered | FillCheck::NoFill => placed.push(stop),
            }
        }
        for index in ioc {
            let order = &self.open[index];
            if !done.contains(&order.id) {
                done.push(order.id);
                cancel(
                    &mut events,
                    order,
                    "IOC order did not fill on its first bar",
                );
            }
        }
        self.triggered.extend(newly_triggered);
        self.open.retain(|o| !done.contains(&o.id));
        self.open.extend(placed);
        self.triggered.retain(|t| !done.contains(t));
        self.last_bar = bar.open_time;
        events
    }

    fn open_orders(&self) -> &[Order] {
        &self.open
    }
}
