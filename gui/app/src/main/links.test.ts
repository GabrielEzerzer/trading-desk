import { describe, expect, it } from 'vitest'
import { TRADINGVIEW_URL, isExternalLink } from './links'

describe('isExternalLink', () => {
  it('opens only the exact attribution link', () => {
    expect(isExternalLink(TRADINGVIEW_URL)).toBe(true)
    for (const url of [
      'https://www.tradingview.com',
      'https://www.tradingview.com/?x=1',
      'https://www.tradingview.com/chart/',
      'http://www.tradingview.com/',
      'https://www.tradingview.com.example.com/',
      'file:///C:/Windows/notepad.exe',
      '',
    ]) {
      expect(isExternalLink(url)).toBe(false)
    }
  })
})
