//! The backtest runner: one configuration and its data in, a full result out.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

use td_core::{
    Candle, FeeSchedule, Fill, Instrument, InstrumentKind, Millis, OrderId, Side, Timeframe,
};

use crate::metrics::{self, EquitySeries, MetricsReport};
use crate::session::{funding_ranges, sorted_funding};
use crate::{
    BarFunding, CandleFillModel, EngineError, EquityPoint, Order, Params, Portfolio, Session,
    SessionConfig, Trade, build_strategy, build_trades,
};

/// The revision of the engine's behaviour, stored in every result so a rerun can tell an engine
/// change from a data change.
///
/// Bump it whenever the engine's behaviour changes: fills, fees, funding, the trade list, metrics
/// or the benchmark, including changes the expected fixture does not cover (it has no gaps, for
/// example). Order ids, and which orders are accepted or rejected, count as behaviour too: paper
/// sessions save manual orders by id, so a change there shifts those ids in saved sessions. A
/// regenerated `tests/fixtures/backtest_expected.json` always needs a bump, but an unchanged
/// fixture does not mean the behaviour is unchanged.
pub const ENGINE_REVISION: u32 = 4;

/// Everything that defines a backtest apart from the data itself.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestConfig {
    /// A name from the strategy registry.
    pub strategy: String,
    /// Parameter values; missing ones take the strategy's defaults.
    #[serde(default)]
    pub params: Params,
    pub instrument: Instrument,
    pub timeframe: Timeframe,
    /// Bars that open at or after `from` and before `to` are used.
    pub from: Millis,
    pub to: Millis,
    /// Starting balance in the quote asset.
    pub starting_cash: f64,
    /// Slippage on market and stop fills, in basis points of price.
    pub slippage_bps: f64,
    /// Replaces the instrument's fee schedule when set.
    #[serde(default)]
    pub fee_override: Option<FeeSchedule>,
    /// The largest perpetual position allowed, as its notional over equity: 1.0 means 1x, the
    /// default. At 1x a long is liquidated only near a price of zero and a short only once the
    /// price has about doubled (see [`Session::on_bar`]). Orders that would take the position
    /// over it are rejected when submitted; orders that reduce the position are always allowed
    /// (see [`crate::Context::submit`]). Spot ignores it. Left out of the saved result when it is
    /// the default.
    #[serde(
        default = "default_max_leverage",
        skip_serializing_if = "is_default_max_leverage"
    )]
    pub max_leverage: f64,
    /// When set, bars that open at or after `warmup_from` and before `from` are fed to the
    /// strategy first with trading disabled (see [`Session::warm_up`]), so its indicators are
    /// ready at the first traded bar. They add no equity points and are not in the metrics;
    /// orders the strategy submits on them are rejected and recorded in the result's `orders`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warmup_from: Option<Millis>,
    /// When set, a position still open at the end is closed with a real fill: at the close of
    /// the second-to-last traded bar every open order is cancelled and a reduce-only market order
    /// for the whole position is submitted, so it fills at the last bar's open with slippage and
    /// the taker fee, like any market order. Orders the strategy submits from then on never fill.
    /// The trade list then ends with a closed trade instead of an open one marked at the last
    /// close. The strategy tester sets it on out-of-sample runs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub close_at_end: bool,
}

fn default_max_leverage() -> f64 {
    1.0
}

fn is_default_max_leverage(max_leverage: &f64) -> bool {
    *max_leverage == default_max_leverage()
}

