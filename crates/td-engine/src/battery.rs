//! The strategy test battery (#35): every test of a strategy's plan on one instrument, run as one
//! job, ending in a verdict (#32).
//!
//! [`run_battery`] runs in #35's order:
//!
//! 1. **Integrity on the plan's backtest.** I1 counts the buckets of the period's open hours that
//!    have no bar; I2 runs the backtest twice and compares fills and equity; I3 runs it on the
//!    first half of the period alone and compares the orders decided there with the full run's.
//!    If any of them fails, the verdict is `invalid` and nothing else runs.
//! 2. **The sweep** over the full period, for the parameter plateau (R5).
//! 3. **R1**, the rolling walk-forward, stitched: the out-of-sample curve every later line is
//!    judged on.
//! 4. **The statistics that need no further runs:** R2, R5, R7 (a block bootstrap of the daily
//!    returns), R8 (where the profit came from), I4, I5 and the kind rule's inputs.
//! 5. **R4**, the walk-forward's chosen parameters run again on each test window at
//!    `cost_stress` times the fees and slippage.
//! 6. **R6**, the walk-forward again with its windows moved by `start_offsets` different offsets.
//! 7. **R3**, the luck test, last: the sweep repeated on shuffled bars.
//!
//! Steps 5 to 7 run only when their lines could still change the verdict. When the lines before
//! them already decide it (a sample below the minimum, a failed evidence line, or a failed I5),
//! whatever R3, R4 and R6 gave would leave the state as it is, so they are skipped and their
//! lines say so.
//!
//! # Progress and cancelling
//!
//! The battery reports one running total over every run it makes: the three integrity backtests,
//! every backtest of the sweep, of the walk-forward's train sweeps, of R6's walk-forwards and of
//! R3's sweeps, and R4's runs. (The tester's own extra runs, a sweep's best run again and a
//! walk-forward's test runs, are not reported by it and so not counted.) The total is known before
//! anything runs. On a full run every count from 1 to the total is reported once; when steps are
//! skipped, or a walk-forward or the luck test stops early, the count jumps ahead and the battery
//! reports the total itself at the end.
//! (Reports come from the worker threads, so they can arrive slightly out of order.)
//!
//! The progress callback is also the cancel signal, as for every tester call: returning
//! `ControlFlow::Break(())` stops the battery. Before R3, that returns
//! [`TesterError::Cancelled`]. During R3 the battery returns a verdict with the luck line
//! inconclusive and a note: every other line is complete, and only the luck test is missing.
//!
//! # Determinism
//!
//! The same plan, rule, data and seed give an identical result, at any thread count: every
//! tester call is deterministic, and every seeded statistic draws from a seed derived from the
//! battery's seed and its own fixed index.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use td_core::{Candle, FeeSchedule, Instrument, Millis};

use crate::metrics::{self, DAY_MS, EquitySeries};
use crate::plan::check_start_offsets;
use crate::risk::{self, DayRollover};
use crate::rng::derive_seed;
use crate::runner::select_bars;
use crate::stats::{
    self, BootstrapSpec, DEFAULT_RESAMPLES, MeanBootstrap, PropFirmOdds, PropFirmSpec, RuinSpec,
};
use crate::verdict::{
    IntegrityInputs, OosIntegrityInputs, evaluate_integrity, evaluate_oos_integrity,
};
use crate::{
    BacktestConfig, BacktestResult, Criterion, Direction, DrawdownMode, EquityPoint,
    EvidenceInputs, KindInputs, MAX_RUNS, Order, Outcome, PermutationResult, PermutationSpec,
    PortfolioBenchmark, Progress, SweepResult, SweepRow, TestPlan, TesterError, Trade, TradeSide,
    Verdict, VerdictRule, WalkForwardResult, WalkForwardSpec, combine, evaluate, evaluate_evidence,
    permutation_test, run, sweep, walk_forward,
};

/// The seed streams of the battery's seeded steps, derived from its seed.
const SEED_DAILY_BOOTSTRAP: u64 = 1;
const SEED_TRADE_BOOTSTRAP: u64 = 2;
const SEED_RUIN: u64 = 3;
const SEED_PROP_FIRM: u64 = 4;
const SEED_LUCK: u64 = 5;

/// The evidence lines of the runs that come after the statistics (R3, R4, R6).
const LATER_LINES: [&str; 3] = ["luck_p", "stressed_return", "start_offsets"];

/// Everything one battery produced: the verdict, and the runs and numbers behind it for saving
/// and showing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatteryResult {
    pub verdict: Verdict,
    pub seed: u64,
    /// The runs the progress counted, its total.
    pub runs: usize,
    pub integrity: IntegrityReport,
    /// The sweep over the full period. `None` when integrity failed.
    pub sweep: Option<SweepResult>,
    /// R1. `None` when integrity failed, no window fits the data, or a train window had no
    /// qualifying run (`walk_forward_error` says which).
    pub walk_forward: Option<WalkForwardResult>,
    pub walk_forward_error: Option<String>,
    /// The inputs of the evidence lines.
    pub evidence: EvidenceInputs,
    /// The inputs of the kind lines.
    pub kind: KindInputs,
    pub oos_integrity: OosIntegrityInputs,
    /// R5's neighbours of the best parameter set on the grid.
    pub plateau: Option<PlateauReport>,
    /// R7: the block bootstrap of the out-of-sample daily returns.
    pub daily_bootstrap: Option<MeanBootstrap>,
    /// The bootstrap of the closed out-of-sample trades' net P&L, for `high_risk`'s lower bound.
    pub trade_bootstrap: Option<MeanBootstrap>,
    /// The prop-firm challenge odds, for a `prop_firm` rule.
    pub prop_firm_odds: Option<PropFirmOdds>,
    /// R8.
    pub breakdown: Option<Breakdown>,
    /// R4. `None` when it did not run.
    pub cost_stress: Option<CostStress>,
    /// R6, one per offset. Empty when it did not run.
    pub start_offsets: Vec<StartOffset>,
    /// R3. `None` when it did not run, was cancelled or had no qualifying real sweep
    /// (`luck_note` says which).
    pub permutation: Option<PermutationResult>,
    pub luck_note: Option<String>,
}

