// The quit's last backstop, run outside the app. A timer in the main process cannot do this job:
// `app.exit` ends Electron's message loop and tears down Node before Chromium's own shutdown, which
// is where a hang happens, so such a timer never fires. This starts a small detached Node process
// (Electron's own binary with ELECTRON_RUN_AS_NODE) that waits and then kills the app by pid. If
// the app has already gone, the kill fails quietly.
//
// shortcut: the pid could in theory be reused within the wait; 3 s makes that unlikely. A packaged
// build must keep Electron's RunAsNode fuse on, or this spawn does nothing; revisit with packaging.

import { type ChildProcess, spawn } from 'node:child_process'

const SCRIPT = `const [pid, ms] = process.argv.slice(1).map(Number)
setTimeout(() => {
  try { process.kill(pid, 'SIGKILL') } catch {}
}, ms)`

/** Starts a detached process that kills `pid` after `afterMs`. Returns it so tests can clean up. */
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
