// Generated from the Rust API types by `cargo run -p td-server --bin gen-api-types`.
// Do not edit by hand; CI fails when this file is out of date.

/**
 * An exchange the desk reads data from and simulates orders against.
 */
export type Venue = "binance" | "bybit" | "dukascopy";

export type InstrumentKind = "spot" | "perpetual" | "fx";

/**
 * Identifies an instrument across venues. The same symbol can exist as spot and as a perpetual on
 * one venue, so the kind is part of the key.
 */
export type InstrumentId = { venue: Venue, 
/**
 * The venue's own symbol, for example `BTCUSDT`.
 */
symbol: string, kind: InstrumentKind, };

/**
 * Trading fees as fractions of notional: 0.001 means 0.1%.
 */
export type FeeSchedule = { maker: number, taker: number, };

/**
 * A tradable instrument with the venue's trading rules.
 */
export type Instrument = { id: InstrumentId, 
/**
 * The asset being bought or sold, for example `BTC`.
 */
base: string, 
/**
 * The asset prices, notional and fees are in, for example `USDT`.
 */
quote: string, 
/**
 * Smallest price increment.
 */
tick_size: number, 
/**
 * Smallest quantity increment.
 */
lot_size: number, 
/**
 * Smallest order quantity, in the base asset.
 */
min_qty: number, 
/**
 * Smallest order value (quantity times price), in the quote asset.
 */
min_notional: number, fees: FeeSchedule, };

/**
 * A candle interval. Serialized and displayed as `1m`, `5m`, `15m`, `30m`, `1h`, `4h`, `1d`, `1w`.
 */
export type Timeframe = "1m" | "5m" | "15m" | "30m" | "1h" | "4h" | "1d" | "1w";

/**
 * One OHLCV bar. `open_time` is the start of the bar's timeframe bucket.
 */
export type Candle = { open_time: number, open: number, high: number, low: number, close: number, 
/**
 * Traded volume in the base asset.
 */
volume: number, 
/**
 * Traded volume in the quote asset.
 */
quote_volume: number, };

export type Side = "buy" | "sell";

/**
 * How an order is priced.
 *
 * A buy stop triggers when a bar's high reaches `trigger` or above; a sell stop triggers when a
 * bar's low reaches `trigger` or below. Once triggered, `Stop` fills as a market order and
 * `StopLimit` rests as a limit order at `price`. A take-profit is expressed as a `Limit` order;
 * there is no separate type for it.
 */
export type OrderType = { "type": "market" } | { "type": "limit", price: number, } | { "type": "stop", trigger: number, } | { "type": "stop_limit", trigger: number, price: number, };

export type TimeInForce = "gtc" | "ioc";

export type OrderId = number;

/**
 * An order as a strategy or the user submits it. Quantity is in the base asset and always
 * positive; `side` carries the direction.
 */
export type OrderRequest = { instrument: InstrumentId, side: Side, qty: number, order_type: OrderType, tif: TimeInForce, 
/**
 * Only reduce an open position, never open or grow one.
 */
reduce_only: boolean, 
/**
 * A free label from the submitter, carried through to reports.
 */
client_tag: string | null, };

/**
 * Whether a fill added liquidity to the book (maker) or took it (taker).
 */
export type Liquidity = "maker" | "taker";

/**
 * A full or partial execution of an order.
 */
export type Fill = { order_id: OrderId, instrument: InstrumentId, side: Side, time: number, price: number, qty: number, 
/**
 * The fee paid, in the quote asset. Negative for a rebate.
 */
fee: number, liquidity: Liquidity, };

export type ParamKind = "int" | "float" | "bool";

export type ParamValue = boolean | number | number;

/**
 * One tunable parameter of a strategy, as the tester and the API list it.
 */
export type ParamSpec = { name: string, kind: ParamKind, default: ParamValue, 
/**
 * Inclusive bounds for `Int` and `Float` parameters; `None` for `Bool` or no bound.
 */
min: number | null, max: number | null, description: string, };

/**
 * Parameter values by name.
 */
export type Params = { [key in string]: ParamValue };

/**
 * Everything that defines a backtest apart from the data itself.
 */
export type BacktestConfig = { 
/**
 * A name from the strategy registry.
 */
strategy: string, 
/**
 * Parameter values; missing ones take the strategy's defaults.
 */
params: Params, instrument: Instrument, timeframe: Timeframe, 
/**
 * Bars that open at or after `from` and before `to` are used.
 */
from: number, to: number, 
/**
 * Starting balance in the quote asset.
 */
starting_cash: number, 
/**
 * Slippage on market and stop fills, in basis points of price.
 */
slippage_bps: number, 
/**
 * Replaces the instrument's fee schedule when set.
 */
fee_override: FeeSchedule | null, 
/**
 * The largest perpetual position allowed, as its notional over equity: 1.0 means 1x, the
 * default. At 1x a long is liquidated only near a price of zero and a short only once the
 * price has about doubled (see [`Session::on_bar`]). Orders that would take the position
 * over it are rejected when submitted; orders that reduce the position are always allowed
 * (see [`crate::Context::submit`]). Spot ignores it. Left out of the saved result when it is
 * the default.
 */
max_leverage?: number, 
/**
 * When set, bars that open at or after `warmup_from` and before `from` are fed to the
 * strategy first with trading disabled (see [`Session::warm_up`]), so its indicators are
 * ready at the first traded bar. They add no equity points and are not in the metrics;
 * orders the strategy submits on them are rejected and recorded in the result's `orders`.
 */
warmup_from?: number | null, 
/**
 * When set, a position still open at the end is closed with a real fill: at the close of
 * the second-to-last traded bar every open order is cancelled and a reduce-only market order
 * for the whole position is submitted, so it fills at the last bar's open with slippage and
 * the taker fee, like any market order. Orders the strategy submits from then on never fill.
 * The trade list then ends with a closed trade instead of an open one marked at the last
 * close. The strategy tester sets it on out-of-sample runs.
 */
close_at_end?: boolean, };

