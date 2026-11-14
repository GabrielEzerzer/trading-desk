// Generated from the Rust API types by `cargo run -p td-server --bin gen-api-types`.
// Do not edit by hand; CI fails when this file is out of date.

/**
 * An exchange the desk reads data from and simulates orders against.
 */
export type Venue = "binance" | "bybit" | "oanda";

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
 * Trading fees. A fill pays `price * qty * rate + qty * per_unit`, where the rate is the maker or
 * taker fraction of notional (0.001 means 0.1%) and `per_unit` a commission per unit of the base
 * asset, in the quote currency.
 */
export type FeeSchedule = { maker: number, taker: number, 
/**
 * Commission per unit of the base asset on every fill, in the quote currency, as FX brokers
 * charge. Left out of saved JSON when it is zero, the default.
 */
per_unit?: number, };

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
 * The fee paid, in the account currency: the quote asset, or the base currency on a
 * base-currency account. Negative for a rebate.
 */
fee: number, liquidity: Liquidity, 
/**
 * True for a fill of the end-of-run close-out, the order a backtest submits to close what is
 * open when it is told to end flat (each walk-forward test window does): the strategy did
 * not choose it. False for every other fill, and then left out of JSON, so a fill saved
 * before the field existed reads as false and every other fill is written as it was.
 */
close_out?: boolean, };

/**
 * The currency an account holds and reports money in, against the instrument it trades.
 *
 * - `Quote`: the instrument's quote currency, as a USDT account trading BTC/USDT. Every amount
 *   the engine computes from prices is already in it, so nothing is converted.
 * - `Base`: the instrument's base currency, as a dollar account trading USD/JPY. Profit, loss and
 *   commission arise in the quote currency (yen) and are converted at the pair's own price: an
 *   amount in the quote currency divided by the price is the amount in the base currency.
 *
 * shortcut: only the account's currency on either side of the pair it trades. A cross pair on a
 * third-currency account (EUR/JPY on a dollar account) needs a second price series to convert
 * at; add a conversion series when the desk trades crosses.
 */
export type AccountCurrency = "quote" | "base";

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
 *
 * It loads from JSON as it saves, except that `slippage_bps` may be left out or null: it then
 * takes [`default_slippage_bps`] for the instrument, and the configuration holds that number
 * from then on, so the saved result records the slippage the run used.
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
 * Starting balance in the account currency (`account_currency`).
 */
starting_cash: number, 
/**
 * Slippage on market and stop fills, in basis points of price. Left out when loading, it is
 * the instrument's default ([`default_slippage_bps`]); a value given, zero included, is
 * used as it is.
 */
slippage_bps: number, 
/**
 * Replaces the instrument's fee schedule when set.
 */
fee_override: FeeSchedule | null, 
/**
 * The largest position allowed on a margined kind (perpetual, FX), as its notional over
 * equity: 1.0 means 1x, the default. On a perpetual at 1x a long is liquidated only near a
 * price of zero and a short only once the price has about doubled (see [`Session::on_bar`]).
 * Orders that would take the position over it are rejected when submitted; orders that
 * reduce the position are always allowed (see [`crate::Context::submit`]). A kind without
 * margin, such as spot, ignores it. Left out of the saved result when it is the default.
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
close_at_end?: boolean, 
/**
 * The currency the account holds and the result reports every amount in: the instrument's
 * quote currency, the default, or its base currency, as a dollar account trading USD/JPY
 * (FX only; see [`Session::new`]). Left out of the saved result when it is the default.
 */
account_currency?: AccountCurrency, 
/**
 * Instruments on the same venue whose closed bars at `timeframe` the strategy can read beside
 * its own, aligned to them ([`crate::Context::reference`]). They are read-only: orders,
 * fills, costs and the account stay on `instrument`. Their bars are given to the run with
 * [`run_with_references`] and are part of the data fingerprint. Left out of the saved result
 * when there are none, the default.
 */
references?: Array<InstrumentId>, };

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
 * SHA-256 of the bars (warm-up bars first), asks and funding the run used, the funding being
 * every event the strategy could see; see [`data_fingerprint`].
 */
data_fingerprint: string, 
/**
 * Equity at each bar's close.
 */
equity: Array<EquityPoint>, 
/**
 * Every order submitted, rejected ones included (warm-up bars' too), in submission order.
 */
orders: Array<Order>, fills: Array<Fill>, trades: Array<Trade>, metrics: MetricsReport, benchmark: Benchmark, 
/**
 * The lowest and highest equity inside each bar, one per bar like `equity`; see
 * [`risk::intrabar`]. Empty in a result saved before results had them.
 */
equity_extremes: Array<EquityRange>, };

/**
 * Buy and hold of the same instrument over the same bars: a backtest's traded bars, or in a
 * walk-forward each test window's bars, stitched like the test runs (see
 * [`crate::WalkForwardResult::benchmark`]).
 */
export type Benchmark = { 
/**
 * Quantity bought at the first bar's open: all starting cash, after the taker fee and any
 * per-unit commission, rounded down to the lot size. In a walk-forward, the first test
 * window's purchase.
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
correlation: number | null, 
/**
 * The strategy's annualised alpha against the benchmark, on the same per-bar returns as the
 * correlation: the intercept of the least-squares fit of the strategy's returns on the
 * benchmark's, times the returns observed per 365-day year (simple, not compounded), with a
 * risk-free rate of zero; see [`metrics::alpha_beta`]. The returns per year are their count
 * over the time from the first to the last point of the strategy's equity series, so a market
 * that closes (FX at weekends) or a gap in the data is not counted as bars it did not have. A
 * return across a gap in the bars, or between two walk-forward test windows, counts as one
 * return. `None` when the correlation would be, and in a result saved before benchmarks had
 * it.
 */
alpha: number | null, 
/**
 * The slope of that fit, `cov(strategy, benchmark) / var(benchmark)`: how much of the
 * benchmark's move the strategy's equity follows.
 */
beta: number | null, };

/**
 * Equity at the close of the bar that opened at `time`.
 */
export type EquityPoint = { time: number, equity: number, };

/**
 * The lowest and highest equity one bar reached, in the account currency.
 */
export type EquityRange = { low: number, high: number, };

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
 * Close of the bar the order was decided on: the last price before it was sent, on the
 * order's side. For a market with an ask series that is the ask close for a buy and the bid
 * close for a sell.
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
 *
 * Prices are in the quote currency; every money amount (P&L, fees, funding, excursions) is in the
 * account currency, converted as [`build_trades`] describes.
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
 * close it would exit at (the ask close for a short on a market with quotes), with no exit
 * fee.
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
 * which is what the portfolio's average-cost realised P&L adds up to. On a base account it is
 * that realised P&L itself: each reducing fill's P&L against the average cost, converted at
 * that fill's price (and an open trade's remainder at the mark).
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
 * `net_pnl / (entry_price * qty)`: net P&L over the largest entry notional, the notional
 * converted to the account currency at `entry_price` on a base account.
 */
return_on_notional: number, 
/**
 * Bars from the entry fill's bar to the exit fill's bar; for an open trade, to the end of the
 * last bar. It counts bars fed, not time: missing bars in a gap are not counted, so across a
 * gap it understates how long the position was held. For a closed trade, the time between
 * the entry and exit fills' bars is `exit_time - entry_time`.
 */
bars_held: number, 
/**
 * The protective stop in force when the trade opened, if any: see [`attach_stops`]. `None`
 * without one, and in a result saved before trades kept their stop.
 */
stop: number | null, 
/**
 * `net_pnl` over the initial risk, `qty * |entry_price - stop|` (converted at the stop on a
 * base account): the trade's result in multiples of what it risked at its stop; see
 * [`attach_stops`]. `None` without a stop, with a stop at or beyond the entry price (no
 * risk), or when not finite.
 */
r_multiple: number | null, 
/**
 * Maximum adverse excursion: the largest unrealised loss of the open position at any price
 * it met, in the account currency (converted at that price on a base account), before fees
 * and funding; 0 if it never went against the entry. See [`build_trades`] for the prices
 * counted. `None` in a result saved before trades had it.
 */
mae: number | null, 
/**
 * Maximum favourable excursion: the largest unrealised gain, measured like `mae`.
 */
mfe: number | null, 
/**
 * Closed by the end-of-run close-out (see [`crate::BacktestConfig::close_at_end`]) rather
 * than by the strategy: in a walk-forward, at the end of its test window.
 */
force_closed: boolean, 
/**
 * The price move in pips, on a market with quotes (see [`td_core::Instrument::pip_size`]):
 * `sign * (exit_price - entry_price) / pip`. The spread is in it, since buys fill on the ask
 * and sells on the bid; commission and funding are not. An open trade's is on its mark, as its
 * P&L is. `None` without pips, and left out of saved JSON then.
 */
pips?: number | null, 
/**
 * `net_pnl` in pips: `net_pnl` over the value of one pip on the quantity the trade closed (its
 * whole size for one entry and one exit), in the account currency, converted at `exit_price`
 * on a base account. It is after commission. On a quote account, or when the trade exits at
 * one price, `pips - net_pips` is the commission in pips; on a base account with exits at
 * different prices it is not, since each exit's P&L is converted at its own price.
 */
