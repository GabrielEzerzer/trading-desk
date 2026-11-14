//! The mock venue's state and its v5 endpoints.
//!
//! Rules follow the v5 pages read on 2026-11-14 (URLs in `td-orders`' module docs and in
//! `fixtures/README.md`). Where the documentation is silent the mock picks a rule and says so here:
//! - A reduce-only conditional order may be placed with no position; if it triggers with nothing
//!   to reduce it ends `Deactivated`.
//! - A cancelled conditional order that never triggered ends `Deactivated`, a cancelled plain order
//!   `Cancelled`.
//! - Market orders fill in full at the price the test set, as takers, at the instrument's taker
//!   fee. A reduce-only market order is cut to the position's size.
//! - The minimum notional applies to orders that are not reduce-only.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use td_core::{Instrument, Rounding, Side};

use crate::http::{Reply, Request};
use crate::{Fault, IdRule};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const WEEK_MS: i64 = 7 * DAY_MS;

type VenueResult = Result<Value, (i64, String)>;

pub(crate) struct Market {
    pub instrument: Instrument,
    pub price: f64,
}

#[derive(Clone)]
pub(crate) struct Order {
    pub order_id: String,
    pub link_id: String,
    pub symbol: String,
    pub side: Side,
    pub qty: f64,
    pub status: &'static str,
    pub trigger: Option<f64>,
    pub trigger_direction: i64,
    pub reduce_only: bool,
    pub cum_qty: f64,
    pub cum_value: f64,
    pub cum_fee: f64,
    pub price_on_created: f64,
    pub created: i64,
    pub updated: i64,
}

impl Order {
    pub fn is_open(&self) -> bool {
        matches!(self.status, "New" | "PartiallyFilled" | "Untriggered")
    }
}

#[derive(Clone)]
pub(crate) struct Exec {
    pub exec_id: String,
    pub order_id: String,
    pub link_id: String,
    pub symbol: String,
    pub side: Side,
    pub price: f64,
    pub qty: f64,
    pub fee: f64,
    pub fee_rate: f64,
    pub exec_type: &'static str,
    pub order_qty: f64,
    pub time: i64,
    pub seq: u64,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct Pos {
    /// Signed size.
    pub size: f64,
    pub avg: f64,
}

pub(crate) struct Logged {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

pub(crate) struct State {
    pub key: String,
    pub secret: String,
    pub markets: HashMap<String, Market>,
    pub orders: Vec<Order>,
    pub execs: Vec<Exec>,
    pub positions: HashMap<String, Pos>,
    pub wallet: f64,
    pub cum_realised: f64,
    pub skew_ms: i64,
    pub hedge: bool,
    pub id_rule: IdRule,
    pub history_delay_ms: i64,
    pub echo_credentials: bool,
    /// Whether real-time queries by id list closed orders (the venue's recent-closed cache, which
    /// it clears on a restart).
    pub closed_cache: bool,
    pub faults_once: Vec<(String, Fault)>,
    pub faults_always: Vec<(String, Fault)>,
    pub log: Vec<Logged>,
    pub next: u64,
}

impl State {
    pub fn new(key: &str, secret: &str) -> Self {
        State {
            key: key.into(),
            secret: secret.into(),
            markets: HashMap::new(),
            orders: Vec::new(),
            execs: Vec::new(),
            positions: HashMap::new(),
            wallet: 100_000.0,
            cum_realised: 0.0,
            skew_ms: 0,
            hedge: false,
            id_rule: IdRule::Always,
            history_delay_ms: 0,
            echo_credentials: false,
            closed_cache: true,
            faults_once: Vec::new(),
            faults_always: Vec::new(),
            log: Vec::new(),
            next: 1,
        }
    }

    pub fn now(&self) -> i64 {
        let local = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        local + self.skew_ms
    }

    fn next_id(&mut self) -> String {
        let n = self.next;
        self.next += 1;
        format!("{n:08x}-7d3e-4c1a-9b2f-{:012x}", n * 7919)
    }

    fn take_fault(&mut self, path: &str) -> Option<Fault> {
        if let Some(i) = self.faults_once.iter().position(|(p, _)| p == path) {
            return Some(self.faults_once.remove(i).1);
        }
        self.faults_always
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, f)| f.clone())
    }

    /// Moves the price and triggers resting stops it crosses.
    pub fn set_price(&mut self, symbol: &str, price: f64) {
        self.markets.get_mut(symbol).expect("unknown symbol").price = price;
        for i in 0..self.orders.len() {
            let o = &self.orders[i];
            if o.symbol != symbol || o.status != "Untriggered" {
                continue;
            }
            let t = o.trigger.unwrap();
            let hit = (o.trigger_direction == 1 && price >= t)
                || (o.trigger_direction == 2 && price <= t);
            if !hit {
                continue;
            }
            let qty = if o.reduce_only {
                self.reducible(symbol, o.side).min(o.qty)
            } else {
                o.qty
            };
            if qty <= 0.0 {
                let now = self.now();
                let o = &mut self.orders[i];
                o.status = "Deactivated";
                o.updated = now;
            } else {
                self.fill(i, price, qty);
            }
        }
    }

