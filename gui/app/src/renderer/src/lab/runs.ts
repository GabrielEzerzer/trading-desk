// How the engine's runs read in the Lab: a saved backtest as the result screen, the run list as each
// strategy's latest run, a stored dataset as a run's range, and a failed call as the state to show.

import { ApiError } from '@td/shared'
import type {
  CoverageResponse,
  InstrumentId,
  MetricsReport,
  RunDetail,
  RunSummary,
  StoredResponse,
  Timeframe,
} from '@td/shared'
import { TIMEFRAME_MS } from '../trading/candles'
import type { EquityPoint, FinishedRun, LastRun, Millis, VerdictPreview } from './source'
import { RunCancelled } from './source'

const MINUTE = 60_000
const DAY = 24 * 60 * MINUTE
/** Weekly bars open on Mondays, counted from Monday 1970-01-05 (td_core's `FIRST_MONDAY`). */
const FIRST_MONDAY = 4 * DAY

/** The timeframes the overrides menu offers, the default first. */
export const TIMEFRAMES = ['1h', '4h', '1d'] as const satisfies readonly Timeframe[]

/** The equity chart draws at most this many points; a long run on short bars has far more. */
const MAX_CHART_POINTS = 1000

/** `binance:BTCUSDT:spot`, the form the engine writes an instrument in. */
export function instrumentKey(id: InstrumentId): string {
  return `${id.venue}:${id.symbol}:${id.kind}`
}

/** The instruments with candles stored, `first` first when it is one of them, then in the store's order. */
export function storedDatasets(stored: StoredResponse, first: InstrumentId): InstrumentId[] {
  const ids = stored.instruments.filter((i) => i.months.length > 0).map((i) => i.instrument)
  const key = instrumentKey(first)
  return [...ids.filter((id) => instrumentKey(id) === key), ...ids.filter((id) => instrumentKey(id) !== key)]
}

/** When a run was saved: its id starts with the save time in milliseconds (td_engine::RunId). */
function runSavedAt(id: string): Millis {
  return Number(id.slice(0, 13))
}

/**
 * A strategy's last run once `finished` has finished: `finished`, unless the last run is newer. A
 * run cancelled while saving still finishes, and can finish after a later run did.
 */
export function laterRun(last: LastRun | null, finished: LastRun): LastRun {
  return last && last.finishedAt > finished.finishedAt ? last : finished
}

/**
 * Each strategy's latest saved backtest.
 *
 * shortcut: sweeps, out-of-sample and walk-forward runs are left out, because the Lab runs one
 * backtest on defaults until #32 gives each strategy its plan.
 */
export function latestRuns(runs: RunSummary[]): Map<string, LastRun> {
  const latest = new Map<string, RunSummary>()
  for (const run of runs) {
    if (run.kind !== 'backtest') continue
    const seen = latest.get(run.strategy)
    if (!seen || run.created_at > seen.created_at) latest.set(run.strategy, run)
  }
  return new Map(
    [...latest].map(([name, run]) => [
      name,
      { id: run.id, finishedAt: run.created_at, totalReturn: run.total_return, decision: null },
    ]),
  )
}

/** Midnight UTC at the end of a data cutoff date (`YYYY-MM-DD`): the first moment the engine refuses. */
export function cutoffEnd(date: string): Millis {
  return Date.parse(`${date}T00:00:00Z`) + DAY
}

/**
 * The whole stored range of a dataset at a timeframe: from the first bar that opens at or after the
 * first stored minute to the end of the last whole bar, ending no later than `until` (the end of a
 * data cutoff). Null when nothing is stored or the range holds no whole bar.
 */
export function datasetRange(
  coverage: CoverageResponse,
  timeframe: Timeframe,
  until?: Millis,
): { from: Millis; to: Millis } | null {
  const step = TIMEFRAME_MS[timeframe]
  const first = coverage.months[0]?.first
  const last = coverage.months.at(-1)?.last
  if (first === undefined || last === undefined) return null
  // Bars are aligned to the epoch, weeks to the first Monday (td_core::Timeframe::floor).
  const origin = timeframe === '1w' ? FIRST_MONDAY : 0
  const from = origin + Math.ceil((first - origin) / step) * step
  const end = Math.min(last + MINUTE, until ?? Number.POSITIVE_INFINITY)
  const to = origin + Math.floor((end - origin) / step) * step
  return to > from ? { from, to } : null
}

/** Every `k`-th point and the last, so the chart stays light whatever the run's length. */
function thin<T>(points: T[]): T[] {
  const k = Math.ceil(points.length / MAX_CHART_POINTS)
  if (k <= 1) return points
  return points.filter((_, i) => i % k === 0 || i === points.length - 1)
}

