import { describe, expect, it } from 'vitest'
import { money, moneyTone } from './PositionLine'

describe('money and its tone', () => {
  it('signs and tones amounts that show as non-zero', () => {
    expect(money(8.1, true)).toBe('+8.10')
    expect(moneyTone(8.1)).toBe('gain')
    expect(money(-2.67, true)).toBe('−2.67')
    expect(moneyTone(-2.67)).toBe('loss')
  })

  it('keeps an amount that rounds to zero unsigned and in ink', () => {
    expect(money(0.004, true)).toBe('0.00')
    expect(moneyTone(0.004)).toBeUndefined()
    expect(money(-0.004, true)).toBe('0.00')
    expect(moneyTone(-0.004)).toBeUndefined()
  })
})
