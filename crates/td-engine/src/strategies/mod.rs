//! Reference strategies.

pub mod donchian;
pub mod sma_cross;

use td_core::{OrderRequest, OrderType, Side, TimeInForce};

use crate::Context;

fn order(
    ctx: &Context,
    side: Side,
    qty: f64,
    order_type: OrderType,
    reduce_only: bool,
) -> OrderRequest {
    OrderRequest {
        instrument: ctx.instrument().id.clone(),
        side,
        qty,
        order_type,
        tif: TimeInForce::Gtc,
        reduce_only,
        client_tag: None,
    }
}

/// The fraction of the budget the reference strategies leave unspent, for a next bar that opens
/// above the close they sized on.
const HEADROOM: f64 = 0.005;

/// The largest quantity whose value at `price`, plus the taker fee and slippage on it, fits in
/// `budget` with [`HEADROOM`] to spare. Within it, a spot buy passes the cash check at submission
/// and a perpetual position stays within 1x of the budget.
fn affordable_qty(ctx: &Context, budget: f64, price: f64) -> f64 {
    let costs = 1.0 + ctx.fees().taker + ctx.slippage_bps() / 10_000.0;
    budget * (1.0 - HEADROOM) / (price * costs)
}

/// -1, 0 or 1. Unlike `f64::signum`, zero maps to zero.
fn direction(x: f64) -> f64 {
    if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    }
}