    /// How much an order on `side` can reduce the position.
    fn reducible(&self, symbol: &str, side: Side) -> f64 {
        let size = self.positions.get(symbol).map_or(0.0, |p| p.size);
        if size * side.sign() < 0.0 {
            size.abs()
        } else {
            0.0
        }
    }

    fn fill(&mut self, index: usize, price: f64, qty: f64) {
        let now = self.now();
        let (symbol, side, order_id, link_id, order_qty) = {
            let o = &self.orders[index];
            (
                o.symbol.clone(),
                o.side,
                o.order_id.clone(),
                o.link_id.clone(),
                o.qty,
            )
        };
        let inst = self.markets[&symbol].instrument.clone();
        let fee_rate = inst.fees.taker;
        let fee = round8(price * qty * fee_rate);
        let pos = self.positions.entry(symbol.clone()).or_default();
        let signed = side.sign() * qty;
        let mut realised = 0.0;
        if pos.size == 0.0 || pos.size.signum() == signed.signum() {
            let total = pos.size.abs() + qty;
            pos.avg = (pos.avg * pos.size.abs() + price * qty) / total;
        } else {
            let closed = qty.min(pos.size.abs());
            realised = (price - pos.avg) * closed * pos.size.signum();
            if qty > pos.size.abs() {
                pos.avg = price;
            }
        }
        pos.size = snap(pos.size + signed, inst.lot_size);
        if pos.size == 0.0 {
            pos.avg = 0.0;
        }
        self.wallet += realised - fee;
        self.cum_realised += realised - fee;
        let o = &mut self.orders[index];
        o.cum_qty = snap(o.cum_qty + qty, inst.lot_size);
        o.cum_value += price * qty;
        o.cum_fee += fee;
        o.status = "Filled";
        o.updated = now;
        let exec_id = self.next_id();
        let seq = self.next;
        self.execs.push(Exec {
            exec_id,
            order_id,
            link_id,
            symbol,
            side,
            price,
            qty,
            fee,
            fee_rate,
            exec_type: "Trade",
            order_qty,
            time: now,
            seq,
        });
    }

    pub fn add_exec(
        &mut self,
        symbol: &str,
        exec_type: &'static str,
        qty: f64,
        fee: f64,
        time: i64,
    ) {
        let price = self.markets[symbol].price;
        let exec_id = self.next_id();
        let link_id = if exec_type == "Trade" {
            format!("seed-{}", self.next)
        } else {
            String::new()
        };
        let seq = self.next;
        self.execs.push(Exec {
            exec_id,
            order_id: String::new(),
            link_id,
            symbol: symbol.into(),
            side: Side::Buy,
            price,
            qty,
            fee,
            fee_rate: 0.0001,
            exec_type,
            order_qty: 0.0,
            time,
            seq,
        });
        self.wallet -= fee;
    }

    fn upnl(&self) -> f64 {
        self.positions
            .iter()
            .map(|(s, p)| (self.markets[s].price - p.avg) * p.size)
            .sum()
    }
}

/// Answers one request: faults first, then authentication, then the endpoint.
pub(crate) fn handle(state: &std::sync::Mutex<State>, req: Request) -> Reply {
    let fault = {
        let mut s = state.lock().unwrap();
        s.log.push(Logged {
            method: req.method.clone(),
            path: req.path.clone(),
            headers: req.headers.clone(),
        });
        s.take_fault(&req.path)
    };
    let now = state.lock().unwrap().now();
    match &fault {
        Some(Fault::Redirect(location)) => {
            return respond(
                302,
                vec![("Location".into(), location.clone())],
                String::new(),
            );
        }
        Some(Fault::IpBlock) => {
            return respond(403, vec![], "access too frequent".into());
        }
        Some(Fault::Http429) => {
            return respond(429, vec![], "Too Many Requests".into());
        }
        Some(Fault::RateLimit { reset_after_ms }) => {
            let headers = vec![
                ("X-Bapi-Limit".into(), "10".into()),
                ("X-Bapi-Limit-Status".into(), "0".into()),
                (
                    "X-Bapi-Limit-Reset-Timestamp".into(),
                    (now + reset_after_ms).to_string(),
                ),
            ];
            let body = envelope(10006, "Too many visits!", json!({}), now);
            return respond(200, headers, body.to_string());
        }
        Some(Fault::TimestampReject) => {
            let body = envelope(
                10002,
                "invalid request, please check your server timestamp or recv_window param",
                json!({}),
                now,
            );
            return respond(200, limit_headers(now), body.to_string());
        }
        Some(Fault::ServerError) => {
            return respond(502, vec![], "<html>502 Bad Gateway</html>".into());
        }
        Some(Fault::DropBeforeApply) => return Reply::Drop,
        _ => {}
    }
    let (status, body) = state.lock().unwrap().dispatch(&req);
    match fault {
        Some(Fault::DropAfterApply) => Reply::Drop,
        Some(Fault::ServerErrorAfterApply) => {
            respond(502, vec![], "<html>502 Bad Gateway</html>".into())
        }
        Some(Fault::Hang(delay)) => Reply::Respond {
            status,
            headers: limit_headers(now),
            body: body.to_string(),
            delay,
        },
        _ => respond(status, limit_headers(now), body.to_string()),
    }
}

