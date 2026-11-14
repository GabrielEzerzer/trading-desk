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
//!   cancelled with a reason on the next bar. An entry with an attached stop is sent and its fill
//!   is looked for for about half a second; a reduce-only stop for the filled quantity is then
//!   placed. When the venue accepted the entry but the fill is not confirmed in that time, the
//!   stop is placed at once for the quantity sent and resized, down as well as up, when the fill
//!   is first seen. A market order is only ever sent to close a quantity the venue confirmed as
//!   filled: if that early stop does not rest, nothing is closed, the stop goes out under the
//!   cover id on the first confirmed fill, and an entry that closes unfilled owes nothing. For a
//!   confirmed fill, a stop the venue refuses is replaced by a reduce-only market order; a stop
//!   whose outcome is unknown gets a cover stop for the fill and faults the broker, and the fill
//!   is closed at market only if the cover is refused. If the close fails too, the broker faults.
//!   A stop that cannot be resized faults the broker, after one more reduce-only stop is placed
//!   for the part of the fill the first one does not cover.
//! - **Unreadable venue.** While the venue could not be read on the last bar, only reduce-only
//!   orders go out; any other order is cancelled with a reason on the next bar. Three bars in a
//!   row without a full read is a fault.
//! - **Fills come from the venue.** Each bar reads the executions since the last one processed,
//!   maps them to engine orders by client id ([`ids`] has the scheme) and reports one fill per
//!   order with the venue's quantity, average price and fee, timed at the bar's open.
//! - **Stale decisions.** An order decided on a bar that closed more than one bar length ago is
//!   not sent, unless the venue already holds its id, in which case it is adopted.
//! - **Unknown outcomes.** An order whose request outcome is unknown is never sent again: the
//!   venue is asked by client id on the next bar (an entry with an attached stop at once, so a
//!   fill it may have made gets its stop), and the broker faults after three failed asks.
//! - **Reconcile.** Every bar the venue's position must equal the fills reported, and the venue's
//!   open orders must be the ones the session holds, each for the quantity sent and, for a stop,
//!   at its trigger; the reads are repeated once to allow for an execution landing between them.
//! - **Faults.** A disagreement, an execution the session did not make, or a protective stop that
//!   cannot be placed or replaced is a [`BrokerFault`]. Once faulted the broker places, cancels and
//!   amends nothing more, from any path: no flatten, no cancel, no resize. The one exception is a
//!   reduce-only stop protecting a fill the venue confirmed in the same call or bar that raised
//!   the fault; never a close at market and never a resize. The fault is kept in the record and
//!   restored when the record is replayed, and nothing in this crate clears it. The owner decides
//!   what happens next.
//! - **Records.** A [`BrokerRecord`] names the session prefix, instrument and timeframe it was
//!   made under; a broker with any of the three different, or one that has already acted live,
//!   faults when handed it. A record holding a value no live run makes (a number that is not
//!   finite, a negative quantity, an execution cursor before its floor) is a fault too. The
//!   replayed session's calls must match the record call for call, and its orders must match the
//!   record's in full: the replay keeps the session's own orders and takes from the record only
//!   where each stands at the venue. Going live after a replay sends nothing from the record: an
//!   order the venue holds is adopted, and one whose outcome was unknown and that the venue does
//!   not hold is cancelled with a reason; a cancel the replayed session asks for an order the
//!   venue still holds answers false. The only orders going live can send are reduce-only stops,
//!   and resizes of them, for fills the venue lists.
//!
//! The session keeps its own model of liquidation and funding on its portfolio. The broker cannot
//! stop that model, so a bar on which the session's model would liquidate the position is a fault
//! in the same bar ([`FaultKind::SessionLiquidation`]); the venue's own liquidation or ADL arrives
//! as a non-trade execution, which is a fault too. Funding executions are not trades and are not
//! reported as fills.
//!
//! The session's leverage cap reads the session's own modelled equity, not the venue's: the
//! caller starts the session with the venue's equity ([`VenueHandle::equity`]) as its starting
//! cash, and from then on the cap reads that cash plus the fills the broker reported.

mod broker;
mod ids;

pub use broker::{BrokerFault, BrokerRecord, FaultKind, VenueBroker, VenueHandle};
pub use ids::{InvalidPrefix, PREFIX_MAX_LEN};
