//! The backtest runner end to end: a deterministic synthetic bar series checked against a
//! committed expected result, saved runs that reload and rerun exactly, and the data fingerprint.

mod common;

use std::path::PathBuf;

use serde_json::Value;
use td_core::{Candle, Millis, Timeframe};
use td_engine::{
    BacktestConfig, BacktestResult, EngineError, ParamValue, Params, RunKind, SavedRun,
    data_fingerprint, list, load, rerun, run, save,
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
        warmup_from: None,
    }
}

fn run_synthetic() -> BacktestResult {
    run(&config(), &synthetic_bars(), &synthetic_funding()).unwrap()
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
    // Trade P&L adds up to the change in equity, with an open trade marked at the last close.
    let net: f64 = r.trades.iter().map(|t| t.net_pnl).sum();
    let change = r.equity.last().unwrap().equity - 10_000.0;
    assert!((net - change).abs() < 1e-6, "trades {net}, equity {change}");
    let funding: f64 = r.trades.iter().map(|t| t.funding).sum();
    assert!((funding - r.metrics.funding_paid).abs() < 1e-9);
    let fees: f64 = r.trades.iter().map(|t| t.fees).sum();
    assert!((fees - r.metrics.fees_paid).abs() < 1e-9);
    assert_eq!(
        r.metrics.trades,
        r.trades.iter().filter(|t| !t.open).count()
    );
    // The benchmark is long from the first bar to the last.
    assert_eq!(r.benchmark.metrics.exposure, Some(1.0));
    assert_eq!(r.benchmark.equity.len(), r.equity.len());
    assert!(r.benchmark.correlation.is_some());
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

    let again = rerun(&loaded, &synthetic_bars(), &synthetic_funding()).unwrap();
    assert!(again.fingerprint_matches);
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
    // No temporary directory is left behind.
    assert_eq!(std::fs::read_dir(&runs).unwrap().count(), 1);
}

#[test]
fn list_is_newest_first_and_skips_other_entries() {
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path();
    let result = SavedRun::Backtest(Box::new(run_synthetic()));
    let first = save(&result, runs).unwrap();
    let mut second = save(&result, runs);
    // Two saves of the same result in the same millisecond get the same id; wait and retry.
    while second.is_err() {
        second = save(&result, runs);
    }
    let second = second.unwrap();
    std::fs::create_dir(runs.join(".tmp-1762790000123-0a1b2c3d")).unwrap();
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
fn a_changed_bar_changes_the_fingerprint() {
    let bars = synthetic_bars();
    let funding = synthetic_funding();
    let base = run(&config(), &bars, &funding).unwrap();

    // Bar 200 is in range: a close moved by one cent changes the fingerprint.
    let mut changed = bars.clone();
    changed[200].close += 0.01;
    let r = rerun(&base, &changed, &funding).unwrap();
    assert!(!r.fingerprint_matches);
    assert_ne!(r.result.data_fingerprint, base.data_fingerprint);

    // Bar 495 is outside the range and funding before the first bar is not charged, so neither
    // is part of the data the run used.
    let mut outside = bars.clone();
    outside[495].close += 0.01;
    let mut extra_funding = funding.clone();
    extra_funding.push((T0 - 3 * HOUR, 0.5));
    let r = rerun(&base, &outside, &extra_funding).unwrap();
    assert!(r.fingerprint_matches);

    // A changed funding rate inside the range changes it too.
    let mut rates = funding.clone();
    rates[5].1 += 1e-6;
    assert!(!rerun(&base, &bars, &rates).unwrap().fingerprint_matches);
    assert_ne!(
        data_fingerprint(&bars, &[]),
        data_fingerprint(&bars[1..], &[])
    );
}

#[test]
fn bad_configurations_are_errors() {
    let bars = synthetic_bars();
    let mut c = config();
    c.to = c.from;
    assert!(matches!(
        run(&c, &bars, &[]),
        Err(EngineError::InvalidConfig(_))
    ));
    let mut c = config();
    c.from = T0 + 1000 * HOUR;
    c.to = T0 + 1100 * HOUR;
    assert!(matches!(
        run(&c, &bars, &[]),
        Err(EngineError::NoBarsInRange { .. })
    ));
    let mut c = config();
    c.strategy = "nope".into();
    assert!(matches!(
        run(&c, &bars, &[]),
        Err(EngineError::UnknownStrategy(_))
    ));
}

#[test]
fn spot_benchmark_buys_with_all_cash_at_the_first_open() {
    let mut c = config();
    c.instrument = common::spot();
    c.params = Params::new();
    let r = run(&c, &synthetic_bars(), &[]).unwrap();
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
