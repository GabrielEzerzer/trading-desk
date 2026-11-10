// Trading's data, read through its `TradingSource`.

import type { Candle, InstrumentId, SessionState, SessionSummary, StartSessionRequest, Timeframe } from '@td/shared'
import { useCallback, useEffect, useRef, useState } from 'react'
import { sentence } from '../engine'
import { addCandle, mergeCandles } from './candles'
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

export interface CandleView {
  candles: Candle[]
  /** Bumps when the list changed anywhere but its end, so the chart redraws it whole. */
  revision: number
}

/** History, if the source has any, then the live stream's closed bars, merged by open time. */
export function useCandles(source: TradingSource | null, id: InstrumentId, timeframe: Timeframe): CandleView {
  const [view, setView] = useState<CandleView>({ candles: [], revision: 0 })
  const { venue, symbol, kind } = id
  useEffect(() => {
    if (!source) return
    const instrument = { venue, symbol, kind }
    let live = true
    // Bars that arrive before the history are kept and merged with it.
    setView((v) => ({ candles: [], revision: v.revision + 1 }))
    const stop = source.candles(instrument, timeframe, (candle) => {
      if (!live) return
      setView((v) => {
        const { list, change } = addCandle(v.candles, candle)
        return { candles: list, revision: change === 'reset' ? v.revision + 1 : v.revision }
      })
    })
    source.history(instrument, timeframe).then(
      (history) => live && setView((v) => ({ candles: mergeCandles(history, v.candles), revision: v.revision + 1 })),
      // A failed history leaves the live stream to fill the chart.
      () => {},
    )
    return () => {
      live = false
      stop()
    }
  }, [source, venue, symbol, kind, timeframe])
  return view
}

export interface PaperSession {
  /** Null while the session list loads. */
  loaded: boolean
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
}

/** The paper session shown for an instrument, followed live. */
export function usePaperSession(source: TradingSource | null, id: InstrumentId): PaperSession {
  const [sessions, setSessions] = useState<SessionSummary[] | null>(null)
  const [state, setState] = useState<SessionState | null>(null)
  const [rejected, setRejected] = useState<PaperSession['rejected']>(null)
  const [notice, setNotice] = useState<PaperSession['notice']>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [revision, setRevision] = useState(0)
  const counter = useRef(0)

  useEffect(() => {
    if (!source) return
    let live = true
    source.sessions().then(
      (list) => live && setSessions(list),
      (e: unknown) => {
        if (!live) return
        setSessions([])
        setError(errorText(e))
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
        setBusy(false)
      }
    },
    [],
  )

  return {
    loaded: sessions !== null,
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
  }
}

/** "14:05", UTC like every time in the desk. */
export function clock(ms: number): string {
  const d = new Date(ms)
  return `${String(d.getUTCHours()).padStart(2, '0')}:${String(d.getUTCMinutes()).padStart(2, '0')}`
}
