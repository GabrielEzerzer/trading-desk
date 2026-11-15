//! Strategies only the crate's own tests can build. The registered ones are broken on purpose so
//! a test can show that a check catches it, or report what they see where a test can read it:
//! [`crate::find_strategy`] finds them in the crate's tests; they are not listed and do not exist
//! outside them. [`SeesFunding`] is not registered: a test builds it and runs it with
//! [`crate::run_strategy`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, OrderRequest, OrderType,
    Side, TimeInForce, Timeframe, Venue,
};

use crate::strategies::{affordable_qty, order};
use crate::{
    AlsoTradedBars, BacktestConfig, BacktestSettings, Context, Direction, EngineError,
    EvidenceSettings, Metric, Objective, ParamAxis, ParamKind, ParamSpace, ParamSpec, ParamValue,
    Params, Period, PortfolioBenchmark, PortfolioLimits, Strategy, StrategyEntry, SweepPlan,
    TestPlan, VerdictRule, WalkForwardPlan,
};

pub(crate) fn entries() -> Vec<StrategyEntry> {
    vec![
        unrepeatable(),
        one_hour(),
        tells_funding(),
        rests_limits(),
        example_series(),
        trades_both(TRADES_BOTH, |p| TradesBoth::build(p, false)),
        trades_both(RECORDS_PAIRS, |p| TradesBoth::build(p, true)),
    ]
}

/// How many `Unrepeatable` strategies have been built in this process.
static BUILDS: AtomicUsize = AtomicUsize::new(0);

/// With its defaults it never trades. With `trade` it buys every tenth bar and sells every tenth
/// bar after, sizing each buy by how many of its kind were built before it, so two runs of the
/// same backtest fill different quantities: it fails I2 and I3, but only with the swept value.
#[derive(Debug, Clone)]
pub struct Unrepeatable {
    pub trade: bool,
    pub share: f64,
}

fn unrepeatable() -> StrategyEntry {
    StrategyEntry {
        name: "test_unrepeatable",
        description: "Trades only when swept, and differently on every run.",
        params: vec![ParamSpec {
            name: "trade",
            kind: ParamKind::Bool,
            default: ParamValue::Bool(false),
            min: None,
            max: None,
            description: "Trade at all.",
        }],
        build,
        plan: plan(),
        rule: VerdictRule::portfolio(PortfolioBenchmark::Cash, PortfolioLimits::default()),
    }
}

fn build(p: &Params) -> Result<Box<dyn Strategy>, EngineError> {
    let n = BUILDS.fetch_add(1, Ordering::Relaxed);
    Ok(Box::new(Unrepeatable {
        trade: p.bool("trade")?,
        share: 0.5 + (n % 10) as f64 * 0.05,
    }))
}

impl Strategy for Unrepeatable {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        if !self.trade || ctx.is_warming_up() || !ctx.bars().len().is_multiple_of(10) {
            return;
        }
        let position = ctx.position();
        let request = if position == 0.0 {
            let qty = affordable_qty(ctx, ctx.cash() * self.share, bar.close);
            order(ctx, Side::Buy, qty, OrderType::Market, false)
        } else {
            order(ctx, Side::Sell, position.abs(), OrderType::Market, true)
        };
        let _ = ctx.submit(request);
    }
}

const HOUR: i64 = 3_600_000;

/// In the market one hour a day and nothing else, so it has no edge of its own: at the close of
/// the bar that opens at `hour` UTC it buys with half its cash, filled at the next bar's open,
/// and at that bar's close it sells, filled at the open of the bar after. On a market with quotes
/// it pays the ask at the open of the hour after `hour` and no other spread.
#[derive(Debug, Clone)]
pub struct OneHour {
    pub hour: i64,
}

fn one_hour() -> StrategyEntry {
    let mut plan = plan();
    plan.sweep.axes = vec![ParamAxis {
        name: "hour".into(),
        space: ParamSpace::Values {
            values: vec![ParamValue::Int(0)],
        },
    }];
    StrategyEntry {
        name: "test_one_hour",
        description: "In the market one hour a day.",
        params: vec![ParamSpec {
            name: "hour",
            kind: ParamKind::Int,
            default: ParamValue::Int(0),
            min: Some(0.0),
            max: Some(23.0),
            description: "The UTC hour whose bar's close sends the buy.",
        }],
        build: |p| {
            Ok(Box::new(OneHour {
                hour: p.int("hour")?,
            }))
        },
        plan,
        rule: VerdictRule::portfolio(PortfolioBenchmark::Cash, PortfolioLimits::default()),
    }
}

