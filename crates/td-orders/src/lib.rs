//! The order route from the engine to a venue's demo-trading REST API: Bybit v5, USDT perpetuals
//! (category `linear`), one-way mode.
//!
//! A blocking [`Client`] places market orders, reduce-only market orders and reduce-only stops
//! that rest at the venue; cancels them and amends a resting stop's trigger and quantity in place;
//! and reads the position, the open orders, one order by its client id, the executions since a
//! time, and the account's equity; and, unsigned, a contract's public trading rules and ticker.
//! Every order carries the caller's [`ClientOrderId`], sent as the venue's `orderLinkId`. Limit
//! orders are not supported.
//!
//! Guards that no configuration can turn off:
//! - **Hosts.** A client reaches `https://api-demo.bybit.com` or `http://127.0.0.1:<port>` (the
//!   mock venue) and nothing else: [`Endpoint`] has no other variant, the base URL is checked
//!   again when the client is built, and every request URL is checked against it. No environment
//!   variable, file or flag picks the host. Redirects are never followed, no system proxy is used,
//!   and nothing is resent automatically by the HTTP layer.
//! - **Keys.** [`DemoKeys`] reads `BYBIT_DEMO_API_KEY` and `BYBIT_DEMO_API_SECRET` from the
//!   environment. It cannot be serialised, its `Debug` prints neither value, and its header values
//!   are marked sensitive. No error or `Display` in the crate holds the key, the secret, a
//!   signature or a raw response body; a venue message that echoes a credential is withheld.
//!   Keys from the environment are refused by the loopback endpoint
//!   ([`Error::DemoKeysOnLoopback`]), and keys made for the mock by the demo one
//!   ([`Error::LoopbackKeysOnDemo`]), so the demo account's keys never reach a local port.
//! - **One order per id.** A client refuses to send an id twice, and sends a create once. An answer
//!   that leaves open whether the venue took it (a timeout, a dropped connection, a server error,
//!   a load or frequency answer) is resolved by asking the venue, never by sending it again; only a
//!   rate-limit (10006) or timestamp (10002) refusal, which the venue documents as a refusal before
//!   processing, is sent again, and a refusal of that resend is looked up too. A lookup that finds
//!   nothing is [`Error::OutcomeUnknown`], never "not placed": the caller keeps the order as
//!   possibly live under its original id, across a restart, keeps asking the venue about it,
//!   counts its possible fill in its exposure, and never sends the same order again under another
//!   id because a lookup came back empty; a timeout or a spent time budget does not withdraw the
//!   request ([`Error::OutcomeUnknown`] has the full contract). A cancel or amend is confirmed by
//!   reads after the venue acknowledges it (see the `ids`, `client` and `orders` module docs for
//!   what the venue documents and the rules built on it).
//! - **Venue filters.** An order is checked with `Instrument::validate_order` (lot and tick grid,
//!   minimum quantity, minimum notional with the reduce-only exemption) before it is sent, and its
//!   numbers are written as exact decimals on the grid.
//! - **Position mode.** Orders are one-way (`positionIdx` 0). An account in hedge mode is
//!   [`Error::HedgeMode`]; the route never changes account settings.
//! - **Rate limits, clock, timeouts.** A request refused with 10006 waits for the venue's reset and
//!   goes again a few times; a read answered 429 does too, while an order request answered 429 is
//!   looked up; an IP block is returned at once. The clock offset is measured against the venue's
//!   server time when the client is built and again on a timestamp rejection, and a server time
//!   more than a day from the local clock is refused ([`Error::ServerTimeOutOfRange`]). Timeouts
//!   are seconds, and each place, cancel and amend runs within one time budget ([`Timing`]).
//! - **Answers.** A response body is read up to 4 MiB after decompression and refused past it
//!   ([`Error::ResponseTooLarge`]). Negative quantities, sizes and prices are refused.
//!
//! The mock venue the tests run against lives in `mock/bybit` (crate `td-mock-bybit`). The
//! `demo-smoke` example runs one round trip on the real demo account and refuses to send anything
//! without `--confirm-send-orders`.

mod client;
mod error;
mod guard;
mod ids;
mod keys;
mod orders;
mod reads;
mod wire;

pub use client::{Client, Timing};
pub use error::{Action, Error};
pub use guard::{DEMO_HOST, Endpoint};
pub use ids::{ClientOrderId, MAX_LEN as CLIENT_ID_MAX_LEN};
pub use keys::{DemoKeys, KEY_VAR, SECRET_VAR};
pub use orders::{CancelOutcome, Placed};
pub use reads::SETTLE_COIN;
pub use wire::{
    Equity, ExecKind, Execution, InstrumentInfo, OrderStatus, Position, Ticker, VenueOrder,
    venue_text,
};
