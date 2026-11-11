use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use td_core::{
    Candle, FeeSchedule, Fill, Instrument, InstrumentKind, Liquidity, Millis, OrderId,
    OrderRequest, OrderType, Side, TimeInForce,
};

use crate::{FillCheck, FillModel, Portfolio};

/// An order the session accepted or rejected, with its request as rounded at submission.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub id: OrderId,
    pub request: OrderRequest,
    /// Open time of the bar the order was decided on. It can first fill on a later bar. For a
    /// strategy's order that is the bar at whose close it was sent; for one placed between bars
    /// from outside the strategy, such as a manual order, it is the bar in progress when it was
    /// accepted, so it never fills on a price from before then.
    pub submitted_at: Millis,
    /// Close of the bar the order was decided on: the last price before it was sent, on the
    /// order's side. For a market with an ask series that is the ask close for a buy and the bid
    /// close for a sell.
    pub decision_close: f64,
    /// The protective stop to place when this order fills, if one was attached.
    pub attached_stop: Option<AttachedStop>,
    pub status: OrderStatus,
}

/// A protective stop attached to an entry with [`crate::Context::submit_with_stop`]. When the
/// entry fills, the broker places a reduce-only stop on the other side, for the filled quantity,
/// under `id`.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
    /// in the order they happened. `portfolio` is the account before the bar's fills. `breached`
    /// gives the price at which an account's position is liquidated, if the bar reaches it. The
    /// account is checked on the market's way to the open, before each fill between the bars and
    /// before the fills at the open, and again after the fills at the open against the bar's
    /// range. Whenever the market reaches the liquidation price first, the stops it crosses before
    /// that price fill first, each followed by a fresh check; if the account is still liquidated,
    /// every fill left on the bar is skipped and its order left open, since the liquidation comes
    /// first and cancels them (see [`crate::Session::on_bar`]).
    ///
    /// `bar` is the bid candle and `ask` the ask candle of the same bar. A buy triggers and fills
    /// on the ask, a sell on the bid; a market without quotes passes its one candle as both.
    fn on_bar(
        &mut self,
        bar: &Candle,
        ask: &Candle,
        portfolio: &Portfolio,
        breached: &dyn Fn(&Portfolio) -> Option<f64>,
    ) -> Vec<BrokerEvent>;
    fn open_orders(&self) -> &[Order];
}

