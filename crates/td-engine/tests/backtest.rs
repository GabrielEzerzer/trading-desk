//! The backtest runner end to end: a deterministic synthetic bar series checked against a
//! committed expected result, saved runs that reload and rerun exactly, and the data fingerprint.

mod common;

use std::path::PathBuf;

use serde_json::Value;
use td_core::{Candle, Millis, Timeframe};
use td_engine::{
    BacktestConfig, BacktestResult, ENGINE_REVISION, EngineError, ParamValue, Params, RunKind,
    SavedRun, build_strategy, config_fingerprint, data_fingerprint, list, load, rerun, run,
    run_strategy, save, save_with_files,
};

const HOUR: Millis = 3_600_000;
/// 2024-01-01 00:00 UTC.
const T0: Millis = 1_704_067_200_000;
const BARS: i64 = 500;

/// A triangle wave with period 1 between -1 and 1. Only floor, abs and arithmetic, which IEEE 754
/// defines exactly, so the series is the same on every platform (unlike `sin`).
fn tri(x: f64) -> f64 {
    let f = x - x.floor();
    4.0 * (f - 0.5).abs() - 1.0
}

fn price(i: i64) -> f64 {
    let x = i as f64;
    100.0 + 0.01 * x + 5.0 * tri(x / 96.0) + 2.0 * tri(x / 13.0) + 1.2 * tri(x / 4.7)
}

/// Hourly bars: a rising trend with three triangle waves of different periods on top, so the
/// strategy sees both clean trends and whipsaws. Each bar opens at the
/// previous close.
fn synthetic_bars() -> Vec<Candle> {
    (0..BARS)
        .map(|i| {
            let close = price(i);
            let open = if i == 0 { close - 0.2 } else { price(i - 1) };
            Candle {
                open_time: T0 + i * HOUR,
                open,
                high: open.max(close) + 0.25,
                low: open.min(close) - 0.25,
                close,
                volume: 10.0,
                quote_volume: 10.0 * close,
            }
        })
        .collect()
}

/// Funding every 8 hours from 8 hours before the first bar, mostly positive.
fn synthetic_funding() -> Vec<(Millis, f64)> {
    (-1..BARS / 8 + 2)
        .map(|k| {
            let rate = if k % 3 == 0 { -0.00005 } else { 0.0001 };
            (T0 + k * 8 * HOUR, rate)
        })
        .collect()
}

/// SMA cross with shorts on a perpetual, over bars 10 to 489 of the series.
fn config() -> BacktestConfig {
    BacktestConfig {
        strategy: "sma_cross".into(),
        params: Params::new()
            .with("fast", ParamValue::Int(6))
            .with("slow", ParamValue::Int(24))
            .with("allow_short", ParamValue::Bool(true))
            .with("equity_fraction", ParamValue::Float(0.9)),
        instrument: common::perp(),
        timeframe: Timeframe::H1,
        from: T0 + 10 * HOUR,
        to: T0 + 490 * HOUR,
        starting_cash: 10_000.0,
        slippage_bps: 5.0,
        fee_override: None,
        max_leverage: 1.0,
        warmup_from: None,
        close_at_end: false,
        account_currency: td_core::AccountCurrency::Quote,
    }
}

fn run_synthetic() -> BacktestResult {
    run(&config(), &synthetic_bars(), &[], &synthetic_funding()).unwrap()
}

fn expected_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/backtest_expected.json")
}

/// Compares two JSON trees. Numbers match within a relative 1e-12, because `powf` (in CAGR) comes
/// from the platform's maths library and may differ in the last bit between Windows and Linux;
/// everything else must be equal.
fn assert_json_eq(actual: &Value, expected: &Value, path: &str) {
    match (actual, expected) {
        (Value::Number(a), Value::Number(e)) => {
            let (a, e) = (a.as_f64().unwrap(), e.as_f64().unwrap());
            assert!(
                (a - e).abs() <= 1e-12 * e.abs().max(1.0),
                "{path}: expected {e}, got {a}"
            );
        }
        (Value::Array(a), Value::Array(e)) => {
            assert_eq!(a.len(), e.len(), "{path}: array length");
            for (i, (a, e)) in a.iter().zip(e).enumerate() {
                assert_json_eq(a, e, &format!("{path}[{i}]"));
            }
        }
        (Value::Object(a), Value::Object(e)) => {
            let keys = |m: &serde_json::Map<String, Value>| m.keys().cloned().collect::<Vec<_>>();
            assert_eq!(keys(a), keys(e), "{path}: keys");
            for (k, e) in e {
                assert_json_eq(&a[k], e, &format!("{path}.{k}"));
            }
        }
        _ => assert_eq!(actual, expected, "{path}"),
    }
}

