//! An example strategy file. A strategy is one file in three parts:
//!
//! 1. the strategy: its `entry()` with name, description and parameters, and the `Strategy`
//!    implementation;
//! 2. its test plan: the instruments, timeframe and period it is tested on, and the settings of
//!    the backtest, the sweep, the walk-forward and the optional out-of-sample split;
//! 3. its verdict rule: a kind (`PropFirm`, `Portfolio` or `HighRisk`) with its limits.
//!
//! To add a strategy, copy this file and edit it. In `src/strategies/private/` it is found on its
//! own; here, as an example, it also needs `pub mod` in `mod.rs` and a line in the registry.

use td_core::{Candle, InstrumentId, InstrumentKind, OrderType, Side, Timeframe, Venue};

use crate::strategies::{affordable_qty, direction, order};
use crate::{
    BacktestSettings, Context, Direction, EngineError, EvidenceSettings, Metric, Objective,
    ParamAxis, ParamKind, ParamSpace, ParamSpec, ParamValue, Params, Period, PortfolioBenchmark,
    PortfolioLimits, Split, Strategy, StrategyEntry, SweepPlan, TestPlan, VerdictRule,
    WalkForwardPlan, date,
};

// ---------------------------------------------------------------------------------------------
// 1. The strategy
// ---------------------------------------------------------------------------------------------

/// Long while the fast simple moving average of closes is above the slow one, flat otherwise.
/// With `allow_short` on a perpetual, short while the fast average is below the slow one.
///
/// The position is `equity_fraction` of equity, capped so that its value plus the taker fee,
/// slippage and a small margin fits in the equity, so a spot buy passes the cash check.
#[derive(Debug, Clone)]
pub struct SmaCross {
    pub fast: usize,
    pub slow: usize,
    pub allow_short: bool,
    pub equity_fraction: f64,
}

pub(crate) fn entry() -> StrategyEntry {
    StrategyEntry {
        name: "sma_cross",
        description: "Long while the fast moving average of closes is above the slow one, flat \
                      otherwise; optionally short below it on perpetuals.",
        params: vec![
            ParamSpec {
                name: "fast",
                kind: ParamKind::Int,
                default: ParamValue::Int(10),
                min: Some(1.0),
                max: None,
                description: "Bars in the fast moving average.",
            },
            ParamSpec {
                name: "slow",
                kind: ParamKind::Int,
                default: ParamValue::Int(30),
                min: Some(2.0),
                max: None,
                description: "Bars in the slow moving average; must be more than fast.",
            },
            ParamSpec {
                name: "allow_short",
                kind: ParamKind::Bool,
                default: ParamValue::Bool(false),
                min: None,
                max: None,
                description: "Go short when the fast average is below the slow one. Perpetuals \
                              only; ignored on spot.",
            },
            ParamSpec {
                name: "equity_fraction",
                kind: ParamKind::Float,
                default: ParamValue::Float(0.95),
                min: Some(0.0),
                max: Some(1.0),
                description: "Position size as a fraction of equity. Sizes near 1 are capped so \
                              that fees, slippage and a small margin still fit in the equity.",
            },
        ],
        build,
        plan: plan(),
        rule: rule(),
    }
}

fn build(p: &Params) -> Result<Box<dyn Strategy>, EngineError> {
    let s = SmaCross {
        fast: p.len("fast")?,
        slow: p.len("slow")?,
        allow_short: p.bool("allow_short")?,
        equity_fraction: p.float("equity_fraction")?,
    };
    if s.fast >= s.slow {
        return Err(EngineError::InvalidParam {
            name: "slow".into(),
            reason: format!("must be more than fast ({})", s.fast),
        });
    }
    Ok(Box::new(s))
}

fn sma(bars: &[Candle], n: usize) -> f64 {
    bars[bars.len() - n..].iter().map(|b| b.close).sum::<f64>() / n as f64
}

impl Strategy for SmaCross {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let bars = ctx.bars();
        // Wait for the previous order to fill before deciding again, and send nothing while
        // warming up: it would only be rejected.
        if ctx.is_warming_up() || bars.len() < self.slow || !ctx.open_orders().is_empty() {
            return;
        }
        let (fast, slow) = (sma(bars, self.fast), sma(bars, self.slow));
        let can_short = self.allow_short && ctx.instrument().id.kind == InstrumentKind::Perpetual;
        let want = if fast > slow {
            1.0
        } else if fast < slow && can_short {
            -1.0
        } else {
            0.0
        };
        let position = ctx.position();
        if direction(position) == want {
            return;
        }
        // On spot a buy only happens from flat, where the equity is all cash.
        let equity = ctx.equity();
        let size =
            (equity * self.equity_fraction / bar.close).min(affordable_qty(ctx, equity, bar.close));
        let target = want * size;
        let delta = target - position;
        let side = if delta > 0.0 { Side::Buy } else { Side::Sell };
        let req = order(ctx, side, delta.abs(), OrderType::Market, false);
        // A rejected order (for example below the minimum size) means no trade on this bar.
        let _ = ctx.submit(req);
    }
}

// ---------------------------------------------------------------------------------------------
// 2. The test plan
// ---------------------------------------------------------------------------------------------

fn plan() -> TestPlan {
    let spot = |symbol: &str| InstrumentId {
        venue: Venue::Binance,
        symbol: symbol.into(),
        kind: InstrumentKind::Spot,
    };
    TestPlan {
        instruments: vec![spot("BTCUSDT"), spot("ETHUSDT")],
        timeframe: Timeframe::H1,
        period: Period {
            from: date(2023, 1, 1),
            to: date(2025, 1, 1),
        },
        backtest: BacktestSettings {
            starting_cash: 10_000.0,
            slippage_bps: 2.0,
            max_leverage: 1.0,
        },
        sweep: SweepPlan {
            fixed: Params::new(),
            axes: vec![
                ParamAxis {
                    name: "fast".into(),
                    space: ParamSpace::Range {
                        from: 5.0,
                        to: 20.0,
                        step: 5.0,
                    },
                },
                ParamAxis {
                    name: "slow".into(),
                    space: ParamSpace::Range {
                        from: 30.0,
                        to: 70.0,
                        step: 10.0,
                    },
                },
            ],
            objective: Objective {
                metric: Metric::Sharpe,
                direction: Direction::Maximize,
            },
            min_trades: 10,
        },
        walk_forward: WalkForwardPlan {
            train_days: 180,
            test_days: 60,
        },
        out_of_sample: Some(Split::Fraction(0.7)),
        evidence: EvidenceSettings::default(),
    }
}

// ---------------------------------------------------------------------------------------------
// 3. The verdict rule
// ---------------------------------------------------------------------------------------------

fn rule() -> VerdictRule {
    VerdictRule::Portfolio {
        benchmark: PortfolioBenchmark::BuyAndHold,
        limits: PortfolioLimits::default(),
    }
}
