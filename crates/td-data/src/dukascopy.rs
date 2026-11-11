//! Dukascopy as a [`DataSource`]: FX spot pairs from a fixed table, and their 1-minute bid and ask
//! candles from Dukascopy's free historical data feed at datafeed.dukascopy.com, which needs no key.
//! No funding and no live stream.
//!
//! The feed publishes one file per pair, UTC day and side:
//! `<SYMBOL>/<YYYY>/<MM>/<DD>/BID_candles_min_1.bi5` and `ASK_candles_min_1.bi5`, where `MM` counts
//! months from `00` (January) to `11` (December). A file is LZMA (the `.lzma` format with its
//! 13-byte header); decompressed it is 24-byte big-endian records, one per minute: the seconds since
//! the start of the day (u32), then open, close, low, high (u32 each, the price times 1,000 for a
//! JPY-quoted pair and times 100,000 for the rest), then the volume (f32).
//!
//! Checked against real USDJPY files on 2026-11-11: each file held 1,440 records, minute by minute
//! from 0 to 86,340 seconds, weekends included; low ≤ open, close ≤ high held for every record, and
//! each close matched the next minute's open far more often in that order than with open and close
//! swapped. Saturdays, and Fridays after the close and Sundays before the open, are flat
//! zero-volume filler at the last price. A day the feed has not published (today, a future day, or
//! one before the pair's history) answers 404. The server is slow and often refuses a request with
//! a 503 or a failed connection that succeeds when tried again, so each file is retried.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::Duration;

use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, Venue, civil_from_days,
};

use crate::http::Http;
use crate::source::{DataSource, MinuteHistory};
use crate::time::{DAY, MINUTE, Month};
use crate::{DataError, InstrumentList};

const FEED: &str = "https://datafeed.dukascopy.com/datafeed";
/// The bytes in one decompressed record.
const RECORD: usize = 24;
/// The pauses before each retry of a file the feed refused for a while (a failed connection, a 429
/// or a 5xx): five tries over about 40 seconds.
const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
];

/// The pairs served, as the feed names them: the seven majors and the main crosses. Each is its
/// base currency's three letters then its quote currency's.
///
/// shortcut: a fixed table, add a row for a new pair; replace with the feed's instrument list if
/// one turns up.
const PAIRS: [&str; 20] = [
    "EURUSD", "USDJPY", "GBPUSD", "USDCHF", "AUDUSD", "USDCAD", "NZDUSD", "EURJPY", "GBPJPY",
    "AUDJPY", "CADJPY", "CHFJPY", "NZDJPY", "EURGBP", "EURCHF", "EURAUD", "EURCAD", "GBPCHF",
    "GBPAUD", "AUDNZD",
];

/// FX at Dukascopy is charged a commission per unit rather than a fraction of notional, so the
/// fractional fees are 0. The default FX commission is set when the per-unit commission field
/// lands.
const FX_FEES: FeeSchedule = FeeSchedule {
    maker: 0.0,
    taker: 0.0,
};

/// Dukascopy's historical feed: FX pairs only.
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

    /// Each UTC day before today that overlaps `[from, to)`, whole: its bid file and its ask file,
    /// one after the other, with the minutes the store would refuse left out ([`pair_day`]) and
    /// counted on standard error.
    fn minute_history(
        &self,
        http: &Http,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
        now: Millis,
    ) -> Result<MinuteHistory, DataError> {
        history(id, from, to, now, |file| {
            with_retries(&RETRY_DELAYS, || http.get_bytes(&day_url(file)))
        })
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

/// What the feed's integer prices are divided by for a pair quoted in `quote`.
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

/// The minutes [`pair_day`] left out because the store would refuse them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Dropped {
    /// In the kind's closed hours, on either side or both: mostly the feed's weekend filler.
    pub closed: usize,
    /// Open, with a candle on one side only.
    pub one_side: usize,
    /// Open, on both sides, with the ask's open or close below the bid's.
    pub crossed: usize,
}