impl Strategy for OneHour {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        if ctx.is_warming_up() {
            return;
        }
        let hour = bar.open_time / HOUR % 24;
        let position = ctx.position();
        let request = if position == 0.0 && hour == self.hour {
            let qty = affordable_qty(ctx, ctx.cash() * 0.5, bar.close);
            order(ctx, Side::Buy, qty, OrderType::Market, false)
        } else if position > 0.0 && hour == (self.hour + 1) % 24 {
            order(ctx, Side::Sell, position, OrderType::Market, true)
        } else {
            return;
        };
        let _ = ctx.submit(request);
    }
}

/// Four-hour bars of a perpetual over 250 days from time 0, `trade` swept over the one value that
/// trades: four 45-day test windows after a 60-day train window, every one choosing it.
fn plan() -> TestPlan {
    TestPlan::new(
        vec![InstrumentId {
            venue: Venue::Binance,
            symbol: "TESTUSDT".into(),
            kind: InstrumentKind::Perpetual,
        }],
        Timeframe::H4,
        Period::new(0, 250 * 24 * 3_600_000),
        BacktestSettings::new(10_000.0, 1.0, Vec::new()),
        SweepPlan::new(
            Params::new(),
            vec![ParamAxis::new(
                "trade",
                ParamSpace::Values {
                    values: vec![ParamValue::Bool(true)],
                },
            )],
            Objective::new(Metric::TotalReturn, Direction::Maximize),
            0,
        ),
        WalkForwardPlan::new(60, 45),
        EvidenceSettings::default(),
    )
}

/// Never trades. On its first live bar it sends an order for nothing, which is rejected and kept
/// in the result, tagged with the times of the funding [`Context::funding`] gives it there, space
/// separated, so a test can read what a strategy the testers build from the registry sees.
pub struct TellsFunding {
    told: bool,
}

fn tells_funding() -> StrategyEntry {
    let mut plan = plan();
    plan.sweep.axes = Vec::new();
    StrategyEntry {
        name: "test_tells_funding",
        description: "Tags a rejected order with the funding it sees on its first live bar.",
        params: Vec::new(),
        build: |_| Ok(Box::new(TellsFunding { told: false })),
        plan,
        rule: VerdictRule::portfolio(PortfolioBenchmark::Cash, PortfolioLimits::default()),
    }
}

impl Strategy for TellsFunding {
    fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
        if ctx.is_warming_up() || self.told {
            return;
        }
        self.told = true;
        let times: Vec<String> = ctx.funding().iter().map(|(t, _)| t.to_string()).collect();
        let mut request = order(ctx, Side::Buy, 0.0, OrderType::Market, false);
        request.client_tag = Some(times.join(" "));
        let _ = ctx.submit(request);
    }
}

/// Rests one limit at a time: while flat, a buy of one unit one price unit under the close it
/// decides on; while long, a reduce-only sell one unit over it. Each stays until it fills, so a
/// test can read how the fills of resting limits are reported.
pub struct RestsLimits;

fn rests_limits() -> StrategyEntry {
    let mut plan = plan();
    plan.sweep.axes = Vec::new();
    StrategyEntry {
        name: "test_rests_limits",
        description: "Rests a buy limit under the close while flat and a sell limit over it while long.",
        params: Vec::new(),
        build: |_| Ok(Box::new(RestsLimits)),
        plan,
        rule: VerdictRule::portfolio(PortfolioBenchmark::Cash, PortfolioLimits::default()),
    }
}

impl Strategy for RestsLimits {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        if ctx.is_warming_up() || !ctx.open_orders().is_empty() {
            return;
        }
        let request = if ctx.position() == 0.0 {
            let price = bar.close - 1.0;
            order(ctx, Side::Buy, 1.0, OrderType::Limit { price }, false)
        } else {
            let price = bar.close + 1.0;
            order(
                ctx,
                Side::Sell,
                ctx.position(),
                OrderType::Limit { price },
                true,
            )
        };
        let _ = ctx.submit(request);
    }
}

/// The series [`ExampleSeries`] reads. Its made-up rows are in `mock/series/example_series.json`.
pub(crate) const EXAMPLE_SERIES: &str = "example_series";

/// Reads the series [`EXAMPLE_SERIES`]: long with its cash while the series' latest known value is
/// above `level`, flat otherwise. It fails the run when the series is not given, or when it is shown a
/// row known after the close of the bar it is deciding on, so a test reads from a run that
/// completes that it was fed the series and shown only what was known.
pub struct ExampleSeries {
    level: f64,
}

