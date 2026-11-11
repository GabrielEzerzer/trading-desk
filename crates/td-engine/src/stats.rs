//! Seeded resampling statistics for the test battery (#35): a block bootstrap of the mean
//! (R7, and the expectancy check of the `high_risk` kind rule), the risk of ruin, and the odds of
//! passing or breaching a prop-firm challenge.
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
//! 1995), and the values land in the 5 to 10 day range commonly used for daily data.
//!
//! For the mean of trade results, trades are resampled one at a time (a mean block of 1, the
//! plain bootstrap): the spread of a mean depends little on the order of separate trades. The
//! risk of ruin does depend on it, because ruin comes from a run of losses, so [`risk_of_ruin`]
//! draws trades in blocks too.

use serde::{Deserialize, Serialize};

use crate::rng::SplitMix64;

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
// Risk of ruin
// ---------------------------------------------------------------------------------------------

/// How [`risk_of_ruin`] simulates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RuinSpec {
    /// Ruin is equity at or below this fraction of the starting equity, strictly between 0 and
    /// 1: 0.5 is ruin at a loss of half the account.
    pub ruin_level: f64,
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
/// equity paths that fall to `ruin_level` of the starting equity within `horizon` trades.
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
    if !(spec.ruin_level > 0.0 && spec.ruin_level < 1.0) {
        return Err(StatsError::InvalidSpec(
            "the ruin level must be strictly between 0 and 1",
        ));
    }
    if spec.horizon == 0 {
        return Err(StatsError::InvalidSpec("the horizon must be at least 1"));
    }
    let mut draw = Resampler::new(trade_returns.len(), spec.mean_block, spec.seed);
    let ruined = (0..spec.paths)
        .filter(|_| {
            draw.reset();
            let mut equity = 1.0;
            (0..spec.horizon).any(|_| {
                equity *= 1.0 + trade_returns[draw.next()];
                equity <= spec.ruin_level
            })
        })
        .count();
    Ok(ruined as f64 / spec.paths as f64)
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
    /// From the highest daily close so far, by the same amount: breached at
    /// `peak - max_drawdown * start`, the usual trailing rule, whose amount is set by the
    /// starting balance.
    ///
    /// shortcut: the floor keeps trailing above the starting balance; firms that stop it there
    /// would need a cap.
    Trailing,
}

/// A prop-firm challenge, in fractions of the starting balance.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PropFirmSpec {
    /// The profit target: passed once equity reaches `start * (1 + target)`.
    pub target: f64,
    /// The daily loss limit: breached when one day loses at least `daily_loss_limit * start`
    /// from the previous day's close. Infinity turns it off.
    pub daily_loss_limit: f64,
    /// The maximum drawdown, measured as `drawdown` says. Infinity turns it off.
    pub max_drawdown: f64,
    pub drawdown: DrawdownMode,
    /// Days per path; #35's breach rule uses 90.
    pub horizon_days: usize,
    pub paths: usize,
    /// The mean block length; [`default_block_len`] of the number of returns is the default.
    pub mean_block: usize,
    pub seed: u64,
}

/// The odds of a prop-firm challenge over simulated paths.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PropFirmOdds {
    /// The fraction of paths that reach the target within the horizon before breaching any
    /// limit. The `prop_firm` kind rule asks for at least 0.60.
    pub pass: f64,
    /// The fraction of paths that breach a limit within the horizon, trading on after reaching
    /// the target as a funded account would. The `prop_firm` kind rule asks for at most 0.10
    /// over 90 days.
    pub breach: f64,
    pub paths: usize,
}

/// What one path did: the day (counted from 1) it first reached the target and first breached a
/// limit, if it did within the horizon.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PathOutcome {
    target_day: Option<usize>,
    breach_day: Option<usize>,
}

impl PathOutcome {
    /// Reached the target before any breach.
    fn passed(&self) -> bool {
        match (self.target_day, self.breach_day) {
            (Some(t), Some(b)) => t < b,
            (target, _) => target.is_some(),
        }
    }
}

/// Walks one path of daily returns from a balance of 1, checking the limits before the target on
/// each day. No day can do both from daily closes: a breach needs a losing day, and a losing day
/// before the target is reached ends below it.
fn walk_path(spec: &PropFirmSpec, returns: impl Iterator<Item = f64>) -> PathOutcome {
    let target = 1.0 + spec.target;
    let static_floor = 1.0 - spec.max_drawdown;
    let mut out = PathOutcome {
        target_day: None,
        breach_day: None,
    };
    let (mut equity, mut peak) = (1.0f64, 1.0f64);
    for (day, r) in (1..=spec.horizon_days).zip(returns) {
        let prev = equity;
        equity *= 1.0 + r;
        let floor = match spec.drawdown {
            DrawdownMode::Static => static_floor,
            DrawdownMode::Trailing => peak - spec.max_drawdown,
        };
        peak = peak.max(equity);
        if prev - equity >= spec.daily_loss_limit || equity <= floor {
            out.breach_day = Some(day);
            return out;
        }
        if out.target_day.is_none() && equity >= target {
            out.target_day = Some(day);
        }
    }
    out
}

