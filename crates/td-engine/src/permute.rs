//! Bar permutation for the luck test (R3 in #35): a copy of the bars with the order of their moves
//! shuffled, so any structure in time a strategy could exploit is gone while each bar's own shape
//! and the overall drift stay.

use td_core::{Candle, CoreError};

use crate::rng::SplitMix64;

/// Shuffles `bars` with a seeded PRNG and rebuilds them as a new price path.
///
/// Every bar after the first is described relative to the bar before it: its gap (open over the
/// previous close) and its shape (high, low and close over its own open). Those descriptions are
/// shuffled together, one bar at a time, with a Fisher-Yates shuffle, and the path is rebuilt from
/// the first bar: each bar opens at the previous rebuilt close times its gap, and its high, low
/// and close are its open times its shape.
///
/// Preserved, up to the rounding of the rebuild:
/// - the first bar, exactly, and every bar's open time, exactly (only prices move);
/// - the multiset of close-to-close returns, since each bar keeps its gap with its shape;
/// - the multisets of gaps and of bar shapes (each bar's range, body and wicks, relative to its
///   open);
/// - the last close, since the product of all close-to-close returns is unchanged;
/// - each bar's volume, and its quote volume relative to its open (the bar's average traded
///   price keeps its place in the bar's range).
///
/// Destroyed: the order of the moves, and with it trends, mean reversion, volatility clusters
/// and anything else that depends on time. Funding is time-based and not part of the bars, so a
/// caller leaves it as it is.
///
/// The variant that shuffles gaps and shapes separately would also break the pairing of a bar's
/// gap with its body, but then the close-to-close returns are no longer the same multiset. On
/// 24/7 crypto data a bar opens at the previous close, every gap is 1, and the two variants are
/// the same, so this one keeps the stronger guarantee.
///
/// The rebuild works in price ratios with `*` and `/` only, never `ln` and `exp`, whose results
/// depend on the platform's maths library: a seed gives bit-identical bars on every platform.
/// Because rounding is monotonic, a valid input bar rebuilds to a valid bar (high at or above
/// open and close, low at or below them and above zero); the result is still checked.
///
/// Fewer than three bars leave nothing to shuffle and come back unchanged. Each input bar must
/// pass [`Candle::validate`].
pub fn permute_bars(bars: &[Candle], seed: u64) -> Result<Vec<Candle>, CoreError> {
    for b in bars {
        b.validate()?;
    }
    if bars.len() < 3 {
        return Ok(bars.to_vec());
    }
    // order[k] is the source bar of rebuilt bar k + 1.
    let mut order: Vec<usize> = (1..bars.len()).collect();
    let mut rng = SplitMix64(seed);
    for i in (1..order.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        order.swap(i, j);
    }
    let mut out = Vec::with_capacity(bars.len());
    out.push(bars[0]);
    let mut prev_close = bars[0].close;
    for (slot, &src) in bars[1..].iter().zip(&order) {
        let b = bars[src];
        let open = prev_close * (b.open / bars[src - 1].close);
        let bar = Candle {
            open_time: slot.open_time,
            open,
            high: open * (b.high / b.open),
            low: open * (b.low / b.open),
            close: open * (b.close / b.open),
            volume: b.volume,
            quote_volume: open * (b.quote_volume / b.open),
        };
        bar.validate()?;
        prev_close = bar.close;
        out.push(bar);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bar with a gap from the close before it, as real data can have after a missing bucket.
    fn bar(i: i64, open: f64, high: f64, low: f64, close: f64) -> Candle {
        Candle {
            open_time: 1_000 + i * 60_000,
            open,
            high,
            low,
            close,
            volume: i as f64 + 1.0,
            quote_volume: (i as f64 + 1.0) * (low + high) / 2.0,
        }
    }

    /// Sixty bars of a wandering path with gaps, uneven wicks and a few flat bars.
    fn path() -> Vec<Candle> {
        let mut rng = SplitMix64(99);
        let mut close = 100.0;
        (0..60)
            .map(|i| {
                let gap = if i % 7 == 3 {
                    1.0 + (rng.unit() - 0.5) * 0.01
                } else {
                    1.0
                };
                let open = close * gap;
                close = open * (1.0 + (rng.unit() - 0.5) * 0.04);
                if i % 11 == 5 {
                    close = open;
                }
                let high = open.max(close) * (1.0 + rng.unit() * 0.01);
                let low = open.min(close) * (1.0 - rng.unit() * 0.01);
                bar(i, open, high, low, close)
            })
            .collect()
    }

    fn close_to_close(bars: &[Candle]) -> Vec<f64> {
        let mut r: Vec<f64> = bars.windows(2).map(|w| w[1].close / w[0].close).collect();
        r.sort_by(f64::total_cmp);
        r
    }

    fn rel_close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-12 * a.abs().max(b.abs())
    }

    #[test]
    fn permuted_bars_are_valid_keep_the_times_and_the_moves_and_end_where_the_original_does() {
        let bars = path();
        let p = permute_bars(&bars, 7).unwrap();
        assert_eq!(p.len(), bars.len());
        assert_eq!(p[0], bars[0]);
        for (a, b) in p.iter().zip(&bars) {
            assert_eq!(a.open_time, b.open_time);
            a.validate().unwrap();
        }
        assert!(rel_close(
            p.last().unwrap().close,
            bars.last().unwrap().close
        ));
        // Same multiset of close-to-close returns.
        let (got, want) = (close_to_close(&p), close_to_close(&bars));
        assert!(got.iter().zip(&want).all(|(a, b)| rel_close(*a, *b)));
        // Same multiset of bar shapes (high, low, close over open) and volumes.
        let shapes = |bars: &[Candle]| {
            let mut s: Vec<[f64; 4]> = bars[1..]
                .iter()
                .map(|b| [b.volume, b.high / b.open, b.low / b.open, b.close / b.open])
                .collect();
            s.sort_by(|x, y| x[0].total_cmp(&y[0]));
            s
        };
        for (a, b) in shapes(&p).iter().zip(&shapes(&bars)) {
            assert_eq!(a[0], b[0]);
            assert!((1..4).all(|k| rel_close(a[k], b[k])), "{a:?} {b:?}");
        }
        // And the order really changed.
        let moved = p
            .iter()
            .zip(&bars)
            .filter(|(a, b)| a.volume != b.volume)
            .count();
        assert!(moved > 50, "{moved}");
    }

    #[test]
    fn a_seed_gives_identical_bars_and_another_seed_other_bars() {
        let bars = path();
        assert_eq!(permute_bars(&bars, 3), permute_bars(&bars, 3));
        assert_ne!(permute_bars(&bars, 3), permute_bars(&bars, 4));
    }

    #[test]
    fn the_shuffle_is_pinned_for_a_seed() {
        // The source bar of each rebuilt bar, read back from the volumes, for seed 1 on 8 bars.
        let bars: Vec<Candle> = path().into_iter().take(8).collect();
        let p = permute_bars(&bars, 1).unwrap();
        let sources: Vec<usize> = p.iter().map(|b| b.volume as usize - 1).collect();
        assert_eq!(sources, PINNED_ORDER);
    }

    const PINNED_ORDER: [usize; 8] = [0, 6, 7, 5, 4, 1, 2, 3];

    #[test]
    fn short_series_come_back_unchanged_and_invalid_bars_are_errors() {
        let bars = path();
        assert_eq!(permute_bars(&bars[..2], 5).unwrap(), bars[..2]);
        assert!(permute_bars(&[], 5).unwrap().is_empty());
        let mut bad = bars.clone();
        bad[10].high = bad[10].open.min(bad[10].close) * 0.5;
        assert!(matches!(
            permute_bars(&bad, 5),
            Err(CoreError::InvalidCandle { .. })
        ));
    }
}
