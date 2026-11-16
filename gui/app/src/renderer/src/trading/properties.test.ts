// Property tests on the chart's candle merging and the order ticket's grid checks, from fixed seeds.

import type { Candle } from '@td/shared'
import fc from 'fast-check'
import { describe, expect, it } from 'vitest'
import { addCandle, mergeCandles } from './candles'
import { formatStep, onLot, onTick, parseAmount, stepDecimals } from './order'

const runs = { seed: 0x7d6e1, numRuns: 300 }

const candle: fc.Arbitrary<Candle> = fc.record({
  open_time: fc.integer({ min: 0, max: 40 }).map((n) => n * 60_000),
  open: fc.integer({ min: 1, max: 1000 }),
  high: fc.integer({ min: 1, max: 1000 }),
  low: fc.integer({ min: 1, max: 1000 }),
  close: fc.integer({ min: 1, max: 1000 }),
  volume: fc.integer({ min: 0, max: 1000 }),
  quote_volume: fc.integer({ min: 0, max: 1000 }),
})

const candles = fc.array(candle, { maxLength: 30 })

const lastAt = (list: readonly Candle[], time: number) => list.findLast((c) => c.open_time === time)

describe('mergeCandles', () => {
  it('into a sorted list keeps one candle per open time, sorted, the incoming one winning, and merging again changes nothing', () => {
    fc.assert(
      fc.property(candles, candles, (raw, incoming) => {
        const existing = mergeCandles([], raw)
        const merged = mergeCandles(existing, incoming)
        const times = merged.map((c) => c.open_time)
        expect(times.every((t, i) => i === 0 || (times[i - 1] ?? t) < t)).toBe(true)
        expect(new Set(times)).toEqual(new Set([...existing, ...incoming].map((c) => c.open_time)))
        for (const c of merged) expect(c).toBe(lastAt(incoming, c.open_time) ?? lastAt(existing, c.open_time))
        expect(mergeCandles(merged, incoming)).toEqual(merged)
      }),
      runs,
    )
  })
})

describe('addCandle', () => {
  it('gives the same list as a merge, and draws in place only at the end', () => {
    fc.assert(
      fc.property(candles, candle, (raw, next) => {
        const list = mergeCandles([], raw)
        const added = addCandle(list, next)
        expect(added.list).toEqual(mergeCandles(list, [next]))
        const last = list.at(-1)
        expect(added.change).toBe(!last || next.open_time >= last.open_time ? 'update' : 'reset')
      }),
      runs,
    )
  })
})

const steps: [number, number][] = [
  [1, 1],
  [1, 2],
  [1, 3],
  [1, 8],
  [5, 1],
  [25, 2],
  [1, 0],
  [5, 0],
]

describe('the order grid', () => {
  it('takes every multiple of a step, refuses a third of a step off it, and reads back what it prints', () => {
    fc.assert(
      fc.property(fc.constantFrom(...steps), fc.integer({ min: 0, max: 10_000_000 }), ([units, decimals], count) => {
        const scale = 10 ** decimals
        const step = units / scale
        const value = (count * units) / scale
        expect(stepDecimals(step)).toBe(decimals)
        expect(onLot(value, step)).toBe(true)
        expect(onTick(value, step)).toBe(true)
        expect(onLot(value + step / 3, step)).toBe(false)
        expect(parseAmount(formatStep(value, step))).toBe(value)
      }),
      runs,
    )
  })
})
