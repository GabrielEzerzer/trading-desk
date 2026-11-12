//! The backtest runner: one configuration and its data in, a full result out.

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

use td_core::{
    AccountCurrency, Candle, FeeSchedule, Fill, Instrument, InstrumentKind, Millis, OrderId, Side,
    Timeframe,
};

use crate::metrics::{self, DAY_MS, DAYS_PER_YEAR, EquitySeries, MetricsReport};
use crate::risk::{self, EquityRange, Intrabar};
use crate::session::{check_quote, funding_ranges, sorted_funding};
use crate::{
    BarFunding, CLOSE_OUT_TAG, CandleFillModel, EngineError, EquityPoint, Order, Params, Portfolio,
    Session, SessionConfig, Strategy, Trade, attach_stops, build_strategy, build_trades,
};

/// The slippage a backtest gets when its configuration leaves `slippage_bps` out, in basis
/// points of price: 2 for BTC and ETH, the most liquid markets, and 5 for every other
/// instrument, which suits the other large caps.
///
/// It depends on the asset's liquidity, not on the market type, so it is keyed by the base asset
/// rather than kept in the per-kind rules table ([`td_core::InstrumentKind::rules`]).
///
/// shortcut: one default for everything outside BTC and ETH; small caps slip more than 5 bps and
/// FX majors far less (their ask series already carries the spread), so set their slippage
/// explicitly, or carry a liquidity tier on the instrument when strategies trade them.
pub fn default_slippage_bps(instrument: &Instrument) -> f64 {
    match instrument.base.as_str() {
        "BTC" | "ETH" => 2.0,
        _ => 5.0,
    }
}

/// The revision of the engine's behaviour, stored in every result so a rerun can tell an engine
/// change from a data change.
///
/// Bump it whenever the engine's behaviour changes: fills, fees, funding, the trade list, metrics
/// or the benchmark, including changes the expected fixture does not cover (it has no gaps, for
/// example). Order ids, and which orders are accepted or rejected, count as behaviour too: paper
/// sessions save manual orders by id, so a change there shifts those ids in saved sessions. A
/// regenerated `tests/fixtures/backtest_expected.json` always needs a bump, but an unchanged
/// fixture does not mean the behaviour is unchanged. Results carry the revision, so a bump also
/// moves td-bench's checksums: update `harness/evals/perf-checksums.txt` in the same change.
pub const ENGINE_REVISION: u32 = 5;

/// Everything that defines a backtest apart from the data itself.
///
/// It loads from JSON as it saves, except that `slippage_bps` may be left out or null: it then
/// takes [`default_slippage_bps`] for the instrument, and the configuration holds that number
/// from then on, so the saved result records the slippage the run used.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize)]
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
    /// Starting balance in the account currency (`account_currency`).
    pub starting_cash: f64,
    /// Slippage on market and stop fills, in basis points of price. Left out when loading, it is
    /// the instrument's default ([`default_slippage_bps`]); a value given, zero included, is
    /// used as it is.
    pub slippage_bps: f64,
    /// Replaces the instrument's fee schedule when set.
    #[serde(default)]
    pub fee_override: Option<FeeSchedule>,
    /// The largest position allowed on a margined kind (perpetual, FX), as its notional over
    /// equity: 1.0 means 1x, the default. On a perpetual at 1x a long is liquidated only near a
    /// price of zero and a short only once the price has about doubled (see [`Session::on_bar`]).
    /// Orders that would take the position over it are rejected when submitted; orders that
    /// reduce the position are always allowed (see [`crate::Context::submit`]). A kind without
    /// margin, such as spot, ignores it. Left out of the saved result when it is the default.
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
    /// The currency the account holds and the result reports every amount in: the instrument's
    /// quote currency, the default, or its base currency, as a dollar account trading USD/JPY
    /// (FX only; see [`Session::new`]). Left out of the saved result when it is the default.
    #[serde(default, skip_serializing_if = "AccountCurrency::is_quote")]
    pub account_currency: AccountCurrency,
}