/// Rewrites the expected result file. Run it on purpose after a deliberate change to fills,
/// trades or metrics, then review the diff of the file before committing it:
/// `cargo test -p td-engine --test backtest -- --ignored regenerate_expected_result`
///
/// A change that needs this is an engine behaviour change: bump `ENGINE_REVISION` in
/// `src/runner.rs` in the same change, before regenerating, so saved results from the old engine
/// show `engine_matches: false` on a rerun. Behaviour changes this fixture does not exercise need
/// the bump too.
#[test]
#[ignore = "regenerates tests/fixtures/backtest_expected.json"]
fn regenerate_expected_result() {
    let json = serde_json::to_string_pretty(&run_synthetic()).unwrap();
    std::fs::create_dir_all(expected_path().parent().unwrap()).unwrap();
    std::fs::write(expected_path(), json + "\n").unwrap();
}

#[test]
fn full_backtest_matches_the_expected_result_file() {
    let result = run_synthetic();
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(expected_path()).unwrap()).unwrap();
    assert_json_eq(&serde_json::to_value(&result).unwrap(), &expected, "result");
}

/// A strategy the caller built runs exactly as the registry's would, and an unregistered name is
/// only a label then.
#[test]
fn run_strategy_with_a_built_strategy_matches_run() {
    let mut c = config();
    let strategy = build_strategy(&c.strategy, &c.params).unwrap();
    c.strategy = "not_registered".into();
    let built = run_strategy(&c, strategy, &synthetic_bars(), &[], &synthetic_funding()).unwrap();
    let registered = run_synthetic();
    assert!(!built.fills.is_empty());
    assert_eq!(built.fills, registered.fills);
    assert_eq!(built.equity, registered.equity);
    assert_eq!(built.metrics, registered.metrics);
    assert_eq!(built.config.strategy, "not_registered");
}

#[test]
fn full_backtest_is_internally_consistent() {
    let r = run_synthetic();
    // Bars 10 to 489 are in range.
    assert_eq!(r.equity.len(), 480);
    assert_eq!(r.equity[0].time, T0 + 10 * HOUR);
    // The run trades in both directions, flips through zero and pays funding.
    assert!(r.trades.len() >= 4, "{} trades", r.trades.len());
    assert!(
        r.trades
            .iter()
            .any(|t| t.side == td_engine::TradeSide::Short)
    );
    assert!(
        r.trades
            .windows(2)
            .any(|w| w[0].exit_time == w[1].entry_time)
    );
    assert!(r.metrics.funding_paid != 0.0);
    assert_trades_add_up(&r);
    // The benchmark is long from the first bar to the last.
    assert_eq!(r.benchmark.metrics.exposure, Some(1.0));
    assert_eq!(r.benchmark.equity.len(), r.equity.len());
    assert!(r.benchmark.correlation.is_some());
}

/// Trade P&L adds up to the change in equity, with an open trade marked at the last close, and
/// the trades' fees and funding add up to the run's.
#[track_caller]
fn assert_trades_add_up(r: &BacktestResult) {
    let net: f64 = r.trades.iter().map(|t| t.net_pnl).sum();
    let change = r.equity.last().unwrap().equity - r.config.starting_cash;
    assert!((net - change).abs() < 1e-6, "trades {net}, equity {change}");
    let funding: f64 = r.trades.iter().map(|t| t.funding).sum();
    assert!((funding - r.metrics.funding_paid).abs() < 1e-9);
    let fees: f64 = r.trades.iter().map(|t| t.fees).sum();
    assert!((fees - r.metrics.fees_paid).abs() < 1e-9);
    assert_eq!(
        r.metrics.trades,
        r.trades.iter().filter(|t| !t.open).count()
    );
}

