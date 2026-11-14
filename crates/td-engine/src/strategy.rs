use td_core::{
    Candle, FeeSchedule, Fill, Instrument, Millis, OrderId, OrderRequest, OrderType, Rounding,
    Side, Timeframe,
};

use crate::broker::ENTRY_CANCELLED;
use crate::{AttachedStop, Broker, Order, OrderRejected, OrderStatus, Portfolio};

/// A trading strategy. The same value runs in a backtest and in paper trading.
///
/// All callbacks run at a bar's close, after that bar's fills, funding and equity mark. On each
/// bar the session calls `on_start` (first bar only), then `on_fill` once per fill on this bar,
/// then `on_bar`. Orders submitted from any of them can first fill on the next bar. Warm-up bars
/// ([`crate::Session::warm_up`]) get the same calls, but nothing fills on them and every order
/// submitted is rejected.
pub trait Strategy: Send {
    fn on_start(&mut self, _ctx: &mut Context) {}
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle);
    fn on_fill(&mut self, _ctx: &mut Context, _fill: &Fill) {}
}

/// What a strategy can see and do during a callback.
pub struct Context<'a> {
    pub(crate) bars: &'a [Candle],
    /// The current bar's ask candle, for a market with an ask series.
    pub(crate) ask: Option<Candle>,
    pub(crate) instrument: &'a Instrument,
    pub(crate) timeframe: Timeframe,
    pub(crate) fees: FeeSchedule,
    pub(crate) slippage_bps: f64,
    pub(crate) max_leverage: f64,
    pub(crate) portfolio: &'a Portfolio,
    /// The funding events fed with the bars so far, oldest first; see [`Context::funding`].
    pub(crate) funding: &'a [(Millis, f64)],
    pub(crate) broker: &'a mut dyn Broker,
    pub(crate) orders: &'a mut Vec<Order>,
    pub(crate) warming_up: bool,
    /// The reason the strategy gave to [`Context::fail`], if it did.
    pub(crate) failure: &'a mut Option<String>,
}