/// The counts behind I1 to I3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrityReport {
    /// Buckets of the period's open hours: those not wholly inside the market's closed hours.
    pub open_buckets: usize,
    /// Open buckets with no bar.
    pub dropped_buckets: usize,
    pub inputs: IntegrityInputs,
    /// I3's split: the first half is the bars that open before it.
    pub prefix_split: Option<Millis>,
}

/// R5: the best parameter set and its neighbours on the grid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlateauReport {
    /// The best row's grid coordinates.
    pub best: Vec<u64>,
    pub best_objective: f64,
    /// The neighbours' grid coordinates and objective values, `None` where the objective is
    /// undefined (it counts as the worst value).
    pub neighbours: Vec<(Vec<u64>, Option<f64>)>,
    pub median_ratio: Option<f64>,
    pub positive_share: Option<f64>,
}

/// R4: each test window's chosen parameters run again at higher costs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostStress {
    pub multiplier: f64,
    /// Each window's return at the stressed costs.
    pub window_returns: Vec<f64>,
    /// The windows compounded, as the stitched curve compounds them.
    pub total_return: f64,
}

/// R6: one walk-forward with its windows moved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartOffset {
    /// How many of the period's last bars this run leaves out, which moves every window boundary
    /// back by as many bars.
    pub shift_bars: usize,
    /// The windows that fit.
    pub windows: usize,
    /// The stitched out-of-sample return; `None` when the walk-forward could not run.
    pub oos_return: Option<f64>,
    pub error: Option<String>,
}

/// R8: where the out-of-sample profit came from. Reported only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Breakdown {
    pub long: SideResult,
    pub short: SideResult,
    /// Net funding paid on the stitched curve; negative when it was received.
    pub funding_paid: f64,
    /// Windows where buy and hold rose over the test window, and the strategy's return in them,
    /// compounded.
    pub rising: RegimeResult,
    /// Windows where it fell or stayed flat.
    pub falling: RegimeResult,
    /// The best closed trade's net P&L.
    pub best_trade_pnl: Option<f64>,
    /// The out-of-sample return without it.
    pub return_without_best_trade: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SideResult {
    /// Closed trades.
    pub trades: usize,
    /// Their net P&L, in the quote asset.
    pub net_pnl: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct RegimeResult {
    pub windows: usize,
    pub strategy_return: f64,
}

/// One running count over every run, reported through the caller's callback.
struct Meter<'a> {
    progress: Option<Progress<'a>>,
    total: usize,
    /// Runs counted before the current step.
    base: usize,
    /// The highest count reported so far.
    reported: AtomicUsize,
}

impl Meter<'_> {
    fn report(&self, done: usize) -> ControlFlow<()> {
        self.reported.fetch_max(done, Ordering::Relaxed);
        self.progress
            .map_or(ControlFlow::Continue(()), |p| p(done, self.total))
    }

    /// Counts one run of the battery's own and checks for a cancel.
    fn tick(&mut self) -> Result<(), TesterError> {
        self.base += 1;
        match self.report(self.base) {
            ControlFlow::Break(()) => Err(TesterError::Cancelled),
            ControlFlow::Continue(()) => Ok(()),
        }
    }

    /// Reports the total when steps were skipped, stopped early or cancelled, so the count always
    /// reaches it. A cancel here changes nothing: the battery has finished.
    fn finish(&self) {
        if self.reported.load(Ordering::Relaxed) < self.total {
            let _ = self.report(self.total);
        }
    }

    /// The callback for a tester call whose runs start at the current count.
    fn step(&self) -> impl Fn(usize, usize) -> ControlFlow<()> + Sync + '_ {
        let base = self.base;
        move |done, _| self.report(base + done)
    }
}

/// One battery job: which strategy, under which plan and rule, on which instrument.
#[derive(Debug, Clone, Copy)]
pub struct BatterySpec<'a> {
    /// A registered strategy's name.
    pub strategy: &'a str,
    pub plan: &'a TestPlan,
    pub rule: &'a VerdictRule,
    /// One of the plan's instruments.
    pub instrument: &'a Instrument,
    /// Seeds the luck test and the bootstraps.
    pub seed: u64,
}

