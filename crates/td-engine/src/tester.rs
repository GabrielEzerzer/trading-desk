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

use std::collections::{BTreeSet, HashMap};
use std::ops::{ControlFlow, Range};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use td_core::{Candle, Fill, Millis, Side, Timeframe};

use crate::metrics::{self, EquitySeries, MetricsReport};
use crate::permute::permute_quotes;
use crate::risk::{EquityRange, Intrabar};
use crate::rng::{SplitMix64, derive_seed};
use crate::runner::{compare, config_fingerprint, hold_over, match_asks, select_bars};
use crate::{
    BacktestConfig, BacktestResult, Benchmark, EngineError, EquityPoint, ParamKind, ParamSpec,
    ParamValue, Params, Trade, find_strategy, run,
};

/// The most backtests one sweep may run: the grid size, or the sample count in random mode. A
/// walk-forward may run at most this many over all its train windows together.
pub const MAX_RUNS: usize = 100_000;

/// A progress callback: runs done so far, then the total. Called from the worker threads after
/// each run.
///
/// Returning `ControlFlow::Break(())` stops the whole call: runs not started yet are skipped,
/// runs already going finish, and the call returns [`TesterError::Cancelled`]. Returning
/// `Continue` leaves the result exactly as without a callback.
pub type Progress<'a> = &'a (dyn Fn(usize, usize) -> ControlFlow<()> + Sync);

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

    /// The progress callback asked to stop.
    #[error("the run was cancelled")]
    Cancelled,
}

fn in_window(window: &Option<usize>) -> String {
    window.map_or(String::new(), |w| format!(" in walk-forward window {w}"))
}

// ---------------------------------------------------------------------------------------------
// The sweep specification
// ---------------------------------------------------------------------------------------------

/// The values one parameter takes in a sweep.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamAxis {
    pub name: String,
    pub space: ParamSpace,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
///
/// `Sharpe` and `Sortino` are undefined on a range with fewer than
/// [`metrics::MIN_DAILY_RETURNS`] daily returns on days with a bar close, and `Cagr` and `Calmar`
/// on a range shorter than [`metrics::MIN_CAGR_DAYS`] days; a run whose objective is undefined
/// does not qualify (see [`SweepRow::qualified`]). In a walk-forward that applies to each train
/// window's length, and in an out-of-sample test to the in-sample segment's.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Maximize,
    Minimize,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Objective {
    pub metric: Metric,
    pub direction: Direction,
}

/// A parameter sweep.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
    /// Runs with fewer closed trades than this rank after every run that has enough. Defaults to
    /// 1 when left out, so a parameter set that never trades (no drawdown, no losses) cannot top
    /// a ranking by drawdown; set 0 to rank such sets too.
    #[serde(default = "default_min_trades")]
    pub min_trades: usize,
}

fn default_min_trades() -> usize {
    1
}

impl SweepSpec {
    /// Checks the spec without data: the strategy, every fixed and swept parameter value, and the
    /// run count, as [`sweep`] does before it runs anything. What depends on the data (the range,
    /// rules across parameters) is checked by the run.
    pub fn check(&self) -> Result<(), TesterError> {
        Plan::new(self).map(|_| ())
    }

    /// The backtests one sweep of this spec runs and reports progress for, after the checks of
    /// [`SweepSpec::check`].
    pub(crate) fn runs(&self) -> Result<usize, TesterError> {
        Plan::new(self).map(|p| p.indices.len())
    }

    /// The most grid neighbours a parameter set can have on this spec's grid: the sets at most one
    /// step from it on every axis, itself left out. A set at the end of an axis with three or more
    /// values has fewer.
    pub(crate) fn max_neighbours(&self) -> Result<usize, TesterError> {
        let plan = Plan::new(self)?;
        Ok(plan
            .axes
            .iter()
            .map(|a| a.count.min(3) as usize)
            .product::<usize>()
            - 1)
    }

    /// The grid neighbours of the set at `coords` on this spec's grid, such as a walk-forward
    /// window's choice ([`WindowResult::coords`]): every set at most one step from it on every
    /// axis, itself left out, in grid order. Each comes with its step on each axis (-1, 0 or 1)
    /// and its parameters as a sweep row of the spec would hold them. Errors when `coords` is not
    /// a place on the grid.
    pub(crate) fn neighbours(&self, coords: &[u64]) -> Result<Vec<(Vec<i8>, Params)>, TesterError> {
        let plan = Plan::new(self)?;
        if coords.len() != plan.axes.len()
            || plan.axes.iter().zip(coords).any(|(a, &k)| k >= a.count)
        {
            return Err(TesterError::InvalidSpec(format!(
                "{coords:?} is not a place on the grid"
            )));
        }
        let mut out = Vec::new();
        // Every step vector in grid order: the first axis changes slowest.
        let n = plan.axes.len() as u32;
        for code in 0..3usize.pow(n) {
            let steps: Vec<i8> = (0..n)
                .rev()
                .map(|i| (code / 3usize.pow(i) % 3) as i8 - 1)
                .collect();
            if steps.iter().all(|&d| d == 0) {
                continue;
            }
            let mut index = 0u64;
            let mut on_grid = true;
            for ((a, &k), &d) in plan.axes.iter().zip(coords).zip(&steps) {
                match k.checked_add_signed(i64::from(d)) {
                    Some(at) if at < a.count => index = index * a.count + at,
                    _ => on_grid = false,
                }
            }
            if on_grid {
                let params = plan.params(&self.base.params, index).resolve(&plan.specs)?;
                out.push((steps, params));
            }
        }
        Ok(out)
    }
}

impl OosSpec {
    /// [`SweepSpec::check`], plus the out-of-sample rules that need no data.
    pub fn check(&self) -> Result<(), TesterError> {
        refuse_base_warmup(&self.sweep)?;
        self.sweep.check()
    }
}

impl WalkForwardSpec {
    /// [`SweepSpec::check`], plus the walk-forward rules that need no data, among them that the
    /// first train window can be long enough for a Sharpe, Sortino, CAGR or Calmar objective with
    /// no gaps in the data. Whether the windows fit the range, and whether each train window in
    /// the data is long enough, is checked by the run before it runs anything.
    pub fn check(&self) -> Result<(), TesterError> {
        refuse_base_warmup(&self.sweep)?;
        self.sweep.check()?;
        refuse_short_train_bars(self)
    }
}