fn respond(status: u16, mut headers: Vec<(String, String)>, body: String) -> Reply {
    if !body.is_empty() && status == 200 {
        headers.push(("Content-Type".into(), "application/json".into()));
    }
    Reply::Respond {
        status,
        headers,
        body,
        delay: Duration::ZERO,
    }
}

fn limit_headers(now: i64) -> Vec<(String, String)> {
    vec![
        ("X-Bapi-Limit".into(), "10".into()),
        ("X-Bapi-Limit-Status".into(), "9".into()),
        ("X-Bapi-Limit-Reset-Timestamp".into(), now.to_string()),
    ]
}

fn envelope(code: i64, msg: &str, result: Value, now: i64) -> Value {
    json!({
        "retCode": code,
        "retMsg": msg,
        "result": result,
        "retExtInfo": {},
        "time": now,
    })
}

impl State {
    fn dispatch(&mut self, req: &Request) -> (u16, Value) {
        let now = self.now();
        if req.method == "GET" && req.path == "/v5/market/time" {
            let nanos = now as i128 * 1_000_000 + 123_456;
            let result = json!({
                "timeSecond": (now / 1000).to_string(),
                "timeNano": nanos.to_string(),
            });
            return (200, envelope(0, "OK", result, now));
        }
        if let Err((code, msg)) = self.authenticate(req) {
            return (200, envelope(code, &self.echo(&msg, req), json!({}), now));
        }
        let params: HashMap<String, String> = req.params().into_iter().collect();
        let body: Value = if req.method == "POST" {
            match serde_json::from_str(&req.body) {
                Ok(v) => v,
                Err(_) => {
                    let msg = self.echo("request body is not JSON", req);
                    return (200, envelope(10001, &msg, json!({}), now));
                }
            }
        } else {
            Value::Null
        };
        let result = match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/v5/order/create") => self.create(&body),
            ("POST", "/v5/order/cancel") => self.cancel(&body),
            ("POST", "/v5/order/amend") => self.amend(&body),
            ("GET", "/v5/order/realtime") => self.realtime(&params),
            ("GET", "/v5/order/history") => self.history(&params),
            ("GET", "/v5/execution/list") => self.executions(&params),
            ("GET", "/v5/position/list") => self.position_list(&params),
            ("GET", "/v5/account/wallet-balance") => self.wallet_balance(&params),
            _ => return (404, envelope(10017, "Route not found.", json!({}), now)),
        };
        match result {
            Ok(v) => (200, envelope(0, "OK", v, now)),
            Err((code, msg)) => (200, envelope(code, &self.echo(&msg, req), json!({}), now)),
        }
    }

    /// With echo on, every error message carries the key and the signature, as a careless venue
    /// might, so the client's scrubbing is tested.
    fn echo(&self, msg: &str, req: &Request) -> String {
        if self.echo_credentials {
            let sign = req.header("x-bapi-sign").unwrap_or("");
            format!("{msg} apiKey[{}] sign[{sign}]", self.key)
        } else {
            msg.to_string()
        }
    }

    fn authenticate(&self, req: &Request) -> Result<(), (i64, String)> {
        let key = req.header("x-bapi-api-key").unwrap_or("");
        if key != self.key {
            return Err((10003, "API key is invalid.".into()));
        }
        let ts: i64 = req
            .header("x-bapi-timestamp")
            .and_then(|v| v.parse().ok())
            .ok_or((10001, "params error: X-BAPI-TIMESTAMP".to_string()))?;
        let recv: i64 = req
            .header("x-bapi-recv-window")
            .and_then(|v| v.parse().ok())
            .unwrap_or(5000);
        let now = self.now();
        if !(now - recv <= ts && ts < now + 1000) {
            return Err((
                10002,
                format!(
                    "invalid request, please check your server timestamp or recv_window param. \
                     req_timestamp[{ts}],server_timestamp[{now}],recv_window[{recv}]"
                ),
            ));
        }
        let payload = if req.method == "POST" {
            if req.header("content-type") != Some("application/json") {
                return Err((10001, "Content-Type must be application/json".into()));
            }
            req.body.as_str()
        } else {
            req.query.as_str()
        };
        let expected = sign(&self.secret, &format!("{ts}{}{recv}{payload}", self.key));
        if req.header("x-bapi-sign") != Some(expected.as_str()) {
            // The real venue echoes the signed text, key included.
            return Err((
                10004,
                format!("error sign! origin_string[{ts}{}{recv}{payload}]", self.key),
            ));
        }
        Ok(())
    }

    fn market(&self, symbol: &str) -> Result<&Market, (i64, String)> {
        self.markets
            .get(symbol)
            .ok_or((10001, "params error: symbol invalid".to_string()))
    }

    fn create(&mut self, b: &Value) -> VenueResult {
        linear(b.get("category").and_then(Value::as_str))?;
        let symbol = str_field(b, "symbol")?;
        let (inst, price) = {
            let m = self.market(&symbol)?;
            (m.instrument.clone(), m.price)
        };
        let side = match b.get("side").and_then(Value::as_str) {
            Some("Buy") => Side::Buy,
            Some("Sell") => Side::Sell,
            _ => return Err((10001, "params error: side invalid".into())),
        };
        if b.get("orderType").and_then(Value::as_str) != Some("Market") {
            return Err((
                10001,
                "the mock venue supports orderType Market only".into(),
            ));
        }
        let qty = on_grid(&str_field(b, "qty")?, inst.lot_size)
            .ok_or((10001, "Qty invalid".to_string()))?;
        if qty < inst.min_qty * (1.0 - 1e-9) {
            return Err((
                10001,
                "Qty invalid: below the minimum order quantity".into(),
            ));
        }
        let idx = b.get("positionIdx").and_then(Value::as_i64).unwrap_or(0);
        if (self.hedge && idx == 0) || (!self.hedge && idx != 0) {
            return Err((10001, "position idx not match position mode".into()));
        }
        let link = b
            .get("orderLinkId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !link.is_empty() {
            let valid = link.len() <= 36
                && link
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_');
            if !valid {
                return Err((10001, "params error: orderLinkId invalid".into()));
            }
            let taken = self.orders.iter().any(|o| {
                o.link_id == link
                    && match self.id_rule {
                        IdRule::Always => true,
                        IdRule::OpenOnly => o.is_open(),
                    }
            });
            if taken {
                return Err((110072, "OrderLinkedID is duplicate".into()));
            }
        }
        let reduce_only = b
            .get("reduceOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut trigger = None;
        let mut direction = 0;
        if let Some(t) = b
            .get("triggerPrice")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            let t =
                on_grid(t, inst.tick_size).ok_or((10001, "TriggerPrice invalid".to_string()))?;
            direction = b
                .get("triggerDirection")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            check_direction(direction, t, price)?;
            trigger = Some(t);
        }
        let reference = trigger.unwrap_or(price);
        if !reduce_only && qty * reference < inst.min_notional * (1.0 - 1e-9) {
            return Err((110094, "Order notional value below the lower limit".into()));
        }
        let fill_qty = if trigger.is_none() && reduce_only {
            let r = self.reducible(&symbol, side);
            if r == 0.0 {
                return Err((
                    110017,
                    "current position is zero, cannot fix reduce-only order qty".into(),
                ));
            }
            r.min(qty)
        } else {
            qty
        };
        let now = self.now();
        let order_id = self.next_id();
        self.orders.push(Order {
            order_id: order_id.clone(),
            link_id: link.clone(),
            symbol,
            side,
            qty,
            status: if trigger.is_some() {
                "Untriggered"
            } else {
                "New"
            },
            trigger,
            trigger_direction: direction,
            reduce_only,
            cum_qty: 0.0,
            cum_value: 0.0,
            cum_fee: 0.0,
            price_on_created: price,
            created: now,
            updated: now,
        });
        if trigger.is_none() {
            let i = self.orders.len() - 1;
            self.fill(i, price, fill_qty);
        }
        Ok(json!({ "orderId": order_id, "orderLinkId": link }))
    }

    fn find_open(&self, b: &Value) -> Result<usize, (i64, String)> {
        linear(b.get("category").and_then(Value::as_str))?;
        let symbol = str_field(b, "symbol")?;
        self.market(&symbol)?;
        let link = b.get("orderLinkId").and_then(Value::as_str).unwrap_or("");
        let id = b.get("orderId").and_then(Value::as_str).unwrap_or("");
        if link.is_empty() && id.is_empty() {
            return Err((10001, "orderId or orderLinkId is required".into()));
        }
        self.orders
            .iter()
            .position(|o| {
                o.symbol == symbol
                    && o.is_open()
                    && if id.is_empty() {
                        o.link_id == link
                    } else {
                        o.order_id == id
                    }
            })
            .ok_or((110001, "order not exists or too late to cancel".into()))
    }

    fn cancel(&mut self, b: &Value) -> VenueResult {
        let i = self.find_open(b)?;
        let now = self.now();
        let o = &mut self.orders[i];
        o.status = if o.status == "Untriggered" {
            "Deactivated"
        } else {
            "Cancelled"
        };
        o.updated = now;
        Ok(json!({ "orderId": o.order_id, "orderLinkId": o.link_id }))
    }

    fn amend(&mut self, b: &Value) -> VenueResult {
        let i = self.find_open(b)?;
        let (inst, price) = {
            let m = &self.markets[&self.orders[i].symbol];
            (m.instrument.clone(), m.price)
        };
        let trigger = match b.get("triggerPrice").and_then(Value::as_str) {
            Some(t) => {
                if self.orders[i].trigger.is_none() {
                    return Err((10001, "the order has no trigger price".into()));
                }
                let t = on_grid(t, inst.tick_size)
                    .ok_or((10001, "TriggerPrice invalid".to_string()))?;
                check_direction(self.orders[i].trigger_direction, t, price)?;
                Some(t)
            }
            None => None,
        };
        let qty = match b.get("qty").and_then(Value::as_str) {
            Some(q) => {
                let q = on_grid(q, inst.lot_size).ok_or((10001, "Qty invalid".to_string()))?;
                if q < inst.min_qty * (1.0 - 1e-9) {
                    return Err((
                        10001,
                        "Qty invalid: below the minimum order quantity".into(),
                    ));
                }
                Some(q)
            }
            None => None,
        };
        let now = self.now();
        let o = &mut self.orders[i];
        if let Some(t) = trigger {
            o.trigger = Some(t);
        }
        if let Some(q) = qty {
            o.qty = q;
        }
        o.updated = now;
        Ok(json!({ "orderId": o.order_id, "orderLinkId": o.link_id }))
    }

    fn realtime(&self, p: &HashMap<String, String>) -> VenueResult {
        linear(p.get("category").map(String::as_str))?;
        let symbol = p.get("symbol").cloned().unwrap_or_default();
        if symbol.is_empty() && !p.contains_key("settleCoin") && !p.contains_key("baseCoin") {
            return Err((10001, "symbol, baseCoin or settleCoin is required".into()));
        }
        let link = p.get("orderLinkId").map(String::as_str).unwrap_or("");
        let id = p.get("orderId").map(String::as_str).unwrap_or("");
        let mut list: Vec<&Order> = self
            .orders
            .iter()
            .filter(|o| symbol.is_empty() || o.symbol == symbol)
            .filter(|o| {
                // Asked by id, closed orders are listed too (the venue's recent-500 cache).
                if !id.is_empty() {
                    o.order_id == id && (o.is_open() || self.closed_cache)
                } else if !link.is_empty() {
                    o.link_id == link && (o.is_open() || self.closed_cache)
                } else {
                    o.is_open()
                }
            })
            .collect();
        list.sort_by_key(|o| std::cmp::Reverse(o.created));
        let items: Vec<Value> = list.iter().map(|o| self.order_json(o, false)).collect();
        page(items, p, 20, 50)
    }

    fn history(&self, p: &HashMap<String, String>) -> VenueResult {
        linear(p.get("category").map(String::as_str))?;
        let now = self.now();
        let symbol = p.get("symbol").map(String::as_str).unwrap_or("");
        let link = p.get("orderLinkId").map(String::as_str).unwrap_or("");
        let id = p.get("orderId").map(String::as_str).unwrap_or("");
        let mut list: Vec<&Order> = self
            .orders
            .iter()
            .filter(|o| !o.is_open())
            .filter(|o| o.updated + self.history_delay_ms <= now)
            .filter(|o| {
                let keep = if matches!(o.status, "Cancelled" | "Deactivated" | "Rejected") {
                    DAY_MS
                } else {
                    WEEK_MS
                };
                o.updated >= now - keep
            })
            .filter(|o| symbol.is_empty() || o.symbol == symbol)
            .filter(|o| link.is_empty() || o.link_id == link)
            .filter(|o| id.is_empty() || o.order_id == id)
            .collect();
        list.sort_by_key(|o| std::cmp::Reverse(o.created));
        let items: Vec<Value> = list.iter().map(|o| self.order_json(o, true)).collect();
        page(items, p, 20, 50)
    }

    fn executions(&self, p: &HashMap<String, String>) -> VenueResult {
        linear(p.get("category").map(String::as_str))?;
        let now = self.now();
        let symbol = p.get("symbol").map(String::as_str).unwrap_or("");
        let link = p.get("orderLinkId").map(String::as_str).unwrap_or("");
        let num = |k: &str| p.get(k).and_then(|v| v.parse::<i64>().ok());
        let (start, end) = match (num("startTime"), num("endTime")) {
            (Some(s), Some(e)) => {
                if e - s > WEEK_MS || e < s {
                    return Err((10001, "The time range should be within 7 days".into()));
                }
                (s, e)
            }
            (Some(s), None) => (s, s + WEEK_MS),
            (None, Some(e)) => (e - WEEK_MS, e),
            (None, None) => (now - WEEK_MS, now),
        };
        let mut list: Vec<(usize, &Exec)> = self
            .execs
            .iter()
            .enumerate()
            .filter(|(_, e)| symbol.is_empty() || e.symbol == symbol)
            .filter(|(_, e)| {
                if link.is_empty() {
                    e.time >= start && e.time <= end
                } else {
                    e.link_id == link && e.time >= now - WEEK_MS
                }
            })
            .collect();
        list.sort_by(|a, b| b.1.time.cmp(&a.1.time).then(b.0.cmp(&a.0)));
        let items: Vec<Value> = list.iter().map(|(_, e)| self.exec_json(e)).collect();
        page(items, p, 50, 100)
    }

    fn position_list(&self, p: &HashMap<String, String>) -> VenueResult {
        linear(p.get("category").map(String::as_str))?;
        let Some(symbol) = p.get("symbol") else {
            return Err((10001, "symbol or settleCoin is required".into()));
        };
        let m = self.market(symbol)?;
        let now = self.now();
        let pos = self.positions.get(symbol).copied().unwrap_or_default();
        let entry = |idx: i64, pos: Pos| {
            let side = if pos.size > 0.0 {
                "Buy"
            } else if pos.size < 0.0 {
                "Sell"
            } else {
                ""
            };
            json!({
                "positionIdx": idx,
                "riskId": 1,
                "riskLimitValue": "2000000",
                "symbol": symbol,
                "side": side,
                "size": fmt_step(pos.size.abs(), m.instrument.lot_size),
                "avgPrice": num_text(pos.avg),
                "positionValue": num_text(pos.avg * pos.size.abs()),
                "tradeMode": 0,
                "autoAddMargin": 0,
                "positionStatus": "Normal",
                "leverage": "10",
                "markPrice": fmt_step(m.price, m.instrument.tick_size),
                "liqPrice": "",
                "bustPrice": "",
                "positionIM": "0",
                "positionMM": "0",
                "takeProfit": "0",
                "stopLoss": "0",
                "trailingStop": "0",
                "unrealisedPnl": num_text((m.price - pos.avg) * pos.size),
                "curRealisedPnl": "0",
                "cumRealisedPnl": num_text(self.cum_realised),
                "adlRankIndicator": 0,
                "createdTime": now.to_string(),
                "updatedTime": now.to_string(),
                "seq": self.next,
                "isReduceOnly": false,
            })
        };
        let list = if self.hedge {
            vec![entry(1, Pos::default()), entry(2, Pos::default())]
        } else {
            vec![entry(0, pos)]
        };
        Ok(json!({ "category": "linear", "list": list, "nextPageCursor": "" }))
    }

    fn wallet_balance(&self, p: &HashMap<String, String>) -> VenueResult {
        if p.get("accountType").map(String::as_str) != Some("UNIFIED") {
            return Err((10001, "accountType only support UNIFIED".into()));
        }
        let upnl = self.upnl();
        let equity = self.wallet + upnl;
        let wants_usdt = p
            .get("coin")
            .is_none_or(|c| c.split(',').any(|c| c == "USDT"));
        let coins = if wants_usdt {
            vec![json!({
                "coin": "USDT",
                "equity": num_text(equity),
                "usdValue": num_text(equity),
                "walletBalance": num_text(self.wallet),
                "unrealisedPnl": num_text(upnl),
                "cumRealisedPnl": num_text(self.cum_realised),
                "locked": "0",
                "borrowAmount": "0",
                "totalOrderIM": "0",
                "totalPositionIM": "0",
                "totalPositionMM": "0",
                "bonus": "0",
                "accruedInterest": "0",
                "spotHedgingQty": "0",
                "collateralSwitch": true,
                "marginCollateral": true,
                "availableToWithdraw": "",
                "availableToBorrow": "",
            })]
        } else {
            vec![]
        };
        Ok(json!({
            "list": [{
                "accountType": "UNIFIED",
                "totalEquity": num_text(equity),
                "totalWalletBalance": num_text(self.wallet),
                "totalMarginBalance": num_text(equity),
                "totalAvailableBalance": num_text(equity),
                "totalPerpUPL": num_text(upnl),
                "totalInitialMargin": "0",
                "totalMaintenanceMargin": "0",
                "accountIMRate": "0",
                "accountMMRate": "0",
                "accountLTV": "0",
                "coin": coins,
            }]
        }))
    }

    pub(crate) fn order_json(&self, o: &Order, history: bool) -> Value {
        let inst = &self.markets[&o.symbol].instrument;
        let tick = |x: f64| fmt_step(x, inst.tick_size);
        let avg = if o.cum_qty > 0.0 {
            num_text(o.cum_value / o.cum_qty)
        } else {
            String::new()
        };
        let mut v = json!({
            "orderId": o.order_id,
            "orderLinkId": o.link_id,
            "blockTradeId": "",
            "symbol": o.symbol,
            "price": "0",
            "qty": fmt_step(o.qty, inst.lot_size),
            "side": side_text(o.side),
            "isLeverage": "",
            "positionIdx": if self.hedge { 1 } else { 0 },
            "orderStatus": o.status,
            "createType": "CreateByUser",
            "cancelType": if o.status == "Cancelled" || o.status == "Deactivated" { "CancelByUser" } else { "UNKNOWN" },
            "rejectReason": "EC_NoError",
            "avgPrice": avg,
            "leavesQty": fmt_step(if o.is_open() { o.qty - o.cum_qty } else { 0.0 }, inst.lot_size),
            "leavesValue": "0",
            "cumExecQty": fmt_step(o.cum_qty, inst.lot_size),
            "cumExecValue": num_text(o.cum_value),
            "cumExecFee": num_text(o.cum_fee),
            "timeInForce": "IOC",
            "orderType": "Market",
            "stopOrderType": if o.trigger.is_some() { "Stop" } else { "" },
            "orderIv": "",
            "marketUnit": "",
            "triggerPrice": o.trigger.map(tick).unwrap_or_default(),
            "takeProfit": "",
            "stopLoss": "",
            "tpslMode": "",
            "ocoTriggerBy": "",
            "tpLimitPrice": "",
            "slLimitPrice": "",
            "tpTriggerBy": "",
            "slTriggerBy": "",
            "triggerDirection": o.trigger_direction,
            "triggerBy": if o.trigger.is_some() { "LastPrice" } else { "" },
            "lastPriceOnCreated": tick(o.price_on_created),
            "basePrice": "",
            "reduceOnly": o.reduce_only,
            "closeOnTrigger": false,
            "placeType": "",
            "smpType": "None",
            "smpGroup": 0,
            "smpOrderId": "",
            "createdTime": o.created.to_string(),
            "updatedTime": o.updated.to_string(),
        });
        if history {
            v["slippageToleranceType"] = json!("UNKNOWN");
            v["slippageTolerance"] = json!("");
        }
        v
    }

    pub(crate) fn exec_json(&self, e: &Exec) -> Value {
        let inst = &self.markets[&e.symbol].instrument;
        json!({
            "symbol": e.symbol,
            "orderType": if e.exec_type == "Trade" { "Market" } else { "UNKNOWN" },
            "underlyingPrice": "",
            "orderLinkId": e.link_id,
            "side": side_text(e.side),
            "indexPrice": "",
            "orderId": e.order_id,
            "stopOrderType": "UNKNOWN",
            "leavesQty": "0",
            "execTime": e.time.to_string(),
            "feeCurrency": "",
            "isMaker": false,
            "execFee": num_text(e.fee),
            "feeRate": num_text(e.fee_rate),
            "execId": e.exec_id,
            "tradeIv": "",
            "blockTradeId": "",
            "markPrice": fmt_step(e.price, inst.tick_size),
            "execPrice": fmt_step(e.price, inst.tick_size),
            "markIv": "",
            "orderQty": fmt_step(e.order_qty, inst.lot_size),
            "orderPrice": "0",
            "execValue": num_text(e.price * e.qty),
            "execType": e.exec_type,
            "execQty": fmt_step(e.qty, inst.lot_size),
            "closedSize": "",
            "extraFees": "",
            "seq": e.seq,
        })
    }
}