/// Runs the battery of `spec`. `bars` holds the plan's period and may hold warm-up bars before
/// it; `funding` is `(time, rate)` in any order.
///
/// The instrument must be one of the plan's and the rule must pass its checks. The plan itself is
/// not checked again with [`TestPlan::validate`]: the registry's test checks every registered
/// plan, and its floors (the permutation count, the period holding the minimum sample) would stop
/// a test from running a smaller battery. A period that holds too little data gives
/// `not_enough_evidence`.
///
/// Errors: a refused rule or spec, a luck test over [`MAX_RUNS`] (refused before anything runs),
/// a data or engine error, or a cancel before R3.
pub fn run_battery(
    spec: &BatterySpec,
    bars: &[Candle],
    funding: &[(Millis, f64)],
    progress: Option<Progress>,
) -> Result<BatteryResult, TesterError> {
    let BatterySpec {
        strategy,
        plan,
        rule,
        instrument,
        seed,
    } = *spec;
    let instrument = instrument.clone();
    if !plan.instruments.contains(&instrument.id) {
        return Err(TesterError::InvalidSpec(format!(
            "{} is not one of the plan's instruments",
            instrument.id
        )));
    }
    rule.validate()
        .map_err(|e| TesterError::InvalidSpec(e.to_string()))?;
    let base = plan.backtest_config(strategy, instrument.clone());
    let sweep_spec = plan.sweep_spec(strategy, instrument.clone());
    let wf_spec = plan.walk_forward_spec(strategy, instrument.clone());
    wf_spec.check()?;
    let ev = &plan.evidence;
    check_start_offsets(wf_spec.windows.test_bars, ev.start_offsets)
        .map_err(TesterError::InvalidSpec)?;
    let (_, traded) = select_bars(&base, bars)?;

    // The total before anything runs.
    let per_sweep = sweep_spec.runs()?;
    let window_count = |bars: usize| wf_spec.windows.windows(bars).map_or(0, |w| w.len());
    let windows = window_count(traded.len());
    let offsets = start_offset_shifts(wf_spec.windows.test_bars, ev.start_offsets as usize);
    let offset_windows: Vec<usize> = offsets
        .iter()
        .map(|&s| window_count(traded.len().saturating_sub(s)))
        .collect();
    let permutations = ev.luck.permutations as usize;
    // R3 runs last but is refused up front, before the hours of runs ahead of it.
    let luck_runs = per_sweep as u128 * (permutations as u128 + 1);
    if luck_runs > MAX_RUNS as u128 {
        return Err(TesterError::TooManyRuns {
            runs: luck_runs,
            limit: MAX_RUNS,
        });
    }
    let total = 3
        + per_sweep
        + windows * per_sweep
        + windows
        + offset_windows.iter().sum::<usize>() * per_sweep
        + per_sweep * (permutations + 1);
    let mut meter = Meter {
        progress,
        total,
        base: 0,
        reported: AtomicUsize::new(0),
    };

    // 1. Integrity: I1 to I3.
    let (open_buckets, dropped_buckets) = bucket_counts(&base, &traded);
    let first = run(&base, bars, funding)?;
    meter.tick()?;
    let again = run(&base, bars, funding)?;
    meter.tick()?;
    let split = (traded.len() >= 2).then(|| traded[traded.len() / 2].open_time);
    let prefix_differences = match split {
        Some(at) => {
            let half = run(
                &BacktestConfig {
                    to: at,
                    ..base.clone()
                },
                bars,
                funding,
            )?;
            Some(prefix_differences(&first.orders, &half.orders, at))
        }
        None => None,
    };
    meter.tick()?;
    let integrity_inputs = IntegrityInputs {
        dropped_share: (open_buckets > 0).then(|| dropped_buckets as f64 / open_buckets as f64),
        repeat_differences: Some(repeat_differences(&first, &again)),
        prefix_differences,
    };
    let integrity = IntegrityReport {
        open_buckets,
        dropped_buckets,
        inputs: integrity_inputs.clone(),
        prefix_split: split,
    };
    let mut criteria = evaluate_integrity(&integrity_inputs);
    let mut result = BatteryResult {
        verdict: Verdict::new(instrument.id.clone(), Vec::new()),
        seed,
        runs: total,
        integrity,
        sweep: None,
        walk_forward: None,
        walk_forward_error: None,
        evidence: EvidenceInputs::default(),
        kind: KindInputs::default(),
        oos_integrity: OosIntegrityInputs::default(),
        plateau: None,
        daily_bootstrap: None,
        trade_bootstrap: None,
        prop_firm_odds: None,
        breakdown: None,
        cost_stress: None,
        start_offsets: Vec::new(),
        permutation: None,
        luck_note: None,
    };
    if criteria.iter().any(|c| c.outcome == Outcome::Fail) {
        meter.finish();
        result.verdict = Verdict::new(instrument.id, criteria);
        return Ok(result);
    }

    // 2. The sweep over the full period.
    let swept = sweep(&sweep_spec, bars, funding, Some(&meter.step()))?;
    meter.base += per_sweep;

    // 3. R1: the rolling walk-forward, stitched.
    let (wf, wf_error) = if windows == 0 {
        let error = match wf_spec.windows.windows(traded.len()) {
            Err(e) => e.to_string(),
            Ok(_) => "no walk-forward window fits the period".into(),
        };
        (None, Some(error))
    } else {
        match walk_forward(&wf_spec, bars, funding, Some(&meter.step())) {
            Ok(wf) => (Some(wf), None),
            Err(e @ (TesterError::NoQualifyingRun { .. } | TesterError::InvalidSpec(_))) => {
                (None, Some(e.to_string()))
            }
            Err(e) => return Err(e),
        }
    };
    meter.base += windows * per_sweep;

    // 4. The statistics that need no further runs.
    let mut evidence = EvidenceInputs::default();
    let mut kind = KindInputs::default();
    let mut oos_integrity = OosIntegrityInputs::default();
    let plateau = plateau(
        &swept,
        sweep_spec.objective.metric,
        sweep_spec.objective.direction,
    );
    if let Some(p) = &plateau {
        evidence.plateau_median_ratio = p.median_ratio;
        evidence.plateau_positive_share = p.positive_share;
    }
    let cash = base.starting_cash;
    if let Some(wf) = &wf {
        let m = &wf.metrics;
        evidence.oos_return = m.total_return;
        evidence.profitable_windows = Some(
            wf.windows
                .iter()
                .filter(|w| {
                    w.out_of_sample
                        .metrics
                        .total_return
                        .is_some_and(|r| r > 0.0)
                })
                .count() as f64
                / wf.windows.len() as f64,
        );
        evidence.oos_trades = m.trades;
        evidence.oos_days =
            (wf.equity.len() as i64 * plan.timeframe.duration_ms()) as f64 / DAY_MS as f64;
        evidence.windows = wf.windows.len();

        let series = EquitySeries::new(cash, &wf.equity, plan.timeframe);
        let daily = daily_returns(&series);
        result.daily_bootstrap = stats::bootstrap_mean(
            &daily,
            &BootstrapSpec::daily(daily.len(), derive_seed(seed, SEED_DAILY_BOOTSTRAP)),
        )
        .ok();
        let closed: Vec<f64> = wf
            .trades
            .iter()
            .filter(|t| !t.open)
            .map(|t| t.net_pnl)
            .collect();
        result.trade_bootstrap = stats::bootstrap_mean(
            &closed,
            &BootstrapSpec::trades(derive_seed(seed, SEED_TRADE_BOOTSTRAP)),
        )
        .ok();
        let breakdown = breakdown(wf, bars, cash);

        oos_integrity = OosIntegrityInputs {
            sharpe: m.sharpe,
            peak_leverage: m.peak_leverage,
            lowest_equity: wf
                .equity_extremes
                .iter()
                .map(|r| r.low)
                .reduce(f64::min)
                .map(|low| low / cash),
            curve: true,
        };

        kind = KindInputs {
            sharpe: m.sharpe,
            max_drawdown: m.max_drawdown,
            benchmark_max_drawdown: wf.benchmark.as_ref().and_then(|b| b.metrics.max_drawdown),
            alpha: match rule {
                VerdictRule::Portfolio {
                    benchmark: PortfolioBenchmark::Cash,
                    ..
                } => m.cagr,
                _ => wf.benchmark.as_ref().and_then(|b| b.alpha),
            },
            p_mean_not_positive: result
                .daily_bootstrap
                .as_ref()
                .map(|b| b.p_mean_at_most_zero),
            trades: m.trades,
            expectancy: m.avg_trade_pnl,
            expectancy_lower_bound: result.trade_bootstrap.as_ref().map(|b| b.lower_bound),
            profit_factor: m.profit_factor,
            risk_of_ruin: match rule {
                VerdictRule::HighRisk { limits } => {
                    let seed = derive_seed(seed, SEED_RUIN);
                    ruin(&wf.trades, &wf.equity, cash, limits.ruin_drawdown, seed)
                }
                _ => None,
            },
            return_without_best_trade: breakdown.return_without_best_trade,
            worst_daily_loss: worst_daily_loss(&series, &wf.equity_extremes, cash),
            intrabar_drawdown_static: m.static_drawdown_intrabar,
            intrabar_drawdown_trailing: risk::trailing_drawdown(cash, &wf.equity_extremes)
                .map(|d| d.amount / cash),
            pass_probability: None,
            breach_probability: None,
        };
        if let VerdictRule::PropFirm { firm, limits } = rule {
            let spec = PropFirmSpec {
                target: firm.profit_target,
                daily_loss_limit: firm.daily_loss,
                max_drawdown: firm.max_drawdown,
                drawdown: match firm.drawdown {
                    DrawdownMode::Static => stats::DrawdownMode::Static,
                    DrawdownMode::Trailing => stats::DrawdownMode::Trailing,
                },
                // The pass odds use the breach horizon too, so they are the chance of passing
                // within `breach_horizon_days`: a product decision, since the firm rule sets no
                // time limit of its own (see `FirmLimits`).
                horizon_days: limits.breach_horizon_days as usize,
                paths: DEFAULT_RESAMPLES,
                mean_block: stats::default_block_len(daily.len()),
                seed: derive_seed(seed, SEED_PROP_FIRM),
            };
            result.prop_firm_odds = stats::prop_firm_odds(&daily, &spec).ok();
            kind.pass_probability = result.prop_firm_odds.map(|o| o.pass);
            kind.breach_probability = result.prop_firm_odds.map(|o| o.breach);
        }
        result.breakdown = Some(breakdown);
    }
    criteria.extend(evaluate_oos_integrity(
        &oos_integrity,
        plan.backtest.max_leverage,
    ));
    let kind_lines = evaluate(rule, &kind);

    // 5 to 7 run only when their lines could still change the verdict.
    let decided =
        later_lines_cannot_change(&criteria, &evaluate_evidence(ev, &evidence), &kind_lines);
    if let (Some(wf), false) = (&wf, decided) {
        // 5. R4: the chosen parameters on each test window at higher costs.
        let fees = base.fee_override.unwrap_or(instrument.fees);
        let stressed_fees = FeeSchedule {
            maker: fees.maker * ev.cost_stress,
            taker: fees.taker * ev.cost_stress,
        };
        let mut window_returns = Vec::with_capacity(wf.windows.len());
        let mut level = 1.0f64;
        for w in &wf.windows {
            let config = BacktestConfig {
                fee_override: Some(stressed_fees),
                slippage_bps: w.out_of_sample.config.slippage_bps * ev.cost_stress,
                ..w.out_of_sample.config.clone()
            };
            let r = run(&config, bars, funding)?;
            meter.tick()?;
            let growth = r.equity.last().map_or(1.0, |p| p.equity / cash);
            window_returns.push(growth - 1.0);
            // Once the account is gone, later windows trade nothing, as in the stitched curve.
            level = if level > 0.0 { level * growth } else { 0.0 };
        }
        meter.base += windows - wf.windows.len();
        let stress = CostStress {
            multiplier: ev.cost_stress,
            window_returns,
            total_return: level.max(0.0) - 1.0,
        };
        evidence.stressed_return = Some(stress.total_return);
        result.cost_stress = Some(stress);

        // 6. R6: the walk-forward with its windows moved back.
        let main_sign = evidence.oos_return.map(sign);
        for (&shift, &planned) in offsets.iter().zip(&offset_windows) {
            let mut offset = StartOffset {
                shift_bars: shift,
                windows: planned,
                oos_return: None,
                error: None,
            };
            if planned > 0 {
                // The range ends at the open of the first bar left out. Every shift is at least
                // 1 (`check_start_offsets`), and a window fits in the `len - shift` bars before
                // it, so that bar exists.
                let mut spec: WalkForwardSpec = wf_spec.clone();
                spec.sweep.base.to = traded[traded.len() - shift].open_time;
                match walk_forward(&spec, bars, funding, Some(&meter.step())) {
                    Ok(r) => offset.oos_return = r.metrics.total_return,
                    Err(
                        e @ (TesterError::NoQualifyingRun { .. } | TesterError::InvalidSpec(_)),
                    ) => {
                        offset.error = Some(e.to_string());
                    }
                    Err(e) => return Err(e),
                }
            } else {
                offset.error = Some("no walk-forward window fits the shortened range".into());
            }
            meter.base += planned * per_sweep;
            result.start_offsets.push(offset);
        }
        evidence.start_offsets_same_sign = result
            .start_offsets
            .iter()
            .filter(|o| main_sign.is_some() && o.oos_return.map(sign) == main_sign)
            .count();

        // 7. R3: the luck test, last.
        let spec = PermutationSpec {
            sweep: sweep_spec.clone(),
            permutations,
            seed: derive_seed(seed, SEED_LUCK),
        };
        match permutation_test(&spec, bars, funding, Some(&meter.step())) {
            Ok(p) => {
                evidence.luck_p = Some(p.p_value);
                let smallest = 1.0 / (p.qualified_copies as f64 + 1.0);
                if smallest > ev.luck.pass_p {
                    result.luck_note = Some(format!(
                        "Only {} of {} shuffled copies had a qualifying run, so the smallest p the \
                         test could give was {smallest:.3}, above the pass threshold {}.",
                        p.qualified_copies, p.spec.permutations, ev.luck.pass_p
                    ));
                }
                result.permutation = Some(p);
            }
            Err(TesterError::Cancelled) => {
                result.luck_note = Some(
                    "The luck test was cancelled, so it gave no p-value; run the battery again \
                     to complete it."
                        .into(),
                );
            }
            Err(e @ TesterError::NoQualifyingRun { .. }) => {
                result.luck_note = Some(format!("The luck test could not run: {e}."));
            }
            Err(e) => return Err(e),
        }
        meter.base += per_sweep * (permutations + 1);
    }

    let mut evidence_lines = evaluate_evidence(ev, &evidence);
    for line in &mut evidence_lines {
        if decided && LATER_LINES.contains(&line.id.as_str()) {
            line.outcome = Outcome::Inconclusive;
            line.value = None;
            line.note = Some(
                "Not run: the lines above already decide the verdict, whatever this test gave."
                    .into(),
            );
        } else if line.id == "luck_p"
            && let Some(note) = &result.luck_note
        {
            line.note = Some(note.clone());
        }
    }
    criteria.extend(evidence_lines);
    criteria.extend(kind_lines);
    meter.finish();

    result.verdict = Verdict::new(instrument.id, criteria);
    result.sweep = Some(swept);
    result.walk_forward = wf;
    result.walk_forward_error = wf_error;
    result.evidence = evidence;
    result.kind = kind;
    result.oos_integrity = oos_integrity;
    result.plateau = plateau;
    Ok(result)
}

