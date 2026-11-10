// Writes lab.json, the Lab's mock fixtures: plausible results for a few strategies, made from a
// seeded random walk so they are the same on every run. Run it again only to change the fixtures:
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

const fixtures = {
  strategies: [
    { name: 'sma_cross', kind: 'Trend following', description: 'Goes long when the fast moving average crosses above the slow one, and short or flat when it crosses below.', last: sma },
    { name: 'rsi_reversion', kind: 'Mean reversion', description: 'Buys when RSI falls below its oversold level and sells when it recovers to the middle.', last: rsi },
    { name: 'donchian_breakout', kind: 'Breakout', description: 'Enters on a close beyond the highest high or lowest low of the channel, with an ATR stop.', last: null },
    { name: 'funding_carry', kind: 'Carry', description: 'Holds the side of a perpetual that collects funding while the rate stays above its threshold.', last: null },
  ],
  runs: {
    donchian_breakout: {
      steps: [
        [0.04, 'Loading candles for SOLUSDT perpetual, Bybit'],
        [0.12, 'Sweeping parameters, 8 of 48'],
        [0.3, 'Sweeping parameters, 24 of 48'],
        [0.46, 'Sweeping parameters, 48 of 48'],
        [0.6, 'Walk-forward window 2 of 5'],
        [0.76, 'Walk-forward window 4 of 5'],
        [0.9, 'Testing out of sample'],
        [1, 'Writing results'],
      ],
      result: donchian,
    },
    funding_carry: {
      steps: [
        [0.04, 'Loading candles for ETHUSDT perpetual, Bybit'],
        [0.09, 'Loading funding rates'],
      ],
      error: 'No funding rates are stored for ETHUSDT perpetual on Bybit before March 2023. Download them first, then run again.',
    },
  },
}

for (const s of fixtures.strategies) {
  if (s.last) console.log(s.name, s.last.decision, s.last.criteria.map((c) => `${c.label}=${c.value}`).join(' '))
}
console.log('donchian', donchian.decision, donchian.criteria.map((c) => `${c.label}=${c.value}`).join(' '))
writeFileSync(new URL('./lab.json', import.meta.url), JSON.stringify(fixtures, null, 1) + '\n')
