import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { overallProgress } from '../src/renderer/src/lab/progress'
import { RunCancelled } from '../src/renderer/src/lab/source'
import { createMockLabSource } from './lab'

describe('mock Lab source', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('lists one Trade, one No trade and strategies that never ran, with readable names', async () => {
    const source = createMockLabSource({ listMs: 10 })
    const listing = source.strategies()
    await vi.advanceTimersByTimeAsync(10)
    const list = await listing
    expect(list.map((s) => s.label)).toEqual(['SMA cross', 'RSI reversion', 'Donchian breakout', 'Funding carry'])
    expect(list.map((s) => s.last?.decision ?? null)).toEqual(['trade', 'no_trade', null, null])
  })

  it('reports progress in order and finishes with a Trade verdict stamped now', async () => {
    const source = createMockLabSource({ stepMs: 100, now: () => 42 })
    const seen: number[] = []
    const steps: number[] = []
    const run = source.run('donchian_breakout', (p) => {
      seen.push(overallProgress(p))
      steps.push(p.step)
      expect(p.steps).toBe(4)
    })
    await vi.advanceTimersByTimeAsync(10_000)
    const verdict = await run.result
    expect(verdict.decision).toBe('trade')
    expect(verdict.finishedAt).toBe(42)
    expect(steps.at(-1)).toBe(4)
    expect(seen).toEqual([...seen].sort((a, b) => a - b))
  })

  it('fails with the server-style message', async () => {
    const source = createMockLabSource({ stepMs: 100 })
    const run = source.run('funding_carry', () => {})
    const outcome = expect(run.result).rejects.toThrow(/No funding rates are stored/)
    await vi.advanceTimersByTimeAsync(10_000)
    await outcome
  })

  it('stops reporting and rejects with RunCancelled when cancelled', async () => {
    const source = createMockLabSource({ stepMs: 100 })
    const seen: number[] = []
    const run = source.run('donchian_breakout', (p) => seen.push(p.step))
    await vi.advanceTimersByTimeAsync(150)
    run.cancel()
    await expect(run.result).rejects.toBeInstanceOf(RunCancelled)
    const count = seen.length
    await vi.advanceTimersByTimeAsync(10_000)
    expect(seen).toHaveLength(count)
  })
})
