import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { type QuitDeps, createQuit } from './quit'

function setup(stopEngine: () => Promise<string>, overrides: Partial<QuitDeps> = {}) {
  const calls: string[] = []
  const deps: QuitDeps = {
    stopEngine: () => {
      calls.push('stopEngine')
      return stopEngine()
    },
    killEngine: () => calls.push('killEngine'),
    closeConnections: () => calls.push('closeConnections'),
    exit: (code) => calls.push(`exit ${code}`),
    armWatchdog: (afterMs) => calls.push(`armWatchdog ${afterMs}`),
    log: (line) => calls.push(`log ${line}`),
    ...overrides,
  }
  const quit = createQuit(deps, { stopTimeoutMs: 1_000, terminateAfterMs: 500 })
  const event = () => {
    const e = { prevented: false, preventDefault: () => (e.prevented = true) }
    return e
  }
  return { quit, calls, event }
}

describe('createQuit', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('closes connections, stops the engine, then exits once, without re-entering quit', async () => {
    const { quit, calls, event } = setup(() => Promise.resolve('clean'))
    const first = event()
    quit.onBeforeQuit(first)
    expect(first.prevented).toBe(true)
    expect(quit.quitting).toBe(true)
    expect(calls).toEqual(['closeConnections', 'stopEngine'])

    // A second quit while stopping (window-all-closed racing another quit) is swallowed here.
    const second = event()
    quit.onBeforeQuit(second)
    expect(second.prevented).toBe(true)

    await vi.advanceTimersByTimeAsync(0)
    expect(calls).toEqual(['closeConnections', 'stopEngine', 'log stopped (clean)', 'armWatchdog 500', 'exit 0'])

    // A quit after exit was asked for changes nothing, and the stop timeout never fires.
    quit.onBeforeQuit(event())
    await vi.advanceTimersByTimeAsync(400)
    expect(calls.filter((c) => c.startsWith('exit'))).toHaveLength(1)
    expect(calls).not.toContain('killEngine')
  })

  it('kills the engine and exits anyway when the stop does not finish in time', async () => {
    const { quit, calls } = setup(() => new Promise(() => {}))
    quit.onBeforeQuit({ preventDefault() {} })
    await vi.advanceTimersByTimeAsync(999)
    expect(calls).not.toContain('exit 0')
    await vi.advanceTimersByTimeAsync(1)
    expect(calls).toContain('killEngine')
    expect(calls.at(-1)).toBe('exit 0')
  })

  it('kills the engine and exits when stopping throws or rejects', async () => {
    for (const stop of [
      () => Promise.reject(new Error('pipe broke')),
      () => {
        throw new Error('no engine')
      },
    ]) {
      const { quit, calls } = setup(stop)
      quit.onBeforeQuit({ preventDefault() {} })
      await vi.advanceTimersByTimeAsync(0)
      expect(calls).toContain('killEngine')
      expect(calls.at(-1)).toBe('exit 0')
    }
  })

  it('arms the watchdog exactly once, just before exit, even when the stop timeout also fires', async () => {
    const { quit, calls } = setup(() => new Promise(() => {}))
    quit.onBeforeQuit({ preventDefault() {} })
    expect(calls).not.toContain('armWatchdog 500')
    await vi.advanceTimersByTimeAsync(1_000)
    quit.onBeforeQuit({ preventDefault() {} })
    await vi.advanceTimersByTimeAsync(5_000)
    expect(calls.filter((c) => c.startsWith('armWatchdog'))).toEqual(['armWatchdog 500'])
    expect(calls.slice(-2)).toEqual(['armWatchdog 500', 'exit 0'])
  })

  it('still exits when arming the watchdog throws', async () => {
    const { quit, calls } = setup(() => Promise.resolve('clean'), {
      armWatchdog: () => {
        throw new Error('spawn failed')
      },
    })
    quit.onBeforeQuit({ preventDefault() {} })
    await vi.advanceTimersByTimeAsync(0)
    expect(calls.slice(-2)).toEqual(['log arming the quit watchdog failed: spawn failed', 'exit 0'])
  })

  it('still stops the engine when closing connections throws', async () => {
    const { quit, calls } = setup(() => Promise.resolve('clean'), {
      closeConnections: () => {
        throw new Error('socket gone')
      },
    })
    quit.onBeforeQuit({ preventDefault() {} })
    await vi.advanceTimersByTimeAsync(0)
    expect(calls).toEqual([
      'log closing connections failed: socket gone',
      'stopEngine',
      'log stopped (clean)',
      'armWatchdog 500',
      'exit 0',
    ])
  })
})
