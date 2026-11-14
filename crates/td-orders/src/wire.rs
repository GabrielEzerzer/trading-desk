//! The venue's JSON, read into the route's own types, and numbers written as exact decimals.
//!
//! Field names and meanings come from the v5 pages for real-time orders
//! (<https://bybit-exchange.github.io/docs/v5/order/open-order>), order history
//! (`.../order/order-list`), executions (`.../order/execution`), positions (`.../position`), wallet
//! balance (`.../account/wallet-balance`) and the enums (`.../enum`), read on 2026-11-14. The mock
//! venue's fixtures under `mock/bybit/fixtures/` hold the shapes, and the tests below parse them.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use td_core::{Liquidity, Millis, Side};

use crate::Error;

/// The envelope every v5 answer comes in.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Envelope {
    pub ret_code: i64,
    #[serde(default)]
    pub ret_msg: String,
    #[serde(default)]
    pub result: serde_json::Value,
}

/// Reads `value` as `T`. A failure names the endpoint and the kind of mismatch, never the value:
/// serde's own messages can quote the offending text.
pub(crate) fn from_value<T: DeserializeOwned>(
    endpoint: &'static str,
    value: serde_json::Value,
) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|e| Error::BadResponse {
        endpoint,
        reason: format!(
            "the result does not have the documented shape ({:?})",
            e.classify()
        ),
    })
}

pub(crate) fn envelope(endpoint: &'static str, body: &[u8]) -> Result<Envelope, Error> {
    serde_json::from_slice(body).map_err(|e| Error::BadResponse {
        endpoint,
        reason: format!(
            "the body is not the documented JSON ({:?} at line {} column {})",
            e.classify(),
            e.line(),
            e.column()
        ),
    })
}

/// A page of a list endpoint.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Page<T> {
    #[serde(default = "Vec::new")]
    pub list: Vec<T>,
    #[serde(default)]
    pub next_page_cursor: String,
}

/// An order's status, as the venue's `orderStatus` enum names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    /// A conditional order waiting for its trigger.
    Untriggered,
    Rejected,
    PartiallyFilledCanceled,
    Filled,
    Cancelled,
    /// A conditional order passing from untriggered to new.
    Triggered,
    /// A conditional order cancelled before it triggered.
    Deactivated,
    /// A status this route does not know, kept as the venue spelled it.
    Other(String),
}

impl OrderStatus {
    fn parse(s: &str) -> Self {
        match s {
            "New" => Self::New,
            "PartiallyFilled" => Self::PartiallyFilled,
            "Untriggered" => Self::Untriggered,
            "Rejected" => Self::Rejected,
            "PartiallyFilledCanceled" => Self::PartiallyFilledCanceled,
            "Filled" => Self::Filled,
            "Cancelled" => Self::Cancelled,
            "Triggered" => Self::Triggered,
            "Deactivated" => Self::Deactivated,
            other => Self::Other(other.to_string()),
        }
    }

    /// Whether the order can still fill, be cancelled or be amended. `Triggered` is an instant on
    /// the way to `New`, so it counts as open. An unknown status counts as open, so the route
    /// never treats an order it does not understand as finished.
    pub fn is_open(&self) -> bool {
        matches!(
            self,
            Self::New
                | Self::PartiallyFilled
                | Self::Untriggered
                | Self::Triggered
                | Self::Other(_)
        )
    }

    /// Whether the order was cancelled (before or after a partial fill) or deactivated.
    pub fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::Deactivated | Self::PartiallyFilledCanceled
        )
    }
}

impl std::fmt::Display for OrderStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Other(s) => f.write_str(s),
            known => write!(f, "{known:?}"),
        }
    }
}

/// An order as the venue reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct VenueOrder {
    pub order_id: String,
    /// The `orderLinkId`; empty for an order placed without one (by hand on the venue's site).
    pub client_id: String,
    pub symbol: String,
    pub side: Side,
    pub status: OrderStatus,
    pub qty: f64,
    pub filled_qty: f64,
    /// The average fill price, when anything has filled.
    pub avg_price: Option<f64>,
    /// The trigger price of a conditional order; `None` for a plain order.
    pub trigger_price: Option<f64>,
    pub reduce_only: bool,
    /// The venue's reject reason enum, `EC_NoError` when there is none.
    pub reject_reason: String,
    pub created: Millis,
    pub updated: Millis,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawOrder {
    order_id: String,
    #[serde(default)]
    order_link_id: String,
    symbol: String,
    side: String,
    order_status: String,
    qty: String,
    #[serde(default)]
    cum_exec_qty: String,
    #[serde(default)]
    avg_price: String,
    #[serde(default)]
    trigger_price: String,
    #[serde(default)]
    reduce_only: bool,
    #[serde(default)]
    reject_reason: String,
    created_time: String,
    updated_time: String,
}

