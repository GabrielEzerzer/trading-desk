// The Lab on the real engine: the strategy list with each strategy's latest saved backtest, the
// instruments with stored candles, backtest and download jobs followed on the `jobs` topic, and
// saved runs read back for the result screen.

import { ApiError } from '@td/shared'
import type { BacktestConfig, InstrumentId, JobInfo, Params, StrategyInfo } from '@td/shared'
import { api } from '../api'
import { datasetRange, instrumentKey, latestRuns, toFinished, toVerdict } from './runs'
import { type LabJob, type LabSource, RunCancelled, readableName } from './source'

// shortcut: one backtest on defaults until #32 gives each strategy its plan. The money settings
// are fixed here until then.
const STARTING_CASH = 10_000
const SLIPPAGE_BPS = 2

/** The earliest `from` a download accepts: 2017-01-01 UTC, before either venue has data. */
const HISTORY_FROM = 1_483_228_800_000

// shortcut: the engine has no route that lists what the store holds, so the Lab asks for the
// coverage of these instruments and of every instrument a saved run used. Ask the engine for its
// stored instruments once it has a route for them.
const CANDIDATES: InstrumentId[] = ['BTCUSDT', 'ETHUSDT'].flatMap((symbol) =>
  (['binance', 'bybit'] as const).flatMap((venue) =>
    (['spot', 'perpetual'] as const).map((kind) => ({ venue, symbol, kind })),
  ),
)

const defaults = (info: StrategyInfo | undefined): Params =>
  Object.fromEntries((info?.params ?? []).map((p) => [p.name, p.default]))

/**
 * Starts a job and follows it on the `jobs` topic until it ends: resolves with the finished job,
 * rejects with the job's error, or with `RunCancelled` once `cancel` is called.
 */
function followJob(start: () => Promise<number>, onUpdate: (job: JobInfo) => void): LabJob<JobInfo> {
  let jobId: number | null = null
  let cancelled = false
  let off = () => {}
  let reject: (reason: unknown) => void = () => {}
  // Messages can arrive before `start` returns the id, so the latest state of each job is kept.
  const seen = new Map<number, JobInfo>()

  const result = new Promise<JobInfo>((resolve, rejectJob) => {
    reject = (reason) => {
      off()
      rejectJob(reason)
    }
    const take = (job: JobInfo) => {
      if (cancelled) return
      if (job.status === 'done') {
        off()
        resolve(job)
      } else if (job.status === 'failed') {
        reject(new Error(job.error?.message ?? 'The job failed.'))
      } else if (job.status === 'cancelled') {
        reject(new RunCancelled())
      } else {
        onUpdate(job)
      }
    }
    off = window.td.onMessage((message) => {
      if (message.type !== 'job') return
      if (jobId === null) seen.set(message.job.id, message.job)
      else if (message.job.id === jobId) take(message.job)
    })
    // Subscribing first means the engine's snapshot after `subscribed` includes the new job.
    // The topic stays subscribed: other work may be following jobs too, and main sends it once.
    window.td
      .subscribe('jobs')
      .then(start)
      .then(
        (id) => {
          jobId = id
          if (cancelled) {
            void api.cancelJob(id).catch(() => {})
            return
          }
          const early = seen.get(id)
          seen.clear()
          if (early) take(early)
        },
        (e: unknown) => reject(e),
      )
  })

  return {
    result,
    cancel() {
      if (cancelled) return
      cancelled = true
      // A job that has already finished answers 409; there is nothing left to stop.
      if (jobId !== null) void api.cancelJob(jobId).catch(() => {})
      reject(new RunCancelled())
    },
  }
}

const strategyInfo = new Map<string, StrategyInfo>()

export const engineLabSource: LabSource = {
  defaultInstrument: CANDIDATES[0] as InstrumentId,

  async strategies() {
    const [list, runs] = await Promise.all([api.strategies(), api.runs()])
    const latest = latestRuns(runs)
    for (const s of list) strategyInfo.set(s.name, s)
    // shortcut: the engine reports no kind yet; read it from the engine with #32.
    return list.map((s) => ({
      name: s.name,
      label: readableName(s.name),
      kind: null,
      description: s.description,
      last: latest.get(s.name) ?? null,
    }))
  },

  async datasets() {
    const runs = await api.runs()
    const candidates = new Map(CANDIDATES.map((id) => [instrumentKey(id), id]))
    for (const run of runs) candidates.set(instrumentKey(run.instrument), run.instrument)
    const covered = await Promise.all(
      [...candidates.values()].map(async (id) => ((await api.coverage(id)).months.length > 0 ? id : null)),
    )
    return covered.filter((id) => id !== null)
  },

  async open(last) {
    return toVerdict(await api.run(last.id))
  },

  run(name, target, onProgress) {
    const { instrument, timeframe } = target
    const job = followJob(
      async () => {
        const [coverage, listed] = await Promise.all([
          api.coverage(instrument),
          api.instruments(instrument.venue, instrument.kind),
        ])
        const range = datasetRange(coverage, timeframe)
        if (!range) {
          throw new ApiError(422, 'data_missing', `No ${timeframe} bars are stored for ${instrumentKey(instrument)}.`)
        }
        const rules = listed.instruments.find((i) => i.id.symbol === instrument.symbol)
        if (!rules) throw new Error(`${instrument.venue} does not list ${instrument.symbol} ${instrument.kind}.`)
        const info = strategyInfo.get(name) ?? (await api.strategies()).find((s) => s.name === name)
        const config: BacktestConfig = {
          strategy: name,
          params: defaults(info),
          instrument: rules,
          timeframe,
          from: range.from,
          to: range.to,
          starting_cash: STARTING_CASH,
          slippage_bps: SLIPPAGE_BPS,
          fee_override: null,
        }
        return (await api.startBacktest(config)).job_id
      },
      (info) => {
        const stage = info.message ? info.message.charAt(0).toUpperCase() + info.message.slice(1) : 'Waiting to start'
        onProgress({ step: 1, steps: 1, stage, fraction: info.progress })
      },
    )
    return {
      result: job.result.then(async (info) => toFinished(await api.jobResult(info.id))),
      cancel: job.cancel,
    }
  },

  download(instrument, onProgress) {
    const job = followJob(
      async () => (await api.downloadData({ instrument, from: HISTORY_FROM, to: Date.now() })).job_id,
      (info) => onProgress({ fraction: info.progress }),
    )
    return { result: job.result.then(() => {}), cancel: job.cancel }
  },
}
