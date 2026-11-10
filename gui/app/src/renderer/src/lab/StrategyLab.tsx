import { useCallback, useEffect, useRef, useState } from 'react'
import { sentence } from '../engine'
import { Composer, type ComposerHandle } from './Composer'
import { Result } from './Result'
import { type LabSource, type LabStrategy, RunCancelled, chooseLabSource } from './source'

interface ActiveRun {
  name: string
  progress: number
  message: string
  cancel(): void
}

const message = (e: unknown) => sentence(e instanceof Error ? e.message : String(e))

/**
 * The Lab: the strategy list until one is chosen, then that strategy's verdict, its run in
 * progress, its failed run, or the offer to run it. The composer floats at the bottom throughout.
 */
export function StrategyLab() {
  const [source, setSource] = useState<LabSource | null>(null)
  const [strategies, setStrategies] = useState<LabStrategy[] | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [attempt, setAttempt] = useState(0)
  const [selectedName, setSelectedName] = useState<string | null>(null)
  const [run, setRun] = useState<ActiveRun | null>(null)
  const [runError, setRunError] = useState<{ name: string; message: string } | null>(null)
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
        return s.strategies()
      })
      .then(
        (list) => live && setStrategies(list),
        (e: unknown) => live && setLoadError(message(e)),
      )
    return () => {
      live = false
    }
  }, [attempt])

  const selected = strategies?.find((s) => s.name === selectedName) ?? null

  const select = (name: string | null) => {
    setSelectedName(name)
    scroller.current?.scrollTo({ top: 0 })
    // The chosen row unmounts; focus moves to the pill that now shows the choice, not to body.
    requestAnimationFrame(() => composer.current?.focusPicker())
  }

  const start = () => {
    if (!source || !selected || run) return
    const name = selected.name
    setRunError(null)
    const handle = source.run(name, (p) => setRun((r) => (r && r.name === name ? { ...r, ...p } : r)))
    setRun({ name, progress: 0, message: 'Starting the run', cancel: handle.cancel })
    handle.result.then(
      (verdict) => {
        setStrategies((list) => list?.map((s) => (s.name === name ? { ...s, last: verdict } : s)) ?? list)
        setFresh(name)
        setRun(null)
      },
      (e: unknown) => {
        setRun(null)
        if (!(e instanceof RunCancelled)) setRunError({ name, message: message(e) })
      },
    )
  }

  const counted = useCallback(() => setFresh(null), [])

  let runBlocked: string | null = null
  if (!source?.canRun) runBlocked = source?.cannotRunReason ?? 'Running is not available.'
  else if (!selected) runBlocked = 'Choose a strategy first.'

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
  } else if (!strategies) {
    body = (
      <div className="state" role="status">
        <p className="state-quiet">Loading strategies</p>
      </div>
    )
  } else if (!selected) {
    body = <StrategyList strategies={strategies} onChoose={select} />
  } else if (run?.name === selected.name) {
    body = <Running strategy={selected} run={run} />
  } else if (runError?.name === selected.name) {
    body = (
      <div className="state" role="alert">
        <p className="eyebrow">{selected.label}</p>
        <p className="verdict">Run failed</p>
        <p className="state-text">{runError.message}</p>
      </div>
    )
  } else if (selected.last) {
    body = (
      <Result
        key={`${selected.name}-${selected.last.finishedAt}`}
        strategy={selected}
        verdict={selected.last}
        fresh={fresh === selected.name}
        onCounted={counted}
      />
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
        <p className="state-text">
          {source?.canRun ? 'Run its test plan to get a verdict.' : source?.cannotRunReason}
        </p>
      </div>
    )
  }

  const phase = run?.name === selectedName ? 'running' : runError?.name === selectedName ? 'failed' : 'idle'
  const viewKey = `${loadError ? 'error' : strategies ? 'ready' : 'loading'}-${selectedName ?? 'list'}-${phase}`

  return (
    <div className="lab">
      <div className="lab-scroll" ref={scroller}>
        {/* A new key per view, so each new view fades in once. */}
        <div className="column fade-in" key={viewKey}>
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
          onSelect={select}
          onRun={start}
          onCancel={() => run?.cancel()}
        />
      )}
    </div>
  )
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
              <span className="strategy-meta">
                {[s.kind, s.last ? (s.last.decision === 'trade' ? 'Trade' : 'No trade') : 'Not run']
                  .filter(Boolean)
                  .join(' · ')}
              </span>
            </button>
          </li>
        ))}
      </ul>
    </nav>
  )
}

function Running({ strategy, run }: { strategy: LabStrategy; run: ActiveRun }) {
  const percent = Math.round(run.progress * 100)
  return (
    <div className="state">
      <p className="eyebrow">{strategy.label}</p>
      <p className="run-status shimmer" role="status" aria-live="polite">
        {run.message}
      </p>
      <div
        className="progress"
        role="progressbar"
        aria-label="Run progress"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={percent}
      >
        <ProgressFill value={run.progress} />
      </div>
    </div>
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
