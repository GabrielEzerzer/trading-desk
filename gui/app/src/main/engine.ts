// Starts, restarts and watches the engine sidecar, kept free of Electron so it can be tested with
// the pieces injected, like quit.ts. Once a quit has begun nothing here starts an engine, opens a
// socket or reports a failure: the quit owns the engine from then on.

import type { EngineStatus, Handshake } from '@td/shared'
import type { Connection } from './bridge'
import type { ExitInfo, StopResult } from './sidecar'

/** The part of a `Sidecar` this module uses. */
export interface EngineProcess {
  readonly ready: Promise<Handshake>
  readonly exited: Promise<ExitInfo>
  stop(): Promise<StopResult>
  kill(): void
}

export interface EngineDeps {
  /** Starts a new engine process (a `Sidecar`). */
  spawn(): EngineProcess
  /** Opens the WebSocket to a running engine. */
  openSocket(connection: Connection): void
  /** Closes it and stops it reconnecting. */
  closeSocket(): void
  /** Called on every status change, to tell the window. */
  onStatus(status: EngineStatus): void
  quit: { readonly quitting: boolean }
  log(line: string): void
}

export interface Engine {
  readonly status: EngineStatus
  /** The running engine's address and token; null while it is starting, failed or stopping. */
  readonly connection: Connection | null
  start(): Promise<void>
  /** Starts a new engine after the last one failed. Anything else is left alone. */
  restart(): Promise<void>
  /** Stops the current engine, for the quit; resolves with how it went. */
  stop(): Promise<string>
  /** Kills the current engine at once. */
  kill(): void
  /** Forgets the connection and closes the socket. */
  disconnect(): void
}

export function createEngine(deps: EngineDeps): Engine {
  let status: EngineStatus = { state: 'connecting' }
  let current: EngineProcess | null = null
  let connection: Connection | null = null

  function setStatus(next: EngineStatus): void {
    status = next
    deps.onStatus(next)
  }

  function disconnect(): void {
    connection = null
    deps.closeSocket()
  }

  async function start(): Promise<void> {
    if (deps.quit.quitting) return
    setStatus({ state: 'connecting' })
    const engine = deps.spawn()
    current = engine
    void engine.exited.then(({ code, signal }) => {
      if (current !== engine) return
      disconnect()
      if (!deps.quit.quitting && status.state === 'running') {
        setStatus({ state: 'failed', message: `the engine stopped (${signal ? `signal ${signal}` : `exit code ${code}`})` })
      }
    })
    try {
      const handshake = await engine.ready
      // The handshake can arrive after a quit began; never open a socket to an engine being stopped.
      if (deps.quit.quitting || current !== engine) return
      connection = handshake
      deps.openSocket(handshake)
      setStatus({ state: 'running' })
    } catch (e) {
      if (deps.quit.quitting || current !== engine) return
      const message = e instanceof Error ? e.message : String(e)
      deps.log(message)
      setStatus({ state: 'failed', message })
    }
  }

  async function restart(): Promise<void> {
    if (deps.quit.quitting || status.state !== 'failed') return
    setStatus({ state: 'connecting' })
    const old = current
    if (old) deps.log(`stopped before restart (${await old.stop()})`)
    await start()
  }

  return {
    get status() {
      return status
    },
    get connection() {
      return connection
    },
    start,
    restart,
    stop: () => (current ? current.stop() : Promise.resolve('exited')),
    kill: () => current?.kill(),
    disconnect,
  }
}
