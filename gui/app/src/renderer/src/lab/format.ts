// How the Lab writes numbers and dates. Numbers render in tabular figures (styles.css), so these
// only decide the text. A typographic minus keeps negative numbers the same width as positive ones.

import type { InstrumentId } from '@td/shared'
import type { Criterion, NumberFormat } from './source'

const MINUS = '−'

function signed(text: string, value: number, sign: boolean): string {
  if (value < 0) return MINUS + text
  return sign && value > 0 ? `+${text}` : text
}

/** `0.2466` as percent is "24.7%"; ratios get two decimals; counts are whole numbers. */
export function formatNumber(value: number, format: NumberFormat, options: { sign?: boolean } = {}): string {
  const sign = options.sign ?? false
  const abs = Math.abs(value)
  switch (format) {
    case 'percent':
      return signed(`${(abs * 100).toLocaleString('en-US', { minimumFractionDigits: 1, maximumFractionDigits: 1 })}%`, value, sign)
    case 'ratio':
      return signed(abs.toFixed(2), value, sign)
    case 'count':
      return signed(Math.round(abs).toLocaleString('en-US'), value, sign)
  }
}

/** A percent that is already in percent units, as trade returns are: `3.2` is "+3.20%". */
export function formatReturnPct(value: number): string {
  return signed(`${Math.abs(value).toFixed(2)}%`, value, true)
}

/** Prices with two decimals, or four below 1: 61,234.50 but 0.5123. */
export function formatPrice(value: number): string {
  const digits = value >= 1 ? 2 : 4
  return value.toLocaleString('en-US', { minimumFractionDigits: digits, maximumFractionDigits: digits })
}

// Fixed three-letter months: locale data varies ("Sept" in some), and table columns should not.
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

/** "2 Mar 2024", in UTC like every timestamp in the desk. */
export function formatDay(ms: number): string {
  const d = new Date(ms)
  return `${d.getUTCDate()} ${MONTHS[d.getUTCMonth()]} ${d.getUTCFullYear()}`
}

/** "Mar 2024". */
export function formatMonth(ms: number): string {
  const d = new Date(ms)
  return `${MONTHS[d.getUTCMonth()]} ${d.getUTCFullYear()}`
}

const VENUES: Record<InstrumentId['venue'], string> = { binance: 'Binance', bybit: 'Bybit' }

/** "BTCUSDT perpetual, Binance". */
export function formatInstrument(id: InstrumentId): string {
  return `${id.symbol} ${id.kind}, ${VENUES[id.venue]}`
}

/** "at least 1.00" or "at most 25.0%". */
export function formatLimit(criterion: Criterion): string {
  const word = criterion.rule === 'at_least' ? 'at least' : 'at most'
  return `${word} ${formatNumber(criterion.limit, criterion.format)}`
}

/** The colour role of a P&L number: gain above zero, loss below, ink at zero. */
export function pnlTone(value: number): 'gain' | 'loss' | undefined {
  if (value > 0) return 'gain'
  if (value < 0) return 'loss'
  return undefined
}