#[test]
fn trades_close_on_attached_stops_and_the_result_round_trips() {
    // Donchian attaches a protective stop to every entry; a tight stop gets hit on this series.
    let mut c = config();
    c.strategy = "donchian_breakout".into();
    c.params = Params::new()
        .with("entry", ParamValue::Int(12))
        .with("exit", ParamValue::Int(6))
        .with("atr_stop", ParamValue::Float(1.0))
        .with("allow_short", ParamValue::Bool(true));
    let r = run(&c, &synthetic_bars(), &[], &synthetic_funding()).unwrap();
    let stops: Vec<_> = r
        .orders
        .iter()
        .filter_map(|o| o.attached_stop)
        .map(|s| s.id)
        .collect();
    let stop_fills = r
        .fills
        .iter()
        .filter(|f| stops.contains(&f.order_id))
        .count();
    assert!(stop_fills > 0, "no attached stop filled");
    // Every fill of an attached stop closes a trade on its bar.
    for f in r.fills.iter().filter(|f| stops.contains(&f.order_id)) {
        assert!(r.trades.iter().any(|t| !t.open && t.exit_time == f.time));
    }
    assert_trades_add_up(&r);
    let json = serde_json::to_string(&r).unwrap();
    let loaded: BacktestResult = serde_json::from_str(&json).unwrap();
    assert_eq!(loaded, r);
}

#[test]
fn save_load_and_rerun_reproduce_the_result_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path().join("runs");
    let result = run_synthetic();
    let saved = SavedRun::Backtest(Box::new(result.clone()));
    let original = serde_json::to_string(&result).unwrap();

    let id = save(&saved, &runs).unwrap();
    let config_json = std::fs::read_to_string(runs.join(id.as_str()).join("config.json")).unwrap();
    assert_eq!(
        serde_json::from_str::<BacktestConfig>(&config_json).unwrap(),
        config()
    );
    let SavedRun::Backtest(loaded) = load(&runs, id.as_str()).unwrap() else {
        panic!("saved as a backtest");
    };
    assert_eq!(serde_json::to_string(&loaded).unwrap(), original);
    assert_eq!(*loaded, result);

    let again = rerun(&loaded, &synthetic_bars(), &[], &synthetic_funding()).unwrap();
    assert!(again.fingerprint_matches);
    assert!(again.engine_matches);
    assert_eq!(serde_json::to_string(&again.result).unwrap(), original);

    let listed = list(&runs).unwrap();
    assert_eq!(listed.len(), 1);
    let s = &listed[0];
    assert_eq!(s.id, id);
    assert_eq!(s.kind, RunKind::Backtest);
    assert_eq!(s.strategy, "sma_cross");
    assert_eq!(s.instrument, config().instrument.id);
    assert_eq!(s.timeframe, Timeframe::H1);
    assert_eq!((s.from, s.to), (config().from, config().to));
    assert_eq!(s.total_return, result.metrics.total_return);
    assert_eq!(s.trades, result.metrics.trades);
    assert!(s.created_at > T0);
    assert_eq!(&saved.summary(id.clone()), s);
    assert_eq!(
        config_fingerprint(&config(), &synthetic_bars(), &[], &synthetic_funding()).unwrap(),
        result.data_fingerprint
    );
    // No temporary directory is left behind.
    assert_eq!(std::fs::read_dir(&runs).unwrap().count(), 1);
}

