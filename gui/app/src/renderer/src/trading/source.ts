// Where Trading's data comes from. The section only talks to a `TradingSource`: the engine one uses
// the engine's API and WebSocket through main, and the mock one (app/mock/trading.ts, chosen with
// TD_MOCK=1, like the Lab's) plays fixtures and a simulated session.

import type {
  Candle,
  Instrument,
  InstrumentId,
  InstrumentKind,
  OrderType,
  ServerMessage,
  SessionEvent,
  SessionState,
  SessionSummary,
  Side,
  StartSessionRequest,
  StrategyInfo,
  Timeframe,
  Venue,
} from '@td/shared'
import { api } from '../api'

/** The strategy name of a session where you place every order yourself. */
export const MANUAL = 'manual'

// shortcut: the body of a manual order is decided with its endpoint (#36); align this then.
export interface ManualOrder {
  side: Side
  qty: number
  order_type: OrderType
}

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

/** A capability the source may lack, with the sentence that says so where the control is. */
export type Capability = { available: true } | { available: false; reason: string }

export interface TradingSource {
  /** Manual orders. */
  readonly orders: Capability
  /** Sessions with the "Manual" strategy, where only your own orders trade. */
  readonly manualSessions: Capability
  instruments(): Promise<InstrumentList>
  strategies(): Promise<StrategyInfo[]>
  /** Closed bars before now, oldest first; empty when the source has no history. */
  history(id: InstrumentId, timeframe: Timeframe): Promise<Candle[]>
  /** Each closed bar from now on. Returns a function that stops it. */
  candles(id: InstrumentId, timeframe: Timeframe, onCandle: (candle: Candle) => void): () => void
  sessions(): Promise<SessionSummary[]>
  session(id: string): Promise<SessionState>
  startSession(request: StartSessionRequest): Promise<SessionSummary>
  stopSession(id: string): Promise<SessionSummary>
  resumeSession(id: string): Promise<SessionSummary>
  /** Follows one session. Returns a function that stops it. */
  watchSession(id: string, feed: SessionFeed): () => void
  placeOrder(sessionId: string, order: ManualOrder): Promise<void>
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

const VENUE_NAMES: Record<Venue, string> = { binance: 'Binance', bybit: 'Bybit' }

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
    else problems.push(`The ${VENUE_NAMES[venue]} ${kind} list did not load.`)
  })
  if (instruments.length === 0 && problems.length === LISTS.length) {
    const first = results.find((r) => r.status === 'rejected')
    throw first?.reason instanceof Error ? first.reason : new Error('No instrument list loaded.')
  }
  return { instruments, problems }
}

const UNTIL_36 = 'Manual orders arrive with the order endpoint (#36).'

/** Subscribes to a topic and passes on its messages; returns the function that ends it. */
function follow(topic: string, onMessage: (message: ServerMessage) => void): () => void {
  const off = window.td.onMessage(onMessage)
  void window.td.subscribe(topic)
  return () => {
    off()
    void window.td.unsubscribe(topic)
  }
}

/**
 * The real engine. It has no candle history and no manual orders yet (#36), so the chart starts
 * empty and fills from the live stream, and the composer says orders are not available.
 */
export const engineTradingSource: TradingSource = {
  orders: { available: false, reason: UNTIL_36 },
  manualSessions: { available: false, reason: 'Manual sessions arrive with the order endpoint (#36).' },
  instruments: () => loadInstrumentLists(async (venue, kind) => (await api.instruments(venue, kind)).instruments),
  strategies: () => api.strategies(),
  // shortcut: the engine serves no candle history yet (#36); the chart fills from the live stream.
  history: async () => [],
  candles(id, timeframe, onCandle) {
    const topic = candleTopic(id, timeframe)
    return follow(topic, (m) => {
      if (m.type === 'candle' && m.topic === topic) onCandle(m.candle)
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
  placeOrder: () => Promise.reject(new Error(UNTIL_36)),
}

/** The engine, or the fixtures in app/mock/ when the app was started with TD_MOCK=1. */
export async function chooseTradingSource(): Promise<TradingSource> {
  if (!window.td.mock) return engineTradingSource
  const { mockTradingSource } = await import('../../../../mock/trading')
  return mockTradingSource()
}
