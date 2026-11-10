//! The strategy tester end to end: sweeps with a known best, validation, reproducible random
//! sampling, parallel against serial, out-of-sample warm-up, walk-forward windows and stitching,
//! and saved tester results.

mod common;

use std::ops::ControlFlow;
use std::sync::Mutex;

use td_core::{Candle, Millis, Timeframe};
use td_engine::{
    BacktestConfig, Direction, EngineError, MAX_RUNS, Metric, Objective, OosSpec, ParamAxis,
    ParamSpace, ParamValue, Params, RunKind, SavedRun, Split, SweepMode, SweepSpec, TesterError,
    WalkForwardSpec, WindowMode, WindowSpec, config_fingerprint, list, load, out_of_sample, run,
    save, sweep, walk_forward,
};

const HOUR: Millis = 3_600_000;
/// 2024-01-01 00:00 UTC.
const T0: Millis = 1_704_067_200_000;

fn candle(i: i64, open: f64, close: f64) -> Candle {
    Candle {
        open_time: T0 + i * HOUR,
        open,
        high: open.max(close) + 0.25,
        low: open.min(close) - 0.25,
        close,
        volume: 10.0,
        quote_volume: 10.0 * close,
    }
}

/// Hourly bars on a straight rising line, each opening at the previous close.
fn rising_bars(n: i64) -> Vec<Candle> {
    let price = |i: i64| 100.0 + 0.5 * i as f64;
    (0..n)
        .map(|i| candle(i, if i == 0 { 99.8 } else { price(i - 1) }, price(i)))
        .collect()
}

/// A triangle wave with period 1 between -1 and 1, exact on every platform.
fn tri(x: f64) -> f64 {
    let f = x - x.floor();
    4.0 * (f - 0.5).abs() - 1.0
}

/// Hourly bars: a rising trend with three triangle waves on top, so different SMA pairs do
/// differently.
fn wavy_bars(n: i64) -> Vec<Candle> {
    let price = |i: i64| {
        let x = i as f64;
        100.0 + 0.01 * x + 5.0 * tri(x / 96.0) + 2.0 * tri(x / 13.0) + 1.2 * tri(x / 4.7)
    };
    (0..n)
        .map(|i| {
            candle(
                i,
                if i == 0 { price(0) - 0.2 } else { price(i - 1) },
                price(i),
            )
        })
        .collect()
}

fn base(instrument: td_core::Instrument, bars: i64) -> BacktestConfig {
    BacktestConfig {
        strategy: "sma_cross".into(),
        params: Params::new(),
        instrument,
        timeframe: Timeframe::H1,
        from: T0,
        to: T0 + bars * HOUR,
        starting_cash: 10_000.0,
        slippage_bps: 0.0,
        fee_override: None,
        max_leverage: 1.0,
        warmup_from: None,
        close_at_end: false,
    }
}

fn values(name: &str, v: &[ParamValue]) -> ParamAxis {
    ParamAxis {
        name: name.into(),
        space: ParamSpace::Values { values: v.to_vec() },
    }
}

fn range(name: &str, from: f64, to: f64, step: f64) -> ParamAxis {
    ParamAxis {
        name: name.into(),
        space: ParamSpace::Range { from, to, step },
    }
}

fn ints(v: &[i64]) -> Vec<ParamValue> {
    v.iter().map(|&x| ParamValue::Int(x)).collect()
}

fn floats(v: &[f64]) -> Vec<ParamValue> {
    v.iter().map(|&x| ParamValue::Float(x)).collect()
}

fn spec(base: BacktestConfig, axes: Vec<ParamAxis>) -> SweepSpec {
    SweepSpec {
        base,
        axes,
        mode: SweepMode::Grid,
        objective: Objective {
            metric: Metric::TotalReturn,
            direction: Direction::Maximize,
        },
        min_trades: 0,
    }
}

/// Long-only SMA cross on a rising line with fast fixed at 1, so the strategy enters at bar
/// `slow - 1` and never exits.
fn rising_spec() -> SweepSpec {
    let mut b = base(common::spot(), 300);
    b.params = Params::new().with("fast", ParamValue::Int(1));
    spec(
        b,
        vec![
            values("slow", &ints(&[20, 5, 10])),
            values("equity_fraction", &floats(&[0.5, 0.9])),
        ],
    )
}

/// SMA cross with shorts on a perpetual over the wavy series: 6 x 4 x 2 = 48 runs.
fn wavy_spec() -> SweepSpec {
    let mut b = base(common::perp(), 500);
    b.slippage_bps = 5.0;
    spec(
        b,
        vec![
            range("fast", 3.0, 8.0, 1.0),
            values("slow", &ints(&[12, 18, 24, 30])),
            ParamAxis {
                name: "allow_short".into(),
                space: ParamSpace::Bool,
            },
        ],
    )
}

fn pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap()
}

fn int(p: &Params, name: &str) -> i64 {
    p.int(name).unwrap()
}

#[test]
fn the_sweep_finds_the_best_parameters_known_by_construction() {
    // On a rising line an earlier entry and a larger position both earn more, so the best set
    // is the shortest slow average with the largest fraction, and for each fraction the ranking
    // follows slow ascending. The best set is fourth in grid order, so a tie-break cannot fake it.
    let r = sweep(&rising_spec(), &rising_bars(300), &[], None).unwrap();
    assert_eq!(r.rows.len(), 6);
    let top = &r.rows[0];
    assert_eq!(top.index, 3);
    assert_eq!(int(&top.params, "slow"), 5);
    assert_eq!(top.params.float("equity_fraction"), Ok(0.9));
    assert_eq!(int(&top.params, "fast"), 1);
    let last = r.rows.last().unwrap();
    assert_eq!(int(&last.params, "slow"), 20);
    assert_eq!(last.params.float("equity_fraction"), Ok(0.5));
    for fraction in [0.5, 0.9] {
        let slows: Vec<i64> = r
            .rows
            .iter()
            .filter(|row| row.params.float("equity_fraction") == Ok(fraction))
            .map(|row| int(&row.params, "slow"))
            .collect();
        assert_eq!(slows, [5, 10, 20]);
    }
    // Rows are in objective order.
    let returns: Vec<f64> = r
        .rows
        .iter()
        .map(|row| row.metrics.unwrap().total_return.unwrap())
        .collect();
    assert!(returns.windows(2).all(|w| w[0] >= w[1]), "{returns:?}");
    // The best run in full is the top row run again.
    let best = r.best.as_ref().unwrap();
    assert_eq!(best.config.params, top.params);
    assert_eq!(best.metrics.total_return, top.metrics.unwrap().total_return);
    assert_eq!(best.orders[0].submitted_at, T0 + 4 * HOUR);
    assert_eq!(r.data_fingerprint, best.data_fingerprint);
}

