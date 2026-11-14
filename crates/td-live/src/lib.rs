//! A [`td_engine::Broker`] that sends a session's orders through the order route (`td-orders`)
//! to a venue's demo-trading environment and reports what the venue did, so the strategy code a
//! backtest runs can trade there unchanged.
//!
//! Build a [`VenueBroker`] for one instrument with [`VenueBroker::new`], hand it to
//! `Session::with_broker`, and keep the [`VenueHandle`] it returns: the handle reads the broker's
//! fault, takes the per-bar [`BrokerRecord`] a restarted session replays, and reads the venue's
//! position, the session's resting orders and the account's equity.
//!
//! What the broker does:
//! - **Orders.** A market order, reduce-only or not, is sent at once; so is a reduce-only stop.
//!   Limit and stop-limit orders, and a stop that is not reduce-only, are not sent: they are
//!   cancelled with a reason on the next bar. An entry with an attached stop is sent, its fill is
//!   confirmed within half a second, and a reduce-only stop for the filled quantity is placed at
//!   once (or when the fill is first seen). A stop the venue will not take because the market is
//!   already through it is replaced by a reduce-only market order; if that fails too, the broker
//!   faults.
//! - **Fills come from the venue.** Each bar reads the executions since the last one processed,
//!   maps them to engine orders by client id ([`ids`] has the scheme) and reports one fill per
//!   order with the venue's quantity, average price and fee, timed at the bar's open.
//! - **Stale decisions.** An order decided on a bar that closed more than one bar length ago is
//!   not sent, unless the venue already holds its id, in which case it is adopted.
//! - **Unknown outcomes.** An order whose request outcome is unknown is never sent again: the
//!   venue is asked by client id on the next bar, and the broker faults after three failed asks.
//! - **Reconcile.** Every bar the venue's position must equal the fills reported, and the venue's
//!   open orders must be the ones the session holds; the reads are repeated once to allow for an
//!   execution landing between them.
//! - **Faults.** A disagreement, an execution the session did not make, or a protective stop that
//!   cannot be placed or replaced is a [`BrokerFault`]. Once faulted the broker sends nothing more
//!   and touches nothing at the venue: no flatten, no cancel. The owner decides what happens next.
//!
//! The session keeps its own model of liquidation and funding on its portfolio. The broker cannot
//! stop that model, so a bar on which the session's model would liquidate the position is a fault
//! in the same bar ([`FaultKind::SessionLiquidation`]); the venue's own liquidation or ADL arrives
//! as a non-trade execution, which is a fault too. Funding executions are not trades and are not
//! reported as fills.

mod broker;
mod ids;

pub use broker::{BrokerFault, BrokerRecord, FaultKind, VenueBroker, VenueHandle};
pub use ids::{InvalidPrefix, PREFIX_MAX_LEN};
