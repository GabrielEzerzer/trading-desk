//! Seeded resampling statistics for the test battery (#35): a block bootstrap of the mean
//! (R7, and the expectancy check of the `high_risk` kind rule), a block bootstrap of the max
//! drawdown (the `portfolio` kind rule), the risk of ruin, the odds of passing or breaching a
//! prop-firm challenge, and the value of one attempt at it (#172).
//!
//! Every function takes plain slices, so it does not depend on how the caller builds its daily
//! returns or trade list, and a seed: the same inputs and seed give bit-identical output on every
//! platform and at any thread count. The draws come from [`SplitMix64`] on one thread, in a fixed
//! order, and the arithmetic is only `+ - * /` and comparisons, which IEEE 754 rounds the same
//! everywhere (no `ln`, `exp` or `powf`, whose results depend on the platform's maths library).
//!
//! # Resampling
//!
//! Daily returns are resampled with the stationary bootstrap of Politis and Romano (1994): a
//! resampled series is built from blocks of consecutive days, each starting on a uniformly drawn
//! day, with lengths drawn from a geometric distribution of mean `mean_block`; a block that runs
//! past the last day wraps to the first. Blocks keep the short-range dependence of daily returns
//! (volatility clusters, a strategy's runs of good and bad days) that drawing single days would
//! destroy and that would make the spread of the mean look narrower than it is. The stationary
//! version is used instead of fixed-length blocks because its result changes smoothly with the
//! block length instead of depending on where the cuts fall, and with the wrap every day is drawn
//! equally often.
//!
//! The default mean block length is the integer cube root of the number of returns, at least 1
//! ([`default_block_len`]): 7 days for a year of returns, 9 for two. Block lengths growing like
//! `n^(1/3)` are the rate-optimal choice for the variance of a mean (Hall, Horowitz and Jing,
//! 1995), and the values land in the 5 to 10 day range commonly used for daily data. The
//! drawdown bootstrap uses the same default: its blocks keep short runs of bad days together, and
//! drawing blocks in a new mix shows how deep the drawdown could have been had such runs met. A
//! losing run much longer than a block is broken up, so the resampled drawdown can come out below
//! the historical one; the historical drawdown line still judges that case.
//!
//! For the mean of trade results, trades are resampled one at a time (a mean block of 1, the
//! plain bootstrap): the spread of a mean depends little on the order of separate trades. The
//! risk of ruin does depend on it, because ruin comes from a run of losses, so [`risk_of_ruin`]
//! draws trades in blocks too.

use serde::{Deserialize, Serialize};

use crate::rng::SplitMix64;
use crate::verdict::AttemptTerms;

/// Resamples, or paths, per call unless the caller sets another count. With 2,000 the standard
/// error of an estimated probability is at most `sqrt(0.25 / 2000)`, about 1.1 percentage points,
/// and a 5% quantile sits on about 100 resamples below it.
pub const DEFAULT_RESAMPLES: usize = 2_000;

/// The default confidence of [`MeanBootstrap::lower_bound`].
pub const DEFAULT_CONFIDENCE: f64 = 0.95;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StatsError {
    #[error("there are no values to resample")]
    NoData,

    #[error("value {index} is not a finite number")]
    NonFinite { index: usize },

    #[error("{0}")]
    InvalidSpec(&'static str),
}

/// The integer cube root of `n`, at least 1: the default mean block length for `n` daily returns.
/// Computed on integers, so it is exact.
pub fn default_block_len(n: usize) -> usize {
    let mut root = 1usize;
    while (root + 1).checked_pow(3).is_some_and(|c| c <= n) {
        root += 1;
    }
    root
}

/// Draws indices into a series of `n` values by the stationary bootstrap: each step starts a new
/// block at a uniform index with probability `1 / mean_block`, and otherwise takes the next
/// index, wrapping at the end. The first step always starts a block.
struct Resampler {
    rng: SplitMix64,
    n: u64,
    restart: f64,
    at: Option<u64>,
}

impl Resampler {
    fn new(n: usize, mean_block: usize, seed: u64) -> Resampler {
        Resampler {
            rng: SplitMix64(seed),
            n: n as u64,
            restart: 1.0 / mean_block as f64,
            at: None,
        }
    }

    /// Starts a new resampled series: its first index begins a new block.
    fn reset(&mut self) {
        self.at = None;
    }

    fn next(&mut self) -> usize {
        let i = match self.at {
            // With a mean block of 1 every step starts a block; no draw is spent deciding that.
            Some(prev) if self.restart < 1.0 && self.rng.unit() >= self.restart => {
                (prev + 1) % self.n
            }
            _ => self.rng.below(self.n),
        };
        self.at = Some(i);
        i as usize
    }
}

fn check_values(values: &[f64]) -> Result<(), StatsError> {
    if values.is_empty() {
        return Err(StatsError::NoData);
    }
    match values.iter().position(|v| !v.is_finite()) {
        Some(index) => Err(StatsError::NonFinite { index }),
        None => Ok(()),
    }
}

/// Checks the daily returns as [`check_values`] does, that `traded` holds one flag per return,
/// and that `lows`, when given, holds one finite low per return, each at or under its return.
fn check_days(
    daily_returns: &[f64],
    lows: Option<&[f64]>,
    traded: &[bool],
) -> Result<(), StatsError> {
    check_values(daily_returns)?;
    if traded.len() != daily_returns.len() {
        return Err(StatsError::InvalidSpec(
            "there must be one traded flag per daily return",
        ));
    }
    if let Some(lows) = lows {
        if lows.len() != daily_returns.len() {
            return Err(StatsError::InvalidSpec(
                "there must be one daily low per daily return",
            ));
        }
        if let Some(index) = lows.iter().position(|v| !v.is_finite()) {
            return Err(StatsError::NonFinite { index });
        }
        if lows.iter().zip(daily_returns).any(|(low, r)| low > r) {
            return Err(StatsError::InvalidSpec(
                "a day's low cannot be above its close",
            ));
        }
    }
    Ok(())
}

fn check_count(count: usize, mean_block: usize) -> Result<(), StatsError> {
    if count == 0 {
        return Err(StatsError::InvalidSpec("at least one resample is needed"));
    }
    if mean_block == 0 {
        return Err(StatsError::InvalidSpec(
            "the mean block length must be at least 1",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Bootstrap of the mean
// ---------------------------------------------------------------------------------------------

/// How [`bootstrap_mean`] resamples.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BootstrapSpec {
    pub resamples: usize,
    /// The mean block length of the stationary bootstrap; 1 draws single values.
    pub mean_block: usize,
    /// The confidence of the one-sided lower bound, strictly between 0 and 1.
    pub confidence: f64,
    pub seed: u64,
}

impl BootstrapSpec {
    /// The defaults for `n` daily returns: [`DEFAULT_RESAMPLES`] resamples, blocks of mean
    /// [`default_block_len`]`(n)` and a 95% lower bound.
    pub fn daily(n: usize, seed: u64) -> BootstrapSpec {
        BootstrapSpec {
            resamples: DEFAULT_RESAMPLES,
            mean_block: default_block_len(n),
            confidence: DEFAULT_CONFIDENCE,
            seed,
        }
    }

    /// The defaults for trade P&Ls: as [`BootstrapSpec::daily`], but single trades.
    pub fn trades(seed: u64) -> BootstrapSpec {
        BootstrapSpec {
            mean_block: 1,
            ..BootstrapSpec::daily(1, seed)
        }
    }
}

/// Quantiles of the resampled means.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quantiles {
    pub p05: f64,
    pub p25: f64,
    pub p50: f64,
    pub p75: f64,
    pub p95: f64,
}

/// The distribution of a sample's mean under resampling.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeanBootstrap {
    /// The mean of the sample itself.
    pub mean: f64,
    /// The fraction of resampled means at or below zero. The `portfolio` kind rule asks for at
    /// most 0.10 on the daily returns.
    pub p_mean_at_most_zero: f64,
    /// The one-sided lower confidence bound on the mean: the `1 - confidence` quantile of the
    /// resampled means (the percentile method). The `high_risk` kind rule asks for it to be above
    /// zero on the trade P&Ls; that rule asks a one-sided question, so the bound is one-sided.
    pub lower_bound: f64,
    pub quantiles: Quantiles,
    pub resamples: usize,
    pub mean_block: usize,
}

/// Bootstraps the mean of `values`: daily returns for R7 and the `portfolio` rule, with
/// [`BootstrapSpec::daily`], or trade P&Ls for the expectancy check of the `high_risk` rule,
/// with [`BootstrapSpec::trades`]. Each resample has as many values as the sample.
pub fn bootstrap_mean(values: &[f64], spec: &BootstrapSpec) -> Result<MeanBootstrap, StatsError> {
    check_values(values)?;
    check_count(spec.resamples, spec.mean_block)?;
    if !(spec.confidence > 0.0 && spec.confidence < 1.0) {
        return Err(StatsError::InvalidSpec(
            "the confidence must be strictly between 0 and 1",
        ));
    }
    let n = values.len();
    let mut draw = Resampler::new(n, spec.mean_block, spec.seed);
    let mut means: Vec<f64> = (0..spec.resamples)
        .map(|_| {
            draw.reset();
            (0..n).map(|_| values[draw.next()]).sum::<f64>() / n as f64
        })
        .collect();
    let at_most_zero = means.iter().filter(|&&m| m <= 0.0).count();
    means.sort_by(f64::total_cmp);
    let q = |p: f64| quantile(&means, p);
    Ok(MeanBootstrap {
        mean: values.iter().sum::<f64>() / n as f64,
        p_mean_at_most_zero: at_most_zero as f64 / spec.resamples as f64,
        lower_bound: q(1.0 - spec.confidence),
        quantiles: Quantiles {
            p05: q(0.05),
            p25: q(0.25),
            p50: q(0.50),
            p75: q(0.75),
            p95: q(0.95),
        },
        resamples: spec.resamples,
        mean_block: spec.mean_block,
    })
}

/// The `p` quantile of ascending `sorted` (not empty), interpolating linearly between the two
/// nearest order statistics (Hyndman and Fan's type 7, the default of R and NumPy).
fn quantile(sorted: &[f64], p: f64) -> f64 {
    let h = (sorted.len() - 1) as f64 * p;
    let lo = h.floor() as usize;
    match sorted.get(lo + 1) {
        Some(&next) => sorted[lo] + (h - lo as f64) * (next - sorted[lo]),
        None => sorted[lo],
    }
}

// ---------------------------------------------------------------------------------------------
// Bootstrap of the max drawdown
// ---------------------------------------------------------------------------------------------

/// The distribution of the max drawdown of a series of daily returns under resampling.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrawdownBootstrap {
    /// The max drawdown of the returns in the order they happened.
    pub historical: f64,
    /// The median of the resampled paths' max drawdowns.
    pub p50: f64,
    /// The 95th percentile of the resampled paths' max drawdowns. The `portfolio` kind rule asks
    /// for it to be at most its `max_resampled_drawdown`.
    pub p95: f64,
    pub resamples: usize,
    pub mean_block: usize,
}

/// Bootstraps the max drawdown of `daily_returns`, for the `portfolio` rule: each of `resamples`
/// paths draws as many returns as the sample with the stationary bootstrap of mean block length
/// `mean_block` and compounds them from an equity of 1. Days are drawn with replacement, so each
/// path has its own mix of days as well as its own order: the spread reflects both the luck of
/// the order and the uncertainty in the returns themselves. A strategy whose losses happened to
/// fall between its wins shows a shallow historical drawdown that many resampled paths do not.
pub fn bootstrap_drawdown(
    daily_returns: &[f64],
    resamples: usize,
    mean_block: usize,
    seed: u64,
) -> Result<DrawdownBootstrap, StatsError> {
    check_values(daily_returns)?;
    check_count(resamples, mean_block)?;
    let n = daily_returns.len();
    let mut draw = Resampler::new(n, mean_block, seed);
    let mut drawdowns: Vec<f64> = (0..resamples)
        .map(|_| {
            draw.reset();
            max_drawdown((0..n).map(|_| daily_returns[draw.next()]))
        })
        .collect();
    drawdowns.sort_by(f64::total_cmp);
    Ok(DrawdownBootstrap {
        historical: max_drawdown(daily_returns.iter().copied()),
        p50: quantile(&drawdowns, 0.50),
        p95: quantile(&drawdowns, 0.95),
        resamples,
        mean_block,
    })
}

/// The largest fall, as a fraction of the running peak, when `returns` are compounded from an
/// equity of 1. The peak is the highest equity so far, the start included, so a path that only
/// rises has a drawdown of 0.
fn max_drawdown(returns: impl Iterator<Item = f64>) -> f64 {
    let (mut equity, mut peak, mut worst) = (1.0f64, 1.0f64, 0.0f64);
    for r in returns {
        equity *= 1.0 + r;
        peak = peak.max(equity);
        worst = worst.max((peak - equity) / peak);
    }
    worst
}

// ---------------------------------------------------------------------------------------------
// Risk of ruin
// ---------------------------------------------------------------------------------------------

/// How [`risk_of_ruin`] simulates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RuinSpec {
    /// The drawdown from the path's running peak that counts as ruin, strictly between 0 and 1:
    /// a path is ruined once its equity is at or below `peak * (1 - ruin_drawdown)`, where the
    /// peak is the highest equity so far, the start included. 0.5 is ruin at a fall of half the
    /// account from its high, wherever that high was.
    pub ruin_drawdown: f64,
    /// Trades per path. A natural choice is the number of trades in the sample: the chance of
    /// ruin over another stretch as long as the test.
    pub horizon: usize,
    pub paths: usize,
    /// The mean block length of the stationary bootstrap over the trades; [`default_block_len`]
    /// of the number of trades is the default, and 1 draws single trades.
    pub mean_block: usize,
    pub seed: u64,
}

