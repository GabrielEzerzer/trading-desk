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
  {
    name: 'trading',
    config: { engine: 'running', mock: true },
    steps: async (page) => {
      await page.keyboard.press('Control+2')
      await wait(500)
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
