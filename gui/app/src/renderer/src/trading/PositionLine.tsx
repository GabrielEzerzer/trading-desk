import * as Menu from '@radix-ui/react-dropdown-menu'
import * as Popover from '@radix-ui/react-popover'
import type { Fill, Instrument, InstrumentId, PaperOrder, SessionState, Status, StrategyInfo, Timeframe, Trade } from '@td/shared'
import { type ReactNode, useEffect, useState } from 'react'
import { CheckIcon, ChevronIcon } from '../icons'
import { readableName } from '../lab/source'
import { type PaperSession, clock } from './hooks'
import { formatStep, parseAmount } from './order'
import { isRunning } from './session'
import { type Capability, MANUAL } from './source'

const MINUS = '−'

/** Money in the quote asset: two decimals, separators, a typographic minus, a sign when asked. */
export function money(value: number, sign = false): string {
  const text = Math.abs(value).toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })
  if (value < 0 && Math.abs(value) >= 0.005) return MINUS + text
  return sign && value >= 0.005 ? `+${text}` : text
}

/** The colour role of an amount shown by `money`: ink when it rounds to zero. */
export function moneyTone(value: number): 'gain' | 'loss' | undefined {
  if (Math.abs(value) < 0.005) return undefined
  return value > 0 ? 'gain' : 'loss'
}

export function strategyLabel(name: string): string {
  return name === MANUAL ? 'Manual' : readableName(name)
}

export function statusText(status: Status): string {
  switch (status.status) {
    case 'warming_up':
      return 'Warming up'
    case 'live':
      return 'Live'
    case 'reconnecting':
      return 'Reconnecting to the feed'
    case 'stopped':
      return 'Stopped'
    case 'error':
      return 'Stopped with an error'
  }
}

interface Props {
  session: PaperSession
  instrumentId: InstrumentId
  instrument: Instrument | null
  timeframe: Timeframe
  strategies: StrategyInfo[] | null
  manualSessions: Capability
}

/**
 * One quiet line above the composer: the session's status, position, average price, unrealised
 * and realised P&L, and equity. It opens a frosted panel with the session's controls and its open
 * orders, fills and trades. With no session it offers to start one.
 */
export function PositionLine({ session, instrumentId, instrument, timeframe, strategies, manualSessions }: Props) {
  const [panelOpen, setPanelOpen] = useState(false)
  const [startOpen, setStartOpen] = useState(false)
  const [notice, setNotice] = useState<string | null>(null)
  const { summary, state } = session

  // A dropped bucket or warning shows for half a minute, then the line is quiet again.
  useEffect(() => {
    if (!session.notice) {
      setNotice(null)
      return
    }
    setNotice(session.notice.text)
    const timer = setTimeout(() => setNotice(null), 30_000)
    return () => clearTimeout(timer)
  }, [session.notice])

  const start = (
    <StartSession
      open={startOpen}
      onOpenChange={setStartOpen}
      session={session}
      instrumentId={instrumentId}
      instrument={instrument}
      timeframe={timeframe}
      strategies={strategies}
      manualSessions={manualSessions}
      trigger={
        <button type="button" className="line-button">
          {summary ? 'New session' : 'Start a session'}
        </button>
      }
    />
  )

  if (!session.loaded) {
    if (!session.loadError) return <div className="position-line" />
    return (
      <div className="position-line">
        <span className="line-text line-error" role="alert" title={session.loadError}>
          The paper sessions did not load. {session.loadError}
        </span>
        <button type="button" className="line-button" onClick={session.reload}>
          Retry
        </button>
      </div>
    )
  }

  if (!summary) {
    return (
      <div className="position-line">
        <span className="line-text">No paper session on {instrumentId.symbol}</span>
        {start}
      </div>
    )
  }

  const status = state?.status ?? summary.status
  const running = isRunning(status)
  const base = instrument?.base ?? state?.config.instrument.base ?? ''
  const quote = instrument?.quote ?? state?.config.instrument.quote ?? ''
  const position = state?.position
  const lot = state?.config.instrument.lot_size ?? instrument?.lot_size ?? 0.0001
  const tick = state?.config.instrument.tick_size ?? instrument?.tick_size ?? 0.01

  return (
    <>
      {notice && running && (
        <p className="position-notice" role="status">
          {notice}
        </p>
      )}
      <div className="position-line">
        <Popover.Root open={panelOpen} onOpenChange={setPanelOpen}>
          <Popover.Trigger asChild>
            <button type="button" className="line-button position-summary" aria-label="Session, orders, fills and trades">
              <span className="status-dot" data-status={status.status} aria-hidden="true" />
              <span className="line-text">
                {strategyLabel(summary.strategy)} · {statusText(status)}
              </span>
              {position && (
                <>
                  <span className="line-item">
                    {position.qty === 0 ? (
                      <span className="line-label">Flat</span>
                    ) : (
                      <>
                        <span className="line-value">
                          {position.qty < 0 ? MINUS : ''}
                          {formatStep(Math.abs(position.qty), lot)} {base}
                        </span>
                        <span className="line-label">at</span>
                        <span className="line-value">{formatStep(position.avg_entry, tick)}</span>
                      </>
                    )}
                  </span>
                  <span className="line-item">
                    <span className="line-label">Unrealised</span>
                    <span className="line-value" data-tone={moneyTone(position.unrealised_pnl)}>
                      {money(position.unrealised_pnl, true)}
                    </span>
                  </span>
                  <span className="line-item line-optional">
                    <span className="line-label">Realised</span>
                    <span className="line-value" data-tone={moneyTone(position.realised_pnl)}>
                      {money(position.realised_pnl, true)}
                    </span>
                  </span>
                </>
              )}
              <span className="line-item">
                <span className="line-label">Equity</span>
                <span className="line-value">
                  {money(state?.equity ?? summary.equity)} {quote}
                </span>
              </span>
              <ChevronIcon />
            </button>
          </Popover.Trigger>
          <Popover.Portal>
            <Popover.Content className="menu session-panel" side="top" align="center" sideOffset={10} collisionPadding={16}>
              <SessionPanel session={session} running={running} status={status} start={start} base={base} lot={lot} tick={tick} />
            </Popover.Content>
          </Popover.Portal>
        </Popover.Root>
        {!running && (
          <button type="button" className="line-button" onClick={session.resume} aria-disabled={session.busy}>
            Resume
          </button>
        )}
      </div>
    </>
  )
}

