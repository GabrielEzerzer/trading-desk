import { ApiError } from '@td/shared'
import type { CoverageResponse, RunDetail, RunSummary } from '@td/shared'
import { describe, expect, it } from 'vitest'
import engine from '../../../../mock/engine.json'
import { datasetRange, laterRun, latestRuns, runFailure, toFinished, toVerdict } from './runs'
import { RunCancelled } from './source'

const details = engine.details as unknown as Record<string, RunDetail>
const runs = engine.runs as unknown as RunSummary[]
const detail = (id: string) => {
  const d = details[id]
  if (!d) throw new Error(`no fixture run ${id}`)
  return d
}
const SMA = '1794242520000-9e4b7a10'
const RSI_SHORT = '1794299400000-b7c8d9e0'
const DONCHIAN = engine.runResult

describe('a saved backtest as the result screen', () => {
  it('has no verdict or criteria until #32, and reads the numbers of the backtest', () => {
    const d = detail(SMA)
    if (d.run.kind !== 'backtest') throw new Error('the fixture is a backtest')
    const { metrics, config } = d.run.result
    const v = toVerdict(d)
    expect(v.decision).toBeNull()
    expect(v.criteria).toEqual([])
    expect(v.instrument).toEqual(config.instrument.id)
    expect(v.period).toEqual({ from: config.from, to: config.to - 1 })
    expect(v.finishedAt).toBe(Date.UTC(2026, 10, 9, 16, 42))
    expect(v.keyNumbers.map((k) => [k.label, k.value])).toEqual([
      ['Return', metrics.total_return],
      ['CAGR', metrics.cagr],
      ['Sharpe', metrics.sharpe],
      ['Max drawdown', metrics.max_drawdown],
    ])
    expect(v.metrics.find((m) => m.label === 'Trades')).toMatchObject({ strategy: metrics.trades, benchmark: null })
    expect(v.trades).toHaveLength(d.run.result.trades.length)
    expect(v.trades[0]?.returnPct).toBeCloseTo((d.run.result.trades[0]?.return_on_notional ?? 0) * 100)
  })

  it('keeps a trade still open at the end of the run marked as open', () => {
    const trades = toVerdict(detail(DONCHIAN)).trades
    expect(trades.at(-1)?.open).toBe(true)
    expect(trades.slice(0, -1).every((t) => t.open === false)).toBe(true)
  })

  it('draws growth of one unit of starting cash for the strategy and buy and hold', () => {
    const v = toVerdict(detail(SMA))
    const d = detail(SMA)
    if (d.run.kind !== 'backtest') throw new Error('the fixture is a backtest')
    const last = v.equity.at(-1)
    expect(last?.strategy).toBeCloseTo(1 + (d.run.result.metrics.total_return ?? 0), 3)
    expect(last?.benchmark).toBeCloseTo(1 + (d.run.result.benchmark.metrics.total_return ?? 0), 3)
  })

  it('keeps the metrics the engine could not compute on a short run as null', () => {
    const v = toVerdict(detail(RSI_SHORT))
    const key = Object.fromEntries(v.keyNumbers.map((k) => [k.label, k.value]))
    expect(key.CAGR).toBeNull()
    expect(key.Sharpe).toBeNull()
    expect(key.Return).toEqual(expect.any(Number))
    const row = Object.fromEntries(v.metrics.map((m) => [m.label, m]))
    for (const label of ['CAGR', 'Volatility', 'Sharpe', 'Sortino']) {
      expect(row[label]?.strategy, label).toBeNull()
      expect(row[label]?.benchmark, label).toBeNull()
    }
  })

  it('leaves the benchmark out where the run has no buy-and-hold point', () => {
    const d = structuredClone(detail(SMA))
    if (d.run.kind !== 'backtest') throw new Error('the fixture is a backtest')
    d.run.result.benchmark.equity = []
    expect(toVerdict(d).equity.every((p) => p.benchmark === null)).toBe(true)
  })

  it('thins a long equity curve for the chart and keeps its last point', () => {
    const d = structuredClone(detail(SMA))
    if (d.run.kind !== 'backtest') throw new Error('the fixture is a backtest')
    const one = d.run.result.equity[0]
    if (!one) throw new Error('the fixture has equity')
    d.run.result.equity = Array.from({ length: 5000 }, (_, i) => ({ time: one.time + i * 3_600_000, equity: 10_000 + i }))
    const points = toVerdict(d).equity
    expect(points.length).toBeLessThanOrEqual(1001)
    expect(points.at(-1)?.t).toBe(one.time + 4999 * 3_600_000)
  })

  it('refuses run kinds the Lab does not show yet', () => {
    const d = { ...detail(SMA), run: { kind: 'sweep', result: {} } } as unknown as RunDetail
    expect(() => toVerdict(d)).toThrow(/sweep run; the Lab shows backtests only/)
  })

  it('gives a finished run its row for the strategy list', () => {
    const { last } = toFinished(detail(RSI_SHORT))
    expect(last).toEqual({ id: RSI_SHORT, finishedAt: Date.UTC(2026, 10, 10, 8, 30), totalReturn: expect.any(Number), decision: null })
  })
})

