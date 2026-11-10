//! Performance metrics over an equity series and a trade list.
//!
//! The equity series a report is built from is the starting cash followed by the equity at each
//! bar's close, so a run of n bars gives n + 1 values and n per-bar returns. Its times are the
//! first bar's open time, then each bar's close time (open time plus the timeframe).
//!
//! Every value that can be undefined is an `Option`: `None` means there is not enough data, the
//! formula would divide by zero, or the result is not a finite number (CAGR over a few minutes
//! can compound past `f64::MAX`). Every public function here returns `None` rather than NaN or an
//! infinity, so the report serialises to JSON and loads back equal. The plain `f64` fields are the
//! first and last equity and sums of fill fees and funding, finite for finite inputs.

use serde::{Deserialize, Serialize};
use td_core::{Fill, Millis, Timeframe};

use crate::{EquityPoint, Trade};

/// Days in a year for annualising: crypto markets trade every day.
pub const DAYS_PER_YEAR: f64 = 365.0;
const MS_PER_YEAR: f64 = DAYS_PER_YEAR * 86_400_000.0;

/// The metrics report for one equity series and its trades.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsReport {
    pub start_equity: f64,
    pub end_equity: f64,
    /// See [`total_return`].
    pub total_return: Option<f64>,
    /// See [`cagr`].
    pub cagr: Option<f64>,
    /// Annualised volatility of per-bar returns; see [`volatility`].
    pub volatility: Option<f64>,
    /// See [`sharpe`].
    pub sharpe: Option<f64>,
    /// See [`sortino`].
    pub sortino: Option<f64>,
    /// Depth of the largest drawdown as a fraction of its peak; see [`max_drawdown`].
    pub max_drawdown: Option<f64>,
    /// From the largest drawdown's peak to its recovery, or to the end if it never recovered.
    pub max_drawdown_duration_ms: Option<Millis>,
    pub max_drawdown_duration_bars: Option<usize>,
    /// See [`calmar`].
    pub calmar: Option<f64>,
    /// Closed trades. The trade statistics below count closed trades only; an open trade at the
    /// end is in the trade list but not in these numbers.
    pub trades: usize,
    /// See [`win_rate`].
    pub win_rate: Option<f64>,
    /// See [`profit_factor`].
    pub profit_factor: Option<f64>,
    /// Mean net P&L per closed trade.
    pub avg_trade_pnl: Option<f64>,
    /// Median net P&L per closed trade.
    pub median_trade_pnl: Option<f64>,
    /// Mean bars held per closed trade: bars, not time (see [`Trade::bars_held`]).
    pub avg_bars_held: Option<f64>,
    /// See [`exposure`].
    pub exposure: Option<f64>,
    /// See [`turnover`].
    pub turnover: Option<f64>,
    /// Sum of fill fees; negative if rebates outweighed fees.
    pub fees_paid: f64,
    /// Net funding paid; negative when the position received funding.
    pub funding_paid: f64,
}

/// An equity series in the form the metrics read: the starting value, then one value per bar
/// close, with matching times.
#[derive(Debug, Clone, PartialEq)]
pub struct EquitySeries {
    pub times: Vec<Millis>,
    pub values: Vec<f64>,
}

impl EquitySeries {
    /// Builds the series from the starting cash and an equity curve with one point per bar,
    /// stamped with the bar's open time.
    pub fn new(starting_cash: f64, curve: &[EquityPoint], timeframe: Timeframe) -> EquitySeries {
        let step = timeframe.duration_ms();
        let first = curve.first().map_or(0, |p| p.time);
        let mut times = vec![first];
        let mut values = vec![starting_cash];
        for p in curve {
            times.push(p.time + step);
            values.push(p.equity);
        }
        EquitySeries { times, values }
    }
}

