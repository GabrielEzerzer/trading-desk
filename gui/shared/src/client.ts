// A typed client with one function per engine route. It takes the function that sends a request:
// in the renderer that goes through `window.td`, in the main process straight to the engine.

import type {
  BacktestConfig,
  CoverageResponse,
  DownloadRequest,
  HealthResponse,
  InstrumentId,
  InstrumentKind,
  InstrumentsResponse,
  JobCreated,
  JobInfo,
  RunDetail,
  RunSummary,
  SessionState,
  SessionSummary,
  StartSessionRequest,
  StrategyInfo,
  Venue,
} from './api-types'
import type { ApiRequest, ApiResult } from './bridge'

/** The engine's `{"error":{"code","message"}}`, or a failure to reach it (`status` 0). */
export class ApiError extends Error {
  readonly status: number
  readonly code: string

  constructor(status: number, code: string, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
  }
}

export type Send = (request: ApiRequest) => Promise<ApiResult>

export function createClient(send: Send) {
  async function call<T>(request: ApiRequest): Promise<T> {
    const result = await send(request)
    if (!result.ok) {
      throw new ApiError(result.status, result.error.code, result.error.message)
    }
    // shortcut: bodies are trusted to match the generated types, since the engine is ours and the
    // types come from its Rust structs; validate at runtime if a body ever comes from elsewhere.
    return result.body as T
  }

  return {
    health: () => call<HealthResponse>({ method: 'GET', path: '/health' }),
    instruments: (venue: Venue, kind: InstrumentKind) =>
      call<InstrumentsResponse>({ method: 'GET', path: '/instruments', query: { venue, kind } }),
    /** Starts a download job. */
    downloadData: (request: DownloadRequest) =>
      call<JobCreated>({ method: 'POST', path: '/data/download', body: request }),
    coverage: (instrument: InstrumentId) =>
      call<CoverageResponse>({
        method: 'GET',
        path: '/data/coverage',
        query: { instrument: `${instrument.venue}:${instrument.symbol}:${instrument.kind}` },
      }),
    strategies: () => call<StrategyInfo[]>({ method: 'GET', path: '/strategies' }),
    jobs: () => call<JobInfo[]>({ method: 'GET', path: '/jobs' }),
    job: (id: number) => call<JobInfo>({ method: 'GET', path: `/jobs/${id}` }),
    cancelJob: (id: number) => call<JobInfo>({ method: 'POST', path: `/jobs/${id}/cancel` }),
    /** A finished run job's saved run; 409 `job_not_done` until it is done. */
    jobResult: (id: number) => call<RunDetail>({ method: 'GET', path: `/jobs/${id}/result` }),
    /** Starts a backtest job; 422 `data_missing` when the store lacks the range, 429 `too_many_runs`. */
    startBacktest: (config: BacktestConfig) =>
      call<JobCreated>({ method: 'POST', path: '/runs/backtest', body: config }),
    runs: () => call<RunSummary[]>({ method: 'GET', path: '/runs' }),
    run: (id: string) => call<RunDetail>({ method: 'GET', path: `/runs/${id}` }),
    /** Every paper session, oldest first. */
    sessions: () => call<SessionSummary[]>({ method: 'GET', path: '/sessions' }),
    session: (id: string) => call<SessionState>({ method: 'GET', path: `/sessions/${encodeURIComponent(id)}` }),
    /** Creates a paper session and starts it. */
    startSession: (request: StartSessionRequest) =>
      call<SessionSummary>({ method: 'POST', path: '/sessions', body: request }),
    stopSession: (id: string) =>
      call<SessionSummary>({ method: 'POST', path: `/sessions/${encodeURIComponent(id)}/stop` }),
    /** Replays a stopped session and starts it again. */
    resumeSession: (id: string) =>
      call<SessionSummary>({ method: 'POST', path: `/sessions/${encodeURIComponent(id)}/resume` }),
  }
}

export type Client = ReturnType<typeof createClient>
