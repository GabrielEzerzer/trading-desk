//! An example strategy file in three parts: the strategy, its test plan and its verdict rule
//! (see `sma_cross.rs`).

use td_core::{
    Candle, InstrumentId, InstrumentKind, OrderId, OrderType, Rounding, Side, Timeframe, Venue,
};

use crate::strategies::{affordable_qty, order};
use crate::{
    BacktestSettings, Context, Direction, DrawdownMode, EngineError, EvidenceSettings, FirmLimits,
    Metric, Objective, ParamAxis, ParamKind, ParamSpace, ParamSpec, ParamValue, Params, Period,
    PropFirmLimits, Strategy, StrategyEntry, SweepPlan, TestPlan, VerdictRule, WalkForwardPlan,
    date,
};

// ---------------------------------------------------------------------------------------------
// 1. The strategy
// ---------------------------------------------------------------------------------------------

/// Donchian channel breakout with volatility-based sizing.
///
/// Enters long when the close breaks above the highest high of the previous `entry` bars (short
/// below the lowest low, with `allow_short` on a kind that can short), and exits when the close breaks the
/// opposite `exit` channel. The position is sized so that a stop `atr_stop` ATRs from the entry
/// loses `risk` of equity. That stop sits `atr_stop` ATRs from the close the entry is decided on
/// and is attached to the entry, so it is placed as a reduce-only stop order when the entry fills
/// and already covers the entry's own bar. The strategy never trades without that stop: when the
/// distance reaches the close, a long's stop would sit at or below zero, so that entry is skipped
/// and no order is sent. The size is also capped so that the entry's value,
/// plus the taker fee, slippage and a small margin, fits in the cash: the strategy never uses
/// leverage, and a spot entry passes the cash check.
///
/// ATR here is the simple mean of the true range over `atr_period` bars.
#[derive(Debug, Clone)]
pub struct DonchianBreakout {
    pub entry: usize,
    pub exit: usize,
    pub atr_period: usize,
    pub atr_stop: f64,
    pub risk: f64,
    pub allow_short: bool,
    /// The protective stop attached to the current entry.
    stop: Option<OrderId>,
}

pub(crate) fn entry() -> StrategyEntry {
    let int = |name, default, description| ParamSpec {
        name,
        kind: ParamKind::Int,
        default: ParamValue::Int(default),
        min: Some(1.0),
        max: None,
        description,
    };
    StrategyEntry {
        name: "donchian_breakout",
        description: "Enters on a break of the entry channel, exits on a break of the exit \
                      channel, sized so an ATR-based stop risks a set fraction of equity.",
        params: vec![
            int("entry", 20, "Bars in the entry channel."),
            int("exit", 10, "Bars in the exit channel."),
            int("atr_period", 14, "Bars in the average true range."),
            ParamSpec {
                name: "atr_stop",
                kind: ParamKind::Float,
                default: ParamValue::Float(2.0),
                min: Some(0.1),
                max: None,
                description: "Distance from the close the entry is decided on to its stop, in ATRs. \
                              A long entry whose stop this puts at or below zero is skipped.",
            },
            ParamSpec {
                name: "risk",
                kind: ParamKind::Float,
                default: ParamValue::Float(0.01),
                min: Some(0.0),
                max: Some(1.0),
                description: "Fraction of equity lost if the stop is hit.",
            },
            ParamSpec {
                name: "allow_short",
                kind: ParamKind::Bool,
                default: ParamValue::Bool(false),
                min: None,
                max: None,
                description: "Also trade downside breakouts, on markets that allow shorts \
                              (perpetuals, FX); ignored on spot.",
            },
        ],
        build,
        plan: plan(),
        rule: rule(),
    }
}

fn build(p: &Params) -> Result<Box<dyn Strategy>, EngineError> {
    Ok(Box::new(DonchianBreakout {
        entry: p.len("entry")?,
        exit: p.len("exit")?,
        atr_period: p.len("atr_period")?,
        atr_stop: p.float("atr_stop")?,
        risk: p.float("risk")?,
        allow_short: p.bool("allow_short")?,
        stop: None,
    }))
}

/// Simple mean of the true range over the last `n` bars; needs `n + 1` bars.
fn atr(bars: &[Candle], n: usize) -> f64 {
    let tail = &bars[bars.len() - n - 1..];
    let sum: f64 = tail
        .windows(2)
        .map(|w| {
            let (prev, b) = (w[0].close, &w[1]);
            (b.high - b.low)
                .max((b.high - prev).abs())
                .max((b.low - prev).abs())
        })
        .sum();
    sum / n as f64
}

