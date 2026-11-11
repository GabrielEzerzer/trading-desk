// The viewer's recently shown instruments, newest first, kept in this browser's localStorage. A
// convenience only: storage can be empty, full or blocked, and the list then starts empty.

import type { InstrumentId } from '@td/shared'
import { sameInstrument } from './session'

const KEY = 'td.trading.recent'
const MAX = 5

function isInstrumentId(value: unknown): value is InstrumentId {
  if (typeof value !== 'object' || value === null) return false
  const v = value as Record<string, unknown>
  return (
    (v.venue === 'binance' || v.venue === 'bybit') &&
    (v.kind === 'spot' || v.kind === 'perpetual') &&
    typeof v.symbol === 'string' &&
    /^[A-Z0-9_-]{1,32}$/.test(v.symbol)
  )
}

export function loadRecent(storage: Pick<Storage, 'getItem'> | undefined = globalThis.localStorage): InstrumentId[] {
  try {
    const parsed: unknown = JSON.parse(storage?.getItem(KEY) ?? '[]')
    if (!Array.isArray(parsed)) return []
    return parsed
      .filter(isInstrumentId)
      .slice(0, MAX)
      .map(({ venue, symbol, kind }) => ({ venue, symbol, kind }))
  } catch {
    return []
  }
}

/** Puts `id` first and saves the list; returns the new list even if saving failed. */
export function pushRecent(
  id: InstrumentId,
  storage: Pick<Storage, 'getItem' | 'setItem'> | undefined = globalThis.localStorage,
): InstrumentId[] {
  const next = [id, ...loadRecent(storage).filter((r) => !sameInstrument(r, id))].slice(0, MAX)
  try {
    storage?.setItem(KEY, JSON.stringify(next))
  } catch {
    // Storage is full or blocked: the list lasts for this view only.
  }
  return next
}
