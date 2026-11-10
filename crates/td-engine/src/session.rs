use std::ops::Range;

use serde::{Deserialize, Serialize};
use td_core::{
    Candle, FeeSchedule, Fill, Instrument, InstrumentKind, Liquidity, Millis, OrderId,
    OrderRequest, OrderType, Rounding, Side, TimeInForce, Timeframe,
};

use crate::broker::ENTRY_CANCELLED;
use crate::{
    BarFunding, Broker, BrokerEvent, CandleFillModel, Context, EngineError, Order, OrderStatus,
    Portfolio, SimBroker, Strategy,
};

/// Why [`Session::close_out`] cancels the open orders.
const CLOSED_OUT: &str = "cancelled to close out the position at the end of the run";

/// Why a liquidation cancels the open orders, as a venue does before it liquidates.
const LIQUIDATED: &str = "cancelled by a liquidation";

/// The `client_tag` of the order a liquidation records.
pub const LIQUIDATION_TAG: &str = "liquidation";

/// The maintenance margin of a perpetual position, as a fraction of its notional at the current
/// price: below it the position is liquidated (see [`Session::on_bar`]). 0.5% is close to the
/// first tier on Binance and Bybit for small positions in the largest perpetuals; smaller coins
/// and larger positions have higher rates, so for them this can liquidate later than a venue.
///
/// shortcut: one rate for every instrument and size; carry the venue's tiers on the instrument
/// when strategies trade smaller coins or large size at high leverage.
pub const MAINTENANCE_MARGIN_RATE: f64 = 0.005;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SessionConfig {
    /// Starting balance in the quote asset.
    pub starting_cash: f64,
    /// Slippage applied to market and stop fills, in basis points of price.
    pub slippage_bps: f64,
    /// Replaces the instrument's fee schedule when set.
    pub fees: Option<FeeSchedule>,
    /// The largest perpetual position allowed, as its notional over the account's equity: 1.0
    /// means 1x. Checked when an order is submitted (see [`Context::submit`]). Spot ignores it.
    pub max_leverage: f64,
}

/// Equity at the close of the bar that opened at `time`.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EquityPoint {
    pub time: Millis,
    pub equity: f64,
}

/// One strategy trading one instrument on one timeframe, fed one closed bar at a time.
///
/// shortcut: one instrument and one timeframe per session; upgrade when portfolio or
/// multi-timeframe strategies are needed.
pub struct Session {
    strategy: Box<dyn Strategy>,
    instrument: Instrument,
    timeframe: Timeframe,
    fees: FeeSchedule,
    slippage_bps: f64,
    max_leverage: f64,
    broker: Box<dyn Broker>,
    portfolio: Portfolio,
    bars: Vec<Candle>,
    orders: Vec<Order>,
    fills: Vec<Fill>,
    equity: Vec<EquityPoint>,
    bar_funding: Vec<BarFunding>,
}

impl Session {
    /// A session on the simulated broker with the candle fill model.
    pub fn new(
        strategy: Box<dyn Strategy>,
        instrument: Instrument,
        timeframe: Timeframe,
        config: SessionConfig,
    ) -> Result<Session, EngineError> {
        if !(config.starting_cash.is_finite() && config.starting_cash >= 0.0) {
            return Err(EngineError::InvalidConfig(
                "starting cash must be a finite number, zero or more",
            ));
        }
        if !(config.slippage_bps.is_finite() && (0.0..10_000.0).contains(&config.slippage_bps)) {
            return Err(EngineError::InvalidConfig(
                "slippage must be at least 0 and below 10000 basis points",
            ));
        }
        if !(config.max_leverage.is_finite() && config.max_leverage > 0.0) {
            return Err(EngineError::InvalidConfig(
                "max leverage must be a finite number above zero",
            ));
        }
        let fees = config.fees.unwrap_or(instrument.fees);
        if !(fees.maker.is_finite() && fees.taker.is_finite()) {
            return Err(EngineError::InvalidConfig("fees must be finite numbers"));
        }
        let model = CandleFillModel {
            slippage_bps: config.slippage_bps,
        };
        let broker = SimBroker::new(instrument.clone(), fees, model);
        Ok(Session {
            strategy,
            portfolio: Portfolio::new(instrument.id.kind, config.starting_cash),
            instrument,
            timeframe,
            fees,
            slippage_bps: config.slippage_bps,
            max_leverage: config.max_leverage,
            broker: Box::new(broker),
            bars: Vec::new(),
            orders: Vec::new(),
            fills: Vec::new(),
            equity: Vec::new(),
            bar_funding: Vec::new(),
        })
    }

