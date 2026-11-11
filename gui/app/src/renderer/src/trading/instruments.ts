// Finding instruments in the switcher's menu.

import type { Instrument, InstrumentId, InstrumentKind } from '@td/shared'

export type KindFilter = 'all' | InstrumentKind

// shortcut: shows the first 60 matches; virtualise the list if scrolling the full venue lists is
// ever wanted.
export const MAX_ROWS = 60

const VENUES = { binance: 'Binance', bybit: 'Bybit' } as const

export function venueLabel(id: InstrumentId): string {
  return `${VENUES[id.venue]} ${id.kind}`
}

/** Symbol matches first by prefix, then anywhere; "btc/usdt" and "btc usdt" find BTCUSDT. */
export function searchInstruments(list: readonly Instrument[], query: string, kind: KindFilter): Instrument[] {
  const q = query.toUpperCase().replace(/[\s/\-_]/g, '')
  const pool = kind === 'all' ? list : list.filter((i) => i.id.kind === kind)
  if (!q) return pool.slice()
  const starts: Instrument[] = []
  const contains: Instrument[] = []
  for (const i of pool) {
    const symbol = i.id.symbol.replace(/[-_]/g, '')
    if (symbol.startsWith(q)) starts.push(i)
    else if (symbol.includes(q)) contains.push(i)
  }
  return [...starts, ...contains]
}