#[test]
fn list_is_newest_first_and_skips_other_entries() {
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path();
    let result = SavedRun::Backtest(Box::new(run_synthetic()));
    let first = save(&result, runs).unwrap();
    // Two saves of the same result in the same millisecond get the same id; wait and retry, a
    // bounded number of times. Any other error fails at once.
    let second = (0..100)
        .find_map(|_| match save(&result, runs) {
            Ok(id) => Some(id),
            Err(td_engine::StoreError::RunExists(_)) => {
                std::thread::sleep(std::time::Duration::from_millis(2));
                None
            }
            Err(e) => panic!("saving the second run failed: {e}"),
        })
        .expect("the second save still had the first run's id after 100 tries");
    std::fs::create_dir(runs.join(".tmp-1762790000123-0a1b2c3d")).unwrap();
    // A save's temporary directory, here holding a whole run, is not listed.
    let tmp = runs.join(".tmp-1762790000123-0a1b2c3d-4242-7");
    std::fs::create_dir(&tmp).unwrap();
    std::fs::copy(
        runs.join(first.as_str()).join("result.json"),
        tmp.join("result.json"),
    )
    .unwrap();
    std::fs::create_dir(runs.join("not-a-run")).unwrap();
    std::fs::create_dir(runs.join("1762790000123-0a1b2c3d")).unwrap();
    let ids: Vec<_> = list(runs).unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![second, first]);
    assert!(matches!(
        load(runs, "../outside"),
        Err(td_engine::StoreError::InvalidRunId(_))
    ));
}

#[test]
fn concurrent_saves_of_the_same_run_neither_fail_nor_clobber_each_other() {
    let saved = SavedRun::Backtest(Box::new(run_synthetic()));
    let mut same_id = 0;
    for _ in 0..200 {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path();
        let barrier = std::sync::Barrier::new(2);
        let (a, b) = std::thread::scope(|s| {
            let save_after_barrier = || {
                barrier.wait();
                save_with_files(&saved, runs, &[("data.json", "{}".as_bytes())])
            };
            let a = s.spawn(save_after_barrier);
            let b = s.spawn(save_after_barrier);
            (a.join().unwrap(), b.join().unwrap())
        });
        let mut ids = Vec::new();
        for r in [a, b] {
            match r {
                Ok(id) => ids.push(id),
                Err(td_engine::StoreError::RunExists(_)) => same_id += 1,
                Err(e) => panic!("a concurrent save failed: {e}"),
            }
        }
        // One save at least succeeds, and two that do have different ids.
        assert!(!ids.is_empty());
        ids.sort();
        ids.dedup();
        for id in &ids {
            assert_eq!(load(runs, id.as_str()).unwrap(), saved);
            let run_dir = runs.join(id.as_str());
            assert_eq!(std::fs::read(run_dir.join("data.json")).unwrap(), b"{}");
            assert!(run_dir.join("config.json").is_file());
        }
        // Only the saved runs are left: no temporary directory.
        let mut left: Vec<_> = std::fs::read_dir(runs)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let ids: Vec<_> = ids.iter().map(|id| id.to_string()).collect();
        assert_eq!(left, ids);
    }
    eprintln!("{same_id} of 200 rounds saved both runs in the same millisecond");
}

#[test]
fn extra_files_are_saved_with_the_run_and_only_under_plain_new_names() {
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path();
    let saved = SavedRun::Backtest(Box::new(run_synthetic()));
    let id = save_with_files(&saved, runs, &[("data.json", "{}".as_bytes())]).unwrap();
    let run_dir = runs.join(id.as_str());
    assert_eq!(std::fs::read(run_dir.join("data.json")).unwrap(), b"{}");
    assert!(run_dir.join("result.json").is_file());

    for name in [
        "result.json",
        "config.json",
        "Result.JSON",
        "CONFIG.json",
        "../escape.json",
        "a/b.json",
        "..",
    ] {
        assert!(
            matches!(
                save_with_files(&saved, runs, &[(name, "{}".as_bytes())]),
                Err(td_engine::StoreError::InvalidFileName(_))
            ),
            "{name}"
        );
    }
    // Nothing but the first run was written.
    assert_eq!(std::fs::read_dir(runs).unwrap().count(), 1);
}