fn example_series() -> StrategyEntry {
    let mut plan = plan();
    plan.sweep.axes = vec![ParamAxis::new(
        "level",
        ParamSpace::Range {
            from: 40.0,
            to: 60.0,
            step: 10.0,
        },
    )];
    StrategyEntry {
        name: "example_series",
        description: "Long while the series example_series is above a level.",
        params: vec![ParamSpec {
            name: "level",
            kind: ParamKind::Float,
            default: ParamValue::Float(50.0),
            min: None,
            max: None,
            description: "The value above which it is long.",
        }],
        build: |p| {
            Ok(Box::new(ExampleSeries {
                level: p.float("level")?,
            }))
        },
        plan,
        rule: VerdictRule::portfolio(PortfolioBenchmark::Cash, PortfolioLimits::default()),
    }
}

impl Strategy for ExampleSeries {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let close = bar.open_time + ctx.timeframe().duration_ms();
        let Some(rows) = ctx.series(EXAMPLE_SERIES) else {
            ctx.fail(format!("no series {EXAMPLE_SERIES}"));
            return;
        };
        let Some(latest) = rows.last().copied() else {
            return;
        };
        if latest.known_at > close {
            ctx.fail(format!(
                "shown a row known at {} on the bar closing at {close}",
                latest.known_at
            ));
            return;
        }
        if ctx.is_warming_up() {
            return;
        }
        let position = ctx.position();
        let request = if position == 0.0 && latest.value > self.level {
            let qty = affordable_qty(ctx, ctx.cash(), bar.close);
            order(ctx, Side::Buy, qty, OrderType::Market, false)
        } else if position > 0.0 && latest.value <= self.level {
            order(ctx, Side::Sell, position, OrderType::Market, true)
        } else {
            return;
        };
        let _ = ctx.submit(request);
    }
}

/// What [`SeesFunding`] saw on each bar it was called on, in order: the bar's open time, whether
/// it was a warm-up bar, and [`Context::funding`].
pub(crate) type FundingSeen = Arc<Mutex<Vec<(Millis, bool, Vec<(Millis, f64)>)>>>;

/// Records what [`Context::funding`] gives it on each bar, so a test can compare it with the
/// funding the run was given. On its first live bar it buys one unit at market and holds it, so a
/// perpetual run pays funding.
pub(crate) struct SeesFunding(pub FundingSeen);

impl Strategy for SeesFunding {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let seen = (bar.open_time, ctx.is_warming_up(), ctx.funding().to_vec());
        self.0.lock().unwrap().push(seen);
        if !ctx.is_warming_up() && ctx.position() == 0.0 && ctx.open_orders().is_empty() {
            let _ = ctx.submit(order(ctx, Side::Buy, 1.0, OrderType::Market, false));
        }
    }
}

pub(crate) const TRADES_BOTH: &str = "test_trades_both";
pub(crate) const RECORDS_PAIRS: &str = "test_records_pairs";
pub(crate) const PAIR_STEP: Millis = 4 * HOUR;
pub(crate) const FIRST_SHAPE_TO_SECOND: f64 = 2.0;

pub(crate) static PAIRS_SEEN: Mutex<Vec<(f64, f64)>> = Mutex::new(Vec::new());

pub(crate) fn perpetual(symbol: &str, taker: f64) -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Binance,
            symbol: format!("{symbol}USDT"),
            kind: InstrumentKind::Perpetual,
        },
        base: symbol.into(),
        quote: "USDT".into(),
        tick_size: 0.01,
        lot_size: 0.001,
        min_qty: 0.001,
        min_notional: 1.0,
        fees: FeeSchedule {
            maker: taker / 2.0,
            taker,
            per_unit: 0.0,
        },
    }
}

fn shape(k: i64) -> f64 {
    ((k * 7) % 23 - 11) as f64 / 1_000.0
}

fn shaped_bar(k: i64, open: f64, change: f64) -> Candle {
    let close = open * (1.0 + change);
    Candle {
        open_time: k * PAIR_STEP,
        open,
        high: open.max(close) * 1.001,
        low: open.min(close) * 0.999,
        close,
        volume: 1.0,
        quote_volume: open,
    }
}