/// Whether R3, R4 and R6 can no longer change the verdict: the state comes out the same whatever
/// each of their lines turns out to be (passed, failed or inconclusive).
fn later_lines_cannot_change(
    integrity: &[Criterion],
    evidence: &[Criterion],
    kind: &[Criterion],
) -> bool {
    let outcomes = [Outcome::Pass, Outcome::Fail, Outcome::Inconclusive];
    let mut states = Vec::with_capacity(27);
    for a in outcomes {
        for b in outcomes {
            for c in outcomes {
                let mut lines: Vec<Criterion> = integrity.to_vec();
                lines.extend(evidence.iter().cloned().map(|mut l| {
                    match LATER_LINES.iter().position(|id| *id == l.id) {
                        Some(0) => l.outcome = a,
                        Some(1) => l.outcome = b,
                        Some(_) => l.outcome = c,
                        None => {}
                    }
                    l
                }));
                lines.extend(kind.iter().cloned());
                states.push(combine(&lines));
            }
        }
    }
    states.windows(2).all(|w| w[0] == w[1])
}

/// R6's `n` shifts, in bars left out at the end of the period: the k-th is `k / (n + 1)` of a
/// test window, rounded down, so the window boundaries move to `n` evenly spaced points inside a
/// test window. With at least `n + 1` bars per test window (`check_start_offsets`) the shifts
/// step by at least one bar and stay below a test window, so they are different, non-zero and
/// never land back on the main run's boundaries.
fn start_offset_shifts(test_bars: usize, n: usize) -> Vec<usize> {
    (1..=n).map(|k| k * test_bars / (n + 1)).collect()
}

