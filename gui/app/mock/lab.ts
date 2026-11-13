// The Lab's mock source, used when the app runs with TD_MOCK=1 and by the headless screenshots'
// mock states. It reads lab.json (made by generate.mjs): two strategies with results, one that never
// ran and whose run finishes, and one whose run fails. Its stored data is the fixtures' datasets,
// each with a run of months up to Oct 2025 and made-up file sizes, and deletes change it in memory.
// The engine states drive the engine source instead, through the bridge stub in app/shots/ and
// engine.json.

import type { DeleteResult, InstrumentId, StoredInstrumentInfo, StoredResponse } from '@td/shared'
import {
  type DeleteTarget,
  type LabSource,
  type LabStrategy,
  type RunProgress,
  type VerdictPreview,
  RunCancelled,
  readableName,
} from '../src/renderer/src/lab/source'
import fixtures from './lab.json'

interface RunScript {
  steps: RunProgress[]
  result?: VerdictPreview
  error?: string
}

interface Fixtures {
  strategies: Omit<LabStrategy, 'label'>[]
  results: Record<string, VerdictPreview>
  datasets: InstrumentId[]
  runs: Record<string, RunScript>
}

const data = fixtures as unknown as Fixtures

export interface MockOptions {
  /** How long each progress step takes. */
  stepMs?: number
  /** How long the strategy list takes to arrive, like a call to the engine. */
  listMs?: number
  now?: () => number
}

/** Plays progress reports one step apart, then settles with `finish`; `cancel` stops it. */
function play<T>(steps: (() => void)[], stepMs: number, finish: () => T | Error) {
  let timer: ReturnType<typeof setTimeout> | undefined
  let reject: (reason: Error) => void = () => {}
  const result = new Promise<T>((resolve, rejectJob) => {
    reject = rejectJob
    let i = 0
    const next = () => {
      const step = steps[i]
      if (step) {
        step()
        i += 1
        timer = setTimeout(next, stepMs)
        return
      }
      const outcome = finish()
      if (outcome instanceof Error) rejectJob(outcome)
      else resolve(outcome)
    }
    timer = setTimeout(next, 0)
  })
  return {
    result,
    cancel() {
      clearTimeout(timer)
      reject(new RunCancelled())
    },
  }
}

/** Months held by each dataset, in the fixtures' order, all ending Oct 2025. */
const MONTHS_HELD = [46, 29, 20]

/** The fixtures' datasets as the store would list them, with sizes near a real month file's. */
function mockStored(): StoredResponse {
  const instruments = data.datasets.map((instrument, i): StoredInstrumentInfo => {
    const count = MONTHS_HELD[i % MONTHS_HELD.length] ?? 12
    const months = Array.from({ length: count }, (_, k) => {
      const index = 2025 * 12 + 9 - (count - 1 - k)
      const month = `${Math.floor(index / 12)}-${String((index % 12) + 1).padStart(2, '0')}`
      return { month, bytes: 1_650_000 + ((k * 7919 + i * 104_729) % 380_000) }
    })
    const funding_bytes = instrument.kind === 'perpetual' ? 18_000 + count * 2_400 : null
    const bytes = months.reduce((sum, m) => sum + m.bytes, 0) + (funding_bytes ?? 0)
    return { instrument, months, funding_bytes, bytes }
  })
  return { data_dir: String.raw`C:\Users\trader\AppData\Local\trading-desk`, instruments, unrecognised: [] }
}

/** Deletes `target` from `stored` as the engine's store does, and says what went. */
function deleteFrom(stored: StoredResponse, target: DeleteTarget): DeleteResult {
  const removed: DeleteResult = { files: 0, bytes: 0 }
  const take = (bytes: number) => {
    removed.files += 1
    removed.bytes += bytes
  }
  const key = (id: InstrumentId) => `${id.venue}:${id.symbol}:${id.kind}`
  stored.instruments = stored.instruments.flatMap((info) => {
    if (target !== 'all' && key(info.instrument) !== key(target.instrument)) return [info]
    const range = target !== 'all' && target.from && target.to ? { from: target.from, to: target.to } : null
    const months = info.months.filter((m) => {
      const goes = !range || (m.month >= range.from && m.month <= range.to)
      if (goes) take(m.bytes)
      return !goes
    })
    // A range leaves the funding file, as the engine's delete of months does.
    let funding_bytes = info.funding_bytes
    if (!range && funding_bytes !== null) {
      take(funding_bytes)
      funding_bytes = null
    }
    if (months.length === 0 && funding_bytes === null) return []
    const bytes = months.reduce((sum, m) => sum + m.bytes, 0) + (funding_bytes ?? 0)
    return [{ ...info, months, funding_bytes, bytes }]
  })
  return removed
}

export function createMockLabSource({ stepMs = 700, listMs = 300, now = Date.now }: MockOptions = {}): LabSource {
  const stored = mockStored()
  return {
    defaultInstrument: data.datasets[0] as InstrumentId,
    neverRunText: 'Run its test plan to get a verdict.',
    dataCutoff: async () => null,
    strategies: () =>
      new Promise((resolve) => {
        setTimeout(() => resolve(data.strategies.map((s) => ({ ...s, label: readableName(s.name) }))), listMs)
      }),
    stored: async () => structuredClone(stored),
    deleteData: async (target) => deleteFrom(stored, target),
    openDataFolder: async () => {},
    async open(last) {
      const result = data.results[last.id]
      if (!result) throw new Error(`No saved run ${last.id}.`)
      return result
    },
    run(name, _target, onProgress) {
      const strategy = data.strategies.find((s) => s.name === name)
      const saved = strategy?.last ? data.results[strategy.last.id] : undefined
      // A strategy with a result and no script runs again quickly and lands on the same result.
      const script: RunScript | undefined =
        data.runs[name] ??
        (saved
          ? {
              steps: [
                { step: 1, steps: 3, stage: 'Loading candles' },
                { step: 2, steps: 3, stage: 'Walk-forward windows', done: 3, total: 5 },
                { step: 3, steps: 3, stage: 'Writing results' },
              ],
              result: saved,
            }
          : undefined)
      if (!script) return { result: Promise.reject(new Error(`No strategy named ${name}.`)), cancel() {} }
      return play(
        script.steps.map((step) => () => onProgress(step)),
        stepMs,
        () => {
          if (!script.result) return new Error(script.error ?? 'The run failed.')
          const finishedAt = now()
          const verdict = { ...script.result, finishedAt }
          const totalReturn = verdict.keyNumbers[0]?.value ?? null
          return { last: { id: `${name}-${finishedAt}`, finishedAt, totalReturn, decision: verdict.decision }, verdict }
        },
      )
    },
    download(_instrument, onProgress) {
      return play(
        [0.25, 0.5, 0.75, 1].map((fraction) => () => onProgress({ fraction })),
        stepMs,
        () => undefined,
      )
    },
  }
}

export const mockLabSource = createMockLabSource()
