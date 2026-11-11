//! The strategy tester end to end: sweeps with a known best, validation, reproducible random
//! sampling, parallel against serial, out-of-sample warm-up, walk-forward windows and stitching,
//! and saved tester results.

mod common;

use std::ops::ControlFlow;
use std::sync::Mutex;

use td_core::{Candle, FeeSchedule, Millis, Timeframe};
use td_engine::{
    BacktestConfig, Direction, EngineError, MAX_RUNS, Metric, Objective, OosSpec, ParamAxis,
    ParamSpace, ParamValue, Params, RunKind, SavedRun, Split, SweepMode, SweepSpec, TesterError,
    WalkForwardResult, WalkForwardSpec, WindowMode, WindowSpec, config_fingerprint, list, load,
    out_of_sample, run, save, sweep, walk_forward,
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
        account_currency: td_core::AccountCurrency::Quote,
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
    let r = sweep(&rising_spec(), &rising_bars(300), &[], &[], None).unwrap();
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
    let r = sweep(&s, &rising_bars(300), &[], &[], None).unwrap();
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
    let r = sweep(&s, &rising_bars(300), &[], &[], None).unwrap();
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
        let err = sweep(&spec(base(common::spot(), 50), axes), &bars, &[], &[], None).unwrap_err();
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
        sweep(&fixed, &bars, &[], &[], None),
        Err(TesterError::InvalidSpace { .. })
    ));
    let mut s = spec(base(common::spot(), 50), vec![range("fast", 1.0, 5.0, 1.0)]);
    s.mode = SweepMode::Random {
        samples: 0,
        seed: 1,
    };
    assert!(matches!(
        sweep(&s, &bars, &[], &[], None),
        Err(TesterError::InvalidSpec(_))
    ));
    s.mode = SweepMode::Random {
        samples: MAX_RUNS + 1,
        seed: 1,
    };
    assert!(matches!(
        sweep(&s, &bars, &[], &[], None),
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
    assert_eq!(sweep(&big, &bars, &[], &[], None).unwrap().rows.len(), 3);
}

#[test]
fn ranges_expand_in_order_and_floats_stop_at_the_end() {
    let mut b = base(common::spot(), 300);
    b.params = Params::new()
        .with("fast", ParamValue::Int(1))
        .with("slow", ParamValue::Int(5));
    let s = spec(b, vec![range("equity_fraction", 0.1, 1.0, 0.3)]);
    let r = sweep(&s, &rising_bars(300), &[], &[], None).unwrap();
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
    let a = sweep(&s, &bars, &[], &[], None).unwrap();
    let b = sweep(&s, &bars, &[], &[], None).unwrap();
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
    let c = sweep(&s, &bars, &[], &[], None).unwrap();
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
    let serial = pool(1).install(|| sweep(&s, &bars, &[], &[], Some(&progress)).unwrap());
    let parallel = pool(4).install(|| sweep(&s, &bars, &[], &[], None).unwrap());
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
    let r = out_of_sample(&oos, &bars, &[], &[], None).unwrap();
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
    let r2 = out_of_sample(&at, &bars, &[], &[], None).unwrap();
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
    let r = out_of_sample(&oos, &bars, &[], &[], None).unwrap();
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
                out_of_sample(&s, &bars, &[], &[], None),
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
        out_of_sample(&s, &bars, &[], &[], None),
        Err(TesterError::InvalidSpec(_))
    ));
    s.sweep.base.warmup_from = None;
    // Every in-sample run holds one trade, closed out at the end.
    s.sweep.min_trades = 2;
    assert!(matches!(
        out_of_sample(&s, &bars, &[], &[], None),
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
    let r = walk_forward(
        &wf_spec(WindowMode::Rolling),
        &bars,
        &[],
        &[],
        Some(&progress),
    )
    .unwrap();
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
        assert_eq!(&run(&o.config, &known, &[], &[]).unwrap(), o);
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
    let r = walk_forward(&wf_spec(WindowMode::Anchored), &bars, &[], &[], None).unwrap();
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
    let swept = sweep(&wavy_spec(), &bars, &[], &[], None).unwrap();
    let wf = walk_forward(&wf_spec(WindowMode::Rolling), &bars, &[], &[], None).unwrap();
    let oos = out_of_sample(
        &OosSpec {
            sweep: rising_spec(),
            split: Split::Fraction(0.5),
        },
        &rising_bars(300),
        &[],
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
    assert_eq!(sweep(&spec, &bars, &[], &[], None).unwrap(), swept);

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
        config_fingerprint(&swept.spec.base, &bars, &[], &[]).unwrap(),
        swept.data_fingerprint
    );
    assert_eq!(
        config_fingerprint(&wf.spec.sweep.base, &bars, &[], &[]).unwrap(),
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
    let r = walk_forward(&rising_wf_spec(), &bars, &[], &[], None).unwrap();
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
    let r = walk_forward(&rising_wf_spec(), &bars, &[], &[], None).unwrap();
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

    let wf = walk_forward(&rising_wf_spec(), &bars, &[], &[], None).unwrap();
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
    let err = walk_forward(&s, &rising_bars(300), &[], &[], Some(&started)).unwrap_err();
    assert!(
        matches!(err, TesterError::TooManyRuns { runs: 299_000, limit } if limit == MAX_RUNS),
        "{err}"
    );
}

#[test]
fn the_walk_forward_result_lists_the_stitched_trades_and_fills() {
    let bars = rising_bars(300);
    let r = walk_forward(&rising_wf_spec(), &bars, &[], &[], None).unwrap();
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
    let r = pool(1).install(|| sweep(&wavy_spec(), &bars, &[], &[], Some(&progress)));
    assert_eq!(r, Err(TesterError::Cancelled));
    assert_eq!(*calls.lock().unwrap(), 3, "48 runs, stopped after 3");

    *calls.lock().unwrap() = 0;
    let progress = stop_after(8);
    let r = pool(1).install(|| {
        walk_forward(
            &wf_spec(WindowMode::Rolling),
            &bars,
            &[],
            &[],
            Some(&progress),
        )
    });
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
    let r = pool(1).install(|| out_of_sample(&oos, &bars, &[], &[], Some(&progress)));
    assert_eq!(r, Err(TesterError::Cancelled));
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[test]
fn a_progress_callback_that_continues_leaves_the_result_unchanged() {
    let bars = wavy_bars(500);
    let progress = |_: usize, _: usize| ControlFlow::Continue(());
    let with = sweep(&wavy_spec(), &bars, &[], &[], Some(&progress)).unwrap();
    let without = sweep(&wavy_spec(), &bars, &[], &[], None).unwrap();
    assert_eq!(with, without);
}

#[test]
fn checking_a_spec_without_data_finds_what_the_run_would_refuse_first() {
    let bars = wavy_bars(500);
    assert_eq!(wavy_spec().check(), Ok(()));
    let mut bad = wavy_spec();
    bad.axes.push(values("fast", &ints(&[2])));
    assert!(matches!(bad.check(), Err(TesterError::InvalidSpace { .. })));
    assert_eq!(bad.check(), sweep(&bad, &bars, &[], &[], None).map(|_| ()));
    let mut unknown = wavy_spec();
    unknown.base.strategy = "nope".into();
    assert_eq!(
        unknown.check(),
        sweep(&unknown, &bars, &[], &[], None).map(|_| ())
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
        out_of_sample(&oos, &bars, &[], &[], None).map(|_| ())
    );
    let mut wf = wf_spec(WindowMode::Rolling);
    assert_eq!(wf.check(), Ok(()));
    wf.sweep = warm;
    assert_eq!(
        wf.check(),
        walk_forward(&wf, &bars, &[], &[], None).map(|_| ())
    );
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
    let r = sweep(&s, &wavy_bars(500), &[], &[], None).unwrap();
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
    let r = sweep(&s, &wavy_bars(500), &[], &[], None).unwrap();
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
    let r = sweep(&s, &wavy_bars(n), &[], &[], None).unwrap();
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
fn the_walk_forward_benchmark_buys_and_holds_in_each_test_window_and_compounds_them() {
    let bars = rising_bars(300);
    let funding = wf_benchmark_funding();
    let r = walk_forward(&rising_wf_spec(), &bars, &[], &funding, None).unwrap();
    // Five contiguous test windows of 50 bars, from bar 50 to bar 299.
    assert_eq!(r.windows.len(), 5);
    assert_eq!(r.windows[0].test_from, T0 + 50 * HOUR);
    let b = r.benchmark.as_ref().unwrap();
    // Each window buys again at its first bar's open, with 5 bps of slippage rounded up to the
    // tick, and all its cash after the 0.2% taker fee rounded down to the lot. Bar 50 opens at
    // 124.5: 124.56225 rounds up to 124.57, and 10 000 / (124.57 * 1.002) = 80.1159... rounds
    // down to 80.115. Bar 100 opens at 149.5: 149.58 and 66.7205... -> 66.72. Bar 150: 174.59 and
    // 57.1627... -> 57.162. Bar 200: 199.6 and 50.0002... -> 50. Bar 250: 224.62 and 44.4308...
    // -> 44.43. Funding at bar 60's open is paid in the first window at that open (129.5), and
    // the event inside bar 100 in the second window at bar 100's close (150), on that window's
    // quantity. The events before bar 50 and at the end of the range are ignored.
    let windows: [(i64, f64, f64, f64); 5] = [
        (50, 124.57, 80.115, 80.115 * 129.5 * 0.0001),
        (100, 149.58, 66.72, 66.72 * 150.0 * -0.00005),
        (150, 174.59, 57.162, 0.0),
        (200, 199.6, 50.0, 0.0),
        (250, 224.62, 44.43, 0.0),
    ];
    // The benchmark reports the first window's purchase.
    assert_eq!(b.entry_price, 124.57);
    assert!((b.qty - 80.115).abs() < 1e-9, "{}", b.qty);
    // It has the stitched curve's times.
    assert_eq!(
        b.equity.iter().map(|p| p.time).collect::<Vec<_>>(),
        r.equity.iter().map(|p| p.time).collect::<Vec<_>>()
    );
    assert_eq!(b.equity.len(), 250);
    // Each window is held to the close of its last bar, 100 + 0.5 * (first + 49), less its entry
    // fee and funding, and its return compounds on the windows before it, as the test runs do.
    let mut level = 1.0;
    let mut paid = 0.0;
    for (first, price, qty, funding) in windows {
        let close = 100.0 + 0.5 * (first + 49) as f64;
        let end = 10_000.0 - price * qty * 0.002 - funding + qty * (close - price);
        paid += funding * level;
        level *= end / 10_000.0;
        let last = T0 + (first + 49) * HOUR;
        let got = b.equity.iter().find(|p| p.time == last).unwrap().equity;
        assert!(
            (got - 10_000.0 * level).abs() < 1e-8,
            "{first}: {got} vs {}",
            10_000.0 * level
        );
    }
    assert!((b.metrics.funding_paid - paid).abs() < 1e-12);
    assert!((b.metrics.total_return.unwrap() - (level - 1.0)).abs() < 1e-12);
    assert_eq!(
        b.excess_return,
        Some(r.metrics.total_return.unwrap() - b.metrics.total_return.unwrap())
    );
    assert!(b.correlation.is_some());
}

/// The test windows of [`gap_wf_spec`]: 40 bars every 50.
const GAP_TESTS: [std::ops::Range<i64>; 5] = [60..100, 110..150, 160..200, 210..250, 260..300];

/// Hourly bars, each opening at the previous close, that rise by 2 a bar outside the test windows
/// of [`gap_wf_spec`]. Inside each, the first and last bars are flat and the 38 others rise by
/// 0.25, so a strategy that enters on the first bar's close and closes out on the last bar's open
/// holds every price move of the window. Every price is exact in binary and on the 0.01 tick.
fn gap_bars() -> Vec<Candle> {
    let mut open = 100.0;
    (0..300)
        .map(|i| {
            let close = match GAP_TESTS.iter().find(|t| t.contains(&i)) {
                Some(t) if i == t.start || i == t.end - 1 => open,
                Some(_) => open + 0.25,
                None => open + 2.0,
            };
            let c = candle(i, open, close);
            open = close;
            c
        })
        .collect()
}

/// [`rising_wf_spec`] with tests of 40 bars every 50, all of the equity in, and no fees or
/// slippage.
fn gap_wf_spec() -> WalkForwardSpec {
    let mut s = rising_wf_spec();
    s.windows.test_bars = 40;
    let b = &mut s.sweep.base;
    b.params = b
        .params
        .clone()
        .with("equity_fraction", ParamValue::Float(1.0));
    b.slippage_bps = 0.0;
    b.fee_override = Some(FeeSchedule {
        maker: 0.0,
        taker: 0.0,
        per_unit: 0.0,
    });
    s
}

/// Pearson correlation, written out.
fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let cov: f64 = a.iter().zip(b).map(|(x, y)| (x - ma) * (y - mb)).sum();
    let va: f64 = a.iter().map(|x| (x - ma).powi(2)).sum();
    let vb: f64 = b.iter().map(|y| (y - mb).powi(2)).sum();
    cov / (va * vb).sqrt()
}

#[test]
fn the_walk_forward_benchmark_is_out_of_the_market_between_test_windows_like_the_strategy() {
    let bars = gap_bars();
    let r = walk_forward(&gap_wf_spec(), &bars, &[], &[], None).unwrap();
    let starts: Vec<Millis> = r.windows.iter().map(|w| w.test_from).collect();
    assert_eq!(starts, GAP_TESTS.map(|t| T0 + t.start * HOUR));
    // In every window the strategy buys on the second bar's open, at the first bar's flat price,
    // and sells on the last bar's open, at the last bar's flat price: it holds every move.
    for (w, t) in r.windows.iter().zip(GAP_TESTS) {
        let fills = &w.out_of_sample.fills;
        assert_eq!(fills.len(), 2, "{fills:?}");
        let (first, last) = (t.start as usize, t.end as usize - 1);
        assert_eq!(fills[0].time, T0 + (t.start + 1) * HOUR);
        assert_eq!(fills[0].price, bars[first].close);
        assert_eq!(fills[1].time, bars[last].open_time);
        assert_eq!(fills[1].price, bars[last].close);
    }
    let b = r.benchmark.as_ref().unwrap();
    // Out of the market between test windows, as the stitched curve is: the same 200 times.
    assert_eq!(
        b.equity.iter().map(|p| p.time).collect::<Vec<_>>(),
        r.equity.iter().map(|p| p.time).collect::<Vec<_>>()
    );

    // Each window opens at 220 + 29.5 k (38 rises of 0.25 in a window, then 10 of 2 before the
    // next), and buy and hold spends the starting cash on as many 0.001 lots as it buys:
    // 10 000 / 220 = 45.4545... -> 45.454, / 249.5 = 40.0801... -> 40.08, / 279 = 35.8422... ->
    // 35.842, / 308.5 = 32.4149... -> 32.414, / 338 = 29.5857... -> 29.585.
    let held = [45.454, 40.08, 35.842, 32.414, 29.585];
    assert_eq!(b.entry_price, 220.0);
    assert_eq!(b.qty, held[0]);
    // The strategy's quantity in each window, from its entry fill.
    let traded: Vec<f64> = r
        .windows
        .iter()
        .map(|w| w.out_of_sample.fills[0].qty)
        .collect();
    // Holding `q` from 10 000 through a window makes q * 9.5, and the windows compound: the
    // benchmark's total return, with nothing for the gaps between windows, which rise by 20 each.
    let level = |qs: &[f64]| qs.iter().map(|q| 1.0 + q * 9.5 / 10_000.0).product::<f64>();
    let total = b.metrics.total_return.unwrap();
    assert!((total - (level(&held) - 1.0)).abs() < 1e-12, "{total}");
    assert!((r.metrics.total_return.unwrap() - (level(&traded) - 1.0)).abs() < 1e-12);
    assert_eq!(
        b.excess_return,
        Some(r.metrics.total_return.unwrap() - total)
    );

    // The correlation pairs each stitched bar's return with the benchmark's over the same bar.
    // Holding `q` through a window from 10 000, the return of its j-th bar is nothing on the flat
    // first and last bars, and q * 0.25 over the equity before it, 10 000 + q * 0.25 * (j - 1),
    // on the others. A window's first return is measured from the end of the window before (or
    // the starting cash), at which the stitched curves stand at 10 000 times their factor.
    let returns = |qs: &[f64]| {
        qs.iter()
            .flat_map(|&q| {
                (0..40).map(move |j| match j {
                    0 | 39 => 0.0,
                    _ => q * 0.25 / (10_000.0 + q * 0.25 * (j - 1) as f64),
                })
            })
            .collect::<Vec<f64>>()
    };
    let expected = pearson(&returns(&traded), &returns(&held));
    let got = b.correlation.unwrap();
    assert!((got - expected).abs() < 1e-10, "{got} vs {expected}");
    // The two hold the same bars in sizes within 0.5% of each other.
    assert!(got > 0.9999, "{got}");
}

/// [`rising_wf_spec`] with `train` bars of training, one test window of 50 bars and `metric` as
/// the objective, over exactly the bars it needs, so the train window starts at 00:00 UTC.
fn train_floor_spec(metric: Metric, train: usize) -> (WalkForwardSpec, Vec<Candle>) {
    let mut s = rising_wf_spec();
    s.sweep.objective.metric = metric;
    s.windows.train_bars = train;
    let n = train as i64 + 50;
    s.sweep.base.to = T0 + n * HOUR;
    (s, rising_bars(n))
}

/// Runs `s` and returns its refusal's message, checking that nothing ran before it.
fn refused_before_running(s: &WalkForwardSpec, bars: &[Candle]) -> String {
    let calls = Mutex::new(0usize);
    let progress = |_: usize, _: usize| {
        *calls.lock().unwrap() += 1;
        ControlFlow::Continue(())
    };
    let err = walk_forward(s, bars, &[], &[], Some(&progress)).unwrap_err();
    assert_eq!(calls.into_inner().unwrap(), 0, "ran before refusing");
    match err {
        TesterError::InvalidSpec(m) => m,
        other => panic!("{other:?}"),
    }
}

#[test]
fn checking_a_walk_forward_refuses_train_windows_that_cannot_be_long_enough_for_its_objective() {
    let check = |metric, train, mode| {
        let (mut s, bars) = train_floor_spec(metric, train);
        s.windows.mode = mode;
        let checked = s.check();
        // The run refuses the same, before anything runs.
        if let Err(TesterError::InvalidSpec(m)) = &checked {
            assert_eq!(&refused_before_running(&s, &bars), m);
        }
        checked
    };
    let refusal = |m: &str| Err(TesterError::InvalidSpec(m.into()));
    use WindowMode::{Anchored, Rolling};
    // 50 hourly bars starting at 23:00 run to 01:00 three days on: four UTC days end in them.
    assert_eq!(
        check(Metric::Sharpe, 50, Rolling),
        refusal(
            "a Sharpe objective needs at least 30 daily returns in every train window, and a \
             train window of 50 1h bars has at most 4"
        )
    );
    // 673 hourly bars starting at 23:00 end at 00:00 29 days on: 29 daily returns. One more bar
    // reaches into a 30th day.
    assert_eq!(
        check(Metric::Sortino, 673, Rolling),
        refusal(
            "a Sortino objective needs at least 30 daily returns in every train window, and a \
             train window of 673 1h bars has at most 29"
        )
    );
    assert_eq!(check(Metric::Sortino, 674, Rolling).map(|_| ()), Ok(()));
    // Anchored, the first train window also takes the bars left over before the first test,
    // fewer than the step of 50: at most 99 bars.
    assert_eq!(
        check(Metric::Sharpe, 50, Anchored),
        refusal(
            "a Sharpe objective needs at least 30 daily returns in every train window, and the \
             first train window, at most 99 1h bars, has at most 6"
        )
    );
    // An hour short of 90 days, and 90 days.
    assert_eq!(
        check(Metric::Cagr, 2_159, Rolling),
        refusal(
            "a CAGR objective needs train windows of at least 90 days, and a train window of 2159 \
             1h bars spans 89 days 23 hours"
        )
    );
    assert_eq!(check(Metric::Cagr, 2_160, Rolling), Ok(()));
    // 2112 bars are 88 days, but an anchored first train window may have 49 more.
    assert_eq!(check(Metric::Cagr, 2_112, Anchored), Ok(()));
    assert!(matches!(
        check(Metric::Calmar, 50, Rolling),
        Err(TesterError::InvalidSpec(m)) if m.starts_with("a Calmar objective needs")
    ));
    // Objectives without a floor are not checked.
    assert_eq!(check(Metric::TotalReturn, 50, Rolling), Ok(()));

    // Daily bars start at 00:00: 29 of them hold 29 daily returns.
    let (mut s, _) = train_floor_spec(Metric::Sharpe, 29);
    s.sweep.base.timeframe = Timeframe::D1;
    assert_eq!(
        s.check(),
        refusal(
            "a Sharpe objective needs at least 30 daily returns in every train window, and a \
             train window of 29 1d bars has at most 29"
        )
    );
    s.windows.train_bars = 30;
    assert_eq!(s.check(), Ok(()));

    // Five weekly bars span 35 days, but 30 of those daily returns are zeros carried forward: the
    // ratio would come from five weekly moves, so the window also needs 30 bars.
    let (mut s, _) = train_floor_spec(Metric::Sharpe, 5);
    s.sweep.base.timeframe = Timeframe::W1;
    assert_eq!(
        s.check(),
        refusal(
            "a Sharpe objective needs at least 30 bars as well as 30 daily returns in every train \
             window, and a train window of 5 1w bars is shorter"
        )
    );
    s.windows.train_bars = 30;
    assert_eq!(s.check(), Ok(()));
}

const WEEK: Millis = 7 * 24 * HOUR;

/// [`rising_bars`] one a week from `T0`, a Monday.
fn weekly_bars(n: i64) -> Vec<Candle> {
    rising_bars(n)
        .into_iter()
        .zip(0..)
        .map(|(b, i)| Candle {
            open_time: T0 + i * WEEK,
            ..b
        })
        .collect()
}

/// [`rising_wf_spec`] on weekly bars with a Sharpe objective, over `n` weekly bars.
fn weekly_wf_spec(n: i64, windows: WindowSpec) -> (WalkForwardSpec, Vec<Candle>) {
    let mut s = rising_wf_spec();
    s.sweep.objective.metric = Metric::Sharpe;
    s.sweep.base.timeframe = Timeframe::W1;
    s.sweep.base.to = T0 + n * WEEK;
    s.windows = windows;
    (s, weekly_bars(n))
}

#[test]
fn a_walk_forward_on_weekly_bars_needs_30_bars_in_every_train_window() {
    let rolling = |train_bars| WindowSpec {
        train_bars,
        test_bars: 5,
        step_bars: 5,
        mode: WindowMode::Rolling,
    };
    let (s, bars) = weekly_wf_spec(10, rolling(5));
    assert_eq!(
        refused_before_running(&s, &bars),
        "a Sharpe objective needs at least 30 bars as well as 30 daily returns in every train \
         window, and a train window of 5 1w bars is shorter"
    );
    // Anchored, the spec allows a first train window of up to 34 bars; over exactly the bars one
    // window needs it has 25, which the run refuses before running.
    let (s, bars) = weekly_wf_spec(
        35,
        WindowSpec {
            train_bars: 25,
            test_bars: 10,
            step_bars: 10,
            mode: WindowMode::Anchored,
        },
    );
    assert_eq!(s.check(), Ok(()));
    assert_eq!(
        refused_before_running(&s, &bars),
        "a Sharpe objective needs at least 30 daily returns on days with a bar close in every \
         train window, and train window 0 (25 bars over 175 days) has 25"
    );
    // Thirty weekly bars are enough: the train window has a Sharpe ratio.
    let (s, bars) = weekly_wf_spec(35, rolling(30));
    let r = walk_forward(&s, &bars, &[], &[], None).unwrap();
    assert!(r.windows[0].in_sample.sharpe.is_some());
}

#[test]
fn an_out_of_sample_test_whose_in_sample_segment_is_too_short_for_its_objective_is_refused_before_running()
 {
    let oos = |days: i64, metric| {
        let n = days * 24;
        let mut sweep = rising_spec();
        sweep.base.to = T0 + n * HOUR;
        sweep.objective.metric = metric;
        let s = OosSpec {
            sweep,
            split: Split::Fraction(0.5),
        };
        let calls = Mutex::new(0usize);
        let progress = |_: usize, _: usize| {
            *calls.lock().unwrap() += 1;
            ControlFlow::Continue(())
        };
        let r = out_of_sample(&s, &rising_bars(n), &[], &[], Some(&progress));
        (r, calls.into_inner().unwrap())
    };
    // 40 days of hourly bars split in half: the in-sample segment has 20 daily returns.
    let (r, calls) = oos(40, Metric::Sharpe);
    assert_eq!(calls, 0, "ran before refusing");
    assert_eq!(
        r.unwrap_err(),
        TesterError::InvalidSpec(
            "a Sharpe objective needs at least 30 daily returns in the in-sample segment, and it \
             (480 bars over 20 days) has 20"
                .into()
        )
    );
    let (r, calls) = oos(120, Metric::Cagr);
    assert_eq!(calls, 0, "ran before refusing");
    assert_eq!(
        r.unwrap_err(),
        TesterError::InvalidSpec(
            "a CAGR objective needs an in-sample segment of at least 90 days, and it is 1440 bars \
             over 60 days"
                .into()
        )
    );
    // 70 days split in half: 35 daily returns in sample, and the whole test runs.
    let (r, calls) = oos(70, Metric::Sharpe);
    assert!(r.unwrap().in_sample.best.unwrap().metrics.sharpe.is_some());
    assert_eq!(calls, 6);
}

/// Hourly [`rising_bars`] over the first `n` hours, without those `keep` rejects.
fn gappy_bars(n: i64, keep: impl Fn(i64) -> bool) -> Vec<Candle> {
    rising_bars(n)
        .into_iter()
        .filter(|b| keep((b.open_time - T0) / HOUR))
        .collect()
}

#[test]
fn a_segment_whose_daily_returns_are_mostly_carried_across_a_gap_is_refused_before_running() {
    // In sample, 30 hourly bars, 15 on day 0 and 15 on day 29: 30 daily returns and 30 bars, but
    // only two days hold a bar close. Then 50 bars out of sample.
    let split = 29 * 24 + 15;
    let bars = gappy_bars(split + 50, |h| !(15..29 * 24).contains(&h));
    let mut sweep = rising_spec();
    sweep.base.to = T0 + (split + 50) * HOUR;
    sweep.objective.metric = Metric::Sharpe;
    let s = OosSpec {
        sweep,
        split: Split::At(T0 + split * HOUR),
    };
    let calls = Mutex::new(0usize);
    let progress = |_: usize, _: usize| {
        *calls.lock().unwrap() += 1;
        ControlFlow::Continue(())
    };
    let r = out_of_sample(&s, &bars, &[], &[], Some(&progress));
    assert_eq!(calls.into_inner().unwrap(), 0, "ran before refusing");
    assert_eq!(
        r.unwrap_err(),
        TesterError::InvalidSpec(
            "a Sharpe objective needs at least 30 daily returns on days with a bar close in the \
             in-sample segment, and it (30 bars over 29 days 15 hours) has 2"
                .into()
        )
    );

    // A train window of 696 hourly bars passes the spec check. Without day 14 they run over 30
    // days, so 30 daily returns, but close on 29 of them.
    let (mut s, _) = train_floor_spec(Metric::Sharpe, 696);
    s.sweep.base.to = T0 + 770 * HOUR;
    let bars = gappy_bars(770, |h| h / 24 != 14);
    assert_eq!(s.check(), Ok(()));
    assert_eq!(
        refused_before_running(&s, &bars),
        "a Sharpe objective needs at least 30 daily returns on days with a bar close in every \
         train window, and train window 0 (696 bars over 30 days) has 29"
    );
}

#[test]
fn a_walk_forward_whose_train_windows_in_the_data_are_too_short_for_its_objective_is_refused_before_running()
 {
    // 696 hourly bars could hold 30 daily returns, so the spec passes its check, but from 00:00
    // they are exactly 29 days: 29 daily returns, one short.
    let (s, bars) = train_floor_spec(Metric::Sortino, 696);
    assert_eq!(s.check(), Ok(()));
    assert_eq!(
        refused_before_running(&s, &bars),
        "a Sortino objective needs at least 30 daily returns in every train window, and train \
         window 0 (696 bars over 29 days) has 29"
    );
    // Anchored over exactly the bars one window needs, the first train window has no leftover
    // bars: 2112 bars, 88 days.
    let (mut s, bars) = train_floor_spec(Metric::Cagr, 2_112);
    s.windows.mode = WindowMode::Anchored;
    assert_eq!(s.check(), Ok(()));
    assert_eq!(
        refused_before_running(&s, &bars),
        "a CAGR objective needs train windows of at least 90 days, and train window 0 is 2112 \
         bars over 88 days"
    );

    // One more bar reaches into a 30th day: the walk-forward runs and the train window has a
    // Sharpe ratio, so the run refuses exactly what the train sweeps could not rank.
    let (s, bars) = train_floor_spec(Metric::Sharpe, 697);
    let r = walk_forward(&s, &bars, &[], &[], None).unwrap();
    assert!(r.windows[0].in_sample.sharpe.is_some());
    let (s, bars) = train_floor_spec(Metric::Cagr, 2_160);
    let r = walk_forward(&s, &bars, &[], &[], None).unwrap();
    assert!(r.windows[0].in_sample.cagr.is_some());
    // Objectives without a floor are not checked.
    let (s, bars) = train_floor_spec(Metric::TotalReturn, 50);
    assert!(walk_forward(&s, &bars, &[], &[], None).is_ok());
}

#[test]
fn a_walk_forward_saved_before_it_had_a_benchmark_loads_without_one() {
    let dir = tempfile::tempdir().unwrap();
    let wf = walk_forward(&rising_wf_spec(), &rising_bars(300), &[], &[], None).unwrap();
    let id = save(&SavedRun::WalkForward(Box::new(wf.clone())), dir.path()).unwrap();
    // Rewrite result.json as a save from before walk-forwards had a benchmark: no `benchmark` key
    // on the walk-forward result (each window's backtest keeps its own).
    let path = dir.path().join(id.as_str()).join("result.json");
    let mut json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let result = json["result"].as_object_mut().unwrap();
    assert!(result.remove("benchmark").is_some());
    std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
    let SavedRun::WalkForward(loaded) = load(dir.path(), id.as_str()).unwrap() else {
        panic!("not a walk-forward");
    };
    assert_eq!(loaded.benchmark, None);
    assert_eq!(
        *loaded,
        WalkForwardResult {
            benchmark: None,
            ..wf
        }
    );
}

#[test]
fn walk_forward_tests_up_to_the_last_bar_and_warms_the_first_train_sweep_on_the_leftover() {
    // 310 bars with 50-bar windows: 10 bars are left over at the start, not at the end.
    let bars = rising_bars(310);
    let mut s = rising_wf_spec();
    s.sweep.base.to = T0 + 310 * HOUR;
    let r = walk_forward(&s, &bars, &[], &[], None).unwrap();
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
        &[],
    )
    .unwrap();
    assert_ne!(first.in_sample.total_return, cold.metrics.total_return);
}

#[test]
fn rows_carry_their_coordinates_on_each_axis() {
    // slow [20, 5, 10] x equity_fraction [0.5, 0.9]: the last axis changes fastest, so index i
    // is at (i / 2, i % 2).
    let r = sweep(&rising_spec(), &rising_bars(300), &[], &[], None).unwrap();
    let slows = [20, 5, 10];
    let fractions = [0.5, 0.9];
    for row in &r.rows {
        let i = row.index;
        assert_eq!(row.coords, vec![i / 2, i % 2]);
        assert_eq!(int(&row.params, "slow"), slows[row.coords[0] as usize]);
        assert_eq!(
            row.params.float("equity_fraction"),
            Ok(fractions[row.coords[1] as usize])
        );
    }
    // The best row, index 3, is slow 5 with fraction 0.9.
    assert_eq!(r.rows[0].coords, vec![1, 1]);

    // fast 3..=8 x slow [12, 18, 24, 30] x allow_short [false, true]: 13 = 1 * 8 + 2 * 2 + 1.
    let r = sweep(&wavy_spec(), &wavy_bars(500), &[], &[], None).unwrap();
    let row = r.rows.iter().find(|row| row.index == 13).unwrap();
    assert_eq!(row.coords, vec![1, 2, 1]);
    assert_eq!(int(&row.params, "fast"), 4);
    assert_eq!(int(&row.params, "slow"), 24);
    assert_eq!(row.params.bool("allow_short"), Ok(true));
}

#[test]
fn walk_forward_counts_the_trades_closed_at_each_test_end() {
    let bars = wavy_bars(500);
    let r = walk_forward(&wf_spec(WindowMode::Rolling), &bars, &[], &[], None).unwrap();
    let mut forced = 0;
    for w in &r.windows {
        let o = &w.out_of_sample;
        let closed_out: Vec<_> = o.trades.iter().filter(|t| t.force_closed).collect();
        assert!(closed_out.len() <= 1);
        // Closed at the open of the window's last test bar.
        for t in &closed_out {
            assert_eq!(t.exit_time, w.test_to - HOUR);
        }
        assert_eq!(o.metrics.force_closed_trades, closed_out.len());
        forced += closed_out.len();
    }
    assert!(forced > 0, "no test window ended in a position");
    assert_eq!(r.metrics.force_closed_trades, forced);
    assert_eq!(r.trades.iter().filter(|t| t.force_closed).count(), forced);
}

#[test]
fn walk_forward_scales_each_windows_extremes_like_its_equity() {
    let bars = wavy_bars(500);
    let r = walk_forward(&wf_spec(WindowMode::Rolling), &bars, &[], &[], None).unwrap();
    assert_eq!(r.equity_extremes.len(), r.equity.len());
    let mut level = 10_000.0;
    let mut i = 0;
    let mut j = 0;
    let mut peak: f64 = 0.0;
    for w in &r.windows {
        let o = &w.out_of_sample;
        let scale = level / 10_000.0;
        for x in &o.equity_extremes {
            assert!((r.equity_extremes[i].low - x.low * scale).abs() < 1e-9);
            assert!((r.equity_extremes[i].high - x.high * scale).abs() < 1e-9);
            i += 1;
        }
        for t in &o.trades {
            assert!((r.trades[j].mae.unwrap() - t.mae.unwrap() * scale).abs() < 1e-9);
            assert!((r.trades[j].mfe.unwrap() - t.mfe.unwrap() * scale).abs() < 1e-9);
            assert_eq!(r.trades[j].r_multiple, t.r_multiple);
            j += 1;
        }
        peak = peak.max(o.metrics.peak_leverage.unwrap());
        level = r.equity[i - 1].equity;
    }
    assert_eq!(r.metrics.peak_leverage, Some(peak));
    let m = &r.metrics;
    assert!(m.max_drawdown_intrabar.unwrap() >= m.max_drawdown.unwrap());
    assert!(m.static_drawdown_intrabar.is_some());
    // The stitched buy and hold has its own extremes, and alpha and beta against it.
    let b = r.benchmark.as_ref().unwrap();
    assert!(b.metrics.peak_leverage.unwrap() > 0.98);
    assert!(b.metrics.max_drawdown_intrabar.is_some());
    assert!(b.alpha.is_some() && b.beta.is_some());
}
