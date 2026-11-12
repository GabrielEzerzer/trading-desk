import type { ApiRequest, ApiResult, Candle, InstrumentId, ServerMessage, SessionEvent, SessionState } from '@td/shared'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { HISTORY_BARS, MANUAL, engineTradingSource } from './source'

// A stand-in for the preload's `window.td`: the parts a subscription uses.
function fakeBridge() {
  const listeners = new Set<(message: ServerMessage) => void>()
  const td = {
    subscribe: vi.fn(async (_topic: string) => {}),
    unsubscribe: vi.fn(async (_topic: string) => {}),
    onMessage: vi.fn((listener: (message: ServerMessage) => void) => {
      listeners.add(listener)
      return () => {
        listeners.delete(listener)
      }
    }),
  }
  const emit = (message: ServerMessage) => {
    for (const listener of [...listeners]) listener(message)
  }
  return { td, listeners, emit }
}

const btc: InstrumentId = { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' }
const candle: Candle = { open_time: 60_000, open: 1, high: 2, low: 0.5, close: 1.5, volume: 3, quote_volume: 4.5 }
const state = (id: string) => ({ id }) as SessionState
const bar: SessionEvent = { event: 'bar', bar: candle, warm_up: false, equity: 100 }

let bridge: ReturnType<typeof fakeBridge>

beforeEach(() => {
  bridge = fakeBridge()
  vi.stubGlobal('window', { td: bridge.td })
})

afterEach(() => vi.unstubAllGlobals())

describe('engine candle stream', () => {
  it('passes on its own topic only, forming and closed, and unsubscribes on stop', () => {
    const feed = { onClosed: vi.fn(), onForming: vi.fn() }
    const stop = engineTradingSource.candles(btc, '1m', feed)
    const topic = 'candles:binance:BTCUSDT:spot:1m'
    expect(bridge.td.subscribe).toHaveBeenCalledWith(topic)

    const forming = { ...candle, close: 1.2 }
    bridge.emit({ type: 'candle_forming', topic, candle: forming })
    bridge.emit({ type: 'candle', topic, candle })
    bridge.emit({ type: 'candle', topic: 'candles:binance:BTCUSDT:spot:5m', candle })
    bridge.emit({ type: 'candle_forming', topic: 'candles:binance:BTCUSDT:spot:5m', candle })
    bridge.emit({ type: 'session', id: 'a', event: bar })
    expect(feed.onForming.mock.calls).toEqual([[forming]])
    expect(feed.onClosed.mock.calls).toEqual([[candle]])

    stop()
    expect(bridge.td.unsubscribe).toHaveBeenCalledWith(topic)
    expect(bridge.listeners.size).toBe(0)
    bridge.emit({ type: 'candle', topic, candle })
    bridge.emit({ type: 'candle_forming', topic, candle })
    expect(feed.onClosed).toHaveBeenCalledTimes(1)
    expect(feed.onForming).toHaveBeenCalledTimes(1)
  })
})

describe('engine candle history', () => {
  it('asks GET /candles for the last bars of the instrument and timeframe', async () => {
    const request = vi.fn(async (_r: ApiRequest): Promise<ApiResult> => ({
      ok: true,
      status: 200,
      body: { instrument: btc, timeframe: '5m', from: 0, to: 1, truncated: false, candles: [candle], from_venue: [], missing: [] },
    }))
    vi.stubGlobal('window', { td: { ...bridge.td, request } })
    expect(await engineTradingSource.history(btc, '5m')).toEqual([candle])
    expect(request).toHaveBeenCalledWith({
      method: 'GET',
      path: '/candles',
      query: { instrument: 'binance:BTCUSDT:spot', timeframe: '5m', limit: String(HISTORY_BARS) },
    })
  })

  it('rejects with the engine’s words when the venue fails', async () => {
    const message = 'fetching binance:BTCUSDT:spot 5m bars from the venue failed: timed out'
    const request = vi.fn(async (): Promise<ApiResult> => ({ ok: false, status: 502, error: { code: 'venue', message } }))
    vi.stubGlobal('window', { td: { ...bridge.td, request } })
    await expect(engineTradingSource.history(btc, '5m')).rejects.toThrow(message)
  })
})

describe('engine session feed', () => {
  it('passes on its own session only and unsubscribes on stop', () => {
    const onState = vi.fn()
    const onEvent = vi.fn()
    const stop = engineTradingSource.watchSession('a', { onState, onEvent })
    expect(bridge.td.subscribe).toHaveBeenCalledWith('session:a')

    bridge.emit({ type: 'session_state', session: state('a') })
    bridge.emit({ type: 'session_state', session: state('b') })
    bridge.emit({ type: 'session', id: 'a', event: bar })
    bridge.emit({ type: 'session', id: 'b', event: bar })
    bridge.emit({ type: 'candle', topic: 'session:a', candle })
    expect(onState).toHaveBeenCalledTimes(1)
    expect(onState).toHaveBeenCalledWith(state('a'))
    expect(onEvent).toHaveBeenCalledTimes(1)
    expect(onEvent).toHaveBeenCalledWith(bar)

    stop()
    expect(bridge.td.unsubscribe).toHaveBeenCalledWith('session:a')
    expect(bridge.listeners.size).toBe(0)
    bridge.emit({ type: 'session', id: 'a', event: bar })
    expect(onEvent).toHaveBeenCalledTimes(1)
  })
})

describe('engine manual orders', () => {
  const placed = { source: 'manual', id: 4, status: { status: 'open' } }

  it('names a manual session the way td-server does', () => {
    expect(MANUAL).toBe('none')
  })

  it('offers orders and manual sessions, and places and cancels through the order routes', async () => {
    expect(engineTradingSource.orders).toEqual({ available: true })
    expect(engineTradingSource.manualSessions).toEqual({ available: true })
    const request = vi.fn(async (_r: ApiRequest): Promise<ApiResult> => ({ ok: true, status: 201, body: placed }))
    vi.stubGlobal('window', { td: { ...bridge.td, request } })

    await expect(
      engineTradingSource.placeOrder('s-1', { side: 'buy', qty: 0.05, type: 'limit', limit_price: 60_000 }),
    ).resolves.toBeUndefined()
    await expect(engineTradingSource.cancelOrder('s-1', 4)).resolves.toBeUndefined()
    expect(request.mock.calls.map(([r]) => r)).toEqual([
      { method: 'POST', path: '/sessions/s-1/orders', body: { side: 'buy', qty: 0.05, type: 'limit', limit_price: 60_000 } },
      { method: 'POST', path: '/sessions/s-1/orders/4/cancel' },
    ])
  })

  it("rejects with the engine's reason when it refuses the order", async () => {
    const message = 'the session has not fed its first live bar yet; orders can be placed once it has'
    const request = vi.fn(async (): Promise<ApiResult> => ({ ok: false, status: 409, error: { code: 'not_live', message } }))
    vi.stubGlobal('window', { td: { ...bridge.td, request } })
    await expect(engineTradingSource.placeOrder('s-1', { side: 'sell', qty: 1, type: 'market' })).rejects.toThrow(message)
  })
})