impl<'de> Deserialize<'de> for BacktestConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The configuration as JSON gives it, with the slippage optional.
        #[derive(Deserialize)]
        struct Given {
            strategy: String,
            #[serde(default)]
            params: Params,
            instrument: Instrument,
            timeframe: Timeframe,
            from: Millis,
            to: Millis,
            starting_cash: f64,
            #[serde(default)]
            slippage_bps: Option<f64>,
            #[serde(default)]
            fee_override: Option<FeeSchedule>,
            #[serde(default = "default_max_leverage")]
            max_leverage: f64,
            #[serde(default)]
            warmup_from: Option<Millis>,
            #[serde(default)]
            close_at_end: bool,
            #[serde(default)]
            account_currency: AccountCurrency,
        }
        let g = Given::deserialize(deserializer)?;
        Ok(BacktestConfig {
            slippage_bps: g
                .slippage_bps
                .unwrap_or_else(|| default_slippage_bps(&g.instrument)),
            strategy: g.strategy,
            params: g.params,
            instrument: g.instrument,
            timeframe: g.timeframe,
            from: g.from,
            to: g.to,
            starting_cash: g.starting_cash,
            fee_override: g.fee_override,
            max_leverage: g.max_leverage,
            warmup_from: g.warmup_from,
            close_at_end: g.close_at_end,
            account_currency: g.account_currency,
        })
    }
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
    /// per-unit commission, rounded down to the lot size. In a walk-forward, the first test
    /// window's purchase.
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
    /// The strategy's annualised alpha against the benchmark, on the same per-bar returns as the
    /// correlation: the intercept of the least-squares fit of the strategy's returns on the
    /// benchmark's, times the returns observed per 365-day year (simple, not compounded), with a
    /// risk-free rate of zero; see [`metrics::alpha_beta`]. The returns per year are their count
    /// over the time from the first to the last point of the strategy's equity series, so a market
    /// that closes (FX at weekends) or a gap in the data is not counted as bars it did not have. A
    /// return across a gap in the bars, or between two walk-forward test windows, counts as one
    /// return. `None` when the correlation would be, and in a result saved before benchmarks had
    /// it.
    #[serde(default)]
    pub alpha: Option<f64>,
    /// The slope of that fit, `cov(strategy, benchmark) / var(benchmark)`: how much of the
    /// benchmark's move the strategy's equity follows.
    #[serde(default)]
    pub beta: Option<f64>,
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
    /// The lowest and highest equity inside each bar, one per bar like `equity`; see
    /// [`risk::intrabar`]. Empty in a result saved before results had them.
    #[serde(default)]
    pub equity_extremes: Vec<EquityRange>,
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
/// from the bid. An ask that matches no bar in `bars`, or a second ask at one time, is an error, and
/// so is one that is not a valid candle or is below its bid (see [`EngineError::CrossedQuote`]), on
/// a warm-up bar as on a traded one.
/// `funding` is `(time, rate)` in any order. An event in a gap between traded bars is charged at
/// the next bar's open, like one due at that open; events before the first traded bar or from the
/// last traded bar's end on are ignored.
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
    run_built(config, bars, asks, funding, || {
        build_strategy(&config.strategy, &config.params)
    })
}

/// [`run`] with a strategy built by the caller instead of from the registry, for a strategy that
/// is not registered: the cross-check harness's (`examples/crosscheck.rs`) uses it. The
/// configuration's `strategy` and `params` are then only labels in the result, so [`rerun`] cannot
/// run such a result again.
pub fn run_strategy(
    config: &BacktestConfig,
    strategy: Box<dyn Strategy>,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
) -> Result<BacktestResult, EngineError> {
    run_built(config, bars, asks, funding, || Ok(strategy))
}

/// The backtest of [`run`], building the strategy with `build` once the data has been checked, so
/// a bad range or ask is reported before an unknown strategy, as it always was.
fn run_built(
    config: &BacktestConfig,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
    build: impl FnOnce() -> Result<Box<dyn Strategy>, EngineError>,
) -> Result<BacktestResult, EngineError> {
    let (warmup, traded) = select_bars(config, bars)?;
    let mut used_asks = match_asks(config.instrument.id.kind, bars, asks, &warmup)?;
    let traded_asks = match_asks(config.instrument.id.kind, bars, asks, &traded)?;
    // The session checks each traded bar's ask as it is fed, but warm-up bars are fed without one.
    check_quotes(&warmup, &used_asks)?;
    used_asks.extend_from_slice(&traded_asks);
    let bars = traded;
    let tf = config.timeframe;
    let funding = sorted_funding(funding);
    let ranges = funding_ranges(tf, &bars, &funding);

    let strategy = build()?;
    let session_config = SessionConfig {
        starting_cash: config.starting_cash,
        slippage_bps: config.slippage_bps,
        fees: config.fee_override,
        max_leverage: config.max_leverage,
        account_currency: config.account_currency,
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
    let account = config.account_currency;
    let pip = config.instrument.pip_size();
    let mut trades = build_trades(
        &fills,
        &bars,
        &traded_asks,
        session.bar_funding(),
        account,
        pip,
    );
    attach_stops(&mut trades, &bars, session.bar_stops(), account);
    mark_forced_closes(&mut trades, &fills, session.orders());
    let series = EquitySeries::new(config.starting_cash, session.equity(), tf);
    let intrabar = risk::intrabar(
        config.starting_cash,
        &bars,
        &traded_asks,
        session.equity(),
        &fills,
        session.bar_funding(),
        account,
    );
    let report = metrics::report_with(
        &series,
        Some(&intrabar),
        &positions,
        &trades,
        &fills,
        session.portfolio().funding_paid(),
        account,
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
        equity_extremes: intrabar.ranges,
    })
}