type Tab = 'orders' | 'fills' | 'trades'

function SessionPanel({
  session,
  running,
  status,
  start,
  base,
  lot,
  tick,
}: {
  session: PaperSession
  running: boolean
  status: Status
  start: ReactNode
  base: string
  lot: number
  tick: number
}) {
  const [tab, setTab] = useState<Tab>('orders')
  const state = session.state
  const summary = session.summary
  if (!summary) return null
  const counts: Record<Tab, number> = {
    orders: state?.open_orders.length ?? 0,
    fills: state?.fills.length ?? 0,
    trades: state?.trades.length ?? 0,
  }

  return (
    <div className="session-panel-inner">
      <div className="panel-head">
        <div>
          <p className="panel-title">
            {strategyLabel(summary.strategy)} on {summary.instrument.symbol} · {summary.timeframe}
          </p>
          <p className="panel-sub">
            {status.status === 'live' && state?.live_from ? `Live since ${clock(state.live_from)} UTC` : statusText(status)}
            {status.status === 'error' ? `: ${status.message}` : ''}
          </p>
        </div>
        <div className="panel-actions">
          {running ? (
            <button type="button" className="button-small" onClick={session.stop} aria-disabled={session.busy}>
              Stop
            </button>
          ) : (
            <>
              {start}
              <button type="button" className="button-small" onClick={session.resume} aria-disabled={session.busy}>
                Resume
              </button>
            </>
          )}
        </div>
      </div>
      {session.error && (
        <p className="panel-error" role="alert">
          {session.error}
        </p>
      )}
      <div className="segmented" role="tablist" aria-label="Session records">
        {(['orders', 'fills', 'trades'] as const).map((t) => (
          <button key={t} type="button" role="tab" aria-selected={tab === t} onClick={() => setTab(t)}>
            {t === 'orders' ? 'Open orders' : t === 'fills' ? 'Fills' : 'Trades'}
            <span className="segmented-count">{counts[t]}</span>
          </button>
        ))}
      </div>
      <div className="panel-table" role="tabpanel">
        {!state ? (
          <p className="panel-empty">Loading the session</p>
        ) : tab === 'orders' ? (
          <OrdersTable
            orders={state.open_orders}
            base={base}
            lot={lot}
            tick={tick}
            onCancel={running ? session.cancel : null}
            busy={session.busy}
          />
        ) : tab === 'fills' ? (
          <FillsTable fills={state.fills} lot={lot} tick={tick} />
        ) : (
          <TradesTable trades={state.trades} lot={lot} tick={tick} />
        )}
      </div>
    </div>
  )
}

