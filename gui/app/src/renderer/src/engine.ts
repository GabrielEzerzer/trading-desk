import { useEffect, useState } from 'react'
import type { EngineStatus } from '@td/shared'

/** The engine's state as main reports it, kept current. */
export function useEngineStatus(): EngineStatus {
  const [status, setStatus] = useState<EngineStatus>({ state: 'connecting' })

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

  return status
}

/**
 * The engine's own message as a sentence: a capital and a full stop. A first word that is a name
 * such as `td-server` keeps its case.
 */
export function sentence(message: string): string {
  const first = message.split(/\s/, 1)[0] ?? ''
  const text = /^[a-z]+$/.test(first) ? message.charAt(0).toUpperCase() + message.slice(1) : message
  return /[.!?]$/.test(text) ? text : `${text}.`
}
