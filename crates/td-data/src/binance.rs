// Adapted from tipping-point engine/src/store/import.rs at 7dc6d12 (SHA-256 of an archive, the
// published .CHECKSUM, and reading an archive's one member) and engine/src/venue/binance/archive.rs
// at 7dc6d12 (archive URLs and CSV rows).

//! Binance history: 1-minute klines from the public archive at data.binance.vision, recent klines
//! from the spot and USD-M REST APIs, and funding rates from the USD-M REST API.
//!
//! Archive files are `<SYMBOL>-1m-<YYYY-MM>.zip` (monthly) and `<SYMBOL>-1m-<YYYY-MM-DD>.zip`
//! (daily), each holding one CSV of the same name and published with a `.CHECKSUM` beside it
//! (`<sha256>  <file name>`). A row is
//! `open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_buy_volume,taker_buy_quote_volume,ignore`;
//! USD-M files from 2022 on start with that header line. Spot files from 2025-01-01 on give times
//! in microseconds, earlier files in milliseconds.

use std::fmt::Write as _;
use std::io::{Cursor, Read};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use td_core::{Candle, InstrumentId, InstrumentKind, Millis, Timeframe};

use crate::DataError;
use crate::http::Http;
use crate::time::{MINUTE, Month, format_date, now_ms, round_to_minute};

const ARCHIVE: &str = "https://data.binance.vision/data";
const SPOT_KLINES_URL: &str = "https://api.binance.com/api/v3/klines";
const USDM_KLINES_URL: &str = "https://fapi.binance.com/fapi/v1/klines";
/// The most rows a kline request returns on spot (USD-M allows 1500; one limit keeps it simple).
const KLINE_LIMIT: usize = 1000;
/// A pause between kline pages, well inside both APIs' request-weight limits.
const KLINE_DELAY: Duration = Duration::from_millis(200);
const FUNDING_URL: &str = "https://fapi.binance.com/fapi/v1/fundingRate";
/// The most rows `/fapi/v1/fundingRate` returns at once.
const FUNDING_LIMIT: usize = 1000;
/// The funding endpoint shares a limit of 500 requests per 5 minutes per IP; one request every
/// 0.6 s stays inside it.
const FUNDING_DELAY: Duration = Duration::from_millis(600);
/// Times at or above this are microseconds: 1e14 ms is the year 5138, 1e14 µs is 1973.
const MICROS_FROM: i64 = 100_000_000_000_000;

/// The span one archive file covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchivePeriod {
    Month(Month),
    /// The day that starts at this midnight UTC.
    Day(Millis),
}

/// The archive file's name, `<SYMBOL>-1m-<period>.zip`.
pub fn archive_file_name(symbol: &str, period: ArchivePeriod) -> String {
    let period = match period {
        ArchivePeriod::Month(month) => month.to_string(),
        ArchivePeriod::Day(day) => format_date(day),
    };
    format!("{symbol}-1m-{period}.zip")
}

/// Where the archive publishes the 1-minute klines of `id` for `period`.
pub fn archive_url(id: &InstrumentId, period: ArchivePeriod) -> String {
    let market = match id.kind {
        InstrumentKind::Spot => "spot",
        InstrumentKind::Perpetual => "futures/um",
    };
    let span = match period {
        ArchivePeriod::Month(_) => "monthly",
        ArchivePeriod::Day(_) => "daily",
    };
    format!(
        "{ARCHIVE}/{market}/{span}/klines/{sym}/1m/{file}",
        sym = id.symbol,
        file = archive_file_name(&id.symbol, period)
    )
}

/// Downloads one archive file, checks it against its `.CHECKSUM` and parses its klines. `None`
/// when the archive has no such file (before the symbol listed, or not yet published).
pub fn fetch_archive(
    http: &Http,
    id: &InstrumentId,
    period: ArchivePeriod,
) -> Result<Option<Vec<Candle>>, DataError> {
    let url = archive_url(id, period);
    let file = archive_file_name(&id.symbol, period);
    let Some(zip) = http.get_bytes(&url)? else {
        return Ok(None);
    };
    let checksum =
        http.get_bytes(&format!("{url}.CHECKSUM"))?
            .ok_or_else(|| DataError::Archive {
                file: file.clone(),
                reason: "no .CHECKSUM is published beside it".into(),
            })?;
    verify_checksum(&file, &zip, &String::from_utf8_lossy(&checksum))?;
    let csv = read_archive_csv(&file, &zip)?;
    parse_kline_csv(&file, &csv).map(Some)
}

/// The SHA-256 of `bytes`, in lower-case hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Checks `bytes` against the text of the `.CHECKSUM` published beside `file`:
/// `<sha256>  <file name>`.
pub fn verify_checksum(file: &str, bytes: &[u8], checksum: &str) -> Result<(), DataError> {
    let expected = checksum
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let well_formed = expected.len() == 64 && expected.bytes().all(|b| b.is_ascii_hexdigit());
    if !well_formed {
        return Err(DataError::Archive {
            file: file.to_string(),
            reason: format!("its .CHECKSUM does not start with a SHA-256: {checksum:?}"),
        });
    }
    let actual = sha256_hex(bytes);
    if actual != expected {
        return Err(DataError::ChecksumMismatch {
            file: file.to_string(),
            expected,
            actual,
        });
    }
    Ok(())
}

