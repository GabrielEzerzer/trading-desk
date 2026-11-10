// The quit's last backstop, run outside the app. A timer in the main process cannot do this job:
// `app.exit` ends Electron's message loop and tears down Node before Chromium's own shutdown, which
// is where a hang happens, so such a timer never fires. This starts a small detached Node process
// (Electron's own binary with ELECTRON_RUN_AS_NODE) that checks the app's pid every 100 ms, exits
// as soon as the app has gone, and kills it if it is still alive at the deadline. Exiting early
// keeps a reused pid from being killed after a normal quit.
//
// shortcut: Windows could still hand the pid to a new process within one 100 ms check. A packaged
// build must keep Electron's RunAsNode fuse on, or this spawn does nothing; revisit with packaging.

import { type ChildProcess, spawn } from 'node:child_process'

const SCRIPT = `const [pid, ms] = process.argv.slice(1).map(Number)
const deadline = Date.now() + ms
const timer = setInterval(() => {
  try { process.kill(pid, 0) } catch { process.exit(0) }
  if (Date.now() < deadline) return
  clearInterval(timer)
  try { process.kill(pid, 'SIGKILL') } catch {}
}, 100)`

/** Starts a detached process that kills `pid` after `afterMs` unless it exits first. Returns it so tests can clean up. */
export function spawnWatchdog(pid: number, afterMs: number, execPath: string = process.execPath): ChildProcess {
  const child = spawn(execPath, ['-e', SCRIPT, String(pid), String(afterMs)], {
    env: { ...process.env, ELECTRON_RUN_AS_NODE: '1' },
    detached: true,
    stdio: 'ignore',
    windowsHide: true,
  })
  // A watchdog that fails to start must not crash the quit with an unhandled 'error' event.
  child.on('error', () => {})
  child.unref()
  return child
}
