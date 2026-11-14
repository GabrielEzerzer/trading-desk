//! A stateful mock of the venue's v5 REST API on 127.0.0.1, for `td-orders`' tests. Test harness,
//! never app code.
//!
//! It serves the endpoints the order route uses (server time, instruments info, tickers, create,
//! cancel, amend, real-time orders, order history, executions, positions, wallet balance) over
//! real HTTP, so the tests run
//! the real client with its signing. It checks the API key, the HMAC signature over the exact query
//! string or body, and the receive window; enforces the client-id rule ([`IdRule`]); applies the
//! instrument's lot, tick, minimum quantity and minimum notional filters; fills market orders at
//! the price the test sets; holds reduce-only stops and triggers them when the test moves the
//! price; and keeps positions, executions with fees, and the USDT wallet.
//!
//! A test can inject a [`Fault`] into the next request on a path, or into every one, and can make
//! the order lists lag ([`MockVenue::set_lag`]). Response shapes follow the documented examples
//! kept in `fixtures/`.

// The order JSON has more fields than serde_json's json! macro expands at the default limit.
#![recursion_limit = "256"]

mod http;
mod venue;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use td_core::{Instrument, Millis, Side};

use crate::http::Server;
use crate::venue::{Market, State};

/// The fake API key tests use. It can never work against the real venue.
pub const TEST_KEY: &str = "td-mock-key-not-a-real-key";
/// The fake API secret tests use.
pub const TEST_SECRET: &str = "td-mock-secret-not-a-real-secret";

/// Which orders an `orderLinkId` must differ from. The venue's documentation says the id is
/// "always unique" without saying over which orders; [`IdRule::Always`] is that reading and the
/// default, and [`IdRule::OpenOnly`] is the weaker one, for checking that the client does not
/// depend on the stronger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdRule {
    /// Every order the account holds, open or closed.
    Always,
    /// Open orders only.
    OpenOnly,
}

/// A failure injected into a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// Apply the request, then hold the answer this long (past the client's timeout).
    Hang(Duration),
    /// Close the connection after reading the request, without applying it.
    DropBeforeApply,
    /// Apply the request, then close the connection without answering.
    DropAfterApply,
    /// Answer `retCode` 10006 with `X-Bapi-Limit-Status: 0` and a reset this far ahead.
    RateLimit { reset_after_ms: i64 },
    /// Answer HTTP 429, the venue's system-level frequency protection.
    Http429,
    /// Answer HTTP 403 "access too frequent", the IP-level block.
    IpBlock,
    /// Answer `retCode` 10002 whatever the timestamp.
    TimestampReject,
    /// Answer HTTP 502 without applying the request.
    ServerError,
    /// Apply the request, then answer HTTP 502.
    ServerErrorAfterApply,
    /// Answer 302 with this `Location`.
    Redirect(String),
    /// Answer this `retCode` without applying the request, with the venue's documented message
    /// for 40004, 10014, 110079, 429 and 10429.
    Answer(i64),
    /// Check the request, then answer `retCode` `answer` (or close the connection when `None`)
    /// at once and apply the request `after` later, as a venue still processing it would.
    ApplyLater {
        after: Duration,
        answer: Option<i64>,
    },
    /// Record the order a create asks for as `Rejected` with this reject reason, without filling
    /// it, then close the connection without answering.
    RejectThenDrop(String),
    /// Apply the request, then answer with its JSON padded past 5 MiB and gzip-compressed, so the
    /// compressed answer is small and the decompressed one is not.
    HugeBody,
}

/// An order as the mock holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct MockOrder {
    pub order_id: String,
    pub link_id: String,
    pub symbol: String,
    pub side: Side,
    pub status: String,
    pub qty: f64,
    pub filled_qty: f64,
    pub trigger: Option<f64>,
    pub reduce_only: bool,
}

/// An execution as the mock holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct MockExecution {
    pub exec_id: String,
    pub link_id: String,
    pub exec_type: String,
    pub price: f64,
    pub qty: f64,
    pub fee: f64,
    pub time: Millis,
}

/// A request the mock received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockRequest {
    pub method: String,
    pub path: String,
    /// Header names in lowercase, with their values.
    pub headers: Vec<(String, String)>,
}

/// The running mock venue. It stops when dropped.
pub struct MockVenue {
    state: Arc<Mutex<State>>,
    server: Server,
}

