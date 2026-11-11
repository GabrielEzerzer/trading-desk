// Trading's data, read through its `TradingSource`.

import type { Candle, InstrumentId, SessionState, SessionSummary, StartSessionRequest, Timeframe } from '@td/shared'
import { useCallback, useEffect, useRef, useState } from 'react'
import { sentence } from '../engine'
import { type CandleSeries, EMPTY_SERIES, withClosed, withForming, withHistory } from './candles'
import { applySessionEvent, pickSession, refetches, rejection } from './session'
import type { InstrumentList, TradingSource } from './source'

export const errorText = (e: unknown) => sentence(e instanceof Error ? e.message : String(e))

export function useInstruments(source: TradingSource | null) {
  const [list, setList] = useState<InstrumentList | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [attempt, setAttempt] = useState(0)
  useEffect(() => {
    if (!source) return
    let live = true
    setError(null)
    source.instruments().then(
      (l) => live && setList(l),
      (e: unknown) => live && setError(errorText(e)),
    )
    return () => {
      live = false
    }
  }, [source, attempt])
  return { list, error, retry: () => setAttempt((n) => n + 1) }
}

export interface CandleView extends CandleSeries {
  /** The history is loading; live bars may already be drawn. */
  loading: boolean
  /** Why the history did not load, in the source's words; live bars still fill the chart. */
  error: string | null
}

const EMPTY_VIEW: CandleView = { ...EMPTY_SERIES, loading: true, error: null }

/**
 * Follows one instrument and timeframe: subscribes to its live bars, then loads the history and
 * merges it under them. `onChange` gets every new view until `stop`; `retry` loads the history
 * again after it failed. A history answer or a bar that arrives after `stop` is dropped.
 */
export function followCandles(
  source: Pick<TradingSource, 'history' | 'candles'>,
  id: InstrumentId,
  timeframe: Timeframe,
  onChange: (view: CandleView) => void,
): { stop(): void; retry(): void } {
  let view = EMPTY_VIEW
  let live = true
  const set = (next: CandleView) => {
    view = next
    onChange(view)
  }
  const stopFeed = source.candles(id, timeframe, {
    onClosed: (candle) => live && set(withClosed(view, candle)),
    onForming: (candle) => live && set(withForming(view, candle)),
  })
  const load = () => {
    set({ ...view, loading: true, error: null })
    source.history(id, timeframe).then(
      (history) => live && set({ ...withHistory(view, history), loading: false }),
      (e: unknown) => live && set({ ...view, loading: false, error: errorText(e) }),
    )
  }
  load()
  return {
    stop() {
      live = false
      stopFeed()
    },
    retry() {
      if (live && !view.loading) load()
    },
  }
}

/** The chart's bars for an instrument and timeframe: history, then the live stream's bars. */
export function useCandles(
  source: TradingSource | null,
  id: InstrumentId,
  timeframe: Timeframe,
): CandleView & { retry(): void } {
  const { venue, symbol, kind } = id
  const key = `${venue}:${symbol}:${kind}:${timeframe}`
  // Keyed, so the render right after a switch shows nothing of the previous choice.
  const [state, setState] = useState<{ key: string; view: CandleView }>({ key, view: EMPTY_VIEW })
  const follow = useRef<{ retry(): void } | null>(null)
  useEffect(() => {
    if (!source) return
    const shown = `${venue}:${symbol}:${kind}:${timeframe}`
    const f = followCandles(source, { venue, symbol, kind }, timeframe, (view) => setState({ key: shown, view }))
    follow.current = f
    return () => {
      follow.current = null
      f.stop()
    }
  }, [source, venue, symbol, kind, timeframe])
  const retry = useCallback(() => follow.current?.retry(), [])
  return { ...(state.key === key ? state.view : EMPTY_VIEW), retry }
}