/// Builds the full report.
///
/// `positions` is the position after each bar's fills, one per bar. `trades` may include an open
/// trade at the end; the trade statistics skip it. `funding_paid` is the run's net funding.
pub fn report(
    series: &EquitySeries,
    timeframe: Timeframe,
    positions: &[f64],
    trades: &[Trade],
    fills: &[Fill],
    funding_paid: f64,
) -> MetricsReport {
    let values = &series.values;
    let elapsed = match (series.times.first(), series.times.last()) {
        (Some(a), Some(b)) => b - a,
        _ => 0,
    };
    let bpy = bars_per_year(timeframe);
    let returns = per_bar_returns(values);
    let returns = returns.as_deref();
    let growth = cagr(values, elapsed);
    let dd = max_drawdown(&series.times, values);
    let closed: Vec<&Trade> = trades.iter().filter(|t| !t.open).collect();
    let pnls: Vec<f64> = closed.iter().map(|t| t.net_pnl).collect();
    let held: Vec<f64> = closed.iter().map(|t| t.bars_held as f64).collect();
    MetricsReport {
        start_equity: values.first().copied().unwrap_or(0.0),
        end_equity: values.last().copied().unwrap_or(0.0),
        total_return: total_return(values),
        cagr: growth,
        volatility: returns.and_then(|r| volatility(r, bpy)),
        sharpe: returns.and_then(|r| sharpe(r, bpy)),
        sortino: returns.and_then(|r| sortino(r, bpy)),
        max_drawdown: dd.map(|d| d.depth),
        max_drawdown_duration_ms: dd.map(|d| d.duration_ms),
        max_drawdown_duration_bars: dd.map(|d| d.duration_bars),
        calmar: calmar(growth, dd.map(|d| d.depth)),
        trades: closed.len(),
        win_rate: win_rate(&pnls),
        profit_factor: profit_factor(&pnls),
        avg_trade_pnl: mean(&pnls),
        median_trade_pnl: median(&pnls),
        avg_bars_held: mean(&held),
        exposure: exposure(positions),
        turnover: turnover(fills, values.get(1..).unwrap_or(&[])),
        fees_paid: fills.iter().map(|f| f.fee).sum(),
        funding_paid,
    }
}

/// Bars in a 365-day year: `365 * 86_400_000 / timeframe_ms`.
pub fn bars_per_year(timeframe: Timeframe) -> f64 {
    MS_PER_YEAR / timeframe.duration_ms() as f64
}

/// `Some(x)` if `x` is finite; NaN and the infinities are `None`.
fn finite(x: f64) -> Option<f64> {
    x.is_finite().then_some(x)
}

/// `last / first - 1`. `None` with fewer than two values, a first value at or below zero, or a
/// result that is not finite.
pub fn total_return(values: &[f64]) -> Option<f64> {
    match (values.first(), values.last()) {
        (Some(&a), Some(&b)) if values.len() >= 2 && a > 0.0 => finite(b / a - 1.0),
        _ => None,
    }
}

/// `r_i = v_i / v_{i-1} - 1` for each consecutive pair. `None` with fewer than two values, if
/// any value a return is measured from is at or below zero (the return is then meaningless), or if
/// a return is not finite.
pub fn per_bar_returns(values: &[f64]) -> Option<Vec<f64>> {
    if values.len() < 2 || values[..values.len() - 1].iter().any(|&v| v <= 0.0) {
        return None;
    }
    values
        .windows(2)
        .map(|w| finite(w[1] / w[0] - 1.0))
        .collect()
}

/// Compound annual growth rate over the actual elapsed time, with a 365-day year:
/// `(last / first) ^ (365 days / elapsed) - 1`. `None` if the elapsed time is not positive, the
/// first value is at or below zero, the last value is below zero, or the result is not finite: a
/// gain over a short run (0.8% in two minutes) compounds past `f64::MAX`.
pub fn cagr(values: &[f64], elapsed_ms: Millis) -> Option<f64> {
    let (&a, &b) = (values.first()?, values.last()?);
    if elapsed_ms <= 0 || a <= 0.0 || b < 0.0 {
        return None;
    }
    finite((b / a).powf(MS_PER_YEAR / elapsed_ms as f64) - 1.0)
}

