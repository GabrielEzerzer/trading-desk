// The live chart, drawn with TradingView Lightweight Charts.
//
// The library's licence (Apache-2.0, README "License") asks for the attribution notice from its
// NOTICE file and a link to https://www.tradingview.com/ on a page users see. The chart shows both
// in its top right corner. The library's own `attributionLogo` injects a <style> element, which the
// app's content security policy (style-src 'self') blocks, so the logo is turned off and the link
// is a plain "Charting by TradingView". Main opens exactly that link in the browser (links.ts).

import type { Candle, Fill, Instrument, Order, Timeframe } from '@td/shared'
import {
  CandlestickSeries,
  ColorType,
  CrosshairMode,
  type IChartApi,
  type IPriceLine,
  type ISeriesApi,
  type ISeriesMarkersPluginApi,
  LineStyle,
  type SeriesMarker,
  type Time,
  type UTCTimestamp,
  createChart,
  createSeriesMarkers,
} from 'lightweight-charts'
import { useEffect, useRef, useState } from 'react'
import { bucketOf, onlyLastChanged } from './candles'
import { stepDecimals } from './order'

const TRADINGVIEW_URL = 'https://www.tradingview.com/'

interface Props {
  candles: readonly Candle[]
  /** Changes when the chart shows another instrument or timeframe: it then redraws from scratch. */
  seriesKey: string
  /** Changes when the list changed other than at its end (history arrived, a backfill): redraw. */
  revision: number
  timeframe: Timeframe
  instrument: Instrument | null
  fills: readonly Fill[]
  /** The open position's average entry, or null when flat. */
  entry: number | null
  openOrders: readonly Order[]
}

const seconds = (ms: number) => (ms / 1000) as UTCTimestamp

const toBar = (c: Candle) => ({ time: seconds(c.open_time), open: c.open, high: c.high, low: c.low, close: c.close })

const inRange = (t: number, first: number, last: number) => t >= first && t <= last

function token(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim()
}

