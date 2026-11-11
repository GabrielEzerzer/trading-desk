import type { Candle, InstrumentId, ServerMessage, SessionEvent, SessionState } from '@td/shared'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { engineTradingSource } from './source'

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
  it('passes on its own topic only and unsubscribes on stop', () => {
    const onCandle = vi.fn()
    const stop = engineTradingSource.candles(btc, '1m', onCandle)
    const topic = 'candles:binance:BTCUSDT:spot:1m'
    expect(bridge.td.subscribe).toHaveBeenCalledWith(topic)

    bridge.emit({ type: 'candle', topic, candle })
    bridge.emit({ type: 'candle', topic: 'candles:binance:BTCUSDT:spot:5m', candle })
    bridge.emit({ type: 'session', id: 'a', event: bar })
    expect(onCandle).toHaveBeenCalledTimes(1)
    expect(onCandle).toHaveBeenCalledWith(candle)

    stop()
    expect(bridge.td.unsubscribe).toHaveBeenCalledWith(topic)
    expect(bridge.listeners.size).toBe(0)
    bridge.emit({ type: 'candle', topic, candle })
    expect(onCandle).toHaveBeenCalledTimes(1)
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
