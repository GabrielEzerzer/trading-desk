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