const metricRows = (s: MetricsReport, b: MetricsReport) => [
  { label: 'Total return', strategy: s.total_return, benchmark: b.total_return, format: 'percent' as const },
  { label: 'CAGR', strategy: s.cagr, benchmark: b.cagr, format: 'percent' as const },
  { label: 'Volatility', strategy: s.volatility, benchmark: b.volatility, format: 'percent' as const },
  { label: 'Sharpe', strategy: s.sharpe, benchmark: b.sharpe, format: 'ratio' as const },
  { label: 'Sortino', strategy: s.sortino, benchmark: b.sortino, format: 'ratio' as const },
  { label: 'Max drawdown', strategy: s.max_drawdown, benchmark: b.max_drawdown, format: 'percent' as const },
  { label: 'Win rate', strategy: s.win_rate, benchmark: null, format: 'percent' as const },
  { label: 'Profit factor', strategy: s.profit_factor, benchmark: null, format: 'ratio' as const },
  { label: 'Trades', strategy: s.trades, benchmark: null, format: 'count' as const },
]

/**
 * A saved backtest as the result screen shows it. There is no verdict rule until #32, so it has no
 * decision and no criteria. Throws for the other run kinds, which the Lab does not show yet.
 */
export function toVerdict(detail: RunDetail): VerdictPreview {
  if (detail.run.kind !== 'backtest') {
    throw new Error(`Run ${detail.id} is a ${detail.run.kind.replace('_', '-')} run; the Lab shows backtests only.`)
  }
  const { config, equity, benchmark, metrics, trades } = detail.run.result
  // Growth of one unit of starting cash, so the curve's last point is the total return.
  const start = metrics.start_equity
  const benchStart = benchmark.metrics.start_equity
  const bench = new Map(benchmark.equity.map((p) => [p.time, p.equity]))
  const points: EquityPoint[] = equity.map((p) => {
    const b = bench.get(p.time)
    return { t: p.time, strategy: p.equity / start, benchmark: b !== undefined && benchStart > 0 ? b / benchStart : null }
  })
  return {
    decision: null,
    instrument: config.instrument.id,
    // The engine's `to` is exclusive; the period names the last bar's month.
    period: { from: config.from, to: config.to - 1 },
    finishedAt: runSavedAt(detail.id),
    criteria: [],
    keyNumbers: [
      { label: 'Return', value: metrics.total_return, format: 'percent', tone: 'pnl' },
      { label: 'CAGR', value: metrics.cagr, format: 'percent', tone: 'pnl' },
      { label: 'Sharpe', value: metrics.sharpe, format: 'ratio' },
      { label: 'Max drawdown', value: metrics.max_drawdown, format: 'percent' },
    ],
    benchmarkLabel: 'Buy and hold',
    equity: thin(points),
    metrics: metricRows(metrics, benchmark.metrics),
    trades: trades.map((t, i) => ({
      id: i + 1,
      side: t.side,
      entryTime: t.entry_time,
      exitTime: t.exit_time,
      entryPrice: t.entry_price,
      exitPrice: t.exit_price,
      returnPct: t.return_on_notional * 100,
      open: t.open,
    })),
    // An old run that saved no code hash is unknown, and says nothing.
    codeChanged: detail.code_matches === false,
    dataCutoff: detail.data_cutoff ?? null,
  }
}

/** A run job's saved backtest, as the strategy list's row and the result screen. */
export function toFinished(detail: RunDetail): FinishedRun {
  const verdict = toVerdict(detail)
  const totalReturn = detail.run.kind === 'backtest' ? detail.run.result.metrics.total_return : null
  return { last: { id: detail.id, finishedAt: verdict.finishedAt, totalReturn, decision: null }, verdict }
}

/** What a run that did not finish shows. */
export type RunFailure =
  | { kind: 'cancelled' }
  /** The store lacks the run's range: the "Data missing" state. */
  | { kind: 'data_missing'; message: string }
  /** The engine holds as many queued and running runs as it takes; the message says how many. */
  | { kind: 'too_many_runs'; message: string }
  | { kind: 'failed'; message: string }

export function runFailure(e: unknown): RunFailure {
  if (e instanceof RunCancelled) return { kind: 'cancelled' }
  if (e instanceof ApiError && e.code === 'data_missing') return { kind: 'data_missing', message: e.message }
  if (e instanceof ApiError && e.code === 'too_many_runs') return { kind: 'too_many_runs', message: e.message }
  return { kind: 'failed', message: e instanceof Error ? e.message : String(e) }
}
