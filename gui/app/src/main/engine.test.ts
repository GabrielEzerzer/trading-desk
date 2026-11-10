import { describe, expect, it } from 'vitest'
import type { EngineStatus, Handshake } from '@td/shared'
import { type EngineProcess, createEngine } from './engine'
import type { ExitInfo, StopResult } from './sidecar'

const HANDSHAKE: Handshake = { port: 45678, token: 'f'.repeat(64) }

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (error: Error) => void
  const promise = new Promise<T>((res, rej) => {
    resolve = res
    reject = rej
  })
  return { promise, resolve, reject }
}

/** An engine process whose handshake, exit and stop the test settles by hand. */
class FakeEngine implements EngineProcess {
  readonly handshake = deferred<Handshake>()
  readonly exit = deferred<ExitInfo>()
  readonly stopping = deferred<StopResult>()
  readonly ready = this.handshake.promise
  readonly exited = this.exit.promise
  stops = 0
  stop(): Promise<StopResult> {
    this.stops++
    return this.stopping.promise
  }
  kill(): void {}
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 0))

function setup() {
  const spawned: FakeEngine[] = []
  const opened: Handshake[] = []
  const statuses: EngineStatus[] = []
  let closes = 0
  const quit = { quitting: false }
  const engine = createEngine({
    spawn: () => {
      const fake = new FakeEngine()
      spawned.push(fake)
      return fake
    },
    openSocket: (connection) => opened.push(connection),
    closeSocket: () => closes++,
    onStatus: (status) => statuses.push(status),
    quit,
    log: () => {},
  })
  return { engine, spawned, opened, statuses, quit, closes: () => closes }
}

describe('createEngine', () => {
  it('opens the socket and reports running once the handshake arrives', async () => {
    const { engine, spawned, opened } = setup()
    const started = engine.start()
    spawned[0]!.handshake.resolve(HANDSHAKE)
    await started
    expect(opened).toEqual([HANDSHAKE])
    expect(engine.connection).toEqual(HANDSHAKE)
    expect(engine.status).toEqual({ state: 'running' })
  })

  it('never opens the socket when a quit arrives before the handshake', async () => {
    const { engine, spawned, opened, quit } = setup()
    const started = engine.start()
    quit.quitting = true
    spawned[0]!.handshake.resolve(HANDSHAKE)
    await started
    expect(opened).toEqual([])
    expect(engine.connection).toBeNull()
    expect(engine.status).toEqual({ state: 'connecting' })
  })

  it('reports no failure when the handshake fails after a quit began', async () => {
    const { engine, spawned, statuses, quit } = setup()
    const started = engine.start()
    quit.quitting = true
    spawned[0]!.handshake.reject(new Error('td-server exited before its handshake'))
    await started
    expect(statuses.map((s) => s.state)).toEqual(['connecting'])
  })

  it('creates no second engine when a quit arrives during a restart', async () => {
    const { engine, spawned, quit } = setup()
    const started = engine.start()
    spawned[0]!.handshake.reject(new Error('no handshake'))
    await started
    expect(engine.status.state).toBe('failed')

    const restarted = engine.restart()
    expect(spawned[0]!.stops).toBe(1)
    quit.quitting = true
    spawned[0]!.stopping.resolve('clean')
    await flush()
    expect(spawned).toHaveLength(1)
    await restarted
  })

  it('does nothing on a restart after a quit began, even from failed', async () => {
    const { engine, spawned, quit } = setup()
    const started = engine.start()
    spawned[0]!.handshake.reject(new Error('no handshake'))
    await started
    quit.quitting = true
    const restarted = engine.restart()
    await flush()
    expect(spawned[0]!.stops).toBe(0)
    expect(spawned).toHaveLength(1)
    expect(engine.status.state).toBe('failed')
    await restarted
  })

  it('reports a failure when a running engine exits, but nothing after a quit, and never restarts or reconnects', async () => {
    for (const quitting of [false, true]) {
      const { engine, spawned, opened, statuses, quit, closes } = setup()
      const started = engine.start()
      spawned[0]!.handshake.resolve(HANDSHAKE)
      await started
      quit.quitting = quitting
      spawned[0]!.exit.resolve({ code: 1, signal: null })
      await flush()
      expect(closes()).toBe(1)
      expect(engine.connection).toBeNull()
      expect(spawned).toHaveLength(1)
      expect(opened).toHaveLength(1)
      expect(statuses.at(-1)).toEqual(quitting ? { state: 'running' } : { state: 'failed', message: 'the engine stopped (exit code 1)' })
    }
  })

  it('ignores the exit of an engine it has already replaced', async () => {
    const { engine, spawned, closes } = setup()
    const started = engine.start()
    spawned[0]!.handshake.reject(new Error('no handshake'))
    await started
    const restarted = engine.restart()
    spawned[0]!.stopping.resolve('killed')
    await flush()
    spawned[1]!.handshake.resolve(HANDSHAKE)
    await restarted
    expect(engine.status).toEqual({ state: 'running' })

    spawned[0]!.exit.resolve({ code: null, signal: 'SIGTERM' })
    await flush()
    expect(closes()).toBe(0)
    expect(engine.connection).toEqual(HANDSHAKE)
    expect(engine.status).toEqual({ state: 'running' })
  })
})
