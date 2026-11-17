use td_core::{
    Candle, FeeSchedule, Fill, Instrument, InstrumentId, Millis, OrderId, OrderRequest, OrderType,
    Rounding, Side, Timeframe,
};

use crate::broker::ENTRY_CANCELLED;
use crate::session::Leg;
use crate::{AttachedStop, Broker, Order, OrderRejected, OrderStatus, Portfolio, SeriesRow};

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
    /// Each reference instrument's bars, one slot per bar in `bars`; see [`Context::reference`].
    pub(crate) references: &'a [(InstrumentId, Vec<Option<Candle>>)],
    /// Each series' rows known by the current bar's close, and no later ones; see
    /// [`Context::series`].
    pub(crate) series: &'a [(String, Vec<SeriesRow>)],
    pub(crate) broker: &'a mut dyn Broker,
    pub(crate) orders: &'a mut Vec<Order>,
    pub(crate) warming_up: bool,
    /// The reason the strategy gave to [`Context::fail`], if it did.
    pub(crate) failure: &'a mut Option<String>,
    /// The instruments traded beside the first, in a run with several; empty otherwise.
    pub(crate) others: &'a mut [Leg],
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

    /// The first traded instrument's book, which holds the account's balance.
    pub fn portfolio(&self) -> &Portfolio {
        self.portfolio
    }

    /// The position in [`Context::instrument`]; see [`Context::position_of`] for the other
    /// traded instruments.
    pub fn position(&self) -> f64 {
        self.portfolio.position()
    }

    /// The account's balance.
    pub fn cash(&self) -> f64 {
        self.portfolio.cash()
    }

    /// The account's equity, in the account currency: with several traded instruments, the
    /// balance and every position marked at its instrument's last close.
    pub fn equity(&self) -> f64 {
        if self.others.is_empty() {
            return self.portfolio.equity();
        }
        let others: f64 = self.others.iter().map(|leg| leg.portfolio.equity()).sum();
        self.portfolio.equity() + others
    }

    /// Every instrument the run trades: [`Context::instrument`] first, then those it trades
    /// beside it ([`crate::BacktestConfig::also_traded`]), in the order the run names them.
    pub fn traded(&self) -> impl Iterator<Item = &Instrument> {
        std::iter::once(self.instrument).chain(self.others.iter().map(|leg| &leg.instrument))
    }

    /// The rules of `id`, one of the instruments the run trades: its tick, lot, minimums and
    /// fees. `None` for one it does not trade.
    pub fn traded_instrument(&self, id: &InstrumentId) -> Option<&Instrument> {
        self.traded().find(|inst| inst.id == *id)
    }

    /// The closed bars of `id`, one of the instruments the run trades beside the first, aligned to
    /// [`Context::bars`] as [`Context::reference`] aligns a reference instrument's: one slot per
    /// bar there, warm-up bars included, holding its bar that opens at that bar's open time, or
    /// `None` where it has no bar. Its bars at times the first instrument has none are traded but
    /// not shown here. `None` for the first instrument, whose bars are [`Context::bars`], and for
    /// one the run does not trade.
    pub fn bars_of(&self, id: &InstrumentId) -> Option<&[Option<Candle>]> {
        self.leg(id).map(|leg| leg.slots.as_slice())
    }

    /// The current bar of `id`, one of the instruments the run trades: the bar that closed with
    /// [`Context::bar`]. `None` when it has no bar at this time, or the run does not trade it.
    pub fn bar_of(&self, id: &InstrumentId) -> Option<&Candle> {
        if *id == self.instrument.id {
            return Some(self.bar());
        }
        self.leg(id)?.slots.last()?.as_ref()
    }

    /// The position in `id`, one of the instruments the run trades; `None` for one it does not.
    pub fn position_of(&self, id: &InstrumentId) -> Option<f64> {
        let k = self.index(id)?;
        Some(self.position_at(k))
    }

    /// The funding rates settled on `id`, one of the instruments the run trades, as
    /// [`Context::funding`] shows the first instrument's: every event from the first bar fed to
    /// the last one settled before the current bar's close. A rate due while it has no bar is
    /// charged at its next bar's open. `None` for one the run does not trade.
    pub fn funding_of(&self, id: &InstrumentId) -> Option<&[(Millis, f64)]> {
        if *id == self.instrument.id {
            return Some(self.funding);
        }
        self.leg(id).map(|leg| &leg.funding[..leg.shown])
    }

    /// Whether `id`, one of the instruments the run trades, has had a bar by the current bar's
    /// open, counting the bars given to the run before its range: false before an instrument's
    /// first bar, such as one listed after the run starts. It never tells of a bar still to come.
    /// False for one the run does not trade.
    pub fn has_had_bar(&self, id: &InstrumentId) -> bool {
        if *id == self.instrument.id {
            return true;
        }
        self.leg(id)
            .and_then(|leg| leg.listed_from)
            .is_some_and(|t| t <= self.bar().open_time)
    }

    pub fn has_left(&self, id: &InstrumentId) -> bool {
        self.leg(id)
            .and_then(|leg| leg.left_at)
            .is_some_and(|t| t <= self.bar().open_time)
    }

    /// The open orders of `id`, one of the instruments the run trades; `None` for one it does not.
    pub fn open_orders_of(&self, id: &InstrumentId) -> Option<&[Order]> {
        let k = self.index(id)?;
        Some(self.orders_at(k))
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
    /// first live bar, are only history, since nothing is held before the first live bar. The time
    /// is the settlement time, in a backtest and in a paper or live session alike, so a paper or
    /// live session shows a strategy the series a backtest of the same bars shows it, a rate due in
    /// a gap between bars included. The one difference: a rate the venue has not yet published
    /// when the bar it is due in closes reaches a paper or live session's strategy late, on a later
    /// bar, timed at that bar's open or, when bars are missing before it, at the end of the last
    /// bar fed, and the session warns on the bar it was due in and again when it arrives.
    ///
    /// The series runs from the first bar fed, warm-up or live, which takes no funding from before
    /// its open (see [`crate::Session::on_bar`]), to the last settlement strictly before the
    /// current bar's close, the current bar's `open_time` plus the timeframe: a bar takes the
    /// events in its own window and in the gap before it, and a settlement due exactly at the close
    /// is the next bar's open, so it is visible on the next bar. A backtest feeds every event from
    /// its first warm-up bar's open, and a paper or live session the venue's rates from its first
    /// warm-up bar. It is empty for a kind without funding (spot, FX).
    pub fn funding(&self) -> &[(Millis, f64)] {
        self.funding
    }

    /// The closed bars of `instrument`, one of the reference instruments the run names
    /// ([`crate::BacktestConfig::references`]), aligned to [`Context::bars`]: one slot per bar
    /// there, warm-up bars included, holding the reference bar that opens at that bar's open time,
    /// or `None` where the reference instrument has no bar. Both are on the run's timeframe, so the
    /// last slot is the reference bar that closed with the current bar, and no slot holds a bar
    /// that closes after it. A missing bar stays `None`; it is never filled from the one before. A
    /// reference bar at a time the run has no bar of its own is not shown. `None` when the run does
    /// not name `instrument`, as in a run that names no references.
    ///
    /// A reference instrument is only read: an order for it is rejected, since orders, fills,
    /// costs and the account stay on the traded instruments ([`Context::traded`]). Backtests,
    /// sweeps, out-of-sample and walk-forward runs and the battery supply it; a paper or live
    /// session names none (see the README).
    pub fn reference(&self, instrument: &InstrumentId) -> Option<&[Option<Candle>]> {
        self.references
            .iter()
            .find(|(id, _)| id == instrument)
            .map(|(_, slots)| slots.as_slice())
    }

    /// The rows of `name`, one of the series the run names ([`crate::BacktestConfig::series`]),
    /// known at or before the current bar's close, oldest first: a row joins on the first bar whose
    /// close is at or after its `known_at`, and a row known later is not here. The last row is the
    /// latest value known. A run with one traded instrument starts only when each series has a row
    /// known by the close of its first bar, warm-up included, so this is never empty in a run that
    /// names `name`. A run with several traded instruments
    /// ([`crate::BacktestConfig::also_traded`]) starts when each series has a row known by the
    /// close of its last traded bar, so this is `Some(&[])` until the first row is known. `None`
    /// when the run does not name it; a strategy that needs a series should then stop with
    /// [`Context::fail`].
    ///
    /// Backtests, sweeps, out-of-sample and walk-forward runs and the battery supply series; a
    /// paper or live session refuses to start from a run that names one (see the README).
    pub fn series(&self, name: &str) -> Option<&[SeriesRow]> {
        self.series
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, rows)| rows.as_slice())
    }

    /// The open orders of [`Context::instrument`]; see [`Context::open_orders_of`] for the other
    /// traded instruments.
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
    ///
    /// In a run with several traded instruments the order goes to the instrument it names, and is
    /// rounded, validated, priced and filled on that instrument's rules, bars and fees. An order
    /// for an instrument without a bar at the current bar's time is rejected, since it has no
    /// price to be checked at; one for an instrument the run does not trade, a reference
    /// included, is rejected as with one instrument. The leverage cap is on the account's gross
    /// notional: the position this order could build, as above, plus, for every other traded
    /// instrument, the largest position its own open orders could build on either side, or its
    /// position if larger, each valued at its last close.
    pub fn submit(&mut self, request: OrderRequest) -> Result<OrderId, OrderRejected> {
        let decided_on = self.bar().open_time;
        self.send(request, None, decided_on, false)
            .map(|(id, _)| id)
    }

    /// [`Context::submit`] for the end-of-run close-out ([`crate::Session::close_out`]): an order
    /// for another traded instrument without a bar at the current time is still sent, valued at
    /// its last close.
    pub(crate) fn submit_close_out(
        &mut self,
        request: OrderRequest,
    ) -> Result<OrderId, OrderRejected> {
        let decided_on = self.bar().open_time;
        self.send(request, None, decided_on, true).map(|(id, _)| id)
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
        self.send(request, None, decided_on, false)
            .map(|(id, _)| id)
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
        let inst = self.inst_at(self.index(&request.instrument).unwrap_or(0));
        let trigger = inst.round_price(stop_trigger, Rounding::Nearest);
        let decided_on = self.bar().open_time;
        let (id, stop) = self.send(request, Some(trigger), decided_on, false)?;
        Ok((id, stop.expect("an accepted entry has its stop attached")))
    }

    /// Records the order and sends it to the broker if it passes the checks, with a protective
    /// stop attached when `stop_trigger` is set, as decided on the bar that opened at
    /// `submitted_at` (see [`Order::submitted_at`]). Returns the order's id and its stop's. An
    /// order for another traded instrument without a bar at the current time is rejected unless
    /// `without_bar`.
    fn send(
        &mut self,
        request: OrderRequest,
        stop_trigger: Option<f64>,
        submitted_at: Millis,
        without_bar: bool,
    ) -> Result<(OrderId, Option<OrderId>), OrderRejected> {
        let id = OrderId(self.orders.len() as u64 + 1);
        // An instrument the run does not trade is checked as the first's, which rejects it.
        let k = self.index(&request.instrument).unwrap_or(0);
        let decision_close = self.close_at(k, request.side);
        let (request, mut check) = self.prepare(k, request);
        if k > 0 && !without_bar && self.others[k - 1].slots.last().is_none_or(Option::is_none) {
            check = Err(format!(
                "{} has no bar at {}, so it has no price to check an order at",
                request.instrument,
                self.bar().open_time
            ));
        }
        if self.has_left(&request.instrument) {
            check = Err(format!(
                "{} stopped trading at its last bar, so it takes no more orders",
                request.instrument
            ));
        }
        if self.warming_up {
            check = Err(WARM_UP.into());
        }
        let attached_stop = stop_trigger.map(|trigger| AttachedStop {
            id: OrderId(id.0 + 1),
            trigger,
        });
        if let (Ok(()), Some(stop)) = (&check, attached_stop) {
            check = self.check_stop(k, &request, stop);
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
                if k == 0 {
                    self.broker.submit(order);
                } else {
                    self.others[k - 1].broker.submit(order);
                }
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
        let cancelled =
            self.broker.cancel(id) || self.others.iter_mut().any(|leg| leg.broker.cancel(id));
        if !cancelled {
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

    /// The other traded instrument `id`, if the run trades it beside the first.
    fn leg(&self, id: &InstrumentId) -> Option<&Leg> {
        self.others.iter().find(|leg| leg.instrument.id == *id)
    }

    /// Which traded instrument `id` is: 0 for the first, 1 + its place among the others, or
    /// `None` for one the run does not trade.
    fn index(&self, id: &InstrumentId) -> Option<usize> {
        if *id == self.instrument.id {
            return Some(0);
        }
        self.others
            .iter()
            .position(|leg| leg.instrument.id == *id)
            .map(|i| i + 1)
    }

    fn inst_at(&self, k: usize) -> &Instrument {
        if k == 0 {
            self.instrument
        } else {
            &self.others[k - 1].instrument
        }
    }

    fn position_at(&self, k: usize) -> f64 {
        if k == 0 {
            self.position()
        } else {
            self.others[k - 1].portfolio.position()
        }
    }

    fn fees_at(&self, k: usize) -> FeeSchedule {
        if k == 0 {
            self.fees
        } else {
            self.others[k - 1].instrument.fees
        }
    }

    fn orders_at(&self, k: usize) -> &[Order] {
        if k == 0 {
            self.open_orders()
        } else {
            self.others[k - 1].broker.open_orders()
        }
    }

    /// Traded instrument `k`'s close on `side` at the current bar's time ([`Context::close_on`]
    /// for the first); without a bar there, another instrument's last close.
    fn close_at(&self, k: usize, side: Side) -> f64 {
        if k == 0 {
            return self.close_on(side);
        }
        let leg = &self.others[k - 1];
        match leg.slots.last() {
            Some(Some(bar)) => bar.close,
            _ => leg.last_close.unwrap_or(0.0),
        }
    }

    /// Checks a protective stop for `entry`, an order for traded instrument `k` that has passed
    /// its own checks.
    fn check_stop(&self, k: usize, entry: &OrderRequest, stop: AttachedStop) -> Result<(), String> {
        let inst = self.inst_at(k);
        if entry.reduce_only {
            return Err("a reduce-only order cannot have a protective stop attached".into());
        }
        if !inst.id.kind.rules().can_short && entry.side == Side::Sell {
            return Err("spot cannot attach a protective stop to a sell: it has no short".into());
        }
        let close = self.close_at(k, Side::Sell);
        inst.validate_order(&stop.request(entry, entry.qty), close)
            .map_err(|e| format!("protective stop: {e}"))?;
        let price = reference_price(entry.order_type, self.close_at(k, entry.side));
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

    /// Rounds and checks an order for traded instrument `k`, as [`Context::submit`] describes.
    fn prepare(&self, k: usize, mut req: OrderRequest) -> (OrderRequest, Result<(), String>) {
        let inst = self.inst_at(k);
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
        if let Err(e) = inst.validate_order(&req, self.close_at(k, Side::Sell)) {
            return (req, Err(e.to_string()));
        }
        let rules = inst.id.kind.rules();
        if !rules.can_short && req.side == Side::Sell {
            let pending: f64 = self
                .orders_at(k)
                .iter()
                .filter(|o| o.request.side == Side::Sell)
                .filter(|o| !req.reduce_only || !o.request.reduce_only)
                .map(|o| o.request.qty)
                .sum();
            let available = self.position_at(k) - pending;
            if req.qty > available * (1.0 + 1e-9) {
                let open = if req.reduce_only {
                    "the open sells that are not reduce-only"
                } else {
                    "every open sell, reduce-only ones included"
                };
                let reason = format!(
                    "spot cannot sell more than it holds: selling {}, {} available after {open}",
                    req.qty, available
                );
                return (req, Err(reason));
            }
        }
        if !rules.margin && req.side == Side::Buy {
            // Open buys count against the cash too, so several buys cannot add up to more than it.
            let cost = |j: usize, r: &OrderRequest| {
                let fees = self.fees_at(j);
                let close = self.close_at(j, Side::Buy);
                r.qty * reference_price(r.order_type, close) * (1.0 + fees.taker)
                    + r.qty * fees.per_unit
            };
            let pending: f64 = (0..=self.others.len())
                .flat_map(|j| self.orders_at(j).iter().map(move |o| (j, o)))
                .filter(|(_, o)| o.request.side == Side::Buy)
                .map(|(j, o)| cost(j, &o.request))
                .sum();
            let needed = cost(k, &req) + pending;
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
            let notional = self.side_notional(k, req.side, Some(&req));
            // With several traded instruments the cap is on the gross notional: what the others'
            // positions and open orders could reach comes on top.
            let notional = if self.others.is_empty() {
                notional
            } else {
                notional.map(|n| n + self.others_reach(k))
            };
            if let Some(notional) = notional {
                let equity = self.equity();
                let allowed = self.max_leverage * equity;
                if notional > allowed * (1.0 + 1e-9) {
                    let reason = if self.others.is_empty() {
                        format!(
                            "over the leverage cap of {}x: the position would be worth {notional} \
                             with the open orders on this side, {allowed} allowed on {equity} of \
                             equity",
                            self.max_leverage
                        )
                    } else {
                        format!(
                            "over the leverage cap of {}x: the account's gross notional could \
                             reach {notional} with this order and every traded instrument's \
                             positions and open orders, {allowed} allowed on {equity} of equity",
                            self.max_leverage
                        )
                    };
                    return (req, Err(reason));
                }
            }
        }
        (req, Ok(()))
    }

    /// The largest position traded instrument `k` could build on `side`, valued as
    /// [`Context::submit`] describes, from its open orders on that side and `new`: with no
    /// position or on its side, the position plus the orders that are not reduce-only; against
    /// it, the fresh position on the other side if the orders could flip it, or `None` when they
    /// can only shrink it.
    fn side_notional(&self, k: usize, side: Side, new: Option<&OrderRequest>) -> Option<f64> {
        let close = self.close_at(k, Side::Sell);
        let position = self.position_at(k);
        // Every order here is on this side, and a market order is valued at its side's close.
        let last = self.close_at(k, side);
        let account = self.portfolio.account();
        // The value of `qty` at `price`, in the account currency.
        let value = |qty: f64, price: f64| account.from_quote(qty * price, price);
        // This order and the open orders on its side, and those of them that are not
        // reduce-only, with their value.
        let orders: Vec<&OrderRequest> = self
            .orders_at(k)
            .iter()
            .map(|o| &o.request)
            .filter(|r| r.side == side)
            .chain(new)
            .collect();
        let total: f64 = orders.iter().map(|r| r.qty).sum();
        // The plain orders as (quantity, price), dearest first.
        let mut plain: Vec<(f64, f64)> = orders
            .iter()
            .filter(|r| !r.reduce_only)
            .map(|r| (r.qty, reference_price(r.order_type, last)))
            .collect();
        plain.sort_by(|a, b| b.1.total_cmp(&a.1));
        let plain_qty: f64 = plain.iter().map(|(q, _)| q).sum();
        let plain_value: f64 = plain.iter().map(|&(q, p)| value(q, p)).sum();
        if side.sign() * position < 0.0 {
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
            Some((value(position, close) + side.sign() * plain_value).abs())
        }
    }

    /// What every traded instrument but `k` could reach: for each, the largest of its position
    /// and the positions its open orders could build on either side ([`Context::side_notional`]).
    fn others_reach(&self, k: usize) -> f64 {
        (0..=self.others.len())
            .filter(|&j| j != k)
            .map(|j| {
                let held = self.position_at(j).abs() * self.close_at(j, Side::Sell);
                [Side::Buy, Side::Sell]
                    .into_iter()
                    .filter_map(|side| self.side_notional(j, side, None))
                    .fold(held, f64::max)
            })
            .sum()
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