fn linear(category: Option<&str>) -> Result<(), (i64, String)> {
    if category == Some("linear") {
        Ok(())
    } else {
        Err((
            10001,
            "params error: the mock venue serves category linear only".into(),
        ))
    }
}

fn str_field(b: &Value, name: &str) -> Result<String, (i64, String)> {
    b.get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or((10001, format!("params error: {name} is required")))
}

fn check_direction(direction: i64, trigger: f64, price: f64) -> Result<(), (i64, String)> {
    match direction {
        1 if trigger > price => Ok(()),
        2 if trigger < price => Ok(()),
        1 => Err((
            110092,
            format!("expect Rising, but trigger_price[{trigger}] <= current[{price}]"),
        )),
        2 => Err((
            110093,
            format!("expect Falling, but trigger_price[{trigger}] >= current[{price}]"),
        )),
        _ => Err((10001, "params error: triggerDirection invalid".into())),
    }
}

/// Reads a decimal that must sit exactly on the `step` grid, with no more decimals than the step
/// has (so float noise such as `0.30000000000000004` is refused).
fn on_grid(text: &str, step: f64) -> Option<f64> {
    let step_decimals = decimals(&step.to_string());
    if decimals(text) > step_decimals || text.starts_with('+') || text.contains('e') {
        return None;
    }
    let value: f64 = text
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite() && *v > 0.0)?;
    (snap(value, step) == value).then_some(value)
}

