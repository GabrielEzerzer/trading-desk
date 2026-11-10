import { formatMonth, formatNumber } from './format'
import type { EquityPoint } from './source'

const W = 1000
const H = 220
const PAD = 6

/** The SVG path for one series on a log scale, so equal percentage moves look equal. */
export function linePath(points: EquityPoint[], pick: (p: EquityPoint) => number, lo: number, hi: number): string {
  const first = points[0]
  const last = points.at(-1)
  if (!first || !last) return ''
  const span = last.t - first.t || 1
  const logLo = Math.log(lo)
  const logSpan = Math.log(hi) - logLo || 1
  return points
    .map((p, i) => {
      const x = ((p.t - first.t) / span) * W
      const y = PAD + (1 - (Math.log(pick(p)) - logLo) / logSpan) * (H - 2 * PAD)
      return `${i === 0 ? 'M' : 'L'}${x.toFixed(1)} ${y.toFixed(1)}`
    })
    .join('')
}

/**
 * The strategy's equity against its benchmark, both as growth of one unit, on a log scale. Inline
 * SVG stretched to the column; the strokes keep their width as it stretches.
 */
export function EquityChart({ points, benchmarkLabel }: { points: EquityPoint[]; benchmarkLabel: string }) {
  const first = points[0]
  const last = points.at(-1)
  if (!first || !last) return null
  const values = points.flatMap((p) => [p.strategy, p.benchmark])
  const lo = Math.min(1, ...values)
  const hi = Math.max(1, ...values)
  const baseline = PAD + (1 - (0 - Math.log(lo)) / (Math.log(hi) - Math.log(lo) || 1)) * (H - 2 * PAD)
  const strategyReturn = last.strategy - 1
  const benchmarkReturn = last.benchmark - 1

  return (
    <figure className="chart">
      <figcaption className="chart-legend">
        <span className="legend-item">
          <span className="legend-swatch" data-series="strategy" aria-hidden="true" />
          Strategy <span className="legend-value">{formatNumber(strategyReturn, 'percent', { sign: true })}</span>
        </span>
        <span className="legend-item">
          <span className="legend-swatch" data-series="benchmark" aria-hidden="true" />
          {benchmarkLabel} <span className="legend-value">{formatNumber(benchmarkReturn, 'percent', { sign: true })}</span>
        </span>
      </figcaption>
      <svg
        className="chart-plot"
        viewBox={`0 0 ${W} ${H}`}
        preserveAspectRatio="none"
        role="img"
        aria-label={`Equity from ${formatMonth(first.t)} to ${formatMonth(last.t)}, log scale: strategy ${formatNumber(strategyReturn, 'percent', { sign: true })}, ${benchmarkLabel.toLowerCase()} ${formatNumber(benchmarkReturn, 'percent', { sign: true })}`}
      >
        <line className="chart-baseline" x1="0" x2={W} y1={baseline} y2={baseline} />
        <path className="chart-line" data-series="benchmark" d={linePath(points, (p) => p.benchmark, lo, hi)} />
        <path className="chart-line" data-series="strategy" d={linePath(points, (p) => p.strategy, lo, hi)} />
      </svg>
      <div className="chart-axis" aria-hidden="true">
        <span>{formatMonth(first.t)}</span>
        <span>Log scale</span>
        <span>{formatMonth(last.t)}</span>
      </div>
    </figure>
  )
}
