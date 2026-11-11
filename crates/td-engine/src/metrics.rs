//! Performance metrics over an equity series and a trade list.
//!
//! The equity series a report is built from is the starting cash followed by the equity at each
//! bar's close, so a run of n bars gives n + 1 values and n per-bar returns. Its times are the
//! first bar's open time, then each bar's close time (open time plus the timeframe).
//!
//! Volatility, Sharpe and Sortino are computed from daily returns, not per-bar returns, because
//! the bars can have gaps (the data layer drops a bucket with any missing minute) and a return
//! across a gap is not a one-bar return. The daily series ([`daily_equity`]) is the starting
//! equity, then for each UTC day the run touches the last equity value at or before 00:00 UTC of
//! the next day; the last day may be partial. A day with no equity value carries the previous
//! day's equity forward with a return of zero, so a move across a missing day lands on the next
//! day with data. Simple daily returns are annualised with a 365-day year (crypto trades every
//! day): `sqrt(365)` for volatility, Sharpe and Sortino, with a risk-free rate and target of zero.
//! The daily basis also makes them comparable across timeframes. CAGR and Calmar use the elapsed
//! time; the maximum drawdown and its duration use the full per-bar series.
//!
//! The report leaves the annualised figures out of short runs, where they mean nothing: volatility,
//! Sharpe and Sortino need [`MIN_DAILY_RETURNS`] daily returns on days with a bar close, and CAGR
//! and Calmar need a run of [`MIN_CAGR_DAYS`] days. The functions below compute their formula on
//! whatever they are given; the floors are applied by [`report`].
//!
//! Every value that can be undefined is an `Option`: `None` means there is not enough data, the
//! formula would divide by zero, or the result is not a finite number (CAGR over a few minutes
//! can compound past `f64::MAX`). Every public function here returns `None` rather than NaN or an
//! infinity, so the report serialises to JSON and loads back equal. The plain `f64` fields are the
//! first and last equity and sums of fill fees and funding, finite for finite inputs.

use serde::{Deserialize, Serialize};
use td_core::{Fill, Millis, Timeframe};

use crate::risk::{self, DayRollover, Intrabar};
use crate::{EquityPoint, Trade};

/// Days in a year for annualising: crypto markets trade every day.
pub const DAYS_PER_YEAR: f64 = 365.0;
pub(crate) const DAY_MS: Millis = 86_400_000;
const MS_PER_YEAR: f64 = DAYS_PER_YEAR * DAY_MS as f64;

/// The fewest daily returns the report computes volatility, Sharpe and Sortino from; below it they
/// are `None`. A sample standard deviation from a handful of days is mostly noise, and annualising
/// it by `sqrt(365)` turns a lucky day or two into a ratio that looks exceptional (a 26-hour run
/// gave a Sharpe of 12.5). Thirty, about a month of daily returns, is a common rule-of-thumb
/// minimum for a sample standard deviation.
///
/// The daily returns are taken on the UTC calendar days the equity curve spans (see
/// [`daily_equity`]): a day inside a data gap, between two walk-forward test windows in a stitched
/// curve, or inside a bar longer than a day counts as a daily return of zero. The floor counts
/// only the daily returns on days that hold a bar close ([`observed_daily_returns`]), so a curve
/// cannot reach it on carried-forward zeros: 30 hourly bars on two days a month apart, or five
/// weekly bars spanning 35 days, have no ratios. That also means at least this many bars. Bars of
/// a day or shorter without gaps close on every day they span, so for them the two counts agree.
/// The zeros still count in the values themselves, where they pull Sharpe and Sortino toward zero
/// (and usually lower volatility) rather than inflate them.
pub const MIN_DAILY_RETURNS: usize = 30;

/// The shortest run, from the first bar's open to the last bar's close, the report computes CAGR
/// (and so Calmar) for; shorter runs have `None`. Compounding a short run's return to a full year
/// raises it to the power of `365 / days`, so a few percent over a day reads as thousands of
/// percent a year (a 26-hour run gave a CAGR of 3,165%). Ninety days, a quarter, keeps that power
/// at about 4 or below.
pub const MIN_CAGR_DAYS: i64 = 90;

