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
const fixtures = JSON.parse(readFileSync(join(here, '..', 'mock', 'lab.json'), 'utf8'))
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

/** The engine's strategy list, as GET /strategies returns it. */
const engineStrategies = fixtures.strategies.map((s) => ({ name: s.name, description: s.description, params: [] }))

/** The init script that installs the bridge stub with this config before the page loads. */
export function stubScript(config) {
  const full = { strategies: engineStrategies, hangStrategies: false, ...config }
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

// Trading. Its states carry `section: 'trading'`: the Lab's layout gates leave them out, and they
// are looked at through the screenshots until Trading has gates of its own.

const trading = JSON.parse(readFileSync(join(here, '..', 'mock', 'trading.json'), 'utf8'))
const instruments = trading.instruments.map((i) => i.instrument)
const btc = instruments.find((i) => i.id.symbol === 'BTCUSDT' && i.id.venue === 'binance' && i.id.kind === 'spot')
const engineStart = { open_time: trading.end, open: 64180, high: 64230, low: 64150, close: 64210, volume: 14, quote_volume: 14 * 64210 }

/** A running strategy session as the engine reports it, for Trading on the engine. */
const engineSession = {
  id: '1794311000000-sma',
  config: { strategy: 'sma_cross', params: {}, instrument: btc, timeframe: '1m', starting_cash: 10000, slippage_bps: 2, fees: null },
  status: { status: 'live' },
  cash: 2302.4,
  equity: 10009.6,
  position: { qty: 0.12, avg_entry: 64142.5, mark_price: 64210, unrealised_pnl: 8.1, realised_pnl: 0, fees_paid: 7.7, funding_paid: 0 },
  equity_curve: [],
  open_orders: [],
  fills: [],
  trades: [],
  funding: [],
  live_from: trading.end - 3_600_000,
  last_bar: engineStart,
  dropped: [],
  warnings: [],
}

/** Trading on the engine: instruments from the fixtures and a closed bar every 250 ms. */
const engineTrading = { engine: 'running', mock: false, instruments, candles: { start: engineStart, everyMs: 250 } }

const toTrading = async (page) => {
  await page.keyboard.press('Control+2')
  await page.locator('main[aria-label="Trading"]').waitFor()
}

/** Trading on the mock source, ready: instruments loaded, sessions read and a price shown. */
const readyTrading = async (page) => {
  await toTrading(page)
  await page.locator('.position-line .line-button').first().waitFor()
  await page.locator('.ticker-price').filter({ hasText: /\d/ }).waitFor()
}

const startManual = async (page) => {
  await readyTrading(page)
  await page.getByRole('button', { name: 'Start a session' }).click()
  await page.getByRole('button', { name: 'Start', exact: true }).click()
  await page.locator('.position-summary').waitFor()
}

const placeOrder = async (page, side, size) => {
  await page.getByPlaceholder('Size').fill(size)
  await page.getByRole('button', { name: side, exact: true }).click()
}

/** Starts a Manual session and buys 0.05 BTC at market; the mock fills it at the next bar's open. */
const holdBtc = async (page) => {
  await startManual(page)
  await placeOrder(page, 'Buy', '0.05')
  await page.locator('.position-summary').filter({ hasText: '0.05000 BTC at' }).waitFor()
}

const stubCandles = (page, count) => page.waitForFunction((n) => (window.__stubCandles ?? 0) >= n, count)

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
  {
    name: 'lab-engine-cannot-run',
    config: { engine: 'running', mock: false },
    steps: async (page) => {
      await chooseRow(page, /SMA cross/)
      await page.hover('.run')
      await page.getByRole('tooltip').waitFor()
    },
  },
  {
    name: 'trading',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await page.getByText('No paper session on BTCUSDT').waitFor()
    },
  },
  {
    name: 'trading-engine-starting',
    section: 'trading',
    config: { engine: 'starting', mock: true },
    steps: async (page) => {
      await toTrading(page)
      await page.getByText('Starting the engine').filter({ visible: true }).waitFor()
    },
  },
  {
    name: 'trading-engine-failed',
    section: 'trading',
    config: { engine: 'failed', mock: true },
    steps: async (page) => {
      await toTrading(page)
      await page.getByText('The engine failed').filter({ visible: true }).waitFor()
    },
  },
  {
    name: 'trading-instruments-loading',
    section: 'trading',
    config: { ...engineTrading, hangInstruments: true, candles: null },
    steps: async (page) => {
      await toTrading(page)
      await page.locator('.chart-empty').getByText('Loading instruments').waitFor()
    },
  },
  {
    name: 'trading-instruments-failed',
    section: 'trading',
    config: { ...engineTrading, failInstruments: true, candles: null },
    steps: async (page) => {
      await toTrading(page)
      await page.locator('.chart-empty').getByText('The instrument lists did not load.').waitFor()
    },
  },
  {
    name: 'trading-engine-empty',
    section: 'trading',
    config: { ...engineTrading, candles: null },
    steps: async (page) => {
      await toTrading(page)
      await page.getByText('Waiting for the first closed 1m bar').waitFor()
      await page.getByText('No paper session on BTCUSDT').waitFor()
    },
  },
  {
    name: 'trading-engine-filling',
    section: 'trading',
    config: engineTrading,
    steps: async (page) => {
      await toTrading(page)
      await stubCandles(page, 36)
    },
  },
  {
    name: 'trading-engine-session',
    section: 'trading',
    config: { ...engineTrading, session: engineSession },
    steps: async (page) => {
      await toTrading(page)
      await page.locator('.position-summary').filter({ hasText: 'SMA cross · Live' }).waitFor()
      await stubCandles(page, 24)
    },
  },
  {
    name: 'trading-engine-start',
    section: 'trading',
    config: engineTrading,
    steps: async (page) => {
      await toTrading(page)
      await page.getByRole('button', { name: 'Start a session' }).click()
      await page.getByRole('button', { name: /^Strategy:/ }).click()
      await page.getByRole('menu').waitFor()
    },
  },
  {
    name: 'trading-ticker-menu',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await page.keyboard.press('Control+k')
      await page.getByRole('listbox', { name: 'Instruments' }).waitFor()
    },
  },
  {
    name: 'trading-ticker-search',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await page.keyboard.press('Control+k')
      await page.getByRole('combobox', { name: 'Search instruments' }).fill('eth')
      await page.keyboard.press('ArrowDown')
      await page.getByRole('option', { name: /ETHBTC/, selected: true }).waitFor()
    },
  },
  {
    name: 'trading-ticker-perpetual',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await page.keyboard.press('Control+k')
      await page.getByRole('combobox', { name: 'Search instruments' }).fill('ethusdt')
      await page.getByRole('option', { name: /ETHUSDT.*Bybit perpetual/ }).click()
      await page.locator('.ticker-venue').filter({ hasText: 'Bybit perpetual' }).waitFor()
      await page.locator('.ticker-price').filter({ hasText: /^3,/ }).waitFor()
      await page.keyboard.press('Control+k')
      await page.getByRole('radio', { name: 'Perpetual' }).click()
      await page.getByRole('radio', { name: 'Perpetual', checked: true }).waitFor()
    },
  },
  {
    name: 'trading-start-session',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await page.getByRole('button', { name: 'Start a session' }).click()
      await page.getByText('Start a paper session').waitFor()
    },
  },
  {
    name: 'trading-order-invalid',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await startManual(page)
      await page.getByPlaceholder('Size').fill('0.00005')
      await page.mouse.move(0, 0)
      await page.getByText(/the venue’s minimum is|the venue's minimum is/).waitFor()
    },
  },
  {
    name: 'trading-session-running',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await holdBtc(page)
      await page.mouse.move(0, 0)
    },
  },
  {
    name: 'trading-position-panel',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await holdBtc(page)
      await placeOrder(page, 'Sell', '0.02')
      await page.locator('.position-summary').filter({ hasText: '0.03000 BTC at' }).waitFor()
      await page.locator('.position-summary').click()
      await page.getByRole('tab', { name: /Fills/ }).click()
      await page.getByRole('tab', { name: /Fills/, selected: true }).waitFor()
    },
  },
  {
    name: 'trading-limit-order',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await startManual(page)
      await page.getByPlaceholder('Size').fill('0.05')
      await page.getByRole('button', { name: /^Order type:/ }).click()
      await page.getByRole('menuitemradio', { name: 'Limit' }).click()
      await page.getByPlaceholder('Price').fill('63800')
      await page.getByRole('button', { name: 'Buy', exact: true }).click()
      await page.mouse.move(0, 0)
      await page.getByText('Buy 0.05000 BTC at 63,800.00 sent.').waitFor()
    },
  },
  {
    name: 'trading-order-rejected',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await startManual(page)
      await placeOrder(page, 'Sell', '0.05')
      await page.mouse.move(0, 0)
      await page.getByText(/^Order rejected:/).waitFor()
    },
  },
  {
    name: 'trading-reconnecting',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await holdBtc(page)
      await page.evaluate(() => window.tdMockTrading?.reconnect())
      await page.mouse.move(0, 0)
      await page.locator('.position-summary').filter({ hasText: 'Reconnecting to the feed' }).waitFor()
    },
  },
  {
    name: 'trading-dropped',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await startManual(page)
      await page.locator('.position-summary').filter({ hasText: 'Manual · Live' }).waitFor()
      await page.evaluate(() => window.tdMockTrading?.dropBucket())
      await page.mouse.move(0, 0)
      await page.locator('.position-notice').waitFor()
    },
  },
  {
    name: 'trading-session-stopped',
    section: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await holdBtc(page)
      await page.locator('.position-summary').click()
      await page.getByRole('button', { name: 'Stop', exact: true }).click()
      await page.locator('.position-summary').filter({ hasText: 'Manual · Stopped' }).waitFor()
      await page.mouse.click(300, 200)
      await page.locator('.session-panel').waitFor({ state: 'detached' })
    },
  },
]