#[test]
fn funding_in_gaps_is_charged_at_the_next_open_and_attributed_to_the_trade_held() {
    // Every bar that opens on a funding time (each 8 hours) is missing, so every funding event in
    // range falls in a gap and is due at the next bar's open.
    let bars: Vec<Candle> = synthetic_bars()
        .into_iter()
        .filter(|b| (b.open_time - T0) % (8 * HOUR) != 0)
        .collect();
    let funding = synthetic_funding();
    let mut c = config();
    c.params = c.params.with("allow_short", ParamValue::Bool(false));
    let r = run(&c, &bars, &[], &funding).unwrap();
    assert!(r.metrics.funding_paid != 0.0);
    // Long-only, so some exits leave the position flat on a bar right after a gap: the funding due
    // at that open belongs to the trade that was closed, not to nothing.
    assert_trades_add_up(&r);

    // The benchmark holds the same quantity throughout and pays each event at the next open. The
    // first used bar is at 10:00; events from 16:00 to bar 488 are in range, each charged at bar
    // 8k + 1, which opens at price(8k).
    let expected: f64 = funding
        .iter()
        .filter(|&&(t, _)| t > c.from && t < c.to)
        .map(|&(t, rate)| r.benchmark.qty * price((t - T0) / HOUR) * rate)
        .sum();
    assert!(expected != 0.0);
    let paid = r.benchmark.metrics.funding_paid;
    assert!(
        (paid - expected).abs() < 1e-9,
        "benchmark paid {paid}, expected {expected}"
    );

    // Gap funding is part of the data the run used.
    let mut rates = funding.clone();
    let k = rates
        .iter()
        .position(|&(t, _)| t == T0 + 16 * HOUR)
        .unwrap();
    rates[k].1 += 1e-6;
    assert!(!rerun(&r, &bars, &[], &rates).unwrap().fingerprint_matches);
}

#[test]
fn a_result_from_another_engine_revision_does_not_match() {
    let mut saved = run_synthetic();
    assert_eq!(saved.engine_revision, ENGINE_REVISION);
    // A saved run from an engine that behaved differently, on the same data.
    saved.engine_revision = ENGINE_REVISION + 1;
    let again = rerun(&saved, &synthetic_bars(), &[], &synthetic_funding()).unwrap();
    assert!(again.fingerprint_matches);
    assert!(!again.engine_matches);
    assert_eq!(again.result.engine_revision, ENGINE_REVISION);
}

#[test]
fn a_changed_bar_changes_the_fingerprint() {
    let bars = synthetic_bars();
    let funding = synthetic_funding();
    let base = run(&config(), &bars, &[], &funding).unwrap();

    // Bar 200 is in range: a close moved by one cent changes the fingerprint.
    let mut changed = bars.clone();
    changed[200].close += 0.01;
    let r = rerun(&base, &changed, &[], &funding).unwrap();
    assert!(!r.fingerprint_matches);
    assert_ne!(r.result.data_fingerprint, base.data_fingerprint);

    // Bar 495 is outside the range and funding before the first bar is not charged, so neither
    // is part of the data the run used.
    let mut outside = bars.clone();
    outside[495].close += 0.01;
    let mut extra_funding = funding.clone();
    extra_funding.push((T0 - 3 * HOUR, 0.5));
    let r = rerun(&base, &outside, &[], &extra_funding).unwrap();
    assert!(r.fingerprint_matches);

    // A changed funding rate inside the range changes it too.
    let mut rates = funding.clone();
    rates[5].1 += 1e-6;
    assert!(
        !rerun(&base, &bars, &[], &rates)
            .unwrap()
            .fingerprint_matches
    );
    assert_ne!(
        data_fingerprint(&bars, &[], &[]),
        data_fingerprint(&bars[1..], &[], &[])
    );
}

#[test]
fn bad_configurations_are_errors() {
    let bars = synthetic_bars();
    let mut c = config();
    c.to = c.from;
    assert!(matches!(
        run(&c, &bars, &[], &[]),
        Err(EngineError::InvalidConfig(_))
    ));
    let mut c = config();
    c.from = T0 + 1000 * HOUR;
    c.to = T0 + 1100 * HOUR;
    assert!(matches!(
        run(&c, &bars, &[], &[]),
        Err(EngineError::NoBarsInRange { .. })
    ));
    let mut c = config();
    c.strategy = "nope".into();
    assert!(matches!(
        run(&c, &bars, &[], &[]),
        Err(EngineError::UnknownStrategy(_))
    ));
}