net_pips?: number | null, };

export type TradeSide = "long" | "short";

/**
 * The metrics report for one equity series and its trades. Money amounts are in the run's account
 * currency (see [`crate::BacktestConfig::account_currency`]).
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
funding_paid: number, 
/**
 * The drawdown from the running peak measured inside bars: see
 * [`risk::trailing_drawdown`], with each bar's equity range. `None` when the report was
 * built without ranges (see [`report_with`]), and in a result saved before reports had it.
 */
max_drawdown_intrabar: number | null, 
/**
 * The fall from the starting equity to the lowest close, as a fraction of the starting
 * equity; see [`risk::static_drawdown`].
 */
static_drawdown: number | null, 
/**
 * [`MetricsReport::static_drawdown`] to the lowest equity inside any bar. `None` without
 * ranges.
 */
static_drawdown_intrabar: number | null, 
/**
 * The largest loss from one UTC day's start to its end, as a fraction of the starting
 * equity; see [`risk::worst_daily_loss`]. Other day rollovers are in [`risk::daily_pnl`].
 */
worst_daily_loss: number | null, 
/**
 * The largest fall from one UTC day's start to its lowest equity inside a bar, as a
 * fraction of the starting equity; see [`risk::worst_daily_loss_intraday`]. `None` without
 * ranges.
 */
worst_daily_loss_intrabar: number | null, 
/**
 * See [`risk::Intrabar::peak_leverage`]. `None` without ranges.
 */
peak_leverage: number | null, 
/**
 * Mean R multiple of the closed trades that have one (see [`crate::attach_stops`]); trades
 * without a stop are left out. The P&L expectancy per trade is `avg_trade_pnl`.
 */
expectancy_r: number | null, 
/**
 * See [`payoff_ratio`].
 */
payoff_ratio: number | null, 
/**
 * See [`longest_losing_streak`].
 */
longest_losing_streak: number, 
/**
 * Closed trades the end-of-run close-out closed (see [`Trade::force_closed`]): at most one
 * in a backtest with `close_at_end`, up to one per test window in a walk-forward.
 */
force_closed_trades: number, 
/**
 * Mean [`Trade::pips`] of the closed trades that have it, on a market with pips; `None`
 * otherwise, and left out of saved JSON then.
 */
avg_trade_pips?: number | null, 
/**
 * Mean [`Trade::net_pips`], after commission, of the closed trades that have it.
 */
avg_trade_net_pips?: number | null, };

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
 * The row's place on each axis of the grid, in the order of the spec's axes: the position
 * of its value in that axis's values, from 0. The grid index is these coordinates read as a
 * mixed-radix number with the last axis as its lowest digit. Empty in a result saved before
 * rows had them.
 */
coords: Array<number>, 
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
 * The stitched equity's range through each bar, one per point of `equity`: each test run's
 * [`BacktestResult::equity_extremes`] scaled by its segment's factor. Empty in a result saved
 * before walk-forwards had them, or when a test run has none.
 */
equity_extremes: Array<EquityRange>, 
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
export type RunKind = "backtest" | "sweep" | "oos" | "walk_forward" | "plan";

/**
 * A result as saved, tagged with its kind: `{"kind": "...", "result": {...}}`.
 */
export type SavedRun = { "kind": "backtest", "result": BacktestResult } | { "kind": "sweep", "result": SweepResult } | { "kind": "oos", "result": OosResult } | { "kind": "walk_forward", "result": WalkForwardResult } | { "kind": "plan", "result": PlanRun };

/**
 * One row of the run list. The strategy and range are the base configuration's. The metrics
 * are the backtest's, the sweep's best run's (none if no run qualified), the out-of-sample
 * segment's, or the stitched walk-forward curve's (for a plan run, its battery's walk-forward,
 * none when that did not run).
 */
export type RunSummary = { id: RunId, created_at: number, kind: RunKind, strategy: string, instrument: InstrumentId, timeframe: Timeframe, from: number, to: number, total_return: number | null, cagr: number | null, sharpe: number | null, max_drawdown: number | null, trades: number, };

/**
 * Everything the battery needs to test a strategy, with no data.
 */
export type TestPlan = { 
/**
 * Each instrument is tested on its own and gets its own verdict.
 */
instruments: Array<InstrumentId>, timeframe: Timeframe, period: Period, backtest: BacktestSettings, sweep: SweepPlan, walk_forward: WalkForwardPlan, 
/**
 * A single in-sample / out-of-sample split, reported only. `None` leaves it out.
 */
out_of_sample: Split | null, evidence: EvidenceSettings, };

/**
 * Bars that open at or after `from` and before `to`. Both must be bar boundaries.
 */
export type Period = { from: number, to: number, };

/**
 * What every run of the plan shares.
 */
export type BacktestSettings = { 
/**
 * Starting balance in the account currency (`account_currency`).
 */
starting_cash: number, 
/**
 * The perpetual leverage cap, as in [`BacktestConfig::max_leverage`]; 1.0 is 1x.
 */
max_leverage: number, 
/**
 * Slippage set for some of the plan's instruments. An instrument not listed here uses the
 * backtest config's default slippage for that instrument (#35: 2 bps on BTC and ETH, 5 bps
 * on other large caps); an empty list uses the default everywhere.
 */
slippage: Array<SlippageOverride>, 
/**
 * The currency the plan's runs hold and report in, as in
 * [`BacktestConfig::account_currency`]: the quote currency, the default, or the base
 * currency, as a dollar account trading USD/JPY. Left out of JSON when it is the default.
 */
account_currency?: AccountCurrency, 
/**
 * Fees every run of the plan pays in place of each instrument's own, as in
 * [`BacktestConfig::fee_override`]: a broker's commission per unit of an FX pair, for
 * example. Left out of JSON when it is not set, the default.
 */
fee_override?: FeeSchedule | null, };

/**
 * One instrument's slippage, overriding the backtest config's default for it.
 */
export type SlippageOverride = { 
/**
 * One of the plan's instruments.
 */
instrument: InstrumentId, 
/**
 * Slippage on market and stop fills, in basis points of price.
 */
bps: number, };

/**
 * The parameter grid. It always runs as a full grid, because the plateau test (R5) reads each
 * set's neighbours on it.
 */
export type SweepPlan = { 
/**
 * Parameters the sweep does not vary; the rest take the strategy's defaults.
 */
fixed: Params, axes: Array<ParamAxis>, objective: Objective, 
/**
 * See [`SweepSpec::min_trades`].
 */
min_trades: number, };

/**
 * Rolling walk-forward windows, in days of bars: a day is 24 hours of bars of the plan's
 * timeframe, so on an FX pair, closed at weekends, a window spans more calendar days. Each test
 * segment starts where the previous one ended, so the stitched out-of-sample curve has no holes.
 * Both lengths must be whole bars of the timeframe.
 */
export type WalkForwardPlan = { train_days: number, test_days: number, };

/**
 * The common evidence layer (#35, R2 to R6). Every field is a number: there is no way to switch a
 * test off. The defaults are #35's.
 */
export type EvidenceSettings = { min_sample: MinSample, luck: LuckTest, 
/**
 * R4: fees (the per-unit commission too), slippage and on a market with quotes each bar's
 * spread are multiplied by this, and the strategy must stay profitable.
 */
cost_stress: number, plateau: Plateau, 
/**
 * R6: how many different start offsets must all give a result of the same sign.
 */
start_offsets: number, };

/**
 * R2: below any of these, the verdict is `not_enough_evidence`.
 *
 * A day here is a day of bars, as in [`WalkForwardPlan`]: 24 hours of the market's open bars,
 * so the days a market is closed (an FX pair's weekends) do not count. It is the unit the
 * walk-forward windows are laid in, so a run's out-of-sample days are its windows times
 * `test_days`.
 */
export type MinSample = { 
/**
 * Closed out-of-sample trades.
 */
trades: number, 
/**
 * Days of bars covered by the stitched out-of-sample curve, as defined above.
 */
days: number, 
/**
 * Walk-forward windows.
 */
windows: number, };

/**
 * R3: the same sweep repeated on bar permutations.
 */
export type LuckTest = { permutations: number, 
/**
 * p at or below this passes.
 */
pass_p: number, 
/**
 * p above `pass_p` and at or below this gives `not_enough_evidence`; above it fails.
 */
inconclusive_p: number, };

/**
 * R5: how the neighbours on the grid of the sets the walk-forward chose must do on its test
 * windows ([`crate::PlateauReport`]).
 */
export type Plateau = { 
/**
 * The median over the neighbours of each one's objective over the chosen sets' on the same
 * windows must be at least this.
 */
min_median_ratio: number, 
/**
 * At least this share of the neighbours must be profitable.
 */
min_positive_share: number, };

export type RuleKind = "prop_firm" | "portfolio" | "high_risk";

/**
 * What counts as tradable for one strategy: a kind with that strategy's limits. Each kind's
 * limits default to #35's; a strategy overrides a field by naming it.
 */
