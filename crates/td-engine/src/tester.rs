//! The strategy tester: parameter sweeps, an in-sample / out-of-sample split and walk-forward
//! optimisation, all built on the backtest runner.
//!
//! A sweep runs one backtest per parameter set, in parallel on the rayon thread pool, and ranks
//! the sets by an objective metric. The out-of-sample split optimises on the first part of a
//! range and runs the chosen parameters on the rest. Walk-forward repeats that over a sequence of
//! train and test windows and stitches the test segments into one equity curve.
//!
//! Results do not depend on the number of threads: every run is independent and deterministic,
//! and the ranking breaks ties on the grid order.

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use td_core::{Candle, Fill, Millis, Side};

use crate::metrics::{self, EquitySeries, MetricsReport};
use crate::runner::{config_fingerprint, select_bars};
use crate::{
    BacktestConfig, BacktestResult, EngineError, EquityPoint, ParamKind, ParamSpec, ParamValue,
    Params, Trade, find_strategy, run,
};

/// The most backtests one sweep may run: the grid size, or the sample count in random mode. A
/// walk-forward may run at most this many over all its train windows together.
pub const MAX_RUNS: usize = 100_000;

/// A progress callback: runs done so far, then the total. Called from the worker threads.
pub type Progress<'a> = &'a (dyn Fn(usize, usize) + Sync);

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TesterError {
    #[error(transparent)]
    Engine(#[from] EngineError),

    #[error("parameter {name:?}: {reason}")]
    InvalidSpace { name: String, reason: String },

    #[error("this would run {runs} backtests, more than the limit of {limit}")]
    TooManyRuns { runs: u128, limit: usize },

    #[error("{0}")]
    InvalidSpec(String),

    #[error(
        "no run made at least {min_trades} closed trades with a defined objective{}",
        in_window(window)
    )]
    NoQualifyingRun {
        min_trades: usize,
        /// The walk-forward window, counted from 0, for a walk-forward run.
        window: Option<usize>,
    },
}

fn in_window(window: &Option<usize>) -> String {
    window.map_or(String::new(), |w| format!(" in walk-forward window {w}"))
}

// ---------------------------------------------------------------------------------------------
// The sweep specification
// ---------------------------------------------------------------------------------------------

/// The values one parameter takes in a sweep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParamSpace {
    /// These values, in this order.
    Values { values: Vec<ParamValue> },
    /// `from`, `from + step`, `from + 2 * step` and so on up to `to` inclusive, for an `Int` or
    /// `Float` parameter. For an `Int` parameter all three must be whole numbers. Float values
    /// are computed as `from + k * step` and never exceed `to`.
    Range { from: f64, to: f64, step: f64 },
    /// `false`, then `true`, for a `Bool` parameter.
    Bool,
}

/// One swept parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamAxis {
    pub name: String,
    pub space: ParamSpace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepMode {
    /// Every combination of the axes' values.
    Grid,
    /// `samples` distinct combinations drawn uniformly from the grid with a PRNG seeded by
    /// `seed`, so the same seed draws the same sets. If `samples` is at least the grid size,
    /// the whole grid runs.
    Random { samples: usize, seed: u64 },
}

/// A metrics report field a sweep can rank by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    TotalReturn,
    Cagr,
    Sharpe,
    Sortino,
    Calmar,
    ProfitFactor,
    WinRate,
    /// The drawdown depth as a positive fraction; rank it with `Minimize`.
    MaxDrawdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Maximize,
    Minimize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Objective {
    pub metric: Metric,
    pub direction: Direction,
}

/// A parameter sweep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepSpec {
    /// The backtest every parameter set shares. Its `params` fix parameters that are not swept;
    /// parameters in neither take the strategy's defaults. A parameter may not be both fixed and
    /// swept.
    pub base: BacktestConfig,
    /// The swept parameters. The grid order has the first axis changing slowest.
    pub axes: Vec<ParamAxis>,
    pub mode: SweepMode,
    pub objective: Objective,
    /// Runs with fewer closed trades than this rank after every run that has enough.
    #[serde(default)]
    pub min_trades: usize,
}

/// The metrics a sweep keeps per run.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Headline {
    pub total_return: Option<f64>,
    pub cagr: Option<f64>,
    pub sharpe: Option<f64>,
    pub sortino: Option<f64>,
    pub calmar: Option<f64>,
    pub profit_factor: Option<f64>,
    pub win_rate: Option<f64>,
    pub max_drawdown: Option<f64>,
    /// Closed trades.
    pub trades: usize,
}

impl Headline {
    pub fn new(m: &MetricsReport) -> Headline {
        Headline {
            total_return: m.total_return,
            cagr: m.cagr,
            sharpe: m.sharpe,
            sortino: m.sortino,
            calmar: m.calmar,
            profit_factor: m.profit_factor,
            win_rate: m.win_rate,
            max_drawdown: m.max_drawdown,
            trades: m.trades,
        }
    }

    pub fn value(&self, metric: Metric) -> Option<f64> {
        match metric {
            Metric::TotalReturn => self.total_return,
            Metric::Cagr => self.cagr,
            Metric::Sharpe => self.sharpe,
            Metric::Sortino => self.sortino,
            Metric::Calmar => self.calmar,
            Metric::ProfitFactor => self.profit_factor,
            Metric::WinRate => self.win_rate,
            Metric::MaxDrawdown => self.max_drawdown,
        }
    }
}

