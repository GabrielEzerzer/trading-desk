//! The strategy test battery (#35): every test of a strategy's plan on one instrument, run as one
//! job, ending in a verdict (#32).
//!
//! [`run_battery`] runs in #35's order:
//!
//! 1. **Integrity on the plan's backtest.** I1 counts the buckets of the period's open hours that
//!    have no bar; I2 runs the backtest twice and compares fills and equity; I3 runs it on the
//!    first half of the period alone and compares the orders decided there with the full run's.
//!    If any of them fails, the verdict is `invalid` and nothing else runs.
//! 2. **The sweep** over the full period, reported only.
//! 3. **R1**, the rolling walk-forward, stitched: the out-of-sample curve every later line is
//!    judged on. Then I2 and I3 again on each parameter set the walk-forward chose, the plan's
//!    backtest with those parameters over the same period, since the verdict rests on them: a
//!    plan whose fixed parameters place no orders passes I3 on its own backtest vacuously. Each
//!    line keeps the worst result over every set checked, and a failure there makes the verdict
//!    `invalid` too; the steps after the statistics are then skipped. Then R5's runs: each
//!    window's grid neighbours of the set it chose, run on its test bars (see [`PlateauReport`]),
//!    so the plateau is judged out of sample and never on a sweep's best row.
//! 4. **The statistics that need no further runs:** R2, R5, R7 (a block bootstrap of the daily
//!    returns), R8 (where the profit came from), I4, I5 and the kind rule's inputs, among them a
//!    block bootstrap of the max drawdown over the same daily returns for the `portfolio` kind.
//! 5. **R4**, the walk-forward's chosen parameters run again on each test window at
//!    `cost_stress` times the fees (the per-unit commission too) and slippage, and on a market
//!    with quotes at `cost_stress` times each bar's spread.
//! 6. **R6**, the walk-forward again with its windows moved by `start_offsets` different offsets.
//! 7. **R3**, the luck test, last: the sweep repeated on shuffled bars, with each bar's ask moved
//!    with it on a market with quotes.
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
//! R3's sweeps, R4's runs, three integrity runs per walk-forward window and R5's neighbour runs,
//! on every market. A window whose parameters were already checked counts its three without
//! running them again. R5 counts the most neighbours a set can have on the grid for each window
//! ([`SweepSpec::max_neighbours`]); a window whose chosen set sits at the end of an axis of three
//! or more values has fewer, and counts the rest without running them. (The tester's own extra
//! runs, a sweep's best run again and a walk-forward's test runs, are not reported by it and so
//! not counted.) The total is known before anything runs. On a full run every count from 1 to
//! the total is reported once; when steps are skipped, or a walk-forward or the luck test stops
//! early, the count jumps ahead and the battery reports the total itself at the end. (Reports
//! come from the worker threads, so they can arrive slightly out of order.)
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

use std::collections::{BTreeSet, HashMap};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use td_core::{
    AccountCurrency, Candle, Carry, FeeSchedule, Fill, Instrument, InstrumentId, Millis, Timeframe,
};

use crate::metrics::{self, DAY_MS, EquitySeries};
use crate::plan::check_start_offsets;
use crate::portfolio::next_position;
use crate::risk::{self, DayRollover};
use crate::rng::derive_seed;
use crate::runner::select_bars;
use crate::stats::{
    self, BootstrapSpec, DEFAULT_RESAMPLES, DrawdownBootstrap, MeanBootstrap, PropFirmAttempt,
    PropFirmOdds, PropFirmSpec, RuinSpec,
};
use crate::strategies::direction;
use crate::tester::stitched_headline;
use crate::verdict::{
    IntegrityInputs, OosIntegrityInputs, TradingDayTest, evaluate_integrity, evaluate_oos_integrity,
};
use crate::{
    BacktestConfig, BacktestResult, Criterion, Direction, DrawdownMode, EngineError, EquityPoint,
    EvidenceInputs, Headline, KindInputs, MAX_RUNS, Objective, Order, Outcome, Params,
    PermutationResult, PermutationSpec, PortfolioBenchmark, Progress, SweepResult, SweepSpec,
    TestPlan, TesterError, Trade, TradeSide, Verdict, VerdictRule, WalkForwardResult,
    WalkForwardSpec, combine, evaluate, evaluate_evidence, permutation_test, run, sweep,
    walk_forward,
};

/// The seed streams of the battery's seeded steps, derived from its seed.
const SEED_DAILY_BOOTSTRAP: u64 = 1;
const SEED_TRADE_BOOTSTRAP: u64 = 2;
const SEED_RUIN: u64 = 3;
const SEED_PROP_FIRM: u64 = 4;
const SEED_LUCK: u64 = 5;
const SEED_DRAWDOWN_BOOTSTRAP: u64 = 6;
const SEED_PROP_ATTEMPT: u64 = 7;

/// The evidence lines of the runs that come after the statistics (R3, R4, R6).
const LATER_LINES: [&str; 3] = ["luck_p", "stressed_return", "start_offsets"];

/// Everything one battery produced: the verdict, and the runs and numbers behind it for saving
/// and showing.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
    /// R5: the walk-forward's chosen sets against their grid neighbours, out of sample. `None`
    /// without a walk-forward.
    pub plateau: Option<PlateauReport>,
    /// R7: the block bootstrap of the out-of-sample daily returns.
    pub daily_bootstrap: Option<MeanBootstrap>,
    /// The block bootstrap of the max drawdown over the same daily returns, for a `portfolio`
    /// rule. `None` for the other kinds, without a curve, and in a result saved before it existed.
    pub drawdown_bootstrap: Option<DrawdownBootstrap>,
    /// The bootstrap of the closed out-of-sample trades' net P&L, for `high_risk`'s lower bound.
    pub trade_bootstrap: Option<MeanBootstrap>,
    /// The prop-firm challenge odds, for a `prop_firm` rule: passing within the rule's
    /// `pass_horizon_days`, breaching within its `breach_horizon_days`.
    pub prop_firm_odds: Option<PropFirmOdds>,
    /// The value of one attempt, for a `prop_firm` rule with attempt terms: the evaluation to the
    /// rule's `pass_horizon_days`, then the funded account. `None` otherwise, and in a result
    /// saved before it existed.
    pub prop_firm_attempt: Option<PropFirmAttempt>,
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
    /// True when a cancel stopped R3, so the luck line lacks its p-value. False when R3 finished,
    /// was skipped or did not run; a result saved before this field existed reads as false.
    #[serde(default)]
    pub luck_cancelled: bool,
}

/// The counts behind I1 to I3.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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

/// R5, out of sample: the walk-forward's chosen parameter sets against their neighbours on the
/// grid. A neighbour is a direction on the grid, a step of -1, 0 or 1 on each axis (not all 0).
/// In each window it is the set that window chose moved one step that way, run on the window's
/// test bars as its chosen set was (warmed up on the train bars, closed out at the end). Each
/// direction is judged only on the windows where its set ran: a window where it is off the grid
/// or the strategy refuses it is left out, never filled with the chosen set's own run, which would
/// pull the direction towards R1. Its runs are stitched as R1's curve is, and so are the chosen
/// sets' own runs in the same windows, its reference. A direction that ran in no window is left
/// out.
///
/// The ratio is the median over the directions of each one's objective against its reference: the
/// objective over the reference with `Maximize`, for a direction whose reference is above zero,
/// and the reference over the objective with `Minimize`, so a ratio below 1 is worse either way.
/// A direction with an undefined objective has the worst ratio (minus infinity with `Maximize`, 0
/// with `Minimize`); one whose reference is undefined, or at or below zero with `Maximize`, has
/// none and is left out of the median. The share is the directions whose stitched return is
/// above zero.
///
/// A result saved before R5 moved out of sample reads with `directions` empty and its ratio and
/// share as they were judged then, on the full-period sweep.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlateauReport {
    /// R1's stitched objective; `None` when it is undefined.
    pub best_objective: Option<f64>,
    #[serde(default)]
    pub directions: Vec<PlateauDirection>,
    pub median_ratio: Option<f64>,
    pub positive_share: Option<f64>,
}

