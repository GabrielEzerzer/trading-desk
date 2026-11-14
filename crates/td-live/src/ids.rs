//! The client order ids a [`crate::VenueBroker`] sends, the one place the scheme is defined.
//!
//! Every id is `{prefix}-{role}{n}`, where `prefix` names the session (1 to
//! [`PREFIX_MAX_LEN`] ASCII letters, digits or `_`), `n` is the engine's [`OrderId`] and `role`
//! is one letter:
//! - `o`: the engine order `n` itself: a strategy's order, a manual order through
//!   `Session::submit`, or an attached stop once its entry fills.
//! - `c`: the reduce-only market order sent in place of stop `n` when the venue will not take the
//!   stop (its trigger is already through the market). Its executions are reported as fills of
//!   order `n`.
//! - `f`: the stop guard's own orders when a loss limit fires or the owner flattens the session
//!   (`crate::guard`): a reduce-only market order that closes the venue's position. Here `n` is
//!   the guard's own counter, saved in the session's stop-state file before each send, not an
//!   engine order. The broker does not track these; one seen on a bar is a fault, which is
//!   harmless because the session has halted before the guard sends one.
//! - `x`: reserved for orders the session sends itself outside the engine, such as a later
//!   "close the position" action. This crate sends none; an execution carrying one is a fault
//!   until that action exists.
//!
//! The engine numbers orders from 1 in submission order, so a replayed session gives every order
//! the id it had live, and a retry of an order can only ever carry the id the venue already holds.
//! The longest id is 12 + 1 + 1 + 20 (the digits of `u64::MAX`) = 34 characters, within the
//! venue's limit of [`CLIENT_ID_MAX_LEN`].
//!
//! The prefix must not be reused by another session on the same account: the venue keeps ids of
//! closed orders, and a new session's order 1 would collide with the old one's.

use td_core::OrderId;
use td_orders::{CLIENT_ID_MAX_LEN, ClientOrderId};

/// The longest session prefix.
pub const PREFIX_MAX_LEN: usize = 12;

/// The digits of the largest order number.
const MAX_DIGITS: usize = 20;

const _: () = assert!(PREFIX_MAX_LEN + 2 + MAX_DIGITS <= CLIENT_ID_MAX_LEN);

/// What an id stands for (see the module documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Order,
    Close,
    /// A flatten order the stop guard sends; `n` is the guard's counter.
    Flatten,
    /// Reserved; never sent by this crate.
    Session,
}

impl Role {
    fn tag(self) -> char {
        match self {
            Role::Order => 'o',
            Role::Close => 'c',
            Role::Flatten => 'f',
            Role::Session => 'x',
        }
    }
}

/// A session prefix that cannot make valid ids.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid session prefix: {0}")]
pub struct InvalidPrefix(pub &'static str);

pub(crate) fn check_prefix(prefix: &str) -> Result<(), InvalidPrefix> {
    if prefix.is_empty() {
        return Err(InvalidPrefix("it is empty"));
    }
    if prefix.len() > PREFIX_MAX_LEN {
        return Err(InvalidPrefix("it is longer than 12 characters"));
    }
    if !prefix
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(InvalidPrefix(
            "it may hold only ASCII letters, digits and '_'",
        ));
    }
    Ok(())
}

/// The id for engine order `id` in `role`. `prefix` must have passed [`check_prefix`].
pub(crate) fn client_id(prefix: &str, role: Role, id: OrderId) -> ClientOrderId {
    ClientOrderId::new(format!("{prefix}-{}{}", role.tag(), id.0))
        .expect("a checked prefix keeps every id within the venue's limit")
}

/// The role and engine order of an id this session made, `None` for any other id.
pub(crate) fn parse(prefix: &str, id: &str) -> Option<(Role, OrderId)> {
    let rest = id.strip_prefix(prefix)?.strip_prefix('-')?;
    let mut chars = rest.chars();
    let role = match chars.next()? {
        'o' => Role::Order,
        'c' => Role::Close,
        'f' => Role::Flatten,
        'x' => Role::Session,
        _ => return None,
    };
    let digits = chars.as_str();
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    if !canonical {
        return None;
    }
    digits.parse().ok().map(|n| (role, OrderId(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_longest_id_fits_the_venue_limit() {
        let prefix = "a".repeat(PREFIX_MAX_LEN);
        check_prefix(&prefix).unwrap();
        let id = client_id(&prefix, Role::Session, OrderId(u64::MAX));
        assert_eq!(id.as_str().len(), 34);
        assert!(id.as_str().len() <= CLIENT_ID_MAX_LEN);
    }

    #[test]
    fn prefixes_are_checked() {
        assert!(check_prefix("run_7").is_ok());
        assert!(check_prefix("").is_err());
        assert!(check_prefix(&"a".repeat(13)).is_err());
        assert!(check_prefix("a-b").is_err(), "'-' separates the parts");
        assert!(check_prefix("a b").is_err());
    }

    #[test]
    fn ids_round_trip_and_other_ids_are_not_ours() {
        for role in [Role::Order, Role::Close, Role::Flatten, Role::Session] {
            let id = client_id("s1", role, OrderId(42));
            assert_eq!(parse("s1", id.as_str()), Some((role, OrderId(42))));
        }
        assert_eq!(client_id("s1", Role::Order, OrderId(3)).as_str(), "s1-o3");
        // Another session whose prefix starts with this one's.
        assert_eq!(parse("s1", "s12-o3"), None);
        assert_eq!(parse("s12", "s1-o3"), None);
        for other in [
            "", "s1", "s1-", "s1-o", "s1-q3", "s1-o03", "s1-o3a", "seed-1", "s1-o-3",
        ] {
            assert_eq!(parse("s1", other), None, "{other}");
        }
    }
}