/// One parameter set of a sweep and how it did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepRow {
    /// Position in the grid order. Runs that rank equal are ordered by it.
    pub index: u64,
    /// The full parameter set the run used, defaults included.
    pub params: Params,
    /// Whether the run ranks by its objective: it ran, made at least `min_trades` closed trades
    /// and its objective is defined. Other runs rank after all qualified ones: first those that
    /// ran, then those the strategy refused, each in grid order.
    pub qualified: bool,
    /// `None` when the strategy refused the parameter set.
    pub metrics: Option<Headline>,
    /// Why the strategy refused the parameter set (a rule across parameters, such as an SMA
    /// cross needing fast below slow).
    pub error: Option<String>,
}

/// The result of a sweep.
///
/// shortcut: rows keep headline metrics only, not equity curves or trades, so memory stays
/// bounded with many runs; the best run is run again to get its full result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepResult {
    pub spec: SweepSpec,
    pub engine_version: String,
    /// The fingerprint of the data in the base range; see [`crate::data_fingerprint`].
    pub data_fingerprint: String,
    /// Every parameter set, best first.
    pub rows: Vec<SweepRow>,
    /// The best row's full backtest. `None` when no row qualified.
    pub best: Option<BacktestResult>,
}

// ---------------------------------------------------------------------------------------------
// Grid expansion
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum AxisValues {
    List(Vec<ParamValue>),
    Int { from: i64, step: i64 },
    Float { from: f64, step: f64, to: f64 },
}

#[derive(Debug, Clone)]
struct Axis {
    name: String,
    values: AxisValues,
    count: u64,
}

impl Axis {
    fn value(&self, k: u64) -> ParamValue {
        match &self.values {
            AxisValues::List(v) => v[k as usize],
            AxisValues::Int { from, step } => ParamValue::Int(from + k as i64 * step),
            AxisValues::Float { from, step, to } => {
                ParamValue::Float((from + k as f64 * step).min(*to))
            }
        }
    }
}

/// A checked sweep: its axes and the grid indices to run, ascending.
struct Plan {
    specs: Vec<ParamSpec>,
    axes: Vec<Axis>,
    indices: Vec<u64>,
}

impl Plan {
    fn new(spec: &SweepSpec) -> Result<Plan, TesterError> {
        let base = &spec.base;
        let entry = find_strategy(&base.strategy)
            .ok_or_else(|| EngineError::UnknownStrategy(base.strategy.clone()))?;
        let specs = entry.params;
        base.params.resolve(&specs)?;
        let mut axes: Vec<Axis> = Vec::with_capacity(spec.axes.len());
        for a in &spec.axes {
            if axes.iter().any(|x| x.name == a.name) {
                return Err(space_err(&a.name, "swept more than once"));
            }
            if base.params.get(&a.name).is_some() {
                return Err(space_err(&a.name, "both fixed in the base and swept"));
            }
            axes.push(axis(a, &specs, &base.params)?);
        }
        let total = axes
            .iter()
            .fold(1u128, |n, a| n.saturating_mul(a.count as u128));
        let indices = match spec.mode {
            SweepMode::Grid => {
                if total > MAX_RUNS as u128 {
                    return Err(TesterError::TooManyRuns {
                        runs: total,
                        limit: MAX_RUNS,
                    });
                }
                (0..total as u64).collect()
            }
            SweepMode::Random { samples, seed } => {
                if samples == 0 {
                    return Err(TesterError::InvalidSpec(
                        "random mode needs at least one sample".into(),
                    ));
                }
                if samples > MAX_RUNS {
                    return Err(TesterError::TooManyRuns {
                        runs: samples as u128,
                        limit: MAX_RUNS,
                    });
                }
                if total <= samples as u128 {
                    (0..total as u64).collect()
                } else {
                    let total = u64::try_from(total).map_err(|_| {
                        TesterError::InvalidSpec(
                            "the parameter space is too large to sample".into(),
                        )
                    })?;
                    sample_indices(total, samples, seed)
                }
            }
        };
        Ok(Plan {
            specs,
            axes,
            indices,
        })
    }

    /// The swept values at grid index `index` on top of the base's fixed values. The last axis
    /// changes fastest.
    fn params(&self, base: &Params, mut index: u64) -> Params {
        let mut p = base.clone();
        for a in self.axes.iter().rev() {
            p.set(&a.name, a.value(index % a.count));
            index /= a.count;
        }
        p
    }
}

fn space_err(name: &str, reason: &str) -> TesterError {
    TesterError::InvalidSpace {
        name: name.to_string(),
        reason: reason.to_string(),
    }
}