fn mean_f(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Sample standard deviation (n - 1 in the denominator). Needs at least two values; `None` if the
/// squares overflow, so a ratio over it is never a misleading 0.
fn sample_std(xs: &[f64]) -> Option<f64> {
    if xs.len() < 2 {
        return None;
    }
    let m = mean_f(xs);
    let ss: f64 = xs.iter().map(|x| (x - m) * (x - m)).sum();
    finite((ss / (xs.len() - 1) as f64).sqrt())
}

/// Annualised volatility: the sample standard deviation of per-bar returns times
/// `sqrt(bars_per_year)`. `None` with fewer than two returns or a result that is not finite.
pub fn volatility(returns: &[f64], bars_per_year: f64) -> Option<f64> {
    finite(sample_std(returns)? * bars_per_year.sqrt())
}

/// Annualised Sharpe ratio with a risk-free rate of zero:
/// `mean(r) / sample_std(r) * sqrt(bars_per_year)`. `None` with fewer than two returns, zero
/// volatility or a result that is not finite.
pub fn sharpe(returns: &[f64], bars_per_year: f64) -> Option<f64> {
    let sd = sample_std(returns)?;
    if sd > 0.0 {
        finite(mean_f(returns) / sd * bars_per_year.sqrt())
    } else {
        None
    }
}

/// Annualised Sortino ratio with a target return of zero:
/// `mean(r) / dd * sqrt(bars_per_year)`, where the downside deviation
/// `dd = sqrt(sum(min(r_i, 0)^2) / n)` is taken over all n bars, not only the losing ones.
/// `None` with fewer than two returns (as for Sharpe), no losing bar or a result that is not
/// finite.
pub fn sortino(returns: &[f64], bars_per_year: f64) -> Option<f64> {
    if returns.len() < 2 {
        return None;
    }
    let down: f64 = returns.iter().map(|r| r.min(0.0).powi(2)).sum();
    let dd = (down / returns.len() as f64).sqrt();
    // An infinite downside deviation would give a misleading 0.
    if dd > 0.0 && dd.is_finite() {
        finite(mean_f(returns) / dd * bars_per_year.sqrt())
    } else {
        None
    }
}

/// The largest peak-to-trough fall of an equity series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drawdown {
    /// `1 - trough / peak`, where the peak is the running maximum before the trough. 0 when the
    /// series never falls.
    pub depth: f64,
    /// Index of the peak, the trough, and the recovery (the first later value at or above the
    /// peak), if there is one.
    pub peak: usize,
    pub trough: usize,
    pub recovery: Option<usize>,
    /// From the peak to the recovery, or to the last value if the series never recovered.
    pub duration_ms: Millis,
    pub duration_bars: usize,
}

/// The maximum drawdown and its duration. The first of equally deep drawdowns wins. `None` for an
/// empty series, one whose running peak is at or below zero, or a depth that is not finite.
pub fn max_drawdown(times: &[Millis], values: &[f64]) -> Option<Drawdown> {
    if values.is_empty() || times.len() != values.len() {
        return None;
    }
    let (mut peak, mut best_peak, mut trough, mut depth) = (0, 0, 0, 0.0);
    for (i, &v) in values.iter().enumerate() {
        if values[peak] <= 0.0 {
            return None;
        }
        if v >= values[peak] {
            peak = i;
        } else {
            let d = 1.0 - v / values[peak];
            if !d.is_finite() {
                return None;
            }
            if d > depth {
                (best_peak, trough, depth) = (peak, i, d);
            }
        }
    }
    if depth == 0.0 {
        return Some(Drawdown {
            depth: 0.0,
            peak: 0,
            trough: 0,
            recovery: None,
            duration_ms: 0,
            duration_bars: 0,
        });
    }
    let recovery = (trough + 1..values.len()).find(|&j| values[j] >= values[best_peak]);
    let end = recovery.unwrap_or(values.len() - 1);
    Some(Drawdown {
        depth,
        peak: best_peak,
        trough,
        recovery,
        duration_ms: times[end] - times[best_peak],
        duration_bars: end - best_peak,
    })
}

/// `cagr / max_drawdown`. `None` if either is missing, the drawdown is zero or the result is not
/// finite.
pub fn calmar(cagr: Option<f64>, max_drawdown: Option<f64>) -> Option<f64> {
    let dd = max_drawdown?;
    if dd > 0.0 { finite(cagr? / dd) } else { None }
}

/// Fraction of trades with net P&L above zero. `None` with no trades.
pub fn win_rate(pnls: &[f64]) -> Option<f64> {
    (!pnls.is_empty()).then(|| pnls.iter().filter(|&&p| p > 0.0).count() as f64 / pnls.len() as f64)
}

