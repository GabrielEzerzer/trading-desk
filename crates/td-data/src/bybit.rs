//! Bybit history from the v5 REST API: klines (`/v5/market/kline`) and funding rates
//! (`/v5/market/funding/history`). Both return pages newest first, so they are read backwards from
//! the end of the range.

use std::time::Duration;

use serde::Deserialize;
use td_core::{Candle, InstrumentId, Millis, Timeframe};

use crate::DataError;
use crate::http::Http;
use crate::instruments::{bybit_category, bybit_result};
use crate::time::{ROUNDING_PAD, funding_on_the_minute, now_ms, round_to_minute};

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

/// Bybit's name for a kline interval.
fn interval(timeframe: Timeframe) -> &'static str {
    match timeframe {
        Timeframe::M1 => "1",
        Timeframe::M5 => "5",
        Timeframe::M15 => "15",
        Timeframe::M30 => "30",
        Timeframe::H1 => "60",
        Timeframe::H4 => "240",
        Timeframe::D1 => "D",
        Timeframe::W1 => "W",
    }
}

/// Downloads the klines of `timeframe` for a spot or linear instrument with open times in
/// `[from, to)`, oldest first. A bar still in progress is left out.
pub fn fetch_klines(
    http: &Http,
    id: &InstrumentId,
    timeframe: Timeframe,
    from: Millis,
    to: Millis,
) -> Result<Vec<Candle>, DataError> {
    let end = to.min(timeframe.floor(now_ms())) - 1;
    let open_time = |c: &Candle| c.open_time;
    page_backwards(from, end, KLINE_LIMIT, open_time, |page_end| {
        let query = [
            ("category", bybit_category(id.kind).to_string()),
            ("symbol", id.symbol.clone()),
            ("interval", interval(timeframe).to_string()),
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
/// given (newest first). Funding times are rounded to the nearest minute (see
/// [`round_to_minute`]), the same rule as for Binance.
pub fn parse_funding_page(body: &str) -> Result<Vec<(Millis, f64)>, DataError> {
    let rates = parse_funding_stamps(body)?;
    Ok(rates
        .into_iter()
        .map(|(t, rate)| (round_to_minute(t), rate))
        .collect())
}

/// [`parse_funding_page`] with the times as Bybit stamps them, for paging.
fn parse_funding_stamps(body: &str) -> Result<Vec<(Millis, f64)>, DataError> {
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
    read_funding(from, to, |start, page_end| {
        let query = [
            ("category", "linear".to_string()),
            ("symbol", symbol.to_string()),
            ("startTime", start.to_string()),
            ("endTime", page_end.to_string()),
            ("limit", FUNDING_LIMIT.to_string()),
        ];
        let page = parse_funding_stamps(&http.get_text(FUNDING_URL, &query)?)?;
        std::thread::sleep(DELAY);
        Ok(page)
    })
}

/// Reads the funding rates in `[from, to)` through `fetch(start, end)`, which returns the newest
/// [`FUNDING_LIMIT`] rates stamped in `[start, end]`, newest first, with the times as the venue
/// stamps them.
fn read_funding(
    from: Millis,
    to: Millis,
    mut fetch: impl FnMut(Millis, Millis) -> Result<Vec<(Millis, f64)>, DataError>,
) -> Result<Vec<(Millis, f64)>, DataError> {
    // Paged by the stamps themselves, so each page ends just before the oldest stamp read; only
    // then are the times put on the minute and kept to the range.
    let (lo, end) = (from - ROUNDING_PAD, to + ROUNDING_PAD - 1);
    let funding_time = |&(t, _): &(Millis, f64)| t;
    let stamped = page_backwards(lo, end, FUNDING_LIMIT, funding_time, |page_end| {
        fetch(lo, page_end)
    })?;
    Ok(funding_on_the_minute(stamped, from, to))
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

    /// A newest-first funding endpoint holding `stamps`, oldest first: it answers with the newest
    /// [`FUNDING_LIMIT`] rates stamped in the range asked for, newest first, with the times as
    /// stamped.
    fn venue(
        stamps: &[(Millis, f64)],
    ) -> impl FnMut(Millis, Millis) -> Result<Vec<(Millis, f64)>, DataError> + '_ {
        move |start, end| {
            let page = stamps.iter().rev().copied();
            Ok(page
                .filter(|(t, _)| (start..=end).contains(t))
                .take(FUNDING_LIMIT)
                .collect())
        }
    }

    const H8: Millis = 8 * 3_600_000;
    /// 2024-02-01 00:00 UTC: a funding time on both venues, and the start of a month.
    const T: Millis = 1_706_745_600_000;

    #[test]
    fn a_rate_stamped_just_before_a_window_boundary_is_read_once_in_the_window_it_rounds_into() {
        // 00:00 on 1 February stamped 5 ms early, between a rate on time and one 8 ms late.
        let stamps = [(T - H8, 0.0001), (T - 5, 0.0002), (T + H8 + 8, 0.0003)];
        let before = read_funding(T - H8, T, venue(&stamps)).expect("reads");
        let after = read_funding(T, T + H8, venue(&stamps)).expect("reads");
        assert_eq!(before, [(T - H8, 0.0001)]);
        assert_eq!(after, [(T, 0.0002)]);
    }

    #[test]
    fn two_stamps_on_one_minute_keep_the_later() {
        let stamps = [(T - 5, 0.0001), (T + 8, 0.0002)];
        let rates = read_funding(T - H8, T + H8, venue(&stamps)).expect("reads");
        assert_eq!(rates, [(T, 0.0002)]);
    }

    #[test]
    fn funding_paged_by_its_stamps_comes_back_once_each_on_the_minute() {
        // More rates than a page holds, stamped alternately 7 ms late and 5 ms early.
        let n = FUNDING_LIMIT as Millis + 3;
        let stamps: Vec<(Millis, f64)> = (0..n)
            .map(|i| (T + i * H8 + if i % 2 == 0 { 7 } else { -5 }, i as f64))
            .collect();
        let rates = read_funding(T, T + n * H8, venue(&stamps)).expect("reads");
        let expected: Vec<(Millis, f64)> = (0..n).map(|i| (T + i * H8, i as f64)).collect();
        assert_eq!(rates, expected);
    }
}