#[test]
fn runs_below_the_minimum_trades_rank_last_and_leave_no_best() {
    // The rising-line runs hold one open trade and close none.
    let mut s = rising_spec();
    s.min_trades = 1;
    let r = sweep(&s, &rising_bars(300), &[], None).unwrap();
    assert!(r.rows.iter().all(|row| !row.qualified));
    // Unqualified rows stay in grid order.
    let order: Vec<u64> = r.rows.iter().map(|row| row.index).collect();
    assert_eq!(order, [0, 1, 2, 3, 4, 5]);
    assert!(r.best.is_none());
}

#[test]
fn parameter_sets_the_strategy_refuses_become_rows_with_errors() {
    // fast 6 is not below slow 5: the strategy refuses it when building.
    let mut b = base(common::spot(), 300);
    b.params = Params::new().with("slow", ParamValue::Int(5));
    let s = spec(b, vec![values("fast", &ints(&[6, 1, 2]))]);
    let r = sweep(&s, &rising_bars(300), &[], None).unwrap();
    assert_eq!(r.rows.len(), 3);
    let refused = r.rows.last().unwrap();
    assert_eq!(int(&refused.params, "fast"), 6);
    assert!(!refused.qualified);
    assert!(refused.metrics.is_none());
    assert!(refused.error.as_ref().unwrap().contains("slow"));
    assert!(r.rows[..2].iter().all(|row| row.qualified));
}

#[test]
fn oversized_grids_and_invalid_spaces_are_errors() {
    let bars = rising_bars(50);
    let check = |axes: Vec<ParamAxis>, ok: fn(&TesterError) -> bool| {
        let err = sweep(&spec(base(common::spot(), 50), axes), &bars, &[], None).unwrap_err();
        assert!(ok(&err), "{err}");
    };
    // 1000 x 999 runs.
    check(
        vec![
            range("fast", 1.0, 1000.0, 1.0),
            range("slow", 2.0, 1000.0, 1.0),
        ],
        |e| matches!(e, TesterError::TooManyRuns { runs: 999_000, limit } if *limit == MAX_RUNS),
    );
    let invalid_param =
        |e: &TesterError| matches!(e, TesterError::Engine(EngineError::InvalidParam { .. }));
    check(vec![values("nope", &ints(&[1]))], invalid_param);
    check(
        vec![values("equity_fraction", &floats(&[0.5, 1.5]))],
        invalid_param,
    );
    check(
        vec![values("fast", &[ParamValue::Bool(true)])],
        invalid_param,
    );
    // The range's far end is out of bounds.
    check(vec![range("equity_fraction", 0.5, 2.0, 0.5)], invalid_param);
    let invalid_space = |e: &TesterError| matches!(e, TesterError::InvalidSpace { .. });
    check(vec![values("fast", &[])], invalid_space);
    check(vec![range("allow_short", 0.0, 1.0, 1.0)], invalid_space);
    check(
        vec![ParamAxis {
            name: "fast".into(),
            space: ParamSpace::Bool,
        }],
        invalid_space,
    );
    check(vec![range("fast", 1.0, 5.0, 0.5)], invalid_space);
    check(vec![range("fast", 5.0, 1.0, 1.0)], invalid_space);
    check(vec![range("fast", 1.0, 5.0, 0.0)], invalid_space);
    check(
        vec![values("fast", &ints(&[1])), values("fast", &ints(&[2]))],
        invalid_space,
    );

    let mut fixed = spec(base(common::spot(), 50), vec![values("fast", &ints(&[1]))]);
    fixed.base.params = Params::new().with("fast", ParamValue::Int(2));
    assert!(matches!(
        sweep(&fixed, &bars, &[], None),
        Err(TesterError::InvalidSpace { .. })
    ));
    let mut s = spec(base(common::spot(), 50), vec![range("fast", 1.0, 5.0, 1.0)]);
    s.mode = SweepMode::Random {
        samples: 0,
        seed: 1,
    };
    assert!(matches!(
        sweep(&s, &bars, &[], None),
        Err(TesterError::InvalidSpec(_))
    ));
    s.mode = SweepMode::Random {
        samples: MAX_RUNS + 1,
        seed: 1,
    };
    assert!(matches!(
        sweep(&s, &bars, &[], None),
        Err(TesterError::TooManyRuns { .. })
    ));
    // Random mode may sample a space larger than the grid limit.
    let mut big = spec(
        base(common::spot(), 50),
        vec![
            range("fast", 1.0, 1000.0, 1.0),
            range("slow", 1001.0, 2000.0, 1.0),
        ],
    );
    big.mode = SweepMode::Random {
        samples: 3,
        seed: 1,
    };
    assert_eq!(sweep(&big, &bars, &[], None).unwrap().rows.len(), 3);
}

#[test]
fn ranges_expand_in_order_and_floats_stop_at_the_end() {
    let mut b = base(common::spot(), 300);
    b.params = Params::new()
        .with("fast", ParamValue::Int(1))
        .with("slow", ParamValue::Int(5));
    let s = spec(b, vec![range("equity_fraction", 0.1, 1.0, 0.3)]);
    let r = sweep(&s, &rising_bars(300), &[], None).unwrap();
    let mut by_index: Vec<(u64, f64)> = r
        .rows
        .iter()
        .map(|row| (row.index, row.params.float("equity_fraction").unwrap()))
        .collect();
    by_index.sort_by_key(|x| x.0);
    let got: Vec<f64> = by_index.iter().map(|x| x.1).collect();
    assert_eq!(got.len(), 4);
    for (g, e) in got.iter().zip([0.1, 0.4, 0.7, 1.0]) {
        assert!((g - e).abs() < 1e-12 && *g <= 1.0, "{got:?}");
    }
}