impl RawOrder {
    pub(crate) fn into_order(self, endpoint: &'static str) -> Result<VenueOrder, Error> {
        Ok(VenueOrder {
            side: side(endpoint, &self.side)?,
            status: OrderStatus::parse(&self.order_status),
            qty: num(endpoint, "qty", &self.qty)?,
            filled_qty: num_or_zero(endpoint, "cumExecQty", &self.cum_exec_qty)?,
            avg_price: positive_or_none(endpoint, "avgPrice", &self.avg_price)?,
            trigger_price: positive_or_none(endpoint, "triggerPrice", &self.trigger_price)?,
            reduce_only: self.reduce_only,
            created: millis(endpoint, "createdTime", &self.created_time)?,
            updated: millis(endpoint, "updatedTime", &self.updated_time)?,
            order_id: self.order_id,
            client_id: self.order_link_id,
            symbol: self.symbol,
            reject_reason: self.reject_reason,
        })
    }
}

/// What an execution was, as the venue's `execType` enum names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecKind {
    /// A fill of one of the account's orders.
    Trade,
    /// A fill from auto-deleveraging.
    AdlTrade,
    /// A funding fee.
    Funding,
    /// A liquidation (takeover) fill.
    BustTrade,
    /// A close from delivery or delisting.
    Delivery,
    Settle,
    BlockTrade,
    MovePosition,
    /// A type this route does not know, kept as the venue spelled it.
    Other(String),
}

impl ExecKind {
    fn parse(s: &str) -> Self {
        match s {
            "Trade" => Self::Trade,
            "AdlTrade" => Self::AdlTrade,
            "Funding" => Self::Funding,
            "BustTrade" => Self::BustTrade,
            "Delivery" => Self::Delivery,
            "Settle" => Self::Settle,
            "BlockTrade" => Self::BlockTrade,
            "MovePosition" => Self::MovePosition,
            other => Self::Other(other.to_string()),
        }
    }
}

