//! The backtest runner: one configuration and its data in, a full result out.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use td_core::{Candle, FeeSchedule, Fill, Instrument, Millis, OrderId, Side, Timeframe};

use crate::metrics::{self, EquitySeries, MetricsReport};
use crate::session::{funding_ranges, sorted_funding};
use crate::{
    CandleFillModel, EngineError, EquityPoint, Order, Params, Portfolio, Session, SessionConfig,
    Trade, build_strategy, build_trades,
};

/// Everything that defines a backtest apart from the data itself.
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
}

/// Buy and hold of the same instrument over the same bars.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestResult {
    pub config: BacktestConfig,
    /// The td-engine crate version that produced the result.
    pub engine_version: String,
    /// SHA-256 of the bars and funding the run used; see [`data_fingerprint`].
    pub data_fingerprint: String,
    /// Equity at each bar's close.
    pub equity: Vec<EquityPoint>,
    /// Every order submitted, rejected ones included, in submission order.
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
}

/// Runs a backtest.
///
/// `bars` may cover more than the configured range; only bars inside `from..to` are used.
/// `funding` is `(time, rate)` in any order; events outside the used bars' windows are ignored.
/// The strategy is built through the registry and run on a [`Session`], then the trade list,
/// metrics and buy-and-hold benchmark are derived from the session's record.
pub fn run(
    config: &BacktestConfig,
    bars: &[Candle],
    funding: &[(Millis, f64)],
) -> Result<BacktestResult, EngineError> {
    if config.from >= config.to {
        return Err(EngineError::InvalidConfig("from must be before to"));
    }
    let bars: Vec<Candle> = bars
        .iter()
        .filter(|b| (config.from..config.to).contains(&b.open_time))
        .copied()
        .collect();
    if bars.is_empty() {
        return Err(EngineError::NoBarsInRange {
            from: config.from,
            to: config.to,
        });
    }
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
    let mut positions = Vec::with_capacity(bars.len());
    let mut bar_funding = Vec::with_capacity(bars.len());
    let mut used_funding = Vec::new();
    for (bar, range) in bars.iter().zip(&ranges) {
        let events = &funding[range.clone()];
        let before = session.portfolio().funding_paid();
        session.on_bar(*bar, events)?;
        bar_funding.push(session.portfolio().funding_paid() - before);
        positions.push(session.portfolio().position());
        used_funding.extend_from_slice(events);
    }

    let fills = session.fills().to_vec();
    let trades = build_trades(&fills, &bars, &bar_funding);
    let series = EquitySeries::new(config.starting_cash, session.equity(), tf);
    let report = metrics::report(
        &series,
        tf,
        &positions,
        &trades,
        &fills,
        session.portfolio().funding_paid(),
    );
    let benchmark = benchmark(config, &bars, &funding, &ranges, &series, &report);

    Ok(BacktestResult {
        config: config.clone(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        data_fingerprint: data_fingerprint(&bars, &used_funding),
        equity: session.equity().to_vec(),
        orders: session.orders().to_vec(),
        fills,
        trades,
        metrics: report,
        benchmark,
    })
}

/// Runs a saved result's configuration again on `bars` and `funding`.
pub fn rerun(
    saved: &BacktestResult,
    bars: &[Candle],
    funding: &[(Millis, f64)],
) -> Result<Rerun, EngineError> {
    let result = run(&saved.config, bars, funding)?;
    let fingerprint_matches = result.data_fingerprint == saved.data_fingerprint;
    Ok(Rerun {
        result,
        fingerprint_matches,
    })
}

/// Buys at the first bar's open with all starting cash (taker fee, the run's slippage) and holds
/// to the last close, paying funding on a perpetual like the strategy does.
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
    if qty.is_finite() && qty > 0.0 {
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

    let mut equity = Vec::with_capacity(bars.len());
    let mut positions = Vec::with_capacity(bars.len());
    let mut bar_funding = Vec::with_capacity(bars.len());
    for (bar, range) in bars.iter().zip(ranges) {
        let before = portfolio.funding_paid();
        for &(_, rate) in &funding[range.clone()] {
            portfolio.charge_funding(rate, bar.close);
        }
        bar_funding.push(portfolio.funding_paid() - before);
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
        config.timeframe,
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
