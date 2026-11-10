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
    let end = to.min(closed_before + 1) - 1;
    let open_time = |c: &Candle| c.open_time;
    page_backwards(from, end, KLINE_LIMIT, open_time, |page_end| {
        let query = [
            ("category", bybit_category(id.kind).to_string()),
            ("symbol", id.symbol.clone()),
            ("interval", "1".to_string()),
            ("start", from.to_string()),
            ("end", page_end.to_string()),
            ("limit", KLINE_LIMIT.to_string()),
        ];
        let page = parse_kline_page(&http.get_text(KLINE_URL, &query)?)?;
        std::thread::sleep(DELAY);
        Ok(page)
    })
}

/// Reads a newest-first endpoint backwards over `[from, end]`. `fetch(end)` returns a page of up
/// to `limit` rows, the newest at or before `end`; after a full page the next one ends just before
/// its oldest row, and a short page is the last. Returns the rows in `[from, end]`, oldest first.
fn page_backwards<T>(
    from: Millis,
    end: Millis,
    limit: usize,
    time: impl Fn(&T) -> Millis,
    mut fetch: impl FnMut(Millis) -> Result<Vec<T>, DataError>,
) -> Result<Vec<T>, DataError> {
    let mut rows = Vec::new();
    let mut page_end = end;
    while page_end >= from {
        let page = fetch(page_end)?;
        let full = page.len() == limit;
        let Some(oldest) = page.iter().map(&time).min() else {
            break;
        };
        rows.extend(page);
        if !full {
            break;
        }
        // A full page whose oldest row is not older than the page asked for would page forever.
        if oldest > page_end {
            return Err(DataError::Parse {
                what: "a page of rows".into(),
                reason: format!("a full page ending at {page_end} held rows from {oldest}"),
            });
        }
        page_end = oldest - 1;
    }
    rows.retain(|row| (from..=end).contains(&time(row)));
    rows.sort_by_key(&time);
    Ok(rows)
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
    let funding_time = |&(t, _): &(Millis, f64)| t;
    let mut rates = page_backwards(from, to - 1, FUNDING_LIMIT, funding_time, |page_end| {
        let query = [
            ("category", "linear".to_string()),
            ("symbol", symbol.to_string()),
            ("startTime", from.to_string()),
            ("endTime", page_end.to_string()),
            ("limit", FUNDING_LIMIT.to_string()),
        ];
        let page = parse_funding_page(&http.get_text(FUNDING_URL, &query)?)?;
        std::thread::sleep(DELAY);
        Ok(page)
    })?;
    rates.dedup_by_key(|&mut (t, _)| t);
    Ok(rates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_page_that_does_not_move_back_is_an_error() {
        let result = page_backwards(0, 100, 2, |t: &Millis| *t, |_| Ok(vec![150, 160]));
        assert!(matches!(result, Err(DataError::Parse { .. })));
    }

    /// Pages backwards over `[from, end]` through a newest-first endpoint holding `times`, which
    /// answers with the newest `limit` of them in the range asked for. Returns the rows read and
    /// the end of each page asked for.
    fn read_back(
        times: &[Millis],
        from: Millis,
        end: Millis,
        limit: usize,
    ) -> (Vec<Millis>, Vec<Millis>) {
        let mut ends = Vec::new();
        let rows = page_backwards(
            from,
            end,
            limit,
            |&t: &Millis| t,
            |page_end| {
                ends.push(page_end);
                let page = times.iter().rev().copied();
                Ok(page
                    .filter(|t| (from..=page_end).contains(t))
                    .take(limit)
                    .collect())
            },
        )
        .expect("pages");
        (rows, ends)
    }

    #[test]
    fn backward_paging_neither_drops_nor_repeats_a_row_at_a_page_boundary() {
        for limit in [KLINE_LIMIT, FUNDING_LIMIT] {
            // Rows one millisecond apart, so the end of the second page, one before the oldest row
            // of the full first page, lands exactly on a row. The second page is short.
            let n = limit as Millis + 7;
            let times: Vec<Millis> = (0..n).collect();
            let (rows, ends) = read_back(&times, 0, n - 1, limit);
            assert_eq!(rows, times, "page size {limit}");
            assert_eq!(ends, [n - 1, 6], "page size {limit}");
            // A full page that reaches back to `from` is the last one asked for.
            let (rows, ends) = read_back(&times[7..], 7, n - 1, limit);
            assert_eq!(rows, times[7..], "page size {limit}");
            assert_eq!(ends, [n - 1], "page size {limit}");
        }
    }

    #[test]
    fn backward_paging_keeps_only_rows_in_the_range() {
        // An endpoint that also returns a row after `end` and one before `from`.
        let rows = page_backwards(
            10,
            20,
            1000,
            |&t: &Millis| t,
            |_| Ok(vec![25, 20, 15, 10, 5]),
        )
        .expect("pages");
        assert_eq!(rows, [10, 15, 20]);
    }
}