    /// Feeds the next closed bar, with the funding events (`(time, rate)`) that fall inside it or
    /// in the gap since the previous bar.
    ///
    /// The bar must be valid, on a timeframe boundary and later than the previous bar; gaps are
    /// allowed. On an error nothing changes. Then, in order: funding due at the bar's open, or in
    /// the gap between the previous bar's end and this bar's open, is charged on the position held
    /// going into the bar, at the open price; open orders are checked against this bar; a
    /// perpetual position is checked for liquidation; funding due later in the bar is charged on
    /// the position after this bar's fills, at the close; equity is marked at the close; and the
    /// strategy is called. The first bar fed takes no funding from before its open.
    ///
    /// Venues usually charge funding at 00:00, 08:00 and 16:00 UTC, which are bar open times on
    /// every timeframe shorter than a day.
    ///
    /// Liquidation: a perpetual position is liquidated when its equity at the bar's adverse
    /// extreme (the high for a short, the low for a long), less any funding it owes later in the
    /// bar, falls below [`MAINTENANCE_MARGIN_RATE`] of its notional at that price. The market is
    /// taken to move from the previous bar to the open, trading through the resting limits the
    /// bar opens through on the way (see [`crate::SimBroker`]), so the position is first checked
    /// on that way: before each of those fills, and before the fills at the open, it is
    /// liquidated if its liquidation price lies strictly before that fill's limit, or the open,
    /// on the market's way there. Only the stops the bar opens through
    /// whose triggers the market reached before the liquidation price (a sell stop at or above it
    /// for a long, a buy stop at or below it for a short) fill before it, and the check is made
    /// again after each. Then it is checked on the position held after the fills at the bar's
    /// open, and last on the position after all of the bar's fills. The path inside the bar is
    /// unknown, so the check after the fills at the open comes before the fills inside the range,
    /// as stops come before limits, except for the stops the price must cross on its way from the
    /// open to the liquidation price: those fill first, nearest the open first, and the check is
    /// made again after each. Whenever it liquidates before some of the bar's fills, those fills
    /// do not happen and their orders are cancelled with the rest. The
    /// liquidation price is where equity and maintenance meet, rounded to the tick on the side
    /// that triggers sooner; if the bar opened beyond it, the open. Every open order is cancelled,
    /// and the whole position is closed at that price with no slippage, recorded as a filled
    /// reduce-only order tagged [`LIQUIDATION_TAG`] whose fill closes the trade in the trade list.
    /// The fill's fee is the taker fee plus a liquidation fee of whatever equity is left after it,
    /// since a venue keeps the rest of the margin, so the account ends flat at exactly zero and
    /// the run goes on with nothing to trade. A perpetual account left flat with a negative
    /// balance (a gap past the liquidation price, or a stop that filled beyond it) is floored at
    /// zero, so equity never goes below zero; see [`Portfolio::shortfall_written_off`].
    ///
    /// This is a pessimistic simplification of a venue's cross margin with one position: the
    /// whole bar range counts as coming after the fills each check follows, as for attached
    /// stops. So a position opened inside the bar is checked against the part of the range before
    /// it too, and when its attached stop fills inside the same bar, beyond the liquidation price,
    /// it fills at its own, worse, price; the floor keeps that loss to the equity.
    pub fn on_bar(&mut self, bar: Candle, funding: &[(Millis, f64)]) -> Result<(), EngineError> {
        self.check_bar(&bar, funding)?;

        // Funding due at the open, or in the gap before it, is owed by the position held going
        // into the bar, before this bar's fills.
        let mut charged = BarFunding::default();
        for &(time, rate) in funding {
            if time <= bar.open_time {
                charged.at_open += self.portfolio.charge_funding(rate, bar.open);
            }
        }
        let first_new_fill = self.fills.len();
        let instrument = &self.instrument;
        let breached = |book: &Portfolio| liquidation_trigger(instrument, book, &bar, funding);
        for event in self.broker.on_bar(&bar, &self.portfolio, &breached) {
            match event {
                BrokerEvent::Filled(fill) => {
                    self.order_mut(fill.order_id).status = OrderStatus::Filled;
                    self.portfolio
                        .apply_fill(fill.side, fill.qty, fill.price, fill.fee);
                    self.fills.push(fill);
                }
                BrokerEvent::Cancelled { id, reason } => {
                    self.order_mut(id).status = OrderStatus::Cancelled(reason);
                }
                BrokerEvent::Placed(order) => {
                    let id = order.id;
                    *self.order_mut(id) = order;
                }
            }
        }
        self.liquidate(&bar, funding);
        self.portfolio.write_off_shortfall();
        // shortcut: funding due later in the bar is charged on the position after all of this
        // bar's fills, at the close, since the candle model does not know when a fill happened
        // inside the bar or what the price was at the funding time. Upgrade with a trade-level
        // fill model that times each fill.
        for &(time, rate) in funding {
            if time > bar.open_time {
                charged.in_bar += self.portfolio.charge_funding(rate, bar.close);
            }
        }
        self.bar_funding.push(charged);
        self.portfolio.mark(bar.close);
        self.equity.push(EquityPoint {
            time: bar.open_time,
            equity: self.portfolio.equity(),
        });
        self.bars.push(bar);
        self.call_strategy(&bar, first_new_fill, false);
        Ok(())
    }