export type VerdictRule = { "kind": "prop_firm", firm: FirmLimits, limits: PropFirmLimits, 
/**
 * What one attempt costs and what a funded account pays, so the battery can price an
 * attempt in money and the kind gains its value line. `None`, and absent from a rule
 * saved before the field existed, prices nothing. Left out of JSON when `None`, so a rule
 * without terms is written as it was before the field existed.
 */
attempt?: AttemptTerms | null, } | { "kind": "portfolio", benchmark: PortfolioBenchmark, limits: PortfolioLimits, } | { "kind": "high_risk", limits: HighRiskLimits, };

/**
 * A prop firm's own rules. Losses and drawdowns are fractions of the starting balance. Each
 * optional rule is left out of JSON when it is off, so a firm without it is written as it was
 * before the rule existed.
 *
 * shortcut: no time limit on the challenge; add one if a firm the desk trades on has it.
 */
export type FirmLimits = { 
/**
 * The largest loss allowed within one day.
 */
daily_loss: number, 
/**
 * The largest drawdown allowed, measured as `drawdown` says.
 */
max_drawdown: number, drawdown: DrawdownMode, 
/**
 * The profit that passes the challenge, or its first phase when `second_target` is set.
 */
profit_target: number, 
/**
 * The profit target of a second phase, which starts from a fresh balance on the day after
 * the first phase passes. Both phases share the rule's `pass_horizon_days`. `None` is a
 * one-phase challenge.
 */
second_target?: number | null, 
/**
 * The fewest trading days each phase must hold before its target can pass it: days that
 * pass `trading_day`'s test (without one, days on which the strategy filled an order),
 * counted within the phase. 0 sets no minimum.
 */
min_trading_days?: number, 
/**
 * The best-day rule: a phase passes only once its largest single day's profit is at most
 * this share of the phase's total profit. `None` sets no such rule.
 */
best_day_share?: number | null, 
/**
 * The day cap: the most one day counts toward an evaluation phase's target, as a share of
 * that phase's target. A winning day counts its profit up to `day_target_share *
 * profit_target` (or `second_target` in the second phase); a losing day counts its whole
 * loss; the phase passes once the counted profit reaches the target. Profit above the cap
 * stays in the account, and the loss limits act on the account. Independent of
 * `best_day_share`: with both set, both must hold. The funded account has no such rule.
 * `None` counts every day in full.
 */
day_target_share?: number | null, 
/**
 * The firm's own test for a trading day, which counts for `min_trading_days` and for the
 * funded account's `days_to_first_payout`. `None` counts any day with a fill.
 */
trading_day?: TradingDayTest | null, 
/**
 * For a trailing drawdown, the level the floor stops rising at, as a fraction of the starting
 * balance above the start: 0 stops it at the starting balance. `None` lets it trail for ever.
 */
trailing_floor_lock?: number | null, 
/**
 * Where the firm's day ends, for its daily loss limit and its trading days. When set, the
 * worst-daily-loss line measures these days only, and the attempt walks read each day's
 * return, its low and its trading-day flag on them. `None` measures the line on the worse of
 * days ending at midnight UTC and at 17:00 New York, and walks days ending at midnight UTC.
 */
day_rollover?: DayRollover | null, };

/**
 * The terms of one attempt at a prop-firm challenge: its fees, the funded account it leads to
 * and the size traded in each. Money is in the account currency; fractions are of the starting
 * balance. A firm's numbers are plan input; none are built in.
 */
export type AttemptTerms = { 
/**
 * The starting balance of the evaluation and of the funded account, which turns fractions
 * into money.
 */
account_size: number, 
/**
 * The fee paid to start the evaluation.
 */
evaluation_fee: number, 
/**
 * A fee charged on passing the evaluation.
 */
pass_fee: number, 
/**
 * Whether the evaluation fee is paid back with the first payout.
 */
refund_fee_with_first_payout: boolean, 
/**
 * The trader's share of each payout's profit, above 0 and at most 1.
 */
profit_split: number, 
/**
 * The trading days the funded account must hold before its first payout: the first payout
 * may be asked for at the close of the day on which the account has held this many trading
 * days, as the firm's `trading_day` test counts them (without one, days with a fill).
 */
days_to_first_payout: number, 
/**
 * A wait in calendar days before the first payout, counted from the funded account's first
 * trading day (as `days_to_first_payout` counts them): the first payout may be asked for at
 * the close of the day this many days after that one, traded or not. With both set, both
 * must be met. `None` sets no such wait, and is left out of JSON.
 */
calendar_days_to_first_payout?: number | null, 
/**
 * The days from one payout to the earliest next one, traded or not.
 */
payout_interval_days: number, 
/**
 * The smallest payout the firm makes, after the split.
 */
min_payout: number, 
/**
 * The most profit one payout may count, as a fraction of the starting balance. A payout
 * counts only the profit above the start, or above the floor if that is higher, and never
 * moves the floor. `None` sets no cap.
 */
payout_cap: number | null, 
/**
 * The most payouts one funded account makes. `None` sets no limit.
 */
max_payouts: number | null, 
/**
 * Whether the first payout fixes the floor for the rest of the account: at the starting
 * balance, or where the floor stands at that payout if that is higher.
 */
payout_locks_floor: boolean, 
/**
 * Whether each payout returns the balance to the start: it counts the profit above the
 * start up to `payout_cap`, and any profit above the cap is lost rather than left in the
 * account. The loss limits stay as they were. It cannot be set with `payout_locks_floor`,
 * and a `prop_firm` rule refuses it with a trailing drawdown: either would put the floor
 * at or above the start a payout returns to. `false` is left out of JSON.
 */
payout_resets_balance?: boolean, 
/**
 * Days the funded account is simulated for after a pass.
 */
funded_horizon_days: number, 
/**
 * The size traded in the evaluation, as a multiple of the size the strategy was tested at:
 * each daily return is multiplied by it.
 */
evaluation_size: number, 
/**
 * The same for the funded account.
 */
funded_size: number, 
/**
 * The factor the mean daily return is scaled by for the second set of figures, between 0
 * and 1: 0.5 asks what an attempt is worth if the edge is half what the test shows.
 */
reduced_edge_factor: number, };

/**
 * A firm's test for a trading day. A day counts when one of its fills passes the test; each fill
 * is judged on its own, so several small closes on one day do not add up to a large one. With
 * every field off, any fill passes, as without a test.
 *
 * A fill closes a trade when it reduces a position: the closed quantity is the part of the
 * position it takes off, so a partial close counts by its own size, and a fill that flips the
 * position closes all of the old one (and opens a new one, which is not a close). Positions and
 * their average entry prices are tracked per instrument across the fills.
 */
export type TradingDayTest = { 
/**
 * Only a closing fill can count, on the day the close happens: a day that only opens or
 * adds to a position is not a trading day. Setting either smallest value below implies it.
 */
needs_close?: boolean, 
/**
 * The smallest value of the closed part of the trade, as a share of the starting balance:
 * the closed quantity times the position's average entry price, in the account currency at
 * that price, at the size the strategy was tested at. Since it is judged at that size, a
 * rule that sets it is refused with an `attempt` whose `evaluation_size` or `funded_size`
 * is not 1, until the flags are built per size. `None` sets no smallest size.
 */
min_trade_size?: number | null, 
/**
 * The smallest profit or loss of the closed part, as a share of its own value, before fees:
 * `|exit price - average entry price| / average entry price`. `None` sets no smallest move.
 */
min_trade_move?: number | null, };

/**
 * When one trading day ends and the next begins, for the daily P&L and daily loss limits.
 */
export type DayRollover = { "kind": "utc", offset_minutes: number, } | { "kind": "new_york1700" };

export type DrawdownMode = "static" | "trailing";

export type PropFirmLimits = { 
/**
 * The worst daily loss and the max drawdown, measured inside bars, must stay within this
 * share of the firm's limits.
 */
share_of_firm_limits: number, 
/**
 * The chance of reaching the profit target within `pass_horizon_days`, before breaching a
 * limit, must be at least this.
 */
min_pass_probability: number, 
/**
 * Days a simulated challenge runs for the pass odds: a path that has not reached the target
 * by then counts as not passed. 90 by default, a quarter; a plan for a challenge with no time
 * limit sets how long it is willing to run one. A rule saved before the field existed reads
 * as 90, the horizon it was judged on.
 */
pass_horizon_days: number, 
/**
 * The chance of breaching a limit within `breach_horizon_days` must be at most this.
 */
max_breach_probability: number, 
/**
 * Days a simulated attempt runs for the breach odds, and the length of each attempt the
 * max-drawdown line reads on the test's curve.
 */
breach_horizon_days: number, };

export type PortfolioBenchmark = "buy_and_hold" | "cash";

