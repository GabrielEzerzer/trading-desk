import { type Client, createClient, type InstrumentId } from '@td/shared'
import { describe, expect, it } from 'vitest'
import { RefusedRequest, validateRequest, validateTopic } from './bridge'

const btc: InstrumentId = { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' }
const sessionId = '1762790400000-0a1b2c3d'

// Arguments for every client method. The type makes a method added to the client fail typecheck
// until it has a sample here, so the test below sends it through main's checks too.
const SAMPLES: { [K in keyof Client]: Parameters<Client[K]> } = {
  health: [],
  instruments: ['bybit', 'perpetual'],
  downloadData: [{ instrument: btc, from: 1, to: 2 }],
  coverage: [btc],
  stored: [],
  deleteData: [{ instrument: btc, from: '2024-01', to: '2024-02' }],
  deleteAllData: [],
  candles: [btc, '5m', 500],
  strategies: [],
  jobs: [],
  job: [12],
  cancelJob: [12],
  jobResult: [12],
  startBacktest: [{} as Parameters<Client['startBacktest']>[0]],
  runs: [],
  run: ['1794242520000-0a1b2c3d'],
  sessions: [],
  session: [sessionId],
  startSession: [{} as Parameters<Client['startSession']>[0]],
  stopSession: [sessionId],
  resumeSession: [sessionId],
  placeOrder: [sessionId, {} as Parameters<Client['placeOrder']>[1]],
  cancelOrder: [sessionId, 3],
}

describe('the client and main agree', () => {
  it('passes every request the client makes', async () => {
    const client = createClient(async (request) => {
      validateRequest(request)
      return { ok: true, status: 200, body: {} }
    })
    for (const [name, method] of Object.entries(client)) {
      const args = (SAMPLES as Record<string, unknown[] | undefined>)[name]
      expect(args, `a sample for ${name}`).toBeDefined()
      await expect((method as (...a: unknown[]) => Promise<unknown>)(...args!), name).resolves.toBeDefined()
    }
  })
})

describe('validateRequest', () => {
  it('passes the routes the app may call, as clean copies', () => {
    expect(validateRequest({ method: 'GET', path: '/health' })).toEqual({ method: 'GET', path: '/health' })
    expect(validateRequest({ method: 'GET', path: '/jobs/12' })).toEqual({ method: 'GET', path: '/jobs/12' })
    expect(validateRequest({ method: 'POST', path: '/jobs/12/cancel' })).toEqual({
      method: 'POST',
      path: '/jobs/12/cancel',
    })
    expect(
      validateRequest({ method: 'GET', path: '/instruments', query: { venue: 'binance', kind: 'spot' } }),
    ).toEqual({ method: 'GET', path: '/instruments', query: { venue: 'binance', kind: 'spot' } })
    const body = { instrument: { venue: 'bybit', symbol: 'BTCUSDT', kind: 'perpetual' }, from: 1, to: 2 }
    expect(validateRequest({ method: 'POST', path: '/data/download', body })).toEqual({
      method: 'POST',
      path: '/data/download',
      body,
    })
  })

  it('passes the candle history the chart asks for', () => {
    const query = { instrument: 'binance:BTCUSDT:spot', timeframe: '5m', limit: '500' }
    expect(validateRequest({ method: 'GET', path: '/candles', query })).toEqual({ method: 'GET', path: '/candles', query })
  })

  it('passes the run routes the Lab uses', () => {
    const id = '1794242520000-0a1b2c3d'
    expect(validateRequest({ method: 'GET', path: '/runs' })).toEqual({ method: 'GET', path: '/runs' })
    expect(validateRequest({ method: 'GET', path: `/runs/${id}` })).toEqual({ method: 'GET', path: `/runs/${id}` })
    expect(validateRequest({ method: 'GET', path: '/jobs/7/result' })).toEqual({ method: 'GET', path: '/jobs/7/result' })
    const config = { strategy: 'sma_cross', params: {}, from: 1, to: 2 }
    expect(validateRequest({ method: 'POST', path: '/runs/backtest', body: config })).toEqual({
      method: 'POST',
      path: '/runs/backtest',
      body: config,
    })
  })

  it.each([
    ['a run id without its hash', { method: 'GET', path: '/runs/1794242520000' }],
    ['a run id with a short time', { method: 'GET', path: '/runs/179424252000-0a1b2c3d' }],
    ['a run id with upper-case hex', { method: 'GET', path: '/runs/1794242520000-0A1B2C3D' }],
    ['a run id with a long hash', { method: 'GET', path: '/runs/1794242520000-0a1b2c3d9' }],
    ['a run id that climbs out', { method: 'GET', path: '/runs/../health' }],
    ['a trailing slash on the run list', { method: 'GET', path: '/runs/' }],
    ['POST on the run list', { method: 'POST', path: '/runs', body: {} }],
    ['GET on the backtest route', { method: 'GET', path: '/runs/backtest' }],
    ['the backtest route without a body', { method: 'POST', path: '/runs/backtest' }],
    ['a run route the Lab does not use', { method: 'POST', path: '/runs/sweep', body: {} }],
    ['POST on a job result', { method: 'POST', path: '/jobs/7/result' }],
    ['a job result with a bad id', { method: 'GET', path: '/jobs/x/result' }],
    ['a job result with a suffix', { method: 'GET', path: '/jobs/7/results' }],
  ])('refuses %s', (_name, input) => {
    expect(() => validateRequest(input)).toThrow(RefusedRequest)
  })

  it.each([
    ['an unknown path', { method: 'GET', path: '/admin' }],
    ['a path that climbs out', { method: 'GET', path: '/jobs/../health' }],
    ['a query string in the path', { method: 'GET', path: '/health?x=1' }],
    ['a full URL', { method: 'GET', path: 'http://example.com/health' }],
    ['a job id that is not a number', { method: 'GET', path: '/jobs/abc' }],
    ['the WebSocket path', { method: 'GET', path: '/ws' }],
    ['an unknown method', { method: 'DELETE', path: '/jobs/1' }],
    ['the wrong method for a route', { method: 'POST', path: '/health' }],
    ['GET on a POST route', { method: 'GET', path: '/data/download' }],
    ['an unknown field', { method: 'GET', path: '/health', headers: { authorization: 'x' } }],
    [
      'an unknown query parameter',
      { method: 'GET', path: '/instruments', query: { venue: 'binance', kind: 'spot', x: '1' } },
    ],
    ['a missing query parameter', { method: 'GET', path: '/instruments', query: { venue: 'binance' } }],
    ['a query value that is not a string', { method: 'GET', path: '/data/coverage', query: { instrument: 5 } }],
    ['a body on a route that takes none', { method: 'GET', path: '/health', body: {} }],
    ['a POST route without a body', { method: 'POST', path: '/data/download' }],
    ['an array body', { method: 'POST', path: '/data/download', body: [] }],
    ['a delete without a body', { method: 'POST', path: '/data/delete' }],
    ['a body on delete-all', { method: 'POST', path: '/data/delete-all', body: {} }],
    ['GET on delete-all', { method: 'GET', path: '/data/delete-all' }],
    ['a request that is not an object', 'GET /health'],
    ['null', null],
  ])('refuses %s', (_name, input) => {
    expect(() => validateRequest(input)).toThrow(RefusedRequest)
  })

  it('refuses an oversized body', () => {
    const body = { instrument: 'x'.repeat(70 * 1024) }
    expect(() => validateRequest({ method: 'POST', path: '/data/download', body })).toThrow(/too large/)
  })
})

describe('stored data routes', () => {
  it('passes the listing, a delete with its body and delete-all', () => {
    expect(validateRequest({ method: 'GET', path: '/data/stored' })).toEqual({ method: 'GET', path: '/data/stored' })
    const body = { instrument: { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' }, from: '2024-01', to: '2024-02' }
    expect(validateRequest({ method: 'POST', path: '/data/delete', body })).toEqual({
      method: 'POST',
      path: '/data/delete',
      body,
    })
    expect(validateRequest({ method: 'POST', path: '/data/delete-all' })).toEqual({
      method: 'POST',
      path: '/data/delete-all',
    })
  })
})

describe('paper session routes', () => {
  const start = {
    strategy: 'sma_cross',
    params: {},
    instrument: { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' },
    timeframe: '1h',
    starting_cash: 10_000,
    slippage_bps: 2,
  }

  it('passes the session routes', () => {
    expect(validateRequest({ method: 'GET', path: '/sessions' })).toEqual({ method: 'GET', path: '/sessions' })
    expect(validateRequest({ method: 'POST', path: '/sessions', body: start })).toEqual({
      method: 'POST',
      path: '/sessions',
      body: start,
    })
    for (const path of ['/sessions/1762790400000-0a1b2c3d', '/sessions/my_session']) {
      expect(validateRequest({ method: 'GET', path })).toEqual({ method: 'GET', path })
    }
    expect(validateRequest({ method: 'POST', path: '/sessions/1762790400000-0a1b2c3d/stop' })).toEqual({
      method: 'POST',
      path: '/sessions/1762790400000-0a1b2c3d/stop',
    })
    expect(validateRequest({ method: 'POST', path: '/sessions/abc/resume' })).toEqual({
      method: 'POST',
      path: '/sessions/abc/resume',
    })
    const order = { side: 'buy', qty: 0.5, type: 'market' }
    expect(validateRequest({ method: 'POST', path: '/sessions/abc/orders', body: order })).toEqual({
      method: 'POST',
      path: '/sessions/abc/orders',
      body: order,
    })
    expect(validateRequest({ method: 'POST', path: '/sessions/abc/orders/12/cancel' })).toEqual({
      method: 'POST',
      path: '/sessions/abc/orders/12/cancel',
    })
  })

  it.each([
    ['a session id with a dot', { method: 'GET', path: '/sessions/a.b' }],
    ['a session id that climbs out', { method: 'GET', path: '/sessions/../health' }],
    ['an encoded slash in the id', { method: 'GET', path: '/sessions/a%2Fb' }],
    ['a session id over 64 characters', { method: 'GET', path: `/sessions/${'a'.repeat(65)}` }],
    ['an empty session id', { method: 'GET', path: '/sessions/' }],
    ['an unknown session action', { method: 'POST', path: '/sessions/abc/delete' }],
    ['GET on stop', { method: 'GET', path: '/sessions/abc/stop' }],
    ['a body on stop', { method: 'POST', path: '/sessions/abc/stop', body: {} }],
    ['a body on resume', { method: 'POST', path: '/sessions/abc/resume', body: { force: true } }],
    ['a start without a body', { method: 'POST', path: '/sessions' }],
    ['a start with an array body', { method: 'POST', path: '/sessions', body: [start] }],
    ['a query on the session list', { method: 'GET', path: '/sessions', query: { status: 'live' } }],
    ['an order without a body', { method: 'POST', path: '/sessions/abc/orders' }],
    ['GET on the orders', { method: 'GET', path: '/sessions/abc/orders' }],
    ['a cancel of an order id that is not a number', { method: 'POST', path: '/sessions/abc/orders/x/cancel' }],
    ['a body on cancel', { method: 'POST', path: '/sessions/abc/orders/1/cancel', body: {} }],
    ['candle history without its query', { method: 'GET', path: '/candles' }],
    [
      'candle history with a range',
      { method: 'GET', path: '/candles', query: { instrument: 'binance:BTCUSDT:spot', timeframe: '1m', limit: '500', from: '1' } },
    ],
  ])('refuses %s', (_name, input) => {
    expect(() => validateRequest(input)).toThrow(RefusedRequest)
  })
})

describe('validateTopic', () => {
  it('passes the jobs, candles and session topics', () => {
    for (const topic of [
      'jobs',
      'candles:binance:BTCUSDT:spot:1m',
      'candles:bybit:1000PEPEUSDT:perpetual:4h',
      'candles:binance:ETHUSDT:spot:1w',
      'session:1762790400000-0a1b2c3d',
      'session:my_session',
    ]) {
      expect(validateTopic(topic)).toBe(topic)
    }
  })

  it.each([
    ['jobs with a space', 'jobs '],
    ['an empty topic', ''],
    ['a number', 7],
    ['null', null],
    ['an unknown venue', 'candles:kraken:BTCUSDT:spot:1m'],
    ['a lowercase symbol', 'candles:binance:btcusdt:spot:1m'],
    ['a symbol with a path in it', 'candles:binance:../x:spot:1m'],
    ['a symbol with a colon', 'candles:binance:BTC:USDT:spot:1m'],
    ['an unknown kind', 'candles:binance:BTCUSDT:future:1m'],
    ['an unknown timeframe', 'candles:binance:BTCUSDT:spot:2m'],
    ['a missing timeframe', 'candles:binance:BTCUSDT:spot'],
    ['a symbol over 32 characters', `candles:binance:${'A'.repeat(33)}:spot:1m`],
    ['a trailing newline', 'candles:binance:BTCUSDT:spot:1m\n'],
    ['an empty session id', 'session:'],
    ['a session id with a colon', 'session:a:b'],
    ['a session id over 64 characters', `session:${'a'.repeat(65)}`],
    ['an unknown topic', 'sessions'],
  ])('refuses %s', (_name, topic) => {
    expect(() => validateTopic(topic)).toThrow(RefusedRequest)
  })
})