/// The odds of passing and of breaching a prop-firm challenge, from `paths` paths of
/// `horizon_days` daily returns drawn from `daily_returns` with the stationary bootstrap.
///
/// The daily loss is measured from one daily close to the next, from the daily returns alone.
/// A loss inside a day that recovers by its close is not seen here; the kind rule's check of the
/// worst daily loss inside bars covers that from the equity curve.
pub fn prop_firm_odds(
    daily_returns: &[f64],
    spec: &PropFirmSpec,
) -> Result<PropFirmOdds, StatsError> {
    check_values(daily_returns)?;
    check_count(spec.paths, spec.mean_block)?;
    let positive = |x: f64| x > 0.0; // false for NaN; infinity allowed
    if !(positive(spec.target) && spec.target.is_finite()) {
        return Err(StatsError::InvalidSpec(
            "the profit target must be a positive number",
        ));
    }
    if !(positive(spec.daily_loss_limit) && positive(spec.max_drawdown)) {
        return Err(StatsError::InvalidSpec(
            "the loss limits must be positive (infinity turns one off)",
        ));
    }
    if spec.horizon_days == 0 {
        return Err(StatsError::InvalidSpec(
            "the horizon must be at least 1 day",
        ));
    }
    let mut draw = Resampler::new(daily_returns.len(), spec.mean_block, spec.seed);
    let (mut passed, mut breached) = (0usize, 0usize);
    for _ in 0..spec.paths {
        draw.reset();
        // Draw the whole horizon even when the path breaches early, so path k uses the same
        // draws whatever the limits are.
        let path: Vec<f64> = (0..spec.horizon_days)
            .map(|_| daily_returns[draw.next()])
            .collect();
        let o = walk_path(spec, path.into_iter());
        passed += usize::from(o.passed());
        breached += usize::from(o.breach_day.is_some());
    }
    Ok(PropFirmOdds {
        pass: passed as f64 / spec.paths as f64,
        breach: breached as f64 / spec.paths as f64,
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
                ruin_level: 0.9,
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

    fn ruin_spec(horizon: usize) -> RuinSpec {
        RuinSpec {
            ruin_level: 0.5,
            horizon,
            paths: 200,
            mean_block: 1,
            seed: 3,
        }
    }

    #[test]
    fn risk_of_ruin_is_zero_when_every_trade_wins_and_one_when_each_loses_past_the_level() {
        assert_eq!(risk_of_ruin(&[0.01, 0.2, 0.05], &ruin_spec(100)), Ok(0.0));
        // One trade of -60% takes equity from 1 to 0.4, past a ruin level of 0.5.
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
        assert!(p > 0.0 && p < 0.5, "{p}");
        assert_eq!(p, PINNED_RUIN);
    }

    /// 807 of 2,000 paths.
    const PINNED_RUIN: f64 = 0.4035;

    #[test]
    fn blocks_keep_a_run_of_losses_that_single_draws_scatter() {
        // A bad regime of 8 trades at -10% (0.9^8 = 0.430, past half the account), then 92
        // trades at +2%. Replaying this history from a uniformly random starting trade, with the
        // wrap, ruins 9 of the 100 starts: the 8 starts with 0 to 7 wins before the losses
        // (1.02^7 * 0.9^8 = 0.495 is ruin, 1.02^8 * 0.9^8 = 0.504 is not) and the start on the
        // second loss (0.9^7 = 0.478).
        let mut x = vec![-0.1; 8];
        x.extend([0.02; 92]);
        let ruined_starts = (0..x.len())
            .filter(|&start| {
                let mut equity = 1.0;
                (0..x.len()).any(|k| {
                    equity *= 1.0 + x[(start + k) % x.len()];
                    equity <= 0.5
                })
            })
            .count();
        assert_eq!(ruined_starts, 9);
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
        // Blocks land near the 9% of the random starts, past #35's 5% gate; single draws do not.
        assert!(blocks > 0.05 && blocks < 0.15, "{blocks}");
        assert!(single < 0.01, "{single}");
        assert_eq!((blocks, single), PINNED_CLUSTERED_RUIN);
    }

    /// 180 and 1 of 2,000 paths, seed 3.
    const PINNED_CLUSTERED_RUIN: (f64, f64) = (0.09, 0.0005);

    fn challenge(horizon_days: usize) -> PropFirmSpec {
        PropFirmSpec {
            target: 0.10,
            daily_loss_limit: 0.05,
            max_drawdown: 0.10,
            drawdown: DrawdownMode::Static,
            horizon_days,
            paths: 300,
            mean_block: 3,
            seed: 11,
        }
    }

    #[test]
    fn a_challenge_that_gains_every_day_passes_and_one_that_breaches_on_day_one_fails() {
        // +1% a day: 1.01^9 = 1.0937 is short of the 10% target and 1.01^10 = 1.1046 reaches it.
        let gains = [0.01; 20];
        let odds = prop_firm_odds(&gains, &challenge(10)).unwrap();
        assert_eq!((odds.pass, odds.breach), (1.0, 0.0));
        let odds = prop_firm_odds(&gains, &challenge(9)).unwrap();
        assert_eq!((odds.pass, odds.breach), (0.0, 0.0));
        // -6% on the first day breaches the 5% daily loss limit.
        let odds = prop_firm_odds(&[-0.06, -0.07], &challenge(90)).unwrap();
        assert_eq!((odds.pass, odds.breach), (0.0, 1.0));
    }

    fn walk(spec: &PropFirmSpec, path: &[f64]) -> PathOutcome {
        walk_path(spec, path.iter().copied())
    }

    #[test]
    fn the_daily_loss_limit_and_both_drawdown_modes_break_where_worked_by_hand() {
        let off = PropFirmSpec {
            daily_loss_limit: f64::INFINITY,
            max_drawdown: f64::INFINITY,
            ..challenge(10)
        };
        // Daily loss: from 1.1, -4% loses 0.044 and -4.8% loses 0.0528, measured against a limit
        // of 0.05 of the starting balance. Against the day's opening equity, -4.8% would be under
        // a 5% limit and hold.
        let daily = PropFirmSpec {
            daily_loss_limit: 0.05,
            ..off
        };
        assert_eq!(walk(&daily, &[0.1, -0.04]).breach_day, None);
        assert_eq!(walk(&daily, &[0.1, -0.048]).breach_day, Some(2));
        // A loss of exactly the limit breaches: 1 - 0.875 is 0.125, all exact in binary.
        let eighth = PropFirmSpec {
            daily_loss_limit: 0.125,
            ..off
        };
        assert_eq!(walk(&eighth, &[-0.125]).breach_day, Some(1));
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
        assert_eq!(walk(&fixed, &[0.1, -0.08]).breach_day, None);
        assert_eq!(walk(&trailing, &[0.1, -0.08]).breach_day, None);
        assert_eq!(walk(&fixed, &[0.1, -0.1]).breach_day, None);
        assert_eq!(walk(&trailing, &[0.1, -0.1]).breach_day, Some(2));
        // Static: three days of -4% reach 0.8847, below 0.9, on day 3.
        assert_eq!(walk(&fixed, &[-0.04, -0.04, -0.04]).breach_day, Some(3));
        // Landing exactly on the static floor breaches: 1 - 0.25 is 0.75, exact in binary.
        let quarter = PropFirmSpec {
            max_drawdown: 0.25,
            ..off
        };
        assert_eq!(walk(&quarter, &[-0.25]).breach_day, Some(1));
        // The trailing floor sits max_drawdown of the starting balance below the peak, not a
        // fraction of the peak: from a peak of 1.2, -9% is 1.092, under 1.2 - 0.1 = 1.1 but
        // above 1.2 * 0.9 = 1.08.
        assert_eq!(walk(&trailing, &[0.2, -0.09]).breach_day, Some(2));
        // The path trades on after the target: day 1 reaches 1.1, day 2 breaches the trailing
        // floor. That is a pass, because the target came first, and a breach.
        let o = walk(&trailing, &[0.1, -0.1]);
        assert_eq!(o.target_day, Some(1));
        assert!(o.passed());
        // Breaching on day 1 before reaching the target on day 2 would be no pass; a breach
        // ends the walk, so the target is never seen.
        let o = walk(&trailing, &[-0.15, 0.4]);
        assert_eq!((o.breach_day, o.target_day), (Some(1), None));
        assert!(!o.passed());
    }

    #[test]
    fn prop_firm_odds_are_pinned_for_a_mixed_series() {
        let x: Vec<f64> = symmetric().iter().map(|r| r * 6.0 + 0.003).collect();
        let odds = prop_firm_odds(&x, &challenge(90)).unwrap();
        assert_eq!((odds.pass, odds.breach), PINNED_PROP);
        let again = prop_firm_odds(&x, &challenge(90)).unwrap();
        assert_eq!(odds, again);
    }

    /// 277 and 27 of 300 paths.
    const PINNED_PROP: (f64, f64) = (277.0 / 300.0, 27.0 / 300.0);

    #[test]
    fn prop_firm_specs_out_of_range_are_errors() {
        let bad = [
            PropFirmSpec {
                target: 0.0,
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
            challenge(0),
            PropFirmSpec {
                paths: 0,
                ..challenge(5)
            },
        ];
        for spec in bad {
            assert!(matches!(
                prop_firm_odds(&[0.01], &spec),
                Err(StatsError::InvalidSpec(_))
            ));
        }
    }
}
