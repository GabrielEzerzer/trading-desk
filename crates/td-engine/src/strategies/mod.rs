//! The example strategies, and the private ones when their repo is checked out.
//!
//! One strategy is one file in three parts: the `Strategy` implementation with its parameters, its
//! test plan and its verdict rule (see `sma_cross.rs`). An example is registered in
//! [`crate::registry`]; a private strategy, a file in `private/`, is found by build.rs.
//! `gotobi_calendar` is a calendar for strategies to use, not a strategy.

pub mod donchian;
pub mod gotobi_calendar;
pub mod sma_cross;
#[cfg(test)]
pub(crate) mod test_only;

/// The strategies in `private/`, compiled in by build.rs; an empty list without the folder.
pub(crate) mod private {
    include!(concat!(env!("OUT_DIR"), "/private_strategies.rs"));
}

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
/// `budget` (in the account currency) with [`HEADROOM`] to spare. Within it, a spot buy passes the
/// cash check at submission and a margined position stays within 1x of the budget. The value is
/// converted to the account currency at the current close ([`Context::to_account`]).
fn affordable_qty(ctx: &Context, budget: f64, price: f64) -> f64 {
    let costs = 1.0 + ctx.fees().taker + ctx.slippage_bps() / 10_000.0;
    budget * (1.0 - HEADROOM) / ctx.to_account(price * costs)
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

/// The fixture strategies in `tests/fixtures/private_strategies/`, compiled in the same way.
#[cfg(test)]
pub(crate) mod fixture {
    include!(concat!(env!("OUT_DIR"), "/fixture_strategies.rs"));
}
