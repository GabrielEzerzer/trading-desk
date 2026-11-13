import { describe, expect, it } from 'vitest'
import type { ApiRequest, ApiResult } from './bridge'
import { ApiError, createClient } from './client'

const btc = { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' } as const

describe('createClient', () => {
  it('builds the request for each route and returns the body', async () => {
    const sent: ApiRequest[] = []
    const client = createClient(async (request) => {
      sent.push(request)
      return { ok: true, status: 200, body: { version: '0.1.0' } }
    })
    expect(await client.health()).toEqual({ version: '0.1.0' })
    await client.coverage({ venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' })
    await client.cancelJob(7)
    await client.session('1762790400000-0a1b2c3d')
    await client.stopSession('a')
    await client.resumeSession('a')
    await client.placeOrder('a', { side: 'buy', qty: 0.5, type: 'limit', limit_price: 100 })
    await client.cancelOrder('a', 3)
    await client.stored()
    await client.deleteData({ instrument: btc, from: '2024-01', to: '2024-03' })
    await client.deleteAllData()
    expect(sent).toEqual([
      { method: 'GET', path: '/health' },
      { method: 'GET', path: '/data/coverage', query: { instrument: 'binance:BTCUSDT:spot' } },
      { method: 'POST', path: '/jobs/7/cancel' },
      { method: 'GET', path: '/sessions/1762790400000-0a1b2c3d' },
      { method: 'POST', path: '/sessions/a/stop' },
      { method: 'POST', path: '/sessions/a/resume' },
      { method: 'POST', path: '/sessions/a/orders', body: { side: 'buy', qty: 0.5, type: 'limit', limit_price: 100 } },
      { method: 'POST', path: '/sessions/a/orders/3/cancel' },
      { method: 'GET', path: '/data/stored' },
      { method: 'POST', path: '/data/delete', body: { instrument: btc, from: '2024-01', to: '2024-03' } },
      { method: 'POST', path: '/data/delete-all' },
    ])
  })

  it("throws the engine's error as an ApiError with its code and status", async () => {
    const failed: ApiResult = {
      ok: false,
      status: 404,
      error: { code: 'not_found', message: 'no job 9' },
    }
    const client = createClient(async () => failed)
    const error = await client.job(9).catch((e: unknown) => e)
    expect(error).toBeInstanceOf(ApiError)
    expect(error).toMatchObject({ status: 404, code: 'not_found', message: 'no job 9' })
  })
})