/// -1, 0 or 1.
fn sign(x: f64) -> i8 {
    if x > 0.0 {
        1
    } else if x < 0.0 {
        -1
    } else {
        0
    }
}

/// I1: the buckets of `config`'s period that are not wholly inside the market's closed hours,
/// and how many of them have no bar among `traded`, the bars of the period in time order.
fn bucket_counts(config: &BacktestConfig, traded: &[Candle]) -> (usize, usize) {
    let step = config.timeframe.duration_ms();
    let kind = config.instrument.id.kind;
    let mut times = traded.iter().map(|b| b.open_time).peekable();
    let (mut open, mut dropped) = (0, 0);
    let mut t = config.from;
    while t < config.to {
        while times.next_if(|&x| x < t).is_some() {}
        let present = times.next_if_eq(&t).is_some();
        if kind.closed_spans(t, t + step).first() != Some(&(t..t + step)) {
            open += 1;
            dropped += usize::from(!present);
        }
        t += step;
    }
    (open, dropped)
}

/// I2: 0 when the two runs have identical fills and equity, 1 otherwise.
fn repeat_differences(a: &BacktestResult, b: &BacktestResult) -> usize {
    usize::from(a.fills != b.fills || a.equity != b.equity)
}

/// I3: the orders decided before `split` that differ between `full` and `prefix`, a run of the
/// same backtest that ends at `split`. Orders are compared on what was decided (the time, the
/// request, the close it was decided on and any attached stop), not on their status, since an
/// order sent at the end of the prefix cannot fill there. An order only one run has counts as
/// one difference.
fn prefix_differences(full: &[Order], prefix: &[Order], split: Millis) -> usize {
    let decided = |o: &Order| {
        (
            o.submitted_at,
            o.request.clone(),
            o.decision_close,
            o.attached_stop,
        )
    };
    let before: Vec<_> = full
        .iter()
        .filter(|o| o.submitted_at < split)
        .map(decided)
        .collect();
    let prefix: Vec<_> = prefix.iter().map(decided).collect();
    let differing = before.iter().zip(&prefix).filter(|(a, b)| a != b).count();
    differing + before.len().abs_diff(prefix.len())
}