/// Marks the trade each fill of the end-of-run close-out order closed (see
/// [`Session::close_out`]) as force-closed: the last closed trade that exits on the fill's bar.
fn mark_forced_closes(trades: &mut [Trade], fills: &[Fill], orders: &[Order]) {
    for fill in fills {
        let order = fill
            .order_id
            .0
            .checked_sub(1)
            .and_then(|i| orders.get(i as usize));
        if order.is_some_and(|o| o.request.client_tag.as_deref() == Some(CLOSE_OUT_TAG))
            && let Some(t) = trades
                .iter_mut()
                .rev()
                .find(|t| !t.open && t.exit_time == fill.time)
        {
            t.force_closed = true;
        }
    }
}

/// The ask of each of `used`, from `asks` matched by open time, or `None` for every bar when the
/// kind has no quotes. `all` is every bar given to the run, which every ask must match. Errors for
/// asks given on a kind without quotes, an ask that matches no bar in `all`, two asks with one open
/// time, and a bar in `used` without an ask on a kind with quotes.
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
    let mut by_time = HashMap::with_capacity(asks.len());
    for ask in asks {
        if by_time.insert(ask.open_time, *ask).is_some() {
            return Err(EngineError::DuplicateAsk {
                time: ask.open_time,
            });
        }
    }
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

