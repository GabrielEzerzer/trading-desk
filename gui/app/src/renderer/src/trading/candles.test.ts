import type { Candle } from '@td/shared'
import { describe, expect, it } from 'vitest'
import {
  EMPTY_SERIES,
  addCandle,
  bucketOf,
  mergeCandles,
  onlyLastChanged,
  withClosed,
  withForming,
  withHistory,
} from './candles'

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

describe('the live series', () => {
  it('draws the forming bar as the last candle until its closed bar replaces it', () => {
    let s = withForming(EMPTY_SERIES, bar(60, 1))
    s = withForming(s, bar(60, 2))
    expect(s.candles).toEqual([bar(60, 2)])
    expect(s.closed).toBeNull()
    s = withClosed(s, bar(60, 3))
    expect(s.candles).toEqual([bar(60, 3)])
    expect(s.closed).toBe(60)
    s = withForming(s, bar(120, 4))
    expect(times(s.candles)).toEqual([60, 120])
    expect(s.revision).toBe(0)
  })

  it('ignores a forming bar that a closed bar already settled', () => {
    const s = withClosed(withForming(EMPTY_SERIES, bar(60, 1)), bar(60, 2))
    expect(withForming(s, bar(60, 9))).toBe(s)
    expect(withForming(s, bar(0, 9))).toBe(s)
  })

  it('puts the history under live bars and redraws', () => {
    const live = withForming(withClosed(EMPTY_SERIES, bar(180, 18)), bar(240, 24))
    const s = withHistory(live, [bar(60, 6), bar(120, 12), bar(180, 17)])
    expect(times(s.candles)).toEqual([60, 120, 180, 240])
    expect(s.candles[2]?.close).toBe(18)
    expect(s.candles[3]?.close).toBe(24)
    expect(s.closed).toBe(180)
    expect(s.revision).toBe(live.revision + 1)
  })

  it('lets the history replace a forming bar that has closed since', () => {
    const forming = withForming(EMPTY_SERIES, bar(120, 11))
    const s = withHistory(forming, [bar(60, 6), bar(120, 12)])
    expect(s.candles).toEqual([bar(60, 6), bar(120, 12)])
    expect(s.closed).toBe(120)
    // Its late forming updates no longer apply.
    expect(withForming(s, bar(120, 13))).toBe(s)
  })

  it('lets the history replace an earlier forming bar that never got its closed bar', () => {
    const live = withForming(withForming(withClosed(EMPTY_SERIES, bar(60, 6)), bar(120, 11)), bar(180, 17))
    const s = withHistory(live, [bar(60, 5), bar(120, 12)])
    expect(s.candles).toEqual([bar(60, 6), bar(120, 12), bar(180, 17)])
    expect(s.closed).toBe(120)
  })

  it('keeps a forming bar that history has no bar for', () => {
    const live = withForming(withForming(EMPTY_SERIES, bar(120, 11)), bar(180, 17))
    const s = withHistory(live, [bar(60, 6), bar(180, 18)])
    expect(s.candles).toEqual([bar(60, 6), bar(120, 11), bar(180, 18)])
  })

  it('changes nothing for an empty history', () => {
    const s = withClosed(EMPTY_SERIES, bar(60))
    expect(withHistory(s, [])).toBe(s)
  })
})

describe('onlyLastChanged', () => {
  const drawn = withForming(withClosed(EMPTY_SERIES, bar(1)), bar(2, 101))

  it('holds for a forming bar that moves and for one appended bar', () => {
    const moved = withForming(drawn, bar(2, 102))
    expect(onlyLastChanged(drawn.candles, moved.candles)).toBe(true)
    expect(onlyLastChanged(moved.candles, withForming(withClosed(moved, bar(2, 103)), bar(2, 103)).candles)).toBe(true)
    expect(onlyLastChanged([], [bar(1)])).toBe(true)
  })

  it('fails when a bar closes and the next starts forming before one render', () => {
    const next = withForming(withClosed(drawn, bar(2, 103)), bar(3, 104))
    expect(next.revision).toBe(drawn.revision)
    expect(onlyLastChanged(drawn.candles, next.candles)).toBe(false)
  })

  it('fails when two bars arrive before one render', () => {
    const next = withClosed(withClosed(drawn, bar(3)), bar(4))
    expect(onlyLastChanged(drawn.candles, next.candles)).toBe(false)
    expect(onlyLastChanged([], [bar(1), bar(2)])).toBe(false)
  })
})
