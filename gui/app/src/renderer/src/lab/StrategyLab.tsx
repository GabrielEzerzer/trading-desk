import type { InstrumentId, Timeframe } from '@td/shared'
import { useCallback, useEffect, useRef, useState } from 'react'
import { sentence } from '../engine'
import { Composer, type ComposerHandle } from './Composer'
import { formatCutoff, formatDay, formatInstrument, formatNumber } from './format'
import { overallProgress, progressLabel } from './progress'
import { Result } from './Result'
import { type RunFailure, TIMEFRAMES, instrumentKey, laterRun, runFailure } from './runs'
import {
  type LabSource,
  type LabStrategy,
  type LastRun,
  RunCancelled,
  type RunProgress,
  type VerdictPreview,
  chooseLabSource,
} from './source'

interface ActiveRun {
  name: string
  /** Null until the run reports its first step. */
  progress: RunProgress | null
  cancel(): void
}

/** A download of an instrument's history, from the "No data downloaded" or "Data missing" state. */
interface ActiveDownload {
  instrument: InstrumentId
  fraction: number
  /** Cancel was pressed; the view stays until the engine ends the job. */
  cancelling: boolean
  cancel(): void
}

/** Why the selected strategy's last run attempt did not finish, and what it ran on. */
interface Failure {
  name: string
  failure: Exclude<RunFailure, { kind: 'cancelled' }>
  instrument: InstrumentId
}

const message = (e: unknown) => sentence(e instanceof Error ? e.message : String(e))

/**
 * The Lab: the strategy list until one is chosen, then that strategy's result, its run in
 * progress, its failed run, the offer to download data, or the offer to run it. The composer
 * floats at the bottom throughout.
 */