#[test]
fn random_mode_is_reproducible_with_a_seed() {
    let bars = wavy_bars(500);
    let mut s = spec(
        base(common::perp(), 500),
        vec![
            range("fast", 2.0, 20.0, 1.0),
            range("slow", 21.0, 60.0, 3.0),
        ],
    );
    s.mode = SweepMode::Random {
        samples: 15,
        seed: 7,
    };
    let a = sweep(&s, &bars, &[], None).unwrap();
    let b = sweep(&s, &bars, &[], None).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.rows.len(), 15);
    let mut indices: Vec<u64> = a.rows.iter().map(|r| r.index).collect();
    indices.sort_unstable();
    indices.dedup();
    assert_eq!(indices.len(), 15);
    assert!(indices.iter().all(|&i| i < 19 * 14));

    s.mode = SweepMode::Random {
        samples: 15,
        seed: 8,
    };
    let c = sweep(&s, &bars, &[], None).unwrap();
    let sets = |r: &td_engine::SweepResult| {
        let mut v: Vec<u64> = r.rows.iter().map(|row| row.index).collect();
        v.sort_unstable();
        v
    };
    assert_ne!(sets(&a), sets(&c));
}

#[test]
fn parallel_and_serial_sweeps_give_identical_results() {
    let bars = wavy_bars(500);
    let s = wavy_spec();
    let calls = Mutex::new(Vec::new());
    let progress = |done: usize, total: usize| {
        calls.lock().unwrap().push((done, total));
        ControlFlow::Continue(())
    };
    let serial = pool(1).install(|| sweep(&s, &bars, &[], Some(&progress)).unwrap());
    let parallel = pool(4).install(|| sweep(&s, &bars, &[], None).unwrap());
    assert_eq!(serial.rows.len(), 48);
    assert_eq!(
        serde_json::to_string(&serial).unwrap(),
        serde_json::to_string(&parallel).unwrap()
    );
    // The ranking differs from the grid order, so the test covers a real sort.
    let order: Vec<u64> = serial.rows.iter().map(|r| r.index).collect();
    assert_ne!(order, (0..48).collect::<Vec<_>>());
    let mut calls = calls.into_inner().unwrap();
    calls.sort_unstable();
    assert_eq!(calls, (1..=48).map(|n| (n, 48)).collect::<Vec<_>>());
}

#[test]
fn out_of_sample_warms_up_on_the_in_sample_bars() {
    let bars = rising_bars(300);
    let oos = OosSpec {
        sweep: rising_spec(),
        split: Split::Fraction(0.5),
    };
    let r = out_of_sample(&oos, &bars, &[], None).unwrap();
    assert_eq!(r.split_at, T0 + 150 * HOUR);
    assert_eq!(r.in_sample.spec.base.to, r.split_at);
    let best = r.in_sample.best.as_ref().unwrap();
    assert_eq!(int(&best.config.params, "slow"), 5);
    let o = &r.out_of_sample;
    assert_eq!(o.config.params, best.config.params);
    assert_eq!((o.config.from, o.config.to), (r.split_at, T0 + 300 * HOUR));
    assert_eq!(o.config.warmup_from, Some(T0));
    assert_eq!(o.equity.len(), 150);
    assert_eq!(o.equity[0].time, r.split_at);
    // Warmed up, the strategy orders at the close of the first out-of-sample bar.
    assert_eq!(o.orders[0].submitted_at, r.split_at);
    // Without warm-up it must first see `slow` bars of its own.
    let cold = run(
        &BacktestConfig {
            warmup_from: None,
            ..o.config.clone()
        },
        &bars,
        &[],
    )
    .unwrap();
    assert_eq!(cold.orders[0].submitted_at, r.split_at + 4 * HOUR);
    // Both segments are too short for a CAGR, so there is no degradation; the CAGR ratio is
    // checked on long segments in the next test.
    assert_eq!(r.degradation, None);

    // A split at a time gives the same segments.
    let at = OosSpec {
        split: Split::At(T0 + 150 * HOUR),
        ..oos.clone()
    };
    let r2 = out_of_sample(&at, &bars, &[], None).unwrap();
    assert_eq!(r2.out_of_sample, r.out_of_sample);
}

#[test]
fn total_return_degradation_compares_annualised_growth_across_unequal_segments() {
    const DAY: Millis = 24 * HOUR;
    const IS_DAYS: i64 = 730;
    const OOS_DAYS: i64 = 182;
    // Daily bars: +20% over two years in-sample, then +10% over half a year out of sample.
    let price = |i: i64| {
        if i <= IS_DAYS {
            100.0 * 1.2f64.powf(i as f64 / IS_DAYS as f64)
        } else {
            120.0 * 1.1f64.powf((i - IS_DAYS) as f64 / OOS_DAYS as f64)
        }
    };
    let bars: Vec<Candle> = (0..IS_DAYS + OOS_DAYS)
        .map(|i| {
            let (open, close) = (if i == 0 { 99.9 } else { price(i - 1) }, price(i));
            Candle {
                open_time: T0 + i * DAY,
                high: open.max(close) + 0.05,
                low: open.min(close) - 0.05,
                ..candle(i, open, close)
            }
        })
        .collect();
    let mut b = base(common::spot(), 0);
    b.timeframe = Timeframe::D1;
    b.to = T0 + (IS_DAYS + OOS_DAYS) * DAY;
    b.params = Params::new().with("fast", ParamValue::Int(1));
    let oos = OosSpec {
        sweep: spec(
            b,
            vec![
                values("slow", &ints(&[5])),
                values("equity_fraction", &floats(&[0.9])),
            ],
        ),
        split: Split::At(T0 + IS_DAYS * DAY),
    };
    let r = out_of_sample(&oos, &bars, &[], None).unwrap();
    let is = &r.in_sample.best.as_ref().unwrap().metrics;
    let o = &r.out_of_sample.metrics;
    // The cumulative return is smaller out of sample, the annualised one larger.
    assert!(o.total_return.unwrap() < is.total_return.unwrap());
    assert!(o.cagr.unwrap() > is.cagr.unwrap());
    let d = r.degradation.unwrap();
    assert!(
        d > 1.0,
        "degradation {d} should show the better out-of-sample growth"
    );
    assert_eq!(d, o.cagr.unwrap() / is.cagr.unwrap());
}

