// Checks a manual order against the instrument's trading rules before it is sent, so the composer
// can say why it cannot be placed. The engine rounds and validates again at submission
// (Instrument::round_qty, round_price and validate_order); this check never rounds silently, it
// names the rule instead.

import type { Instrument } from '@td/shared'

export type OrderKind = 'market' | 'limit'

export interface OrderDraft {
  /** As typed. */
  qty: string
  kind: OrderKind
  /** As typed; only read for a limit order. */
  price: string
}

export type OrderCheck =
  | { ok: true; qty: number; price: number | null; notional: number }
  | { ok: false; reason: string }

// Floating-point slack when checking that a value sits on a step: 0.3 / 0.1 is 2.9999999999999996.
const EPSILON = 1e-9

/** How many decimals a step needs: 0.001 needs 3, 0.5 needs 1, 1 needs 0. */
export function stepDecimals(step: number): number {
  for (let d = 0; d <= 12; d++) {
    const scaled = step * 10 ** d
    if (Math.abs(scaled - Math.round(scaled)) < EPSILON * Math.max(1, scaled)) return d
  }
  return 12
}

/** Whether `value` is a whole number of `step`s. */
export function onStep(value: number, step: number): boolean {
  if (step <= 0) return true
  const steps = value / step
  return Math.abs(steps - Math.round(steps)) < EPSILON * Math.max(1, Math.abs(steps))
}

/** A number in a step's decimals with thousands separators: 0.001 BTC, 64,210.50. */
export function formatStep(value: number, step: number): string {
  const digits = stepDecimals(step)
  return value.toLocaleString('en-US', { minimumFractionDigits: digits, maximumFractionDigits: digits })
}

/** Reads what was typed: digits with one optional decimal point. Commas are thousands separators. */
export function parseAmount(text: string): number | null {
  const clean = text.replace(/,/g, '').trim()
  if (!/^(\d+\.?\d*|\.\d+)$/.test(clean)) return null
  const value = Number(clean)
  return Number.isFinite(value) ? value : null
}

/**
 * Checks size, price and order value. A market order's value is estimated at `lastPrice`, the
 * latest close; with no price yet its value cannot be checked.
 */
export function checkOrder(instrument: Instrument, draft: OrderDraft, lastPrice: number | null): OrderCheck {
  const { base, quote, lot_size, min_qty, tick_size, min_notional } = instrument
  if (draft.qty.trim() === '') return { ok: false, reason: 'Enter a size.' }
  const qty = parseAmount(draft.qty)
  if (qty === null) return { ok: false, reason: 'Enter the size as a number, for example 0.5.' }
  if (qty <= 0) return { ok: false, reason: 'Enter a size above zero.' }
  if (qty < min_qty - EPSILON * min_qty) {
    return { ok: false, reason: `The size must be at least ${formatStep(min_qty, lot_size)} ${base}.` }
  }
  if (!onStep(qty, lot_size)) {
    return { ok: false, reason: `The size must be a multiple of ${formatStep(lot_size, lot_size)} ${base}.` }
  }

  let price: number | null = null
  if (draft.kind === 'limit') {
    if (draft.price.trim() === '') return { ok: false, reason: 'Enter a limit price.' }
    price = parseAmount(draft.price)
    if (price === null) return { ok: false, reason: 'Enter the limit price as a number.' }
    if (price <= 0) return { ok: false, reason: 'Enter a limit price above zero.' }
    if (!onStep(price, tick_size)) {
      return { ok: false, reason: `The price must be a multiple of ${formatStep(tick_size, tick_size)} ${quote}.` }
    }
  }

  const valuePrice = price ?? lastPrice
  if (valuePrice === null) return { ok: false, reason: 'Waiting for a price to check the order value.' }
  const notional = qty * valuePrice
  if (notional < min_notional - EPSILON * min_notional) {
    const value = formatStep(notional, 0.01)
    return {
      ok: false,
      reason: `The order value is ${value} ${quote}; the venue's minimum is ${formatStep(min_notional, 0.01)} ${quote}.`,
    }
  }
  return { ok: true, qty, price, notional }
}
