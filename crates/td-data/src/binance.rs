// Adapted from tipping-point engine/src/store/import.rs at 7dc6d12 (SHA-256 of an archive, the
// published .CHECKSUM, and reading an archive's one member) and engine/src/venue/binance/archive.rs
// at 7dc6d12 (archive URLs and CSV rows), and engine/src/venue/binance/rest.rs at 7dc6d12 (reading
// the PRICE_FILTER and LOT_SIZE filters from exchangeInfo).

//! Binance as a [`DataSource`]: spot and USD-M perpetual instruments from `exchangeInfo`,
//! 1-minute history from the public archive at data.binance.vision, recent klines from the spot
//! and USD-M REST APIs, funding rates from the USD-M REST API, and the public kline streams.
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
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, Timeframe, Venue,
};

use crate::http::Http;
use crate::instruments::number;
use crate::live::{KlineUpdate, LiveStream, frame_number};
use crate::source::{DataSource, MinuteHistory};
use crate::time::{
    DAY, MINUTE, Month, ROUNDING_PAD, format_date, funding_on_the_minute, now_ms, round_to_minute,
};
use crate::{DataError, InstrumentList};

const ARCHIVE: &str = "https://data.binance.vision/data";
const SPOT_INFO_URL: &str = "https://api.binance.com/api/v3/exchangeInfo";
const USDM_INFO_URL: &str = "https://fapi.binance.com/fapi/v1/exchangeInfo";
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

// Base-tier fees: the regular user (VIP 0) rates without the BNB discount, from
// binance.com/en/fee/schedule. Written on 2026-11-10 from the rates as known then; the page could
// not be fetched that day to re-check them, so check before relying on them.

/// Binance spot: 0.1% maker, 0.1% taker.
pub const BINANCE_SPOT_FEES: FeeSchedule = FeeSchedule {
    maker: 0.001,
    taker: 0.001,
};
/// Binance USD-M futures: 0.02% maker, 0.05% taker.
pub const BINANCE_USDM_FEES: FeeSchedule = FeeSchedule {
    maker: 0.0002,
    taker: 0.0005,
};

/// Binance spot and USD-M perpetuals.
#[derive(Debug, Clone, Copy, Default)]
pub struct Binance;

impl DataSource for Binance {
    fn venue(&self) -> Venue {
        Venue::Binance
    }