    /// Feeds a closed bar that only adds to the strategy's history, before the first live bar.
    /// Walk-forward tests and paper trading use it to give a strategy the history its indicators
    /// need before it trades.
    ///
    /// The bar and its funding events are checked as in [`Session::on_bar`], the bar joins the
    /// history, and the strategy is called on it the same way, with `on_start` on the first bar fed
    /// whether that is a warm-up bar or a live one. But trading is disabled: every order the
    /// strategy submits is rejected (see [`Context::is_warming_up`]), nothing fills, no funding is
    /// charged, no equity point or bar funding is recorded and the portfolio does not change. It is an error once
    /// a bar has been fed with `on_bar`.
    pub fn warm_up(&mut self, bar: Candle, funding: &[(Millis, f64)]) -> Result<(), EngineError> {
        // Every live bar records an equity point.
        if !self.equity.is_empty() {
            return Err(EngineError::WarmUpAfterLive);
        }
        self.check_bar(&bar, funding)?;
        self.bars.push(bar);
        self.call_strategy(&bar, self.fills.len(), true);
        Ok(())
    }

    /// Closes out at the end of a run, after the second-to-last bar: cancels every open order
    /// (and the attached stops of entries that have not filled) and, if a position is open,
    /// submits a reduce-only market order for all of it, tagged `close-out`. The order goes
    /// through the same checks as a strategy's and fills on the next bar's open like any market
    /// order, with slippage and the taker fee, so the exit is a real fill decided only from what
    /// is known at this close. Orders the strategy submits after this are not cancelled. Does
    /// nothing before the first live bar, when nothing can be open.
    ///
    /// shortcut: a position too small for the instrument's minimums (a spot dust holding below
    /// the minimum notional) has its close-out rejected and stays open; size the close from the
    /// venue's dust rules if that matters.
    pub(crate) fn close_out(&mut self) {
        if self.equity.is_empty() {
            return;
        }
        self.cancel_open_orders(CLOSED_OUT);
        let position = self.portfolio.position();
        if position == 0.0 {
            return;
        }
        let request = OrderRequest {
            instrument: self.instrument.id.clone(),
            side: if position > 0.0 {
                Side::Sell
            } else {
                Side::Buy
            },
            qty: position.abs(),
            order_type: OrderType::Market,
            tif: TimeInForce::Gtc,
            reduce_only: true,
            client_tag: Some("close-out".into()),
        };
        // A rejected close-out is recorded with its reason, and the position stays open.
        let _ = self.context().submit(request);
    }

