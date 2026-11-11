// Starts and stops the td-server engine. It prints one handshake line, {"port","token"}, on
// stdout and logs on stderr; it exits when its stdin closes. If the app itself dies, the OS closes
// the pipe, so the engine stops on its own and is not left behind.

import { type ChildProcess, spawn } from 'node:child_process'
import { join } from 'node:path'
import { createInterface } from 'node:readline'
import type { Handshake } from '@td/shared'

export interface SidecarOptions {
  bin: string
  args?: string[]
  env?: NodeJS.ProcessEnv
  /** How long to wait for the handshake line. */
  handshakeTimeoutMs?: number
  /** How long `stop` waits after closing stdin before it kills the process. */
  stopGraceMs?: number
  log: (line: string) => void
}

export interface ExitInfo {
  code: number | null
  signal: NodeJS.Signals | null
}

/** `clean`: it exited after stdin closed. `killed`: it had to be killed. `exited`: already gone. */
export type StopResult = 'clean' | 'killed' | 'exited'

const HANDSHAKE_TIMEOUT_MS = 15_000
// The server gives open requests and running jobs up to 5 s each (JOB_GRACE in td-server) before
// it exits on its own; killing it sooner could cut a store write short.
const STOP_GRACE_MS = 11_000
const STDERR_TAIL = 10

/** Where the engine binary is: `TD_SERVER_BIN`, else the release build in the repo's target dir. The
 * debug build runs the same code about nine times slower (td-bench), so the app never starts it. */
export function sidecarBin(env: NodeJS.ProcessEnv, appPath: string, isPackaged: boolean): string {
  if (env.TD_SERVER_BIN) return env.TD_SERVER_BIN
  const exe = process.platform === 'win32' ? 'td-server.exe' : 'td-server'
  // shortcut: packaging is not set up; ship the binary in resources when the app is packaged.
  if (isPackaged) return join(process.resourcesPath, exe)
  return join(appPath, '..', '..', 'target', 'release', exe)
}

export class Sidecar {
  /** Resolves with the handshake; rejects if it does not arrive in time or the process exits first. */
  readonly ready: Promise<Handshake>
  /** Resolves once the process has exited and its output is drained. */
  readonly exited: Promise<ExitInfo>
  readonly pid: number | undefined
  private readonly child: ChildProcess
  private readonly log: (line: string) => void
  private readonly stopGraceMs: number
  private hasExited = false
  private stopping: Promise<StopResult> | null = null

  constructor(options: SidecarOptions) {
    this.log = options.log
    this.stopGraceMs = options.stopGraceMs ?? STOP_GRACE_MS
    const timeoutMs = options.handshakeTimeoutMs ?? HANDSHAKE_TIMEOUT_MS
    const child = spawn(options.bin, options.args ?? [], {
      env: options.env,
      stdio: ['pipe', 'pipe', 'pipe'],
      windowsHide: true,
    })
    this.child = child
    this.pid = child.pid
    // Writing to or closing the pipe of a process that has gone raises EPIPE here; exit handles it.
    child.stdin?.on('error', () => {})

    const stderrTail: string[] = []
    if (child.stderr) {
      createInterface({ input: child.stderr }).on('line', (line) => {
        this.log(line)
        stderrTail.push(line)
        if (stderrTail.length > STDERR_TAIL) stderrTail.shift()
      })
    }

    let spawnError: Error | null = null
    this.exited = new Promise((resolve) => {
      child.once('error', (error) => {
        spawnError = error
        // A process that failed to spawn emits no 'close'.
        if (child.pid === undefined) {
          this.hasExited = true
          resolve({ code: null, signal: null })
        }
      })
      child.once('close', (code, signal) => {
        this.hasExited = true
        resolve({ code, signal })
      })
    })

    this.ready = new Promise<Handshake>((resolve, reject) => {
      let settled = false
      const settle = (error: Error | null, handshake?: Handshake) => {
        if (settled) return
        settled = true
        clearTimeout(timer)
        if (error) reject(error)
        else if (handshake) resolve(handshake)
      }
      const timer = setTimeout(() => {
        settle(new Error(`td-server sent no handshake within ${timeoutMs / 1000} s`))
        this.kill()
      }, timeoutMs)

      let first = true
      if (child.stdout) {
        createInterface({ input: child.stdout }).on('line', (line) => {
          if (!first) {
            this.log(`unexpected stdout: ${line}`)
            return
          }
          first = false
          const handshake = parseHandshake(line)
          if (handshake) {
            settle(null, handshake)
          } else {
            settle(new Error('td-server printed something other than its handshake line'))
            this.kill()
          }
        })
      }

      void this.exited.then(({ code, signal }) => {
        if (spawnError) {
          settle(new Error(`cannot start td-server at ${options.bin}: ${spawnError.message}`))
          return
        }
        const how = signal ? `signal ${signal}` : `exit code ${code}`
        const tail = stderrTail.length > 0 ? `: ${stderrTail.join(' | ')}` : ''
        settle(new Error(`td-server exited before its handshake (${how})${tail}`))
      })
    })
    // Callers that never await `ready` must not crash the process with an unhandled rejection.
    this.ready.catch(() => {})
  }

  /**
   * Closes stdin, which tells the engine to stop, and kills it if it has not exited after the
   * grace period. Safe to call more than once and before the handshake.
   */
  stop(): Promise<StopResult> {
    this.stopping ??= this.stopNow()
    return this.stopping
  }

  /** Kills the process at once. For the last moment of the app's own exit, when nothing can wait. */
  kill(): void {
    if (!this.hasExited) this.child.kill()
  }

  private async stopNow(): Promise<StopResult> {
    if (this.hasExited) return 'exited'
    this.child.stdin?.end()
    let timer: NodeJS.Timeout | undefined
    const graceful = await Promise.race([
      this.exited.then(() => true),
      new Promise<false>((resolve) => {
        timer = setTimeout(() => resolve(false), this.stopGraceMs)
      }),
    ])
    clearTimeout(timer)
    if (graceful) return 'clean'
    this.log(`td-server did not exit within ${this.stopGraceMs} ms of stdin closing; killing it`)
    this.child.kill()
    await this.exited
    return 'killed'
  }
}

const TOKEN = /^[A-Za-z0-9._-]{32,}$/

export function parseHandshake(line: string): Handshake | null {
  let value: unknown
  try {
    value = JSON.parse(line)
  } catch {
    return null
  }
  if (typeof value !== 'object' || value === null) return null
  const { port, token } = value as Record<string, unknown>
  if (typeof port !== 'number' || !Number.isInteger(port) || port < 1 || port > 65535) return null
  if (typeof token !== 'string' || !TOKEN.test(token)) return null
  return { port, token }
}