export type PortfolioLimits = { 
/**
 * Out-of-sample Sharpe at least this.
 */
min_sharpe: number, 
/**
 * Max drawdown, as a fraction of its peak, at most this.
 */
max_drawdown: number, 
/**
 * Max drawdown at most this share of the benchmark's. Not judged against `cash`, which has
 * none.
 */
max_drawdown_vs_benchmark: number, 
/**
 * Annualised alpha against the benchmark strictly above this.
 */
min_alpha: number, 
/**
 * The bootstrap chance that the mean daily return is zero or less, at most this.
 */
max_p_mean_not_positive: number, 
/**
 * The 95th percentile of the max drawdown over resampled paths of the daily returns (days
 * drawn with replacement in blocks, as R7 does), at most this. The default is above `max_drawdown`'s because the 95th percentile of
 * resampled paths sits well above the drawdown of the one path that happened, and grows with
 * the length of the out-of-sample period; at 0.45 the line fails a strategy whose drawdown was
 * small only because its losing days happened not to cluster, not every strategy near the
 * historical limit. A rule saved before this limit existed reads with the default.
 */
max_resampled_drawdown: number, };

export type HighRiskLimits = { 
/**
 * Closed out-of-sample trades, at least this many.
 */
min_trades: number, 
/**
 * Mean net P&L per closed trade strictly above this, in the run's account currency (on a
 * plan's runs, the one its backtest settings name).
 */
min_expectancy: number, 
/**
 * The bootstrap's lower bound on that mean strictly above this.
 */
min_expectancy_lower_bound: number, 
/**
 * Profit factor at least this.
 */
min_profit_factor: number, 
/**
 * The chance of losing `ruin_drawdown` of the account, at most this.
 */
max_risk_of_ruin: number, 
/**
 * The drawdown from the running peak of equity that counts as ruin, above 0 and below 1.
 */
ruin_drawdown: number, 
/**
 * The out-of-sample return with the single best trade removed, strictly above this.
 */
min_return_without_best_trade: number, };

export type Layer = "integrity" | "evidence" | "kind";

/**
 * How a value is held against its limit.
 */
export type Comparison = "at_least" | "above" | "at_most";

/**
 * How the GUI writes a value and its limit.
 */
export type Unit = "percent" | "ratio" | "count" | "days" | "quote";

export type Outcome = "pass" | "warn" | "fail" | "inconclusive";

/**
 * One criterion of a verdict.
 */
export type Criterion = { layer: Layer, 
/**
 * A stable key, such as `max_drawdown`.
 */
id: string, 
/**
 * What the GUI shows, such as "Max drawdown".
 */
name: string, 
/**
 * `None` when the battery could not compute it; the criterion then does not pass.
 */
value: number | null, comparison: Comparison, 
/**
 * `None` when there is no limit to hold the value against, such as a drawdown against a
 * benchmark whose drawdown is missing; the line is then inconclusive and `note` says why.
 */
limit: number | null, unit: Unit, outcome: Outcome, 
/**
 * Why the line warns or is inconclusive, or what its value leaves out, in words the GUI
 * shows. `None` when the value and the limit say it all.
 */
note: string | null, };

export type VerdictState = "trade" | "no_trade" | "not_enough_evidence" | "invalid";

/**
 * One instrument's verdict.
 */
export type Verdict = { instrument: InstrumentId, state: VerdictState, 
/**
 * Every line of every layer that was judged, integrity first.
 */
criteria: Array<Criterion>, 
/**
 * Costs the runs behind the verdict leave out, and assumptions a line passed on that the
 * engine cannot check, each named, so a verdict that rests on one is visible. They do not
 * change the state. Empty in a verdict saved before warnings existed.
 */
warnings: Array<string>, };

/**
 * A limit of a strategy's rule that is looser than its kind's default (#113), so a verdict
 * judged under it can say so. A tightened limit is not listed.
 */
export type LoosenedLimit = { 
/**
 * The limit's field in the kind's limits, such as `min_sharpe`.
 */
name: string, 
/**
 * The rule's value.
 */
value: number, 
/**
 * The kind's default.
 */
default: number, };

/**
 * One instrument's test battery (#35) under a strategy's plan, as saved: what was tested, the
 * engine that tested it and everything the battery produced, its verdict and seed included.
 */
export type PlanRun = { strategy: string, 
/**
 * The plan's backtest on the instrument ([`TestPlan::backtest_config`]): the period, the
 * fixed parameters, the starting cash, the slippage and the instrument's trading rules.
 */
config: BacktestConfig, 
/**
 * The plan and the rule as they were when the battery ran.
 */
plan: TestPlan, rule: VerdictRule, 
/**
 * The td-engine crate version that produced the run.
 */
engine_version: string, 
/**
 * The [`ENGINE_REVISION`] that produced the run. A verdict from another revision may differ
 * from what this engine would give on the same data.
 */
engine_revision: number, 
/**
 * The fingerprint of the data the battery was given, over `config`'s range; see
 * [`config_fingerprint`].
 */
data_fingerprint: string, battery: BatteryResult, };

/**
 * Everything one battery produced: the verdict, and the runs and numbers behind it for saving
 * and showing.
 */
export type BatteryResult = { verdict: Verdict, seed: number, 
/**
 * The runs the progress counted, its total.
 */
runs: number, integrity: IntegrityReport, 
/**
 * The sweep over the full period. `None` when integrity failed.
 */
sweep: SweepResult | null, 
/**
 * R1. `None` when integrity failed, no window fits the data, or a train window had no
 * qualifying run (`walk_forward_error` says which).
 */
walk_forward: WalkForwardResult | null, walk_forward_error: string | null, 
/**
 * The inputs of the evidence lines.
 */
evidence: EvidenceInputs, 
/**
 * The inputs of the kind lines.
 */
kind: KindInputs, oos_integrity: OosIntegrityInputs, 
/**
 * R5: the walk-forward's chosen sets against their grid neighbours, out of sample. `None`
 * without a walk-forward.
 */
plateau: PlateauReport | null, 
/**
 * R7: the block bootstrap of the out-of-sample daily returns.
 */
daily_bootstrap: MeanBootstrap | null, 
/**
 * The block bootstrap of the max drawdown over the same daily returns, for a `portfolio`
 * rule. `None` for the other kinds, without a curve, and in a result saved before it existed.
 */
drawdown_bootstrap: DrawdownBootstrap | null, 
/**
 * The bootstrap of the closed out-of-sample trades' net P&L, for `high_risk`'s lower bound.
 */
trade_bootstrap: MeanBootstrap | null, 
/**
 * The prop-firm challenge odds, for a `prop_firm` rule: passing within the rule's
 * `pass_horizon_days`, breaching within its `breach_horizon_days`.
 */
prop_firm_odds: PropFirmOdds | null, 
/**
 * The value of one attempt, for a `prop_firm` rule with attempt terms: the evaluation to the
 * rule's `pass_horizon_days`, then the funded account. `None` otherwise, and in a result
 * saved before it existed.
 */
prop_firm_attempt: PropFirmAttempt | null, 
/**
 * R8.
 */
breakdown: Breakdown | null, 
/**
 * R4. `None` when it did not run.
 */
cost_stress: CostStress | null, 
/**
 * R6, one per offset. Empty when it did not run.
 */
start_offsets: Array<StartOffset>, 
/**
 * R3. `None` when it did not run, was cancelled or had no qualifying real sweep
 * (`luck_note` says which).
 */
permutation: PermutationResult | null, luck_note: string | null, 
/**
 * True when a cancel stopped R3, so the luck line lacks its p-value. False when R3 finished,
 * was skipped or did not run; a result saved before this field existed reads as false.
 */
luck_cancelled: boolean, };

/**
 * The counts behind I1 to I3.
 */
export type IntegrityReport = { 
/**
 * Buckets of the period's open hours: those not wholly inside the market's closed hours.
 */
open_buckets: number, 
/**
 * Open buckets with no bar.
 */
dropped_buckets: number, inputs: IntegrityInputs, 
/**
 * I3's split: the first half is the bars that open before it.
 */
prefix_split: number | null, };

/**
 * The integrity checks of the data and the engine (I1 to I3), computed by the battery on the
 * plan's backtest before anything else runs. I2 and I3 then also run on each parameter set the
 * walk-forward chose, and hold the worst result over every set checked. A check the battery
 * could not make is `None` and fails, which makes the verdict `invalid`.
 */
export type IntegrityInputs = { 
/**
 * I1: the buckets of the period's open hours that have no bar, over all of them.
 */
dropped_share: number | null, 
/**
 * I2: 0 when two runs of each backtest checked gave identical fills and equity, 1
 * otherwise.
 */
repeat_differences: number | null, 
/**
 * I3: the orders decided on the first half of the period that differ between a run on
 * that half alone and the full run, counting an order only one of them has; the most over
 * the backtests checked.
 */
prefix_differences: number | null, };

/**
 * The statistics the evidence layer reads (#35, R1 to R6), computed by the battery.
 */
