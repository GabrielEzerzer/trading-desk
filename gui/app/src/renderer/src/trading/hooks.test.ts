import type { Candle, InstrumentId, Timeframe } from '@td/shared'
import { describe, expect, it, vi } from 'vitest'
import { type CandleView, followCandles } from './hooks'
import type { CandleFeed } from './source'

const btc: InstrumentId = { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' }
const bar = (open_time: number, close = 100): Candle => ({
  open_time,
  open: close,
  high: close,
  low: close,
  close,
  volume: 1,
  quote_volume: close,
})

/** A source whose history answers only when the test says, and whose feed the test drives. */
function fakeSource() {
  const pending: { resolve(c: Candle[]): void; reject(e: unknown): void }[] = []
  const feeds: CandleFeed[] = []
  const stop = vi.fn()
  const source = {
    history: vi.fn(
      (_id: InstrumentId, _tf: Timeframe) => new Promise<Candle[]>((resolve, reject) => pending.push({ resolve, reject })),
    ),
    candles: vi.fn((_id: InstrumentId, _tf: Timeframe, feed: CandleFeed) => {
      feeds.push(feed)
      return stop
    }),
  }
  return { source, pending, stop, feed: () => feeds.at(-1)! }
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0))

describe('followCandles', () => {
  it('shows live bars while the history loads, then both', async () => {
    const { source, pending, feed } = fakeSource()
    const views: CandleView[] = []
    followCandles(source, btc, '1m', (v) => views.push(v))
    expect(views.at(-1)).toMatchObject({ loading: true, error: null, candles: [] })

    feed().onForming(bar(180, 1))
    expect(views.at(-1)).toMatchObject({ loading: true, candles: [bar(180, 1)] })

    pending[0]?.resolve([bar(60), bar(120)])
    await settle()
    expect(views.at(-1)).toMatchObject({ loading: false, error: null, candles: [bar(60), bar(120), bar(180, 1)] })
  })

  it('reports a failed history in its words, keeps the live bars, and loads again on retry', async () => {
    const { source, pending, feed } = fakeSource()
    const views: CandleView[] = []
    const follow = followCandles(source, btc, '5m', (v) => views.push(v))
    pending[0]?.reject(new Error('fetching binance:BTCUSDT:spot 5m bars from the venue failed: timed out'))
    await settle()
    expect(views.at(-1)).toMatchObject({
      loading: false,
      error: 'Fetching binance:BTCUSDT:spot 5m bars from the venue failed: timed out.',
    })
    feed().onClosed(bar(300))
    expect(views.at(-1)?.candles).toEqual([bar(300)])

    follow.retry()
    expect(views.at(-1)).toMatchObject({ loading: true, error: null, candles: [bar(300)] })
    // A second retry while it loads asks nothing more.
    follow.retry()
    expect(source.history).toHaveBeenCalledTimes(2)
    pending[1]?.resolve([bar(0)])
    await settle()
    expect(views.at(-1)).toMatchObject({ loading: false, error: null, candles: [bar(0), bar(300)] })
  })

  it('drops a history answer and live bars that arrive after stop', async () => {
    const { source, pending, stop, feed } = fakeSource()
    const onChange = vi.fn()
    const follow = followCandles(source, btc, '1m', onChange)
    const before = onChange.mock.calls.length
    follow.stop()
    expect(stop).toHaveBeenCalledTimes(1)
    pending[0]?.resolve([bar(60)])
    await settle()
    feed().onClosed(bar(120))
    feed().onForming(bar(180))
    expect(onChange).toHaveBeenCalledTimes(before)
    follow.retry()
    expect(source.history).toHaveBeenCalledTimes(1)
  })
})