export type BacktestResult = { config: BacktestConfig, 
/**
 * The td-engine crate version that produced the result.
 */
engine_version: string, 
/**
 * The [`ENGINE_REVISION`] that produced the result.
 */
engine_revision: number, 
/**
 * SHA-256 of the bars (warm-up bars first) and funding the run used; see
 * [`data_fingerprint`].
 */
data_fingerprint: string, 
/**
 * Equity at each bar's close.
 */
equity: Array<EquityPoint>, 
/**
 * Every order submitted, rejected ones included (warm-up bars' too), in submission order.
 */
orders: Array<Order>, fills: Array<Fill>, trades: Array<Trade>, metrics: MetricsReport, benchmark: Benchmark, };

/**
 * Buy and hold of the same instrument over the same bars: a backtest's traded bars, or in a
 * walk-forward each test window's bars, stitched like the test runs (see
 * [`crate::WalkForwardResult::benchmark`]).
 */
export type Benchmark = { 
/**
 * Quantity bought at the first bar's open: all starting cash, after the taker fee, rounded
 * down to the lot size. In a walk-forward, the first test window's purchase.
 */
qty: number, 
/**
 * The first bar's open with slippage, rounded up to the tick. In a walk-forward, the first
 * test window's.
 */
entry_price: number, equity: Array<EquityPoint>, metrics: MetricsReport, 
/**
 * Strategy total return minus benchmark total return.
 */
excess_return: number | null, 
/**
 * Pearson correlation of the strategy's per-bar returns with the benchmark's over the same
 * intervals: the benchmark's equity is read at each time of the strategy's equity series.
 * In a backtest the two have the same bars, and in a walk-forward the same test windows, so
 * the times match one to one.
 */
correlation: number | null, };

/**
 * Equity at the close of the bar that opened at `time`.
 */
export type EquityPoint = { time: number, equity: number, };

/**
 * An order the session accepted or rejected, with its request as rounded at submission.
 */
export type Order = { id: OrderId, request: OrderRequest, 
/**
 * Open time of the bar the order was decided on. It can first fill on a later bar. For a
 * strategy's order that is the bar at whose close it was sent; for one placed between bars
 * from outside the strategy, such as a manual order, it is the bar in progress when it was
 * accepted, so it never fills on a price from before then.
 */
submitted_at: number, 
/**
 * Close of the bar the order was decided on: the last price before it was sent.
 */
decision_close: number, 
/**
 * The protective stop to place when this order fills, if one was attached.
 */
attached_stop: AttachedStop | null, status: OrderStatus, };

/**
 * A protective stop attached to an entry with [`crate::Context::submit_with_stop`]. When the
 * entry fills, the broker places a reduce-only stop on the other side, for the filled quantity,
 * under `id`.
 */
export type AttachedStop = { id: OrderId, trigger: number, };

export type OrderStatus = { "status": "attached" } | { "status": "open" } | { "status": "filled" } | { "status": "cancelled", "reason": string } | { "status": "rejected", "reason": string };

/**
 * One round trip: the position leaves zero, then returns to it.
 */
export type Trade = { side: TradeSide, 
/**
 * Open time of the bar of the first fill.
 */
entry_time: number, 
/**
 * Open time of the bar of the fill that closed the trade, or of the last bar for an open
 * trade.
 */
exit_time: number, 
/**
 * Still open at the end of the run; its exit is the remaining position marked at the last
 * close, with no exit fee.
 */
open: boolean, 
/**
 * The largest absolute position during the trade.
 */
qty: number, 
/**
 * Volume-weighted price of the fills that opened or added to the position.
 */
entry_price: number, 
/**
 * Volume-weighted price of the fills that reduced or closed it (and the mark, if open).
 */
exit_price: number, 
/**
 * `sign * exit_qty * (exit_price - entry_price)`, before fees and funding. Over a closed
 * round trip this is the exit notional minus the entry notional (reversed for a short),
 * which is what the portfolio's average-cost realised P&L adds up to.
 */
gross_pnl: number, 
/**
 * Fees of the trade's fills. A fill that flips the position splits its fee between the two
 * trades in proportion to the quantity that closed and that opened.
 */
fees: number, 
/**
 * Funding charged on the trade's position: at a bar's open (including funding due in a gap
 * before the bar) while it was held going into the bar, and later in a bar while it was held
 * after the bar's fills.
 */
funding: number, 
/**
 * `gross_pnl - fees - funding`.
 */
net_pnl: number, 
/**
 * `net_pnl / (entry_price * qty)`: net P&L over the largest entry notional.
 */
return_on_notional: number, 
/**
 * Bars from the entry fill's bar to the exit fill's bar; for an open trade, to the end of the
 * last bar. It counts bars fed, not time: missing bars in a gap are not counted, so across a
 * gap it understates how long the position was held. For a closed trade, the time between
 * the entry and exit fills' bars is `exit_time - entry_time`.
 */
bars_held: number, };

