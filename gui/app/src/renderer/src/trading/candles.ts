// The chart's candles: history (when the source has any) merged with the live stream's closed bars.

import type { Candle, Timeframe } from '@td/shared'

export const TIMEFRAME_MS: Record<Timeframe, number> = {
  '1m': 60_000,
  '5m': 300_000,
  '15m': 900_000,
  '30m': 1_800_000,
  '1h': 3_600_000,
  '4h': 14_400_000,
  '1d': 86_400_000,
  '1w': 604_800_000,
}

/**
 * Merges `incoming` into `existing`, sorted by open time, one candle per open time. A candle in
 * `incoming` replaces one in `existing` with the same open time: a live bar is newer than history,
 * and a backfilled bar replaces what was there.
 */
export function mergeCandles(existing: readonly Candle[], incoming: readonly Candle[]): Candle[] {
  if (incoming.length === 0) return existing.slice()
  const byTime = new Map<number, Candle>()
  for (const c of existing) byTime.set(c.open_time, c)
  for (const c of incoming) byTime.set(c.open_time, c)
  return [...byTime.values()].sort((a, b) => a.open_time - b.open_time)
}

/**
 * How one new bar changes a sorted list: appended or replacing the last (`update`, which the chart
 * can draw in place), or landing earlier (`reset`, which redraws everything).
 */
export function addCandle(list: readonly Candle[], candle: Candle): { list: Candle[]; change: 'update' | 'reset' } {
  const last = list.at(-1)
  if (!last || candle.open_time > last.open_time) return { list: [...list, candle], change: 'update' }
  if (candle.open_time === last.open_time) return { list: [...list.slice(0, -1), candle], change: 'update' }
  return { list: mergeCandles(list, [candle]), change: 'reset' }
}

/** The open time of the bar that holds `time`. */
export function bucketOf(time: number, timeframe: Timeframe): number {
  const span = TIMEFRAME_MS[timeframe]
  // shortcut: weekly buckets here start on the Unix epoch's Thursday; match the engine's week start
  // if weekly charts are offered.
  return Math.floor(time / span) * span
}
