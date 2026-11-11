// Types for harness.mjs, for the TypeScript specs in app/e2e.
import type { Page } from '@playwright/test'
import type { Server } from 'node:http'

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
  /** Time between a job's steps, 700 ms by default. */
  stepMs?: number
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
export function serve(): Promise<{ server: Server; url: string }>
export function stubScript(config: State['config']): string
export function openApp(page: Page, url: string, config: State['config'], options?: { clock?: boolean }): Promise<void>
export function realTimers(page: Page): Promise<void>
export function settle(page: Page): Promise<void>
