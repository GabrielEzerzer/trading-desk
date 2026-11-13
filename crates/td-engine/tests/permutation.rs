//! The luck test by bar permutation end to end: a planted edge gives a small p-value and noise
//! does not, results are reproducible for a seed at any thread count, and a cancel stops early
//! with no p-value.

mod common;

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicUsize, Ordering};

use td_core::{Candle, Millis, Timeframe};
use td_engine::{
    BacktestConfig, Direction, MAX_RUNS, Metric, Objective, ParamAxis, ParamSpace, ParamValue,
    Params, PermutationSpec, SweepMode, SweepSpec, TesterError, permutation_test,
};

const HOUR: Millis = 3_600_000;
/// 2024-01-01 00:00 UTC.
const T0: Millis = 1_704_067_200_000;
const BARS: i64 = 600;

/// A small xorshift generator for the synthetic noise, uniform in `[-0.5, 0.5)`.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64 - 0.5
    }
}

/// Hourly bars from a list of closes, each opening at the previous close.
fn bars_from(closes: &[f64]) -> Vec<Candle> {
    closes
        .iter()
        .enumerate()
        .map(|(i, &close)| {
            let open = if i == 0 { close } else { closes[i - 1] };
            Candle {
                open_time: T0 + i as i64 * HOUR,
                open,
                high: open.max(close) * 1.001,
                low: open.min(close) * 0.999,
                close,
                volume: 10.0,
                quote_volume: 10.0 * close,
            }
        })
        .collect()
}

/// A triangle wave with period 1 between -1 and 1.
fn tri(x: f64) -> f64 {
    let f = x - x.floor();
    4.0 * (f - 0.5).abs() - 1.0
}

/// A planted edge: long up and down legs of 60 bars (a triangle wave that ends where it starts)
/// with a little noise. A moving-average cross that goes short rides both legs; shuffled, the
/// same moves come in random order and the legs are gone.
fn trending_bars() -> Vec<Candle> {
    let mut noise = Noise(0x5EED_1234);
    let closes: Vec<f64> = (0..BARS)
        .map(|i| 100.0 + 10.0 * tri(i as f64 / 120.0) + 0.6 * noise.next())
        .collect();
    bars_from(&closes)
}

/// Pure noise: a random walk of independent hourly moves of up to 0.5%, so the order of the bars
/// carries nothing to find.
fn noise_bars(seed: u64) -> Vec<Candle> {
    let mut noise = Noise(seed);
    let mut close = 100.0;
    let closes: Vec<f64> = (0..BARS)
        .map(|_| {
            close *= 1.0 + 0.01 * noise.next();
            close
        })
        .collect();
    bars_from(&closes)
}

fn ints(v: &[i64]) -> ParamSpace {
    ParamSpace::Values {
        values: v.iter().map(|&x| ParamValue::Int(x)).collect(),
    }
}

/// SMA cross with shorts on a perpetual, 3 x 2 = 6 parameter sets, by total return.
fn spec(permutations: usize, seed: u64) -> PermutationSpec {
    let base = BacktestConfig {
        strategy: "sma_cross".into(),
        params: Params::new().with("allow_short", ParamValue::Bool(true)),
        instrument: common::perp(),
        timeframe: Timeframe::H1,
        from: T0,
        to: T0 + BARS * HOUR,
        starting_cash: 10_000.0,
        slippage_bps: 2.0,
        fee_override: None,
        max_leverage: 1.0,
        warmup_from: None,
        close_at_end: false,
        account_currency: td_core::AccountCurrency::Quote,
    };
    PermutationSpec {
        sweep: SweepSpec {
            base,
            axes: vec![
                ParamAxis {
                    name: "fast".into(),
                    space: ints(&[3, 5, 8]),
                },
                ParamAxis {
                    name: "slow".into(),
                    space: ints(&[20, 40]),
                },
            ],
            mode: SweepMode::Grid,
            objective: Objective {
                metric: Metric::TotalReturn,
                direction: Direction::Maximize,
            },
            min_trades: 1,
        },
        permutations,
        seed,
    }
}

fn pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap()
}

#[test]
fn a_planted_edge_beats_every_shuffled_copy_and_noise_does_not() {
    let edge = permutation_test(&spec(19, 7), &trending_bars(), &[], &[], None).unwrap();
    assert_eq!(edge.permuted_best.len(), 19);
    assert!(edge.real_best > 0.0, "{edge:?}");
    // Every copy qualifies and none comes close: p is the smallest 19 copies allow.
    assert_eq!(edge.qualified_copies, 19, "{edge:?}");
    assert_eq!(edge.at_least_as_good, 0, "{edge:?}");
    assert_eq!(edge.p_value, 0.05);

    let noise = permutation_test(&spec(19, 7), &noise_bars(42), &[], &[], None).unwrap();
    // Pinned for the seeds: the real best is in the middle of the copies' bests.
    assert_eq!(noise.at_least_as_good, PINNED_NOISE_COUNT, "{noise:?}");
    assert_eq!(noise.p_value, (1 + PINNED_NOISE_COUNT) as f64 / 20.0);
    assert!(noise.p_value > 0.2, "{noise:?}");
}

