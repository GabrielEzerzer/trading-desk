import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'
import { Sidecar, parseHandshake } from './sidecar'

const FAKE = fileURLToPath(new URL('../../test/fake-sidecar.mjs', import.meta.url))

function fake(mode: string, options: { handshakeTimeoutMs?: number; stopGraceMs?: number } = {}) {
  const lines: string[] = []
  const sidecar = new Sidecar({
    bin: process.execPath,
    args: [FAKE, mode],
    log: (line) => lines.push(line),
    ...options,
  })
  return { sidecar, lines }
}

function isAlive(pid: number | undefined): boolean {
  if (pid === undefined) return false
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

describe('Sidecar', () => {
  it('reads the handshake, logs stderr, and stops cleanly when stdin closes', async () => {
    const { sidecar, lines } = fake('ok')
    expect(await sidecar.ready).toEqual({ port: 45678, token: 'f'.repeat(64) })
    expect(await sidecar.stop()).toBe('clean')
    expect(await sidecar.exited).toEqual({ code: 0, signal: null })
    expect(lines).toContain('fake: listening')
    expect(isAlive(sidecar.pid)).toBe(false)
    // A second stop answers the same.
    expect(await sidecar.stop()).toBe('clean')
  })

  it('fails with the exit code and stderr when the process exits before its handshake', async () => {
    const { sidecar } = fake('early')
    await expect(sidecar.ready).rejects.toThrow(
      /exited before its handshake \(exit code 3\): fake: cannot open the data directory/,
    )
    expect(await sidecar.stop()).toBe('exited')
  })

  it('fails and kills the process when no handshake arrives in time', async () => {
    const { sidecar } = fake('silent', { handshakeTimeoutMs: 500 })
    await expect(sidecar.ready).rejects.toThrow(/no handshake within 0.5 s/)
    await sidecar.exited
    expect(isAlive(sidecar.pid)).toBe(false)
  })

  it('kills a process that ignores stdin closing once the grace period ends', async () => {
    const { sidecar, lines } = fake('stubborn', { stopGraceMs: 300 })
    await sidecar.ready
    expect(await sidecar.stop()).toBe('killed')
    expect(isAlive(sidecar.pid)).toBe(false)
    expect(lines.some((l) => l.includes('killing it'))).toBe(true)
  })

  it('refuses a first line that is not a handshake', async () => {
    const { sidecar } = fake('garbage')
    await expect(sidecar.ready).rejects.toThrow(/something other than its handshake/)
    await sidecar.exited
    expect(isAlive(sidecar.pid)).toBe(false)
  })

  it('fails clearly when the binary does not exist', async () => {
    const bin = 'Z:/no/such/td-server.exe'
    const sidecar = new Sidecar({ bin, log: () => {} })
    await expect(sidecar.ready).rejects.toThrow(`cannot start td-server at ${bin}`)
    expect(await sidecar.stop()).toBe('exited')
  })
})

describe('parseHandshake', () => {
  it('accepts only a port and a well-formed token', () => {
    const token = 'a'.repeat(64)
    expect(parseHandshake(JSON.stringify({ port: 1234, token }))).toEqual({ port: 1234, token })
    expect(parseHandshake(JSON.stringify({ port: 0, token }))).toBeNull()
    expect(parseHandshake(JSON.stringify({ port: 1234, token: 'short' }))).toBeNull()
    expect(parseHandshake(JSON.stringify({ port: '1234', token }))).toBeNull()
    expect(parseHandshake('not json')).toBeNull()
  })
})
