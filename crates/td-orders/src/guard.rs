//! The two places the route may send a request: the venue's demo-trading service, and the mock
//! venue on 127.0.0.1. Nothing else can be expressed, and the base URL is checked again when a
//! client is built, so a later change cannot widen it by accident.

use reqwest::Url;

use crate::Error;

/// The venue's demo-trading REST host, from <https://bybit-exchange.github.io/docs/v5/demo>.
pub const DEMO_HOST: &str = "api-demo.bybit.com";

/// Where a [`crate::Client`] sends its requests. There is no variant for the venue's real-money
/// hosts or its testnet, and no way to name a host from configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// `https://api-demo.bybit.com`, the venue's demo-trading service.
    Demo,
    /// `http://127.0.0.1:<port>`, the mock venue in tests.
    Loopback(u16),
}

impl Endpoint {
    /// The base URL, checked by [`check_base`].
    pub(crate) fn base_url(self) -> Result<Url, Error> {
        let text = match self {
            Endpoint::Demo => format!("https://{DEMO_HOST}"),
            Endpoint::Loopback(port) => format!("http://127.0.0.1:{port}"),
        };
        check_base(&text)
    }
}

/// Accepts exactly `https://api-demo.bybit.com` (default port) and `http://127.0.0.1:<port>` with
/// a non-zero port, each with no user information, path, query or fragment. Everything else is
/// refused, including the venue's real-money hosts, its testnet, look-alike hosts and other ports
/// or schemes.
pub(crate) fn check_base(text: &str) -> Result<Url, Error> {
    let refuse = |reason: &'static str| Error::HostRefused { reason };
    let url = Url::parse(text).map_err(|_| refuse("not a URL"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(refuse("a base URL may not carry user information"));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(refuse("a base URL may not carry a path, query or fragment"));
    }
    // For http and https the URL parser reads any IPv4 form (`127.1`, `2130706433`) as an address
    // and prints it dotted, so comparing the printed host is exact.
    match (url.scheme(), url.host_str(), url.port()) {
        ("https", Some(DEMO_HOST), None) => Ok(url),
        ("http", Some("127.0.0.1"), Some(port)) if port != 0 => Ok(url),
        _ => Err(refuse(
            "only https://api-demo.bybit.com and http://127.0.0.1:<port> are allowed",
        )),
    }
}

/// Whether `url` is on the same scheme, host and port as `base`: every request URL is checked
/// against the base before it is sent.
pub(crate) fn same_origin(base: &Url, url: &Url) -> bool {
    base.scheme() == url.scheme()
        && base.host() == url.host()
        && base.port() == url.port()
        && url.username().is_empty()
        && url.password().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_endpoints_pass() {
        assert_eq!(
            Endpoint::Demo.base_url().unwrap().as_str(),
            "https://api-demo.bybit.com/"
        );
        assert_eq!(
            Endpoint::Loopback(8123).base_url().unwrap().as_str(),
            "http://127.0.0.1:8123/"
        );
        // An explicit default port is the same URL.
        assert!(check_base("https://api-demo.bybit.com:443").is_ok());
    }

    #[test]
    fn real_money_testnet_and_look_alike_hosts_are_refused() {
        for bad in [
            "https://api.bybit.com",
            "https://api.bytick.com",
            "https://api.bybit.kz",
            "https://api.bybit.tr",
            "https://api.bybit.eu",
            "https://api.bybit.ae",
            "https://api.bybit.id",
            "https://api.bybitgeorgia.ge",
            "https://api.manepa.jp",
            "https://api.spark-fintech.com",
            "https://api-testnet.bybit.com",
            "https://api-demo-testnet.bybit.com",
            "https://api-demo.bybit.com.evil.example",
            "https://api-demo.bybit.com.",
            "https://evil-api-demo.bybit.com",
            "https://api-demo.bybit.co",
            "https://api-demo.bybit.com@evil.example",
            "https://user:pass@api-demo.bybit.com",
            "https://evil.example#@api-demo.bybit.com",
            "https://evil.example/?@api-demo.bybit.com",
            "https://evil.example\\@api-demo.bybit.com",
            "http://api-demo.bybit.com",
            "https://api-demo.bybit.com:8443",
            "https://api-demo.bybit.com:80",
            "wss://api-demo.bybit.com",
            "ftp://api-demo.bybit.com",
            "https://api-demo.bybit.com/v5",
            "https://api-demo.bybit.com/?x=1",
            "https://api-demo.bybit.com/#frag",
            "https://3.114.1.1",
            "api-demo.bybit.com",
            "",
        ] {
            assert!(
                matches!(check_base(bad), Err(Error::HostRefused { .. })),
                "{bad} was not refused"
            );
        }
    }

    #[test]
    fn only_127_0_0_1_with_a_port_over_http_is_a_loopback() {
        for bad in [
            "http://127.0.0.1",
            "http://127.0.0.1:0",
            "https://127.0.0.1:8080",
            "http://127.0.0.2:8080",
            "http://localhost:8080",
            "http://[::1]:8080",
            "http://0.0.0.0:8080",
            "http://10.0.0.1:8080",
            "http://user@127.0.0.1:8080",
            "http://127.0.0.1:8080/path",
            "http://127.0.0.1.evil.example:8080",
        ] {
            assert!(
                matches!(check_base(bad), Err(Error::HostRefused { .. })),
                "{bad} was not refused"
            );
        }
        assert!(check_base("http://127.0.0.1:1").is_ok());
    }

    #[test]
    fn same_origin_compares_scheme_host_and_port() {
        let base = check_base("http://127.0.0.1:9000").unwrap();
        assert!(same_origin(
            &base,
            &Url::parse("http://127.0.0.1:9000/v5/order/create").unwrap()
        ));
        for other in [
            "http://127.0.0.1:9001/v5/order/create",
            "https://127.0.0.1:9000/v5/order/create",
            "http://evil.example:9000/v5/order/create",
            "http://u@127.0.0.1:9000/v5/order/create",
        ] {
            assert!(!same_origin(&base, &Url::parse(other).unwrap()), "{other}");
        }
    }
}