export type EvidenceInputs = { 
/**
 * R1: total return of the stitched out-of-sample curve.
 */
oos_return: number | null, 
/**
 * R1: the share of walk-forward windows with a positive return.
 */
profitable_windows: number | null, 
/**
 * R2: closed out-of-sample trades.
 */
oos_trades: number, 
/**
 * R2: days of bars covered by the stitched out-of-sample curve, as in
 * [`MinSample`](crate::MinSample): 24 hours of the market's open bars, so the days it is
 * closed do not count. On data with no gaps this is the windows times `test_days`.
 */
oos_days: number, 
/**
 * R2: walk-forward windows.
 */
windows: number, 
/**
 * R3: the luck test's p. `None` when it was cancelled or not run.
 */
luck_p: number | null, 
/**
 * R4: the out-of-sample return with fees, the per-unit commission, slippage and on a market
 * with quotes each bar's spread multiplied by the cost stress.
 */
stressed_return: number | null, 
/**
 * R5: the neighbours' median objective over R1's, both out of sample
 * ([`crate::PlateauReport`]).
 */
plateau_median_ratio: number | null, 
/**
 * R5: the share of neighbours with a positive out-of-sample return.
 */
plateau_positive_share: number | null, 
/**
 * R6: how many start offsets gave a result of the same sign as the main run.
 */
start_offsets_same_sign: number, };

/**
 * The out-of-sample statistics the kind rules read, computed by the battery on the stitched
 * walk-forward out-of-sample curve. Fractions are of equity unless said otherwise. A statistic
 * the battery could not compute is `None` and fails its criterion. Each kind reads only its own
 * fields.
 */
export type KindInputs = { 
/**
 * Annualised Sharpe ratio of daily returns.
 */
sharpe: number | null, 
/**
 * Max drawdown from the running peak, at bar closes.
 */
max_drawdown: number | null, 
/**
 * The buy-and-hold benchmark's max drawdown over the same bars.
 */
benchmark_max_drawdown: number | null, 
/**
 * Annualised alpha against the rule's benchmark.
 */
alpha: number | null, 
/**
 * The block bootstrap's chance that the mean daily return is zero or less (R7).
 */
p_mean_not_positive: number | null, 
/**
 * The 95th percentile of the max drawdown over resampled paths of the daily returns (days
 * drawn with replacement in blocks, as R7 does).
 */
resampled_drawdown: number | null, 
/**
 * Closed trades.
 */
trades: number, 
/**
 * Mean net P&L per closed trade, in the run's account currency.
 */
expectancy: number | null, 
/**
 * The bootstrap's lower bound on the expectancy.
 */
expectancy_lower_bound: number | null, profit_factor: number | null, 
/**
 * The chance of a drawdown of the rule's `ruin_drawdown` from the running peak.
 */
risk_of_ruin: number | null, 
/**
 * The total return with the single best trade removed.
 */
return_without_best_trade: number | null, 
/**
 * The worst loss within one day, on the firm's day when its rule states one, as a fraction
 * of that day's starting balance: what an attempt started that morning would have lost.
 */
worst_daily_loss: number | null, 
/**
 * Max drawdown from the starting balance. For a `prop_firm` rule, the worst over attempts of
 * its `breach_horizon_days` started on each day of the curve, each from its own starting
 * balance; for the other kinds, over the whole curve.
 */
intrabar_drawdown_static: number | null, 
/**
 * Max drawdown from the running peak, as a fraction of the starting balance, over the same
 * span as `intrabar_drawdown_static`: for a `prop_firm` rule, each attempt's peak starts at
 * its own starting balance, and the fall is a fraction of that balance. With the firm's
 * `trailing_floor_lock`, the peak counts only up to where the floor reaches the lock, so the
 * fall is to the floor the firm applies.
 */
intrabar_drawdown_trailing: number | null, 
/**
 * The chance of reaching the firm's profit target before breaching a limit.
 */
pass_probability: number | null, 
/**
 * The chance of breaching a limit within the rule's horizon.
 */
breach_probability: number | null, 
/**
 * The value of one attempt in the account currency with the edge reduced by the rule's
 * `reduced_edge_factor`, for a rule with attempt terms.
 */
attempt_value: number | null, 
/**
 * Whether the pass odds, the breach odds and the attempt's value checked the loss limits on
 * each day's lowest equity, inside the day. False when they were checked at daily closes
 * only, because the curve had no equity inside its bars, and for the other kinds. Left out
 * when false, so a result saved before it existed reads as checked at closes, as it was.
 */
limits_inside_day?: boolean, };

/**
 * The integrity checks read on the stitched walk-forward out-of-sample curve (I4 and I5),
 * computed once it exists.
 */
export type OosIntegrityInputs = { 
/**
 * I4: the annualised Sharpe ratio of the curve's daily returns.
 */
sharpe: number | null, 
/**
 * I5: the highest notional over equity inside any bar.
 */
peak_leverage: number | null, 
/**
 * I5: the lowest equity inside any bar, over the starting cash. Zero is a bankrupt account:
 * a liquidation leaves the account at exactly zero.
 */
lowest_equity: number | null, 
/**
 * Whether the stitched out-of-sample curve exists. A curve can exist without the equity
 * ranges inside its bars that I5 reads (a test run that kept none), and the lines then say
 * so instead of blaming the sample.
 */
curve: boolean, };

/**
 * R5, out of sample: the walk-forward's chosen parameter sets against their neighbours on the
 * grid. A neighbour is a direction on the grid, a step of -1, 0 or 1 on each axis (not all 0).
 * In each window it is the set that window chose moved one step that way, run on the window's
 * test bars as its chosen set was (warmed up on the train bars, closed out at the end). Each
 * direction is judged only on the windows where its set ran: a window where it is off the grid
 * or the strategy refuses it is left out, never filled with the chosen set's own run, which would
 * pull the direction towards R1. Its runs are stitched as R1's curve is, and so are the chosen
 * sets' own runs in the same windows, its reference. A direction that ran in no window is left
 * out.
 *
 * The ratio is the median over the directions of each one's objective against its reference: the
 * objective over the reference with `Maximize`, for a direction whose reference is above zero,
 * and the reference over the objective with `Minimize`, so a ratio below 1 is worse either way.
 * A direction with an undefined objective has the worst ratio (minus infinity with `Maximize`, 0
 * with `Minimize`); one whose reference is undefined, or at or below zero with `Maximize`, has
 * none and is left out of the median. The share is the directions whose stitched return is
 * above zero.
 *
 * A result saved before R5 moved out of sample reads with `directions` empty and its ratio and
 * share as they were judged then, on the full-period sweep.
 */
export type PlateauReport = { 
/**
 * R1's stitched objective; `None` when it is undefined.
 */
best_objective: number | null, directions: Array<PlateauDirection>, median_ratio: number | null, positive_share: number | null, };

/**
 * One of R5's neighbours, stitched over the windows.
 */
export type PlateauDirection = { 
/**
 * The step on each axis of the grid, in the order of the plan's axes.
 */
steps: Array<number>, 
/**
 * The windows in which the stepped set ran, the only ones it is judged on.
 */
windows: number, 
/**
 * The stitched objective; `None` when it is undefined.
 */
objective: number | null, total_return: number | null, 
/**
 * The chosen sets' own runs in the same windows, stitched the same way: their objective.
 * `None` when it is undefined, or in a result saved before directions had it.
 */
reference: number | null, };

/**
 * The distribution of a sample's mean under resampling.
 */
export type MeanBootstrap = { 
/**
 * The mean of the sample itself.
 */
mean: number, 
/**
 * The fraction of resampled means at or below zero. The `portfolio` kind rule asks for at
 * most 0.10 on the daily returns.
 */
p_mean_at_most_zero: number, 
/**
 * The one-sided lower confidence bound on the mean: the `1 - confidence` quantile of the
 * resampled means (the percentile method). The `high_risk` kind rule asks for it to be above
 * zero on the trade P&Ls; that rule asks a one-sided question, so the bound is one-sided.
 */
lower_bound: number, quantiles: Quantiles, resamples: number, mean_block: number, };

/**
 * The distribution of the max drawdown of a series of daily returns under resampling.
 */
export type DrawdownBootstrap = { 
/**
 * The max drawdown of the returns in the order they happened.
 */
historical: number, 
/**
 * The median of the resampled paths' max drawdowns.
 */
p50: number, 
/**
 * The 95th percentile of the resampled paths' max drawdowns. The `portfolio` kind rule asks
 * for it to be at most its `max_resampled_drawdown`.
 */
p95: number, resamples: number, mean_block: number, };

/**
 * Quantiles of the resampled means.
 */
export type Quantiles = { p05: number, p25: number, p50: number, p75: number, p95: number, };

/**
 * The odds of a prop-firm challenge over simulated paths. The battery takes `pass` and `breach`
 * from runs to their own horizons, and the same run when the horizons are equal.
 */
