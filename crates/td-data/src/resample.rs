use td_core::{Candle, Timeframe};

use crate::time::MINUTE;

/// Builds bars of `timeframe` from 1-minute candles sorted by open time with no duplicates.
///
/// Each bar opens at its [`Timeframe::floor`] bucket and takes the open of its first minute, the
/// highest high, the lowest low, the close of its last minute and the summed volumes. Minutes
/// missing inside a bucket are not filled. The last bar is incomplete when the bucket's final
/// minute is missing; it is left out unless `include_incomplete` is set.
pub fn resample(minutes: &[Candle], timeframe: Timeframe, include_incomplete: bool) -> Vec<Candle> {
    let mut bars: Vec<Candle> = Vec::new();
    let mut last_minute = None;
    for m in minutes {
        let bucket = timeframe.floor(m.open_time);
        match bars.last_mut() {
            Some(bar) if bar.open_time == bucket => {
                bar.high = bar.high.max(m.high);
                bar.low = bar.low.min(m.low);
                bar.close = m.close;
                bar.volume += m.volume;
                bar.quote_volume += m.quote_volume;
            }
            _ => bars.push(Candle {
                open_time: bucket,
                ..*m
            }),
        }
        last_minute = Some(m.open_time);
    }
    if let (Some(bar), Some(last)) = (bars.last(), last_minute) {
        let complete = last == bar.open_time + timeframe.duration_ms() - MINUTE;
        if !complete && !include_incomplete {
            bars.pop();
        }
    }
    bars
}