/** Fills the sheet edge to edge. Candles take the gain and loss colours; everything else is neutral. */
export function Chart({ candles, seriesKey, revision, timeframe, instrument, fills, entry, openOrders }: Props) {
  const host = useRef<HTMLDivElement>(null)
  const chart = useRef<IChartApi | null>(null)
  const series = useRef<ISeriesApi<'Candlestick'> | null>(null)
  const markers = useRef<ISeriesMarkersPluginApi<Time> | null>(null)
  const drawn = useRef({ key: '', revision: -1, candles: [] as readonly Candle[] })
  const [scaleWidth, setScaleWidth] = useState(0)

  // One chart per mount; the section unmounts when hidden, which disposes it.
  useEffect(() => {
    const el = host.current
    if (!el) return
    const ink = token('--ink')
    const grey = token('--grey-1')
    const label = token('--grey-2')
    const hairline = token('--hairline')
    const gain = token('--gain')
    const loss = token('--loss')
    const api = createChart(el, {
      autoSize: true,
      layout: {
        background: { type: ColorType.Solid, color: token('--sheet') },
        textColor: grey,
        fontFamily: token('--font-sans'),
        fontSize: 12,
        attributionLogo: false,
      },
      grid: { vertLines: { visible: false }, horzLines: { color: hairline } },
      rightPriceScale: { borderVisible: false, scaleMargins: { top: 0.16, bottom: 0.26 } },
      timeScale: { borderVisible: false, rightOffset: 6, timeVisible: true, secondsVisible: false },
      crosshair: {
        mode: CrosshairMode.Normal,
        vertLine: { color: label, style: LineStyle.Dashed, labelBackgroundColor: ink },
        horzLine: { color: label, style: LineStyle.Dashed, labelBackgroundColor: ink },
      },
      handleScale: { axisPressedMouseMove: { time: true, price: false } },
    })
    const candlesSeries = api.addSeries(CandlestickSeries, {
      upColor: gain,
      downColor: loss,
      wickUpColor: gain,
      wickDownColor: loss,
      borderVisible: false,
    })
    chart.current = api
    series.current = candlesSeries
    markers.current = createSeriesMarkers(candlesSeries, [])
    // The canvas measured its labels before Inter loaded; measure again once it has.
    void document.fonts.ready.then(() => api.applyOptions({ layout: { fontFamily: token('--font-sans') } }))
    // The time scale narrows whenever the price scale widens, so its size change is when the
    // attribution link moves to stay clear of the price labels.
    const measure = () => setScaleWidth(api.priceScale('right').width())
    api.timeScale().subscribeSizeChange(measure)
    return () => {
      api.timeScale().unsubscribeSizeChange(measure)
      api.remove()
      chart.current = null
      series.current = null
      markers.current = null
      drawn.current = { key: '', revision: -1, candles: [] }
    }
  }, [])

  useEffect(() => {
    if (!instrument) return
    const tick = instrument.tick_size
    series.current?.applyOptions({ priceFormat: { type: 'price', precision: stepDecimals(tick), minMove: tick } })
  }, [instrument])

  // A new or changed last bar is drawn in place; another instrument, timeframe, a backfill (a new
  // `revision`) or a change before the last bar redraws everything.
  useEffect(() => {
    const s = series.current
    if (!s) return
    const prev = drawn.current
    const last = candles.at(-1)
    if (prev.key === seriesKey && prev.revision === revision && last && onlyLastChanged(prev.candles, candles)) {
      s.update(toBar(last))
    } else {
      s.setData(candles.map(toBar))
      if (prev.key !== seriesKey) chart.current?.timeScale().scrollToRealTime()
    }
    drawn.current = { key: seriesKey, revision, candles }
  }, [candles, seriesKey, revision])

  // The markers plugin moves a marker outside the drawn bars onto the first or last bar, which
  // would show a fill at the wrong time, so only fills inside the loaded bars get one.
  const first = candles[0]?.open_time
  const last = candles.at(-1)?.open_time
  useEffect(() => {
    const shown =
      first === undefined || last === undefined ? [] : fills.filter((f) => inRange(bucketOf(f.time, timeframe), first, last))
    const list: SeriesMarker<Time>[] = shown.map((f) => ({
      time: seconds(bucketOf(f.time, timeframe)),
      position: f.side === 'buy' ? 'belowBar' : 'aboveBar',
      shape: f.side === 'buy' ? 'arrowUp' : 'arrowDown',
      color: token(f.side === 'buy' ? '--gain' : '--loss'),
      size: 0.8,
    }))
    list.sort((a, b) => (a.time as number) - (b.time as number))
    markers.current?.setMarkers(list)
  }, [fills, timeframe, seriesKey, first, last])

  // The position's entry and each resting limit order are quiet lines across the chart.
  useEffect(() => {
    const s = series.current
    if (!s) return
    const lines: IPriceLine[] = []
    if (entry !== null) {
      lines.push(
        s.createPriceLine({ price: entry, color: token('--grey-1'), lineStyle: LineStyle.Dotted, lineWidth: 1, title: 'Entry' }),
      )
    }
    for (const order of openOrders) {
      if (order.request.order_type.type !== 'limit') continue
      lines.push(
        s.createPriceLine({
          price: order.request.order_type.price,
          color: token('--grey-2'),
          lineStyle: LineStyle.Dashed,
          lineWidth: 1,
          title: order.request.side === 'buy' ? 'Buy limit' : 'Sell limit',
        }),
      )
    }
    return () => {
      for (const line of lines) s.removePriceLine(line)
    }
  }, [entry, openOrders, seriesKey])

  return (
    <div className="chart-host">
      <div className="chart-canvas" ref={host} />
      <p className="chart-attribution" style={{ right: `calc(${scaleWidth}px + var(--space-3))` }}>
        <a href={TRADINGVIEW_URL} target="_blank" rel="noreferrer">
          Charting by TradingView
        </a>
        <span>TradingView Lightweight Charts™ · Copyright © 2025 TradingView, Inc.</span>
      </p>
    </div>
  )
}
