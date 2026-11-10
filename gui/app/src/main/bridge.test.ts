import { describe, expect, it } from 'vitest'
import { RefusedRequest, validateRequest, validateTopic } from './bridge'

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

describe('validateTopic', () => {
  it('passes the jobs topic and refuses anything else', () => {
    expect(validateTopic('jobs')).toBe('jobs')
    for (const topic of ['candles:binance:BTCUSDT:spot:1m', 'jobs ', '', 7, null]) {
      expect(() => validateTopic(topic)).toThrow(RefusedRequest)
    }
  })
})