    /// Submits an order from outside the strategy, such as a person's manual order, between bars.
    /// It is taken as decided at the close of the last bar fed, exactly like an order the strategy
    /// submits from that bar's callbacks: rounded, validated and checked against the spot cash and
    /// holding and the leverage cap as [`Context::submit`] describes, and first able to fill on
    /// the next bar, a market order at its open.
    ///
    /// Unlike a strategy's, a rejected order is not recorded: the reason is returned and nothing
    /// changes, so the next order takes the id this one would have had. It is refused before the
    /// first live bar, while trading is disabled or no bar has been fed. Returns the order as
    /// recorded.
    pub fn submit(&mut self, request: OrderRequest) -> Result<Order, String> {
        if self.equity.is_empty() {
            return Err("orders can be placed once the session has fed its first live bar".into());
        }
        let mut ctx = self.context();
        match ctx.submit(request) {
            Ok(id) => Ok(self.order_mut(id).clone()),
            Err(rejected) => {
                // Context records the rejected order last; it is taken back out.
                let taken = self.orders.pop();
                debug_assert_eq!(taken.map(|o| o.id), Some(rejected.id));
                Err(rejected.reason)
            }
        }
    }

    /// Cancels an open order from outside the strategy, between bars, recording `reason`, as
    /// [`Context::cancel`] does. Returns false if it is not open.
    pub fn cancel(&mut self, id: OrderId, reason: &str) -> bool {
        self.context().cancel_with(id, reason)
    }

