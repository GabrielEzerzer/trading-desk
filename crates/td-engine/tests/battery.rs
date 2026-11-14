//! The strategy test battery end to end on synthetic four-hour bars: a planted edge reaches
//! `trade`, noise does not, a short range gives `not_enough_evidence`, missing bars give
//! `invalid`, a cancel in the luck test gives an inconclusive luck line, the same seed gives the
//! same result at any thread count, the progress count adds up, on a market with quotes the
//! asks reach every run (the luck test's too) and R4 widens the spread, R5 is judged on the test
//! windows, a position held through an unmodelled rollover is warned of, and a result saved
//! before the drawdown bootstrap existed still reads.

mod common;

use std::ops::ControlFlow;
use std::sync::Mutex;

use td_core::{
    AccountCurrency, Candle, FeeSchedule, Instrument, InstrumentKind, Millis, Timeframe,
};
use td_engine::{
    AttemptTerms, BatteryResult, BatterySpec, DayRollover, Direction, DrawdownMode,
    EvidenceSettings, FirmLimits, HighRiskLimits, Layer, MAX_RUNS, Metric, Objective, Outcome,
    ParamAxis, ParamSpace, ParamValue, Params, Period, PortfolioBenchmark, PortfolioLimits,
    PropFirmLimits, SlippageOverride, SweepPlan, TestPlan, TesterError, VerdictRule, VerdictState,
    WalkForwardPlan, metrics, risk, run_battery,
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
/// R5's runs counted per window: the most neighbours a set has on the 3 x 3 grid.
const NEIGHBOURS: usize = 8;
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
            account_currency: AccountCurrency::Quote,
            fee_override: None,
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
            second_target: None,
            min_trading_days: 0,
            best_day_share: None,
            day_target_share: None,
            trading_day: None,
            trailing_floor_lock: None,
            day_rollover: None,
        },
        limits: PropFirmLimits::default(),
        attempt: None,
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
        + windows * NEIGHBOURS
        + windows
        + offset_windows * PER_SWEEP
        + PER_SWEEP * (PERMUTATIONS + 1)
}

/// R5's neighbours that are off the grid, over every window: a set has 8 neighbours on the 3 x 3
/// grid when it is in the middle of both axes, and fewer at an edge. Those are counted but never
/// run.
fn off_grid_neighbours(r: &BatteryResult) -> usize {
    let wf = r.walk_forward.as_ref().unwrap();
    wf.windows
        .iter()
        .map(|w| {
            let choices = |name: &str, middle: i64| {
                if w.params.get(name) == Some(ParamValue::Int(middle)) {
                    3
                } else {
                    2
                }
            };
            NEIGHBOURS + 1 - choices("fast", 3) * choices("slow", 12)
        })
        .sum()
}

