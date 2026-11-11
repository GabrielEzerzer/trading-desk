import type { InstrumentId, SessionState, SessionSummary, Status } from '@td/shared'
import { describe, expect, it } from 'vitest'
import { loadRecent, pushRecent } from './recent'
import { applySessionEvent, pickSession, rejection } from './session'
import { loadInstrumentLists } from './source'

const btc: InstrumentId = { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' }
const eth: InstrumentId = { venue: 'binance', symbol: 'ETHUSDT', kind: 'spot' }

const summary = (id: string, instrument: InstrumentId, status: Status['status']): SessionSummary => ({
  id,
  strategy: 'manual',
  instrument,
  timeframe: '1m',
  status: status === 'error' ? { status, message: 'x' } : ({ status } as Status),
  equity: 10_000,
})

describe('pickSession', () => {
  it('prefers the latest running session on the instrument', () => {
    const list = [summary('1', btc, 'live'), summary('2', btc, 'stopped'), summary('3', eth, 'live')]
    expect(pickSession(list, btc)?.id).toBe('1')
  })

  it('falls back to the latest session on the instrument', () => {
    const list = [summary('1', btc, 'stopped'), summary('2', btc, 'error')]
    expect(pickSession(list, btc)?.id).toBe('2')
  })

  it('finds none for another instrument', () => {
    expect(pickSession([summary('1', btc, 'live')], { ...btc, kind: 'perpetual' })).toBeNull()
  })
})

describe('applySessionEvent', () => {
  const state = {
    id: 's',
    status: { status: 'live' },
    dropped: [],
    warnings: [],
  } as unknown as SessionState

  it('applies status, dropped buckets and warnings in place', () => {
    expect(applySessionEvent(state, { event: 'status', status: { status: 'reconnecting' } })).toMatchObject({
      state: { status: { status: 'reconnecting' } },
      refetch: false,
    })
    expect(applySessionEvent(state, { event: 'dropped', open_time: 60_000 }).state.dropped).toEqual([60_000])
    expect(applySessionEvent(state, { event: 'warning', message: 'slow feed' }).state.warnings).toEqual(['slow feed'])
  })

  it('asks for the full state after a bar', () => {
    const bar = { open_time: 0, open: 1, high: 1, low: 1, close: 1, volume: 0, quote_volume: 0 }
    expect(applySessionEvent(state, { event: 'bar', bar, warm_up: false, equity: 1 })).toEqual({ state, refetch: true })
  })

  it('reads a rejection from an order event', () => {
    const order = {
      id: 1,
      request: { instrument: btc, side: 'buy', qty: 1, order_type: { type: 'market' }, tif: 'gtc', reduce_only: false, client_tag: null },
      submitted_at: 0,
      decision_close: 1,
      attached_stop: null,
      status: { status: 'rejected', reason: 'not enough cash' },
    } as const
    expect(rejection({ event: 'order', order })).toBe('not enough cash')
    expect(rejection({ event: 'order', order: { ...order, status: { status: 'open' } } })).toBeNull()
  })
})

describe('loadInstrumentLists', () => {
  const instrument = { id: btc } as never

  it('keeps the lists that loaded and names the ones that did not', async () => {
    const list = await loadInstrumentLists(async (venue, kind) => {
      if (venue === 'bybit' && kind === 'perpetual') throw new Error('venue down')
      return venue === 'binance' && kind === 'spot' ? [instrument] : []
    })
    expect(list).toEqual({ instruments: [instrument], problems: ['The Bybit perpetual list did not load.'] })
  })

  it('throws when every list failed', async () => {
    await expect(loadInstrumentLists(async () => Promise.reject(new Error('offline')))).rejects.toThrow('offline')
  })
})

describe('recent instruments', () => {
  function memory() {
    const data = new Map<string, string>()
    return { getItem: (k: string) => data.get(k) ?? null, setItem: (k: string, v: string) => void data.set(k, v) }
  }

  it('keeps the newest first, without repeats, at most five', () => {
    const storage = memory()
    for (const symbol of ['A', 'B', 'C', 'D', 'E', 'F']) pushRecent({ ...btc, symbol }, storage)
    pushRecent({ ...btc, symbol: 'C' }, storage)
    expect(loadRecent(storage).map((r) => r.symbol)).toEqual(['C', 'F', 'E', 'D', 'B'])
  })

  it('reads nothing from broken, foreign or blocked storage', () => {
    expect(loadRecent({ getItem: () => '{not json' })).toEqual([])
    expect(loadRecent({ getItem: () => JSON.stringify([{ venue: 'kraken', symbol: 'X', kind: 'spot' }, 'x']) })).toEqual([])
    expect(
      loadRecent({
        getItem: () => {
          throw new Error('blocked')
        },
      }),
    ).toEqual([])
    expect(loadRecent(undefined)).toEqual([])
  })

  it('still returns the list when saving fails', () => {
    const storage = {
      getItem: () => null,
      setItem: () => {
        throw new Error('full')
      },
    }
    expect(pushRecent(eth, storage)).toEqual([eth])
  })
})
