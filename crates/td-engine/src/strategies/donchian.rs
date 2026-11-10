use td_core::{Candle, Fill, InstrumentKind, OrderId, OrderType, Side};

use super::{affordable_qty, order};
use crate::{
    Context, EngineError, ParamKind, ParamSpec, ParamValue, Params, Strategy, StrategyEntry,
};

/// Donchian channel breakout with volatility-based sizing.
///
/// Enters long when the close breaks above the highest high of the previous `entry` bars (short
/// below the lowest low, with `allow_short` on a perpetual), and exits when the close breaks the
/// opposite `exit` channel. The position is sized so that a stop `atr_stop` ATRs from the entry
/// loses `risk` of equity, and that stop is placed as a reduce-only stop order once the entry
/// fills. The size is also capped so that the entry's value, plus the taker fee, slippage and a
/// small margin, fits in the cash: the strategy never uses leverage, and a spot entry passes the
/// cash check.
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
    /// The pending entry order and the ATR it was sized with.
    pending_entry: Option<(OrderId, f64)>,
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
                description: "Distance from the entry price to the stop, in ATRs.",
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
                description: "Also trade downside breakouts. Perpetuals only; ignored on spot.",
            },
        ],
        build,
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
        pending_entry: None,
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
        if n < self.entry.max(self.exit).max(self.atr_period) + 1 {
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
            let can_short =
                self.allow_short && ctx.instrument().id.kind == InstrumentKind::Perpetual;
            let side = if bar.close > highest(channel) {
                Side::Buy
            } else if can_short && bar.close < lowest(channel) {
                Side::Sell
            } else {
                return;
            };
            // Flat here, so the cash is the whole equity.
            let affordable = affordable_qty(ctx, ctx.cash(), bar.close);
            let qty = (self.risk * ctx.equity() / (self.atr_stop * atr)).min(affordable);
            if let Ok(id) = ctx.submit(order(ctx, side, qty, OrderType::Market, false)) {
                self.pending_entry = Some((id, atr));
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

    fn on_fill(&mut self, ctx: &mut Context, fill: &Fill) {
        match self.pending_entry {
            Some((id, atr)) if id == fill.order_id => {
                self.pending_entry = None;
                let distance = self.atr_stop * atr;
                let (side, trigger) = match fill.side {
                    Side::Buy => (Side::Sell, fill.price - distance),
                    Side::Sell => (Side::Buy, fill.price + distance),
                };
                if trigger > 0.0 {
                    let stop = OrderType::Stop { trigger };
                    let req = order(ctx, side, ctx.position().abs(), stop, true);
                    self.stop = ctx.submit(req).ok();
                }
            }
            _ if Some(fill.order_id) == self.stop => self.stop = None,
            _ => {}
        }
    }
}
