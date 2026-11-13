// Where Trading's data comes from. The section only talks to a `TradingSource`: the engine one uses
// the engine's API and WebSocket through main, and the mock one (app/mock/trading.ts, chosen with
// TD_MOCK=1, like the Lab's) plays fixtures and a simulated session.

import type {
  Candle,
  Instrument,
  InstrumentId,
  InstrumentKind,
  ManualOrder,
  OrderId,
  ServerMessage,
  SessionEvent,
  SessionState,
  SessionSummary,
  StartSessionRequest,
  StrategyInfo,
  Timeframe,
  Venue,
} from '@td/shared'
import { api } from '../api'
import { formatVenue } from '../lab/format'

/** The strategy name of a session where you place every order yourself (td-server's `MANUAL_STRATEGY`). */
export const MANUAL = 'none'

/** The body of `POST /sessions/{id}/orders`. */
export type { ManualOrder }

export interface InstrumentList {
  instruments: Instrument[]
  /** Venue lists that did not load, as sentences. The others are still shown. */
  problems: string[]
}

/** What a `session:` subscription delivers: the full state first and again after a gap, then changes. */
export interface SessionFeed {
  onState(state: SessionState): void
  onEvent(event: SessionEvent): void
}

/** What a `candles:` subscription delivers: the bar still forming as it changes, and each bar as it closes. */
export interface CandleFeed {
  /** A closed bar. It replaces the forming bar with the same open time. */
  onClosed(candle: Candle): void
  /** The bar in progress so far. It is drawn as the last candle until its closed bar arrives. */
  onForming(candle: Candle): void
}

/** How many closed bars a chart loads before the live stream continues it. */
export const HISTORY_BARS = 500

/** A capability the source may lack, with the sentence that says so where the control is. */
export type Capability = { available: true } | { available: false; reason: string }

export interface TradingSource {
  /** Manual orders. */
  readonly orders: Capability
  /** Sessions with the "Manual" strategy, where only your own orders trade. */
  readonly manualSessions: Capability
  instruments(): Promise<InstrumentList>
  strategies(): Promise<StrategyInfo[]>
  /** About the last `HISTORY_BARS` closed bars before now, oldest first. */
  history(id: InstrumentId, timeframe: Timeframe): Promise<Candle[]>
  /** The bar forming and each closed bar from now on. Returns a function that stops it. */
  candles(id: InstrumentId, timeframe: Timeframe, feed: CandleFeed): () => void
  sessions(): Promise<SessionSummary[]>
  session(id: string): Promise<SessionState>
  startSession(request: StartSessionRequest): Promise<SessionSummary>
  stopSession(id: string): Promise<SessionSummary>
  resumeSession(id: string): Promise<SessionSummary>
  /** Follows one session. Returns a function that stops it. */
  watchSession(id: string, feed: SessionFeed): () => void
  placeOrder(sessionId: string, order: ManualOrder): Promise<void>
  /** Cancels an open manual order. */
  cancelOrder(sessionId: string, orderId: OrderId): Promise<void>
}

export function candleTopic(id: InstrumentId, timeframe: Timeframe): string {
  return `candles:${id.venue}:${id.symbol}:${id.kind}:${timeframe}`
}

const LISTS: [Venue, InstrumentKind][] = [
  ['binance', 'spot'],
  ['binance', 'perpetual'],
  ['bybit', 'spot'],
  ['bybit', 'perpetual'],
]

/** Loads the four venue lists; one that fails is named in `problems`, and only all failing throws. */
export async function loadInstrumentLists(
  load: (venue: Venue, kind: InstrumentKind) => Promise<Instrument[]>,
): Promise<InstrumentList> {
  const results = await Promise.allSettled(LISTS.map(([venue, kind]) => load(venue, kind)))
  const instruments: Instrument[] = []
  const problems: string[] = []
  results.forEach((result, i) => {
    const [venue, kind] = LISTS[i] as [Venue, InstrumentKind]
    if (result.status === 'fulfilled') instruments.push(...result.value)
    else problems.push(`The ${formatVenue(venue)} ${kind} list did not load.`)
  })
  if (instruments.length === 0 && problems.length === LISTS.length) {
    const first = results.find((r) => r.status === 'rejected')
    throw first?.reason instanceof Error ? first.reason : new Error('No instrument list loaded.')
  }
  return { instruments, problems }
}

/** Subscribes to a topic and passes on its messages; returns the function that ends it. */
function follow(topic: string, onMessage: (message: ServerMessage) => void): () => void {
  const off = window.td.onMessage(onMessage)
  void window.td.subscribe(topic)
  return () => {
    off()
    void window.td.unsubscribe(topic)
  }
}

/** The real engine. */
export const engineTradingSource: TradingSource = {
  orders: { available: true },
  manualSessions: { available: true },
  instruments: () => loadInstrumentLists(async (venue, kind) => (await api.instruments(venue, kind)).instruments),
  strategies: () => api.strategies(),
  history: async (id, timeframe) => (await api.candles(id, timeframe, HISTORY_BARS)).candles,
  candles(id, timeframe, feed) {
    const topic = candleTopic(id, timeframe)
    return follow(topic, (m) => {
      if (m.type === 'candle' && m.topic === topic) feed.onClosed(m.candle)
      else if (m.type === 'candle_forming' && m.topic === topic) feed.onForming(m.candle)
    })
  },
  sessions: () => api.sessions(),
  session: (id) => api.session(id),
  startSession: (request) => api.startSession(request),
  stopSession: (id) => api.stopSession(id),
  resumeSession: (id) => api.resumeSession(id),
  watchSession(id, feed) {
    return follow(`session:${id}`, (m) => {
      if (m.type === 'session_state' && m.session.id === id) feed.onState(m.session)
      else if (m.type === 'session' && m.id === id) feed.onEvent(m.event)
    })
  },
  async placeOrder(sessionId, order) {
    await api.placeOrder(sessionId, order)
  },
  async cancelOrder(sessionId, orderId) {
    await api.cancelOrder(sessionId, orderId)
  },
}

/** The engine, or the fixtures in app/mock/ when the app was started with TD_MOCK=1. */
export async function chooseTradingSource(): Promise<TradingSource> {
  if (!window.td.mock) return engineTradingSource
  const { mockTradingSource } = await import('../../../../mock/trading')
  return mockTradingSource()
}