#[test]
fn bad_splits_and_preset_warmup_are_errors() {
    let bars = rising_bars(300);
    for split in [
        Split::Fraction(0.0),
        Split::Fraction(1.0),
        Split::Fraction(0.001),
        Split::At(T0),
        Split::At(T0 + 300 * HOUR),
    ] {
        let s = OosSpec {
            sweep: rising_spec(),
            split,
        };
        assert!(
            matches!(
                out_of_sample(&s, &bars, &[], None),
                Err(TesterError::InvalidSpec(_))
            ),
            "{split:?}"
        );
    }
    let mut s = OosSpec {
        sweep: rising_spec(),
        split: Split::Fraction(0.5),
    };
    s.sweep.base.warmup_from = Some(T0);
    assert!(matches!(
        out_of_sample(&s, &bars, &[], None),
        Err(TesterError::InvalidSpec(_))
    ));
    s.sweep.base.warmup_from = None;
    // Every in-sample run holds one trade, closed out at the end.
    s.sweep.min_trades = 2;
    assert!(matches!(
        out_of_sample(&s, &bars, &[], None),
        Err(TesterError::NoQualifyingRun {
            min_trades: 2,
            window: None
        })
    ));
}

fn wf_spec(mode: WindowMode) -> WalkForwardSpec {
    let mut s = wavy_spec();
    s.axes = vec![
        values("fast", &ints(&[3, 5, 8])),
        values("slow", &ints(&[12, 24])),
    ];
    WalkForwardSpec {
        sweep: s,
        windows: WindowSpec {
            train_bars: 200,
            test_bars: 50,
            step_bars: 50,
            mode,
        },
    }
}

#[test]
fn walk_forward_tests_each_window_after_its_train_and_stitches_the_tests() {
    let bars = wavy_bars(500);
    let calls = Mutex::new(0usize);
    let progress = |done: usize, total: usize| {
        assert!(done <= total && total == 6 * 6);
        *calls.lock().unwrap() += 1;
        ControlFlow::Continue(())
    };
    let r = walk_forward(&wf_spec(WindowMode::Rolling), &bars, &[], Some(&progress)).unwrap();
    assert_eq!(calls.into_inner().unwrap(), 36);
    // Tests start at bars 200, 250, ... 450.
    assert_eq!(r.windows.len(), 6);
    for (k, w) in r.windows.iter().enumerate() {
        let start = 200 + 50 * k as i64;
        assert_eq!(w.test_from, T0 + start * HOUR);
        assert_eq!(w.test_to, T0 + (start + 50) * HOUR);
        assert_eq!(w.train_to, w.test_from);
        assert_eq!(w.train_from, T0 + (start - 200) * HOUR);
        let o = &w.out_of_sample;
        assert_eq!(o.config.warmup_from, Some(w.train_from));
        assert_eq!(o.config.params, w.params);
        assert_eq!(o.equity.len(), 50);
        assert!(
            o.equity
                .iter()
                .all(|p| p.time >= w.test_from && p.time < w.test_to)
        );
        // No look-ahead: the test run is the same without any bar from its test_to on, and the
        // warm-up bars add nothing to its equity or trades.
        let known: Vec<Candle> = bars
            .iter()
            .filter(|b| b.open_time < w.test_to)
            .copied()
            .collect();
        assert_eq!(&run(&o.config, &known, &[]).unwrap(), o);
        assert!(o.fills.iter().all(|f| f.time >= w.test_from));
        assert!(o.trades.iter().all(|t| t.entry_time >= w.test_from));
    }
    for pair in r.windows.windows(2) {
        assert_eq!(pair[0].test_to, pair[1].test_from);
    }

    // The stitched curve chains the segments: each starts where the previous one ended.
    assert_eq!(r.equity.len(), 300);
    let mut level = 10_000.0;
    let mut i = 0;
    for w in &r.windows {
        let scale = level / 10_000.0;
        for p in &w.out_of_sample.equity {
            assert_eq!(r.equity[i].time, p.time);
            assert!((r.equity[i].equity - p.equity * scale).abs() < 1e-9);
            i += 1;
        }
        level = r.equity[i - 1].equity;
    }
    assert_eq!(r.metrics.end_equity, level);
    let trades: usize = r
        .windows
        .iter()
        .map(|w| w.out_of_sample.metrics.trades)
        .sum();
    assert_eq!(r.metrics.trades, trades);
    // Scaled trade P&L adds up to the stitched equity's change, as in a single backtest.
    let ratio = |w: &td_engine::WindowResult| {
        let o = &w.out_of_sample;
        o.equity.last().unwrap().equity / 10_000.0
    };
    let compounded: f64 = r.windows.iter().map(ratio).product::<f64>() * 10_000.0;
    assert!((compounded - level).abs() < 1e-6);

    let changes = r
        .windows
        .windows(2)
        .filter(|p| p[0].params != p[1].params)
        .count();
    assert_eq!(r.param_changes, changes);
    assert_eq!(r.param_change_rate, Some(changes as f64 / 5.0));
}

#[test]
fn anchored_walk_forward_trains_from_the_start() {
    let bars = wavy_bars(500);
    let r = walk_forward(&wf_spec(WindowMode::Anchored), &bars, &[], None).unwrap();
    assert_eq!(r.windows.len(), 6);
    for w in &r.windows {
        assert_eq!(w.train_from, T0);
        assert_eq!(w.train_to, w.test_from);
        assert_eq!(w.out_of_sample.config.warmup_from, Some(T0));
    }
}

