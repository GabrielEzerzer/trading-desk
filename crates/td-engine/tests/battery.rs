//! The strategy test battery end to end on synthetic four-hour bars: a planted edge reaches
//! `trade`, noise does not, a short range gives `not_enough_evidence`, missing bars give
//! `invalid`, a cancel in the luck test gives an inconclusive luck line, the same seed gives the
//! same result at any thread count, the progress count adds up, and on a market with quotes the
//! asks reach the runs.

mod common;

use std::ops::ControlFlow;
use std::sync::Mutex;

use td_core::{Candle, Instrument, InstrumentKind, Millis, Timeframe};
use td_engine::{
    BatteryResult, BatterySpec, Direction, DrawdownMode, EvidenceSettings, FirmLimits,
    HighRiskLimits, Layer, MAX_RUNS, Metric, Objective, Outcome, ParamAxis, ParamSpace, ParamValue,
    Params, Period, PortfolioBenchmark, PortfolioLimits, PropFirmLimits, SweepPlan, TestPlan,
    TesterError, VerdictRule, VerdictState, WalkForwardPlan, risk, run_battery,
};

const HOUR: Millis = 3_600_000;
const BAR: Millis = 4 * HOUR;
const DAY: Millis = 24 * HOUR;
/// 2024-01-01 00:00 UTC.
const T0: Millis = 1_704_067_200_000;
/// 250 days of four-hour bars: four 45-day test windows after a 60-day train window, 180
/// out-of-sample days.
const DAYS: i64 = 250;
/// The sweep: 3 x 3 parameter sets.
const PER_SWEEP: usize = 9;
const PERMUTATIONS: usize = 19;

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

/// Four-hour bars from a list of closes, each opening at the previous close.
fn bars_from(closes: &[f64]) -> Vec<Candle> {
    bars_every(BAR, closes)
}

