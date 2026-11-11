import type { Candle } from '@td/shared'
import { describe, expect, it } from 'vitest'
import { addCandle, bucketOf, mergeCandles } from './candles'

const bar = (open_time: number, close = 100): Candle => ({
  open_time,
  open: close,
  high: close,
  low: close,
  close,
  volume: 1,
  quote_volume: close,
})

const times = (list: Candle[]) => list.map((c) => c.open_time)

describe('mergeCandles', () => {
  it('joins history and live in open-time order', () => {
    const history = [bar(1), bar(2), bar(3)]
    const live = [bar(5), bar(4)]
    expect(times(mergeCandles(history, live))).toEqual([1, 2, 3, 4, 5])
  })

  it('keeps one candle per open time, the incoming one', () => {
    const history = [bar(1, 10), bar(2, 20), bar(3, 30)]
    const live = [bar(3, 31), bar(4, 40)]
    const merged = mergeCandles(history, live)
    expect(times(merged)).toEqual([1, 2, 3, 4])
    expect(merged[2]?.close).toBe(31)
  })

  it('handles empty sides and leaves its inputs alone', () => {
    const history = [bar(2), bar(1)]
    expect(times(mergeCandles([], history))).toEqual([1, 2])
    expect(times(mergeCandles([bar(1)], []))).toEqual([1])
    expect(times(history)).toEqual([2, 1])
  })
})

describe('addCandle', () => {
  it('appends a newer bar as an update', () => {
    const { list, change } = addCandle([bar(1), bar(2)], bar(3))
    expect(times(list)).toEqual([1, 2, 3])
    expect(change).toBe('update')
  })

  it('replaces the last bar in place when its open time repeats', () => {
    const { list, change } = addCandle([bar(1), bar(2, 20)], bar(2, 21))
    expect(times(list)).toEqual([1, 2])
    expect(list[1]?.close).toBe(21)
    expect(change).toBe('update')
  })

  it('merges a backfilled earlier bar and asks for a redraw', () => {
    const { list, change } = addCandle([bar(1), bar(3)], bar(2))
    expect(times(list)).toEqual([1, 2, 3])
    expect(change).toBe('reset')
  })

  it('starts an empty chart', () => {
    expect(addCandle([], bar(7))).toEqual({ list: [bar(7)], change: 'update' })
  })
})

describe('bucketOf', () => {
  it('finds the open time of the bar holding a time', () => {
    expect(bucketOf(3_600_000 * 5 + 1234, '1h')).toBe(3_600_000 * 5)
    expect(bucketOf(60_000 * 7, '1m')).toBe(60_000 * 7)
    expect(bucketOf(900_000 * 3 - 1, '15m')).toBe(900_000 * 2)
  })
})