/// [`Dukascopy::minute_history`], with `fetch` reading one day file by its [`day_file`] path
/// (`None` when the feed does not have it), and the minutes left out. Days the feed lacks are
/// named in [`MinuteHistory::missing_files`] and have no minutes; any error from `fetch` fails the
/// whole call, so a month never comes back short of a day the feed failed to serve.
pub(crate) fn history(
    id: &InstrumentId,
    from: Millis,
    to: Millis,
    now: Millis,
    mut fetch: impl FnMut(&str) -> Result<Option<Vec<u8>>, DataError>,
) -> Result<(MinuteHistory, Dropped), DataError> {
    fx_only(id.kind)?;
    if !PAIRS.contains(&id.symbol.as_str()) {
        return Err(DataError::InvalidInstrument {
            symbol: id.symbol.clone(),
            reason: "not one of the FX pairs read from dukascopy".into(),
        });
    }
    let scale = scale(&id.symbol[3..]);
    let mut history = MinuteHistory::default();
    let mut dropped = Dropped::default();
    let today = now.div_euclid(DAY) * DAY;
    let mut day = from.div_euclid(DAY) * DAY;
    while day < to && day < today {
        let mut read = |side: Side| -> Result<Vec<Candle>, DataError> {
            let file = day_file(&id.symbol, day, side);
            match fetch(&file)? {
                Some(bytes) => decode_day(&file, &bytes, day, scale),
                None => {
                    history.missing_files.push(file);
                    Ok(Vec::new())
                }
            }
        };
        let bids = read(Side::Bid)?;
        let asks = read(Side::Ask)?;
        pair_day(id.kind, bids, asks, &mut history, &mut dropped);
        day += DAY;
    }
    if dropped != Dropped::default() {
        eprintln!(
            "td-data: {id} {}: left out {} minutes in the closed hours, {} on one side only and \
             {} crossed",
            Month::containing(from),
            dropped.closed,
            dropped.one_side,
            dropped.crossed
        );
    }
    Ok((history, dropped))
}

/// Adds the minutes of one day that [`crate::Store::write_quotes`] takes to `history`, bids as its
/// candles and asks beside them, and counts the rest in `dropped`, in this order: a minute in
/// `kind`'s closed hours, one with a candle on one side only, and one whose ask opens or closes
/// below its bid.
fn pair_day(
    kind: InstrumentKind,
    bids: Vec<Candle>,
    asks: Vec<Candle>,
    history: &mut MinuteHistory,
    dropped: &mut Dropped,
) {
    let mut asks: BTreeMap<Millis, Candle> = asks.into_iter().map(|c| (c.open_time, c)).collect();
    for bid in bids {
        let ask = asks.remove(&bid.open_time);
        if !kind.is_open(bid.open_time) {
            dropped.closed += 1;
            continue;
        }
        let Some(ask) = ask else {
            dropped.one_side += 1;
            continue;
        };
        if ask.open < bid.open || ask.close < bid.close {
            dropped.crossed += 1;
            continue;
        }
        history.candles.push(bid);
        history.asks.push(ask);
    }
    for t in asks.into_keys() {
        if kind.is_open(t) {
            dropped.one_side += 1;
        } else {
            dropped.closed += 1;
        }
    }
}

/// Calls `get` until it gives anything but a passing failure (a failed connection, a 429 or a
/// 5xx), sleeping each of `delays` in turn between tries, and returns the last answer.
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
        DataError::Status { status, .. } => *status == 429 || *status >= 500,
        _ => false,
    }
}

/// Which side of the quote a file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Bid,
    Ask,
}

impl Side {
    fn name(self) -> &'static str {
        match self {
            Side::Bid => "BID",
            Side::Ask => "ASK",
        }
    }
}

/// The feed's path to the 1-minute candles of `symbol` for one side of the UTC day that `day`
/// falls in, relative to [`FEED`].
pub(crate) fn day_file(symbol: &str, day: Millis, side: Side) -> String {
    let (year, month, date) = civil_from_days(day.div_euclid(DAY));
    format!(
        "{symbol}/{year:04}/{:02}/{date:02}/{}_candles_min_1.bi5",
        month - 1,
        side.name()
    )
}