/// One of R5's neighbours, stitched over the windows.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlateauDirection {
    /// The step on each axis of the grid, in the order of the plan's axes.
    pub steps: Vec<i8>,
    /// The windows in which the stepped set ran, the only ones it is judged on.
    pub windows: usize,
    /// The stitched objective; `None` when it is undefined.
    pub objective: Option<f64>,
    pub total_return: Option<f64>,
    /// The chosen sets' own runs in the same windows, stitched the same way: their objective.
    /// `None` when it is undefined, or in a result saved before directions had it.
    #[serde(default)]
    pub reference: Option<f64>,
}

/// R4: each test window's chosen parameters run again at higher costs.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostStress {
    /// The fees, the per-unit commission, slippage and, on a market with quotes, each bar's spread
    /// are multiplied by this.
    pub multiplier: f64,
    /// Each window's return at the stressed costs.
    pub window_returns: Vec<f64>,
    /// The windows compounded, as the stitched curve compounds them.
    pub total_return: f64,
}

/// R6: one walk-forward with its windows moved.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Breakdown {
    pub long: SideResult,
    pub short: SideResult,
    /// Net funding paid on the stitched curve; negative when it was received.
    pub funding_paid: f64,
    /// Windows where the instrument's price rose over the test window, and the strategy's return
    /// in them, compounded. The price is the quote currency per unit of the base, so a long
    /// position gained in those windows in either currency: on a dollar account trading USD/JPY,
    /// a rising window is one where the dollar strengthened against the yen.
    pub rising: RegimeResult,
    /// Windows where it fell or stayed flat.
    pub falling: RegimeResult,
    /// The best closed trade's net P&L.
    pub best_trade_pnl: Option<f64>,
    /// The out-of-sample return without it.
    pub return_without_best_trade: Option<f64>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SideResult {
    /// Closed trades.
    pub trades: usize,
    /// Their net P&L, in the account currency.
    pub net_pnl: f64,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
/// it. `bars`, `asks` and `funding` are the data as [`crate::run`] takes it: `asks` is empty for a
/// kind whose rules have no quotes, and every run, sweep, walk-forward and luck test gets them
/// (R4 with each spread widened).
///
/// The instrument must be one of the plan's and the rule must pass its checks. The plan itself is
/// not checked again with [`TestPlan::validate`]: the registry's test checks every registered
/// plan, and its floors (the permutation count, the period holding the minimum sample) would stop
/// a test from running a smaller battery. A period that holds too little data gives
/// `not_enough_evidence`.
///
/// Errors: a refused rule or spec, a luck test, or a walk-forward with R5's neighbour runs, over
/// [`MAX_RUNS`] (refused before anything runs), a data or engine error, or a cancel before R3.
pub fn run_battery(
    spec: &BatterySpec,
    bars: &[Candle],
    asks: &[Candle],
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
    let Layout {
        per_sweep,
        windows,
        neighbours,
        offsets,
        offset_windows,
        permutations,
        total,
    } = layout(&sweep_spec, &wf_spec, plan, traded.len())?;
    let mut meter = Meter {
        progress,
        total,
        base: 0,
        reported: AtomicUsize::new(0),
    };

    // 1. Integrity: I1 to I3.
    let (open_buckets, dropped_buckets) = bucket_counts(&base, &traded);
    let split = (traded.len() >= 2).then(|| traded[traded.len() / 2].open_time);
    let base_checks = repeat_and_prefix(&base, bars, asks, funding, split, &mut meter)?;
    let mut integrity_inputs = IntegrityInputs {
        dropped_share: (open_buckets > 0).then(|| dropped_buckets as f64 / open_buckets as f64),
        repeat_differences: Some(base_checks.0),
        prefix_differences: base_checks.1,
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
        drawdown_bootstrap: None,
        trade_bootstrap: None,
        prop_firm_odds: None,
        prop_firm_attempt: None,
        breakdown: None,
        cost_stress: None,
        start_offsets: Vec::new(),
        permutation: None,
        luck_note: None,
        luck_cancelled: false,
    };
    if criteria.iter().any(|c| c.outcome == Outcome::Fail) {
        meter.finish();
        result.verdict = Verdict::new(instrument.id, criteria);
        return Ok(result);
    }

    // 2. The sweep over the full period.
    let swept = sweep(&sweep_spec, bars, asks, funding, Some(&meter.step()))?;
    meter.base += per_sweep;

    // 3. R1: the rolling walk-forward, stitched.
    let (wf, wf_error) = if windows == 0 {
        let error = match wf_spec.windows.windows(traded.len()) {
            Err(e) => e.to_string(),
            Ok(_) => "no walk-forward window fits the period".into(),
        };
        (None, Some(error))
    } else {
        match walk_forward(&wf_spec, bars, asks, funding, Some(&meter.step())) {
            Ok(wf) => (Some(wf), None),
            Err(e @ (TesterError::NoQualifyingRun { .. } | TesterError::InvalidSpec(_))) => {
                (None, Some(e.to_string()))
            }
            Err(e) => return Err(e),
        }
    };
    meter.base += windows * per_sweep;

    // 3b. I2 and I3 again on each parameter set the walk-forward chose, since the verdict rests
    // on those and not on the plan's backtest. Each set is checked once; a window that chose a
    // set already checked counts its three runs without making them.
    if let Some(wf) = &wf {
        let mut checked: Vec<(&Params, (usize, Option<usize>))> = vec![(&base.params, base_checks)];
        for w in &wf.windows {
            if checked.iter().any(|(p, _)| **p == w.params) {
                for _ in 0..3 {
                    meter.tick()?;
                }
                continue;
            }
            let config = BacktestConfig {
                params: w.params.clone(),
                ..base.clone()
            };
            let checks = repeat_and_prefix(&config, bars, asks, funding, split, &mut meter)?;
            checked.push((&w.params, checks));
        }
        meter.base += 3 * (windows - wf.windows.len());
        // The worst result over every set, and the set it came from when that is not the plan's
        // backtest (the first, which wins a tie).
        let mut repeat = (base_checks.0, None);
        let mut prefix = (base_checks.1, None);
        for &(params, (r, p)) in &checked[1..] {
            if r > repeat.0 {
                repeat = (r, Some(params));
            }
            if p > prefix.0 {
                prefix = (p, Some(params));
            }
        }
        integrity_inputs.repeat_differences = Some(repeat.0);
        integrity_inputs.prefix_differences = prefix.0;
        result.integrity.inputs = integrity_inputs.clone();
        criteria = evaluate_integrity(&integrity_inputs);
        for (id, from) in [("determinism", repeat.1), ("prefix_consistency", prefix.1)] {
            if let (Some(params), Some(line)) = (from, criteria.iter_mut().find(|c| c.id == id)) {
                line.note = Some(format!(
                    "Failed with the parameters the walk-forward chose: {}.",
                    params_text(params)
                ));
            }
        }
    } else {
        meter.base += 3 * windows;
    }

    // 3c. R5's runs: each window's grid neighbours of the set it chose, on its test bars.
    let plateau = match &wf {
        Some(wf) => {
            let mut runs = Vec::with_capacity(wf.windows.len());
            for w in &wf.windows {
                let found = sweep_spec.neighbours(&w.params)?;
                meter.base += neighbours - found.len();
                let mut window = Vec::with_capacity(found.len());
                for (steps, params) in found {
                    let config = BacktestConfig {
                        params,
                        ..w.out_of_sample.config.clone()
                    };
                    // A set the strategy refuses is left out, as a sweep leaves its row unranked.
                    match run(&config, bars, asks, funding) {
                        Ok(r) => window.push((steps, r)),
                        Err(EngineError::InvalidParam { .. }) => {}
                        Err(e) => return Err(e.into()),
                    }
                    meter.tick()?;
                }
                runs.push(window);
            }
            meter.base += neighbours * (windows - wf.windows.len());
            Some(plateau(wf, &runs, &base, sweep_spec.objective))
        }
        None => {
            meter.base += neighbours * windows;
            None
        }
    };

    // 4. The statistics that need no further runs.
    let mut evidence = EvidenceInputs::default();
    let mut kind = KindInputs::default();
    let mut oos_integrity = OosIntegrityInputs::default();
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
        if let VerdictRule::Portfolio { .. } = rule {
            result.drawdown_bootstrap = stats::bootstrap_drawdown(
                &daily,
                DEFAULT_RESAMPLES,
                stats::default_block_len(daily.len()),
                derive_seed(seed, SEED_DRAWDOWN_BOOTSTRAP),
            )
            .ok();
        }
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
            resampled_drawdown: result.drawdown_bootstrap.as_ref().map(|b| b.p95),
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
            attempt_value: None,
        };
        if let VerdictRule::PropFirm {
            firm,
            limits,
            attempt,
        } = rule
        {
            // A firm counts trading days, not calendar days: the days of `daily` with a fill that
            // passes the firm's test, or with any fill without one.
            let step = plan.timeframe.duration_ms();
            let traded = traded_days(
                &series,
                &wf.fills,
                step,
                daily.len(),
                firm.trading_day.as_ref(),
                cash,
                base.account_currency,
            );
            let spec = PropFirmSpec {
                target: firm.profit_target,
                second_target: firm.second_target,
                min_days: firm.min_trading_days as usize,
                best_day_share: firm.best_day_share,
                day_target_share: firm.day_target_share,
                trailing_lock: firm.trailing_floor_lock,
                daily_loss_limit: firm.daily_loss,
                max_drawdown: firm.max_drawdown,
                drawdown: match firm.drawdown {
                    DrawdownMode::Static => stats::DrawdownMode::Static,
                    DrawdownMode::Trailing => stats::DrawdownMode::Trailing,
                },
                horizon_days: limits.breach_horizon_days as usize,
                paths: DEFAULT_RESAMPLES,
                mean_block: stats::default_block_len(daily.len()),
                seed: derive_seed(seed, SEED_PROP_FIRM),
            };
            // The firm rule sets no time limit of its own (see `FirmLimits`), so the pass odds
            // run to the rule's `pass_horizon_days`. With the two horizons equal, both odds come
            // from the same paths.
            let breach = stats::prop_firm_odds(&daily, &traded, &spec).ok();
            let pass = if limits.pass_horizon_days == limits.breach_horizon_days {
                breach
            } else {
                let spec = PropFirmSpec {
                    horizon_days: limits.pass_horizon_days as usize,
                    ..spec
                };
                stats::prop_firm_odds(&daily, &traded, &spec).ok()
            };
            result.prop_firm_odds = pass.zip(breach).map(|(p, b)| PropFirmOdds {
                pass: p.pass,
                breach: b.breach,
                paths: b.paths,
            });
            kind.pass_probability = result.prop_firm_odds.map(|o| o.pass);
            kind.breach_probability = result.prop_firm_odds.map(|o| o.breach);
            if let Some(terms) = attempt {
                let spec = PropFirmSpec {
                    horizon_days: limits.pass_horizon_days as usize,
                    seed: derive_seed(seed, SEED_PROP_ATTEMPT),
                    ..spec
                };
                result.prop_firm_attempt =
                    stats::prop_firm_attempt(&daily, &traded, &spec, terms).ok();
                kind.attempt_value = result.prop_firm_attempt.map(|a| a.reduced.value);
            }
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
        let stressed_fees = stressed(fees, ev.cost_stress);
        let stressed_asks = widened(bars, asks, ev.cost_stress);
        let mut window_returns = Vec::with_capacity(wf.windows.len());
        let mut level = 1.0f64;
        for w in &wf.windows {
            let config = BacktestConfig {
                fee_override: Some(stressed_fees),
                slippage_bps: w.out_of_sample.config.slippage_bps * ev.cost_stress,
                ..w.out_of_sample.config.clone()
            };
            let r = run(&config, bars, &stressed_asks, funding)?;
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
        let main_sign = evidence.oos_return.map(direction);
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
                match walk_forward(&spec, bars, asks, funding, Some(&meter.step())) {
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
            .filter(|o| main_sign.is_some() && o.oos_return.map(direction) == main_sign)
            .count();

        // 7. R3: the luck test, last.
        let spec = PermutationSpec {
            sweep: sweep_spec.clone(),
            permutations,
            seed: derive_seed(seed, SEED_LUCK),
        };
        match permutation_test(&spec, bars, asks, funding, Some(&meter.step())) {
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
                result.luck_cancelled = true;
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

    let carry = instrument.id.kind.rules().unmodelled_carry;
    let warnings = match (&wf, carry) {
        (Some(wf), Some(carry)) => carry_warning(carry, &wf.trades, plan.timeframe)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    };
    result.verdict = Verdict {
        warnings,
        ..Verdict::new(instrument.id, criteria)
    };
    result.sweep = Some(swept);
    result.walk_forward = wf;
    result.walk_forward_error = wf_error;
    result.evidence = evidence;
    result.kind = kind;
    result.oos_integrity = oos_integrity;
    result.plateau = plateau;
    Ok(result)
}

/// The runs a battery counts, known before it runs, for a caller that reports one progress total
/// over several batteries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryRuns {
    /// The total [`run_battery`] reports progress against.
    pub total: usize,
    /// The count after which the luck test (R3), the last step, runs: the counts above it are
    /// R3's, and a cancel there keeps the verdict with the luck line inconclusive.
    pub luck_from: usize,
}

/// The progress total of [`run_battery`] for `spec` on `bars`, without running anything. Errors
/// as `run_battery` would on a refused spec or a period with no bars.
pub fn battery_runs(spec: &BatterySpec, bars: &[Candle]) -> Result<BatteryRuns, TesterError> {
    let instrument = spec.instrument.clone();
    let base = spec.plan.backtest_config(spec.strategy, instrument.clone());
    let sweep_spec = spec.plan.sweep_spec(spec.strategy, instrument.clone());
    let wf_spec = spec.plan.walk_forward_spec(spec.strategy, instrument);
    wf_spec.check()?;
    let (_, traded) = select_bars(&base, bars)?;
    let l = layout(&sweep_spec, &wf_spec, spec.plan, traded.len())?;
    Ok(BatteryRuns {
        total: l.total,
        luck_from: l.total - l.per_sweep * (l.permutations + 1),
    })
}

/// How a battery's runs are laid out: the counts its progress total is made of.
struct Layout {
    per_sweep: usize,
    windows: usize,
    /// R5's runs counted per window: [`SweepSpec::max_neighbours`].
    neighbours: usize,
    offsets: Vec<usize>,
    offset_windows: Vec<usize>,
    permutations: usize,
    total: usize,
}

fn layout(
    sweep_spec: &SweepSpec,
    wf_spec: &WalkForwardSpec,
    plan: &TestPlan,
    traded: usize,
) -> Result<Layout, TesterError> {
    let ev = &plan.evidence;
    let per_sweep = sweep_spec.runs()?;
    let neighbours = sweep_spec.max_neighbours()?;
    let window_count = |bars: usize| wf_spec.windows.windows(bars).map_or(0, |w| w.len());
    let windows = window_count(traded);
    let offsets = start_offset_shifts(wf_spec.windows.test_bars, ev.start_offsets as usize);
    let offset_windows: Vec<usize> = offsets
        .iter()
        .map(|&s| window_count(traded.saturating_sub(s)))
        .collect();
    let permutations = ev.luck.permutations as usize;
    // R3 runs last but is refused up front, before the hours of runs ahead of it.
    let luck_runs = per_sweep as u128 * (permutations as u128 + 1);
    // So are R1's train sweeps with R5's neighbour runs, before the integrity runs and the sweep.
    let wf_runs = windows as u128 * (per_sweep as u128 + neighbours as u128);
    for runs in [luck_runs, wf_runs] {
        if runs > MAX_RUNS as u128 {
            return Err(TesterError::TooManyRuns {
                runs,
                limit: MAX_RUNS,
            });
        }
    }
    let total = 3
        + per_sweep
        + windows * per_sweep
        + 3 * windows
        + windows * neighbours
        + windows
        + offset_windows.iter().sum::<usize>() * per_sweep
        + per_sweep * (permutations + 1);
    Ok(Layout {
        per_sweep,
        windows,
        neighbours,
        offsets,
        offset_windows,
        permutations,
        total,
    })
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

/// R4's asks on a market with quotes: each ask moved away from its bar so that the spread at each
/// of its four prices is `multiplier` times the bar's own, then its high and low widened to hold
/// its open and close, since the spread at the high or low can be narrower than at the open or
/// close. With `multiplier` at least 1 the ask stays at or above its bar, so the stressed quotes
/// pass the same checks as the real ones. Empty when `asks` is: a market without quotes pays no
/// spread.
fn widened(bars: &[Candle], asks: &[Candle], multiplier: f64) -> Vec<Candle> {
    let bids: HashMap<Millis, &Candle> = bars.iter().map(|b| (b.open_time, b)).collect();
    let widen = |bid: f64, ask: f64| bid + multiplier * (ask - bid);
    asks.iter()
        .map(|a| match bids.get(&a.open_time) {
            Some(b) => {
                let (open, close) = (widen(b.open, a.open), widen(b.close, a.close));
                Candle {
                    open,
                    high: widen(b.high, a.high).max(open).max(close),
                    low: widen(b.low, a.low).min(open).min(close),
                    close,
                    ..*a
                }
            }
            // An ask with no bar is never read: the run looks asks up by their bar.
            None => *a,
        })
        .collect()
}

/// The warning for a market whose rules name a financing cost the engine does not model
/// (`MarketRules::unmodelled_carry`), when any of `trades` held its position through a point
/// where that cost is charged. A trade's times are the opens of the bars it opened and closed in,
/// on `timeframe`, so a trade counts as held through a rollover that falls after its entry bar's
/// open and before its exit bar's end: a stop can fill anywhere inside the exit bar, and its time
/// does not say whether that was before the rollover or after it.
fn carry_warning(carry: Carry, trades: &[Trade], timeframe: Timeframe) -> Option<String> {
    let held = match carry {
        Carry::DailyRollover => trades
            .iter()
            .filter(|t| {
                DayRollover::NewYork1700.next_after(t.entry_time)
                    < t.exit_time + timeframe.duration_ms()
            })
            .count(),
    };
    (held > 0).then(|| {
        format!(
            "{held} of {} out-of-sample trades held a position through {}, which the engine does \
             not charge yet, so their results leave that cost out.",
            trades.len(),
            carry.name()
        )
    })
}

/// R4's fees: every fee of `fees`, the per-unit commission too, times `multiplier`.
fn stressed(fees: FeeSchedule, multiplier: f64) -> FeeSchedule {
    FeeSchedule {
        maker: fees.maker * multiplier,
        taker: fees.taker * multiplier,
        per_unit: fees.per_unit * multiplier,
    }
}

/// I2 and I3 on one backtest, in three counted runs: the backtest twice, then once on the bars
/// before `split` alone. Returns I2's and I3's differences; I3's is `None` without a split.
fn repeat_and_prefix(
    config: &BacktestConfig,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
    split: Option<Millis>,
    meter: &mut Meter,
) -> Result<(usize, Option<usize>), TesterError> {
    let first = run(config, bars, asks, funding)?;
    meter.tick()?;
    let again = run(config, bars, asks, funding)?;
    meter.tick()?;
    let prefix = match split {
        Some(at) => {
            let half = run(
                &BacktestConfig {
                    to: at,
                    ..config.clone()
                },
                bars,
                asks,
                funding,
            )?;
            Some(prefix_differences(&first.orders, &half.orders, at))
        }
        None => None,
    };
    meter.tick()?;
    Ok((repeat_differences(&first, &again), prefix))
}

/// A parameter set as `name = value` pairs, for a note.
fn params_text(params: &Params) -> String {
    match serde_json::to_value(params) {
        Ok(serde_json::Value::Object(map)) => map
            .iter()
            .map(|(name, value)| format!("{name} = {value}"))
            .collect::<Vec<_>>()
            .join(", "),
        _ => format!("{params:?}"),
    }
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

/// R5 from the walk-forward and `runs`, each window's neighbour runs with their steps (see
/// [`PlateauReport`]).
fn plateau(
    wf: &WalkForwardResult,
    runs: &[Vec<(Vec<i8>, BacktestResult)>],
    base: &BacktestConfig,
    objective: Objective,
) -> PlateauReport {
    let steps: BTreeSet<&Vec<i8>> = runs.iter().flatten().map(|(s, _)| s).collect();
    let directions: Vec<PlateauDirection> = steps
        .into_iter()
        .map(|steps| {
            // Only the windows where this direction ran, with the chosen set's run beside each.
            let (own, moved): (Vec<&BacktestResult>, Vec<&BacktestResult>) = wf
                .windows
                .iter()
                .zip(runs)
                .filter_map(|(w, found)| {
                    let (_, r) = found.iter().find(|(s, _)| s == steps)?;
                    Some((&w.out_of_sample, r))
                })
                .unzip();
            let h = stitched_headline(base, &moved);
            PlateauDirection {
                steps: steps.clone(),
                windows: moved.len(),
                objective: h.value(objective.metric),
                total_return: h.total_return,
                reference: stitched_headline(base, &own).value(objective.metric),
            }
        })
        .collect();
    let best_objective = Headline::new(&wf.metrics).value(objective.metric);
    let (median_ratio, positive_share) = plateau_numbers(&directions, objective.direction);
    PlateauReport {
        best_objective,
        directions,
        median_ratio,
        positive_share,
    }
}

/// R5's ratio and share from the directions, each with its reference (see [`PlateauReport`]).
/// Both are `None` without directions, and the ratio also when no direction has one.
fn plateau_numbers(
    directions: &[PlateauDirection],
    direction: Direction,
) -> (Option<f64>, Option<f64>) {
    let positive_share = (!directions.is_empty()).then(|| {
        directions
            .iter()
            .filter(|d| d.total_return.is_some_and(|x| x > 0.0))
            .count() as f64
            / directions.len() as f64
    });
    let ratio = |d: &PlateauDirection| match (direction, d.reference, d.objective) {
        (Direction::Maximize, Some(r), Some(o)) if r > 0.0 => Some(o / r),
        (Direction::Maximize, Some(r), None) if r > 0.0 => Some(f64::NEG_INFINITY),
        (Direction::Maximize, _, _) => None,
        (Direction::Minimize, Some(r), Some(o)) if o > 0.0 => Some(r / o),
        // Neither drew down: as good as each other. Only the neighbour drew down nothing: better.
        (Direction::Minimize, Some(r), Some(_)) => Some(if r == 0.0 { 1.0 } else { f64::INFINITY }),
        (Direction::Minimize, Some(_), None) => Some(0.0),
        (Direction::Minimize, None, _) => None,
    };
    let values: Vec<f64> = directions.iter().filter_map(ratio).collect();
    let median_ratio = metrics::median(&values);
    (median_ratio, positive_share)
}

/// The returns from one UTC day's end to the next of an equity series. A day that starts with no
/// equity left has a return of zero: the account had nothing to lose.
fn daily_returns(series: &EquitySeries) -> Vec<f64> {
    metrics::daily_equity(&series.times, &series.values)
        .windows(2)
        .map(|w| if w[0] > 0.0 { w[1] / w[0] - 1.0 } else { 0.0 })
        .collect()
}

/// Whether the strategy traded on each of the `days` days of [`daily_returns`] of `series`: true
/// on a day that holds a fill. A fill is stamped with the open of its bar, of `step`
/// milliseconds, and shows in the equity at the bar's close, so it is put on the day that close
/// falls on, by [`metrics::daily_equity`]'s boundaries (UTC days, a close at exactly 00:00 ending
/// the day before): the day whose return it moved.
///
/// With a firm's `test`, only a fill that passes it marks its day ([`TradingDayTest`]). The
/// fills are replayed per instrument with [`next_position`], the portfolio's rule, to know what
/// each one closes and at what average entry; `cash`, the starting balance, and `account`, the
/// account currency, turn a closed trade's value into a share of the balance.
///
/// shortcut: the smallest trade size is judged at the size the strategy was tested at, not at the
/// attempt's `evaluation_size` or `funded_size`, since one set of flags serves both; build flags
/// per size once a plan trades an attempt at a size that moves trades across the firm's smallest.
fn traded_days(
    series: &EquitySeries,
    fills: &[Fill],
    step: Millis,
    days: usize,
    test: Option<&TradingDayTest>,
    cash: f64,
    account: AccountCurrency,
) -> Vec<bool> {
    let mut traded = vec![false; days];
    let Some(first) = series.times.first() else {
        return traded;
    };
    let first_day = first.div_euclid(DAY_MS);
    // Each instrument's signed position and average entry price.
    let mut positions: HashMap<&InstrumentId, (f64, f64)> = HashMap::new();
    for f in fills {
        let counts = test.is_none_or(|test| {
            let (pos, avg) = positions.entry(&f.instrument).or_insert((0.0, 0.0));
            let dq = f.side.sign() * f.qty;
            // A fill against the position closes up to all of it; any rest opens a new one.
            let close = (*pos != 0.0 && dq.signum() != pos.signum()).then(|| {
                let closed = f.qty.min(pos.abs());
                let value = account.from_quote(closed * *avg, *avg) / cash;
                (value, (f.price - *avg).abs() / *avg)
            });
            (*pos, *avg) = next_position(*pos, *avg, dq, f.price);
            test.passes(close)
        });
        if !counts {
            continue;
        }
        let day = (f.time + step - 1).div_euclid(DAY_MS) - first_day;
        if let Some(t) = usize::try_from(day).ok().and_then(|d| traded.get_mut(d)) {
            *t = true;
        }
    }
    traded
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
                per_unit: 0.0,
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
            account_currency: td_core::AccountCurrency::Quote,
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
        let a = run(&config, &bars, &[], &[]).unwrap();
        assert!(!a.fills.is_empty());
        assert_eq!(repeat_differences(&a, &a.clone()), 0);

        let mut equity = a.clone();
        equity.equity.last_mut().unwrap().equity += 1.0;
        assert_eq!(repeat_differences(&a, &equity), 1);

        let mut fills = a.clone();
        fills.fills[0].price += 0.01;
        assert_eq!(repeat_differences(&a, &fills), 1);
    }

    /// A strategy that never trades with its defaults passes I2 and I3 on the plan's backtest
    /// vacuously. The walk-forward chooses its one swept value, which trades and fills
    /// differently on every run, and the checks on that set make the verdict `invalid`. Every
    /// window chooses the same set, so it is checked once and the count still reaches the total
    /// one run at a time.
    #[test]
    fn i2_and_i3_also_check_the_parameters_the_walk_forward_chose() {
        let entry = crate::find_strategy("test_unrepeatable").unwrap();
        let plan = entry.plan;
        let step = plan.timeframe.duration_ms();
        let count = ((plan.period.to - plan.period.from) / step) as usize;
        let bars: Vec<Candle> = (0..count)
            .map(|i| {
                let close = 100.0 + 10.0 * ((i as f64) / 15.0).sin();
                Candle {
                    open_time: plan.period.from + i as Millis * step,
                    open: close,
                    high: close * 1.001,
                    low: close * 0.999,
                    close,
                    volume: 1.0,
                    quote_volume: close,
                }
            })
            .collect();
        let instrument = test_instrument();
        let spec = BatterySpec {
            strategy: entry.name,
            plan: &plan,
            rule: &entry.rule,
            instrument: &instrument,
            seed: 7,
        };
        let calls = std::sync::Mutex::new(Vec::new());
        let progress = |done, total| {
            calls.lock().unwrap().push((done, total));
            ControlFlow::Continue(())
        };
        let r = run_battery(&spec, &bars, &[], &[], Some(&progress)).unwrap();

        let wf = r.walk_forward.as_ref().unwrap();
        assert_eq!(wf.windows.len(), 4);
        let chosen = Params::new().with("trade", crate::ParamValue::Bool(true));
        assert!(wf.windows.iter().all(|w| w.params == chosen));
        assert_eq!(r.verdict.state, crate::VerdictState::Invalid);
        let line = |id: &str| r.verdict.criteria.iter().find(|c| c.id == id).unwrap();
        for id in ["determinism", "prefix_consistency"] {
            assert_eq!(line(id).outcome, Outcome::Fail, "{id}");
            assert_eq!(
                line(id).note.as_deref(),
                Some("Failed with the parameters the walk-forward chose: trade = true.")
            );
        }
        assert_eq!(line("dropped_buckets").outcome, Outcome::Pass);
        assert_eq!(r.integrity.inputs.repeat_differences, Some(1));
        assert!(r.integrity.inputs.prefix_differences > Some(0));

        // 3 integrity runs, 1 for the sweep, 4 train sweeps of 1 and 3 per window are counted
        // one at a time; the later steps, which the failed lines leave nothing to decide, are
        // skipped and the count jumps to the total.
        let calls = calls.into_inner().unwrap();
        assert!(calls.iter().all(|&(_, total)| total == r.runs));
        let mut done: Vec<usize> = calls.iter().map(|&(d, _)| d).collect();
        done.sort_unstable();
        assert_eq!(done, (1..=20).chain([r.runs]).collect::<Vec<_>>());
    }

    /// One of R5's neighbours, with its objective (Sharpe or max drawdown, as the test reads it),
    /// the chosen sets' objective over the same windows and its total return.
    fn direction(
        steps: [i8; 2],
        objective: Option<f64>,
        reference: Option<f64>,
        total_return: f64,
    ) -> PlateauDirection {
        PlateauDirection {
            steps: steps.to_vec(),
            windows: 1,
            objective,
            total_return: Some(total_return),
            reference,
        }
    }

    #[test]
    fn the_plateau_reads_each_neighbour_against_its_own_windows() {
        use Direction::{Maximize, Minimize};
        // (1, 1)'s objective is undefined and has the worst ratio.
        let directions = |second: Option<f64>, reference: Option<f64>| {
            vec![
                direction([0, 1], Some(1.5), reference, 0.2),
                direction([1, 0], second, reference, 0.1),
                direction([1, 1], None, reference, -0.05),
            ]
        };
        // Ratios [-inf, 0.5, 0.75] against a reference of 2.0: the median is 0.5. Two of three
        // neighbours made money.
        assert_eq!(
            plateau_numbers(&directions(Some(1.0), Some(2.0)), Maximize),
            (Some(0.5), Some(2.0 / 3.0))
        );
        // Each against its own reference: 1.5 / 1.5 and 1.0 / 4.0, and the undefined one.
        let own = [
            direction([0, 1], Some(1.5), Some(1.5), 0.2),
            direction([1, 0], Some(1.0), Some(4.0), 0.1),
            direction([1, 1], None, Some(2.0), -0.05),
        ];
        assert_eq!(plateau_numbers(&own, Maximize).0, Some(0.25));
        // With a reference at or below zero, or undefined, a ratio would read backwards or not at
        // all, so the direction has none; the share still counts it.
        for reference in [Some(0.0), Some(-0.5), None] {
            assert_eq!(
                plateau_numbers(&directions(Some(1.0), reference), Maximize),
                (None, Some(2.0 / 3.0)),
                "reference {reference:?}"
            );
        }
        let mixed = [
            own[0].clone(),
            direction([1, 0], Some(-1.0), Some(-0.5), -0.1),
        ];
        assert_eq!(plateau_numbers(&mixed, Maximize), (Some(1.0), Some(0.5)));
        // Two undefined neighbours of three put the median at the worst ratio: no finite ratio.
        assert_eq!(
            plateau_numbers(&directions(None, Some(2.0)), Maximize).0,
            None
        );
        // Minimize, on max drawdown: the ratio is the reference over the objective, so below 1 is
        // worse here too, and an undefined neighbour has a ratio of 0. Sorted [0, 1/3, 1/2]: the
        // median is 0.1 / 0.3.
        let min = vec![
            direction([0, 1], Some(0.2), Some(0.1), 0.1),
            direction([1, 1], Some(0.3), Some(0.1), 0.2),
            direction([1, 0], None, Some(0.1), -0.05),
        ];
        assert_eq!(
            plateau_numbers(&min, Minimize),
            (Some(0.1 / 0.3), Some(2.0 / 3.0))
        );
        // No neighbours: nothing to judge.
        assert_eq!(plateau_numbers(&[], Maximize), (None, None));
    }

    #[test]
    fn the_cost_stress_widens_each_spread_and_keeps_the_ask_valid() {
        let bid = Candle {
            open_time: 7,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
            volume: 1.0,
            quote_volume: 100.0,
        };
        // Spreads of 0.25 at the open and close, 0 at the high and 0.5 at the low.
        let ask = Candle {
            open: 100.25,
            high: 101.0,
            low: 99.5,
            close: 100.75,
            ..bid
        };
        // Three times each spread; the high, whose spread was 0, is raised to the new close.
        let w = widened(&[bid], &[ask], 3.0);
        assert_eq!(
            w,
            vec![Candle {
                open: 100.75,
                high: 101.25,
                low: 100.5,
                close: 101.25,
                ..ask
            }]
        );
        assert!(w[0].low >= bid.low && w[0].high >= bid.high);
        // At 1 the asks are the real ones, and a market without quotes has none to widen.
        assert_eq!(widened(&[bid], &[ask], 1.0), vec![ask]);
        assert!(widened(&[bid], &[], 3.0).is_empty());
    }

    #[test]
    fn a_position_held_through_the_rollover_warns_of_the_unmodelled_swap() {
        // Keyed on the rules, not the kind: a market whose carry is not modelled has one named.
        let carry = InstrumentKind::Fx.rules().unmodelled_carry.unwrap();
        // 2024-01-10 is in US standard time, so the rollover is at 22:00 UTC.
        let day = td_core::days_from_civil(2024, 1, 10) * DAY_MS;
        let held = |entry_hour: Millis, exit_hour: Millis| Trade {
            entry_time: day + entry_hour * HOUR,
            exit_time: day + exit_hour * HOUR,
            ..trade(0, 0.0, false)
        };
        let h1 = td_core::Timeframe::H1;
        // In at 01:00 and out in the bar that opens at 21:00 and ends at the rollover: intraday,
        // no warning.
        assert_eq!(carry_warning(carry, &[held(1, 21)], h1), None);
        // In at 20:00 and out at 01:00 the next day: held through the rollover.
        let warning = carry_warning(carry, &[held(1, 21), held(20, 25)], h1).unwrap();
        assert_eq!(
            warning,
            "1 of 2 out-of-sample trades held a position through the overnight swap at the daily \
             rollover (17:00 New York), which the engine does not charge yet, so their results \
             leave that cost out."
        );
        // Closed in the bar that opens at the rollover: counted, as it may have filled after it.
        assert!(carry_warning(carry, &[held(20, 22)], h1).is_some());
        // On four-hour bars, in at 12:00 and stopped out inside the bar that opens at 20:00: that
        // bar holds the 22:00 rollover, and the stop may have filled after it.
        let h4 = td_core::Timeframe::H4;
        assert!(carry_warning(carry, &[held(12, 20)], h4).is_some());
        assert_eq!(carry_warning(carry, &[held(12, 16)], h4), None);
        // Perpetual funding is modelled, so a perpetual has no carry to warn of.
        assert_eq!(InstrumentKind::Perpetual.rules().unmodelled_carry, None);
        assert_eq!(InstrumentKind::Spot.rules().unmodelled_carry, None);
    }

    #[test]
    fn the_cost_stress_multiplies_every_fee_the_per_unit_commission_too() {
        let fees = FeeSchedule {
            maker: 0.001,
            taker: 0.002,
            per_unit: 0.000_035,
        };
        assert_eq!(
            stressed(fees, 2.0),
            FeeSchedule {
                maker: 0.002,
                taker: 0.004,
                per_unit: 0.000_07,
            }
        );
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
            net_pips: None,
            pips: None,
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

    fn ints(v: &[i64]) -> crate::ParamSpace {
        crate::ParamSpace::Values {
            values: v.iter().map(|&x| crate::ParamValue::Int(x)).collect(),
        }
    }

    /// sma_cross's plan on 250 days of four-hour bars of a triangle wave, as the battery's
    /// integration tests use, swept over `axes`: four 45-day test windows after a 60-day train
    /// window.
    fn sma_plan(axes: Vec<crate::ParamAxis>) -> (TestPlan, Vec<Candle>) {
        let step = 4 * HOUR;
        let count = 250 * 6;
        let mut plan = crate::find_strategy("sma_cross").unwrap().plan;
        plan.instruments = vec![test_instrument().id];
        plan.timeframe = td_core::Timeframe::H4;
        plan.period = crate::Period {
            from: 0,
            to: count as Millis * step,
        };
        plan.walk_forward = crate::WalkForwardPlan {
            train_days: 60,
            test_days: 45,
        };
        plan.out_of_sample = None;
        plan.sweep.axes = axes;
        plan.sweep.min_trades = 3;
        plan.evidence.luck.permutations = 19;
        let bars = (0..count)
            .map(|i| {
                let x = i as f64 / 60.0;
                let close = 100.0 + 10.0 * (4.0 * (x - x.floor() - 0.5).abs() - 1.0);
                Candle {
                    open_time: i as Millis * step,
                    open: close,
                    high: close * 1.001,
                    low: close * 0.999,
                    close,
                    volume: 1.0,
                    quote_volume: close,
                }
            })
            .collect();
        (plan, bars)
    }

    /// The 3 x 3 grid of fast and slow periods.
    fn sma_axes() -> Vec<crate::ParamAxis> {
        vec![
            crate::ParamAxis {
                name: "fast".into(),
                space: ints(&[2, 3, 4]),
            },
            crate::ParamAxis {
                name: "slow".into(),
                space: ints(&[8, 12, 16]),
            },
        ]
    }

    fn sma_battery(plan: &TestPlan, bars: &[Candle], seed: u64) -> BatteryResult {
        let rule = VerdictRule::Portfolio {
            benchmark: PortfolioBenchmark::Cash,
            limits: crate::PortfolioLimits::default(),
        };
        let spec = BatterySpec {
            strategy: "sma_cross",
            plan,
            rule: &rule,
            instrument: &test_instrument(),
            seed,
        };
        run_battery(&spec, bars, &[], &[], None).unwrap()
    }

    /// The drawdown bootstrap draws from its own seed stream, not R7's: swapped, its paths would
    /// repeat R7's draws exactly.
    #[test]
    fn the_drawdown_bootstrap_draws_from_its_own_seed_stream() {
        let (plan, bars) = sma_plan(sma_axes());
        let seed = 7;
        let r = sma_battery(&plan, &bars, seed);

        let wf = r.walk_forward.as_ref().unwrap();
        let cash = plan.backtest.starting_cash;
        let daily = daily_returns(&EquitySeries::new(cash, &wf.equity, plan.timeframe));
        let drawn = |stream| {
            stats::bootstrap_drawdown(
                &daily,
                DEFAULT_RESAMPLES,
                stats::default_block_len(daily.len()),
                derive_seed(seed, stream),
            )
            .ok()
        };
        assert!(r.drawdown_bootstrap.is_some());
        assert_eq!(r.drawdown_bootstrap, drawn(SEED_DRAWDOWN_BOOTSTRAP));
        assert_ne!(r.drawdown_bootstrap, drawn(SEED_DAILY_BOOTSTRAP));
    }

    /// A fill of `qty` at `price` at 10:00 UTC on day `day` of [`flagged_days`]' series.
    fn day_fill(day: i64, side: Side, qty: f64, price: f64) -> Fill {
        Fill {
            order_id: td_core::OrderId(0),
            instrument: test_instrument().id,
            side,
            time: day * DAY_MS + 10 * 3_600_000,
            price,
            qty,
            fee: 0.0,
            liquidity: td_core::Liquidity::Taker,
        }
    }

    /// The days, counted from 1, that `traded_days` marks for `fills` on hourly bars, over five
    /// days that start at day 1's midnight, on a quote account of 10,000.
    fn flagged_days(fills: &[Fill], test: Option<TradingDayTest>) -> Vec<usize> {
        let series = EquitySeries {
            times: vec![DAY_MS],
            values: vec![10_000.0],
        };
        let traded = traded_days(
            &series,
            fills,
            3_600_000,
            5,
            test.as_ref(),
            10_000.0,
            td_core::AccountCurrency::Quote,
        );
        (1..=5).filter(|&d| traded[d - 1]).collect()
    }

    fn day_test(
        needs_close: bool,
        size: Option<f64>,
        moved: Option<f64>,
    ) -> Option<TradingDayTest> {
        Some(TradingDayTest {
            needs_close,
            min_trade_size: size,
            min_trade_move: moved,
        })
    }

    const NO_DAYS: [usize; 0] = [];

    #[test]
    fn a_firms_trading_day_test_counts_only_the_closing_fill_of_a_large_enough_trade() {
        // Buy 1 at 1,000 on day 1 and sell it at 1,010 on day 3: a trade worth 1,000, 10% of the
        // balance, that moved 1%.
        let fills = [
            day_fill(1, Side::Buy, 1.0, 1_000.0),
            day_fill(3, Side::Sell, 1.0, 1_010.0),
        ];
        assert_eq!(flagged_days(&fills, None), [1, 3]);
        // A test with nothing set counts any fill, as no test does.
        assert_eq!(flagged_days(&fills, day_test(false, None, None)), [1, 3]);
        assert_eq!(flagged_days(&fills, day_test(true, None, None)), [3]);
        // A smallest size alone also needs the close; 10% meets 0.1 and misses 0.2.
        assert_eq!(flagged_days(&fills, day_test(false, Some(0.1), None)), [3]);
        assert_eq!(
            flagged_days(&fills, day_test(false, Some(0.2), None)),
            NO_DAYS
        );
    }

    #[test]
    fn a_close_that_moves_less_than_the_smallest_move_is_not_a_trading_day() {
        let at = |price| {
            [
                day_fill(1, Side::Buy, 1.0, 1_000.0),
                day_fill(3, Side::Sell, 1.0, price),
            ]
        };
        let test = day_test(false, None, Some(0.02));
        // A 1% gain misses a 2% smallest move; a 3% loss meets it, as a loss counts by its size.
        assert_eq!(flagged_days(&at(1_010.0), test), NO_DAYS);
        assert_eq!(flagged_days(&at(970.0), test), [3]);
        assert_eq!(
            flagged_days(&at(1_010.0), day_test(false, None, Some(0.005))),
            [3]
        );
    }

    #[test]
    fn a_partial_close_counts_by_the_part_closed_and_each_close_on_its_own() {
        // Buy 2 at 1,000, then close 0.5 on day 2 (worth 5% of the balance) and 1.5 on day 3
        // (15%). Against a 10% smallest size only day 3 counts; the whole position, 20%, would
        // have passed on day 2.
        let fills = [
            day_fill(1, Side::Buy, 2.0, 1_000.0),
            day_fill(2, Side::Sell, 0.5, 1_010.0),
            day_fill(3, Side::Sell, 1.5, 1_010.0),
        ];
        assert_eq!(flagged_days(&fills, day_test(false, Some(0.1), None)), [3]);
        assert_eq!(
            flagged_days(&fills, day_test(false, Some(0.04), None)),
            [2, 3]
        );
        // Two closes of 5% on one day are two small closes, not one of 10%.
        let split = [
            day_fill(1, Side::Buy, 1.0, 1_000.0),
            day_fill(2, Side::Sell, 0.5, 1_010.0),
            day_fill(2, Side::Sell, 0.5, 1_010.0),
        ];
        assert_eq!(
            flagged_days(&split, day_test(false, Some(0.08), None)),
            NO_DAYS
        );
    }

    #[test]
    fn a_fill_that_flips_the_position_closes_the_old_one_and_opens_a_new_one() {
        // Long 1 at 1,000; on day 2 sell 3 at 1,010, which closes the long (worth 10%, moved 1%)
        // and opens a short of 2 at 1,010; on day 3 buy 2 at 990, which closes the short (worth
        // 20.2%, moved 20/1,010 = 1.98%).
        let fills = [
            day_fill(1, Side::Buy, 1.0, 1_000.0),
            day_fill(2, Side::Sell, 3.0, 1_010.0),
            day_fill(3, Side::Buy, 2.0, 990.0),
        ];
        assert_eq!(flagged_days(&fills, day_test(true, None, None)), [2, 3]);
        // The flip closes 1, not 3: under a 15% smallest size only day 3 counts.
        assert_eq!(flagged_days(&fills, day_test(false, Some(0.15), None)), [3]);
        // The short was entered at the flip's price: its 1.98% meets 1.95%, which day 2's 1%
        // misses. Measured from the long's 1,000 it would have moved 1%.
        assert_eq!(
            flagged_days(&fills, day_test(false, None, Some(0.0195))),
            [3]
        );
    }

    /// A prop firm's minimum counts only the days on which the walk-forward filled an order: the
    /// battery flags the days of its fills, on the daily returns' own days, and runs the odds on
    /// those flags rather than counting every day.
    #[test]
    fn only_days_with_a_fill_count_toward_a_firms_minimum() {
        let (plan, bars) = sma_plan(sma_axes());
        let seed = 7;
        let firm = crate::FirmLimits {
            daily_loss: 0.05,
            max_drawdown: 0.10,
            drawdown: DrawdownMode::Static,
            profit_target: 0.02,
            second_target: None,
            min_trading_days: 20,
            best_day_share: None,
            day_target_share: None,
            trading_day: None,
            trailing_floor_lock: None,
        };
        let rule = VerdictRule::PropFirm {
            firm,
            limits: crate::PropFirmLimits::default(),
            attempt: None,
        };
        let spec = BatterySpec {
            strategy: "sma_cross",
            plan: &plan,
            rule: &rule,
            instrument: &test_instrument(),
            seed,
        };
        let r = run_battery(&spec, &bars, &[], &[], None).unwrap();

        let wf = r.walk_forward.as_ref().unwrap();
        let cash = plan.backtest.starting_cash;
        let series = EquitySeries::new(cash, &wf.equity, plan.timeframe);
        let daily = daily_returns(&series);
        let step = plan.timeframe.duration_ms();
        let traded = traded_days(
            &series,
            &wf.fills,
            step,
            daily.len(),
            None,
            cash,
            td_core::AccountCurrency::Quote,
        );
        // A four-hour bar lies inside one UTC day, so a fill's day is the day its bar opened.
        let first = series.times[0].div_euclid(DAY_MS);
        let filled: BTreeSet<usize> = wf
            .fills
            .iter()
            .map(|f| (f.time.div_euclid(DAY_MS) - first) as usize)
            .collect();
        let flagged: BTreeSet<usize> = (0..daily.len()).filter(|&d| traded[d]).collect();
        assert_eq!(flagged, filled);
        assert!(
            !filled.is_empty() && filled.len() < daily.len(),
            "{} days with a fill of {}",
            filled.len(),
            daily.len()
        );

        // The two horizons are equal, so one run of the odds gives both.
        let odds = |traded: &[bool]| {
            let spec = PropFirmSpec {
                target: firm.profit_target,
                second_target: None,
                min_days: firm.min_trading_days as usize,
                best_day_share: None,
                day_target_share: None,
                trailing_lock: None,
                daily_loss_limit: firm.daily_loss,
                max_drawdown: firm.max_drawdown,
                drawdown: stats::DrawdownMode::Static,
                horizon_days: 90,
                paths: DEFAULT_RESAMPLES,
                mean_block: stats::default_block_len(daily.len()),
                seed: derive_seed(seed, SEED_PROP_FIRM),
            };
            stats::prop_firm_odds(&daily, traded, &spec).unwrap()
        };
        let fills = odds(&traded);
        let every_day = odds(&vec![true; daily.len()]);
        assert_eq!(r.prop_firm_odds, Some(fills));
        assert!(
            fills.pass < every_day.pass,
            "{fills:?} against {every_day:?}"
        );
    }

    /// A decimal parameter swept over whole numbers: the chosen sets hold `equity_fraction` as the
    /// decimal 1.0, not the listed 1, and R5 still finds them on the grid and runs their
    /// neighbours.
    #[test]
    fn r5_runs_on_a_decimal_parameter_swept_over_whole_numbers() {
        let mut axes = sma_axes();
        axes.push(crate::ParamAxis {
            name: "equity_fraction".into(),
            space: ints(&[0, 1]),
        });
        let (plan, bars) = sma_plan(axes);
        let r = sma_battery(&plan, &bars, 7);

        let wf = r.walk_forward.as_ref().unwrap();
        for w in &wf.windows {
            assert_eq!(
                w.params.get("equity_fraction"),
                Some(crate::ParamValue::Float(1.0))
            );
        }
        let plateau = r.plateau.as_ref().unwrap();
        assert!(!plateau.directions.is_empty());
        assert!(plateau.directions.iter().all(|d| d.windows > 0));
    }

    /// Each of R5's neighbours runs on its window's test bars with the window's own test config:
    /// every direction's numbers are its runs stitched by hand from those configs, over the
    /// windows where it ran, against the chosen sets' runs in the same windows.
    #[test]
    fn r5_runs_each_neighbour_on_its_windows_test_config() {
        let (plan, bars) = sma_plan(sma_axes());
        let r = sma_battery(&plan, &bars, 7);

        let wf = r.walk_forward.as_ref().unwrap();
        let plateau = r.plateau.as_ref().unwrap();
        let base = plan.backtest_config("sma_cross", test_instrument());
        let metric = plan.sweep.objective.metric;
        assert!(!plateau.directions.is_empty());
        for d in &plateau.directions {
            let (mut moved, mut own) = (Vec::new(), Vec::new());
            for w in &wf.windows {
                let found = wf.spec.sweep.neighbours(&w.params).unwrap();
                let Some((_, params)) = found.into_iter().find(|(s, _)| *s == d.steps) else {
                    continue;
                };
                let config = BacktestConfig {
                    params,
                    ..w.out_of_sample.config.clone()
                };
                moved.push(run(&config, &bars, &[], &[]).unwrap());
                own.push(&w.out_of_sample);
            }
            let h = stitched_headline(&base, &moved.iter().collect::<Vec<_>>());
            assert_eq!(d.windows, moved.len(), "{d:?}");
            assert_eq!(d.objective, h.value(metric), "{d:?}");
            assert_eq!(d.total_return, h.total_return, "{d:?}");
            assert_eq!(d.reference, stitched_headline(&base, &own).value(metric));
        }
    }

    /// A cliff on one side of the chosen sets fails R5. One direction ran only in the even
    /// windows, exactly as the chosen set did there; the other only in the odd windows, where it
    /// lost to fees of 20% a fill. Each is judged on its own windows alone: filling the other
    /// windows with the chosen set's run would hide most of the cliff.
    #[test]
    fn a_cliff_on_one_side_fails_the_plateau() {
        let (plan, bars) = sma_plan(sma_axes());
        let r = sma_battery(&plan, &bars, 7);
        let wf = r.walk_forward.as_ref().unwrap();
        assert_eq!(wf.windows.len(), 4);

        let costly = |w: &crate::WindowResult| {
            let config = BacktestConfig {
                fee_override: Some(FeeSchedule {
                    maker: 0.2,
                    taker: 0.2,
                    per_unit: 0.0,
                }),
                ..w.out_of_sample.config.clone()
            };
            run(&config, &bars, &[], &[]).unwrap()
        };
        let runs: Vec<Vec<(Vec<i8>, BacktestResult)>> = wf
            .windows
            .iter()
            .enumerate()
            .map(|(k, w)| match k % 2 {
                0 => vec![(vec![-1, 0], w.out_of_sample.clone())],
                _ => vec![(vec![1, 0], costly(w))],
            })
            .collect();
        let base = plan.backtest_config("sma_cross", test_instrument());
        let p = plateau(wf, &runs, &base, plan.sweep.objective);

        let [flat, cliff] = p.directions.as_slice() else {
            panic!("{:?}", p.directions)
        };
        assert_eq!(
            (flat.steps.as_slice(), flat.windows),
            ([-1, 0].as_slice(), 2)
        );
        assert_eq!(flat.objective, flat.reference);
        let odd: Vec<&BacktestResult> = runs.iter().skip(1).step_by(2).map(|w| &w[0].1).collect();
        let h = stitched_headline(&base, &odd);
        assert_eq!((cliff.windows, cliff.total_return), (2, h.total_return));
        assert!(h.total_return.unwrap() < 0.0, "{h:?}");
        let settings = &plan.evidence.plateau;
        assert!(
            p.positive_share.unwrap() < settings.min_positive_share,
            "{p:?}"
        );
        assert!(p.median_ratio.unwrap() < settings.min_median_ratio, "{p:?}");
    }

    /// A plateau saved before R5 moved out of sample, with its ratio and share, no directions and
    /// no warnings on its verdict, still reads.
    #[test]
    fn a_plateau_saved_before_the_directions_still_reads() {
        let old = serde_json::json!({
            "best_objective": 1.25,
            "median_ratio": 0.8,
            "positive_share": 0.9,
        });
        let p: PlateauReport = serde_json::from_value(old).unwrap();
        assert_eq!(
            p,
            PlateauReport {
                best_objective: Some(1.25),
                directions: Vec::new(),
                median_ratio: Some(0.8),
                positive_share: Some(0.9),
            }
        );
        let direction: PlateauDirection = serde_json::from_value(serde_json::json!({
            "steps": [1, 0],
            "windows": 2,
            "objective": 0.5,
            "total_return": 0.1,
        }))
        .unwrap();
        assert_eq!(direction.reference, None);
        let mut verdict =
            serde_json::to_value(Verdict::new(test_instrument().id, Vec::new())).unwrap();
        verdict.as_object_mut().unwrap().remove("warnings");
        let verdict: Verdict = serde_json::from_value(verdict).unwrap();
        assert!(verdict.warnings.is_empty());
    }
}