#[test]
fn sweeps_and_walk_forwards_save_and_load_with_their_kind() {
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path();
    let bars = wavy_bars(500);
    let swept = sweep(&wavy_spec(), &bars, &[], None).unwrap();
    let wf = walk_forward(&wf_spec(WindowMode::Rolling), &bars, &[], None).unwrap();
    let oos = out_of_sample(
        &OosSpec {
            sweep: rising_spec(),
            split: Split::Fraction(0.5),
        },
        &rising_bars(300),
        &[],
        None,
    )
    .unwrap();
    let saved = [
        SavedRun::Sweep(Box::new(swept.clone())),
        SavedRun::WalkForward(Box::new(wf.clone())),
        SavedRun::Oos(Box::new(oos.clone())),
    ];
    let mut ids = Vec::new();
    for s in &saved {
        let mut id = save(s, runs);
        // Two saves in the same millisecond with the same content clash; retry.
        while id.is_err() {
            id = save(s, runs);
        }
        ids.push(id.unwrap());
    }
    for (s, id) in saved.iter().zip(&ids) {
        assert_eq!(&load(runs, id.as_str()).unwrap(), s);
    }
    // config.json holds the spec, so the run can be repeated from it.
    let spec_json =
        std::fs::read_to_string(runs.join(ids[0].as_str()).join("config.json")).unwrap();
    let spec: SweepSpec = serde_json::from_str(&spec_json).unwrap();
    assert_eq!(sweep(&spec, &bars, &[], None).unwrap(), swept);

    let listed = list(runs).unwrap();
    let kind_of = |id| listed.iter().find(|s| &s.id == id).unwrap();
    let s = kind_of(&ids[0]);
    assert_eq!(s.kind, RunKind::Sweep);
    assert_eq!(
        s.total_return,
        swept.best.as_ref().unwrap().metrics.total_return
    );
    let w = kind_of(&ids[1]);
    assert_eq!(w.kind, RunKind::WalkForward);
    assert_eq!((w.from, w.to), (T0, T0 + 500 * HOUR));
    assert_eq!(w.total_return, wf.metrics.total_return);
    let o = kind_of(&ids[2]);
    assert_eq!(o.kind, RunKind::Oos);
    assert_eq!(o.total_return, oos.out_of_sample.metrics.total_return);
    // A loaded run gives the same row as the list, and its fingerprint is the base config's.
    for (s, id) in saved.iter().zip(&ids) {
        assert_eq!(&s.summary(id.clone()), kind_of(id));
        assert_eq!(s.engine_revision(), Some(td_engine::ENGINE_REVISION));
    }
    assert_eq!(saved[0].data_fingerprint(), swept.data_fingerprint);
    assert_eq!(
        config_fingerprint(&swept.spec.base, &bars, &[]).unwrap(),
        swept.data_fingerprint
    );
    assert_eq!(
        config_fingerprint(&wf.spec.sweep.base, &bars, &[]).unwrap(),
        wf.data_fingerprint
    );
}

/// Long-only SMA cross with fast 1 on a perpetual over the rising line, in rolling windows of 50
/// bars: every test window enters near its start and is still long at its last bar.
fn rising_wf_spec() -> WalkForwardSpec {
    let mut b = base(common::perp(), 300);
    b.slippage_bps = 5.0;
    b.params = Params::new().with("fast", ParamValue::Int(1));
    WalkForwardSpec {
        sweep: spec(b, vec![values("slow", &ints(&[5, 10]))]),
        windows: WindowSpec {
            train_bars: 50,
            test_bars: 50,
            step_bars: 50,
            mode: WindowMode::Rolling,
        },
    }
}

/// Checks that `o` ends flat: its position is closed by a reduce-only market sell decided at the
/// close of the second-to-last bar before `to` and filled at the last bar's open, with slippage
/// and the taker fee, and every trade is closed.
fn assert_closed_out(o: &td_engine::BacktestResult, bars: &[Candle], to: Millis) {
    let last = bars.iter().rfind(|b| b.open_time < to).unwrap();
    assert!(!o.trades.is_empty());
    assert!(o.trades.iter().all(|t| !t.open), "{:?}", o.trades);
    let exit = o.fills.last().unwrap();
    assert_eq!(exit.side, td_core::Side::Sell);
    assert_eq!(exit.time, last.open_time);
    assert!(exit.price < last.open && exit.price >= last.open * (1.0 - 5e-4) - 0.01);
    assert!((exit.fee - exit.price * exit.qty * 0.002).abs() < 1e-9);
    let order = o.orders.iter().find(|x| x.id == exit.order_id).unwrap();
    assert_eq!(order.submitted_at, last.open_time - HOUR);
    assert!(order.request.reduce_only);
    assert_eq!(order.request.order_type, td_core::OrderType::Market);
    let position: f64 = o.fills.iter().map(|f| f.side.sign() * f.qty).sum();
    assert!(position.abs() < 1e-9, "{position}");
    // The trade's exit fee is in its P&L.
    let t = o.trades.last().unwrap();
    assert_eq!(t.exit_time, last.open_time);
    assert!(t.fees > exit.fee - 1e-9);
}

#[test]
fn walk_forward_closes_every_test_window_with_a_real_exit_fill() {
    let bars = rising_bars(300);
    let r = walk_forward(&rising_wf_spec(), &bars, &[], None).unwrap();
    assert_eq!(r.windows.len(), 5);
    let mut exit_fees = 0.0;
    for w in &r.windows {
        let o = &w.out_of_sample;
        assert!(o.config.close_at_end);
        assert_closed_out(o, &bars, w.test_to);
        assert_eq!(o.metrics.trades, o.trades.len());
        exit_fees += o.fills.last().unwrap().fee;
    }
    // Each window's one trade is closed and counted; on a rising line every one wins.
    assert_eq!(r.metrics.trades, 5);
    assert_eq!(r.metrics.win_rate, Some(1.0));
    assert!(r.metrics.avg_trade_pnl.unwrap() > 0.0);
    // The stitched fees include the exit fees (scaled up, since equity grew).
    assert!(r.metrics.fees_paid > exit_fees);
}