fn decimals(text: &str) -> usize {
    text.split_once('.').map_or(0, |(_, f)| f.len())
}

/// `value` rounded to the nearest multiple of `step`, with the engine's rounding.
fn snap(value: f64, step: f64) -> f64 {
    let grid = Instrument {
        id: td_core::InstrumentId {
            venue: td_core::Venue::Bybit,
            symbol: String::new(),
            kind: td_core::InstrumentKind::Perpetual,
        },
        base: String::new(),
        quote: String::new(),
        tick_size: step,
        lot_size: step,
        min_qty: 0.0,
        min_notional: 0.0,
        fees: td_core::FeeSchedule {
            maker: 0.0,
            taker: 0.0,
            per_unit: 0.0,
        },
    };
    grid.round_price(value, Rounding::Nearest)
}

fn fmt_step(value: f64, step: f64) -> String {
    let d = decimals(&step.to_string());
    format!("{:.*}", d, snap(value, step))
}

fn num_text(x: f64) -> String {
    let t = format!("{:.8}", round8(x));
    let t = t.trim_end_matches('0').trim_end_matches('.');
    if t.is_empty() || t == "-0" {
        "0".into()
    } else {
        t.into()
    }
}

fn round8(x: f64) -> f64 {
    (x * 1e8).round() / 1e8
}