#[test]
fn spot_benchmark_buys_with_all_cash_at_the_first_open() {
    let mut c = config();
    c.instrument = common::spot();
    c.params = Params::new();
    let r = run(&c, &synthetic_bars(), &[], &[]).unwrap();
    let b = &r.benchmark;
    // Bar 10 opens at price(9); 5 bps slippage rounded up to the cent, then all cash after the
    // 0.2% taker fee, rounded down to 0.001.
    let open = price(9);
    let entry = (open * 1.0005 * 100.0).ceil() / 100.0;
    assert!((b.entry_price - entry).abs() < 1e-9);
    let qty = (10_000.0 / (entry * 1.002) * 1000.0).floor() / 1000.0;
    assert!((b.qty - qty).abs() < 1e-12);
    // Equity at the last close: leftover cash plus the holding.
    let cash = 10_000.0 - qty * entry * 1.002;
    let last = price(489);
    let end = b.equity.last().unwrap().equity;
    assert!((end - (cash + qty * last)).abs() < 1e-6);
    let expected_excess = r.metrics.total_return.unwrap() - b.metrics.total_return.unwrap();
    assert_eq!(b.excess_return, Some(expected_excess));
}

#[test]
fn a_two_bar_one_minute_run_has_no_infinite_metric_and_round_trips() {
    // The benchmark gains about 0.8% in two minutes, which would compound past f64::MAX a year;
    // the run is far under the 90 days a CAGR needs, so it has none (the overflow guard itself
    // is checked in the metrics unit tests).
    let bars = [
        common::bar(0, 100.0, 100.6, 99.9, 100.5),
        common::bar(1, 100.5, 101.0, 100.4, 101.0),
    ];
    let mut c = config();
    c.instrument = common::spot();
    c.params = Params::new();
    c.timeframe = Timeframe::M1;
    c.from = 0;
    c.to = 2 * common::MIN;
    c.slippage_bps = 0.0;
    let r = run(&c, &bars, &[], &[]).unwrap();
    assert!(r.benchmark.metrics.total_return.unwrap() > 0.007);
    assert_eq!(r.benchmark.metrics.cagr, None);
    assert_eq!(r.benchmark.metrics.calmar, None);

    let dir = tempfile::tempdir().unwrap();
    let saved = SavedRun::Backtest(Box::new(r));
    let id = save(&saved, dir.path()).unwrap();
    assert_eq!(load(dir.path(), id.as_str()).unwrap(), saved);
}

fn donchian(c: &mut BacktestConfig) {
    c.strategy = "donchian_breakout".into();
    c.params = Params::new()
        .with("entry", ParamValue::Int(12))
        .with("exit", ParamValue::Int(6))
        .with("atr_stop", ParamValue::Float(1.0))
        .with("allow_short", ParamValue::Bool(true));
}

#[test]
fn trades_keep_the_stop_attached_to_their_entry_and_its_r_multiple() {
    let mut c = config();
    donchian(&mut c);
    let r = run(&c, &synthetic_bars(), &[], &synthetic_funding()).unwrap();
    let mut with_stop = 0;
    for t in &r.trades {
        // The stop attached to the order whose fill opened the trade.
        let attached = r
            .orders
            .iter()
            .filter(|o| {
                r.fills
                    .iter()
                    .any(|f| f.order_id == o.id && f.time == t.entry_time)
            })
            .find_map(|o| o.attached_stop);
        let Some(stop) = attached else { continue };
        with_stop += 1;
        assert_eq!(
            t.stop,
            Some(stop.trigger),
            "trade entered at {}",
            t.entry_time
        );
        let risk = t.qty * (t.entry_price - stop.trigger).abs();
        let r_multiple = t.r_multiple.unwrap();
        assert!((r_multiple - t.net_pnl / risk).abs() < 1e-12);
    }
    assert!(with_stop >= 4, "{with_stop} trades with a stop");
    let rs: Vec<f64> = r
        .trades
        .iter()
        .filter(|t| !t.open)
        .filter_map(|t| t.r_multiple)
        .collect();
    let mean = rs.iter().sum::<f64>() / rs.len() as f64;
    assert!((r.metrics.expectancy_r.unwrap() - mean).abs() < 1e-12);
    // An exit price is one the position met, so the excursions cover the trade's own result.
    for t in &r.trades {
        assert!(t.mae.unwrap() >= (-t.gross_pnl).max(0.0) - 1e-9);
        assert!(t.mfe.unwrap() >= t.gross_pnl.max(0.0) - 1e-9);
    }
    // The SMA cross places no stops, so no trade has one.
    let sma = run_synthetic();
    assert!(
        sma.trades
            .iter()
            .all(|t| t.stop.is_none() && t.r_multiple.is_none())
    );
    assert_eq!(sma.metrics.expectancy_r, None);
}

