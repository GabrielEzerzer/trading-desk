// The contract between the renderer, the preload script and the main process. The renderer never
// sees the engine's token: it asks main for each call, and main checks the call against the
// route list before sending it with the token.

import type { ErrorBody, ServerMessage } from './api-types'

export type ApiMethod = 'GET' | 'POST'

/** One call to the engine's HTTP API, as the renderer asks for it. */
export interface ApiRequest {
  method: ApiMethod
  /** The route's path, with no query string, for example `/jobs/3`. */
  path: string
  query?: Record<string, string>
  /** Sent as JSON. */
  body?: unknown
}

/**
 * What a call returned. Errors are values, not exceptions, so they keep their code across IPC.
 * `status` is 0 when the call never reached the engine (refused by main, engine not running or
 * not answering).
 */
export type ApiResult =
  | { ok: true; status: number; body: unknown }
  | { ok: false; status: number; error: ErrorBody }

export type EngineStatus =
  | { state: 'connecting' }
  | { state: 'running' }
  | { state: 'failed'; message: string }

/** What the preload script exposes to the renderer as `window.td`. */
export interface TdBridge {
  request(request: ApiRequest): Promise<ApiResult>
  /** Subscribes the app's one WebSocket to a topic; its messages arrive through `onMessage`. */
  subscribe(topic: string): Promise<void>
  unsubscribe(topic: string): Promise<void>
  /** Every WebSocket message from the engine. Returns a function that stops listening. */
  onMessage(listener: (message: ServerMessage) => void): () => void
  status(): Promise<EngineStatus>
  onStatus(listener: (status: EngineStatus) => void): () => void
  /** Starts the engine again after it failed. Does nothing while it is starting or running. */
  restartEngine(): Promise<void>
  /**
   * Opens the engine's data folder in the file explorer. Main asks the engine for the folder; the
   * renderer cannot name a path. Resolves with null once it is open, or with why it did not open.
   */
  openDataFolder(): Promise<string | null>
  /**
   * True when the app was started with `TD_MOCK=1`: the renderer then reads its Lab data from the
   * fixtures in `app/mock/` instead of the engine. Not a secret.
   */
  readonly mock: boolean
}

/** IPC channel names. */
export const IPC = {
  request: 'td:request',
  subscribe: 'td:subscribe',
  unsubscribe: 'td:unsubscribe',
  message: 'td:message',
  status: 'td:status',
  statusChanged: 'td:status-changed',
  restartEngine: 'td:restart-engine',
  openDataFolder: 'td:open-data-folder',
} as const

/** The renderer argument main adds when the app runs on mock data (`TD_MOCK=1`). */
export const MOCK_ARG = '--td-mock'
