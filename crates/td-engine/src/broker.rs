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
    pub status: OrderStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum OrderStatus {
    Open,
    Filled,
    Cancelled(String),
    Rejected(String),
}

/// What happened to open orders on a bar.
#[derive(Debug, Clone, PartialEq)]
pub enum BrokerEvent {
    Filled(Fill),
    Cancelled { id: OrderId, reason: String },
}

/// Where orders go: the simulated broker in backtests and paper trading, and later an exchange
/// testnet or a real account.
pub trait Broker: Send {
    /// Takes an order the session has already rounded and validated.
    fn submit(&mut self, order: Order);
    /// Cancels an open order. Returns false if no open order has this id.
    fn cancel(&mut self, id: OrderId) -> bool;
    /// Processes a new closed bar and returns the fills and cancellations it caused, in the order
    /// they happened. `position` is the signed position before the bar.
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
pub struct SimBroker<M: FillModel> {
    instrument: Instrument,
    fees: FeeSchedule,
    model: M,
    open: Vec<Order>,
    /// Stop-limit orders whose trigger was reached on an earlier bar.
    triggered: Vec<OrderId>,
}

impl<M: FillModel> SimBroker<M> {
    pub fn new(instrument: Instrument, fees: FeeSchedule, model: M) -> Self {
        SimBroker {
            instrument,
            fees,
            model,
            open: Vec::new(),
            triggered: Vec::new(),
        }
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
            let was_triggered = self.triggered.contains(&order.id);
            let priority = match order.request.order_type {
                OrderType::Market => 0,
                OrderType::Stop { .. } => 1,
                OrderType::StopLimit { .. } if !was_triggered => 1,
                OrderType::Limit { .. } | OrderType::StopLimit { .. } => 2,
            };
            match self
                .model
                .check(order, was_triggered, bar, &self.instrument)
            {
                FillCheck::Fill { price, liquidity } => {
                    candidates.push((priority, order.id, index, price, liquidity))
                }
                FillCheck::Triggered => newly_triggered.push(order.id),
                FillCheck::NoFill => {}
            }
            if order.request.tif == TimeInForce::Ioc {
                ioc.push(order.id);
            }
        }
        candidates.sort_by_key(|&(priority, id, ..)| (priority, id));

        let mut events = Vec::new();
        let mut done = Vec::new();
        for (_, id, index, price, liquidity) in candidates {
            let req = &self.open[index].request;
            done.push(id);
            let mut qty = req.qty;
            if req.reduce_only {
                // How much of the position this order's side can close.
                let reducible = match req.side {
                    Side::Buy => -position,
                    Side::Sell => position,
                };
                qty = self.instrument.round_qty(qty.min(reducible.max(0.0)));
                if qty <= 0.0 {
                    events.push(BrokerEvent::Cancelled {
                        id,
                        reason: "reduce-only order would not reduce the position".into(),
                    });
                    continue;
                }
            }
            let rate = match liquidity {
                Liquidity::Maker => self.fees.maker,
                Liquidity::Taker => self.fees.taker,
            };
            position += req.side.sign() * qty;
            events.push(BrokerEvent::Filled(Fill {
                order_id: id,
                instrument: req.instrument.clone(),
                side: req.side,
                // The candle model knows only the bar a fill happened in, not the moment.
                time: bar.open_time,
                price,
                qty,
                fee: price * qty * rate,
                liquidity,
            }));
        }
        for id in ioc {
            if !done.contains(&id) {
                done.push(id);
                events.push(BrokerEvent::Cancelled {
                    id,
                    reason: "IOC order did not fill on its first bar".into(),
                });
            }
        }
        self.triggered.extend(newly_triggered);
        self.open.retain(|o| !done.contains(&o.id));
        self.triggered.retain(|t| !done.contains(t));
        events
    }

    fn open_orders(&self) -> &[Order] {
        &self.open
    }
}
