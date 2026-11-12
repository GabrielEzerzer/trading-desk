// The chart's candles: the history merged with the live stream, whose last bar may still be forming.

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

/**
 * Whether `next` differs from the drawn `prev` only at its end (the last bar replaced, or one bar
 * appended), so the chart can draw that one bar in place. Two changes in one render, such as a bar
 * closing and the next one starting to form, change the bar before the last too. Untouched bars
 * keep their identity through `addCandle`, so the bar before the last tells.
 */
export function onlyLastChanged(prev: readonly Candle[], next: readonly Candle[]): boolean {
  const grew = next.length - prev.length
  if (grew !== 0 && grew !== 1) return false
  return next.length < 2 || next.at(-2) === prev.at(grew === 1 ? -1 : -2)
}

/** The bars a chart draws. Only the last one can still be forming. */
export interface CandleSeries {
  candles: Candle[]
  /** Bumps when the list changed anywhere but its end, so the chart redraws it whole. */
  revision: number
  /** The open time of the newest closed bar, or null before any; a later bar is still forming. */
  closed: number | null
}

export const EMPTY_SERIES: CandleSeries = { candles: [], revision: 0, closed: null }

function withBar<S extends CandleSeries>(series: S, candle: Candle): S {
  const { list, change } = addCandle(series.candles, candle)
  return { ...series, candles: list, revision: change === 'reset' ? series.revision + 1 : series.revision }
}

/** A closed bar: it takes its open time's place, the forming bar's included. */
export function withClosed<S extends CandleSeries>(series: S, candle: Candle): S {
  const next = withBar(series, candle)
  return { ...next, closed: Math.max(series.closed ?? candle.open_time, candle.open_time) }
}

/** The forming bar: drawn as the last candle, unless a closed bar at or after it has arrived. */
export function withForming<S extends CandleSeries>(series: S, candle: Candle): S {
  if (series.closed !== null && candle.open_time <= series.closed) return series
  return withBar(series, candle)
}

/**
 * The history under the live bars. A live bar wins over history at the same open time, except a
 * forming one (any after `closed`, the last or an earlier one whose closed bar never came) that
 * history already holds closed, which history replaces.
 */
export function withHistory<S extends CandleSeries>(series: S, history: readonly Candle[]): S {
  const end = history.at(-1)?.open_time
  if (end === undefined) return series
  const { closed } = series
  const live = series.candles.filter((c) => (closed !== null && c.open_time <= closed) || c.open_time > end)
  return {
    ...series,
    candles: mergeCandles(history, live),
    revision: series.revision + 1,
    closed: Math.max(series.closed ?? end, end),
  }
}

/** The open time of the bar that holds `time`. */
export function bucketOf(time: number, timeframe: Timeframe): number {
  const span = TIMEFRAME_MS[timeframe]
  // shortcut: weekly buckets here start on the Unix epoch's Thursday; match the engine's week start
  // if weekly charts are offered.
  return Math.floor(time / span) * span
}
