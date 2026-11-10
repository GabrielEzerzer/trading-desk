// Where the Lab's data comes from. The Lab only talks to a `LabSource`: the engine one
// (engine-source.ts) runs backtests on the engine, and the mock one (app/mock/lab.ts, chosen with
// TD_MOCK=1) reads fixtures.

import type { InstrumentId, Timeframe } from '@td/shared'

/** Milliseconds since the Unix epoch, UTC. */
export type Millis = number

export type NumberFormat = 'percent' | 'ratio' | 'count'

export interface Criterion {
  label: string
  value: number
  limit: number
  rule: 'at_least' | 'at_most'
  format: NumberFormat
  pass: boolean
}

export interface KeyNumber {
  label: string
  /** Null when the engine cannot compute it, for example a Sharpe ratio over a short run. */
  value: number | null
  format: NumberFormat
  /** `pnl` numbers take the gain or loss colour by sign. */
  tone?: 'pnl'
}

/** Growth of one unit of capital. */
export interface EquityPoint {
  t: Millis
  strategy: number
  /** Null when the run carries no benchmark curve at this time. */
  benchmark: number | null
}

export interface MetricRow {
  label: string
  strategy: number | null
  benchmark: number | null
  format: NumberFormat
}

export interface TradeRow {
  id: number
  side: 'long' | 'short'
  entryTime: Millis
  exitTime: Millis
  entryPrice: number
  exitPrice: number
  returnPct: number
}

// shortcut: replace with the engine's verdict type from #32
export interface VerdictPreview {
  /** Null while the strategy has no verdict rule (#32). */
  decision: 'trade' | 'no_trade' | null
  instrument: InstrumentId
  period: { from: Millis; to: Millis }
  finishedAt: Millis
  /** Empty while the strategy has no verdict rule. */
  criteria: Criterion[]
  keyNumbers: KeyNumber[]
  benchmarkLabel: string
  equity: EquityPoint[]
  metrics: MetricRow[]
  trades: TradeRow[]
}

/** A strategy's latest finished run, as the strategy list shows it. */
export interface LastRun {
  /** Where the full result is, for `LabSource.open`. */
  id: string
  finishedAt: Millis
  totalReturn: number | null
  decision: 'trade' | 'no_trade' | null
}

export interface LabStrategy {
  /** The engine's id, for example `sma_cross`. */
  name: string
  /** What people read, for example "SMA cross". */
  label: string
  /** For example "Trend following"; null while the engine does not say. */
  kind: string | null
  description: string
  /** The latest finished run, or null when it never ran. */
  last: LastRun | null
}

/** What a run runs on: the overrides menu's instrument and timeframe. */
export interface RunTarget {
  instrument: InstrumentId
  timeframe: Timeframe
}

/** Where a run is in its test plan: the step, and how far that step has counted when it counts. */
export interface RunProgress {
  /** The current step, from 1 to `steps`. */
  step: number
  steps: number
  /** What the step does, for example "Sweeping parameters". */
  stage: string
  /** Items this step has finished, and how many it has, when the step counts items. */
  done?: number
  total?: number
  /** How far the step has run, from 0 to 1, when it reports a share instead of a count. */
  fraction?: number
}

/** How far a download has run. */
export interface DownloadProgress {
  /** From 0 to 1. */
  fraction: number
}

/** Work the engine does in the background: it resolves, rejects with why, or with `RunCancelled`. */
export interface LabJob<T> {
  result: Promise<T>
  cancel(): void
}

/** A run that finished: its row for the strategy list, and its result. */
export interface FinishedRun {
  last: LastRun
  verdict: VerdictPreview
}

export type LabRun = LabJob<FinishedRun>

export class RunCancelled extends Error {
  override name = 'RunCancelled'
  constructor() {
    super('the run was cancelled')
  }
}

export interface LabSource {
  strategies(): Promise<LabStrategy[]>
  /** The instruments with candles stored, the default first. */
  datasets(): Promise<InstrumentId[]>
  /** The instrument to offer for download when nothing is stored. */
  readonly defaultInstrument: InstrumentId
  /** The full result of a strategy's latest run. */
  open(last: LastRun): Promise<VerdictPreview>
  run(name: string, target: RunTarget, onProgress: (progress: RunProgress) => void): LabRun
  /** Downloads an instrument's history into the store. */
  download(instrument: InstrumentId, onProgress: (progress: DownloadProgress) => void): LabJob<void>
}

const ACRONYMS = new Set(['sma', 'ema', 'rsi', 'atr', 'macd', 'vwap'])

// shortcut: names are made readable from the engine's ids; take the label from the engine once it
// sends one (#32).
export function readableName(id: string): string {
  return id
    .split(/[_\s]+/)
    .filter(Boolean)
    .map((word, i) => {
      if (ACRONYMS.has(word.toLowerCase())) return word.toUpperCase()
      return i === 0 ? word.charAt(0).toUpperCase() + word.slice(1) : word.toLowerCase()
    })
    .join(' ')
}

/** The engine, or the fixtures in app/mock/ when the app was started with TD_MOCK=1. */
export async function chooseLabSource(): Promise<LabSource> {
  if (!window.td.mock) {
    const { engineLabSource } = await import('./engine-source')
    return engineLabSource
  }
  const { mockLabSource } = await import('../../../../mock/lab')
  return mockLabSource
}