export interface PaperSession {
  /** False while the session list loads, and when it did not load. */
  loaded: boolean
  /** Why the session list did not load, as a sentence; `reload` tries again. */
  loadError: string | null
  summary: SessionSummary | null
  state: SessionState | null
  /** The latest rejection, with a counter so the same reason twice still shows. */
  rejected: { reason: string; at: number } | null
  /** The latest dropped bucket or warning, as a sentence, with when it arrived. */
  notice: { text: string; at: number } | null
  error: string | null
  busy: boolean
  start(request: StartSessionRequest): Promise<boolean>
  stop(): void
  resume(): void
  reload(): void
}

/** The paper session shown for an instrument, followed live. */
export function usePaperSession(source: TradingSource | null, id: InstrumentId): PaperSession {
  const [sessions, setSessions] = useState<SessionSummary[] | null>(null)
  const [state, setState] = useState<SessionState | null>(null)
  const [rejected, setRejected] = useState<PaperSession['rejected']>(null)
  const [notice, setNotice] = useState<PaperSession['notice']>(null)
  const [error, setError] = useState<string | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [revision, setRevision] = useState(0)
  const counter = useRef(0)
  // A double click runs the second call before React re-renders with `busy`, so `act` checks this.
  const inFlight = useRef(false)

  useEffect(() => {
    if (!source) return
    let live = true
    setLoadError(null)
    source.sessions().then(
      (list) => live && setSessions(list),
      (e: unknown) => {
        if (!live) return
        // An empty list would read as "no session" and offer to start one beside a running one.
        setSessions(null)
        setLoadError(errorText(e))
      },
    )
    return () => {
      live = false
    }
  }, [source, revision])

  const summary = sessions ? pickSession(sessions, id) : null
  const sessionId = summary?.id ?? null

  useEffect(() => {
    setState(null)
    setRejected(null)
    setNotice(null)
    if (!source || !sessionId) return
    let live = true
    let refetch: ReturnType<typeof setTimeout> | undefined
    const scheduleRefetch = () => {
      clearTimeout(refetch)
      // Several events arrive per bar; one fetch after the burst covers them.
      refetch = setTimeout(() => {
        source.session(sessionId).then(
          (s) => live && setState(s),
          () => {},
        )
      }, 50)
    }
    const stop = source.watchSession(sessionId, {
      onState: (s) => live && setState(s),
      onEvent: (event) => {
        if (!live) return
        const reason = rejection(event)
        if (reason) setRejected({ reason: sentence(reason), at: ++counter.current })
        if (event.event === 'dropped') {
          setNotice({ text: `The feed left out the bar at ${clock(event.open_time)}: too many minutes were missing.`, at: Date.now() })
        } else if (event.event === 'warning') {
          setNotice({ text: sentence(event.message), at: Date.now() })
        }
        setState((s) => (s ? applySessionEvent(s, event).state : s))
        if (refetches(event)) scheduleRefetch()
        if (event.event === 'status') setSessions((list) => list?.map((x) => (x.id === sessionId ? { ...x, status: event.status } : x)) ?? list)
      },
    })
    return () => {
      live = false
      clearTimeout(refetch)
      stop()
    }
  }, [source, sessionId])

  const act = useCallback(
    async (run: () => Promise<unknown>): Promise<boolean> => {
      if (inFlight.current) return false
      inFlight.current = true
      setBusy(true)
      setError(null)
      try {
        await run()
        setRevision((n) => n + 1)
        return true
      } catch (e) {
        setError(errorText(e))
        return false
      } finally {
        inFlight.current = false
        setBusy(false)
      }
    },
    [],
  )

  return {
    loaded: sessions !== null,
    loadError,
    summary,
    state: state && state.id === sessionId ? state : null,
    rejected,
    notice,
    error,
    busy,
    start: (request) => (source ? act(() => source.startSession(request)) : Promise.resolve(false)),
    stop: () => {
      if (source && sessionId) void act(() => source.stopSession(sessionId))
    },
    resume: () => {
      if (source && sessionId) void act(() => source.resumeSession(sessionId))
    },
    reload: () => setRevision((n) => n + 1),
  }
}

/** "14:05", UTC like every time in the desk. */
export function clock(ms: number): string {
  const d = new Date(ms)
  return `${String(d.getUTCHours()).padStart(2, '0')}:${String(d.getUTCMinutes()).padStart(2, '0')}`
}
