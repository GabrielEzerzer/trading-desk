import { formatMonth, formatNumber } from './format'
import type { EquityPoint } from './source'

const W = 1000
const H = 220
const PAD = 6

/** The SVG path for one series on a log scale, so equal percentage moves look equal. */
function linePath(
  points: EquityPoint[],
  pick: (p: EquityPoint) => number | null,
  lo: number,
  hi: number,
): string {
  const first = points[0]
  const last = points.at(-1)
  if (!first || !last) return ''
  const span = last.t - first.t || 1
  const logLo = Math.log(lo)
  const logSpan = Math.log(hi) - logLo || 1
  let path = ''
  for (const p of points) {
    const value = pick(p)
    // A point without a value, or at zero where a log scale has no place, is left out; the line
    // joins its neighbours.
    if (value === null || value <= 0) continue
    const x = ((p.t - first.t) / span) * W
    const y = PAD + (1 - (Math.log(value) - logLo) / logSpan) * (H - 2 * PAD)
    path += `${path ? 'L' : 'M'}${x.toFixed(1)} ${y.toFixed(1)}`
  }
  return path
}

/**
 * The strategy's equity against its benchmark, both as growth of one unit, on a log scale. Inline
 * SVG stretched to the column; the strokes keep their width as it stretches. A run without a
 * benchmark curve draws the strategy alone.
 */
export function EquityChart({ points, benchmarkLabel }: { points: EquityPoint[]; benchmarkLabel: string }) {
  const first = points[0]
  const last = points.at(-1)
  if (!first || !last) return null
  // A value at or below zero has no place on a log scale.
  const values = points.flatMap((p) => [p.strategy, p.benchmark ?? 1]).filter((v) => v > 0)
  const lo = Math.min(1, ...values)
  const hi = Math.max(1, ...values)
  const baseline = PAD + (1 - (0 - Math.log(lo)) / (Math.log(hi) - Math.log(lo) || 1)) * (H - 2 * PAD)
  const strategyReturn = last.strategy - 1
  const lastBenchmark = points.findLast((p) => p.benchmark !== null)?.benchmark ?? null
  const benchmarkReturn = lastBenchmark === null ? null : lastBenchmark - 1
  const pct = (x: number) => formatNumber(x, 'percent', { sign: true })
  const benchmarkText = benchmarkReturn === null ? '' : `, ${benchmarkLabel.toLowerCase()} ${pct(benchmarkReturn)}`

  return (
    <figure className="chart">
      <figcaption className="chart-legend">
        <span className="legend-item">
          <span className="legend-swatch" data-series="strategy" aria-hidden="true" />
          Strategy <span className="legend-value">{pct(strategyReturn)}</span>
        </span>
        {benchmarkReturn !== null && (
          <span className="legend-item">
            <span className="legend-swatch" data-series="benchmark" aria-hidden="true" />
            {benchmarkLabel} <span className="legend-value">{pct(benchmarkReturn)}</span>
          </span>
        )}
      </figcaption>
      <svg
        className="chart-plot"
        viewBox={`0 0 ${W} ${H}`}
        preserveAspectRatio="none"
        role="img"
        aria-label={`Equity from ${formatMonth(first.t)} to ${formatMonth(last.t)}, log scale: strategy ${pct(strategyReturn)}${benchmarkText}`}
      >
        <line className="chart-baseline" x1="0" x2={W} y1={baseline} y2={baseline} />
        {benchmarkReturn !== null && (
          <path className="chart-line" data-series="benchmark" d={linePath(points, (p) => p.benchmark, lo, hi)} />
        )}
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