/// Builds one axis and checks its values against the parameter's spec with [`Params::resolve`].
/// Bounds and kind are per parameter, so checking each axis value on its own checks every
/// combination. A range is checked at both ends, which bounds every value between.
fn axis(a: &ParamAxis, specs: &[ParamSpec], base: &Params) -> Result<Axis, TesterError> {
    let spec =
        specs
            .iter()
            .find(|s| s.name == a.name)
            .ok_or_else(|| EngineError::InvalidParam {
                name: a.name.clone(),
                reason: "unknown parameter".into(),
            })?;
    let (values, count) = match &a.space {
        ParamSpace::Values { values } => {
            if values.is_empty() {
                return Err(space_err(&a.name, "no values"));
            }
            (AxisValues::List(values.clone()), values.len() as u64)
        }
        ParamSpace::Bool => {
            if spec.kind != ParamKind::Bool {
                return Err(space_err(&a.name, "a bool space needs a Bool parameter"));
            }
            let both = vec![ParamValue::Bool(false), ParamValue::Bool(true)];
            (AxisValues::List(both), 2)
        }
        &ParamSpace::Range { from, to, step } => {
            if !(from.is_finite() && to.is_finite() && step.is_finite()) {
                return Err(space_err(&a.name, "range bounds and step must be finite"));
            }
            if step <= 0.0 || from > to {
                return Err(space_err(
                    &a.name,
                    "a range needs a positive step and from at or below to",
                ));
            }
            let steps = ((to - from) / step + 1e-9).floor();
            // Far more than any sweep can run; the run limit is checked on the product.
            if steps >= 1e15 {
                return Err(space_err(&a.name, "the range has too many values"));
            }
            let count = steps as u64 + 1;
            match spec.kind {
                ParamKind::Int => {
                    let whole = |x: f64| x.fract() == 0.0 && x.abs() < 1e15;
                    if !(whole(from) && whole(to) && whole(step)) {
                        return Err(space_err(
                            &a.name,
                            "an Int range needs whole-number bounds and step",
                        ));
                    }
                    let values = AxisValues::Int {
                        from: from as i64,
                        step: step as i64,
                    };
                    (values, count)
                }
                ParamKind::Float => (AxisValues::Float { from, step, to }, count),
                ParamKind::Bool => {
                    return Err(space_err(
                        &a.name,
                        "a range needs an Int or Float parameter",
                    ));
                }
            }
        }
    };
    let axis = Axis {
        name: a.name.clone(),
        values,
        count,
    };
    let checked: Vec<u64> = match axis.values {
        AxisValues::List(_) => (0..count).collect(),
        _ => vec![0, count - 1],
    };
    for k in checked {
        base.clone().with(&a.name, axis.value(k)).resolve(specs)?;
    }
    Ok(axis)
}

/// SplitMix64: a small, fast PRNG with a 64-bit state, enough to draw sweep samples.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`, by rejecting draws from the incomplete last block.
    fn below(&mut self, n: u64) -> u64 {
        let limit = u64::MAX - u64::MAX % n;
        loop {
            let x = self.next();
            if x < limit {
                return x % n;
            }
        }
    }
}