#[test]
fn the_close_out_at_the_end_counts_as_a_forced_close() {
    let open_at_end = run_synthetic();
    assert!(
        open_at_end.trades.last().unwrap().open,
        "the run must end in a position"
    );
    assert_eq!(open_at_end.metrics.force_closed_trades, 0);
    assert!(open_at_end.trades.iter().all(|t| !t.force_closed));

    let mut c = config();
    c.close_at_end = true;
    let r = run(&c, &synthetic_bars(), &[], &synthetic_funding()).unwrap();
    let (last, rest) = r.trades.split_last().unwrap();
    // Closed by the market order decided on the second-to-last close, at the last bar's open.
    assert!(!last.open && last.force_closed);
    assert_eq!(last.exit_time, T0 + 489 * HOUR);
    assert!(rest.iter().all(|t| !t.force_closed));
    assert_eq!(r.metrics.force_closed_trades, 1);
}

#[test]
fn equity_extremes_bracket_each_close_and_feed_the_intrabar_metrics() {
    let r = run_synthetic();
    assert_eq!(r.equity_extremes.len(), r.equity.len());
    for (x, p) in r.equity_extremes.iter().zip(&r.equity) {
        assert!(
            x.low <= p.equity && p.equity <= x.high,
            "{x:?} around {}",
            p.equity
        );
    }
    let m = &r.metrics;
    assert!(m.max_drawdown_intrabar.unwrap() >= m.max_drawdown.unwrap());
    assert!(m.static_drawdown_intrabar.unwrap() >= m.static_drawdown.unwrap());
    assert!(m.worst_daily_loss_intrabar.unwrap() >= m.worst_daily_loss.unwrap());
    // The strategy puts 90% of its equity in the position, so leverage peaks near 0.9.
    let lev = m.peak_leverage.unwrap();
    assert!(lev > 0.85 && lev < 1.1, "peak leverage {lev}");
    // Buy and hold has all its cash in the position from the first bar.
    let b = &r.benchmark.metrics;
    assert!(b.peak_leverage.unwrap() > 0.98);
    assert_eq!(b.force_closed_trades, 0);
}

#[test]
fn alpha_and_beta_are_on_aligned_per_bar_returns_annualised_by_returns_per_year() {
    use td_engine::metrics::{EquitySeries, alpha_beta, per_bar_returns};
    let r = run_synthetic();
    let s = EquitySeries::new(r.config.starting_cash, &r.equity, Timeframe::H1);
    let b = EquitySeries::new(r.config.starting_cash, &r.benchmark.equity, Timeframe::H1);
    assert_eq!(s.times, b.times);
    // The returns over the years from the first time to the last.
    let years = (s.times.last().unwrap() - s.times[0]) as f64 / (365.0 * 86_400_000.0);
    let s = per_bar_returns(&s.values).unwrap();
    let b = per_bar_returns(&b.values).unwrap();
    let per_year = s.len() as f64 / years;
    // The synthetic bars have no gaps, so that is 365 days of 24 hourly bars.
    assert!((per_year - 8760.0).abs() < 1e-9, "{per_year}");
    let (alpha, beta) = alpha_beta(&s, &b, per_year).unwrap();
    assert_eq!(r.benchmark.alpha, Some(alpha));
    assert_eq!(r.benchmark.beta, Some(beta));
    let (alpha_by_timeframe, _) = alpha_beta(&s, &b, 8760.0).unwrap();
    assert!((alpha - alpha_by_timeframe).abs() <= 1e-12 * alpha.abs().max(1.0));
}