const side = (s: 'buy' | 'sell') => (s === 'buy' ? 'Buy' : 'Sell')

function OrdersTable({
  orders,
  base,
  lot,
  tick,
  onCancel,
  busy,
}: {
  orders: PaperOrder[]
  base: string
  lot: number
  tick: number
  /** Cancels a manual order; null while the session is stopped. */
  onCancel: ((id: number) => void) | null
  busy: boolean
}) {
  if (orders.length === 0) return <p className="panel-empty">No open orders.</p>
  return (
    <table className="table">
      <thead>
        <tr>
          <th scope="col">Side</th>
          <th scope="col">Type</th>
          <th scope="col" className="num">
            Size ({base})
          </th>
          <th scope="col" className="num">
            Price
          </th>
          <th scope="col" className="num">
            Placed
          </th>
          {onCancel && (
            <th scope="col">
              <span className="visually-hidden">Cancel</span>
            </th>
          )}
        </tr>
      </thead>
      <tbody>
        {orders.map((o) => {
          const type = o.request.order_type
          return (
            <tr key={o.id}>
              <td data-tone={o.request.side === 'buy' ? 'gain' : 'loss'}>{side(o.request.side)}</td>
              <td>{type.type === 'market' ? 'Market' : type.type === 'limit' ? 'Limit' : 'Stop'}</td>
              <td className="num">{formatStep(o.request.qty, lot)}</td>
              <td className="num">{type.type === 'limit' ? formatStep(type.price, tick) : 'Next open'}</td>
              <td className="num muted">{clock(o.submitted_at)}</td>
              {onCancel && (
                <td className="num">
                  {o.source === 'manual' && (
                    <button
                      type="button"
                      className="button-small"
                      aria-label={`Cancel the ${side(o.request.side).toLowerCase()} order of ${formatStep(o.request.qty, lot)} ${base}`}
                      aria-disabled={busy}
                      onClick={() => !busy && onCancel(o.id)}
                    >
                      Cancel
                    </button>
                  )}
                </td>
              )}
            </tr>
          )
        })}
      </tbody>
    </table>
  )
}