    fn kinds(&self) -> &'static [InstrumentKind] {
        &[InstrumentKind::Spot, InstrumentKind::Perpetual]
    }

    fn instruments(&self, http: &Http, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
        match kind {
            InstrumentKind::Spot => fetch_spot_instruments(http, SPOT_INFO_URL),
            // `/fapi/v1/exchangeInfo` takes no filters; gzip alone shrinks it.
            InstrumentKind::Perpetual => parse_binance_usdm(&http.get_text(USDM_INFO_URL, &[])?),
            InstrumentKind::Fx => Err(not_listed(kind)),
        }
    }

    fn default_fees(&self, kind: InstrumentKind) -> Result<FeeSchedule, DataError> {
        match kind {
            InstrumentKind::Spot => Ok(BINANCE_SPOT_FEES),
            InstrumentKind::Perpetual => Ok(BINANCE_USDM_FEES),
            InstrumentKind::Fx => Err(not_listed(kind)),
        }
    }

    /// From the archive: the monthly file for a whole past month (falling back to daily files if
    /// the monthly one is not published yet) and daily files for the current month, which exist up
    /// to yesterday.
    fn minute_history(
        &self,
        http: &Http,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
        now: Millis,
    ) -> Result<MinuteHistory, DataError> {
        // Refused here as well as per file, so a range that needs no file is refused too.
        archive_market(id.kind)?;
        let mut missing_files = Vec::new();
        let candles = archive_month(
            &id.symbol,
            Month::containing(from),
            (from, to),
            now,
            &mut missing_files,
            |period| fetch_archive(http, id, period),
        )?;
        Ok(MinuteHistory {
            candles,
            missing_files,
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

impl LiveStream for Binance {
    fn url(&self, id: &InstrumentId) -> String {
        let lower = id.symbol.to_lowercase();
        match id.kind {
            InstrumentKind::Spot => format!("wss://stream.binance.com:9443/ws/{lower}@kline_1m"),
            // USD-M serves market streams under `/market`; the old `/ws/<stream>` path still accepts
            // the connection but sent no frames when checked on 2026-11-10.
            InstrumentKind::Perpetual => {
                format!("wss://fstream.binance.com/market/ws/{lower}@kline_1m")
            }
            // Never connected to: a stream for a kind the source does not list refuses to open
            // (see `SharedStream::open_on`).
            InstrumentKind::Fx => String::new(),
        }
    }

    // The stream is named in the URL and the server does the pinging, so the greeting is empty.

    fn parse(&self, text: &str) -> Result<Vec<KlineUpdate>, DataError> {
        parse_stream_frame(text).map(|update| update.into_iter().collect())
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

/// The spot symbols open for trading, from the exchangeInfo at `url`. Binance is asked for those
/// symbols only and without their permission sets, which td-data does not read: 2.5 MB instead of
/// 17.8 MB before gzip. [`parse_binance_spot`] keeps only `TRADING` symbols either way, so the
/// list is the same as from the full response.
fn fetch_spot_instruments(http: &Http, url: &str) -> Result<InstrumentList, DataError> {
    let query = [
        ("symbolStatus", "TRADING".to_string()),
        ("showPermissionSets", "false".to_string()),
    ];
    parse_binance_spot(&http.get_text(url, &query)?)
}

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
pub fn archive_url(id: &InstrumentId, period: ArchivePeriod) -> Result<String, DataError> {
    let market = archive_market(id.kind)?;
    let span = match period {
        ArchivePeriod::Month(_) => "monthly",
        ArchivePeriod::Day(_) => "daily",
    };
    Ok(format!(
        "{ARCHIVE}/{market}/{span}/klines/{sym}/1m/{file}",
        sym = id.symbol,
        file = archive_file_name(&id.symbol, period)
    ))
}

/// The archive's folder for a kind.
fn archive_market(kind: InstrumentKind) -> Result<&'static str, DataError> {
    match kind {
        InstrumentKind::Spot => Ok("spot"),
        InstrumentKind::Perpetual => Ok("futures/um"),
        InstrumentKind::Fx => Err(not_listed(kind)),
    }
}

fn not_listed(kind: InstrumentKind) -> DataError {
    DataError::KindNotListed {
        venue: Venue::Binance,
        kind,
    }
}

/// Downloads one archive file, checks it against its `.CHECKSUM` and parses its klines. `None`
/// when the archive has no such file (before the symbol listed, or not yet published).
pub fn fetch_archive(
    http: &Http,
    id: &InstrumentId,
    period: ArchivePeriod,
) -> Result<Option<Vec<Candle>>, DataError> {
    let url = archive_url(id, period)?;
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

/// One month of klines from the archive, where `fetch` downloads one archive file (`None`
/// when the archive has no such file). A month before the current one comes from its monthly
/// file; when that is not published, and for the current month, the daily files for the days that
/// overlap `[lo, hi)` up to yesterday are used instead, and the names of those the archive lacks
/// are added to `missing`.
fn archive_month(
    symbol: &str,
    month: Month,
    (lo, hi): (Millis, Millis),
    now: Millis,
    missing: &mut Vec<String>,
    mut fetch: impl FnMut(ArchivePeriod) -> Result<Option<Vec<Candle>>, DataError>,
) -> Result<Vec<Candle>, DataError> {
    if month < Month::containing(now)
        && let Some(candles) = fetch(ArchivePeriod::Month(month))?
    {
        return Ok(candles);
    }
    let today = now.div_euclid(DAY) * DAY;
    let mut candles = Vec::new();
    let mut day = lo.div_euclid(DAY) * DAY;
    while day < hi && day < today {
        let period = ArchivePeriod::Day(day);
        match fetch(period)? {
            Some(rows) => candles.extend(rows),
            None => missing.push(archive_file_name(symbol, period)),
        }
        day += DAY;
    }
    Ok(candles)
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
        InstrumentKind::Fx => return Err(not_listed(id.kind)),
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
    let rates = parse_funding_stamps(body)?;
    Ok(rates
        .into_iter()
        .map(|(t, rate)| (round_to_minute(t), rate))
        .collect())
}

/// [`parse_funding`] with the times as Binance stamps them, for paging.
fn parse_funding_stamps(body: &str) -> Result<Vec<(Millis, f64)>, DataError> {
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
            Ok((row.funding_time, rate))
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
    let mut first = true;
    read_funding(from, to, |start, end| {
        // A pause between pages, not before the first.
        if !std::mem::take(&mut first) {
            std::thread::sleep(FUNDING_DELAY);
        }
        let query = [
            ("symbol", symbol.to_string()),
            ("startTime", start.to_string()),
            ("endTime", end.to_string()),
            ("limit", FUNDING_LIMIT.to_string()),
        ];
        parse_funding_stamps(&http.get_text(FUNDING_URL, &query)?)
    })
}

/// Reads the funding rates in `[from, to)` through `fetch(start, end)`, which returns the oldest
/// [`FUNDING_LIMIT`] rates stamped in `[start, end]`, oldest first, with the times as the venue
/// stamps them.
fn read_funding(
    from: Millis,
    to: Millis,
    mut fetch: impl FnMut(Millis, Millis) -> Result<Vec<(Millis, f64)>, DataError>,
) -> Result<Vec<(Millis, f64)>, DataError> {
    // Paged by the stamps themselves, so each page starts just after the last stamp read; only
    // then are the times put on the minute and kept to the range.
    let (lo, hi) = (from - ROUNDING_PAD, to + ROUNDING_PAD);
    let stamped = page_forwards(
        lo,
        hi,
        FUNDING_LIMIT,
        |&(t, _)| t,
        |start| fetch(start, hi - 1),
    )?;
    Ok(funding_on_the_minute(stamped, from, to))
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

#[derive(Debug, Deserialize)]
struct BinanceInfo {
    symbols: Vec<BinanceSymbol>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BinanceSymbol {
    symbol: String,
    status: String,
    base_asset: String,
    quote_asset: String,
    /// USD-M only: `PERPETUAL`, `CURRENT_QUARTER`, `TRADIFI_PERPETUAL`, ...
    contract_type: Option<String>,
    filters: Vec<BinanceFilter>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BinanceFilter {
    filter_type: String,
    tick_size: Option<String>,
    step_size: Option<String>,
    min_qty: Option<String>,
    /// Spot's NOTIONAL and MIN_NOTIONAL filters.
    min_notional: Option<String>,
    /// USD-M's MIN_NOTIONAL filter.
    notional: Option<String>,
}

/// Reads `/api/v3/exchangeInfo`: every spot symbol with status `TRADING`.
pub fn parse_binance_spot(body: &str) -> Result<InstrumentList, DataError> {
    parse_binance(body, InstrumentKind::Spot)
}

/// Reads `/fapi/v1/exchangeInfo`: every `PERPETUAL` contract with status `TRADING`. Quarterly
/// futures and TradFi perpetuals are left out.
pub fn parse_binance_usdm(body: &str) -> Result<InstrumentList, DataError> {
    parse_binance(body, InstrumentKind::Perpetual)
}

fn parse_binance(body: &str, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
    let info: BinanceInfo =
        serde_json::from_str(body).map_err(|e| DataError::parse("Binance exchangeInfo", e))?;
    let mut list = InstrumentList::default();
    for s in &info.symbols {
        let listed = s.status == "TRADING"
            && match kind {
                InstrumentKind::Spot => true,
                InstrumentKind::Perpetual => s.contract_type.as_deref() == Some("PERPETUAL"),
                InstrumentKind::Fx => false,
            };
        if listed {
            list.push(&s.symbol, binance_instrument(s, kind));
        }
    }
    Ok(list)
}

fn binance_instrument(s: &BinanceSymbol, kind: InstrumentKind) -> Result<Instrument, String> {
    let filter = |name: &str| s.filters.iter().find(|f| f.filter_type == name);
    let field = |name: &str, value: Option<&Option<String>>| -> Result<f64, String> {
        let text = value
            .and_then(Option::as_deref)
            .ok_or_else(|| format!("no {name} filter value"))?;
        number(name, text)
    };
    let price = filter("PRICE_FILTER");
    let lot = filter("LOT_SIZE");
    let min_notional = match kind {
        // Spot moved from MIN_NOTIONAL to NOTIONAL in 2023; read whichever is there.
        InstrumentKind::Spot => {
            let f = filter("NOTIONAL").or_else(|| filter("MIN_NOTIONAL"));
            field("NOTIONAL.minNotional", f.map(|f| &f.min_notional))?
        }
        InstrumentKind::Perpetual => field(
            "MIN_NOTIONAL.notional",
            filter("MIN_NOTIONAL").map(|f| &f.notional),
        )?,
        InstrumentKind::Fx => return Err(not_listed(kind).to_string()),
    };
    Ok(Instrument {
        id: InstrumentId {
            venue: Venue::Binance,
            symbol: s.symbol.clone(),
            kind,
        },
        base: s.base_asset.clone(),
        quote: s.quote_asset.clone(),
        tick_size: field("PRICE_FILTER.tickSize", price.map(|f| &f.tick_size))?,
        lot_size: field("LOT_SIZE.stepSize", lot.map(|f| &f.step_size))?,
        min_qty: field("LOT_SIZE.minQty", lot.map(|f| &f.min_qty))?,
        min_notional,
        fees: Binance.default_fees(kind).map_err(|e| e.to_string())?,
    })
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
pub fn parse_stream_frame(text: &str) -> Result<Option<KlineUpdate>, DataError> {
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
        open: frame_number(what, &k.o)?,
        high: frame_number(what, &k.h)?,
        low: frame_number(what, &k.l)?,
        close: frame_number(what, &k.c)?,
        volume: frame_number(what, &k.v)?,
        quote_volume: frame_number(what, &k.q)?,
    };
    Ok(Some(KlineUpdate {
        candle,
        closed: k.x,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::stub;

    /// The spot list is asked for with the filters, and a gzipped, filtered response reads the same
    /// as the full response did.
    #[test]
    fn spot_instruments_are_asked_for_filtered_and_read_the_same_as_the_full_list() {
        let full = include_str!("../tests/fixtures/binance_spot_exchange_info.json");
        // What Binance sends for the filtered request: the same document without the symbols that
        // are not trading.
        let mut filtered: serde_json::Value = serde_json::from_str(full).expect("JSON");
        let symbols = filtered["symbols"].as_array_mut().expect("symbols");
        let before = symbols.len();
        symbols.retain(|s| s["status"] == "TRADING");
        assert!(
            symbols.len() < before,
            "the fixture has a symbol that is not trading"
        );
        let (base, server) = stub::serve_once(
            &[
                ("Content-Type", "application/json"),
                ("Content-Encoding", "gzip"),
            ],
            stub::gzip(&filtered.to_string()),
        );
        let http = Http::new().expect("an HTTP client");
        let url = format!("{base}/api/v3/exchangeInfo");
        let list = fetch_spot_instruments(&http, &url).expect("lists");
        assert_eq!(list, parse_binance_spot(full).expect("parses"));
        let request = server.join().expect("the server");
        let line = request.lines().next().unwrap_or_default();
        assert_eq!(
            line, "get /api/v3/exchangeinfo?symbolstatus=trading&showpermissionsets=false http/1.1",
            "{request}"
        );
        assert!(request.contains("accept-encoding: gzip"), "{request}");
    }

    /// Reads `month` through [`archive_month`] from an archive that holds the files `has` says it
    /// holds, each with one candle at the start of its period. Returns the open times read, the
    /// files asked for and the names reported missing.
    fn month_from_archive(
        month: Month,
        range: (Millis, Millis),
        now: Millis,
        has: impl Fn(ArchivePeriod) -> bool,
    ) -> (Vec<Millis>, Vec<ArchivePeriod>, Vec<String>) {
        let (mut asked, mut missing) = (Vec::new(), Vec::new());
        let candles = archive_month("BTCUSDT", month, range, now, &mut missing, |period| {
            asked.push(period);
            let open_time = match period {
                ArchivePeriod::Month(m) => m.start(),
                ArchivePeriod::Day(day) => day,
            };
            let candle = Candle {
                open_time,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 0.0,
                quote_volume: 0.0,
            };
            Ok(has(period).then(|| vec![candle]))
        })
        .expect("reads");
        let times = candles.iter().map(|c| c.open_time).collect();
        (times, asked, missing)
    }

    #[test]
    fn a_missing_monthly_file_falls_back_to_the_daily_files() {
        let jan = Month::parse("2024-01").expect("a month");
        let now = jan.end() + 40 * DAY;
        // 2024-01-10 up to 2024-01-13, from an archive with no monthly file and no file for the 11th.
        let [d10, d11, d12] = [9, 10, 11].map(|n| jan.start() + n * DAY);
        let has = |p| !matches!(p, ArchivePeriod::Month(_)) && p != ArchivePeriod::Day(d11);
        let (read, asked, missing) = month_from_archive(jan, (d10, d12 + DAY), now, has);
        assert_eq!(
            asked,
            [
                ArchivePeriod::Month(jan),
                ArchivePeriod::Day(d10),
                ArchivePeriod::Day(d11),
                ArchivePeriod::Day(d12)
            ]
        );
        assert_eq!(read, [d10, d12]);
        assert_eq!(missing, ["BTCUSDT-1m-2024-01-11.zip"]);
    }

    #[test]
    fn a_published_monthly_file_is_used_alone_and_the_current_month_reads_up_to_yesterday() {
        let jan = Month::parse("2024-01").expect("a month");
        let (read, asked, missing) =
            month_from_archive(jan, (jan.start(), jan.end()), jan.end() + DAY, |_| true);
        assert_eq!(asked, [ArchivePeriod::Month(jan)]);
        assert_eq!(read, [jan.start()]);
        assert!(missing.is_empty());
        // At noon on 2024-01-03 the month has no monthly file yet, and today has no daily file.
        let now = jan.start() + 2 * DAY + DAY / 2;
        let (read, asked, _) = month_from_archive(jan, (jan.start(), now), now, |_| true);
        let days = [jan.start(), jan.start() + DAY];
        assert_eq!(asked, days.map(ArchivePeriod::Day));
        assert_eq!(read, days);
    }

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

    /// An oldest-first funding endpoint holding `stamps`, oldest first: it answers with the oldest
    /// [`FUNDING_LIMIT`] rates stamped in the range asked for, with the times as stamped.
    fn venue(
        stamps: &[(Millis, f64)],
    ) -> impl FnMut(Millis, Millis) -> Result<Vec<(Millis, f64)>, DataError> + '_ {
        move |start, end| {
            let page = stamps.iter().copied();
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
