//! Dukascopy as a [`DataSource`]: FX spot pairs from a fixed table, and their 1-minute bid and ask
//! candles built from Dukascopy's tick files, read from its requester-pays S3 bucket. No funding
//! and no live stream.
//!
//! The format is built to Dukascopy's "Data export" page,
//! <https://wiki.dukascopy.com/en/development/data-export/>, read on 2026-11-11, and has not been
//! checked against a real file: the constants, [`day_key`] and [`decode_ticks`] are where to
//! correct it. What the page says:
//!
//! - the bucket is `cfg-public-proper-wallaby` in eu-west-1, with Requester Pays, so a request is
//!   signed with AWS credentials and its transfer is billed to them;
//! - one file per pair and UTC day at `SYMBOL/YEAR/MONTH/DAY_ticks.bi5`, the month counted from
//!   `00` (January) to `11` (December); a missing file means no ticks that day;
//! - a file is a raw LZMA stream; decompressed, it is 20-byte big-endian records: the time in
//!   milliseconds since the start of the UTC day (u32), the ask and the bid (u32, the price times
//!   1,000 for a JPY-quoted pair and times 100,000 for the rest), then the ask and bid volumes
//!   (f32, in millions of units of the base currency).
//!
//! What it leaves open, and the reading taken: the day is two digits, zero-padded (its example is
//! `15`, and its own script matches `(\d{2})_ticks`); "raw LZMA" is the `.lzma` format with its
//! 13-byte header, which its Python example's `lzma.decompress` reads; it names no earliest date.
//! It warns that older files may count milliseconds from the start of the hour, without naming
//! them: [`decode_ticks`] refuses a file whose times go backwards, which such a file would do at
//! each hour.

use std::io::Cursor;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, Venue, civil_from_days,
};

use crate::http::Http;
use crate::sigv4::{self, AwsCredentials, EMPTY_SHA256};
use crate::source::{DataSource, MinuteHistory};
use crate::time::{DAY, MINUTE, Month, now_ms};
use crate::{DataError, InstrumentList};

/// The requester-pays bucket and its region, from the "Data export" page.
const BUCKET: &str = "cfg-public-proper-wallaby";
const REGION: &str = "eu-west-1";
/// The bytes in one decompressed tick record.
const RECORD: usize = 20;
/// The file's volumes are in millions of units of the base currency.
const VOLUME_UNIT: f64 = 1_000_000.0;
/// The pauses before each retry of a file after a failed connection or a 5xx: five tries over
/// about 40 seconds.
const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
];

/// The pairs served, as the bucket names them: the seven majors and the main crosses. Each is its
/// base currency's three letters then its quote currency's.
///
/// shortcut: a fixed table, add a row for a new pair; replace with the bucket's own list if one
/// is needed.
const PAIRS: [&str; 20] = [
    "EURUSD", "USDJPY", "GBPUSD", "USDCHF", "AUDUSD", "USDCAD", "NZDUSD", "EURJPY", "GBPJPY",
    "AUDJPY", "CADJPY", "CHFJPY", "NZDJPY", "EURGBP", "EURCHF", "EURAUD", "EURCAD", "GBPCHF",
    "GBPAUD", "AUDNZD",
];

/// FX at Dukascopy is charged a commission per unit rather than a fraction of notional, so the
/// fractional fees are 0. No default per-unit commission is set: a run sets its own.
const FX_FEES: FeeSchedule = FeeSchedule {
    maker: 0.0,
    taker: 0.0,
    per_unit: 0.0,
};

/// Dukascopy's historical tick data: FX pairs only.
#[derive(Debug, Clone, Copy, Default)]
pub struct Dukascopy;

impl DataSource for Dukascopy {
    fn venue(&self) -> Venue {
        Venue::Dukascopy
    }

    fn kinds(&self) -> &'static [InstrumentKind] {
        &[InstrumentKind::Fx]
    }

    /// The fixed table: no request is made.
    fn instruments(&self, _: &Http, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
        fx_only(kind)?;
        let mut list = InstrumentList::default();
        for symbol in PAIRS {
            list.push(symbol, Ok(instrument(symbol)));
        }
        Ok(list)
    }

    fn default_fees(&self, kind: InstrumentKind) -> Result<FeeSchedule, DataError> {
        fx_only(kind)?;
        Ok(FX_FEES)
    }

    /// [`history`] from the bucket, with the AWS credentials read from the environment.
    fn minute_history(
        &self,
        http: &Http,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
        now: Millis,
    ) -> Result<MinuteHistory, DataError> {
        let endpoint = format!("https://{BUCKET}.s3.{REGION}.amazonaws.com");
        history(
            id,
            from,
            to,
            now,
            |name| std::env::var(name).ok(),
            &RETRY_DELAYS,
            |credentials, key| get_object(http, credentials, &endpoint, key),
        )
        .map(|(history, _)| history)
    }
}

/// [`Dukascopy::instruments`]' row for `symbol`: base and quote from its letters, a tick of 0.001
/// for a JPY-quoted pair and 0.00001 for the rest, and quantities in whole units of the base
/// currency from 1,000 (a micro lot). The minimum order value is 1 unit of the quote currency, far
/// below what the minimum quantity is worth, so the minimum quantity is the rule that binds.
fn instrument(symbol: &str) -> Instrument {
    let (base, quote) = symbol.split_at(3);
    Instrument {
        id: InstrumentId {
            venue: Venue::Dukascopy,
            symbol: symbol.to_string(),
            kind: InstrumentKind::Fx,
        },
        base: base.to_string(),
        quote: quote.to_string(),
        tick_size: 1.0 / scale(quote),
        lot_size: 1.0,
        min_qty: 1_000.0,
        min_notional: 1.0,
        fees: FX_FEES,
    }
}

/// What the file's integer prices are divided by for a pair quoted in `quote`.
fn scale(quote: &str) -> f64 {
    if quote == "JPY" { 1_000.0 } else { 100_000.0 }
}

fn fx_only(kind: InstrumentKind) -> Result<(), DataError> {
    match kind {
        InstrumentKind::Fx => Ok(()),
        InstrumentKind::Spot | InstrumentKind::Perpetual => Err(DataError::KindNotListed {
            venue: Venue::Dukascopy,
            kind,
        }),
    }
}

/// What [`history`] left out or added, reported on standard error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Ticks whose ask was below their bid.
    pub crossed_ticks: usize,
    /// Minutes with ticks in the kind's closed hours.
    pub closed_minutes: usize,
    /// Open minutes without a tick, filled with a flat candle at the last price.
    pub filled_minutes: usize,
}

