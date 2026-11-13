// The Lab's mock source, used when the app runs with TD_MOCK=1 and by the headless screenshots'
// mock states. It reads lab.json (made by generate.mjs): two strategies with results, one that never
// ran and whose run finishes, and one whose run fails. The engine states drive the engine source
// instead, through the bridge stub in app/shots/ and engine.json.

import type { InstrumentId } from '@td/shared'
import {
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

export function createMockLabSource({ stepMs = 700, listMs = 300, now = Date.now }: MockOptions = {}): LabSource {
  return {
    defaultInstrument: data.datasets[0] as InstrumentId,
    neverRunText: 'Run its test plan to get a verdict.',
    dataCutoff: async () => null,
    strategies: () =>
      new Promise((resolve) => {
        setTimeout(() => resolve(data.strategies.map((s) => ({ ...s, label: readableName(s.name) }))), listMs)
      }),
    datasets: async () => data.datasets,
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
