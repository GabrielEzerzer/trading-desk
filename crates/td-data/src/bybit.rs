//! Bybit as a [`DataSource`], from the v5 REST API and public streams: spot and linear perpetual
//! instruments (`/v5/market/instruments-info`), klines (`/v5/market/kline`), which serve the
//! 1-minute history too, funding rates (`/v5/market/funding/history`) and the public kline streams.
//! Klines and funding come in pages newest first, so they are read backwards from the end of the
//! range.

use std::time::Duration;

use serde::Deserialize;
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, Timeframe, Venue,
};

use crate::http::Http;
use crate::instruments::number;
use crate::live::{Greeting, KlineUpdate, LiveStream, frame_number};
use crate::source::{DataSource, MinuteHistory};
use crate::time::{ROUNDING_PAD, funding_on_the_minute, now_ms, round_to_minute};
use crate::{DataError, InstrumentList};

// Base-tier fees: the non-VIP rates from Bybit's "Trading Fee Structure" help article. Written on
// 2026-11-10 from the rates as known then; the page could not be fetched that day to re-check
// them, so check before relying on them.

/// Bybit spot: 0.1% maker, 0.1% taker.
pub const BYBIT_SPOT_FEES: FeeSchedule = FeeSchedule {
    maker: 0.001,
    taker: 0.001,
    per_unit: 0.0,
};
/// Bybit linear perpetuals: 0.02% maker, 0.055% taker.
pub const BYBIT_LINEAR_FEES: FeeSchedule = FeeSchedule {
    maker: 0.0002,
    taker: 0.00055,
    per_unit: 0.0,
};

const INSTRUMENTS_URL: &str = "https://api.bybit.com/v5/market/instruments-info";
const KLINE_URL: &str = "https://api.bybit.com/v5/market/kline";
const FUNDING_URL: &str = "https://api.bybit.com/v5/market/funding/history";
/// The most rows a kline page holds.
const KLINE_LIMIT: usize = 1000;
/// The most rows a funding page holds.
const FUNDING_LIMIT: usize = 200;
/// A pause between requests, well inside Bybit's per-IP limit of 600 requests per 5 seconds.
const DELAY: Duration = Duration::from_millis(100);
/// Bybit closes a public connection that sends nothing for a while; it asks for a ping every 20 s.
const PING_EVERY: Duration = Duration::from_secs(20);

/// Bybit spot and linear perpetuals.
#[derive(Debug, Clone, Copy, Default)]
pub struct Bybit;

impl DataSource for Bybit {
    fn venue(&self) -> Venue {
        Venue::Bybit
    }