/// Gross profit over gross loss: `sum(p > 0) / |sum(p < 0)|` on net trade P&L. 0 when every
/// trade lost; `None` with no losing trade (including no trades), rather than infinity, or a
/// result that is not finite.
pub fn profit_factor(pnls: &[f64]) -> Option<f64> {
    let profit: f64 = pnls.iter().filter(|&&p| p > 0.0).sum();
    let loss: f64 = pnls.iter().filter(|&&p| p < 0.0).sum();
    if loss < 0.0 {
        finite(profit / -loss)
    } else {
        None
    }
}

/// Arithmetic mean. `None` when empty or when the result is not finite.
pub fn mean(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        None
    } else {
        finite(mean_f(xs))
    }
}

/// Median: the middle value, or the mean of the two middle values for an even count. `None` when
/// empty or not finite.
pub fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut s = xs.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    finite(if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    })
}

/// Fraction of bars with a non-zero position after the bar's fills. `None` with no bars.
pub fn exposure(positions: &[f64]) -> Option<f64> {
    (!positions.is_empty())
        .then(|| positions.iter().filter(|&&p| p != 0.0).count() as f64 / positions.len() as f64)
}

/// Traded notional over average equity: `sum(qty * price) / mean(equity)`, with the equity at
/// each bar's close. `None` with no bars, an average equity at or below zero or a result that is
/// not finite.
pub fn turnover(fills: &[Fill], equity: &[f64]) -> Option<f64> {
    let avg = mean(equity)?;
    let traded: f64 = fills.iter().map(|f| f.qty * f.price).sum();
    if avg > 0.0 {
        finite(traded / avg)
    } else {
        None
    }
}