#[test]
fn the_out_of_sample_run_is_closed_out_at_its_end() {
    let mut s = rising_spec();
    s.base.instrument = common::perp();
    s.base.slippage_bps = 5.0;
    let bars = rising_bars(300);
    let r = out_of_sample(
        &OosSpec {
            sweep: s,
            split: Split::Fraction(0.5),
        },
        &bars,
        &[],
        None,
    )
    .unwrap();
    let o = &r.out_of_sample;
    assert!(o.config.close_at_end);
    assert_closed_out(o, &bars, T0 + 300 * HOUR);
    assert_eq!(o.metrics.trades, 1);
}

#[test]
fn rolling_train_sweeps_after_the_first_warm_up_on_the_bars_before_them() {
    let bars = rising_bars(300);
    let r = walk_forward(&rising_wf_spec(), &bars, &[], None).unwrap();
    for (k, w) in r.windows.iter().enumerate() {
        let warm = (k > 0).then(|| w.train_from - 50 * HOUR);
        let expected = run(
            &BacktestConfig {
                from: w.train_from,
                to: w.train_to,
                warmup_from: warm,
                close_at_end: true,
                ..with(&rising_wf_spec().sweep.base, &w.params)
            },
            &bars,
            &[],
        )
        .unwrap();
        assert_eq!(
            w.in_sample.total_return, expected.metrics.total_return,
            "window {k}"
        );
    }
    // Warmed, a train run enters on its first bar; cold, it would wait for `slow` bars.
    let w = &r.windows[1];
    let cold = run(
        &BacktestConfig {
            from: w.train_from,
            to: w.train_to,
            close_at_end: true,
            ..with(&rising_wf_spec().sweep.base, &w.params)
        },
        &bars,
        &[],
    )
    .unwrap();
    assert_ne!(w.in_sample.total_return, cold.metrics.total_return);
}

#[test]
fn a_position_held_at_the_end_counts_alike_in_and_out_of_sample() {
    // Long from near the start of each segment to its end: the in-sample runs close out like
    // the out-of-sample run, so both sides count the one trade.
    let bars = rising_bars(300);
    let r = out_of_sample(
        &OosSpec {
            sweep: rising_spec(),
            split: Split::Fraction(0.5),
        },
        &bars,
        &[],
        None,
    )
    .unwrap();
    assert!(r.in_sample.spec.base.close_at_end);
    let best = r.in_sample.best.as_ref().unwrap();
    assert!(best.config.close_at_end);
    assert!(best.trades.iter().all(|t| !t.open));
    assert_eq!(best.metrics.trades, 1);
    assert_eq!(r.out_of_sample.metrics.trades, 1);
    assert!(r.in_sample.rows.iter().all(|row| {
        let m = row.metrics.as_ref().unwrap();
        m.trades == 1
    }));

    let wf = walk_forward(&rising_wf_spec(), &bars, &[], None).unwrap();
    for (k, w) in wf.windows.iter().enumerate() {
        assert_eq!(w.in_sample.trades, 1, "window {k}");
        assert_eq!(w.out_of_sample.metrics.trades, 1, "window {k}");
    }
}

fn with(base: &BacktestConfig, params: &Params) -> BacktestConfig {
    BacktestConfig {
        params: params.clone(),
        ..base.clone()
    }
}

#[test]
fn walk_forwards_over_the_run_limit_are_refused_before_running() {
    // 1000 parameter sets in each of 299 one-bar windows.
    let mut s = rising_wf_spec();
    s.sweep.base.params = Params::new().with("slow", ParamValue::Int(2000));
    s.sweep.axes = vec![range("fast", 1.0, 1000.0, 1.0)];
    s.windows = WindowSpec {
        train_bars: 1,
        test_bars: 1,
        step_bars: 1,
        mode: WindowMode::Rolling,
    };
    let started = |_: usize, _: usize| panic!("the walk-forward started running");
    let err = walk_forward(&s, &rising_bars(300), &[], Some(&started)).unwrap_err();
    assert!(
        matches!(err, TesterError::TooManyRuns { runs: 299_000, limit } if limit == MAX_RUNS),
        "{err}"
    );
}

#[test]
fn the_walk_forward_result_lists_the_stitched_trades_and_fills() {
    let bars = rising_bars(300);
    let r = walk_forward(&rising_wf_spec(), &bars, &[], None).unwrap();
    let fills: usize = r.windows.iter().map(|w| w.out_of_sample.fills.len()).sum();
    assert_eq!(r.fills.len(), fills);
    assert_eq!(r.trades.len(), 5);
    assert!(r.trades.iter().all(|t| !t.open));
    assert!(
        r.trades
            .windows(2)
            .all(|p| p[0].exit_time <= p[1].entry_time)
    );
    assert!(r.fills.windows(2).all(|p| p[0].time <= p[1].time));
    // Scaled, the trades' net P&L adds up to the stitched equity's change, and the fills' fees
    // to the stitched fees.
    let pnl: f64 = r.trades.iter().map(|t| t.net_pnl).sum();
    assert!(
        (pnl - (r.metrics.end_equity - 10_000.0)).abs() < 1e-6,
        "{pnl}"
    );
    let fees: f64 = r.fills.iter().map(|f| f.fee).sum();
    assert_eq!(fees, r.metrics.fees_paid);
    // The second window's trade is scaled by the first window's growth.
    let first = &r.windows[0].out_of_sample;
    let scale = first.equity.last().unwrap().equity / 10_000.0;
    let own = &r.windows[1].out_of_sample.trades[0];
    assert!((r.trades[1].net_pnl - own.net_pnl * scale).abs() < 1e-9);
}