/// The probability of ruin for the `high_risk` kind rule: the fraction of `paths` simulated
/// equity paths that fall `ruin_drawdown` below their running peak within `horizon` trades. A
/// path that first rises 60% and then falls 50% from that high is ruined, although it is still
/// above its start: the rule's drawdown is measured from the peak.
///
/// `trade_returns` are each trade's P&L as a fraction of the equity when it was entered (the
/// caller divides the trade's P&L by that equity). A path draws `horizon` of them with the
/// stationary bootstrap, restarted for each path, and compounds them, `equity *= 1 + r`, so a
/// strategy that sizes by a fraction of equity is simulated as it trades. Trades, not daily
/// returns, because ruin is a question about a run of losing trades, and a `high_risk`
/// strategy's few large trades are what its equity moves on.
///
/// Blocks of consecutive trades keep the runs of losses that a strategy hits in a bad regime.
/// Drawing single trades would scatter those losses across the path and report a much smaller
/// chance of ruin than the strategy's own history shows; a mean block of 1 does exactly that.
///
/// shortcut: assumes one position at a time, so trades compound one after another; overlapping
/// positions would need their returns combined per day first.
pub fn risk_of_ruin(trade_returns: &[f64], spec: &RuinSpec) -> Result<f64, StatsError> {
    check_values(trade_returns)?;
    check_count(spec.paths, spec.mean_block)?;
    if !(spec.ruin_drawdown > 0.0 && spec.ruin_drawdown < 1.0) {
        return Err(StatsError::InvalidSpec(
            "the ruin drawdown must be strictly between 0 and 1",
        ));
    }
    if spec.horizon == 0 {
        return Err(StatsError::InvalidSpec("the horizon must be at least 1"));
    }
    let mut draw = Resampler::new(trade_returns.len(), spec.mean_block, spec.seed);
    let ruined_paths = (0..spec.paths)
        .filter(|_| {
            draw.reset();
            let path = (0..spec.horizon).map(|_| trade_returns[draw.next()]);
            ruined(path, spec.ruin_drawdown)
        })
        .count();
    Ok(ruined_paths as f64 / spec.paths as f64)
}

/// Whether compounding `returns` from an equity of 1 ever takes the equity to `ruin_drawdown`
/// or more below its running peak, the start included. Stops at the first such trade.
fn ruined(returns: impl Iterator<Item = f64>, ruin_drawdown: f64) -> bool {
    let keep = 1.0 - ruin_drawdown;
    let (mut equity, mut peak) = (1.0f64, 1.0f64);
    for r in returns {
        equity *= 1.0 + r;
        peak = peak.max(equity);
        if equity <= peak * keep {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------------------------
// Prop-firm challenge odds
// ---------------------------------------------------------------------------------------------

/// Where a prop firm measures its maximum drawdown from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrawdownMode {
    /// From the starting balance: breached at `start * (1 - max_drawdown)`.
    Static,
    /// From the highest daily close before the day, the start included, by the same amount:
    /// breached at `peak - max_drawdown * start`, the usual trailing rule, whose amount is set by
    /// the starting balance. [`PropFirmSpec::trailing_lock`] stops the floor rising at a level.
    ///
    /// A day's low is checked against the floor the closes before it set, and its close raises
    /// the floor only for the days after, as a firm that trails its floor on daily closes does.
    /// A close cannot fall to a floor it raised itself, and is at or above the day's low, so the
    /// low is the only check.
    ///
    /// shortcut: a firm that trails its floor on equity inside the day is not modelled: against
    /// such a firm a day that rose before it fell is judged against too low a floor, so breaches
    /// are understated. Carry each day's high once a plan's firm trails inside the day.
    Trailing,
}

/// A prop-firm challenge, in fractions of the starting balance.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PropFirmSpec {
    /// The profit target: passed once equity reaches `start * (1 + target)`. With
    /// `second_target`, the target of the first phase.
    pub target: f64,
    /// The target of a second phase, which starts from a fresh balance on the day after the
    /// first phase passes. `None` is a one-phase challenge.
    pub second_target: Option<f64>,
    /// The fewest trading days each phase holds before its target can pass it: days the traded
    /// flags mark, counted within the phase, so a day without a trade moves the balance but not
    /// this count. 0 sets no minimum.
    pub min_days: usize,
    /// A phase passes only once its largest single day's profit is at most this share of the
    /// phase's total profit. `None` sets no such rule.
    pub best_day_share: Option<f64>,
    /// The most one day counts toward a phase's target, as a share of that target: a winning
    /// day counts `min(profit, day_target_share * target)`, a losing day its whole loss, and the
    /// phase passes only once the counted profit reaches its target. The loss limits still act
    /// on the balance. Evaluation only. `None` counts every day in full.
    pub day_target_share: Option<f64>,
    /// For a trailing drawdown, the level the floor stops rising at, as a fraction of the start
    /// above the start: the floor is at most `start * (1 + trailing_lock)`. `None` lets it trail.
    pub trailing_lock: Option<f64>,
    /// The daily loss limit: breached when one day's lowest equity (its close, without daily
    /// lows) is at least `daily_loss_limit * start` under the previous day's close. Infinity
    /// turns it off.
    pub daily_loss_limit: f64,
    /// The maximum drawdown, measured as `drawdown` says. Infinity turns it off.
    pub max_drawdown: f64,
    pub drawdown: DrawdownMode,
    /// Days per path. The battery runs the paths once per horizon of the `prop_firm` rule: to its
    /// `pass_horizon_days` for the pass odds and its `breach_horizon_days` for the breach odds,
    /// both 90 by default. Both phases of a two-phase challenge share it.
    pub horizon_days: usize,
    pub paths: usize,
    /// The mean block length; [`default_block_len`] of the number of returns is the default.
    pub mean_block: usize,
    pub seed: u64,
}

impl PropFirmSpec {
    /// The floor on a day whose highest close before it is `peak`, the start included.
    fn floor(&self, peak: f64) -> f64 {
        match self.drawdown {
            DrawdownMode::Static => 1.0 - self.max_drawdown,
            DrawdownMode::Trailing => {
                let trailing = peak - self.max_drawdown;
                self.trailing_lock
                    .map_or(trailing, |lock| trailing.min(1.0 + lock))
            }
        }
    }

    /// Whether a day that started from a close of `prev` and fell to `low` at its lowest breached
    /// the daily loss limit or fell to `floor`, the floor in force when it began, whatever it
    /// closed at. `low` is the day's close when the walk has no daily lows.
    fn breached(&self, prev: f64, low: f64, floor: f64) -> bool {
        prev - low >= self.daily_loss_limit || low <= floor
    }

    fn check(&self) -> Result<(), StatsError> {
        check_count(self.paths, self.mean_block)?;
        let positive = |x: f64| x > 0.0; // false for NaN; infinity allowed
        let target = |x: f64| positive(x) && x.is_finite();
        if !(target(self.target) && self.second_target.is_none_or(target)) {
            return Err(StatsError::InvalidSpec(
                "the profit targets must be positive numbers",
            ));
        }
        if !(positive(self.daily_loss_limit) && positive(self.max_drawdown)) {
            return Err(StatsError::InvalidSpec(
                "the loss limits must be positive (infinity turns one off)",
            ));
        }
        if !self.best_day_share.is_none_or(|s| s > 0.0 && s <= 1.0) {
            return Err(StatsError::InvalidSpec(
                "the best-day share must be above 0 and at most 1",
            ));
        }
        if !self.day_target_share.is_none_or(|s| s > 0.0 && s <= 1.0) {
            return Err(StatsError::InvalidSpec(
                "the share of the target one day may count must be above 0 and at most 1",
            ));
        }
        if !self.trailing_lock.is_none_or(|l| l >= 0.0 && l.is_finite()) {
            return Err(StatsError::InvalidSpec(
                "the trailing floor's lock must be a finite number of at least 0",
            ));
        }
        if self.horizon_days == 0 {
            return Err(StatsError::InvalidSpec(
                "the horizon must be at least 1 day",
            ));
        }
        Ok(())
    }
}

/// The odds of a prop-firm challenge over simulated paths. The battery takes `pass` and `breach`
/// from runs to their own horizons, and the same run when the horizons are equal.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PropFirmOdds {
    /// The fraction of paths that pass every phase within the horizon before breaching any
    /// limit. The `prop_firm` kind rule asks for at least 0.60.
    pub pass: f64,
    /// The fraction of paths that breach a limit within the horizon, trading on after reaching
    /// the target as a funded account would. The `prop_firm` kind rule asks for at most 0.10
    /// over 90 days.
    pub breach: f64,
    pub paths: usize,
}

/// Walks one path of days, each its return and its low (see [`prop_firm_odds`]), on one balance
/// from 1, trading on after the target as a funded account would, and returns the day (counted
/// from 1) it first breached a limit, if it did within the horizon.
fn walk_path(spec: &PropFirmSpec, days: impl Iterator<Item = (f64, f64)>) -> Option<usize> {
    let (mut equity, mut peak) = (1.0f64, 1.0f64);
    for (day, (r, low)) in (1..=spec.horizon_days).zip(days) {
        let prev = equity;
        equity *= 1.0 + r;
        if spec.breached(prev, prev * (1.0 + low), spec.floor(peak)) {
            return Some(day);
        }
        peak = peak.max(equity);
    }
    None
}

/// Walks the evaluation along one path: the first phase, then the second if the spec has one,
/// each from a fresh balance of 1, with each day's return and its low (`lows`, one per return)
/// multiplied by `size`. A phase passes on the first day its target is met, its best-day rule
/// holds and it has held `spec.min_days` days that `traded` (one flag per return) marks. Returns
/// the day (counted from 1 along `returns`) the last phase passed, or `None` after a breach or
/// once `returns` run out.
///
/// With `spec.day_target_share`, the target is met by the counted profit: the balance less the
/// profit no day was allowed to count (each winning day's profit above the cap). A day that
/// crosses the target counts up to the cap like any other, so it passes the phase only if what it
/// counts is enough. Without a capped day the counted profit is the balance's profit exactly.
///
/// The limits are checked before the target each day, so a day whose low breaches and whose
/// close reaches the target is a breach: the order inside the day is unknown, and the firm ends
/// the account at the breach. Without lows no day can do both: a breach needs a losing day, and
/// a losing day before the target is reached ends below it.
///
/// shortcut: the size depends only on the phase, not on the balance or the distance to the
/// target or the floor (returns compound on the balance, as in [`walk_path`]); add a sizing
/// plan once a plan needs one.
fn walk_evaluation(
    spec: &PropFirmSpec,
    returns: &[f64],
    lows: &[f64],
    traded: &[bool],
    size: f64,
) -> Option<usize> {
    let mut later = spec.second_target;
    let mut target = spec.target;
    let (mut equity, mut peak, mut best, mut trading_days) = (1.0f64, 1.0f64, 0.0f64, 0usize);
    // The profit above the day cap, which stays in the balance but does not count.
    let mut uncounted = 0.0f64;
    for (((day, &r), &low), &t) in (1..).zip(returns).zip(lows).zip(traded) {
        let prev = equity;
        equity *= 1.0 + size * r;
        trading_days += usize::from(t);
        if spec.breached(prev, prev * (1.0 + size * low), spec.floor(peak)) {
            return None;
        }
        peak = peak.max(equity);
        best = best.max(equity - prev);
        if let Some(share) = spec.day_target_share {
            let cap = share * target;
            if equity - prev > cap {
                uncounted += equity - prev - cap;
            }
        }
        let consistent = spec
            .best_day_share
            .is_none_or(|share| best <= share * (equity - 1.0));
        if equity - uncounted >= 1.0 + target && trading_days >= spec.min_days && consistent {
            match later.take() {
                Some(next) => {
                    target = next;
                    (equity, peak, best, trading_days, uncounted) = (1.0, 1.0, 0.0, 0, 0.0);
                }
                None => return Some(day),
            }
        }
    }
    None
}

/// The odds of passing and of breaching a prop-firm challenge, from `paths` paths of
/// `horizon_days` days drawn from `daily_returns` with the stationary bootstrap. `traded` says
/// for each daily return whether the strategy traded that day, and must hold one flag per return:
/// a drawn day brings its return and its flag, and the firm's minimum counts the flags.
///
/// `lows`, when given, holds each day's lowest return inside the day: its lowest equity against
/// the previous day's close, one per return and at or under it. A drawn day brings its low with
/// its return, and the loss limits are checked on that low, so a day that dips through a limit
/// and closes back above it breaches, as a firm that watches equity at every moment counts it.
/// Without lows they are checked at each daily close, and a dip that recovers by the close is not
/// seen; lows equal to the returns give exactly that.
pub fn prop_firm_odds(
    daily_returns: &[f64],
    lows: Option<&[f64]>,
    traded: &[bool],
    spec: &PropFirmSpec,
) -> Result<PropFirmOdds, StatsError> {
    check_days(daily_returns, lows, traded)?;
    spec.check()?;
    let lows = lows.unwrap_or(daily_returns);
    let mut draw = Resampler::new(daily_returns.len(), spec.mean_block, spec.seed);
    let (mut passed, mut breached) = (0usize, 0usize);
    let mut path = Vec::with_capacity(spec.horizon_days);
    let mut path_lows = Vec::with_capacity(spec.horizon_days);
    let mut flags = Vec::with_capacity(spec.horizon_days);
    for _ in 0..spec.paths {
        draw.reset();
        // Draw the whole horizon even when the path breaches early, so path k uses the same
        // draws whatever the limits are. Each drawn day brings its return, its low and its flag.
        path.clear();
        path_lows.clear();
        flags.clear();
        for _ in 0..spec.horizon_days {
            let i = draw.next();
            path.push(daily_returns[i]);
            path_lows.push(lows[i]);
            flags.push(traded[i]);
        }
        passed += usize::from(walk_evaluation(spec, &path, &path_lows, &flags, 1.0).is_some());
        let days = path.iter().copied().zip(path_lows.iter().copied());
        breached += usize::from(walk_path(spec, days).is_some());
    }
    Ok(PropFirmOdds {
        pass: passed as f64 / spec.paths as f64,
        breach: breached as f64 / spec.paths as f64,
        paths: spec.paths,
    })
}

// ---------------------------------------------------------------------------------------------
// The value of a prop-firm attempt
// ---------------------------------------------------------------------------------------------