/// Buy and hold of the same instrument over the same bars: a backtest's traded bars, or in a
/// walk-forward each test window's bars, stitched like the test runs (see
/// [`crate::WalkForwardResult::benchmark`]).
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Benchmark {
    /// Quantity bought at the first bar's open: all starting cash, after the taker fee and any
    /// per-unit commission, rounded down to the lot size. In a walk-forward, the first test window's purchase.
    pub qty: f64,
    /// The first bar's open with slippage, rounded up to the tick. In a walk-forward, the first
    /// test window's.
    pub entry_price: f64,
    pub equity: Vec<EquityPoint>,
    pub metrics: MetricsReport,
    /// Strategy total return minus benchmark total return.
    pub excess_return: Option<f64>,
    /// Pearson correlation of the strategy's per-bar returns with the benchmark's over the same
    /// intervals: the benchmark's equity is read at each time of the strategy's equity series.
    /// In a backtest the two have the same bars, and in a walk-forward the same test windows, so
    /// the times match one to one.
    pub correlation: Option<f64>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestResult {
    pub config: BacktestConfig,
    /// The td-engine crate version that produced the result.
    pub engine_version: String,
    /// The [`ENGINE_REVISION`] that produced the result.
    pub engine_revision: u32,
    /// SHA-256 of the bars (warm-up bars first), asks and funding the run used; see
    /// [`data_fingerprint`].
    pub data_fingerprint: String,
    /// Equity at each bar's close.
    pub equity: Vec<EquityPoint>,
    /// Every order submitted, rejected ones included (warm-up bars' too), in submission order.
    pub orders: Vec<Order>,
    pub fills: Vec<Fill>,
    pub trades: Vec<Trade>,
    pub metrics: MetricsReport,
    pub benchmark: Benchmark,
}

/// A saved configuration run again.
#[derive(Debug, Clone, PartialEq)]
pub struct Rerun {
    pub result: BacktestResult,
    /// Whether the data used this time hashes the same as the data of the saved run.
    pub fingerprint_matches: bool,
    /// Whether this engine has the saved run's [`ENGINE_REVISION`]. When it does not, a different
    /// result on the same data may come from the engine rather than the data.
    pub engine_matches: bool,
}

/// Runs a backtest.
///
/// `bars` may cover more than the configured range; only bars inside `from..to` are traded, and
/// bars inside `warmup_from..from` warm the strategy up first. `asks` is the ask series beside
/// `bars`, which then hold the bid, matched to them by open time: empty for a kind whose rules have
/// no quotes, and for a kind with quotes one for every bar used (warm-up bars too), never filled in
/// from the bid. An ask that matches no bar in `bars` is an error. `funding` is `(time, rate)` in
/// any order. An event in a gap between traded bars is charged at the next bar's open, like one due at
/// that open; events before the first traded bar or from the last traded bar's end on are ignored.
///
/// The strategy is built through the registry and run on a [`Session`], then the trade list,
/// metrics and buy-and-hold benchmark are derived from the session's record.
///
/// The caller loads the candles and funding, and is responsible for reporting any buckets the
/// data layer dropped as invalid; the result does not record them.
pub fn run(
    config: &BacktestConfig,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
) -> Result<BacktestResult, EngineError> {
    let (warmup, traded) = select_bars(config, bars)?;
    let mut used_asks = match_asks(config.instrument.id.kind, bars, asks, &warmup)?;
    let traded_asks = match_asks(config.instrument.id.kind, bars, asks, &traded)?;
    used_asks.extend_from_slice(&traded_asks);
    let bars = traded;
    let tf = config.timeframe;
    let funding = sorted_funding(funding);
    let ranges = funding_ranges(tf, &bars, &funding);

    let strategy = build_strategy(&config.strategy, &config.params)?;
    let session_config = SessionConfig {
        starting_cash: config.starting_cash,
        slippage_bps: config.slippage_bps,
        fees: config.fee_override,
        max_leverage: config.max_leverage,
    };
    let mut session = Session::new(strategy, config.instrument.clone(), tf, session_config)?;
    for bar in &warmup {
        session.warm_up(*bar, &[])?;
    }
    let mut positions = Vec::with_capacity(bars.len());
    let mut used_funding = Vec::new();
    // The bar whose close decides the close-out, so it fills at the last bar's open.
    let close_out_at = bars.len().checked_sub(2).filter(|_| config.close_at_end);
    for (i, ((bar, ask), range)) in bars.iter().zip(&traded_asks).zip(&ranges).enumerate() {
        let events = &funding[range.clone()];
        session.on_bar_with_ask(*bar, *ask, events)?;
        positions.push(session.portfolio().position());
        used_funding.extend_from_slice(events);
        if close_out_at == Some(i) {
            session.close_out();
        }
    }

    let fills = session.fills().to_vec();
    let trades = build_trades(&fills, &bars, session.bar_funding());
    let series = EquitySeries::new(config.starting_cash, session.equity(), tf);
    let report = metrics::report(
        &series,
        &positions,
        &trades,
        &fills,
        session.portfolio().funding_paid(),
    );
    let benchmark = benchmark(
        config,
        &bars,
        &traded_asks,
        &funding,
        &ranges,
        &series,
        &report,
    );
    let mut used_bars = warmup;
    used_bars.extend_from_slice(&bars);
    let used_asks: Vec<Candle> = used_asks.into_iter().flatten().collect();

    Ok(BacktestResult {
        config: config.clone(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        engine_revision: ENGINE_REVISION,
        data_fingerprint: data_fingerprint(&used_bars, &used_asks, &used_funding),
        equity: session.equity().to_vec(),
        orders: session.orders().to_vec(),
        fills,
        trades,
        metrics: report,
        benchmark,
    })
}

/// The ask of each of `used`, from `asks` matched by open time, or `None` for every bar when the
/// kind has no quotes. `all` is every bar given to the run, which every ask must match. Errors for
/// asks given on a kind without quotes, an ask that matches no bar in `all`, and a bar in `used`
/// without an ask on a kind with quotes.
pub(crate) fn match_asks(
    kind: InstrumentKind,
    all: &[Candle],
    asks: &[Candle],
    used: &[Candle],
) -> Result<Vec<Option<Candle>>, EngineError> {
    if !kind.rules().quotes {
        if !asks.is_empty() {
            return Err(EngineError::AsksWithoutQuotes { kind });
        }
        return Ok(vec![None; used.len()]);
    }
    let times: HashSet<Millis> = all.iter().map(|b| b.open_time).collect();
    if let Some(ask) = asks.iter().find(|a| !times.contains(&a.open_time)) {
        return Err(EngineError::UnmatchedAsk {
            time: ask.open_time,
        });
    }
    let by_time: HashMap<Millis, Candle> = asks.iter().map(|a| (a.open_time, *a)).collect();
    used.iter()
        .map(|bar| match by_time.get(&bar.open_time) {
            Some(ask) => Ok(Some(*ask)),
            None => Err(EngineError::MissingAsk {
                time: bar.open_time,
                kind,
            }),
        })
        .collect()
}

/// The warm-up bars (`warmup_from..from`) and the traded bars (`from..to`) of `bars`, after
/// checking the range.
pub(crate) fn select_bars(
    config: &BacktestConfig,
    bars: &[Candle],
) -> Result<(Vec<Candle>, Vec<Candle>), EngineError> {
    if config.from >= config.to {
        return Err(EngineError::InvalidConfig("from must be before to"));
    }
    let warmup_from = config.warmup_from.unwrap_or(config.from);
    if warmup_from > config.from {
        return Err(EngineError::InvalidConfig(
            "warmup_from must not be after from",
        ));
    }
    let within = |from: Millis, to: Millis| -> Vec<Candle> {
        bars.iter()
            .filter(|b| (from..to).contains(&b.open_time))
            .copied()
            .collect()
    };
    let traded = within(config.from, config.to);
    if traded.is_empty() {
        return Err(EngineError::NoBarsInRange {
            from: config.from,
            to: config.to,
        });
    }
    Ok((within(warmup_from, config.from), traded))
}

/// The fingerprint [`run`] would record for `config` on this data, without running it. A tester
/// result's `data_fingerprint` is this over its base configuration, so comparing it with a saved
/// run's tells whether the data changed since, as [`rerun`] does for a backtest.
pub fn config_fingerprint(
    config: &BacktestConfig,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
) -> Result<String, EngineError> {
    let (mut used, traded) = select_bars(config, bars)?;
    let mut used_asks = match_asks(config.instrument.id.kind, bars, asks, &used)?;
    used_asks.extend(match_asks(config.instrument.id.kind, bars, asks, &traded)?);
    let used_asks: Vec<Candle> = used_asks.into_iter().flatten().collect();
    let funding = sorted_funding(funding);
    let used_funding: Vec<(Millis, f64)> = funding_ranges(config.timeframe, &traded, &funding)
        .into_iter()
        .flat_map(|r| funding[r].iter().copied())
        .collect();
    used.extend_from_slice(&traded);
    Ok(data_fingerprint(&used, &used_asks, &used_funding))
}

/// Runs a saved result's configuration again on `bars`, `asks` and `funding`, as [`run`] takes
/// them.
pub fn rerun(
    saved: &BacktestResult,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
) -> Result<Rerun, EngineError> {
    let result = run(&saved.config, bars, asks, funding)?;
    let fingerprint_matches = result.data_fingerprint == saved.data_fingerprint;
    let engine_matches = result.engine_revision == saved.engine_revision;
    Ok(Rerun {
        result,
        fingerprint_matches,
        engine_matches,
    })
}

/// The record of buying and holding over one range, before it is compared with a strategy.
pub(crate) struct Held {
    pub(crate) qty: f64,
    pub(crate) entry_price: f64,
    /// One point per bar, stamped with the bar's open time.
    pub(crate) equity: Vec<EquityPoint>,
    /// The position after each bar's fills.
    pub(crate) positions: Vec<f64>,
    /// The one entry fill, or none if the cash buys less than a lot.
    pub(crate) fills: Vec<Fill>,
    /// The entry as one trade, still open at the end.
    pub(crate) trades: Vec<Trade>,
    pub(crate) funding_paid: f64,
}

/// [`hold`] over `bars`, the bars of `config`'s range, with their `asks` from [`match_asks`] and
/// `funding` in any order and the same rules as [`run`]: an event in a gap between bars is charged
/// at the next bar's open, and events before the first bar or from the last bar's end on are
/// ignored.
pub(crate) fn hold_over(
    config: &BacktestConfig,
    bars: &[Candle],
    asks: &[Option<Candle>],
    funding: &[(Millis, f64)],
) -> Held {
    let funding = sorted_funding(funding);
    let ranges = funding_ranges(config.timeframe, bars, &funding);
    hold(config, bars, asks, &funding, &ranges)
}

/// Buy and hold over `bars`, compared with the strategy: see [`hold`] and [`compare`].
fn benchmark(
    config: &BacktestConfig,
    bars: &[Candle],
    asks: &[Option<Candle>],
    funding: &[(Millis, f64)],
    ranges: &[std::ops::Range<usize>],
    strategy_series: &EquitySeries,
    strategy_report: &MetricsReport,
) -> Benchmark {
    let held = hold(config, bars, asks, funding, ranges);
    let series = EquitySeries::new(config.starting_cash, &held.equity, config.timeframe);
    let report = metrics::report(
        &series,
        &held.positions,
        &held.trades,
        &held.fills,
        held.funding_paid,
    );
    compare(
        held.qty,
        held.entry_price,
        held.equity,
        &series,
        report,
        strategy_series,
        strategy_report,
    )
}

/// The benchmark from buy and hold's curve and metrics, with the strategy's excess return over it
/// and the correlation of their per-bar returns.
pub(crate) fn compare(
    qty: f64,
    entry_price: f64,
    equity: Vec<EquityPoint>,
    series: &EquitySeries,
    report: MetricsReport,
    strategy_series: &EquitySeries,
    strategy_report: &MetricsReport,
) -> Benchmark {
    let excess_return = match (strategy_report.total_return, report.total_return) {
        (Some(s), Some(b)) => Some(s - b),
        _ => None,
    };
    Benchmark {
        qty,
        entry_price,
        equity,
        metrics: report,
        excess_return,
        correlation: aligned_correlation(strategy_series, series),
    }
}

/// Buys at the first bar's open with all starting cash (taker fee, per-unit commission, the run's
/// slippage) and holds to the last close, paying funding on a perpetual like the session does:
/// funding due at a bar's open or in the gap before it on the position held going into the bar,
/// at the open price, so none at the first open, and funding due later in the bar at the close.
/// With an ask series the purchase is at the first ask open, and the long is marked at the bid
/// closes, as the session marks a long. `asks` holds each bar's ask, `None` without quotes.
fn hold(
    config: &BacktestConfig,
    bars: &[Candle],
    asks: &[Option<Candle>],
    funding: &[(Millis, f64)],
    ranges: &[std::ops::Range<usize>],
) -> Held {
    let inst = &config.instrument;
    let fees = config.fee_override.unwrap_or(inst.fees);
    let model = CandleFillModel {
        slippage_bps: config.slippage_bps,
    };
    let first = bars[0];
    let entry = asks.first().copied().flatten().unwrap_or(first);
    let price = model.slip(entry.open, Side::Buy, inst);
    let qty = inst.round_qty(config.starting_cash / (price * (1.0 + fees.taker) + fees.per_unit));
    let mut portfolio = Portfolio::new(inst.id.kind, config.starting_cash);
    let mut fills = Vec::new();
    let mut equity = Vec::with_capacity(bars.len());
    let mut positions = Vec::with_capacity(bars.len());
    let mut bar_funding = Vec::with_capacity(bars.len());
    for (k, (bar, range)) in bars.iter().zip(ranges).enumerate() {
        let events = &funding[range.clone()];
        let mut charged = BarFunding::default();
        for &(_, rate) in events.iter().filter(|&&(time, _)| time <= bar.open_time) {
            charged.at_open += portfolio.charge_funding(rate, bar.open);
        }
        if k == 0 && qty.is_finite() && qty > 0.0 {
            let fill = Fill {
                order_id: OrderId(0),
                instrument: inst.id.clone(),
                side: Side::Buy,
                time: first.open_time,
                price,
                qty,
                fee: fees.fee(price, qty, fees.taker),
                liquidity: td_core::Liquidity::Taker,
            };
            portfolio.apply_fill(fill.side, fill.qty, fill.price, fill.fee);
            fills.push(fill);
        }
        for &(_, rate) in events.iter().filter(|&&(time, _)| time > bar.open_time) {
            charged.in_bar += portfolio.charge_funding(rate, bar.close);
        }
        bar_funding.push(charged);
        portfolio.mark(bar.close);
        equity.push(EquityPoint {
            time: bar.open_time,
            equity: portfolio.equity(),
        });
        positions.push(portfolio.position());
    }

    let trades = build_trades(&fills, bars, &bar_funding);
    Held {
        qty: fills.first().map_or(0.0, |f| f.qty),
        entry_price: price,
        equity,
        positions,
        fills,
        trades,
        funding_paid: portfolio.funding_paid(),
    }
}

/// The correlation of the strategy's per-bar returns with the benchmark's, with the benchmark's
/// equity read at the strategy series' times. Every strategy time must be one of the benchmark's
/// (both are the first bar's open, then bar closes, and the benchmark holds through every bar);
/// `None` otherwise. With equal times this is the plain correlation of the two return series.
fn aligned_correlation(strategy: &EquitySeries, bench: &EquitySeries) -> Option<f64> {
    let mut sampled = Vec::with_capacity(strategy.times.len());
    let mut j = 0;
    for &t in &strategy.times {
        while j < bench.times.len() && bench.times[j] < t {
            j += 1;
        }
        if bench.times.get(j) != Some(&t) {
            return None;
        }
        sampled.push(bench.values[j]);
    }
    let s = metrics::per_bar_returns(&strategy.values)?;
    let b = metrics::per_bar_returns(&sampled)?;
    metrics::correlation(&s, &b)
}

/// SHA-256, as lowercase hex, over the bars, asks and funding in a fixed byte form: the bar count
/// as a little-endian u64, then per bar the open time as a little-endian i64 and open, high, low,
/// close, volume and quote volume as the little-endian bits of each f64; then the funding count
/// and per event its time and the bits of its rate, in time order. When there are asks, their
/// count and each ask in the same form as a bar follow. Without asks nothing is added, so a
/// fingerprint from before asks existed is unchanged.
pub fn data_fingerprint(bars: &[Candle], asks: &[Candle], funding: &[(Millis, f64)]) -> String {
    let mut h = Sha256::new();
    let candles = |h: &mut Sha256, candles: &[Candle]| {
        h.update((candles.len() as u64).to_le_bytes());
        for b in candles {
            h.update(b.open_time.to_le_bytes());
            for v in [b.open, b.high, b.low, b.close, b.volume, b.quote_volume] {
                h.update(v.to_bits().to_le_bytes());
            }
        }
    };
    candles(&mut h, bars);
    h.update((funding.len() as u64).to_le_bytes());
    for &(time, rate) in funding {
        h.update(time.to_le_bytes());
        h.update(rate.to_bits().to_le_bytes());
    }
    if !asks.is_empty() {
        candles(&mut h, asks);
    }
    hex(&h.finalize())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
