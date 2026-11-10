// Writes the Lab's mock fixtures, made from a seeded random walk so they are the same on every run:
//
// - lab.json, for the mock source (TD_MOCK=1): plausible results with verdicts for a few strategies.
// - engine.json, for the bridge stub in app/shots/ that stands in for the engine: the strategy list,
//   saved backtests as the engine's API returns them (one short enough that its Sharpe, Sortino,
//   volatility and CAGR are null), stored-data coverage and instrument rules.
//
// Run it again only to change the fixtures:
//
//   node app/mock/generate.mjs        from gui/

import { writeFileSync } from 'node:fs'

const DAY = 86_400_000

function rng(seed) {
  let s = seed >>> 0
  return () => {
    s = (s + 0x6d2b79f5) >>> 0
    let t = s
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

function normal(rand) {
  return Math.sqrt(-2 * Math.log(rand() || 1e-12)) * Math.cos(2 * Math.PI * rand())
}

const round = (x, d) => Math.round(x * 10 ** d) / 10 ** d

/** A daily price path and a strategy equity path that takes part of its moves plus an edge. */
function series({ seed, from, days, startPrice, drift, vol, exposure, edge, edgeVol }) {
  const rand = rng(seed)
  const price = [startPrice]
  const equity = [1]
  for (let i = 1; i < days; i++) {
    const move = drift + vol * normal(rand)
    price.push(price[i - 1] * Math.exp(move))
    const own = exposure * move + edge + edgeVol * normal(rand)
    equity.push(equity[i - 1] * Math.exp(own))
  }
  const points = []
  for (let i = 0; i < days; i += 7) {
    points.push({ t: from + i * DAY, strategy: round(equity[i], 4), benchmark: round(price[i] / startPrice, 4) })
  }
  const last = days - 1
  points.push({ t: from + last * DAY, strategy: round(equity[last], 4), benchmark: round(price[last] / startPrice, 4) })
  return { price, equity, points }
}

function stats(curve) {
  const rets = curve.slice(1).map((v, i) => v / curve[i] - 1)
  const mean = rets.reduce((a, b) => a + b, 0) / rets.length
  const sd = Math.sqrt(rets.reduce((a, b) => a + (b - mean) ** 2, 0) / rets.length)
  let peak = curve[0]
  let maxDd = 0
  for (const v of curve) {
    peak = Math.max(peak, v)
    maxDd = Math.max(maxDd, 1 - v / peak)
  }
  const years = curve.length / 365
  const total = curve.at(-1) / curve[0] - 1
  return {
    total,
    cagr: (1 + total) ** (1 / years) - 1,
    sharpe: (mean / sd) * Math.sqrt(365),
    maxDd,
    vol: sd * Math.sqrt(365),
  }
}

function trades({ seed, count, from, days, price, winRate, avgWin, avgLoss, allowShort }) {
  const rand = rng(seed)
  const out = []
  const span = Math.floor(days / count)
  for (let i = 0; i < count; i++) {
    const open = i * span + Math.floor(rand() * span * 0.3)
    const hold = 1 + Math.floor(rand() * span * 0.6)
    const close = Math.min(open + hold, days - 1)
    const win = rand() < winRate
    const r = win ? avgWin * (0.4 + rand() * 1.2) : -avgLoss * (0.4 + rand() * 1.2)
    const side = allowShort && rand() < 0.4 ? 'short' : 'long'
    const entry = price[open]
    const exit = side === 'long' ? entry * (1 + r) : entry * (1 - r)
    out.push({
      id: i + 1,
      side,
      entryTime: from + open * DAY,
      exitTime: from + close * DAY,
      entryPrice: round(entry, 2),
      exitPrice: round(exit, 2),
      returnPct: round(r * 100, 2),
    })
  }
  return out
}

function result({ instrument, venue, kind, from, days, finishedAt, walk, trade, benchmarkLabel }) {
  const s = series({ ...walk, from, days })
  const list = trades({ ...trade, from, days, price: s.price })
  const st = stats(s.equity)
  const bh = stats(s.price)
  const wins = list.filter((t) => t.returnPct > 0)
  const losses = list.filter((t) => t.returnPct <= 0)
  const winRate = wins.length / list.length
  const grossWin = wins.reduce((a, t) => a + t.returnPct, 0)
  const grossLoss = -losses.reduce((a, t) => a + t.returnPct, 0)
  const profitFactor = grossLoss === 0 ? grossWin : grossWin / grossLoss
  // Out-of-sample Sharpe is lower than the full-period one, as it is in practice.
  const oosSharpe = st.sharpe * 0.82
  const criteria = [
    { label: 'Out-of-sample Sharpe', value: round(oosSharpe, 2), limit: 1, rule: 'at_least', format: 'ratio' },
    { label: 'Max drawdown', value: round(st.maxDd, 4), limit: 0.25, rule: 'at_most', format: 'percent' },
    { label: 'Trades', value: list.length, limit: 30, rule: 'at_least', format: 'count' },
  ].map((c) => ({ ...c, pass: c.rule === 'at_least' ? c.value >= c.limit : c.value <= c.limit }))
  return {
    decision: criteria.every((c) => c.pass) ? 'trade' : 'no_trade',
    instrument: { venue, symbol: instrument, kind },
    period: { from, to: from + (days - 1) * DAY },
    finishedAt,
    criteria,
    keyNumbers: [
      { label: 'Return', value: round(st.total, 4), format: 'percent', tone: 'pnl' },
      { label: 'CAGR', value: round(st.cagr, 4), format: 'percent', tone: 'pnl' },
      { label: 'Sharpe', value: round(st.sharpe, 2), format: 'ratio' },
      { label: 'Max drawdown', value: round(st.maxDd, 4), format: 'percent' },
    ],
    benchmarkLabel,
    equity: s.points,
    metrics: [
      { label: 'Total return', strategy: round(st.total, 4), benchmark: round(bh.total, 4), format: 'percent' },
      { label: 'CAGR', strategy: round(st.cagr, 4), benchmark: round(bh.cagr, 4), format: 'percent' },
      { label: 'Volatility', strategy: round(st.vol, 4), benchmark: round(bh.vol, 4), format: 'percent' },
      { label: 'Sharpe', strategy: round(st.sharpe, 2), benchmark: round(bh.sharpe, 2), format: 'ratio' },
      { label: 'Max drawdown', strategy: round(st.maxDd, 4), benchmark: round(bh.maxDd, 4), format: 'percent' },
      { label: 'Win rate', strategy: round(winRate, 4), benchmark: null, format: 'percent' },
      { label: 'Profit factor', strategy: round(profitFactor, 2), benchmark: null, format: 'ratio' },
      { label: 'Trades', strategy: list.length, benchmark: null, format: 'count' },
    ],
    trades: list,
  }
}

const FROM = Date.UTC(2021, 0, 1)
const DAYS = 1642 // to 2025-06-30

const sma = result({
  instrument: 'BTCUSDT',
  venue: 'binance',
  kind: 'perpetual',
  from: FROM,
  days: DAYS,
  finishedAt: Date.UTC(2026, 10, 9, 16, 42),
  walk: { seed: 7, startPrice: 29_000, drift: 0.0007, vol: 0.034, exposure: 0.2, edge: 0.0011, edgeVol: 0.008 },
  trade: { seed: 11, count: 41, winRate: 0.46, avgWin: 0.11, avgLoss: 0.04, allowShort: true },
  benchmarkLabel: 'Buy and hold',
})

const rsi = result({
  instrument: 'ETHUSDT',
  venue: 'bybit',
  kind: 'spot',
  from: FROM,
  days: DAYS,
  finishedAt: Date.UTC(2026, 10, 8, 10, 15),
  walk: { seed: 21, startPrice: 730, drift: 0.0003, vol: 0.045, exposure: 0.5, edge: -0.0001, edgeVol: 0.012 },
  trade: { seed: 23, count: 126, winRate: 0.58, avgWin: 0.025, avgLoss: 0.033, allowShort: false },
  benchmarkLabel: 'Buy and hold',
})

const donchian = result({
  instrument: 'SOLUSDT',
  venue: 'bybit',
  kind: 'perpetual',
  from: FROM,
  days: DAYS,
  finishedAt: 0, // set when the mock run finishes
  walk: { seed: 33, startPrice: 1.5, drift: 0.0009, vol: 0.055, exposure: 0.12, edge: 0.0008, edgeVol: 0.009 },
  trade: { seed: 35, count: 57, winRate: 0.39, avgWin: 0.16, avgLoss: 0.05, allowShort: true },
  benchmarkLabel: 'Buy and hold',
})

// A run id as the engine makes them: the save time, then 8 hex digits of the result's hash.
const runId = (savedAt, hash) => `${String(savedAt).padStart(13, '0')}-${hash}`

/** A strategy list row for a result: its id, when it finished, its return and its verdict. */
const lastRun = (id, r) => ({ id, finishedAt: r.finishedAt, totalReturn: r.keyNumbers[0].value, decision: r.decision })

const SMA_ID = runId(sma.finishedAt, '5a1c0e2b')
const RSI_ID = runId(rsi.finishedAt, 'c03d91f7')

const fixtures = {
  strategies: [
    { name: 'sma_cross', kind: 'Trend following', description: 'Goes long when the fast moving average crosses above the slow one, and short or flat when it crosses below.', last: lastRun(SMA_ID, sma) },
    { name: 'rsi_reversion', kind: 'Mean reversion', description: 'Buys when RSI falls below its oversold level and sells when it recovers to the middle.', last: lastRun(RSI_ID, rsi) },
    { name: 'donchian_breakout', kind: 'Breakout', description: 'Enters on a close beyond the highest high or lowest low of the channel, with an ATR stop.', last: null },
    { name: 'funding_carry', kind: 'Carry', description: 'Holds the side of a perpetual that collects funding while the rate stays above its threshold.', last: null },
  ],
  results: { [SMA_ID]: sma, [RSI_ID]: rsi },
  datasets: [
    { venue: 'binance', symbol: 'BTCUSDT', kind: 'perpetual' },
    { venue: 'bybit', symbol: 'ETHUSDT', kind: 'spot' },
    { venue: 'bybit', symbol: 'SOLUSDT', kind: 'perpetual' },
  ],
  runs: {
    donchian_breakout: {
      steps: [
        { step: 1, steps: 4, stage: 'Loading candles for SOLUSDT perpetual, Bybit' },
        { step: 2, steps: 4, stage: 'Sweeping parameters', done: 8, total: 48 },
        { step: 2, steps: 4, stage: 'Sweeping parameters', done: 24, total: 48 },
        { step: 2, steps: 4, stage: 'Sweeping parameters', done: 48, total: 48 },
        { step: 3, steps: 4, stage: 'Walk-forward windows', done: 2, total: 5 },
        { step: 3, steps: 4, stage: 'Walk-forward windows', done: 4, total: 5 },
        { step: 4, steps: 4, stage: 'Testing out of sample' },
      ],
      result: donchian,
    },
    funding_carry: {
      steps: [
        { step: 1, steps: 3, stage: 'Loading candles for ETHUSDT perpetual, Bybit' },
        { step: 2, steps: 3, stage: 'Loading funding rates' },
      ],
      error: 'No funding rates are stored for ETHUSDT perpetual on Bybit before March 2023. Download them first, then run again.',
    },
  },
}

// The engine's side, for the bridge stub: saved backtests in the API's own shapes.

const STARTING_CASH = 10_000

function instrumentRules(id, tick, lot) {
  const base = id.symbol.replace(/USDT$/, '')
  return { id, base, quote: 'USDT', tick_size: tick, lot_size: lot, min_qty: lot, min_notional: 5, fees: { maker: 0.0002, taker: 0.0005 } }
}

const BTC_PERP = instrumentRules({ venue: 'binance', symbol: 'BTCUSDT', kind: 'perpetual' }, 0.1, 0.001)
const ETH_SPOT = instrumentRules({ venue: 'bybit', symbol: 'ETHUSDT', kind: 'spot' }, 0.01, 0.0001)
// The instrument the Lab offers to download when nothing is stored.
const BTC_SPOT = instrumentRules({ venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' }, 0.01, 0.00001)

/** A metrics report; the per-day ratios are null when the run is too short for them, as the engine does. */
function metricsReport(curve, list, short) {
  const st = stats(curve)
  const pnl = list.map((t) => t.net_pnl)
  const wins = pnl.filter((x) => x > 0)
  const losses = pnl.filter((x) => x <= 0)
  const sorted = [...pnl].sort((a, b) => a - b)
  const grossLoss = -losses.reduce((a, b) => a + b, 0)
  return {
    start_equity: STARTING_CASH,
    end_equity: round(curve.at(-1) * STARTING_CASH, 2),
    total_return: round(st.total, 4),
    cagr: short ? null : round(st.cagr, 4),
    volatility: short ? null : round(st.vol, 4),
    sharpe: short ? null : round(st.sharpe, 2),
    sortino: short ? null : round(st.sharpe * 1.35, 2),
    max_drawdown: round(st.maxDd, 4),
    max_drawdown_duration_ms: null,
    max_drawdown_duration_bars: null,
    calmar: short || st.maxDd === 0 ? null : round(st.cagr / st.maxDd, 2),
    trades: list.length,
    win_rate: list.length ? round(wins.length / list.length, 4) : null,
    profit_factor: grossLoss > 0 ? round(wins.reduce((a, b) => a + b, 0) / grossLoss, 2) : null,
    avg_trade_pnl: list.length ? round(pnl.reduce((a, b) => a + b, 0) / list.length, 2) : null,
    median_trade_pnl: list.length ? round(sorted[Math.floor(sorted.length / 2)], 2) : null,
    avg_bars_held: list.length ? round(list.reduce((a, t) => a + t.bars_held, 0) / list.length, 1) : null,
    exposure: list.length ? 0.42 : null,
    turnover: list.length ? 3.1 : null,
    fees_paid: round(list.reduce((a, t) => a + t.fees, 0), 2),
    funding_paid: 0,
  }
}

/** A saved backtest as GET /runs/{id} returns it, and its row in GET /runs. */
function savedBacktest({ id, strategy, params, instrument, from, days, walk, trade, short = false }) {
  const s = series({ ...walk, from, days })
  const list = trades({ ...trade, from, days, price: s.price }).map((t) => {
    const qty = round(STARTING_CASH / t.entryPrice, 4)
    const sign = t.side === 'long' ? 1 : -1
    const gross = round(sign * qty * (t.exitPrice - t.entryPrice), 2)
    const fees = round(qty * (t.entryPrice + t.exitPrice) * 0.0005, 2)
    return {
      side: t.side,
      entry_time: t.entryTime,
      exit_time: t.exitTime,
      open: false,
      qty,
      entry_price: t.entryPrice,
      exit_price: t.exitPrice,
      gross_pnl: gross,
      fees,
      funding: 0,
      net_pnl: round(gross - fees, 2),
      return_on_notional: round((gross - fees) / (t.entryPrice * qty), 4),
      bars_held: Math.max(1, Math.round((t.exitTime - t.entryTime) / DAY)),
    }
  })
  const to = from + days * DAY
  const config = { strategy, params, instrument, timeframe: '1d', from, to, starting_cash: STARTING_CASH, slippage_bps: 2, fee_override: null }
  const equity = s.equity.map((e, i) => ({ time: from + i * DAY, equity: round(e * STARTING_CASH, 2) }))
  const startPrice = s.price[0]
  const relative = s.price.map((p) => p / startPrice)
  const benchmark = {
    qty: round(STARTING_CASH / startPrice, 4),
    entry_price: round(startPrice, 2),
    equity: relative.map((r, i) => ({ time: from + i * DAY, equity: round(r * STARTING_CASH, 2) })),
    metrics: metricsReport(relative, [], short),
    excess_return: null,
    correlation: null,
  }
  const metrics = metricsReport(s.equity, list, short)
  const result = { config, engine_version: '0.1.0', engine_revision: 3, data_fingerprint: 'f'.repeat(64), equity, orders: [], fills: [], trades: list, metrics, benchmark }
  const detail = {
    id,
    run: { kind: 'backtest', result },
    data: { bars: days, gaps: [], dropped: [], tolerated: [], funding_rates: 0 },
    data_error: null,
    check: null,
  }
  const summary = {
    id,
    created_at: Number(id.slice(0, 13)),
    kind: 'backtest',
    strategy,
    instrument: instrument.id,
    timeframe: '1d',
    from,
    to,
    total_return: metrics.total_return,
    cagr: metrics.cagr,
    sharpe: metrics.sharpe,
    max_drawdown: metrics.max_drawdown,
    trades: metrics.trades,
  }
  return { detail, summary }
}

const ENGINE_FROM = Date.UTC(2024, 0, 1)
const ENGINE_DAYS = 547 // to 2025-06-30

const smaPast = savedBacktest({
  id: runId(Date.UTC(2026, 10, 9, 16, 42), '9e4b7a10'),
  strategy: 'sma_cross',
  params: { fast: 10, slow: 30 },
  instrument: BTC_PERP,
  from: ENGINE_FROM,
  days: ENGINE_DAYS,
  walk: { seed: 41, startPrice: 42_000, drift: 0.0012, vol: 0.03, exposure: 0.35, edge: 0.0006, edgeVol: 0.007 },
  trade: { seed: 43, count: 18, winRate: 0.5, avgWin: 0.09, avgLoss: 0.035, allowShort: true },
})

// An older run of the same strategy: the list shows the latest one.
const smaOlder = savedBacktest({
  id: runId(Date.UTC(2026, 9, 2, 9, 5), '1f2e3d4c'),
  strategy: 'sma_cross',
  params: { fast: 10, slow: 30 },
  instrument: BTC_PERP,
  from: ENGINE_FROM,
  days: 120,
  walk: { seed: 45, startPrice: 42_000, drift: 0.001, vol: 0.03, exposure: 0.3, edge: 0, edgeVol: 0.01 },
  trade: { seed: 47, count: 4, winRate: 0.5, avgWin: 0.05, avgLoss: 0.03, allowShort: true },
})

// Three weeks of bars: too short for the per-day ratios, so they are null.
const rsiShort = savedBacktest({
  id: runId(Date.UTC(2026, 10, 10, 8, 30), 'b7c8d9e0'),
  strategy: 'rsi_reversion',
  params: { period: 14, oversold: 30 },
  instrument: ETH_SPOT,
  from: Date.UTC(2025, 5, 2),
  days: 21,
  walk: { seed: 51, startPrice: 2_500, drift: 0.001, vol: 0.03, exposure: 0.5, edge: 0.001, edgeVol: 0.01 },
  trade: { seed: 53, count: 3, winRate: 0.67, avgWin: 0.03, avgLoss: 0.02, allowShort: false },
  short: true,
})

// What a backtest job saves in the stub's run flows.
const donchianRun = savedBacktest({
  id: runId(Date.UTC(2026, 10, 10, 12, 0), '0d1e2f3a'),
  strategy: 'donchian_breakout',
  params: { channel: 20 },
  instrument: BTC_PERP,
  from: ENGINE_FROM,
  days: ENGINE_DAYS,
  walk: { seed: 61, startPrice: 42_000, drift: 0.0012, vol: 0.03, exposure: 0.15, edge: 0.0009, edgeVol: 0.008 },
  trade: { seed: 63, count: 22, winRate: 0.41, avgWin: 0.12, avgLoss: 0.04, allowShort: true },
})

/** GET /data/coverage's months for a stored range, one entry per calendar month. */
function coverageMonths(from, to) {
  const months = []
  for (let t = from; t < to; ) {
    const d = new Date(t)
    const monthStart = Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), 1)
    const next = Date.UTC(d.getUTCFullYear(), d.getUTCMonth() + 1, 1)
    const end = Math.min(to, next)
    const month = `${d.getUTCFullYear()}-${String(d.getUTCMonth() + 1).padStart(2, '0')}`
    months.push({ month, minutes: (end - t) / 60_000, month_minutes: (next - monthStart) / 60_000, first: t, last: end - 60_000, gaps: [] })
    t = end
  }
  return months
}

const paramSpecs = (...specs) =>
  specs.map(([name, kind, value, min, max, description]) => ({ name, kind, default: value, min, max, description }))

const engine = {
  strategies: [
    { name: 'sma_cross', description: fixtures.strategies[0].description, params: paramSpecs(['fast', 'int', 10, 2, 200, 'Fast average length, in bars.'], ['slow', 'int', 30, 3, 400, 'Slow average length, in bars.']) },
    { name: 'rsi_reversion', description: fixtures.strategies[1].description, params: paramSpecs(['period', 'int', 14, 2, 100, 'RSI length, in bars.'], ['oversold', 'float', 30, 1, 50, 'The RSI level to buy below.']) },
    { name: 'donchian_breakout', description: fixtures.strategies[2].description, params: paramSpecs(['channel', 'int', 20, 5, 200, 'Channel length, in bars.']) },
    { name: 'funding_carry', description: fixtures.strategies[3].description, params: [] },
  ],
  instruments: [BTC_PERP, ETH_SPOT, BTC_SPOT],
  // Stored minutes per instrument, keyed `venue:symbol:kind`.
  coverage: {
    'binance:BTCUSDT:perpetual': coverageMonths(ENGINE_FROM, Date.UTC(2025, 6, 1)),
    'bybit:ETHUSDT:spot': coverageMonths(Date.UTC(2025, 5, 2), Date.UTC(2025, 5, 23)),
  },
  runs: [smaPast.summary, smaOlder.summary, rsiShort.summary],
  details: Object.fromEntries([smaPast, rsiShort, donchianRun].map((r) => [r.detail.id, r.detail])),
  // The run the stub's backtest jobs save.
  runResult: donchianRun.detail.id,
}

for (const r of [sma, rsi]) console.log(r.instrument.symbol, r.decision, r.criteria.map((c) => `${c.label}=${c.value}`).join(' '))
console.log(engine.runs.map((r) => `${r.strategy} return=${r.total_return} sharpe=${r.sharpe}`).join(' | '))
writeFileSync(new URL('./engine.json', import.meta.url), JSON.stringify(engine, null, 1) + '\n')
console.log('donchian', donchian.decision, donchian.criteria.map((c) => `${c.label}=${c.value}`).join(' '))
writeFileSync(new URL('./lab.json', import.meta.url), JSON.stringify(fixtures, null, 1) + '\n')
