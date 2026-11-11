// The main-process half of the API bridge. The renderer is a trust boundary: everything it sends
// is checked here against the routes and topics the app may use before main adds the token and
// passes it on to the engine.

import type { ApiMethod, ApiRequest, ApiResult, ErrorBody } from '@td/shared'

interface Route {
  method: ApiMethod
  path: RegExp
  /** Query parameters the route requires; no others are allowed. */
  query?: readonly string[]
  /** Whether the route takes a JSON object body. */
  body?: boolean
}

// The routes in crates/td-server/src/api/mod.rs, kept in step with the client in @td/shared.
const ROUTES: readonly Route[] = [
  { method: 'GET', path: /^\/health$/ },
  { method: 'GET', path: /^\/instruments$/, query: ['venue', 'kind'] },
  { method: 'POST', path: /^\/data\/download$/, body: true },
  { method: 'GET', path: /^\/data\/coverage$/, query: ['instrument'] },
  { method: 'GET', path: /^\/strategies$/ },
  { method: 'GET', path: /^\/jobs$/ },
  { method: 'GET', path: /^\/jobs\/\d{1,15}$/ },
  { method: 'POST', path: /^\/jobs\/\d{1,15}\/cancel$/ },
  { method: 'GET', path: /^\/jobs\/\d{1,15}\/result$/ },
  { method: 'POST', path: /^\/runs\/backtest$/, body: true },
  { method: 'GET', path: /^\/runs$/ },
  // A run id: the save time as 13 digits, `-`, then 8 lowercase hex digits (td_engine::RunId).
  { method: 'GET', path: /^\/runs\/\d{13}-[0-9a-f]{8}$/ },
  { method: 'GET', path: /^\/sessions$/ },
  { method: 'POST', path: /^\/sessions$/, body: true },
  { method: 'GET', path: /^\/sessions\/[A-Za-z0-9_-]{1,64}$/ },
  { method: 'POST', path: /^\/sessions\/[A-Za-z0-9_-]{1,64}\/(?:stop|resume)$/ },
]

// The WebSocket topics the engine serves (crates/td-server/src/api/ws.rs). Symbols follow the
// store's rule (crates/td-data/src/store.rs `check_symbol`, narrowed to ASCII) and session ids the
// paper session's (crates/td-server/src/paper.rs `check_id`).
const TOPICS: readonly RegExp[] = [
  /^jobs$/,
  /^candles:(?:binance|bybit):[A-Z0-9_-]{1,32}:(?:spot|perpetual):(?:1m|5m|15m|30m|1h|4h|1d|1w)$/,
  /^session:[A-Za-z0-9_-]{1,64}$/,
]

const MAX_QUERY_VALUE = 256
const MAX_BODY_BYTES = 64 * 1024
const REQUEST_TIMEOUT_MS = 30_000

/** A request or topic from the renderer that main will not pass on. */
export class RefusedRequest extends Error {
  override name = 'RefusedRequest'
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null) return false
  const proto = Object.getPrototypeOf(value)
  return proto === Object.prototype || proto === null
}

/** Checks a request from the renderer and returns a clean copy, or throws `RefusedRequest`. */
export function validateRequest(input: unknown): ApiRequest {
  if (!isPlainObject(input)) throw new RefusedRequest('a request must be an object')
  for (const key of Object.keys(input)) {
    if (!['method', 'path', 'query', 'body'].includes(key)) {
      throw new RefusedRequest(`a request has no field ${JSON.stringify(key)}`)
    }
  }
  const { method, path, query, body } = input
  if (method !== 'GET' && method !== 'POST') throw new RefusedRequest('the method must be GET or POST')
  if (typeof path !== 'string') throw new RefusedRequest('the path must be a string')
  const route = ROUTES.find((r) => r.method === method && r.path.test(path))
  if (!route) {
    throw new RefusedRequest(`${method} ${path.slice(0, 100)} is not a route the app may call`)
  }

  const allowed = route.query ?? []
  if (query !== undefined && !isPlainObject(query)) throw new RefusedRequest('the query must be an object')
  const given = query ?? {}
  for (const [key, value] of Object.entries(given)) {
    if (!allowed.includes(key)) throw new RefusedRequest(`${path} takes no query parameter ${JSON.stringify(key)}`)
    if (typeof value !== 'string' || value.length > MAX_QUERY_VALUE) {
      throw new RefusedRequest(`the query parameter ${key} must be a string of at most ${MAX_QUERY_VALUE} characters`)
    }
  }
  for (const key of allowed) {
    if (!(key in given)) throw new RefusedRequest(`${path} needs the query parameter ${key}`)
  }

  const clean: ApiRequest = { method, path }
  if (allowed.length > 0) clean.query = Object.fromEntries(allowed.map((k) => [k, given[k] as string]))
  if (route.body) {
    if (!isPlainObject(body)) throw new RefusedRequest(`${method} ${path} needs a JSON object body`)
    let json: string
    try {
      json = JSON.stringify(body)
    } catch {
      throw new RefusedRequest('the body cannot be sent as JSON')
    }
    if (Buffer.byteLength(json) > MAX_BODY_BYTES) throw new RefusedRequest('the body is too large')
    clean.body = JSON.parse(json)
  } else if (body !== undefined) {
    throw new RefusedRequest(`${method} ${path} takes no body`)
  }
  return clean
}

/** Checks a WebSocket topic from the renderer, or throws `RefusedRequest`. */
export function validateTopic(input: unknown): string {
  if (typeof input !== 'string' || !TOPICS.some((t) => t.test(input))) {
    throw new RefusedRequest(`${JSON.stringify(input)?.slice(0, 100)} is not a topic the app may subscribe to`)
  }
  return input
}

export interface Connection {
  port: number
  token: string
}

function failure(status: number, code: string, message: string): ApiResult {
  return { ok: false, status, error: { code, message } }
}

function isErrorBody(value: unknown): value is { error: ErrorBody } {
  if (!isPlainObject(value) || !isPlainObject(value.error)) return false
  return typeof value.error.code === 'string' && typeof value.error.message === 'string'
}

/**
 * Sends a checked request to the engine with the token. Never throws. `signal` aborts it early,
 * as the app does with every open request when it quits.
 */
export async function sendRequest(connection: Connection, request: ApiRequest, signal?: AbortSignal): Promise<ApiResult> {
  const url = new URL(request.path, `http://127.0.0.1:${connection.port}`)
  for (const [key, value] of Object.entries(request.query ?? {})) url.searchParams.set(key, value)
  const headers: Record<string, string> = { authorization: `Bearer ${connection.token}` }
  if (request.body !== undefined) headers['content-type'] = 'application/json'
  let status: number
  let text: string
  try {
    const response = await fetch(url, {
      method: request.method,
      headers,
      body: request.body === undefined ? undefined : JSON.stringify(request.body),
      redirect: 'error',
      signal: signal
        ? AbortSignal.any([signal, AbortSignal.timeout(REQUEST_TIMEOUT_MS)])
        : AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    })
    status = response.status
    text = await response.text()
  } catch (e) {
    return failure(0, 'engine_unreachable', `the engine did not answer: ${e instanceof Error ? e.message : String(e)}`)
  }
  let body: unknown
  try {
    body = JSON.parse(text)
  } catch {
    return failure(status, 'bad_response', `the engine answered ${status} with a body that is not JSON`)
  }
  if (status >= 200 && status < 300) return { ok: true, status, body }
  if (isErrorBody(body)) return { ok: false, status, error: { code: body.error.code, message: body.error.message } }
  return failure(status, `http_${status}`, `the engine answered ${status}`)
}