impl MockVenue {
    /// Starts a mock that accepts [`TEST_KEY`] and [`TEST_SECRET`], with a 100 000 USDT wallet.
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(State::new(TEST_KEY, TEST_SECRET)));
        let handler_state = state.clone();
        let server = Server::start(Arc::new(move |req| venue::handle(&handler_state, req)));
        MockVenue { state, server }
    }

    pub fn port(&self) -> u16 {
        self.server.port
    }

    /// Lists an instrument with its filters and fees, at `price`.
    pub fn add_instrument(&self, instrument: &Instrument, price: f64) {
        self.state.lock().unwrap().markets.insert(
            instrument.id.symbol.clone(),
            Market {
                instrument: instrument.clone(),
                price,
            },
        );
    }

    /// Moves the last price, triggering the resting stops it crosses.
    pub fn set_price(&self, symbol: &str, price: f64) {
        self.state.lock().unwrap().set_price(symbol, price);
    }

    /// Moves the mock's clock this many milliseconds ahead of the real one (negative: behind).
    pub fn set_skew(&self, ms: i64) {
        self.state.lock().unwrap().skew_ms = ms;
    }

    /// The mock's current time.
    pub fn now(&self) -> Millis {
        self.state.lock().unwrap().now()
    }

    /// Puts the account in hedge mode (positions on `positionIdx` 1 and 2).
    pub fn set_hedge_mode(&self, on: bool) {
        self.state.lock().unwrap().hedge = on;
    }

    pub fn set_id_rule(&self, rule: IdRule) {
        self.state.lock().unwrap().id_rule = rule;
    }

    /// Keeps closed orders out of the order history for this long after they close.
    pub fn set_history_delay(&self, ms: i64) {
        self.state.lock().unwrap().history_delay_ms = ms;
    }

    /// Makes the real-time orders, the order history and the executions show what a create,
    /// cancel, amend, fill or trigger did only this long after it happened: until then a new order
    /// or execution is missing from them and a changed order shows as it was. Positions and the
    /// wallet do not lag, and the client-id rule and cancel and amend act on the current state.
    pub fn set_lag(&self, ms: i64) {
        self.state.lock().unwrap().lag_ms = ms;
    }

    /// Has the account borrow this much USDT on spot, which the venue's coin `equity` subtracts
    /// and `walletBalance` does not.
    pub fn set_spot_borrow(&self, amount: f64) {
        self.state.lock().unwrap().spot_borrow = amount;
    }

    /// Fills `qty` of the open order with `link_id` at the last price and cancels the rest, as the
    /// venue does to an order cancelled after a partial fill.
    pub fn cancel_after_partial_fill(&self, link_id: &str, qty: f64) {
        self.state
            .lock()
            .unwrap()
            .cancel_after_partial_fill(link_id, qty);
    }

    /// Turns the real-time endpoint's list of recent closed orders on or off; off is the venue
    /// just after a restart, when closed orders show only in the (possibly delayed) history.
    pub fn set_closed_cache(&self, on: bool) {
        self.state.lock().unwrap().closed_cache = on;
    }

    /// Makes every error message carry the API key and the request's signature.
    pub fn set_echo_credentials(&self, on: bool) {
        self.state.lock().unwrap().echo_credentials = on;
    }

    /// Injects `fault` into the next request on `path`.
    pub fn inject(&self, path: &str, fault: Fault) {
        self.state
            .lock()
            .unwrap()
            .faults_once
            .push((path.into(), fault));
    }

    /// Injects `fault` into every request on `path` until [`MockVenue::clear_faults`].
    pub fn inject_always(&self, path: &str, fault: Fault) {
        self.state
            .lock()
            .unwrap()
            .faults_always
            .push((path.into(), fault));
    }

    pub fn clear_faults(&self) {
        let mut s = self.state.lock().unwrap();
        s.faults_once.clear();
        s.faults_always.clear();
    }

    /// Adds a funding fee execution on `symbol` at the current time.
    pub fn add_funding(&self, symbol: &str, qty: f64, fee: f64) {
        let mut s = self.state.lock().unwrap();
        let now = s.now();
        s.add_exec(symbol, "Funding", qty, fee, now);
    }

    /// Adds `n` trade executions on `symbol`, the first at `start` and each `step_ms` later.
    pub fn seed_trades(&self, symbol: &str, n: usize, start: Millis, step_ms: i64) {
        let mut s = self.state.lock().unwrap();
        for i in 0..n {
            s.add_exec(symbol, "Trade", 0.001, 0.01, start + i as i64 * step_ms);
        }
    }

    pub fn orders(&self) -> Vec<MockOrder> {
        let s = self.state.lock().unwrap();
        s.orders
            .iter()
            .map(|o| MockOrder {
                order_id: o.order_id.clone(),
                link_id: o.link_id.clone(),
                symbol: o.symbol.clone(),
                side: o.side,
                status: o.status.to_string(),
                qty: o.qty,
                filled_qty: o.cum_qty,
                trigger: o.trigger,
                reduce_only: o.reduce_only,
            })
            .collect()
    }

    /// The orders carrying `link_id`.
    pub fn orders_with_link_id(&self, link_id: &str) -> Vec<MockOrder> {
        self.orders()
            .into_iter()
            .filter(|o| o.link_id == link_id)
            .collect()
    }

    pub fn executions(&self) -> Vec<MockExecution> {
        let s = self.state.lock().unwrap();
        s.execs
            .iter()
            .map(|e| MockExecution {
                exec_id: e.exec_id.clone(),
                link_id: e.link_id.clone(),
                exec_type: e.exec_type.to_string(),
                price: e.price,
                qty: e.qty,
                fee: e.fee,
                time: e.time,
            })
            .collect()
    }

    /// The signed position size on `symbol`.
    pub fn position(&self, symbol: &str) -> f64 {
        self.state
            .lock()
            .unwrap()
            .positions
            .get(symbol)
            .map_or(0.0, |p| p.size)
    }

    /// The USDT wallet balance.
    pub fn wallet_balance(&self) -> f64 {
        self.state.lock().unwrap().wallet
    }

    /// Every request received, in order.
    pub fn requests(&self) -> Vec<MockRequest> {
        self.state
            .lock()
            .unwrap()
            .log
            .iter()
            .map(|l| MockRequest {
                method: l.method.clone(),
                path: l.path.clone(),
                headers: l.headers.clone(),
            })
            .collect()
    }

    /// How many requests arrived on `path`.
    pub fn request_count(&self, path: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|l| l.path == path)
            .count()
    }
}