    /// A strategy context on the last bar fed, with trading enabled. There must be a bar.
    fn context(&mut self) -> Context<'_> {
        Context {
            bars: &self.bars,
            instrument: &self.instrument,
            timeframe: self.timeframe,
            fees: self.fees,
            slippage_bps: self.slippage_bps,
            max_leverage: self.max_leverage,
            portfolio: &self.portfolio,
            broker: &mut *self.broker,
            orders: &mut self.orders,
            warming_up: false,
        }
    }

    /// Cancels every open order, and the attached stops of entries that have not filled, with
    /// `reason`.
    fn cancel_open_orders(&mut self, reason: &str) {
        for order in self.broker.open_orders().to_vec() {
            self.broker.cancel(order.id);
            self.order_mut(order.id).status = OrderStatus::Cancelled(reason.into());
            if let Some(stop) = order.attached_stop
                && self.order_mut(stop.id).status == OrderStatus::Attached
            {
                self.order_mut(stop.id).status = OrderStatus::Cancelled(ENTRY_CANCELLED.into());
            }
        }
    }

    /// Liquidates the perpetual position after `bar`'s fills if the bar's adverse extreme breaches
    /// the maintenance margin, as [`Session::on_bar`] describes. `funding` is the bar's funding.
    fn liquidate(&mut self, bar: &Candle, funding: &[(Millis, f64)]) {
        let Some(trigger) = liquidation_trigger(&self.instrument, &self.portfolio, bar, funding)
        else {
            return;
        };
        let q = self.portfolio.position();
        let long = q > 0.0;
        // At the trigger, or at the open if the bar opened past it.
        let price = if long {
            bar.open.min(trigger)
        } else {
            bar.open.max(trigger)
        };
        self.cancel_open_orders(LIQUIDATED);
        let (side, qty) = (if long { Side::Sell } else { Side::Buy }, q.abs());
        let id = OrderId(self.orders.len() as u64 + 1);
        self.orders.push(Order {
            id,
            request: OrderRequest {
                instrument: self.instrument.id.clone(),
                side,
                qty,
                order_type: OrderType::Market,
                tif: TimeInForce::Ioc,
                reduce_only: true,
                client_tag: Some(LIQUIDATION_TAG.into()),
            },
            // Decided and filled on this bar, at the liquidation price.
            submitted_at: bar.open_time,
            decision_close: price,
            attached_stop: None,
            status: OrderStatus::Filled,
        });
        // The taker fee, then a liquidation fee of whatever equity is left after it: the venue
        // keeps the rest of the margin. None is left when the bar gapped through the price.
        let taker = price * qty * self.fees.taker;
        let left = self.portfolio.cash() + q * (price - self.portfolio.avg_entry()) - taker;
        let fee = taker + left.max(0.0);
        self.portfolio.apply_fill(side, qty, price, fee);
        if left >= 0.0 {
            // Exactly nothing is left, whatever float residue the fee's arithmetic leaves.
            self.portfolio.clear_cash();
        }
        self.fills.push(Fill {
            order_id: id,
            instrument: self.instrument.id.clone(),
            side,
            time: bar.open_time,
            price,
            qty,
            fee,
            liquidity: Liquidity::Taker,
        });
    }

    /// Checks that `bar` can be fed next, with `funding` inside it or in the gap before it.
    fn check_bar(&self, bar: &Candle, funding: &[(Millis, f64)]) -> Result<(), EngineError> {
        bar.validate()?;
        if self.timeframe.floor(bar.open_time) != bar.open_time {
            return Err(EngineError::MisalignedBar {
                time: bar.open_time,
                timeframe: self.timeframe,
            });
        }
        if let Some(prev) = self.bars.last() {
            if bar.open_time == prev.open_time {
                return Err(EngineError::DuplicateBar {
                    time: bar.open_time,
                });
            }
            if bar.open_time < prev.open_time {
                return Err(EngineError::OutOfOrderBar {
                    prev: prev.open_time,
                    got: bar.open_time,
                });
            }
        }
        let step = self.timeframe.duration_ms();
        let end = bar.open_time + step;
        // From the previous bar's end, so funding due in a gap is charged at this bar's open.
        let start = self
            .bars
            .last()
            .map_or(bar.open_time, |prev| prev.open_time + step);
        for &(time, rate) in funding {
            if self.instrument.id.kind == InstrumentKind::Spot {
                return Err(EngineError::FundingOnSpot);
            }
            if !(start..end).contains(&time) {
                return Err(EngineError::FundingOutsideBar {
                    time,
                    bar_open: bar.open_time,
                });
            }
            if !rate.is_finite() {
                return Err(EngineError::InvalidFundingRate { time, rate });
            }
        }
        Ok(())
    }

    /// Calls the strategy on `bar`, which has just been pushed: `on_start` on the first bar fed,
    /// `on_fill` for each fill from `first_new_fill` on, then `on_bar`.
    fn call_strategy(&mut self, bar: &Candle, first_new_fill: usize, warming_up: bool) {
        let mut ctx = Context {
            bars: &self.bars,
            instrument: &self.instrument,
            timeframe: self.timeframe,
            fees: self.fees,
            slippage_bps: self.slippage_bps,
            max_leverage: self.max_leverage,
            portfolio: &self.portfolio,
            broker: &mut *self.broker,
            orders: &mut self.orders,
            warming_up,
        };
        if ctx.bars.len() == 1 {
            self.strategy.on_start(&mut ctx);
        }
        for fill in &self.fills[first_new_fill..] {
            self.strategy.on_fill(&mut ctx, fill);
        }
        self.strategy.on_bar(&mut ctx, bar);
    }

    fn order_mut(&mut self, id: OrderId) -> &mut Order {
        // Ids are assigned in submission order starting at 1.
        &mut self.orders[id.0 as usize - 1]
    }

    pub fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    pub fn timeframe(&self) -> Timeframe {
        self.timeframe
    }

    pub fn bars(&self) -> &[Candle] {
        &self.bars
    }

    /// Equity at each live bar's close, one point per bar fed with [`Session::on_bar`]. Warm-up
    /// bars record none, so the series starts at the first live bar.
    pub fn equity(&self) -> &[EquityPoint] {
        &self.equity
    }

    /// The funding charged on each live bar, split into the part charged at the open and the
    /// part charged after the bar's fills; one entry per bar fed with [`Session::on_bar`], like
    /// [`Session::equity`].
    pub fn bar_funding(&self) -> &[BarFunding] {
        &self.bar_funding
    }

    pub fn fills(&self) -> &[Fill] {
        &self.fills
    }

    /// Every order submitted, rejected ones included, in submission order.
    pub fn orders(&self) -> &[Order] {
        &self.orders
    }

    pub fn open_orders(&self) -> &[Order] {
        self.broker.open_orders()
    }

    pub fn portfolio(&self) -> &Portfolio {
        &self.portfolio
    }
}