fn side_text(side: Side) -> &'static str {
    match side {
        Side::Buy => "Buy",
        Side::Sell => "Sell",
    }
}

fn page(items: Vec<Value>, p: &HashMap<String, String>, default: usize, max: usize) -> VenueResult {
    let limit = match p.get("limit") {
        Some(l) => l
            .parse::<usize>()
            .ok()
            .filter(|l| (1..=max).contains(l))
            .ok_or((10001, "params error: limit invalid".to_string()))?,
        None => default,
    };
    let offset = match p.get("cursor") {
        Some(c) => c
            .parse::<usize>()
            .map_err(|_| (10001, "params error: cursor invalid".to_string()))?,
        None => 0,
    };
    let end = (offset + limit).min(items.len());
    let next = if end < items.len() {
        end.to_string()
    } else {
        String::new()
    };
    let list: Vec<Value> = items.into_iter().skip(offset).take(limit).collect();
    Ok(json!({ "category": "linear", "list": list, "nextPageCursor": next }))
}

pub(crate) fn sign(secret: &str, text: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(text.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_keys(name: &str, list: bool) -> Vec<String> {
        let path = format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let obj = if list {
            &v["result"]["list"][0]
        } else {
            &v["result"]
        };
        obj.as_object().unwrap().keys().cloned().collect()
    }

    fn assert_keys_documented(sent: &Value, fixture: &str, list: bool) {
        let documented = fixture_keys(fixture, list);
        for k in sent.as_object().unwrap().keys() {
            assert!(documented.contains(k), "{k} is not in {fixture}");
        }
    }

    fn state() -> State {
        let mut s = State::new("k", "s");
        s.markets.insert(
            "BTCUSDT".into(),
            Market {
                instrument: Instrument {
                    id: td_core::InstrumentId {
                        venue: td_core::Venue::Bybit,
                        symbol: "BTCUSDT".into(),
                        kind: td_core::InstrumentKind::Perpetual,
                    },
                    base: "BTC".into(),
                    quote: "USDT".into(),
                    tick_size: 0.1,
                    lot_size: 0.001,
                    min_qty: 0.001,
                    min_notional: 5.0,
                    fees: td_core::FeeSchedule {
                        maker: 0.0002,
                        taker: 0.00055,
                        per_unit: 0.0,
                    },
                },
                price: 65000.0,
            },
        );
        s
    }

    #[test]
    fn every_field_the_mock_sends_is_in_the_documented_shape() {
        let mut s = state();
        let r = s.create(&json!({
            "category": "linear", "symbol": "BTCUSDT", "side": "Buy", "orderType": "Market",
            "qty": "0.002", "positionIdx": 0, "orderLinkId": "a1",
        }));
        assert_keys_documented(&r.unwrap(), "order-create.json", false);
        let o = s.orders[0].clone();
        assert_keys_documented(&s.order_json(&o, false), "order-realtime.json", true);
        assert_keys_documented(&s.order_json(&o, true), "order-history.json", true);
        let e = s.execs[0].clone();
        assert_keys_documented(&s.exec_json(&e), "execution-list.json", true);
        let p: HashMap<String, String> = [
            ("category".into(), "linear".into()),
            ("symbol".into(), "BTCUSDT".into()),
        ]
        .into();
        let pos = s.position_list(&p).unwrap();
        assert_keys_documented(&pos["list"][0], "position-list.json", true);
        let w: HashMap<String, String> = [("accountType".into(), "UNIFIED".into())].into();
        let wallet = s.wallet_balance(&w).unwrap();
        let account = wallet["list"][0].clone();
        let mut top = account.clone();
        top.as_object_mut().unwrap().remove("coin");
        assert_keys_documented(&top, "wallet-balance.json", true);
        let documented: Value = serde_json::from_str(
            &std::fs::read_to_string(format!(
                "{}/fixtures/wallet-balance.json",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap(),
        )
        .unwrap();
        let coin_keys = documented["result"]["list"][0]["coin"][0]
            .as_object()
            .unwrap();
        for k in account["coin"][0].as_object().unwrap().keys() {
            assert!(
                coin_keys.contains_key(k),
                "{k} is not a documented coin field"
            );
        }
    }

    #[test]
    fn grid_reading_refuses_noise_and_off_grid_values() {
        assert_eq!(on_grid("0.3", 0.1), Some(0.3));
        assert_eq!(on_grid("0.30000000000000004", 0.1), None);
        assert_eq!(on_grid("0.0015", 0.001), None);
        assert_eq!(on_grid("0.25", 0.1), None);
        assert_eq!(on_grid("1e-3", 0.001), None);
        assert_eq!(on_grid("-1", 1.0), None);
        assert_eq!(on_grid("1005", 5.0), Some(1005.0));
        assert_eq!(on_grid("1003", 5.0), None);
    }

    #[test]
    fn a_long_closed_by_a_sell_realises_the_price_move_less_fees() {
        let mut s = state();
        let order = |side: &str, link: &str, reduce: bool| {
            json!({
                "category": "linear", "symbol": "BTCUSDT", "side": side, "orderType": "Market",
                "qty": "0.010", "positionIdx": 0, "orderLinkId": link, "reduceOnly": reduce,
            })
        };
        s.create(&order("Buy", "o1", false)).unwrap();
        s.set_price("BTCUSDT", 66000.0);
        s.create(&order("Sell", "o2", true)).unwrap();
        let fees = round8(65000.0 * 0.01 * 0.00055) + round8(66000.0 * 0.01 * 0.00055);
        assert!((s.wallet - (100_000.0 + 10.0 - fees)).abs() < 1e-9);
        assert_eq!(s.positions["BTCUSDT"].size, 0.0);
        // A second reduce-only order has nothing to reduce.
        let err = s.create(&order("Sell", "o3", true)).unwrap_err();
        assert_eq!(err.0, 110017);
    }
}
