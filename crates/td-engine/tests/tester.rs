//! The strategy tester end to end: sweeps with a known best, validation, reproducible random
//! sampling, parallel against serial, out-of-sample warm-up, walk-forward windows and stitching,
//! and saved tester results.

mod common;

use std::sync::Mutex;

use td_core::{Candle, Millis, Timeframe};
use td_engine::{
    BacktestConfig, Direction, EngineError, MAX_RUNS, Metric, Objective, OosSpec, ParamAxis,
    ParamSpace, ParamValue, Params, RunKind, SavedRun, Session, Split, SweepMode, SweepSpec,
    TesterError, WalkForwardSpec, WindowMode, WindowSpec, list, load, out_of_sample, run, save,
    sweep, walk_forward,
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
        warmup_from: None,
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
    let progress = |done: usize, total: usize| calls.lock().unwrap().push((done, total));
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
    let expected = o.metrics.total_return.unwrap() / best.metrics.total_return.unwrap();
    assert_eq!(r.degradation, Some(expected));

    // A split at a time gives the same segments.
    let at = OosSpec {
        split: Split::At(T0 + 150 * HOUR),
        ..oos.clone()
    };
    let r2 = out_of_sample(&at, &bars, &[], None).unwrap();
    assert_eq!(r2.out_of_sample, r.out_of_sample);
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
    s.sweep.min_trades = 1;
    assert!(matches!(
        out_of_sample(&s, &bars, &[], None),
        Err(TesterError::NoQualifyingRun {
            min_trades: 1,
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
}

#[test]
fn warm_up_shows_bars_without_trading_or_equity() {
    use td_core::{OrderId, OrderType, Side};
    let inst = common::spot();
    let order = common::req(&inst, Side::Buy, 1.0, OrderType::Market);
    let (script, log) = common::Script::new(vec![(0, order.clone()), (2, order)]);
    let mut session = Session::new(script, inst, Timeframe::M1, common::config(0.0)).unwrap();
    session
        .warm_up(common::bar(0, 100.0, 101.0, 99.0, 100.0), &[])
        .unwrap();
    session
        .warm_up(common::bar(1, 100.0, 101.0, 99.0, 100.0), &[])
        .unwrap();
    assert!(session.equity().is_empty());
    session
        .on_bar(common::bar(2, 100.0, 101.0, 99.0, 100.0), &[])
        .unwrap();
    assert_eq!(session.bars().len(), 3);
    assert_eq!(session.equity().len(), 1);
    let log = log.lock().unwrap();
    assert_eq!(log[0].as_ref().unwrap_err().id, OrderId(0));
    assert_eq!(log[1], Ok(OrderId(1)));
    assert_eq!(session.orders().len(), 1);
    // Warm-up after trading has started is refused.
    assert!(matches!(
        session.warm_up(common::bar(3, 100.0, 101.0, 99.0, 100.0), &[]),
        Err(EngineError::InvalidConfig(_))
    ));
}
