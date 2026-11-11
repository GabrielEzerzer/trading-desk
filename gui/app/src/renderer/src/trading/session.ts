// Following a paper session: which one the section shows, and how its events change what is shown.

import type { InstrumentId, SessionEvent, SessionState, SessionSummary, Status } from '@td/shared'

export function sameInstrument(a: InstrumentId, b: InstrumentId): boolean {
  return a.venue === b.venue && a.symbol === b.symbol && a.kind === b.kind
}

export function isRunning(status: Status): boolean {
  return status.status === 'warming_up' || status.status === 'live' || status.status === 'reconnecting'
}

/**
 * The session to show for an instrument: a running one if any, else the latest. Sessions arrive
 * oldest first (ids start with their creation time), so the latest is the last.
 */
export function pickSession(sessions: readonly SessionSummary[], instrument: InstrumentId): SessionSummary | null {
  const mine = sessions.filter((s) => sameInstrument(s.instrument, instrument))
  return mine.findLast((s) => isRunning(s.status)) ?? mine.at(-1) ?? null
}

const MAX_WARNINGS = 20

/**
 * Applies one event. Status, dropped buckets and warnings change the state here. Fills, orders,
 * funding and bars move the position, which only the engine computes, so they ask for the full
 * state again (`refetch`).
 *
 * shortcut: refetches GET /sessions/{id} after those events instead of applying them; apply them
 * here if a session's state ever grows too large to fetch once per bar.
 */
export function applySessionEvent(state: SessionState, event: SessionEvent): { state: SessionState; refetch: boolean } {
  return { state: applyInPlace(state, event), refetch: refetches(event) }
}

/** Whether an event moves the position, so the full state must be fetched again. */
export function refetches(event: SessionEvent): boolean {
  return event.event === 'fill' || event.event === 'order' || event.event === 'funding' || event.event === 'bar'
}

function applyInPlace(state: SessionState, event: SessionEvent): SessionState {
  switch (event.event) {
    case 'status':
      return { ...state, status: event.status }
    case 'dropped':
      return { ...state, dropped: [...state.dropped, event.open_time] }
    case 'warning':
      return { ...state, warnings: [...state.warnings, event.message].slice(-MAX_WARNINGS) }
    default:
      return state
  }
}

/** The order rejection an event reports, if it is one. */
export function rejection(event: SessionEvent): string | null {
  if (event.event === 'order' && event.order.status.status === 'rejected') return event.order.status.reason
  return null
}
