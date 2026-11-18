// Serves the last renderer build (app/out/renderer) on localhost with app/shots/bridge-stub.js standing in for
// `window.td`, so any renderer state opens in an ordinary browser tab. Run `pnpm serve [--port <n>]` from gui/.
// The page's query string picks the stub's configuration: `preset`, then single fields (gui/README.md).

import { readFileSync } from 'node:fs'
import { createServer } from 'node:http'
import { dirname, extname, join, normalize, resolve, sep } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const app = resolve(dirname(fileURLToPath(import.meta.url)), '..', 'app')
const engineData = JSON.parse(readFileSync(join(app, 'mock', 'engine.json'), 'utf8'))
const trading = JSON.parse(readFileSync(join(app, 'mock', 'trading.json'), 'utf8'))
const stubSource = readFileSync(join(app, 'shots', 'bridge-stub.js'), 'utf8')

export const DEFAULT_PORT = 4319
export const STUB_PATH = '/__bridge-stub.js'

const instruments = trading.instruments.map((i) => i.instrument)
const btc = instruments.find((i) => i.id.symbol === 'BTCUSDT' && i.id.venue === 'binance' && i.id.kind === 'spot')
const engineStart = { open_time: trading.end, open: 64180, high: 64230, low: 64150, close: 64210, volume: 14, quote_volume: 14 * 64210 }
const engineTrading = { engine: 'running', mock: false, instruments, candles: { start: engineStart, everyMs: 250 } }
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

export const PRESETS = {
  mock: { engine: 'running', mock: true },
  engine: { engine: 'running', mock: false },
  'engine-trading': engineTrading,
  'engine-session': { ...engineTrading, session: engineSession },
}

const parseValue = (text) => {
  try {
    return JSON.parse(text)
  } catch {
    return text
  }
}

export function stubConfig(params) {
  const name = params.get('preset') ?? 'mock'
  const preset = PRESETS[name]
  if (!preset) throw new Error(`unknown preset "${name}"; the presets are ${Object.keys(PRESETS).join(', ')}`)
  const fields = Object.fromEntries([...params].filter(([key]) => key !== 'preset').map(([key, value]) => [key, parseValue(value)]))
  return { hangStrategies: false, engineData, ...preset, ...fields }
}

export function injectStub(html, search) {
  const tag = `<script src="${STUB_PATH}${search.replaceAll('&', '&amp;').replaceAll('"', '&quot;')}"></script>`
  const at = html.indexOf('<script')
  if (at >= 0) return html.slice(0, at) + tag + html.slice(at)
  return html.replace('</head>', `${tag}</head>`)
}

const TYPES = {
  '.html': 'text/html',
  '.js': 'text/javascript',
  '.css': 'text/css',
  '.json': 'application/json',
  '.woff2': 'font/woff2',
  '.woff': 'font/woff',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
}

export function createStubServer({ root = join(app, 'out', 'renderer') } = {}) {
  const base = resolve(root)
  return createServer((req, res) => {
    const url = new URL(req.url ?? '/', 'http://localhost')
    const send = (status, type, body) => res.writeHead(status, { 'content-type': type, 'cache-control': 'no-store' }).end(body)
    let config
    try {
      config = stubConfig(url.searchParams)
    } catch (e) {
      send(400, 'text/plain', e.message)
      return
    }
    if (url.pathname === STUB_PATH) {
      send(200, 'text/javascript', `${stubSource}\ninstallBridgeStub(${JSON.stringify(config)})\n`)
      return
    }
    let path
    try {
      path = decodeURIComponent(url.pathname)
    } catch {
      send(400, 'text/plain', 'malformed path')
      return
    }
    const file = normalize(join(base, path === '/' ? 'index.html' : path))
    if (!file.startsWith(base + sep)) {
      send(403, 'text/plain', 'outside the renderer build')
      return
    }
    let body
    try {
      body = readFileSync(file)
    } catch {
      send(404, 'text/plain', 'not found')
      return
    }
    if (file === join(base, 'index.html')) body = injectStub(body.toString('utf8'), url.search)
    send(200, TYPES[extname(file)] ?? 'application/octet-stream', body)
  })
}

function portArg(argv) {
  const at = argv.indexOf('--port')
  if (at < 0) return DEFAULT_PORT
  const port = Number(argv[at + 1])
  if (!Number.isInteger(port) || port < 0 || port > 65535) throw new Error(`--port takes a port number, not "${argv[at + 1]}"`)
  return port
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const root = join(app, 'out', 'renderer')
  try {
    readFileSync(join(root, 'index.html'))
  } catch {
    console.error(`No renderer build in ${root}. Run pnpm build first.`)
    process.exit(1)
  }
  const server = createStubServer({ root })
  server.on('error', (e) => {
    console.error(e.message)
    process.exit(1)
  })
  server.listen(portArg(process.argv.slice(2)), '127.0.0.1', () => {
    console.log(`Serving the renderer build on the bridge stub at http://127.0.0.1:${server.address().port}/`)
  })
}