export type TradeSide = "long" | "short";

/**
 * The metrics report for one equity series and its trades.
 */
export type MetricsReport = { start_equity: number, end_equity: number, 
/**
 * See [`total_return`].
 */
total_return: number | null, 
/**
 * See [`cagr`]. `None` for a run shorter than [`MIN_CAGR_DAYS`].
 */
cagr: number | null, 
/**
 * Annualised volatility of daily returns; see [`volatility`] and [`daily_equity`]. `None`
 * with fewer than [`MIN_DAILY_RETURNS`] daily returns on days with a bar close, as are
 * Sharpe and Sortino.
 */
volatility: number | null, 
/**
 * Annualised Sharpe ratio of daily returns; see [`sharpe`].
 */
sharpe: number | null, 
/**
 * Annualised Sortino ratio of daily returns; see [`sortino`].
 */
sortino: number | null, 
/**
 * Depth of the largest drawdown as a fraction of its peak; see [`max_drawdown`].
 */
max_drawdown: number | null, 
/**
 * From the largest drawdown's peak to its recovery, or to the end if it never recovered.
 */
max_drawdown_duration_ms: number | null, max_drawdown_duration_bars: number | null, 
/**
 * See [`calmar`]. `None` whenever CAGR is, so for a run shorter than [`MIN_CAGR_DAYS`].
 */
calmar: number | null, 
/**
 * Closed trades. The trade statistics below count closed trades only; an open trade at the
 * end is in the trade list but not in these numbers.
 */
trades: number, 
/**
 * See [`win_rate`].
 */
win_rate: number | null, 
/**
 * See [`profit_factor`].
 */
profit_factor: number | null, 
/**
 * Mean net P&L per closed trade.
 */
avg_trade_pnl: number | null, 
/**
 * Median net P&L per closed trade.
 */
median_trade_pnl: number | null, 
/**
 * Mean bars held per closed trade: bars, not time (see [`Trade::bars_held`]).
 */
avg_bars_held: number | null, 
/**
 * See [`exposure`].
 */
exposure: number | null, 
/**
 * See [`turnover`].
 */
turnover: number | null, 
/**
 * Sum of fill fees; negative if rebates outweighed fees.
 */
fees_paid: number, 
/**
 * Net funding paid; negative when the position received funding.
 */
funding_paid: number, };

/**
 * The values one parameter takes in a sweep.
 */
export type ParamSpace = { "kind": "values", values: Array<ParamValue>, } | { "kind": "range", from: number, to: number, step: number, } | { "kind": "bool" };

/**
 * One swept parameter.
 */
export type ParamAxis = { name: string, space: ParamSpace, };

export type SweepMode = "grid" | { "random": { samples: number, seed: number, } };

/**
 * A metrics report field a sweep can rank by.
 *
 * `Sharpe` and `Sortino` are undefined on a range with fewer than
 * [`metrics::MIN_DAILY_RETURNS`] daily returns on days with a bar close, and `Cagr` and `Calmar`
 * on a range shorter than [`metrics::MIN_CAGR_DAYS`] days; a run whose objective is undefined
 * does not qualify (see [`SweepRow::qualified`]). In a walk-forward that applies to each train
 * window's length, and in an out-of-sample test to the in-sample segment's.
 */
export type Metric = "total_return" | "cagr" | "sharpe" | "sortino" | "calmar" | "profit_factor" | "win_rate" | "max_drawdown";

export type Direction = "maximize" | "minimize";

export type Objective = { metric: Metric, direction: Direction, };

/**
 * A parameter sweep.
 */
export type SweepSpec = { 
/**
 * The backtest every parameter set shares. Its `params` fix parameters that are not swept;
 * parameters in neither take the strategy's defaults. A parameter may not be both fixed and
 * swept.
 */
base: BacktestConfig, 
/**
 * The swept parameters. The grid order has the first axis changing slowest.
 */
axes: Array<ParamAxis>, mode: SweepMode, objective: Objective, 
/**
 * Runs with fewer closed trades than this rank after every run that has enough. Defaults to
 * 1 when left out, so a parameter set that never trades (no drawdown, no losses) cannot top
 * a ranking by drawdown; set 0 to rank such sets too.
 */
min_trades: number, };

/**
 * The metrics a sweep keeps per run.
 */
export type Headline = { total_return: number | null, cagr: number | null, sharpe: number | null, sortino: number | null, calmar: number | null, profit_factor: number | null, win_rate: number | null, max_drawdown: number | null, 
/**
 * Closed trades.
 */
trades: number, };

/**
 * One parameter set of a sweep and how it did.
 */
export type SweepRow = { 
/**
 * Position in the grid order. Runs that rank equal are ordered by it.
 */
index: number, 
/**
 * The full parameter set the run used, defaults included.
 */
params: Params, 
/**
 * Whether the run ranks by its objective: it ran, made at least `min_trades` closed trades
 * and its objective is defined (see [`Metric`] for when it is not). Other runs rank after
 * all qualified ones: first those that ran, then those the strategy refused, each in grid
 * order.
 */
qualified: boolean, 
/**
 * `None` when the strategy refused the parameter set.
 */
metrics: Headline | null, 
/**
 * Why the strategy refused the parameter set (a rule across parameters, such as an SMA
 * cross needing fast below slow).
 */
error: string | null, };

/**
 * The result of a sweep.
 *
 * shortcut: rows keep headline metrics only, not equity curves or trades, so memory stays
 * bounded with many runs; the best run is run again to get its full result.
 */
