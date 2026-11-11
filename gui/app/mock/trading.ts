// Trading's mock source, used when the app runs with TD_MOCK=1 and by the headless screenshots.
// It reads trading.json (made by generate-trading.mjs): instruments for both venues and candle
// history for BTCUSDT and ETHUSDT. Other instruments reuse BTCUSDT's path scaled to their own
// price. Live bars continue the history quickly (one closed bar every `liveMs`, whatever the
// timeframe, with the bar in progress shown twice before it closes), and paper sessions run on them: manual orders fill at the next bar's open, and a
// strategy session trades a simple moving-average cross so its markers can be designed.
//
// It also exposes `window.tdMockTrading` so the screenshots can show the feed reconnecting and a
// dropped bucket on demand.

import type {
  Candle,
  PaperFill as Fill,
  Instrument,
  InstrumentId,
  PaperOrder as Order,
  SessionEvent,
  SessionState,
  SessionSummary,
  StartSessionRequest,
  Status,
  StrategyInfo,
  TestPlan,
  Timeframe,
  PaperTrade as Trade,
  VerdictRule,
} from '@td/shared'
import { TIMEFRAME_MS, addCandle } from '../src/renderer/src/trading/candles'
import { onLot, onTick } from '../src/renderer/src/trading/order'
import { sameInstrument } from '../src/renderer/src/trading/session'
import { type CandleFeed, MANUAL, type ManualOrder, type SessionFeed, type TradingSource } from '../src/renderer/src/trading/source'
import labFixtures from './lab.json'
import fixtures from './trading.json'

type Row = [number, number, number, number, number, number]

interface Fixtures {
  end: number
  instruments: { instrument: Instrument; ref: number }[]
  history: Record<string, Record<'1m' | '1h', Row[]>>
}

const data = fixtures as unknown as Fixtures

const toCandle = ([open_time, open, high, low, close, volume]: Row): Candle => ({
  open_time,
  open,
  high,
  low,
  close,
  volume,
  quote_volume: Math.round(volume * close * 100) / 100,
})

/** Bars of a coarser timeframe made from finer ones. */
export function aggregate(bars: readonly Candle[], timeframe: Timeframe): Candle[] {
  const span = TIMEFRAME_MS[timeframe]
  const out: Candle[] = []
  for (const b of bars) {
    const t = Math.floor(b.open_time / span) * span
    const last = out.at(-1)
    if (last && last.open_time === t) {
      last.high = Math.max(last.high, b.high)
      last.low = Math.min(last.low, b.low)
      last.close = b.close
      last.volume += b.volume
      last.quote_volume += b.quote_volume
    } else out.push({ ...b, open_time: t })
  }
  return out
}

const roundTo = (x: number, step: number) => Math.round(Math.round(x / step) * step * 1e9) / 1e9

function findInstrument(id: InstrumentId) {
  return data.instruments.find((i) => sameInstrument(i.instrument.id, id))
}

/** The fixture history for an instrument and timeframe, oldest first. */
export function fixtureHistory(id: InstrumentId, timeframe: Timeframe): Candle[] {
  const entry = findInstrument(id)
  const own = data.history[id.symbol]
  const series = own ?? data.history.BTCUSDT
  if (!series || !entry) return []
  const fine: '1m' | '1h' = TIMEFRAME_MS[timeframe] < TIMEFRAME_MS['1h'] ? '1m' : '1h'
  let bars = series[fine].map(toCandle)
  if (!own) {
    // Another instrument: BTCUSDT's path scaled so it ends at this instrument's price.
    const last = bars.at(-1)?.close ?? 1
    const factor = entry.ref / last
    const tick = entry.instrument.tick_size
    bars = bars.map((b) => ({
      ...b,
      open: roundTo(b.open * factor, tick),
      high: roundTo(b.high * factor, tick),
      low: roundTo(b.low * factor, tick),
      close: roundTo(b.close * factor, tick),
      quote_volume: b.quote_volume,
    }))
  }
  return timeframe === fine ? bars : aggregate(bars, timeframe)
}