export type PropFirmOdds = { 
/**
 * The fraction of paths that pass every phase within the horizon before breaching any
 * limit. The `prop_firm` kind rule asks for at least 0.60.
 */
pass: number, 
/**
 * The fraction of paths that breach a limit within the horizon: in a phase of the
 * evaluation, or after the last phase passes, trading on as the funded account from a fresh
 * balance and a fresh peak, as a firm starts each phase and the funded account again at the
 * account size. With attempt terms the funded account takes its payouts by them, which
 * leave the balance and move no floor, unless the terms lock the floor at the first payout;
 * without them it takes none. The `prop_firm` kind rule asks for at most 0.10 over 90 days.
 */
breach: number, paths: number, 
/**
 * The days the paths were drawn from, one per daily return. `None` in a result saved before
 * it existed.
 */
days: number | null, 
/**
 * How many of `days` count as trading days: the days the traded flags mark, so a reader can
 * see how sparse the days are that the firm's minimum and the wait before the first payout
 * count. `None` in a result saved before it existed.
 */
trading_days: number | null, };

/**
 * The value of a prop-firm attempt at the edge the test shows, at a reduced one and at none.
 */
export type PropFirmAttempt = { 
/**
 * From the daily returns as they are.
 */
full: AttemptFigures, 
/**
 * From the daily returns with a positive mean scaled by `reduced_edge_factor` and their
 * shape unchanged; with a negative mean, the same as `full`, since a reduction never adds
 * edge. The `prop_firm` kind rule asks for its `value` to be above zero.
 */
reduced: AttemptFigures, 
/**
 * From the daily returns with their mean removed (a factor of 0) and their shape unchanged:
 * what the terms alone are worth, with no edge. A positive value here says the terms pay for
 * an attempt by themselves, so a positive `reduced` value does not show that the edge does.
 * With a negative mean, removing it reads better than `full`: this is the terms without an
 * edge, not a reduction of the one the test shows.
 * Reported only: no line of the verdict judges it. `None` in a result saved before it
 * existed.
 */
zero_edge: AttemptFigures | null, reduced_edge_factor: number, paths: number, };

/**
 * What one attempt is worth, from one series of daily returns.
 */
export type AttemptFigures = { 
/**
 * The fraction of attempts that pass the evaluation at the terms' evaluation size.
 */
pass: number, 
/**
 * The mean paid to the trader per funded account, after the split, in the account currency.
 * `None` when no attempt passed.
 */
payouts_per_funded: number | null, 
/**
 * The fraction of attempts that end with at least one payout.
 */
payout_chance: number, 
/**
 * The value of one attempt in the account currency: the mean over attempts of the payouts
 * received plus any refunded fee, less the fees paid.
 */
value: number, };

/**
 * R8: where the out-of-sample profit came from. Reported only.
 */
export type Breakdown = { long: SideResult, short: SideResult, 
/**
 * Net funding paid on the stitched curve; negative when it was received.
 */
funding_paid: number, 
/**
 * Windows where the instrument's price rose over the test window, and the strategy's return
 * in them, compounded. The price is the quote currency per unit of the base, so a long
 * position gained in those windows in either currency: on a dollar account trading USD/JPY,
 * a rising window is one where the dollar strengthened against the yen.
 */
rising: RegimeResult, 
/**
 * Windows where it fell or stayed flat.
 */
falling: RegimeResult, 
/**
 * The best closed trade's net P&L.
 */
best_trade_pnl: number | null, 
/**
 * The out-of-sample return without it.
 */
return_without_best_trade: number | null, };

export type SideResult = { 
/**
 * Closed trades.
 */
trades: number, 
/**
 * Their net P&L, in the account currency.
 */
net_pnl: number, };

export type RegimeResult = { windows: number, strategy_return: number, };

/**
 * R4: each test window's chosen parameters run again at higher costs.
 */
export type CostStress = { 
/**
 * The fees, the per-unit commission, slippage and, on a market with quotes, each bar's spread
 * are multiplied by this.
 */
multiplier: number, 
/**
 * Each window's return at the stressed costs.
 */
window_returns: Array<number>, 
/**
 * The windows compounded, as the stitched curve compounds them.
 */
total_return: number, };

/**
 * R6: one walk-forward with its windows moved.
 */
export type StartOffset = { 
/**
 * How many of the period's last bars this run leaves out, which moves every window boundary
 * back by as many bars.
 */
shift_bars: number, 
/**
 * The windows that fit.
 */
windows: number, 
/**
 * The stitched out-of-sample return; `None` when the walk-forward could not run.
 */
oos_return: number | null, error: string | null, };

/**
 * A luck test by bar permutation (R3 in #35): the same sweep on the real bars and on
 * `permutations` shuffled copies of them.
 */
export type PermutationSpec = { sweep: SweepSpec, 
/**
 * Shuffled copies to sweep; #35 uses 199, so the smallest possible p-value is 0.005 when
 * every copy has a qualifying run.
 */
permutations: number, 
/**
 * Copy k is shuffled with a seed derived from this one and k, so the same seed shuffles the
 * same copies.
 */
seed: number, };

/**
 * The result of a luck test by bar permutation.
 */
export type PermutationResult = { spec: PermutationSpec, engine_version: string, 
/**
 * The fingerprint of the real data in the base range; see [`crate::data_fingerprint`].
 */
data_fingerprint: string, 
/**
 * The best objective value of the sweep on the real bars.
 */
real_best: number, 
/**
 * The best objective value of each shuffled copy's sweep, in copy order; `None` when no run
 * on that copy qualified.
 */
permuted_best: Array<number | null>, 
/**
 * Copies with a qualifying run: the entries of `permuted_best` that are not `None`.
 */
qualified_copies: number, 
/**
 * Qualifying copies whose best is at least as good as the real best: at or above it when
 * maximising, at or below it when minimising, ties included.
 */
at_least_as_good: number, 
/**
 * `(1 + at_least_as_good) / (1 + qualified_copies)`: the chance of a best this good if the
 * bars' order carried no information, given that the real sweep qualified. The test runs
 * only when it did, so the real bars are compared with the copies that qualified too; if
 * the order carried nothing, the real best is then equally likely to rank anywhere among
 * those `1 + qualified_copies` bests, which makes this the exact permutation p-value
 * (counting the real sweep as one of the draws keeps it above zero; Phipson and Smyth,
 * 2010). Dividing by all the copies instead would count the ones with no qualifying run as
 * worse than the real best and bias the p-value low.
 *
 * The smallest it can be is `1 / (1 + qualified_copies)`. When that is above a pass
 * threshold, the test cannot pass however good the real best is: too few copies qualified.
 */
p_value: number, };

/**
 * A session beside its backtest.
 */
export type SessionComparison = { session: SideSummary, backtest: SideSummary, 
/**
 * The session's P&L over the backtest's; `null` when the backtest's P&L is zero. Negative
 * when one made money and the other lost it.
 */
pnl_ratio: number | null, 
/**
 * Fills paired one to one through their orders, in the order of the orders' key.
 */
fills: Array<FillPair>, 
/**
 * Fills of the session with no partner in the backtest.
 */
session_only_fills: Array<Fill>, 
/**
 * Fills of the backtest with no partner in the session.
 */
backtest_only_fills: Array<Fill>, 
/**
 * Orders paired one to one, in the order of their key.
 */
orders: Array<OrderPair>, session_only_orders: Array<Order>, backtest_only_orders: Array<Order>, 
/**
 * Every difference found, each named; empty when the two sides agree.
 */
differences: Array<Difference>, };

/**
 * One side's costs and result.
 */
export type SideSummary = { 
/**
 * Orders submitted, rejected ones included.
 */
orders: number, fills: number, 
/**
 * Sum of fill fees; negative if rebates outweighed fees.
 */
fees: number, 
/**
 * Net funding paid; negative when the position received funding.
 */
funding: number, starting_equity: number, 
/**
 * Equity at the last live bar's close, or the starting equity before the first.
 */
final_equity: number, 
/**
 * `final_equity - starting_equity`.
 */
pnl: number, };

/**
 * A session fill and the backtest fill paired with it, with the session's value less the
 * backtest's for each field.
 */
export type FillPair = { 
/**
 * The open time of the bar the fills' orders were decided on.
 */
decided_at: number, session: Fill, backtest: Fill, time_diff_ms: number, price_diff: number, qty_diff: number, fee_diff: number, };

/**
 * A session order and the backtest order paired with it.
 */
export type OrderPair = { session: Order, backtest: Order, };

/**
 * What a [`Difference`] is about.
 */
export type DifferenceKind = "fill_time" | "fill_price" | "fill_qty" | "fill_fee" | "session_only_fill" | "backtest_only_fill" | "order_request" | "order_status" | "session_only_order" | "backtest_only_order" | "fees" | "funding" | "result";

/**
 * One difference between the session and the backtest, in a plain sentence.
 */
export type Difference = { kind: DifferenceKind, message: string, };

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

export type HealthResponse = { version: string, 
/**
 * The data cutoff date (`TD_DATA_CUTOFF`, `YYYY-MM-DD` UTC) when one is in force: every run,
 * sweep, walk-forward and test battery whose range ends after that date is refused. `null`
 * when there is none.
 */
data_cutoff: string | null, };

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
 * Not before 2017-01-01 00:00 UTC (1483228800000) for binance and bybit, or before
 * 2007-01-01 00:00 UTC (1167609600000) for oanda; earlier is a 400.
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
 * The month's open minutes by the instrument's trading hours, which a complete month stores:
 * every minute of the month for a market that never closes, the minutes outside its weekends
 * for FX.
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
 * The data folder, as an absolute path. It also holds saved runs and paper sessions, which
 * are not market data.
 */