    fn kinds(&self) -> &'static [InstrumentKind] {
        &[InstrumentKind::Spot, InstrumentKind::Perpetual]
    }

    fn instruments(&self, http: &Http, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
        let mut list = InstrumentList::default();
        let mut cursor = String::new();
        loop {
            let mut query = vec![
                ("category", category(kind)?.to_string()),
                ("limit", "1000".to_string()),
            ];
            if !cursor.is_empty() {
                query.push(("cursor", cursor.clone()));
            }
            let body = http.get_text(INSTRUMENTS_URL, &query)?;
            let page = bybit_page(INSTRUMENTS_URL, &body)?;
            add_bybit(&mut list, kind, page.list);
            if page.next_page_cursor.is_empty() {
                return Ok(list);
            }
            cursor = page.next_page_cursor;
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn default_fees(&self, kind: InstrumentKind) -> Result<FeeSchedule, DataError> {
        match kind {
            InstrumentKind::Spot => Ok(BYBIT_SPOT_FEES),
            InstrumentKind::Perpetual => Ok(BYBIT_LINEAR_FEES),
            InstrumentKind::Fx => Err(not_listed(kind)),
        }
    }

    /// From the kline endpoint, which has the whole history.
    fn minute_history(
        &self,
        http: &Http,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
        _now: Millis,
    ) -> Result<MinuteHistory, DataError> {
        Ok(MinuteHistory {
            candles: fetch_klines(http, id, Timeframe::M1, from, to)?,
            asks: Vec::new(),
            missing_files: Vec::new(),
        })
    }

    fn funding(
        &self,
        http: &Http,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        fetch_funding(http, &id.symbol, from, to)
    }

    fn live(&self) -> Result<&dyn LiveStream, DataError> {
        Ok(self)
    }
}

impl LiveStream for Bybit {
    fn url(&self, id: &InstrumentId) -> String {
        // An empty URL for a kind the source does not list, which is never connected to: such a
        // stream refuses to open (see `SharedStream::open_on`).
        category(id.kind)
            .map(|category| format!("wss://stream.bybit.com/v5/public/{category}"))
            .unwrap_or_default()
    }

    /// A subscription to the symbol's 1-minute klines, and a ping every 20 s.
    fn greeting(&self, id: &InstrumentId) -> Greeting {
        Greeting {
            subscribe: Some(format!(
                r#"{{"op":"subscribe","args":["kline.1.{}"]}}"#,
                id.symbol
            )),
            heartbeat: Some((PING_EVERY, r#"{"op":"ping"}"#.into())),
        }
    }

    fn parse(&self, text: &str) -> Result<Vec<KlineUpdate>, DataError> {
        parse_stream_frame(text)
    }

    fn klines(
        &self,
        http: &Http,
        id: &InstrumentId,
        timeframe: Timeframe,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<Candle>, DataError> {
        fetch_klines(http, id, timeframe, from, to)
    }
}

/// Bybit's name for the market an instrument kind trades in.
fn category(kind: InstrumentKind) -> Result<&'static str, DataError> {
    match kind {
        InstrumentKind::Spot => Ok("spot"),
        InstrumentKind::Perpetual => Ok("linear"),
        InstrumentKind::Fx => Err(not_listed(kind)),
    }
}

fn not_listed(kind: InstrumentKind) -> DataError {
    DataError::KindNotListed {
        venue: Venue::Bybit,
        kind,
    }
}

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
    let category = category(id.kind)?;
    let end = to.min(timeframe.floor(now_ms())) - 1;
    let open_time = |c: &Candle| c.open_time;
    page_backwards(from, end, KLINE_LIMIT, open_time, |page_end| {
        let query = [
            ("category", category.to_string()),
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BybitResponse {
    ret_code: i64,
    ret_msg: String,
    /// Read only once `ret_code` says the call succeeded: error responses carry `{}` here.
    #[serde(default)]
    result: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BybitInstrumentPage {
    list: Vec<BybitInstrument>,
    #[serde(default)]
    next_page_cursor: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BybitInstrument {
    symbol: String,
    status: String,
    base_coin: String,
    quote_coin: String,
    /// Linear only: `LinearPerpetual` or `LinearFutures`.
    contract_type: Option<String>,
    price_filter: BybitPriceFilter,
    lot_size_filter: BybitLotFilter,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BybitPriceFilter {
    tick_size: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BybitLotFilter {
    min_order_qty: String,
    /// Spot's quantity step.
    base_precision: Option<String>,
    /// Spot's minimum order value.
    min_order_amt: Option<String>,
    /// Linear's quantity step.
    qty_step: Option<String>,
    /// Linear's minimum order value.
    min_notional_value: Option<String>,
}

/// Unwraps a Bybit v5 response, turning a non-zero `retCode` into an error.
fn bybit_result<T: serde::de::DeserializeOwned>(url: &str, body: &str) -> Result<T, DataError> {
    let response: BybitResponse =
        serde_json::from_str(body).map_err(|e| DataError::parse(url, e))?;
    if response.ret_code != 0 {
        return Err(DataError::Api {
            url: url.to_string(),
            message: format!("retCode {}: {}", response.ret_code, response.ret_msg),
        });
    }
    serde_json::from_value(response.result).map_err(|e| DataError::parse(url, e))
}

fn bybit_page(url: &str, body: &str) -> Result<BybitInstrumentPage, DataError> {
    bybit_result(url, body)
}

/// Reads one page of `/v5/market/instruments-info`: every instrument with status `Trading`, and
/// for linear only the `LinearPerpetual` contracts.
pub fn parse_bybit_instruments(
    body: &str,
    kind: InstrumentKind,
) -> Result<InstrumentList, DataError> {
    let page = bybit_page("Bybit instruments-info", body)?;
    let mut list = InstrumentList::default();
    add_bybit(&mut list, kind, page.list);
    Ok(list)
}

fn add_bybit(list: &mut InstrumentList, kind: InstrumentKind, page: Vec<BybitInstrument>) {
    for s in page {
        let listed = s.status == "Trading"
            && match kind {
                InstrumentKind::Spot => true,
                InstrumentKind::Perpetual => s.contract_type.as_deref() == Some("LinearPerpetual"),
                InstrumentKind::Fx => false,
            };
        if listed {
            list.push(&s.symbol, bybit_instrument(&s, kind));
        }
    }
}

fn bybit_instrument(s: &BybitInstrument, kind: InstrumentKind) -> Result<Instrument, String> {
    let lot = &s.lot_size_filter;
    let (step, min_value) = match kind {
        InstrumentKind::Spot => (&lot.base_precision, &lot.min_order_amt),
        InstrumentKind::Perpetual => (&lot.qty_step, &lot.min_notional_value),
        InstrumentKind::Fx => return Err(not_listed(kind).to_string()),
    };
    let required = |name: &str, value: &Option<String>| -> Result<f64, String> {
        number(name, value.as_deref().ok_or_else(|| format!("no {name}"))?)
    };
    Ok(Instrument {
        id: InstrumentId {
            venue: Venue::Bybit,
            symbol: s.symbol.clone(),
            kind,
        },
        base: s.base_coin.clone(),
        quote: s.quote_coin.clone(),
        tick_size: number("tickSize", &s.price_filter.tick_size)?,
        lot_size: required("quantity step", step)?,
        min_qty: number("minOrderQty", &lot.min_order_qty)?,
        min_notional: required("minimum order value", min_value)?,
        fees: Bybit.default_fees(kind).map_err(|e| e.to_string())?,
    })
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
pub fn parse_stream_frame(text: &str) -> Result<Vec<KlineUpdate>, DataError> {
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
                open: frame_number(what, &k.open)?,
                high: frame_number(what, &k.high)?,
                low: frame_number(what, &k.low)?,
                close: frame_number(what, &k.close)?,
                volume: frame_number(what, &k.volume)?,
                quote_volume: frame_number(what, &k.turnover)?,
            };
            Ok(KlineUpdate {
                candle,
                closed: k.confirm,
            })
        })
        .collect()
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