#[test]
fn a_progress_callback_that_breaks_stops_the_sweep_before_its_remaining_runs() {
    let bars = wavy_bars(500);
    let count = Mutex::new(0usize);
    let calls = &count;
    let stop_after = |n: usize| {
        move |_: usize, _: usize| {
            let mut c = calls.lock().unwrap();
            *c += 1;
            if *c >= n {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        }
    };
    // One thread, so the runs go one at a time and none starts after the stop.
    let progress = stop_after(3);
    let r = pool(1).install(|| sweep(&wavy_spec(), &bars, &[], Some(&progress)));
    assert_eq!(r, Err(TesterError::Cancelled));
    assert_eq!(*calls.lock().unwrap(), 3, "48 runs, stopped after 3");

    *calls.lock().unwrap() = 0;
    let progress = stop_after(8);
    let r = pool(1)
        .install(|| walk_forward(&wf_spec(WindowMode::Rolling), &bars, &[], Some(&progress)));
    assert_eq!(r, Err(TesterError::Cancelled));
    assert_eq!(
        *calls.lock().unwrap(),
        8,
        "36 runs over 6 windows, stopped in the second"
    );

    *calls.lock().unwrap() = 0;
    let oos = OosSpec {
        sweep: wavy_spec(),
        split: Split::Fraction(0.5),
    };
    let progress = stop_after(1);
    let r = pool(1).install(|| out_of_sample(&oos, &bars, &[], Some(&progress)));
    assert_eq!(r, Err(TesterError::Cancelled));
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[test]
fn a_progress_callback_that_continues_leaves_the_result_unchanged() {
    let bars = wavy_bars(500);
    let progress = |_: usize, _: usize| ControlFlow::Continue(());
    let with = sweep(&wavy_spec(), &bars, &[], Some(&progress)).unwrap();
    let without = sweep(&wavy_spec(), &bars, &[], None).unwrap();
    assert_eq!(with, without);
}

#[test]
fn checking_a_spec_without_data_finds_what_the_run_would_refuse_first() {
    let bars = wavy_bars(500);
    assert_eq!(wavy_spec().check(), Ok(()));
    let mut bad = wavy_spec();
    bad.axes.push(values("fast", &ints(&[2])));
    assert!(matches!(bad.check(), Err(TesterError::InvalidSpace { .. })));
    assert_eq!(bad.check(), sweep(&bad, &bars, &[], None).map(|_| ()));
    let mut unknown = wavy_spec();
    unknown.base.strategy = "nope".into();
    assert_eq!(
        unknown.check(),
        sweep(&unknown, &bars, &[], None).map(|_| ())
    );

    let mut warm = wavy_spec();
    warm.base.warmup_from = Some(T0);
    let oos = OosSpec {
        sweep: warm.clone(),
        split: Split::Fraction(0.5),
    };
    assert!(matches!(oos.check(), Err(TesterError::InvalidSpec(_))));
    assert_eq!(
        oos.check(),
        out_of_sample(&oos, &bars, &[], None).map(|_| ())
    );
    let mut wf = wf_spec(WindowMode::Rolling);
    assert_eq!(wf.check(), Ok(()));
    wf.sweep = warm;
    assert_eq!(wf.check(), walk_forward(&wf, &bars, &[], None).map(|_| ()));
}

#[test]
fn a_parameter_set_that_never_trades_ranks_last_when_minimising_drawdown_by_default() {
    // slow 1000 is longer than the 500 bars, so that set never trades and has no drawdown at
    // all: ranked by drawdown alone it would come first.
    let mut b = base(common::perp(), 500);
    b.params = Params::new().with("fast", ParamValue::Int(6));
    let mut s = spec(b, vec![values("slow", &ints(&[1000, 24, 12]))]);
    s.objective = Objective {
        metric: Metric::MaxDrawdown,
        direction: Direction::Minimize,
    };
    // A spec that leaves min_trades out, as the apps may.
    let mut json = serde_json::to_value(&s).unwrap();
    json.as_object_mut().unwrap().remove("min_trades");
    let s: SweepSpec = serde_json::from_value(json).unwrap();
    assert_eq!(s.min_trades, 1);
    let r = sweep(&s, &wavy_bars(500), &[], None).unwrap();
    let last = r.rows.last().unwrap();
    assert_eq!(int(&last.params, "slow"), 1000);
    assert_eq!(last.metrics.unwrap().trades, 0);
    assert_eq!(last.metrics.unwrap().max_drawdown, Some(0.0));
    assert!(!last.qualified);
    assert!(r.rows[..2].iter().all(|row| row.qualified));
    assert_ne!(int(&r.best.as_ref().unwrap().config.params, "slow"), 1000);
}

#[test]
fn an_objective_undefined_on_a_short_range_qualifies_no_run_and_ranks_in_grid_order() {
    // 500 hourly bars are under 21 days: too few daily returns for a Sharpe ratio.
    let mut s = wavy_spec();
    s.objective = Objective {
        metric: Metric::Sharpe,
        direction: Direction::Maximize,
    };
    let r = sweep(&s, &wavy_bars(500), &[], None).unwrap();
    assert!(
        r.rows
            .iter()
            .all(|row| !row.qualified && row.metrics.unwrap().sharpe.is_none())
    );
    let order: Vec<u64> = r.rows.iter().map(|row| row.index).collect();
    assert_eq!(order, (0..48).collect::<Vec<_>>());
    assert!(r.best.is_none());
}

#[test]
fn a_sharpe_objective_ranks_a_run_without_a_sharpe_ratio_after_every_run_with_one() {
    // 60 days of hourly bars, enough daily returns; the set that never trades has a flat
    // curve, so no Sharpe ratio, and ranks after the sets that do.
    let n = 60 * 24;
    let mut b = base(common::perp(), n);
    b.params = Params::new().with("fast", ParamValue::Int(6));
    let mut s = spec(b, vec![values("slow", &ints(&[5000, 24, 12]))]);
    s.objective = Objective {
        metric: Metric::Sharpe,
        direction: Direction::Maximize,
    };
    s.min_trades = 0;
    let r = sweep(&s, &wavy_bars(n), &[], None).unwrap();
    assert!(r.rows[..2].iter().all(|row| row.qualified));
    let last = r.rows.last().unwrap();
    assert_eq!(int(&last.params, "slow"), 5000);
    assert_eq!(last.metrics.unwrap().sharpe, None);
    assert!(!last.qualified);
}

#[test]
fn a_total_return_degradation_is_undefined_when_a_segment_is_too_short_for_a_cagr() {
    // 300 hourly bars split in half: both segments are under 90 days, so neither has a CAGR.
    let r = out_of_sample(
        &OosSpec {
            sweep: rising_spec(),
            split: Split::Fraction(0.5),
        },
        &rising_bars(300),
        &[],
        None,
    )
    .unwrap();
    assert_eq!(r.out_of_sample.metrics.cagr, None);
    assert!(r.out_of_sample.metrics.total_return.is_some());
    assert_eq!(r.degradation, None);
}

/// Funding for the walk-forward benchmark test: one event before the first test bar (ignored),
/// one at the open of bar 60, one inside bar 100 and one at the end of the range (ignored).
fn wf_benchmark_funding() -> Vec<(Millis, f64)> {
    vec![
        (T0 + 300 * HOUR, 0.0003),
        (T0 + 60 * HOUR, 0.0001),
        (T0 + 100 * HOUR + 30 * 60_000, -0.00005),
        (T0 + 10 * HOUR, 0.0002),
    ]
}

#[test]
fn the_walk_forward_benchmark_buys_at_the_first_test_bar_and_holds_to_the_last() {
    let bars = rising_bars(300);
    let funding = wf_benchmark_funding();
    let r = walk_forward(&rising_wf_spec(), &bars, &funding, None).unwrap();
    // Tests run from bar 50 to bar 299.
    assert_eq!(r.windows[0].test_from, T0 + 50 * HOUR);
    let b = r.benchmark.as_ref().unwrap();
    // Bar 50 opens at 124.5; with 5 bps of slippage 124.56225, rounded up to the tick.
    assert_eq!(b.entry_price, 124.57);
    // 10 000 / (124.57 * 1.002) = 80.1159..., rounded down to the lot.
    assert!((b.qty - 80.115).abs() < 1e-9, "{}", b.qty);
    assert_eq!(b.equity.len(), 250);
    assert_eq!(b.equity[0].time, T0 + 50 * HOUR);
    assert_eq!(b.equity.last().unwrap().time, T0 + 299 * HOUR);
    // Held to the close of bar 299 at 249.5, less the entry fee and the two funding payments in
    // the span: at the open of bar 60 (129.5) and inside bar 100, at its close (150).
    let fee = 124.57 * 80.115 * 0.002;
    let paid = 80.115 * 129.5 * 0.0001 + 80.115 * 150.0 * -0.00005;
    let end = 10_000.0 - fee - paid + 80.115 * (249.5 - 124.57);
    let got = b.equity.last().unwrap().equity;
    assert!((got - end).abs() < 1e-9, "{got} vs {end}");
    assert!((b.metrics.funding_paid - paid).abs() < 1e-12);
    assert!((b.metrics.total_return.unwrap() - (end / 10_000.0 - 1.0)).abs() < 1e-12);
    assert_eq!(
        b.excess_return,
        Some(r.metrics.total_return.unwrap() - b.metrics.total_return.unwrap())
    );
    assert!(b.correlation.is_some());
}

#[test]
fn the_walk_forward_benchmark_holds_through_the_gaps_between_test_windows() {
    let bars = rising_bars(300);
    let mut s = rising_wf_spec();
    // Tests of 40 bars every 50: 60..100, 110..150, ..., 260..300.
    s.windows.test_bars = 40;
    let r = walk_forward(&s, &bars, &wf_benchmark_funding(), None).unwrap();
    assert_eq!(r.windows[0].test_from, T0 + 60 * HOUR);
    assert_eq!(r.equity.len(), 200);
    let b = r.benchmark.as_ref().unwrap();
    // Bars 60 to 299, gaps included, held continuously.
    assert_eq!(b.equity.len(), 240);
    assert_eq!(b.equity[0].time, T0 + 60 * HOUR);
    // Bar 60 opens at 129.5: 129.56475, rounded up to the tick.
    assert_eq!(b.entry_price, 129.57);
    // 10 000 / (129.57 * 1.002) = 77.0243..., rounded down to the lot.
    let qty = 77.024;
    assert!((b.qty - qty).abs() < 1e-9, "{}", b.qty);
    // Funding at bar 60's open falls before the entry and is not charged; bar 100's is.
    let fee = 129.57 * qty * 0.002;
    let end = 10_000.0 - fee - qty * 150.0 * -0.00005 + qty * (249.5 - 129.57);
    let got = b.equity.last().unwrap().equity;
    assert!((got - end).abs() < 1e-9, "{got} vs {end}");
    // The correlation pairs each stitched bar's return with the benchmark's over the same time.
    assert!(b.correlation.is_some());
}

#[test]
fn walk_forward_tests_up_to_the_last_bar_and_warms_the_first_train_sweep_on_the_leftover() {
    // 310 bars with 50-bar windows: 10 bars are left over at the start, not at the end.
    let bars = rising_bars(310);
    let mut s = rising_wf_spec();
    s.sweep.base.to = T0 + 310 * HOUR;
    let r = walk_forward(&s, &bars, &[], None).unwrap();
    assert_eq!(r.windows.len(), 5);
    assert_eq!(r.windows.last().unwrap().test_to, T0 + 310 * HOUR);
    assert_eq!(r.equity.last().unwrap().time, T0 + 309 * HOUR);
    let first = &r.windows[0];
    assert_eq!(
        (first.train_from, first.test_from),
        (T0 + 10 * HOUR, T0 + 60 * HOUR)
    );
    // The first train sweep is warmed up on the 10 leftover bars.
    let warmed = run(
        &BacktestConfig {
            from: first.train_from,
            to: first.train_to,
            warmup_from: Some(T0),
            close_at_end: true,
            ..with(&s.sweep.base, &first.params)
        },
        &bars,
        &[],
    )
    .unwrap();
    assert_eq!(first.in_sample.total_return, warmed.metrics.total_return);
    let cold = run(
        &BacktestConfig {
            from: first.train_from,
            to: first.train_to,
            close_at_end: true,
            ..with(&s.sweep.base, &first.params)
        },
        &bars,
        &[],
    )
    .unwrap();
    assert_ne!(first.in_sample.total_return, cold.metrics.total_return);
}