data_dir: string, 
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
 * One month file of an instrument's candles and its size on disk, with its closure record's,
 * which a delete of the month removes with it.
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
 * the venue's earliest download date (2017-01-01, or 2007-01-01 for oanda).
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
 * Slots in `[from, to)` with no bar in the store or at the venue, apart from those wholly
 * inside the market's closed hours (an FX weekend), which have no bar to miss.
 */
missing: Array<BarSpan>, };

/**
 * A run of bar slots: open times from `from` up to but not including `to`, `bars` of them.
 */
export type BarSpan = { from: number, to: number, bars: number, };

/**
 * A strategy the apps can run, with the parameters it takes, the plan the battery tests it on
 * and the rule its verdict is judged by.
 */
export type StrategyInfo = { name: string, description: string, params: Array<ParamSpec>, 
/**
 * The rule's kind, for the Lab's picker; the same as `rule.kind`.
 */
kind: RuleKind, plan: TestPlan, rule: VerdictRule, };

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
 * Set when `status` is `done`, and on a `cancelled` or `failed` plan job that kept some
 * instruments.
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

export type JobKind = "download" | "backtest" | "sweep" | "oos" | "walk_forward" | "plan";

export type JobStatus = "queued" | "running" | "done" | "failed" | "cancelled";

/**
 * What a job was asked to do, tagged by `kind`.
 */
export type JobRequest = { "kind": "download" } & DownloadRequest | { "kind": "backtest" } & BacktestConfig | { "kind": "sweep" } & SweepSpec | { "kind": "oos" } & OosSpec | { "kind": "walk_forward" } & WalkForwardSpec | { "kind": "plan" } & PlanRequest;

/**
 * What a finished job produced, tagged by `kind`.
 */
export type JobResult = { "kind": "download" } & DownloadResult | { "kind": "backtest" } & RunDone | { "kind": "sweep" } & RunDone | { "kind": "oos" } & RunDone | { "kind": "walk_forward" } & RunDone | { "kind": "plan" } & PlanDone;

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
 * `POST /runs/plan`: run a strategy's test plan.
 */
export type PlanRequest = { 
/**
 * A registered strategy's name.
 */
strategy: string, 
/**
 * Some of the plan's instruments, run in the plan's order; every one of them when left out.
 * Each must be one of the plan's.
 */
instruments?: Array<InstrumentId>, 
/**
 * Reference instruments every run of each battery names
 * ([`td_engine::BacktestConfig::references`]): the strategy reads their bars beside its own
 * but trades only the plan's instrument. Each must be on the venue of every instrument run,
 * and the store must hold their bars over the plan's period; they are not downloaded. None
 * when left out.
 */
references?: Array<InstrumentId>, };

/**
 * What a plan job kept: one entry per instrument whose battery finished or reached the luck test
 * before a cancel, in the plan's order. A failed plan job holds the instruments it saved before
 * the error.
 */
export type PlanDone = { strategy: string, instruments: Array<PlanInstrumentDone>, };

/**
 * One instrument's saved battery.
 */
export type PlanInstrumentDone = { 
/**
 * The saved run, at `GET /runs/{run_id}`, kind `plan`.
 */
run_id: RunId, 
/**
 * The instrument is `verdict.instrument`.
 */
verdict: Verdict, 
/**
 * The seed of the battery's luck test and bootstraps.
 */
seed: number, 
/**
 * Buckets in the period with no bar; the list is the saved run's `data.dropped`.
 */
dropped_buckets: number, };

/**
 * One row of `GET /verdicts`: the latest verdict of a strategy on an instrument, the newest
 * complete one when the pair has any.
 */
export type VerdictSummary = { strategy: string, instrument: InstrumentId, 
/**
 * The state and every criterion line; the runs behind it are at `GET /runs/{run_id}`.
 */
verdict: Verdict, 
/**
 * The newest complete battery of this strategy on this instrument, or the newest battery when
 * none is complete.
 */
run_id: RunId, created_at: number, 
/**
 * The engine revision that produced the verdict.
 */
engine_revision: number, 
/**
 * False when another engine revision produced it: this engine may judge the same data
 * differently, so run the plan again.
 */
engine_matches: boolean, 
/**
 * False when the strategy is no longer registered, its plan or rule has changed since the
 * battery ran, or the plan no longer tests the instrument: run the plan again.
 */
plan_matches: boolean, 
/**
 * Whether the strategy's source file is still the one the battery ran with: false when it has
 * been edited since, `null` when that is unknown (a battery saved before code hashes were
 * recorded, or a strategy no longer registered). The hash covers the strategy's own file only,
 * not shared helpers it calls, so a change to a shared helper leaves this true.
 */
code_matches: boolean | null, 
/**
 * True unless `engine_matches`, `plan_matches` and `code_matches` are all true: the verdict
 * may not be what this engine, plan, rule and code would give, so run the plan again.
 */
stale: boolean, 
/**
 * Every limit of the rule the verdict was judged under that is looser than its kind's default,
 * with both values; empty when none is, or when the saved rule no longer reads as a rule
 * (`plan_matches` is then false). Tightened limits are not listed.
 */
loosened: Array<LoosenedLimit>, 
/**
 * False when a cancel stopped the battery's luck test, so its luck line is inconclusive.
 */
complete: boolean, seed: number, 
/**
 * How many batteries have been saved for this strategy on this instrument, this one included.
 * An instrument a cancel stopped before the luck test saved none and is not counted. Keyed by
 * strategy name: renaming a strategy starts the count again.
 */
runs: number, 
/**
 * How many distinct versions of the strategy's source file have been through the battery on
 * this instrument, this one included; batteries saved before code hashes were recorded count
 * together as one version. Keyed by strategy name like `runs`.
 */
code_versions: number, 
/**
 * The reference instruments the battery's runs named; left out when none. A verdict with
 * references and one without share a row, the newer winning as above.
 */
references?: Array<InstrumentId>, };

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
check: RunCheck | null, 
/**
 * The hash of the strategy's source file when the run was made ([`td_engine::code_hash`]);
 * `null` for a run saved before hashes were recorded.
 */
code_hash: string | null, 
/**
 * Whether the strategy's source file is still the one the run was made with; `null` when that
 * is unknown: the run saved no hash, or the strategy is no longer registered. The hash covers
 * the strategy's own file only, not shared helpers it calls.
 */
code_matches: boolean | null, 
/**
 * The data cutoff date (`TD_DATA_CUTOFF`) in force when the run was made; `null` when there
 * was none, or for a run saved before cutoffs were recorded.
 */
data_cutoff: string | null, };

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
 * hours, apart from recorded closures. Together with any next to it that only closed hours
 * part it from, each holds less than a day of open time, or two days for FX in a month
 * downloaded before closures were recorded; more refuses the run.
 */
gaps: Array<GapInfo>, 
/**
 * The spans in the range (warm-up included), oldest first, that downloads recorded the venue
 * had nothing for and that count as closed, such as 25 December for FX: neither gaps nor
 * dropped buckets. Empty in a run saved before closures were reported.
 */
closures: Array<GapInfo>, 
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
 * The largest position allowed on a margined kind (perpetual, FX), as its notional over
 * equity, as in a backtest: 1.0 means 1x, the default, which a config saved without it also
 * gets. A kind without margin, such as spot, ignores it.
 */
max_leverage: number, };

/**
 * A funding rate fed with a bar.
 */
export type Funding = { 
/**
 * The time the engine was given: `due`, unless the rate came after the bar it is due in; then
 * the end of the last bar fed before this one, or this bar's open if it is the first.
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
fees?: FeeSchedule, 
/**
 * The saved verdict (a plan run's id) the session is started from: a verdict of the same
 * strategy on the same instrument. Left out, the strategy's latest verdict on the instrument
 * as `GET /verdicts` gives it, if it has one. Refused on a manual session, and for a verdict
 * judged with reference instruments, named or latest, since a session cannot feed them.
 */
verdict_run_id?: RunId, };

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
 * Close of the bar the order was decided on: the last price before it was sent, on the
 * order's side. For a market with an ask series that is the ask close for a buy and the bid
 * close for a sell.
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
 * The fee paid, in the account currency: the quote asset, or the base currency on a
 * base-currency account. Negative for a rebate.
 */
fee: number, liquidity: Liquidity, 
/**
 * True for a fill of the end-of-run close-out, the order a backtest submits to close what is
 * open when it is told to end flat (each walk-forward test window does): the strategy did
 * not choose it. False for every other fill, and then left out of JSON, so a fill saved
 * before the field existed reads as false and every other fill is written as it was.
 */
close_out?: boolean, };

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
 * close it would exit at (the ask close for a short on a market with quotes), with no exit
 * fee.
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
 * which is what the portfolio's average-cost realised P&L adds up to. On a base account it is
 * that realised P&L itself: each reducing fill's P&L against the average cost, converted at
 * that fill's price (and an open trade's remainder at the mark).
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
 * `net_pnl / (entry_price * qty)`: net P&L over the largest entry notional, the notional
 * converted to the account currency at `entry_price` on a base account.
 */