/// The full URL of [`day_file`].
pub(crate) fn day_url(file: &str) -> String {
    format!("{FEED}/{file}")
}

/// Reads one day's `.bi5` file, `file` naming it in errors: the 1-minute candles of the UTC day
/// starting at `day`, oldest first, with prices divided by `scale` (1,000 for a JPY-quoted pair,
/// 100,000 for the rest). An empty file is a day with no minutes. The feed gives no quote volume,
/// so `quote_volume` is 0; `volume` is the feed's figure as it stands.
pub(crate) fn decode_day(
    file: &str,
    bi5: &[u8],
    day: Millis,
    scale: f64,
) -> Result<Vec<Candle>, DataError> {
    if bi5.is_empty() {
        return Ok(Vec::new());
    }
    let bad = |reason: String| DataError::Archive {
        file: file.to_string(),
        reason,
    };
    let mut raw = Vec::new();
    lzma_rs::lzma_decompress(&mut Cursor::new(bi5), &mut raw)
        .map_err(|e| bad(format!("does not decompress as LZMA: {e}")))?;
    let (records, rest) = raw.as_chunks::<RECORD>();
    if !rest.is_empty() {
        return Err(bad(format!(
            "decompresses to {} bytes, which is not a whole number of {RECORD}-byte records",
            raw.len()
        )));
    }
    let mut candles: Vec<Candle> = Vec::with_capacity(records.len());
    for record in records {
        let word = |i: usize| u32::from_be_bytes(record[i..i + 4].try_into().expect("4 bytes"));
        let seconds = Millis::from(word(0));
        let open_time = day + seconds * 1000;
        if seconds * 1000 % MINUTE != 0 || seconds * 1000 >= DAY {
            return Err(bad(format!(
                "a record at {seconds} s is not on a minute of the day"
            )));
        }
        if candles.last().is_some_and(|c| c.open_time >= open_time) {
            return Err(bad(format!(
                "the record at {seconds} s is not after the one before it"
            )));
        }
        let price = |i: usize| f64::from(word(i)) / scale;
        candles.push(Candle {
            open_time,
            open: price(4),
            close: price(8),
            low: price(12),
            high: price(16),
            volume: f64::from(f32::from_be_bytes(
                record[20..24].try_into().expect("4 bytes"),
            )),
            quote_volume: 0.0,
        });
    }
    Ok(candles)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// One record as the feed writes it: seconds into the day, then open, close, low and high in
    /// points, then the volume.
    pub(crate) type Record = (u32, [u32; 4], f32);

    /// `records` as a `.bi5` file: encoded and LZMA-compressed.
    pub(crate) fn bi5(records: &[Record]) -> Vec<u8> {
        let mut raw = Vec::with_capacity(records.len() * RECORD);
        for (seconds, prices, volume) in records {
            raw.extend(seconds.to_be_bytes());
            for p in prices {
                raw.extend(p.to_be_bytes());
            }
            raw.extend(volume.to_be_bytes());
        }
        let mut file = Vec::new();
        lzma_rs::lzma_compress(&mut Cursor::new(raw), &mut file).expect("compresses");
        file
    }

    /// One USDJPY day as the feed writes it, bids then asks: 1,440 minutes, prices in thousandths
    /// that move with the minute while the market is open, and flat zero-volume filler at the last
    /// price while it is closed. Each ask is 7 points above its bid.
    pub(crate) fn feed_day(day: Millis) -> (Vec<Record>, Vec<Record>) {
        let (mut bids, mut asks) = (Vec::new(), Vec::new());
        let mut last = 150_000;
        for m in 0..1440u32 {
            let t = day + Millis::from(m) * MINUTE;
            let bid = if InstrumentKind::Fx.is_open(t) {
                let i = u32::try_from(t / MINUTE).expect("fits");
                let (open, close) = (150_000 + i % 37 * 10, 150_000 + (i + 11) % 37 * 10);
                last = close;
                (
                    m * 60,
                    [open, close, open.min(close) - 3, open.max(close) + 4],
                    1.5,
                )
            } else {
                (m * 60, [last; 4], 0.0)
            };
            let (seconds, prices, volume) = bid;
            bids.push(bid);
            asks.push((seconds, prices.map(|p| p + 7), volume));
        }
        (bids, asks)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::bi5;
    use super::*;
    use crate::time::parse_date;

    fn candle(open_time: Millis, [open, close, low, high]: [f64; 4], volume: f64) -> Candle {
        Candle {
            open_time,
            open,
            high,
            low,
            close,
            volume,
            quote_volume: 0.0,
        }
    }

    #[test]
    fn a_jpy_quoted_day_reads_in_thousandths_in_open_close_low_high_order() {
        let day = parse_date("2024-01-08").expect("a date");
        let file = bi5(&[
            (0, [150_123, 150_130, 150_101, 150_145], 12.5),
            (60, [150_130, 150_118, 150_110, 150_131], 0.0),
            (86_340, [151_000, 151_002, 150_999, 151_004], 3.25),
        ]);
        let candles = decode_day("f", &file, day, 1_000.0).expect("decodes");
        assert_eq!(
            candles,
            [
                candle(day, [150.123, 150.13, 150.101, 150.145], 12.5),
                candle(day + MINUTE, [150.13, 150.118, 150.11, 150.131], 0.0),
                candle(day + DAY - MINUTE, [151.0, 151.002, 150.999, 151.004], 3.25),
            ]
        );
    }

    #[test]
    fn a_non_jpy_day_reads_in_hundred_thousandths() {
        let day = parse_date("2016-03-09").expect("a date");
        let file = bi5(&[(720 * 60, [109_871, 109_902, 109_850, 109_911], 1.5)]);
        let candles = decode_day("f", &file, day, 100_000.0).expect("decodes");
        assert_eq!(
            candles,
            [candle(
                day + 720 * MINUTE,
                [1.09871, 1.09902, 1.0985, 1.09911],
                1.5
            )]
        );
    }

    #[test]
    fn an_empty_file_is_a_day_without_minutes() {
        assert_eq!(decode_day("f", &[], 0, 1_000.0).expect("decodes"), []);
        // An LZMA stream of no records reads the same.
        assert_eq!(decode_day("f", &bi5(&[]), 0, 1_000.0).expect("decodes"), []);
    }

    #[test]
    fn a_file_that_is_not_whole_records_or_not_lzma_or_off_the_minute_is_refused() {
        let error = |bytes: &[u8]| match decode_day("USDJPY/x.bi5", bytes, 0, 1_000.0) {
            Err(DataError::Archive { file, reason }) => {
                assert_eq!(file, "USDJPY/x.bi5");
                reason
            }
            other => panic!("expected an archive error, got {other:?}"),
        };
        assert!(error(b"<html>503</html>").contains("LZMA"));
        let mut raw = Vec::new();
        lzma_rs::lzma_compress(&mut Cursor::new(vec![0u8; 25]), &mut raw).expect("compresses");
        assert!(error(&raw).contains("25 bytes"));
        assert!(error(&bi5(&[(30, [1, 1, 1, 1], 0.0)])).contains("30 s"));
        assert!(error(&bi5(&[(86_400, [1, 1, 1, 1], 0.0)])).contains("86400 s"));
        let backwards = bi5(&[(120, [1, 1, 1, 1], 0.0), (60, [1, 1, 1, 1], 0.0)]);
        assert!(error(&backwards).contains("60 s is not after"));
    }

    #[test]
    fn day_files_count_months_from_zero() {
        let jan = parse_date("2024-01-05").expect("a date");
        assert_eq!(
            day_file("USDJPY", jan, Side::Bid),
            "USDJPY/2024/00/05/BID_candles_min_1.bi5"
        );
        // Any time in the day names the same file.
        let dec = parse_date("2025-12-25").expect("a date") + DAY - MINUTE;
        assert_eq!(
            day_file("EURUSD", dec, Side::Ask),
            "EURUSD/2025/11/25/ASK_candles_min_1.bi5"
        );
        let early = parse_date("2003-10-01").expect("a date");
        assert_eq!(
            day_url(&day_file("GBPJPY", early, Side::Bid)),
            "https://datafeed.dukascopy.com/datafeed/GBPJPY/2003/09/01/BID_candles_min_1.bi5"
        );
    }
}

/// The source on a fake feed: [`history`] reading files from memory, and the download path into a
/// temp store.
#[cfg(test)]
mod source_tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::fixtures::{Record, bi5, feed_day};
    use super::*;
    use crate::Store;
    use crate::download::download_candles_from;
    use crate::time::parse_date;

    const HOUR: Millis = 60 * MINUTE;

    /// What the fake feed answers for one file: its bytes, `None` for a 404, or another status.
    type Served = Result<Option<Vec<u8>>, u16>;

    fn usdjpy() -> InstrumentId {
        InstrumentId {
            venue: Venue::Dukascopy,
            symbol: "USDJPY".into(),
            kind: InstrumentKind::Fx,
        }
    }

    /// Thursday 2024-01-11. That week is in New York standard time, so the FX weekend runs from
    /// Friday 22:00 UTC to Sunday 22:00 UTC.
    fn thursday() -> Millis {
        parse_date("2024-01-11").expect("a date")
    }

    /// A Thursday minute whose ask is missing.
    fn bid_only() -> Millis {
        thursday() + 12 * HOUR
    }

    /// A Thursday minute whose ask opens below its bid.
    fn crossed() -> Millis {
        thursday() + 13 * HOUR
    }

    /// A Thursday minute whose bid is missing.
    fn ask_only() -> Millis {
        thursday() + 14 * HOUR
    }

    /// Thursday to Sunday as the feed serves them, with [`bid_only`], [`crossed`] and [`ask_only`]
    /// spoiled.
    fn feed() -> HashMap<String, Served> {
        let mut files = HashMap::new();
        for d in 0..4 {
            let day = thursday() + d * DAY;
            let (mut bids, mut asks) = feed_day(day);
            if d == 0 {
                let second = |t: Millis| u32::try_from((t - day) / 1000).expect("fits");
                asks.retain(|r| r.0 != second(bid_only()));
                let ask: &mut Record = asks
                    .iter_mut()
                    .find(|r| r.0 == second(crossed()))
                    .expect("the minute");
                ask.1[0] = bids[(second(crossed()) / 60) as usize].1[0] - 1;
                bids.retain(|r| r.0 != second(ask_only()));
            }
            files.insert(day_file("USDJPY", day, Side::Bid), Ok(Some(bi5(&bids))));
            files.insert(day_file("USDJPY", day, Side::Ask), Ok(Some(bi5(&asks))));
        }
        files
    }

    /// Serves `files`, noting each path asked for in `asked`; a path not in `files` is a 404.
    fn serve<'a>(
        files: &'a HashMap<String, Served>,
        asked: &'a RefCell<Vec<String>>,
    ) -> impl FnMut(&str) -> Result<Option<Vec<u8>>, DataError> + 'a {
        move |file| {
            asked.borrow_mut().push(file.to_string());
            match files.get(file) {
                None => Ok(None),
                Some(Ok(body)) => Ok(body.clone()),
                Some(Err(status)) => Err(DataError::Status {
                    url: day_url(file),
                    status: *status,
                    body: String::new(),
                }),
            }
        }
    }

    /// The open minutes from Thursday to Sunday, without the spoiled ones.
    fn kept_minutes() -> Vec<Millis> {
        (0..4 * DAY / MINUTE)
            .map(|i| thursday() + i * MINUTE)
            .filter(|&t| {
                InstrumentKind::Fx.is_open(t) && ![bid_only(), crossed(), ask_only()].contains(&t)
            })
            .collect()
    }

    fn gap(t: Millis) -> crate::Gap {
        crate::Gap {
            from: t,
            to: t + MINUTE,
        }
    }

    fn far_future() -> Millis {
        parse_date("2030-01-01").expect("a date")
    }

    #[test]
    fn bids_and_asks_cover_the_same_open_minutes_and_the_rest_are_counted() {
        let files = feed();
        let asked = RefCell::new(Vec::new());
        let (from, to) = (thursday(), thursday() + 4 * DAY);
        let (history, dropped) =
            history(&usdjpy(), from, to, far_future(), serve(&files, &asked)).expect("reads");

        let bid_times: Vec<Millis> = history.candles.iter().map(|c| c.open_time).collect();
        let ask_times: Vec<Millis> = history.asks.iter().map(|c| c.open_time).collect();
        assert_eq!(bid_times, kept_minutes());
        assert_eq!(ask_times, bid_times);
        // Thursday less three, Friday to 22:00, nothing on Saturday, Sunday from 22:00.
        assert_eq!(bid_times.len(), 1437 + 1320 + 120);
        assert_eq!(
            dropped,
            Dropped {
                // Friday from 22:00, all Saturday, and Sunday to 22:00.
                closed: 120 + 1440 + 1320,
                one_side: 2,
                crossed: 1,
            }
        );
        assert!(history.missing_files.is_empty());

        // The prices are the feed's, each ask 7 points above its bid.
        let first = (history.candles[0], history.asks[0]);
        assert_eq!(first.0.open_time, thursday());
        assert!(
            (first.1.open - first.0.open - 0.007).abs() < 1e-9,
            "{first:?}"
        );
        for (bid, ask) in history.candles.iter().zip(&history.asks) {
            assert!(
                ask.open >= bid.open && ask.close >= bid.close,
                "{bid:?} {ask:?}"
            );
        }

        // Each day's bid file, then its ask file.
        let expected: Vec<String> = (0..4)
            .flat_map(|d| {
                let day = thursday() + d * DAY;
                [Side::Bid, Side::Ask].map(|side| day_file("USDJPY", day, side))
            })
            .collect();
        assert_eq!(*asked.borrow(), expected);
    }

    #[test]
    fn an_http_failure_on_one_day_fails_the_month() {
        let mut files = feed();
        let saturday = thursday() + 2 * DAY;
        files.insert(day_file("USDJPY", saturday, Side::Ask), Err(503));
        let asked = RefCell::new(Vec::new());
        let result = history(
            &usdjpy(),
            thursday(),
            thursday() + 4 * DAY,
            far_future(),
            serve(&files, &asked),
        );
        assert!(
            matches!(result, Err(DataError::Status { status: 503, .. })),
            "{result:?}"
        );
        // Nothing after the failed file is asked for.
        assert_eq!(
            asked.borrow().last(),
            Some(&day_file("USDJPY", saturday, Side::Ask))
        );
    }

    #[test]
    fn a_day_the_feed_lacks_is_named_and_has_no_minutes_and_today_is_not_asked_for() {
        let mut files = feed();
        let friday = thursday() + DAY;
        files.remove(&day_file("USDJPY", friday, Side::Bid));
        files.remove(&day_file("USDJPY", friday, Side::Ask));
        let asked = RefCell::new(Vec::new());
        // Saturday is today, so Saturday and Sunday are not published yet.
        let now = thursday() + 2 * DAY + 9 * HOUR;
        let (history, dropped) = history(
            &usdjpy(),
            thursday(),
            thursday() + 4 * DAY,
            now,
            serve(&files, &asked),
        )
        .expect("reads");
        assert_eq!(
            history.missing_files,
            [
                day_file("USDJPY", friday, Side::Bid),
                day_file("USDJPY", friday, Side::Ask)
            ]
        );
        assert_eq!(history.candles.len(), 1437);
        assert_eq!(history.asks.len(), 1437);
        assert_eq!(dropped.closed, 0);
        assert_eq!(asked.borrow().len(), 4, "{:?}", asked.borrow());
    }

    #[test]
    fn a_passing_failure_is_retried_and_any_other_answer_is_not() {
        let no_wait = [Duration::ZERO; 4];
        let status = |status| DataError::Status {
            url: "u".into(),
            status,
            body: String::new(),
        };
        // Two 503s, then the file.
        let mut calls = 0;
        let answer = with_retries(&no_wait, || {
            calls += 1;
            if calls < 3 {
                Err(status(503))
            } else {
                Ok(Some(vec![1]))
            }
        });
        assert_eq!(answer.ok(), Some(Some(vec![1])));
        assert_eq!(calls, 3);
        // A 404 (no file) and a 400 come back at once.
        let mut calls = 0;
        let answer = with_retries(&no_wait, || {
            calls += 1;
            Ok::<Option<Vec<u8>>, DataError>(None)
        });
        assert!(matches!(answer, Ok(None)) && calls == 1);
        let mut calls = 0;
        let answer = with_retries::<()>(&no_wait, || {
            calls += 1;
            Err(status(400))
        });
        assert!(matches!(answer, Err(DataError::Status { status: 400, .. })) && calls == 1);
        // A feed that keeps refusing is tried five times, then its last answer stands.
        let mut calls = 0;
        let answer = with_retries::<()>(&no_wait, || {
            calls += 1;
            Err(status(429))
        });
        assert!(matches!(answer, Err(DataError::Status { status: 429, .. })) && calls == 5);
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
        assert_eq!((fees.maker, fees.taker), (0.0, 0.0));
        assert!(matches!(
            source.funding(&http, &usdjpy(), 0, DAY),
            Err(DataError::NoFunding(Venue::Dukascopy))
        ));
        assert!(matches!(
            source.live(),
            Err(DataError::NoLiveStream(Venue::Dukascopy))
        ));
        // A pair outside the table is refused before any request.
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
        // Prices round onto the feed's own grid.
        assert_eq!(
            jpy.round_price(150.1234, td_core::Rounding::Nearest),
            150.123
        );
        assert_eq!(
            eur.round_price(1.098_716, td_core::Rounding::Nearest),
            1.09872
        );
    }

    /// The feed of [`feed`] as a source, for the download path.
    struct FakeFeed(HashMap<String, Served>);

    impl DataSource for FakeFeed {
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
            history(id, from, to, now, serve(&self.0, &asked)).map(|(history, _)| history)
        }
    }

    #[test]
    fn a_download_stores_bids_with_asks_and_the_weekend_is_no_gap() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let http = Http::new().expect("an HTTP client");
        let files = feed();
        let id = usdjpy();
        let (from, to) = (thursday(), thursday() + 4 * DAY);
        let report = download_candles_from(
            &FakeFeed(files.clone()),
            &http,
            &store,
            &id,
            from,
            to,
            &mut |_| {},
        )
        .expect("downloads");

        let asked = RefCell::new(Vec::new());
        let (served, _) =
            history(&id, from, to, far_future(), serve(&files, &asked)).expect("reads");
        let month = Month::containing(from);
        assert_eq!(report.months, [(month, served.candles.len())]);
        // The spoiled minutes are the only gaps: the weekend is not one.
        let spoiled = [gap(bid_only()), gap(crossed()), gap(ask_only())];
        assert_eq!(report.gaps, spoiled);

        let stored_bids = store.read_month(&id, month).expect("bids");
        assert_eq!(stored_bids, served.candles);
        // The month file has ask columns; ask volumes are not stored.
        let stored_asks = store.read_month_asks(&id, month).expect("asks");
        // Each ask is the fake feed's 7 points above its bid, on every price.
        for (bid, ask) in stored_bids.iter().zip(&stored_asks) {
            let prices = |c: &Candle| [c.open, c.high, c.low, c.close];
            for (b, a) in prices(bid).into_iter().zip(prices(ask)) {
                assert!((a - b - 0.007).abs() < 1e-9, "{bid:?} {ask:?}");
            }
            assert_eq!((bid.volume, ask.volume), (1.5, 0.0));
        }
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
        assert_eq!(coverage[0].minutes, kept_minutes().len());
        assert_eq!(coverage[0].first, thursday());
        assert_eq!(coverage[0].last, thursday() + 4 * DAY - MINUTE);
        assert_eq!(coverage[0].gaps, spoiled);

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