/// The reason orders submitted on a warm-up bar are rejected with.
const WARM_UP: &str = "warm-up: trading is disabled";

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

    /// The fees fills are charged: the session's override, or else the instrument's own.
    pub fn fees(&self) -> FeeSchedule {
        self.fees
    }

    /// The slippage the session applies to market and stop fills, in basis points of price.
    pub fn slippage_bps(&self) -> f64 {
        self.slippage_bps
    }

    /// The largest position the session allows on a margined kind, as its notional over equity;
    /// see [`Context::submit`]. A kind without margin, such as spot, ignores it.
    pub fn max_leverage(&self) -> f64 {
        self.max_leverage
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

    /// The account's equity, in the account currency.
    pub fn equity(&self) -> f64 {
        self.portfolio.equity()
    }

    /// `quote_amount`, in the instrument's quote currency, in the account currency at the current
    /// bar's close (the bid close on a market with quotes); the amount itself on a quote account.
    /// A strategy sizes a trade with it as `risk_fraction * ctx.equity() /
    /// ctx.to_account(stop_distance)`: on USD/JPY at 150.00 with 100,000 dollars of equity, 0.5%
    /// risk on a 0.12 yen (12-pip) stop is `500 / (0.12 / 150) = 625,000` units.
    pub fn to_account(&self, quote_amount: f64) -> f64 {
        self.portfolio
            .account()
            .from_quote(quote_amount, self.bar().close)
    }

    /// The funding rates settled on this instrument so far, as `(time, rate)`, oldest first: every
    /// event fed to the session with a bar, warm-up bars included. From the first live bar on,
    /// that is the series the cost model charges (a rate charged on a flat position is still
    /// here); the warm-up bars' events, and those in a gap between the last warm-up bar and the
    /// first live bar, are only history, since nothing is held before the first live bar. In a
    /// backtest the time is the settlement time; a paper session gives a rate published after its
    /// own bar closed with a later bar, at a time inside that bar, and a rate due in a gap before a
    /// bar at that bar's open.
    ///
    /// The series runs from the first bar fed, warm-up or live, which takes no funding from before
    /// its open (see [`crate::Session::on_bar`]), to the last settlement strictly before the
    /// current bar's close, the current bar's `open_time` plus the timeframe: a bar takes the
    /// events in its own window and in the gap before it, and a settlement due exactly at the close
    /// is the next bar's open, so it is visible on the next bar. A backtest feeds every event from
    /// its first warm-up bar's open, and a paper session the venue's rates from its first warm-up
    /// bar. It is empty for a kind without funding (spot, FX).
    pub fn funding(&self) -> &[(Millis, f64)] {
        self.funding
    }

    pub fn open_orders(&self) -> &[Order] {
        self.broker.open_orders()
    }

    /// Stops the run: when this callback returns, the session's [`crate::Session::on_bar`] (or
    /// [`crate::Session::warm_up`]) returns [`crate::EngineError::StrategyFailed`] with `reason`,
    /// so a backtest ends with that error instead of a result. It is for a strategy that cannot
    /// trade on what it is given, such as a timeframe or a date its rules do not cover, where
    /// carrying on would give a run with no trades that looks like a real result. Orders submitted
    /// in the same callback stay recorded but the run never reaches the bar they could fill on.
    /// Only the first reason given on a bar is kept.
    pub fn fail(&mut self, reason: impl Into<String>) {
        if self.failure.is_none() {
            *self.failure = Some(reason.into());
        }
    }

    /// True on a warm-up bar ([`crate::Session::warm_up`]), where every order submitted is
    /// rejected, so a strategy can skip submitting.
    pub fn is_warming_up(&self) -> bool {
        self.warming_up
    }

    /// Rounds the order to the instrument's lot and tick, validates it and sends it to the
    /// broker. Quantities round down; limit prices round in the order's favour-safe direction
    /// (buys down, sells up) so an order never trades worse than asked; triggers round to the
    /// nearest tick. On spot, a sell must fit in the holding less the open sells, and a buy must
    /// fit in the cash together with the open buys: each buy is valued at its limit price, its
    /// trigger, or this bar's close for a market order, plus the taker fee and any per-unit
    /// commission. A spot buy is checked
    /// again when it fills and cut to the cash then (see [`crate::SimBroker`]).
    ///
    /// On a margined kind (perpetual, FX), an order that is not reduce-only is rejected if the
    /// position it could build is worth more than [`Context::max_leverage`] times the equity. A
    /// reduce-only order is never checked, as on a venue, so a strategy can always reduce or close,
    /// even when losses have taken it over the cap. With no position, or on the side of the
    /// position, the position checked is the position plus this order and the open orders on its
    /// side that are not reduce-only. Against the position, this order and the open orders on its
    /// side, reduce-only or not, are always allowed when together they are no larger than the
    /// position, since they can then only shrink it. When they are larger they could flip it: the
    /// worst case is that the reduce-only orders close what they can first and the others open a
    /// fresh position on the other side, and that fresh position is what is capped. The position
    /// is valued at this bar's close, and each
    /// order at its limit price, its trigger, or this bar's close on its side for a market order
    /// (the ask close for a buy on a market with quotes), without the fee. On a base account each
    /// of those values is converted to the account currency at the price it is valued at, so a
    /// position of `q` units of USD/JPY is worth `q` dollars, before it is compared with the
    /// equity.
    ///
    /// shortcut: since a reduce-only order is never checked, one sent after plain orders against
    /// the position (a reduce-only market close after a plain take-profit limit) makes room for
    /// them: if the close fills first, they open a fresh position the check did not count. That
    /// position is no larger than the plain orders, and they passed the check while the position
    /// stood, so they open past the cap by at most the size of the position the close takes away:
    /// the leverage reached is at most the cap plus the position's leverage before the close, up
    /// to twice the cap. Check a plain order's leverage when it fills, against the equity then, if
    /// strategies combine orders that way.
    ///
    /// Every order is rejected on a warm-up bar. A rejected order is recorded with its reason and
    /// returned as an error.
    pub fn submit(&mut self, request: OrderRequest) -> Result<OrderId, OrderRejected> {
        let decided_on = self.bar().open_time;
        self.send(request, None, decided_on).map(|(id, _)| id)
    }

    /// [`Context::submit`] for an order placed from outside the strategy while the bar that
    /// opened at `decided_on` was in progress, at or after this bar: it is checked against this
    /// bar's close, as any order, but first fills on the bar after `decided_on`.
    pub(crate) fn submit_during(
        &mut self,
        request: OrderRequest,
        decided_on: Millis,
    ) -> Result<OrderId, OrderRejected> {
        debug_assert!(decided_on >= self.bar().open_time);
        self.send(request, None, decided_on).map(|(id, _)| id)
    }

    /// Submits an entry with a protective stop attached. When the entry fills, a reduce-only stop
    /// on the other side, for the filled quantity, is placed at `stop_trigger` and checked against
    /// the entry's own bar straight away, with the whole bar range counted as coming after the
    /// fill. So the stop fills on that bar, at its trigger plus slippage, if the bar reaches it,
    /// or at the open if the entry filled at an open that was already through it. A stop
    /// submitted from `on_fill` instead could first fill on the next bar.
    ///
    /// The entry is checked as in [`Context::submit`]. The trigger rounds to the nearest tick and
    /// must be below the entry's price for a buy and above it for a sell, where the entry is
    /// valued at its limit price, its trigger, or this bar's close on its side for a market order
    /// (the ask close for a buy on a market with quotes). A
    /// reduce-only entry or a spot sell cannot have a stop attached.
    ///
    /// Returns the entry's id and the stop's. The stop is recorded as [`OrderStatus::Attached`]
    /// until the entry fills; from then on it is an ordinary open order. Until then it cannot be
    /// cancelled on its own, and cancelling the entry cancels it too.
    pub fn submit_with_stop(
        &mut self,
        request: OrderRequest,
        stop_trigger: f64,
    ) -> Result<(OrderId, OrderId), OrderRejected> {
        let trigger = self.instrument.round_price(stop_trigger, Rounding::Nearest);
        let decided_on = self.bar().open_time;
        let (id, stop) = self.send(request, Some(trigger), decided_on)?;
        Ok((id, stop.expect("an accepted entry has its stop attached")))
    }

    /// Records the order and sends it to the broker if it passes the checks, with a protective
    /// stop attached when `stop_trigger` is set, as decided on the bar that opened at
    /// `submitted_at` (see [`Order::submitted_at`]). Returns the order's id and its stop's.
    fn send(
        &mut self,
        request: OrderRequest,
        stop_trigger: Option<f64>,
        submitted_at: Millis,
    ) -> Result<(OrderId, Option<OrderId>), OrderRejected> {
        let id = OrderId(self.orders.len() as u64 + 1);
        let decision_close = self.close_on(request.side);
        let (request, mut check) = self.prepare(request);
        if self.warming_up {
            check = Err(WARM_UP.into());
        }
        let attached_stop = stop_trigger.map(|trigger| AttachedStop {
            id: OrderId(id.0 + 1),
            trigger,
        });
        if let (Ok(()), Some(stop)) = (&check, attached_stop) {
            check = self.check_stop(&request, stop);
        }
        let status = match &check {
            Ok(()) => OrderStatus::Open,
            Err(reason) => OrderStatus::Rejected(reason.clone()),
        };
        let order = Order {
            id,
            request,
            submitted_at,
            decision_close,
            // A rejected entry places no stop, so none is recorded for it.
            attached_stop: attached_stop.filter(|_| check.is_ok()),
            status,
        };
        self.orders.push(order.clone());
        match check {
            Ok(()) => {
                if let Some(stop) = order.attached_stop {
                    self.orders.push(Order {
                        id: stop.id,
                        request: stop.request(&order.request, order.request.qty),
                        submitted_at,
                        decision_close,
                        attached_stop: None,
                        status: OrderStatus::Attached,
                    });
                }
                let stop = order.attached_stop.map(|stop| stop.id);
                self.broker.submit(order);
                Ok((id, stop))
            }
            Err(reason) => Err(OrderRejected { id, reason }),
        }
    }

    /// Cancels an open order. Returns false if it is not open. Cancelling an entry that has not
    /// filled also cancels its attached stop.
    pub fn cancel(&mut self, id: OrderId) -> bool {
        self.cancel_with(id, "cancelled by the strategy")
    }

    /// [`Context::cancel`] with the reason the order records.
    pub(crate) fn cancel_with(&mut self, id: OrderId, reason: &str) -> bool {
        if !self.broker.cancel(id) {
            return false;
        }
        let mut attached = None;
        if let Some(order) = self.orders.iter_mut().find(|o| o.id == id) {
            order.status = OrderStatus::Cancelled(reason.into());
            attached = order.attached_stop;
        }
        if let Some(stop) = attached
            && let Some(order) = self.orders.iter_mut().find(|o| o.id == stop.id)
        {
            order.status = OrderStatus::Cancelled(ENTRY_CANCELLED.into());
        }
        true
    }

    /// This bar's close on `side`: a buy is decided against the ask, a sell against the bid.
    fn close_on(&self, side: Side) -> f64 {
        match (side, self.ask) {
            (Side::Buy, Some(ask)) => ask.close,
            _ => self.bar().close,
        }
    }

    /// Checks a protective stop for `entry`, which has passed its own checks.
    fn check_stop(&self, entry: &OrderRequest, stop: AttachedStop) -> Result<(), String> {
        let inst = self.instrument;
        if entry.reduce_only {
            return Err("a reduce-only order cannot have a protective stop attached".into());
        }
        if !inst.id.kind.rules().can_short && entry.side == Side::Sell {
            return Err("spot cannot attach a protective stop to a sell: it has no short".into());
        }
        let close = self.bar().close;
        inst.validate_order(&stop.request(entry, entry.qty), close)
            .map_err(|e| format!("protective stop: {e}"))?;
        let price = reference_price(entry.order_type, self.close_on(entry.side));
        let (wrong_side, side) = match entry.side {
            Side::Buy => (stop.trigger >= price, "below"),
            Side::Sell => (stop.trigger <= price, "above"),
        };
        if wrong_side {
            return Err(format!(
                "a protective stop must be {side} its entry: stop at {}, entry at {price}",
                stop.trigger
            ));
        }
        Ok(())
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
        let rules = inst.id.kind.rules();
        if !rules.can_short && req.side == Side::Sell {
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
        if !rules.margin && req.side == Side::Buy {
            // Open buys count against the cash too, so several buys cannot add up to more than it.
            let close = self.bar().close;
            let cost = |r: &OrderRequest| {
                r.qty * reference_price(r.order_type, close) * (1.0 + self.fees.taker)
                    + r.qty * self.fees.per_unit
            };
            let pending: f64 = self
                .open_orders()
                .iter()
                .filter(|o| o.request.side == Side::Buy)
                .map(|o| cost(&o.request))
                .sum();
            let needed = cost(&req) + pending;
            let cash = self.cash();
            if needed > cash * (1.0 + 1e-9) {
                let reason = format!(
                    "spot cannot buy for more than its cash: {needed} needed with the taker fee \
                     and open buys, {cash} available"
                );
                return (req, Err(reason));
            }
        }
        if rules.margin && !req.reduce_only {
            let close = self.bar().close;
            let position = self.position();
            // Every order here is on this order's side, and a market order is valued at its side's
            // close.
            let last = self.close_on(req.side);
            let account = self.portfolio.account();
            // The value of `qty` at `price`, in the account currency.
            let value = |qty: f64, price: f64| account.from_quote(qty * price, price);
            // This order and the open orders on its side, and those of them that are not
            // reduce-only, with their value.
            let side: Vec<&OrderRequest> = self
                .open_orders()
                .iter()
                .map(|o| &o.request)
                .filter(|r| r.side == req.side)
                .chain([&req])
                .collect();
            let total: f64 = side.iter().map(|r| r.qty).sum();
            // The plain orders as (quantity, price), dearest first.
            let mut plain: Vec<(f64, f64)> = side
                .iter()
                .filter(|r| !r.reduce_only)
                .map(|r| (r.qty, reference_price(r.order_type, last)))
                .collect();
            plain.sort_by(|a, b| b.1.total_cmp(&a.1));
            let plain_qty: f64 = plain.iter().map(|(q, _)| q).sum();
            let plain_value: f64 = plain.iter().map(|&(q, p)| value(q, p)).sum();
            let notional = if req.side.sign() * position < 0.0 {
                // Against the position: the fresh position on the other side if it closes first.
                // Any of the orders may be the ones that close it, so the fresh quantity is valued
                // at the dearest of them, which is the largest it can be worth.
                let fresh = plain_qty.min(total - position.abs());
                (total > position.abs() * (1.0 + 1e-9)).then(|| {
                    let mut left = fresh;
                    let mut worth = 0.0;
                    for (qty, price) in &plain {
                        let take = qty.min(left);
                        worth += value(take, *price);
                        left -= take;
                        if left <= 0.0 {
                            break;
                        }
                    }
                    worth
                })
            } else {
                Some((value(position, close) + req.side.sign() * plain_value).abs())
            };
            if let Some(notional) = notional {
                let equity = self.equity();
                let allowed = self.max_leverage * equity;
                if notional > allowed * (1.0 + 1e-9) {
                    let reason = format!(
                        "over the leverage cap of {}x: the position would be worth {notional} with \
                         the open orders on this side, {allowed} allowed on {equity} of equity",
                        self.max_leverage
                    );
                    return (req, Err(reason));
                }
            }
        }
        (req, Ok(()))
    }
}

/// The price an order is valued at before it fills: its limit price, its trigger for a stop, or
/// `last` for a market order. `Instrument::validate_order` values the minimum notional the same
/// way.
fn reference_price(order_type: OrderType, last: f64) -> f64 {
    match order_type {
        OrderType::Market => last,
        OrderType::Limit { price } | OrderType::StopLimit { price, .. } => price,
        OrderType::Stop { trigger } => trigger,
    }
}