/// The text of the one CSV an archive zip holds, which must be named after the zip. The zip's CRC
/// is checked as the member is read.
pub fn read_archive_csv(file: &str, bytes: &[u8]) -> Result<String, DataError> {
    let bad = |reason: String| DataError::Archive {
        file: file.to_string(),
        reason,
    };
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| bad(e.to_string()))?;
    if zip.len() != 1 {
        return Err(bad(format!("it holds {} files, not one", zip.len())));
    }
    let mut member = zip.by_index(0).map_err(|e| bad(e.to_string()))?;
    let held = member.name().map_err(|e| bad(e.to_string()))?.into_owned();
    let expected = format!("{}.csv", file.strip_suffix(".zip").unwrap_or(file));
    if held != expected {
        return Err(bad(format!("it holds {held:?}, not {expected:?}")));
    }
    let mut text = String::new();
    member
        .read_to_string(&mut text)
        .map_err(|e| bad(e.to_string()))?;
    Ok(text)
}

/// Parses archive kline rows into candles. A header line is skipped; times in microseconds are
/// converted to milliseconds.
pub fn parse_kline_csv(file: &str, text: &str) -> Result<Vec<Candle>, DataError> {
    let mut candles = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || (index == 0 && line.starts_with("open_time")) {
            continue;
        }
        let what = || format!("{file} line {}", index + 1);
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 8 {
            return Err(DataError::parse(
                what(),
                format!("{} fields, expected at least 8", fields.len()),
            ));
        }
        let float = |i: usize| {
            fields[i]
                .parse::<f64>()
                .map_err(|_| DataError::parse(what(), format!("{:?} is not a number", fields[i])))
        };
        let raw_time = fields[0]
            .parse::<i64>()
            .map_err(|_| DataError::parse(what(), format!("{:?} is not a time", fields[0])))?;
        let open_time = normalise_time(raw_time);
        if open_time.rem_euclid(MINUTE) != 0 {
            return Err(DataError::parse(
                what(),
                format!("open time {raw_time} is not on a minute"),
            ));
        }
        candles.push(Candle {
            open_time,
            open: float(1)?,
            high: float(2)?,
            low: float(3)?,
            close: float(4)?,
            volume: float(5)?,
            quote_volume: float(7)?,
        });
    }
    Ok(candles)
}

/// Milliseconds from an archive time in milliseconds or microseconds, told apart by magnitude.
pub fn normalise_time(t: i64) -> Millis {
    if t >= MICROS_FROM { t / 1000 } else { t }
}

/// Parses a `/api/v3/klines` or `/fapi/v1/klines` response into candles, oldest first. A row is
/// `[openTime, open, high, low, close, volume, closeTime, quoteVolume, ...]` with the prices and
/// volumes as strings.
pub fn parse_klines(body: &str) -> Result<Vec<Candle>, DataError> {
    let what = "Binance klines";
    let rows: Vec<Vec<serde_json::Value>> =
        serde_json::from_str(body).map_err(|e| DataError::parse(what, e))?;
    rows.iter()
        .map(|row| {
            let field = |i: usize| {
                row.get(i).ok_or_else(|| {
                    DataError::parse(what, format!("a row has {} fields, not 12", row.len()))
                })
            };
            let float = |i: usize| -> Result<f64, DataError> {
                let value = field(i)?;
                value
                    .as_str()
                    .and_then(|text| text.parse().ok())
                    .ok_or_else(|| DataError::parse(what, format!("{value} is not a number")))
            };
            let time = field(0)?;
            let open_time = time
                .as_i64()
                .ok_or_else(|| DataError::parse(what, format!("{time} is not a time")))?;
            Ok(Candle {
                open_time,
                open: float(1)?,
                high: float(2)?,
                low: float(3)?,
                close: float(4)?,
                volume: float(5)?,
                quote_volume: float(7)?,
            })
        })
        .collect()
}

