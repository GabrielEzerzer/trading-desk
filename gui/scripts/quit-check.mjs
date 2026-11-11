// Proves the app quits: launches the built app (run `pnpm build` first) with the real engine,
// waits for the engine to run, closes the window the way the user does, and checks that Electron,
// its helper processes and td-server are all gone within a few seconds. Exits 1 if any is left.
//
//   node scripts/quit-check.mjs            from gui/
//
// Needs target/release/td-server(.exe), or TD_SERVER_BIN.

import { existsSync, mkdtempSync, rmSync } from 'node:fs'
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
const QUIT_WITHIN_MS = 5_000
const dataDir = mkdtempSync(join(tmpdir(), 'td-quit-'))

const app = await electron.launch({
  args: [join(gui, 'app')],
  env: { ...process.env, TD_SERVER_BIN: bin, TD_DATA_DIR: dataDir, NODE_ENV: 'production' },
})
const page = await app.firstWindow()
await page.waitForFunction(async () => (await window.td.status()).state === 'running', null, { timeout: 20_000 })
const tree = processTree(app.process().pid)
const engine = tree.filter((p) => p.name.toLowerCase().startsWith('td-server'))
console.log(`quit-check: ${tree.length} processes under Electron, td-server pid ${engine.map((p) => p.pid).join(',') || 'none'}`)
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
rmSync(dataDir, { recursive: true, force: true })
if (left.length > 0) {
  console.error(`quit-check: FAIL, still running ${QUIT_WITHIN_MS} ms after close: ${left.map((p) => `${p.name} ${p.pid}`).join(', ')}`)
  killAll(left)
  process.exit(1)
}
console.log(`quit-check: PASS, all ${tree.length} processes exited ${ms} ms after the window closed`)
process.exit(0)
