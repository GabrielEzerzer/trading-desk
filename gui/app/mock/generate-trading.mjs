// Writes trading.json, Trading's mock fixtures: instruments for both venues with plausible trading
// rules, and candle history for BTCUSDT and ETHUSDT: 1950 hourly bars, then 3000 one-minute bars
// that continue them, with the last 50 hourly bars made from those minutes so both timeframes tell
// one story. Seeded, so every run writes the same file. Run it again only to change the fixtures:
//
//   node app/mock/generate-trading.mjs        from gui/

import { writeFileSync } from 'node:fs'

const MINUTE = 60_000
const HOUR = 3_600_000
// The history ends here; the mock's live bars continue from it.
const END = Date.UTC(2026, 10, 10, 12, 0)
const HOURS = 1950
const MINUTES = 3000

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

const onTick = (x, tick) => Math.round(Math.round(x / tick) * tick * 1e8) / 1e8

/** Bars of a random walk with slowly changing volatility, as [open_time, o, h, l, c, volume]. */
function walk(rand, { from, count, span, start, vol, tick, volume }) {
  const bars = []
  let price = start
  let regime = 1
  for (let i = 0; i < count; i++) {
    regime = Math.min(2.5, Math.max(0.4, regime * Math.exp(0.08 * normal(rand))))
    const open = price
    // Four steps inside the bar give it a high and low that are not just open and close.
    let high = open
    let low = open
    for (let k = 0; k < 4; k++) {
      price *= Math.exp((vol * regime * normal(rand)) / 2)
      high = Math.max(high, price)
      low = Math.min(low, price)
    }
    const v = volume * regime * (0.5 + rand())
    bars.push([from + i * span, onTick(open, tick), onTick(high, tick), onTick(low, tick), onTick(price, tick), Math.round(v * 1000) / 1000])
  }
  return bars
}

function aggregate(bars, span) {
  const out = []
  for (const b of bars) {
    const t = Math.floor(b[0] / span) * span
    const last = out.at(-1)
    if (last && last[0] === t) {
      last[2] = Math.max(last[2], b[2])
      last[3] = Math.min(last[3], b[3])
      last[4] = b[4]
      last[5] = Math.round((last[5] + b[5]) * 1000) / 1000
    } else out.push([t, b[1], b[2], b[3], b[4], b[5]])
  }
  return out
}

/** Scales every price so the last close lands on `last`, the instrument's reference price. */
function scaleTo(bars, factor, tick) {
  return bars.map(([t, o, h, l, c, v]) => [t, onTick(o * factor, tick), onTick(h * factor, tick), onTick(l * factor, tick), onTick(c * factor, tick), v])
}

function history(seed, { start, last, hourVol, tick, hourVolume }) {
  const rand = rng(seed)
  const minuteFrom = END - MINUTES * MINUTE
  const hourFrom = minuteFrom - HOURS * HOUR
  const hours = walk(rand, { from: hourFrom, count: HOURS, span: HOUR, start, vol: hourVol, tick, volume: hourVolume })
  const minutes = walk(rand, {
    from: minuteFrom,
    count: MINUTES,
    span: MINUTE,
    start: hours.at(-1)[4],
    vol: hourVol / Math.sqrt(60),
    tick,
    volume: hourVolume / 60,
  })
  const factor = last / minutes.at(-1)[4]
  return { '1m': scaleTo(minutes, factor, tick), '1h': scaleTo([...hours, ...aggregate(minutes, HOUR)], factor, tick) }
}

const fees = (maker, taker) => ({ maker, taker })

/** [symbol, base, quote, tick, lot, min_qty, min_notional, reference price]. */
const SPOT = [
  ['BTCUSDT', 'BTC', 'USDT', 0.01, 0.00001, 0.00001, 5, 64210],
  ['ETHUSDT', 'ETH', 'USDT', 0.01, 0.0001, 0.0001, 5, 3120],
  ['SOLUSDT', 'SOL', 'USDT', 0.01, 0.001, 0.001, 5, 152.4],
  ['BNBUSDT', 'BNB', 'USDT', 0.01, 0.001, 0.001, 5, 588.1],
  ['XRPUSDT', 'XRP', 'USDT', 0.0001, 0.1, 0.1, 5, 0.5321],
  ['DOGEUSDT', 'DOGE', 'USDT', 0.00001, 1, 1, 1, 0.12345],
  ['ADAUSDT', 'ADA', 'USDT', 0.0001, 0.1, 0.1, 5, 0.4012],
  ['ETHBTC', 'ETH', 'BTC', 0.00001, 0.0001, 0.0001, 0.0001, 0.04859],
]
const PERP = [
  ['BTCUSDT', 'BTC', 'USDT', 0.1, 0.001, 0.001, 100, 64230],
  ['ETHUSDT', 'ETH', 'USDT', 0.01, 0.001, 0.001, 20, 3121],
  ['SOLUSDT', 'SOL', 'USDT', 0.01, 0.01, 0.01, 5, 152.5],
  ['1000PEPEUSDT', '1000PEPE', 'USDT', 0.0000001, 1, 1, 5, 0.0091234],
  ['XRPUSDT', 'XRP', 'USDT', 0.0001, 0.1, 0.1, 5, 0.5322],
]

function instruments(venue, kind, rows, fee) {
  return rows.map(([symbol, base, quote, tick_size, lot_size, min_qty, min_notional, ref]) => ({
    instrument: { id: { venue, symbol, kind }, base, quote, tick_size, lot_size, min_qty, min_notional, fees: fee },
    ref,
  }))
}

const fixtures = {
  end: END,
  instruments: [
    ...instruments('binance', 'spot', SPOT, fees(0.001, 0.001)),
    ...instruments('binance', 'perpetual', PERP, fees(0.0002, 0.0005)),
    ...instruments('bybit', 'spot', SPOT.slice(0, 5), fees(0.001, 0.001)),
    ...instruments('bybit', 'perpetual', PERP.slice(0, 3), fees(0.0002, 0.00055)),
  ],
  history: {
    BTCUSDT: history(7, { start: 58_900, last: 64_210, hourVol: 0.0065, tick: 0.01, hourVolume: 820 }),
    ETHUSDT: history(11, { start: 2_760, last: 3_120, hourVol: 0.0085, tick: 0.01, hourVolume: 9_400 }),
  },
}

writeFileSync(new URL('./trading.json', import.meta.url), `${JSON.stringify(fixtures)}\n`)
console.log('wrote trading.json')