/// The liquidation price of `book`'s perpetual position, where its equity meets maintenance
/// rounded to the tick on the side that triggers sooner, if `bar` reaches it, or `None` if it does
/// not; see [`Session::on_bar`]. The bar may open past it. `funding` is the bar's funding.
fn liquidation_trigger(
    inst: &Instrument,
    book: &Portfolio,
    bar: &Candle,
    funding: &[(Millis, f64)],
) -> Option<f64> {
    let q = book.position();
    if inst.id.kind != InstrumentKind::Perpetual || q == 0.0 {
        return None;
    }
    // Funding the position will pay later in this bar, at the close.
    let owed: f64 = funding
        .iter()
        .filter(|&&(time, _)| time > bar.open_time)
        .map(|&(_, rate)| q * bar.close * rate)
        .sum();
    let cash = book.cash() - owed.max(0.0);
    // Equity at price p is cash + q * (p - entry); maintenance is m * |q| * p.
    let m = MAINTENANCE_MARGIN_RATE;
    let at = (q * book.avg_entry() - cash) / (q - m * q.abs());
    if q > 0.0 {
        // At or below zero, no price can liquidate the long.
        if at <= 0.0 {
            return None;
        }
        let trigger = inst.round_price(at, Rounding::Up);
        (bar.low <= trigger).then_some(trigger)
    } else {
        // At or below zero, the short is below maintenance at any price.
        let trigger = inst.round_price(at.max(0.0), Rounding::Down);
        (bar.high >= trigger).then_some(trigger)
    }
}

/// Runs a strategy over stored bars by feeding them to a new [`Session`] one at a time.
///
/// `funding` is a series of `(time, rate)`, in any order; each event is charged on the bar whose
/// time window contains it, an event in a gap between bars at the next bar's open, and events
/// before the first bar or from the last bar's end on are not charged.
pub fn run_backtest(
    strategy: Box<dyn Strategy>,
    instrument: Instrument,
    timeframe: Timeframe,
    bars: &[Candle],
    funding: &[(Millis, f64)],
    config: SessionConfig,
) -> Result<Session, EngineError> {
    let funding = sorted_funding(funding);
    let ranges = funding_ranges(timeframe, bars, &funding);
    let mut session = Session::new(strategy, instrument, timeframe, config)?;
    for (bar, range) in bars.iter().zip(ranges) {
        session.on_bar(*bar, &funding[range])?;
    }
    Ok(session)
}

pub(crate) fn sorted_funding(funding: &[(Millis, f64)]) -> Vec<(Millis, f64)> {
    let mut funding = funding.to_vec();
    funding.sort_by_key(|&(time, _)| time);
    funding
}