describe('latest runs for the strategy list', () => {
  it('keeps the newest backtest of each strategy and leaves the other run kinds out', () => {
    const sweep = { ...runs[0], id: '1899999999999-00000000', created_at: 1_899_999_999_999, kind: 'sweep' } as RunSummary
    const latest = latestRuns([...runs, sweep])
    expect(latest.get('sma_cross')?.id).toBe(SMA)
    expect(latest.get('rsi_reversion')).toMatchObject({ id: RSI_SHORT, decision: null })
    expect(latest.has('donchian_breakout')).toBe(false)
  })
})

describe('the range of a dataset', () => {
  const coverage = (first: number, last: number): CoverageResponse => ({
    instrument: { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' },
    months: [{ month: '2024-03', minutes: 0, month_minutes: 0, first, last, gaps: [] }],
    funding: null,
  })
  const H = 3_600_000

  it('starts at the first whole bar and ends after the last whole bar', () => {
    // First minute 00:30, last minute 05:59 (the 05:00 bar is whole), so 01:00 to 06:00.
    const t0 = Date.UTC(2024, 2, 1)
    expect(datasetRange(coverage(t0 + 30 * 60_000, t0 + 6 * H - 60_000), '1h')).toEqual({ from: t0 + H, to: t0 + 6 * H })
    expect(datasetRange(coverage(t0, t0 + 2 * 24 * H - 60_000), '1d')).toEqual({ from: t0, to: t0 + 2 * 24 * H })
  })

  it('is null when nothing is stored or no whole bar fits', () => {
    const t0 = Date.UTC(2024, 2, 1)
    expect(datasetRange({ ...coverage(0, 0), months: [] }, '1h')).toBeNull()
    expect(datasetRange(coverage(t0 + 60_000, t0 + 3 * H), '1d')).toBeNull()
  })
})

describe('a failed run as the state to show', () => {
  it('maps the engine codes, a cancel and anything else', () => {
    expect(runFailure(new RunCancelled())).toEqual({ kind: 'cancelled' })
    expect(runFailure(new ApiError(422, 'data_missing', 'no candles before March 2024'))).toEqual({
      kind: 'data_missing',
      message: 'no candles before March 2024',
    })
    expect(runFailure(new ApiError(429, 'too_many_runs', '8 runs are already queued or running'))).toEqual({
      kind: 'too_many_runs',
      message: '8 runs are already queued or running',
    })
    expect(runFailure(new ApiError(400, 'invalid_config', 'fast must be below slow'))).toEqual({
      kind: 'failed',
      message: 'fast must be below slow',
    })
    expect(runFailure(new Error('the job failed: out of memory'))).toEqual({ kind: 'failed', message: 'the job failed: out of memory' })
    expect(runFailure('engine gone')).toEqual({ kind: 'failed', message: 'engine gone' })
  })
})

describe("a finished run as the strategy's last run", () => {
  const run = (finishedAt: number) => ({ id: `${finishedAt}-00000000`, finishedAt, totalReturn: 0.1, decision: null })

  it('replaces an older last run, or none', () => {
    expect(laterRun(null, run(2))).toEqual(run(2))
    expect(laterRun(run(1), run(2))).toEqual(run(2))
  })

  it('keeps a newer last run, when a run cancelled while saving finishes late', () => {
    expect(laterRun(run(3), run(2))).toEqual(run(3))
  })
})
