// The gates' fixtures: one static server per worker for the built renderer, a page that fails its
// test on any console error, and `open(config)` that starts the app on the bridge stub.
import { test as base, expect } from '@playwright/test'
import { openApp, serve } from '../app/shots/harness.mjs'

export type StubConfig = { engine: 'starting' | 'failed' | 'running'; mock: boolean; hangStrategies?: boolean; failSessions?: number }

export const test = base.extend<{ open: (config: StubConfig, options?: { clock?: boolean }) => Promise<void> }, { appURL: string }>({
  appURL: [
    // Playwright requires an object pattern as the first argument, even an empty one.
    async ({}, use) => {
      const { server, url } = await serve()
      try {
        await use(url)
      } finally {
        server.closeAllConnections()
        await new Promise((resolve) => server.close(resolve))
      }
    },
    { scope: 'worker' },
  ],
  open: async ({ page, appURL }, use) => {
    const errors: string[] = []
    page.on('pageerror', (e) => errors.push(e.message))
    page.on('console', (m) => {
      if (m.type() === 'error') errors.push(m.text())
    })
    await use((config, options) => openApp(page, appURL, config, options))
    expect(errors, 'console errors and uncaught exceptions').toEqual([])
  },
})

export { expect }
