import type { Instrument } from '@td/shared'
import { describe, expect, it } from 'vitest'
import { searchInstruments, venueLabel } from './instruments'

const make = (symbol: string, kind: 'spot' | 'perpetual' = 'spot', venue: 'binance' | 'bybit' = 'binance') =>
  ({ id: { venue, symbol, kind } }) as Instrument

const list = [make('ETHBTC'), make('BTCUSDT'), make('ETHUSDT'), make('BTCUSDT', 'perpetual'), make('WBTCUSDT', 'spot', 'bybit')]
const symbols = (found: Instrument[]) => found.map((i) => `${i.id.symbol}:${i.id.kind}`)

describe('searchInstruments', () => {
  it('puts prefix matches before matches anywhere', () => {
    expect(symbols(searchInstruments(list, 'btc', 'all'))).toEqual([
      'BTCUSDT:spot',
      'BTCUSDT:perpetual',
      'ETHBTC:spot',
      'WBTCUSDT:spot',
    ])
  })

  it('ignores slashes, spaces and case', () => {
    expect(symbols(searchInstruments(list, 'eth / usdt', 'all'))).toEqual(['ETHUSDT:spot'])
  })

  it('filters by kind, with or without a query', () => {
    expect(symbols(searchInstruments(list, '', 'perpetual'))).toEqual(['BTCUSDT:perpetual'])
    expect(symbols(searchInstruments(list, 'usdt', 'spot'))).toEqual(['BTCUSDT:spot', 'ETHUSDT:spot', 'WBTCUSDT:spot'])
  })

  it('names the venue and kind', () => {
    expect(venueLabel({ venue: 'bybit', symbol: 'X', kind: 'perpetual' })).toBe('Bybit perpetual')
  })
})