export function StrategyLab() {
  const [source, setSource] = useState<LabSource | null>(null)
  const [strategies, setStrategies] = useState<LabStrategy[] | null>(null)
  const [datasets, setDatasets] = useState<InstrumentId[] | null>(null)
  const [dataCutoff, setDataCutoff] = useState<string | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [attempt, setAttempt] = useState(0)
  const [selectedName, setSelectedName] = useState<string | null>(null)
  const [chosenData, setChosenData] = useState<string | null>(null)
  const [timeframe, setTimeframe] = useState<Timeframe>(TIMEFRAMES[0])
  const [run, setRun] = useState<ActiveRun | null>(null)
  const [failure, setFailure] = useState<Failure | null>(null)
  const [download, setDownload] = useState<ActiveDownload | null>(null)
  const [downloadError, setDownloadError] = useState<string | null>(null)
  // Full results by run id, read once per run.
  const [results, setResults] = useState<Record<string, VerdictPreview>>({})
  const [openError, setOpenError] = useState<{ id: string; message: string } | null>(null)
  // The strategy whose result just arrived from a run; its numbers count up once, then it clears.
  const [fresh, setFresh] = useState<string | null>(null)
  const composer = useRef<ComposerHandle>(null)
  const scroller = useRef<HTMLDivElement>(null)

  useEffect(() => {
    let live = true
    setLoadError(null)
    chooseLabSource()
      .then((s) => {
        if (live) setSource(s)
        return Promise.all([s.strategies(), s.datasets(), s.dataCutoff()])
      })
      .then(
        ([list, stored, cutoff]) => {
          if (!live) return
          setStrategies(list)
          setDatasets(stored)
          setDataCutoff(cutoff)
        },
        (e: unknown) => live && setLoadError(message(e)),
      )
    return () => {
      live = false
    }
  }, [attempt])

  const selected = strategies?.find((s) => s.name === selectedName) ?? null
  const instrument = datasets?.find((d) => instrumentKey(d) === chosenData) ?? datasets?.[0] ?? null
  const last = selected?.last ?? null
  const shown = last ? results[last.id] : undefined

  // A past result is read when its strategy is opened, and shown settled.
  useEffect(() => {
    if (!source || !last || results[last.id]) return
    let live = true
    setOpenError(null)
    source.open(last).then(
      (verdict) => live && setResults((r) => ({ ...r, [last.id]: verdict })),
      (e: unknown) => live && setOpenError({ id: last.id, message: message(e) }),
    )
    return () => {
      live = false
    }
  }, [source, last, results])

  const select = (name: string | null) => {
    setSelectedName(name)
    scroller.current?.scrollTo({ top: 0 })
    // The chosen row unmounts; focus moves to the pill that now shows the choice, not to body.
    requestAnimationFrame(() => composer.current?.focusPicker())
  }

  const start = () => {
    if (!source || !selected || !instrument || run) return
    const name = selected.name
    const target = { instrument, timeframe }
    setFailure(null)
    // Cancel leaves the running screen at once. The source settles the result only when the job
    // has ended, and a cancel that came too late still saves the run, so its result is kept.
    let cancelled = false
    const cancel = () => {
      cancelled = true
      handle.cancel()
      setRun((r) => (r?.cancel === cancel ? null : r))
    }
    const handle = source.run(name, target, (progress) =>
      setRun((r) => (r?.cancel === cancel ? { ...r, progress } : r)),
    )
    setRun({ name, progress: null, cancel })
    handle.result.then(
      (finished) => {
        setResults((r) => ({ ...r, [finished.last.id]: finished.verdict }))
        setStrategies(
          (list) => list?.map((s) => (s.name === name ? { ...s, last: laterRun(s.last, finished.last) } : s)) ?? list,
        )
        if (!cancelled) setFresh(name)
        setRun((r) => (r?.cancel === cancel ? null : r))
      },
      (e: unknown) => {
        setRun((r) => (r?.cancel === cancel ? null : r))
        const f = runFailure(e)
        if (!cancelled && f.kind !== 'cancelled') setFailure({ name, failure: f, instrument: target.instrument })
      },
    )
  }

  const startDownload = (target: InstrumentId) => {
    if (!source || download) return
    setDownloadError(null)
    const handle = source.download(target, ({ fraction }) =>
      setDownload((d) => (d && d.instrument === target ? { ...d, fraction } : d)),
    )
    // The view stays until the job ends: the engine refuses a second download while one runs.
    const cancel = () => {
      handle.cancel()
      setDownload((d) => (d && d.instrument === target ? { ...d, cancelling: true } : d))
    }
    setDownload({ instrument: target, fraction: 0, cancelling: false, cancel })
    handle.result
      .then(async () => {
        const stored = await source.datasets()
        setDatasets(stored)
        setChosenData(instrumentKey(target))
        setFailure((f) => (f?.failure.kind === 'data_missing' ? null : f))
      })
      .catch((e: unknown) => {
        if (!(e instanceof RunCancelled)) setDownloadError(message(e))
      })
      .finally(() => setDownload(null))
  }

  const counted = useCallback(() => setFresh(null), [])

  let runBlocked: string | null = null
  if (!selected) runBlocked = 'Choose a strategy first.'
  else if (download) runBlocked = 'Wait for the download to finish.'
  else if (!instrument) runBlocked = 'Download data first.'

  const failed = failure?.name === selectedName ? failure : null
  const noData = datasets?.length === 0 || failed?.failure.kind === 'data_missing'

  let body: React.ReactNode
  if (loadError) {
    body = (
      <div className="state" role="alert">
        <p className="state-title">The strategies did not load</p>
        <p className="state-text">{loadError}</p>
        <button type="button" className="button-float" onClick={() => setAttempt((n) => n + 1)}>
          Retry
        </button>
      </div>
    )
  } else if (!strategies || !source) {
    body = (
      <div className="state" role="status">
        <p className="state-quiet">Loading strategies</p>
      </div>
    )
  } else if (!selected) {
    body = <StrategyList strategies={strategies} onChoose={select} />
  } else if (run?.name === selected.name) {
    body = <Running strategy={selected} run={run} />
  } else if (noData) {
    // With nothing stored this shows even over a saved result, so a download is always on offer.
    const target = failed?.instrument ?? source.defaultInstrument
    body = (
      <NoData
        strategy={selected}
        instrument={download?.instrument ?? target}
        title={failed?.failure.kind === 'data_missing' ? 'Data missing' : 'No data downloaded'}
        reason={
          failed?.failure.kind === 'data_missing'
            ? sentence(failed.failure.message)
            : `Nothing is stored yet. Download the history of ${formatInstrument(target)} to run this strategy.`
        }
        download={download}
        error={downloadError}
        onDownload={() => startDownload(target)}
      />
    )
  } else if (failed?.failure.kind === 'too_many_runs') {
    body = (
      <div className="state">
        <p className="eyebrow">{selected.label}</p>
        <p className="state-text" role="status">
          {sentence(failed.failure.message)}
        </p>
      </div>
    )
  } else if (failed?.failure.kind === 'failed') {
    body = (
      <div className="state" role="alert">
        <p className="eyebrow">{selected.label}</p>
        <p className="verdict">Run failed</p>
        <p className="state-text">{sentence(failed.failure.message)}</p>
      </div>
    )
  } else if (last && shown) {
    body = (
      <Result
        key={`${selected.name}-${shown.finishedAt}`}
        strategy={selected}
        verdict={shown}
        fresh={fresh === selected.name}
        onCounted={counted}
      />
    )
  } else if (last && openError?.id === last.id) {
    body = (
      <div className="state" role="alert">
        <p className="eyebrow">{selected.label}</p>
        <p className="state-title">The result did not load</p>
        <p className="state-text">{openError.message}</p>
      </div>
    )
  } else if (last) {
    body = (
      <div className="state" role="status">
        <p className="eyebrow">{selected.label}</p>
        <p className="state-quiet">Loading the result</p>
      </div>
    )
  } else {
    body = (
      <div className="state">
        <p className="eyebrow">{selected.label}</p>
        <div className="verdict-line">
          <p className="verdict" data-decision="none">
            Not run
          </p>
          {selected.kind && <span className="verdict-kind">{selected.kind}</span>}
        </div>
        <p className="state-text">{selected.description}</p>
        <p className="state-text">{source.neverRunText}</p>
      </div>
    )
  }

  const phase =
    run?.name === selectedName ? 'running' : failed ? 'failed' : download ? 'downloading' : noData ? 'no-data' : 'idle'
  const viewKey = `${loadError ? 'error' : strategies ? 'ready' : 'loading'}-${selectedName ?? 'list'}-${phase}`

  return (
    <div className="lab">
      {/* Focusable, so the keyboard can scroll a result that has no controls in it. */}
      <div className="lab-scroll" ref={scroller} tabIndex={0}>
        {/* A new key per view, so each new view fades in once. */}
        <div className="column fade-in" key={viewKey}>
          {dataCutoff && (
            <p className="cutoff-note">Data cutoff in force: runs use no bars after {formatCutoff(dataCutoff)}.</p>
          )}
          {body}
        </div>
      </div>
      {strategies && (
        <Composer
          ref={composer}
          strategies={strategies}
          selected={selected}
          running={run !== null}
          runBlocked={runBlocked}
          datasets={datasets}
          instrument={instrument}
          timeframe={timeframe}
          onInstrument={setChosenData}
          onTimeframe={setTimeframe}
          onSelect={select}
          onRun={start}
          onCancel={() => run?.cancel()}
        />
      )}
    </div>
  )
}

