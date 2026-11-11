use td_core::{Candle, InstrumentKind, Millis, Timeframe};

use crate::time::MINUTE;

/// How much of a bucket may be missing, in percent of its open minutes, for its bar to still be
/// built as normal, provided its first and last open minutes are there. The count is rounded down,
/// so for a market that never closes 1m to 1h bars tolerate no missing minute, 4h bars 2, 1d bars
/// 14 and 1w bars 100. Venue archives have short outage gaps, and dropping a whole daily bar for
/// one missing minute would distort a daily backtest more than a bar built from 99% of its minutes
/// does.
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

/// Builds bars of `timeframe` from 1-minute candles of a market of `kind`, sorted by open time with
/// no duplicates.
///
/// Each bar opens at its [`Timeframe::floor`] bucket and takes the open of its first minute, the
/// highest high, the lowest low, the close of its last minute and the summed volumes. Missing
/// minutes are not filled. A bucket's expected minutes are its open minutes, by the kind's trading
/// hours ([`InstrumentKind::closed_spans`]): for a market that never closes that is every minute,
/// and a 4h bucket across the FX weekend's close expects only the minutes before it. A bucket with
/// every open minute is complete. A bucket whose first and last open minutes are there and that
/// misses no more than 1% of its open minutes, rounded down, is tolerated: its bar is built as
/// normal and listed in `tolerated`. Any other bucket is incomplete: it is left out and listed in
/// `dropped`, unless `include_incomplete` is set, in which case its bar is built as normal and not
/// listed. A bucket with no minutes at all, such as one wholly inside a market's closed hours,
/// gives no bar and is not listed.
///
/// Ask candles resample by the same rule: given the asks of the same minutes, the same buckets are
/// built, dropped and tolerated, and each ask bar is the exact OHLC of its minutes.
pub fn resample(
    kind: InstrumentKind,
    minutes: &[Candle],
    timeframe: Timeframe,
    include_incomplete: bool,
) -> LoadedCandles {
    let mut loaded = LoadedCandles::default();
    for bucket in
        minutes.chunk_by(|a, b| timeframe.floor(a.open_time) == timeframe.floor(b.open_time))
    {
        let (first, last) = (bucket[0], bucket[bucket.len() - 1]);
        let open_time = timeframe.floor(first.open_time);
        let open = open_minutes(kind, open_time, open_time + timeframe.duration_ms());
        let missing = open.count - bucket.len() as i64;
        let ends_present = Some(first.open_time) == open.first && Some(last.open_time) == open.last;
        if missing > 0 {
            if ends_present && missing <= open.count * TOLERATED_MISSING_PERCENT / 100 {
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

/// The open minutes of `[from, to)`, both on the minute: how many, and the first and last.
struct OpenMinutes {
    count: i64,
    first: Option<Millis>,
    last: Option<Millis>,
}

fn open_minutes(kind: InstrumentKind, from: Millis, to: Millis) -> OpenMinutes {
    let closed = kind.closed_spans(from, to);
    let closed_ms: Millis = closed.iter().map(|s| s.end - s.start).sum();
    let count = (to - from - closed_ms) / MINUTE;
    if count <= 0 {
        return OpenMinutes {
            count: 0,
            first: None,
            last: None,
        };
    }
    // The spans are in order and clipped to the range, so only the first can start at `from` and
    // only the last can end at `to`.
    let first = match closed.first() {
        Some(s) if s.start == from => s.end,
        _ => from,
    };
    let last = match closed.last() {
        Some(s) if s.end == to => s.start,
        _ => to,
    } - MINUTE;
    OpenMinutes {
        count,
        first: Some(first),
        last: Some(last),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many minutes a bucket of `timeframe` may miss and still be tolerated, in a market that
    /// never closes.
    fn tolerated_missing(timeframe: Timeframe) -> i64 {
        open_minutes(InstrumentKind::Spot, 0, timeframe.duration_ms()).count
            * TOLERATED_MISSING_PERCENT
            / 100
    }

    #[test]
    fn one_percent_rounded_down_is_tolerated() {
        let tolerated: Vec<i64> = Timeframe::ALL.into_iter().map(tolerated_missing).collect();
        // 1m, 5m, 15m, 30m, 1h, 4h, 1d, 1w.
        assert_eq!(tolerated, [0, 0, 0, 0, 0, 2, 14, 100]);
    }
}
