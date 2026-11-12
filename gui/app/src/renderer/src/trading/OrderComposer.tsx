import * as Menu from '@radix-ui/react-dropdown-menu'
import type { Instrument } from '@td/shared'
import { useEffect, useState } from 'react'
import { CheckIcon, ChevronIcon } from '../icons'
import { errorText } from './hooks'
import { type OrderKind, checkOrder, formatStep, stepDecimals } from './order'
import type { ManualOrder } from './source'

interface Props {
  instrument: Instrument | null
  lastPrice: number | null
  /** Why no order can be placed now, as a sentence, or null when it can. */
  blocked: string | null
  /** The session's latest rejection; `at` changes with each one. */
  rejected: { reason: string; at: number } | null
  onPlace(order: ManualOrder): Promise<void>
}

const KINDS: { id: OrderKind; label: string }[] = [
  { id: 'market', label: 'Market' },
  { id: 'limit', label: 'Limit' },
]

/**
 * Floats at the bottom centre: size, order type (and a limit price), and Sell and Buy as the send
 * buttons in the loss and gain colours. One line under the controls says why an order cannot be
 * placed, what was rejected, or what was sent.
 */
export function OrderComposer({ instrument, lastPrice, blocked, rejected, onPlace }: Props) {
  const [qty, setQty] = useState('')
  const [kind, setKind] = useState<OrderKind>('market')
  const [price, setPrice] = useState('')
  const [sending, setSending] = useState(false)
  const [note, setNote] = useState<{ text: string; tone: 'status' | 'alert' } | null>(null)
  // A rejection shows until the next edit or order.
  const [seenRejection, setSeenRejection] = useState(rejected?.at ?? 0)

  const symbolKey = instrument ? `${instrument.id.venue}:${instrument.id.symbol}:${instrument.id.kind}` : ''
  useEffect(() => {
    setQty('')
    setPrice('')
    setNote(null)
  }, [symbolKey])

  const edit = (apply: () => void) => {
    apply()
    setNote(null)
    if (rejected) setSeenRejection(rejected.at)
  }

  const chooseKind = (next: OrderKind) =>
    edit(() => {
      setKind(next)
      if (next === 'limit' && price === '' && lastPrice !== null && instrument) {
        setPrice(lastPrice.toFixed(stepDecimals(instrument.tick_size)))
      }
    })

  const check = instrument ? checkOrder(instrument, { qty, kind, price }, lastPrice) : null
  const showRejection = rejected !== null && rejected.at !== seenRejection

  let message: { text: string; tone: 'status' | 'alert' } | null = null
  if (blocked) message = { text: blocked, tone: 'status' }
  else if (showRejection) message = { text: `Order rejected: ${rejected.reason}`, tone: 'alert' }
  else if (note) message = note
  else if (check && !check.ok && qty.trim() !== '') message = { text: check.reason, tone: 'status' }

  const canSend = !blocked && !sending && check?.ok === true

  const send = async (side: 'buy' | 'sell') => {
    if (!canSend || !instrument || !check?.ok) return
    if (rejected) setSeenRejection(rejected.at)
    setSending(true)
    setNote(null)
    const order: ManualOrder =
      check.price === null
        ? { side, qty: check.qty, type: 'market' }
        : { side, qty: check.qty, type: 'limit', limit_price: check.price }
    try {
      await onPlace(order)
      const what = `${side === 'buy' ? 'Buy' : 'Sell'} ${formatStep(check.qty, instrument.lot_size)} ${instrument.base}`
      const where = check.price === null ? 'at market' : `at ${formatStep(check.price, instrument.tick_size)}`
      setNote({ text: `${what} ${where} sent.`, tone: 'status' })
    } catch (e) {
      setNote({ text: errorText(e), tone: 'alert' })
    } finally {
      setSending(false)
    }
  }

  const sendButton = (side: 'buy' | 'sell') => (
    <button type="button" className="send" data-side={side} aria-disabled={!canSend} onClick={() => void send(side)}>
      {side === 'buy' ? 'Buy' : 'Sell'}
    </button>
  )

  return (
    <div className="order-composer" role="group" aria-label="Place a paper order" data-noted={message ? '' : undefined}>
      <div className="order-row">
        <label className="field">
          <span className="visually-hidden">Size</span>
          <input
            inputMode="decimal"
            placeholder="Size"
            autoComplete="off"
            spellCheck={false}
            value={qty}
            aria-invalid={check !== null && !check.ok && qty.trim() !== '' ? true : undefined}
            onChange={(e) => {
              const value = e.target.value
              edit(() => setQty(value))
            }}
          />
          <span className="field-unit">{instrument?.base ?? ''}</span>
        </label>
        <Menu.Root modal={false}>
          <Menu.Trigger asChild>
            <button type="button" className="pill pill-quiet" aria-label={`Order type: ${kind === 'market' ? 'Market' : 'Limit'}`}>
              <span className="pill-label">{kind === 'market' ? 'Market' : 'Limit'}</span>
              <ChevronIcon />
            </button>
          </Menu.Trigger>
          <Menu.Portal>
            <Menu.Content className="menu menu-narrow" side="top" align="start" sideOffset={10} collisionPadding={16}>
              <Menu.RadioGroup value={kind} onValueChange={(v) => chooseKind(v as OrderKind)}>
                {KINDS.map((k) => (
                  <Menu.RadioItem key={k.id} value={k.id} className="menu-item">
                    <Menu.ItemIndicator className="menu-check">
                      <CheckIcon />
                    </Menu.ItemIndicator>
                    <span className="menu-label">{k.label}</span>
                  </Menu.RadioItem>
                ))}
              </Menu.RadioGroup>
            </Menu.Content>
          </Menu.Portal>
        </Menu.Root>
        {kind === 'limit' && (
          <label className="field">
            <span className="visually-hidden">Limit price</span>
            <input
              inputMode="decimal"
              placeholder="Price"
              autoComplete="off"
              spellCheck={false}
              value={price}
              onChange={(e) => {
                const value = e.target.value
                edit(() => setPrice(value))
              }}
            />
            <span className="field-unit">{instrument?.quote ?? ''}</span>
          </label>
        )}
        <span className="composer-spacer" />
        {sendButton('sell')}
        {sendButton('buy')}
      </div>
      {message && (
        <p className="order-note" role={message.tone} data-tone={message.tone}>
          {message.text}
        </p>
      )}
    </div>
  )
}