/// The metrics report for one equity series and its trades.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsReport {
    pub start_equity: f64,
    pub end_equity: f64,
    /// See [`total_return`].
    pub total_return: Option<f64>,
    /// See [`cagr`]. `None` for a run shorter than [`MIN_CAGR_DAYS`].
    pub cagr: Option<f64>,
    /// Annualised volatility of daily returns; see [`volatility`] and [`daily_equity`]. `None`
    /// with fewer than [`MIN_DAILY_RETURNS`] daily returns on days with a bar close, as are
    /// Sharpe and Sortino.
    pub volatility: Option<f64>,
    /// Annualised Sharpe ratio of daily returns; see [`sharpe`].
    pub sharpe: Option<f64>,
    /// Annualised Sortino ratio of daily returns; see [`sortino`].
    pub sortino: Option<f64>,
    /// Depth of the largest drawdown as a fraction of its peak; see [`max_drawdown`].
    pub max_drawdown: Option<f64>,
    /// From the largest drawdown's peak to its recovery, or to the end if it never recovered.
    pub max_drawdown_duration_ms: Option<Millis>,
    pub max_drawdown_duration_bars: Option<usize>,
    /// See [`calmar`]. `None` whenever CAGR is, so for a run shorter than [`MIN_CAGR_DAYS`].
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
    /// The drawdown from the running peak measured inside bars: see
    /// [`risk::trailing_drawdown`], with each bar's equity range. `None` when the report was
    /// built without ranges (see [`report_with`]), and in a result saved before reports had it.
    #[serde(default)]
    pub max_drawdown_intrabar: Option<f64>,
    /// The fall from the starting equity to the lowest close, as a fraction of the starting
    /// equity; see [`risk::static_drawdown`].
    #[serde(default)]
    pub static_drawdown: Option<f64>,
    /// [`MetricsReport::static_drawdown`] to the lowest equity inside any bar. `None` without
    /// ranges.
    #[serde(default)]
    pub static_drawdown_intrabar: Option<f64>,
    /// The largest loss from one UTC day's start to its end, as a fraction of the starting
    /// equity; see [`risk::worst_daily_loss`]. Other day rollovers are in [`risk::daily_pnl`].
    #[serde(default)]
    pub worst_daily_loss: Option<f64>,
    /// The largest fall from one UTC day's start to its lowest equity inside a bar, as a
    /// fraction of the starting equity; see [`risk::worst_daily_loss_intraday`]. `None` without
    /// ranges.
    #[serde(default)]
    pub worst_daily_loss_intrabar: Option<f64>,
    /// See [`risk::Intrabar::peak_leverage`]. `None` without ranges.
    #[serde(default)]
    pub peak_leverage: Option<f64>,
    /// Mean R multiple of the closed trades that have one (see [`crate::attach_stops`]); trades
    /// without a stop are left out. The P&L expectancy per trade is `avg_trade_pnl`.
    #[serde(default)]
    pub expectancy_r: Option<f64>,
    /// See [`payoff_ratio`].
    #[serde(default)]
    pub payoff_ratio: Option<f64>,
    /// See [`longest_losing_streak`].
    #[serde(default)]
    pub longest_losing_streak: usize,
    /// Closed trades the end-of-run close-out closed (see [`Trade::force_closed`]): at most one
    /// in a backtest with `close_at_end`, up to one per test window in a walk-forward.
    #[serde(default)]
    pub force_closed_trades: usize,
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

/// Builds the full report, with the floors [`MIN_DAILY_RETURNS`] and [`MIN_CAGR_DAYS`], without
/// the measures inside bars: [`report_with`] and no ranges.
pub fn report(
    series: &EquitySeries,
    positions: &[f64],
    trades: &[Trade],
    fills: &[Fill],
    funding_paid: f64,
) -> MetricsReport {
    report_with(series, None, positions, trades, fills, funding_paid)
}