const PINNED_NOISE_COUNT: usize = 6;

#[test]
fn the_p_value_counts_copies_at_least_as_good_as_the_real_best() {
    let r = permutation_test(&spec(9, 3), &noise_bars(42), &[], &[], None).unwrap();
    let qualified = r.permuted_best.iter().flatten().count();
    let count = r
        .permuted_best
        .iter()
        .flatten()
        .filter(|&&x| x >= r.real_best)
        .count();
    assert_eq!(r.qualified_copies, qualified);
    assert_eq!(r.at_least_as_good, count);
    assert_eq!(r.p_value, (1 + count) as f64 / (1 + qualified) as f64);
    // The real best is the plain sweep's best.
    let swept = td_engine::sweep(&r.spec.sweep, &noise_bars(42), &[], &[], None).unwrap();
    let top = swept.rows[0].metrics.unwrap().total_return.unwrap();
    assert_eq!(r.real_best, top);

    // Minimising, at least as good means at or below the real best.
    let mut s = spec(9, 3);
    s.sweep.objective = Objective {
        metric: Metric::MaxDrawdown,
        direction: Direction::Minimize,
    };
    let r = permutation_test(&s, &noise_bars(42), &[], &[], None).unwrap();
    let count = r
        .permuted_best
        .iter()
        .flatten()
        .filter(|&&x| x <= r.real_best)
        .count();
    assert_eq!(r.at_least_as_good, count);
    assert!(count < 9, "{r:?}");
}

#[test]
fn a_seed_gives_the_same_result_at_any_thread_count_and_another_seed_differs() {
    let bars = noise_bars(9);
    let serial = pool(1).install(|| permutation_test(&spec(5, 11), &bars, &[], &[], None).unwrap());
    let parallel =
        pool(4).install(|| permutation_test(&spec(5, 11), &bars, &[], &[], None).unwrap());
    assert_eq!(serial, parallel);
    let other = permutation_test(&spec(5, 12), &bars, &[], &[], None).unwrap();
    assert_eq!(other.real_best, serial.real_best);
    assert_ne!(other.permuted_best, serial.permuted_best);
}

#[test]
fn progress_counts_every_run_once_and_a_break_cancels_with_no_p_value() {
    let bars = noise_bars(5);
    // 6 runs per sweep, the real sweep and 4 copies: 30 in all.
    let calls = AtomicUsize::new(0);
    let max_done = AtomicUsize::new(0);
    let counting = |done: usize, total: usize| {
        assert_eq!(total, 30);
        calls.fetch_add(1, Ordering::Relaxed);
        max_done.fetch_max(done, Ordering::Relaxed);
        ControlFlow::Continue(())
    };
    let full = pool(2).install(|| permutation_test(&spec(4, 1), &bars, &[], &[], Some(&counting)));
    assert_eq!(calls.load(Ordering::Relaxed), 30);
    assert_eq!(max_done.load(Ordering::Relaxed), 30);
    assert_eq!(
        full.unwrap(),
        permutation_test(&spec(4, 1), &bars, &[], &[], None).unwrap()
    );

    // Stop during the second copy's sweep: on one thread nothing runs after the break.
    let calls = AtomicUsize::new(0);
    let stop = |done: usize, _: usize| {
        calls.fetch_add(1, Ordering::Relaxed);
        if done >= 15 {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let r = pool(1).install(|| permutation_test(&spec(4, 1), &bars, &[], &[], Some(&stop)));
    assert_eq!(r, Err(TesterError::Cancelled));
    assert_eq!(calls.load(Ordering::Relaxed), 15);
}

#[test]
fn bad_specs_are_refused_before_running() {
    let bars = noise_bars(5);
    assert!(matches!(
        permutation_test(&spec(0, 1), &bars, &[], &[], None),
        Err(TesterError::InvalidSpec(_))
    ));
    // 6 runs per sweep: the real sweep and the copies together may not pass the run limit.
    let copies = MAX_RUNS / 6;
    assert_eq!(
        permutation_test(&spec(copies, 1), &bars, &[], &[], None),
        Err(TesterError::TooManyRuns {
            runs: 6 * (copies as u128 + 1),
            limit: MAX_RUNS,
        })
    );
    // A real sweep with no qualifying run has nothing to test.
    let mut never = spec(3, 1);
    never.sweep.min_trades = 10_000;
    assert_eq!(
        permutation_test(&never, &bars, &[], &[], None),
        Err(TesterError::NoQualifyingRun {
            min_trades: 10_000,
            window: None,
        })
    );
}
