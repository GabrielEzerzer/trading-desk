//! Property tests on resampling: every bucket a run of minutes touches is built, tolerated or
//! dropped by the 1% rule, and a built bar is the exact OHLCV of its minutes.

use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use td_core::{Candle, InstrumentKind, Millis, Timeframe};
use td_data::resample;
use td_data::time::MINUTE;

fn config() -> Config {
    Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(0x7d_da_7a),
        failure_persistence: None,
        ..Config::default()
    }
}

fn minute(open_time: Millis, price: u16, spread: u8, volume: u16) -> Candle {
    let low = f64::from(price) + 1.0;
    Candle {
        open_time,
        open: low + f64::from(spread) / 2.0,
        high: low + f64::from(spread),
        low,
        close: low + f64::from(spread) / 3.0,
        volume: f64::from(volume),
        quote_volume: f64::from(volume) * 2.0,
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn each_bucket_is_built_tolerated_or_dropped_and_a_bar_is_its_minutes(
        timeframe in prop::sample::select(Timeframe::ALL[..7].to_vec()),
        start in 28_000_000..29_000_000i64,
        bars in prop::collection::vec((any::<u16>(), any::<u8>(), any::<u16>(), 0..100u8), 1..3_000),
        gap_percent in prop_oneof![Just(0u8), 1..3u8],
        include_incomplete in any::<bool>(),
    ) {
        let minutes: Vec<Candle> = bars
            .iter()
            .enumerate()
            .filter(|(_, b)| b.3 >= gap_percent)
            .map(|(i, b)| minute((start + i as i64) * MINUTE, b.0, b.1, b.2))
            .collect();
        prop_assume!(!minutes.is_empty());
        let loaded = resample(InstrumentKind::Spot, &minutes, timeframe, include_incomplete);

        let mut buckets: BTreeMap<Millis, Vec<Candle>> = BTreeMap::new();
        for m in &minutes {
            buckets.entry(timeframe.floor(m.open_time)).or_default().push(*m);
        }
        let expected = timeframe.duration_ms() / MINUTE;
        let (mut bars_out, mut dropped, mut tolerated) = (Vec::new(), Vec::new(), Vec::new());
        for (&open_time, ms) in &buckets {
            let missing = expected - ms.len() as i64;
            let ends = ms[0].open_time == open_time
                && ms[ms.len() - 1].open_time == open_time + timeframe.duration_ms() - MINUTE;
            if missing > 0 && ends && missing <= expected / 100 {
                tolerated.push((open_time, missing as u32));
            } else if missing > 0 && !include_incomplete {
                dropped.push(open_time);
                continue;
            }
            bars_out.push(Candle {
                open_time,
                open: ms[0].open,
                high: ms.iter().map(|m| m.high).fold(f64::MIN, f64::max),
                low: ms.iter().map(|m| m.low).fold(f64::MAX, f64::min),
                close: ms[ms.len() - 1].close,
                volume: ms.iter().map(|m| m.volume).sum(),
                quote_volume: ms.iter().map(|m| m.quote_volume).sum(),
            });
        }
        prop_assert_eq!(&loaded.candles, &bars_out);
        prop_assert_eq!(&loaded.dropped, &dropped);
        prop_assert_eq!(&loaded.tolerated, &tolerated);

        let total: f64 = minutes.iter().map(|m| m.volume).sum();
        let kept: f64 = loaded.candles.iter().map(|c| c.volume).sum();
        let lost: f64 = dropped.iter().flat_map(|t| &buckets[t]).map(|m| m.volume).sum();
        prop_assert_eq!(kept + lost, total);
    }
}
