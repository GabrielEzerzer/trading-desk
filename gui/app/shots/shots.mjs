// Headless screenshots of the renderer, for looking at the design: every state at three window
// sizes, into gui/shots/ (git-ignored). Run `pnpm shots` from gui/, which builds the app first.
//
// It never launches Electron. It serves the built renderer from app/out/renderer on a local port
// for the length of the run, opens one headless Chromium, injects bridge-stub.js as `window.td`,
// and closes both in `finally`.

import { mkdirSync, readFileSync } from 'node:fs'
import { createServer } from 'node:http'
import { dirname, extname, join, normalize, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { chromium } from 'playwright'

const here = dirname(fileURLToPath(import.meta.url))
const root = resolve(here, '..', 'out', 'renderer')
const outDir = resolve(here, '..', '..', 'shots')
const fixtures = JSON.parse(readFileSync(join(here, '..', 'mock', 'lab.json'), 'utf8'))
const trading = JSON.parse(readFileSync(join(here, '..', 'mock', 'trading.json'), 'utf8'))
const stubSource = readFileSync(join(here, 'bridge-stub.js'), 'utf8')

const SIZES = [
  { name: 'min-900x600', width: 900, height: 600 },
  { name: '1100x720', width: 1100, height: 720 },
  { name: '1600x1000', width: 1600, height: 1000 },
]

const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.woff2': 'font/woff2', '.svg': 'image/svg+xml' }

/** The engine's strategy list, as GET /strategies returns it. */
const engineStrategies = fixtures.strategies.map((s) => ({ name: s.name, description: s.description, params: [] }))

const wait = (ms) => new Promise((r) => setTimeout(r, ms))

const instruments = trading.instruments.map((i) => i.instrument)
const btc = instruments.find((i) => i.id.symbol === 'BTCUSDT' && i.id.venue === 'binance' && i.id.kind === 'spot')
const engineStart = { open_time: trading.end, open: 64180, high: 64230, low: 64150, close: 64210, volume: 14, quote_volume: 14 * 64210 }

/** A running strategy session as the engine reports it, for Trading in engine mode. */
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

const engineTrading = { engine: 'running', mock: false, instruments, candles: { start: engineStart, everyMs: 250 } }

async function toTrading(page) {
  await page.keyboard.press('Control+2')
  await page.locator('.trading, .state').filter({ visible: true }).first().waitFor({ timeout: 5000 })
  await wait(500)
}

/** Trading with the mock source, ready: instruments loaded and the sessions list read. */
async function readyTrading(page) {
  await toTrading(page)
  await page.locator('.position-line .line-button').first().waitFor({ timeout: 5000 })
  await wait(1600)
}

async function startManual(page) {
  await page.getByRole('button', { name: 'Start a session' }).click()
  await wait(300)
  await page.getByRole('button', { name: 'Start', exact: true }).click()
  await page.locator('.position-summary').waitFor({ timeout: 5000 })
}

async function order(page, side, size) {
  await page.getByPlaceholder('Size').fill(size)
  await page.getByRole('button', { name: side, exact: true }).click()
}

async function openPanel(page) {
  await page.locator('.position-summary').click()
  await wait(400)
}

// Each state: the stub's config and the steps that bring the page there.
const STATES = [
  { name: 'engine-starting', config: { engine: 'starting', mock: true } },
  {
    name: 'engine-starting-tooltip',
    config: { engine: 'starting', mock: true },
    steps: async (page) => {
      await page.hover('.engine-dot')
      await wait(700)
    },
  },
  { name: 'engine-failed', config: { engine: 'failed', mock: true } },
  { name: 'strategies-loading', config: { engine: 'running', mock: false, hangStrategies: true } },
  { name: 'lab-list', config: { engine: 'running', mock: true } },
  { name: 'lab-list-engine', config: { engine: 'running', mock: false } },
  {
    name: 'lab-strategy-menu',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.click('.pill')
      await wait(400)
    },
  },
  {
    name: 'lab-never-run',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.getByRole('button', { name: /Donchian breakout/ }).click()
      await wait(400)
      await page.mouse.move(0, 0)
    },
  },
  {
    name: 'lab-strategy-menu-selected',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.getByRole('button', { name: /Donchian breakout/ }).click()
      await wait(300)
      await page.click('.pill')
      await wait(400)
    },
  },
  {
    name: 'lab-overrides-menu',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.getByRole('button', { name: /Donchian breakout/ }).click()
      await wait(300)
      await page.getByRole('button', { name: 'Overrides' }).click()
      await wait(400)
    },
  },
  {
    name: 'lab-running',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.getByRole('button', { name: /Donchian breakout/ }).click()
      await wait(300)
      await page.getByRole('button', { name: 'Run', exact: true }).click()
      await page.mouse.move(0, 0)
      await wait(2300)
    },
  },
  {
    name: 'lab-result-fresh-trade',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.getByRole('button', { name: /Donchian breakout/ }).click()
      await wait(300)
      await page.getByRole('button', { name: 'Run', exact: true }).click()
      await page.mouse.move(0, 0)
      await page.getByRole('heading', { name: 'Trade', exact: true }).waitFor({ timeout: 15_000 })
      await wait(1200)
    },
  },
  {
    name: 'lab-result-trade',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.getByRole('button', { name: /SMA cross/ }).click()
      await page.mouse.move(0, 0)
      await wait(500)
    },
  },
  {
    name: 'lab-result-no-trade',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.getByRole('button', { name: /RSI reversion/ }).click()
      await page.mouse.move(0, 0)
      await wait(500)
    },
  },
  {
    name: 'lab-result-evidence',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.getByRole('button', { name: /SMA cross/ }).click()
      await page.mouse.move(0, 0)
      await wait(400)
      await page.evaluate(() => document.getElementById('equity-title')?.scrollIntoView({ block: 'start' }))
      await wait(300)
    },
  },
  {
    name: 'lab-result-long-trades',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.getByRole('button', { name: /RSI reversion/ }).click()
      await page.mouse.move(0, 0)
      await wait(400)
      await page.evaluate(() => {
        const scroller = document.querySelector('.lab-scroll')
        const title = document.getElementById('trades-title')
        if (scroller && title) scroller.scrollTop = title.offsetTop + 600
      })
      await wait(300)
    },
  },
  {
    name: 'lab-run-failed',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.getByRole('button', { name: /Funding carry/ }).click()
      await wait(300)
      await page.getByRole('button', { name: 'Run', exact: true }).click()
      await page.mouse.move(0, 0)
      await page.getByText('Run failed').waitFor({ timeout: 10_000 })
      await wait(400)
    },
  },
  {
    name: 'lab-engine-cannot-run',
    config: { engine: 'running', mock: false },
    steps: async (page) => {
      await page.getByRole('button', { name: /SMA cross/ }).click()
      await wait(400)
      await page.hover('.run')
      await wait(700)
    },
  },
  { name: 'trading-engine-starting', config: { engine: 'starting', mock: true }, steps: toTrading },
  { name: 'trading-engine-failed', config: { engine: 'failed', mock: true }, steps: toTrading },
  {
    name: 'trading-instruments-loading',
    config: { ...engineTrading, hangInstruments: true, candles: null },
    steps: toTrading,
  },
  {
    name: 'trading-instruments-failed',
    config: { ...engineTrading, failInstruments: true, candles: null },
    steps: toTrading,
  },
  { name: 'trading-engine-empty', config: { ...engineTrading, candles: null }, steps: toTrading },
  {
    name: 'trading-engine-filling',
    config: engineTrading,
    steps: async (page) => {
      await toTrading(page)
      await wait(9000)
    },
  },
  {
    name: 'trading-engine-session',
    config: { ...engineTrading, session: engineSession },
    steps: async (page) => {
      await toTrading(page)
      await wait(6000)
    },
  },
  {
    name: 'trading-engine-start',
    config: engineTrading,
    steps: async (page) => {
      await toTrading(page)
      await page.getByRole('button', { name: 'Start a session' }).click()
      await wait(300)
      await page.getByRole('button', { name: /^Strategy:/ }).click()
      await wait(400)
    },
  },
  { name: 'trading-no-session', config: { engine: 'running', mock: true }, steps: readyTrading },
  {
    name: 'trading-ticker-menu',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await page.keyboard.press('Control+k')
      await wait(500)
    },
  },
  {
    name: 'trading-ticker-search',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await page.keyboard.press('Control+k')
      await wait(300)
      await page.keyboard.type('eth')
      await page.keyboard.press('ArrowDown')
      await wait(300)
    },
  },
  {
    name: 'trading-ticker-perpetual',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await page.keyboard.press('Control+k')
      await wait(300)
      await page.keyboard.type('ethusdt')
      await page.getByRole('option', { name: /ETHUSDT.*Bybit perpetual/ }).click()
      await wait(1600)
      await page.keyboard.press('Control+k')
      await wait(300)
      await page.getByRole('radio', { name: 'Perpetual' }).click()
      await wait(300)
    },
  },
  {
    name: 'trading-start-session',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await page.getByRole('button', { name: 'Start a session' }).click()
      await wait(400)
    },
  },
  {
    name: 'trading-order-invalid',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await startManual(page)
      await page.getByPlaceholder('Size').fill('0.00005')
      await page.mouse.move(0, 0)
      await wait(300)
    },
  },
  {
    name: 'trading-session-running',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await startManual(page)
      await order(page, 'Buy', '0.05')
      await page.mouse.move(0, 0)
      await wait(5000)
    },
  },
  {
    name: 'trading-position-panel',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await startManual(page)
      await order(page, 'Buy', '0.05')
      await wait(3500)
      await order(page, 'Sell', '0.02')
      await wait(3500)
      await openPanel(page)
      await page.getByRole('tab', { name: /Fills/ }).click()
      await wait(300)
    },
  },
  {
    name: 'trading-limit-order',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await startManual(page)
      await page.getByPlaceholder('Size').fill('0.05')
      await page.getByRole('button', { name: /^Order type:/ }).click()
      await wait(300)
      await page.getByRole('menuitemradio', { name: 'Limit' }).click()
      await wait(300)
      await page.getByPlaceholder('Price').fill('63800')
      await page.getByRole('button', { name: 'Buy', exact: true }).click()
      await page.mouse.move(0, 0)
      await wait(3500)
    },
  },
  {
    name: 'trading-order-rejected',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await startManual(page)
      await order(page, 'Sell', '0.05')
      await page.mouse.move(0, 0)
      await wait(3500)
    },
  },
  {
    name: 'trading-reconnecting',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await startManual(page)
      await order(page, 'Buy', '0.05')
      await wait(3500)
      await page.evaluate(() => window.tdMockTrading?.reconnect())
      await page.mouse.move(0, 0)
      await wait(900)
    },
  },
  {
    name: 'trading-dropped',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await startManual(page)
      await wait(3500)
      await page.evaluate(() => window.tdMockTrading?.dropBucket())
      await page.mouse.move(0, 0)
      await wait(2000)
    },
  },
  {
    name: 'trading-session-stopped',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await readyTrading(page)
      await startManual(page)
      await order(page, 'Buy', '0.05')
      await wait(3500)
      await openPanel(page)
      await page.getByRole('button', { name: 'Stop', exact: true }).click()
      await wait(500)
      await page.mouse.click(300, 200)
      await page.mouse.move(0, 0)
      await wait(600)
    },
  },
]