/// Downloads the klines of `timeframe` for a spot or USD-M instrument with open times in
/// `[from, to)` from the REST API, oldest first. A bar still in progress is left out.
pub fn fetch_klines(
    http: &Http,
    id: &InstrumentId,
    timeframe: Timeframe,
    from: Millis,
    to: Millis,
) -> Result<Vec<Candle>, DataError> {
    let url = match id.kind {
        InstrumentKind::Spot => SPOT_KLINES_URL,
        InstrumentKind::Perpetual => USDM_KLINES_URL,
    };
    let to = to.min(timeframe.floor(now_ms()));
    let open_time = |c: &Candle| c.open_time;
    let mut candles = page_forwards(from, to, KLINE_LIMIT, open_time, |start| {
        if start > from {
            std::thread::sleep(KLINE_DELAY);
        }
        let query = [
            ("symbol", id.symbol.clone()),
            ("interval", timeframe.as_str().to_string()),
            ("startTime", start.to_string()),
            ("endTime", (to - 1).to_string()),
            ("limit", KLINE_LIMIT.to_string()),
        ];
        parse_klines(&http.get_text(url, &query)?)
    })?;
    candles.retain(|c| (from..to).contains(&c.open_time));
    Ok(candles)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FundingRow {
    funding_time: Millis,
    funding_rate: String,
}

/// Parses a `/fapi/v1/fundingRate` response into `(funding time, rate)`, oldest first. Funding
/// times are rounded to the nearest minute (see [`round_to_minute`]): Binance can stamp a rate a
/// few milliseconds after the hour it belongs to.
pub fn parse_funding(body: &str) -> Result<Vec<(Millis, f64)>, DataError> {
    let rows: Vec<FundingRow> =
        serde_json::from_str(body).map_err(|e| DataError::parse(FUNDING_URL, e))?;
    rows.into_iter()
        .map(|row| {
            let rate = row.funding_rate.parse::<f64>().map_err(|_| {
                DataError::parse(
                    FUNDING_URL,
                    format!("rate {:?} is not a number", row.funding_rate),
                )
            })?;
            Ok((round_to_minute(row.funding_time), rate))
        })
        .collect()
}

/// Downloads the funding rates of a USD-M perpetual in `[from, to)`, paging forward by start time.
pub fn fetch_funding(
    http: &Http,
    symbol: &str,
    from: Millis,
    to: Millis,
) -> Result<Vec<(Millis, f64)>, DataError> {
    let mut rates = page_forwards(
        from,
        to,
        FUNDING_LIMIT,
        |&(t, _)| t,
        |start| {
            // A pause between pages, not before the first.
            if start > from {
                std::thread::sleep(FUNDING_DELAY);
            }
            let query = [
                ("symbol", symbol.to_string()),
                ("startTime", start.to_string()),
                ("endTime", (to - 1).to_string()),
                ("limit", FUNDING_LIMIT.to_string()),
            ];
            parse_funding(&http.get_text(FUNDING_URL, &query)?)
        },
    )?;
    // A page starts one millisecond after the previous page's last time, which rounding may have
    // moved back from the venue's stamp, so that rate can come again at the top of the next page.
    rates.dedup_by_key(|&mut (t, _)| t);
    Ok(rates)
}

/// Reads an oldest-first endpoint forwards over `[from, to)`. `fetch(start)` returns a page of up
/// to `limit` rows, the oldest at or after `start`; after a full page the next one starts just
/// after its last row, and a short page is the last.
fn page_forwards<T>(
    from: Millis,
    to: Millis,
    limit: usize,
    time: impl Fn(&T) -> Millis,
    mut fetch: impl FnMut(Millis) -> Result<Vec<T>, DataError>,
) -> Result<Vec<T>, DataError> {
    let mut rows = Vec::new();
    let mut start = from;
    while start < to {
        let page = fetch(start)?;
        let full = page.len() == limit;
        let Some(last) = page.last().map(&time) else {
            break;
        };
        rows.extend(page);
        if !full {
            break;
        }
        // A full page whose last row is before the start asked for would page forever.
        if last < start {
            return Err(DataError::Parse {
                what: "a page of rows".into(),
                reason: format!("a full page starting at {start} ended at {last}"),
            });
        }
        start = last + 1;
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_page_that_does_not_move_forward_is_an_error() {
        let time = |&(t, _): &(Millis, f64)| t;
        let result = page_forwards(100, 200, 2, time, |_| Ok(vec![(10, 0.0), (20, 0.0)]));
        assert!(matches!(result, Err(DataError::Parse { .. })));
    }

    /// Pages forwards over `[from, to)` through an oldest-first endpoint holding a rate at each of
    /// `times`, which answers with the oldest `limit` of them in the range asked for. Returns the
    /// times read and the start of each page asked for.
    fn read_forward(
        times: &[Millis],
        from: Millis,
        to: Millis,
        limit: usize,
    ) -> (Vec<Millis>, Vec<Millis>) {
        let mut starts = Vec::new();
        let rates = page_forwards(
            from,
            to,
            limit,
            |&(t, _)| t,
            |start| {
                starts.push(start);
                let page = times.iter().map(|&t| (t, 0.0001));
                Ok(page
                    .filter(|(t, _)| (start..to).contains(t))
                    .take(limit)
                    .collect())
            },
        )
        .expect("pages");
        (rates.into_iter().map(|(t, _)| t).collect(), starts)
    }

    #[test]
    fn forward_paging_neither_drops_nor_repeats_a_rate_at_a_page_boundary() {
        // Rates one millisecond apart, so the start of the second page, one after the last rate of
        // the full first page, lands exactly on a rate. The second page is short.
        let limit = FUNDING_LIMIT as Millis;
        let times: Vec<Millis> = (0..limit + 3).collect();
        let (read, starts) = read_forward(&times, 0, limit + 3, FUNDING_LIMIT);
        assert_eq!(read, times);
        assert_eq!(starts, [0, limit]);
        // A full page that reaches the end of the range is the last one asked for.
        let (read, starts) = read_forward(&times, 3, limit + 3, FUNDING_LIMIT);
        assert_eq!(read, times[3..]);
        assert_eq!(starts, [3]);
    }
}
