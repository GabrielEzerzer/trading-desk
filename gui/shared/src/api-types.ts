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
 * for a perpetual its funding rates in the same range.
 */
export type DownloadRequest = { instrument: InstrumentId, from: number, to: number, };

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
 * A message from the client.
 */
export type ClientMessage = { "type": "subscribe", topic: string, } | { "type": "unsubscribe", topic: string, };

/**
 * A message from the server.
 */
export type ServerMessage = { "type": "subscribed", topic: string, } | { "type": "unsubscribed", topic: string, } | { "type": "job", job: JobInfo, } | { "type": "error", error: ErrorBody, };
