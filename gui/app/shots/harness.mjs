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

/** Opens the stored market data from the strategy list. */
const openStoredData = async (page) => {
  await chooseRow(page, /^Stored data/)
  await page.getByRole('heading', { name: 'Market data', level: 1 }).waitFor()
}

/** Opens the stored data and asks to delete all of BTCUSDT perpetual. */
const askDeleteBtc = async (page) => {
  await openStoredData(page)
  await page.getByRole('button', { name: 'Delete BTCUSDT perpetual, Binance' }).click()
  await page.getByRole('menuitem', { name: /All of it/ }).click()
  await page.getByRole('group', { name: 'Confirm the delete' }).waitFor()
}

// Trading. Its states carry `section: 'trading'`. Those past the engine's start run on
// Playwright's clock, held still: the mock emits a bar every 1.5 s and the engine stub one every
// 250 ms, so a state reached in real time would show however many bars had come by then. `hold`
// stops the page's time before Trading opens, so its feeds start at that instant; from then on
// only `tick` moves it. Each state then shows the same bars, prices and fills on every run, which
// the screenshot baselines need. The steps still wait for what each state must show.

const trading = JSON.parse(readFileSync(join(here, '..', 'mock', 'trading.json'), 'utf8'))
const instruments = trading.instruments.map((i) => i.instrument)
const btc = instruments.find((i) => i.id.symbol === 'BTCUSDT' && i.id.venue === 'binance' && i.id.kind === 'spot')
const engineStart = { open_time: trading.end, open: 64180, high: 64230, low: 64150, close: 64210, volume: 14, quote_volume: 14 * 64210 }