/** "Trade", or the date and total return of a run with no verdict rule yet, or "Not run". */
function lastRunText(last: LastRun | null): string {
  if (!last) return 'Not run'
  if (last.decision) return last.decision === 'trade' ? 'Trade' : 'No trade'
  const total = last.totalReturn === null ? '–' : formatNumber(last.totalReturn, 'percent', { sign: true })
  return `${formatDay(last.finishedAt)} · ${total}`
}

function StrategyList({ strategies, onChoose }: { strategies: LabStrategy[]; onChoose(name: string): void }) {
  if (strategies.length === 0) {
    return <p className="state-quiet">The engine lists no strategies.</p>
  }
  return (
    <nav aria-labelledby="strategies-title">
      <h1 id="strategies-title" className="eyebrow">
        Strategies
      </h1>
      <ul className="strategy-list">
        {strategies.map((s) => (
          <li key={s.name}>
            <button type="button" className="strategy-row" onClick={() => onChoose(s.name)}>
              <span className="strategy-name">{s.label}</span>
              <span className="strategy-meta">{[s.kind, lastRunText(s.last)].filter(Boolean).join(' · ')}</span>
            </button>
          </li>
        ))}
      </ul>
    </nav>
  )
}

interface NoDataProps {
  strategy: LabStrategy
  instrument: InstrumentId
  title: string
  reason: string
  download: ActiveDownload | null
  error: string | null
  onDownload(): void
}