/// [`Dukascopy::minute_history`]: the minute candles of each UTC day before today that overlaps
/// `[from, to)`, whole, built from that day's tick file ([`minute_candles`], [`add_day`]).
///
/// The credentials are read through `env` before any request, and the call fails without them.
/// `get` sends one signed GET for an object key and answers its status and body; a failed
/// connection or a 5xx is tried again after each of `delays`. A day whose file is not there
/// (a 404) is named in [`MinuteHistory::missing_files`] and has no minutes; a 403 and any other
/// failure fail the whole call, so a month never comes back short of a day the bucket failed to
/// serve.
pub(crate) fn history(
    id: &InstrumentId,
    from: Millis,
    to: Millis,
    now: Millis,
    env: impl Fn(&str) -> Option<String>,
    delays: &[Duration],
    mut get: impl FnMut(&AwsCredentials, &str) -> Result<(u16, Vec<u8>), DataError>,
) -> Result<(MinuteHistory, Counts), DataError> {
    fx_only(id.kind)?;
    if !PAIRS.contains(&id.symbol.as_str()) {
        return Err(DataError::InvalidInstrument {
            symbol: id.symbol.clone(),
            reason: "not one of the FX pairs read from dukascopy".into(),
        });
    }
    let credentials = AwsCredentials::from_lookup(env)?;
    let scale = scale(&id.symbol[3..]);
    let mut history = MinuteHistory::default();
    let mut counts = Counts::default();
    // The bid and ask closes of the last minute added, which a quiet minute is filled at.
    let mut last = None;
    let today = now.div_euclid(DAY) * DAY;
    let mut day = from.div_euclid(DAY) * DAY;
    while day < to && day < today {
        let key = day_key(&id.symbol, day);
        let url = format!("s3://{BUCKET}/{key}");
        let file = with_retries(delays, || {
            let (status, body) = get(&credentials, &key)?;
            answer(&url, status, body)
        })?;
        match file {
            Some(bi5) => {
                let raw = decompress(&key, &bi5)?;
                let (ticks, crossed) = decode_ticks(&key, &raw, day, scale)?;
                counts.crossed_ticks += crossed;
                let quotes = minute_candles(&ticks);
                add_day(id.kind, day, quotes, &mut last, &mut history, &mut counts);
            }
            None => {
                history.missing_files.push(key);
                last = None;
            }
        }
        day += DAY;
    }
    if counts != Counts::default() {
        eprintln!(
            "td-data: {id} {}: left out {} ticks with the ask below the bid and {} minutes in the \
             closed hours; filled {} open minutes without a tick at the last price",
            Month::containing(from),
            counts.crossed_ticks,
            counts.closed_minutes,
            counts.filled_minutes
        );
    }
    Ok((history, counts))
}

/// Reads S3's answer for the object at `url`: its body, `None` when there is no such object (a
/// 404, or S3's `NoSuchKey`), or an error. S3's error bodies can echo the access key and the
/// signature, so none is kept: only the error code is.
fn answer(url: &str, status: u16, body: Vec<u8>) -> Result<Option<Vec<u8>>, DataError> {
    let code = s3_code(&body);
    match status {
        200..=299 => Ok(Some(body)),
        404 => Ok(None),
        _ if code == Some("NoSuchKey") => Ok(None),
        403 => Err(DataError::AwsRefused {
            url: url.to_string(),
            code: code.unwrap_or("with no code").to_string(),
        }),
        _ => Err(DataError::Status {
            url: url.to_string(),
            status,
            body: code.unwrap_or_default().to_string(),
        }),
    }
}

/// The `<Code>` of an S3 error body, when it is a plain word.
fn s3_code(body: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(body).ok()?;
    let (_, rest) = text.split_once("<Code>")?;
    let (code, _) = rest.split_once("</Code>")?;
    let word =
        !code.is_empty() && code.len() <= 64 && code.bytes().all(|b| b.is_ascii_alphanumeric());
    word.then_some(code)
}

/// One signed GET of the object `key` at `endpoint` (`https://<bucket>.s3.<region>.amazonaws.com`
/// in use), as the requester who pays: its status and body. The signature and the session token
/// go in headers marked sensitive, and never in the URL.
fn get_object(
    http: &Http,
    credentials: &AwsCredentials,
    endpoint: &str,
    key: &str,
) -> Result<(u16, Vec<u8>), DataError> {
    // A key is a pair from the table, digits, '/', '_' and '.', so it needs no URI encoding.
    let path = format!("/{key}");
    let host = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, host)| host);
    let date = sigv4::amz_date(now_ms());
    let mut signed: Vec<(&'static str, &str)> = vec![
        ("host", host),
        ("x-amz-content-sha256", EMPTY_SHA256),
        ("x-amz-date", &date),
        ("x-amz-request-payer", "requester"),
    ];
    if let Some(token) = credentials.session_token() {
        signed.push(("x-amz-security-token", token));
    }
    let authorization = sigv4::authorization(credentials, REGION, &path, &signed, &date);
    let mut headers = HeaderMap::new();
    // The client sends the host from the URL.
    for (name, value) in signed[1..]
        .iter()
        .copied()
        .chain([("authorization", authorization.as_str())])
    {
        let mut value = HeaderValue::from_str(value)
            .map_err(|_| DataError::parse(format!("the {name} header"), "not a header value"))?;
        value.set_sensitive(matches!(name, "authorization" | "x-amz-security-token"));
        headers.insert(HeaderName::from_static(name), value);
    }
    http.get_with_headers(&format!("{endpoint}{path}"), headers)
}

/// Calls `get` until it gives anything but a passing failure (a failed connection or a 5xx),
/// sleeping each of `delays` in turn between tries, and returns the last answer.
fn with_retries<T>(
    delays: &[Duration],
    mut get: impl FnMut() -> Result<T, DataError>,
) -> Result<T, DataError> {
    for &delay in delays {
        match get() {
            Err(e) if passing(&e) => std::thread::sleep(delay),
            answer => return answer,
        }
    }
    get()
}

fn passing(e: &DataError) -> bool {
    match e {
        DataError::Http { .. } => true,
        DataError::Status { status, .. } => *status >= 500,
        _ => false,
    }
}

/// The bucket's key for the tick file of `symbol` on the UTC day that `day` falls in:
/// `SYMBOL/YEAR/MONTH/DAY_ticks.bi5`, the month counted from `00`, the day two digits.
pub(crate) fn day_key(symbol: &str, day: Millis) -> String {
    let (year, month, date) = civil_from_days(day.div_euclid(DAY));
    format!("{symbol}/{year:04}/{:02}/{date:02}_ticks.bi5", month - 1)
}

/// One tick: its time and both sides, prices divided down and volumes in units of the base
/// currency.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Tick {
    pub time: Millis,
    pub ask: f64,
    pub bid: f64,
    pub ask_volume: f64,
    pub bid_volume: f64,
}

