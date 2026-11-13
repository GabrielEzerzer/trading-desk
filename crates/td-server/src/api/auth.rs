//! Who may talk to the server: a loopback peer, addressing the server by its loopback name and
//! port, holding the per-launch token.
//!
//! The token travels as `Authorization: Bearer <token>`. Browsers cannot set headers on a
//! WebSocket, so an upgrade request to `/ws`, and only there, may carry it as the subprotocol
//! `td-token.<token>` in `Sec-WebSocket-Protocol` instead; the `/ws` handler echoes that
//! subprotocol back.
//!
//! The apps send their HTTP calls and hold their one WebSocket in Electron's main process, which
//! keeps the token; the renderer never receives it. If the renderer ever opens a socket of its
//! own, the main process must hand it a WebSocket-only token or a single-use ticket, never the
//! API token.
//!
//! The Host check stops DNS rebinding: a web page whose domain was re-pointed at 127.0.0.1 still
//! sends its own domain in `Host`. No CORS headers are sent, so browsers keep other origins from
//! reading responses.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::rejection::ExtensionRejection;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{AUTHORIZATION, HOST, SEC_WEBSOCKET_PROTOCOL, UPGRADE};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use super::AppState;
use super::error::ApiError;

/// The prefix of the WebSocket subprotocol that carries the token.
pub const WS_TOKEN_PROTOCOL_PREFIX: &str = "td-token.";

/// The one path where the subprotocol token counts.
const WS_PATH: &str = "/ws";

/// The secret a client must present. New for every launch unless `TD_TOKEN` fixes it.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

impl Token {
    /// 32 random bytes from the operating system's generator, as 64 lowercase hex digits.
    pub fn generate() -> Result<Token, getrandom::Error> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)?;
        Ok(Token(bytes.iter().map(|b| format!("{b:02x}")).collect()))
    }

    /// A token given by the caller (`TD_TOKEN`). It must be at least 32 characters of letters,
    /// digits, `-`, `_` or `.`, so it fits in a header and a subprotocol name and is not trivially
    /// guessable.
    pub fn from_value(value: &str) -> Result<Token, String> {
        let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
        if value.len() < 32 || !value.chars().all(allowed) {
            return Err(
                "the token must be at least 32 characters of letters, digits, '-', '_' or '.'"
                    .into(),
            );
        }
        Ok(Token(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Compares in time that does not depend on where the strings differ.
    pub fn matches(&self, candidate: &str) -> bool {
        self.0.as_bytes().ct_eq(candidate.as_bytes()).into()
    }
}

/// Decides whether a request may go through. Checked in this order: the peer must be a loopback
/// address (403 `forbidden_peer`), `Host` must be `127.0.0.1:<port>` or `localhost:<port>` (403
/// `forbidden_host`), and the token must match (401 `unauthorized`). A request with no known peer
/// address is refused. `path` is the request's path, which decides whether a subprotocol token
/// counts.
fn check_request(
    token: &Token,
    port: u16,
    peer: Option<SocketAddr>,
    path: &str,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    match peer {
        Some(addr) if addr.ip().to_canonical().is_loopback() => {}
        _ => {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden_peer",
                "only connections from this machine are accepted",
            ));
        }
    }
    let host = headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let host_ok = [format!("127.0.0.1:{port}"), format!("localhost:{port}")]
        .iter()
        .any(|allowed| host.eq_ignore_ascii_case(allowed));
    if !host_ok {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden_host",
            format!("the Host header must be 127.0.0.1:{port} or localhost:{port}"),
        ));
    }
    if presented_tokens(headers, path == WS_PATH).any(|t| token.matches(t)) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a valid token is required: send Authorization: Bearer <token>",
        ))
    }
}

/// The tokens a request presents: the bearer token, and on a WebSocket upgrade to `/ws` (`ws`)
/// each offered `td-token.` subprotocol.
fn presented_tokens(headers: &HeaderMap, ws: bool) -> impl Iterator<Item = &str> {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
        });
    let upgrade = ws
        && headers
            .get(UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let protocols = headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter(move |_| upgrade)
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|p| p.trim().strip_prefix(WS_TOKEN_PROTOCOL_PREFIX));
    bearer.into_iter().chain(protocols)
}

