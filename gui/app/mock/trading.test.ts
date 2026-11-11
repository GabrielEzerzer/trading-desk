import type { Candle, InstrumentId, SessionEvent, SessionState } from '@td/shared'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { MANUAL } from '../src/renderer/src/trading/source'
import { aggregate, createMockTradingSource, fixtureHistory } from './trading'

const btc: InstrumentId = { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' }

describe('mock Trading fixtures', () => {
  it('has a few thousand 1m and 1h bars, sorted and unique by open time', () => {
    for (const timeframe of ['1m', '1h'] as const) {
      const bars = fixtureHistory(btc, timeframe)
      expect(bars.length).toBeGreaterThanOrEqual(2000)
      for (let i = 1; i < bars.length; i++) expect(bars[i]!.open_time).toBeGreaterThan(bars[i - 1]!.open_time)
    }
  })

  it('aggregates finer bars into coarser ones', () => {
    const minute = (t: number, o: number, h: number, l: number, c: number) => ({
      open_time: t * 60_000,
      open: o,
      high: h,
      low: l,
      close: c,
      volume: 1,
      quote_volume: c,
    })
    const bars = aggregate([minute(0, 10, 12, 9, 11), minute(1, 11, 15, 10, 14), minute(5, 14, 14, 13, 13)], '5m')
    expect(bars).toEqual([
      { open_time: 0, open: 10, high: 15, low: 9, close: 14, volume: 2, quote_volume: 25 },
      { open_time: 300_000, open: 14, high: 14, low: 13, close: 13, volume: 1, quote_volume: 13 },
    ])
  })

  it('scales the shared path for instruments without their own history', () => {
    const sol = fixtureHistory({ venue: 'binance', symbol: 'SOLUSDT', kind: 'spot' }, '1h')
    expect(sol.at(-1)?.close).toBeCloseTo(152.4, 2)
  })
})

describe('mock live bars', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('shows the bar in progress twice before it closes, then closes on time', async () => {
    const source = createMockTradingSource({ liveMs: 1000 })
    const last = (await source.history(btc, '1m')).at(-1)!
    const seen: [string, number, Candle][] = []
    const start = Date.now()
    const stop = source.candles(btc, '1m', {
      onClosed: (c) => seen.push(['closed', Date.now() - start, c]),
      onForming: (c) => seen.push(['forming', Date.now() - start, c]),
    })
    await vi.advanceTimersByTimeAsync(1000)
    expect(seen.map(([kind, at]) => [kind, at])).toEqual([
      ['forming', 750],
      ['forming', 900],
      ['closed', 1000],
    ])
    const [first, second, closed] = seen.map(([, , c]) => c) as [Candle, Candle, Candle]
    for (const c of [first, second]) {
      expect(c.open_time).toBe(closed.open_time)
      expect(c.open).toBe(last.close)
      expect(c.high).toBeLessThanOrEqual(closed.high)
      expect(c.low).toBeGreaterThanOrEqual(closed.low)
      expect(c.volume).toBeLessThan(closed.volume)
    }
    expect(second).not.toEqual(first)
    expect(closed.open_time).toBe(last.open_time + 60_000)
    expect((await source.history(btc, '1m')).at(-1)).toEqual(closed)

    stop()
    await vi.advanceTimersByTimeAsync(3000)
    expect(seen).toHaveLength(3)
  })
})

describe('mock paper session', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  async function manualSession() {
    const source = createMockTradingSource({ liveMs: 100 })
    const started = await source.startSession({
      strategy: MANUAL,
      params: {},
      instrument: btc,
      timeframe: '1m',
      starting_cash: 10_000,
      slippage_bps: 0,
    })
    let state: SessionState | null = null
    const events: SessionEvent[] = []
    source.watchSession(started.id, { onState: (s) => (state = s), onEvent: (e) => events.push(e) })
    return { source, id: started.id, events, state: () => state }
  }

  it('fills a manual market order at the next bar open', async () => {
    const { source, id, events } = await manualSession()
    await source.placeOrder(id, { side: 'buy', qty: 0.01, order_type: { type: 'market' } })
    await vi.advanceTimersByTimeAsync(100)
    const fill = events.find((e) => e.event === 'fill')
    const bar = events.find((e) => e.event === 'bar')
    expect(fill?.event === 'fill' && bar?.event === 'bar' && fill.fill.price).toBe(bar?.event === 'bar' && bar.bar.open)
    const state = await source.session(id)
    expect(state.position.qty).toBeCloseTo(0.01, 8)
    expect(state.open_orders).toEqual([])
    expect(state.fills).toHaveLength(1)
  })

  it('rejects selling more than a spot session holds, with a reason', async () => {
    const { source, id, events } = await manualSession()
    await source.placeOrder(id, { side: 'sell', qty: 0.01, order_type: { type: 'market' } })
    const order = events.find((e) => e.event === 'order')
    expect(order?.event === 'order' && order.order.status).toMatchObject({ status: 'rejected' })
  })

  it('realises P&L on a round trip and goes flat', async () => {
    const { source, id } = await manualSession()
    await source.placeOrder(id, { side: 'buy', qty: 0.01, order_type: { type: 'market' } })
    await vi.advanceTimersByTimeAsync(100)
    await source.placeOrder(id, { side: 'sell', qty: 0.01, order_type: { type: 'market' } })
    await vi.advanceTimersByTimeAsync(100)
    const state = await source.session(id)
    expect(state.position.qty).toBe(0)
    expect(state.trades).toHaveLength(1)
    expect(state.trades[0]?.open).toBe(false)
    const [buy, sell] = state.fills
    expect(state.position.realised_pnl).toBeCloseTo(0.01 * (sell!.price - buy!.price), 8)
  })

  it('stops and resumes', async () => {
    const { source, id } = await manualSession()
    expect((await source.stopSession(id)).status).toEqual({ status: 'stopped' })
    await expect(source.stopSession(id)).rejects.toThrow('not running')
    expect((await source.resumeSession(id)).status.status).not.toBe('stopped')
  })
})
