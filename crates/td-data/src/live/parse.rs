//! Reading kline messages from Binance's and Bybit's public streams.

use serde::Deserialize;
use td_core::{Candle, Millis, Venue};

use crate::DataError;

/// One kline update from a stream: the bar as it stands, and whether it has closed. A venue sends
/// many updates for the bar in progress and one with `closed` set when its minute ends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KlineUpdate {
    pub candle: Candle,
    pub closed: bool,
}

/// Reads one text frame from `venue`'s kline stream. Frames that carry no kline (subscription
/// acknowledgements, pongs) give an empty list; a failed subscription or a kline that does not
/// read is an error.
pub fn parse_message(venue: Venue, text: &str) -> Result<Vec<KlineUpdate>, DataError> {
    match venue {
        Venue::Binance => parse_binance(text).map(|update| update.into_iter().collect()),
        Venue::Bybit => parse_bybit(text),
    }
}

#[derive(Debug, Deserialize)]
struct BinanceEvent {
    k: Option<BinanceKline>,
}

#[derive(Debug, Deserialize)]
struct BinanceKline {
    t: Millis,
    i: String,
    o: String,
    h: String,
    l: String,
    c: String,
    v: String,
    q: String,
    x: bool,
}

/// Reads a Binance `<symbol>@kline_1m` frame (spot and USD-M use the same format).
pub fn parse_binance(text: &str) -> Result<Option<KlineUpdate>, DataError> {
    let what = "a Binance kline frame";
    let event: BinanceEvent = serde_json::from_str(text).map_err(|e| DataError::parse(what, e))?;
    let Some(k) = event.k else {
        return Ok(None);
    };
    if k.i != "1m" {
        return Err(DataError::parse(
            what,
            format!("interval {:?}, not 1m", k.i),
        ));
    }
    let candle = Candle {
        open_time: k.t,
        open: number(what, &k.o)?,
        high: number(what, &k.h)?,
        low: number(what, &k.l)?,
        close: number(what, &k.c)?,
        volume: number(what, &k.v)?,
        quote_volume: number(what, &k.q)?,
    };
    Ok(Some(KlineUpdate {
        candle,
        closed: k.x,
    }))
}

#[derive(Debug, Deserialize)]
struct BybitFrame {
    topic: Option<String>,
    data: Option<serde_json::Value>,
    success: Option<bool>,
    ret_msg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BybitKline {
    start: Millis,
    interval: String,
    open: String,
    close: String,
    high: String,
    low: String,
    volume: String,
    turnover: String,
    confirm: bool,
}

/// Reads a Bybit v5 public frame: `kline.1.<SYMBOL>` data, or an operation response, where
/// `success: false` (a refused subscription) is an error.
pub fn parse_bybit(text: &str) -> Result<Vec<KlineUpdate>, DataError> {
    let what = "a Bybit kline frame";
    let frame: BybitFrame = serde_json::from_str(text).map_err(|e| DataError::parse(what, e))?;
    if frame.success == Some(false) {
        return Err(DataError::Api {
            url: "Bybit stream".into(),
            message: frame.ret_msg.unwrap_or_default(),
        });
    }
    let is_kline = frame
        .topic
        .as_deref()
        .is_some_and(|t| t.starts_with("kline."));
    let (true, Some(data)) = (is_kline, frame.data) else {
        return Ok(Vec::new());
    };
    let klines: Vec<BybitKline> =
        serde_json::from_value(data).map_err(|e| DataError::parse(what, e))?;
    klines
        .into_iter()
        .map(|k| {
            if k.interval != "1" {
                return Err(DataError::parse(
                    what,
                    format!("interval {:?}, not 1", k.interval),
                ));
            }
            let candle = Candle {
                open_time: k.start,
                open: number(what, &k.open)?,
                high: number(what, &k.high)?,
                low: number(what, &k.low)?,
                close: number(what, &k.close)?,
                volume: number(what, &k.volume)?,
                quote_volume: number(what, &k.turnover)?,
            };
            Ok(KlineUpdate {
                candle,
                closed: k.confirm,
            })
        })
        .collect()
}

fn number(what: &str, text: &str) -> Result<f64, DataError> {
    text.parse()
        .map_err(|_| DataError::parse(what, format!("{text:?} is not a number")))
}