/// What one attempt is worth, from one series of daily returns.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AttemptFigures {
    /// The fraction of attempts that pass the evaluation at the terms' evaluation size.
    pub pass: f64,
    /// The mean paid to the trader per funded account, after the split, in the account currency.
    /// `None` when no attempt passed.
    pub payouts_per_funded: Option<f64>,
    /// The fraction of attempts that end with at least one payout.
    pub payout_chance: f64,
    /// The value of one attempt in the account currency: the mean over attempts of the payouts
    /// received plus any refunded fee, less the fees paid.
    pub value: f64,
}

/// The value of a prop-firm attempt at the edge the test shows and at a reduced one.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PropFirmAttempt {
    /// From the daily returns as they are.
    pub full: AttemptFigures,
    /// From the daily returns with their mean scaled by `reduced_edge_factor` and their shape
    /// unchanged. The `prop_firm` kind rule asks for its `value` to be above zero.
    pub reduced: AttemptFigures,
    pub reduced_edge_factor: f64,
    pub paths: usize,
}

/// What one funded account did.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Funded {
    /// The profit paid out, before the split, as a fraction of the starting balance.
    gross: f64,
    payouts: usize,
    /// The balance the account ended on: at its breach, or at the horizon.
    balance: f64,
}

/// Walks a funded account along `returns`, from a fresh balance of 1 under the challenge's loss
/// limits, with each day's return and its low (`lows`, checked against the limits as in
/// [`walk_evaluation`]) multiplied by the terms' funded size. The first payout waits
/// until the account has held `days_to_first_payout` days that `traded` marks; each later one
/// waits `payout_interval_days` days of the series, traded or not. A payout is asked for at
/// the close of every day the terms allow one, for all the profit above the starting balance, or
/// above the floor in force if that is higher, up to the cap, and is made when the trader's share
/// reaches the smallest payout. The amount leaves the balance and moves no floor: a trailing
/// floor keeps its peak. With `payout_locks_floor`, the first payout fixes the floor where it
/// stands, or at the start if that is higher. The account ends at a breach or when `returns` run
/// out.
///
/// shortcut: the funded account keeps the evaluation's loss limits; give it its own once a firm
/// in a plan sets different ones.
///
/// shortcut: a firm that lowers its floor by the amount withdrawn is not modelled, and the model
/// is conservative for it, giving less room to lose after a payout; make it a firm rule once a
/// plan's firm does that.
///
/// shortcut: payouts are always asked for as early and as large as allowed, so with a floor at or
/// above the start a payout that takes all the profit leaves the account on its floor, and its
/// next day at or below that floor ends it. Add a payout plan once a plan needs to keep a buffer.
fn walk_funded(
    spec: &PropFirmSpec,
    terms: &AttemptTerms,
    returns: &[f64],
    lows: &[f64],
    traded: &[bool],
) -> Funded {
    let first = terms.days_to_first_payout as usize;
    let interval = terms.payout_interval_days as usize;
    let most = terms.max_payouts.map_or(usize::MAX, |m| m as usize);
    let mut out = Funded {
        gross: 0.0,
        payouts: 0,
        balance: 1.0,
    };
    // The floor a payout fixed, with `payout_locks_floor`.
    let (mut peak, mut fixed, mut last) = (1.0f64, None::<f64>, None::<usize>);
    let mut trading_days = 0usize;
    for (((day, &r), &low), &t) in (1..).zip(returns).zip(lows).zip(traded) {
        trading_days += usize::from(t);
        let prev = out.balance;
        out.balance *= 1.0 + terms.funded_size * r;
        let floor = fixed.unwrap_or_else(|| spec.floor(peak));
        if spec.breached(prev, prev * (1.0 + terms.funded_size * low), floor) {
            break;
        }
        peak = peak.max(out.balance);
        let due = match last {
            None => trading_days >= first,
            Some(last) => day - last >= interval,
        };
        if !due || out.payouts >= most {
            continue;
        }
        // The floor in force after today's close, or the start if that is higher. A payout takes
        // at most the balance above this level (a cap only takes less) and moves neither the
        // peak nor a fixed floor, so the floor in force after it is at most this level: the floor
        // as it was, or fixed at this level when the payout locks it. The balance is left at or
        // above that floor, static or trailing, locked or not.
        let floor = fixed.unwrap_or_else(|| spec.floor(peak)).max(1.0);
        let profit = out.balance - floor;
        let amount = terms.payout_cap.map_or(profit, |cap| profit.min(cap));
        if amount > 0.0 && terms.profit_split * amount * terms.account_size >= terms.min_payout {
            out.balance -= amount;
            out.gross += amount;
            out.payouts += 1;
            last = Some(day);
            if terms.payout_locks_floor {
                fixed = Some(floor);
            }
        }
    }
    out
}

/// One attempt along one path: the evaluation over the first `spec.horizon_days` returns, then,
/// after a pass, the funded account over the `funded_horizon_days` returns from the day after.
/// `returns` holds both horizons, `lows` a low and `traded` a flag for each of its days. `None`
/// when the evaluation did not pass.
fn walk_attempt(
    spec: &PropFirmSpec,
    terms: &AttemptTerms,
    returns: &[f64],
    lows: &[f64],
    traded: &[bool],
) -> Option<Funded> {
    let h = spec.horizon_days;
    let size = terms.evaluation_size;
    let day = walk_evaluation(spec, &returns[..h], &lows[..h], &traded[..h], size)?;
    let end = day + terms.funded_horizon_days as usize;
    Some(walk_funded(
        spec,
        terms,
        &returns[day..end],
        &lows[day..end],
        &traded[day..end],
    ))
}

/// Running sums over attempts, in the account currency.
#[derive(Debug, Default)]
struct Tally {
    attempts: usize,
    passed: usize,
    paid_out: usize,
    /// Paid to the trader, after the split.
    payouts: f64,
    value: f64,
}

impl Tally {
    fn add(&mut self, terms: &AttemptTerms, attempt: Option<Funded>) {
        self.attempts += 1;
        let mut value = -terms.evaluation_fee;
        if let Some(funded) = attempt {
            self.passed += 1;
            let paid = terms.profit_split * funded.gross * terms.account_size;
            self.payouts += paid;
            value += paid - terms.pass_fee;
            if funded.payouts > 0 {
                self.paid_out += 1;
                if terms.refund_fee_with_first_payout {
                    value += terms.evaluation_fee;
                }
            }
        }
        self.value += value;
    }

    fn figures(&self) -> AttemptFigures {
        let n = self.attempts as f64;
        AttemptFigures {
            pass: self.passed as f64 / n,
            payouts_per_funded: (self.passed > 0).then(|| self.payouts / self.passed as f64),
            payout_chance: self.paid_out as f64 / n,
            value: self.value / n,
        }
    }
}

