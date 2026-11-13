// Starts the real td-server binary and talks to it the way main does, and checks its answers hold the
// values the Lab reads. Build both binaries first with
// `cargo build --release -p td-server --bin td-server --bin td-bench`; td-bench seeds the data folder
// with synthetic candles. Without the binaries the test is skipped (gui.yml builds no Rust), unless
// TD_ENGINE_TEST=1, which ci.yml sets so that there a missing binary fails the run.
//
// shortcut: the routes that reach a venue (instruments, candles, a download that runs, a paper
// session) are not called, since tests never touch the network; crates/td-server/tests cover them
// against a fake venue.

import { execFileSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { afterAll, describe, expect, it } from 'vitest'
import { type BacktestConfig, type Instrument, type JobInfo, type ServerMessage, createClient } from '@td/shared'
import { type Connection, sendRequest } from './bridge'
import { Sidecar, sidecarBin } from './sidecar'
import { EngineSocket } from './socket'

const APP_PATH = fileURLToPath(new URL('../..', import.meta.url))
const BIN = sidecarBin(process.env, APP_PATH, false)
const BENCH = join(dirname(BIN), process.platform === 'win32' ? 'td-bench.exe' : 'td-bench')
const present = existsSync(BIN) && existsSync(BENCH)

if (!present) {
  const message = `no binaries at ${BIN} and ${BENCH}. Run cargo build --release -p td-server --bin td-server --bin td-bench.`
  if (process.env.TD_ENGINE_TEST === '1') throw new Error(`The td-server integration test must run, but there are ${message}`)
  console.warn(`Skipping the td-server integration test: ${message}`)
}

// What td-bench's `backtest_sma_1m_3mo` workload stores: Binance BTCUSDT spot, 2022-01 to 2022-03.
const SEED_WORKLOAD = 'backtest_sma_1m_3mo'
const INSTRUMENT: Instrument = {
  id: { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' },
  base: 'BTC',
  quote: 'USDT',
  tick_size: 0.01,
  lot_size: 0.00001,
  min_qty: 0.00001,
  min_notional: 5,
  fees: { maker: 0.001, taker: 0.001 },
}
const BTC = INSTRUMENT.id

describe.skipIf(!present)('the real td-server', () => {
  const dirs: string[] = []
  afterAll(async () => {
    for (const dir of dirs) await rm(dir, { recursive: true, force: true })
  })

  async function start(seed?: string) {
    const dataDir = await mkdtemp(join(tmpdir(), 'td-gui-test-'))
    dirs.push(dataDir)
    if (seed) execFileSync(BENCH, ['--split', 'all', '--filter', seed, '--seed', dataDir], { stdio: ['ignore', 'ignore', 'inherit'] })
    const env: NodeJS.ProcessEnv = { ...process.env, TD_DATA_DIR: dataDir }
    delete env.TD_TOKEN
    delete env.TD_DATA_CUTOFF
    const lines: string[] = []
    const sidecar = new Sidecar({ bin: BIN, env, log: (line) => lines.push(line) })
    return { sidecar, lines }
  }

  it('answers /health and /strategies through the main-process client, streams jobs, and stops cleanly', async () => {
    const { sidecar, lines } = await start()
    try {
      const connection: Connection = await sidecar.ready
      const client = createClient((request) => sendRequest(connection, request))

      expect((await client.health()).version).toMatch(/^\d+\.\d+\.\d+/)
      const strategies = await client.strategies()
      expect(strategies.length).toBeGreaterThan(0)
      expect(strategies[0]).toHaveProperty('params')
      await expect(client.job(999)).rejects.toMatchObject({ status: 404, code: 'not_found' })

      // A request with the wrong token is refused by the server itself.
      const wrong = await sendRequest({ ...connection, token: 'b'.repeat(64) }, { method: 'GET', path: '/health' })
      expect(wrong).toMatchObject({ ok: false, status: 401, error: { code: 'unauthorized' } })

      // The WebSocket authenticates with the token in its subprotocol.
      const messages: ServerMessage[] = []
      const subscribed = new Promise<void>((resolve) => {
        const socket = new EngineSocket((message) => {
          messages.push(message)
          if (message.type === 'subscribed') {
            socket.close()
            resolve()
          }
        }, (line) => lines.push(line))
        socket.subscribe('jobs')
        socket.open(connection)
      })
      await subscribed
      expect(messages[0]).toEqual({ type: 'subscribed', topic: 'jobs' })
    } finally {
      expect(await sidecar.stop()).toBe('clean')
    }
    expect(lines.some((line) => line.includes('stdin closed'))).toBe(true)
  })

  it('reads stored data, runs a backtest the Lab can show, deletes data, and resubscribes after a drop', async () => {
    const { sidecar, lines } = await start(SEED_WORKLOAD)
    let socket: EngineSocket | undefined
    try {
      const connection: Connection = await sidecar.ready
      const client = createClient((request) => sendRequest(connection, request))

      // Stored data and coverage, read as the Lab reads them.
      const stored = await client.stored()
      expect(stored.instruments.map((i) => i.instrument)).toEqual([BTC])
      const months = stored.instruments[0]!.months
      expect(months.map((m) => m.month)).toEqual(['2022-01', '2022-02', '2022-03'])
      const coverage = await client.coverage(BTC)
      expect(coverage.funding).toBeNull()
      expect(coverage.months.map((m) => m.bytes)).toEqual(months.map((m) => m.bytes))
      expect(coverage.months[0]!.first).toBe(Date.UTC(2022, 0, 1))
      expect(coverage.months.at(-1)!.last).toBe(Date.UTC(2022, 3, 1) - 60_000)

      expect(await client.runs()).toEqual([])
      expect(await client.jobs()).toEqual([])
      await expect(client.downloadData({ instrument: BTC, from: Date.UTC(2022, 1, 1), to: Date.UTC(2022, 0, 1) })).rejects.toMatchObject({
        status: 400,
        code: 'bad_request',
      })

      // The socket resubscribes when its connection drops, and then streams the run's job.
      const messages: ServerMessage[] = []
      let onMessage = (_message: ServerMessage) => {}
      socket = new EngineSocket((message) => {
        messages.push(message)
        onMessage(message)
      }, (line) => lines.push(line))
      const next = (match: (message: ServerMessage) => boolean) =>
        new Promise<ServerMessage>((resolve) => {
          onMessage = (message) => {
            if (match(message)) resolve(message)
          }
        })
      let subscribed = next((m) => m.type === 'subscribed')
      socket.subscribe('jobs')
      socket.open(connection)
      await subscribed
      subscribed = next((m) => m.type === 'subscribed')
      // Drop the connection under the socket, as a network blip or an engine restart would.
      ;(socket as unknown as { socket: WebSocket }).socket.close()
      await subscribed
      expect(messages.filter((m) => m.type === 'subscribed')).toHaveLength(2)
      expect(lines.some((line) => line.includes('reconnecting'))).toBe(true)

      const config: BacktestConfig = {
        strategy: 'sma_cross',
        params: {},
        instrument: INSTRUMENT,
        timeframe: '1h',
        from: Date.UTC(2022, 1, 1),
        to: Date.UTC(2022, 2, 1),
        starting_cash: 10_000,
        slippage_bps: 1,
        fee_override: null,
      }
      const finished = next((m) => m.type === 'job' && ['done', 'failed', 'cancelled'].includes(m.job.status))
      const { job_id } = await client.startBacktest(config)
      const job = ((await finished) as { job: JobInfo }).job
      expect(job).toMatchObject({ id: job_id, kind: 'backtest', status: 'done', error: null })
      expect((await client.job(job_id)).status).toBe('done')
      await expect(client.cancelJob(job_id)).rejects.toMatchObject({ status: 409, code: 'job_finished' })

      // The finished run holds what the result screen and the strategy row read (lab/runs.ts).
      const result = await client.jobResult(job_id)
      if (result.run.kind !== 'backtest') throw new Error(`job ${job_id} saved a ${result.run.kind} run`)
      const { config: ran, equity, benchmark, metrics, trades } = result.run.result
      expect(ran).toMatchObject({ instrument: INSTRUMENT, from: config.from, to: config.to })
      expect(equity.length).toBeGreaterThan(0)
      expect(equity.every((p) => Number.isFinite(p.time) && Number.isFinite(p.equity))).toBe(true)
      expect(benchmark.equity.length).toBeGreaterThan(0)
      expect(metrics.start_equity).toBeGreaterThan(0)
      expect(benchmark.metrics.start_equity).toBeGreaterThan(0)
      expect(Number.isFinite(metrics.total_return)).toBe(true)
      expect(Array.isArray(trades)).toBe(true)
      expect(result).toMatchObject({ check: null, data_error: null })
      const runs = await client.runs()
      expect(runs).toEqual([
        expect.objectContaining({
          id: result.id,
          kind: 'backtest',
          strategy: 'sma_cross',
          created_at: Number(result.id.slice(0, 13)),
          total_return: metrics.total_return,
        }),
      ])
      const saved = await client.run(result.id)
      expect(saved.check).not.toBeNull()
      expect(saved.run).toEqual(result.run)

      // No sessions; the session routes answer for one that does not exist.
      expect(await client.sessions()).toEqual([])
      const missing = { status: 404, code: 'not_found' }
      await expect(client.session('nope')).rejects.toMatchObject(missing)
      await expect(client.stopSession('nope')).rejects.toMatchObject(missing)
      await expect(client.resumeSession('nope')).rejects.toMatchObject(missing)
      await expect(client.placeOrder('nope', { side: 'buy', qty: 0.001, type: 'market' })).rejects.toMatchObject(missing)
      await expect(client.cancelOrder('nope', 1)).rejects.toMatchObject(missing)

      // Deletes: one month, then everything; the saved run stays.
      const [jan, feb, mar] = months
      expect(await client.deleteData({ instrument: BTC, from: '2022-03', to: '2022-03' })).toEqual({ files: 1, bytes: mar!.bytes })
      expect(await client.deleteAllData()).toEqual({ files: 2, bytes: jan!.bytes + feb!.bytes })
      expect((await client.stored()).instruments).toEqual([])
      expect((await client.runs()).map((r) => r.id)).toEqual([result.id])
    } finally {
      socket?.close()
      expect(await sidecar.stop()).toBe('clean')
    }
  })
})