export type SweepResult = { spec: SweepSpec, engine_version: string, 
/**
 * The fingerprint of the data in the base range; see [`crate::data_fingerprint`].
 */
data_fingerprint: string, 
/**
 * Every parameter set, best first.
 */
rows: Array<SweepRow>, 
/**
 * The best row's full backtest. `None` when no row qualified.
 */
best: BacktestResult | null, };

/**
 * Where the base range splits into in-sample and out-of-sample.
 */
export type Split = { "fraction": number } | { "at": number };

export type OosSpec = { 
/**
 * The sweep over the whole range; `base.from..base.to` is split in two.
 */
sweep: SweepSpec, split: Split, };

export type OosResult = { spec: OosSpec, engine_version: string, 
/**
 * The fingerprint of the data in the whole range.
 */
data_fingerprint: string, 
/**
 * Open time of the first out-of-sample bar; the in-sample segment ends before it.
 */
split_at: number, 
/**
 * The sweep over the in-sample segment. Its best run supplies the parameters.
 */
in_sample: SweepResult, 
/**
 * The chosen parameters on the out-of-sample segment, warmed up with the in-sample bars.
 * It runs with `close_at_end`: a position still open is closed by a market order decided at
 * the close of the second-to-last bar and filled at the last bar's open, with slippage and
 * the taker fee, so its trades are all closed and their costs real. The in-sample sweep's
 * runs close out the same way, so `degradation` and `min_trades` compare like with like.
 */
out_of_sample: BacktestResult, 
/**
 * Out-of-sample objective over in-sample objective. With `TotalReturn` as the objective it
 * is the ratio of CAGRs instead, because the two segments usually differ in length and a
 * cumulative return grows with it. `None` if either value is undefined or the in-sample
 * value is zero. For a maximised metric below 1 is worse out of sample; for a minimised one
 * above 1 is worse. Not meaningful when the in-sample value is negative.
 *
 * With `TotalReturn`, `Cagr` or `Calmar` it is therefore `None` when either segment is
 * shorter than [`metrics::MIN_CAGR_DAYS`] days, and with `Sharpe` or `Sortino` when either
 * has fewer than [`metrics::MIN_DAILY_RETURNS`] daily returns on days with a bar close (the
 * in-sample segment is refused up front then, so this is the out-of-sample one). It does not
 * fall back to the ratio of total returns, which would compare segments of different
 * lengths; compare the two runs' metrics directly then.
 */
degradation: number | null, };

export type WindowMode = "rolling" | "anchored";

/**
 * Walk-forward windows, in bars of the base range.
 *
 * The windows are laid back from the end of the range, so the most recent bars are always
 * tested: the last test segment ends on the last bar, and each earlier one starts `step` bars
 * before the next. There are as many windows as fit with a whole train window before the first
 * test segment. A window's train window is the `train` bars before its test segment (rolling)
 * or every bar before it (anchored), so a test segment never overlaps its own train window.
 * `step` must be at least `test`, so test segments never overlap each other; with
 * `step == test` they are contiguous, and with a longer step the untested bars sit before each
 * test segment.
 *
 * The leftover bars at the start, fewer than `step`, are never tested. In rolling mode the first
 * train sweep is warmed up on them (as many as the train length); in anchored mode every train
 * window starts at the first bar, so they are part of each one's training data.
 */
export type WindowSpec = { train_bars: number, test_bars: number, step_bars: number, mode: WindowMode, };

export type WalkForwardSpec = { 
/**
 * The sweep run on every train window; `base.from..base.to` is the whole range.
 */
sweep: SweepSpec, windows: WindowSpec, };

/**
 * One walk-forward window and its out-of-sample run.
 *
 * shortcut: keeps the chosen parameters and their in-sample headline metrics, not the train
 * sweep's rows, so a long walk-forward stays small.
 */
export type WindowResult = { 
/**
 * The train segment: bars that open in `train_from..train_to`.
 */
train_from: number, train_to: number, 
/**
 * The test segment: bars that open in `test_from..test_to`; `test_from == train_to`.
 */
test_from: number, test_to: number, 
/**
 * The best parameters on the train segment.
 */
params: Params, in_sample: Headline, 
/**
 * The chosen parameters on the test segment, warmed up with the train bars and closed out
 * at its end; see [`WalkForwardResult`].
 */
out_of_sample: BacktestResult, };

/**
 * The result of a walk-forward run.
 *
 * At a test window's end the position is closed for real: each test run has `close_at_end`
 * set, so at the close of the window's second-to-last bar its open orders are cancelled and a
 * reduce-only market order for the whole position is submitted, which fills at the last bar's
 * open with slippage and the taker fee. Every window therefore ends flat unless the close-out
 * is rejected (see `Session::close_out`); its trades are then all closed and counted in the
 * trade statistics, and the exit costs are in the stitched equity. The next window starts flat
 * from where the stitched equity ended.
 *
 * Train sweeps close out the same way, so a window's in-sample and out-of-sample trade counts
 * (and `min_trades`) count a position held at the end alike. A rolling window's train sweep is
 * warmed up on the bars just before it, as many as the window's train length (fewer near the
 * start of the range), the same warm-up its test run gets; the first window's uses the leftover
 * bars before it (see [`WindowSpec`]) and starts cold if there are none. Every anchored window's
 * starts cold, because its train window begins at the first bar.
 *
 * The stitched metrics are computed like a single backtest's (see [`MetricsReport`]): the
 * stitched Sharpe, Sortino and volatility come from the stitched curve's daily UTC returns, not
 * from its per-bar returns, so they are comparable with a backtest's on any timeframe. The floors
 * on annualised metrics apply to the stitched curve as a whole, so it can have a Sharpe ratio or
 * a CAGR when no single test window is long enough for one.
 */
