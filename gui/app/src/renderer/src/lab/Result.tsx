import { CheckIcon, CrossIcon } from '../icons'
import { useCountUp } from '../motion'
import { EquityChart } from './EquityChart'
import {
  formatDay,
  formatInstrument,
  formatLimit,
  formatMonth,
  formatNumber,
  formatPrice,
  formatReturnPct,
  pnlTone,
} from './format'
import type { LabStrategy, VerdictPreview } from './source'

interface Props {
  strategy: LabStrategy
  verdict: VerdictPreview
  /** True right after a run finished: the numbers count up once. */
  fresh: boolean
  onCounted(): void
}

/**
 * The answer first, then the evidence: verdict and kind, one line per criterion, the key numbers,
 * the equity curve against its benchmark, then the metrics and trade tables.
 */
export function Result({ strategy, verdict, fresh, onCounted }: Props) {
  const progress = useCountUp(fresh, onCounted)
  const trade = verdict.decision === 'trade'
  // Counting numbers are scaled toward their true value; at progress 1 they are exact.
  const count = (value: number) => (progress === 1 ? value : value * progress)

  return (
    <article className="result" aria-labelledby="verdict">
      <p className="eyebrow">
        {strategy.label} · {formatInstrument(verdict.instrument)} · {formatMonth(verdict.period.from)} to{' '}
        {formatMonth(verdict.period.to)}
      </p>
      <div className="verdict-line">
        <h2 id="verdict" className="verdict" data-decision={verdict.decision}>
          {trade ? 'Trade' : 'No trade'}
        </h2>
        {strategy.kind && <span className="verdict-kind">{strategy.kind}</span>}
      </div>

      <ul className="criteria" aria-label="Criteria">
        {verdict.criteria.map((c) => (
          <li key={c.label} className="criterion" data-pass={c.pass}>
            <span className="criterion-label">{c.label}</span>
            <span className="criterion-value">{formatNumber(count(c.value), c.format)}</span>
            <span className="criterion-limit">{formatLimit(c)}</span>
            <span className="criterion-result">
              {c.pass ? <CheckIcon /> : <CrossIcon />}
              {c.pass ? 'Pass' : 'Fail'}
            </span>
          </li>
        ))}
      </ul>

      <dl className="key-numbers">
        {verdict.keyNumbers.map((k) => (
          <div key={k.label} className="key-number">
            <dt>{k.label}</dt>
            <dd data-tone={k.tone === 'pnl' ? pnlTone(k.value, k.format) : undefined}>
              {formatNumber(count(k.value), k.format, { sign: k.tone === 'pnl' })}
            </dd>
          </div>
        ))}
      </dl>

      <section className="evidence" aria-labelledby="equity-title">
        <h3 id="equity-title" className="section-label">
          Equity
        </h3>
        <EquityChart points={verdict.equity} benchmarkLabel={verdict.benchmarkLabel} />
      </section>

      <section className="evidence" aria-labelledby="metrics-title">
        <h3 id="metrics-title" className="section-label">
          Metrics
        </h3>
        <table className="table">
          <thead>
            <tr>
              <th scope="col">Metric</th>
              <th scope="col" className="num">
                Strategy
              </th>
              <th scope="col" className="num">
                {verdict.benchmarkLabel}
              </th>
            </tr>
          </thead>
          <tbody>
            {verdict.metrics.map((m) => (
              <tr key={m.label}>
                <th scope="row">{m.label}</th>
                <td className="num">{formatNumber(m.strategy, m.format)}</td>
                <td className="num muted">{m.benchmark === null ? '–' : formatNumber(m.benchmark, m.format)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>

      <section className="evidence" aria-labelledby="trades-title">
        <h3 id="trades-title" className="section-label">
          Trades <span className="section-count">{verdict.trades.length}</span>
        </h3>
        <table className="table table-trades">
          <thead>
            <tr>
              <th scope="col" className="num">
                #
              </th>
              <th scope="col">Side</th>
              <th scope="col">Opened</th>
              <th scope="col">Closed</th>
              <th scope="col" className="num">
                Entry
              </th>
              <th scope="col" className="num">
                Exit
              </th>
              <th scope="col" className="num">
                Return
              </th>
            </tr>
          </thead>
          <tbody>
            {verdict.trades.map((t) => (
              <tr key={t.id}>
                <td className="num muted">{t.id}</td>
                <td>{t.side === 'long' ? 'Long' : 'Short'}</td>
                <td>{formatDay(t.entryTime)}</td>
                <td>{formatDay(t.exitTime)}</td>
                <td className="num">{formatPrice(t.entryPrice)}</td>
                <td className="num">{formatPrice(t.exitPrice)}</td>
                <td className="num" data-tone={pnlTone(t.returnPct, 'return_pct')}>
                  {formatReturnPct(t.returnPct)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>

      <p className="footnote">Finished {formatDay(verdict.finishedAt)}</p>
    </article>
  )
}
