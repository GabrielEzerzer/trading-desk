// Where the Lab's data comes from. The Lab only talks to a `LabSource`: the engine one reads the
// real strategy list, and the mock one (app/mock/lab.ts, chosen with TD_MOCK=1) reads fixtures.

import type { InstrumentId } from '@td/shared'
import { api } from '../api'

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
  value: number
  format: NumberFormat
  /** `pnl` numbers take the gain or loss colour by sign. */
  tone?: 'pnl'
}

/** Growth of one unit of capital. */
export interface EquityPoint {
  t: Millis
  strategy: number
  benchmark: number
}

export interface MetricRow {
  label: string
  strategy: number
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
  decision: 'trade' | 'no_trade'
  instrument: InstrumentId
  period: { from: Millis; to: Millis }
  finishedAt: Millis
  criteria: Criterion[]
  keyNumbers: KeyNumber[]
  benchmarkLabel: string
  equity: EquityPoint[]
  metrics: MetricRow[]
  trades: TradeRow[]
}

export interface LabStrategy {
  /** The engine's id, for example `sma_cross`. */
  name: string
  /** What people read, for example "SMA cross". */
  label: string
  /** For example "Trend following"; null while the engine does not say. */
  kind: string | null
  description: string
  /** The last finished run's verdict, or null when it never ran. */
  last: VerdictPreview | null
}

export interface RunProgress {
  /** From 0 to 1. */
  progress: number
  message: string
}

export interface LabRun {
  /** Resolves with the verdict, rejects with the reason it failed, or with `RunCancelled`. */
  result: Promise<VerdictPreview>
  cancel(): void
}

export class RunCancelled extends Error {
  override name = 'RunCancelled'
  constructor() {
    super('the run was cancelled')
  }
}

export interface LabSource {
  /** False while the source cannot run test plans; the Run button says why. */
  readonly canRun: boolean
  readonly cannotRunReason?: string
  strategies(): Promise<LabStrategy[]>
  run(name: string, onProgress: (progress: RunProgress) => void): LabRun
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

/** The real engine: its strategy list, with no results until the engine runs test plans (#32). */
export const engineLabSource: LabSource = {
  canRun: false,
  cannotRunReason: 'The engine cannot run test plans yet.',
  async strategies() {
    const list = await api.strategies()
    // shortcut: the engine reports no kind or past verdicts yet; read them from it with #32.
    return list.map((s) => ({ name: s.name, label: readableName(s.name), kind: null, description: s.description, last: null }))
  },
  run() {
    return { result: Promise.reject(new Error('The engine cannot run test plans yet.')), cancel() {} }
  },
}

/** The engine, or the fixtures in app/mock/ when the app was started with TD_MOCK=1. */
export async function chooseLabSource(): Promise<LabSource> {
  if (!window.td.mock) return engineLabSource
  const { mockLabSource } = await import('../../../../mock/lab')
  return mockLabSource
}
