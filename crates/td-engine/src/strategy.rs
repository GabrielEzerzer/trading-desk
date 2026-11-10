use td_core::{
    Candle, Fill, Instrument, InstrumentKind, OrderId, OrderRequest, OrderType, Rounding, Side,
    Timeframe,
};

use crate::{Broker, Order, OrderRejected, OrderStatus, Portfolio};

/// A trading strategy. The same value runs in a backtest and in paper trading.
///
/// All callbacks run at a bar's close, after that bar's fills, funding and equity mark. On each
/// bar the session calls `on_start` (first bar only), then `on_fill` once per fill on this bar,
/// then `on_bar`. Orders submitted from any of them can first fill on the next bar.
pub trait Strategy: Send {
    fn on_start(&mut self, _ctx: &mut Context) {}
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle);
    fn on_fill(&mut self, _ctx: &mut Context, _fill: &Fill) {}
}

/// What a strategy can see and do during a callback.
pub struct Context<'a> {
    pub(crate) bars: &'a [Candle],
    pub(crate) instrument: &'a Instrument,
    pub(crate) timeframe: Timeframe,
    pub(crate) portfolio: &'a Portfolio,
    pub(crate) broker: &'a mut dyn Broker,
    pub(crate) orders: &'a mut Vec<Order>,
    pub(crate) warming_up: bool,
}

impl Context<'_> {
    /// The bars seen so far, oldest first, ending at the current bar.
    pub fn bars(&self) -> &[Candle] {
        self.bars
    }

    /// The current bar, which has just closed.
    pub fn bar(&self) -> &Candle {
        self.bars
            .last()
            .expect("callbacks run after a bar is pushed")
    }

    pub fn instrument(&self) -> &Instrument {
        self.instrument
    }

    pub fn timeframe(&self) -> Timeframe {
        self.timeframe
    }

    pub fn portfolio(&self) -> &Portfolio {
        self.portfolio
    }

    pub fn position(&self) -> f64 {
        self.portfolio.position()
    }

    pub fn cash(&self) -> f64 {
        self.portfolio.cash()
    }

    pub fn equity(&self) -> f64 {
        self.portfolio.equity()
    }

    pub fn open_orders(&self) -> &[Order] {
        self.broker.open_orders()
    }

    /// True while the session feeds warm-up bars (see [`crate::Session::warm_up`]): the strategy
    /// sees the bars but cannot trade.
    pub fn is_warming_up(&self) -> bool {
        self.warming_up
    }

    /// Rounds the order to the instrument's lot and tick, validates it and sends it to the
    /// broker. Quantities round down; limit prices round in the order's favour-safe direction
    /// (buys down, sells up) so an order never trades worse than asked; triggers round to the
    /// nearest tick. A rejected order is recorded with its reason and returned as an error.
    ///
    /// While warming up every order is refused with id 0 and nothing is recorded, so warm-up
    /// leaves no trace in the run's order list.
    pub fn submit(&mut self, request: OrderRequest) -> Result<OrderId, OrderRejected> {
        if self.warming_up {
            return Err(OrderRejected {
                id: OrderId(0),
                reason: "trading is disabled while warming up".into(),
            });
        }
        let id = OrderId(self.orders.len() as u64 + 1);
        let submitted_at = self.bar().open_time;
        let (request, check) = self.prepare(request);
        let status = match &check {
            Ok(()) => OrderStatus::Open,
            Err(reason) => OrderStatus::Rejected(reason.clone()),
        };
        let order = Order {
            id,
            request,
            submitted_at,
            status,
        };
        self.orders.push(order.clone());
        match check {
            Ok(()) => {
                self.broker.submit(order);
                Ok(id)
            }
            Err(reason) => Err(OrderRejected { id, reason }),
        }
    }

    /// Cancels an open order. Returns false if it is not open.
    pub fn cancel(&mut self, id: OrderId) -> bool {
        if !self.broker.cancel(id) {
            return false;
        }
        if let Some(order) = self.orders.iter_mut().find(|o| o.id == id) {
            order.status = OrderStatus::Cancelled("cancelled by the strategy".into());
        }
        true
    }

    fn prepare(&self, mut req: OrderRequest) -> (OrderRequest, Result<(), String>) {
        let inst = self.instrument;
        let limit_rounding = match req.side {
            Side::Buy => Rounding::Down,
            Side::Sell => Rounding::Up,
        };
        req.qty = inst.round_qty(req.qty);
        req.order_type = match req.order_type {
            OrderType::Market => OrderType::Market,
            OrderType::Limit { price } => OrderType::Limit {
                price: inst.round_price(price, limit_rounding),
            },
            OrderType::Stop { trigger } => OrderType::Stop {
                trigger: inst.round_price(trigger, Rounding::Nearest),
            },
            OrderType::StopLimit { trigger, price } => OrderType::StopLimit {
                trigger: inst.round_price(trigger, Rounding::Nearest),
                price: inst.round_price(price, limit_rounding),
            },
        };
        if let Err(e) = inst.validate_order(&req, self.bar().close) {
            return (req, Err(e.to_string()));
        }
        if inst.id.kind == InstrumentKind::Spot && req.side == Side::Sell {
            // Open sells count against the holding, so several sells cannot add up to a short.
            let pending: f64 = self
                .open_orders()
                .iter()
                .filter(|o| o.request.side == Side::Sell)
                .map(|o| o.request.qty)
                .sum();
            let available = self.position() - pending;
            if req.qty > available * (1.0 + 1e-9) {
                let reason = format!(
                    "spot cannot sell more than it holds: selling {}, {} available",
                    req.qty, available
                );
                return (req, Err(reason));
            }
        }
        (req, Ok(()))
    }
}