/// R5 on the sweep: the best row's neighbours are the rows that ran (the strategy did not refuse
/// them) and sit at most one step from it on every axis of the grid. An undefined objective counts
/// as the worst value. With `Maximize` the ratio is the neighbours' median over the best, defined
/// when the best is above zero; with `Minimize` it is the best over the median, so a ratio below 1
/// is worse either way. `None` when no row qualified.
fn plateau(
    swept: &SweepResult,
    metric: crate::Metric,
    direction: Direction,
) -> Option<PlateauReport> {
    let best = swept.rows.first().filter(|r| r.qualified)?;
    let best_objective = best.metrics.and_then(|m| m.value(metric))?;
    let near = |r: &SweepRow| {
        r.coords.len() == best.coords.len()
            && r.coords
                .iter()
                .zip(&best.coords)
                .all(|(a, b)| a.abs_diff(*b) <= 1)
    };
    let neighbours: Vec<&SweepRow> = swept
        .rows
        .iter()
        .filter(|r| r.index != best.index && r.error.is_none() && near(r))
        .collect();
    let objective = |r: &SweepRow| r.metrics.and_then(|m| m.value(metric));
    let positive_share = (!neighbours.is_empty()).then(|| {
        neighbours
            .iter()
            .filter(|r| {
                r.metrics
                    .and_then(|m| m.total_return)
                    .is_some_and(|x| x > 0.0)
            })
            .count() as f64
            / neighbours.len() as f64
    });
    let worst = match direction {
        Direction::Maximize => f64::NEG_INFINITY,
        Direction::Minimize => f64::INFINITY,
    };
    let mut values: Vec<f64> = neighbours
        .iter()
        .map(|r| objective(r).unwrap_or(worst))
        .collect();
    values.sort_by(f64::total_cmp);
    let median = match values.len() {
        0 => None,
        n if n % 2 == 1 => Some(values[n / 2]),
        n => Some((values[n / 2 - 1] + values[n / 2]) / 2.0),
    };
    let median_ratio = median
        .and_then(|m| match direction {
            Direction::Maximize if best_objective > 0.0 => Some(m / best_objective),
            Direction::Minimize if m > 0.0 => Some(best_objective / m),
            Direction::Minimize if m == 0.0 && best_objective == 0.0 => Some(1.0),
            _ => None,
        })
        .filter(|r| r.is_finite());
    Some(PlateauReport {
        best: best.coords.clone(),
        best_objective,
        neighbours: neighbours
            .iter()
            .map(|r| (r.coords.clone(), objective(r)))
            .collect(),
        median_ratio,
        positive_share,
    })
}

/// The returns from one UTC day's end to the next of an equity series. A day that starts with no
/// equity left has a return of zero: the account had nothing to lose.
fn daily_returns(series: &EquitySeries) -> Vec<f64> {
    metrics::daily_equity(&series.times, &series.values)
        .windows(2)
        .map(|w| if w[0] > 0.0 { w[1] / w[0] - 1.0 } else { 0.0 })
        .collect()
}

/// The prop-firm worst daily loss inside bars, as a fraction of the starting balance: the worse
/// of the days ending at midnight UTC and at 17:00 New York, since the rule does not say where its
/// firm rolls the day over. `None` without equity ranges.
fn worst_daily_loss(
    series: &EquitySeries,
    ranges: &[crate::EquityRange],
    cash: f64,
) -> Option<f64> {
    if ranges.is_empty() {
        return None;
    }
    [DayRollover::default(), DayRollover::NewYork1700]
        .into_iter()
        .map(|rollover| {
            let days = risk::daily_pnl(series, Some(ranges), rollover);
            risk::worst_daily_loss_intraday(&days, cash)
        })
        .collect::<Option<Vec<f64>>>()?
        .into_iter()
        .reduce(f64::max)
}

/// `high_risk`'s risk of ruin from the closed trades of the stitched curve: each trade's net P&L
/// over the stitched equity before its entry, drawn in blocks over as many trades as the sample
/// has. Ruin is a fall of the rule's `ruin_drawdown` from a path's running peak, as the rule
/// means it.
fn ruin(
    trades: &[Trade],
    equity: &[EquityPoint],
    cash: f64,
    ruin_drawdown: f64,
    seed: u64,
) -> Option<f64> {
    let returns = trade_returns(trades, equity, cash);
    let spec = RuinSpec {
        ruin_drawdown,
        horizon: returns.len(),
        paths: DEFAULT_RESAMPLES,
        mean_block: stats::default_block_len(returns.len()),
        seed,
    };
    stats::risk_of_ruin(&returns, &spec).ok()
}

/// Each closed trade's net P&L over the equity at the close of the last bar before its entry
/// (the starting cash before the first bar), skipping trades entered with no equity left.
fn trade_returns(trades: &[Trade], equity: &[EquityPoint], cash: f64) -> Vec<f64> {
    let equity_before = |t: Millis| {
        let i = equity.partition_point(|p: &EquityPoint| p.time < t);
        i.checked_sub(1).map_or(cash, |i| equity[i].equity)
    };
    trades
        .iter()
        .filter(|t| !t.open)
        .filter_map(|t| {
            let e = equity_before(t.entry_time);
            (e > 0.0).then(|| t.net_pnl / e)
        })
        .collect()
}

/// R8 on the stitched walk-forward.
fn breakdown(wf: &WalkForwardResult, bars: &[Candle], cash: f64) -> Breakdown {
    let mut long = SideResult::default();
    let mut short = SideResult::default();
    for t in wf.trades.iter().filter(|t| !t.open) {
        let side = match t.side {
            TradeSide::Long => &mut long,
            TradeSide::Short => &mut short,
        };
        side.trades += 1;
        side.net_pnl += t.net_pnl;
    }
    let mut rising = RegimeResult::default();
    let mut falling = RegimeResult::default();
    let (mut up, mut down) = (1.0, 1.0);
    for w in &wf.windows {
        let test: Vec<&Candle> = bars
            .iter()
            .filter(|b| (w.test_from..w.test_to).contains(&b.open_time))
            .collect();
        let held = match (test.first(), test.last()) {
            (Some(a), Some(b)) => b.close / a.open - 1.0,
            _ => 0.0,
        };
        let r = w.out_of_sample.metrics.total_return.unwrap_or(0.0);
        if held > 0.0 {
            rising.windows += 1;
            up *= 1.0 + r;
        } else {
            falling.windows += 1;
            down *= 1.0 + r;
        }
    }
    rising.strategy_return = up - 1.0;
    falling.strategy_return = down - 1.0;
    let best_trade_pnl = wf
        .trades
        .iter()
        .filter(|t| !t.open)
        .map(|t| t.net_pnl)
        .reduce(f64::max);
    let end = wf.equity.last().map_or(cash, |p| p.equity);
    Breakdown {
        long,
        short,
        funding_paid: wf.metrics.funding_paid,
        rising,
        falling,
        best_trade_pnl,
        return_without_best_trade: best_trade_pnl.map(|b| (end - b) / cash - 1.0),
    }
}

