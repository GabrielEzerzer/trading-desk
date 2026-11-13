// Proves the app quits: launches the built app (run `pnpm build` first) with the real engine,
// waits for the engine to run, opens a section, closes the window the way the user does, and checks
// that Electron, its helper processes and td-server are all gone within a few seconds. Exits 1 if
// any is left. Check both sections: Trading, with its live chart open, is the case that hangs in #71.
//
//   node scripts/quit-check.mjs [lab|trading]     from gui/; the Lab when no section is given
//
// Needs target/release/td-server(.exe), or TD_SERVER_BIN.

import { existsSync, mkdtempSync } from 'node:fs'
import { rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { _electron as electron } from 'playwright'
import { killAll, processTree, waitGone } from './procs.mjs'

const gui = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const exe = process.platform === 'win32' ? 'td-server.exe' : 'td-server'
const bin = process.env.TD_SERVER_BIN ?? join(gui, '..', 'target', 'release', exe)
if (!existsSync(bin)) {
  console.error(`quit-check: no engine binary at ${bin}; run cargo build --release -p td-server --bin td-server`)
  process.exit(2)
}
const section = process.argv[2] ?? 'lab'
if (section !== 'lab' && section !== 'trading') {
  console.error(`quit-check: unknown section ${section}; pass lab or trading`)
  process.exit(2)
}
const QUIT_WITHIN_MS = 5_000
const dataDir = mkdtempSync(join(tmpdir(), 'td-quit-'))
// A killed td-server can hold files for a moment on Windows, so retry. The promise version, since
// rmSync on Node 24 does not retry a Windows sharing violation.
const removeDataDir = () => rm(dataDir, { recursive: true, force: true, maxRetries: 10 })

const app = await electron.launch({
  args: [join(gui, 'app')],
  env: { ...process.env, TD_SERVER_BIN: bin, TD_DATA_DIR: dataDir, NODE_ENV: 'production' },
})
const page = await app.firstWindow()
await page.waitForFunction(async () => (await window.td.status()).state === 'running', null, { timeout: 20_000 })
if (section === 'trading') {
  await page.getByRole('radio', { name: 'Trading' }).click()
  await page.locator('canvas').first().waitFor({ timeout: 20_000 })
  // Long enough for the history to load and the live stream to deliver bars.
  await page.waitForTimeout(6_000)
  // The chart mounts before any data arrives, so an empty chart (no history, offline) would pass
  // without ever running the live stream that hangs the quit.
  const empty = page.locator('.chart-empty')
  if ((await empty.count()) > 0) {
    console.error(`quit-check: FAIL, the Trading chart has no data: ${(await empty.first().innerText({ timeout: 1_000 }).catch(() => '')).replace(/\s+/g, ' ')}`)
    killAll(processTree(app.process().pid))
    await removeDataDir()
    process.exit(2)
  }
}
const tree = processTree(app.process().pid)
const engine = tree.filter((p) => p.name.toLowerCase().startsWith('td-server'))
console.log(`quit-check: ${section}, ${tree.length} processes under Electron, td-server pid ${engine.map((p) => p.pid).join(',') || 'none'}`)
if (engine.length !== 1) {
  killAll(tree)
  console.error('quit-check: FAIL, expected exactly one td-server under Electron')
  process.exit(1)
}

const started = Date.now()
// What the window's close button does.
await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0]?.close())
const left = await waitGone(tree, QUIT_WITHIN_MS)
const ms = Date.now() - started
if (left.length > 0) {
  console.error(`quit-check: FAIL, still running ${QUIT_WITHIN_MS} ms after close: ${left.map((p) => `${p.name} ${p.pid}`).join(', ')}`)
  killAll(left)
  await removeDataDir()
  process.exit(1)
}
await removeDataDir()
console.log(`quit-check: PASS, all ${tree.length} processes exited ${ms} ms after the window closed`)
process.exit(0)
