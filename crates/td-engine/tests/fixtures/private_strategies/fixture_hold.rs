//! A fixture for the private strategies loader: a strategy file in the examples' format, compiled
//! into the crate's tests by build.rs the way a file in `src/strategies/private/` is compiled
//! into the engine.

use td_core::{AccountCurrency, Candle, InstrumentId, InstrumentKind, OrderType, Side, Timeframe, Venue};

use crate::strategies::{affordable_qty, order};
use crate::{
    BacktestSettings, Context, Direction, EngineError, EvidenceSettings, HighRiskLimits,
    Metric, Objective, ParamAxis, ParamKind, ParamSpace, ParamSpec, ParamValue, Params, Period,
    Strategy, StrategyEntry, SweepPlan, TestPlan, VerdictRule, WalkForwardPlan, date,
};

// 1. The strategy

/// Buys with all its cash on the first bar after `wait` bars, then holds.
#[derive(Debug, Clone)]
pub struct FixtureHold {
    pub wait: usize,
}

pub(crate) fn entry() -> StrategyEntry {
    StrategyEntry {
        name: "fixture_hold",
        description: "Buys once after a wait, then holds.",
        params: vec![ParamSpec {
            name: "wait",
            kind: ParamKind::Int,
            default: ParamValue::Int(1),
            min: Some(1.0),
            max: None,
            description: "Bars to wait before buying.",
        }],
        build,
        plan: plan(),
        rule: rule(),
    }
}

fn build(p: &Params) -> Result<Box<dyn Strategy>, EngineError> {
    Ok(Box::new(FixtureHold { wait: p.len("wait")? }))
}

impl Strategy for FixtureHold {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        if ctx.is_warming_up() || ctx.bars().len() < self.wait || ctx.position() != 0.0 {
            return;
        }
        let qty = affordable_qty(ctx, ctx.cash(), bar.close);
        let _ = ctx.submit(order(ctx, Side::Buy, qty, OrderType::Market, false));
    }
}

// 2. The test plan

fn plan() -> TestPlan {
    TestPlan {
        instruments: vec![InstrumentId {
            venue: Venue::Bybit,
            symbol: "SOLUSDT".into(),
            kind: InstrumentKind::Spot,
        }],
        timeframe: Timeframe::D1,
        period: Period {
            from: date(2021, 1, 1),
            to: date(2025, 1, 1),
        },
        backtest: BacktestSettings {
            starting_cash: 1_000.0,
            max_leverage: 1.0,
            // Each instrument takes the backtest config's default slippage; list a
            // SlippageOverride here to set one.
            slippage: Vec::new(),
            account_currency: AccountCurrency::Quote,
        },
        sweep: SweepPlan {
            fixed: Params::new(),
            axes: vec![ParamAxis {
                name: "wait".into(),
                space: ParamSpace::Values {
                    values: vec![ParamValue::Int(1), ParamValue::Int(5)],
                },
            }],
            objective: Objective {
                metric: Metric::TotalReturn,
                direction: Direction::Maximize,
            },
            min_trades: 0,
        },
        walk_forward: WalkForwardPlan {
            train_days: 365,
            test_days: 120,
        },
        out_of_sample: None,
        evidence: EvidenceSettings::default(),
    }
}

// 3. The verdict rule

fn rule() -> VerdictRule {
    VerdictRule::HighRisk {
        limits: HighRiskLimits {
            min_profit_factor: 1.5,
            ..HighRiskLimits::default()
        },
    }
}
