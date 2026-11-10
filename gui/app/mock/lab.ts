// The Lab's mock source, used when the app runs with TD_MOCK=1 and by the headless screenshots.
// It reads lab.json (made by generate.mjs): two strategies with results, one that never ran and
// whose run finishes, and one whose run fails.

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

export function createMockLabSource({ stepMs = 700, listMs = 300, now = Date.now }: MockOptions = {}): LabSource {
  return {
    canRun: true,
    strategies: () =>
      new Promise((resolve) => {
        setTimeout(() => resolve(data.strategies.map((s) => ({ ...s, label: readableName(s.name) }))), listMs)
      }),
    run(name, onProgress) {
      const strategy = data.strategies.find((s) => s.name === name)
      // A strategy with a result and no script runs again quickly and lands on the same result.
      const script: RunScript | undefined =
        data.runs[name] ??
        (strategy?.last
          ? {
              steps: [
                { step: 1, steps: 3, stage: 'Loading candles' },
                { step: 2, steps: 3, stage: 'Walk-forward windows', done: 3, total: 5 },
                { step: 3, steps: 3, stage: 'Writing results' },
              ],
              result: strategy.last,
            }
          : undefined)
      let timer: ReturnType<typeof setTimeout> | undefined
      let reject: (reason: Error) => void = () => {}
      const result = new Promise<VerdictPreview>((resolve, rejectRun) => {
        reject = rejectRun
        if (!script) {
          rejectRun(new Error(`No strategy named ${name}.`))
          return
        }
        let i = 0
        const next = () => {
          const step = script.steps[i]
          if (step) {
            onProgress(step)
            i += 1
            timer = setTimeout(next, stepMs)
            return
          }
          if (script.result) resolve({ ...script.result, finishedAt: now() })
          else rejectRun(new Error(script.error ?? 'The run failed.'))
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
    },
  }
}

export const mockLabSource = createMockLabSource()