/// Bars `step` apart from a list of closes, each opening at the previous close.
fn bars_every(step: Millis, closes: &[f64]) -> Vec<Candle> {
    closes
        .iter()
        .enumerate()
        .map(|(i, &close)| {
            let open = if i == 0 { close } else { closes[i - 1] };
            Candle {
                open_time: T0 + i as i64 * step,
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

fn bar_count(days: i64) -> usize {
    (days * DAY / BAR) as usize
}

/// A triangle wave with period 1 between -1 and 1.
fn tri(x: f64) -> f64 {
    let f = x - x.floor();
    4.0 * (f - 0.5).abs() - 1.0
}

/// A planted edge: up and down legs of 30 bars (five days), a triangle wave, with a little noise.
/// A moving-average cross that goes short rides both legs; shuffled, the legs are gone.
fn edge_bars(days: i64) -> Vec<Candle> {
    bars_from(&edge_closes(bar_count(days)))
}

fn edge_closes(bars: usize) -> Vec<f64> {
    let mut noise = Noise(0x5EED_1234);
    (0..bars)
        .map(|i| 100.0 + 10.0 * tri(i as f64 / 60.0) + 0.6 * noise.next())
        .collect()
}

/// Pure noise: a random walk of independent moves of up to 1%.
fn noise_bars(seed: u64) -> Vec<Candle> {
    let mut noise = Noise(seed);
    let mut close = 100.0;
    let closes: Vec<f64> = (0..bar_count(DAYS))
        .map(|_| {
            close *= 1.0 + 0.02 * noise.next();
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

/// SMA cross with shorts on a perpetual over `days` days from `T0`: fast {2, 3, 4} x slow
/// {8, 12, 16} by Sharpe, 60-day train and 45-day test windows, 19 permutations (so the luck test
/// passes only when the real sweep beats every copy), at least 10 out-of-sample trades.
fn plan(days: i64) -> TestPlan {
    let mut evidence = EvidenceSettings::default();
    evidence.luck.permutations = PERMUTATIONS as u32;
    evidence.min_sample.trades = 10;
    TestPlan {
        instruments: vec![common::perp().id],
        timeframe: Timeframe::H4,
        period: Period {
            from: T0,
            to: T0 + days * DAY,
        },
        backtest: td_engine::BacktestSettings {
            starting_cash: 10_000.0,
            max_leverage: 1.0,
            slippage: Vec::new(),
        },
        sweep: SweepPlan {
            fixed: Params::new().with("allow_short", ParamValue::Bool(true)),
            axes: vec![
                ParamAxis {
                    name: "fast".into(),
                    space: ints(&[2, 3, 4]),
                },
                ParamAxis {
                    name: "slow".into(),
                    space: ints(&[8, 12, 16]),
                },
            ],
            objective: Objective {
                metric: Metric::Sharpe,
                direction: Direction::Maximize,
            },
            min_trades: 3,
        },
        walk_forward: WalkForwardPlan {
            train_days: 60,
            test_days: 45,
        },
        out_of_sample: None,
        evidence,
    }
}

fn cash_rule() -> VerdictRule {
    VerdictRule::Portfolio {
        benchmark: PortfolioBenchmark::Cash,
        limits: PortfolioLimits::default(),
    }
}

fn high_risk_rule() -> VerdictRule {
    VerdictRule::HighRisk {
        limits: HighRiskLimits {
            min_trades: 10,
            ..HighRiskLimits::default()
        },
    }
}

fn prop_firm_rule() -> VerdictRule {
    VerdictRule::PropFirm {
        firm: FirmLimits {
            daily_loss: 0.05,
            max_drawdown: 0.10,
            drawdown: DrawdownMode::Trailing,
            profit_target: 0.08,
        },
        limits: PropFirmLimits::default(),
    }
}

fn battery(
    plan: &TestPlan,
    rule: &VerdictRule,
    bars: &[Candle],
    seed: u64,
    progress: Option<td_engine::Progress>,
) -> Result<BatteryResult, TesterError> {
    let instrument: Instrument = common::perp();
    let spec = BatterySpec {
        strategy: "sma_cross",
        plan,
        rule,
        instrument: &instrument,
        seed,
    };
    run_battery(&spec, bars, &[], &[], progress)
}

fn line<'a>(r: &'a BatteryResult, id: &str) -> &'a td_engine::Criterion {
    r.verdict
        .criteria
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("no line {id}"))
}

/// The progress total for a battery that runs every step, from the window counts.
fn full_total(windows: usize, offset_windows: usize) -> usize {
    3 + PER_SWEEP
        + windows * PER_SWEEP
        + 3 * windows
        + windows
        + offset_windows * PER_SWEEP
        + PER_SWEEP * (PERMUTATIONS + 1)
}

#[test]
fn a_planted_edge_reaches_trade_with_every_input_from_the_runs() {
    let bars = edge_bars(DAYS);
    let plan = plan(DAYS);
    let r = battery(&plan, &cash_rule(), &bars, 7, None).unwrap();
    assert_eq!(
        r.verdict.state,
        VerdictState::Trade,
        "{:#?}",
        r.verdict.criteria
    );

    // Every step ran.
    let wf = r.walk_forward.as_ref().unwrap();
    let stress = r.cost_stress.as_ref().unwrap();
    let luck = r.permutation.as_ref().unwrap();
    assert_eq!(wf.windows.len(), 4);
    assert_eq!(r.start_offsets.len(), 8);
    assert_eq!(luck.spec.permutations, PERMUTATIONS);
    assert!(r.sweep.is_some() && r.breakdown.is_some() && r.plateau.is_some());

    // The evidence inputs are the runs' numbers.
    let m = &wf.metrics;
    assert_eq!(r.evidence.oos_return, m.total_return);
    assert_eq!(r.evidence.oos_trades, m.trades);
    assert_eq!(r.evidence.windows, 4);
    assert_eq!(r.evidence.oos_days, 180.0);
    assert_eq!(r.evidence.luck_p, Some(luck.p_value));
    assert_eq!(r.evidence.stressed_return, Some(stress.total_return));
    assert!(stress.total_return < m.total_return.unwrap());
    assert_eq!(r.evidence.start_offsets_same_sign, 8);
    let plateau = r.plateau.as_ref().unwrap();
    assert_eq!(r.evidence.plateau_median_ratio, plateau.median_ratio);
    assert_eq!(r.evidence.plateau_positive_share, plateau.positive_share);

    // The kind inputs too: Sharpe, drawdown, alpha against cash (the strategy's own CAGR) and the
    // bootstrap of the daily returns.
    assert_eq!(r.kind.sharpe, m.sharpe);
    assert_eq!(r.kind.max_drawdown, m.max_drawdown);
    assert_eq!(r.kind.alpha, m.cagr);
    assert_eq!(r.kind.trades, m.trades);
    assert_eq!(r.kind.expectancy, m.avg_trade_pnl);
    assert_eq!(r.kind.profit_factor, m.profit_factor);
    assert_eq!(r.kind.intrabar_drawdown_static, m.static_drawdown_intrabar);
    assert_eq!(
        r.kind.p_mean_not_positive,
        r.daily_bootstrap.as_ref().map(|b| b.p_mean_at_most_zero)
    );
    assert_eq!(
        r.kind.expectancy_lower_bound,
        r.trade_bootstrap.as_ref().map(|b| b.lower_bound)
    );
    assert_eq!(
        r.kind.return_without_best_trade,
        r.breakdown.as_ref().unwrap().return_without_best_trade
    );
    assert_eq!(r.oos_integrity.sharpe, m.sharpe);
    assert_eq!(r.oos_integrity.peak_leverage, m.peak_leverage);

    // Each line holds its input.
    assert_eq!(line(&r, "sharpe").value, m.sharpe);
    assert_eq!(line(&r, "alpha").value, m.cagr);
    assert_eq!(line(&r, "oos_return").value, m.total_return);
    assert_eq!(line(&r, "luck_p").value, Some(luck.p_value));
    assert_eq!(line(&r, "luck_p").outcome, Outcome::Pass);

    // Every layer was judged, integrity first.
    let layers: Vec<Layer> = r.verdict.criteria.iter().map(|c| c.layer).collect();
    assert_eq!(layers[0], Layer::Integrity);
    for layer in [Layer::Integrity, Layer::Evidence, Layer::Kind] {
        assert!(layers.contains(&layer));
    }
    for id in [
        "dropped_buckets",
        "determinism",
        "prefix_consistency",
        "too_good_to_be_true",
        "lowest_equity",
        "peak_leverage",
    ] {
        assert_ne!(line(&r, id).outcome, Outcome::Fail, "{id}");
    }
    assert_eq!(r.integrity.dropped_buckets, 0);
    assert_eq!(r.integrity.open_buckets, bar_count(DAYS));
}

#[test]
fn the_other_kinds_get_their_inputs_from_the_same_runs() {
    let bars = edge_bars(DAYS);
    let plan = plan(DAYS);

    let high = battery(&plan, &high_risk_rule(), &bars, 7, None).unwrap();
    let wf = high.walk_forward.as_ref().unwrap();
    let ruin = high.kind.risk_of_ruin.unwrap();
    assert!((0.0..=1.0).contains(&ruin));
    assert_eq!(line(&high, "risk_of_ruin").value, Some(ruin));
    assert_eq!(line(&high, "trades").value, Some(wf.metrics.trades as f64));
    assert!(high.prop_firm_odds.is_none());

    // Alpha against buy and hold is the stitched benchmark's.
    let held = VerdictRule::Portfolio {
        benchmark: PortfolioBenchmark::BuyAndHold,
        limits: PortfolioLimits::default(),
    };
    let held = battery(&plan, &held, &bars, 7, None).unwrap();
    let wf = held.walk_forward.as_ref().unwrap();
    let alpha = wf.benchmark.as_ref().unwrap().alpha;
    assert!(alpha.is_some());
    assert_ne!(alpha, wf.metrics.cagr);
    assert_eq!(held.kind.alpha, alpha);
    assert_eq!(line(&held, "alpha").value, alpha);

    let prop = battery(&plan, &prop_firm_rule(), &bars, 7, None).unwrap();
    let cash = plan.backtest.starting_cash;
    let trailing =
        risk::trailing_drawdown(cash, &prop.walk_forward.as_ref().unwrap().equity_extremes)
            .unwrap()
            .amount
            / cash;
    assert!(trailing > 0.0);
    assert_eq!(prop.kind.intrabar_drawdown_trailing, Some(trailing));
    // The firm in this rule trails, so its drawdown line reads the trailing value.
    assert_eq!(line(&prop, "max_drawdown").value, Some(trailing));
    let odds = prop.prop_firm_odds.unwrap();
    assert_eq!(prop.kind.pass_probability, Some(odds.pass));
    assert_eq!(prop.kind.breach_probability, Some(odds.breach));
    let worst = prop.kind.worst_daily_loss.unwrap();
    assert!(worst >= 0.0);
    // The worse of the two day rollovers is at least the UTC day's, which the metrics report.
    assert!(
        worst
            >= prop
                .walk_forward
                .unwrap()
                .metrics
                .worst_daily_loss_intrabar
                .unwrap()
                - 1e-12
    );
    assert!(prop.kind.risk_of_ruin.is_none());
}

#[test]
fn pure_noise_does_not_reach_trade() {
    let plan = plan(DAYS);
    for seed in [0xA11CE, 0xB0B5] {
        let bars = noise_bars(seed);
        for rule in [cash_rule(), high_risk_rule(), prop_firm_rule()] {
            let r = battery(&plan, &rule, &bars, 7, None).unwrap();
            assert_ne!(
                r.verdict.state,
                VerdictState::Trade,
                "noise {seed:#x}: {:#?}",
                r.verdict.criteria
            );
        }
    }
}

#[test]
fn a_range_too_short_for_the_minimum_sample_gives_not_enough_evidence() {
    // 195 days hold three test windows, one short of the minimum.
    let days = 195;
    let calls = Mutex::new(Vec::new());
    let progress = |done, total| {
        calls.lock().unwrap().push((done, total));
        ControlFlow::Continue(())
    };
    let r = battery(
        &plan(days),
        &cash_rule(),
        &edge_bars(days),
        7,
        Some(&progress),
    )
    .unwrap();
    assert_eq!(r.verdict.state, VerdictState::NotEnoughEvidence);
    assert_eq!(r.evidence.windows, 3);
    assert_eq!(line(&r, "windows").outcome, Outcome::Inconclusive);
    // The runs that could not change that are skipped, and say so.
    assert!(r.cost_stress.is_none() && r.permutation.is_none() && r.start_offsets.is_empty());
    for id in ["luck_p", "stressed_return", "start_offsets"] {
        let l = line(&r, id);
        assert_eq!(l.outcome, Outcome::Inconclusive);
        assert!(l.note.as_deref().unwrap().starts_with("Not run"));
    }
    // The count still reaches the total.
    let calls = calls.into_inner().unwrap();
    assert_eq!(calls.last(), Some(&(r.runs, r.runs)));

    // With no room for a single window there is no out-of-sample curve at all.
    let days = 90;
    let r = battery(&plan(days), &cash_rule(), &edge_bars(days), 7, None).unwrap();
    assert_eq!(r.verdict.state, VerdictState::NotEnoughEvidence);
    assert!(r.walk_forward.is_none() && r.walk_forward_error.is_some());
    assert_eq!(r.evidence.windows, 0);
    assert_eq!(r.evidence.oos_trades, 0);
}

#[test]
fn missing_bars_give_invalid_and_nothing_else_runs() {
    // Every 40th bar missing: 2.5% of the buckets, over the 1% limit.
    let bars: Vec<Candle> = edge_bars(DAYS)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| i % 40 != 20)
        .map(|(_, b)| b)
        .collect();
    let r = battery(&plan(DAYS), &cash_rule(), &bars, 7, None).unwrap();
    assert_eq!(r.verdict.state, VerdictState::Invalid);
    assert_eq!(r.integrity.dropped_buckets, bar_count(DAYS) / 40);
    assert_eq!(line(&r, "dropped_buckets").outcome, Outcome::Fail);
    assert!(r.sweep.is_none() && r.walk_forward.is_none());
    assert!(
        r.verdict
            .criteria
            .iter()
            .all(|c| c.layer == Layer::Integrity)
    );
}

#[test]
fn a_cancel_in_the_luck_test_leaves_its_line_inconclusive() {
    let bars = edge_bars(DAYS);
    let plan = plan(DAYS);
    // Stop halfway through the luck test, the last step.
    let luck_runs = PER_SWEEP * (PERMUTATIONS + 1);
    let progress = |done: usize, total: usize| {
        if done > total - luck_runs / 2 {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let r = battery(&plan, &cash_rule(), &bars, 7, Some(&progress)).unwrap();
    assert!(r.permutation.is_none());
    assert_eq!(r.evidence.luck_p, None);
    let luck = line(&r, "luck_p");
    assert_eq!(luck.outcome, Outcome::Inconclusive);
    assert!(luck.note.as_deref().unwrap().contains("cancelled"));
    assert_eq!(r.verdict.state, VerdictState::NotEnoughEvidence);
    // Every step before it finished.
    assert!(r.cost_stress.is_some());
    assert_eq!(r.start_offsets.len(), 8);
    assert_eq!(line(&r, "stressed_return").outcome, Outcome::Pass);
}

#[test]
fn a_cancel_before_the_luck_test_is_an_error() {
    let bars = edge_bars(DAYS);
    let plan = plan(DAYS);
    // At the first integrity run.
    let first = |_: usize, _: usize| ControlFlow::Break(());
    assert_eq!(
        battery(&plan, &cash_rule(), &bars, 7, Some(&first)),
        Err(TesterError::Cancelled)
    );
    // In the sweep.
    let sweep = |done: usize, _: usize| {
        if done > 5 {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    assert_eq!(
        battery(&plan, &cash_rule(), &bars, 7, Some(&sweep)),
        Err(TesterError::Cancelled)
    );
    // In R6, just before the luck test.
    let luck_runs = PER_SWEEP * (PERMUTATIONS + 1);
    let offsets = |done: usize, total: usize| {
        if done > total - luck_runs - PER_SWEEP {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    assert_eq!(
        battery(&plan, &cash_rule(), &bars, 7, Some(&offsets)),
        Err(TesterError::Cancelled)
    );
}

#[test]
fn the_same_seed_gives_the_same_result_at_any_thread_count() {
    let bars = edge_bars(DAYS);
    let plan = plan(DAYS);
    let two = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap()
        .install(|| battery(&plan, &cash_rule(), &bars, 11, None).unwrap());
    let many = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap()
        .install(|| battery(&plan, &cash_rule(), &bars, 11, None).unwrap());
    assert_eq!(two, many);
    assert!(two.permutation.is_some());
}

#[test]
fn progress_counts_every_run_once_against_one_total() {
    let bars = edge_bars(DAYS);
    let plan = plan(DAYS);
    let calls = Mutex::new(Vec::new());
    let progress = |done, total| {
        calls.lock().unwrap().push((done, total));
        ControlFlow::Continue(())
    };
    let r = battery(&plan, &cash_rule(), &bars, 7, Some(&progress)).unwrap();
    let calls = calls.into_inner().unwrap();

    // The offsets move the range's end back by 30 bars each: two still hold four windows, six
    // hold three.
    let offset_windows = 2 * 4 + 6 * 3;
    assert_eq!(r.runs, full_total(4, offset_windows));
    assert!(calls.iter().all(|&(_, total)| total == r.runs));
    let mut done: Vec<usize> = calls.iter().map(|&(d, _)| d).collect();
    done.sort_unstable();
    assert_eq!(done, (1..=r.runs).collect::<Vec<_>>());
    let windows: usize = r.start_offsets.iter().map(|o| o.windows).sum();
    assert_eq!(windows, offset_windows);
}

/// Daily bars over 400 days with 120-day train windows (four legs of the wave) and test windows
/// of `test_days`.
fn daily_plan(test_days: u32) -> (TestPlan, Vec<Candle>) {
    let days = 400;
    let mut plan = plan(days);
    plan.timeframe = Timeframe::D1;
    plan.walk_forward = WalkForwardPlan {
        train_days: 120,
        test_days,
    };
    (plan, bars_every(DAY, &edge_closes(days as usize)))
}

/// R6's windows step by one test window, so a shift moves their edges by the shift modulo the
/// test length: seven-bar test windows give eight offsets at most six new edge positions, and
/// two of them would repeat a run. The battery refuses them before anything runs.
#[test]
fn test_windows_too_short_for_the_start_offsets_are_refused_before_anything_runs() {
    let (plan, bars) = daily_plan(7);
    let calls = Mutex::new(0usize);
    let progress = |_: usize, _: usize| {
        *calls.lock().unwrap() += 1;
        ControlFlow::Continue(())
    };
    let r = battery(&plan, &cash_rule(), &bars, 7, Some(&progress));
    assert_eq!(
        r,
        Err(TesterError::InvalidSpec(
            "the walk-forward test window is 7 bars, too short for 8 start offsets: R6 needs at \
             least 9 bars per test window to move the window edges to 8 different places"
                .into()
        ))
    );
    assert_eq!(calls.into_inner().unwrap(), 0);
}

/// Nine-bar test windows are the shortest that hold eight offsets: each moves the windows by a
/// different number of bars, one to eight, and R6 runs all of them.
#[test]
fn start_offsets_run_on_the_shortest_test_windows_that_hold_them() {
    let (plan, bars) = daily_plan(9);
    let r = battery(&plan, &cash_rule(), &bars, 7, None).unwrap();
    // The lines before R6 left it something to decide, so it ran.
    assert!(r.cost_stress.is_some(), "{:#?}", r.verdict.criteria);
    let shifts: Vec<usize> = r.start_offsets.iter().map(|o| o.shift_bars).collect();
    assert_eq!(shifts, (1..=8).collect::<Vec<_>>());
    assert!(r.start_offsets.iter().all(|o| o.windows > 0));
    assert!(
        r.start_offsets.iter().all(|o| o.error.is_none()),
        "{:#?}",
        r.start_offsets
    );
}

/// A luck test over the run limit is refused before the first run, not after every other step:
/// a 25 x 25 grid at 199 permutations is 125,000 runs.
#[test]
fn a_luck_test_over_the_run_limit_is_refused_before_anything_runs() {
    let mut plan = plan(DAYS);
    let axis = |name: &str, from: i64| ParamAxis {
        name: name.into(),
        space: ints(&(from..from + 25).collect::<Vec<_>>()),
    };
    plan.sweep.axes = vec![axis("fast", 1), axis("slow", 30)];
    plan.evidence.luck.permutations = 199;
    let calls = Mutex::new(0usize);
    let progress = |_: usize, _: usize| {
        *calls.lock().unwrap() += 1;
        ControlFlow::Continue(())
    };
    let r = battery(&plan, &cash_rule(), &edge_bars(DAYS), 7, Some(&progress));
    assert_eq!(
        r,
        Err(TesterError::TooManyRuns {
            runs: 125_000,
            limit: MAX_RUNS,
        })
    );
    assert_eq!(calls.into_inner().unwrap(), 0);
}

/// 340 days of the planted edge on FX, without the four-hour bars that lie wholly in the weekend's
/// closed hours (Friday 22:00 to Sunday 22:00 UTC), with asks a constant `spread` above the bars.
/// 31 of every 42 bars are left, enough for four 45-day test windows after a 60-day train window.
fn fx_bars(spread: f64) -> (Vec<Candle>, Vec<Candle>) {
    let bars: Vec<Candle> = edge_bars(340)
        .into_iter()
        .filter(|b| {
            let end = b.open_time + BAR;
            let closed = InstrumentKind::Fx.closed_spans(b.open_time, end);
            !matches!(closed.as_slice(), [span] if *span == (b.open_time..end))
        })
        .collect();
    let asks = bars
        .iter()
        .map(|b| Candle {
            open: b.open + spread,
            high: b.high + spread,
            low: b.low + spread,
            close: b.close + spread,
            ..*b
        })
        .collect();
    (bars, asks)
}

fn fx_battery(bars: &[Candle], asks: &[Candle]) -> BatteryResult {
    let instrument = common::instrument(InstrumentKind::Fx);
    let mut plan = plan(340);
    plan.instruments = vec![instrument.id.clone()];
    let spec = BatterySpec {
        strategy: "sma_cross",
        plan: &plan,
        rule: &cash_rule(),
        instrument: &instrument,
        seed: 7,
    };
    run_battery(&spec, bars, asks, &[], None).unwrap()
}

/// The battery runs on FX with its asks: every run fills buys on the ask, so a spread lowers each
/// swept set's return against the same bars with a zero spread. I1 leaves the closed weekend out of
/// the buckets, and the luck test, which cannot shuffle asks yet, is inconclusive.
#[test]
fn on_a_market_with_quotes_the_asks_reach_every_run() {
    let (bars, wide) = fx_bars(0.05);
    let (_, zero) = fx_bars(0.0);
    let r = fx_battery(&bars, &wide);
    let flat = fx_battery(&bars, &zero);

    assert_eq!(r.integrity.open_buckets, bars.len());
    assert_eq!(r.integrity.dropped_buckets, 0);
    assert_ne!(
        r.verdict.state,
        VerdictState::Invalid,
        "{:#?}",
        r.verdict.criteria
    );
    assert_eq!(r.walk_forward.as_ref().unwrap().windows.len(), 4);

    // The sweep and the walk-forward ran on the asks.
    let rows = &r.sweep.as_ref().unwrap().rows;
    let flat_rows = &flat.sweep.as_ref().unwrap().rows;
    let mut compared = 0;
    for row in rows
        .iter()
        .filter(|row| row.metrics.is_some_and(|m| m.trades > 0))
    {
        let same = flat_rows.iter().find(|f| f.index == row.index).unwrap();
        let ret = |row: &td_engine::SweepRow| row.metrics.unwrap().total_return.unwrap();
        assert!(ret(row) < ret(same), "set {}", row.index);
        compared += 1;
    }
    assert_eq!(compared, PER_SWEEP);
    assert_ne!(
        r.walk_forward.as_ref().unwrap().metrics.total_return,
        flat.walk_forward.as_ref().unwrap().metrics.total_return
    );

    // R4 and R6 ran, and the luck test did not.
    assert!(r.cost_stress.is_some(), "{:#?}", r.verdict.criteria);
    assert_eq!(r.start_offsets.len(), 8);
    assert_ne!(r.cost_stress, flat.cost_stress);
    assert!(r.permutation.is_none());
    let luck = line(&r, "luck_p");
    assert_eq!(luck.outcome, Outcome::Inconclusive);
    assert!(luck.note.as_deref().unwrap().contains("quotes"));
}