export type WalkForwardResult = { spec: WalkForwardSpec, engine_version: string, 
/**
 * The fingerprint of the data in the whole range.
 */
data_fingerprint: string, windows: Array<WindowResult>, 
/**
 * The test segments stitched into one curve; see [`stitch`].
 */
equity: Array<EquityPoint>, 
/**
 * Metrics of the stitched curve, its trades and its fills.
 */
metrics: MetricsReport, 
/**
 * Buy and hold in each test window, stitched like the test runs: each window buys at its
 * first test bar's open with the starting cash and holds to its last test bar's close, with
 * the fill, fee and funding rules of a backtest's benchmark, and the windows are compounded
 * with [`stitch`] as the test segments are. The two curves are therefore exposed over the
 * same bars: with a step longer than the test, neither holds through the bars between test
 * windows. Its excess return is the stitched total return minus its own.
 *
 * Unlike a test run it never sells, so it pays one taker fee per window to the test run's
 * two, and it holds through each window's last bar, on whose open a test run closes out.
 * `None` only in a result saved before walk-forwards had a benchmark.
 */
benchmark: Benchmark | null, 
/**
 * Every test segment's trades in time order, with quantities, P&L, fees and funding scaled
 * by the same factor as the segment's equity, so they add up to the stitched equity's
 * changes. Segments after the stitched equity reached zero (factor 0, see [`stitch`]) traded
 * nothing in the stitched account and add no trades.
 */
trades: Array<Trade>, 
/**
 * Every test segment's fills in time order, with quantities and fees scaled the same way and
 * the same segments left out.
 */
fills: Array<Fill>, 
/**
 * How many windows chose different parameters from the window before.
 */
param_changes: number, 
/**
 * `param_changes / (windows - 1)`: 0 when every window chose the same parameters, 1 when
 * every window changed them. `None` with one window.
 */
param_change_rate: number | null, };

/**
 * A run's id: the save time in milliseconds since the epoch, zero-padded to 13 digits so ids
 * sort by time, then `-` and the first 8 hex digits of the SHA-256 of `result.json`.
 */
export type RunId = string;

/**
 * What a saved run is.
 */
export type RunKind = "backtest" | "sweep" | "oos" | "walk_forward";

/**
 * A result as saved, tagged with its kind: `{"kind": "...", "result": {...}}`.
 */
export type SavedRun = { "kind": "backtest", "result": BacktestResult } | { "kind": "sweep", "result": SweepResult } | { "kind": "oos", "result": OosResult } | { "kind": "walk_forward", "result": WalkForwardResult };

/**
 * One row of the run list. The strategy and range are the base configuration's. The metrics
 * are the backtest's, the sweep's best run's (none if no run qualified), the out-of-sample
 * segment's, or the stitched walk-forward curve's.
 */
export type RunSummary = { id: RunId, created_at: number, kind: RunKind, strategy: string, instrument: InstrumentId, timeframe: Timeframe, from: number, to: number, total_return: number | null, cagr: number | null, sharpe: number | null, max_drawdown: number | null, trades: number, };

/**
 * The line the binary prints to stdout once it listens, for the app that started it.
 */
export type Handshake = { port: number, token: string, };

/**
 * The body of every error response.
 */
export type ErrorResponse = { error: ErrorBody, };

/**
 * What went wrong: a stable machine-readable `code` and a sentence for people.
 */
export type ErrorBody = { code: string, message: string, };

export type HealthResponse = { version: string, };

export type InstrumentsResponse = { instruments: Array<Instrument>, rejected: Array<RejectedInstrument>, };

/**
 * One instrument the venue lists but that failed validation, and why.
 */
export type RejectedInstrument = { symbol: string, reason: string, };

/**
 * What to download: the 1-minute candles of `instrument` with open times in `[from, to)`, and
 * for a perpetual its funding rates in the same range. Anything after now is skipped.
 */
export type DownloadRequest = { instrument: InstrumentId, 
/**
 * Not before 2017-01-01 00:00 UTC (1483228800000); earlier is a 400.
 */
from: number, to: number, };

/**
 * What a download did.
 */
export type DownloadResult = { 
/**
 * Candles fetched in total.
 */
candles: number, months: Array<MonthCount>, 
/**
 * Archive files the venue does not have (before the symbol listed, or not yet published).
 */
missing_files: Array<string>, 
/**
 * Minutes in the range, up to now, still not stored after the download.
 */
gaps: Array<GapInfo>, 
/**
 * Funding rates fetched; `null` for spot.
 */
funding_rates: number | null, };

export type MonthCount = { 
/**
 * `YYYY-MM`.
 */
month: string, candles: number, };

/**
 * A run of missing minutes: open times from `from` up to but not including `to`.
 */
export type GapInfo = { from: number, to: number, minutes: number, };

export type CoverageResponse = { instrument: InstrumentId, months: Array<MonthCoverageInfo>, 
/**
 * `null` for spot.
 */
funding: FundingCoverage | null, };

/**
 * Which minutes the store holds for one month.
 */
