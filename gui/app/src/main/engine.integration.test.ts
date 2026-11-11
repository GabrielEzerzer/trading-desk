// Starts the real td-server binary and talks to it the way main does. Build it first with
// `cargo build --release -p td-server`; without the binary (JS-only CI) the test is skipped.

import { existsSync } from 'node:fs'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { afterAll, describe, expect, it } from 'vitest'
import { type ServerMessage, createClient } from '@td/shared'
import { type Connection, sendRequest } from './bridge'
import { Sidecar, sidecarBin } from './sidecar'
import { EngineSocket } from './socket'

const APP_PATH = fileURLToPath(new URL('../..', import.meta.url))
const BIN = sidecarBin(process.env, APP_PATH, false)
const present = existsSync(BIN)

if (!present) {
  console.warn(`Skipping the td-server integration test: no binary at ${BIN}. Run cargo build --release -p td-server.`)
}

describe.skipIf(!present)('the real td-server', () => {
  const dirs: string[] = []
  afterAll(async () => {
    for (const dir of dirs) await rm(dir, { recursive: true, force: true })
  })

  it('answers /health and /strategies through the main-process client, streams jobs, and stops cleanly', async () => {
    const dataDir = await mkdtemp(join(tmpdir(), 'td-gui-test-'))
    dirs.push(dataDir)
    const env: NodeJS.ProcessEnv = { ...process.env, TD_DATA_DIR: dataDir }
    delete env.TD_TOKEN
    const lines: string[] = []
    const sidecar = new Sidecar({ bin: BIN, env, log: (line) => lines.push(line) })
    try {
      const connection: Connection = await sidecar.ready
      const client = createClient((request) => sendRequest(connection, request))

      expect((await client.health()).version).toMatch(/^\d+\.\d+\.\d+/)
      const strategies = await client.strategies()
      expect(strategies.length).toBeGreaterThan(0)
      expect(strategies[0]).toHaveProperty('params')
      await expect(client.job(999)).rejects.toMatchObject({ status: 404, code: 'not_found' })

      // A request with the wrong token is refused by the server itself.
      const wrong = await sendRequest({ ...connection, token: 'b'.repeat(64) }, { method: 'GET', path: '/health' })
      expect(wrong).toMatchObject({ ok: false, status: 401, error: { code: 'unauthorized' } })

      // The WebSocket authenticates with the token in its subprotocol.
      const messages: ServerMessage[] = []
      const subscribed = new Promise<void>((resolve) => {
        const socket = new EngineSocket((message) => {
          messages.push(message)
          if (message.type === 'subscribed') {
            socket.close()
            resolve()
          }
        }, (line) => lines.push(line))
        socket.subscribe('jobs')
        socket.open(connection)
      })
      await subscribed
      expect(messages[0]).toEqual({ type: 'subscribed', topic: 'jobs' })
    } finally {
      expect(await sidecar.stop()).toBe('clean')
    }
    expect(lines.some((line) => line.includes('stdin closed'))).toBe(true)
  })
})
