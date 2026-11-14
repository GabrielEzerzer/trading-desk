//! The demo account's API key and secret, and the v5 request signature.
//!
//! Signing, as <https://bybit-exchange.github.io/docs/v5/guide> describes it (read 2026-11-14): the
//! signed text is `timestamp + api key + recv_window + queryString` for a GET and
//! `timestamp + api key + recv_window + jsonBodyString` for a POST, signed with HMAC-SHA256 and
//! written as lowercase hex. It goes with the `X-BAPI-API-KEY`, `X-BAPI-TIMESTAMP`,
//! `X-BAPI-RECV-WINDOW` and `X-BAPI-SIGN` headers. The query string or body signed is the exact
//! one sent.

use std::fmt;

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::{Error, wire};

/// The environment variable that holds the demo account's API key.
pub const KEY_VAR: &str = "BYBIT_DEMO_API_KEY";
/// The environment variable that holds the demo account's API secret.
pub const SECRET_VAR: &str = "BYBIT_DEMO_API_SECRET";

/// The key and secret of a venue demo-trading account.
///
/// It has no `Serialize`, its `Debug` prints neither value, and nothing in the crate displays
/// them. Keys for the real demo service come only from the environment ([`DemoKeys::from_env`]).
/// Keys made with [`DemoKeys::for_loopback`] are refused by every endpoint but the loopback one.
///
/// shortcut: the strings are not wiped from memory on drop; add `zeroize` if the desk ever holds
/// keys for an account with real money (it never should, by design).
#[derive(Clone)]
pub struct DemoKeys {
    key: String,
    secret: String,
    loopback_only: bool,
}

impl DemoKeys {
    /// Reads `BYBIT_DEMO_API_KEY` and `BYBIT_DEMO_API_SECRET`. A missing, empty or non-printable
    /// value is an error that names the variable, never its value.
    pub fn from_env() -> Result<Self, Error> {
        let read = |name: &'static str| {
            let value = std::env::var(name).map_err(|_| Error::MissingKey { name })?;
            check_value(name, value)
        };
        Ok(DemoKeys {
            key: read(KEY_VAR)?,
            secret: read(SECRET_VAR)?,
            loopback_only: false,
        })
    }

    /// Keys for a test against the mock venue on 127.0.0.1. A client built with them for the demo
    /// service refuses to start, and keys from the environment are refused by the loopback one.
    pub fn for_loopback(key: &str, secret: &str) -> Result<Self, Error> {
        Self::from_values(key, secret, true)
    }

    /// Keys from given values, so a unit test can make keys that are not loopback-only without
    /// reading the environment.
    pub(crate) fn from_values(key: &str, secret: &str, loopback_only: bool) -> Result<Self, Error> {
        Ok(DemoKeys {
            key: check_value(KEY_VAR, key.to_string())?,
            secret: check_value(SECRET_VAR, secret.to_string())?,
            loopback_only,
        })
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn loopback_only(&self) -> bool {
        self.loopback_only
    }

    /// The lowercase hex HMAC-SHA256 of `timestamp + key + recv_window + payload`, where the
    /// payload is the query string of a GET or the body of a POST.
    pub(crate) fn sign(&self, timestamp: i64, recv_window: u64, payload: &str) -> String {
        let text = format!("{timestamp}{}{recv_window}{payload}", self.key);
        hmac_hex(self.secret.as_bytes(), text.as_bytes())
    }

    /// Replaces a venue message that contains the key, the secret or `signature` (the venue's
    /// signature error can echo the signed text, which holds the key), and trims the rest
    /// ([`wire::venue_text`]). The key and secret are visible ASCII and the signature is hex, so
    /// they are looked for in the message reduced to its visible ASCII characters: a credential
    /// split by anything else (a space, a hidden or format character) is found whole. The
    /// signature is compared without regard to case. The cut to length comes last, so a
    /// credential at the cut is found whole too. Callers hand it the venue's whole text, never
    /// text already cut.
    pub(crate) fn scrub(&self, message: &str, signature: &str) -> String {
        let visible: String = message.chars().filter(char::is_ascii_graphic).collect();
        let leaks = [self.key.as_str(), self.secret.as_str()]
            .iter()
            .any(|s| !s.is_empty() && visible.contains(s))
            || (!signature.is_empty()
                && visible
                    .to_ascii_lowercase()
                    .contains(&signature.to_ascii_lowercase()));
        if leaks {
            return "(message withheld: it contained a credential)".into();
        }
        wire::venue_text(message)
    }
}

