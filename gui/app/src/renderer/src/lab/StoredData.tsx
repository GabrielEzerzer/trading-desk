import * as Menu from '@radix-ui/react-dropdown-menu'
import type { DeleteResult, StoredInstrumentInfo, StoredResponse } from '@td/shared'
import { type Ref, useEffect, useRef, useState } from 'react'
import { sentence } from '../engine'
import { CheckIcon, ChevronIcon } from '../icons'
import { formatBytes, formatInstrument, formatStoreMonth, formatVenue } from './format'
import { instrumentKey } from './runs'
import type { DeleteTarget } from './source'

/** What the confirmation asks to delete. */
type Asking = { kind: 'instrument'; key: string } | { kind: 'range'; key: string; from: string; to: string } | { kind: 'all' }

interface Props {
  stored: StoredResponse
  /** Why the list could not be read again, or null. */
  refreshError: string | null
  /** Reads the list again; resolves false when that failed (`refreshError` then says why). */
  onReload(): Promise<boolean>
  /** Deletes `target` and reads the list again; rejects when the engine refuses. */
  onDelete(target: DeleteTarget): Promise<DeleteResult>
  onOpenFolder(): Promise<void>
  headingRef?: Ref<HTMLHeadingElement>
}

const message = (e: unknown) => sentence(e instanceof Error ? e.message : String(e))
const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? '' : 's'}`

/** "Jan 2024 to Mar 2024", or one month alone. */
function monthSpan(from: string, to: string): string {
  return from === to ? formatStoreMonth(from) : `${formatStoreMonth(from)} to ${formatStoreMonth(to)}`
}

interface Plan {
  target: DeleteTarget
  /** Names what goes, for the line after the delete. */
  label: string
  bytes: number
}

/** What `asking` deletes from `stored`, or null when it names an instrument no longer stored. */
function plan(asking: Asking, stored: StoredResponse): Plan | null {
  if (asking.kind === 'all') {
    return { target: 'all', label: 'all market data', bytes: stored.instruments.reduce((sum, i) => sum + i.bytes, 0) }
  }
  const info = stored.instruments.find((i) => instrumentKey(i.instrument) === asking.key)
  if (!info) return null
  const name = formatInstrument(info.instrument)
  if (asking.kind === 'instrument') return { target: { instrument: info.instrument }, label: name, bytes: info.bytes }
  const months = info.months.filter((m) => m.month >= asking.from && m.month <= asking.to)
  return {
    target: { instrument: info.instrument, from: asking.from, to: asking.to },
    label: `${monthSpan(asking.from, asking.to)} of ${name}`,
    bytes: months.reduce((sum, m) => sum + m.bytes, 0),
  }
}

/**
 * The Lab's view of stored market data: the data folder, its size, each instrument with its months
 * and size, and deletes of one instrument, a range of its months or all of it. Each delete asks
 * first, in place, naming what goes and the space it frees.
 */
export function StoredData({ stored, refreshError, onReload, onDelete, onOpenFolder, headingRef }: Props) {
  const [asking, setAsking] = useState<Asking | null>(null)
  const [deleting, setDeleting] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)
  const [done, setDone] = useState<string | null>(null)
  const [folderError, setFolderError] = useState<string | null>(null)
  const doneRef = useRef<HTMLParagraphElement>(null)
  // Counts confirmations asked for, so only the latest one opens once its read of the list is back.
  const opening = useRef(0)

  const total = stored.instruments.reduce((sum, i) => sum + i.bytes, 0)
  const count = stored.instruments.length

  /** Changes or closes the open confirmation, such as its range of months. */
  const ask = (next: Asking | null) => {
    if (deleting) return
    opening.current += 1
    setAsking(next)
    setDeleteError(null)
    setDone(null)
  }

  /**
   * Opens a confirmation once the list is read again, so it names what is stored now and not what
   * was stored when the view opened. When that read fails, the error shows with Retry instead.
   */
  const openConfirmation = async (next: Asking) => {
    if (deleting) return
    ask(null)
    const n = opening.current
    if ((await onReload()) && n === opening.current) setAsking(next)
  }

  const confirm = async (p: Plan) => {
    if (deleting) return
    setDeleting(true)
    setDeleteError(null)
    try {
      const result = await onDelete(p.target)
      setAsking(null)
      setDone(`Deleted ${p.label}. ${formatBytes(result.bytes)} freed.`)
      // The row it was asked from may be gone; the keyboard lands on the line that says what went.
      requestAnimationFrame(() => doneRef.current?.focus({ preventScroll: true }))
    } catch (e) {
      setDeleteError(message(e))
    } finally {
      setDeleting(false)
    }
  }

  const openFolder = () => {
    setFolderError(null)
    onOpenFolder().catch((e: unknown) => setFolderError(message(e)))
  }

  const current = asking ? plan(asking, stored) : null
  const confirmation = (body: React.ReactNode) =>
    current && (
      <Confirmation body={body} plan={current} deleting={deleting} error={deleteError} onConfirm={confirm} onCancel={() => ask(null)} />
    )

  return (
    <section aria-labelledby="stored-title">
      <h1 id="stored-title" className="eyebrow" ref={headingRef} tabIndex={-1}>
        Market data
      </h1>
      <div className="verdict-line">
        {count === 0 ? (
          <p className="verdict" data-decision="none">
            Nothing stored
          </p>
        ) : (
          <>
            <p className="verdict">{formatBytes(total)}</p>
            <span className="verdict-kind">in {plural(count, 'instrument')}</span>
          </>
        )}
      </div>

      <div className="data-folder">
        <span className="data-path" title={stored.data_dir}>
          {stored.data_dir}
        </span>
        <button type="button" className="button-small" onClick={openFolder}>
          Open folder
        </button>
      </div>
      {folderError && (
        <p className="state-text" role="alert">
          The folder did not open. {folderError}
        </p>
      )}
      {refreshError && (
        <div className="data-note" role="alert">
          <p className="state-text">The list did not load again. {refreshError}</p>
          <button type="button" className="button-small" onClick={() => void onReload()}>
            Retry
          </button>
        </div>
      )}
      {done && (
        <p className="data-done" role="status" ref={doneRef} tabIndex={-1}>
          {done}
        </p>
      )}

      {count === 0 ? (
        <p className="state-text">
          Download history from a strategy that has none, or with the td-data command line tool.
        </p>
      ) : (
        <>
          <table className="table data-table">
            <thead>
              <tr>
                <th scope="col">Instrument</th>
                <th scope="col">Venue</th>
                <th scope="col">History</th>
                <th scope="col" className="num">
                  Size
                </th>
                <th scope="col">
                  <span className="visually-hidden">Delete</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {stored.instruments.map((info) => {
                const key = instrumentKey(info.instrument)
                const open = asking && asking.kind !== 'all' && asking.key === key ? asking : null
                return (
                  <InstrumentRows
                    key={key}
                    info={info}
                    disabled={deleting}
                    onAsk={(next) => void openConfirmation(next)}
                    confirmation={open && confirmation(<Question info={info} asking={open} onRange={ask} disabled={deleting} />)}
                  />
                )
              })}
            </tbody>
          </table>
          <div className="data-foot">
            {asking?.kind === 'all' ? (
              confirmation(
                <p className="confirm-text">
                  Delete all market data, {plural(count, 'instrument')}? Saved runs and paper sessions stay.
                </p>,
              )
            ) : (
              <button type="button" className="button-small" onClick={() => void openConfirmation({ kind: 'all' })} aria-disabled={deleting}>
                Delete all market data
              </button>
            )}
          </div>
        </>
      )}
      <p className="footnote">
        Saved runs and paper sessions are kept in the same folder and are not deleted here.
        {stored.unrecognised.length === 1 && ' One other entry in it is left alone.'}
        {stored.unrecognised.length > 1 && ` ${stored.unrecognised.length} other entries in it are left alone.`}
      </p>
    </section>
  )
}

interface RowProps {
  info: StoredInstrumentInfo
  disabled: boolean
  onAsk(asking: Asking): void
  confirmation: React.ReactNode
}

function InstrumentRows({ info, disabled, onAsk, confirmation }: RowProps) {
  const key = instrumentKey(info.instrument)
  const first = info.months[0]?.month
  const last = info.months.at(-1)?.month
  const name = formatInstrument(info.instrument)
  return (
    <>
      <tr>
        <td>
          {info.instrument.symbol} <span className="muted">{info.instrument.kind}</span>
        </td>
        <td className="muted">{formatVenue(info.instrument.venue)}</td>
        <td>
          {first && last ? (
            <>
              {monthSpan(first, last).replace(' to ', ' – ')} <span className="muted">{plural(info.months.length, 'month')}</span>
            </>
          ) : (
            <span className="muted">Funding rates only</span>
          )}
        </td>
        <td className="num">{formatBytes(info.bytes)}</td>
        <td className="data-action">
          <Menu.Root modal={false}>
            <Menu.Trigger asChild disabled={disabled}>
              <button type="button" className="row-action" aria-label={`Delete ${name}`}>
                Delete
              </button>
            </Menu.Trigger>
            <Menu.Portal>
              <Menu.Content className="menu" side="bottom" align="end" sideOffset={6} collisionPadding={16}>
                <Menu.Item className="menu-item" onSelect={() => onAsk({ kind: 'instrument', key })}>
                  <span className="menu-label">All of it</span>
                  <span className="menu-hint">{formatBytes(info.bytes)}</span>
                </Menu.Item>
                {first && last && (
                  <Menu.Item className="menu-item" onSelect={() => onAsk({ kind: 'range', key, from: first, to: first })}>
                    <span className="menu-label">A range of months</span>
                  </Menu.Item>
                )}
              </Menu.Content>
            </Menu.Portal>
          </Menu.Root>
        </td>
      </tr>
      {confirmation && (
        <tr className="confirm-row">
          <td colSpan={5}>{confirmation}</td>
        </tr>
      )}
    </>
  )
}

interface QuestionProps {
  info: StoredInstrumentInfo
  asking: Exclude<Asking, { kind: 'all' }>
  onRange(asking: Asking): void
  disabled: boolean
}

/** The question for one instrument: all of it, or the months from one pill to another. */
function Question({ info, asking, onRange, disabled }: QuestionProps) {
  const name = formatInstrument(info.instrument)
  const funding = info.funding_bytes !== null
  if (asking.kind === 'instrument' && info.months.length === 0) {
    return <p className="confirm-text">Delete the funding rates of {name}?</p>
  }
  if (asking.kind === 'instrument') {
    return (
      <p className="confirm-text">
        Delete {name}: {info.months.length === 1 ? 'its one month' : `all ${info.months.length} months`}
        {funding && ' and its funding rates'}?
      </p>
    )
  }
  const months = info.months.map((m) => m.month)
  const count = months.filter((m) => m >= asking.from && m <= asking.to).length
  return (
    <>
      <div className="confirm-range">
        <span>Delete {name} from</span>
        <MonthPicker
          label="From"
          months={months}
          value={asking.from}
          disabled={disabled}
          onChange={(from) => onRange({ ...asking, from, to: asking.to < from ? from : asking.to })}
        />
        <span>to</span>
        <MonthPicker
          label="To"
          months={months.filter((m) => m >= asking.from)}
          value={asking.to}
          disabled={disabled}
          onChange={(to) => onRange({ ...asking, to })}
        />
      </div>
      <p className="confirm-text">
        That is {plural(count, 'month')}.{funding && ' The funding rates stay.'}
      </p>
    </>
  )
}

interface MonthPickerProps {
  label: string
  months: string[]
  value: string
  disabled: boolean
  onChange(month: string): void
}

function MonthPicker({ label, months, value, disabled, onChange }: MonthPickerProps) {
  return (
    <Menu.Root modal={false}>
      <Menu.Trigger asChild disabled={disabled}>
        <button type="button" className="pill pill-month" aria-label={`${label}: ${formatStoreMonth(value)}`}>
          <span className="pill-label">{formatStoreMonth(value)}</span>
          <ChevronIcon />
        </button>
      </Menu.Trigger>
      <Menu.Portal>
        <Menu.Content className="menu menu-months" side="bottom" align="start" sideOffset={6} collisionPadding={16}>
          <Menu.RadioGroup value={value} onValueChange={onChange}>
            {months.map((m) => (
              <Menu.RadioItem key={m} value={m} className="menu-item">
                <Menu.ItemIndicator className="menu-check">
                  <CheckIcon />
                </Menu.ItemIndicator>
                <span className="menu-label">{formatStoreMonth(m)}</span>
              </Menu.RadioItem>
            ))}
          </Menu.RadioGroup>
        </Menu.Content>
      </Menu.Portal>
    </Menu.Root>
  )
}

interface ConfirmationProps {
  body: React.ReactNode
  plan: Plan
  deleting: boolean
  error: string | null
  onConfirm(plan: Plan): void
  onCancel(): void
}

/** The question, the space the delete frees, and Delete and Cancel. */
function Confirmation({ body, plan: p, deleting, error, onConfirm, onCancel }: ConfirmationProps) {
  // The question opens, or its failure grows, below the fold: its buttons scroll up into view.
  const actions = useRef<HTMLDivElement>(null)
  useEffect(() => {
    actions.current?.scrollIntoView({ block: 'nearest' })
  }, [error])
  return (
    <div className="confirm" role="group" aria-label="Confirm the delete">
      {body}
      <p className="confirm-text">This frees {formatBytes(p.bytes)}.</p>
      {error && (
        <p className="confirm-error" role="alert">
          The delete failed. {error}
        </p>
      )}
      <div className="confirm-actions" ref={actions}>
        <button
          type="button"
          className="button-dark"
          aria-disabled={deleting}
          onClick={() => {
            if (!deleting) onConfirm(p)
          }}
        >
          {deleting ? 'Deleting' : 'Delete'}
        </button>
        <button type="button" className="button-small" aria-disabled={deleting} onClick={deleting ? undefined : onCancel}>
          Cancel
        </button>
      </div>
    </div>
  )
}
