import * as Popover from '@radix-ui/react-popover'
import type { Instrument, InstrumentId } from '@td/shared'
import { useEffect, useId, useMemo, useRef, useState } from 'react'
import { ChevronIcon } from '../icons'
import { type KindFilter, MAX_ROWS, searchInstruments, venueLabel } from './instruments'
import { formatStep } from './order'
import { sameInstrument } from './session'
import type { InstrumentList } from './source'

const FILTERS: { id: KindFilter; label: string }[] = [
  { id: 'all', label: 'All' },
  { id: 'spot', label: 'Spot' },
  { id: 'perpetual', label: 'Perpetual' },
]

interface Props {
  value: InstrumentId
  instrument: Instrument | null
  lastPrice: number | null
  list: InstrumentList | null
  listError: string | null
  recent: InstrumentId[]
  open: boolean
  onOpenChange(open: boolean): void
  onChoose(id: InstrumentId): void
  onRetry(): void
}

/**
 * The white pill at the chart's top left: symbol, venue and the last price. It opens a frosted,
 * searchable menu of every instrument both venues list (also on Ctrl+K).
 */
export function TickerSwitcher({ value, instrument, lastPrice, list, listError, recent, open, onOpenChange, onChoose, onRetry }: Props) {
  const tint = usePriceTint(lastPrice)
  return (
    <Popover.Root open={open} onOpenChange={onOpenChange}>
      <Popover.Trigger asChild>
        <button type="button" className="ticker" aria-label={`${value.symbol}, ${venueLabel(value)}. Change instrument (Ctrl+K)`}>
          <span className="ticker-name">
            <span className="ticker-symbol">{value.symbol}</span>
            <span className="ticker-venue">{venueLabel(value)}</span>
          </span>
          <span className="ticker-price" data-tint={tint}>
            {lastPrice === null ? '—' : instrument ? formatStep(lastPrice, instrument.tick_size) : lastPrice.toLocaleString('en-US')}
          </span>
          <ChevronIcon />
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content className="menu ticker-menu" side="bottom" align="start" sideOffset={10} collisionPadding={16}>
          <TickerMenu value={value} list={list} listError={listError} recent={recent} onChoose={onChoose} onRetry={onRetry} />
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  )
}

/**
 * Tints the price toward the direction of its latest change for a moment, with no animation: the
 * colour swaps in and back.
 */
function usePriceTint(price: number | null): 'gain' | 'loss' | undefined {
  const previous = useRef<number | null>(null)
  const [tint, setTint] = useState<'gain' | 'loss' | undefined>()
  useEffect(() => {
    const before = previous.current
    previous.current = price
    // A new instrument or timeframe starts with no price: drop the old tint with it.
    if (before === null || price === null) {
      setTint(undefined)
      return
    }
    if (before === price) return
    setTint(price > before ? 'gain' : 'loss')
    const timer = setTimeout(() => setTint(undefined), 700)
    return () => clearTimeout(timer)
  }, [price])
  return tint
}

function TickerMenu({
  value,
  list,
  listError,
  recent,
  onChoose,
  onRetry,
}: Pick<Props, 'value' | 'list' | 'listError' | 'recent' | 'onChoose' | 'onRetry'>) {
  const [query, setQuery] = useState('')
  const [kind, setKind] = useState<KindFilter>('all')
  const [active, setActive] = useState(0)
  const listId = useId()

  const instruments = list?.instruments ?? []
  const matches = useMemo(() => searchInstruments(instruments, query, kind), [instruments, query, kind])
  const recentRows = useMemo(
    () =>
      query || !list
        ? []
        : recent
            .map((r) => instruments.find((i) => sameInstrument(i.id, r)))
            .filter((i): i is Instrument => i !== undefined && (kind === 'all' || i.id.kind === kind)),
    [query, list, recent, instruments, kind],
  )
  const shown = matches.slice(0, MAX_ROWS)
  const rows = [...recentRows, ...shown]
  const activeIndex = Math.min(active, Math.max(0, rows.length - 1))
  const optionId = (i: number) => `${listId}-${i}`

  useEffect(() => setActive(0), [query, kind])

  const onKeyDown = (event: React.KeyboardEvent) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault()
      const step = event.key === 'ArrowDown' ? 1 : -1
      const next = (activeIndex + step + rows.length) % Math.max(1, rows.length)
      setActive(next)
      document.getElementById(optionId(next))?.scrollIntoView({ block: 'nearest' })
    } else if (event.key === 'Enter') {
      const row = rows[activeIndex]
      if (row) {
        event.preventDefault()
        onChoose(row.id)
      }
    }
  }

  const row = (i: Instrument, index: number) => (
    <li
      key={`${index < recentRows.length ? 'r' : 'm'}-${i.id.venue}-${i.id.kind}-${i.id.symbol}`}
      id={optionId(index)}
      role="option"
      aria-selected={index === activeIndex}
      className="menu-item ticker-row"
      data-highlighted={index === activeIndex ? '' : undefined}
      data-current={sameInstrument(i.id, value) ? '' : undefined}
      onMouseMove={() => setActive(index)}
      onClick={() => onChoose(i.id)}
    >
      <span className="menu-label">{i.id.symbol}</span>
      <span className="menu-hint">{venueLabel(i.id)}</span>
    </li>
  )

  let body: React.ReactNode
  if (listError) {
    body = (
      <div className="menu-state" role="alert">
        <p className="menu-state-text">The instrument lists did not load. {listError}</p>
        <button type="button" className="button-float" onClick={onRetry}>
          Retry
        </button>
      </div>
    )
  } else if (!list) {
    body = (
      <p className="menu-state menu-state-text shimmer" role="status">
        Loading instruments
      </p>
    )
  } else if (rows.length === 0) {
    body = <p className="menu-state menu-state-text">{query ? `No instrument matches “${query}”.` : 'No instruments listed.'}</p>
  } else {
    body = (
      <ul className="ticker-list" id={listId} role="listbox" aria-label="Instruments">
        {recentRows.length > 0 && (
          <li role="presentation" className="menu-group-label">
            Recent
          </li>
        )}
        {recentRows.map((i, n) => row(i, n))}
        {recentRows.length > 0 && (
          <li role="presentation" className="menu-group-label">
            All instruments
          </li>
        )}
        {shown.map((i, n) => row(i, recentRows.length + n))}
        {matches.length > shown.length && (
          <li role="presentation" className="menu-group-label">
            {`Showing ${shown.length} of ${matches.length.toLocaleString('en-US')}. Type to narrow the list.`}
          </li>
        )}
      </ul>
    )
  }

  return (
    <div className="ticker-menu-inner">
      <input
        className="menu-search"
        type="text"
        role="combobox"
        aria-expanded="true"
        aria-controls={listId}
        aria-activedescendant={rows.length ? optionId(activeIndex) : undefined}
        aria-label="Search instruments"
        placeholder="Search symbols"
        autoComplete="off"
        spellCheck={false}
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={onKeyDown}
      />
      <div className="segmented" role="radiogroup" aria-label="Kind">
        {FILTERS.map((f) => (
          <button key={f.id} type="button" role="radio" aria-checked={kind === f.id} onClick={() => setKind(f.id)}>
            {f.label}
          </button>
        ))}
      </div>
      {body}
      {list?.problems.map((p) => (
        <p key={p} className="menu-note">
          {p}
        </p>
      ))}
    </div>
  )
}