export type MonthCoverageInfo = { 
/**
 * `YYYY-MM`.
 */
month: string, 
/**
 * Stored 1-minute candles.
 */
minutes: number, 
/**
 * Minutes in the whole month.
 */
month_minutes: number, 
/**
 * Open time of the first and last stored candle.
 */
first: number, last: number, 
/**
 * Missing minutes between `first` and `last`.
 */
gaps: Array<GapInfo>, 
/**
 * The month file's size on disk.
 */
bytes: number, };

export type FundingCoverage = { rates: number, first: number | null, last: number | null, };

export type StoredResponse = { 
/**
 * Every instrument with at least one month file or a funding file, in instrument order.
 */
instruments: Array<StoredInstrumentInfo>, 
/**
 * Paths in the data folder, relative to it, that name no venue, kind, symbol or month the
 * store writes. They are neither counted nor deleted.
 */
unrecognised: Array<string>, };

/**
 * What the store holds for one instrument.
 */
export type StoredInstrumentInfo = { instrument: InstrumentId, 
/**
 * The month files, oldest first; empty when only a funding file is stored.
 */
months: Array<StoredMonthInfo>, 
/**
 * The funding file's size; `null` when there is none, which is always so for spot.
 */
funding_bytes: number | null, 
/**
 * The month files and the funding file together.
 */
bytes: number, };

/**
 * One month file of an instrument's candles and its size on disk.
 */
export type StoredMonthInfo = { 
/**
 * `YYYY-MM`.
 */
month: string, bytes: number, };

/**
 * What to delete: the candles of `instrument` for the months `from` to `to`, both included, or,
 * with neither given, every month and a perpetual's funding file.
 */
export type DeleteRequest = { instrument: InstrumentId, 
/**
 * `YYYY-MM`; give both `from` and `to`, or neither.
 */
from?: string, 
/**
 * `YYYY-MM`, included.
 */
to?: string, };

/**
 * What a delete removed: data files, and temp files a crashed download left beside them.
 */
export type DeleteResult = { files: number, bytes: number, };

/**
 * `GET /candles`: closed bars, oldest first.
 */
export type CandlesResponse = { instrument: InstrumentId, timeframe: Timeframe, 
/**
 * The bar slots the response covers: open times in `[from, to)`, each on a bar boundary. `to`
 * is the asked `to` rounded up to a boundary, but no later than the open time of the bar
 * still forming; `from` is later than asked when the limit cut the range, and never before
 * 2017-01-01.
 */
from: number, to: number, 
/**
 * `from` was given and the range held more than `limit` bar slots, so the oldest were left
 * out.
 */
truncated: boolean, 
/**
 * Closed bars, oldest first.
 */
candles: Array<Candle>, 
/**
 * The bars fetched from the venue rather than read from the store.
 */
from_venue: Array<BarSpan>, 
/**
 * Slots in `[from, to)` with no bar in the store or at the venue.
 */
missing: Array<BarSpan>, };

/**
 * A run of bar slots: open times from `from` up to but not including `to`, `bars` of them.
 */
export type BarSpan = { from: number, to: number, bars: number, };

/**
 * A strategy the apps can run, with the parameters it takes.
 */
export type StrategyInfo = { name: string, description: string, params: Array<ParamSpec>, };

/**
 * Returned when a job is accepted.
 */
export type JobCreated = { job_id: number, };

/**
 * A job as the API shows it.
 */
export type JobInfo = { id: number, kind: JobKind, status: JobStatus, 
/**
 * From 0 to 1.
 */
progress: number, 
/**
 * The latest progress line, for people.
 */
message: string, request: JobRequest, 
/**
 * Set when `status` is `done`.
 */
result: JobResult | null, 
/**
 * Set when `status` is `failed`.
 */
error: ErrorBody | null, 
/**
 * True once a cancel was asked for; a running job stops at its next checkpoint.
 */
cancel_requested: boolean, created_at: number, started_at: number | null, finished_at: number | null, };

export type JobKind = "download" | "backtest" | "sweep" | "oos" | "walk_forward";

export type JobStatus = "queued" | "running" | "done" | "failed" | "cancelled";

/**
 * What a job was asked to do, tagged by `kind`.
 */
export type JobRequest = { "kind": "download" } & DownloadRequest | { "kind": "backtest" } & BacktestConfig | { "kind": "sweep" } & SweepSpec | { "kind": "oos" } & OosSpec | { "kind": "walk_forward" } & WalkForwardSpec;

/**
 * What a finished job produced, tagged by `kind`.
 */
export type JobResult = { "kind": "download" } & DownloadResult | { "kind": "backtest" } & RunDone | { "kind": "sweep" } & RunDone | { "kind": "oos" } & RunDone | { "kind": "walk_forward" } & RunDone;

/**
 * What a finished run job produced: the saved run's id and its row in the run list. The whole
 * result is at `GET /jobs/{id}/result` and `GET /runs/{run_id}`.
 */
export type RunDone = { run_id: RunId, summary: RunSummary, 
/**
 * How many buckets in the range have no bar, empty or incomplete; the list is the result's
 * `data.dropped`, and the missing minutes are its `data.gaps`.
 */
dropped_buckets: number, };

/**
 * A saved run in full.
 */
export type RunDetail = { id: RunId, run: SavedRun, 
/**
 * The data the run was given; `null` for a run saved without one (by another tool) or whose
 * report cannot be read.
 */
data: DataReport | null, 
/**
 * Why the run's data report cannot be read, when it has one that is damaged; `null` otherwise.
 */
data_error: string | null, 
/**
 * Whether the run would come out the same today; set by `GET /runs/{id}`, `null` from
 * `GET /jobs/{id}/result`.
 */
check: RunCheck | null, };