/// Checks that every run the battery made was counted once against the total: the counts are
/// distinct, reach the total, and the only ones not reported are R5's neighbours off the grid,
/// which R5 reports as the runs it made.
fn assert_every_run_counted_once(r: &BatteryResult, calls: &[(usize, usize)]) {
    assert!(calls.iter().all(|&(_, total)| total == r.runs));
    let mut done: Vec<usize> = calls.iter().map(|&(d, _)| d).collect();
    done.sort_unstable();
    let reported = done.len();
    done.dedup();
    assert_eq!(done.len(), reported, "a count was reported twice");
    assert_eq!(done.last(), Some(&r.runs));
    let off_grid = off_grid_neighbours(r);
    assert_eq!(done.len(), r.runs - off_grid);
    let made = r.walk_forward.as_ref().unwrap().windows.len() * NEIGHBOURS - off_grid;
    let directions = &r.plateau.as_ref().unwrap().directions;
    assert_eq!(directions.iter().map(|d| d.windows).sum::<usize>(), made);
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
    assert!(!r.luck_cancelled);
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
    // R5 is judged on the test windows: against R1's own stitched objective, with a neighbour
    // in every direction the grid has around the sets the windows chose, each run on test bars.
    let plateau = r.plateau.as_ref().unwrap();
    assert_eq!(plateau.best_objective, m.sharpe);
    assert!(!plateau.directions.is_empty());
    assert!(
        plateau
            .directions
            .iter()
            .all(|d| (1..=4).contains(&d.windows))
    );
    assert_eq!(r.evidence.plateau_median_ratio, plateau.median_ratio);
    assert_eq!(r.evidence.plateau_positive_share, plateau.positive_share);
    assert!(plateau.median_ratio.is_some() && plateau.positive_share.is_some());
    // A perpetual's funding is modelled, so nothing is warned of.
    assert!(r.verdict.warnings.is_empty());

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
    // The drawdown bootstrap runs on the same daily returns as R7, in the same blocks, and its
    // 95th percentile is the resampled drawdown line's value. Daily closes are a subset of the
    // bar closes, so the historical order's drawdown is at most the bar-close drawdown.
    let dd = r.drawdown_bootstrap.as_ref().unwrap();
    let daily = r.daily_bootstrap.as_ref().unwrap();
    assert_eq!(
        (dd.resamples, dd.mean_block),
        (daily.resamples, daily.mean_block)
    );
    assert!(dd.historical <= m.max_drawdown.unwrap() + 1e-12, "{dd:?}");
    assert!(dd.historical <= dd.p95 && dd.p50 <= dd.p95, "{dd:?}");
    assert_eq!(r.kind.resampled_drawdown, Some(dd.p95));
    let resampled = line(&r, "resampled_drawdown");
    assert_eq!(
        (resampled.value, resampled.limit, resampled.outcome),
        (Some(dd.p95), Some(0.45), Outcome::Pass)
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
    // The drawdown bootstrap is the portfolio kind's alone.
    assert!(high.drawdown_bootstrap.is_none() && high.kind.resampled_drawdown.is_none());

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
    let extremes = &prop.walk_forward.as_ref().unwrap().equity_extremes;
    // The first 90 of the 180 days are one of the attempts the drawdown line reads, so the worst
    // attempt falls at least as far as the bars of its first 80 days do.
    let first_attempt = extremes.len() * 80 / 180;
    let first = risk::trailing_drawdown(cash, &extremes[..first_attempt])
        .unwrap()
        .amount
        / cash;
    let trailing = prop.kind.intrabar_drawdown_trailing.unwrap();
    assert!(
        first > 0.0 && trailing >= first,
        "{trailing} against {first}"
    );
    // The firm in this rule trails, so its drawdown line reads the trailing value.
    assert_eq!(line(&prop, "max_drawdown").value, Some(trailing));
    let odds = prop.prop_firm_odds.unwrap();
    assert_eq!(prop.kind.pass_probability, Some(odds.pass));
    assert_eq!(prop.kind.breach_probability, Some(odds.breach));
    let worst = prop.kind.worst_daily_loss.unwrap();
    assert!(worst >= 0.0);
    // The worse of the two day rollovers is at least the UTC day's, each day's fall read as a
    // share of that day's starting balance.
    let wf = prop.walk_forward.as_ref().unwrap();
    let series = metrics::EquitySeries::new(cash, &wf.equity, plan.timeframe);
    let utc = risk::daily_pnl(&series, Some(extremes), DayRollover::default());
    let utc_worst = risk::worst_daily_loss_intraday_own(&utc).unwrap();
    assert!(
        utc_worst > 0.0 && worst >= utc_worst,
        "{worst} against {utc_worst}"
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
    assert!(r.luck_cancelled);
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

/// The drawdown bootstrap draws from its own seed stream: the same battery seed gives the same
/// percentiles, another seed other ones, on the same returns.
#[test]
fn the_drawdown_bootstrap_is_fixed_by_the_battery_seed() {
    let bars = edge_bars(DAYS);
    let plan = plan(DAYS);
    let run = |seed| battery(&plan, &cash_rule(), &bars, seed, None).unwrap();
    let (a, again, other) = (run(7), run(7), run(8));
    let dd = |r: &BatteryResult| r.drawdown_bootstrap.clone().unwrap();
    assert_eq!(dd(&a), dd(&again));
    assert_eq!(dd(&a).historical, dd(&other).historical);
    assert_ne!((dd(&a).p50, dd(&a).p95), (dd(&other).p50, dd(&other).p95));
    assert_eq!(
        line(&other, "resampled_drawdown").value,
        Some(dd(&other).p95)
    );
}

/// A battery result saved before the drawdown bootstrap existed, without it and without the
/// kind input it feeds, still reads, with both missing.
#[test]
fn a_result_saved_before_the_drawdown_bootstrap_still_reads() {
    let bars = edge_bars(DAYS);
    let plan = plan(DAYS);
    let r = battery(&plan, &cash_rule(), &bars, 7, None).unwrap();
    let mut json = serde_json::to_value(&r).unwrap();
    let fields = json.as_object_mut().unwrap();
    assert!(fields.remove("drawdown_bootstrap").is_some());
    let kind = fields["kind"].as_object_mut().unwrap();
    assert!(kind.remove("resampled_drawdown").is_some());
    let old: BatteryResult = serde_json::from_value(json).unwrap();
    assert!(old.drawdown_bootstrap.is_none() && old.kind.resampled_drawdown.is_none());
    // The saved verdict keeps the lines it was saved with.
    assert_eq!(old.verdict, r.verdict);
    assert_eq!(old.daily_bootstrap, r.daily_bootstrap);
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
    assert_every_run_counted_once(&r, &calls);
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

/// FX, on the plan of the perpetual's tests over 340 days.
fn fx_plan() -> (Instrument, TestPlan) {
    let instrument = common::instrument(InstrumentKind::Fx);
    let mut plan = plan(340);
    plan.instruments = vec![instrument.id.clone()];
    (instrument, plan)
}

fn fx_run(
    plan: &TestPlan,
    bars: &[Candle],
    asks: &[Candle],
    progress: Option<td_engine::Progress>,
) -> BatteryResult {
    let instrument = common::instrument(InstrumentKind::Fx);
    let spec = BatterySpec {
        strategy: "sma_cross",
        plan,
        rule: &cash_rule(),
        instrument: &instrument,
        seed: 7,
    };
    run_battery(&spec, bars, asks, &[], progress).unwrap()
}

fn fx_battery(bars: &[Candle], asks: &[Candle]) -> BatteryResult {
    fx_run(&fx_plan().1, bars, asks, None)
}

/// The FX plan with no fees and no slippage, so the spread is its only cost.
fn spread_only_plan() -> TestPlan {
    let (instrument, mut plan) = fx_plan();
    plan.backtest.fee_override = Some(FeeSchedule {
        maker: 0.0,
        taker: 0.0,
        per_unit: 0.0,
    });
    plan.backtest.slippage = vec![SlippageOverride {
        instrument: instrument.id,
        bps: 0.0,
    }];
    plan
}

/// The battery runs on FX with its asks: every run fills buys on the ask, so a spread lowers each
/// swept set's return against the same bars with a zero spread. I1 leaves the closed weekend out of
/// the buckets, and the luck test rebuilds the asks on its shuffled bars.
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

    // R4, R6 and the luck test ran, the luck test on the asks too: its real sweep is the
    // battery's own, spread and all.
    assert!(r.cost_stress.is_some(), "{:#?}", r.verdict.criteria);
    assert_eq!(r.start_offsets.len(), 8);
    assert_ne!(r.cost_stress, flat.cost_stress);
    let luck = r.permutation.as_ref().expect("the luck test ran");
    let flat_luck = flat.permutation.as_ref().unwrap();
    assert_eq!(luck.spec.permutations, PERMUTATIONS);
    assert_ne!(luck, flat_luck);
    assert!(!r.luck_cancelled);
    let luck_line = line(&r, "luck_p");
    assert_eq!(luck_line.value, Some(luck.p_value));
    assert_ne!(luck_line.outcome, Outcome::Inconclusive, "{luck_line:#?}");
}

/// A strong edge on FX reaches `trade`: with the luck test running on the asks, an FX strategy is
/// no longer held at `not_enough_evidence` whatever its result.
#[test]
fn on_a_market_with_quotes_a_strong_edge_reaches_trade() {
    let (bars, asks) = fx_bars(0.01);
    let r = fx_run(&spread_only_plan(), &bars, &asks, None);
    assert_eq!(
        r.verdict.state,
        VerdictState::Trade,
        "{:#?}",
        r.verdict.criteria
    );
    assert_eq!(line(&r, "luck_p").outcome, Outcome::Pass);
}

/// R4 on a market with quotes widens each bar's spread: with no fees and no slippage there is
/// nothing else to stress, and the stressed run still loses against the plain one. The plan's fee
/// override reaches every run.
#[test]
fn on_a_market_with_quotes_the_cost_stress_widens_the_spread() {
    let plan = spread_only_plan();
    let (bars, asks) = fx_bars(0.01);
    let r = fx_run(&plan, &bars, &asks, None);
    let wf = r.walk_forward.as_ref().unwrap();
    assert!(
        wf.windows
            .iter()
            .all(|w| w.out_of_sample.config.fee_override == plan.backtest.fee_override)
    );
    assert!(
        wf.windows
            .iter()
            .all(|w| w.out_of_sample.config.slippage_bps == 0.0)
    );
    let stress = r.cost_stress.as_ref().expect("R4 ran");
    let plain = wf.metrics.total_return.unwrap();
    assert!(
        stress.total_return < plain - 0.001,
        "stressed {} against plain {plain}",
        stress.total_return
    );

    // With no spread either, there is nothing to stress, and the stressed run is the plain one.
    let (_, zero) = fx_bars(0.0);
    let r = fx_run(&plan, &bars, &zero, None);
    let plain = r
        .walk_forward
        .as_ref()
        .unwrap()
        .metrics
        .total_return
        .unwrap();
    let stressed = r.cost_stress.as_ref().unwrap().total_return;
    assert!(
        (stressed - plain).abs() < 1e-9,
        "{stressed} against {plain}"
    );
}

/// On a market with quotes the progress count is the runs made too: the luck test it counts
/// runs, so the count no longer jumps from R6 to the total.
#[test]
fn on_a_market_with_quotes_every_run_is_counted_once() {
    let (bars, asks) = fx_bars(0.01);
    let calls = Mutex::new(Vec::new());
    let progress = |done, total| {
        calls.lock().unwrap().push((done, total));
        ControlFlow::Continue(())
    };
    let r = fx_run(&spread_only_plan(), &bars, &asks, Some(&progress));
    assert!(r.permutation.is_some());
    let offset_windows: usize = r.start_offsets.iter().map(|o| o.windows).sum();
    assert_eq!(r.runs, full_total(4, offset_windows));
    assert_every_run_counted_once(&r, &calls.into_inner().unwrap());
}

/// FX charges a swap on a position held through the daily rollover, which the engine does not
/// model: the moving-average cross holds for days, so the verdict names that cost.
#[test]
fn a_position_held_through_an_unmodelled_rollover_is_warned_of() {
    let (bars, asks) = fx_bars(0.01);
    let r = fx_run(&spread_only_plan(), &bars, &asks, None);
    let trades = r.walk_forward.as_ref().unwrap().trades.len();
    assert_eq!(r.verdict.warnings.len(), 1, "{:?}", r.verdict.warnings);
    let warning = &r.verdict.warnings[0];
    let named = format!("of {trades} out-of-sample trades held a position through the overnight");
    assert!(warning.contains(&named), "{warning}");
}

/// A prop-firm rule's pass odds run to its own horizon and its breach odds to theirs: a shorter
/// pass horizon changes the pass odds alone, and equal horizons give the odds as before.
#[test]
fn the_pass_odds_run_to_their_own_horizon() {
    let bars = edge_bars(DAYS);
    let plan = plan(DAYS);
    let odds = |pass_horizon_days| {
        let VerdictRule::PropFirm { firm, limits, .. } = prop_firm_rule() else {
            unreachable!()
        };
        let rule = VerdictRule::PropFirm {
            firm,
            limits: PropFirmLimits {
                pass_horizon_days,
                ..limits
            },
            attempt: None,
        };
        let r = battery(&plan, &rule, &bars, 7, None).unwrap();
        let odds = r.prop_firm_odds.unwrap();
        assert_eq!(r.kind.pass_probability, Some(odds.pass));
        assert_eq!(r.kind.breach_probability, Some(odds.breach));
        odds
    };
    let default = PropFirmLimits::default();
    assert_eq!(default.pass_horizon_days, 90);
    assert_eq!(default.pass_horizon_days, default.breach_horizon_days);
    let (same, short) = (odds(90), odds(1));
    assert_eq!(short.breach, same.breach);
    assert!(short.pass < same.pass, "{short:?} against {same:?}");
}

/// A prop-firm rule with attempt terms prices an attempt beside the odds, and its value line
/// reads the value at the reduced edge; without terms there is neither.
#[test]
fn attempt_terms_price_an_attempt_and_add_the_value_line() {
    let bars = edge_bars(DAYS);
    let plan = plan(DAYS);
    let plain = battery(&plan, &prop_firm_rule(), &bars, 7, None).unwrap();
    assert!(plain.prop_firm_attempt.is_none());
    assert!(plain.kind.attempt_value.is_none());
    assert!(
        plain
            .verdict
            .criteria
            .iter()
            .all(|c| c.id != "attempt_value")
    );

    let VerdictRule::PropFirm { firm, limits, .. } = prop_firm_rule() else {
        unreachable!()
    };
    let terms = AttemptTerms {
        account_size: 50_000.0,
        evaluation_fee: 300.0,
        pass_fee: 0.0,
        refund_fee_with_first_payout: true,
        profit_split: 0.8,
        days_to_first_payout: 10,
        calendar_days_to_first_payout: None,
        payout_interval_days: 10,
        min_payout: 50.0,
        payout_cap: None,
        max_payouts: None,
        payout_locks_floor: false,
        payout_resets_balance: false,
        funded_horizon_days: 90,
        evaluation_size: 1.0,
        funded_size: 1.0,
        reduced_edge_factor: 0.5,
    };
    let rule = VerdictRule::PropFirm {
        firm,
        limits,
        attempt: Some(terms),
    };
    let priced = battery(&plan, &rule, &bars, 7, None).unwrap();
    let attempt = priced.prop_firm_attempt.unwrap();
    assert_eq!(attempt.reduced_edge_factor, 0.5);
    assert_eq!(priced.kind.attempt_value, Some(attempt.reduced.value));
    assert_eq!(
        line(&priced, "attempt_value").value,
        Some(attempt.reduced.value)
    );
    // The attempt draws from its own seed stream, so the pass odds are the same with or without
    // terms. The breach odds walk the funded account with the terms' payouts; on these days every
    // path breaches either way.
    assert_eq!(priced.prop_firm_odds, plain.prop_firm_odds);
    // With no passing fee, an attempt loses at most its evaluation fee.
    for f in [attempt.full, attempt.reduced] {
        assert!(f.value >= -terms.evaluation_fee, "{f:?}");
        assert!(
            (0.0..=1.0).contains(&f.pass) && f.payout_chance <= f.pass,
            "{f:?}"
        );
    }
}