#[test]
fn slippage_defaults_by_instrument_when_the_config_leaves_it_out() {
    let mut value = serde_json::to_value(config()).unwrap();
    let with_base = |value: &Value, base: &str, slippage: Option<Value>| {
        let mut v = value.clone();
        v["instrument"]["base"] = Value::from(base);
        match slippage {
            Some(s) => v["slippage_bps"] = s,
            None => {
                v.as_object_mut().unwrap().remove("slippage_bps");
            }
        }
        serde_json::from_value::<BacktestConfig>(v).unwrap()
    };
    assert_eq!(with_base(&value, "BTC", None).slippage_bps, 2.0);
    assert_eq!(
        with_base(&value, "ETH", Some(Value::Null)).slippage_bps,
        2.0
    );
    assert_eq!(with_base(&value, "SOL", None).slippage_bps, 5.0);
    // A slippage given explicitly wins, zero included.
    assert_eq!(
        with_base(&value, "BTC", Some(Value::from(0.0))).slippage_bps,
        0.0
    );
    assert_eq!(
        with_base(&value, "SOL", Some(Value::from(1.5))).slippage_bps,
        1.5
    );
    // The other defaults still apply.
    let obj = value.as_object_mut().unwrap();
    for key in [
        "params",
        "fee_override",
        "max_leverage",
        "warmup_from",
        "close_at_end",
    ] {
        obj.remove(key);
    }
    let c = with_base(&value, "BTC", None);
    assert_eq!(c.params, Params::new());
    assert_eq!(
        (c.fee_override, c.warmup_from, c.close_at_end),
        (None, None, false)
    );
    assert_eq!(c.max_leverage, 1.0);
    // A config round-trips with the slippage it ran with.
    let c = config();
    let back: BacktestConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
    assert_eq!(back, c);
}

#[test]
fn a_result_saved_before_the_risk_measures_still_loads() {
    let mut c = config();
    donchian(&mut c);
    let r = run(&c, &synthetic_bars(), &[], &synthetic_funding()).unwrap();
    let mut v = serde_json::to_value(&r).unwrap();
    v.as_object_mut()
        .unwrap()
        .remove("equity_extremes")
        .unwrap();
    for key in [
        "max_drawdown_intrabar",
        "static_drawdown",
        "static_drawdown_intrabar",
        "worst_daily_loss",
        "worst_daily_loss_intrabar",
        "peak_leverage",
        "expectancy_r",
        "payoff_ratio",
        "longest_losing_streak",
        "force_closed_trades",
    ] {
        v["metrics"].as_object_mut().unwrap().remove(key).unwrap();
        v["benchmark"]["metrics"]
            .as_object_mut()
            .unwrap()
            .remove(key)
            .unwrap();
    }
    for key in ["alpha", "beta"] {
        v["benchmark"].as_object_mut().unwrap().remove(key).unwrap();
    }
    for t in v["trades"].as_array_mut().unwrap() {
        for key in ["stop", "r_multiple", "mae", "mfe", "force_closed"] {
            t.as_object_mut().unwrap().remove(key).unwrap();
        }
    }
    let old: BacktestResult = serde_json::from_value(v).unwrap();
    assert!(old.equity_extremes.is_empty());
    assert_eq!(old.metrics.peak_leverage, None);
    assert_eq!(old.metrics.longest_losing_streak, 0);
    assert_eq!(old.benchmark.alpha, None);
    assert!(
        old.trades
            .iter()
            .all(|t| t.stop.is_none() && t.mae.is_none() && !t.force_closed)
    );
    // Everything that was there is unchanged.
    assert_eq!(old.equity, r.equity);
    assert_eq!(old.metrics.sharpe, r.metrics.sharpe);
    assert_eq!(old.trades.len(), r.trades.len());
}
