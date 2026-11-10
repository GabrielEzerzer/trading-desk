import { describe, expect, it } from 'vitest'
import type { ApiRequest, ApiResult } from './bridge'
import { ApiError, createClient } from './client'

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
    expect(sent).toEqual([
      { method: 'GET', path: '/health' },
      { method: 'GET', path: '/data/coverage', query: { instrument: 'binance:BTCUSDT:spot' } },
      { method: 'POST', path: '/jobs/7/cancel' },
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
