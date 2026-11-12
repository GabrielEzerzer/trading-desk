// Types for harness.mjs, for the TypeScript specs in app/e2e.
import type { Page } from '@playwright/test'
import type { Server } from 'node:http'
import type { Candle, Instrument, SessionState } from '@td/shared'

export interface Size {
  name: string
  width: number
  height: number
}

/** The bridge stub's config (bridge-stub.js); the fixtures are added by `stubScript`. */
export interface StubConfig {
  engine: 'starting' | 'failed' | 'running'
  mock: boolean
  hangStrategies?: boolean
  /** Nothing is stored until a download. */
  noData?: boolean
  /** How POST /runs/backtest answers. */
  backtest?: 'ok' | 'data_missing' | 'too_many_runs' | 'fails'
  /** How POST /data/download's job ends: done, or failed for a missing OANDA token. */
  download?: 'ok' | 'no_token'
  /** Time between a job's steps, 700 ms by default. */
  stepMs?: number
  /** GET /instruments answers from these, else from the engine.json fixtures. */
  instruments?: Instrument[]
  /** GET /instruments never answers. */
  hangInstruments?: boolean
  /** GET /instruments fails. */
  failInstruments?: boolean
  /** The paper session GET /sessions lists; POST /sessions replaces it. */
  session?: SessionState | null
  /** GET /sessions fails this many times before it answers. */
  failSessions?: number
  /**
   * How GET /candles answers, by instrument (`venue:symbol:kind`) or `*`: after that many
   * milliseconds, never (`hang`), or with the engine's 502 (`fail`). At once without an entry.
   */
  history?: Record<string, number | 'hang' | 'fail'>
  /** Live bars on every `candles:` topic: a closed bar each `everyMs`, walking on from `start`. */
  candles?: { start: Candle; everyMs: number } | null
}

export interface State {
  name: string
  /** Set for states outside the Strategy Lab. */
  section?: 'trading'
  config: StubConfig
  /** Runs the page on Playwright's clock, so the steps can stop time. */
  clock?: boolean
  steps(page: Page): Promise<unknown>
}

export const SIZES: Size[]
export const STATES: State[]
/** Trading on the engine stub: instruments from the fixtures and a closed bar every 250 ms. */
export const engineTrading: State['config']
export function serve(): Promise<{ server: Server; url: string }>
export function stubScript(config: State['config']): string
export function openApp(page: Page, url: string, config: State['config'], options?: { clock?: boolean }): Promise<void>
export function realTimers(page: Page): Promise<void>
export function settle(page: Page): Promise<void>