/// `samples` distinct indices in `0..total`, ascending; `samples < total`.
fn sample_indices(total: u64, samples: usize, seed: u64) -> Vec<u64> {
    let mut rng = SplitMix64(seed);
    let n = samples as u64;
    if n * 2 > total {
        // total < 2 * MAX_RUNS here: a partial Fisher-Yates shuffle of the whole grid.
        let mut all: Vec<u64> = (0..total).collect();
        for i in 0..n {
            let j = i + rng.below(total - i);
            all.swap(i as usize, j as usize);
        }
        all.truncate(samples);
        all.sort_unstable();
        all
    } else {
        let mut set = BTreeSet::new();
        while set.len() < samples {
            set.insert(rng.below(total));
        }
        set.into_iter().collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Sweeps
// ---------------------------------------------------------------------------------------------

/// Runs a sweep: one backtest per parameter set, in parallel on the current rayon pool, ranked
/// by the objective.
///
/// The parameter space is checked first: every value against the strategy's specs, the grid
/// size (or sample count) against [`MAX_RUNS`], and the base range against the data. A
/// parameter set the strategy refuses when building (a rule across parameters) becomes a row
/// with its error; any other run error stops the sweep.
pub fn sweep(
    spec: &SweepSpec,
    bars: &[Candle],
    funding: &[(Millis, f64)],
    progress: Option<Progress>,
) -> Result<SweepResult, TesterError> {
    let plan = Plan::new(spec)?;
    sweep_planned(spec, &plan, bars, funding, progress)
}

fn sweep_planned(
    spec: &SweepSpec,
    plan: &Plan,
    bars: &[Candle],
    funding: &[(Millis, f64)],
    progress: Option<Progress>,
) -> Result<SweepResult, TesterError> {
    let data_fingerprint = config_fingerprint(&spec.base, bars, funding)?;
    let total = plan.indices.len();
    let done = AtomicUsize::new(0);
    let runs: Vec<Result<SweepRow, EngineError>> = plan
        .indices
        .par_iter()
        .map(|&index| {
            let row = run_row(spec, plan, index, bars, funding);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some(report) = progress {
                report(n, total);
            }
            row
        })
        .collect();
    let mut rows = runs.into_iter().collect::<Result<Vec<_>, _>>()?;
    let objective = spec.objective;
    rows.sort_by(|a, b| {
        let tier = |r: &SweepRow| match (r.qualified, &r.error) {
            (true, _) => 0,
            (false, None) => 1,
            (false, Some(_)) => 2,
        };
        tier(a)
            .cmp(&tier(b))
            .then_with(
                || match (objective_value(a, objective), objective_value(b, objective)) {
                    (Some(x), Some(y)) if a.qualified && b.qualified => match objective.direction {
                        Direction::Maximize => y.total_cmp(&x),
                        Direction::Minimize => x.total_cmp(&y),
                    },
                    _ => std::cmp::Ordering::Equal,
                },
            )
            .then(a.index.cmp(&b.index))
    });
    let best = match rows.first() {
        Some(top) if top.qualified => {
            Some(run(&with_params(&spec.base, &top.params), bars, funding)?)
        }
        _ => None,
    };
    Ok(SweepResult {
        spec: spec.clone(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        data_fingerprint,
        rows,
        best,
    })
}

fn objective_value(row: &SweepRow, objective: Objective) -> Option<f64> {
    row.metrics.and_then(|m| m.value(objective.metric))
}

fn with_params(base: &BacktestConfig, params: &Params) -> BacktestConfig {
    BacktestConfig {
        params: params.clone(),
        ..base.clone()
    }
}

fn run_row(
    spec: &SweepSpec,
    plan: &Plan,
    index: u64,
    bars: &[Candle],
    funding: &[(Millis, f64)],
) -> Result<SweepRow, EngineError> {
    let params = plan.params(&spec.base.params, index).resolve(&plan.specs)?;
    match run(&with_params(&spec.base, &params), bars, funding) {
        Ok(result) => {
            let h = Headline::new(&result.metrics);
            let qualified = h.trades >= spec.min_trades && h.value(spec.objective.metric).is_some();
            Ok(SweepRow {
                index,
                params,
                qualified,
                metrics: Some(h),
                error: None,
            })
        }
        Err(e @ EngineError::InvalidParam { .. }) => Ok(SweepRow {
            index,
            params,
            qualified: false,
            metrics: None,
            error: Some(e.to_string()),
        }),
        Err(e) => Err(e),
    }
}

/// The tester sets `warmup_from` itself for out-of-sample runs.
fn refuse_base_warmup(spec: &SweepSpec) -> Result<(), TesterError> {
    if spec.base.warmup_from.is_some() {
        return Err(TesterError::InvalidSpec(
            "leave warmup_from empty: the tester warms out-of-sample runs up itself".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// In-sample / out-of-sample
// ---------------------------------------------------------------------------------------------

/// Where the base range splits into in-sample and out-of-sample.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Split {
    /// This fraction of the range's bars is in-sample: the out-of-sample segment starts at bar
    /// `floor(bars * fraction)`. Must be between 0 and 1, exclusive.
    Fraction(f64),
    /// The out-of-sample segment starts at the first bar that opens at or after this time.
    At(Millis),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OosSpec {
    /// The sweep over the whole range; `base.from..base.to` is split in two.
    pub sweep: SweepSpec,
    pub split: Split,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OosResult {
    pub spec: OosSpec,
    pub engine_version: String,
    /// The fingerprint of the data in the whole range.
    pub data_fingerprint: String,
    /// Open time of the first out-of-sample bar; the in-sample segment ends before it.
    pub split_at: Millis,
    /// The sweep over the in-sample segment. Its best run supplies the parameters.
    pub in_sample: SweepResult,
    /// The chosen parameters on the out-of-sample segment, warmed up with the in-sample bars.
    /// It runs with `close_at_end`: a position still open is closed by a market order decided at
    /// the close of the second-to-last bar and filled at the last bar's open, with slippage and
    /// the taker fee, so its trades are all closed and their costs real. The in-sample sweep's
    /// runs close out the same way, so `degradation` and `min_trades` compare like with like.
    pub out_of_sample: BacktestResult,
    /// Out-of-sample objective over in-sample objective. `None` if either is undefined or the
    /// in-sample value is zero. For a maximised metric below 1 is worse out of sample; for a
    /// minimised one above 1 is worse. Not meaningful when the in-sample value is negative.
    pub degradation: Option<f64>,
}

/// Optimises on the in-sample segment, then runs the best parameters on the out-of-sample
/// segment with the in-sample bars as warm-up.
pub fn out_of_sample(
    spec: &OosSpec,
    bars: &[Candle],
    funding: &[(Millis, f64)],
    progress: Option<Progress>,
) -> Result<OosResult, TesterError> {
    let base = &spec.sweep.base;
    refuse_base_warmup(&spec.sweep)?;
    let plan = Plan::new(&spec.sweep)?;
    let (_, traded) = select_bars(base, bars)?;
    let split_at = split_time(&traded, spec.split)?;

    let mut is_spec = spec.sweep.clone();
    is_spec.base.to = split_at;
    // Close out like the out-of-sample run, so both sides count a position held at the end alike.
    is_spec.base.close_at_end = true;
    let in_sample = sweep_planned(&is_spec, &plan, bars, funding, progress)?;
    let best = in_sample
        .best
        .as_ref()
        .ok_or(TesterError::NoQualifyingRun {
            min_trades: spec.sweep.min_trades,
            window: None,
        })?;
    let oos_config = BacktestConfig {
        from: split_at,
        warmup_from: Some(base.from),
        close_at_end: true,
        ..with_params(base, &best.config.params)
    };
    let out_of_sample = run(&oos_config, bars, funding)?;

    let metric = spec.sweep.objective.metric;
    let is_value = Headline::new(&best.metrics).value(metric);
    let oos_value = Headline::new(&out_of_sample.metrics).value(metric);
    let degradation = match (is_value, oos_value) {
        (Some(i), Some(o)) if i != 0.0 => Some(o / i),
        _ => None,
    };
    Ok(OosResult {
        spec: spec.clone(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        data_fingerprint: config_fingerprint(base, bars, funding)?,
        split_at,
        in_sample,
        out_of_sample,
        degradation,
    })
}

/// The open time of the first out-of-sample bar. Both segments must hold at least one bar.
fn split_time(traded: &[Candle], split: Split) -> Result<Millis, TesterError> {
    let n = traded.len();
    let index = match split {
        Split::Fraction(f) => {
            if !(f > 0.0 && f < 1.0) {
                return Err(TesterError::InvalidSpec(format!(
                    "split fraction {f} must be between 0 and 1"
                )));
            }
            (n as f64 * f).floor() as usize
        }
        Split::At(t) => traded.partition_point(|b| b.open_time < t),
    };
    if index == 0 || index >= n {
        return Err(TesterError::InvalidSpec(format!(
            "the split leaves a segment without bars ({index} of {n} bars in-sample)"
        )));
    }
    Ok(traded[index].open_time)
}

// ---------------------------------------------------------------------------------------------
// Walk-forward
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    /// Every train window has the same length and moves forward by the step.
    Rolling,
    /// Every train window starts at the first bar and grows by the step.
    Anchored,
}

/// Walk-forward windows, in bars of the base range.
///
/// Window k tests bars `train + k * step .. train + k * step + test`. Its train window is the
/// `train` bars before that (rolling) or every bar before it (anchored), so a test segment never
/// overlaps its own train window. `step` must be at least `test`, so test segments never overlap
/// each other; with `step == test` they are contiguous. Trailing bars that do not fill a whole
/// test segment are not tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSpec {
    pub train_bars: usize,
    pub test_bars: usize,
    pub step_bars: usize,
    pub mode: WindowMode,
}

/// One walk-forward window as bar index ranges into the base range's bars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub train: Range<usize>,
    pub test: Range<usize>,
}

impl WindowSpec {
    /// The windows over a range of `bars` bars. Errors if the lengths are invalid or not even
    /// one window fits.
    pub fn windows(&self, bars: usize) -> Result<Vec<Window>, TesterError> {
        let invalid = |m: &str| Err(TesterError::InvalidSpec(m.to_string()));
        if self.train_bars == 0 || self.test_bars == 0 {
            return invalid("train and test lengths must be at least one bar");
        }
        if self.step_bars < self.test_bars {
            return invalid("the step must be at least the test length, or test segments overlap");
        }
        let overflow = || TesterError::InvalidSpec("the window lengths are too large".into());
        let needed = self
            .train_bars
            .checked_add(self.test_bars)
            .ok_or_else(overflow)?;
        if needed > bars {
            return Err(TesterError::InvalidSpec(format!(
                "one window needs {needed} bars, the range has {bars}"
            )));
        }
        let mut out = Vec::new();
        let mut start = self.train_bars;
        loop {
            let end = start.checked_add(self.test_bars).ok_or_else(overflow)?;
            if end > bars {
                break;
            }
            let train_start = match self.mode {
                WindowMode::Rolling => start - self.train_bars,
                WindowMode::Anchored => 0,
            };
            out.push(Window {
                train: train_start..start,
                test: start..end,
            });
            start = start.checked_add(self.step_bars).ok_or_else(overflow)?;
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalkForwardSpec {
    /// The sweep run on every train window; `base.from..base.to` is the whole range.
    pub sweep: SweepSpec,
    pub windows: WindowSpec,
}

/// One walk-forward window and its out-of-sample run.
///
/// shortcut: keeps the chosen parameters and their in-sample headline metrics, not the train
/// sweep's rows, so a long walk-forward stays small.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowResult {
    /// The train segment: bars that open in `train_from..train_to`.
    pub train_from: Millis,
    pub train_to: Millis,
    /// The test segment: bars that open in `test_from..test_to`; `test_from == train_to`.
    pub test_from: Millis,
    pub test_to: Millis,
    /// The best parameters on the train segment.
    pub params: Params,
    pub in_sample: Headline,
    /// The chosen parameters on the test segment, warmed up with the train bars and closed out
    /// at its end; see [`WalkForwardResult`].
    pub out_of_sample: BacktestResult,
}

/// The result of a walk-forward run.
///
/// At a test window's end the position is closed for real: each test run has `close_at_end`
/// set, so at the close of the window's second-to-last bar its open orders are cancelled and a
/// reduce-only market order for the whole position is submitted, which fills at the last bar's
/// open with slippage and the taker fee. Every window therefore ends flat unless the close-out
/// is rejected (see `Session::close_out`); its trades are then all closed and counted in the
/// trade statistics, and the exit costs are in the stitched equity. The next window starts flat
/// from where the stitched equity ended.
///
/// Train sweeps close out the same way, so a window's in-sample and out-of-sample trade counts
/// (and `min_trades`) count a position held at the end alike. A rolling window's train sweep is
/// warmed up on the bars just before it, as many as the window's train length (fewer near the
/// start of the range), the same warm-up its test run gets; the first window's, and every
/// anchored window's, starts cold because no bars come before it.
///
/// The stitched metrics are computed like a single backtest's (see [`MetricsReport`]): the
/// stitched Sharpe, Sortino and volatility come from the stitched curve's daily UTC returns, not
/// from its per-bar returns, so they are comparable with a backtest's on any timeframe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalkForwardResult {
    pub spec: WalkForwardSpec,
    pub engine_version: String,
    /// The fingerprint of the data in the whole range.
    pub data_fingerprint: String,
    pub windows: Vec<WindowResult>,
    /// The test segments stitched into one curve; see [`stitch`].
    pub equity: Vec<EquityPoint>,
    /// Metrics of the stitched curve, its trades and its fills.
    pub metrics: MetricsReport,
    /// Every test segment's trades in time order, with quantities, P&L, fees and funding scaled
    /// by the same factor as the segment's equity, so they add up to the stitched equity's
    /// changes. Segments after the stitched equity reached zero (factor 0, see [`stitch`]) traded
    /// nothing in the stitched account and add no trades.
    pub trades: Vec<Trade>,
    /// Every test segment's fills in time order, with quantities and fees scaled the same way and
    /// the same segments left out.
    pub fills: Vec<Fill>,
    /// How many windows chose different parameters from the window before.
    pub param_changes: usize,
    /// `param_changes / (windows - 1)`: 0 when every window chose the same parameters, 1 when
    /// every window changed them. `None` with one window.
    pub param_change_rate: Option<f64>,
}

/// Runs walk-forward optimisation: for each window, a sweep on the train segment, then the best
/// parameters on the test segment, warmed up with the train bars.
pub fn walk_forward(
    spec: &WalkForwardSpec,
    bars: &[Candle],
    funding: &[(Millis, f64)],
    progress: Option<Progress>,
) -> Result<WalkForwardResult, TesterError> {
    let base = &spec.sweep.base;
    refuse_base_warmup(&spec.sweep)?;
    let plan = Plan::new(&spec.sweep)?;
    let (_, traded) = select_bars(base, bars)?;
    let windows = spec.windows.windows(traded.len())?;
    let time_at = |i: usize| traded.get(i).map_or(base.to, |b| b.open_time);

    let per_window = plan.indices.len();
    let runs = per_window as u128 * windows.len() as u128;
    if runs > MAX_RUNS as u128 {
        return Err(TesterError::TooManyRuns {
            runs,
            limit: MAX_RUNS,
        });
    }
    let total = per_window * windows.len();
    let mut results = Vec::with_capacity(windows.len());
    for (k, w) in windows.iter().enumerate() {
        let (train_from, train_to) = (time_at(w.train.start), time_at(w.train.end));
        let (test_from, test_to) = (time_at(w.test.start), time_at(w.test.end));
        let mut train = spec.sweep.clone();
        train.base.from = train_from;
        train.base.to = train_to;
        // Close out like the test run, so both sides count a position held at the end alike.
        train.base.close_at_end = true;
        // Warm up on the bars just before the train window, as many as the test run gets.
        let warm_start = w.train.start.saturating_sub(w.train.len());
        train.base.warmup_from = (warm_start < w.train.start).then(|| time_at(warm_start));
        let offset = k * per_window;
        let window_progress = |done: usize, _: usize| {
            if let Some(report) = progress {
                report(offset + done, total);
            }
        };
        let swept = sweep_planned(&train, &plan, bars, funding, Some(&window_progress))?;
        let best = swept.best.ok_or(TesterError::NoQualifyingRun {
            min_trades: spec.sweep.min_trades,
            window: Some(k),
        })?;
        let test_config = BacktestConfig {
            from: test_from,
            to: test_to,
            warmup_from: Some(train_from),
            close_at_end: true,
            ..with_params(base, &best.config.params)
        };
        results.push(WindowResult {
            train_from,
            train_to,
            test_from,
            test_to,
            params: best.config.params,
            in_sample: Headline::new(&best.metrics),
            out_of_sample: run(&test_config, bars, funding)?,
        });
    }

    let Stitched {
        equity,
        metrics,
        trades,
        fills,
    } = stitched_report(base, &results);
    let param_changes = results
        .windows(2)
        .filter(|w| w[0].params != w[1].params)
        .count();
    let param_change_rate =
        (results.len() > 1).then(|| param_changes as f64 / (results.len() - 1) as f64);
    Ok(WalkForwardResult {
        spec: spec.clone(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        data_fingerprint: config_fingerprint(base, bars, funding)?,
        windows: results,
        equity,
        metrics,
        trades,
        fills,
        param_changes,
        param_change_rate,
    })
}

/// The factor each segment's equity is multiplied by in the stitched curve: the stitched equity
/// at the end of the segments before it, over the starting cash. The first is 1. Once the
/// stitched equity has reached zero or below, every later factor is 0: the account is gone, and
/// a negative factor would turn the later segments' gains into losses.
fn segment_scales(starting_cash: f64, segments: &[&[EquityPoint]]) -> Vec<f64> {
    let mut level = starting_cash;
    segments
        .iter()
        .enumerate()
        .map(|(i, seg)| {
            let scale = match i {
                0 => 1.0,
                _ if level > 0.0 && starting_cash > 0.0 => level / starting_cash,
                _ => 0.0,
            };
            if let Some(last) = seg.last() {
                level = last.equity * scale;
            }
            scale
        })
        .collect()
}

/// Stitches equity curves that each start from `starting_cash` into one curve, compounding:
/// each segment's per-bar returns (its first one measured from the starting cash) are applied
/// to the stitched equity where the previous segment ended. That is the same as multiplying the
/// segment by the stitched equity at the previous segment's end over the starting cash.
pub fn stitch(starting_cash: f64, segments: &[&[EquityPoint]]) -> Vec<EquityPoint> {
    let scales = segment_scales(starting_cash, segments);
    segments
        .iter()
        .zip(scales)
        .flat_map(|(seg, s)| {
            seg.iter().map(move |p| EquityPoint {
                time: p.time,
                equity: p.equity * s,
            })
        })
        .collect()
}

/// The stitched curve with its metrics, trades and fills.
struct Stitched {
    equity: Vec<EquityPoint>,
    metrics: MetricsReport,
    trades: Vec<Trade>,
    fills: Vec<Fill>,
}

/// The stitched curve and its metrics. Trades, fills, positions and funding of each segment are
/// scaled by the segment's factor, so trade P&L adds up to the stitched equity's changes. A
/// segment with factor 0 (after ruin) contributes no trades or fills: zero-sized ones would
/// count as trades in the statistics without having traded anything.
fn stitched_report(base: &BacktestConfig, windows: &[WindowResult]) -> Stitched {
    let cash = base.starting_cash;
    let segments: Vec<&[EquityPoint]> = windows
        .iter()
        .map(|w| w.out_of_sample.equity.as_slice())
        .collect();
    let equity = stitch(cash, &segments);
    let scales = segment_scales(cash, &segments);
    let lot = base.instrument.lot_size;
    let mut positions = Vec::with_capacity(equity.len());
    let mut fills: Vec<Fill> = Vec::new();
    let mut trades: Vec<Trade> = Vec::new();
    let mut funding = 0.0;
    for (w, s) in windows.iter().zip(scales) {
        let r = &w.out_of_sample;
        // The position after each bar's fills, rebuilt from the fills (a fill's time is the open
        // time of the bar it filled on) and snapped to the lot size against rounding error.
        let mut position = 0.0;
        let mut next = 0;
        for p in &r.equity {
            while next < r.fills.len() && r.fills[next].time <= p.time {
                let f = &r.fills[next];
                position += match f.side {
                    Side::Buy => f.qty,
                    Side::Sell => -f.qty,
                };
                next += 1;
            }
            let snapped = if lot > 0.0 {
                (position / lot).round() * lot
            } else {
                position
            };
            positions.push(snapped * s);
        }
        if s == 0.0 {
            continue;
        }
        fills.extend(r.fills.iter().map(|f| Fill {
            qty: f.qty * s,
            fee: f.fee * s,
            ..f.clone()
        }));
        trades.extend(r.trades.iter().map(|t| Trade {
            qty: t.qty * s,
            gross_pnl: t.gross_pnl * s,
            fees: t.fees * s,
            funding: t.funding * s,
            net_pnl: t.net_pnl * s,
            ..t.clone()
        }));
        funding += r.metrics.funding_paid * s;
    }
    let series = EquitySeries::new(cash, &equity, base.timeframe);
    let metrics = metrics::report(&series, &positions, &trades, &fills, funding);
    Stitched {
        equity,
        metrics,
        trades,
        fills,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(time: Millis, equity: f64) -> EquityPoint {
        EquityPoint { time, equity }
    }

    #[test]
    fn stitching_compounds_each_segment_from_where_the_last_ended() {
        let a = [point(0, 110.0), point(1, 99.0)];
        let b = [point(2, 105.0), point(3, 126.0)];
        let c = [point(4, 90.0)];
        let s = stitch(100.0, &[&a, &b, &c]);
        let values: Vec<f64> = s.iter().map(|p| p.equity).collect();
        // Segment b's returns are +5% and +20% on its own 100, so 99 * 1.05 and 99 * 1.26; c is
        // -10% on 124.74.
        let expected = [110.0, 99.0, 103.95, 124.74, 112.266];
        assert_eq!(values.len(), expected.len());
        for (v, e) in values.iter().zip(expected) {
            assert!((v - e).abs() < 1e-9, "{v} vs {e}");
        }
        let times: Vec<Millis> = s.iter().map(|p| p.time).collect();
        assert_eq!(times, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn rolling_windows_slide_and_tests_follow_their_train() {
        let spec = WindowSpec {
            train_bars: 10,
            test_bars: 5,
            step_bars: 5,
            mode: WindowMode::Rolling,
        };
        let w = spec.windows(32).unwrap();
        let expected = [
            (0..10, 10..15),
            (5..15, 15..20),
            (10..20, 20..25),
            (15..25, 25..30),
        ];
        assert_eq!(w.len(), expected.len());
        for (got, (train, test)) in w.iter().zip(expected) {
            assert_eq!((got.train.clone(), got.test.clone()), (train, test));
        }
        for x in &w {
            // No overlap between a test segment and its own train window.
            assert_eq!(x.train.end, x.test.start);
            assert_eq!(x.train.len(), 10);
        }
        for pair in w.windows(2) {
            // Contiguous, non-overlapping test segments.
            assert_eq!(pair[0].test.end, pair[1].test.start);
        }
    }

    #[test]
    fn anchored_windows_always_train_from_the_start() {
        let spec = WindowSpec {
            train_bars: 10,
            test_bars: 5,
            step_bars: 5,
            mode: WindowMode::Anchored,
        };
        let w = spec.windows(30).unwrap();
        assert_eq!(w.len(), 4);
        for (k, x) in w.iter().enumerate() {
            assert_eq!(x.train, 0..10 + 5 * k);
            assert_eq!(x.test, 10 + 5 * k..15 + 5 * k);
        }
    }

    #[test]
    fn a_step_longer_than_the_test_leaves_gaps_but_never_overlaps() {
        let spec = WindowSpec {
            train_bars: 4,
            test_bars: 2,
            step_bars: 3,
            mode: WindowMode::Rolling,
        };
        let w = spec.windows(12).unwrap();
        assert_eq!(
            w.iter().map(|x| x.test.clone()).collect::<Vec<_>>(),
            [4..6, 7..9, 10..12]
        );
        for pair in w.windows(2) {
            assert!(pair[0].test.end <= pair[1].test.start);
        }
    }

    #[test]
    fn bad_window_specs_are_errors() {
        let ok = WindowSpec {
            train_bars: 10,
            test_bars: 5,
            step_bars: 5,
            mode: WindowMode::Rolling,
        };
        for bad in [
            WindowSpec {
                train_bars: 0,
                ..ok
            },
            WindowSpec { test_bars: 0, ..ok },
            WindowSpec { step_bars: 4, ..ok },
        ] {
            assert!(matches!(bad.windows(100), Err(TesterError::InvalidSpec(_))));
        }
        assert!(matches!(ok.windows(14), Err(TesterError::InvalidSpec(_))));
        assert_eq!(ok.windows(15).unwrap().len(), 1);
    }

    #[test]
    fn a_segment_ending_at_or_below_zero_zeroes_every_later_segment() {
        let a = [point(0, 50.0)];
        let b = [point(1, -20.0)];
        let c = [point(2, 150.0)];
        let d = [point(3, 200.0)];
        assert_eq!(
            segment_scales(100.0, &[&a, &b, &c, &d]),
            [1.0, 0.5, 0.0, 0.0]
        );
        let values: Vec<f64> = stitch(100.0, &[&a, &b, &c, &d])
            .iter()
            .map(|p| p.equity)
            .collect();
        assert_eq!(values, [50.0, -10.0, 0.0, 0.0]);
        // Exactly zero is ruin too.
        let z = [point(1, 0.0)];
        assert_eq!(segment_scales(100.0, &[&a, &z, &c]), [1.0, 0.5, 0.0]);
    }

    #[test]
    fn segments_after_ruin_add_no_trades_or_fills_to_the_stitched_lists() {
        use td_core::{FeeSchedule, Instrument, InstrumentId, InstrumentKind, Timeframe, Venue};
        const HOUR: Millis = 3_600_000;
        let instrument = Instrument {
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
        };
        let bars: Vec<Candle> = (0..40)
            .map(|i| {
                let p = 100.0 + i as f64;
                Candle {
                    open_time: i * HOUR,
                    open: p - 0.5,
                    high: p + 0.5,
                    low: p - 1.0,
                    close: p,
                    volume: 10.0,
                    quote_volume: 10.0 * p,
                }
            })
            .collect();
        let base = BacktestConfig {
            strategy: "sma_cross".into(),
            params: Params::new()
                .with("fast", ParamValue::Int(1))
                .with("slow", ParamValue::Int(3)),
            instrument,
            timeframe: Timeframe::H1,
            from: 0,
            to: 40 * HOUR,
            starting_cash: 10_000.0,
            slippage_bps: 0.0,
            fee_override: None,
            warmup_from: None,
            close_at_end: true,
        };
        let window = |from: i64, to: i64| {
            let config = BacktestConfig {
                from: from * HOUR,
                to: to * HOUR,
                ..base.clone()
            };
            let out_of_sample = run(&config, &bars, &[]).unwrap();
            assert!(!out_of_sample.trades.is_empty() && !out_of_sample.fills.is_empty());
            WindowResult {
                train_from: 0,
                train_to: config.from,
                test_from: config.from,
                test_to: config.to,
                params: base.params.clone(),
                in_sample: Headline::new(&out_of_sample.metrics),
                out_of_sample,
            }
        };
        let mut ruined = window(0, 20);
        // Make the first segment end the account; the second then has factor 0.
        ruined.out_of_sample.equity.last_mut().unwrap().equity = 0.0;
        let windows = [ruined, window(20, 40)];
        let first = &windows[0].out_of_sample;
        let st = stitched_report(&base, &windows);
        assert_eq!(st.trades, first.trades);
        assert_eq!(st.fills, first.fills);
        assert_eq!(st.metrics.trades, first.metrics.trades);
        assert_eq!(st.metrics.fees_paid, first.metrics.fees_paid);
        assert!(st.equity[20..].iter().all(|p| p.equity == 0.0));
    }

    #[test]
    fn window_lengths_that_overflow_are_errors() {
        let ok = WindowSpec {
            train_bars: 10,
            test_bars: 5,
            step_bars: 5,
            mode: WindowMode::Rolling,
        };
        for bad in [
            WindowSpec {
                train_bars: usize::MAX,
                ..ok
            },
            WindowSpec {
                test_bars: usize::MAX,
                step_bars: usize::MAX,
                ..ok
            },
            WindowSpec {
                step_bars: usize::MAX,
                ..ok
            },
        ] {
            assert!(
                matches!(bad.windows(100), Err(TesterError::InvalidSpec(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn samples_are_distinct_sorted_and_seeded() {
        for (total, n) in [(10, 7), (1_000_000, 50)] {
            let a = sample_indices(total, n, 42);
            assert_eq!(a.len(), n);
            assert!(a.windows(2).all(|w| w[0] < w[1]));
            assert!(a.iter().all(|&i| i < total));
            assert_eq!(a, sample_indices(total, n, 42));
        }
        assert_ne!(
            sample_indices(1_000_000, 50, 1),
            sample_indices(1_000_000, 50, 2)
        );
    }
}
