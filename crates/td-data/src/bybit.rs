//! Bybit history from the v5 REST API: 1-minute klines (`/v5/market/kline`) and funding rates
//! (`/v5/market/funding/history`). Both return pages newest first, so they are read backwards from
//! the end of the range.

use std::time::Duration;

use serde::Deserialize;
use td_core::{Candle, InstrumentId, Millis};

use crate::DataError;
use crate::http::Http;
use crate::instruments::{bybit_category, bybit_result};
use crate::time::{MINUTE, now_ms};

const KLINE_URL: &str = "https://api.bybit.com/v5/market/kline";
const FUNDING_URL: &str = "https://api.bybit.com/v5/market/funding/history";
/// The most rows a kline page holds.
const KLINE_LIMIT: usize = 1000;
/// The most rows a funding page holds.
const FUNDING_LIMIT: usize = 200;
/// A pause between requests, well inside Bybit's per-IP limit of 600 requests per 5 seconds.
const DELAY: Duration = Duration::from_millis(100);

#[derive(Debug, Deserialize)]
struct KlinePage {
    /// `[startTime, open, high, low, close, volume, turnover]`, all strings, newest first.
    list: Vec<Vec<String>>,
}

/// Parses one `/v5/market/kline` response into candles, in the order given (newest first).
/// Volume is in the base asset (contracts, which are base units on linear), turnover in the quote.
pub fn parse_kline_page(body: &str) -> Result<Vec<Candle>, DataError> {
    let page: KlinePage = bybit_result(KLINE_URL, body)?;
    page.list
        .iter()
        .map(|row| {
            let field = |i: usize| {
                row.get(i).ok_or_else(|| {
                    DataError::parse(KLINE_URL, format!("a row has {} fields, not 7", row.len()))
                })
            };
            let float = |i: usize| -> Result<f64, DataError> {
                let text = field(i)?;
                text.parse()
                    .map_err(|_| DataError::parse(KLINE_URL, format!("{text:?} is not a number")))
            };
            let time = field(0)?;
            let open_time: Millis = time
                .parse()
                .map_err(|_| DataError::parse(KLINE_URL, format!("{time:?} is not a time")))?;
            Ok(Candle {
                open_time,
                open: float(1)?,
                high: float(2)?,
                low: float(3)?,
                close: float(4)?,
                volume: float(5)?,
                quote_volume: float(6)?,
            })
        })
        .collect()
}

/// Downloads the 1-minute klines of a spot or linear instrument with open times in `[from, to)`,
/// oldest first. A bar still in progress is left out.
pub fn fetch_klines(
    http: &Http,
    id: &InstrumentId,
    from: Millis,
    to: Millis,
) -> Result<Vec<Candle>, DataError> {
    let closed_before = now_ms() - MINUTE;
    let mut candles = Vec::new();
    let mut end = to.min(closed_before + 1) - 1;
    while end >= from {
        let query = [
            ("category", bybit_category(id.kind).to_string()),
            ("symbol", id.symbol.clone()),
            ("interval", "1".to_string()),
            ("start", from.to_string()),
            ("end", end.to_string()),
            ("limit", KLINE_LIMIT.to_string()),
        ];
        let page = parse_kline_page(&http.get_text(KLINE_URL, &query)?)?;
        std::thread::sleep(DELAY);
        let full = page.len() == KLINE_LIMIT;
        let Some(oldest) = page.iter().map(|c| c.open_time).min() else {
            break;
        };
        candles.extend(page);
        if !full {
            break;
        }
        end = oldest - 1;
    }
    candles.retain(|c| (from..to).contains(&c.open_time) && c.open_time <= closed_before);
    candles.sort_by_key(|c| c.open_time);
    Ok(candles)
}

#[derive(Debug, Deserialize)]
struct FundingPage {
    list: Vec<FundingRow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FundingRow {
    funding_rate: String,
    funding_rate_timestamp: String,
}

/// Parses one `/v5/market/funding/history` response into `(funding time, rate)`, in the order
/// given (newest first).
pub fn parse_funding_page(body: &str) -> Result<Vec<(Millis, f64)>, DataError> {
    let page: FundingPage = bybit_result(FUNDING_URL, body)?;
    page.list
        .iter()
        .map(|row| {
            let time = row.funding_rate_timestamp.parse::<Millis>().map_err(|_| {
                DataError::parse(
                    FUNDING_URL,
                    format!("{:?} is not a time", row.funding_rate_timestamp),
                )
            })?;
            let rate = row.funding_rate.parse::<f64>().map_err(|_| {
                DataError::parse(
                    FUNDING_URL,
                    format!("{:?} is not a number", row.funding_rate),
                )
            })?;
            Ok((time, rate))
        })
        .collect()
}

/// Downloads the funding rates of a linear perpetual in `[from, to)`, oldest first.
pub fn fetch_funding(
    http: &Http,
    symbol: &str,
    from: Millis,
    to: Millis,
) -> Result<Vec<(Millis, f64)>, DataError> {
    let mut rates = Vec::new();
    let mut end = to - 1;
    while end >= from {
        let query = [
            ("category", "linear".to_string()),
            ("symbol", symbol.to_string()),
            ("startTime", from.to_string()),
            ("endTime", end.to_string()),
            ("limit", FUNDING_LIMIT.to_string()),
        ];
        let page = parse_funding_page(&http.get_text(FUNDING_URL, &query)?)?;
        std::thread::sleep(DELAY);
        let full = page.len() == FUNDING_LIMIT;
        let Some(oldest) = page.iter().map(|&(t, _)| t).min() else {
            break;
        };
        rates.extend(page);
        if !full {
            break;
        }
        end = oldest - 1;
    }
    rates.retain(|&(t, _)| (from..to).contains(&t));
    rates.sort_by_key(|&(t, _)| t);
    rates.dedup_by_key(|&mut (t, _)| t);
    Ok(rates)
}
