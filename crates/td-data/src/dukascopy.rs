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

use std::io::Cursor;

use td_core::{Candle, Millis, civil_from_days};

use crate::DataError;
use crate::time::{DAY, MINUTE};

const FEED: &str = "https://datafeed.dukascopy.com/datafeed";
/// The bytes in one decompressed record.
const RECORD: usize = 24;

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
    if raw.len() % RECORD != 0 {
        return Err(bad(format!(
            "decompresses to {} bytes, which is not a whole number of {RECORD}-byte records",
            raw.len()
        )));
    }
    let mut candles: Vec<Candle> = Vec::with_capacity(raw.len() / RECORD);
    for record in raw.chunks_exact(RECORD) {
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