/// Builds the full report, with the floors [`MIN_DAILY_RETURNS`] and [`MIN_CAGR_DAYS`].
///
/// `intrabar` holds each bar's equity range and the peak leverage (see [`risk::intrabar`]), one
/// range per bar; without it, or with the wrong number of ranges, the measures inside bars are
/// `None`. `positions` is the position after each bar's fills, one per bar. `trades` may include
/// an open trade at the end; the trade statistics skip it. `funding_paid` is the run's net
/// funding.
pub fn report_with(
    series: &EquitySeries,
    intrabar: Option<&Intrabar>,
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
    // The observed days are among the calendar days and each holds a bar close, so this floor also
    // means at least as many daily returns and bars.
    let returns = per_bar_returns(&daily_equity(&series.times, values))
        .filter(|_| observed_daily_returns(&series.times) >= MIN_DAILY_RETURNS);
    let returns = returns.as_deref();
    let growth = if elapsed >= MIN_CAGR_DAYS * DAY_MS {
        cagr(values, elapsed)
    } else {
        None
    };
    let dd = max_drawdown(&series.times, values);
    let closed: Vec<&Trade> = trades.iter().filter(|t| !t.open).collect();
    let pnls: Vec<f64> = closed.iter().map(|t| t.net_pnl).collect();
    let held: Vec<f64> = closed.iter().map(|t| t.bars_held as f64).collect();
    let rs: Vec<f64> = closed.iter().filter_map(|t| t.r_multiple).collect();
    let start = values.first().copied().unwrap_or(0.0);
    let ranges = intrabar
        .map(|x| x.ranges.as_slice())
        .filter(|r| r.len() + 1 == values.len());
    let utc = DayRollover::default();
    let days = risk::daily_pnl(series, None, utc);
    let days_intrabar = ranges.map(|r| risk::daily_pnl(series, Some(r), utc));
    MetricsReport {
        start_equity: values.first().copied().unwrap_or(0.0),
        end_equity: values.last().copied().unwrap_or(0.0),
        total_return: total_return(values),
        cagr: growth,
        volatility: returns.and_then(|r| volatility(r, DAYS_PER_YEAR)),
        sharpe: returns.and_then(|r| sharpe(r, DAYS_PER_YEAR)),
        sortino: returns.and_then(|r| sortino(r, DAYS_PER_YEAR)),
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
        max_drawdown_intrabar: ranges
            .and_then(|r| risk::trailing_drawdown(start, r))
            .map(|d| d.depth),
        static_drawdown: risk::static_drawdown(start, values.iter().copied()),
        static_drawdown_intrabar: ranges
            .and_then(|r| risk::static_drawdown(start, r.iter().map(|x| x.low))),
        worst_daily_loss: risk::worst_daily_loss(&days, start),
        worst_daily_loss_intrabar: days_intrabar
            .and_then(|d| risk::worst_daily_loss_intraday(&d, start)),
        peak_leverage: ranges.and(intrabar).and_then(|x| x.peak_leverage),
        expectancy_r: mean(&rs),
        payoff_ratio: payoff_ratio(&pnls),
        longest_losing_streak: longest_losing_streak(&pnls),
        force_closed_trades: closed.iter().filter(|t| t.force_closed).count(),
    }
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

/// The equity at each UTC day's end, for the daily returns: the first value (the starting
/// equity), then for each UTC day from the one holding the first time to the one holding the last
/// time, the last value whose time is at or before 00:00 UTC of the next day. A value stamped
/// exactly 00:00 is the end of the day before. A day with no value repeats the previous day's
/// equity. `times` must be ascending; the result is empty for an empty series or mismatched
/// lengths.
pub fn daily_equity(times: &[Millis], values: &[f64]) -> Vec<f64> {
    if values.is_empty() || times.len() != values.len() {
        return Vec::new();
    }
    let (first, last) = (times[0], times[times.len() - 1]);
    let mut daily = vec![values[0]];
    let mut i = 0;
    let mut day_end = first.div_euclid(DAY_MS) * DAY_MS + DAY_MS;
    while times.len() > 1 && day_end - DAY_MS < last {
        while i + 1 < times.len() && times[i + 1] <= day_end {
            i += 1;
        }
        daily.push(values[i]);
        day_end += DAY_MS;
    }
    daily
}

/// How many of the daily returns [`daily_equity`] forms over `times` fall on a UTC day that holds
/// one of the times after the first (a bar close): the days whose value is observed rather than
/// carried forward. A time stamped exactly 00:00 belongs to the day before, as in
/// [`daily_equity`]. `times` must be ascending.
pub fn observed_daily_returns(times: &[Millis]) -> usize {
    let mut days = times
        .get(1..)
        .unwrap_or(&[])
        .iter()
        .map(|t| (t - 1).div_euclid(DAY_MS));
    let Some(mut day) = days.next() else {
        return 0;
    };
    let mut count = 1;
    for d in days {
        if d != day {
            (day, count) = (d, count + 1);
        }
    }
    count
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

/// Annualised volatility: the sample standard deviation of the returns times
/// `sqrt(periods_per_year)`. The report passes daily returns and 365. `None` with fewer than two
/// returns or a result that is not finite.
pub fn volatility(returns: &[f64], periods_per_year: f64) -> Option<f64> {
    finite(sample_std(returns)? * periods_per_year.sqrt())
}

/// Annualised Sharpe ratio with a risk-free rate of zero:
/// `mean(r) / sample_std(r) * sqrt(periods_per_year)`, which is the annualised mean
/// `mean(r) * periods_per_year` over the annualised volatility. The report passes daily returns
/// and 365. `None` with fewer than two returns, zero volatility or a result that is not finite.
pub fn sharpe(returns: &[f64], periods_per_year: f64) -> Option<f64> {
    let sd = sample_std(returns)?;
    if sd > 0.0 {
        finite(mean_f(returns) / sd * periods_per_year.sqrt())
    } else {
        None
    }
}

/// Annualised Sortino ratio with a target return of zero:
/// `mean(r) / dd * sqrt(periods_per_year)`, where the downside deviation
/// `dd = sqrt(sum(min(r_i, 0)^2) / n)` is taken over all n periods, not only the losing ones.
/// The report passes daily returns and 365. `None` with fewer than two returns (as for Sharpe),
/// no losing period or a result that is not finite.
pub fn sortino(returns: &[f64], periods_per_year: f64) -> Option<f64> {
    if returns.len() < 2 {
        return None;
    }
    let down: f64 = returns.iter().map(|r| r.min(0.0).powi(2)).sum();
    let dd = (down / returns.len() as f64).sqrt();
    // An infinite downside deviation would give a misleading 0.
    if dd > 0.0 && dd.is_finite() {
        finite(mean_f(returns) / dd * periods_per_year.sqrt())
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

/// Mean winning trade over mean losing trade: `mean(p > 0) / |mean(p < 0)|` on net trade P&L.
/// With the win rate `w` it gives the expectancy per trade as `mean_loss * (w * payoff - (1 -
/// w))`. 0 when no trade won but some lost; `None` with no losing trade (including no trades),
/// as for the profit factor, or a result that is not finite.
pub fn payoff_ratio(pnls: &[f64]) -> Option<f64> {
    let wins: Vec<f64> = pnls.iter().copied().filter(|&p| p > 0.0).collect();
    let losses: Vec<f64> = pnls.iter().copied().filter(|&p| p < 0.0).collect();
    let loss = mean(&losses)?;
    finite(mean(&wins).unwrap_or(0.0) / -loss)
}

/// The most consecutive losing trades (net P&L below zero), in the order given, which for a
/// trade list is the order the trades closed. A break-even trade ends a streak, as it is not a
/// loss. 0 with no losing trade.
pub fn longest_losing_streak(pnls: &[f64]) -> usize {
    let (mut run, mut longest) = (0, 0);
    for &p in pnls {
        run = if p < 0.0 { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    longest
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

/// The alpha and beta of a strategy's returns against a benchmark's over the same periods.
///
/// Beta is the slope of the ordinary least-squares fit of the strategy's returns on the
/// benchmark's, `cov(s, b) / var(b)`; alpha is its intercept, `mean(s) - beta * mean(b)`, the
/// return per period the benchmark does not explain, annualised by multiplying by
/// `periods_per_year` (simple, not compounded), with a risk-free rate of zero.
/// `(alpha, beta)`; `None` if the lengths differ, there are fewer than two returns, the benchmark
/// is constant, or either value is not finite.
pub fn alpha_beta(
    strategy: &[f64],
    benchmark: &[f64],
    periods_per_year: f64,
) -> Option<(f64, f64)> {
    if strategy.len() != benchmark.len() || strategy.len() < 2 {
        return None;
    }
    let (ms, mb) = (mean_f(strategy), mean_f(benchmark));
    let (mut cov, mut vb) = (0.0, 0.0);
    for (s, b) in strategy.iter().zip(benchmark) {
        cov += (s - ms) * (b - mb);
        vb += (b - mb) * (b - mb);
    }
    // An infinite variance would give a misleading beta of 0.
    if vb <= 0.0 || !vb.is_finite() {
        return None;
    }
    let beta = finite(cov / vb)?;
    let alpha = finite((ms - beta * mb) * periods_per_year)?;
    Some((alpha, beta))
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

    const HOUR: Millis = 3_600_000;

    /// Equity 110, 99, 108.9, 119.79, ... after a start of 100: alternately +10% and -10%.
    fn zigzag(n: usize) -> Vec<f64> {
        let mut e = 100.0;
        (0..n)
            .map(|i| {
                e *= if i % 2 == 0 { 1.1 } else { 0.9 };
                e
            })
            .collect()
    }

    /// A report on hourly bars opening at `hours` (from midnight UTC) with the [`zigzag`] equity.
    fn hourly_report(hours: &[i64]) -> MetricsReport {
        let curve: Vec<EquityPoint> = hours
            .iter()
            .zip(zigzag(hours.len()))
            .map(|(&h, equity)| EquityPoint {
                time: h * HOUR,
                equity,
            })
            .collect();
        let series = EquitySeries::new(100.0, &curve, Timeframe::H1);
        report(&series, &[], &[], &[], 0.0)
    }

    #[test]
    fn the_floor_counts_daily_returns_on_days_with_a_bar_close() {
        let risk = |r: &MetricsReport| (r.volatility, r.sharpe, r.sortino);
        // 30 hourly bars, 15 on day 0 and 15 on day 29: 30 daily returns and 30 bars, but only
        // two days hold a bar close; the other 28 returns are carried-forward zeros.
        let hours: Vec<i64> = (0..15).chain(29 * 24..29 * 24 + 15).collect();
        let r = hourly_report(&hours);
        assert_eq!(risk(&r), (None, None, None));
        let mut times = vec![0];
        times.extend(hours.iter().map(|h| (h + 1) * HOUR));
        assert_eq!(daily_equity(&times, &vec![1.0; times.len()]).len() - 1, 30);
        assert_eq!(observed_daily_returns(&times), 2);
        // Without gaps, 697 hourly bars from midnight close on 30 days, and 696 on 29.
        assert!(
            risk(&hourly_report(&(0..697).collect::<Vec<_>>()))
                .1
                .is_some()
        );
        assert_eq!(
            risk(&hourly_report(&(0..696).collect::<Vec<_>>())),
            (None, None, None)
        );
        // The multi-day series closes on days 0, 1 and 3 of its four daily returns.
        let s = multi_day_series();
        assert_eq!(observed_daily_returns(&s.times), 3);
        assert_eq!(observed_daily_returns(&[HOUR]), 0);
        assert_eq!(observed_daily_returns(&[]), 0);
    }

    #[test]
    fn a_return_across_a_data_gap_is_not_a_one_bar_return() {
        // 31 hourly bars, one a day with 23-hour gaps between, against the same bars back to back.
        let contiguous = hourly_report(&(0..31).collect::<Vec<_>>());
        let gapped = hourly_report(&(0..31).map(|d| d * 24).collect::<Vec<_>>());
        let risk = |r: &MetricsReport| (r.volatility, r.sharpe, r.sortino);
        // The contiguous run closes on two UTC days: two daily returns, too few for the ratios.
        assert_eq!(risk(&contiguous), (None, None, None));
        // The gapped run closes on 31 days, so its daily returns are the bar returns: 0.1, -0.1,
        // 0.1, ... (31 of them).
        let mut values = vec![100.0];
        values.extend(zigzag(31));
        let r = per_bar_returns(&values).unwrap();
        assert_eq!(r.len(), 31);
        approx(r[0], 0.1);
        approx(r[1], -0.1);
        assert_eq!(gapped.volatility, volatility(&r, 365.0));
        assert_eq!(gapped.sharpe, sharpe(&r, 365.0));
        assert_eq!(gapped.sortino, sortino(&r, 365.0));
        // 16 returns of 0.1 and 15 of -0.1: mean 1/310, downside sqrt(15 * 0.01 / 31).
        some(
            gapped.sortino,
            (1.0 / 310.0) / (0.15f64 / 31.0).sqrt() * 365f64.sqrt(),
        );
    }

    // From 12:00 on day 0: a value at 18:00, one at exactly 00:00 on day 1 (the end of day 0),
    // two on day 1, none on day 2, two on day 3, the last at 23:00 (a partial last day).
    fn multi_day_series() -> EquitySeries {
        EquitySeries {
            times: vec![
                12 * HOUR,
                18 * HOUR,
                DAY,
                DAY + 12 * HOUR,
                DAY + 20 * HOUR,
                3 * DAY + 6 * HOUR,
                3 * DAY + 23 * HOUR,
            ],
            values: vec![100.0, 104.0, 110.0, 105.0, 99.0, 120.0, 108.9],
        }
    }

    #[test]
    fn daily_equity_samples_each_utc_day_end_and_carries_a_missing_day() {
        let s = multi_day_series();
        // Start, end of day 0 (the 00:00 value), day 1, day 2 carried from day 1, day 3.
        assert_eq!(
            daily_equity(&s.times, &s.values),
            vec![100.0, 110.0, 99.0, 99.0, 108.9]
        );
        // A daily bar series samples one value per bar.
        let times: Vec<Millis> = (0..4).map(|d| d * DAY).collect();
        assert_eq!(
            daily_equity(&times, &[1.0, 2.0, 3.0, 4.0]),
            vec![1.0, 2.0, 3.0, 4.0]
        );
        assert_eq!(daily_equity(&[], &[]), Vec::<f64>::new());
        assert_eq!(daily_equity(&[HOUR], &[1.0]), vec![1.0]);
    }

    #[test]
    fn report_risk_metrics_are_annualised_daily_returns() {
        // The multi-day series, then one value at noon on each of days 4 to 30, alternately up 1%
        // and down 0.5%: 31 daily returns in all, 30 of them on days with a value (day 2 has
        // none). The carried day stays in the returns as a zero.
        let mut s = multi_day_series();
        let mut daily = vec![100.0, 110.0, 99.0, 99.0, 108.9];
        let mut e = 108.9;
        for d in 4..31 {
            e *= if d % 2 == 0 { 1.01 } else { 0.995 };
            s.times.push(d * DAY + 12 * HOUR);
            s.values.push(e);
            daily.push(e);
        }
        assert_eq!(daily_equity(&s.times, &s.values), daily);
        assert_eq!(observed_daily_returns(&s.times), 30);
        let r = report(&s, &[], &[], &[], 0.0);
        let returns = per_bar_returns(&daily).unwrap();
        assert_eq!(returns.len(), 31);
        assert_eq!(returns[2], 0.0);
        // A day less: 30 daily returns, but 29 on days with a value, so below the floor.
        let short = EquitySeries {
            times: s.times[..s.times.len() - 1].to_vec(),
            values: s.values[..s.values.len() - 1].to_vec(),
        };
        let rs = report(&short, &[], &[], &[], 0.0);
        assert_eq!((rs.volatility, rs.sharpe, rs.sortino), (None, None, None));
        assert_eq!(r.volatility, volatility(&returns, 365.0));
        assert_eq!(r.sharpe, sharpe(&returns, 365.0));
        assert_eq!(r.sortino, sortino(&returns, 365.0));
        assert!(r.sharpe.is_some() && r.sortino.is_some());
        // The first four days alone are below the floor.
        let r4 = report(&multi_day_series(), &[], &[], &[], 0.0);
        assert_eq!((r4.volatility, r4.sharpe, r4.sortino), (None, None, None));
        // The drawdown stays on the per-bar series: 110 at 00:00 on day 1 to 99 at 20:00,
        // recovered at 120 three values after the peak.
        some(r.max_drawdown, 0.1);
        assert_eq!(r.max_drawdown_duration_bars, Some(3));
    }

    #[test]
    fn a_run_inside_one_utc_day_has_no_risk_metrics() {
        // 24 hourly bars from midnight close from 01:00 to 24:00, all on day 0: one daily return.
        let curve: Vec<EquityPoint> = (0..24)
            .map(|h| EquityPoint {
                time: h * HOUR,
                equity: 100.0 + (h % 3) as f64,
            })
            .collect();
        let series = EquitySeries::new(100.0, &curve, Timeframe::H1);
        let r = report(&series, &[], &[], &[], 0.0);
        assert_eq!((r.volatility, r.sharpe, r.sortino), (None, None, None));
        assert!(r.max_drawdown.is_some());
    }

    /// A report on daily bars from day 0: equity alternates +2% and -1% after a start of 100,
    /// so every daily return is defined and some lose.
    fn daily_report(days: i64) -> MetricsReport {
        let mut equity = 100.0;
        let curve: Vec<EquityPoint> = (0..days)
            .map(|d| {
                equity *= if d % 2 == 0 { 1.02 } else { 0.99 };
                EquityPoint {
                    time: d * DAY,
                    equity,
                }
            })
            .collect();
        report(
            &EquitySeries::new(100.0, &curve, Timeframe::D1),
            &[],
            &[],
            &[],
            0.0,
        )
    }

    #[test]
    fn a_26_hour_run_has_no_risk_ratios_and_no_cagr() {
        // Hourly bars from midnight for 26 hours, rising 0.3% a bar: two daily returns.
        let curve: Vec<EquityPoint> = (0..26)
            .map(|h| EquityPoint {
                time: h * HOUR,
                equity: 100.0 * 1.003f64.powi(h as i32 + 1) - if h % 5 == 0 { 0.4 } else { 0.0 },
            })
            .collect();
        let r = report(
            &EquitySeries::new(100.0, &curve, Timeframe::H1),
            &[],
            &[],
            &[],
            0.0,
        );
        assert_eq!(
            (r.volatility, r.sharpe, r.sortino, r.cagr, r.calmar),
            (None, None, None, None, None)
        );
        // What does not annualise is still there.
        assert!(r.total_return.unwrap() > 0.0);
        assert!(r.max_drawdown.unwrap() > 0.0);
    }

    #[test]
    fn volatility_sharpe_and_sortino_need_30_daily_returns() {
        let short = daily_report(29);
        assert_eq!(
            (short.volatility, short.sharpe, short.sortino),
            (None, None, None)
        );
        let long = daily_report(30);
        let series = |days: i64| {
            let mut e = 100.0;
            let mut v = vec![100.0];
            for d in 0..days {
                e *= if d % 2 == 0 { 1.02 } else { 0.99 };
                v.push(e);
            }
            per_bar_returns(&v).unwrap()
        };
        let r = series(30);
        assert_eq!(r.len(), 30);
        assert_eq!(long.volatility, volatility(&r, DAYS_PER_YEAR));
        assert_eq!(long.sharpe, sharpe(&r, DAYS_PER_YEAR));
        assert_eq!(long.sortino, sortino(&r, DAYS_PER_YEAR));
        assert!(long.sharpe.is_some() && long.sortino.is_some() && long.volatility.is_some());
    }

    /// [`daily_report`] on weekly bars: the same alternating equity, one value a week.
    fn weekly_series(weeks: i64) -> EquitySeries {
        let mut equity = 100.0;
        let curve: Vec<EquityPoint> = (0..weeks)
            .map(|w| {
                equity *= if w % 2 == 0 { 1.02 } else { 0.99 };
                EquityPoint {
                    time: w * 7 * DAY,
                    equity,
                }
            })
            .collect();
        EquitySeries::new(100.0, &curve, Timeframe::W1)
    }

    #[test]
    fn weekly_bars_need_30_bars_as_well_as_30_daily_returns() {
        let risk = |r: &MetricsReport| (r.volatility, r.sharpe, r.sortino);
        // Five weekly bars span 35 days, so 35 daily returns, but 30 of them are carried-forward
        // zeros: the ratios would come from five weekly moves.
        let five = weekly_series(5);
        assert_eq!(daily_equity(&five.times, &five.values).len() - 1, 35);
        assert_eq!(risk(&report(&five, &[], &[], &[], 0.0)), (None, None, None));
        assert_eq!(
            risk(&report(&weekly_series(29), &[], &[], &[], 0.0)),
            (None, None, None)
        );
        // Thirty weekly bars are 30 bar returns over 210 daily returns.
        let thirty = weekly_series(30);
        let r = report(&thirty, &[], &[], &[], 0.0);
        let daily = per_bar_returns(&daily_equity(&thirty.times, &thirty.values)).unwrap();
        assert_eq!(daily.len(), 210);
        assert_eq!(r.volatility, volatility(&daily, DAYS_PER_YEAR));
        assert_eq!(r.sharpe, sharpe(&daily, DAYS_PER_YEAR));
        assert_eq!(r.sortino, sortino(&daily, DAYS_PER_YEAR));
        assert!(r.sharpe.is_some() && r.sortino.is_some() && r.volatility.is_some());
    }

    #[test]
    fn cagr_and_calmar_need_90_days() {
        // 89 daily bars span 89 days from the first open to the last close; 90 span 90.
        let short = daily_report(89);
        assert_eq!((short.cagr, short.calmar), (None, None));
        let long = daily_report(90);
        let expected = cagr(&[100.0, long.end_equity], 90 * DAY);
        assert!(expected.is_some());
        assert_eq!(long.cagr, expected);
        assert_eq!(long.calmar, calmar(expected, long.max_drawdown));
        assert!(long.calmar.is_some());
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

    #[test]
    fn payoff_ratio_and_losing_streak() {
        // Wins 10 and 8 average 9; losses 5, 3, 4, 2 and 1 average 3: payoff 3. The longest run
        // of losses is -5, -3, -4; the break-even trade ends it before -2, -1.
        let pnls = [10.0, -5.0, -3.0, -4.0, 0.0, -2.0, -1.0, 8.0];
        some(payoff_ratio(&pnls), 3.0);
        assert_eq!(longest_losing_streak(&pnls), 3);
        // No wins: 0. No losses, or no trades: None, as for the profit factor.
        some(payoff_ratio(&[-1.0, -2.0]), 0.0);
        assert_eq!(payoff_ratio(&[1.0, 0.0]), None);
        assert_eq!(payoff_ratio(&[]), None);
        assert_eq!(longest_losing_streak(&[1.0, 0.0]), 0);
        assert_eq!(longest_losing_streak(&[-1.0]), 1);
        assert_eq!(longest_losing_streak(&[]), 0);
    }

    #[test]
    fn alpha_and_beta_are_the_least_squares_fit_on_the_benchmark() {
        // Benchmark mean 0, strategy mean 0.01. cov = 0.02 * 0.01 + (-0.01) * (-0.01) + 0 =
        // 0.0003, var(b) = 0.0002: beta 1.5. Alpha 0.01 - 1.5 * 0 per period, 0.12 over 12.
        let (alpha, beta) = alpha_beta(&[0.03, 0.0, 0.0], &[0.01, -0.01, 0.0], 12.0).unwrap();
        approx(beta, 1.5);
        approx(alpha, 0.12);
        // Twice the benchmark plus 0.001 a period: beta 2, alpha 0.001 * 365.
        let b = [0.01, -0.02, 0.03, 0.0];
        let s: Vec<f64> = b.iter().map(|x| 2.0 * x + 0.001).collect();
        let (alpha, beta) = alpha_beta(&s, &b, 365.0).unwrap();
        approx(beta, 2.0);
        approx(alpha, 0.365);
        // A constant benchmark, unequal lengths or one return: None.
        assert_eq!(alpha_beta(&[0.1, 0.2], &[0.01, 0.01], 1.0), None);
        assert_eq!(alpha_beta(&[0.1, 0.2], &[0.01, 0.02, 0.03], 1.0), None);
        assert_eq!(alpha_beta(&[0.1], &[0.01], 1.0), None);
    }

    fn trade(net_pnl: f64, r_multiple: Option<f64>, open: bool, force_closed: bool) -> Trade {
        Trade {
            side: crate::TradeSide::Long,
            entry_time: 0,
            exit_time: 0,
            open,
            qty: 1.0,
            entry_price: 100.0,
            exit_price: 100.0 + net_pnl,
            gross_pnl: net_pnl,
            fees: 0.0,
            funding: 0.0,
            net_pnl,
            return_on_notional: net_pnl / 100.0,
            bars_held: 1,
            stop: r_multiple.map(|_| 90.0),
            r_multiple,
            mae: None,
            mfe: None,
            force_closed,
        }
    }

    #[test]
    fn report_has_the_risk_and_trade_measures() {
        // Closes 104 at 12:00, 96 at the end of day 0, 99 and 110 on day 1.
        let series = EquitySeries {
            times: vec![0, 12 * HOUR, DAY, DAY + 12 * HOUR, 2 * DAY],
            values: vec![100.0, 104.0, 96.0, 99.0, 110.0],
        };
        let range = |low, high| crate::EquityRange { low, high };
        let intrabar = Intrabar {
            ranges: vec![
                range(98.0, 105.0),
                range(94.0, 106.0),
                range(95.0, 100.0),
                range(97.0, 112.0),
            ],
            peak_leverage: Some(2.5),
        };
        let trades = [
            trade(10.0, Some(2.0), false, false),
            trade(-5.0, Some(-1.0), false, false),
            trade(-3.0, None, false, false),
            trade(-4.0, None, false, false),
            trade(0.0, None, false, false),
            trade(-2.0, None, false, false),
            trade(-1.0, None, false, false),
            trade(8.0, Some(2.0), false, true),
            // Open trades count in none of the trade statistics.
            trade(-50.0, Some(9.0), true, true),
        ];
        let r = report_with(&series, Some(&intrabar), &[0.0; 4], &trades, &[], 0.0);

        // On closes the peak is 104 and the trough 96.
        some(r.max_drawdown, 1.0 - 96.0 / 104.0);
        // Inside bars each bar's high comes before its low: the deepest is from bar 3's high
        // 112 to its low 97.
        some(r.max_drawdown_intrabar, 1.0 - 97.0 / 112.0);
        // From the starting 100: the lowest close is 96, the lowest low 94.
        some(r.static_drawdown, 0.04);
        some(r.static_drawdown_intrabar, 0.06);
        // Day 0 runs 100 to 96 (the 00:00 close ends it), down to 94 inside it; day 1 runs 96
        // to 110, down to 95. Both of the starting 100.
        some(r.worst_daily_loss, 0.04);
        some(r.worst_daily_loss_intrabar, 0.06);
        some(r.peak_leverage, 2.5);
        // R of 2, -1 and 2 among the closed trades with one.
        some(r.expectancy_r, 1.0);
        some(r.payoff_ratio, 3.0);
        assert_eq!(r.longest_losing_streak, 3);
        assert_eq!(r.force_closed_trades, 1);
        assert_eq!(r.trades, 8);

        // Without ranges, or with the wrong number, the measures inside bars are None and the
        // rest is unchanged.
        let short = Intrabar {
            ranges: intrabar.ranges[..3].to_vec(),
            peak_leverage: Some(2.5),
        };
        for without in [
            report_with(&series, None, &[0.0; 4], &trades, &[], 0.0),
            report_with(&series, Some(&short), &[0.0; 4], &trades, &[], 0.0),
            report(&series, &[0.0; 4], &trades, &[], 0.0),
        ] {
            assert_eq!(without.max_drawdown_intrabar, None);
            assert_eq!(without.static_drawdown_intrabar, None);
            assert_eq!(without.worst_daily_loss_intrabar, None);
            assert_eq!(without.peak_leverage, None);
            assert_eq!(without.worst_daily_loss, r.worst_daily_loss);
            assert_eq!(without.static_drawdown, r.static_drawdown);
            assert_eq!(without.expectancy_r, r.expectancy_r);
        }
    }
}