/// The middleware in front of every route, the fallbacks included.
pub(super) async fn guard(
    State(state): State<Arc<AppState>>,
    connect: Result<ConnectInfo<SocketAddr>, ExtensionRejection>,
    request: Request,
    next: Next,
) -> Response {
    let peer = connect.ok().map(|ConnectInfo(addr)| addr);
    let path = request.uri().path();
    match check_request(&state.token, state.port, peer, path, request.headers()) {
        Ok(()) => next.run(request).await,
        Err(e) => {
            eprintln!(
                "td-server: refused {} {} from {peer:?}: {}",
                request.method(),
                request.uri().path(),
                e.body.code
            );
            e.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const PORT: u16 = 4321;

    fn token() -> Token {
        Token::from_value(&"a".repeat(64)).expect("valid")
    }

    fn headers(pairs: &[(axum::http::HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(name.clone(), HeaderValue::from_str(value).expect("ascii"));
        }
        map
    }

    fn loopback() -> Option<SocketAddr> {
        Some(SocketAddr::from(([127, 0, 0, 1], 50000)))
    }

    fn code(result: Result<(), ApiError>) -> Option<String> {
        result.err().map(|e| e.body.code)
    }

    #[test]
    fn generated_tokens_are_64_hex_digits_and_differ() {
        let (a, b) = (
            Token::generate().expect("rng"),
            Token::generate().expect("rng"),
        );
        assert_eq!(a.as_str().len(), 64);
        assert!(a.as_str().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert!(Token::from_value("short").is_err());
        assert!(Token::from_value(&format!("{} x", "a".repeat(40))).is_err());
    }

    #[test]
    fn a_bearer_token_from_loopback_with_the_right_host_passes() {
        let h = headers(&[
            (HOST, "127.0.0.1:4321"),
            (AUTHORIZATION, &format!("Bearer {}", "a".repeat(64))),
        ]);
        assert_eq!(
            check_request(&token(), PORT, loopback(), "/health", &h),
            Ok(())
        );
        let h = headers(&[
            (HOST, "LOCALHOST:4321"),
            (AUTHORIZATION, &format!("bearer {}", "a".repeat(64))),
        ]);
        assert_eq!(
            check_request(&token(), PORT, loopback(), "/health", &h),
            Ok(())
        );
    }

    #[test]
    fn missing_or_wrong_tokens_are_401() {
        let no_token = headers(&[(HOST, "127.0.0.1:4321")]);
        assert_eq!(
            code(check_request(
                &token(),
                PORT,
                loopback(),
                "/health",
                &no_token
            ))
            .as_deref(),
            Some("unauthorized")
        );
        let wrong = headers(&[
            (HOST, "127.0.0.1:4321"),
            (AUTHORIZATION, &format!("Bearer {}", "b".repeat(64))),
        ]);
        assert_eq!(
            code(check_request(&token(), PORT, loopback(), "/health", &wrong)).as_deref(),
            Some("unauthorized")
        );
        let basic = headers(&[
            (HOST, "127.0.0.1:4321"),
            (AUTHORIZATION, &format!("Basic {}", "a".repeat(64))),
        ]);
        assert!(check_request(&token(), PORT, loopback(), "/health", &basic).is_err());
    }

    #[test]
    fn the_subprotocol_token_counts_only_on_a_websocket_upgrade_to_ws() {
        let protocol = format!("other, td-token.{}", "a".repeat(64));
        let upgrade = headers(&[
            (HOST, "127.0.0.1:4321"),
            (UPGRADE, "websocket"),
            (SEC_WEBSOCKET_PROTOCOL, &protocol),
        ]);
        assert_eq!(
            check_request(&token(), PORT, loopback(), "/ws", &upgrade),
            Ok(())
        );
        let plain = headers(&[
            (HOST, "127.0.0.1:4321"),
            (SEC_WEBSOCKET_PROTOCOL, &protocol),
        ]);
        assert!(check_request(&token(), PORT, loopback(), "/ws", &plain).is_err());
        // Anywhere but /ws the subprotocol token does not count, upgrade or not.
        for path in ["/jobs", "/data/download", "/ws/", "/WS"] {
            assert_eq!(
                code(check_request(&token(), PORT, loopback(), path, &upgrade)).as_deref(),
                Some("unauthorized"),
                "{path}"
            );
        }
    }

    #[test]
    fn foreign_hosts_and_peers_are_403_even_with_the_token() {
        let auth = format!("Bearer {}", "a".repeat(64));
        for host in ["evil.example:4321", "127.0.0.1:9999", "127.0.0.1", ""] {
            let h = headers(&[(HOST, host), (AUTHORIZATION, &auth)]);
            assert_eq!(
                code(check_request(&token(), PORT, loopback(), "/health", &h)).as_deref(),
                Some("forbidden_host"),
                "{host}"
            );
        }
        let h = headers(&[(HOST, "127.0.0.1:4321"), (AUTHORIZATION, &auth)]);
        for peer in [
            Some(SocketAddr::from(([192, 168, 1, 5], 50000))),
            Some(SocketAddr::from(([10, 0, 0, 1], 50000))),
            None,
        ] {
            assert_eq!(
                code(check_request(&token(), PORT, peer, "/health", &h)).as_deref(),
                Some("forbidden_peer"),
                "{peer:?}"
            );
        }
    }
}