#[cfg(test)]
mod tests {
    use td_core::{
        InstrumentId, InstrumentKind, OrderRequest, OrderType, Side, TimeInForce, Venue,
    };

    use super::*;
    use crate::{AttachedStop, OrderStatus};

    fn order(submitted_at: Millis, qty: f64, status: OrderStatus) -> Order {
        Order {
            id: td_core::OrderId(submitted_at as u64),
            request: OrderRequest {
                instrument: InstrumentId {
                    venue: Venue::Binance,
                    symbol: "TESTUSDT".into(),
                    kind: InstrumentKind::Perpetual,
                },
                side: Side::Buy,
                qty,
                order_type: OrderType::Market,
                tif: TimeInForce::Gtc,
                reduce_only: false,
                client_tag: None,
            },
            submitted_at,
            decision_close: 100.0,
            attached_stop: None::<AttachedStop>,
            status,
        }
    }

    #[test]
    fn the_prefix_is_compared_on_the_orders_decided_before_the_split() {
        let full = vec![
            order(1, 1.0, OrderStatus::Filled),
            order(2, 2.0, OrderStatus::Filled),
            order(3, 3.0, OrderStatus::Filled),
        ];
        // The prefix ends at 3: its last order could not fill there, and the full run's order at 3
        // is not part of it.
        let prefix = vec![
            order(1, 1.0, OrderStatus::Filled),
            order(2, 2.0, OrderStatus::Open),
        ];
        assert_eq!(prefix_differences(&full, &prefix, 3), 0);
        // A different decision before the split counts.
        let changed = vec![
            order(1, 1.0, OrderStatus::Filled),
            order(2, 2.5, OrderStatus::Open),
        ];
        assert_eq!(prefix_differences(&full, &changed, 3), 1);
        // So does an order only one run has.
        assert_eq!(prefix_differences(&full, &prefix[..1], 3), 1);
        assert_eq!(prefix_differences(&full[..1], &prefix, 3), 1);
    }

    fn test_instrument() -> Instrument {
        Instrument {
            id: InstrumentId {
                venue: Venue::Binance,
                symbol: "TESTUSDT".into(),
                kind: InstrumentKind::Perpetual,
            },
            base: "TEST".into(),
            quote: "USDT".into(),
            tick_size: 0.01,
            lot_size: 0.001,
            min_qty: 0.001,
            min_notional: 1.0,
            fees: FeeSchedule {
                maker: 0.001,
                taker: 0.002,
            },
        }
    }

    const HOUR: Millis = 3_600_000;

    fn test_config() -> BacktestConfig {
        BacktestConfig {
            strategy: "sma_cross".into(),
            params: crate::Params::new()
                .with("fast", crate::ParamValue::Int(3))
                .with("slow", crate::ParamValue::Int(8)),
            instrument: test_instrument(),
            timeframe: td_core::Timeframe::H1,
            from: 0,
            to: 200 * HOUR,
            starting_cash: 10_000.0,
            slippage_bps: 2.0,
            fee_override: None,
            max_leverage: 1.0,
            warmup_from: None,
            close_at_end: false,
        }
    }

    #[test]
    fn a_repeat_must_match_in_fills_and_equity() {
        let bars: Vec<Candle> = (0..200)
            .map(|i| {
                let close = 100.0 + 10.0 * ((i as f64) / 15.0).sin();
                Candle {
                    open_time: i * HOUR,
                    open: close,
                    high: close * 1.001,
                    low: close * 0.999,
                    close,
                    volume: 1.0,
                    quote_volume: close,
                }
            })
            .collect();
        let config = test_config();
        let a = run(&config, &bars, &[]).unwrap();
        assert!(!a.fills.is_empty());
        assert_eq!(repeat_differences(&a, &a.clone()), 0);

        let mut equity = a.clone();
        equity.equity.last_mut().unwrap().equity += 1.0;
        assert_eq!(repeat_differences(&a, &equity), 1);

        let mut fills = a.clone();
        fills.fills[0].price += 0.01;
        assert_eq!(repeat_differences(&a, &fills), 1);
    }

    /// A row of a hand-built 3 x 3 sweep: its grid coordinates, its objective (Sharpe or max
    /// drawdown, as the test reads it) and its total return.
    fn row(coords: [u64; 2], objective: Option<f64>, total_return: f64) -> SweepRow {
        let headline = crate::Headline {
            total_return: Some(total_return),
            cagr: None,
            sharpe: objective,
            sortino: None,
            calmar: None,
            profit_factor: None,
            win_rate: None,
            max_drawdown: objective,
            trades: 10,
        };
        SweepRow {
            index: coords[0] * 3 + coords[1],
            coords: coords.to_vec(),
            params: crate::Params::new(),
            qualified: objective.is_some(),
            metrics: Some(headline),
            error: None,
        }
    }

    /// Rows best first, as a sweep returns them.
    fn swept(rows: Vec<SweepRow>) -> SweepResult {
        SweepResult {
            spec: crate::SweepSpec {
                base: test_config(),
                axes: Vec::new(),
                mode: crate::SweepMode::Grid,
                objective: crate::Objective {
                    metric: crate::Metric::Sharpe,
                    direction: Direction::Maximize,
                },
                min_trades: 0,
            },
            engine_version: String::new(),
            data_fingerprint: String::new(),
            rows,
            best: None,
        }
    }