pub(crate) fn pair_bars(n: i64, start: f64) -> (Vec<Candle>, Vec<Candle>) {
    let (mut first, mut second) = (Vec::new(), Vec::new());
    let (mut a, mut b) = (start, start / 2.0);
    for k in 0..n {
        let change = shape(k);
        let bar_a = shaped_bar(k, a, change);
        let bar_b = shaped_bar(k, b, FIRST_SHAPE_TO_SECOND * change);
        (a, b) = (bar_a.close, bar_b.close);
        first.push(bar_a);
        second.push(bar_b);
    }
    (first, second)
}

type Build = fn(&Params) -> Result<Box<dyn Strategy>, EngineError>;

fn trades_both(name: &'static str, build: Build) -> StrategyEntry {
    let mut plan = plan();
    plan.sweep.axes = vec![ParamAxis::new(
        "qty",
        ParamSpace::Values {
            values: vec![ParamValue::Float(1.0), ParamValue::Float(2.0)],
        },
    )];
    StrategyEntry {
        name,
        description: "Buys and sells every traded instrument on alternate bars.",
        params: vec![ParamSpec {
            name: "qty",
            kind: ParamKind::Float,
            default: ParamValue::Float(1.0),
            min: None,
            max: None,
            description: "The units of each instrument bought.",
        }],
        build,
        plan,
        rule: VerdictRule::portfolio(PortfolioBenchmark::Cash, PortfolioLimits::default()),
    }
}

pub struct TradesBoth {
    qty: f64,
    records: bool,
}

impl TradesBoth {
    fn build(p: &Params, records: bool) -> Result<Box<dyn Strategy>, EngineError> {
        Ok(Box::new(TradesBoth {
            qty: p.float("qty")?,
            records,
        }))
    }

    fn record(&self, ctx: &Context) {
        let ids: Vec<InstrumentId> = ctx.traded().map(|i| i.id.clone()).collect();
        let shapes: Vec<f64> = ids
            .iter()
            .filter_map(|id| ctx.bar_of(id))
            .map(|b| b.close / b.open - 1.0)
            .collect();
        if let [first, second] = shapes[..] {
            PAIRS_SEEN.lock().unwrap().push((first, second));
        }
    }
}

impl Strategy for TradesBoth {
    fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
        if self.records {
            self.record(ctx);
        }
        if ctx.is_warming_up() {
            return;
        }
        let ids: Vec<InstrumentId> = ctx.traded().map(|i| i.id.clone()).collect();
        for id in ids {
            if ctx.bar_of(&id).is_none() || !ctx.open_orders_of(&id).is_some_and(|o| o.is_empty()) {
                continue;
            }
            let position = ctx.position_of(&id).unwrap_or(0.0);
            let (side, qty) = match position == 0.0 {
                true => (Side::Buy, self.qty),
                false => (Side::Sell, position.abs()),
            };
            let _ = ctx.submit(OrderRequest {
                instrument: id,
                side,
                qty,
                order_type: OrderType::Market,
                tif: TimeInForce::Gtc,
                reduce_only: position != 0.0,
                client_tag: None,
            });
        }
    }
}

pub(crate) const PAIR_GAP_BAR: i64 = 30;
pub(crate) const PAIR_FUNDING_RATE: f64 = 0.000_1;

pub(crate) fn pair_first() -> Instrument {
    perpetual("TEST", 0.000_5)
}

pub(crate) fn pair_second() -> Instrument {
    perpetual("BBB", 0.000_5)
}

pub(crate) fn pair_data(n: i64) -> (Vec<Candle>, Vec<AlsoTradedBars>) {
    let (mut first, second) = pair_bars(n, 1_000.0);
    first.retain(|b| b.open_time != PAIR_GAP_BAR * PAIR_STEP);
    let funding = (0..n)
        .step_by(2)
        .map(|k| (k * PAIR_STEP, PAIR_FUNDING_RATE));
    let also = AlsoTradedBars {
        instrument: pair_second().id,
        bars: second,
        funding: funding.collect(),
    };
    (first, vec![also])
}

pub(crate) fn pair_config(strategy: &str, n: i64) -> BacktestConfig {
    BacktestConfig {
        strategy: strategy.into(),
        params: Params::new(),
        instrument: pair_first(),
        timeframe: Timeframe::H4,
        from: 0,
        to: n * PAIR_STEP,
        starting_cash: 10_000.0,
        slippage_bps: 5.0,
        fee_override: None,
        max_leverage: 2.0,
        warmup_from: None,
        close_at_end: false,
        account_currency: td_core::AccountCurrency::Quote,
        references: Vec::new(),
        series: Vec::new(),
        also_traded: vec![pair_second()],
        start_when_all_listed: false,
    }
}
