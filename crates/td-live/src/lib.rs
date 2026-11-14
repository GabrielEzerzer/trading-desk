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
//!   filled. If the venue refuses that early stop, nothing is closed and the stop goes out under
//!   the cover id on the first confirmed fill; if the route did not send it, it goes out under its
//!   own id then; if its outcome is unknown, the venue is asked about it on the first confirmed
//!   fill (see "Unknown outcomes"). An entry that closes unfilled owes nothing for a stop that
//!   did not rest. For a confirmed fill, a stop the venue does not take now is replaced by a
//!   reduce-only market order, since flat is the safe side; if the close fails too, the broker
//!   faults. A stop that cannot be resized faults the broker, after one more reduce-only stop is
//!   placed for the part of the fill the first one does not cover.
//! - **Unreadable venue.** While the venue could not be read on the last bar, only reduce-only
//!   orders go out; any other order is cancelled with a reason on the next bar. Three bars in a
//!   row without a full read is a fault.
//! - **Fills come from the venue.** Each bar reads the executions since the last one processed,
//!   maps them to engine orders by client id ([`ids`] has the scheme) and reports one fill per
//!   order with the venue's quantity, average price and fee, timed at the bar's open.
//! - **Stale decisions.** An order decided on a bar that closed more than one bar length ago is
//!   not sent, unless the venue already holds its id, in which case it is adopted.
//! - **Unknown outcomes.** An order whose request outcome is unknown is settled by asking the
//!   venue, never by sending it again: the venue is asked by client id on the next bar (an entry
//!   with an attached stop at once, so a fill it may have made gets its stop), and the broker
//!   faults after three failed asks. An order the route reports as not sent goes out again, if at
//!   all, under the same id. There is one exception. When an entry's stop has an unknown outcome
//!   and the venue has confirmed the entry's fill, the venue is first asked about that stop. If
//!   it holds the stop, the stop is kept and resized to the fill. If it does not show the stop, a
//!   second reduce-only stop for the fill goes out under the cover id and the broker faults at
//!   once, saying the fill is covered by the second stop and that the first may also rest. A
//!   second reduce-only stop cannot add to the position, and a fill with no stop is the worse
//!   harm. If the venue refuses that second stop, the fill is closed at market and the broker
//!   faults all the same. If the venue cannot be asked, nothing goes out and the broker faults.
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
//!   and resizes of them, for fills the venue lists. An order whose outcome going live could not
//!   settle stays under that rule until the venue settles it: the reads that later find it send
//!   only a reduce-only stop for a fill the venue lists, never a close at market.
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
