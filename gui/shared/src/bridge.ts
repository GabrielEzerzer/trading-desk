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
}

/** IPC channel names. */
export const IPC = {
  request: 'td:request',
  subscribe: 'td:subscribe',
  unsubscribe: 'td:unsubscribe',
  message: 'td:message',
  status: 'td:status',
  statusChanged: 'td:status-changed',
} as const
