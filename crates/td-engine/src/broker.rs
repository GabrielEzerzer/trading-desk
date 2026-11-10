use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use td_core::{
    Candle, FeeSchedule, Fill, Instrument, Liquidity, Millis, OrderId, OrderRequest, OrderType,
    Side, TimeInForce,
};

use crate::{FillCheck, FillModel};

/// An order the session accepted or rejected, with its request as rounded at submission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
/// second of two exits finds less position to close), so each fill moves the position before the
/// next one is applied. First come the resting limits the bar opens through: the market traded
/// through them between the bars, before the open, so they fill at their limits ahead of anything
/// at the open, in the order the market passed them (farthest from the open first, then by order
/// id). Then the fills at the open, by order id: market and IOC orders, limits sent through the
/// market on their first bar, and stops the bar opens through. The path inside the bar is unknown,
/// so the fills inside its range follow in the order that is worse for the strategy: stops and
/// stop-limits triggered on this bar, then limit orders, then by order id. A protective stop
/// therefore beats a take-profit limit when the bar reaches both inside its range, but a
/// take-profit the bar opens through fills first, at its limit, before even a market order sent
/// on the same close.
///
/// A reduce-only limit the bar opens through that has nothing to reduce between the bars, such as
/// a take-profit sent on the same close as its market entry, is not cancelled then. It waits for
/// the open, and after the other fills at the open it takes liquidity there like a limit arriving
/// at the open: at the open with slippage, never past its limit, as a taker, for at most the
/// position it can reduce. If there is still nothing to reduce, it is cancelled. One that has part
/// of its quantity to reduce between the bars fills that part at its limit and is done, as any
/// reduce-only order fills once.
///
/// When an entry with an attached stop fills, the stop is placed at once and checked against the
/// same bar. The path after the fill is unknown, so the whole bar range counts as coming after it:
/// the stop fills at its trigger if the bar reaches it, or at the market price the entry met if
/// that was already through the trigger, as when the bar opens past the stop. An entry that filled
/// between the bars met the market at its limit, and the market then went on to the open, so its
/// stop is checked from the limit: a stop between the limit and the open is crossed on the way and
/// fills at its trigger, before the open. Slippage applies as for any stop. The stop's fill is
/// applied right after its entry's, except when the stop triggers inside the range after an entry
/// that filled at or before the open: then it waits for the bar's fills at the open and takes its
/// place among the fills inside the range.
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

/// An order the fill model filled on the bar being processed, before the fill is applied.
struct Pending {
    order: Order,
    price: f64,
    liquidity: Liquidity,
    /// The market price the order met; see [`FillCheck::Fill`].
    market: f64,
}

impl Pending {
    /// Whether this is a resting limit that `bar` opened through, which filled at its limit between
    /// the bars. Only a resting limit fills as a maker.
    fn between_bars(&self, bar: &Candle) -> bool {
        self.liquidity == Liquidity::Maker && self.market == bar.open
    }
}

/// What follows from applying one fill on the bar being processed.
enum After {
    Nothing,
    /// The order's attached stop also fills on this bar.
    Stop(Pending),
    /// The order had nothing to reduce between the bars and waits for the open, where it would
    /// fill like this.
    Wait(Pending),
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
        // This bar's fills: those at the open, and those inside the range with a priority.
        let mut at_open = Vec::new();
        let mut inside = Vec::new();
        let mut newly_triggered = Vec::new();
        let mut ioc = Vec::new();
        for (index, order) in self.open.iter().enumerate() {
            // The engine's no-lookahead rule: never fill on the bar the order was decided on.
            if order.submitted_at >= bar.open_time {
                continue;
            }
            let first = order.submitted_at >= self.last_bar;
            let was_triggered = self.triggered.contains(&order.id);
            match self
                .model
                .check(order, first, was_triggered, bar, &self.instrument)
            {
                FillCheck::Fill {
                    price,
                    liquidity,
                    market,
                } => {
                    let fill = Pending {
                        order: order.clone(),
                        price,
                        liquidity,
                        market,
                    };
                    if market == bar.open {
                        at_open.push(fill);
                    } else {
                        // Stops and stop-limits triggered on this bar before limits.
                        let priority = match order.request.order_type {
                            OrderType::Stop { .. } => 0,
                            OrderType::StopLimit { .. } if !was_triggered => 0,
                            _ => 1,
                        };
                        inside.push((priority, fill));
                    }
                }
                FillCheck::Triggered => newly_triggered.push(order.id),
                FillCheck::NoFill => {}
            }
            if order.request.tif == TimeInForce::Ioc {
                ioc.push(index);
            }
        }