/** The store lacks the run's data: the offer to download the instrument's history, then its progress. */
function NoData({ strategy, instrument, title, reason, download, error, onDownload }: NoDataProps) {
  return (
    <div className="state">
      <p className="eyebrow">{strategy.label}</p>
      <div className="verdict-line">
        <p className="verdict" data-decision="none">
          {title}
        </p>
      </div>
      {download ? (
        <>
          <ProgressBar
            label={
              download.cancelling
                ? 'Cancelling the download'
                : `Downloading the history of ${formatInstrument(download.instrument)}`
            }
            name="Download progress"
            value={download.fraction}
            valueText={(percent) => `${percent}% downloaded`}
          />
          <button
            type="button"
            className="button-float"
            onClick={download.cancelling ? undefined : download.cancel}
            aria-disabled={download.cancelling}
          >
            Cancel the download
          </button>
        </>
      ) : (
        <>
          <p className="state-text">{reason}</p>
          {error && (
            <p className="state-text" role="alert">
              The download failed. {error}
            </p>
          )}
          <button type="button" className="button-float" onClick={onDownload}>
            Download {instrument.symbol} history
          </button>
        </>
      )}
    </div>
  )
}

function Running({ strategy, run }: { strategy: LabStrategy; run: ActiveRun }) {
  // The bar shows how far the whole run has got; the label names the step it is on.
  return (
    <div className="state">
      <p className="eyebrow">{strategy.label}</p>
      <ProgressBar
        label={run.progress ? progressLabel(run.progress) : 'Starting the run'}
        name="Run progress"
        value={run.progress ? overallProgress(run.progress) : 0}
        valueText={(percent) => `${percent}% done`}
      />
    </div>
  )
}

interface ProgressBarProps {
  label: string
  name: string
  value: number
  valueText(percent: number): string
}

/** Work in progress: its status line with the running shimmer, and a bar in the accent. */
function ProgressBar({ label, name, value, valueText }: ProgressBarProps) {
  const percent = Math.round(value * 100)
  return (
    <>
      <p className="run-status shimmer" role="status" aria-live="polite">
        {label}
      </p>
      <div
        className="progress"
        role="progressbar"
        aria-label={name}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={percent}
        aria-valuetext={valueText(percent)}
      >
        <ProgressFill value={value} />
      </div>
    </>
  )
}

/** The fill is scaled, not resized, so its movement never triggers layout. */
function ProgressFill({ value }: { value: number }) {
  const ref = useRef<HTMLSpanElement>(null)
  useEffect(() => {
    ref.current?.style.setProperty('--progress', String(Math.min(1, Math.max(0, value))))
  }, [value])
  return <span className="progress-fill" ref={ref} />
}
