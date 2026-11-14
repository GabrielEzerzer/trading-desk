//! The caller's client order id, sent as the venue's `orderLinkId`.
//!
//! What the venue documents, read on 2026-11-14 from
//! <https://bybit-exchange.github.io/docs/v5/order/create-order>:
//!
//! - The id is "a max of 36 characters", and "combinations of numbers, letters (upper and lower
//!   cases), dashes, and underscores are supported".
//! - For futures, perpetuals and spot it is optional and "always unique". The page gives no scope
//!   for that uniqueness: it never says whether the venue checks it against open orders only or
//!   against closed orders too, nor for how long. The error page
//!   (<https://bybit-exchange.github.io/docs/v5/error>) lists `110072`, "OrderLinkedID is
//!   duplicate", as the answer to a reused id.
//! - The demo-trading page (<https://bybit-exchange.github.io/docs/v5/demo>) says orders in demo
//!   trading are kept for 7 days, so no venue-side check can reach further back than that there.
//! - Create, amend and cancel are asynchronous: an acknowledgement means the request was accepted,
//!   not that the order is in its final state, and order history "may delay".
//!
//! The route does not depend on the unstated scope of that uniqueness:
//!
//! 1. One [`crate::Client`] refuses to send an id it has already sent (locally, before any
//!    request), so a caller bug cannot place the same id twice in one process. Across processes the
//!    caller's per-session prefix keeps ids apart.
//! 2. A create is sent once. An answer that leaves open whether the venue took it is resolved by
//!    asking the venue about the id, never by sending the id again; only a rate-limit (10006) or
//!    timestamp (10002) refusal, documented as a refusal before processing, is sent again (the
//!    `client` module). A lookup that finds nothing leaves the id spent.
//! 3. A `110072` answer to the create is [`crate::Error::DuplicateClientId`]: the venue holds the
//!    id already.
//!
//! The smoke example (`examples/demo-smoke.rs`) sends a filled order's id again and records whether
//! the venue answers `110072`. That is an observation of the live demo service only; the route
//! relies on it nowhere.

use std::fmt;

use crate::Error;

/// The longest id the venue accepts.
pub const MAX_LEN: usize = 36;

/// A validated client order id: 1 to 36 ASCII letters, digits, dashes or underscores.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClientOrderId(String);

impl ClientOrderId {
    /// Checks `id` against the venue's length and character rules.
    pub fn new(id: impl Into<String>) -> Result<Self, Error> {
        let id = id.into();
        if id.is_empty() || id.len() > MAX_LEN {
            return Err(Error::InvalidClientId {
                reason: "it must be 1 to 36 characters long",
            });
        }
        if !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::InvalidClientId {
                reason: "it may hold only ASCII letters, digits, dashes and underscores",
            });
        }
        Ok(ClientOrderId(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClientOrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_venue_character_set_up_to_36() {
        assert!(ClientOrderId::new("sess-01_42").is_ok());
        assert!(ClientOrderId::new("a".repeat(36)).is_ok());
        assert!(ClientOrderId::new("AZaz09-_").is_ok());
    }

    #[test]
    fn refuses_empty_long_and_foreign_characters() {
        for bad in [
            String::new(),
            "a".repeat(37),
            "has space".into(),
            "dot.ted".into(),
            "slash/".into(),
            "é".into(),
            "a\u{0}".into(),
        ] {
            assert!(
                matches!(
                    ClientOrderId::new(bad.clone()),
                    Err(Error::InvalidClientId { .. })
                ),
                "{bad:?}"
            );
        }
    }
}