/// One execution on the account: a fill, a funding fee, a liquidation.
#[derive(Debug, Clone, PartialEq)]
pub struct Execution {
    pub exec_id: String,
    pub order_id: String,
    /// The `orderLinkId` of the order filled; empty for executions with no order of the account's
    /// own (funding) or an order placed without one.
    pub client_id: String,
    pub symbol: String,
    /// `None` when the venue sends no side, as it may for a funding fee.
    pub side: Option<Side>,
    pub price: f64,
    pub qty: f64,
    /// The fee paid in the settle coin; negative for a rebate.
    pub fee: f64,
    pub liquidity: Liquidity,
    pub time: Millis,
    pub kind: ExecKind,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawExecution {
    exec_id: String,
    #[serde(default)]
    order_id: String,
    #[serde(default)]
    order_link_id: String,
    symbol: String,
    #[serde(default)]
    side: String,
    exec_price: String,
    exec_qty: String,
    exec_fee: String,
    exec_type: String,
    #[serde(default)]
    is_maker: bool,
    exec_time: String,
}

impl RawExecution {
    pub(crate) fn into_execution(self, endpoint: &'static str) -> Result<Execution, Error> {
        Ok(Execution {
            side: match self.side.as_str() {
                "" => None,
                s => Some(side(endpoint, s)?),
            },
            price: num_or_zero(endpoint, "execPrice", &self.exec_price)?,
            qty: num_or_zero(endpoint, "execQty", &self.exec_qty)?,
            fee: num_or_zero(endpoint, "execFee", &self.exec_fee)?,
            liquidity: if self.is_maker {
                Liquidity::Maker
            } else {
                Liquidity::Taker
            },
            time: millis(endpoint, "execTime", &self.exec_time)?,
            kind: ExecKind::parse(&self.exec_type),
            exec_id: self.exec_id,
            order_id: self.order_id,
            client_id: self.order_link_id,
            symbol: self.symbol,
        })
    }
}

/// The account's one-way position on a symbol.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub symbol: String,
    /// Signed size in the base asset: positive long, negative short, zero flat.
    pub size: f64,
    /// The average entry price; zero when flat.
    pub avg_price: f64,
    pub unrealised_pnl: f64,
    pub mark_price: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawPosition {
    pub position_idx: i64,
    pub symbol: String,
    #[serde(default)]
    side: String,
    size: String,
    #[serde(default)]
    avg_price: String,
    #[serde(default)]
    unrealised_pnl: String,
    #[serde(default)]
    mark_price: String,
}

impl RawPosition {
    pub(crate) fn into_position(self, endpoint: &'static str) -> Result<Position, Error> {
        let size = num(endpoint, "size", &self.size)?;
        let sign = match self.side.as_str() {
            "Buy" => 1.0,
            "Sell" => -1.0,
            "" | "None" if size == 0.0 => 0.0,
            _ => {
                return Err(Error::BadResponse {
                    endpoint,
                    reason: "a position with a size has no side".into(),
                });
            }
        };
        Ok(Position {
            size: sign * size,
            avg_price: num_or_zero(endpoint, "avgPrice", &self.avg_price)?,
            unrealised_pnl: num_or_zero(endpoint, "unrealisedPnl", &self.unrealised_pnl)?,
            mark_price: num_or_zero(endpoint, "markPrice", &self.mark_price)?,
            symbol: self.symbol,
        })
    }
}

/// The account's equity in the settle coin, and the account total.
#[derive(Debug, Clone, PartialEq)]
pub struct Equity {
    /// The settle coin the first three fields are in.
    pub coin: String,
    /// The coin's `walletBalance`: deposits plus realised P&L less fees.
    pub wallet_balance: f64,
    /// The coin's `unrealisedPnl` on open positions.
    pub unrealised_pnl: f64,
    /// `wallet_balance + unrealised_pnl`. The venue's own coin `equity` also subtracts spot
    /// borrowing and adds option value, which a perpetuals-only demo account does not have.
    pub equity: f64,
    /// The venue's `totalEquity`: the whole account's equity in USD across every coin.
    pub account_total_equity_usd: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawWallet {
    pub total_equity: String,
    #[serde(default)]
    pub coin: Vec<RawCoin>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawCoin {
    pub coin: String,
    pub wallet_balance: String,
    #[serde(default)]
    pub unrealised_pnl: String,
}

/// The server time answer.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerTime {
    pub time_nano: String,
}

/// The order id in a create, amend or cancel answer.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Ack {
    pub order_id: String,
}

fn side(endpoint: &'static str, s: &str) -> Result<Side, Error> {
    match s {
        "Buy" => Ok(Side::Buy),
        "Sell" => Ok(Side::Sell),
        _ => Err(Error::BadResponse {
            endpoint,
            reason: "a side is neither Buy nor Sell".into(),
        }),
    }
}

pub(crate) fn num(endpoint: &'static str, field: &str, s: &str) -> Result<f64, Error> {
    s.parse::<f64>()
        .ok()
        .filter(|x| x.is_finite())
        .ok_or_else(|| Error::BadResponse {
            endpoint,
            reason: format!("{field} is not a number"),
        })
}

pub(crate) fn num_or_zero(endpoint: &'static str, field: &str, s: &str) -> Result<f64, Error> {
    if s.is_empty() {
        Ok(0.0)
    } else {
        num(endpoint, field, s)
    }
}

/// `None` for an empty or zero value, which the venue sends for a field that does not apply.
fn positive_or_none(endpoint: &'static str, field: &str, s: &str) -> Result<Option<f64>, Error> {
    let x = num_or_zero(endpoint, field, s)?;
    Ok((x != 0.0).then_some(x))
}

fn millis(endpoint: &'static str, field: &str, s: &str) -> Result<Millis, Error> {
    s.parse().map_err(|_| Error::BadResponse {
        endpoint,
        reason: format!("{field} is not a timestamp"),
    })
}

/// Writes `value`, which must already be a multiple of `step`, with exactly as many decimals as
/// `step` has, so the venue gets `0.3`, never `0.30000000000000004`. The engine's rounding
/// (`Instrument::round_price`, `round_qty`) gives the float nearest the decimal; this only prints
/// it. A value that does not print back to itself is refused rather than sent.
pub(crate) fn decimal(value: f64, step: f64) -> Result<String, Error> {
    // Rust prints a float as the shortest decimal that reads back to it, and never in exponent
    // form, so the digits after the point are the step's decimals.
    let step_text = step.to_string();
    let decimals = step_text.split_once('.').map_or(0, |(_, frac)| frac.len());
    let text = format!("{value:.decimals$}");
    if text.parse::<f64>() != Ok(value) {
        return Err(Error::Unsupported {
            what: "a number that cannot be written exactly on the instrument's grid",
        });
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_follow_the_step() {
        assert_eq!(decimal(0.3, 0.1).unwrap(), "0.3");
        assert_eq!(decimal(65000.5, 0.1).unwrap(), "65000.5");
        assert_eq!(decimal(65000.0, 0.1).unwrap(), "65000.0");
        assert_eq!(decimal(0.001, 0.001).unwrap(), "0.001");
        assert_eq!(decimal(12.0, 1.0).unwrap(), "12");
        assert_eq!(decimal(1005.0, 5.0).unwrap(), "1005");
        assert_eq!(decimal(0.00012345, 0.00000001).unwrap(), "0.00012345");
        // A rounded value prints clean even where float arithmetic does not.
        let i = td_core::Instrument {
            lot_size: 0.1,
            ..test_instrument()
        };
        assert_eq!(decimal(i.round_qty(3.0 * 0.1), 0.1).unwrap(), "0.3");
    }

    #[test]
    fn a_value_off_the_grid_is_refused() {
        assert!(decimal(0.30000000000000004, 0.1).is_err());
        assert!(decimal(0.0015, 0.001).is_err());
    }

    fn test_instrument() -> td_core::Instrument {
        td_core::Instrument {
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
        }
    }

    fn fixture(name: &str) -> serde_json::Value {
        let path = format!(
            "{}/../../mock/bybit/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let env: Envelope = serde_json::from_str(&text).unwrap();
        assert_eq!(env.ret_code, 0, "{name}");
        env.result
    }

    #[test]
    fn the_documented_shapes_parse() {
        let orders: Page<RawOrder> = from_value("t", fixture("order-realtime.json")).unwrap();
        let order = orders
            .list
            .into_iter()
            .next()
            .unwrap()
            .into_order("t")
            .unwrap();
        assert_eq!(order.status, OrderStatus::Untriggered);
        assert_eq!(order.trigger_price, Some(58000.0));
        assert!(order.reduce_only);
        assert_eq!(order.avg_price, None);

        let history: Page<RawOrder> = from_value("t", fixture("order-history.json")).unwrap();
        let order = history
            .list
            .into_iter()
            .next()
            .unwrap()
            .into_order("t")
            .unwrap();
        assert_eq!(order.status, OrderStatus::Filled);
        assert_eq!(order.avg_price, Some(65000.1));

        let execs: Page<RawExecution> = from_value("t", fixture("execution-list.json")).unwrap();
        let execs: Vec<Execution> = execs
            .list
            .into_iter()
            .map(|e| e.into_execution("t").unwrap())
            .collect();
        assert_eq!(execs[0].kind, ExecKind::Trade);
        assert_eq!(execs[0].liquidity, Liquidity::Taker);
        assert_eq!(execs[0].fee, 0.03575005);
        assert_eq!(execs[1].kind, ExecKind::Funding);

        let positions: Page<RawPosition> = from_value("t", fixture("position-list.json")).unwrap();
        let p = positions.list.into_iter().next().unwrap();
        assert_eq!(p.position_idx, 0);
        assert_eq!(p.into_position("t").unwrap().size, 0.001);

        let wallets: Page<RawWallet> = from_value("t", fixture("wallet-balance.json")).unwrap();
        assert_eq!(wallets.list[0].coin[0].coin, "USDT");

        let time: ServerTime = from_value("t", fixture("market-time.json")).unwrap();
        assert_eq!(time.time_nano, "1688639403423213947");

        for name in ["order-create.json", "order-amend.json", "order-cancel.json"] {
            let ack: Ack = from_value("t", fixture(name)).unwrap();
            assert!(!ack.order_id.is_empty());
        }
    }

    #[test]
    fn statuses_sort_into_open_and_closed() {
        for open in [
            "New",
            "PartiallyFilled",
            "Untriggered",
            "Triggered",
            "Later",
        ] {
            assert!(OrderStatus::parse(open).is_open(), "{open}");
        }
        for closed in [
            "Rejected",
            "PartiallyFilledCanceled",
            "Filled",
            "Cancelled",
            "Deactivated",
        ] {
            assert!(!OrderStatus::parse(closed).is_open(), "{closed}");
        }
    }

    #[test]
    fn a_bad_body_error_quotes_nothing_from_the_body() {
        let err = envelope("e", br#"{"retCode": "SECRET-ECHO"}"#)
            .err()
            .unwrap();
        assert!(!err.to_string().contains("SECRET-ECHO"), "{err}");
        let err = from_value::<Ack>("e", serde_json::json!({"orderId": ["SECRET-ECHO"]}))
            .err()
            .unwrap();
        assert!(!format!("{err} {err:?}").contains("SECRET-ECHO"));
    }
}
