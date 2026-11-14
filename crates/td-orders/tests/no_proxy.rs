//! No system proxy is used. With every proxy variable pointing at a local listener, the client
//! still talks to the mock directly and the listener is never contacted. It is its own test
//! binary because it sets process-wide environment variables.

use std::net::TcpListener;
use std::time::Duration;

use td_mock_bybit::{MockVenue, TEST_KEY, TEST_SECRET};
use td_orders::{Client, DemoKeys, Endpoint, Timing};

#[test]
fn proxy_variables_are_ignored() {
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let url = format!("http://{}", proxy.local_addr().unwrap());
    // SAFETY: this binary's only test sets them before it starts any other thread.
    unsafe {
        for name in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            std::env::set_var(name, &url);
        }
        for name in ["NO_PROXY", "no_proxy", "REQUEST_METHOD"] {
            std::env::remove_var(name);
        }
    }
    let mock = MockVenue::start();
    let timing = Timing {
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_secs(2),
        recv_window: Duration::from_secs(5),
        settle: Duration::from_millis(50),
        operation_budget: Duration::from_secs(10),
    };
    let keys = DemoKeys::for_loopback(TEST_KEY, TEST_SECRET).unwrap();
    let client = Client::with_timing(Endpoint::Loopback(mock.port()), keys, timing);
    assert!(client.is_ok(), "{:?}", client.err());
    assert_eq!(mock.request_count("/v5/market/time"), 1);
    assert!(proxy.accept().is_err(), "the proxy listener was contacted");
}
