import * as Menu from '@radix-ui/react-dropdown-menu'
import type { EngineStatus, InstrumentId, StrategyInfo, Timeframe } from '@td/shared'
import { useEffect, useMemo, useState } from 'react'
import { EngineState } from '../EngineState'
import { CheckIcon, ChevronIcon } from '../icons'
import { Chart } from './Chart'
import { useCandles, useInstruments, usePaperSession } from './hooks'
import { OrderComposer } from './OrderComposer'
import { PositionLine } from './PositionLine'
import { loadRecent, pushRecent } from './recent'
import { isRunning, sameInstrument } from './session'
import { type ManualOrder, type TradingSource, chooseTradingSource } from './source'
import { TickerSwitcher } from './TickerSwitcher'

const DEFAULT_INSTRUMENT: InstrumentId = { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' }
const TIMEFRAMES: Timeframe[] = ['1m', '5m', '15m', '1h', '4h']
const TIMEFRAME_KEY = 'td.trading.timeframe'

function loadTimeframe(): Timeframe {
  try {
    const saved = localStorage.getItem(TIMEFRAME_KEY)
    return TIMEFRAMES.find((t) => t === saved) ?? '1m'
  } catch {
    return '1m'
  }
}

function saveTimeframe(timeframe: Timeframe): void {
  try {
    localStorage.setItem(TIMEFRAME_KEY, timeframe)
  } catch {
    // Blocked storage: the choice lasts for this view only.
  }
}

/** Trading: "what is the market doing, and what is my paper position?". */
export function Trading({ status }: { status: EngineStatus }) {
  if (status.state !== 'running') return <EngineState status={status} />
  return <TradingDesk />
}

function TradingDesk() {
  const [source, setSource] = useState<TradingSource | null>(null)
  const [recent, setRecent] = useState<InstrumentId[]>(() => loadRecent())
  const [instrumentId, setInstrumentId] = useState<InstrumentId>(() => recent[0] ?? DEFAULT_INSTRUMENT)
  const [timeframe, setTimeframe] = useState<Timeframe>(loadTimeframe)
  const [menuOpen, setMenuOpen] = useState(false)
  const [strategies, setStrategies] = useState<StrategyInfo[] | null>(null)

  useEffect(() => {
    let live = true
    void chooseTradingSource().then((s) => {
      if (!live) return
      setSource(s)
      s.strategies().then(
        (list) => live && setStrategies(list),
        () => live && setStrategies([]),
      )
    })
    return () => {
      live = false
    }
  }, [])

  // Ctrl+K opens the switcher. The section unmounts when hidden, so this only listens while shown.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!event.ctrlKey || event.altKey || event.shiftKey || event.metaKey || event.code !== 'KeyK') return
      event.preventDefault()
      setMenuOpen(true)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const instruments = useInstruments(source)
  const instrument = useMemo(
    () => instruments.list?.instruments.find((i) => sameInstrument(i.id, instrumentId)) ?? null,
    [instruments.list, instrumentId],
  )
  const { candles, revision } = useCandles(source, instrumentId, timeframe)
  const session = usePaperSession(source, instrumentId)
  const last = candles.at(-1) ?? null
  const lastPrice = last?.close ?? null

  const choose = (id: InstrumentId) => {
    setInstrumentId(id)
    setRecent(pushRecent(id))
    setMenuOpen(false)
  }

  const chooseTimeframe = (t: Timeframe) => {
    setTimeframe(t)
    saveTimeframe(t)
  }

  const state = session.state
  const running = session.summary ? isRunning(state?.status ?? session.summary.status) : false

  let blocked: string | null = null
  if (!source) blocked = 'Loading'
  else if (!source.orders.available) blocked = source.orders.reason
  else if (instruments.error) blocked = 'The instrument lists did not load, so the order cannot be checked.'
  else if (!instruments.list) blocked = 'Loading the instrument’s trading rules.'
  else if (!instrument) blocked = `${instrumentId.symbol} is not on the venue’s list.`
  else if (!session.loaded) blocked = 'Loading paper sessions.'
  else if (!session.summary) blocked = 'Start a paper session to place orders.'
  else if (!running) blocked = 'The paper session is stopped. Resume it to place orders.'

  const place = async (order: ManualOrder) => {
    if (source && session.summary) await source.placeOrder(session.summary.id, order)
  }

  const seriesKey = `${instrumentId.venue}:${instrumentId.symbol}:${instrumentId.kind}:${timeframe}`
  const fills = state?.fills ?? []
  const entry = state && state.position.qty !== 0 ? state.position.avg_entry : null

  return (
    <section className="trading" aria-labelledby="trading-title">
      <h1 id="trading-title" className="visually-hidden">
        Trading
      </h1>
      <Chart
        candles={candles}
        seriesKey={seriesKey}
        revision={revision}
        timeframe={timeframe}
        instrument={instrument}
        fills={fills}
        entry={entry}
        openOrders={state?.open_orders ?? []}
      />
      {source && instruments.error ? (
        <div className="chart-empty" role="alert">
          <p className="state-quiet">The instrument lists did not load.</p>
          <p className="chart-empty-note">{instruments.error}</p>
          <button type="button" className="button-float" onClick={instruments.retry}>
            Retry
          </button>
        </div>
      ) : source && !instruments.list ? (
        <div className="chart-empty" role="status">
          <p className="state-quiet shimmer">Loading instruments</p>
        </div>
      ) : (
        source &&
        candles.length === 0 && (
          <div className="chart-empty" role="status">
            <p className="state-quiet">Waiting for the first closed {timeframe} bar</p>
            {!window.td.mock && (
              <p className="chart-empty-note">The engine has no candle history yet (#36), so the chart fills from the live stream.</p>
            )}
          </div>
        )
      )}
      <div className="trading-top">
        <TickerSwitcher
          value={instrumentId}
          instrument={instrument}
          lastPrice={lastPrice}
          list={instruments.list}
          listError={instruments.error}
          recent={recent}
          open={menuOpen}
          onOpenChange={setMenuOpen}
          onChoose={choose}
          onRetry={instruments.retry}
        />
        <Menu.Root modal={false}>
          <Menu.Trigger asChild>
            <button type="button" className="float-pill" aria-label={`Timeframe: ${timeframe}`}>
              <span>{timeframe}</span>
              <ChevronIcon />
            </button>
          </Menu.Trigger>
          <Menu.Portal>
            <Menu.Content className="menu menu-narrow" side="bottom" align="start" sideOffset={10} collisionPadding={16}>
              <Menu.RadioGroup value={timeframe} onValueChange={(t) => chooseTimeframe(t as Timeframe)}>
                {TIMEFRAMES.map((t) => (
                  <Menu.RadioItem key={t} value={t} className="menu-item">
                    <Menu.ItemIndicator className="menu-check">
                      <CheckIcon />
                    </Menu.ItemIndicator>
                    <span className="menu-label">{t}</span>
                  </Menu.RadioItem>
                ))}
              </Menu.RadioGroup>
            </Menu.Content>
          </Menu.Portal>
        </Menu.Root>
      </div>
      <div className="order-dock">
        {source && (
          <PositionLine
            session={session}
            instrumentId={instrumentId}
            instrument={instrument}
            timeframe={timeframe}
            strategies={strategies}
            manualSessions={source.manualSessions}
          />
        )}
        <OrderComposer
          instrument={instrument}
          lastPrice={lastPrice}
          blocked={blocked}
          rejected={session.rejected}
          onPlace={place}
        />
      </div>
    </section>
  )
}