/// The metrics a sweep keeps per run.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepRow {
    /// Position in the grid order. Runs that rank equal are ordered by it.
    pub index: u64,
    /// The row's place on each axis of the grid, in the order of the spec's axes: the position
    /// of its value in that axis's values, from 0. The grid index is these coordinates read as a
    /// mixed-radix number with the last axis as its lowest digit. Empty in a result saved before
    /// rows had them.
    #[serde(default)]
    pub coords: Vec<u64>,
    /// The full parameter set the run used, defaults included.
    pub params: Params,
    /// Whether the run ranks by its objective: it ran, made at least `min_trades` closed trades
    /// and its objective is defined (see [`Metric`] for when it is not). Other runs rank after
    /// all qualified ones: first those that ran, then those the strategy refused, each in grid
    /// order.
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
    fn params(&self, base: &Params, index: u64) -> Params {
        let mut p = base.clone();
        for (a, k) in self.axes.iter().zip(self.coords(index)) {
            p.set(&a.name, a.value(k));
        }
        p
    }

    /// The position on each axis, in axis order, of grid index `index`. The last axis changes
    /// fastest.
    fn coords(&self, mut index: u64) -> Vec<u64> {
        let mut coords = vec![0; self.axes.len()];
        for (c, a) in coords.iter_mut().zip(&self.axes).rev() {
            *c = index % a.count;
            index /= a.count;
        }
        coords
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
/// with its error; any other run error stops the sweep. `bars`, `asks` and `funding` are the data
/// as [`crate::run`] takes it.
pub fn sweep(
    spec: &SweepSpec,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
    progress: Option<Progress>,
) -> Result<SweepResult, TesterError> {
    let plan = Plan::new(spec)?;
    sweep_planned(spec, &plan, bars, asks, funding, progress)
}

fn sweep_planned(
    spec: &SweepSpec,
    plan: &Plan,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
    progress: Option<Progress>,
) -> Result<SweepResult, TesterError> {
    let data_fingerprint = config_fingerprint(&spec.base, bars, asks, funding)?;
    let rows = ranked_rows(spec, plan, bars, asks, funding, progress)?;
    let best = match rows.first() {
        Some(top) if top.qualified => Some(run(
            &with_params(&spec.base, &top.params),
            bars,
            asks,
            funding,
        )?),
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

/// Runs every parameter set of the plan and ranks the rows, best first.
fn ranked_rows(
    spec: &SweepSpec,
    plan: &Plan,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
    progress: Option<Progress>,
) -> Result<Vec<SweepRow>, TesterError> {
    let total = plan.indices.len();
    let done = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    // `None` for a run skipped after a stop.
    let runs: Vec<Option<Result<SweepRow, EngineError>>> = plan
        .indices
        .par_iter()
        .map(|&index| {
            if stopped.load(Ordering::Relaxed) {
                return None;
            }
            let row = run_row(spec, plan, index, bars, asks, funding);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some(report) = progress
                && report(n, total).is_break()
            {
                stopped.store(true, Ordering::Relaxed);
            }
            Some(row)
        })
        .collect();
    if stopped.load(Ordering::Relaxed) {
        return Err(TesterError::Cancelled);
    }
    let mut rows = runs.into_iter().flatten().collect::<Result<Vec<_>, _>>()?;
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
    Ok(rows)
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
    asks: &[Candle],
    funding: &[(Millis, f64)],
) -> Result<SweepRow, EngineError> {
    let params = plan.params(&spec.base.params, index).resolve(&plan.specs)?;
    match run(&with_params(&spec.base, &params), bars, asks, funding) {
        Ok(result) => {
            let h = Headline::new(&result.metrics);
            let qualified = h.trades >= spec.min_trades && h.value(spec.objective.metric).is_some();
            Ok(SweepRow {
                index,
                coords: plan.coords(index),
                params,
                qualified,
                metrics: Some(h),
                error: None,
            })
        }
        Err(e @ EngineError::InvalidParam { .. }) => Ok(SweepRow {
            index,
            coords: plan.coords(index),
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
// The luck test by bar permutation
// ---------------------------------------------------------------------------------------------

/// A luck test by bar permutation (R3 in #35): the same sweep on the real bars and on
/// `permutations` shuffled copies of them.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermutationSpec {
    pub sweep: SweepSpec,
    /// Shuffled copies to sweep; #35 uses 199, so the smallest possible p-value is 0.005 when
    /// every copy has a qualifying run.
    pub permutations: usize,
    /// Copy k is shuffled with a seed derived from this one and k, so the same seed shuffles the
    /// same copies.
    pub seed: u64,
}

/// The result of a luck test by bar permutation.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermutationResult {
    pub spec: PermutationSpec,
    pub engine_version: String,
    /// The fingerprint of the real data in the base range; see [`crate::data_fingerprint`].
    pub data_fingerprint: String,
    /// The best objective value of the sweep on the real bars.
    pub real_best: f64,
    /// The best objective value of each shuffled copy's sweep, in copy order; `None` when no run
    /// on that copy qualified.
    pub permuted_best: Vec<Option<f64>>,
    /// Copies with a qualifying run: the entries of `permuted_best` that are not `None`.
    pub qualified_copies: usize,
    /// Qualifying copies whose best is at least as good as the real best: at or above it when
    /// maximising, at or below it when minimising, ties included.
    pub at_least_as_good: usize,
    /// `(1 + at_least_as_good) / (1 + qualified_copies)`: the chance of a best this good if the
    /// bars' order carried no information, given that the real sweep qualified. The test runs
    /// only when it did, so the real bars are compared with the copies that qualified too; if
    /// the order carried nothing, the real best is then equally likely to rank anywhere among
    /// those `1 + qualified_copies` bests, which makes this the exact permutation p-value
    /// (counting the real sweep as one of the draws keeps it above zero; Phipson and Smyth,
    /// 2010). Dividing by all the copies instead would count the ones with no qualifying run as
    /// worse than the real best and bias the p-value low.
    ///
    /// The smallest it can be is `1 / (1 + qualified_copies)`. When that is above a pass
    /// threshold, the test cannot pass however good the real best is: too few copies qualified.
    pub p_value: f64,
}

/// Runs a luck test by bar permutation: the sweep on the real bars, then on each shuffled copy,
/// keeping each sweep's best objective value, and the p-value of the real best against the
/// copies'.
///
/// A copy shuffles the traded bars, those in the base range, with [`crate::permute_bars`];
/// warm-up bars before the range and funding stay as they are, and the first traded bar is
/// unchanged, so a copy joins the warm-up bars where the real data does. Copy k's seed is
/// derived from the spec's seed and k, so a copy does not depend on the others. `bars`, `asks`
/// and `funding` are the data as [`crate::run`] takes it; on a market with quotes each traded
/// bar's ask is rebuilt on its shuffled bar with the spread of its own time slot
/// (`permute_quotes`), so a copy's runs pay the spreads the real ones would at
/// the same times.
///
/// The sweeps run one after another, each in parallel on the current rayon pool as [`sweep`]
/// does, and each keeps only its ranked rows: the best run is not run again. All the runs
/// together, the real sweep's included, count against [`MAX_RUNS`] before anything runs.
/// Progress counts over all of them, so the callback sees one total; when it asks to stop, the
/// call returns [`TesterError::Cancelled`] and no p-value, since one from part of the copies
/// would be a different test.
///
/// shortcut: copies run one after another, so a grid smaller than the core count leaves cores
/// idle; run copies in parallel if small grids become common.
pub fn permutation_test(
    spec: &PermutationSpec,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
    progress: Option<Progress>,
) -> Result<PermutationResult, TesterError> {
    let copies = spec.permutations;
    if copies == 0 {
        return Err(TesterError::InvalidSpec(
            "a permutation test needs at least one shuffled copy".into(),
        ));
    }
    let plan = Plan::new(&spec.sweep)?;
    let per_sweep = plan.indices.len();
    let runs = per_sweep as u128 * (copies as u128 + 1);
    if runs > MAX_RUNS as u128 {
        return Err(TesterError::TooManyRuns {
            runs,
            limit: MAX_RUNS,
        });
    }
    let base = &spec.sweep.base;
    let data_fingerprint = config_fingerprint(base, bars, asks, funding)?;
    let total = per_sweep * (copies + 1);
    let objective = spec.sweep.objective;
    let best_of =
        |sweep_no: usize, bars: &[Candle], asks: &[Candle]| -> Result<Option<f64>, TesterError> {
            let offset = sweep_no * per_sweep;
            let sweep_progress = |done: usize, _: usize| match progress {
                Some(report) => report(offset + done, total),
                None => ControlFlow::Continue(()),
            };
            let rows = ranked_rows(
                &spec.sweep,
                &plan,
                bars,
                asks,
                funding,
                Some(&sweep_progress),
            )?;
            Ok(rows
                .first()
                .filter(|top| top.qualified)
                .and_then(|top| objective_value(top, objective)))
        };

    // The real sweep ran, so every traded bar has its ask on a market with quotes, and there
    // are none on the others.
    let real_best = best_of(0, bars, asks)?.ok_or(TesterError::NoQualifyingRun {
        min_trades: spec.sweep.min_trades,
        window: None,
    })?;
    let traded: Vec<usize> = (0..bars.len())
        .filter(|&i| (base.from..base.to).contains(&bars[i].open_time))
        .collect();
    let originals: Vec<Candle> = traded.iter().map(|&i| bars[i]).collect();
    // The index in `asks` of each traded bar's ask.
    let traded_asks: Vec<usize> = if asks.is_empty() {
        Vec::new()
    } else {
        let at: HashMap<Millis, usize> = asks
            .iter()
            .enumerate()
            .map(|(j, a)| (a.open_time, j))
            .collect();
        traded
            .iter()
            .map(|&i| {
                let time = bars[i].open_time;
                at.get(&time).copied().ok_or(EngineError::MissingAsk {
                    time,
                    kind: base.instrument.id.kind,
                })
            })
            .collect::<Result<_, _>>()?
    };
    let original_asks: Vec<Candle> = traded_asks.iter().map(|&j| asks[j]).collect();
    let mut copy = bars.to_vec();
    let mut copy_asks = asks.to_vec();
    let mut permuted_best = Vec::with_capacity(copies);
    for k in 0..copies {
        let (shuffled, shuffled_asks) =
            permute_quotes(&originals, &original_asks, derive_seed(spec.seed, k as u64))
                .map_err(EngineError::from)?;
        for (&i, bar) in traded.iter().zip(shuffled) {
            copy[i] = bar;
        }
        for (&j, ask) in traded_asks.iter().zip(shuffled_asks) {
            copy_asks[j] = ask;
        }
        permuted_best.push(best_of(k + 1, &copy, &copy_asks)?);
    }

    let (qualified_copies, at_least_as_good, p_value) =
        permutation_p_value(real_best, &permuted_best, objective.direction);
    Ok(PermutationResult {
        spec: spec.clone(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        data_fingerprint,
        real_best,
        permuted_best,
        qualified_copies,
        at_least_as_good,
        p_value,
    })
}

/// The qualifying copies, those of them at least as good as the real best, and the p-value; see
/// [`PermutationResult::p_value`].
fn permutation_p_value(
    real_best: f64,
    permuted_best: &[Option<f64>],
    direction: Direction,
) -> (usize, usize, f64) {
    let qualified = permuted_best.iter().flatten();
    let at_least_as_good = qualified
        .clone()
        .filter(|&&x| match direction {
            Direction::Maximize => x >= real_best,
            Direction::Minimize => x <= real_best,
        })
        .count();
    let q = qualified.count();
    (
        q,
        at_least_as_good,
        (1 + at_least_as_good) as f64 / (1 + q) as f64,
    )
}

// ---------------------------------------------------------------------------------------------
// In-sample / out-of-sample
// ---------------------------------------------------------------------------------------------

/// Where the base range splits into in-sample and out-of-sample.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Split {
    /// This fraction of the range's bars is in-sample: the out-of-sample segment starts at bar
    /// `floor(bars * fraction)`. Must be between 0 and 1, exclusive.
    Fraction(f64),
    /// The out-of-sample segment starts at the first bar that opens at or after this time.
    At(Millis),
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OosSpec {
    /// The sweep over the whole range; `base.from..base.to` is split in two.
    pub sweep: SweepSpec,
    pub split: Split,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
    /// Out-of-sample objective over in-sample objective. With `TotalReturn` as the objective it
    /// is the ratio of CAGRs instead, because the two segments usually differ in length and a
    /// cumulative return grows with it. `None` if either value is undefined or the in-sample
    /// value is zero. For a maximised metric below 1 is worse out of sample; for a minimised one
    /// above 1 is worse. Not meaningful when the in-sample value is negative.
    ///
    /// With `TotalReturn`, `Cagr` or `Calmar` it is therefore `None` when either segment is
    /// shorter than [`metrics::MIN_CAGR_DAYS`] days, and with `Sharpe` or `Sortino` when either
    /// has fewer than [`metrics::MIN_DAILY_RETURNS`] daily returns on days with a bar close (the
    /// in-sample segment is refused up front then, so this is the out-of-sample one). It does not
    /// fall back to the ratio of total returns, which would compare segments of different
    /// lengths; compare the two runs' metrics directly then.
    pub degradation: Option<f64>,
}

/// Optimises on the in-sample segment, then runs the best parameters on the out-of-sample
/// segment with the in-sample bars as warm-up. An objective undefined on the in-sample segment
/// whatever the parameters (see [`Metric`]) is refused before anything runs. `bars`, `asks` and
/// `funding` are the data as [`crate::run`] takes it.
pub fn out_of_sample(
    spec: &OosSpec,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
    progress: Option<Progress>,
) -> Result<OosResult, TesterError> {
    let base = &spec.sweep.base;
    refuse_base_warmup(&spec.sweep)?;
    let plan = Plan::new(&spec.sweep)?;
    let (_, traded) = select_bars(base, bars)?;
    let split_at = split_time(&traded, spec.split)?;
    let in_sample_bars = traded.partition_point(|b| b.open_time < split_at);
    refuse_short_segment(
        spec.sweep.objective.metric,
        base.timeframe,
        &traded[..in_sample_bars],
        FloorRange::InSample,
    )?;

    let mut is_spec = spec.sweep.clone();
    is_spec.base.to = split_at;
    // Close out like the out-of-sample run, so both sides count a position held at the end alike.
    is_spec.base.close_at_end = true;
    let in_sample = sweep_planned(&is_spec, &plan, bars, asks, funding, progress)?;
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
    let out_of_sample = run(&oos_config, bars, asks, funding)?;

    let degradation = degradation(
        spec.sweep.objective.metric,
        &Headline::new(&best.metrics),
        &Headline::new(&out_of_sample.metrics),
    );
    Ok(OosResult {
        spec: spec.clone(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        data_fingerprint: config_fingerprint(base, bars, asks, funding)?,
        split_at,
        in_sample,
        out_of_sample,
        degradation,
    })
}

/// Out-of-sample over in-sample value of the objective, on a scale that does not depend on how
/// long each segment is. `TotalReturn` is cumulative, so +20% over two years against +10% over
/// half a year would read as 0.5 although the second grows faster; it is compared by CAGR
/// instead. The other objectives need no such change: CAGR, Sharpe, Sortino and Calmar are
/// annualised, profit factor and win rate are per-trade ratios, and the maximum drawdown is a
/// depth, not a sum over time (a longer segment tends to reach a deeper one, but no exact
/// rescaling exists for it). Totals that do grow with length, such as trade counts and fee or
/// funding sums, cannot be objectives. `None` if either value is undefined (a segment too short
/// for a CAGR or a Sharpe ratio, see [`OosResult::degradation`]) or the in-sample value is zero.
fn degradation(metric: Metric, in_sample: &Headline, out_of_sample: &Headline) -> Option<f64> {
    let compared = match metric {
        Metric::TotalReturn => Metric::Cagr,
        m => m,
    };
    match (in_sample.value(compared), out_of_sample.value(compared)) {
        (Some(i), Some(o)) if i != 0.0 => Some(o / i),
        _ => None,
    }
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

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
/// The windows are laid back from the end of the range, so the most recent bars are always
/// tested: the last test segment ends on the last bar, and each earlier one starts `step` bars
/// before the next. There are as many windows as fit with a whole train window before the first
/// test segment. A window's train window is the `train` bars before its test segment (rolling)
/// or every bar before it (anchored), so a test segment never overlaps its own train window.
/// `step` must be at least `test`, so test segments never overlap each other; with
/// `step == test` they are contiguous, and with a longer step the untested bars sit before each
/// test segment.
///
/// The leftover bars at the start, fewer than `step`, are never tested. In rolling mode the first
/// train sweep is warmed up on them (as many as the train length); in anchored mode every train
/// window starts at the first bar, so they are part of each one's training data.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
        // Laid back from the end: `count - 1` steps after the first test segment, the last one ends
        // on the last bar. `(count - 1) * step <= bars - needed`, so nothing below overflows.
        let count = (bars - needed) / self.step_bars + 1;
        let first_test = bars - (count - 1) * self.step_bars - self.test_bars;
        Ok((0..count)
            .map(|k| {
                let start = first_test + k * self.step_bars;
                let train_start = match self.mode {
                    WindowMode::Rolling => start - self.train_bars,
                    WindowMode::Anchored => 0,
                };
                Window {
                    train: train_start..start,
                    test: start..start + self.test_bars,
                }
            })
            .collect())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
    /// Their place on the grid: the train sweep's best row's [`SweepRow::coords`]. Empty in a
    /// result saved before windows had them.
    #[serde(default)]
    pub coords: Vec<u64>,
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
/// start of the range), the same warm-up its test run gets; the first window's uses the leftover
/// bars before it (see [`WindowSpec`]) and starts cold if there are none. Every anchored window's
/// starts cold, because its train window begins at the first bar.
///
/// The stitched metrics are computed like a single backtest's (see [`MetricsReport`]): the
/// stitched Sharpe, Sortino and volatility come from the stitched curve's daily UTC returns, not
/// from its per-bar returns, so they are comparable with a backtest's on any timeframe. The floors
/// on annualised metrics apply to the stitched curve as a whole, so it can have a Sharpe ratio or
/// a CAGR when no single test window is long enough for one.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
    /// The stitched equity's range through each bar, one per point of `equity`: each test run's
    /// [`BacktestResult::equity_extremes`] scaled by its segment's factor. Empty in a result saved
    /// before walk-forwards had them, or when a test run has none.
    #[serde(default)]
    pub equity_extremes: Vec<EquityRange>,
    /// Buy and hold in each test window, stitched like the test runs: each window buys at its
    /// first test bar's open with the starting cash and holds to its last test bar's close, with
    /// the fill, fee and funding rules of a backtest's benchmark, and the windows are compounded
    /// with [`stitch`] as the test segments are. The two curves are therefore exposed over the
    /// same bars: with a step longer than the test, neither holds through the bars between test
    /// windows. Its excess return is the stitched total return minus its own.
    ///
    /// Unlike a test run it never sells, so it pays one taker fee per window to the test run's
    /// two, and it holds through each window's last bar, on whose open a test run closes out.
    /// `None` only in a result saved before walk-forwards had a benchmark.
    #[serde(default)]
    pub benchmark: Option<Benchmark>,
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
/// parameters on the test segment, warmed up with the train bars. `bars`, `asks` and `funding` are
/// the data as [`crate::run`] takes it.
pub fn walk_forward(
    spec: &WalkForwardSpec,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
    progress: Option<Progress>,
) -> Result<WalkForwardResult, TesterError> {
    let base = &spec.sweep.base;
    refuse_base_warmup(&spec.sweep)?;
    let plan = Plan::new(&spec.sweep)?;
    refuse_short_train_bars(spec)?;
    let (_, traded) = select_bars(base, bars)?;
    let windows = spec.windows.windows(traded.len())?;
    refuse_short_train(
        spec.sweep.objective.metric,
        base.timeframe,
        &traded,
        &windows,
    )?;
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
        let window_progress = |done: usize, _: usize| match progress {
            Some(report) => report(offset + done, total),
            None => ControlFlow::Continue(()),
        };
        let swept = sweep_planned(&train, &plan, bars, asks, funding, Some(&window_progress))?;
        let coords = swept
            .rows
            .first()
            .map(|r| r.coords.clone())
            .unwrap_or_default();
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
            coords,
            in_sample: Headline::new(&best.metrics),
            out_of_sample: run(&test_config, bars, asks, funding)?,
        });
    }

    let segments = test_segments(&results);
    let Stitched {
        series,
        equity,
        ranges,
        metrics,
        trades,
        fills,
    } = stitched_report(base, &segments);
    let benchmark = stitched_benchmark(base, &results, bars, asks, funding, &series, &metrics)?;
    let param_changes = results
        .windows(2)
        .filter(|w| w[0].params != w[1].params)
        .count();
    let param_change_rate =
        (results.len() > 1).then(|| param_changes as f64 / (results.len() - 1) as f64);
    Ok(WalkForwardResult {
        spec: spec.clone(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        data_fingerprint: config_fingerprint(base, bars, asks, funding)?,
        windows: results,
        equity,
        metrics,
        equity_extremes: ranges,
        benchmark: Some(benchmark),
        trades,
        fills,
        param_changes,
        param_change_rate,
    })
}

/// The name of `metric` when it is undefined on a run too short for it (see [`Metric`]), `None`
/// otherwise.
fn floored_metric_name(metric: Metric) -> Option<&'static str> {
    match metric {
        Metric::Sharpe => Some("Sharpe"),
        Metric::Sortino => Some("Sortino"),
        Metric::Cagr => Some("CAGR"),
        Metric::Calmar => Some("Calmar"),
        _ => None,
    }
}

/// Refuses, without data, a walk-forward whose objective is undefined on its first train window
/// whatever the data: a Sharpe or Sortino objective when that window cannot hold
/// [`metrics::MIN_DAILY_RETURNS`] daily returns on days with a bar close (without gaps, at most
/// one per day it spans and one per bar), or a CAGR or Calmar objective when it cannot span
/// [`metrics::MIN_CAGR_DAYS`] days. The first train window is the shortest: `train_bars` long when
/// rolling, and when anchored up to `step_bars - 1` bars longer, since it starts at the first bar
/// and so also takes the bars left over before the first test.
///
/// Bars without gaps span their count times the timeframe, and hold the most daily returns when
/// they start on the last bar of a UTC day. A data gap would stretch a window over more days, but
/// a window that reaches the floor only through a gap would have its metric measured partly on
/// days with no bars, so the rule counts bars. [`refuse_short_train`] then checks each window in
/// the data.
fn refuse_short_train_bars(spec: &WalkForwardSpec) -> Result<(), TesterError> {
    let Some(name) = floored_metric_name(spec.sweep.objective.metric) else {
        return Ok(());
    };
    let w = &spec.windows;
    // A train length of zero is refused by the run's window rules, with their own message.
    if w.train_bars == 0 {
        return Ok(());
    }
    let bars = match w.mode {
        WindowMode::Rolling => w.train_bars,
        WindowMode::Anchored => w.train_bars.saturating_add(w.step_bars.saturating_sub(1)),
    };
    let tf = spec.sweep.base.timeframe;
    let bar = tf.duration_ms();
    // A span too long for milliseconds in an i64 is far past every floor.
    let Some(span) = i64::try_from(bars).ok().and_then(|n| n.checked_mul(bar)) else {
        return Ok(());
    };
    let window = match w.mode {
        WindowMode::Rolling => format!("a train window of {bars} {tf} bars"),
        WindowMode::Anchored => format!("the first train window, at most {bars} {tf} bars,"),
    };
    match spec.sweep.objective.metric {
        Metric::Sharpe | Metric::Sortino => {
            // Bars starting `offset` into a UTC day hold a daily return for each 00:00 UTC after
            // the first bar's open up to the last bar's close, and one more for the day the close
            // falls in unless it falls on 00:00 (see [`metrics::daily_equity`]). That is the span
            // plus the offset, in days, rounded up. Bars shorter than a day divide it, so the last
            // one of a day starts a bar short of its end; daily and weekly bars start at 00:00.
            let end = (metrics::DAY_MS - bar).max(0) + span;
            let most = end / metrics::DAY_MS + i64::from(end % metrics::DAY_MS != 0);
            if most < metrics::MIN_DAILY_RETURNS as i64 {
                return Err(TesterError::InvalidSpec(format!(
                    "a {name} objective needs at least {} daily returns in every train window, \
                     and {window} has at most {most}",
                    metrics::MIN_DAILY_RETURNS
                )));
            }
            // Bars longer than a day reach the daily floor with fewer bars than daily returns.
            if bars < metrics::MIN_DAILY_RETURNS {
                return Err(TesterError::InvalidSpec(format!(
                    "a {name} objective needs at least {min} bars as well as {min} daily returns \
                     in every train window, and {window} is shorter",
                    min = metrics::MIN_DAILY_RETURNS
                )));
            }
        }
        _ => {
            if span < metrics::MIN_CAGR_DAYS * metrics::DAY_MS {
                return Err(TesterError::InvalidSpec(format!(
                    "a {name} objective needs train windows of at least {} days, and {window} \
                     spans {}",
                    metrics::MIN_CAGR_DAYS,
                    describe_span(span)
                )));
            }
        }
    }
    Ok(())
}

/// Refuses a walk-forward whose objective is undefined on one of its train windows whatever the
/// parameters; see [`refuse_short_segment`]. Every train sweep there would end without a
/// qualifying run; this says why before anything runs.
fn refuse_short_train(
    metric: Metric,
    timeframe: Timeframe,
    traded: &[Candle],
    windows: &[Window],
) -> Result<(), TesterError> {
    for (k, w) in windows.iter().enumerate() {
        refuse_short_segment(
            metric,
            timeframe,
            &traded[w.train.clone()],
            FloorRange::Train(k),
        )?;
    }
    Ok(())
}

/// The run a [`refuse_short_segment`] refusal is about.
#[derive(Debug, Clone, Copy)]
enum FloorRange {
    /// A walk-forward's train window, by index.
    Train(usize),
    /// An out-of-sample test's in-sample segment.
    InSample,
}

/// Refuses a Sharpe or Sortino objective on a segment with fewer than
/// [`metrics::MIN_DAILY_RETURNS`] daily returns, or as many on days with a bar close (see
/// [`metrics::observed_daily_returns`]), or a CAGR or Calmar objective on one
/// shorter than [`metrics::MIN_CAGR_DAYS`] days: the metric is undefined on it whatever the
/// parameters. The daily returns and length are counted from the bars as the run's metrics count
/// them, from the first bar's open to the last bar's close.
fn refuse_short_segment(
    metric: Metric,
    timeframe: Timeframe,
    segment: &[Candle],
    which: FloorRange,
) -> Result<(), TesterError> {
    let Some(name) = floored_metric_name(metric) else {
        return Ok(());
    };
    // Train windows and in-sample segments are at least one bar long.
    let Some(first) = segment.first().map(|b| b.open_time) else {
        return Ok(());
    };
    let step = timeframe.duration_ms();
    let times: Vec<Millis> = std::iter::once(first)
        .chain(segment.iter().map(|b| b.open_time + step))
        .collect();
    let span = times[times.len() - 1] - first;
    let length = format!("{} bars over {}", segment.len(), describe_span(span));
    let (every, spans, this) = match which {
        FloorRange::Train(k) => (
            "every train window",
            "train windows",
            format!("train window {k}"),
        ),
        FloorRange::InSample => (
            "the in-sample segment",
            "an in-sample segment",
            "it".to_string(),
        ),
    };
    let min = metrics::MIN_DAILY_RETURNS;
    let refusal = match metric {
        Metric::Sharpe | Metric::Sortino => {
            let returns = metrics::daily_equity(&times, &vec![1.0; times.len()]).len() - 1;
            // Counted as the report counts them: only days that hold a bar close.
            let observed = metrics::observed_daily_returns(&times);
            if returns < min {
                format!(
                    "a {name} objective needs at least {min} daily returns in {every}, and {this} \
                     ({length}) has {returns}"
                )
            } else if observed < min {
                format!(
                    "a {name} objective needs at least {min} daily returns on days with a bar \
                     close in {every}, and {this} ({length}) has {observed}"
                )
            } else {
                return Ok(());
            }
        }
        _ if span < metrics::MIN_CAGR_DAYS * metrics::DAY_MS => format!(
            "a {name} objective needs {spans} of at least {} days, and {this} is {length}",
            metrics::MIN_CAGR_DAYS
        ),
        _ => return Ok(()),
    };
    Err(TesterError::InvalidSpec(refusal))
}

/// A span of time as whole days and hours, rounded down, such as "8 days 8 hours".
fn describe_span(ms: Millis) -> String {
    const HOUR_MS: Millis = 3_600_000;
    let days = ms / metrics::DAY_MS;
    let hours = ms % metrics::DAY_MS / HOUR_MS;
    let unit = |n: Millis, one: &str| match n {
        1 => format!("1 {one}"),
        _ => format!("{n} {one}s"),
    };
    match (days, hours) {
        (0, h) => unit(h, "hour"),
        (d, 0) => unit(d, "day"),
        (d, h) => format!("{} {}", unit(d, "day"), unit(h, "hour")),
    }
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

/// One run stitched into a curve: a test run's, or buy and hold's in a test window.
struct Segment<'a> {
    equity: &'a [EquityPoint],
    /// The equity's range through each bar, one per point of `equity`, or empty.
    ranges: &'a [EquityRange],
    peak_leverage: Option<f64>,
    fills: &'a [Fill],
    trades: &'a [Trade],
    funding_paid: f64,
}

/// The windows' test runs as segments of the stitched curve.
fn test_segments(windows: &[WindowResult]) -> Vec<Segment<'_>> {
    windows
        .iter()
        .map(|w| run_segment(&w.out_of_sample))
        .collect()
}

fn run_segment(r: &BacktestResult) -> Segment<'_> {
    Segment {
        equity: &r.equity,
        ranges: &r.equity_extremes,
        peak_leverage: r.metrics.peak_leverage,
        fills: &r.fills,
        trades: &r.trades,
        funding_paid: r.metrics.funding_paid,
    }
}

/// The headline metrics of `runs`, one test run per walk-forward window in order, stitched as a
/// walk-forward stitches its own test runs (see [`WalkForwardResult`]).
pub(crate) fn stitched_headline(base: &BacktestConfig, runs: &[&BacktestResult]) -> Headline {
    let segments: Vec<Segment> = runs.iter().map(|r| run_segment(r)).collect();
    Headline::new(&stitched_report(base, &segments).metrics)
}

/// Buy and hold in each test window, stitched like the test runs and compared with the stitched
/// curve: see [`WalkForwardResult::benchmark`].
fn stitched_benchmark(
    base: &BacktestConfig,
    windows: &[WindowResult],
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
    strategy_series: &EquitySeries,
    strategy_report: &MetricsReport,
) -> Result<Benchmark, TesterError> {
    let held = windows
        .iter()
        .map(|w| {
            let config = BacktestConfig {
                from: w.test_from,
                to: w.test_to,
                warmup_from: None,
                close_at_end: false,
                ..base.clone()
            };
            let (_, test_bars) = select_bars(&config, bars)?;
            let test_asks = match_asks(base.instrument.id.kind, bars, asks, &test_bars)?;
            Ok(hold_over(&config, &test_bars, &test_asks, funding))
        })
        .collect::<Result<Vec<_>, TesterError>>()?;
    let segments: Vec<Segment> = held
        .iter()
        .map(|h| Segment {
            equity: &h.equity,
            ranges: &h.intrabar.ranges,
            peak_leverage: h.intrabar.peak_leverage,
            fills: &h.fills,
            trades: &h.trades,
            funding_paid: h.funding_paid,
        })
        .collect();
    let stitched = stitched_report(base, &segments);
    // `windows` fails unless at least one window fits, so there is a first.
    Ok(compare(
        held[0].qty,
        held[0].entry_price,
        stitched.equity,
        &stitched.series,
        stitched.metrics,
        strategy_series,
        strategy_report,
    ))
}

/// The stitched curve with its metrics, trades and fills.
struct Stitched {
    /// The stitched curve as the metrics read it, starting from the starting cash.
    series: EquitySeries,
    equity: Vec<EquityPoint>,
    /// The stitched equity's range through each bar, or empty unless every segment has one per
    /// bar.
    ranges: Vec<EquityRange>,
    metrics: MetricsReport,
    trades: Vec<Trade>,
    fills: Vec<Fill>,
}

/// The stitched curve and its metrics. Trades, fills, positions and funding of each segment are
/// scaled by the segment's factor, so trade P&L adds up to the stitched equity's changes. A
/// segment with factor 0 (after ruin) contributes no trades or fills: zero-sized ones would
/// count as trades in the statistics without having traded anything.
///
/// Each segment's equity range through a bar is scaled by the same factor, since the stitched
/// account holds the scaled position, so the intrabar metrics are those of the stitched account.
/// They are left out (as in a result without ranges) unless every segment has a range per bar.
/// The peak leverage, a ratio the scaling leaves unchanged, is the highest of the segments with a
/// factor above 0.
fn stitched_report(base: &BacktestConfig, runs: &[Segment]) -> Stitched {
    let cash = base.starting_cash;
    let segments: Vec<&[EquityPoint]> = runs.iter().map(|r| r.equity).collect();
    let equity = stitch(cash, &segments);
    let scales = segment_scales(cash, &segments);
    let lot = base.instrument.lot_size;
    let mut positions = Vec::with_capacity(equity.len());
    let mut fills: Vec<Fill> = Vec::new();
    let mut trades: Vec<Trade> = Vec::new();
    let mut funding = 0.0;
    let mut ranges: Vec<EquityRange> = Vec::with_capacity(equity.len());
    let mut peak_leverage: Option<f64> = None;
    let has_ranges = runs.iter().all(|r| r.ranges.len() == r.equity.len());
    for (r, s) in runs.iter().zip(scales) {
        ranges.extend(r.ranges.iter().map(|x| EquityRange {
            low: x.low * s,
            high: x.high * s,
        }));
        // The position after each bar's fills, rebuilt from the fills (a fill's time is the open
        // time of the bar it filled on) and snapped to the lot size against rounding error.
        let mut position = 0.0;
        let mut next = 0;
        for p in r.equity {
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
        if let Some(l) = r.peak_leverage {
            peak_leverage = Some(peak_leverage.map_or(l, |p| p.max(l)));
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
            mae: t.mae.map(|x| x * s),
            mfe: t.mfe.map(|x| x * s),
            ..t.clone()
        }));
        funding += r.funding_paid * s;
    }
    if !has_ranges {
        ranges.clear();
    }
    let intrabar = Intrabar {
        ranges,
        peak_leverage,
    };
    let series = EquitySeries::new(cash, &equity, base.timeframe);
    let metrics = metrics::report_with(
        &series,
        has_ranges.then_some(&intrabar),
        &positions,
        &trades,
        &fills,
        funding,
        base.account_currency,
    );
    Stitched {
        series,
        equity,
        ranges: intrabar.ranges,
        metrics,
        trades,
        fills,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permute_bars;

    /// On a market with quotes a copy keeps each hour's spread. A strategy with no edge that is in
    /// the market only in the one hour whose spread is tight would otherwise beat every copy on
    /// cost alone, since a copy's bars in that hour come from hours with wide spreads.
    #[test]
    fn the_luck_test_keeps_each_hours_spread() {
        use td_core::{FeeSchedule, Instrument, InstrumentId, InstrumentKind, Venue};
        const HOUR: Millis = 3_600_000;
        // 60 days of hourly bars of a seeded random walk, without the FX weekend's closed hours.
        let mut rng = crate::rng::SplitMix64(5);
        let mut close = 100.0;
        let bars: Vec<Candle> = (0..60 * 24)
            .filter_map(|i| {
                let open = close;
                close = open * (1.0 + (rng.unit() - 0.5) * 0.004);
                let t = i * HOUR;
                let shut =
                    InstrumentKind::Fx.closed_spans(t, t + HOUR).first() == Some(&(t..t + HOUR));
                (!shut).then(|| Candle {
                    open_time: t,
                    open,
                    high: open.max(close) * 1.000_5,
                    low: open.min(close) * 0.999_5,
                    close,
                    volume: 1.0,
                    quote_volume: close,
                })
            })
            .collect();
        // The strategy decides at the close of the 05:00 bar and buys at the 06:00 bar's open:
        // that hour's spread is 0.02%, every other hour's 0.5%.
        let asks: Vec<Candle> = bars
            .iter()
            .map(|b| {
                let s = if b.open_time / HOUR % 24 == 6 {
                    1.000_2
                } else {
                    1.005
                };
                Candle {
                    open: b.open * s,
                    high: b.high * s,
                    low: b.low * s,
                    close: b.close * s,
                    ..*b
                }
            })
            .collect();
        let instrument = Instrument {
            id: InstrumentId {
                venue: Venue::Oanda,
                symbol: "TEST_USD".into(),
                kind: InstrumentKind::Fx,
            },
            base: "TEST".into(),
            quote: "USD".into(),
            tick_size: 0.000_01,
            lot_size: 1.0,
            min_qty: 1.0,
            min_notional: 0.0,
            fees: FeeSchedule {
                maker: 0.0,
                taker: 0.0,
                per_unit: 0.0,
            },
        };
        let spec = PermutationSpec {
            sweep: SweepSpec {
                base: BacktestConfig {
                    strategy: "test_one_hour".into(),
                    params: Params::new(),
                    instrument,
                    timeframe: Timeframe::H1,
                    from: bars[0].open_time,
                    to: bars.last().unwrap().open_time + HOUR,
                    starting_cash: 10_000.0,
                    slippage_bps: 0.0,
                    fee_override: None,
                    max_leverage: 1.0,
                    warmup_from: None,
                    close_at_end: true,
                    account_currency: td_core::AccountCurrency::Quote,
                },
                axes: vec![ParamAxis {
                    name: "hour".into(),
                    space: ParamSpace::Values {
                        values: vec![ParamValue::Int(5)],
                    },
                }],
                mode: SweepMode::Grid,
                objective: Objective {
                    metric: Metric::TotalReturn,
                    direction: Direction::Maximize,
                },
                min_trades: 1,
            },
            permutations: 19,
            seed: 3,
        };
        let r = permutation_test(&spec, &bars, &asks, &[], None).unwrap();
        assert_eq!(r.permuted_best.iter().flatten().count(), 19);
        // Paying 0.5% a trade on about 40 trades, a copy would lose about a fifth more than the
        // real run and the real run would rank first, at the smallest p there is, 1/20.
        assert!(r.p_value > 0.2, "{r:?}");
    }

    #[test]
    fn each_copy_is_the_traded_bars_shuffled_with_its_derived_seed_after_the_real_warm_up() {
        use td_core::{FeeSchedule, Instrument, InstrumentId, InstrumentKind, Venue};
        const HOUR: Millis = 3_600_000;
        let bars: Vec<Candle> = (0..260)
            .map(|i| {
                let x = i as f64;
                let close = 100.0 + 0.02 * x + 3.0 * (x / 17.0).sin() + 1.5 * (x / 5.3).cos();
                let open = 100.0 + 0.02 * (x - 1.0) + 3.0 * ((x - 1.0) / 17.0).sin();
                Candle {
                    open_time: i * HOUR,
                    open,
                    high: open.max(close) + 0.2,
                    low: open.min(close) - 0.2,
                    close,
                    volume: 1.0,
                    quote_volume: close,
                }
            })
            .collect();
        let instrument = Instrument {
            id: InstrumentId {
                venue: Venue::Binance,
                symbol: "TESTUSDT".into(),
                kind: InstrumentKind::Spot,
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
        };
        // Warm-up on bars 0..40, traded range 40..220, and bars after the range in the input.
        let spec = PermutationSpec {
            sweep: SweepSpec {
                base: BacktestConfig {
                    strategy: "sma_cross".into(),
                    params: Params::new(),
                    instrument,
                    timeframe: Timeframe::H1,
                    from: 40 * HOUR,
                    to: 220 * HOUR,
                    starting_cash: 10_000.0,
                    slippage_bps: 0.0,
                    fee_override: None,
                    max_leverage: 1.0,
                    warmup_from: Some(0),
                    close_at_end: false,
                    account_currency: td_core::AccountCurrency::Quote,
                },
                axes: vec![ParamAxis {
                    name: "slow".into(),
                    space: ParamSpace::Values {
                        values: vec![ParamValue::Int(15), ParamValue::Int(25)],
                    },
                }],
                mode: SweepMode::Grid,
                objective: Objective {
                    metric: Metric::TotalReturn,
                    direction: Direction::Maximize,
                },
                min_trades: 0,
            },
            permutations: 3,
            seed: 77,
        };
        let r = permutation_test(&spec, &bars, &[], &[], None).unwrap();
        let best = |bars: &[Candle]| {
            let swept = sweep(&spec.sweep, bars, &[], &[], None).unwrap();
            swept.rows[0].metrics.unwrap().total_return
        };
        assert_eq!(Some(r.real_best), best(&bars));
        for k in 0..3 {
            let shuffled = permute_bars(&bars[40..220], derive_seed(77, k as u64)).unwrap();
            let copy: Vec<Candle> = bars[..40].iter().chain(&shuffled).copied().collect();
            assert_eq!(r.permuted_best[k], best(&copy), "copy {k}");
            assert_ne!(copy, bars[..220]);
        }
    }

    #[test]
    fn the_p_value_divides_by_the_copies_that_qualified_not_all_of_them() {
        // Ten copies, four with no qualifying run; one of the six beats the real best of 0.5.
        let copies = [
            Some(0.1),
            None,
            Some(0.7),
            None,
            Some(0.2),
            Some(0.3),
            None,
            Some(0.4),
            None,
            Some(0.0),
        ];
        let (q, b, p) = permutation_p_value(0.5, &copies, Direction::Maximize);
        assert_eq!((q, b), (6, 1));
        assert_eq!(p, 2.0 / 7.0);
        // No copy qualified: nothing to compare with, so p is 1.
        assert_eq!(
            permutation_p_value(0.5, &[None, None], Direction::Maximize),
            (0, 0, 1.0)
        );
    }

    #[test]
    fn a_copy_that_ties_the_real_best_counts_as_at_least_as_good() {
        let copies = [Some(0.25), Some(0.5), Some(1.0), None];
        // Maximising: 0.5 ties and 1.0 beats; 0.25 does not.
        assert_eq!(
            permutation_p_value(0.5, &copies, Direction::Maximize),
            (3, 2, 0.75)
        );
        // Minimising: 0.5 ties and 0.25 beats; 1.0 does not.
        assert_eq!(
            permutation_p_value(0.5, &copies, Direction::Minimize),
            (3, 2, 0.75)
        );
    }

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
        // 32 bars: the last test ends on bar 31, and bars 0 and 1 are left over.
        let w = spec.windows(32).unwrap();
        let expected = [
            (2..12, 12..17),
            (7..17, 17..22),
            (12..22, 22..27),
            (17..27, 27..32),
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
    fn the_last_test_window_ends_on_the_last_bar_and_the_leftover_is_at_the_start() {
        // Three years of 6-hour bars with a year of training and four months of testing: the
        // windows fit 12 360 bars after the first train window into 8 steps with 680 left over.
        for mode in [WindowMode::Rolling, WindowMode::Anchored] {
            let spec = WindowSpec {
                train_bars: 4_380,
                test_bars: 1_460,
                step_bars: 1_460,
                mode,
            };
            let w = spec.windows(18_200).unwrap();
            assert_eq!(w.len(), 9, "{mode:?}");
            assert_eq!(w.last().unwrap().test.end, 18_200, "{mode:?}");
            // The 680 leftover bars come before the first train window, not after the last test.
            let first = &w[0];
            assert_eq!(first.test.start, 680 + 4_380, "{mode:?}");
            match mode {
                WindowMode::Rolling => assert_eq!(first.train, 680..5_060),
                WindowMode::Anchored => assert_eq!(first.train, 0..5_060),
            }
            // Everything after the leftover and the first train window is tested exactly once.
            for pair in w.windows(2) {
                assert_eq!(pair[0].test.end, pair[1].test.start);
            }
            let tested: usize = w.iter().map(|x| x.test.len()).sum();
            assert_eq!(tested, 18_200 - 5_060);
            for x in &w {
                assert_eq!(x.train.end, x.test.start);
                assert_eq!(x.test.len(), 1_460);
                if mode == WindowMode::Rolling {
                    assert_eq!(x.train.len(), 4_380);
                }
            }
        }
    }

    #[test]
    fn with_a_step_longer_than_the_test_the_gaps_fall_before_each_test_and_the_last_ends_the_range()
    {
        let spec = WindowSpec {
            train_bars: 4,
            test_bars: 2,
            step_bars: 3,
            mode: WindowMode::Rolling,
        };
        // 13 bars: the windows sit one bar later than from the start, so bar 12 is tested.
        let w = spec.windows(13).unwrap();
        assert_eq!(
            w.iter().map(|x| x.test.clone()).collect::<Vec<_>>(),
            [5..7, 8..10, 11..13]
        );
        assert_eq!(w[0].train, 1..5);
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
                per_unit: 0.0,
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
            max_leverage: 1.0,
            warmup_from: None,
            close_at_end: true,
            account_currency: td_core::AccountCurrency::Quote,
        };
        let window = |from: i64, to: i64| {
            let config = BacktestConfig {
                from: from * HOUR,
                to: to * HOUR,
                ..base.clone()
            };
            let out_of_sample = run(&config, &bars, &[], &[]).unwrap();
            assert!(!out_of_sample.trades.is_empty() && !out_of_sample.fills.is_empty());
            WindowResult {
                train_from: 0,
                train_to: config.from,
                test_from: config.from,
                test_to: config.to,
                params: base.params.clone(),
                coords: Vec::new(),
                in_sample: Headline::new(&out_of_sample.metrics),
                out_of_sample,
            }
        };
        let mut ruined = window(0, 20);
        // Make the first segment end the account; the second then has factor 0.
        ruined.out_of_sample.equity.last_mut().unwrap().equity = 0.0;
        let windows = [ruined, window(20, 40)];
        let first = &windows[0].out_of_sample;
        let st = stitched_report(&base, &test_segments(&windows));
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
        ] {
            assert!(
                matches!(bad.windows(100), Err(TesterError::InvalidSpec(_))),
                "{bad:?}"
            );
        }
        // A step longer than the range is no overflow: one window, ending on the last bar.
        let long_step = WindowSpec {
            step_bars: usize::MAX,
            ..ok
        };
        assert_eq!(
            long_step.windows(100).unwrap(),
            [Window {
                train: 85..95,
                test: 95..100,
            }]
        );
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
