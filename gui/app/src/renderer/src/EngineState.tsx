import { useState } from 'react'
import type { EngineStatus } from '@td/shared'
import { sentence } from './engine'

/** What the Lab shows until the engine runs: starting, or failed with Retry. */
export function EngineState({ status }: { status: EngineStatus }) {
  const [retrying, setRetrying] = useState(false)

  if (status.state !== 'failed') {
    return (
      <div className="column state" role="status">
        <p className="state-title shimmer">Starting the engine</p>
      </div>
    )
  }

  const retry = () => {
    setRetrying(true)
    void window.td.restartEngine().finally(() => setRetrying(false))
  }

  return (
    <div className="column state" role="alert">
      <p className="state-title">The engine failed</p>
      <p className="state-text">{sentence(status.message)}</p>
      <button type="button" className="button-float" onClick={retry} aria-disabled={retrying}>
        Retry
      </button>
    </div>
  )
}