/// A `.bi5` file decompressed, `file` naming it in errors. An empty file is a day without ticks.
fn decompress(file: &str, bi5: &[u8]) -> Result<Vec<u8>, DataError> {
    let mut raw = Vec::new();
    if !bi5.is_empty() {
        lzma_rs::lzma_decompress(&mut Cursor::new(bi5), &mut raw).map_err(|e| {
            DataError::Archive {
                file: file.to_string(),
                reason: format!("does not decompress as LZMA: {e}"),
            }
        })?;
    }
    Ok(raw)
}

/// The ticks of the UTC day starting at `day` from its decompressed file, `file` naming it in
/// errors, and how many were dropped because their ask was below their bid. Each record is the
/// time in milliseconds since `day`, the ask, the bid (u32 each, the prices times `scale`), the
/// ask volume and the bid volume (f32 each, in millions), all big-endian. A file that is not a
/// whole number of records, holds a time outside the day, or goes back in time is refused.
pub(crate) fn decode_ticks(
    file: &str,
    raw: &[u8],
    day: Millis,
    scale: f64,
) -> Result<(Vec<Tick>, usize), DataError> {
    let bad = |reason: String| DataError::Archive {
        file: file.to_string(),
        reason,
    };
    let (records, rest) = raw.as_chunks::<RECORD>();
    if !rest.is_empty() {
        return Err(bad(format!(
            "decompresses to {} bytes, which is not a whole number of {RECORD}-byte records",
            raw.len()
        )));
    }
    let mut ticks = Vec::with_capacity(records.len());
    let mut crossed = 0;
    let mut previous = 0;
    for record in records {
        let word = |i: usize| u32::from_be_bytes(record[i..i + 4].try_into().expect("4 bytes"));
        let float = |i: usize| f32::from_be_bytes(record[i..i + 4].try_into().expect("4 bytes"));
        let offset = Millis::from(word(0));
        if offset >= DAY {
            return Err(bad(format!(
                "a tick at {offset} ms is past the end of the day"
            )));
        }
        if offset < previous {
            return Err(bad(format!(
                "the tick at {offset} ms is before the one at {previous} ms (do its times count \
                 from the start of each hour?)"
            )));
        }
        previous = offset;
        let tick = Tick {
            time: day + offset,
            ask: f64::from(word(4)) / scale,
            bid: f64::from(word(8)) / scale,
            ask_volume: f64::from(float(12)) * VOLUME_UNIT,
            bid_volume: f64::from(float(16)) * VOLUME_UNIT,
        };
        if tick.ask < tick.bid {
            crossed += 1;
        } else {
            ticks.push(tick);
        }
    }
    Ok((ticks, crossed))
}

/// The bid and ask candles of each minute that has a tick, from `ticks` in time order, oldest
/// first. Each side's open, high, low and close are its prices in time order; its `volume` is the
/// sum of its tick volumes, in units of the base currency, and its `quote_volume` the sum of each
/// volume times its price. Dukascopy's volumes are the size quoted at the price, not traded
/// volume. As each tick's ask is at or above its bid, so is each of the ask candle's prices.
pub(crate) fn minute_candles(ticks: &[Tick]) -> Vec<(Candle, Candle)> {
    let open = |open_time, price: f64, volume: f64| Candle {
        open_time,
        open: price,
        high: price,
        low: price,
        close: price,
        volume,
        quote_volume: volume * price,
    };
    let extend = |c: &mut Candle, price: f64, volume: f64| {
        c.high = c.high.max(price);
        c.low = c.low.min(price);
        c.close = price;
        c.volume += volume;
        c.quote_volume += volume * price;
    };
    let mut quotes: Vec<(Candle, Candle)> = Vec::new();
    for t in ticks {
        let minute = t.time - t.time.rem_euclid(MINUTE);
        match quotes.last_mut() {
            Some((bid, ask)) if bid.open_time == minute => {
                extend(bid, t.bid, t.bid_volume);
                extend(ask, t.ask, t.ask_volume);
            }
            _ => quotes.push((
                open(minute, t.bid, t.bid_volume),
                open(minute, t.ask, t.ask_volume),
            )),
        }
    }
    quotes
}