/// The value of one attempt at a prop-firm challenge, over `spec.paths` paths drawn from
/// `daily_returns`, their `lows` and their `traded` flags with the stationary bootstrap of
/// [`prop_firm_odds`], read as it reads them: the evaluation under `spec` (to its
/// `horizon_days`), then a funded account under `terms` after a pass, both checked against the
/// loss limits on each day's low when `lows` is given and at each daily close when it is not.
///
/// The second set of figures subtracts `(1 - reduced_edge_factor) * mean` from every daily
/// return and from its low before the size multiplier, so the mean becomes
/// `reduced_edge_factor * mean` and the days keep their shape, their dips and their flags. Both
/// sets use the same draws, so they differ by the edge alone.
pub fn prop_firm_attempt(
    daily_returns: &[f64],
    lows: Option<&[f64]>,
    traded: &[bool],
    spec: &PropFirmSpec,
    terms: &AttemptTerms,
) -> Result<PropFirmAttempt, StatsError> {
    check_days(daily_returns, lows, traded)?;
    spec.check()?;
    if terms.validate().is_err() {
        return Err(StatsError::InvalidSpec(
            "the attempt terms are out of range",
        ));
    }
    let n = daily_returns.len();
    let mean = daily_returns.iter().sum::<f64>() / n as f64;
    let shift = (1.0 - terms.reduced_edge_factor) * mean;
    let lows = lows.unwrap_or(daily_returns);
    let reduced: Vec<f64> = daily_returns.iter().map(|r| r - shift).collect();
    let reduced_lows: Vec<f64> = lows.iter().map(|l| l - shift).collect();
    let days = spec.horizon_days + terms.funded_horizon_days as usize;
    let mut draw = Resampler::new(n, spec.mean_block, spec.seed);
    let (mut full, mut low) = (Tally::default(), Tally::default());
    let mut path = Vec::with_capacity(days);
    let mut flags = Vec::with_capacity(days);
    let mut returns = Vec::with_capacity(days);
    let mut path_lows = Vec::with_capacity(days);
    for _ in 0..spec.paths {
        draw.reset();
        // Draw both horizons whatever happens on the path, so path k uses the same draws
        // whatever the limits and the terms are. Each drawn day brings its return and its low,
        // from either series, and its flag, which the reduced edge leaves as it is.
        path.clear();
        path.extend((0..days).map(|_| draw.next()));
        flags.clear();
        flags.extend(path.iter().map(|&i| traded[i]));
        for (series, series_lows, tally) in [
            (daily_returns, lows, &mut full),
            (&reduced[..], &reduced_lows[..], &mut low),
        ] {
            returns.clear();
            returns.extend(path.iter().map(|&i| series[i]));
            path_lows.clear();
            path_lows.extend(path.iter().map(|&i| series_lows[i]));
            tally.add(
                terms,
                walk_attempt(spec, terms, &returns, &path_lows, &flags),
            );
        }
    }
    Ok(PropFirmAttempt {
        full: full.figures(),
        reduced: low.figures(),
        reduced_edge_factor: terms.reduced_edge_factor,
        paths: spec.paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 101 whole numbers from -50 to 50 in a scrambled order, times 2^-13 (about 0.006):
    /// symmetric around zero with a sample mean of exactly 0, and every sum of them exact.
    fn symmetric() -> Vec<f64> {
        (0..101)
            .map(|i| ((i * 37) % 101 - 50) as f64 / 8192.0)
            .collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1e-300)
    }

    #[test]
    fn the_default_block_length_is_the_integer_cube_root() {
        let cases = [
            (0, 1),
            (1, 1),
            (7, 1),
            (8, 2),
            (26, 2),
            (27, 3),
            (365, 7),
            (511, 7),
            (512, 8),
            (730, 9),
        ];
        for (n, root) in cases {
            assert_eq!(default_block_len(n), root, "n = {n}");
        }
        assert!(default_block_len(usize::MAX) > 2_000_000);
    }

    #[test]
    fn a_constant_series_has_every_resampled_mean_on_one_side_of_zero() {
        for (value, p) in [(0.003, 0.0), (-0.003, 1.0), (0.0, 1.0)] {
            let b = bootstrap_mean(&[value; 50], &BootstrapSpec::daily(50, 9)).unwrap();
            assert_eq!(b.p_mean_at_most_zero, p, "value {value}");
            assert_eq!(b.mean_block, 3);
        }
    }

    #[test]
    fn a_symmetric_zero_mean_series_is_at_or_below_zero_about_half_the_time() {
        let x = symmetric();
        let b = bootstrap_mean(&x, &BootstrapSpec::daily(x.len(), 2026)).unwrap();
        assert_eq!(b.mean, 0.0);
        assert_eq!(b.mean_block, 4);
        // With 2,000 resamples the estimate's standard error is about 0.011.
        assert!((b.p_mean_at_most_zero - 0.5).abs() < 0.035, "{b:?}");
        // Pinned for seed 2026, so a change in the resampling shows up here.
        assert_eq!(b.p_mean_at_most_zero, PINNED_SYMMETRIC_P);
        // The quantiles are ordered and roughly symmetric around zero.
        let q = b.quantiles;
        assert!(q.p05 < q.p25 && q.p25 <= q.p50 && q.p50 <= q.p75 && q.p75 < q.p95);
        assert!((q.p05 + q.p95).abs() < 0.2 * q.p95, "{q:?}");
        // 1 - 0.95 is not exactly 0.05, so the bound can differ from p05 in the last bits.
        assert!(close(b.lower_bound, q.p05), "{b:?}");
    }

    /// 988 of 2,000 resamples.
    const PINNED_SYMMETRIC_P: f64 = 0.494;

    #[test]
    fn the_same_seed_gives_identical_output_and_another_seed_differs() {
        let x = symmetric();
        let a = bootstrap_mean(&x, &BootstrapSpec::daily(x.len(), 1)).unwrap();
        let again = bootstrap_mean(&x, &BootstrapSpec::daily(x.len(), 1)).unwrap();
        let other = bootstrap_mean(&x, &BootstrapSpec::daily(x.len(), 2)).unwrap();
        assert_eq!(a, again);
        assert_ne!(a.quantiles, other.quantiles);

        let trades: Vec<f64> = x.iter().map(|r| r * 10.0).collect();
        let ruin = |seed| {
            let spec = RuinSpec {
                ruin_drawdown: 0.1,
                horizon: 30,
                paths: 500,
                mean_block: default_block_len(trades.len()),
                seed,
            };
            risk_of_ruin(&trades, &spec).unwrap()
        };
        assert_eq!(ruin(1), ruin(1));
        assert_ne!(ruin(1), ruin(2));
    }

    #[test]
    fn the_lower_bound_tracks_the_confidence() {
        let x: Vec<f64> = symmetric().iter().map(|r| r + 0.001).collect();
        let at = |confidence| {
            let spec = BootstrapSpec {
                confidence,
                ..BootstrapSpec::trades(5)
            };
            bootstrap_mean(&x, &spec).unwrap()
        };
        let (b95, b75) = (at(0.95), at(0.75));
        assert!(close(b95.lower_bound, b95.quantiles.p05));
        assert_eq!(b75.lower_bound, b75.quantiles.p25);
        assert!(b95.lower_bound < b75.lower_bound);
        assert_eq!(b95.mean_block, 1);
    }

    #[test]
    fn the_quantile_interpolates_between_order_statistics() {
        let s = [1.0, 2.0, 4.0, 8.0, 16.0];
        assert_eq!(quantile(&s, 0.0), 1.0);
        assert_eq!(quantile(&s, 1.0), 16.0);
        assert_eq!(quantile(&s, 0.5), 4.0);
        // h = 4 * 0.125 = 0.5 -> 1 + 0.5 * (2 - 1); h = 4 * 0.875 = 3.5 -> 8 + 0.5 * 8.
        assert_eq!(quantile(&s, 0.125), 1.5);
        assert_eq!(quantile(&s, 0.875), 12.0);
        assert_eq!(quantile(&[3.0], 0.05), 3.0);
    }

    #[test]
    fn bad_inputs_are_errors() {
        let d = BootstrapSpec::daily(3, 0);
        assert_eq!(bootstrap_mean(&[], &d), Err(StatsError::NoData));
        assert_eq!(
            bootstrap_mean(&[1.0, f64::NAN], &d),
            Err(StatsError::NonFinite { index: 1 })
        );
        for spec in [
            BootstrapSpec { resamples: 0, ..d },
            BootstrapSpec { mean_block: 0, ..d },
            BootstrapSpec {
                confidence: 1.0,
                ..d
            },
            BootstrapSpec {
                confidence: f64::NAN,
                ..d
            },
        ] {
            assert!(matches!(
                bootstrap_mean(&[1.0], &spec),
                Err(StatsError::InvalidSpec(_))
            ));
        }
    }

    fn drawdowns(values: &[f64], seed: u64) -> DrawdownBootstrap {
        let block = default_block_len(values.len());
        bootstrap_drawdown(values, DEFAULT_RESAMPLES, block, seed).unwrap()
    }

    #[test]
    fn returns_that_never_lose_have_no_drawdown() {
        let b = drawdowns(&[0.01, 0.0, 0.02, 0.005, 0.03, 0.0, 0.01, 0.04], 1);
        assert_eq!((b.historical, b.p50, b.p95), (0.0, 0.0, 0.0));
        assert_eq!((b.resamples, b.mean_block), (DEFAULT_RESAMPLES, 2));
    }

    #[test]
    fn the_drawdown_is_measured_from_the_running_peak_not_from_the_start() {
        // +100% then -50%: equity 2, then 1. Back at the start, but half the peak.
        assert_eq!(max_drawdown([1.0, -0.5].into_iter()), 0.5);
        // The peak is the highest equity so far: 1.5, 0.75, 0.9375 is half of 1.5 at its worst.
        assert_eq!(max_drawdown([0.5, -0.5, 0.25].into_iter()), 0.5);
        // The start counts as a peak: a first day of -25% is a drawdown of 0.25.
        assert_eq!(max_drawdown([-0.25, 0.1].into_iter()), 0.25);
        // Two falls from the same peak compound: 1.0 * 0.5 * 0.5 is 0.25, a drawdown of 0.75.
        assert_eq!(max_drawdown([-0.5, -0.5].into_iter()), 0.75);
        // A single return gives that drawdown on every resample.
        let b = drawdowns(&[-0.125], 4);
        assert_eq!((b.historical, b.p50, b.p95), (0.125, 0.125, 0.125));
    }

    #[test]
    fn the_drawdown_bootstrap_is_the_same_for_a_seed_and_differs_for_another() {
        let x: Vec<f64> = symmetric().iter().map(|r| r * 4.0).collect();
        let a = drawdowns(&x, 1);
        assert_eq!(a, drawdowns(&x, 1));
        let other = drawdowns(&x, 2);
        assert_ne!((a.p50, a.p95), (other.p50, other.p95));
        assert_eq!(a.historical, other.historical);
        assert!(a.p50 <= a.p95, "{a:?}");
    }

    #[test]
    fn a_lucky_order_of_losses_shows_a_much_deeper_resampled_drawdown() {
        // Wins of +3% and losses of -2.5%, alternating, so no loss ever follows another: the
        // historical drawdown is one loss, 0.025. Reordered in blocks, the losses meet.
        let x: Vec<f64> = (0..60)
            .map(|i| if i % 2 == 0 { 0.03 } else { -0.025 })
            .collect();
        let b = drawdowns(&x, 7);
        assert!(close(b.historical, 0.025), "{b:?}");
        assert!(b.p95 > 3.0 * b.historical, "{b:?}");
        assert!(b.historical < b.p50 && b.p50 < b.p95, "{b:?}");
        assert_eq!((b.p50, b.p95), PINNED_LUCKY_DRAWDOWN);
    }

    /// Seed 7, 2,000 resamples of 60 days in blocks of mean 3: a median of 0.073, three losses
    /// in a row, and a 95th percentile of 0.137, more than five times the historical 0.025.
    const PINNED_LUCKY_DRAWDOWN: (f64, f64) = (0.07314062500000014, 0.13705950234342723);

    #[test]
    fn bad_inputs_to_the_drawdown_bootstrap_are_errors() {
        assert_eq!(bootstrap_drawdown(&[], 10, 1, 0), Err(StatsError::NoData));
        assert_eq!(
            bootstrap_drawdown(&[0.01, f64::INFINITY], 10, 1, 0),
            Err(StatsError::NonFinite { index: 1 })
        );
        for (resamples, block) in [(0, 1), (10, 0)] {
            assert!(matches!(
                bootstrap_drawdown(&[0.01], resamples, block, 0),
                Err(StatsError::InvalidSpec(_))
            ));
        }
    }

    fn ruin_spec(horizon: usize) -> RuinSpec {
        RuinSpec {
            ruin_drawdown: 0.5,
            horizon,
            paths: 200,
            mean_block: 1,
            seed: 3,
        }
    }

    #[test]
    fn risk_of_ruin_is_zero_when_every_trade_wins_and_one_when_each_loses_past_the_level() {
        assert_eq!(risk_of_ruin(&[0.01, 0.2, 0.05], &ruin_spec(100)), Ok(0.0));
        // One trade of -60% takes equity from 1 to 0.4, a drawdown past 0.5 from the start.
        assert_eq!(risk_of_ruin(&[-0.6, -0.7], &ruin_spec(1)), Ok(1.0));
    }

    #[test]
    fn ruin_counts_from_the_trade_that_crosses_the_level() {
        // Losing 10% a trade: 0.9^6 = 0.531 is above 0.5 and 0.9^7 = 0.478 is below.
        assert_eq!(risk_of_ruin(&[-0.1], &ruin_spec(6)), Ok(0.0));
        assert_eq!(risk_of_ruin(&[-0.1], &ruin_spec(7)), Ok(1.0));
        // Landing exactly on the level is ruin: 1 * (1 - 0.5) is 0.5, exact in binary.
        assert_eq!(risk_of_ruin(&[-0.5], &ruin_spec(1)), Ok(1.0));
    }

    #[test]
    fn ruin_is_a_drawdown_from_the_running_peak_not_from_the_start() {
        // +60% then -50%: equity 1.6, then 0.8. That is half of the peak of 1.6, so ruin at a
        // drawdown of 0.5, though still above the start's 0.5 floor.
        let (rise, fall) = (0.6, -0.5);
        assert!((1.0 + rise) * (1.0 + fall) > 0.5);
        assert!(ruined([rise, fall].into_iter(), 0.5));
        // A fall of 40% from the same peak, to 0.96, is not.
        assert!(!ruined([rise, -0.4].into_iter(), 0.5));
        // The peak is the highest equity so far, not the last one: +60%, -20%, -40% is 1.6,
        // 1.28, then 0.768, below half the 1.6 high (0.8); with -35% last it is 0.832, above.
        assert!(ruined([rise, -0.2, -0.4].into_iter(), 0.5));
        assert!(!ruined([rise, -0.2, -0.35].into_iter(), 0.5));
    }

    #[test]
    fn risk_of_ruin_is_pinned_for_a_mixed_series() {
        // Wins of +10% and losses of -10% in equal numbers: ruin at 0.5 needs a run of losses.
        let x = [0.1, -0.1, 0.1, -0.1, 0.1, -0.1];
        // The default block length for 6 trades is 1, so this draws single trades.
        let spec = RuinSpec {
            paths: 2_000,
            mean_block: default_block_len(x.len()),
            ..ruin_spec(50)
        };
        assert_eq!(spec.mean_block, 1);
        let p = risk_of_ruin(&x, &spec).unwrap();
        assert!(p > 0.0 && p < 1.0, "{p}");
        assert_eq!(p, PINNED_RUIN);
    }

    /// 1,271 of 2,000 paths. Measured from the start it was 807 (0.4035): a path that first
    /// climbs has further to fall before the start's floor than before half its peak.
    const PINNED_RUIN: f64 = 0.6355;

    #[test]
    fn blocks_keep_a_run_of_losses_that_single_draws_scatter() {
        // A bad regime of 8 trades at -10% (0.9^8 = 0.430 of the high before it, past half the
        // account), then 92 trades at +2%. Replaying this history from each of the 100 starting
        // trades, with the wrap, ruins 95 of them: the 92 starts on a win, which meet the whole
        // run after a high, and the starts on the first, second and last loss, whose run is
        // split by the wrap into pieces of which one still holds 7 losses (0.9^7 = 0.478). The
        // other five starts split it into pieces of at most 6 (0.9^6 = 0.531).
        let mut x = vec![-0.1; 8];
        x.extend([0.02; 92]);
        let ruined_starts = (0..x.len())
            .filter(|&start| ruined((0..x.len()).map(|k| x[(start + k) % x.len()]), 0.5))
            .count();
        assert_eq!(ruined_starts, 95);
        let spec = RuinSpec {
            paths: 2_000,
            mean_block: default_block_len(x.len()),
            ..ruin_spec(100)
        };
        assert_eq!(spec.mean_block, 4);
        let blocks = risk_of_ruin(&x, &spec).unwrap();
        let single = risk_of_ruin(
            &x,
            &RuinSpec {
                mean_block: 1,
                ..spec
            },
        )
        .unwrap();
        // Blocks of mean length 4 carry enough of the run to pass #35's 5% gate, short of the
        // whole history's 95%; single draws scatter it.
        assert!(blocks > 0.05 && blocks < 0.95, "{blocks}");
        assert!(single < 0.01, "{single}");
        assert_eq!((blocks, single), PINNED_CLUSTERED_RUIN);
    }

    /// 624 and 3 of 2,000 paths, seed 3. Measured from the start they were 180 and 1 (0.09 and
    /// 0.0005).
    const PINNED_CLUSTERED_RUIN: (f64, f64) = (0.312, 0.0015);

    fn challenge(horizon_days: usize) -> PropFirmSpec {
        PropFirmSpec {
            target: 0.10,
            second_target: None,
            min_days: 0,
            best_day_share: None,
            day_target_share: None,
            trailing_lock: None,
            daily_loss_limit: 0.05,
            max_drawdown: 0.10,
            drawdown: DrawdownMode::Static,
            horizon_days,
            paths: 300,
            mean_block: 3,
            seed: 11,
        }
    }

    /// A 10% target with both loss limits off.
    fn unlimited(horizon_days: usize) -> PropFirmSpec {
        PropFirmSpec {
            daily_loss_limit: f64::INFINITY,
            max_drawdown: f64::INFINITY,
            ..challenge(horizon_days)
        }
    }

    #[test]
    fn a_challenge_that_gains_every_day_passes_and_one_that_breaches_on_day_one_fails() {
        // +1% a day: 1.01^9 = 1.0937 is short of the 10% target and 1.01^10 = 1.1046 reaches it.
        let gains = [0.01; 20];
        let odds = prop_firm_odds(&gains, None, &[true; 20], &challenge(10)).unwrap();
        assert_eq!((odds.pass, odds.breach), (1.0, 0.0));
        let odds = prop_firm_odds(&gains, None, &[true; 20], &challenge(9)).unwrap();
        assert_eq!((odds.pass, odds.breach), (0.0, 0.0));
        // -6% on the first day breaches the 5% daily loss limit.
        let odds = prop_firm_odds(&[-0.06, -0.07], None, &[true; 2], &challenge(90)).unwrap();
        assert_eq!((odds.pass, odds.breach), (0.0, 1.0));
    }

    /// The day `path` breaches on one balance.
    fn walk(spec: &PropFirmSpec, path: &[f64]) -> Option<usize> {
        walk_path(spec, path.iter().map(|&r| (r, r)))
    }

    /// The day `path` passes the evaluation at the size it was tested at.
    fn passes(spec: &PropFirmSpec, path: &[f64]) -> Option<usize> {
        walk_evaluation(spec, path, path, &every_day(path), 1.0)
    }

    /// A flag per day of `x`, each saying the day traded: every day counts as a trading day.
    fn every_day(x: &[f64]) -> Vec<bool> {
        vec![true; x.len()]
    }

    #[test]
    fn the_daily_loss_limit_and_both_drawdown_modes_break_where_worked_by_hand() {
        let off = unlimited(10);
        // Daily loss: from 1.1, -4% loses 0.044 and -4.8% loses 0.0528, measured against a limit
        // of 0.05 of the starting balance. Against the day's opening equity, -4.8% would be under
        // a 5% limit and hold.
        let daily = PropFirmSpec {
            daily_loss_limit: 0.05,
            ..off
        };
        assert_eq!(walk(&daily, &[0.1, -0.04]), None);
        assert_eq!(walk(&daily, &[0.1, -0.048]), Some(2));
        // A loss of exactly the limit breaches: 1 - 0.875 is 0.125, all exact in binary.
        let eighth = PropFirmSpec {
            daily_loss_limit: 0.125,
            ..off
        };
        assert_eq!(walk(&eighth, &[-0.125]), Some(1));
        // 1.1 then -8%: 1.012. Static floor 0.9 holds; trailing floor 1.1 - 0.1 = 1.0 holds.
        // 1.1 then -10%: 0.99. Static holds; trailing breaches.
        let fixed = PropFirmSpec {
            max_drawdown: 0.1,
            ..off
        };
        let trailing = PropFirmSpec {
            drawdown: DrawdownMode::Trailing,
            ..fixed
        };
        assert_eq!(walk(&fixed, &[0.1, -0.08]), None);
        assert_eq!(walk(&trailing, &[0.1, -0.08]), None);
        assert_eq!(walk(&fixed, &[0.1, -0.1]), None);
        assert_eq!(walk(&trailing, &[0.1, -0.1]), Some(2));
        // Static: three days of -4% reach 0.8847, below 0.9, on day 3.
        assert_eq!(walk(&fixed, &[-0.04, -0.04, -0.04]), Some(3));
        // Landing exactly on the static floor breaches: 1 - 0.25 is 0.75, exact in binary.
        let quarter = PropFirmSpec {
            max_drawdown: 0.25,
            ..off
        };
        assert_eq!(walk(&quarter, &[-0.25]), Some(1));
        // The trailing floor sits max_drawdown of the starting balance below the peak, not a
        // fraction of the peak: from a peak of 1.2, -9% is 1.092, under 1.2 - 0.1 = 1.1 but
        // above 1.2 * 0.9 = 1.08.
        assert_eq!(walk(&trailing, &[0.2, -0.09]), Some(2));
        // The path trades on after the target: day 1 reaches 1.1, day 2 breaches the trailing
        // floor. That is a pass on day 1, because the target came first, and a breach on day 2.
        assert_eq!(passes(&trailing, &[0.1, -0.1]), Some(1));
        assert_eq!(walk(&trailing, &[0.1, -0.1]), Some(2));
        // Breaching on day 1 before reaching the target on day 2 is no pass.
        assert_eq!(walk(&trailing, &[-0.15, 0.4]), Some(1));
        assert_eq!(passes(&trailing, &[-0.15, 0.4]), None);
    }

    /// The day `path` breaches on one balance, with each day's low from `lows`.
    fn walk_lows(spec: &PropFirmSpec, path: &[f64], lows: &[f64]) -> Option<usize> {
        walk_path(spec, path.iter().copied().zip(lows.iter().copied()))
    }

    /// The day `path` passes the evaluation, with each day's low from `lows`.
    fn passes_lows(spec: &PropFirmSpec, path: &[f64], lows: &[f64]) -> Option<usize> {
        walk_evaluation(spec, path, lows, &every_day(path), 1.0)
    }

    #[test]
    fn a_dip_inside_the_day_breaches_the_daily_loss_limit_its_close_passes() {
        let daily = PropFirmSpec {
            daily_loss_limit: 0.03,
            ..unlimited(10)
        };
        // Day 2 closes flat but falls 4% inside the day. 1.05 * 1.05 = 1.1025 reaches the 10%
        // target on day 3.
        let path = [0.05, 0.0, 0.05];
        let lows = [0.05, -0.04, 0.05];
        assert_eq!(passes(&daily, &path), Some(3));
        assert_eq!(walk(&daily, &path), None);
        assert_eq!(passes_lows(&daily, &path, &lows), None);
        assert_eq!(walk_lows(&daily, &path, &lows), Some(2));
        // The same day as the whole history: no path breaches at closes, every path with lows.
        let at_closes = prop_firm_odds(&[0.0], None, &[true], &daily).unwrap();
        let inside = prop_firm_odds(&[0.0], Some(&[-0.04]), &[true], &daily).unwrap();
        assert_eq!((at_closes.pass, at_closes.breach), (0.0, 0.0));
        assert_eq!((inside.pass, inside.breach), (0.0, 1.0));
        // A dip of 2% stays inside the limit.
        assert_eq!(passes_lows(&daily, &path, &[0.05, -0.02, 0.05]), Some(3));
        // A day whose low breaches and whose close reaches the target is a breach, not a pass:
        // day 2 closes at 1.05 * 1.05 = 1.1025 but falls to 1.05 * 0.96 = 1.008 inside the day,
        // 0.042 under day 1's close.
        assert_eq!(passes(&daily, &[0.05, 0.05]), Some(2));
        assert_eq!(passes_lows(&daily, &[0.05, 0.05], &[0.05, -0.04]), None);
        // The low is scaled with the size, as the return is: at half size two days of +10% close
        // at 1.05 and 1.1025, and a 4% dip on day 2 is 2% from 1.05, 0.021 of the start, inside
        // the limit; an 8% dip is 0.042 and breaches.
        let half = |lows: &[f64]| walk_evaluation(&daily, &[0.1; 2], lows, &[true; 2], 0.5);
        assert_eq!(half(&[0.1, -0.04]), Some(2));
        assert_eq!(half(&[0.1, -0.08]), None);
    }

    #[test]
    fn a_dip_inside_the_day_breaches_the_floor_its_close_stays_above() {
        let fixed = PropFirmSpec {
            max_drawdown: 0.1,
            ..unlimited(10)
        };
        // Day 2 closes flat at 1.05 but falls to 1.05 * 0.85 = 0.8925 inside the day, under the
        // static floor of 0.9.
        let path = [0.05, 0.0, 0.05];
        let lows = [0.05, -0.15, 0.05];
        assert_eq!(passes(&fixed, &path), Some(3));
        assert_eq!(walk(&fixed, &path), None);
        assert_eq!(passes_lows(&fixed, &path, &lows), None);
        assert_eq!(walk_lows(&fixed, &path, &lows), Some(2));
        // 1.05 * 0.86 = 0.903 stays above it.
        assert_eq!(walk_lows(&fixed, &path, &[0.05, -0.14, 0.05]), None);
    }

    /// A day's low is checked against the trailing floor the closes before it set, and its close
    /// raises the floor only for the days after, in the breach walk, the evaluation and the
    /// funded account alike.
    #[test]
    fn a_days_low_is_checked_against_the_trailing_floor_the_closes_before_it_set() {
        // Day 1 closes at 1.5, which puts the floor at 1.5 - 0.375 = 1.125 from day 2. Day 1's
        // own low of 0.75 is under that, but above the 0.625 the start set, and holds. Day 2's
        // low of 1.5 * 0.75 = 1.125 lands on the floor day 1 set and breaches. All exact in
        // binary.
        let trailing = PropFirmSpec {
            target: 1.0,
            max_drawdown: 0.375,
            drawdown: DrawdownMode::Trailing,
            ..unlimited(10)
        };
        let path = [0.5, 0.0, 0.5];
        let holds = [-0.25, 0.0, 0.5];
        let breaches = [-0.25, -0.25, 0.5];
        assert_eq!(walk_lows(&trailing, &path, &holds), None);
        assert_eq!(walk_lows(&trailing, &path, &breaches), Some(2));
        // 1.5 * 1.5 = 2.25 reaches the 100% target on day 3.
        assert_eq!(passes_lows(&trailing, &path, &holds), Some(3));
        assert_eq!(passes_lows(&trailing, &path, &breaches), None);
        // A funded account first paid on day 3 takes the 2.25 - 1.875 = 0.375 above the floor
        // day 3's close sets, or ends on day 2 with nothing paid.
        let third = AttemptTerms {
            days_to_first_payout: 3,
            ..terms()
        };
        let held = walk_funded(&trailing, &third, &path, &holds, &every_day(&path));
        assert_eq!((held.payouts, held.gross, held.balance), (1, 0.375, 1.875));
        let ended = walk_funded(&trailing, &third, &path, &breaches, &every_day(&path));
        assert_eq!((ended.payouts, ended.gross, ended.balance), (0, 0.0, 1.5));
        // A lock at the start stops the floor at 1.0 however high the closes: day 2's low of
        // 1.125 holds, and one of 1.5 * 0.625 = 0.9375 breaches.
        let locked = PropFirmSpec {
            trailing_lock: Some(0.0),
            ..trailing
        };
        assert_eq!(walk_lows(&locked, &path, &breaches), None);
        assert_eq!(walk_lows(&locked, &path, &[-0.25, -0.375, 0.5]), Some(2));
    }

    #[test]
    fn a_funded_account_that_breaches_inside_the_day_pays_nothing_after_it() {
        let daily = PropFirmSpec {
            daily_loss_limit: 0.03,
            ..unlimited(10)
        };
        // A payout a day: day 1 pays the 0.25 of profit, day 2 is flat, days 3 and 4 pay again.
        let path = [0.25, 0.0, 0.25, 0.25];
        let at_closes = funded(&daily, &terms(), &path);
        assert_eq!((at_closes.payouts, at_closes.gross), (3, 0.75));
        // Day 2 falls 4% inside the day: the account ends there, after its first payout.
        let lows = [0.25, -0.04, 0.25, 0.25];
        let inside = walk_funded(&daily, &terms(), &path, &lows, &every_day(&path));
        assert_eq!(
            (inside.payouts, inside.gross, inside.balance),
            (1, 0.25, 1.0)
        );
        // The funded size scales the low: at half size the dip is 2% and the account trades on.
        let half = AttemptTerms {
            funded_size: 0.5,
            ..terms()
        };
        let halved = walk_funded(&daily, &half, &path, &lows, &every_day(&path));
        assert_eq!(halved.payouts, 3);
        // A breach day pays nothing even when its close would be paid out: at closes every day
        // of +25% pays, and with day 2 falling 4% inside the day the account ends at its close of
        // 1.25, after day 1's payout alone.
        let gains = [0.25; 4];
        let paid = funded(&daily, &terms(), &gains);
        assert_eq!((paid.payouts, paid.gross), (4, 1.0));
        let ended = walk_funded(&daily, &terms(), &gains, &lows, &every_day(&gains));
        assert_eq!((ended.payouts, ended.gross, ended.balance), (1, 0.25, 1.25));
    }

    #[test]
    fn lows_equal_to_the_returns_give_the_results_at_closes() {
        let x: Vec<f64> = symmetric().iter().map(|r| r * 6.0 + 0.003).collect();
        let flags: Vec<bool> = (0..x.len()).map(|i| i % 3 != 0).collect();
        let trailing = PropFirmSpec {
            drawdown: DrawdownMode::Trailing,
            ..challenge(90)
        };
        let specs = [
            challenge(90),
            trailing,
            PropFirmSpec {
                trailing_lock: Some(0.02),
                ..trailing
            },
            PropFirmSpec {
                second_target: Some(0.05),
                min_days: 4,
                best_day_share: Some(0.5),
                day_target_share: Some(0.3),
                ..challenge(90)
            },
        ];
        let lock = AttemptTerms {
            payout_locks_floor: true,
            evaluation_size: 0.5,
            funded_size: 2.0,
            ..terms()
        };
        for spec in specs {
            let odds = prop_firm_odds(&x, Some(&x), &flags, &spec).unwrap();
            assert_eq!(odds, prop_firm_odds(&x, None, &flags, &spec).unwrap());
            for terms in [terms(), lock] {
                let attempt = prop_firm_attempt(&x, Some(&x), &flags, &spec, &terms).unwrap();
                let closes = prop_firm_attempt(&x, None, &flags, &spec, &terms).unwrap();
                assert_eq!(attempt, closes, "{spec:?} {terms:?}");
            }
        }
        // And the pinned odds hold with them.
        let odds = prop_firm_odds(&x, Some(&x), &every_day(&x), &challenge(90)).unwrap();
        assert_eq!((odds.pass, odds.breach), PINNED_PROP);
    }

    /// A day's low comes with its own return. A flat day that dips 4% under a 3% daily limit, drawn
    /// beside a day of +1%, breaches exactly where a day that closes 4% down would: the paths are
    /// the same draws, and up to the first such day both histories are the same. Were a low drawn
    /// apart from its return, the +1% day would sometimes carry the dip and the odds would differ.
    #[test]
    fn a_day_is_drawn_with_its_own_low() {
        let spec = PropFirmSpec {
            target: 0.02,
            daily_loss_limit: 0.03,
            mean_block: 1,
            ..unlimited(4)
        };
        let flags = [true; 2];
        let inside = prop_firm_odds(&[0.01, 0.0], Some(&[0.01, -0.04]), &flags, &spec).unwrap();
        let closed = prop_firm_odds(&[0.01, -0.04], None, &flags, &spec).unwrap();
        assert_eq!(inside, closed);
        assert!(inside.breach > 0.5 && inside.breach < 1.0, "{inside:?}");
        assert!(inside.pass > 0.0 && inside.pass < 0.5, "{inside:?}");
        let terms = AttemptTerms {
            funded_horizon_days: 4,
            ..terms()
        };
        let inside =
            prop_firm_attempt(&[0.01, 0.0], Some(&[0.01, -0.04]), &flags, &spec, &terms).unwrap();
        let closed = prop_firm_attempt(&[0.01, -0.04], None, &flags, &spec, &terms).unwrap();
        // The reduced figures shift each series by its own mean, which differs here.
        assert_eq!(inside.full, closed.full);
        assert!(inside.full.payout_chance > 0.0, "{inside:?}");
    }

    #[test]
    fn daily_lows_must_match_their_returns() {
        let spec = challenge(5);
        let odds = |lows: &[f64]| prop_firm_odds(&[0.01, 0.0], Some(lows), &[true; 2], &spec);
        assert_eq!(
            odds(&[0.01]),
            Err(StatsError::InvalidSpec(
                "there must be one daily low per daily return"
            ))
        );
        assert_eq!(
            odds(&[0.0, f64::NAN]),
            Err(StatsError::NonFinite { index: 1 })
        );
        assert_eq!(
            odds(&[0.02, 0.0]),
            Err(StatsError::InvalidSpec(
                "a day's low cannot be above its close"
            ))
        );
        assert!(odds(&[0.01, 0.0]).is_ok());
        let attempt = prop_firm_attempt(&[0.01], Some(&[0.02]), &[true], &spec, &terms());
        assert!(attempt.is_err());
    }

    #[test]
    fn prop_firm_odds_are_pinned_for_a_mixed_series() {
        let x: Vec<f64> = symmetric().iter().map(|r| r * 6.0 + 0.003).collect();
        let odds = prop_firm_odds(&x, None, &every_day(&x), &challenge(90)).unwrap();
        assert_eq!((odds.pass, odds.breach), PINNED_PROP);
        let again = prop_firm_odds(&x, None, &every_day(&x), &challenge(90)).unwrap();
        assert_eq!(odds, again);
    }

    /// 277 and 27 of 300 paths, the values `main` gave before the new evaluation rules: a spec
    /// with every new rule off is judged exactly as before.
    const PINNED_PROP: (f64, f64) = (277.0 / 300.0, 27.0 / 300.0);

    #[test]
    fn prop_firm_specs_out_of_range_are_errors() {
        let bad = [
            PropFirmSpec {
                target: 0.0,
                ..challenge(5)
            },
            PropFirmSpec {
                second_target: Some(0.0),
                ..challenge(5)
            },
            PropFirmSpec {
                second_target: Some(f64::INFINITY),
                ..challenge(5)
            },
            PropFirmSpec {
                daily_loss_limit: -0.1,
                ..challenge(5)
            },
            PropFirmSpec {
                max_drawdown: f64::NAN,
                ..challenge(5)
            },
            PropFirmSpec {
                best_day_share: Some(1.5),
                ..challenge(5)
            },
            PropFirmSpec {
                day_target_share: Some(0.0),
                ..challenge(5)
            },
            PropFirmSpec {
                day_target_share: Some(1.5),
                ..challenge(5)
            },
            PropFirmSpec {
                trailing_lock: Some(-0.1),
                ..challenge(5)
            },
            challenge(0),
            PropFirmSpec {
                paths: 0,
                ..challenge(5)
            },
        ];
        for spec in bad {
            assert!(matches!(
                prop_firm_odds(&[0.01], None, &[true], &spec),
                Err(StatsError::InvalidSpec(_))
            ));
            assert!(matches!(
                prop_firm_attempt(&[0.01], None, &[true], &spec, &terms()),
                Err(StatsError::InvalidSpec(_))
            ));
        }
        let bad_terms = AttemptTerms {
            profit_split: 0.0,
            ..terms()
        };
        assert!(matches!(
            prop_firm_attempt(&[0.01], None, &[true], &challenge(5), &bad_terms),
            Err(StatsError::InvalidSpec(_))
        ));
        assert_eq!(
            prop_firm_attempt(&[], None, &[], &challenge(5), &terms()),
            Err(StatsError::NoData)
        );
        // One traded flag per daily return, or an error.
        assert!(matches!(
            prop_firm_odds(&[0.01, 0.02], None, &[true], &challenge(5)),
            Err(StatsError::InvalidSpec(_))
        ));
        assert!(matches!(
            prop_firm_attempt(&[0.01], None, &[true, false], &challenge(5), &terms()),
            Err(StatsError::InvalidSpec(_))
        ));
    }

    #[test]
    fn a_pass_counts_only_from_the_minimum_day() {
        let min3 = PropFirmSpec {
            min_days: 3,
            max_drawdown: 0.10,
            ..unlimited(10)
        };
        // Day 1 closes at 1.125, past the 1.10 target. With no minimum it passes on day 1; with a
        // minimum of 3 days it passes on day 3, the minimum day itself, as the balance holds.
        let early = [0.125, 0.0, 0.0, 0.0];
        assert_eq!(passes(&unlimited(10), &early), Some(1));
        assert_eq!(passes(&min3, &early), Some(3));
        // Falling through the static floor before then is a breach, not a pass: day 2 closes at
        // 1.125 * 0.75 = 0.84375, below the 0.9 floor.
        assert_eq!(passes(&min3, &[0.125, -0.25, 0.5, 0.0]), None);
        assert_eq!(walk(&min3, &[0.125, -0.25]), Some(2));
    }

    #[test]
    fn a_phase_counts_only_its_trading_days_toward_the_minimum() {
        // A 5% target, at least 5 trading days, a 30-day horizon, no loss limits. The strategy
        // makes +3% on days 10 and 20 and nothing on any other day: 1.03 after day 10, short of
        // 1.05, and 1.03 * 1.03 = 1.0609 from day 20 on, past it.
        let spec = PropFirmSpec {
            target: 0.05,
            min_days: 5,
            ..unlimited(30)
        };
        let mut path = [0.0; 30];
        (path[9], path[19]) = (0.03, 0.03);
        let trading_on = |days: &[usize]| {
            let mut traded = [false; 30];
            for &day in days {
                traded[day - 1] = true;
            }
            walk_evaluation(&spec, &path, &path, &traded, 1.0)
        };
        // Counting every day as a trading day, day 20 is past the minimum and passes.
        assert_eq!(passes(&spec, &path), Some(20));
        // Trading on days 10 and 20 alone, the phase has 2 trading days by day 20 and no more by
        // day 30: no pass.
        assert_eq!(trading_on(&[10, 20]), None);
        // Three flat trading days before day 20 make day 20 the fifth: it passes on the target.
        assert_eq!(trading_on(&[2, 4, 6, 10, 20]), Some(20));
        // With the three after day 20, the pass waits for the fifth trading day, day 29, where
        // the balance still holds 1.0609; with only two of them it never comes.
        assert_eq!(trading_on(&[10, 20, 23, 26, 29]), Some(29));
        assert_eq!(trading_on(&[10, 20, 23, 26]), None);

        // Each phase counts its own trading days. Two phases, 12.5% then 6.25%, at least 2
        // trading days each, a trade every day but day 4. All exact in binary:
        //   day 1  +12.5%  1.125, phase 1's target, but 1 trading day
        //   day 2   0      1.125 and 2 trading days: phase 1 passes; phase 2 starts from 1
        //   day 3  +6.25%  1.0625, phase 2's target, but 1 trading day in phase 2
        //   day 4   0      no trade: still 1
        //   day 5   0      2 trading days: phase 2 passes
        // Had phase 1's trading days carried over, phase 2 would pass on day 3.
        let two = PropFirmSpec {
            target: 0.125,
            second_target: Some(0.0625),
            min_days: 2,
            ..unlimited(10)
        };
        let path = [0.125, 0.0, 0.0625, 0.0, 0.0];
        let traded = [true, true, true, false, true];
        assert_eq!(walk_evaluation(&two, &path, &path, &traded, 1.0), Some(5));
    }

    #[test]
    fn the_best_day_rule_waits_until_later_profit_shrinks_the_big_day() {
        let half = PropFirmSpec {
            best_day_share: Some(0.5),
            ..unlimited(10)
        };
        // Day 1 makes 0.125, the whole profit, so its share is 1, above 0.5. Every later day
        // gains 1/32 of the balance, all exact in binary:
        //   day 2: 1.125 * 33/32 = 1.16015625, profit 0.16015625, half 0.0801 < 0.125
        //   day 3: * 33/32 = 1.1964111328125, profit 0.1964, half 0.0982 < 0.125
        //   day 4: * 33/32 = 1.233798980712890625, profit 0.2338, half 0.1169 < 0.125
        //   day 5: * 33/32 = 1.272355198860168457, profit 0.2724, half 0.1362 >= 0.125
        // Each later day's gain is under 0.04, so day 1 stays the best day, and the pass comes on
        // day 5. With no rule, day 1's 1.125 already passes the 1.10 target.
        let path = [0.125, 0.03125, 0.03125, 0.03125, 0.03125, 0.03125];
        assert_eq!(passes(&unlimited(10), &path), Some(1));
        assert_eq!(passes(&half, &path), Some(5));
        assert_eq!(passes(&half, &path[..4]), None);
    }

    #[test]
    fn the_day_cap_changes_nothing_while_no_day_makes_more_than_it() {
        // symmetric() plus 0.002 a day: no day moves more than 0.0082, and a phase ends by the day
        // its balance first reaches its target, so no day makes more than 0.0082 * 1.11 = 0.0091.
        // A 0.2 share caps a day at 0.02 of the 10% target and 0.01 of the second phase's 5%, so
        // nothing is capped and the odds come out exactly as with no cap.
        let returns: Vec<f64> = symmetric().iter().map(|r| r + 0.002).collect();
        let flags = every_day(&returns);
        for spec in [
            challenge(90),
            PropFirmSpec {
                second_target: Some(0.05),
                ..challenge(90)
            },
        ] {
            let off = prop_firm_odds(&returns, None, &flags, &spec).unwrap();
            let capped = PropFirmSpec {
                day_target_share: Some(0.2),
                ..spec
            };
            assert_eq!(
                prop_firm_odds(&returns, None, &flags, &capped).unwrap(),
                off
            );
            assert!(off.pass > 0.0, "{off:?}");
            // A cap that binds holds passes back.
            let tight = PropFirmSpec {
                day_target_share: Some(0.02),
                ..spec
            };
            assert!(prop_firm_odds(&returns, None, &flags, &tight).unwrap().pass < off.pass);
        }
    }

    #[test]
    fn a_day_cap_of_the_whole_target_passes_in_one_day_and_of_half_needs_a_second_winning_day() {
        // A 12.5% target; day 1 makes 25%. All exact in binary.
        let target = PropFirmSpec {
            target: 0.125,
            ..unlimited(10)
        };
        let cap = |share| PropFirmSpec {
            day_target_share: Some(share),
            ..target
        };
        let path = [0.25, 0.0, 0.0, 0.25];
        assert_eq!(passes(&target, &path), Some(1));
        // A share of 1: day 1 counts 0.125, the whole target.
        assert_eq!(passes(&cap(1.0), &path), Some(1));
        // A share of 0.5: day 1 counts 0.0625, the flat days nothing, and day 4 the other 0.0625.
        assert_eq!(passes(&cap(0.5), &path), Some(4));
        assert_eq!(passes(&cap(0.5), &path[..3]), None);
        // A losing day counts in full. Day 2 loses 1/16 of 1.25, 0.078125, so after day 2 the
        // counted profit is 0.0625 - 0.078125 = -0.015625; each later +25% day counts 0.0625:
        // 0.046875, 0.109375 and 0.171875, so the pass comes on day 5, not on day 3 as it would if
        // the loss did not count.
        let loss = [0.25, -0.0625, 0.25, 0.25, 0.25];
        assert_eq!(passes(&cap(0.5), &loss), Some(5));
        assert_eq!(passes(&cap(0.5), &loss[..4]), None);
    }

    #[test]
    fn the_day_cap_counts_less_than_the_balance_and_stands_apart_from_the_best_day_rule() {
        // +6%, +2%, +2% against a 10% target: the balance reaches 1.102824 on day 3. With a cap of
        // 0.4 of the target, day 1 counts 0.04 and the two others their 0.0212 and 0.021624, 0.0828
        // in all: not yet passed, whatever the best day's share.
        let cap = PropFirmSpec {
            day_target_share: Some(0.4),
            ..unlimited(10)
        };
        let path = [0.06, 0.02, 0.02, 0.02];
        assert_eq!(passes(&unlimited(10), &path[..3]), Some(3));
        assert_eq!(passes(&cap, &path[..3]), None);
        // Another +2% counts 0.0221: 0.1049 passes. The best-day rule at 0.4 still holds the pass
        // back (0.4 of the 0.1249 profit is under day 1's 0.06), so with both set, neither passes.
        assert_eq!(passes(&cap, &path), Some(4));
        let best = PropFirmSpec {
            best_day_share: Some(0.4),
            ..unlimited(10)
        };
        assert_eq!(passes(&best, &path), None);
        let both = PropFirmSpec {
            best_day_share: Some(0.4),
            ..cap
        };
        assert_eq!(passes(&both, &path), None);
    }

    #[test]
    fn each_phase_counts_from_zero_against_its_own_target() {
        // 12.5% then 6.25%, each day capped at half its phase's target. All exact in binary:
        //   days 1-2  +25% each   count 0.0625 each: phase 1 passes on day 2
        //   day 3     +6.25%      1.0625, counts 0.03125 of phase 2's 0.0625
        //   day 4     +6.25%      1.12890625, counts another 0.03125: phase 2 passes
        // With phase 1's cap, day 3 would pass phase 2; with phase 1's uncounted profit carried
        // over, phase 2 would never pass.
        let two = PropFirmSpec {
            target: 0.125,
            second_target: Some(0.0625),
            day_target_share: Some(0.5),
            ..unlimited(10)
        };
        assert_eq!(passes(&two, &[0.25, 0.25, 0.0625, 0.0625]), Some(4));
        assert_eq!(passes(&two, &[0.25, 0.25, 0.0625]), None);
    }

    #[test]
    fn two_phases_need_both_targets_and_the_second_starts_from_a_fresh_balance() {
        let two = PropFirmSpec {
            second_target: Some(0.05),
            max_drawdown: 0.10,
            ..unlimited(10)
        };
        // Day 1 passes phase 1 at 1.125. Phase 2 starts at 1: day 2's +6.25% closes at 1.0625,
        // past its 1.05 target.
        assert_eq!(passes(&two, &[0.125, 0.0625]), Some(2));
        // A pass needs both targets: phase 1 alone passes the one-phase challenge on day 1.
        let flat = [0.125, 0.0, 0.0];
        assert_eq!(passes(&unlimited(10), &flat), Some(1));
        assert_eq!(passes(&two, &flat), None);
        // Phase 2 counts from a fresh balance of 1, not from phase 1's 1.125: +3.125% closes at
        // 1.03125, short of 1.05 (1.125 * 1.03125 = 1.16 would pass), and -12.5% closes at
        // 0.875, through the 0.9 floor (1.125 * 0.875 = 0.984 would hold).
        assert_eq!(passes(&two, &[0.125, 0.03125]), None);
        assert_eq!(passes(&two, &[0.125, -0.125, 0.5]), None);
        // A day can pass only one phase: the day that passes phase 1 does not count for phase 2.
        assert_eq!(passes(&two, &[0.25]), None);
    }

    #[test]
    fn a_locked_trailing_floor_rises_and_then_holds_at_its_level() {
        let trailing = PropFirmSpec {
            max_drawdown: 0.1,
            drawdown: DrawdownMode::Trailing,
            ..unlimited(10)
        };
        let at_start = PropFirmSpec {
            trailing_lock: Some(0.0),
            ..trailing
        };
        // The floor trails the peak by 0.1 until it reaches the start, then holds there.
        assert_eq!(at_start.floor(1.0), 1.0 - 0.1);
        assert!(close(at_start.floor(1.05), 0.95));
        assert_eq!(at_start.floor(1.1), 1.0);
        assert_eq!(at_start.floor(1.25), 1.0);
        assert_eq!(trailing.floor(1.25), 1.25 - 0.1);
        // Rising: from a peak of 1.05, -10% closes at 0.945, under the 0.95 floor.
        assert_eq!(walk(&at_start, &[0.05, -0.1]), Some(2));
        // Held: from a peak of 1.25, -12.5% closes at 1.09375. The floor trailing for ever is at
        // 1.15 and breaches; locked at the start it is at 1.0 and holds, until a second -12.5%
        // closes at 0.95703125.
        assert_eq!(walk(&trailing, &[0.25, -0.125]), Some(2));
        assert_eq!(walk(&at_start, &[0.25, -0.125]), None);
        assert_eq!(walk(&at_start, &[0.25, -0.125, -0.125]), Some(3));
        // Locked 5% above the start, the floor holds at 1.05: 1.09375 * 0.9375 = 1.025390625
        // breaches it, though it is above the start.
        let above = PropFirmSpec {
            trailing_lock: Some(0.05),
            ..trailing
        };
        assert_eq!(walk(&above, &[0.25, -0.125, -0.0625]), Some(3));
        assert_eq!(walk(&at_start, &[0.25, -0.125, -0.0625]), None);
    }

    /// Neutral attempt terms: a payout a day from day 1, nothing capped, no fees but the
    /// evaluation's, sizes as tested.
    fn terms() -> AttemptTerms {
        AttemptTerms {
            account_size: 100_000.0,
            evaluation_fee: 500.0,
            pass_fee: 0.0,
            refund_fee_with_first_payout: false,
            profit_split: 0.75,
            days_to_first_payout: 1,
            payout_interval_days: 1,
            min_payout: 0.0,
            payout_cap: None,
            max_payouts: None,
            payout_locks_floor: false,
            funded_horizon_days: 10,
            evaluation_size: 1.0,
            funded_size: 1.0,
            reduced_edge_factor: 0.5,
        }
    }

    /// Zero-mean daily returns of one fixed step `u`, drawn one at a time, under a static floor
    /// with no daily limit.
    ///
    /// The returns compound, so after `a` steps up and `b` down the balance is
    /// `(1 + u)^a (1 - u)^b`, whose log is `k u - (a + b) u^2 / 2` to second order, with
    /// `k = a - b` the net steps. The target and the floor sit half a step off the lattice, at
    /// `1 + 5.5 u` and `1 - 3.5 u`, so the drift of `u^2 / 2` per day cannot carry a lattice
    /// point across either: with `u = 2^-10` it stays under `0.2 u` over 400 days. The walk
    /// passes at `k = 6` and breaches at `k = -4`, and a symmetric walk from 0 between those
    /// reaches 6 first with probability `room / (room + target) = 4 / 10 = 0.4`.
    fn ruin_set_up() -> (Vec<f64>, PropFirmSpec, AttemptTerms) {
        let u = 1.0 / 1024.0;
        let spec = PropFirmSpec {
            target: 5.5 * u,
            max_drawdown: 3.5 * u,
            // A walk between barriers 10 steps apart ends within 400 days except with a chance
            // of about 0.951^400 (the largest eigenvalue of the walk, cos(pi / 10)), 2e-9.
            horizon_days: 400,
            paths: 20_000,
            mean_block: 1,
            seed: 172,
            ..unlimited(400)
        };
        let terms = AttemptTerms {
            funded_horizon_days: 1,
            ..terms()
        };
        (vec![u, -u], spec, terms)
    }

    /// Four standard errors of a probability near 0.4 estimated from 20,000 paths:
    /// `4 * sqrt(0.4 * 0.6 / 20000)` = 0.0139. A fixed seed makes the check deterministic; the
    /// width says how far the estimate may sit from the exact 0.4 by chance alone.
    const RUIN_TOLERANCE: f64 = 0.014;

    #[test]
    fn zero_mean_steps_pass_with_the_odds_of_the_gamblers_ruin() {
        let (x, spec, terms) = ruin_set_up();
        let a = prop_firm_attempt(&x, None, &every_day(&x), &spec, &terms).unwrap();
        assert!((a.full.pass - 0.4).abs() < RUIN_TOLERANCE, "{a:?}");
        // The series has a mean of exactly 0, so the reduced edge changes nothing.
        assert_eq!(a.full.pass, a.reduced.pass);
    }

    #[test]
    fn doubling_the_evaluation_size_doubles_each_days_return() {
        // Doubled, each step is 2u and the levels at 5.5u and 3.5u sit between 4u and 6u and
        // between 2u and 4u: the walk passes at 3 steps of 2u and breaches at 2, so the odds
        // stay 2 / (2 + 3) = 0.4. The drift of (2u)^2 / 2 a day stays under the 0.5u margin for
        // the first 256 days, which a walk between barriers 5 steps apart outlives with a chance
        // of about cos(pi / 5)^256 = 0.809^256, 4e-24.
        let (x, spec, terms) = ruin_set_up();
        let doubled = AttemptTerms {
            evaluation_size: 2.0,
            ..terms
        };
        let a = prop_firm_attempt(&x, None, &every_day(&x), &spec, &doubled).unwrap();
        assert!((a.full.pass - 0.4).abs() < RUIN_TOLERANCE, "{a:?}");
        // On a constructed path the multiplier applies to every day: +1% a day reaches the 10%
        // target on day 10 (1.01^9 = 1.0937, 1.01^10 = 1.1046), and +2% a day on day 5
        // (1.02^4 = 1.0824, 1.02^5 = 1.1041).
        let spec = unlimited(20);
        assert_eq!(
            walk_evaluation(&spec, &[0.01; 20], &[0.01; 20], &[true; 20], 1.0),
            Some(10)
        );
        assert_eq!(
            walk_evaluation(&spec, &[0.01; 20], &[0.01; 20], &[true; 20], 2.0),
            Some(5)
        );
        // And to losses: -3% a day, doubled to -6%, breaches a 5% daily limit on day 1.
        let daily = PropFirmSpec {
            daily_loss_limit: 0.05,
            ..spec
        };
        // At size 1, day 1's -3% holds and day 2's +25% closes at 0.97 * 1.25 = 1.2125, a pass.
        let down = [-0.03, 0.25];
        assert_eq!(
            walk_evaluation(&daily, &down, &down, &[true; 2], 1.0),
            Some(2)
        );
        assert_eq!(walk_evaluation(&daily, &down, &down, &[true; 2], 2.0), None);
    }

    fn funded(spec: &PropFirmSpec, terms: &AttemptTerms, path: &[f64]) -> Funded {
        walk_funded(spec, terms, path, path, &every_day(path))
    }

    #[test]
    fn funded_payouts_follow_each_rule_to_the_cent() {
        let floor_75 = PropFirmSpec {
            max_drawdown: 0.25,
            ..unlimited(10)
        };
        let rules = AttemptTerms {
            days_to_first_payout: 3,
            payout_interval_days: 2,
            min_payout: 1_000.0,
            payout_cap: Some(0.0625),
            max_payouts: Some(3),
            ..terms()
        };
        // Balances at each close, the floor at 0.75, all exact in binary:
        //   day 1  +25%      1.25         before the first payout day
        //   day 2   0        1.25         before the first payout day
        //   day 3   0        1.25         first payout day: 0.25 of profit, capped at 0.0625
        //                    1.1875
        //   day 4   0        1.1875       1 day after the payout, under the 2-day interval
        //   day 5   0        1.1875       2 days after: pays the cap again
        //                    1.125
        //   day 6  -10.9375% 1.001953125  (9/8 * 57/64 = 513/512), 1 day after the payout
        //   day 7   0        1.001953125  due, but 0.75 of 1/512 of 100,000 is 146.48, under
        //                                 the smallest payout of 1,000
        //   day 8  +25%      1.25244140625 (513/512 * 5/4): pays the cap, the third payout
        //                    1.18994140625
        //   day 9   0        1 day after the payout
        //   day 10  0        due again, but the account has made its 3 payouts
        //   day 11 -50%      0.594970703125, through the floor: the account ends
        //   day 12 +25%      never traded
        let path = [
            0.25, 0.0, 0.0, 0.0, 0.0, -0.109375, 0.0, 0.25, 0.0, 0.0, -0.5, 0.25,
        ];
        let f = funded(&floor_75, &rules, &path);
        assert_eq!(f.payouts, 3);
        assert_eq!(f.gross, 3.0 * 0.0625);
        assert_eq!(f.balance, 0.594970703125);
        // After the 75% split, on 100,000: 0.75 * 0.1875 * 100,000 = 14,062.50.
        assert_eq!(rules.profit_split * f.gross * rules.account_size, 14_062.5);

        // A payout locks the floor at the start: day 1 pays out the whole 0.25 of profit, which
        // leaves 1.0, and day 2's -6.25% closes at 0.9375, through the locked floor of 1.0. The
        // unlocked floor of 0.75 holds, and day 3's +25% (0.9375 * 1.25 = 1.171875) pays again.
        let lock = AttemptTerms {
            payout_locks_floor: true,
            ..terms()
        };
        let path = [0.25, -0.0625, 0.25];
        let locked = funded(&floor_75, &lock, &path);
        assert_eq!(
            (locked.payouts, locked.gross, locked.balance),
            (1, 0.25, 0.9375)
        );
        let unlocked = funded(&floor_75, &terms(), &path);
        assert_eq!(
            (unlocked.payouts, unlocked.gross, unlocked.balance),
            (2, 0.25 + 0.171875, 1.0)
        );
    }

    #[test]
    fn the_first_payout_waits_for_trading_days_and_the_next_for_calendar_days() {
        // The floor at 0.75, the first payout after 3 trading days, the next 2 days after it, no
        // cap. The account trades on days 1, 4 and 6 only. All exact in binary:
        //   day 1  +25%  trade  1.25, 1 trading day
        //   day 2   0           1.25
        //   day 3   0           1.25, the third day but 1 trading day: no payout yet
        //   day 4   0    trade  2 trading days
        //   day 5   0           still 2
        //   day 6   0    trade  3 trading days: pays the 0.25 of profit, 1.0
        //   day 7  +25%         1.25, 1 day after the payout
        //   day 8   0           2 days after: pays 0.25, 1.0, on a day with no trade, as the
        //                       interval counts calendar days
        let floor_75 = PropFirmSpec {
            max_drawdown: 0.25,
            ..unlimited(10)
        };
        let rules = AttemptTerms {
            days_to_first_payout: 3,
            payout_interval_days: 2,
            ..terms()
        };
        let path = [0.25, 0.0, 0.0, 0.0, 0.0, 0.0, 0.25, 0.0];
        let traded = [true, false, false, true, false, true, false, false];
        let after = |days: usize| {
            let f = walk_funded(
                &floor_75,
                &rules,
                &path[..days],
                &path[..days],
                &traded[..days],
            );
            (f.payouts, f.gross, f.balance)
        };
        assert_eq!(after(5), (0, 0.0, 1.25));
        assert_eq!(after(6), (1, 0.25, 1.0));
        assert_eq!(after(7), (1, 0.25, 1.25));
        assert_eq!(after(8), (2, 0.5, 1.0));
        // Counting every day as a trading day, the first payout comes on day 3.
        let f = funded(&floor_75, &rules, &path[..5]);
        assert_eq!((f.payouts, f.gross, f.balance), (1, 0.25, 1.0));
    }

    #[test]
    fn a_payout_never_moves_the_floor() {
        // A trailing drawdown of 0.125, no lock, payouts from day 1, no cap. All exact in binary:
        //   day 1  +25%     1.25, peak 1.25: the floor is 1.25 - 0.125 = 1.125. The payout takes
        //                   the profit above it, 0.125, which leaves 1.125; the peak stays 1.25.
        //   day 2  -6.25%   1.125 * 0.9375 = 1.0546875, at or below the floor still at 1.125:
        //                   the account ends, and day 3 is never traded.
        // Had the payout taken all 0.25 of profit and lowered the peak by as much, to 1.0, the
        // floor would be 0.875, day 2's 0.9375 would hold, and day 3's +25% would pay again.
        let trailing = PropFirmSpec {
            max_drawdown: 0.125,
            drawdown: DrawdownMode::Trailing,
            ..unlimited(10)
        };
        let f = funded(&trailing, &terms(), &[0.25, -0.0625, 0.25]);
        assert_eq!((f.payouts, f.gross, f.balance), (1, 0.125, 1.0546875));
    }

    #[test]
    fn a_payout_does_not_unlock_a_trailing_floor_that_reached_its_lock() {
        // A trailing drawdown of 0.1 whose floor locks at the start, payouts from day 1, no cap:
        //   day 1  +12%  1.12, peak 1.12: the floor is min(1.12 - 0.1, 1.0) = 1.0, at its lock.
        //                The payout takes the 0.12 of profit above it: 1.0; the peak stays 1.12.
        //   day 2  -5%   0.95, at or below the floor held at 1.0: the account ends.
        // Had the payout unlocked the floor, it would be min(1.0 - 0.1, 1.0) = 0.9 on day 2,
        // 0.95 would hold, and day 3's +10% (0.95 * 1.1 = 1.045) would pay a second time.
        let locks = PropFirmSpec {
            max_drawdown: 0.1,
            drawdown: DrawdownMode::Trailing,
            trailing_lock: Some(0.0),
            ..unlimited(10)
        };
        let f = funded(&locks, &terms(), &[0.12, -0.05, 0.1]);
        assert_eq!(f.payouts, 1);
        assert!(close(f.gross, 0.12) && close(f.balance, 0.95), "{f:?}");
    }

    #[test]
    fn a_payout_that_locks_the_floor_never_lowers_a_floor_above_the_start() {
        // A trailing drawdown of 0.04, no trailing lock, payouts capped at 0.02, and the first
        // payout locks the floor:
        //   day 1  +10%  1.10, peak 1.10, so the trailing floor stands at 1.10 - 0.04 = 1.06,
        //                above the start. The profit above it, 0.04, is over the cap: the payout
        //                takes the cap, 1.08, and the lock fixes the floor at the higher of 1.0
        //                and 1.06.
        //   day 2  -5%   1.08 * 0.95 = 1.026, at or below 1.06: the account ends.
        // A floor locked at the start instead would hold 1.026 above 1.0 and pay the cap again
        // on day 2. With the lock or without it the account ends the same: a payout never makes
        // it safer.
        let trailing = PropFirmSpec {
            max_drawdown: 0.04,
            drawdown: DrawdownMode::Trailing,
            ..unlimited(10)
        };
        let capped = AttemptTerms {
            payout_cap: Some(0.02),
            ..terms()
        };
        let lock = AttemptTerms {
            payout_locks_floor: true,
            ..capped
        };
        let path = [0.1, -0.05];
        let locked = funded(&trailing, &lock, &path);
        assert_eq!(locked.payouts, 1);
        assert!(
            close(locked.gross, 0.02) && close(locked.balance, 1.026),
            "{locked:?}"
        );
        assert_eq!(locked, funded(&trailing, &capped, &path));
    }

    #[test]
    fn a_payout_never_takes_the_balance_under_a_floor_locked_above_the_start() {
        // A trailing drawdown of 0.1 whose floor locks 0.05 above the start, payouts from day 1,
        // no cap:
        //   day 1  +20%  1.2, peak 1.2: the floor is min(1.2 - 0.1, 1.05) = 1.05, at its lock.
        //                The payout takes the profit above that floor, 1.2 - 1.05 = 0.15: 1.05.
        //   day 2  +1%   1.05 * 1.01 = 1.0605, above 1.05: pays 1.0605 - 1.05 = 0.0105, 1.05.
        //   day 3  +1%   the same again: 0.0105, 1.05.
        // In all 0.15 + 0.0105 + 0.0105 = 0.171. Measured from the start, day 1's payout would
        // take 0.2 and leave 1.0 under the floor, and day 2's 1.01 would end the account.
        let locks = PropFirmSpec {
            max_drawdown: 0.1,
            drawdown: DrawdownMode::Trailing,
            trailing_lock: Some(0.05),
            ..unlimited(10)
        };
        let f = funded(&locks, &terms(), &[0.2, 0.01, 0.01]);
        assert_eq!(f.payouts, 3);
        assert!(close(f.gross, 0.171) && close(f.balance, 1.05), "{f:?}");
    }

    #[test]
    fn a_capped_payout_never_takes_the_balance_under_the_floor_a_payout_locked() {
        // A trailing drawdown of 0.125, payouts capped at 0.0625, and the first payout locks the
        // floor. All exact in binary:
        //   day 1  +25%      1.25, peak 1.25, so the floor stands at 1.25 - 0.125 = 1.125. The
        //                    profit above it, 0.125, is over the cap: the payout takes the cap,
        //                    1.1875, and the lock fixes the floor at the higher of 1.0 and 1.125.
        //   day 2  -6.25%    1.1875 * 0.9375 = 1.11328125, at or below 1.125: the account ends,
        //                    and day 3 is never traded.
        // Had the payout lowered the peak by the cap, to 1.1875, the lock would fix the floor at
        // 1.0625, day 2 would hold and pay its 0.05078125 above that floor, and day 3 would pay
        // again.
        let trailing = PropFirmSpec {
            max_drawdown: 0.125,
            drawdown: DrawdownMode::Trailing,
            ..unlimited(10)
        };
        let lock = AttemptTerms {
            payout_cap: Some(0.0625),
            payout_locks_floor: true,
            ..terms()
        };
        let f = funded(&trailing, &lock, &[0.25, -0.0625, 0.0078125]);
        assert_eq!((f.payouts, f.gross, f.balance), (1, 0.0625, 1.11328125));
    }

    /// The figures of one attempt per path, from constructed paths.
    fn figures(spec: &PropFirmSpec, terms: &AttemptTerms, paths: &[[f64; 4]]) -> AttemptFigures {
        let mut tally = Tally::default();
        for path in paths {
            tally.add(terms, walk_attempt(spec, terms, path, path, &[true; 4]));
        }
        tally.figures()
    }

    #[test]
    fn the_value_is_payouts_plus_any_refund_less_the_fees() {
        // A 25% target over a 2-day evaluation, the floor at 0.75, then 2 funded days. Each path
        // holds both horizons; after a pass on day 1 the funded days are the path's 2nd and 3rd.
        let spec = PropFirmSpec {
            target: 0.25,
            max_drawdown: 0.25,
            ..unlimited(2)
        };
        let fees = AttemptTerms {
            pass_fee: 100.0,
            funded_horizon_days: 2,
            ..terms()
        };
        let paths = [
            // Passes on day 1; funded: flat, then +50%, a payout of 0.5 of the balance.
            [0.25, 0.0, 0.5, 0.0],
            // Passes on day 1; funded: flat, then -50% through the floor, with no payout.
            [0.25, 0.0, -0.5, 0.0],
            // Breaches on day 1 and never passes.
            [-0.5, 0.0, 0.0, 0.0],
        ];
        // Paid to the trader: 0.75 * 0.5 * 100,000 = 37,500, on the first path only.
        // Without the refund: 37,500 - 500 - 100 = 36,900; -500 - 100 = -600; -500.
        // Mean: (36,900 - 600 - 500) / 3 = 35,800 / 3.
        let f = figures(&spec, &fees, &paths);
        assert_eq!(f.value, 35_800.0 / 3.0);
        assert_eq!(f.pass, 2.0 / 3.0);
        assert_eq!(f.payouts_per_funded, Some(37_500.0 / 2.0));
        assert_eq!(f.payout_chance, 1.0 / 3.0);
        // With the refund, the first path also gets its 500 back: 37,400; the second made no
        // payout and gets nothing back. Mean: (37,400 - 600 - 500) / 3 = 36,300 / 3.
        let refund = AttemptTerms {
            refund_fee_with_first_payout: true,
            ..fees
        };
        assert_eq!(figures(&spec, &refund, &paths).value, 36_300.0 / 3.0);
        // The passing fee falls on passes only: with no pass, every path pays the 500 alone.
        let fail = figures(&spec, &fees, &[[-0.5, 0.0, 0.0, 0.0]; 3]);
        assert_eq!(
            (fail.value, fail.pass, fail.payouts_per_funded),
            (-500.0, 0.0, None)
        );
    }

    #[test]
    fn with_zero_mean_returns_the_payouts_equal_the_losses_the_firm_absorbs() {
        // Days drawn one at a time from a series of mean exactly 0 make the balance plus what has
        // been paid out a martingale: each day multiplies the balance by 1 + r with E[r] = 0, a
        // payout only moves money from the one to the other, and the account stops at a breach
        // or the horizon. So E[gross payouts] = 1 - E[balance at the end], the balance where the
        // account ended, breach or horizon.
        let x: Vec<f64> = symmetric().iter().map(|r| r * 4.0).collect();
        let spec = PropFirmSpec {
            max_drawdown: 0.05,
            ..unlimited(60)
        };
        let full_split = AttemptTerms {
            profit_split: 1.0,
            evaluation_fee: 0.0,
            days_to_first_payout: 5,
            payout_interval_days: 5,
            funded_horizon_days: 60,
            ..terms()
        };
        let paths = 20_000;
        let mut draw = Resampler::new(x.len(), 1, 2026);
        let (mut gross, mut absorbed, mut sum_sq) = (0.0, 0.0, 0.0);
        for _ in 0..paths {
            draw.reset();
            let path: Vec<f64> = (0..60).map(|_| x[draw.next()]).collect();
            let f = funded(&spec, &full_split, &path);
            gross += f.gross;
            absorbed += 1.0 - f.balance;
            let gap = f.gross - (1.0 - f.balance);
            sum_sq += gap * gap;
        }
        let (gross, absorbed) = (gross / paths as f64, absorbed / paths as f64);
        // The standard error of the mean gap, from the paths' own spread.
        let se =
            (sum_sq / paths as f64 - (gross - absorbed).powi(2)).sqrt() / (paths as f64).sqrt();
        // Payouts happen and losses are absorbed, so the check has something to balance.
        assert!(gross > 0.005 && absorbed > 0.005, "{gross} {absorbed}");
        // Within four standard errors, a chance of about 6e-5 of failing by luck alone.
        assert!(
            (gross - absorbed).abs() < 4.0 * se,
            "{gross} {absorbed} {se}"
        );
    }

    /// A series with a positive mean of about 0.002 a day and the spread of `symmetric() * 4`.
    fn edged() -> Vec<f64> {
        symmetric().iter().map(|r| r * 4.0 + 0.002).collect()
    }

    fn attempt_spec() -> PropFirmSpec {
        PropFirmSpec {
            target: 0.08,
            max_drawdown: 0.08,
            daily_loss_limit: 0.04,
            paths: 500,
            seed: 5,
            ..challenge(60)
        }
    }

    #[test]
    fn a_factor_of_one_keeps_the_edge_and_a_factor_of_zero_removes_it() {
        let x = edged();
        let spec = attempt_spec();
        let at = |factor| {
            let terms = AttemptTerms {
                reduced_edge_factor: factor,
                funded_horizon_days: 60,
                ..terms()
            };
            prop_firm_attempt(&x, None, &every_day(&x), &spec, &terms).unwrap()
        };
        // Subtracting (1 - 1) * mean is subtracting 0: the same numbers on the same draws.
        let one = at(1.0);
        assert_eq!(one.full, one.reduced);
        // A factor of 0 subtracts the whole mean, which leaves symmetric() * 4 up to rounding in
        // the last bits: the figures of that zero-mean series, on the same draws.
        let zero = at(0.0);
        let terms = AttemptTerms {
            funded_horizon_days: 60,
            ..terms()
        };
        let flat: Vec<f64> = symmetric().iter().map(|r| r * 4.0).collect();
        let centred = prop_firm_attempt(&flat, None, &every_day(&flat), &spec, &terms)
            .unwrap()
            .full;
        assert_eq!(zero.full, one.full);
        assert_eq!(zero.reduced.pass, centred.pass);
        assert_eq!(zero.reduced.payout_chance, centred.payout_chance);
        assert!(
            (zero.reduced.value - centred.value).abs() < 1e-6,
            "{zero:?} {centred:?}"
        );
        // The edge is worth something: without it an attempt passes less and is worth less.
        assert!(zero.reduced.pass < zero.full.pass, "{zero:?}");
        assert!(zero.reduced.value < zero.full.value, "{zero:?}");
    }

    #[test]
    fn the_same_seed_gives_the_same_figures_and_paths_do_not_depend_on_earlier_limits() {
        let x = edged();
        let spec = attempt_spec();
        let terms = AttemptTerms {
            funded_horizon_days: 60,
            ..terms()
        };
        let a = prop_firm_attempt(&x, None, &every_day(&x), &spec, &terms).unwrap();
        assert_eq!(
            a,
            prop_firm_attempt(&x, None, &every_day(&x), &spec, &terms).unwrap()
        );
        let other = PropFirmSpec { seed: 6, ..spec };
        assert_ne!(
            a.full,
            prop_firm_attempt(&x, None, &every_day(&x), &other, &terms)
                .unwrap()
                .full
        );
        // Locking the floor after a payout ends funded accounts earlier. Every path still draws
        // both horizons in full, so the next path's evaluation is the same and the pass odds do
        // not move, while the payouts do.
        let lock = AttemptTerms {
            payout_locks_floor: true,
            ..terms
        };
        let locked = prop_firm_attempt(&x, None, &every_day(&x), &spec, &lock).unwrap();
        assert_eq!(locked.full.pass, a.full.pass);
        assert_eq!(locked.reduced.pass, a.reduced.pass);
        assert_ne!(locked.full.payouts_per_funded, a.full.payouts_per_funded);
    }

    #[test]
    fn a_drawn_day_brings_its_own_traded_flag() {
        // Two days: +25% with a trade, and flat with none. A 10% target with at least 2 trading
        // days over a 2-day horizon, days drawn one at a time. A path has 2 trading days only
        // when it draws the +25% day twice, which makes 1.5625 and passes; that is also the one
        // path that reaches a 50% target with no minimum, as one +25% day makes 1.25. So when
        // each draw brings the flag of the day it drew, the two pass on the same paths, about a
        // quarter of them.
        let x = [0.25, 0.0];
        let spec = PropFirmSpec {
            min_days: 2,
            paths: 2_000,
            mean_block: 1,
            seed: 41,
            ..unlimited(2)
        };
        let half = PropFirmSpec {
            target: 0.5,
            min_days: 0,
            ..spec
        };
        let pass = |traded: [bool; 2], spec: &PropFirmSpec| {
            prop_firm_odds(&x, None, &traded, spec).unwrap().pass
        };
        let odds = pass([true, false], &spec);
        assert_eq!(odds, pass([true, false], &half));
        // Four standard errors of a probability of 1/4 from 2,000 paths: 4 * sqrt(3/16 / 2000)
        // = 0.039.
        assert!((odds - 0.25).abs() < 0.04, "{odds}");
        // Flagging both days passes every path with a +25% day, about 3/4; flagging the flat day
        // alone passes none, as two flat days never reach the target.
        let every = pass([true, true], &spec);
        assert!((every - 0.75).abs() < 0.04, "{every}");
        assert_eq!(pass([false, true], &spec), 0.0);

        // An attempt draws its days the same way, for both sets of figures. Halving the edge
        // subtracts 0.0625 from each day, leaving 0.1875 and -0.0625: two +18.75% days make
        // 1.41015625 with 2 trading days and pass, and a path with one of them has 1 trading day
        // whatever its balance. So the reduced figures pass on the same paths as the full ones.
        let terms = AttemptTerms {
            funded_horizon_days: 1,
            ..terms()
        };
        let attempt = |spec: &PropFirmSpec| {
            prop_firm_attempt(&x, None, &[true, false], spec, &terms).unwrap()
        };
        let a = attempt(&spec);
        assert_eq!(a.full.pass, attempt(&half).full.pass);
        assert_eq!(a.reduced.pass, a.full.pass);
        assert!((a.full.pass - 0.25).abs() < 0.04, "{a:?}");
    }
}