fn highest(bars: &[Candle]) -> f64 {
    bars.iter().map(|b| b.high).fold(f64::MIN, f64::max)
}

fn lowest(bars: &[Candle]) -> f64 {
    bars.iter().map(|b| b.low).fold(f64::MAX, f64::min)
}

impl Strategy for DonchianBreakout {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let bars = ctx.bars();
        let n = bars.len();
        // Send nothing while warming up: it would only be rejected.
        if ctx.is_warming_up() || n < self.entry.max(self.exit).max(self.atr_period) + 1 {
            return;
        }
        // Wait while an entry or exit order is in flight.
        if ctx.open_orders().iter().any(|o| Some(o.id) != self.stop) {
            return;
        }
        let previous = &bars[..n - 1];
        let position = ctx.position();
        if position == 0.0 {
            self.stop = None;
            let channel = &previous[previous.len() - self.entry..];
            let atr = atr(bars, self.atr_period);
            if atr <= 0.0 {
                return;
            }
            let can_short = self.allow_short && ctx.instrument().id.kind.rules().can_short;
            let distance = self.atr_stop * atr;
            let (side, trigger) = if bar.close > highest(channel) {
                (Side::Buy, bar.close - distance)
            } else if can_short && bar.close < lowest(channel) {
                (Side::Sell, bar.close + distance)
            } else {
                return;
            };
            // No trade without a stop, and a stop at or below zero cannot be placed.
            if ctx.instrument().round_price(trigger, Rounding::Nearest) <= 0.0 {
                return;
            }
            // Flat here, so the cash is the whole equity.
            let affordable = affordable_qty(ctx, ctx.cash(), bar.close);
            let qty = (self.risk * ctx.equity() / distance).min(affordable);
            let entry = order(ctx, side, qty, OrderType::Market, false);
            if let Ok((_, stop)) = ctx.submit_with_stop(entry, trigger) {
                self.stop = Some(stop);
            }
        } else {
            let channel = &previous[previous.len() - self.exit..];
            let exit = if position > 0.0 {
                bar.close < lowest(channel)
            } else {
                bar.close > highest(channel)
            };
            if exit {
                if let Some(stop) = self.stop.take() {
                    ctx.cancel(stop);
                }
                let side = if position > 0.0 {
                    Side::Sell
                } else {
                    Side::Buy
                };
                let _ = ctx.submit(order(ctx, side, position.abs(), OrderType::Market, true));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 2. The test plan
// ---------------------------------------------------------------------------------------------

fn plan() -> TestPlan {
    TestPlan {
        instruments: vec![InstrumentId {
            venue: Venue::Binance,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Perpetual,
        }],
        timeframe: Timeframe::H4,
        period: Period {
            from: date(2022, 1, 1),
            to: date(2025, 1, 1),
        },
        backtest: BacktestSettings {
            starting_cash: 100_000.0,
            max_leverage: 1.0,
            // Each instrument takes the backtest config's default slippage; list a
            // SlippageOverride here to set one.
            slippage: Vec::new(),
        },
        sweep: SweepPlan {
            fixed: Params::new()
                .with("allow_short", ParamValue::Bool(true))
                .with("risk", ParamValue::Float(0.005)),
            axes: vec![
                ParamAxis {
                    name: "entry".into(),
                    space: ParamSpace::Range {
                        from: 10.0,
                        to: 40.0,
                        step: 10.0,
                    },
                },
                ParamAxis {
                    name: "exit".into(),
                    space: ParamSpace::Range {
                        from: 5.0,
                        to: 20.0,
                        step: 5.0,
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
            train_days: 360,
            test_days: 90,
        },
        out_of_sample: None,
        evidence: EvidenceSettings::default(),
    }
}

// ---------------------------------------------------------------------------------------------
// 3. The verdict rule
// ---------------------------------------------------------------------------------------------

/// A two-step challenge account: 5% daily loss and 10% max drawdown from the starting balance,
/// passed at 10% profit.
fn rule() -> VerdictRule {
    VerdictRule::PropFirm {
        firm: FirmLimits {
            daily_loss: 0.05,
            max_drawdown: 0.10,
            drawdown: DrawdownMode::Static,
            profit_target: 0.10,
        },
        limits: PropFirmLimits::default(),
    }
}
