use td_core::{Candle, Millis, Timeframe};

use crate::time::MINUTE;

/// How much of a bucket may be missing, in percent of its minutes, for its bar to still be built as
/// normal, provided its first and last minutes are there. The count is rounded down, so 1m to 1h
/// bars tolerate no missing minute, 4h bars 2, 1d bars 14 and 1w bars 100. Venue archives have
/// short outage gaps, and dropping a whole daily bar for one missing minute would distort a daily
/// backtest more than a bar built from 99% of its minutes does.
const TOLERATED_MISSING_PERCENT: i64 = 1;

/// Bars resampled from 1-minute candles, with the buckets that were left out or built with minutes
/// missing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedCandles {
    /// The bars, oldest first.
    pub candles: Vec<Candle>,
    /// The open times of the buckets left out as incomplete.
    pub dropped: Vec<Millis>,
    /// The open times of the bars built with a tolerated number of minutes missing, and how many.
    pub tolerated: Vec<(Millis, u32)>,
}

/// Builds bars of `timeframe` from 1-minute candles sorted by open time with no duplicates.
///
/// Each bar opens at its [`Timeframe::floor`] bucket and takes the open of its first minute, the
/// highest high, the lowest low, the close of its last minute and the summed volumes. Missing
/// minutes are not filled. A bucket with every minute is complete. A bucket whose first and last
/// minutes are there and that misses no more than 1% of its minutes, rounded down, is tolerated:
/// its bar is built as normal and listed in `tolerated`. Any other bucket is incomplete: it is left
/// out and listed in `dropped`, unless `include_incomplete` is set, in which case its bar is built
/// as normal and not listed. A bucket with no minutes at all gives no bar and is not listed.
pub fn resample(
    minutes: &[Candle],
    timeframe: Timeframe,
    include_incomplete: bool,
) -> LoadedCandles {
    let bucket_minutes = timeframe.duration_ms() / MINUTE;
    let tolerated_missing = tolerated_missing(timeframe);
    let mut loaded = LoadedCandles::default();
    for bucket in
        minutes.chunk_by(|a, b| timeframe.floor(a.open_time) == timeframe.floor(b.open_time))
    {
        let (first, last) = (bucket[0], bucket[bucket.len() - 1]);
        let open_time = timeframe.floor(first.open_time);
        let missing = bucket_minutes - bucket.len() as i64;
        let ends_present = first.open_time == open_time
            && last.open_time == open_time + timeframe.duration_ms() - MINUTE;
        if missing > 0 {
            if ends_present && missing <= tolerated_missing {
                // At most 100, a week's 1%, so it fits.
                loaded.tolerated.push((open_time, missing as u32));
            } else if !include_incomplete {
                loaded.dropped.push(open_time);
                continue;
            }
        }
        let mut bar = Candle { open_time, ..first };
        for m in &bucket[1..] {
            bar.high = bar.high.max(m.high);
            bar.low = bar.low.min(m.low);
            bar.close = m.close;
            bar.volume += m.volume;
            bar.quote_volume += m.quote_volume;
        }
        loaded.candles.push(bar);
    }
    loaded
}

/// How many minutes a bucket of `timeframe` may miss and still be tolerated.
fn tolerated_missing(timeframe: Timeframe) -> i64 {
    timeframe.duration_ms() / MINUTE * TOLERATED_MISSING_PERCENT / 100
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_percent_rounded_down_is_tolerated() {
        let tolerated: Vec<i64> = Timeframe::ALL.into_iter().map(tolerated_missing).collect();
        // 1m, 5m, 15m, 30m, 1h, 4h, 1d, 1w.
        assert_eq!(tolerated, [0, 0, 0, 0, 0, 2, 14, 100]);
    }
}
