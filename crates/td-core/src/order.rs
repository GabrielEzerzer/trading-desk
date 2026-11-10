use serde::{Deserialize, Serialize};

use crate::{InstrumentId, Millis};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// +1.0 for a buy, -1.0 for a sell: multiply a quantity by it to get a signed position change.
    pub fn sign(self) -> f64 {
        match self {
            Side::Buy => 1.0,
            Side::Sell => -1.0,
        }
    }
}

/// How an order is priced. A stop becomes a market order, and a stop-limit a limit order, once the
/// market trades through `trigger`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrderType {
    Market,
    Limit { price: f64 },
    Stop { trigger: f64 },
    StopLimit { trigger: f64, price: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeInForce {
    /// Good till cancelled: rests on the book until filled or cancelled.
    Gtc,
    /// Immediate or cancel: fills what it can at once and cancels the rest.
    Ioc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrderId(pub u64);

/// An order as a strategy or the user submits it. Quantity is in the base asset and always
/// positive; `side` carries the direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub instrument: InstrumentId,
    pub side: Side,
    pub qty: f64,
    pub order_type: OrderType,
    pub tif: TimeInForce,
    /// Only reduce an open position, never open or grow one.
    pub reduce_only: bool,
    /// A free label from the submitter, carried through to reports.
    pub client_tag: Option<String>,
}

/// Whether a fill added liquidity to the book (maker) or took it (taker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Liquidity {
    Maker,
    Taker,
}

/// A full or partial execution of an order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub order_id: OrderId,
    pub time: Millis,
    pub price: f64,
    pub qty: f64,
    /// The fee paid, in the quote asset. Negative for a rebate.
    pub fee: f64,
    pub liquidity: Liquidity,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_sign() {
        assert_eq!(Side::Buy.sign() * 2.0, 2.0);
        assert_eq!(Side::Sell.sign() * 2.0, -2.0);
    }

    #[test]
    fn order_type_serializes_with_a_type_tag() {
        let json = serde_json::to_string(&OrderType::Limit { price: 1.5 }).unwrap();
        assert_eq!(json, r#"{"type":"limit","price":1.5}"#);
        let back: OrderType =
            serde_json::from_str(r#"{"type":"stop_limit","trigger":2.0,"price":1.9}"#).unwrap();
        assert_eq!(
            back,
            OrderType::StopLimit {
                trigger: 2.0,
                price: 1.9
            }
        );
    }
}