/** A running strategy session as the engine reports it, for Trading on the engine. */
const engineSession = {
  id: '1794311000000-sma',
  config: { strategy: 'sma_cross', params: {}, instrument: btc, timeframe: '1m', starting_cash: 10000, slippage_bps: 2, fees: null, max_leverage: 1 },
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
export const engineTrading = { engine: 'running', mock: false, instruments, candles: { start: engineStart, everyMs: 250 } }

/** Milliseconds between the mock's live bars (`liveMs` in app/mock/trading.ts). */
const LIVE_MS = 1500
/** Milliseconds between the engine stub's bars (`engineTrading.candles.everyMs`). */
const ENGINE_MS = 250

/** How far each held page's time has moved since `hold`. */
const elapsed = new WeakMap()

/** Stops the page's time. Call it before Trading opens, so its feeds start at this instant. */
const hold = async (page) => {
  await page.clock.pauseAt((await page.evaluate(() => Date.now())) + 1000)
  elapsed.set(page, 0)
}

/** Moves a held page's time on by `ms`, running the timers and animation frames due in it. */
const tick = async (page, ms) => {
  await page.clock.runFor(ms)
  elapsed.set(page, (elapsed.get(page) ?? 0) + ms)
}

/** Moves the time to 100 ms past the mock's next bar, so whatever that bar fills has filled. */
const nextBar = async (page) => {
  const now = elapsed.get(page) ?? 0
  await tick(page, (Math.floor(now / LIVE_MS) + 1) * LIVE_MS + 100 - now)
}

/**
 * Lets React commit what the last timers changed, then runs `ms` of frames so the chart draws
 * it. React commits on message tasks, which the clock does not hold, so a long `tick` can end with
 * the page's text a bar ahead of the chart's canvas.
 */
const draw = async (page, ms = 100) => {
  await page.evaluate(async () => {
    for (let i = 0; i < 3; i++) {
      await new Promise((resolve) => {
        const channel = new MessageChannel()
        channel.port1.onmessage = resolve
        channel.port2.postMessage(null)
      })
    }
  })
  await tick(page, ms)
}

/**
 * Moves the time to 1 s past the mock's latest bar (past the next one when that is less than a
 * frame away) and draws: the price's 700 ms tint has gone, and no bar is due.
 */
const quiet = async (page) => {
  const now = elapsed.get(page) ?? 0
  let to = Math.floor(now / LIVE_MS) * LIVE_MS + 900
  if (to < now + 20) to += LIVE_MS
  await tick(page, to - now)
  await draw(page)
}

/**
 * Lets the engine stub emit `count` bars, waits until it has, and draws them. It stops 100 ms past
 * the last close, short of the next bar's forming step half way to its close.
 */
const engineBars = async (page, count) => {
  await tick(page, count * ENGINE_MS + 50)
  await page.waitForFunction((n) => (window.__stubCandles ?? 0) >= n, count)
  await draw(page, 50)
}

const toTrading = async (page) => {
  await page.keyboard.press('Control+2')
  await page.locator('main[aria-label="Trading"]').waitFor()
}

/** Trading on a held clock, with its source chosen, its feeds subscribed and its sessions read. */
const heldTrading = async (page) => {
  await hold(page)
  await toTrading(page)
  await page.locator('.position-line .line-button').first().waitFor()
}

/** Trading on the mock source, ready: instruments loaded, sessions read and a price shown. */
const readyTrading = async (page) => {
  await heldTrading(page)
  // The mock's instrument lists take 400 ms.
  await tick(page, 500)
  await page.getByText('Start a paper session to place orders.').waitFor()
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

/** Starts a Manual session and waits out its two bars of warm-up; like the engine, it takes no order before. */
const startLive = async (page) => {
  await startManual(page)
  await nextBar(page)
  await nextBar(page)
  await page.locator('.position-summary').filter({ hasText: 'Manual · Live' }).waitFor()
}

/** A live Manual session that buys 0.05 BTC at market; the mock fills it at the next bar's open. */
const holdBtc = async (page) => {
  await startLive(page)
  await placeOrder(page, 'Buy', '0.05')
  await page.getByText('Buy 0.05000 BTC at market sent.').waitFor()
  await nextBar(page)
  await page.locator('.position-summary').filter({ hasText: /0\.05000 BTC/ }).waitFor()
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
    // The engine started with TD_DATA_CUTOFF, and the strategy's code changed since its saved run.
    name: 'lab-engine-code-changed',
    config: { engine: 'running', mock: false, dataCutoff: '2025-03-31', codeChanged: true },
    steps: async (page) => {
      await chooseRow(page, /SMA cross/)
      await page.getByRole('heading', { name: 'No verdict rule yet' }).waitFor()
      await page.getByText("The strategy's code has changed since this run", { exact: false }).waitFor()
    },
  },
  {
    name: 'lab-engine-cutoff',
    config: { engine: 'running', mock: false, dataCutoff: '2025-03-31' },
    steps: async (page) => page.getByText('Data cutoff in force', { exact: false }).waitFor(),
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
    // Nothing is stored, as after deleting all market data: the saved result shows, with the
    // download offered under its verdict.
    name: 'lab-engine-past-result-no-data',
    config: { engine: 'running', mock: false, noData: true },
    steps: async (page) => {
      await chooseRow(page, /SMA cross/)
      await page.getByRole('heading', { name: 'No verdict rule yet' }).waitFor()
      await page.getByRole('button', { name: 'Download BTCUSDT history' }).waitFor()
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
    // The engine started without OANDA_API_TOKEN: the stub fails every download the way the engine
    // fails an OANDA one.
    name: 'lab-engine-download-no-token',
    config: { engine: 'running', mock: false, noData: true, download: 'no_token' },
    steps: async (page) => {
      await chooseRow(page, /Donchian breakout/)
      await page.getByRole('button', { name: 'Download BTCUSDT history' }).click()
      await page.mouse.move(0, 0)
      await page.getByText('FX downloads from OANDA need an API token.', { exact: false }).waitFor()
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
      await page.getByText(/runs are already queued or running/).waitFor()
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
    name: 'lab-data',
    config: { engine: 'running', mock: true },
    steps: openStoredData,
  },
  {
    name: 'lab-data-empty',
    config: { engine: 'running', mock: false, noData: true },
    steps: async (page) => {
      await openStoredData(page)
      await page.getByText('Nothing stored', { exact: true }).waitFor()
    },
  },
  {
    name: 'lab-data-confirm',
    config: { engine: 'running', mock: false },
    steps: askDeleteBtc,
  },
  {
    name: 'lab-data-confirm-range',
    config: { engine: 'running', mock: false },
    steps: async (page) => {
      await openStoredData(page)
      await page.getByRole('button', { name: 'Delete BTCUSDT perpetual, Binance' }).click()
      await page.getByRole('menuitem', { name: 'A range of months' }).click()
      await page.getByRole('button', { name: 'To: Jan 2024' }).click()
      await page.getByRole('menuitemradio', { name: 'Jun 2024' }).click()
      await page.getByText('That is 6 months.', { exact: false }).waitFor()
    },
  },
  {
    name: 'lab-data-delete-failed',
    config: { engine: 'running', mock: false, deleteFails: true },
    steps: async (page) => {
      await askDeleteBtc(page)
      await page.getByRole('button', { name: 'Delete', exact: true }).click()
      await page.mouse.move(0, 0)
      await page.getByText('The delete failed.', { exact: false }).waitFor()
    },
  },
  {
    name: 'trading',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await readyTrading(page)
      await page.getByText('No paper session on BTCUSDT').waitFor()
      await quiet(page)
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
    clock: true,
    steps: async (page) => {
      await heldTrading(page)
      await page.locator('.chart-empty').getByText('Loading instruments').waitFor()
      await quiet(page)
    },
  },
  {
    name: 'trading-instruments-failed',
    section: 'trading',
    config: { ...engineTrading, failInstruments: true, candles: null },
    clock: true,
    steps: async (page) => {
      await heldTrading(page)
      await page.locator('.chart-empty').getByText('The instrument lists did not load.').waitFor()
      await quiet(page)
    },
  },
  {
    // The history drawn before any live bar: the stub answers GET /candles at once, and the held
    // clock lets no live bar through.
    name: 'trading-engine-history',
    section: 'trading',
    config: engineTrading,
    clock: true,
    steps: async (page) => {
      await heldTrading(page)
      await page.locator('.ticker-price').filter({ hasText: '64,210.00' }).waitFor()
      await page.getByText('No paper session on BTCUSDT').waitFor()
      await page.locator('.chart-empty').waitFor({ state: 'detached' })
      await draw(page)
    },
  },
  {
    // History that has not answered yet, with the live stream's first bars already drawn.
    name: 'trading-engine-history-loading',
    section: 'trading',
    config: { ...engineTrading, history: { '*': 'hang' } },
    clock: true,
    steps: async (page) => {
      await heldTrading(page)
      await page.locator('.chart-empty').getByText('Loading candle history').waitFor()
      await engineBars(page, 6)
    },
  },
  {
    name: 'trading-engine-history-failed',
    section: 'trading',
    config: { ...engineTrading, history: { '*': 'fail' } },
    clock: true,
    steps: async (page) => {
      await heldTrading(page)
      await page.locator('.chart-empty').getByText('The candle history did not load.').waitFor()
      await engineBars(page, 12)
    },
  },
  {
    // A 5m chart whose last candle is the bar still forming: the stub shows it half way between
    // two closes, so after the twelfth close (3000 ms) the time moves to 3150 ms, past the
    // thirteenth bar's forming step (3125 ms) and short of its close (3250 ms).
    name: 'trading-engine-forming',
    section: 'trading',
    config: engineTrading,
    clock: true,
    steps: async (page) => {
      await heldTrading(page)
      await page.getByRole('button', { name: 'Timeframe: 1m' }).click()
      await page.getByRole('menuitemradio', { name: '5m', exact: true }).click()
      await page.getByRole('button', { name: 'Timeframe: 5m' }).waitFor()
      await page.mouse.move(0, 0)
      await page.locator('.chart-empty').waitFor({ state: 'detached' })
      await engineBars(page, 12)
      await tick(page, 50)
      await page.waitForFunction(() => window.__stubFormed > 12)
      await draw(page, 50)
    },
  },
  {
    name: 'trading-engine-filling',
    section: 'trading',
    config: engineTrading,
    clock: true,
    steps: async (page) => {
      await heldTrading(page)
      await engineBars(page, 36)
    },
  },
  {
    name: 'trading-engine-session',
    section: 'trading',
    config: { ...engineTrading, session: engineSession },
    clock: true,
    steps: async (page) => {
      await heldTrading(page)
      await engineBars(page, 24)
      await page.locator('.position-summary').filter({ hasText: 'SMA cross · Live' }).waitFor()
    },
  },
  {
    name: 'trading-engine-start',
    section: 'trading',
    config: engineTrading,
    clock: true,
    steps: async (page) => {
      await heldTrading(page)
      await engineBars(page, 12)
      await page.getByRole('button', { name: 'Start a session' }).click()
      await page.getByRole('button', { name: /^Strategy:/ }).click()
      await page.getByRole('menu').waitFor()
    },
  },
  {
    name: 'trading-ticker-menu',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await readyTrading(page)
      await page.keyboard.press('Control+k')
      await page.getByRole('listbox', { name: 'Instruments' }).waitFor()
      await quiet(page)
    },
  },
  {
    name: 'trading-ticker-search',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await readyTrading(page)
      await page.keyboard.press('Control+k')
      await page.getByRole('combobox', { name: 'Search instruments' }).fill('eth')
      await page.getByRole('option', { name: /ETHUSDT.*Binance spot/, selected: true }).waitFor()
      await page.keyboard.press('ArrowDown')
      await page.getByRole('option', { name: /ETHBTC/, selected: true }).waitFor()
      await quiet(page)
    },
  },
  {
    name: 'trading-ticker-perpetual',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
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
      await quiet(page)
    },
  },
  {
    name: 'trading-start-session',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await readyTrading(page)
      await page.getByRole('button', { name: 'Start a session' }).click()
      await page.locator('.start-panel').getByText('Start a paper session', { exact: true }).waitFor()
      await quiet(page)
    },
  },
  {
    name: 'trading-order-invalid',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await startManual(page)
      await page.getByPlaceholder('Size').fill('0.00005')
      await page.mouse.move(0, 0)
      await page.getByText(/the venue’s minimum is|the venue's minimum is/).waitFor()
      await quiet(page)
    },
  },
  {
    name: 'trading-session-running',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await holdBtc(page)
      await page.locator('.position-summary').filter({ hasText: 'Manual · Live' }).waitFor()
      await page.mouse.move(0, 0)
      await quiet(page)
    },
  },
  {
    name: 'trading-position-panel',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await holdBtc(page)
      await placeOrder(page, 'Sell', '0.02')
      await page.getByText('Sell 0.02000 BTC at market sent.').waitFor()
      await nextBar(page)
      await page.locator('.position-summary').filter({ hasText: /0\.03000 BTC/ }).waitFor()
      await page.locator('.position-summary').click()
      await page.getByRole('tab', { name: /Fills/ }).click()
      await page.getByRole('tab', { name: /Fills/, selected: true }).waitFor()
      await quiet(page)
    },
  },
  {
    name: 'trading-limit-order',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await startLive(page)
      await page.getByPlaceholder('Size').fill('0.05')
      await page.getByRole('button', { name: /^Order type:/ }).click()
      await page.getByRole('menuitemradio', { name: 'Limit' }).click()
      await page.getByPlaceholder('Price').fill('63800')
      await page.getByRole('button', { name: 'Buy', exact: true }).click()
      await page.mouse.move(0, 0)
      await page.getByText('Buy 0.05000 BTC at 63,800.00 sent.').waitFor()
      await quiet(page)
    },
  },
  {
    name: 'trading-order-rejected',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await startLive(page)
      await placeOrder(page, 'Sell', '0.05')
      await page.mouse.move(0, 0)
      await page.getByText(/^Spot sessions cannot sell more than they hold/).waitFor()
      await quiet(page)
    },
  },
  {
    name: 'trading-reconnecting',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await holdBtc(page)
      await page.evaluate(() => window.tdMockTrading?.reconnect())
      await page.mouse.move(0, 0)
      await page.locator('.position-summary').filter({ hasText: 'Reconnecting to the feed' }).waitFor()
      await quiet(page)
    },
  },
  {
    name: 'trading-dropped',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await startLive(page)
      await page.evaluate(() => window.tdMockTrading?.dropBucket())
      await page.mouse.move(0, 0)
      await page.locator('.position-notice').waitFor()
      await quiet(page)
    },
  },
  {
    name: 'trading-session-stopped',
    section: 'trading',
    config: { engine: 'running', mock: true },
    clock: true,
    steps: async (page) => {
      await holdBtc(page)
      await page.locator('.position-summary').click()
      // The panel starts listening for a click outside it on its next task.
      await tick(page, 100)
      await page.getByRole('button', { name: 'Stop', exact: true }).click()
      await page.locator('.position-summary').filter({ hasText: 'Manual · Stopped' }).waitFor()
      await page.mouse.click(300, 200)
      await page.locator('.session-panel').waitFor({ state: 'detached' })
      await page.mouse.move(0, 0)
      await quiet(page)
    },
  },
]