/// For each bar, the range of `funding` (sorted by time) that [`Session::on_bar`] takes with it:
/// the events inside the bar's time window and, from the second bar on, those in the gap since the
/// previous bar's end. Events before the first bar or from the last bar's end on belong to no
/// range.
pub(crate) fn funding_ranges(
    timeframe: Timeframe,
    bars: &[Candle],
    funding: &[(Millis, f64)],
) -> Vec<Range<usize>> {
    let mut next = 0;
    if let Some(first) = bars.first() {
        while next < funding.len() && funding[next].0 < first.open_time {
            next += 1;
        }
    }
    bars.iter()
        .map(|bar| {
            let end = bar.open_time + timeframe.duration_ms();
            let start = next;
            while next < funding.len() && funding[next].0 < end {
                next += 1;
            }
            start..next
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use td_core::{InstrumentId, Venue};

    use super::*;

    /// Shorts 1 at bar 0's close, then at bar 1's close rests a limit buy far below and a limit
    /// sell far above with a stop attached, neither of which fills.
    struct ShortThenRest;

    impl Strategy for ShortThenRest {
        fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
            let req = |side, qty, order_type| OrderRequest {
                instrument: ctx.instrument().id.clone(),
                side,
                qty,
                order_type,
                tif: TimeInForce::Gtc,
                reduce_only: false,
                client_tag: None,
            };
            match ctx.bars().len() {
                1 => {
                    ctx.submit(req(Side::Sell, 1.0, OrderType::Market)).unwrap();
                }
                2 => {
                    let low = req(Side::Buy, 1.0, OrderType::Limit { price: 50.0 });
                    let high = req(Side::Sell, 1.0, OrderType::Limit { price: 150.0 });
                    ctx.submit(low).unwrap();
                    ctx.submit_with_stop(high, 160.0).unwrap();
                }
                _ => {}
            }
        }
    }

    fn bar(i: i64, open: f64) -> Candle {
        Candle {
            open_time: i * 60_000,
            open,
            high: open + 1.0,
            low: open - 1.0,
            close: open,
            volume: 1.0,
            quote_volume: open,
        }
    }

    fn perp() -> Instrument {
        Instrument {
            id: InstrumentId {
                venue: Venue::Binance,
                symbol: "TESTUSDT".into(),
                kind: InstrumentKind::Perpetual,
            },
            base: "TEST".into(),
            quote: "USDT".into(),
            tick_size: 0.01,
            lot_size: 0.001,
            min_qty: 0.001,
            min_notional: 1.0,
            fees: FeeSchedule {
                maker: 0.001,
                taker: 0.002,
            },
        }
    }

    /// Never trades.
    struct Idle;

    impl Strategy for Idle {
        fn on_bar(&mut self, _ctx: &mut Context, _bar: &Candle) {}
    }

    #[test]
    fn orders_from_outside_the_strategy_are_checked_like_its_own_and_fill_on_the_next_bar() {
        let config = SessionConfig {
            starting_cash: 10_000.0,
            slippage_bps: 0.0,
            fees: None,
            max_leverage: 1.0,
        };
        let mut s = Session::new(Box::new(Idle), perp(), Timeframe::M1, config).unwrap();
        let req = |side, qty, order_type| OrderRequest {
            instrument: perp().id,
            side,
            qty,
            order_type,
            tif: TimeInForce::Gtc,
            reduce_only: false,
            client_tag: None,
        };
        // Refused until the first live bar, warm-up bars included.
        assert!(s.submit(req(Side::Buy, 1.0, OrderType::Market)).is_err());
        s.warm_up(bar(0, 100.0), &[]).unwrap();
        assert!(s.submit(req(Side::Buy, 1.0, OrderType::Market)).is_err());
        s.on_bar(bar(1, 100.0), &[]).unwrap();

        // Rejections give the engine's reason and record nothing.
        let reason = s
            .submit(req(Side::Buy, 0.0004, OrderType::Market))
            .unwrap_err();
        assert!(reason.contains("qty"), "{reason}");
        let reason = s
            .submit(req(Side::Buy, 101.0, OrderType::Market))
            .unwrap_err();
        assert!(reason.contains("leverage cap"), "{reason}");
        assert!(s.orders().is_empty());

        // Rounded as a strategy's order is: the quantity down, a buy limit down.
        let market = s
            .submit(req(Side::Buy, 1.0004, OrderType::Market))
            .unwrap();
        assert_eq!((market.id, market.request.qty), (OrderId(1), 1.0));
        assert_eq!((market.submitted_at, market.decision_close), (60_000, 100.0));
        let limit = s
            .submit(req(Side::Buy, 1.0, OrderType::Limit { price: 90.009 }))
            .unwrap();
        assert_eq!(limit.request.order_type, OrderType::Limit { price: 90.0 });
        assert_eq!(s.open_orders().len(), 2);

        // The market order fills at the next bar's open; the limit rests.
        s.on_bar(bar(2, 105.0), &[]).unwrap();
        assert_eq!(s.fills().len(), 1);
        assert_eq!((s.fills()[0].order_id, s.fills()[0].price), (OrderId(1), 105.0));
        assert!(s.cancel(OrderId(2), "cancelled by hand"));
        assert_eq!(
            s.orders()[1].status,
            OrderStatus::Cancelled("cancelled by hand".into())
        );
        assert!(!s.cancel(OrderId(2), "again"), "already cancelled");
        assert!(!s.cancel(OrderId(1), "filled"), "already filled");
        assert!(s.open_orders().is_empty());
    }

    #[test]
    fn closing_out_cancels_open_orders_and_closes_the_position_at_the_next_open() {
        let inst = Instrument {
            id: InstrumentId {
                venue: Venue::Binance,
                symbol: "TESTUSDT".into(),
                kind: InstrumentKind::Perpetual,
            },
            base: "TEST".into(),
            quote: "USDT".into(),
            tick_size: 0.01,
            lot_size: 0.001,
            min_qty: 0.001,
            min_notional: 1.0,
            fees: FeeSchedule {
                maker: 0.001,
                taker: 0.002,
            },
        };
        let config = SessionConfig {
            starting_cash: 10_000.0,
            slippage_bps: 10.0,
            fees: None,
            max_leverage: 1.0,
        };
        let mut s = Session::new(Box::new(ShortThenRest), inst, Timeframe::M1, config).unwrap();
        for i in 0..3 {
            s.on_bar(bar(i, 100.0 + i as f64), &[]).unwrap();
        }
        assert_eq!(s.portfolio().position(), -1.0);
        s.close_out();
        assert_eq!(s.open_orders().len(), 1);
        let status = |id: u64| s.orders()[id as usize - 1].status.clone();
        // Orders 2 and 3 are the resting limits, 4 the stop attached to 3.
        assert_eq!(status(2), OrderStatus::Cancelled(CLOSED_OUT.into()));
        assert_eq!(status(3), OrderStatus::Cancelled(CLOSED_OUT.into()));
        assert_eq!(status(4), OrderStatus::Cancelled(ENTRY_CANCELLED.into()));
        let close = s.orders()[4].clone();
        assert_eq!(close.status, OrderStatus::Open);
        assert_eq!(close.request.side, Side::Buy);
        assert_eq!(close.request.qty, 1.0);
        assert!(close.request.reduce_only);

        s.on_bar(bar(3, 110.0), &[]).unwrap();
        assert_eq!(s.portfolio().position(), 0.0);
        let fill = s.fills().last().unwrap();
        assert_eq!(fill.order_id, close.id);
        // The open plus 10 bps of slippage against the buy, and the taker fee.
        assert_eq!(fill.price, 110.11);
        assert!((fill.fee - 110.11 * 0.002).abs() < 1e-12);
        assert_eq!(s.orders()[4].status, OrderStatus::Filled);
    }
}