    #[test]
    fn the_plateau_reads_the_best_sets_neighbours_on_the_grid() {
        use crate::Metric::{MaxDrawdown, Sharpe};
        // The best set in the corner (0, 0) has three neighbours: (0, 1), (1, 0) and (1, 1).
        // (1, 0)'s objective is undefined and counts as the worst; (0, 2) is refused and (2, 2),
        // better than every neighbour, is two steps away; neither counts.
        let mut refused = row([0, 2], None, 0.0);
        refused.metrics = None;
        refused.error = Some("slow must be more than fast".into());
        let rows = |best: f64| {
            vec![
                row([0, 0], Some(best), 0.4),
                row([2, 2], Some(1.9), 0.3),
                row([1, 1], Some(1.5), 0.2),
                row([0, 1], Some(1.0), 0.1),
                row([1, 2], Some(0.5), 0.05),
                row([1, 0], None, -0.05),
                refused.clone(),
            ]
        };
        let p = plateau(&swept(rows(2.0)), Sharpe, Direction::Maximize).unwrap();
        assert_eq!(p.best, vec![0, 0]);
        assert_eq!(p.best_objective, 2.0);
        assert_eq!(
            p.neighbours,
            vec![
                (vec![1, 1], Some(1.5)),
                (vec![0, 1], Some(1.0)),
                (vec![1, 0], None),
            ]
        );
        // Sorted [-inf, 1.0, 1.5]: the median is 1.0, half the best. Two of three neighbours
        // made money.
        assert_eq!(p.median_ratio, Some(0.5));
        assert_eq!(p.positive_share, Some(2.0 / 3.0));

        // With the best at or below zero a ratio would read backwards, so there is none; the
        // share still counts.
        for best in [0.0, -0.5] {
            let p = plateau(&swept(rows(best)), Sharpe, Direction::Maximize).unwrap();
            assert_eq!(p.median_ratio, None, "best {best}");
            assert_eq!(p.positive_share, Some(2.0 / 3.0));
        }

        // Two undefined neighbours of three put the median at the worst value: no finite ratio.
        let mut two_undefined = rows(2.0);
        two_undefined[3] = row([0, 1], None, 0.1);
        let p = plateau(&swept(two_undefined), Sharpe, Direction::Maximize).unwrap();
        assert_eq!(p.median_ratio, None);

        // Minimize, on max drawdown: the best is the smallest, an undefined neighbour counts as
        // the largest, and the ratio is the best over the median, so below 1 is worse here too.
        // Sorted [0.2, 0.3, inf]: the median is 0.3.
        let min_rows = vec![
            row([0, 0], Some(0.1), 0.4),
            row([0, 1], Some(0.2), 0.1),
            row([1, 1], Some(0.3), 0.2),
            row([2, 2], Some(0.4), 0.3),
            row([1, 0], None, -0.05),
        ];
        let p = plateau(&swept(min_rows), MaxDrawdown, Direction::Minimize).unwrap();
        assert_eq!(p.median_ratio, Some(0.1 / 0.3));
        assert_eq!(p.positive_share, Some(2.0 / 3.0));

        // No qualified best: no plateau.
        let none = vec![row([0, 0], None, 0.1), row([0, 1], None, 0.1)];
        assert_eq!(plateau(&swept(none), Sharpe, Direction::Maximize), None);
    }

    #[test]
    fn start_offsets_are_evenly_spaced_inside_a_test_window() {
        // 270 bars of test window, 8 offsets: k * 30 bars.
        assert_eq!(
            start_offset_shifts(270, 8),
            (1..=8).map(|k| 30 * k).collect::<Vec<_>>()
        );
        // 9 bars, the shortest test window 8 offsets may run on: k bars exactly.
        assert_eq!(start_offset_shifts(9, 8), (1..=8).collect::<Vec<_>>());
        assert_eq!(start_offset_shifts(20, 8), vec![2, 4, 6, 8, 11, 13, 15, 17]);
    }

    fn trade(entry_time: Millis, net_pnl: f64, open: bool) -> Trade {
        Trade {
            side: TradeSide::Long,
            entry_time,
            exit_time: entry_time + 1,
            open,
            qty: 1.0,
            entry_price: 100.0,
            exit_price: 100.0,
            gross_pnl: net_pnl,
            fees: 0.0,
            funding: 0.0,
            net_pnl,
            return_on_notional: 0.0,
            bars_held: 1,
            stop: None,
            r_multiple: None,
            mae: None,
            mfe: None,
            force_closed: false,
        }
    }

    #[test]
    fn risk_of_ruin_is_pinned_on_a_hand_made_trade_list() {
        let cash = 10_000.0;
        let point = |time, equity| EquityPoint { time, equity };
        let equity = [point(0, 10_000.0), point(2, 11_000.0), point(4, 5_500.0)];
        // +1,000 on the 10,000 before it (+10%), -5,500 on the 11,000 before it (-50%); the open
        // trade is left out. A trade entered before the first bar is measured on the cash.
        let trades = [
            trade(1, 1_000.0, false),
            trade(3, -5_500.0, false),
            trade(4, 50.0, true),
        ];
        assert_eq!(trade_returns(&trades, &equity, cash), vec![0.1, -0.5]);
        assert_eq!(
            trade_returns(&[trade(0, 500.0, false)], &equity, cash),
            vec![0.05]
        );
        // Two trades a path, drawn singly: every path but +10% twice is ruined at a drawdown of
        // half from the peak, since 1.1 then -50% is exactly half the 1.1 high. From the start
        // that path would end at 0.55 and survive, so the risk would be about 1/2, not 3/4.
        let risk = ruin(&trades, &equity, cash, 0.5, 3).unwrap();
        assert!((risk - 0.75).abs() < 0.035, "{risk}");
        assert_eq!(risk, PINNED_HAND_RUIN);
    }

    /// 1,482 of 2,000 paths, seed 3.
    const PINNED_HAND_RUIN: f64 = 0.741;
}
