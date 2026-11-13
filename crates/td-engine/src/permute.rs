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
    permute_quotes(bars, &[], seed).map(|(bars, _)| bars)
}

/// [`permute_bars`] for a market with quotes: `asks` is empty or holds each bar's ask, one per bar
/// in the same order. The bars come back exactly as [`permute_bars`] gives them for the same seed.
///
/// Each rebuilt bar's ask keeps the spread of its time slot, not of the bar whose move it took:
/// the spread is a property of the hour (thin hours are wide, busy ones tight), and a strategy
/// that trades only in a tight hour must pay that hour's spread on every copy too, or it would
/// beat the copies on cost alone. At each of its four prices the ask is the rebuilt bar's price
/// times the original ask's price over the original bar's at the same position, and its high and
/// low are then widened to hold its open and close, as R4's widened asks are. The first bar and
/// its ask are unchanged. Because every original ask is at or above its bar and rounding is
/// monotonic, every rebuilt ask is at or above its rebuilt bar at all four prices.
///
/// Panics if `asks` is neither empty nor as long as `bars`: the caller pairs them.
pub(crate) fn permute_quotes(
    bars: &[Candle],
    asks: &[Candle],
    seed: u64,
) -> Result<(Vec<Candle>, Vec<Candle>), CoreError> {
    assert!(
        asks.is_empty() || asks.len() == bars.len(),
        "one ask per bar, or none"
    );
    for b in bars.iter().chain(asks) {
        b.validate()?;
    }
    if bars.len() < 3 {
        return Ok((bars.to_vec(), asks.to_vec()));
    }
    // order[k] is the source bar of rebuilt bar k + 1.
    let mut order: Vec<usize> = (1..bars.len()).collect();
    let mut rng = SplitMix64(seed);
    for i in (1..order.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        order.swap(i, j);
    }
    // `like` scaled as the source bar `b` is when it is rebuilt to open at `open`.
    let scaled = |like: &Candle, b: &Candle, open: f64, open_time| Candle {
        open_time,
        open: open * (like.open / b.open),
        high: open * (like.high / b.open),
        low: open * (like.low / b.open),
        close: open * (like.close / b.open),
        volume: like.volume,
        quote_volume: open * (like.quote_volume / b.open),
    };
    let mut out = Vec::with_capacity(bars.len());
    let mut out_asks = Vec::with_capacity(asks.len());
    out.push(bars[0]);
    out_asks.extend(asks.first().copied());
    let mut prev_close = bars[0].close;
    for (k, (slot, &src)) in bars[1..].iter().zip(&order).enumerate() {
        let b = &bars[src];
        let open = prev_close * (b.open / bars[src - 1].close);
        // `open * (b.open / b.open)` is `open` exactly.
        let bar = scaled(b, b, open, slot.open_time);
        bar.validate()?;
        if let Some(slot_ask) = asks.get(k + 1) {
            // The slot's own spread, as a share of each price, on the rebuilt bar.
            let (open, close) = (
                bar.open * (slot_ask.open / slot.open),
                bar.close * (slot_ask.close / slot.close),
            );
            let ask = Candle {
                open,
                high: (bar.high * (slot_ask.high / slot.high))
                    .max(open)
                    .max(close),
                low: (bar.low * (slot_ask.low / slot.low)).min(open).min(close),
                close,
                ..bar
            };
            ask.validate()?;
            out_asks.push(ask);
        }
        prev_close = bar.close;
        out.push(bar);
    }
    Ok((out, out_asks))
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

    /// On a market with quotes the bars are exactly the ones `permute_bars` gives, each ask stays
    /// at or above its bar, and it has the spread of its time slot as a share of the price, not
    /// the spread of the bar whose move it took.
    #[test]
    fn asks_keep_their_slots_spread_and_stay_at_or_above_their_bars() {
        let bars = path();
        // Spreads that differ per bar and per price, from 0 to about 0.3%.
        let asks: Vec<Candle> = bars
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let s = (i % 5) as f64 * 0.0007;
                Candle {
                    open: b.open * (1.0 + s),
                    high: b.high * (1.0 + 2.0 * s),
                    low: b.low * (1.0 + s / 2.0),
                    close: b.close * (1.0 + s),
                    ..*b
                }
            })
            .collect();
        for seed in 0..20 {
            let (p, pa) = permute_quotes(&bars, &asks, seed).unwrap();
            assert_eq!(p, permute_bars(&bars, seed).unwrap(), "seed {seed}");
            assert_eq!(pa.len(), p.len());
            for (k, (b, a)) in p.iter().zip(&pa).enumerate() {
                assert_eq!(a.open_time, b.open_time);
                assert!(
                    a.open >= b.open && a.high >= b.high && a.low >= b.low && a.close >= b.close,
                    "seed {seed} bar {k}: {b:?} {a:?}"
                );
                a.validate().unwrap();
                // The slot's own spread share at the open and close; the source bar, found by its
                // volume, has a different one unless both spreads happen to match.
                let share = |a: &Candle, b: &Candle| (a.open / b.open, a.close / b.close);
                let (open, close) = share(a, b);
                let (slot_open, slot_close) = share(&asks[k], &bars[k]);
                assert!(
                    rel_close(open, slot_open) && rel_close(close, slot_close),
                    "seed {seed} bar {k}"
                );
                let src = b.volume as usize - 1;
                if src % 5 != k % 5 {
                    assert!(
                        !rel_close(close, share(&asks[src], &bars[src]).1),
                        "bar {k}"
                    );
                }
            }
        }
        assert_eq!(
            permute_quotes(&bars, &[], 3).unwrap().1,
            Vec::<Candle>::new()
        );
    }
}
