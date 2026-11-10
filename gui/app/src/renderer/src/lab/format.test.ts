import { describe, expect, it } from 'vitest'
import { formatDay, formatInstrument, formatLimit, formatNumber, formatPrice, formatReturnPct, pnlTone } from './format'
import { readableName } from './source'
import { sentence } from '../engine'

describe('format', () => {
  it('writes percents, ratios and counts with a typographic minus and optional sign', () => {
    expect(formatNumber(0.2466, 'percent')).toBe('24.7%')
    expect(formatNumber(2.4657, 'percent', { sign: true })).toBe('+246.6%')
    expect(formatNumber(-0.05, 'percent')).toBe('−5.0%')
    expect(formatNumber(1.2, 'ratio')).toBe('1.20')
    expect(formatNumber(1234, 'count')).toBe('1,234')
    expect(formatNumber(0, 'percent', { sign: true })).toBe('0.0%')
  })

  it('writes a figure that rounds to zero with no sign', () => {
    expect(formatNumber(-0.00004, 'percent')).toBe('0.0%')
    expect(formatNumber(0.00004, 'percent', { sign: true })).toBe('0.0%')
    expect(formatNumber(-0.004, 'ratio')).toBe('0.00')
    expect(formatNumber(-0.4, 'count')).toBe('0')
    expect(formatNumber(-0, 'percent', { sign: true })).toBe('0.0%')
    expect(formatReturnPct(-0.004)).toBe('0.00%')
    expect(formatReturnPct(0.004)).toBe('0.00%')
    // Just past the rounding edge the sign comes back.
    expect(formatNumber(-0.0005, 'percent')).toBe('−0.1%')
    expect(formatReturnPct(-0.005)).toBe('−0.01%')
  })

  it('writes trade returns, prices, days, instruments and limits', () => {
    expect(formatReturnPct(3.2)).toBe('+3.20%')
    expect(formatReturnPct(-1.5)).toBe('−1.50%')
    expect(formatPrice(61234.5)).toBe('61,234.50')
    expect(formatPrice(0.51234)).toBe('0.5123')
    expect(formatDay(Date.UTC(2024, 2, 2, 23, 59))).toBe('2 Mar 2024')
    expect(formatDay(Date.UTC(2021, 8, 7))).toBe('7 Sep 2021')
    expect(formatInstrument({ venue: 'binance', symbol: 'BTCUSDT', kind: 'perpetual' })).toBe('BTCUSDT perpetual, Binance')
    expect(formatLimit({ label: 'x', value: 0.2, limit: 0.25, rule: 'at_most', format: 'percent', pass: true })).toBe('at most 25.0%')
  })

  it('tones P&L by the sign of the figure as shown', () => {
    expect(pnlTone(0.1, 'percent')).toBe('gain')
    expect(pnlTone(-0.1, 'percent')).toBe('loss')
    expect(pnlTone(0, 'percent')).toBeUndefined()
    expect(pnlTone(-0.00004, 'percent')).toBeUndefined()
    expect(pnlTone(0.00004, 'percent')).toBeUndefined()
    expect(pnlTone(-0.004, 'return_pct')).toBeUndefined()
    expect(pnlTone(-0.02, 'return_pct')).toBe('loss')
  })

  it('turns engine messages into sentences without capitalising program names', () => {
    expect(sentence('the engine stopped (exit code 1)')).toBe('The engine stopped (exit code 1).')
    expect(sentence('td-server exited before its handshake')).toBe('td-server exited before its handshake.')
    expect(sentence('Done.')).toBe('Done.')
  })

  it('makes engine ids readable', () => {
    expect(readableName('sma_cross')).toBe('SMA cross')
    expect(readableName('donchian_breakout')).toBe('Donchian breakout')
  })
})