/**
 * A saved run checked against the store and the engine as they are now, as
 * [`td_engine::Rerun`] reports for a rerun, without running it again.
 */
export type RunCheck = { 
/**
 * Whether the store's data for the run's range hashes the same as the data the run used.
 * False also when the store no longer has data there.
 */
data_matches: boolean, 
/**
 * Whether this engine has the revision that produced the run. When false, the same data
 * may now give a different result because the engine changed. `null` for a sweep in which
 * no run qualified: it holds no backtest to read the revision from.
 */
engine_matches: boolean | null, };

/**
 * The data a run was given: its bars at the run's timeframe, the minutes the store lacks, and the
 * buckets left out or built with minutes missing. The engine does not record these; the API
 * does, so the apps can show them.
 */
export type DataReport = { 
/**
 * Bars from the start of the range (warm-up included) to its end.
 */
bars: number, 
/**
 * Every stretch of minutes in the range (warm-up included) the store has no candle for,
 * oldest first, including any at the start or the end of the range, in the instrument's open
 * hours. Each is shorter than a day; a longer one refuses the run.
 */
gaps: Array<GapInfo>, 
/**
 * Open times of every bucket in the range the run has no bar for: buckets with no minutes at
 * all, and buckets left out because too many of their minutes are missing (td-data's rule: a
 * bucket needs its first and last open minute and at most 1% of its open minutes missing). A
 * bucket wholly inside the market's closed hours is not listed.
 */
dropped: Array<number>, 
/**
 * Buckets built with a tolerated number of minutes missing.
 */
tolerated: Array<ToleratedBucket>, 
/**
 * Funding rates given to the run; 0 for spot.
 */
funding_rates: number, };

/**
 * A bucket built although a few of its minutes are missing, within td-data's tolerance.
 */
export type ToleratedBucket = { open_time: number, missing_minutes: number, };

/**
 * What a paper session runs: a registered strategy with its parameters, or none for a manual
 * session, on one instrument and timeframe, with the same money settings as a backtest.
 */
export type PaperConfig = { 
/**
 * A registered strategy, or `none` ([`MANUAL_STRATEGY`]) for a manual session, which takes no
 * parameters.
 */
strategy: string, params: Params, 
/**
 * The instrument with the trading rules it had when the session was made. Fixed for the
 * session's life, so a resume replays against the same rules.
 */
instrument: Instrument, timeframe: Timeframe, starting_cash: number, slippage_bps: number, 
/**
 * Replaces the instrument's fee schedule when set.
 */
fees: FeeSchedule | null, 
/**
 * The largest perpetual position allowed, as its notional over equity, as in a backtest:
 * 1.0 means 1x, the default, which a config saved without it also gets. Spot ignores it.
 */
max_leverage: number, };

/**
 * A funding rate fed with a bar.
 */
export type Funding = { 
/**
 * The time the engine was given, inside the bar's window.
 */
time: number, rate: number, 
/**
 * The venue's funding time. Earlier than `time` when the rate was not yet published when its
 * own bar closed and was charged on this later bar.
 */
due: number, };

/**
 * Where a paper session is.
 */
export type Status = { "status": "warming_up" } | { "status": "live" } | { "status": "reconnecting" } | { "status": "stopped" } | { "status": "error", "message": string };

/**
 * `POST /sessions`: what to paper trade. The instrument's trading rules come from the venue's
 * instrument list.
 */
export type StartSessionRequest = { 
/**
 * A registered strategy, as `GET /strategies` lists it, or `none` for a manual session, where
 * only manual orders trade; it takes no parameters.
 */
strategy: string, params: Params, instrument: InstrumentId, timeframe: Timeframe, starting_cash: number, slippage_bps: number, 
/**
 * Replaces the instrument's fee schedule when set.
 */
fees?: FeeSchedule, };

/**
 * A session as `GET /sessions` lists it.
 */
export type SessionSummary = { id: string, strategy: string, instrument: InstrumentId, timeframe: Timeframe, status: Status, 
/**
 * Equity now, at the last live bar's close.
 */
equity: number, };

/**
 * The open position, marked at the last live bar's close.
 */
export type PositionInfo = { 
/**
 * Signed quantity in the base asset: negative is short.
 */
qty: number, avg_entry: number, mark_price: number, unrealised_pnl: number, realised_pnl: number, fees_paid: number, 
/**
 * Net funding paid; negative when the position received funding.
 */
funding_paid: number, };

/**
 * Who placed an order, and so whose its fills are.
 */
export type OrderSource = "strategy" | "manual" | "liquidation";

/**
 * An order with who placed it.
 */
export type PaperOrder = { source: OrderSource, id: OrderId, request: OrderRequest, 
/**
 * Open time of the bar the order was decided on. It can first fill on a later bar. For a
 * strategy's order that is the bar at whose close it was sent; for one placed between bars
 * from outside the strategy, such as a manual order, it is the bar in progress when it was
 * accepted, so it never fills on a price from before then.
 */
submitted_at: number, 
/**
 * Close of the bar the order was decided on: the last price before it was sent.
 */
decision_close: number, 
/**
 * The protective stop to place when this order fills, if one was attached.
 */
attached_stop: AttachedStop | null, status: OrderStatus, };

/**
 * A fill with who placed its order.
 */