impl fmt::Debug for DemoKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DemoKeys(..)")
    }
}

fn check_value(name: &'static str, value: String) -> Result<String, Error> {
    // A header value must be visible ASCII; anything else is a mistake in the variable.
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::InvalidKey { name });
    }
    Ok(value)
}

fn hmac_hex(key: &[u8], data: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(data);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231_test_case_2() {
        assert_eq!(
            hmac_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn signs_timestamp_key_window_and_payload_in_that_order() {
        let keys = DemoKeys::for_loopback("fake-key", "fake-secret").unwrap();
        let payload = "category=linear&symbol=BTCUSDT";
        assert_eq!(
            keys.sign(1658384314791, 5000, payload),
            hmac_hex(
                b"fake-secret",
                b"1658384314791fake-key5000category=linear&symbol=BTCUSDT"
            )
        );
    }

    #[test]
    fn debug_shows_neither_value() {
        let keys = DemoKeys::for_loopback("KEYVALUE123", "SECRETVALUE456").unwrap();
        let shown = format!("{keys:?} {keys:#?}");
        assert!(!shown.contains("KEYVALUE123") && !shown.contains("SECRETVALUE456"));
    }

    #[test]
    fn scrub_withholds_messages_that_carry_a_credential() {
        let keys = DemoKeys::for_loopback("KEYVALUE123", "SECRETVALUE456").unwrap();
        let echoed = "error sign! origin_string[1700000000000KEYVALUE1235000{}]";
        assert!(!keys.scrub(echoed, "").contains("KEYVALUE123"));
        assert!(!keys.scrub("sig abc123", "abc123").contains("abc123"));
        assert_eq!(keys.scrub("Too many visits!", "abc"), "Too many visits!");
        assert_eq!(keys.scrub(&"x".repeat(500), "").len(), 200);
    }

    #[test]
    fn scrub_finds_a_credential_split_by_hidden_characters_or_at_the_cut() {
        let keys = DemoKeys::for_loopback("KEYVALUE123", "SECRETVALUE456").unwrap();
        for split in [
            "KEYV\u{0}ALUE123",
            "KEY\nVALUE123",
            "SECRET\u{7}\rVALUE456",
            "KEYV\u{200B}ALUE123",
            "SECRET\u{FEFF}VALUE\u{202E}456",
        ] {
            let shown = keys.scrub(&format!("error {split} end"), "");
            assert!(
                !shown.contains("KEYVALUE123") && !shown.contains("SECRETVALUE456"),
                "{shown:?}"
            );
            assert!(shown.contains("withheld"), "{shown:?}");
        }
        // A key that straddles the 200-character cut is withheld, not cut in half.
        let shown = keys.scrub(&format!("{}KEYVALUE123", "x".repeat(195)), "");
        assert!(!shown.contains("KEYVA"), "{shown:?}");
    }

    #[test]
    fn scrub_finds_a_credential_split_by_any_character_that_is_not_visible_ascii() {
        let keys = DemoKeys::for_loopback("KEYVALUE123", "SECRETVALUE456").unwrap();
        for split in [
            "KEYV\u{AD}ALUE123",
            "KEYV ALUE123",
            "KEY\u{61C}VALUE123",
            "KEY\u{180E}VALUE\u{34F}123",
            "SECRET\u{E0041}VALUE456",
            "SECRET\u{FE0F}VALUE\u{E0100}456",
            "K E Y V A L U E 1 2 3",
        ] {
            let shown = keys.scrub(&format!("error {split} end"), "");
            assert!(shown.contains("withheld"), "{split:?}: {shown:?}");
        }
        // The signature is hex; an echo in capitals or split by a space is found too.
        let shown = keys.scrub("sig ABC 123DEF", "abc123def");
        assert!(shown.contains("withheld"), "{shown:?}");
        // A message that shares only fragments of a credential is kept.
        let ordinary = "KEY VALUE 12 is not SECRET; VALUE456? no";
        assert_eq!(keys.scrub(ordinary, "abc123def"), ordinary);
    }

    #[test]
    fn empty_or_unprintable_values_are_refused_by_name() {
        let err = DemoKeys::for_loopback("", "s").unwrap_err();
        assert!(matches!(err, Error::InvalidKey { name: KEY_VAR }));
        let err = DemoKeys::for_loopback("k", "has space").unwrap_err();
        assert!(matches!(err, Error::InvalidKey { name: SECRET_VAR }));
    }
}
