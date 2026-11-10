// The app's quit sequence, kept free of Electron so it can be tested with the pieces injected.
//
// Closing the window or any other quit raises Electron's `before-quit`. The first one starts the
// sequence: stop anything that could reconnect or start new work, ask the engine to stop, and
// then end the process with `app.exit(0)`. Every `before-quit` is prevented, so the process only
// ends through that exit: Electron's own quit never continues past a half-finished stop, and the
// sequence never relies on a second `app.quit()` being honoured. Two backstops make it bounded:
// the engine gets `stopTimeoutMs` before it is killed and the app exits anyway, and just before
// `exit` a watchdog process is armed that kills the app if it is still alive `terminateAfterMs`
// later (watchdog.ts). The watchdog lives outside the app because `exit` tears down Node before
// Chromium's shutdown, where a hang would happen, so no timer in this process could fire then.

export interface QuitDeps {
  /** Asks the engine to stop; resolves with how it went, for the log. */
  stopEngine(): Promise<string>
  /** Kills the engine at once. */
  killEngine(): void
  /** Closes everything that could keep working or reconnect: the engine socket, open requests. */
  closeConnections(): void
  /** Ends the process without another `before-quit` (Electron's `app.exit`). */
  exit(code: number): void
  /** Arms a watchdog outside the process that kills it `afterMs` from now, for when `exit` hangs. */
  armWatchdog(afterMs: number): void
  log(line: string): void
}

export interface QuitTimings {
  stopTimeoutMs: number
  terminateAfterMs: number
}

// The sidecar's own stop grace is 11 s (sidecar.ts), after which it kills the engine; this leaves
// two more seconds for that kill and the drain of its output.
export const QUIT_TIMINGS: QuitTimings = { stopTimeoutMs: 13_000, terminateAfterMs: 3_000 }

export interface Quit {
  /** True from the first `before-quit` on. Nothing new may start once it is set. */
  readonly quitting: boolean
  /** The `before-quit` handler. */
  onBeforeQuit(event: { preventDefault(): void }): void
}

export function createQuit(deps: QuitDeps, timings: QuitTimings = QUIT_TIMINGS): Quit {
  let state: 'running' | 'stopping' | 'exiting' = 'running'
  let stopTimer: ReturnType<typeof setTimeout> | undefined

  function finish(): void {
    if (state === 'exiting') return
    state = 'exiting'
    clearTimeout(stopTimer)
    try {
      deps.armWatchdog(timings.terminateAfterMs)
    } catch (e) {
      deps.log(`arming the quit watchdog failed: ${e instanceof Error ? e.message : String(e)}`)
    }
    deps.exit(0)
  }

  return {
    get quitting() {
      return state !== 'running'
    },
    onBeforeQuit(event) {
      event.preventDefault()
      if (state !== 'running') return
      state = 'stopping'
      try {
        deps.closeConnections()
      } catch (e) {
        deps.log(`closing connections failed: ${e instanceof Error ? e.message : String(e)}`)
      }
      stopTimer = setTimeout(() => {
        deps.log(`the engine did not stop within ${timings.stopTimeoutMs} ms; killing it and quitting`)
        deps.killEngine()
        finish()
      }, timings.stopTimeoutMs)
      let stopping: Promise<string>
      try {
        stopping = deps.stopEngine()
      } catch (e) {
        stopping = Promise.reject(e)
      }
      void stopping
        .then(
          (result) => deps.log(`stopped (${result})`),
          (e: unknown) => {
            deps.log(`stopping the engine failed: ${e instanceof Error ? e.message : String(e)}`)
            deps.killEngine()
          },
        )
        .finally(finish)
    },
  }
}
