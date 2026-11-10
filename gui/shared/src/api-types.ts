// Generated from the Rust API types by `cargo run -p td-server --bin gen-api-types`.
// Do not edit by hand; CI fails when this file is out of date.

/**
 * An exchange the desk reads data from and simulates orders against.
 */
export type Venue = "binance" | "bybit";

export type InstrumentKind = "spot" | "perpetual";

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
 * A protective stop attached to an entry with [`crate::Context::submit_with_stop`]. When the
 * entry fills, the broker places a reduce-only stop on the other side, for the filled quantity,
 * under `id`.
 */
export type AttachedStop = { id: OrderId, trigger: number, };

export type OrderStatus = { "status": "attached" } | { "status": "open" } | { "status": "filled" } | { "status": "cancelled", "reason": string } | { "status": "rejected", "reason": string };

/**
 * An order the session accepted or rejected, with its request as rounded at submission.
 */
export type Order = { id: OrderId, request: OrderRequest, 
/**
 * Open time of the bar the order was decided on. It can first fill on a later bar.
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
 * Equity at the close of the bar that opened at `time`.
 */
export type EquityPoint = { time: number, equity: number, };

export type TradeSide = "long" | "short";

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
gaps: Array<GapInfo>, };

export type FundingCoverage = { rates: number, first: number | null, last: number | null, };

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

export type JobKind = "download";

export type JobStatus = "queued" | "running" | "done" | "failed" | "cancelled";

/**
 * What a job was asked to do, tagged by `kind`.
 */
export type JobRequest = { "kind": "download" } & DownloadRequest;

/**
 * What a finished job produced, tagged by `kind`.
 */
export type JobResult = { "kind": "download" } & DownloadResult;

/**
 * What a paper session runs: a registered strategy with its parameters, on one instrument and
 * timeframe, with the same money settings as a backtest.
 */
export type PaperConfig = { strategy: string, params: Params, 
/**
 * The instrument with the trading rules it had when the session was made. Fixed for the
 * session's life, so a resume replays against the same rules.
 */
instrument: Instrument, timeframe: Timeframe, starting_cash: number, slippage_bps: number, 
/**
 * Replaces the instrument's fee schedule when set.
 */
fees: FeeSchedule | null, };

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
 * A registered strategy, as `GET /strategies` lists it.
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
equity_curve: Array<EquityPoint>, open_orders: Array<Order>, fills: Array<Fill>, 
/**
 * Round trips so far; one still open is marked at the last close.
 */
trades: Array<Trade>, 
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
 * sees each change exactly once. Replaying the log reports nothing.
 */
export type SessionEvent = { "event": "funding", funding: Funding, } | { "event": "fill", fill: Fill, } | { "event": "order", order: Order, } | { "event": "bar", bar: Candle, warm_up: boolean, equity: number | null, } | { "event": "status", status: Status, };

/**
 * A message from the client.
 */
export type ClientMessage = { "type": "subscribe", topic: string, } | { "type": "unsubscribe", topic: string, };

/**
 * A message from the server.
 */
export type ServerMessage = { "type": "subscribed", topic: string, } | { "type": "unsubscribed", topic: string, } | { "type": "job", job: JobInfo, } | { "type": "candle", topic: string, candle: Candle, } | { "type": "session_state", session: SessionState, } | { "type": "session", id: string, event: SessionEvent, } | { "type": "error", error: ErrorBody, };