function serve() {
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
    server.listen(0, '127.0.0.1', () => resolveServer(server))
  })
}

const only = process.argv[2] ? new RegExp(process.argv[2]) : null
const server = await serve()
const url = `http://127.0.0.1:${server.address().port}/`
let browser
let failures = 0
try {
  browser = await chromium.launch({ headless: true })
  for (const size of SIZES) {
    mkdirSync(join(outDir, size.name), { recursive: true })
    for (const state of STATES) {
      if (only && !only.test(state.name)) continue
      const context = await browser.newContext({ viewport: { width: size.width, height: size.height } })
      const page = await context.newPage()
      const errors = []
      page.on('pageerror', (e) => errors.push(e.message))
      page.on('console', (m) => m.type() === 'error' && errors.push(m.text()))
      try {
        const config = { strategies: engineStrategies, hangStrategies: false, ...state.config }
        await page.addInitScript(`${stubSource}\ninstallBridgeStub(${JSON.stringify(config)})`)
        await page.goto(url)
        await page.evaluate(() => document.fonts.ready)
        await wait(600)
        await state.steps?.(page)
        const file = join(outDir, size.name, `${state.name}.png`)
        await page.screenshot({ path: file })
        console.log(`${size.name}/${state.name}.png${errors.length ? `  errors: ${errors.join(' | ')}` : ''}`)
        if (errors.length) failures += 1
      } catch (e) {
        failures += 1
        console.error(`${size.name}/${state.name}: FAILED ${e instanceof Error ? e.message : e}`)
      } finally {
        await context.close()
      }
    }
  }
} finally {
  await browser?.close()
  await new Promise((r) => server.close(r))
}
console.log(failures ? `shots: ${failures} with errors` : 'shots: done')
process.exit(failures ? 1 : 0)
