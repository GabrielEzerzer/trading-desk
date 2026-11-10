import { type ChildProcess, spawn } from 'node:child_process'
import { once } from 'node:events'
import { describe, expect, it } from 'vitest'
import { spawnWatchdog } from './watchdog'

function isAlive(pid: number | undefined): boolean {
  if (pid === undefined) return false
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

// Fails before vitest's own timeout, so the finally block still cleans up the children.
function within<T>(promise: Promise<T>, ms: number, what: string): Promise<T> {
  let timer: NodeJS.Timeout | undefined
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${what} within ${ms} ms`)), ms)
  })
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer))
}

async function ensureGone(child: ChildProcess): Promise<void> {
  if (child.exitCode === null && child.signalCode === null) {
    const exited = once(child, 'exit')
    child.kill('SIGKILL')
    await exited
  }
  expect(isAlive(child.pid)).toBe(false)
}

describe('spawnWatchdog', () => {
  it('kills the process it watches after the delay', async () => {
    // Plain node stands in for both the app and Electron's binary; nothing here launches Electron.
    const target = spawn(process.execPath, ['-e', 'setInterval(() => {}, 60_000)'], { stdio: 'ignore', windowsHide: true })
    const targetExit = once(target, 'exit')
    let watchdog: ChildProcess | undefined
    try {
      expect(target.pid).toBeDefined()
      const started = Date.now()
      watchdog = spawnWatchdog(target.pid!, 300)
      const watchdogExit = once(watchdog, 'exit')
      await within(targetExit, 10_000, 'the watched process was not killed')
      expect(Date.now() - started).toBeGreaterThanOrEqual(250)
      expect(isAlive(target.pid)).toBe(false)
      // Its job done, the watchdog ends on its own.
      await within(watchdogExit, 10_000, 'the watchdog did not exit')
      expect(isAlive(watchdog.pid)).toBe(false)
    } finally {
      await ensureGone(target)
      if (watchdog) await ensureGone(watchdog)
    }
  })

  it('exits as soon as the process it watches has gone, without waiting for the delay', async () => {
    const target = spawn(process.execPath, ['-e', 'setTimeout(() => {}, 200)'], { stdio: 'ignore', windowsHide: true })
    let watchdog: ChildProcess | undefined
    try {
      expect(target.pid).toBeDefined()
      const started = Date.now()
      watchdog = spawnWatchdog(target.pid!, 20_000)
      await within(once(watchdog, 'exit'), 10_000, 'the watchdog did not exit after its target')
      expect(Date.now() - started).toBeLessThan(10_000)
    } finally {
      await ensureGone(target)
      if (watchdog) await ensureGone(watchdog)
    }
  })
})
