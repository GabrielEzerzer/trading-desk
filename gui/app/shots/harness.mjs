// What the headless screenshots (shots.mjs) and the Playwright UI gates (gui/e2e) share: the static
// server for the built renderer, the bridge stub that stands in for `window.td`, the window sizes and
// the renderer states with the steps that reach them.
//
// None of this launches Electron. The server serves app/out/renderer on a free localhost port and is
// closed by whoever started it.

import { readFileSync } from 'node:fs'
import { createServer } from 'node:http'
import { dirname, extname, join, normalize, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const root = resolve(here, '..', 'out', 'renderer')
const engineData = JSON.parse(readFileSync(join(here, '..', 'mock', 'engine.json'), 'utf8'))
const stubSource = readFileSync(join(here, 'bridge-stub.js'), 'utf8')

/** The window sizes: the minimum window, a common laptop window and a large one. */
export const SIZES = [
  { name: 'min-900x600', width: 900, height: 600 },
  { name: '1100x720', width: 1100, height: 720 },
  { name: '1600x1000', width: 1600, height: 1000 },
]

const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.woff2': 'font/woff2', '.svg': 'image/svg+xml' }

/** Serves the built renderer on a free localhost port. Resolves with the server and its URL. */
export function serve() {
  const server = createServer((req, res) => {
    const path = decodeURIComponent(new URL(req.url ?? '/', 'http://x').pathname)
    const file = normalize(join(root, path === '/' ? 'index.html' : path))
    if (!file.startsWith(root)) {
      res.writeHead(403).end()
      return
    }
    try {
      const body = readFileSync(file)
      res.writeHead(200, { 'content-type': TYPES[extname(file)] ?? 'application/octet-stream' }).end(body)
    } catch {
      res.writeHead(404).end()
    }
  })
  return new Promise((resolveServer) => {
    server.listen(0, '127.0.0.1', () => resolveServer({ server, url: `http://127.0.0.1:${server.address().port}/` }))
  })
}

/** The init script that installs the bridge stub with this config before the page loads. */
export function stubScript(config) {
  const full = { hangStrategies: false, engineData, ...config }
  return `${stubSource}\ninstallBridgeStub(${JSON.stringify(full)})`
}

/**
 * Opens the app on the stub and waits for its fonts. With `clock`, the page runs on Playwright's
 * clock, so a state can stop time and hold still (a run in progress).
 */
export async function openApp(page, url, config, { clock = false } = {}) {
  if (clock) {
    // Keep the real timers, so a tool that needs them (axe) can have them back: realTimers().
    await page.addInitScript(() => {
      window.__realTimers = { setTimeout, clearTimeout, setInterval, clearInterval }
    })
    await page.clock.install()
  }
  await page.addInitScript(stubScript(config))
  await page.goto(url)
  await page.evaluate(() => document.fonts.ready)
}

/**
 * Gives a page on Playwright's clock its real timers back, for tools that need them (axe). Timers
 * the app set before stay on the stopped clock, so a held state keeps holding.
 */
export async function realTimers(page) {
  await page.evaluate(() => {
    if (window.__realTimers) Object.assign(window, window.__realTimers)
  })
}

/** Waits for every finite animation and transition on the page to finish. */
export async function settle(page) {
  await page.evaluate(() =>
    Promise.all(
      document
        .getAnimations()
        .filter((a) => a.effect?.getTiming().iterations !== Infinity)
        .map((a) => a.finished.catch(() => {})),
    ),
  )
}

const chooseRow = async (page, name) => {
  await page.getByRole('button', { name }).click()
  await page.mouse.move(0, 0)
}

// Each state: the stub's config and the steps that bring the page there. Every step list ends by
// waiting for something the state must show, never on a fixed sleep.
export const STATES = [
  {
    name: 'engine-starting',
    config: { engine: 'starting', mock: true },
    steps: async (page) => page.getByText('Starting the engine').waitFor(),
  },
  {
    name: 'engine-starting-tooltip',
    config: { engine: 'starting', mock: true },
    steps: async (page) => {
      await page.hover('.engine-dot')
      await page.getByRole('tooltip').waitFor()
    },
  },
  {
    name: 'engine-failed',
    config: { engine: 'failed', mock: true },
    steps: async (page) => page.getByText('The engine failed').waitFor(),
  },
  {
    name: 'strategies-loading',
    config: { engine: 'running', mock: false, hangStrategies: true },
    steps: async (page) => page.getByText('Loading strategies').waitFor(),
  },
  {
    name: 'lab-list',
    config: { engine: 'running', mock: true },
    steps: async (page) => page.getByRole('heading', { name: 'Strategies' }).waitFor(),
  },
  {
    name: 'lab-list-engine',
    config: { engine: 'running', mock: false },
    steps: async (page) => page.getByRole('heading', { name: 'Strategies' }).waitFor(),
  },
  {
    name: 'lab-strategy-menu',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.getByRole('heading', { name: 'Strategies' }).waitFor()
      await page.click('.pill')
      await page.getByRole('menu').waitFor()
    },
  },
  {
    name: 'lab-never-run',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await chooseRow(page, /Donchian breakout/)
      await page.getByText('Not run', { exact: true }).waitFor()
    },
  },
  {
    name: 'lab-strategy-menu-selected',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await chooseRow(page, /Donchian breakout/)
      await page.getByText('Not run', { exact: true }).waitFor()
      await page.click('.pill')
      await page.getByRole('menu').waitFor()
    },
  },
  {
    name: 'lab-overrides-menu',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await chooseRow(page, /Donchian breakout/)
      await page.getByRole('button', { name: 'Overrides' }).click()
      await page.getByRole('menu').waitFor()
    },
  },
  {
    name: 'lab-running',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await chooseRow(page, /Donchian breakout/)
      await page.getByText('Not run', { exact: true }).waitFor()
      // Stop the page's time before the run starts, then move it on to the fourth progress report
      // (the mock reports every 700 ms), so the run holds on that step while it is looked at.
      await page.clock.pauseAt((await page.evaluate(() => Date.now())) + 500)
      await page.getByRole('button', { name: 'Run', exact: true }).click()
      await page.mouse.move(0, 0)
      await page.clock.runFor(2100)
      await page.getByText('Step 2 of 4 · Sweeping parameters, 48 of 48').waitFor()
    },
  },
  {
    name: 'lab-result-fresh-trade',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await chooseRow(page, /Donchian breakout/)
      await page.getByRole('button', { name: 'Run', exact: true }).click()
      await page.mouse.move(0, 0)
      await page.getByRole('heading', { name: 'Trade', exact: true }).waitFor({ timeout: 15_000 })
      // The numbers count up on requestAnimationFrame; wait for the return to land on its true value.
      await page.locator('.key-number dd').first().filter({ hasText: '+350.3%' }).waitFor()
    },
  },
  {
    name: 'lab-result-trade',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await chooseRow(page, /SMA cross/)
      await page.getByRole('heading', { name: 'Trade', exact: true }).waitFor()
    },
  },
  {
    name: 'lab-result-no-trade',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await chooseRow(page, /RSI reversion/)
      await page.getByRole('heading', { name: 'No trade', exact: true }).waitFor()
    },
  },
  {
    name: 'lab-result-evidence',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await chooseRow(page, /SMA cross/)
      await page.getByRole('heading', { name: 'Trade', exact: true }).waitFor()
      await page.evaluate(() => document.getElementById('equity-title')?.scrollIntoView({ block: 'start' }))
      await page.waitForFunction(() => (document.querySelector('.lab-scroll')?.scrollTop ?? 0) > 0)
    },
  },
  {
    name: 'lab-result-long-trades',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await chooseRow(page, /RSI reversion/)
      await page.getByRole('heading', { name: 'No trade', exact: true }).waitFor()
      await page.evaluate(() => {
        const scroller = document.querySelector('.lab-scroll')
        const title = document.getElementById('trades-title')
        if (scroller && title) scroller.scrollTop = title.offsetTop + 600
      })
      await page.waitForFunction(() => (document.querySelector('.lab-scroll')?.scrollTop ?? 0) > 0)
    },
  },
  {
    name: 'lab-run-failed',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await chooseRow(page, /Funding carry/)
      await page.getByRole('button', { name: 'Run', exact: true }).click()
      await page.mouse.move(0, 0)
      await page.getByText('Run failed').waitFor({ timeout: 10_000 })
    },
  },
  // The engine source, on the stub's fake engine (app/mock/engine.json).
  {
    name: 'lab-engine-past-result',
    config: { engine: 'running', mock: false },
    steps: async (page) => {
      await chooseRow(page, /SMA cross/)
      await page.getByRole('heading', { name: 'No verdict rule yet' }).waitFor()
    },
  },
  {
    name: 'lab-engine-null-metrics',
    config: { engine: 'running', mock: false },
    steps: async (page) => {
      await chooseRow(page, /RSI reversion/)
      await page.getByRole('heading', { name: 'No verdict rule yet' }).waitFor()
    },
  },
  {
    name: 'lab-engine-null-metrics-table',
    config: { engine: 'running', mock: false },
    steps: async (page) => {
      await chooseRow(page, /RSI reversion/)
      await page.getByRole('heading', { name: 'No verdict rule yet' }).waitFor()
      await page.evaluate(() => document.getElementById('metrics-title')?.scrollIntoView({ block: 'start' }))
      await page.waitForFunction(() => (document.querySelector('.lab-scroll')?.scrollTop ?? 0) > 0)
    },
  },
  {
    name: 'lab-engine-no-data',
    config: { engine: 'running', mock: false, noData: true },
    steps: async (page) => {
      await chooseRow(page, /Donchian breakout/)
      await page.getByRole('button', { name: 'Download BTCUSDT history' }).waitFor()
    },
  },
  {
    name: 'lab-engine-downloading',
    config: { engine: 'running', mock: false, noData: true },
    clock: true,
    steps: async (page) => {
      await chooseRow(page, /Donchian breakout/)
      await page.getByRole('button', { name: 'Download BTCUSDT history' }).waitFor()
      // Stop time before the download starts, then move on to its second report (the stub reports
      // every 700 ms), so it holds at half way while it is looked at.
      await page.clock.pauseAt((await page.evaluate(() => Date.now())) + 500)
      await page.getByRole('button', { name: 'Download BTCUSDT history' }).click()
      await page.mouse.move(0, 0)
      await page.clock.runFor(800)
      await page.locator('[role=progressbar][aria-valuenow="50"]').waitFor()
    },
  },
  {
    name: 'lab-engine-data-missing',
    config: { engine: 'running', mock: false, backtest: 'data_missing' },
    steps: async (page) => {
      await chooseRow(page, /Donchian breakout/)
      await page.getByRole('button', { name: 'Run', exact: true }).click()
      await page.mouse.move(0, 0)
      await page.getByRole('button', { name: 'Download BTCUSDT history' }).waitFor()
    },
  },
  {
    name: 'lab-engine-too-many-runs',
    config: { engine: 'running', mock: false, backtest: 'too_many_runs' },
    steps: async (page) => {
      await chooseRow(page, /Donchian breakout/)
      await page.getByRole('button', { name: 'Run', exact: true }).click()
      await page.mouse.move(0, 0)
      await page.getByText('Another run is in progress. Try again when it finishes.').waitFor()
    },
  },
  {
    name: 'lab-engine-overrides-menu',
    config: { engine: 'running', mock: false },
    steps: async (page) => {
      await chooseRow(page, /Donchian breakout/)
      await page.getByRole('button', { name: 'Overrides' }).click()
      await page.getByRole('menuitemradio', { name: 'BTCUSDT perpetual, Binance' }).waitFor()
    },
  },
  {
    name: 'trading',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.keyboard.press('Control+2')
      await page.locator('main[aria-label="Trading"]').waitFor()
    },
  },
]
