//! Strategies only the crate's own tests can build. The registered ones are broken on purpose so
//! a test can show that a check catches it: [`crate::find_strategy`] finds them in the crate's
//! tests; they are not listed and do not exist outside them. [`SeesFunding`] is not registered: a
//! test builds it and runs it with [`crate::run_strategy`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use td_core::{
    AccountCurrency, Candle, InstrumentId, InstrumentKind, Millis, OrderType, Side, Timeframe,
    Venue,
};

use crate::strategies::{affordable_qty, order};
use crate::{
    BacktestSettings, Context, Direction, EngineError, EvidenceSettings, Metric, Objective,
    ParamAxis, ParamKind, ParamSpace, ParamSpec, ParamValue, Params, Period, PortfolioBenchmark,
    PortfolioLimits, Strategy, StrategyEntry, SweepPlan, TestPlan, VerdictRule, WalkForwardPlan,
};

pub(crate) fn entries() -> Vec<StrategyEntry> {
    vec![unrepeatable(), one_hour()]
}

/// How many `Unrepeatable` strategies have been built in this process.
static BUILDS: AtomicUsize = AtomicUsize::new(0);

/// With its defaults it never trades. With `trade` it buys every tenth bar and sells every tenth
/// bar after, sizing each buy by how many of its kind were built before it, so two runs of the
/// same backtest fill different quantities: it fails I2 and I3, but only with the swept value.
#[derive(Debug, Clone)]
pub struct Unrepeatable {
    pub trade: bool,
    pub share: f64,
}

fn unrepeatable() -> StrategyEntry {
    StrategyEntry {
        name: "test_unrepeatable",
        description: "Trades only when swept, and differently on every run.",
        params: vec![ParamSpec {
            name: "trade",
            kind: ParamKind::Bool,
            default: ParamValue::Bool(false),
            min: None,
            max: None,
            description: "Trade at all.",
        }],
        build,
        plan: plan(),
        rule: VerdictRule::Portfolio {
            benchmark: PortfolioBenchmark::Cash,
            limits: PortfolioLimits::default(),
        },
    }
}

fn build(p: &Params) -> Result<Box<dyn Strategy>, EngineError> {
    let n = BUILDS.fetch_add(1, Ordering::Relaxed);
    Ok(Box::new(Unrepeatable {
        trade: p.bool("trade")?,
        share: 0.5 + (n % 10) as f64 * 0.05,
    }))
}

impl Strategy for Unrepeatable {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        if !self.trade || ctx.is_warming_up() || !ctx.bars().len().is_multiple_of(10) {
            return;
        }
        let position = ctx.position();
        let request = if position == 0.0 {
            let qty = affordable_qty(ctx, ctx.cash() * self.share, bar.close);
            order(ctx, Side::Buy, qty, OrderType::Market, false)
        } else {
            order(ctx, Side::Sell, position.abs(), OrderType::Market, true)
        };
        let _ = ctx.submit(request);
    }
}

const HOUR: i64 = 3_600_000;

/// In the market one hour a day and nothing else, so it has no edge of its own: at the close of
/// the bar that opens at `hour` UTC it buys with half its cash, filled at the next bar's open,
/// and at that bar's close it sells, filled at the open of the bar after. On a market with quotes
/// it pays the ask at the open of the hour after `hour` and no other spread.
#[derive(Debug, Clone)]
pub struct OneHour {
    pub hour: i64,
}

fn one_hour() -> StrategyEntry {
    let mut plan = plan();
    plan.sweep.axes = vec![ParamAxis {
        name: "hour".into(),
        space: ParamSpace::Values {
            values: vec![ParamValue::Int(0)],
        },
    }];
    StrategyEntry {
        name: "test_one_hour",
        description: "In the market one hour a day.",
        params: vec![ParamSpec {
            name: "hour",
            kind: ParamKind::Int,
            default: ParamValue::Int(0),
            min: Some(0.0),
            max: Some(23.0),
            description: "The UTC hour whose bar's close sends the buy.",
        }],
        build: |p| {
            Ok(Box::new(OneHour {
                hour: p.int("hour")?,
            }))
        },
        plan,
        rule: VerdictRule::Portfolio {
            benchmark: PortfolioBenchmark::Cash,
            limits: PortfolioLimits::default(),
        },
    }
}

impl Strategy for OneHour {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        if ctx.is_warming_up() {
            return;
        }
        let hour = bar.open_time / HOUR % 24;
        let position = ctx.position();
        let request = if position == 0.0 && hour == self.hour {
            let qty = affordable_qty(ctx, ctx.cash() * 0.5, bar.close);
            order(ctx, Side::Buy, qty, OrderType::Market, false)
        } else if position > 0.0 && hour == (self.hour + 1) % 24 {
            order(ctx, Side::Sell, position, OrderType::Market, true)
        } else {
            return;
        };
        let _ = ctx.submit(request);
    }
}

/// Four-hour bars of a perpetual over 250 days from time 0, `trade` swept over the one value that
/// trades: four 45-day test windows after a 60-day train window, every one choosing it.
fn plan() -> TestPlan {
    TestPlan {
        instruments: vec![InstrumentId {
            venue: Venue::Binance,
            symbol: "TESTUSDT".into(),
            kind: InstrumentKind::Perpetual,
        }],
        timeframe: Timeframe::H4,
        period: Period {
            from: 0,
            to: 250 * 24 * 3_600_000,
        },
        backtest: BacktestSettings {
            starting_cash: 10_000.0,
            max_leverage: 1.0,
            slippage: Vec::new(),
            account_currency: AccountCurrency::Quote,
            fee_override: None,
        },
        sweep: SweepPlan {
            fixed: Params::new(),
            axes: vec![ParamAxis {
                name: "trade".into(),
                space: ParamSpace::Values {
                    values: vec![ParamValue::Bool(true)],
                },
            }],
            objective: Objective {
                metric: Metric::TotalReturn,
                direction: Direction::Maximize,
            },
            min_trades: 0,
        },
        walk_forward: WalkForwardPlan {
            train_days: 60,
            test_days: 45,
        },
        out_of_sample: None,
        evidence: EvidenceSettings::default(),
    }
}

/// What [`SeesFunding`] saw on each bar it was called on, in order: the bar's open time, whether
/// it was a warm-up bar, and [`Context::funding`].
pub(crate) type FundingSeen = Arc<Mutex<Vec<(Millis, bool, Vec<(Millis, f64)>)>>>;

/// Never trades. Records what [`Context::funding`] gives it on each bar, so a test can compare it
/// with the funding the run was given.
pub(crate) struct SeesFunding(pub FundingSeen);

impl Strategy for SeesFunding {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let seen = (bar.open_time, ctx.is_warming_up(), ctx.funding().to_vec());
        self.0.lock().unwrap().push(seen);
    }
}
