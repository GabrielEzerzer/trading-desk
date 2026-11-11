// Types for harness.mjs, for the TypeScript specs in app/e2e.
import type { Page } from '@playwright/test'
import type { Server } from 'node:http'

export interface Size {
  name: string
  width: number
  height: number
}

export interface State {
  name: string
  /** Set for states outside the Strategy Lab. */
  section?: 'trading'
  config: { engine: 'starting' | 'failed' | 'running'; mock: boolean; hangStrategies?: boolean; failSessions?: number }
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