/// A broker that fills orders with a [`FillModel`] and charges fees from a [`FeeSchedule`].
///
/// Each order sees the candle of its own side: a buy is checked against the ask candle and a sell
/// against the bid, so a market buy fills at the ask open, a short's protective stop (a buy stop)
/// triggers on the ask high and a long's (a sell stop) on the bid low. Below, an order's bar, its
/// open and its range mean its own side's. Without an ask series the two are the same candle.
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
///
/// The liquidation check of [`Broker::on_bar`] follows the same path. On its way to the open the
/// market reaches the limits it trades through, in order, and then the open. Before each of them
/// the position held at that point is liquidated first if its liquidation price lies before it on
/// the way: above it for a long, as the market falls, or below it for a short, as it rises. A
/// price exactly at the liquidation price goes to the order. The stops the bar opens through whose
/// triggers lie before the liquidation price (a sell stop at or above it for a long, a buy stop at
/// or below it for a short) were triggered on the way first, so they fill first, in the order the
/// market reached them, at the open as any stop the bar opens through. After the fills at the
/// open, the account is checked against the bar's range: the stops inside the range that the
/// market crosses on its way from the open to the liquidation price fill first, nearest the open
/// first. Whenever the account is still liquidated, the rest of the bar's fills are skipped.
///
/// shortcut: an attached stop of an entry filled between the bars is applied right after its
/// entry even when the market reached the new position's liquidation price first on the way to
/// the open. If equity at the stop's trigger is still above zero, the account keeps up to the
/// maintenance margin (0.5% of notional) that a venue would have kept on liquidating first.
///
/// A spot buy is cut at fill time to the largest quantity, on the lot grid, whose value at the
/// fill price plus its fee fits in the cash left at that point in the bar. The cash check at
/// submission values a market buy at the decision close, so a gap up at the next open could
/// otherwise overdraw the cash. The cut order counts as filled, for the smaller quantity, and the
/// rest is dropped; an attached stop is placed for the quantity that filled. If what the cash
/// covers is below the instrument's minimums, the order is cancelled instead.
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

    /// Fills `order` at `price` on `bar` and applies the fill to `book`. A reduce-only order is
    /// cut to the position it can close, and a spot buy to the cash; the error is the reason the
    /// order is cancelled when nothing is left to fill.
    fn fill(
        &self,
        order: &Order,
        price: f64,
        liquidity: Liquidity,
        bar: &Candle,
        book: &mut Portfolio,
    ) -> Result<Fill, String> {
        let req = &order.request;
        let inst = &self.instrument;
        let mut qty = req.qty;
        if req.reduce_only {
            // How much of the position this order's side can close.
            let reducible = match req.side {
                Side::Buy => -book.position(),
                Side::Sell => book.position(),
            };
            qty = inst.round_qty(qty.min(reducible.max(0.0)));
            if qty <= 0.0 {
                return Err("reduce-only order would not reduce the position".into());
            }
        }
        let rate = match liquidity {
            Liquidity::Maker => self.fees.maker,
            Liquidity::Taker => self.fees.taker,
        };
        if inst.id.kind == InstrumentKind::Spot && req.side == Side::Buy {
            let unit_cost = price * (1.0 + rate) + self.fees.per_unit;
            let cash = book.cash();
            if qty * unit_cost > cash {
                qty = inst.round_qty(cash.max(0.0) / unit_cost);
                // The lot rounding allows a hair of float error upwards; never overdraw.
                if qty * unit_cost > cash {
                    qty = inst.round_qty(qty - inst.lot_size);
                }
                let cut = OrderRequest {
                    qty,
                    order_type: OrderType::Market,
                    ..req.clone()
                };
                if qty <= 0.0 || inst.validate_order(&cut, price).is_err() {
                    return Err(format!(
                        "spot buy cancelled at fill time: {cash} of cash does not cover the \
                         minimum order at {price} with the fee"
                    ));
                }
            }
        }
        let fee = self.fees.fee(price, qty, rate);
        book.apply_fill(req.side, qty, price, fee);
        Ok(Fill {
            order_id: order.id,
            instrument: req.instrument.clone(),
            side: req.side,
            // The candle model knows only the bar a fill happened in, not the moment.
            time: bar.open_time,
            price,
            qty,
            fee,
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
    /// The open of the bar on the order's side.
    open: f64,
}

impl Pending {
    /// The pending fill of `order` if the model filled it, on a side whose bar opened at `open`.
    fn new(order: Order, check: FillCheck, open: f64) -> Option<Pending> {
        match check {
            FillCheck::Fill {
                price,
                liquidity,
                market,
            } => Some(Pending {
                order,
                price,
                liquidity,
                market,
                open,
            }),
            FillCheck::Triggered | FillCheck::NoFill => None,
        }
    }

    /// Whether this is a resting limit that its side's bar opened through, which filled at its
    /// limit between the bars. Only a resting limit fills as a maker.
    fn between_bars(&self) -> bool {
        self.liquidity == Liquidity::Maker && self.market == self.open
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

/// Of `stops`, each an index with the stop's side and the price at which the market triggers it,
/// the index of the one the market reaches first on its way to `at`, the liquidation price of a
/// long position when `long` and of a short one otherwise: the highest sell stop at or above it
/// for a long, the lowest buy stop at or below it for a short. Ties go to the first.
fn first_crossed(
    stops: impl Iterator<Item = (usize, Side, f64)>,
    long: bool,
    at: f64,
) -> Option<usize> {
    stops
        .filter(|&(_, side, level)| {
            if long {
                side == Side::Sell && level >= at
            } else {
                side == Side::Buy && level <= at
            }
        })
        .min_by(|(_, _, a), (_, _, b)| if long { b.total_cmp(a) } else { a.total_cmp(b) })
        .map(|(index, _, _)| index)
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

    fn on_bar(
        &mut self,
        bar: &Candle,
        ask: &Candle,
        portfolio: &Portfolio,
        breached: &dyn Fn(&Portfolio) -> Option<f64>,
    ) -> Vec<BrokerEvent> {
        // Each order's own side of the bar: the ask for a buy, the bid for a sell.
        let side_bar = |side: Side| match side {
            Side::Buy => ask,
            Side::Sell => bar,
        };
        // A scratch copy of the account that each fill moves, so a reduce-only order sees the
        // position and a spot buy the cash that the earlier fills on this bar left.
        let mut book = portfolio.clone();
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
            let own = side_bar(order.request.side);
            let check = self
                .model
                .check(order, first, was_triggered, own, &self.instrument);
            match check {
                FillCheck::Fill { market, .. } => {
                    let fill = Pending::new(order.clone(), check, own.open)
                        .expect("the model filled the order");
                    if market == own.open {
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

        let mut events = Vec::new();
        let mut done = Vec::new();
        let mut placed = Vec::new();
        // Applies one fill and places the order's attached stop, if it has one. Returns the stop's
        // fill when the stop fills on this bar too, or the order's fill at the open when it waits
        // for the open.
        let mut apply = |fill: Pending, book: &mut Portfolio| {
            let between = fill.between_bars();
            // The rest of this bar, from the entry's fill on. An entry that filled between the bars
            // met the market at its limit, and the market went on from there to the open.
            let from = if between { fill.price } else { fill.market };
            let own = side_bar(fill.order.request.side);
            let order = fill.order;
            done.push(order.id);
            let applied = match self.fill(&order, fill.price, fill.liquidity, bar, book) {
                Ok(applied) => applied,
                Err(reason) => {
                    // A reduce-only limit the market passed between the bars while it had nothing
                    // to reduce waits for the open, where the fills at the open can give it a
                    // position. There it takes liquidity like a limit arriving at the open, which
                    // the model prices as an IOC limit: the open with slippage, never past the
                    // limit, as a taker.
                    if between && order.request.reduce_only {
                        let mut arriving = order.clone();
                        arriving.request.tif = TimeInForce::Ioc;
                        let check =
                            self.model
                                .check(&arriving, false, false, own, &self.instrument);
                        if let Some(waiting) = Pending::new(order.clone(), check, own.open) {
                            return After::Wait(waiting);
                        }
                    }
                    cancel(&mut events, &order, &reason);
                    return After::Nothing;
                }
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
            // The rest of the bar on the stop's side, which starts as far from the entry's price as
            // its open is from the entry side's open: the spread at the open.
            let stop_bar = side_bar(stop.request.side);
            let from = from + (stop_bar.open - own.open);
            let rest = Candle {
                open: from,
                high: stop_bar.high.max(from),
                low: stop_bar.low.min(from),
                ..*stop_bar
            };
            let check = self
                .model
                .check(&stop, true, false, &rest, &self.instrument);
            match Pending::new(stop.clone(), check, stop_bar.open) {
                Some(fill) => After::Stop(fill),
                None => {
                    placed.push(stop);
                    After::Nothing
                }
            }
        };
        // Whether a stop's market was reached by the open: at it, or on the way to it from a limit
        // that filled between the bars.
        let by_open = |stop: &Pending| match stop.order.request.side {
            Side::Sell => stop.market >= stop.open,
            Side::Buy => stop.market <= stop.open,
        };
        // Applies one fill, and its attached stop when that fills now: straight after it for a fill
        // inside the range, or when the market reached the stop by the open for a fill at or before
        // the open. Returns what waits for later: a stop for the range, or an order for the open.
        let mut settle =
            |fill: Pending, book: &mut Portfolio, in_range: bool| match apply(fill, book) {
                After::Stop(stop) if in_range || by_open(&stop) => {
                    apply(stop, book);
                    After::Nothing
                }
                after => after,
            };
        // Limits filled between the bars first, farthest from the open first: the market passed
        // them in that order on its way to the open. They are all on one side, since a buy and a
        // sell limit can both rest unfilled only with the sell above the buy, and an open cannot
        // be through both. Then the fills at the open, by id.
        let gap = |fill: &Pending| fill.between_bars().then(|| (fill.price - fill.open).abs());
        at_open.sort_by(|a, b| {
            match (gap(a), gap(b)) {
                (Some(a), Some(b)) => b.total_cmp(&a),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
            .then(a.order.id.cmp(&b.order.id))
        });
        let (between, mut opens): (Vec<_>, Vec<_>) =
            at_open.into_iter().partition(|fill| fill.between_bars());
        let mut between = between.into_iter();
        // Reduce-only limits that waited for the open fill after the other fills at the open, which
        // can give them a position to reduce; with still nothing to reduce they are cancelled.
        let mut waiting = Vec::new();
        // Fills skipped because the liquidation came first; their orders stay open for it.
        let mut skipped = Vec::new();
        let mut liquidated = false;
        // The market's way to the open: each limit it trades through between the bars, then the
        // open. The walk is on the bid, since no kind that is liquidated has an ask series. Before each, the position held then is liquidated if the market reaches its
        // liquidation price first, after the stops the bar opens through that the market triggered
        // before that price, in the order it reached them.
        loop {
            let next = between.next();
            let point = next.as_ref().map_or(bar.open, |fill| fill.price);
            while let Some(at) = breached(&book) {
                let long = book.position() > 0.0;
                if (long && at <= point) || (!long && at >= point) {
                    break;
                }
                let stops = opens.iter().enumerate().filter_map(|(index, fill)| {
                    let req = &fill.order.request;
                    match req.order_type {
                        OrderType::Stop { trigger } | OrderType::StopLimit { trigger, .. } => {
                            Some((index, req.side, trigger))
                        }
                        _ => None,
                    }
                });
                let Some(index) = first_crossed(stops, long, at) else {
                    liquidated = true;
                    break;
                };
                match settle(opens.remove(index), &mut book, false) {
                    After::Stop(stop) => inside.push((0, stop)),
                    After::Wait(fill) => waiting.push(fill),
                    After::Nothing => {}
                }
            }
            if liquidated {
                skipped.extend(next);
                skipped.extend(between);
                break;
            }
            let Some(fill) = next else {
                break;
            };
            match settle(fill, &mut book, false) {
                After::Stop(stop) => inside.push((0, stop)),
                After::Wait(fill) => waiting.push(fill),
                After::Nothing => {}
            }
        }
        if !liquidated {
            for fill in opens.drain(..) {
                match settle(fill, &mut book, false) {
                    After::Stop(stop) => inside.push((0, stop)),
                    After::Wait(fill) => waiting.push(fill),
                    After::Nothing => {}
                }
            }
            for fill in waiting.drain(..) {
                if let After::Stop(stop) = settle(fill, &mut book, false) {
                    inside.push((0, stop));
                }
            }
        }
        inside.sort_by_key(|(priority, fill)| (*priority, fill.order.id));
        // After the fills at the open: on its way from the open to the liquidation price the
        // market crosses the stops that close against it there first, sell stops at or above the
        // price for a long and buy stops at or below it for a short. Take the one nearest the open,
        // then check again, since the fill moves the position and so the liquidation price.
        while !liquidated {
            let Some(at) = breached(&book) else {
                break;
            };
            let long = book.position() > 0.0;
            let stops = inside
                .iter()
                .enumerate()
                .filter(|(_, (priority, _))| *priority == 0)
                .map(|(index, (_, fill))| (index, fill.order.request.side, fill.market));
            let Some(index) = first_crossed(stops, long, at) else {
                liquidated = true;
                break;
            };
            let (_, fill) = inside.remove(index);
            settle(fill, &mut book, true);
        }
        if liquidated {
            skipped.extend(opens);
            skipped.extend(waiting);
            skipped.extend(inside.into_iter().map(|(_, fill)| fill));
        } else {
            for (_, fill) in inside {
                // An entry filled inside the range has its stop applied straight after it.
                settle(fill, &mut book, true);
            }
        }
        // The liquidation comes before the skipped fills. Their orders stay open for it to cancel,
        // including stops just placed for entries filled earlier on the bar.
        for fill in skipped {
            if self.open.iter().any(|o| o.id == fill.order.id) {
                // An order that waited for the open was taken off when it first came up.
                done.retain(|id| *id != fill.order.id);
            } else {
                placed.push(fill.order);
            }
        }
        for index in ioc {
            let order = &self.open[index];
            if !liquidated && !done.contains(&order.id) {
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
