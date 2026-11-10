import { useEffect, useState } from 'react'
import type { EngineStatus } from '@td/shared'
import { api } from './api'

/** Whether the engine is starting, running (with its version, read through the bridge) or failed. */
export function EngineStatusIndicator() {
  const [status, setStatus] = useState<EngineStatus>({ state: 'connecting' })
  const [version, setVersion] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let changed = false
    const off = window.td.onStatus((next) => {
      changed = true
      setStatus(next)
    })
    void window.td.status().then((current) => {
      if (!changed) setStatus(current)
    })
    return off
  }, [])

  useEffect(() => {
    if (status.state !== 'running') return
    let live = true
    api.health().then(
      (health) => live && setVersion(health.version),
      (e: unknown) => live && setError(e instanceof Error ? e.message : String(e)),
    )
    return () => {
      live = false
    }
  }, [status.state])

  let text: string
  if (status.state === 'failed') text = `Engine failed: ${status.message}`
  else if (status.state === 'connecting') text = 'Engine starting'
  else if (error) text = `Engine running, but not answering: ${error}`
  else text = version ? `Engine ${version}` : 'Engine running'

  return (
    <p className="engine-status" role="status" data-state={status.state}>
      {text}
    </p>
  )
}