export type PaperFill = { source: OrderSource, order_id: OrderId, instrument: InstrumentId, side: Side, time: number, price: number, qty: number, 
/**
 * The fee paid, in the quote asset. Negative for a rebate.
 */
fee: number, liquidity: Liquidity, };

/**
 * Whose fills make up a trade. A liquidation's fill does not count: it only closes a trade that
 * the strategy or a person opened.
 */
export type TradeSource = "strategy" | "manual" | "mixed";

/**
 * A round trip with whose fills it is made of.
 */
export type PaperTrade = { source: TradeSource, side: TradeSide, 
/**
 * Open time of the bar of the first fill.
 */
entry_time: number, 
/**
 * Open time of the bar of the fill that closed the trade, or of the last bar for an open
 * trade.
 */
exit_time: number, 
/**
 * Still open at the end of the run; its exit is the remaining position marked at the last
 * close, with no exit fee.
 */
open: boolean, 
/**
 * The largest absolute position during the trade.
 */
qty: number, 
/**
 * Volume-weighted price of the fills that opened or added to the position.
 */
entry_price: number, 
/**
 * Volume-weighted price of the fills that reduced or closed it (and the mark, if open).
 */
exit_price: number, 
/**
 * `sign * exit_qty * (exit_price - entry_price)`, before fees and funding. Over a closed
 * round trip this is the exit notional minus the entry notional (reversed for a short),
 * which is what the portfolio's average-cost realised P&L adds up to.
 */
gross_pnl: number, 
/**
 * Fees of the trade's fills. A fill that flips the position splits its fee between the two
 * trades in proportion to the quantity that closed and that opened.
 */
fees: number, 
/**
 * Funding charged on the trade's position: at a bar's open (including funding due in a gap
 * before the bar) while it was held going into the bar, and later in a bar while it was held
 * after the bar's fills.
 */
funding: number, 
/**
 * `gross_pnl - fees - funding`.
 */
net_pnl: number, 
/**
 * `net_pnl / (entry_price * qty)`: net P&L over the largest entry notional.
 */
return_on_notional: number, 
/**
 * Bars from the entry fill's bar to the exit fill's bar; for an open trade, to the end of the
 * last bar. It counts bars fed, not time: missing bars in a gap are not counted, so across a
 * gap it understates how long the position was held. For a closed trade, the time between
 * the entry and exit fills' bars is `exit_time - entry_time`.
 */
bars_held: number, };

/**
 * `POST /sessions/{id}/orders`: an order placed by hand. It goes through the same rounding,
 * checks and broker as a strategy's order, checked at the close of the last bar fed, and takes
 * effect from the first bar that opens at or after the moment it is accepted.
 */
export type ManualOrder = { side: Side, 
/**
 * In the base asset; rounded down to the lot size.
 */
qty: number, type: ManualOrderType, 
/**
 * Required for a limit order and refused on a market one. Rounded to the tick in the order's
 * favour: a buy down, a sell up.
 */
limit_price?: number, 
/**
 * Only reduce the open position; false when left out.
 */
reduce_only?: boolean, };

/**
 * A manual order's type. A market order fills at the open of the first bar that opens at or after
 * it is accepted; a limit order rests, good till cancelled, until a bar from then on reaches its
 * price.
 */
export type ManualOrderType = "market" | "limit";

/**
 * Everything about one session: `GET /sessions/{id}`, and the first message of a `session:`
 * subscription.
 */
export type SessionState = { id: string, config: PaperConfig, status: Status, cash: number, 
/**
 * Equity now, at the last live bar's close.
 */
equity: number, position: PositionInfo, 
/**
 * Equity at each live bar's close.
 */
equity_curve: Array<EquityPoint>, open_orders: Array<PaperOrder>, fills: Array<PaperFill>, 
/**
 * Round trips so far; one still open is marked at the last close.
 */
trades: Array<PaperTrade>, 
/**
 * Funding rates charged.
 */
funding: Array<Funding>, 
/**
 * Open time of the first live bar, after warm-up.
 */
live_from: number | null, last_bar: Candle | null, 
/**
 * Buckets the live feed left out because too many of their minutes were missing.
 */
dropped: Array<number>, 
/**
 * The latest problems that did not stop the session.
 */
warnings: Array<string>, };

/**
 * What a session reports as it runs, once [`PaperSession::set_events`] gives it a channel. Sent
 * while the session is locked, so a reader that subscribes and takes the state under the same lock
 * sees each change exactly once. A bar's events are sent once its line is in the log, so a
 * subscriber never sees a bar that a resume would fetch and report again. Replaying the log
 * reports nothing.
 */
export type SessionEvent = { "event": "funding", funding: Funding, } | { "event": "fill", fill: PaperFill, } | { "event": "order", order: PaperOrder, } | { "event": "bar", bar: Candle, warm_up: boolean, equity: number | null, } | { "event": "status", status: Status, } | { "event": "dropped", open_time: number, } | { "event": "warning", message: string, };

/**
 * A message from the client.
 */
export type ClientMessage = { "type": "subscribe", topic: string, } | { "type": "unsubscribe", topic: string, };

/**
 * A message from the server.
 */
export type ServerMessage = { "type": "subscribed", topic: string, } | { "type": "unsubscribed", topic: string, } | { "type": "job", job: JobInfo, } | { "type": "candle", topic: string, candle: Candle, } | { "type": "session_state", session: SessionState, } | { "type": "session", id: string, event: SessionEvent, } | { "type": "error", error: ErrorBody, };
