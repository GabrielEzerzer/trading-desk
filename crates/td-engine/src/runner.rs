//! The backtest runner: one configuration and its data in, a full result out.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use td_core::{Candle, FeeSchedule, Fill, Instrument, Millis, OrderId, Side, Timeframe};

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
/// example). A regenerated `tests/fixtures/backtest_expected.json` always needs a bump, but an
/// unchanged fixture does not mean the behaviour is unchanged.
pub const ENGINE_REVISION: u32 = 1;

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

/// Buy and hold of the same instrument over the same bars.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Benchmark {
    /// Quantity bought at the first bar's open: all starting cash, after the taker fee, rounded
    /// down to the lot size.
    pub qty: f64,
    /// The first bar's open with slippage, rounded up to the tick.
    pub entry_price: f64,
    pub equity: Vec<EquityPoint>,
    pub metrics: MetricsReport,
    /// Strategy total return minus benchmark total return.
    pub excess_return: Option<f64>,
    /// Pearson correlation of the strategy's per-bar returns with the benchmark's.
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
    /// SHA-256 of the bars (warm-up bars first) and funding the run used; see
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
/// bars inside `warmup_from..from` warm the strategy up first. `funding` is `(time, rate)` in any
/// order. An event in a gap between traded bars is charged at the next bar's open, like one due at
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
    funding: &[(Millis, f64)],
) -> Result<BacktestResult, EngineError> {
    let (warmup, bars) = select_bars(config, bars)?;
    let tf = config.timeframe;
    let funding = sorted_funding(funding);
    let ranges = funding_ranges(tf, &bars, &funding);

    let strategy = build_strategy(&config.strategy, &config.params)?;
    let session_config = SessionConfig {
        starting_cash: config.starting_cash,
        slippage_bps: config.slippage_bps,
        fees: config.fee_override,
    };
    let mut session = Session::new(strategy, config.instrument.clone(), tf, session_config)?;
    for bar in &warmup {
        session.warm_up(*bar, &[])?;
    }
    let mut positions = Vec::with_capacity(bars.len());
    let mut used_funding = Vec::new();
    // The bar whose close decides the close-out, so it fills at the last bar's open.
    let close_out_at = bars.len().checked_sub(2).filter(|_| config.close_at_end);
    for (i, (bar, range)) in bars.iter().zip(&ranges).enumerate() {
        let events = &funding[range.clone()];
        session.on_bar(*bar, events)?;
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
    let benchmark = benchmark(config, &bars, &funding, &ranges, &series, &report);
    let mut used_bars = warmup;
    used_bars.extend_from_slice(&bars);

    Ok(BacktestResult {
        config: config.clone(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        engine_revision: ENGINE_REVISION,
        data_fingerprint: data_fingerprint(&used_bars, &used_funding),
        equity: session.equity().to_vec(),
        orders: session.orders().to_vec(),
        fills,
        trades,
        metrics: report,
        benchmark,
    })
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
    funding: &[(Millis, f64)],
) -> Result<String, EngineError> {
    let (mut used, traded) = select_bars(config, bars)?;
    let funding = sorted_funding(funding);
    let used_funding: Vec<(Millis, f64)> = funding_ranges(config.timeframe, &traded, &funding)
        .into_iter()
        .flat_map(|r| funding[r].iter().copied())
        .collect();
    used.extend_from_slice(&traded);
    Ok(data_fingerprint(&used, &used_funding))
}

/// Runs a saved result's configuration again on `bars` and `funding`.
pub fn rerun(
    saved: &BacktestResult,
    bars: &[Candle],
    funding: &[(Millis, f64)],
) -> Result<Rerun, EngineError> {
    let result = run(&saved.config, bars, funding)?;
    let fingerprint_matches = result.data_fingerprint == saved.data_fingerprint;
    let engine_matches = result.engine_revision == saved.engine_revision;
    Ok(Rerun {
        result,
        fingerprint_matches,
        engine_matches,
    })
}

/// Buys at the first bar's open with all starting cash (taker fee, the run's slippage) and holds
/// to the last close, paying funding on a perpetual like the session does: funding due at a bar's
/// open or in the gap before it on the position held going into the bar, at the open price, so
/// none at the first open, and funding due later in the bar at the close.
fn benchmark(
    config: &BacktestConfig,
    bars: &[Candle],
    funding: &[(Millis, f64)],
    ranges: &[std::ops::Range<usize>],
    strategy_series: &EquitySeries,
    strategy_report: &MetricsReport,
) -> Benchmark {
    let inst = &config.instrument;
    let fees = config.fee_override.unwrap_or(inst.fees);
    let model = CandleFillModel {
        slippage_bps: config.slippage_bps,
    };
    let first = bars[0];
    let price = model.slip(first.open, Side::Buy, inst);
    let qty = inst.round_qty(config.starting_cash / (price * (1.0 + fees.taker)));
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
                fee: price * qty * fees.taker,
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
    let series = EquitySeries::new(config.starting_cash, &equity, config.timeframe);
    let report = metrics::report(
        &series,
        &positions,
        &trades,
        &fills,
        portfolio.funding_paid(),
    );
    let excess_return = match (strategy_report.total_return, report.total_return) {
        (Some(s), Some(b)) => Some(s - b),
        _ => None,
    };
    let correlation = match (
        metrics::per_bar_returns(&strategy_series.values),
        metrics::per_bar_returns(&series.values),
    ) {
        (Some(s), Some(b)) => metrics::correlation(&s, &b),
        _ => None,
    };
    Benchmark {
        qty: fills.first().map_or(0.0, |f| f.qty),
        entry_price: price,
        equity,
        metrics: report,
        excess_return,
        correlation,
    }
}

/// SHA-256, as lowercase hex, over the bars and funding in a fixed byte form: the bar count as a
/// little-endian u64, then per bar the open time as a little-endian i64 and open, high, low,
/// close, volume and quote volume as the little-endian bits of each f64; then the funding count
/// and per event its time and the bits of its rate, in time order.
pub fn data_fingerprint(bars: &[Candle], funding: &[(Millis, f64)]) -> String {
    let mut h = Sha256::new();
    h.update((bars.len() as u64).to_le_bytes());
    for b in bars {
        h.update(b.open_time.to_le_bytes());
        for v in [b.open, b.high, b.low, b.close, b.volume, b.quote_volume] {
            h.update(v.to_bits().to_le_bytes());
        }
    }
    h.update((funding.len() as u64).to_le_bytes());
    for &(time, rate) in funding {
        h.update(time.to_le_bytes());
        h.update(rate.to_bits().to_le_bytes());
    }
    hex(&h.finalize())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
