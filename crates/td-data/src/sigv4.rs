//! AWS Signature Version 4 for the one request shape the desk sends to AWS: a GET of one S3
//! object with no query and no body, signed in the `Authorization` header. Credentials come from
//! the environment and are never logged, printed, put in an error or written anywhere.
//!
//! The steps are those of the S3 API Reference, "Signature Calculations for the Authorization
//! Header: Transferring Payload in a Single Chunk (AWS Signature Version 4)", and the test below
//! checks them against that page's "GET Object" example.

use std::fmt;

use aws_lc_rs::hmac;
use sha2::{Digest, Sha256};
use td_core::{Millis, civil_from_days};

use crate::DataError;
use crate::time::DAY;

/// The SHA-256 of an empty body, which a GET sends.
pub(crate) const EMPTY_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

const ACCESS_KEY_ID: &str = "AWS_ACCESS_KEY_ID";
const SECRET_ACCESS_KEY: &str = "AWS_SECRET_ACCESS_KEY";
const SESSION_TOKEN: &str = "AWS_SESSION_TOKEN";

/// An access key, its secret and, for temporary credentials, a session token. Its debug output
/// shows none of them.
pub(crate) struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

impl fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AwsCredentials { .. }")
    }
}

impl AwsCredentials {
    /// Reads `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and, if set, `AWS_SESSION_TOKEN` through
    /// `lookup` (the process environment outside tests). A variable that is unset or empty is
    /// missing, and the error names the missing ones.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, DataError> {
        let read = |name: &str| lookup(name).filter(|v| !v.is_empty());
        let (id, secret) = (read(ACCESS_KEY_ID), read(SECRET_ACCESS_KEY));
        match (id, secret) {
            (Some(access_key_id), Some(secret_access_key)) => Ok(AwsCredentials {
                access_key_id,
                secret_access_key,
                session_token: read(SESSION_TOKEN),
            }),
            (id, secret) => {
                let mut missing = Vec::new();
                if id.is_none() {
                    missing.push(ACCESS_KEY_ID);
                }
                if secret.is_none() {
                    missing.push(SECRET_ACCESS_KEY);
                }
                Err(DataError::MissingAwsCredentials { missing })
            }
        }
    }

    /// The session token of temporary credentials, sent as `x-amz-security-token`.
    pub(crate) fn session_token(&self) -> Option<&str> {
        self.session_token.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn new(access_key_id: &str, secret_access_key: &str, token: Option<&str>) -> Self {
        AwsCredentials {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: token.map(Into::into),
        }
    }
}

/// `t` as SigV4's request time, `YYYYMMDDTHHMMSSZ` in UTC.
pub(crate) fn amz_date(t: Millis) -> String {
    let (year, month, day) = civil_from_days(t.div_euclid(DAY));
    let seconds = t.rem_euclid(DAY) / 1000;
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// The `Authorization` header of a GET of the S3 object at `path` in `region`, sent at `amz_date`
/// with `headers`: every header signed, names in lower case and sorted, `host`, `x-amz-date` and
/// `x-amz-content-sha256` among them. `path` is already URI-encoded; the request has no query.
pub(crate) fn authorization(
    credentials: &AwsCredentials,
    region: &str,
    path: &str,
    headers: &[(&str, &str)],
    amz_date: &str,
) -> String {
    debug_assert!(headers.is_sorted_by_key(|(name, _)| *name));
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let signed_headers = headers
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let payload = headers
        .iter()
        .find(|(name, _)| *name == "x-amz-content-sha256")
        .map_or(EMPTY_SHA256, |(_, value)| value);
    let canonical_request =
        format!("GET\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload}");
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let secret = format!("AWS4{}", credentials.secret_access_key);
    let key = [date, region, "s3", "aws4_request"]
        .iter()
        .fold(secret.into_bytes(), |key, part| hmac_sha256(&key, part));
    let signature = hex(&hmac_sha256(&key, &string_to_sign));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed_headers},Signature={signature}",
        credentials.access_key_id
    )
}

fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&key, data.as_bytes()).as_ref().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// AWS's own example keys, published in its documentation; not real credentials.
#[cfg(test)]
pub(crate) const EXAMPLE_KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
#[cfg(test)]
pub(crate) const EXAMPLE_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::parse_date;

    /// The "GET Object" example of the S3 API Reference page "Signature Calculations for the
    /// Authorization Header: Transferring Payload in a Single Chunk (AWS Signature Version 4)":
    /// the first 10 bytes of `test.txt` in `examplebucket`, us-east-1, on 2013-05-24 at midnight
    /// UTC. A 256-bit signature cannot match that page's by chance.
    #[test]
    fn the_s3_get_object_example_signs_to_the_published_signature() {
        let credentials = AwsCredentials::new(EXAMPLE_KEY_ID, EXAMPLE_SECRET, None);
        let date = amz_date(parse_date("2013-05-24").expect("a date"));
        assert_eq!(date, "20130524T000000Z");
        let header = authorization(
            &credentials,
            "us-east-1",
            "/test.txt",
            &[
                ("host", "examplebucket.s3.amazonaws.com"),
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", EMPTY_SHA256),
                ("x-amz-date", &date),
            ],
            &date,
        );
        assert_eq!(
            header,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn the_request_time_is_utc_to_the_second() {
        let t = parse_date("2024-12-31").expect("a date") + 23 * 3_600_000 + 5 * 60_000 + 9_999;
        assert_eq!(amz_date(t), "20241231T230509Z");
    }

    #[test]
    fn credentials_come_through_the_lookup_and_the_missing_ones_are_named() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        let found = AwsCredentials::from_lookup(env(&[
            ("AWS_ACCESS_KEY_ID", EXAMPLE_KEY_ID),
            ("AWS_SECRET_ACCESS_KEY", EXAMPLE_SECRET),
        ]))
        .expect("credentials");
        assert_eq!(found.session_token(), None);
        let temporary = AwsCredentials::from_lookup(env(&[
            ("AWS_ACCESS_KEY_ID", EXAMPLE_KEY_ID),
            ("AWS_SECRET_ACCESS_KEY", EXAMPLE_SECRET),
            ("AWS_SESSION_TOKEN", "token-example"),
        ]))
        .expect("credentials");
        assert_eq!(temporary.session_token(), Some("token-example"));

        let missing = |vars| match AwsCredentials::from_lookup(env(vars)) {
            Err(DataError::MissingAwsCredentials { missing }) => missing,
            other => panic!("{other:?}"),
        };
        assert_eq!(missing(&[]), ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"]);
        // An empty value counts as unset.
        assert_eq!(
            missing(&[
                ("AWS_ACCESS_KEY_ID", EXAMPLE_KEY_ID),
                ("AWS_SECRET_ACCESS_KEY", "")
            ]),
            ["AWS_SECRET_ACCESS_KEY"]
        );
        assert_eq!(
            missing(&[("AWS_SECRET_ACCESS_KEY", EXAMPLE_SECRET)]),
            ["AWS_ACCESS_KEY_ID"]
        );
    }

    /// Neither the credentials' debug output nor the error for a missing one holds any secret text.
    #[test]
    fn no_secret_reaches_debug_output_or_an_error() {
        let credentials =
            AwsCredentials::new(EXAMPLE_KEY_ID, EXAMPLE_SECRET, Some("token-example"));
        let shown = format!("{credentials:?} {credentials:#?}");
        for secret in [
            EXAMPLE_KEY_ID,
            EXAMPLE_SECRET,
            "token-example",
            "AKIA",
            "EXAMPLE",
        ] {
            assert!(!shown.contains(secret), "{shown}");
        }
        let error = AwsCredentials::from_lookup(|name| {
            (name == "AWS_SECRET_ACCESS_KEY").then(|| EXAMPLE_SECRET.to_string())
        })
        .expect_err("the key id is missing");
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains(EXAMPLE_SECRET), "{shown}");
        assert!(shown.contains("AWS_ACCESS_KEY_ID not set"), "{shown}");
        assert!(shown.contains("AWS_SECRET_ACCESS_KEY"), "{shown}");
    }
}