/// Checks the ask that [`match_asks`] matched to each of `bars`, where there is one, as the session
/// checks a bar's ask (see [`check_quote`]).
fn check_quotes(bars: &[Candle], asks: &[Option<Candle>]) -> Result<(), EngineError> {
    for (bar, ask) in bars.iter().zip(asks) {
        if let Some(ask) = ask {
            check_quote(bar, ask)?;
        }
    }
    Ok(())
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
/// run's tells whether the data changed since, as [`rerun`] does for a backtest. It refuses the
/// asks [`run`] refuses, on warm-up and traded bars alike.
pub fn config_fingerprint(
    config: &BacktestConfig,
    bars: &[Candle],
    asks: &[Candle],
    funding: &[(Millis, f64)],
) -> Result<String, EngineError> {
    let (mut used, traded) = select_bars(config, bars)?;
    let mut used_asks = match_asks(config.instrument.id.kind, bars, asks, &used)?;
    used_asks.extend(match_asks(config.instrument.id.kind, bars, asks, &traded)?);
    used.extend_from_slice(&traded);
    // No session runs here to check the traded bars' asks, so every ask is checked.
    check_quotes(&used, &used_asks)?;
    let used_asks: Vec<Candle> = used_asks.into_iter().flatten().collect();
    let funding = sorted_funding(funding);
    let used_funding: Vec<(Millis, f64)> = funding_ranges(config.timeframe, &traded, &funding)
        .into_iter()
        .flat_map(|r| funding[r].iter().copied())
        .collect();
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
    /// The equity's range through each bar and the peak leverage: see [`risk::intrabar`].
    pub(crate) intrabar: Intrabar,
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
    let report = metrics::report_with(
        &series,
        Some(&held.intrabar),
        &held.positions,
        &held.trades,
        &held.fills,
        held.funding_paid,
        config.account_currency,
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

/// The benchmark from buy and hold's curve and metrics, with the strategy's excess return over it,
/// the correlation of their per-bar returns, and the strategy's alpha and beta against it on the
/// same returns: see [`annualised_alpha_beta`].
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
    let returns = aligned_returns(strategy_series, series);
    let correlation = returns
        .as_ref()
        .and_then(|(s, b)| metrics::correlation(s, b));
    let alpha_beta = returns
        .as_ref()
        .and_then(|r| annualised_alpha_beta(&strategy_series.times, r));
    Benchmark {
        qty,
        entry_price,
        equity,
        metrics: report,
        excess_return,
        correlation,
        alpha: alpha_beta.map(|(a, _)| a),
        beta: alpha_beta.map(|(_, b)| b),
    }
}

/// Buys at the first bar's open with all starting cash (taker fee, per-unit commission, the run's
/// slippage) and holds to the last close, paying funding on a perpetual like the session does:
/// funding due at a bar's open or in the gap before it on the position held going into the bar,
/// at the open price, so none at the first open, and funding due later in the bar at the close.
/// With an ask series the purchase is at the first ask open, and the long is marked at the bid
/// closes, as the session marks a long. `asks` holds each bar's ask, `None` without quotes. On a
/// base account the cash is in the base currency, so the purchase's cost and fee are converted at
/// its price, and the account is valued as the session values it.
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
    let account = config.account_currency;
    let unit_cost = account.from_quote(price * (1.0 + fees.taker) + fees.per_unit, price);
    let qty = inst.round_qty(config.starting_cash / unit_cost);
    let mut portfolio = Portfolio::new_in(inst.id.kind, config.starting_cash, account);
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
                fee: account.from_quote(fees.fee(price, qty, fees.taker), price),
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

    let trades = build_trades(&fills, bars, asks, &bar_funding, account, inst.pip_size());
    let intrabar = risk::intrabar(
        config.starting_cash,
        bars,
        asks,
        &equity,
        &fills,
        &bar_funding,
        account,
    );
    Held {
        qty: fills.first().map_or(0.0, |f| f.qty),
        entry_price: price,
        equity,
        positions,
        fills,
        trades,
        funding_paid: portfolio.funding_paid(),
        intrabar,
    }
}

/// The strategy's per-bar returns and the benchmark's over the same bars, with the benchmark's
/// equity read at the strategy series' times. Every strategy time must be one of the benchmark's
/// (both are the first bar's open, then bar closes, and the benchmark holds through every bar);
/// `None` otherwise, or when either series has a return that cannot be computed.
fn aligned_returns(strategy: &EquitySeries, bench: &EquitySeries) -> Option<(Vec<f64>, Vec<f64>)> {
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
    Some((s, b))
}

/// The strategy's alpha and beta against the benchmark on `returns`, their aligned per-bar returns
/// over the strategy series' `times`, with the alpha annualised by the returns observed per
/// 365-day year: their count over the years from the first time to the last. Without gaps that is
/// the bars in a year of the timeframe; across gaps, such as an FX market's weekends, it is fewer,
/// as many as the market had. `None` when [`metrics::alpha_beta`] is, or the times span no time.
fn annualised_alpha_beta(times: &[Millis], returns: &(Vec<f64>, Vec<f64>)) -> Option<(f64, f64)> {
    let (&first, &last) = (times.first()?, times.last()?);
    if last <= first {
        return None;
    }
    let years = (last - first) as f64 / (DAYS_PER_YEAR * DAY_MS as f64);
    let (s, b) = returns;
    metrics::alpha_beta(s, b, s.len() as f64 / years)
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

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Millis = 3_600_000;

    #[test]
    fn alpha_is_annualised_by_the_returns_observed_per_year_across_weekend_gaps() {
        // Two FX weeks of hourly bars, Monday 00:00 to Friday 24:00, with the 48-hour weekend
        // between them. The series is the first bar's open, then each bar's close: 1h to 120h,
        // then 169h to 288h after the weekend.
        let mut times = vec![0];
        times.extend((1..=120).map(|h| h * HOUR));
        times.extend((169..=288).map(|h| h * HOUR));
        // The benchmark alternates +1% and -1%; the strategy makes 0.1% a bar plus half of it, so
        // beta is 0.5 and alpha 0.001 a bar.
        let bench_returns: Vec<f64> = (0..240)
            .map(|k| if k % 2 == 0 { 0.01 } else { -0.01 })
            .collect();
        let curve = |returns: &mut dyn Iterator<Item = f64>| {
            let mut values = vec![10_000.0];
            for r in returns {
                values.push(values.last().unwrap() * (1.0 + r));
            }
            EquitySeries {
                times: times.clone(),
                values,
            }
        };
        let bench = curve(&mut bench_returns.iter().copied());
        let strategy = curve(&mut bench_returns.iter().map(|b| 0.001 + 0.5 * b));
        let returns = aligned_returns(&strategy, &bench).unwrap();
        let (alpha, beta) = annualised_alpha_beta(&strategy.times, &returns).unwrap();
        assert!((beta - 0.5).abs() < 1e-9, "beta {beta}");
        // 240 returns over 288 hours, a year being 8760: 240 * 8760 / 288 = 7300 a year, so alpha
        // is 0.001 * 7300 = 7.3. A year of the timeframe's bars, 8760, would give 8.76.
        assert!((alpha - 7.3).abs() < 1e-9, "alpha {alpha}");
    }
}
