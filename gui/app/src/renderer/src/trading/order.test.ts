import type { Instrument } from '@td/shared'
import { describe, expect, it } from 'vitest'
import { checkOrder, formatStep, onStep, parseAmount, stepDecimals } from './order'

const btc: Instrument = {
  id: { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' },
  base: 'BTC',
  quote: 'USDT',
  tick_size: 0.01,
  lot_size: 0.00001,
  min_qty: 0.00001,
  min_notional: 5,
  fees: { maker: 0.001, taker: 0.001 },
}

const perp: Instrument = {
  ...btc,
  id: { venue: 'bybit', symbol: 'ETHUSDT', kind: 'perpetual' },
  base: 'ETH',
  tick_size: 0.05,
  lot_size: 0.01,
  min_qty: 0.01,
  min_notional: 5,
}

const market = (qty: string) => ({ qty, kind: 'market' as const, price: '' })
const limit = (qty: string, price: string) => ({ qty, kind: 'limit' as const, price })

describe('steps', () => {
  it('counts the decimals a step needs', () => {
    expect(stepDecimals(1)).toBe(0)
    expect(stepDecimals(0.5)).toBe(1)
    expect(stepDecimals(0.25)).toBe(2)
    expect(stepDecimals(0.001)).toBe(3)
    expect(stepDecimals(1e-8)).toBe(8)
    expect(stepDecimals(10)).toBe(0)
  })

  it('accepts values on the step despite floating-point error', () => {
    expect(onStep(0.3, 0.1)).toBe(true)
    expect(onStep(1.23, 0.01)).toBe(true)
    expect(onStep(64210.55, 0.05)).toBe(true)
    expect(onStep(0.00123, 0.00001)).toBe(true)
    expect(onStep(0.015, 0.01)).toBe(false)
    expect(onStep(64210.53, 0.05)).toBe(false)
  })

  it('formats in the step decimals', () => {
    expect(formatStep(0.001, 0.001)).toBe('0.001')
    expect(formatStep(64210.5, 0.01)).toBe('64,210.50')
  })

  it('reads typed amounts', () => {
    expect(parseAmount('0.5')).toBe(0.5)
    expect(parseAmount('.5')).toBe(0.5)
    expect(parseAmount('1,000.25')).toBe(1000.25)
    expect(parseAmount('2.')).toBe(2)
    expect(parseAmount('-1')).toBeNull()
    expect(parseAmount('1e3')).toBeNull()
    expect(parseAmount('abc')).toBeNull()
    expect(parseAmount('1.2.3')).toBeNull()
  })
})

describe('checkOrder', () => {
  it('passes a market order with enough value at the last price', () => {
    expect(checkOrder(btc, market('0.001'), 64_000)).toEqual({ ok: true, qty: 0.001, price: null, notional: 64 })
  })

  it('passes a limit order and values it at the limit price', () => {
    const check = checkOrder(perp, limit('0.05', '3000.05'), null)
    expect(check).toMatchObject({ ok: true, qty: 0.05, price: 3000.05 })
  })

  it.each([
    ['an empty size', market(''), 'Enter a size.'],
    ['a size that is not a number', market('abc'), 'Enter the size as a number, for example 0.5.'],
    ['a zero size', market('0'), 'Enter a size above zero.'],
    ['a size under the minimum', market('0.001'), 'The size must be at least 0.01 ETH.'],
    ['a size off the lot step', market('0.015'), 'The size must be a multiple of 0.01 ETH.'],
    ['a missing limit price', limit('1', ''), 'Enter a limit price.'],
    ['a limit price off the tick', limit('1', '3000.03'), 'The price must be a multiple of 0.05 USDT.'],
    ['a zero limit price', limit('1', '0'), 'Enter a limit price above zero.'],
    ['an order under the minimum value', limit('0.01', '100'), "The order value is 1.00 USDT; the venue's minimum is 5.00 USDT."],
  ])('refuses %s with the reason', (_name, draft, reason) => {
    expect(checkOrder(perp, draft, 3000)).toEqual({ ok: false, reason })
  })

  it('waits for a price before valuing a market order', () => {
    expect(checkOrder(btc, market('0.001'), null)).toEqual({
      ok: false,
      reason: 'Waiting for a price to check the order value.',
    })
  })

  it('accepts exactly the minimum value and size', () => {
    expect(checkOrder(perp, market('0.01'), 500).ok).toBe(true)
  })
})