return_on_notional: number, 
/**
 * Bars from the entry fill's bar to the exit fill's bar; for an open trade, to the end of the
 * last bar. It counts bars fed, not time: missing bars in a gap are not counted, so across a
 * gap it understates how long the position was held. For a closed trade, the time between
 * the entry and exit fills' bars is `exit_time - entry_time`.
 */
bars_held: number, 
/**
 * The protective stop in force when the trade opened, if any: see [`attach_stops`]. `None`
 * without one, and in a result saved before trades kept their stop.
 */
stop: number | null, 
/**
 * `net_pnl` over the initial risk, `qty * |entry_price - stop|` (converted at the stop on a
 * base account): the trade's result in multiples of what it risked at its stop; see
 * [`attach_stops`]. `None` without a stop, with a stop at or beyond the entry price (no
 * risk), or when not finite.
 */
r_multiple: number | null, 
/**
 * Maximum adverse excursion: the largest unrealised loss of the open position at any price
 * it met, in the account currency (converted at that price on a base account), before fees
 * and funding; 0 if it never went against the entry. See [`build_trades`] for the prices
 * counted. `None` in a result saved before trades had it.
 */
mae: number | null, 
/**
 * Maximum favourable excursion: the largest unrealised gain, measured like `mae`.
 */
mfe: number | null, 
/**
 * Closed by the end-of-run close-out (see [`crate::BacktestConfig::close_at_end`]) rather
 * than by the strategy: in a walk-forward, at the end of its test window.
 */
force_closed: boolean, 
/**
 * The price move in pips, on a market with quotes (see [`td_core::Instrument::pip_size`]):
 * `sign * (exit_price - entry_price) / pip`. The spread is in it, since buys fill on the ask
 * and sells on the bid; commission and funding are not. An open trade's is on its mark, as its
 * P&L is. `None` without pips, and left out of saved JSON then.
 */
pips?: number | null, 
/**
 * `net_pnl` in pips: `net_pnl` over the value of one pip on the quantity the trade closed (its
 * whole size for one entry and one exit), in the account currency, converted at `exit_price`
 * on a base account. It is after commission. On a quote account, or when the trade exits at
 * one price, `pips - net_pips` is the commission in pips; on a base account with exits at
 * different prices it is not, since each exit's P&L is converted at its own price.
 */
net_pips?: number | null, };

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
warnings: Array<string>, 
/**
 * The strategy's code, plan and rule when the session started and the verdict it was started
 * from, with whether the strategy as it is now still matches that verdict or, without one,
 * what the session started with (`stale`). Left out for a manual session and for one started
 * before links were recorded.
 */
link?: LinkState, };

/**
 * What a session was started from: the strategy's code, plan and rule when it started, and the
 * verdict of the strategy on the session's instrument it was started from. Fixed when the session
 * starts and saved with its config; [`LinkState`] shows it with its checks.
 */
export type SessionLink = { 
/**
 * The hash of the strategy's source file when the session started
 * ([`td_engine::code_hash`]); `null` for a strategy without one.
 */
code_hash: string | null, 
/**
 * The strategy's test plan ([`td_engine::TestPlan`]) when the session started, as JSON, so a
 * session saved before a field of it changed still reads.
 */
plan: unknown, 
/**
 * The strategy's verdict rule ([`td_engine::VerdictRule`]) when the session started, as JSON
 * like `plan`.
 */
rule: unknown, 
/**
 * The verdict the session was started from: the one named when it was started, or else the
 * strategy's latest verdict on the instrument as `GET /verdicts` gives it. `null` when the
 * strategy had none there.
 */
verdict: LinkedVerdict | null, };

/**
 * The verdict a session was started from, with what its checks compare.
 */
export type LinkedVerdict = { 
/**
 * The saved plan run, at `GET /runs/{run_id}`.
 */
run_id: RunId, state: VerdictState, 
/**
 * False when a cancel stopped the battery's luck test.
 */
complete: boolean, 
/**
 * The engine revision that produced the verdict.
 */
engine_revision: number, 
/**
 * The hash of the strategy's source file the battery ran with; `null` for a battery saved
 * before code hashes were recorded.
 */
code_hash: string | null, 
/**
 * The plan the battery ran under, as its verdict record holds it.
 */
plan: unknown, 
/**
 * The rule the battery ran under, as its verdict record holds it.
 */
rule: unknown, };

/**
 * A session's [`SessionLink`] with its checks against the strategy as it is now, made when the
 * session's state is read. With a verdict, each check compares the verdict with this engine and
 * the strategy now, and is the one `GET /verdicts` makes for that verdict. Without one, the plan
 * and code checks compare what the session started with (the link's `plan`, `rule` and
 * `code_hash`) with the strategy now, and there is no engine check. A check that cannot be made is
 * `null`, and counts as not matching for `stale`.
 */
export type LinkState = { 
/**
 * With a verdict, whether this engine has the verdict's engine revision; `null` without one.
 */
engine_matches: boolean | null, 
/**
 * With a verdict, whether the strategy's plan and rule are still the ones the verdict was
 * judged under, as far as they concern the session's instrument, and the plan still tests
 * it; false when the strategy is no longer registered. Without one, whether they are still
 * the ones the session started with, as far as they concern the session's instrument; `null`
 * when the strategy is no longer registered.
 */
plan_matches: boolean | null, 
/**
 * With a verdict, whether the strategy's source file is still the one the verdict was judged
 * with; without one, whether it is still the one the session started with. `null` when that
 * is unknown: no hash was saved, or the strategy is no longer registered. The session replays
 * and trades with the current file, so this compares the code it runs now. The hash covers
 * the strategy's own file only, not shared helpers it calls.
 */
code_matches: boolean | null, 
/**
 * True unless each check made is true: all three with a verdict, `plan_matches` and
 * `code_matches` without one. With a verdict: the code, plan or engine the session runs on
 * may no longer be the one that verdict judged. Without one: the code or plan may have
 * changed since the session started.
 */
stale: boolean, 
/**
 * The hash of the strategy's source file when the session started
 * ([`td_engine::code_hash`]); `null` for a strategy without one.
 */
code_hash: string | null, 
/**
 * The strategy's test plan ([`td_engine::TestPlan`]) when the session started, as JSON, so a
 * session saved before a field of it changed still reads.
 */
plan: unknown, 
/**
 * The strategy's verdict rule ([`td_engine::VerdictRule`]) when the session started, as JSON
 * like `plan`.
 */
rule: unknown, 
/**
 * The verdict the session was started from: the one named when it was started, or else the
 * strategy's latest verdict on the instrument as `GET /verdicts` gives it. `null` when the
 * strategy had none there.
 */
verdict: LinkedVerdict | null, };

/**
 * `GET /sessions/{id}/comparison`: a session beside a backtest of the same strategy, parameters,
 * instrument, timeframe and money settings over the same bars.
 */
export type SessionComparisonResponse = { 
/**
 * The backtest that was run: from the session's first live bar (`from`) to the end of its
 * last bar (`to`), warmed up from its first warm-up bar (`warmup_from`).
 */
backtest: BacktestConfig, bars: ComparedBars, 
/**
 * Manual orders placed on the session. The backtest has none, so they, their fills and what
 * they changed show as differences.
 */
manual_orders: number, comparison: SessionComparison, };

/**
 * The bars the backtest ran on: the session's own, as its log holds them, warm-up bars included.
 * Nothing is downloaded and nothing is read from the store, so both sides saw exactly the same
 * prices.
 */
export type ComparedBars = { warm_up_bars: number, live_bars: number, 
/**
 * Buckets the session's live feed left out; the backtest lacks them too.
 */
dropped: Array<number>, 
/**
 * The funding rates the session was charged. The backtest is given each at the venue's
 * funding time (`due`), as a backtest from stored data would be, so a rate the session was
 * charged on a later bar, because it was not yet published when its own bar closed, lands
 * on another bar there; one due before the first live bar is not charged there at all. The
 * rates fed with warm-up bars, which the strategy saw but was not charged, are not counted
 * here; the backtest is given them too, so a strategy that reads funding sees them there.
 */
funding_events: number, 
/**
 * How many of those rates the session charged on a later bar than the one they were due in.
 * Each moves funding, and the equity after it, to another bar in the backtest, so it shows as
 * a funding and a result difference and, for a strategy that sizes its orders on equity, as
 * order and fill quantity differences: none of these is then a difference in how orders were
 * filled.
 */
late_funding: number, };

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
export type ServerMessage = { "type": "subscribed", topic: string, } | { "type": "unsubscribed", topic: string, } | { "type": "job", job: JobInfo, } | { "type": "candle", topic: string, candle: Candle, } | { "type": "candle_forming", topic: string, candle: Candle, } | { "type": "session_state", session: SessionState, } | { "type": "session", id: string, event: SessionEvent, } | { "type": "error", error: ErrorBody, };
