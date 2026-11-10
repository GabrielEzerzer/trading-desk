use std::ops::Range;

use serde::{Deserialize, Serialize};
use td_core::{Candle, FeeSchedule, Fill, Instrument, InstrumentKind, Millis, Timeframe};

use crate::{
    BarFunding, Broker, BrokerEvent, CandleFillModel, Context, EngineError, Order, OrderStatus,
    Portfolio, SimBroker, Strategy,
};

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SessionConfig {
    /// Starting balance in the quote asset.
    pub starting_cash: f64,
    /// Slippage applied to market and stop fills, in basis points of price.
    pub slippage_bps: f64,
    /// Replaces the instrument's fee schedule when set.
    pub fees: Option<FeeSchedule>,
}

/// Equity at the close of the bar that opened at `time`.
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
    /// going into the bar, at the open price; open orders are checked against this bar; funding due
    /// later in the bar is charged on the position after this bar's fills, at the close; equity is
    /// marked at the close; and the strategy is called. The first bar fed takes no funding from
    /// before its open.
    ///
    /// Venues usually charge funding at 00:00, 08:00 and 16:00 UTC, which are bar open times on
    /// every timeframe shorter than a day.
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
        for event in self.broker.on_bar(&bar, self.portfolio.position()) {
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

    fn order_mut(&mut self, id: td_core::OrderId) -> &mut Order {
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