/// Pearson correlation of two equally long series. `None` if the lengths differ, there are fewer
/// than two values, either series is constant, or the result is not finite.
pub fn correlation(a: &[f64], b: &[f64]) -> Option<f64> {
    if a.len() != b.len() || a.len() < 2 {
        return None;
    }
    let (ma, mb) = (mean_f(a), mean_f(b));
    let (mut cov, mut va, mut vb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        cov += (x - ma) * (y - mb);
        va += (x - ma) * (x - ma);
        vb += (y - mb) * (y - mb);
    }
    // An infinite denominator would give a misleading 0.
    let denom = (va * vb).sqrt();
    if va > 0.0 && vb > 0.0 && denom.is_finite() {
        finite(cov / denom)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn approx(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() <= 1e-12 * expected.abs().max(1.0),
            "expected {expected}, got {actual}"
        );
    }

    #[track_caller]
    fn some(actual: Option<f64>, expected: f64) {
        approx(actual.expect("expected a value"), expected);
    }

    const DAY: Millis = 86_400_000;

    #[test]
    fn bars_per_year_uses_a_365_day_year() {
        approx(bars_per_year(Timeframe::D1), 365.0);
        approx(bars_per_year(Timeframe::H1), 8760.0);
        approx(bars_per_year(Timeframe::W1), 365.0 / 7.0);
    }

    #[test]
    fn total_return_and_its_edge_cases() {
        some(total_return(&[100.0, 120.0, 90.0]), -0.1);
        assert_eq!(total_return(&[100.0]), None);
        assert_eq!(total_return(&[0.0, 10.0]), None);
    }

    #[test]
    fn per_bar_returns_and_a_non_positive_base() {
        let r = per_bar_returns(&[100.0, 110.0, 99.0]).unwrap();
        approx(r[0], 0.1);
        approx(r[1], -0.1);
        assert_eq!(per_bar_returns(&[100.0, 0.0, 10.0]), None);
        assert_eq!(per_bar_returns(&[100.0]), None);
        // A final value at zero is still a valid return of -100%.
        assert_eq!(per_bar_returns(&[100.0, 0.0]), Some(vec![-1.0]));
    }

    #[test]
    fn cagr_over_actual_elapsed_time() {
        // 100 -> 121 over two 365-day years is 10% a year.
        some(cagr(&[100.0, 121.0], 730 * DAY), 0.1);
        // 100 -> 110 over half a year: 1.1^2 - 1 = 0.21.
        some(cagr(&[100.0, 105.0, 110.0], 365 * DAY / 2), 0.21);
        // Losing everything is -100%.
        some(cagr(&[100.0, 0.0], 365 * DAY), -1.0);
        assert_eq!(cagr(&[100.0, 110.0], 0), None);
        assert_eq!(cagr(&[100.0, -1.0], DAY), None);
        assert_eq!(cagr(&[0.0, 1.0], DAY), None);
    }

    // Returns 0.1, -0.1, 0.1 (from 100, 110, 99, 108.9). Mean 1/30. Deviations 1/15, -2/15, 1/15;
    // squares sum to 6/225 = 0.02666..., over n - 1 = 2 gives variance 0.01333... = 0.04 / 3.
    const RETS: [f64; 3] = [0.1, -0.1, 0.1];

    #[test]
    fn volatility_is_the_annualised_sample_std() {
        some(
            volatility(&RETS, 365.0),
            (0.04f64 / 3.0).sqrt() * 365f64.sqrt(),
        );
        assert_eq!(volatility(&[0.1], 365.0), None);
    }

    #[test]
    fn sharpe_is_mean_over_std_annualised() {
        let expected = (1.0 / 30.0) / (0.04f64 / 3.0).sqrt() * 365f64.sqrt();
        some(sharpe(&RETS, 365.0), expected);
        // About 5.515.
        assert!((expected - 5.515).abs() < 1e-3);
        // Zero volatility has no Sharpe ratio.
        assert_eq!(sharpe(&[0.01, 0.01, 0.01], 365.0), None);
    }

    #[test]
    fn sortino_uses_downside_deviation_over_all_bars() {
        // Downside: only -0.1 counts; sqrt(0.01 / 3) over all three bars.
        let expected = (1.0 / 30.0) / (0.01f64 / 3.0).sqrt() * 365f64.sqrt();
        some(sortino(&RETS, 365.0), expected);
        assert!((expected - 11.030).abs() < 1e-3);
        // No losing bar: no downside deviation, so no ratio.
        assert_eq!(sortino(&[0.01, 0.02], 365.0), None);
        assert_eq!(sortino(&[], 365.0), None);
        // One return is not enough, as for Sharpe.
        assert_eq!(sortino(&[-0.01], 365.0), None);
    }

    #[test]
    fn max_drawdown_depth_and_duration() {
        let times = [0, 10, 20, 30, 40, 50];
        let values = [100.0, 110.0, 99.0, 105.0, 121.0, 120.0];
        let d = max_drawdown(&times, &values).unwrap();
        // Peak 110 at 1, trough 99 at 2: 1 - 99 / 110 = 0.1. Recovered at 4 (121 >= 110).
        approx(d.depth, 0.1);
        assert_eq!((d.peak, d.trough, d.recovery), (1, 2, Some(4)));
        assert_eq!((d.duration_bars, d.duration_ms), (3, 30));
    }

    #[test]
    fn unrecovered_drawdown_runs_to_the_end() {
        let d = max_drawdown(&[0, 1, 2, 3], &[100.0, 80.0, 90.0, 95.0]).unwrap();
        approx(d.depth, 0.2);
        assert_eq!(d.recovery, None);
        assert_eq!((d.duration_bars, d.duration_ms), (3, 3));
    }

    #[test]
    fn rising_series_has_zero_drawdown() {
        let d = max_drawdown(&[0, 1, 2], &[100.0, 100.0, 101.0]).unwrap();
        assert_eq!(d.depth, 0.0);
        assert_eq!(d.duration_bars, 0);
        assert_eq!(calmar(Some(0.5), Some(d.depth)), None);
        assert_eq!(max_drawdown(&[], &[]), None);
        assert_eq!(max_drawdown(&[0, 1], &[0.0, 1.0]), None);
    }

    #[test]
    fn calmar_is_cagr_over_drawdown() {
        some(calmar(Some(0.3), Some(0.15)), 2.0);
        assert_eq!(calmar(None, Some(0.1)), None);
    }

    #[test]
    fn trade_statistics() {
        let pnls = [10.0, -5.0, 20.0, -10.0];
        some(win_rate(&pnls), 0.5);
        some(profit_factor(&pnls), 30.0 / 15.0);
        some(mean(&pnls), 3.75);
        // Sorted -10, -5, 10, 20: the middle two average to 2.5.
        some(median(&pnls), 2.5);
        some(median(&[3.0, 1.0, 2.0]), 2.0);
    }

    #[test]
    fn trade_statistics_edge_cases() {
        // All losers: profit factor 0. No losers: None, not infinity.
        some(profit_factor(&[-1.0, -2.0]), 0.0);
        assert_eq!(profit_factor(&[1.0, 2.0]), None);
        assert_eq!(profit_factor(&[]), None);
        // A break-even trade is neither a win nor a loss.
        some(win_rate(&[0.0]), 0.0);
        assert_eq!(profit_factor(&[0.0]), None);
        assert_eq!(win_rate(&[]), None);
        assert_eq!(mean(&[]), None);
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn exposure_and_turnover() {
        some(exposure(&[0.0, 1.0, -2.0, 0.0]), 0.5);
        assert_eq!(exposure(&[]), None);
        let fill = |qty: f64, price: f64| Fill {
            order_id: td_core::OrderId(1),
            instrument: td_core::InstrumentId {
                venue: td_core::Venue::Binance,
                symbol: "X".into(),
                kind: td_core::InstrumentKind::Spot,
            },
            side: td_core::Side::Buy,
            time: 0,
            price,
            qty,
            fee: 0.0,
            liquidity: td_core::Liquidity::Taker,
        };
        // Traded 1000 + 1100 over an average equity of 1050.
        let fills = [fill(10.0, 100.0), fill(10.0, 110.0)];
        some(turnover(&fills, &[1000.0, 1100.0]), 2.0);
        assert_eq!(turnover(&fills, &[]), None);
    }

    #[test]
    fn correlation_cases() {
        some(correlation(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]), 1.0);
        some(correlation(&[1.0, 2.0, 3.0], &[3.0, 2.0, 1.0]), -1.0);
        // x = 1, 2, 3 and y = 1, 3, 2: cov 1, var_x 2, var_y 2, so 0.5.
        some(correlation(&[1.0, 2.0, 3.0], &[1.0, 3.0, 2.0]), 0.5);
        assert_eq!(correlation(&[1.0, 1.0], &[1.0, 2.0]), None);
        assert_eq!(correlation(&[1.0], &[1.0]), None);
    }

    #[test]
    fn ratios_that_overflow_are_none_not_infinite() {
        // +0.8% over two one-minute bars compounds to more than f64::MAX a year.
        assert_eq!(cagr(&[100.0, 100.4, 100.8], 2 * 60_000), None);
        assert_eq!(calmar(Some(1e300), Some(1e-300)), None);
        assert_eq!(total_return(&[1e-300, 1e300]), None);
        assert_eq!(per_bar_returns(&[1e-300, 1e300]), None);
        assert_eq!(profit_factor(&[1e300, -1e-300]), None);
        assert_eq!(mean(&[1e308, 1e308]), None);
        assert_eq!(median(&[1e308, 1e308]), None);
        assert_eq!(volatility(&[1e300, -1e300], 365.0), None);
        assert_eq!(sharpe(&[1e300, -1e300, 1e300], 365.0), None);
        assert_eq!(sortino(&[1e300, -1e300], 365.0), None);
        assert_eq!(correlation(&[1e200, -1e200], &[1e200, -1e200]), None);
        assert_eq!(max_drawdown(&[0, 1], &[1e-300, -1e300]), None);
        let fill = Fill {
            order_id: td_core::OrderId(1),
            instrument: td_core::InstrumentId {
                venue: td_core::Venue::Binance,
                symbol: "X".into(),
                kind: td_core::InstrumentKind::Spot,
            },
            side: td_core::Side::Buy,
            time: 0,
            price: 1e300,
            qty: 1e300,
            fee: 0.0,
            liquidity: td_core::Liquidity::Taker,
        };
        assert_eq!(turnover(&[fill], &[1.0]), None);
    }

    #[test]
    fn series_starts_with_the_starting_cash() {
        let curve = [
            EquityPoint {
                time: 0,
                equity: 101.0,
            },
            EquityPoint {
                time: 60_000,
                equity: 102.0,
            },
        ];
        let s = EquitySeries::new(100.0, &curve, Timeframe::M1);
        assert_eq!(s.values, vec![100.0, 101.0, 102.0]);
        assert_eq!(s.times, vec![0, 60_000, 120_000]);
    }
}
