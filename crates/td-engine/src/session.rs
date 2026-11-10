use std::ops::Range;

use serde::{Deserialize, Serialize};
use td_core::{Candle, FeeSchedule, Fill, Instrument, InstrumentKind, Millis, Timeframe};

use crate::{
    Broker, BrokerEvent, CandleFillModel, Context, EngineError, Order, OrderStatus, Portfolio,
    SimBroker, Strategy,
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
    broker: Box<dyn Broker>,
    portfolio: Portfolio,
    bars: Vec<Candle>,
    orders: Vec<Order>,
    fills: Vec<Fill>,
    equity: Vec<EquityPoint>,
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
            broker: Box::new(broker),
            bars: Vec::new(),
            orders: Vec::new(),
            fills: Vec::new(),
            equity: Vec::new(),
        })
    }

    /// Feeds the next closed bar, with the funding events (`(time, rate)`) that fall inside it.
    ///
    /// The bar must be valid, on a timeframe boundary and later than the previous bar; gaps are
    /// allowed. On an error nothing changes. Then, in order: open orders are checked against this
    /// bar, funding is charged, equity is marked at the close, and the strategy is called.
    ///
    /// Funding is charged on the position after this bar's fills, at this bar's close price:
    /// the candle data has no price at the funding moment itself.
    pub fn on_bar(&mut self, bar: Candle, funding: &[(Millis, f64)]) -> Result<(), EngineError> {
        self.check_bar(&bar, funding)?;
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
            }
        }
        for &(_, rate) in funding {
            self.portfolio.charge_funding(rate, bar.close);
        }
        self.portfolio.mark(bar.close);
        self.equity.push(EquityPoint {
            time: bar.open_time,
            equity: self.portfolio.equity(),
        });
        self.bars.push(bar);

        let mut ctx = Context {
            bars: &self.bars,
            instrument: &self.instrument,
            timeframe: self.timeframe,
            portfolio: &self.portfolio,
            broker: &mut *self.broker,
            orders: &mut self.orders,
            warming_up: false,
        };
        if ctx.bars.len() == 1 {
            self.strategy.on_start(&mut ctx);
        }
        for fill in &self.fills[first_new_fill..] {
            self.strategy.on_fill(&mut ctx, fill);
        }
        self.strategy.on_bar(&mut ctx, &bar);
        Ok(())
    }

    /// Feeds a bar the strategy sees while trading is disabled, so its indicators are ready when
    /// trading starts. Warm-up bars come before the first bar fed with [`Session::on_bar`].
    ///
    /// The bar is checked like in `on_bar` and added to the bars the strategy sees, and the
    /// strategy is called (`on_start` on the very first bar, then `on_bar`), with
    /// [`Context::is_warming_up`] true. Orders it submits are refused and not recorded. No equity
    /// point is added and no funding is charged: the position is always zero while warming up, so
    /// the funding events are only checked.
    pub fn warm_up(&mut self, bar: Candle, funding: &[(Millis, f64)]) -> Result<(), EngineError> {
        if !self.equity.is_empty() {
            return Err(EngineError::InvalidConfig(
                "warm-up bars must come before the first traded bar",
            ));
        }
        self.check_bar(&bar, funding)?;
        self.bars.push(bar);
        let mut ctx = Context {
            bars: &self.bars,
            instrument: &self.instrument,
            timeframe: self.timeframe,
            portfolio: &self.portfolio,
            broker: &mut *self.broker,
            orders: &mut self.orders,
            warming_up: true,
        };
        if ctx.bars.len() == 1 {
            self.strategy.on_start(&mut ctx);
        }
        self.strategy.on_bar(&mut ctx, &bar);
        Ok(())
    }

    /// The checks `on_bar` and `warm_up` make before changing anything.
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
        let end = bar.open_time + self.timeframe.duration_ms();
        for &(time, rate) in funding {
            if self.instrument.id.kind == InstrumentKind::Spot {
                return Err(EngineError::FundingOnSpot);
            }
            if !(bar.open_time..end).contains(&time) {
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

    /// Equity at each bar's close, one point per bar fed.
    pub fn equity(&self) -> &[EquityPoint] {
        &self.equity
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
/// time window contains it, and events that fall in no bar's window (before the first bar or in
/// a gap) are not charged.
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

/// For each bar, the range of `funding` (sorted by time) that falls inside the bar's time window.
/// Events before the first bar or in a gap between bars belong to no range.
pub(crate) fn funding_ranges(
    timeframe: Timeframe,
    bars: &[Candle],
    funding: &[(Millis, f64)],
) -> Vec<Range<usize>> {
    let mut next = 0;
    bars.iter()
        .map(|bar| {
            let end = bar.open_time + timeframe.duration_ms();
            while next < funding.len() && funding[next].0 < bar.open_time {
                next += 1;
            }
            let start = next;
            while next < funding.len() && funding[next].0 < end {
                next += 1;
            }
            start..next
        })
        .collect()
}