        const NOTHING_TO_REDUCE: &str = "reduce-only order would not reduce the position";
        let mut events = Vec::new();
        let mut done = Vec::new();
        let mut placed = Vec::new();
        // Applies one fill and places the order's attached stop, if it has one. Returns the stop's
        // fill when the stop fills on this bar too, or the order's fill at the open when it waits
        // for the open.
        let mut apply = |fill: Pending| {
            let between = fill.between_bars(bar);
            // The rest of this bar, from the entry's fill on. An entry that filled between the bars
            // met the market at its limit, and the market went on from there to the open.
            let from = if between { fill.price } else { fill.market };
            let rest = Candle {
                open: from,
                high: bar.high.max(from),
                low: bar.low.min(from),
                ..*bar
            };
            let order = fill.order;
            done.push(order.id);
            let Some(applied) = self.fill(&order, fill.price, fill.liquidity, bar, &mut position)
            else {
                // A reduce-only limit the market passed between the bars while it had nothing to
                // reduce waits for the open, where the fills at the open can give it a position.
                // There it takes liquidity like a limit arriving at the open, which the model
                // prices as an IOC limit: the open with slippage, never past the limit, as a taker.
                if between && order.request.reduce_only {
                    let mut arriving = order.clone();
                    arriving.request.tif = TimeInForce::Ioc;
                    if let FillCheck::Fill {
                        price,
                        liquidity,
                        market,
                    } = self
                        .model
                        .check(&arriving, false, false, bar, &self.instrument)
                    {
                        return After::Wait(Pending {
                            order,
                            price,
                            liquidity,
                            market,
                        });
                    }
                }
                cancel(&mut events, &order, NOTHING_TO_REDUCE);
                return After::Nothing;
            };
            let qty = applied.qty;
            events.push(BrokerEvent::Filled(applied));
            let Some(attached) = order.attached_stop else {
                return After::Nothing;
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
            match self
                .model
                .check(&stop, true, false, &rest, &self.instrument)
            {
                FillCheck::Fill {
                    price,
                    liquidity,
                    market,
                } => After::Stop(Pending {
                    order: stop,
                    price,
                    liquidity,
                    market,
                }),
                FillCheck::Triggered | FillCheck::NoFill => {
                    placed.push(stop);
                    After::Nothing
                }
            }
        };
        // Limits filled between the bars first, farthest from the open first: the market passed
        // them in that order on its way to the open. They are all on one side, since a buy and a
        // sell limit can both rest unfilled only with the sell above the buy, and an open cannot
        // be through both. Then the fills at the open, by id.
        let gap = |fill: &Pending| {
            fill.between_bars(bar)
                .then(|| (fill.price - bar.open).abs())
        };
        at_open.sort_by(|a, b| {
            match (gap(a), gap(b)) {
                (Some(a), Some(b)) => b.total_cmp(&a),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
            .then(a.order.id.cmp(&b.order.id))
        });
        // Whether a stop's market was reached by the open: at it, or on the way to it from a limit
        // that filled between the bars.
        let by_open = |stop: &Pending| match stop.order.request.side {
            Side::Sell => stop.market >= bar.open,
            Side::Buy => stop.market <= bar.open,
        };
        // Reduce-only limits that waited for the open fill after the other fills at the open, which
        // can give them a position to reduce; with still nothing to reduce they are cancelled.
        let mut waiting = Vec::new();
        for fill in at_open {
            match apply(fill) {
                // The entry met a market already through the stop, or the market crossed the stop
                // on its way from the entry's limit to the open, so the stop fills now.
                After::Stop(stop) if by_open(&stop) => {
                    apply(stop);
                }
                After::Stop(stop) => inside.push((0, stop)),
                After::Wait(fill) => waiting.push(fill),
                After::Nothing => {}
            }
        }
        for fill in waiting {
            match apply(fill) {
                After::Stop(stop) if by_open(&stop) => {
                    apply(stop);
                }
                After::Stop(stop) => inside.push((0, stop)),
                After::Wait(_) | After::Nothing => {}
            }
        }
        inside.sort_by_key(|(priority, fill)| (*priority, fill.order.id));
        for (_, fill) in inside {
            // An entry filled inside the range has its stop applied straight after it.
            if let After::Stop(stop) = apply(fill) {
                apply(stop);
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