function rng(seed: number) {
  let s = seed >>> 0
  return () => {
    s = (s + 0x6d2b79f5) >>> 0
    let t = s
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

function seedOf(text: string): number {
  let h = 2166136261
  for (const ch of text) h = Math.imul(h ^ ch.charCodeAt(0), 16777619)
  return h >>> 0
}

/** One instrument and timeframe's live bars, shared by the chart and every session on it. */
/**
 * When the bar in progress is shown between two closes, as fractions of `liveMs`. Late in the
 * cycle, so the screenshots' quiet moment (0.6 of the cycle, harness.mjs `quiet`) still shows the
 * last closed bar with the price's tint gone.
 */
const FORMING_AT = [0.75, 0.9] as const

class Feed {
  readonly bars: Candle[]
  private readonly listeners = new Set<Partial<CandleFeed>>()
  private timer: ReturnType<typeof setInterval> | undefined
  private forming: ReturnType<typeof setTimeout>[] = []
  /** The next bar and the four prices it passes through, worked out before its close. */
  private pending: { candle: Candle; path: number[] } | null = null
  private readonly rand: () => number
  private readonly span: number
  private readonly tick: number

  constructor(
    readonly key: string,
    id: InstrumentId,
    timeframe: Timeframe,
    private readonly liveMs: number,
  ) {
    this.bars = fixtureHistory(id, timeframe)
    this.rand = rng(seedOf(key))
    this.span = TIMEFRAME_MS[timeframe]
    this.tick = findInstrument(id)?.instrument.tick_size ?? 0.01
  }

  /** Each closed bar, and the bar in progress twice between closes. */
  listen(listener: Partial<CandleFeed>): () => void {
    this.listeners.add(listener)
    if (!this.timer) {
      this.timer = setInterval(() => this.next(), this.liveMs)
      this.scheduleForming()
    }
    return () => {
      this.listeners.delete(listener)
      if (this.listeners.size === 0) {
        clearInterval(this.timer)
        this.timer = undefined
        for (const t of this.forming) clearTimeout(t)
        this.forming = []
      }
    }
  }

  private scheduleForming(): void {
    for (const t of this.forming) clearTimeout(t)
    this.forming = FORMING_AT.map((at, i) => setTimeout(() => this.showForming(i + 2), at * this.liveMs))
  }

  /** The next bar, the same for the same seed however often it is shown before it closes. */
  private upcoming(): { candle: Candle; path: number[] } | null {
    if (this.pending) return this.pending
    const last = this.bars.at(-1)
    if (!last) return null
    const vol = 0.0009 * Math.sqrt(this.span / 60_000) ** 0.5
    let price = last.close
    const path: number[] = []
    for (let k = 0; k < 4; k++) {
      price *= Math.exp(vol * (this.rand() + this.rand() + this.rand() - 1.5))
      path.push(price)
    }
    const volume = Math.round(last.volume * (0.6 + this.rand() * 0.8) * 1000) / 1000
    this.pending = { candle: this.barThrough(last, path, volume), path }
    return this.pending
  }

  private barThrough(last: Candle, path: readonly number[], volume: number): Candle {
    const price = path.at(-1) ?? last.close
    return {
      open_time: last.open_time + this.span,
      open: last.close,
      high: roundTo(Math.max(last.close, ...path), this.tick),
      low: roundTo(Math.min(last.close, ...path), this.tick),
      close: roundTo(price, this.tick),
      volume,
      quote_volume: Math.round(volume * price * 100) / 100,
    }
  }

  /** The next bar as it stands after the first `steps` of its four prices. */
  private showForming(steps: number): void {
    const last = this.bars.at(-1)
    const next = this.upcoming()
    if (!last || !next) return
    const volume = Math.round(next.candle.volume * (steps / 4) * 1000) / 1000
    const candle = this.barThrough(last, next.path.slice(0, steps), volume)
    for (const listener of this.listeners) listener.onForming?.(candle)
  }

  private next(): void {
    const next = this.upcoming()
    if (!next) return
    this.pending = null
    this.bars.push(next.candle)
    for (const listener of this.listeners) listener.onClosed?.(next.candle)
    this.scheduleForming()
  }
}

interface OpenTrade {
  side: 'long' | 'short'
  entry_time: number
  entryQty: number
  entryNotional: number
  exitQty: number
  exitNotional: number
  maxQty: number
  fees: number
  bars: number
}

/** A simulated paper session with average-cost accounting. */
class MockSession {
  status: Status = { status: 'warming_up' }
  cash: number
  qty = 0
  avgEntry = 0
  realised = 0
  fees = 0
  mark: number
  readonly equityCurve: { time: number; equity: number }[] = []
  readonly orders: Order[] = []
  readonly fills: Fill[] = []
  readonly trades: Trade[] = []
  readonly dropped: number[] = []
  readonly warnings: string[] = []
  lastBar: Candle | null
  liveFrom: number | null = null
  private open: OpenTrade | null = null
  private nextOrderId = 1
  private warm = 2
  private reconnectBars = 0
  private readonly feeds = new Set<SessionFeed>()
  private off: (() => void) | null = null
  private readonly closes: number[]

  constructor(
    readonly id: string,
    readonly request: StartSessionRequest,
    readonly instrument: Instrument,
    private readonly feed: Feed,
  ) {
    this.cash = request.starting_cash
    this.lastBar = feed.bars.at(-1) ?? null
    this.mark = this.lastBar?.close ?? 0
    this.closes = feed.bars.slice(-30).map((b) => b.close)
  }

  get running(): boolean {
    return this.off !== null
  }

  start(): void {
    this.off = this.feed.listen({ onClosed: (bar) => this.onBar(bar) })
    this.setStatus(this.warm > 0 ? { status: 'warming_up' } : { status: 'live' })
  }

  stop(): void {
    this.off?.()
    this.off = null
    // A market order still waiting for its bar is cancelled with the session.
    for (const order of this.orders) {
      if (order.status.status === 'open') this.updateOrder(order, { status: 'cancelled', reason: 'the session stopped' })
    }
    this.setStatus({ status: 'stopped' })
  }

  watch(feed: SessionFeed): () => void {
    this.feeds.add(feed)
    feed.onState(this.state())
    return () => this.feeds.delete(feed)
  }

  /** Accepts or rejects an order now; an accepted one fills on a later bar. */
  submit(request: ManualOrder): void {
    const order: Order = {
      source: 'manual',
      id: this.nextOrderId++,
      request: {
        instrument: this.instrument.id,
        side: request.side,
        qty: request.qty,
        order_type: request.order_type,
        tif: 'gtc',
        reduce_only: false,
        client_tag: this.request.strategy === MANUAL ? 'manual' : null,
      },
      submitted_at: this.lastBar?.open_time ?? 0,
      decision_close: this.mark,
      attached_stop: null,
      status: { status: 'open' },
    }
    this.orders.push(order)
    const reason = this.refuse(request)
    if (reason) order.status = { status: 'rejected', reason }
    this.emit({ event: 'order', order: { ...order } })
  }

  /** Simulated drop of the venue's stream for three bars. */
  reconnect(): void {
    this.reconnectBars = 3
    this.setStatus({ status: 'reconnecting' })
  }

  /** Simulated bucket the feed left out. */
  dropBucket(): void {
    const time = (this.lastBar?.open_time ?? 0) - TIMEFRAME_MS[this.request.timeframe]
    this.dropped.push(time)
    this.emit({ event: 'dropped', open_time: time })
  }

  summary(): SessionSummary {
    return {
      id: this.id,
      strategy: this.request.strategy,
      instrument: this.instrument.id,
      timeframe: this.request.timeframe,
      status: this.status,
      equity: this.equity(),
    }
  }

  state(): SessionState {
    const trades = [...this.trades]
    if (this.open) trades.push(this.tradeOf(this.open, true))
    return {
      id: this.id,
      config: {
        strategy: this.request.strategy,
        params: this.request.params,
        instrument: this.instrument,
        timeframe: this.request.timeframe,
        starting_cash: this.request.starting_cash,
        slippage_bps: this.request.slippage_bps,
        fees: null,
        // The request has no leverage, so the engine uses its default of 1x.
        max_leverage: 1,
      },
      status: this.status,
      cash: this.cash,
      equity: this.equity(),
      position: {
        qty: this.qty,
        avg_entry: this.avgEntry,
        mark_price: this.mark,
        unrealised_pnl: this.unrealised(),
        realised_pnl: this.realised,
        fees_paid: this.fees,
        funding_paid: 0,
      },
      equity_curve: [...this.equityCurve],
      open_orders: this.orders.filter((o) => o.status.status === 'open').map((o) => ({ ...o })),
      fills: [...this.fills],
      trades,
      funding: [],
      live_from: this.liveFrom,
      last_bar: this.lastBar,
      dropped: [...this.dropped],
      warnings: [...this.warnings],
    }
  }

  private unrealised(): number {
    return this.qty * (this.mark - this.avgEntry)
  }

  private equity(): number {
    return this.request.starting_cash + this.realised + this.unrealised() - this.fees
  }

  private refuse(request: ManualOrder): string | null {
    if (!this.running) return 'the session is not running'
    const price = request.order_type.type === 'limit' ? request.order_type.price : this.mark
    const notional = request.qty * price
    if (this.instrument.id.kind === 'spot') {
      if (request.side === 'sell' && request.qty > this.qty + 1e-12) {
        return `spot sessions cannot sell more than they hold (${this.qty} ${this.instrument.base})`
      }
      const fee = notional * this.instrument.fees.taker
      if (request.side === 'buy' && notional + fee > this.cash) {
        return `not enough cash: the order needs ${(notional + fee).toFixed(2)} ${this.instrument.quote} and the session has ${this.cash.toFixed(2)}`
      }
    } else if (notional > 5 * this.equity()) {
      return 'not enough margin: paper perpetuals allow at most five times equity'
    }
    return null
  }

  private onBar(bar: Candle): void {
    if (this.reconnectBars > 0) {
      // The stream is down: this bar arrives later, as a backfill, when it reconnects.
      this.reconnectBars -= 1
      if (this.reconnectBars === 0) this.setStatus({ status: 'live' })
      else return
    }
    this.lastBar = bar
    this.mark = bar.close
    this.closes.push(bar.close)
    if (this.closes.length > 30) this.closes.shift()
    for (const order of this.orders) {
      if (order.status.status === 'open') this.tryFill(order, bar)
    }
    if (this.open) this.open.bars += 1
    if (this.warm > 0) {
      this.warm -= 1
      this.emit({ event: 'bar', bar, warm_up: true, equity: null })
      if (this.warm === 0) {
        this.liveFrom = bar.open_time
        this.setStatus({ status: 'live' })
      }
      return
    }
    if (this.request.strategy !== MANUAL) this.decide()
    this.equityCurve.push({ time: bar.open_time, equity: this.equity() })
    this.emit({ event: 'bar', bar, warm_up: false, equity: this.equity() })
  }

  /** A moving-average cross: long when the 5-bar average is above the 20-bar one, flat or short below. */
  private decide(): void {
    if (this.closes.length < 21) return
    const avg = (n: number) => this.closes.slice(-n).reduce((a, b) => a + b, 0) / n
    const want = avg(5) > avg(20) ? 1 : this.instrument.id.kind === 'perpetual' ? -1 : 0
    const unit = roundTo((this.request.starting_cash * 0.5) / this.mark, this.instrument.lot_size)
    const target = want * unit
    const delta = roundTo(target - this.qty, this.instrument.lot_size)
    if (Math.abs(delta) < this.instrument.min_qty || this.orders.some((o) => o.status.status === 'open')) return
    this.submit({ side: delta > 0 ? 'buy' : 'sell', qty: Math.abs(delta), order_type: { type: 'market' } })
  }

  private tryFill(order: Order, bar: Candle): void {
    const { side, qty, order_type } = order.request
    let price: number
    let liquidity: 'maker' | 'taker' = 'taker'
    if (order_type.type === 'market') {
      const slip = (this.request.slippage_bps / 10_000) * bar.open
      price = roundTo(side === 'buy' ? bar.open + slip : bar.open - slip, this.instrument.tick_size)
    } else if (order_type.type === 'limit') {
      if (side === 'buy' && bar.low > order_type.price) return
      if (side === 'sell' && bar.high < order_type.price) return
      price = side === 'buy' ? Math.min(bar.open, order_type.price) : Math.max(bar.open, order_type.price)
      liquidity = 'maker'
    } else return
    const fee = qty * price * this.instrument.fees[liquidity]
    const fill: Fill = { source: 'manual', order_id: order.id, instrument: this.instrument.id, side, time: bar.open_time, price, qty, fee, liquidity }
    this.applyFill(fill)
    this.fills.push(fill)
    this.emit({ event: 'fill', fill })
    this.updateOrder(order, { status: 'filled' })
  }

  private applyFill({ side, qty, price, fee, time }: Fill): void {
    const signed = side === 'buy' ? qty : -qty
    this.fees += fee
    if (this.instrument.id.kind === 'spot') this.cash -= signed * price + fee
    else this.cash -= fee
    let rest = qty
    if (this.qty !== 0 && Math.sign(this.qty) !== Math.sign(signed)) {
      const closing = Math.min(qty, Math.abs(this.qty))
      const pnl = closing * (price - this.avgEntry) * Math.sign(this.qty)
      this.realised += pnl
      if (this.instrument.id.kind !== 'spot') this.cash += pnl
      this.qty = roundTo(this.qty + Math.sign(signed) * closing, this.instrument.lot_size)
      rest = roundTo(qty - closing, this.instrument.lot_size)
      if (this.open) {
        this.open.exitQty += closing
        this.open.exitNotional += closing * price
        this.open.fees += fee * (closing / qty)
      }
      if (this.qty === 0) {
        this.avgEntry = 0
        if (this.open) this.trades.push(this.tradeOf(this.open, false, time))
        this.open = null
      }
    }
    if (rest > 0) {
      const held = Math.abs(this.qty)
      this.avgEntry = (this.avgEntry * held + price * rest) / (held + rest)
      this.qty = roundTo(this.qty + Math.sign(signed) * rest, this.instrument.lot_size)
      this.open ??= {
        side: signed > 0 ? 'long' : 'short',
        entry_time: time,
        entryQty: 0,
        entryNotional: 0,
        exitQty: 0,
        exitNotional: 0,
        maxQty: 0,
        fees: 0,
        bars: 0,
      }
      this.open.entryQty += rest
      this.open.entryNotional += rest * price
      this.open.fees += fee * (rest / qty)
      this.open.maxQty = Math.max(this.open.maxQty, Math.abs(this.qty))
    }
  }

  private tradeOf(t: OpenTrade, open: boolean, exitTime?: number): Trade {
    const entry_price = t.entryNotional / t.entryQty
    const remaining = open ? Math.abs(this.qty) : 0
    const exitQty = t.exitQty + remaining
    const exit_price = exitQty > 0 ? (t.exitNotional + remaining * this.mark) / exitQty : this.mark
    const sign = t.side === 'long' ? 1 : -1
    const gross_pnl = sign * exitQty * (exit_price - entry_price)
    const net_pnl = gross_pnl - t.fees
    return {
      source: 'manual',
      side: t.side,
      entry_time: t.entry_time,
      exit_time: exitTime ?? this.lastBar?.open_time ?? t.entry_time,
      open,
      qty: t.maxQty,
      entry_price,
      exit_price,
      gross_pnl,
      fees: t.fees,
      funding: 0,
      net_pnl,
      return_on_notional: net_pnl / (entry_price * t.maxQty),
      bars_held: t.bars,
    }
  }

  private updateOrder(order: Order, status: Order['status']): void {
    order.status = status
    this.emit({ event: 'order', order: { ...order } })
  }

  private setStatus(status: Status): void {
    this.status = status
    this.emit({ event: 'status', status })
  }

  private emit(event: SessionEvent): void {
    for (const feed of this.feeds) feed.onEvent(event)
  }
}

export interface MockTradingOptions {
  /** Milliseconds between live bars, whatever the timeframe. */
  liveMs?: number
  /** How long the instrument lists take, like a call to the venue. */
  listMs?: number
}

declare global {
  interface Window {
    /** Mock only: lets the screenshots show a reconnecting feed and a dropped bucket. */
    tdMockTrading?: { reconnect(): void; dropBucket(): void }
  }
}

// The test plan and verdict rule every mock strategy carries, shaped like sma_cross's own. Trading
// reads only the names and parameters; the Lab has its own fixtures.
const MOCK_PLAN: TestPlan = {
  instruments: [
    { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' },
    { venue: 'binance', symbol: 'ETHUSDT', kind: 'spot' },
  ],
  timeframe: '1h',
  period: { from: Date.UTC(2023, 0, 1), to: Date.UTC(2025, 0, 1) },
  backtest: { starting_cash: 10_000, max_leverage: 1, slippage: [] },
  sweep: {
    fixed: {},
    axes: [
      { name: 'fast', space: { kind: 'range', from: 5, to: 20, step: 5 } },
      { name: 'slow', space: { kind: 'range', from: 30, to: 70, step: 10 } },
    ],
    objective: { metric: 'sharpe', direction: 'maximize' },
    min_trades: 10,
  },
  walk_forward: { train_days: 180, test_days: 60 },
  out_of_sample: { fraction: 0.7 },
  evidence: {
    min_sample: { trades: 30, days: 180, windows: 4 },
    luck: { permutations: 199, pass_p: 0.05, inconclusive_p: 0.1 },
    cost_stress: 2,
    plateau: { min_median_ratio: 0.6, min_positive_share: 0.8 },
    start_offsets: 8,
  },
}

const MOCK_RULE: VerdictRule = {
  kind: 'portfolio',
  benchmark: 'buy_and_hold',
  limits: {
    min_sharpe: 1,
    max_drawdown: 0.3,
    max_drawdown_vs_benchmark: 0.75,
    min_alpha: 0,
    max_p_mean_not_positive: 0.1,
  },
}

export function createMockTradingSource({ liveMs = 1500, listMs = 400 }: MockTradingOptions = {}): TradingSource {
  const feeds = new Map<string, Feed>()
  const sessions = new Map<string, MockSession>()

  const feedFor = (id: InstrumentId, timeframe: Timeframe) => {
    const key = `${id.venue}:${id.symbol}:${id.kind}:${timeframe}`
    let feed = feeds.get(key)
    if (!feed) {
      feed = new Feed(key, id, timeframe, liveMs)
      feeds.set(key, feed)
    }
    return feed
  }

  const sessionOf = (id: string) => {
    const session = sessions.get(id)
    if (!session) throw new Error(`no session ${id}`)
    return session
  }

  const strategies: StrategyInfo[] = (labFixtures as { strategies: { name: string; description: string }[] }).strategies.map(
    (s) => ({
      name: s.name,
      description: s.description,
      params: [],
      kind: MOCK_RULE.kind,
      plan: MOCK_PLAN,
      rule: MOCK_RULE,
    }),
  )

  if (typeof window !== 'undefined') {
    const running = () => [...sessions.values()].filter((s) => s.running).at(-1)
    window.tdMockTrading = {
      reconnect: () => running()?.reconnect(),
      dropBucket: () => running()?.dropBucket(),
    }
  }

  return {
    orders: { available: true },
    manualSessions: { available: true },
    instruments: () =>
      new Promise((resolve) =>
        setTimeout(() => resolve({ instruments: data.instruments.map((i) => i.instrument), problems: [] }), listMs),
      ),
    strategies: async () => strategies,
    history: async (id, timeframe) => feedFor(id, timeframe).bars.slice(),
    candles: (id, timeframe, feed) => feedFor(id, timeframe).listen(feed),
    sessions: async () => [...sessions.values()].map((s) => s.summary()),
    session: async (id) => sessionOf(id).state(),
    async startSession(request) {
      if (request.strategy !== MANUAL && !strategies.some((s) => s.name === request.strategy)) {
        throw new Error(`no strategy named ${request.strategy}`)
      }
      if (!(request.starting_cash > 0)) throw new Error('starting cash must be above zero')
      const instrument = findInstrument(request.instrument)?.instrument
      if (!instrument) throw new Error(`${request.instrument.symbol} is not an instrument the venue lists`)
      const id = `${Date.now()}-${Math.floor(Math.random() * 0xffffffff).toString(16).padStart(8, '0')}`
      const session = new MockSession(id, request, instrument, feedFor(request.instrument, request.timeframe))
      sessions.set(id, session)
      session.start()
      return session.summary()
    },
    async stopSession(id) {
      const session = sessionOf(id)
      if (!session.running) throw new Error('the session is not running')
      session.stop()
      return session.summary()
    },
    async resumeSession(id) {
      const session = sessionOf(id)
      if (session.running) throw new Error('the session is already running')
      session.start()
      return session.summary()
    },
    watchSession: (id, feed) => sessionOf(id).watch(feed),
    async placeOrder(sessionId, order) {
      const session = sessionOf(sessionId)
      const lot = session.instrument.lot_size
      if (!onLot(order.qty, lot)) throw new Error(`the size must be a multiple of ${lot}`)
      const tick = session.instrument.tick_size
      if (order.order_type.type === 'limit' && !onTick(order.order_type.price, tick)) {
        throw new Error(`the limit price must be a multiple of ${tick}`)
      }
      session.submit(order)
    },
  }
}

let shared: TradingSource | null = null

/** One mock per page, so sessions outlive the section being hidden and shown again. */
export function mockTradingSource(): TradingSource {
  shared ??= createMockTradingSource()
  return shared
}