/// Adds one day's minute `quotes` (bid and ask, oldest first, all inside the day starting at
/// `day`) to `history`, as [`crate::Store::write_quotes`] takes them:
///
/// - a minute in `kind`'s closed hours is left out and counted, and does not move `last`;
/// - an open minute with no tick is filled with a flat candle on each side at that side's last
///   close (`last`), with no volume, and counted; before the first tick there is nothing to fill
///   at, so the minute stays missing.
///
/// shortcut: a month's first minutes before its first tick stay missing, because each call starts
/// with nothing to fill at; carry the last price in from the month before if those gaps matter.
///
/// shortcut: a long quiet stretch in open hours is filled at a stale price (the raw feed's candle
/// files did the same); cap the fill if a holiday's empty file ever shows up as a flat day.
fn add_day(
    kind: InstrumentKind,
    day: Millis,
    quotes: Vec<(Candle, Candle)>,
    last: &mut Option<(f64, f64)>,
    history: &mut MinuteHistory,
    counts: &mut Counts,
) {
    let flat = |open_time, price| Candle {
        open_time,
        open: price,
        high: price,
        low: price,
        close: price,
        volume: 0.0,
        quote_volume: 0.0,
    };
    let mut quotes = quotes.into_iter().peekable();
    for t in (0..DAY / MINUTE).map(|m| day + m * MINUTE) {
        let quote = quotes.next_if(|(bid, _)| bid.open_time == t);
        if !kind.is_open(t) {
            counts.closed_minutes += usize::from(quote.is_some());
            continue;
        }
        let (bid, ask) = match (quote, *last) {
            (Some(quote), _) => quote,
            (None, Some((bid, ask))) => {
                counts.filled_minutes += 1;
                (flat(t, bid), flat(t, ask))
            }
            (None, None) => continue,
        };
        *last = Some((bid.close, ask.close));
        history.candles.push(bid);
        history.asks.push(ask);
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// One record as the file stores it: milliseconds into the day, the ask and the bid in points,
    /// the ask and bid volumes in millions.
    pub(crate) type Record = (u32, u32, u32, f32, f32);

    /// `records` as a decompressed file.
    pub(crate) fn raw(records: &[Record]) -> Vec<u8> {
        let mut raw = Vec::with_capacity(records.len() * RECORD);
        for &(ms, ask, bid, ask_volume, bid_volume) in records {
            raw.extend(ms.to_be_bytes());
            raw.extend(ask.to_be_bytes());
            raw.extend(bid.to_be_bytes());
            raw.extend(ask_volume.to_be_bytes());
            raw.extend(bid_volume.to_be_bytes());
        }
        raw
    }

    /// `records` as a `.bi5` file: encoded and LZMA-compressed.
    pub(crate) fn bi5(records: &[Record]) -> Vec<u8> {
        let mut file = Vec::new();
        lzma_rs::lzma_compress(&mut Cursor::new(raw(records)), &mut file).expect("compresses");
        file
    }

    /// One USDJPY day of ticks: two in each open minute of the day from minute `first` on, except
    /// the minutes in `quiet`, at 1 s and at 30 s into the minute. The bid moves with the minute
    /// in thousandths and the ask is 7 points above it.
    pub(crate) fn tick_day(day: Millis, first: u32, quiet: &[u32]) -> Vec<Record> {
        let mut records = Vec::new();
        for m in first..1440 {
            let t = day + Millis::from(m) * MINUTE;
            if !InstrumentKind::Fx.is_open(t) || quiet.contains(&m) {
                continue;
            }
            let i = u32::try_from(t / MINUTE).expect("fits");
            let bids = [150_000 + i % 37 * 10, 150_000 + (i + 11) % 37 * 10];
            for (k, (bid, offset)) in bids.into_iter().zip([1_000, 30_000]).enumerate() {
                let volume = if k == 0 { 1.5 } else { 0.25 };
                records.push((m * 60_000 + offset, bid + 7, bid, volume, volume * 2.0));
            }
        }
        records
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{bi5, raw};
    use super::*;
    use crate::time::parse_date;

    const HOUR: Millis = 60 * MINUTE;

    #[test]
    fn day_keys_count_months_from_zero_and_pad_the_day() {
        let jan = parse_date("2024-01-05").expect("a date");
        assert_eq!(day_key("USDJPY", jan), "USDJPY/2024/00/05_ticks.bi5");
        // Any time in the day names the same file.
        let dec = parse_date("2025-12-25").expect("a date") + DAY - MINUTE;
        assert_eq!(day_key("EURUSD", dec), "EURUSD/2025/11/25_ticks.bi5");
        let first = parse_date("2003-10-01").expect("a date");
        assert_eq!(day_key("GBPJPY", first), "GBPJPY/2003/09/01_ticks.bi5");
    }

    fn tick(time: Millis, ask: f64, bid: f64, ask_volume: f64, bid_volume: f64) -> Tick {
        Tick {
            time,
            ask,
            bid,
            ask_volume,
            bid_volume,
        }
    }

    #[test]
    fn a_jpy_quoted_file_reads_in_thousandths_ask_before_bid_from_the_start_of_the_day() {
        let day = parse_date("2024-01-08").expect("a date");
        let file = raw(&[
            (0, 150_130, 150_123, 1.5, 2.25),
            (61_500, 150_131, 150_118, 0.75, 0.5),
            (86_399_999, 151_004, 151_002, 3.0, 1.0),
        ]);
        let (ticks, crossed) = decode_ticks("f", &file, day, 1_000.0).expect("decodes");
        assert_eq!(crossed, 0);
        assert_eq!(
            ticks,
            [
                tick(day, 150.13, 150.123, 1_500_000.0, 2_250_000.0),
                tick(day + 61_500, 150.131, 150.118, 750_000.0, 500_000.0),
                tick(day + DAY - 1, 151.004, 151.002, 3_000_000.0, 1_000_000.0),
            ]
        );
    }

    #[test]
    fn a_non_jpy_file_reads_in_hundred_thousandths() {
        let day = parse_date("2016-03-09").expect("a date");
        let file = raw(&[(12 * 3_600_000, 109_874, 109_871, 1.0, 2.0)]);
        let (ticks, _) = decode_ticks("f", &file, day, 100_000.0).expect("decodes");
        assert_eq!(ticks, [tick(day + 12 * HOUR, 1.09874, 1.09871, 1e6, 2e6)]);
    }

    #[test]
    fn an_empty_file_is_a_day_without_ticks() {
        assert_eq!(
            decompress("f", &[]).expect("decompresses"),
            Vec::<u8>::new()
        );
        let raw = decompress("f", &bi5(&[])).expect("decompresses");
        assert_eq!(
            decode_ticks("f", &raw, 0, 1_000.0).expect("decodes"),
            (vec![], 0)
        );
    }

    #[test]
    fn a_compressed_file_decompresses_to_its_records() {
        let records = [(5, 150_130, 150_123, 1.5, 2.25)];
        assert_eq!(
            decompress("f", &bi5(&records)).expect("decompresses"),
            raw(&records)
        );
    }

    #[test]
    fn a_crossed_tick_is_dropped_and_counted() {
        let file = raw(&[
            (0, 150_130, 150_123, 1.0, 1.0),
            (10, 150_120, 150_123, 1.0, 1.0),
            (20, 150_123, 150_123, 1.0, 1.0),
        ]);
        let (ticks, crossed) = decode_ticks("f", &file, 0, 1_000.0).expect("decodes");
        assert_eq!(crossed, 1);
        assert_eq!(ticks.iter().map(|t| t.time).collect::<Vec<_>>(), [0, 20]);
    }

    #[test]
    fn a_file_that_is_truncated_or_not_lzma_or_out_of_order_is_refused() {
        let reason = |result: Result<_, DataError>| match result {
            Err(DataError::Archive { file, reason }) => {
                assert_eq!(file, "USDJPY/x.bi5");
                reason
            }
            other => panic!("expected an archive error, got {other:?}"),
        };
        let decode = |raw: &[u8]| decode_ticks("USDJPY/x.bi5", raw, 0, 1_000.0).map(|_| ());
        let two = raw(&[(0, 2, 1, 0.0, 0.0), (1, 2, 1, 0.0, 0.0)]);
        assert!(reason(decode(&two[..39])).contains("39 bytes"));
        assert!(
            reason(decompress("USDJPY/x.bi5", b"<Error>503</Error>").map(|_| ())).contains("LZMA")
        );
        let late = raw(&[(86_400_000, 2, 1, 0.0, 0.0)]);
        assert!(reason(decode(&late)).contains("past the end of the day"));
        // Times that start again, as an hourly time base would at each hour.
        let hourly = raw(&[(3_599_000, 2, 1, 0.0, 0.0), (1_000, 2, 1, 0.0, 0.0)]);
        assert!(reason(decode(&hourly)).contains("start of each hour"));
    }

    /// Thursday 2024-01-11. That week is in New York standard time, so the FX weekend runs from
    /// Friday 22:00 UTC to Sunday 22:00 UTC.
    fn thursday() -> Millis {
        parse_date("2024-01-11").expect("a date")
    }

    fn add(day: Millis, ticks: &[Tick], last: &mut Option<(f64, f64)>) -> (MinuteHistory, Counts) {
        let mut history = MinuteHistory::default();
        let mut counts = Counts::default();
        let quotes = minute_candles(ticks);
        add_day(
            InstrumentKind::Fx,
            day,
            quotes,
            last,
            &mut history,
            &mut counts,
        );
        (history, counts)
    }

    fn side(
        open_time: Millis,
        [open, high, low, close]: [f64; 4],
        volume: f64,
        quote: f64,
    ) -> Candle {
        Candle {
            open_time,
            open,
            high,
            low,
            close,
            volume,
            quote_volume: quote,
        }
    }

    #[test]
    fn a_minute_of_ticks_is_one_candle_a_side_in_time_order() {
        let t = thursday() + 9 * HOUR;
        let ticks = [
            tick(t + 100, 150.105, 150.100, 1.0, 2.0),
            tick(t + 20_000, 150.125, 150.120, 3.0, 4.0),
            tick(t + 40_000, 150.093, 150.090, 1.0, 1.0),
            tick(t + 59_999, 150.110, 150.104, 2.0, 1.0),
            // The next minute.
            tick(t + MINUTE, 150.2, 150.19, 1.0, 1.0),
        ];
        let quotes = minute_candles(&ticks);
        assert_eq!(quotes.len(), 2);
        let (bid, ask) = quotes[0];
        let bid_quote = 150.100 * 2.0 + 150.120 * 4.0 + 150.090 + 150.104;
        assert_eq!(
            bid,
            side(t, [150.100, 150.120, 150.090, 150.104], 8.0, bid_quote)
        );
        let ask_quote = 150.105 + 150.125 * 3.0 + 150.093 + 150.110 * 2.0;
        assert_eq!(
            ask,
            side(t, [150.105, 150.125, 150.093, 150.110], 7.0, ask_quote)
        );
        assert_eq!(quotes[1].0, side(t + MINUTE, [150.19; 4], 1.0, 150.19));
    }

    #[test]
    fn a_quiet_minute_between_two_is_filled_flat_at_each_sides_last_close() {
        let t = thursday() + 9 * HOUR;
        let ticks = [
            tick(t + 1_000, 150.105, 150.100, 1.0, 1.0),
            tick(t + 2_000, 150.112, 150.108, 1.0, 1.0),
            tick(t + 2 * MINUTE + 5, 150.2, 150.19, 1.0, 1.0),
        ];
        let mut last = None;
        let (history, counts) = add(thursday(), &ticks, &mut last);
        let at =
            |c: &[Candle], t: Millis| *c.iter().find(|c| c.open_time == t).expect("the minute");
        assert_eq!(
            at(&history.candles, t + MINUTE),
            side(t + MINUTE, [150.108; 4], 0.0, 0.0)
        );
        assert_eq!(
            at(&history.asks, t + MINUTE),
            side(t + MINUTE, [150.112; 4], 0.0, 0.0)
        );
        // Nothing before the first tick; every open minute after it, to the end of the day.
        assert_eq!(history.candles[0].open_time, t);
        let after = (DAY - 9 * HOUR) / MINUTE;
        assert_eq!(history.candles.len() as i64, after);
        assert_eq!(history.asks.len(), history.candles.len());
        assert_eq!(counts.filled_minutes as i64, after - 2);
        assert_eq!(counts.closed_minutes, 0);
        // The rest of the day is filled at the last minute's closes, which carry to the next day.
        assert_eq!(history.candles.last().map(|c| c.close), Some(150.19));
        assert_eq!(last, Some((150.19, 150.2)));
    }

    #[test]
    fn closed_hours_are_never_filled_and_their_ticks_are_left_out() {
        let friday = thursday() + DAY;
        let (saturday, sunday) = (friday + DAY, friday + 2 * DAY);
        let mut last = Some((150.0, 150.01));
        // Friday: a tick at 21:00, then one after the close at 22:30.
        let (fri, counts) = add(
            friday,
            &[
                tick(friday + 21 * HOUR, 150.11, 150.1, 1.0, 1.0),
                tick(friday + 22 * HOUR + 30 * MINUTE, 150.21, 150.2, 1.0, 1.0),
            ],
            &mut last,
        );
        assert_eq!(
            fri.candles.len(),
            22 * 60,
            "every open minute, none after 22:00"
        );
        assert_eq!(
            fri.candles.last().map(|c| c.open_time),
            Some(friday + 22 * HOUR - MINUTE)
        );
        assert_eq!(counts.closed_minutes, 1);
        assert_eq!(counts.filled_minutes, 22 * 60 - 1);
        // The tick after the close does not move the price a fill is at.
        assert_eq!(last, Some((150.1, 150.11)));

        // Saturday: an empty file, and one tick, all closed.
        let (sat, counts) = add(saturday, &[], &mut last);
        assert!(sat.candles.is_empty());
        assert_eq!(counts, Counts::default());
        let (sat, counts) = add(
            saturday,
            &[tick(saturday + HOUR, 150.3, 150.2, 1.0, 1.0)],
            &mut last,
        );
        assert!(sat.candles.is_empty());
        assert_eq!(counts.closed_minutes, 1);

        // Sunday: nothing before the 22:00 open; from the open, the last price carries until the
        // first tick at 22:05.
        let (sun, counts) = add(
            sunday,
            &[
                tick(sunday + 21 * HOUR, 150.3, 150.2, 1.0, 1.0),
                tick(sunday + 22 * HOUR + 5 * MINUTE, 150.31, 150.3, 1.0, 1.0),
            ],
            &mut last,
        );
        assert_eq!(sun.candles.len(), 120);
        assert_eq!(
            sun.candles[0],
            side(sunday + 22 * HOUR, [150.1; 4], 0.0, 0.0)
        );
        assert_eq!(counts.closed_minutes, 1);
        assert_eq!(counts.filled_minutes, 120 - 1);
    }

    #[test]
    fn nothing_is_filled_before_the_first_tick_of_a_call() {
        let t = thursday() + 10 * HOUR;
        let mut last = None;
        let (history, counts) = add(thursday(), &[tick(t, 150.11, 150.1, 1.0, 1.0)], &mut last);
        assert_eq!(history.candles[0].open_time, t);
        assert_eq!(counts.filled_minutes as i64, (DAY - 10 * HOUR) / MINUTE - 1);
    }
}

/// The source on a fake bucket: [`history`] reading files from memory, the requests it signs, and
/// the download path into a temp store.
#[cfg(test)]
mod source_tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::fixtures::{Record, bi5, tick_day};
    use super::*;
    use crate::Store;
    use crate::download::download_candles_from;
    use crate::http::stub;
    use crate::sigv4::{EXAMPLE_KEY_ID, EXAMPLE_SECRET};
    use crate::time::parse_date;

    const HOUR: Millis = 60 * MINUTE;
    const NO_WAIT: [Duration; 4] = [Duration::ZERO; 4];

    /// What the fake bucket answers for one key: a status and a body.
    type Files = HashMap<String, (u16, Vec<u8>)>;

    fn usdjpy() -> InstrumentId {
        InstrumentId {
            venue: Venue::Dukascopy,
            symbol: "USDJPY".into(),
            kind: InstrumentKind::Fx,
        }
    }

    /// The process environment as the credentials' lookup sees it, with AWS's example keys.
    fn env(name: &str) -> Option<String> {
        match name {
            "AWS_ACCESS_KEY_ID" => Some(EXAMPLE_KEY_ID.into()),
            "AWS_SECRET_ACCESS_KEY" => Some(EXAMPLE_SECRET.into()),
            _ => None,
        }
    }

    /// Thursday 2024-01-11. That week is in New York standard time, so the FX weekend runs from
    /// Friday 22:00 UTC to Sunday 22:00 UTC.
    fn thursday() -> Millis {
        parse_date("2024-01-11").expect("a date")
    }

    fn day(d: i64) -> Millis {
        thursday() + d * DAY
    }

    /// Thursday to Sunday as the bucket would serve them, with no Saturday file. Thursday has two
    /// quiet minutes (10:00 and 10:01) and a crossed tick at 11:00; Friday a tick after the close
    /// at 23:00; Sunday's ticks start at minute `sunday_first`.
    fn bucket(sunday_first: u32) -> Files {
        let mut thu = tick_day(day(0), 0, &[600, 601]);
        thu.push((660 * 60_000 + 15_000, 150_000, 150_010, 1.0, 1.0));
        thu.sort_by_key(|r| r.0);
        let mut fri = tick_day(day(1), 0, &[]);
        fri.push((23 * 3_600_000, 150_017, 150_010, 1.0, 1.0));
        let sun = tick_day(day(3), sunday_first, &[]);
        let ok = |records: &[Record]| (200, bi5(records));
        HashMap::from([
            (day_key("USDJPY", day(0)), ok(&thu)),
            (day_key("USDJPY", day(1)), ok(&fri)),
            (day_key("USDJPY", day(3)), ok(&sun)),
        ])
    }

    /// Serves `files`, noting each key asked for in `asked`; a key not in `files` answers S3's
    /// 404.
    fn serve<'a>(
        files: &'a Files,
        asked: &'a RefCell<Vec<String>>,
    ) -> impl FnMut(&AwsCredentials, &str) -> Result<(u16, Vec<u8>), DataError> + 'a {
        move |_, key| {
            asked.borrow_mut().push(key.to_string());
            Ok(files
                .get(key)
                .cloned()
                .unwrap_or_else(|| (404, b"<Error><Code>NoSuchKey</Code></Error>".to_vec())))
        }
    }

    fn far_future() -> Millis {
        parse_date("2030-01-01").expect("a date")
    }

    fn read(
        files: &Files,
        asked: &RefCell<Vec<String>>,
    ) -> Result<(MinuteHistory, Counts), DataError> {
        history(
            &usdjpy(),
            day(0),
            day(4),
            far_future(),
            env,
            &NO_WAIT,
            serve(files, asked),
        )
    }

    /// The open minutes from Thursday to Sunday that [`bucket`] leaves covered.
    fn covered(sunday_first: u32) -> Vec<Millis> {
        (0..4 * DAY / MINUTE)
            .map(|i| day(0) + i * MINUTE)
            .filter(|&t| InstrumentKind::Fx.is_open(t))
            .filter(|&t| t < day(3) || t >= day(3) + Millis::from(sunday_first) * MINUTE)
            .collect()
    }

    #[test]
    fn bids_and_asks_cover_the_same_open_minutes_and_a_missing_day_resets_the_fill() {
        let files = bucket(22 * 60 + 3);
        let asked = RefCell::new(Vec::new());
        let (history, counts) = read(&files, &asked).expect("reads");

        let bid_times: Vec<Millis> = history.candles.iter().map(|c| c.open_time).collect();
        let ask_times: Vec<Millis> = history.asks.iter().map(|c| c.open_time).collect();
        assert_eq!(bid_times, covered(22 * 60 + 3));
        assert_eq!(ask_times, bid_times);
        // Thursday whole, Friday to 22:00, nothing on Saturday, Sunday from its first tick at
        // 22:03: after the missing Saturday nothing is filled before it.
        assert_eq!(bid_times.len(), 1440 + 1320 + 117);
        assert_eq!(
            counts,
            Counts {
                crossed_ticks: 1,
                closed_minutes: 1,
                filled_minutes: 2,
            }
        );
        assert_eq!(history.missing_files, [day_key("USDJPY", day(2))]);
        // One key a day, in order.
        let keys: Vec<String> = (0..4).map(|d| day_key("USDJPY", day(d))).collect();
        assert_eq!(*asked.borrow(), keys);

        // The prices are the file's, each ask 7 points above its bid, and every ask at or above
        // its bid on all four prices.
        let prices = |c: &Candle| [c.open, c.high, c.low, c.close];
        for (bid, ask) in history.candles.iter().zip(&history.asks) {
            for (b, a) in prices(bid).into_iter().zip(prices(ask)) {
                assert!((a - b - 0.007).abs() < 1e-9, "{bid:?} {ask:?}");
            }
        }
        // The quiet minutes are flat at the minute before's closes.
        let at = |t: Millis| {
            history
                .candles
                .iter()
                .position(|c| c.open_time == t)
                .expect("a minute")
        };
        let quiet = at(day(0) + 10 * HOUR);
        let before = history.candles[quiet - 1];
        assert_eq!(history.candles[quiet].open, before.close);
        assert_eq!(history.candles[quiet + 1].high, before.close);
        assert_eq!(history.candles[quiet].volume, 0.0);
    }

    #[test]
    fn a_403_is_the_credentials_error_and_keeps_nothing_of_the_answer() {
        let mut files = bucket(22 * 60);
        let body = format!(
            "<Error><Code>SignatureDoesNotMatch</Code><AWSAccessKeyId>{EXAMPLE_KEY_ID}\
             </AWSAccessKeyId><SignatureProvided>f0e8bdb87c96</SignatureProvided></Error>"
        );
        files.insert(day_key("USDJPY", day(1)), (403, body.into_bytes()));
        let asked = RefCell::new(Vec::new());
        let error = read(&files, &asked).expect_err("refused");
        let shown = format!("{error} {error:?}");
        assert!(
            matches!(&error, DataError::AwsRefused { code, .. } if code == "SignatureDoesNotMatch"),
            "{shown}"
        );
        assert!(shown.contains("AWS_ACCESS_KEY_ID"), "{shown}");
        for secret in [EXAMPLE_KEY_ID, EXAMPLE_SECRET, "f0e8bdb87c96"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        // Not retried, and nothing after it is asked for.
        assert_eq!(asked.borrow().len(), 2);
    }

    #[test]
    fn a_5xx_that_persists_fails_the_month_and_a_passing_one_is_retried() {
        let mut files = bucket(22 * 60);
        files.insert(
            day_key("USDJPY", day(1)),
            (503, b"<Error><Code>SlowDown</Code></Error>".to_vec()),
        );
        let asked = RefCell::new(Vec::new());
        let result = read(&files, &asked);
        assert!(
            matches!(&result, Err(DataError::Status { status: 503, body, .. }) if body == "SlowDown"),
            "{result:?}"
        );
        // Thursday once, then Friday five times.
        assert_eq!(asked.borrow().len(), 1 + 5);

        // Two 503s, then the file.
        let mut calls = 0;
        let result = with_retries(&NO_WAIT, || {
            calls += 1;
            if calls < 3 {
                answer("u", 503, Vec::new())
            } else {
                answer("u", 200, vec![1])
            }
        });
        assert_eq!(result.ok(), Some(Some(vec![1])));
        assert_eq!(calls, 3);
        // A 400 is not retried.
        let mut calls = 0;
        let result = with_retries(&NO_WAIT, || {
            calls += 1;
            answer("u", 400, Vec::new())
        });
        assert!(matches!(result, Err(DataError::Status { status: 400, .. })) && calls == 1);
    }

    #[test]
    fn s3_answers_read_as_a_file_no_file_or_an_error() {
        assert_eq!(answer("u", 200, vec![7]).ok(), Some(Some(vec![7])));
        assert_eq!(
            answer("u", 404, b"<Code>NoSuchKey</Code>".to_vec()).ok(),
            Some(None)
        );
        assert_eq!(answer("u", 404, Vec::new()).ok(), Some(None));
        // S3's NoSuchKey under another status is still no file.
        assert_eq!(
            answer("u", 400, b"<Code>NoSuchKey</Code>".to_vec()).ok(),
            Some(None)
        );
        let refused = answer("s3://b/k", 403, b"<Code>AccessDenied</Code>".to_vec());
        assert!(
            matches!(&refused, Err(DataError::AwsRefused { url, code }) if url == "s3://b/k" && code == "AccessDenied")
        );
        // A code that is not a plain word is not kept.
        let odd = answer("u", 500, b"<Code>a b<x></Code>".to_vec());
        assert!(
            matches!(&odd, Err(DataError::Status { status: 500, body, .. }) if body.is_empty())
        );
    }

    #[test]
    fn without_credentials_the_source_fails_before_any_request() {
        let files = bucket(22 * 60);
        let asked = RefCell::new(Vec::new());
        let result = history(
            &usdjpy(),
            day(0),
            day(4),
            far_future(),
            |_| None,
            &NO_WAIT,
            serve(&files, &asked),
        );
        let message = result.map(|_| ()).map_err(|e| e.to_string());
        assert_eq!(
            message,
            Err(
                "dukascopy's history is read from a requester-pays S3 bucket, which needs AWS \
                 credentials: AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY not set (set \
                 AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, and AWS_SESSION_TOKEN with \
                 temporary credentials)"
                    .into()
            )
        );
        assert!(asked.borrow().is_empty());
    }

    /// The request [`get_object`] sends, read back by a local one-shot server: the key's path, the
    /// requester-pays header, and an authorization header of SigV4's form whose signature is the
    /// one computed for exactly the headers sent.
    #[test]
    fn a_get_carries_the_requester_pays_header_and_a_sigv4_authorization() {
        let http = Http::new().expect("an HTTP client");
        let key = day_key("USDJPY", parse_date("2024-01-15").expect("a date"));
        for token in [None, Some("token-example")] {
            let (base, server) = stub::serve_once(&[], b"ticks".to_vec());
            let credentials = AwsCredentials::new(EXAMPLE_KEY_ID, EXAMPLE_SECRET, token);
            let (status, body) = get_object(&http, &credentials, &base, &key).expect("an answer");
            assert_eq!((status, body), (200, b"ticks".to_vec()));
            // The server lower-cases what it read.
            let request = server.join().expect("the server");
            let header = |name: &str| {
                request
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{name}: ")))
                    .unwrap_or_else(|| panic!("no {name} in {request}"))
                    .to_string()
            };
            assert!(
                request.starts_with("get /usdjpy/2024/00/15_ticks.bi5 http/1.1\r\n"),
                "{request}"
            );
            assert_eq!(header("x-amz-request-payer"), "requester");
            assert_eq!(header("x-amz-content-sha256"), EMPTY_SHA256);
            let host = base.strip_prefix("http://").expect("a base");
            assert_eq!(header("host"), host);
            let date = header("x-amz-date").to_uppercase();
            assert_eq!(date.len(), 16, "{date}");
            let mut signed = vec![
                ("host", host),
                ("x-amz-content-sha256", EMPTY_SHA256),
                ("x-amz-date", date.as_str()),
                ("x-amz-request-payer", "requester"),
            ];
            if let Some(token) = token {
                assert_eq!(header("x-amz-security-token"), token);
                signed.push(("x-amz-security-token", token));
            }
            let expected = sigv4::authorization(
                &credentials,
                REGION,
                "/USDJPY/2024/00/15_ticks.bi5",
                &signed,
                &date,
            );
            assert_eq!(header("authorization"), expected.to_lowercase());
            let names = if token.is_some() {
                "host;x-amz-content-sha256;x-amz-date;x-amz-request-payer;x-amz-security-token"
            } else {
                "host;x-amz-content-sha256;x-amz-date;x-amz-request-payer"
            };
            let form = format!(
                "AWS4-HMAC-SHA256 Credential={EXAMPLE_KEY_ID}/{}/eu-west-1/s3/aws4_request,SignedHeaders={names},Signature=",
                &date[..8]
            );
            let signature = expected
                .strip_prefix(&form)
                .unwrap_or_else(|| panic!("{expected}"));
            assert!(
                signature.len() == 64 && signature.bytes().all(|b| b.is_ascii_hexdigit()),
                "{expected}"
            );
            // The secret itself is never sent.
            assert!(
                !request.contains(&EXAMPLE_SECRET.to_lowercase()),
                "{request}"
            );
        }
    }

    #[test]
    fn only_fx_is_listed_with_no_funding_and_no_live_stream() {
        let http = Http::new().expect("an HTTP client");
        let source = crate::source(Venue::Dukascopy);
        assert_eq!(source.venue(), Venue::Dukascopy);
        assert_eq!(source.kinds(), [InstrumentKind::Fx]);
        for kind in [InstrumentKind::Spot, InstrumentKind::Perpetual] {
            let id = InstrumentId { kind, ..usdjpy() };
            let refused = |e: DataError| {
                e.to_string() == format!("dukascopy does not list {kind} instruments")
            };
            assert!(source.default_fees(kind).is_err_and(refused));
            assert!(source.instruments(&http, kind).is_err_and(refused));
            assert!(
                source
                    .minute_history(&http, &id, 0, MINUTE, DAY)
                    .is_err_and(refused)
            );
        }
        let fees = source.default_fees(InstrumentKind::Fx).expect("fees");
        assert_eq!((fees.maker, fees.taker, fees.per_unit), (0.0, 0.0, 0.0));
        assert!(matches!(
            source.funding(&http, &usdjpy(), 0, DAY),
            Err(DataError::NoFunding(Venue::Dukascopy))
        ));
        assert!(matches!(
            source.live(),
            Err(DataError::NoLiveStream(Venue::Dukascopy))
        ));
        // A pair outside the table is refused before the credentials are read or any request.
        let gold = InstrumentId {
            symbol: "XAUUSD".into(),
            ..usdjpy()
        };
        let refused = source.minute_history(&http, &gold, 0, MINUTE, DAY);
        assert!(
            matches!(&refused, Err(DataError::InvalidInstrument { symbol, .. }) if symbol == "XAUUSD"),
            "{refused:?}"
        );
    }

    #[test]
    fn the_pair_table_has_ticks_by_quote_currency_and_micro_lot_minimums() {
        let http = Http::new().expect("an HTTP client");
        let list = crate::fetch_instruments(&http, Venue::Dukascopy, InstrumentKind::Fx)
            .expect("the table");
        assert!(list.rejected.is_empty(), "{:?}", list.rejected);
        assert_eq!(list.instruments.len(), 20);
        let find = |symbol: &str| {
            list.instruments
                .iter()
                .find(|i| i.id.symbol == symbol)
                .unwrap_or_else(|| panic!("{symbol}"))
                .clone()
        };
        let jpy = find("USDJPY");
        assert_eq!((jpy.base.as_str(), jpy.quote.as_str()), ("USD", "JPY"));
        assert_eq!(jpy.tick_size, 0.001);
        assert_eq!(find("GBPJPY").tick_size, 0.001);
        let eur = find("EURUSD");
        assert_eq!((eur.base.as_str(), eur.quote.as_str()), ("EUR", "USD"));
        assert_eq!(eur.tick_size, 0.00001);
        for i in &list.instruments {
            assert_eq!((i.lot_size, i.min_qty), (1.0, 1_000.0), "{}", i.id);
            assert_eq!(i.id.venue, Venue::Dukascopy);
            assert_eq!(i.id.kind, InstrumentKind::Fx);
        }
        // Prices round onto the file's own grid.
        assert_eq!(
            jpy.round_price(150.1234, td_core::Rounding::Nearest),
            150.123
        );
        assert_eq!(
            eur.round_price(1.098_716, td_core::Rounding::Nearest),
            1.09872
        );
    }

    /// The bucket of [`bucket`] as a source, for the download path.
    struct FakeBucket(Files);

    impl DataSource for FakeBucket {
        fn venue(&self) -> Venue {
            Venue::Dukascopy
        }

        fn kinds(&self) -> &'static [InstrumentKind] {
            Dukascopy.kinds()
        }

        fn instruments(
            &self,
            http: &Http,
            kind: InstrumentKind,
        ) -> Result<InstrumentList, DataError> {
            Dukascopy.instruments(http, kind)
        }

        fn default_fees(&self, kind: InstrumentKind) -> Result<FeeSchedule, DataError> {
            Dukascopy.default_fees(kind)
        }

        fn minute_history(
            &self,
            _: &Http,
            id: &InstrumentId,
            from: Millis,
            to: Millis,
            now: Millis,
        ) -> Result<MinuteHistory, DataError> {
            let asked = RefCell::new(Vec::new());
            history(id, from, to, now, env, &NO_WAIT, serve(&self.0, &asked)).map(|(h, _)| h)
        }
    }

    #[test]
    fn a_download_stores_bids_with_asks_and_the_weekend_is_no_gap() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let http = Http::new().expect("an HTTP client");
        // Sunday's ticks start at the 22:00 open.
        let files = bucket(22 * 60);
        let id = usdjpy();
        let (from, to) = (day(0), day(4));
        let report = download_candles_from(
            &FakeBucket(files.clone()),
            &http,
            &store,
            &id,
            from,
            to,
            &mut |_| {},
        )
        .expect("downloads");

        let asked = RefCell::new(Vec::new());
        let (served, _) = read(&files, &asked).expect("reads");
        let month = Month::containing(from);
        assert_eq!(report.months, [(month, served.candles.len())]);
        assert_eq!(served.candles.len(), covered(22 * 60).len());
        assert_eq!(report.missing_files, [day_key("USDJPY", day(2))]);
        assert_eq!(report.gaps, []);

        let stored_bids = store.read_month(&id, month).expect("bids");
        assert_eq!(stored_bids, served.candles);
        // The month file has ask columns; ask volumes are not stored.
        let stored_asks = store.read_month_asks(&id, month).expect("asks");
        let served_asks: Vec<Candle> = served
            .asks
            .iter()
            .map(|a| Candle {
                volume: 0.0,
                quote_volume: 0.0,
                ..*a
            })
            .collect();
        assert_eq!(stored_asks, served_asks);

        let coverage = store.coverage(&id).expect("coverage");
        assert_eq!(coverage.len(), 1);
        assert_eq!(coverage[0].minutes, covered(22 * 60).len());
        assert_eq!(coverage[0].first, day(0));
        assert_eq!(coverage[0].last, day(4) - MINUTE);
        assert_eq!(coverage[0].gaps, []);

        let stored = store.stored().expect("the listing");
        assert!(stored.unrecognised.is_empty(), "{:?}", stored.unrecognised);
        assert_eq!(stored.instruments.len(), 1);
        assert_eq!(stored.instruments[0].id, id);
        assert_eq!(stored.instruments[0].months.len(), 1);
        let path = store.candle_path(&id, month).expect("a path");
        let relative = path.strip_prefix(dir.path()).expect("inside the store");
        assert!(
            relative.starts_with(std::path::Path::new("candles").join("dukascopy").join("fx")),
            "{}",
            relative.display()
        );

        let deleted = store.delete_instrument(&id).expect("deletes");
        assert_eq!(deleted.files, 1);
        assert!(store.stored().expect("the listing").instruments.is_empty());
    }
}