function FillsTable({ fills, lot, tick }: { fills: Fill[]; lot: number; tick: number }) {
  if (fills.length === 0) return <p className="panel-empty">No fills yet.</p>
  return (
    <table className="table">
      <thead>
        <tr>
          <th scope="col">Time</th>
          <th scope="col">Side</th>
          <th scope="col" className="num">
            Size
          </th>
          <th scope="col" className="num">
            Price
          </th>
          <th scope="col" className="num">
            Fee
          </th>
        </tr>
      </thead>
      <tbody>
        {[...fills].reverse().map((f, i) => (
          <tr key={`${f.order_id}-${f.time}-${i}`}>
            <td className="muted">{clock(f.time)}</td>
            <td>{side(f.side)}</td>
            <td className="num">{formatStep(f.qty, lot)}</td>
            <td className="num">{formatStep(f.price, tick)}</td>
            <td className="num muted">{f.fee.toFixed(4)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

function TradesTable({ trades, lot, tick }: { trades: Trade[]; lot: number; tick: number }) {
  if (trades.length === 0) return <p className="panel-empty">No trades yet.</p>
  return (
    <table className="table">
      <thead>
        <tr>
          <th scope="col">Side</th>
          <th scope="col" className="num">
            Size
          </th>
          <th scope="col" className="num">
            Entry
          </th>
          <th scope="col" className="num">
            Exit
          </th>
          <th scope="col" className="num">
            Net P&amp;L
          </th>
        </tr>
      </thead>
      <tbody>
        {[...trades].reverse().map((t, i) => (
          <tr key={`${t.entry_time}-${i}`}>
            <td>
              {t.side === 'long' ? 'Long' : 'Short'}
              {t.open && <span className="muted"> · open</span>}
            </td>
            <td className="num">{formatStep(t.qty, lot)}</td>
            <td className="num">{formatStep(t.entry_price, tick)}</td>
            <td className="num">{formatStep(t.exit_price, tick)}</td>
            <td className="num" data-tone={moneyTone(t.net_pnl)}>
              {money(t.net_pnl, true)}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

function StartSession({
  open,
  onOpenChange,
  session,
  instrumentId,
  instrument,
  timeframe,
  strategies,
  manualSessions,
  trigger,
}: {
  open: boolean
  onOpenChange(open: boolean): void
  session: PaperSession
  instrumentId: InstrumentId
  instrument: Instrument | null
  timeframe: Timeframe
  strategies: StrategyInfo[] | null
  manualSessions: Capability
  trigger: ReactNode
}) {
  const [strategy, setStrategy] = useState<string>(manualSessions.available ? MANUAL : (strategies?.[0]?.name ?? ''))
  const [cash, setCash] = useState('10000')
  const [error, setError] = useState<string | null>(null)
  const chosen = strategy || strategies?.[0]?.name || ''
  const spec = strategies?.find((s) => s.name === chosen)
  const amount = parseAmount(cash)

  let blocked: string | null = null
  if (!instrument) blocked = 'Waiting for the instrument’s trading rules.'
  else if (!chosen) blocked = 'Choose a strategy.'
  else if (amount === null || amount <= 0) blocked = 'Enter starting cash above zero.'

  const submit = async () => {
    if (blocked || amount === null) return
    setError(null)
    const ok = await session.start({
      strategy: chosen,
      params: Object.fromEntries((spec?.params ?? []).map((p) => [p.name, p.default])),
      instrument: instrumentId,
      timeframe,
      starting_cash: amount,
      slippage_bps: 2,
    })
    if (ok) onOpenChange(false)
  }

  useEffect(() => {
    if (open) setError(null)
  }, [open])
  useEffect(() => {
    if (session.error && open) setError(session.error)
  }, [session.error, open])

  return (
    <Popover.Root open={open} onOpenChange={onOpenChange}>
      <Popover.Trigger asChild>{trigger}</Popover.Trigger>
      <Popover.Portal>
        <Popover.Content className="menu start-panel" side="top" align="center" sideOffset={10} collisionPadding={16}>
          <p className="panel-title">Start a paper session</p>
          <p className="panel-sub">
            Runs on {instrumentId.symbol} {timeframe} bars from now on.
          </p>
          <div className="start-fields">
            <span className="start-label">Strategy</span>
            <Menu.Root modal={false}>
              <Menu.Trigger asChild>
                <button type="button" className="pill pill-quiet" aria-label={`Strategy: ${strategyLabel(chosen)}`}>
                  <span className="pill-label">{chosen ? strategyLabel(chosen) : 'Choose'}</span>
                  <ChevronIcon />
                </button>
              </Menu.Trigger>
              <Menu.Portal>
                <Menu.Content className="menu" side="top" align="start" sideOffset={8} collisionPadding={16}>
                  <Menu.RadioGroup value={chosen} onValueChange={setStrategy}>
                    <Menu.RadioItem value={MANUAL} className="menu-item" disabled={!manualSessions.available}>
                      <Menu.ItemIndicator className="menu-check">
                        <CheckIcon />
                      </Menu.ItemIndicator>
                      <span className="menu-stack">
                        <span className="menu-label">Manual</span>
                        <span className="menu-hint">
                          {manualSessions.available ? 'Only your own orders trade' : manualSessions.reason}
                        </span>
                      </span>
                    </Menu.RadioItem>
                    {(strategies ?? []).map((s) => (
                      <Menu.RadioItem key={s.name} value={s.name} className="menu-item">
                        <Menu.ItemIndicator className="menu-check">
                          <CheckIcon />
                        </Menu.ItemIndicator>
                        <span className="menu-label">{strategyLabel(s.name)}</span>
                      </Menu.RadioItem>
                    ))}
                  </Menu.RadioGroup>
                </Menu.Content>
              </Menu.Portal>
            </Menu.Root>
            <label className="start-label" htmlFor="start-cash">
              Starting cash
            </label>
            <span className="field field-panel">
              <input
                id="start-cash"
                inputMode="decimal"
                autoComplete="off"
                value={cash}
                onChange={(e) => setCash(e.target.value)}
              />
              <span className="field-unit">{instrument?.quote ?? ''}</span>
            </span>
          </div>
          {(error ?? blocked) && (
            <p className={error ? 'panel-error' : 'panel-sub'} role={error ? 'alert' : undefined}>
              {error ?? blocked}
            </p>
          )}
          <button type="button" className="button-dark" onClick={() => void submit()} aria-disabled={blocked !== null || session.busy}>
            Start
          </button>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  )
}
