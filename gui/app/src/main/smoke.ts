// The install check's hook (gui/scripts/install-check.mjs), on only when TD_SMOKE=1: the app ends
// itself once the engine's handshake has arrived and the window has loaded, and fails if either
// fails or both have not happened within the time limit. Kept free of Electron so it can be tested.

import type { EngineStatus } from '@td/shared'

export interface SmokeDeps {
  /** Both arrived: quit the normal way, which exits with code 0. */
  pass(): void
  /** Exit with SMOKE_FAIL_CODE. */
  fail(reason: string): void
}

export interface Smoke {
  engine(status: EngineStatus): void
  loaded(): void
  loadFailed(reason: string): void
}

export const SMOKE_TIMEOUT_MS = 30_000
/** The smoke's failure exit code, apart from the 1 the quit watchdog's kill gives on Windows. */
export const SMOKE_FAIL_CODE = 2

export function createSmoke(deps: SmokeDeps, timeoutMs = SMOKE_TIMEOUT_MS): Smoke {
  let engineUp = false
  let windowUp = false
  let done = false
  const finish = (reason: string | null) => {
    if (done) return
    done = true
    clearTimeout(timer)
    if (reason === null) deps.pass()
    else deps.fail(reason)
  }
  const timer = setTimeout(() => {
    const missing = [engineUp ? null : 'the engine handshake', windowUp ? null : 'the window load'].filter(Boolean)
    finish(`waited ${timeoutMs / 1000} s for ${missing.join(' and ')}`)
  }, timeoutMs)
  const check = () => {
    if (engineUp && windowUp) finish(null)
  }
  return {
    engine(status) {
      if (status.state === 'running') engineUp = true
      else if (status.state === 'failed') finish(`the engine failed: ${status.message}`)
      check()
    },
    loaded() {
      windowUp = true
      check()
    },
    loadFailed(reason) {
      finish(`the window did not load: ${reason}`)
    },
  }
}
