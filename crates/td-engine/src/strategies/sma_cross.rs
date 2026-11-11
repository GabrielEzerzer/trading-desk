use td_core::{Candle, OrderType, Side};

use super::{affordable_qty, direction, order};
use crate::{
    Context, EngineError, ParamKind, ParamSpec, ParamValue, Params, Strategy, StrategyEntry,
};

/// Long while the fast simple moving average of closes is above the slow one, flat otherwise.
/// With `allow_short` on a kind that can short (perpetual, FX), short while the fast average is below the slow one.
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
                      otherwise; optionally short below it where the market allows shorts.",
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
                description: "Go short when the fast average is below the slow one, on markets \
                              that allow shorts (perpetuals, FX); ignored on spot.",
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
        let can_short = self.allow_short && ctx.instrument().id.kind.rules().can_short;
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
