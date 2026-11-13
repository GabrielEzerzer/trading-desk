// Proves the Windows installer works on a fresh profile. It installs the built installer silently
// into a temporary folder, checks that the installed engine starts on an empty data folder and
// answers what Strategy Lab asks for first, launches the installed app once with TD_SMOKE=1 (the
// app quits by itself once the engine's handshake has arrived and its page has called main through
// window.td), then kills whatever is left, uninstalls silently and deletes the temporary folders.
// Prints one line,
// INSTALL-CHECK PASS or INSTALL-CHECK FAIL: <reason>, and exits 0 or 1; install-verdict.mjs says
// how the launch is read, including the pass with a note when the quit watchdog ends the app.
//
//   pnpm dist && pnpm install-check          from gui/
//
// It starts one real Electron window, so run it by hand, once, never in a loop.

import { execFileSync, spawn, spawnSync } from 'node:child_process'
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { createInterface } from 'node:readline'
import { fileURLToPath } from 'node:url'
import { launchVerdict } from './install-verdict.mjs'

const gui = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const { productName, version } = JSON.parse(readFileSync(join(gui, 'app', 'package.json'), 'utf8'))
const installer = join(gui, 'app', 'dist', `${productName} Setup ${version}.exe`)
// The app quits itself within 30 s (SMOKE_TIMEOUT_MS) plus its stop; this is the backstop.
const LAUNCH_LIMIT_MS = 60_000
const ENGINE_LIMIT_MS = 15_000
const UNINSTALL_LIMIT_MS = 60_000

const temp = mkdtempSync(join(tmpdir(), 'td-install-'))
const installDir = join(temp, 'app')
let app = null
let installed = false
// '' or the watchdog note for the pass line (install-verdict.mjs).
let passNote = ''

function step(line) {
  console.log(`install-check: ${line}`)
}

function delay(ms) {
  return new Promise((r) => setTimeout(r, ms))
}

/** Uninstall entries for this product in the current user's registry, as text. */
function registeredInstalls() {
  const out = spawnSync('reg', ['query', 'HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall', '/s', '/f', productName, '/d'], {
    encoding: 'utf8',
  })
  return out.stdout.includes('DisplayName') ? out.stdout : ''
}

/** A child of the environment without anything that would point the app elsewhere. */
function cleanEnv(extra) {
  const env = { ...process.env, ...extra }
  for (const name of ['TD_SERVER_BIN', 'TD_MOCK', 'TD_TOKEN', 'ELECTRON_RUN_AS_NODE', 'ELECTRON_RENDERER_URL', 'NODE_OPTIONS']) {
    delete env[name]
  }
  return env
}

/** Starts the installed engine on its own, on an empty data folder, and asks for what the Lab loads first. */
async function checkEngine(bin) {
  const dataDir = join(temp, 'engine-data')
  const engine = spawn(bin, [], { env: cleanEnv({ TD_DATA_DIR: dataDir }), stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true })
  // An exe that cannot start (blocked by Defender or Smart App Control, say) emits 'error'; without
  // a listener that would crash this script before it uninstalls.
  engine.on('error', () => {})
  const stderr = []
  createInterface({ input: engine.stderr }).on('line', (line) => stderr.push(line))
  try {
    const line = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`no handshake within ${ENGINE_LIMIT_MS / 1000} s`)), ENGINE_LIMIT_MS)
      createInterface({ input: engine.stdout }).once('line', (l) => {
        clearTimeout(timer)
        resolve(l)
      })
      engine.once('exit', (code) => reject(new Error(`exited with code ${code}: ${stderr.join(' | ')}`)))
      engine.once('error', (e) => reject(new Error(`cannot start the installed engine: ${e.message}`)))
    })
    const { port, token } = JSON.parse(line)
    const get = async (path) => {
      const res = await fetch(`http://127.0.0.1:${port}${path}`, { headers: { authorization: `Bearer ${token}` } })
      if (!res.ok) throw new Error(`GET ${path} answered ${res.status}: ${await res.text()}`)
      return res.json()
    }
    const strategies = await get('/strategies')
    const runs = await get('/runs')
    const coverage = await get('/data/coverage?instrument=binance:BTCUSDT:spot')
    if (!Array.isArray(strategies) || strategies.length === 0) throw new Error('GET /strategies listed no strategies')
    if (!Array.isArray(runs) || runs.length !== 0) throw new Error('GET /runs was not an empty list on an empty data folder')
    if (!Array.isArray(coverage.months) || coverage.months.length !== 0) throw new Error('GET /data/coverage was not empty on an empty data folder')
    step(`installed engine started on an empty data folder: ${strategies.length} strategies, no runs, no data`)
  } finally {
    if (engine.pid !== undefined) {
      engine.stdin.end()
      await Promise.race([new Promise((r) => engine.once('exit', r)), delay(ENGINE_LIMIT_MS)])
      if (engine.exitCode === null) engine.kill()
    }
  }
}

/** Launches the installed app once with the smoke hook on; returns its exit code and output. */
async function launch(exe, dataDir, profileDir) {
  app = spawn(exe, [], {
    env: cleanEnv({ TD_SMOKE: '1', TD_DATA_DIR: dataDir, TD_USER_DATA_DIR: profileDir }),
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  // As for the engine: a blocked exe must fail the check, not crash it before cleanup.
  app.on('error', () => {})
  const output = []
  for (const stream of [app.stdout, app.stderr]) createInterface({ input: stream }).on('line', (line) => line && output.push(line))
  const code = await Promise.race([
    new Promise((resolve, reject) => {
      app.once('exit', (c) => resolve(c))
      app.once('error', (e) => reject(new Error(`cannot start the installed app: ${e.message}`)))
    }),
    delay(LAUNCH_LIMIT_MS).then(() => 'timeout'),
  ])
  // Let the last lines drain.
  await delay(200)
  return { code, output }
}

/** The processes running from the install folder, as "<pid> <command line>". */
function installedProcesses() {
  return execFileSync(
    'powershell.exe',
    [
      '-NoProfile',
      '-Command',
      `Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -like '${installDir}\\*' } | ForEach-Object { "$($_.ProcessId) $($_.CommandLine)" }`,
    ],
    { encoding: 'utf8' },
  )
    .split(/\r?\n/)
    .filter(Boolean)
}

/** Waits up to `ms` for nothing to run from the install folder; returns what still runs. */
async function waitInstalledGone(ms) {
  const deadline = Date.now() + ms
  for (;;) {
    const left = installedProcesses()
    if (left.length === 0 || Date.now() > deadline) return left
    await delay(500)
  }
}

/** Kills the app's tree if it still runs, gives its helpers a moment to exit, kills what is left
 * running from the install folder and waits until it is gone, so the uninstaller finds no file in use. */
async function killAll() {
  if (app && app.pid !== undefined && app.exitCode === null && app.signalCode === null) {
    spawnSync('taskkill', ['/T', '/F', '/PID', String(app.pid)], { stdio: 'ignore' })
  }
  for (const line of await waitInstalledGone(5_000)) {
    step(`killing a process still running from the install folder 5 s on: ${line}`)
    spawnSync('taskkill', ['/T', '/F', '/PID', line.split(' ')[0]], { stdio: 'ignore' })
  }
  const left = await waitInstalledGone(10_000)
  if (left.length > 0) throw new Error(`still running from the install folder after taskkill: ${left.join('; ')}`)
}

/** Up to ten paths left under a folder, relative to it. */
function leftovers(dir) {
  return readdirSync(dir, { recursive: true }).slice(0, 10).join(', ')
}

async function uninstall() {
  const uninstaller = readdirSync(installDir).find((name) => /^Uninstall .*\.exe$/.test(name))
  if (!uninstaller) throw new Error(`no uninstaller in ${installDir}`)
  // The uninstaller copies itself to a temporary folder and returns at once. It deletes the folder
  // first and its registry entries after, so wait for both.
  spawnSync(join(installDir, uninstaller), ['/S'], { stdio: 'ignore' })
  const deadline = Date.now() + UNINSTALL_LIMIT_MS
  while ((existsSync(installDir) || registeredInstalls()) && Date.now() < deadline) await delay(500)
  const after = `${UNINSTALL_LIMIT_MS / 1000} s after uninstalling`
  if (existsSync(installDir)) throw new Error(`the install folder is still there ${after}, holding: ${leftovers(installDir)}`)
  if (registeredInstalls()) throw new Error(`the uninstall entry is still in the registry ${after}`)
  step('uninstalled; the install folder and the uninstall entry are gone')
}

async function main() {
  if (process.platform !== 'win32') throw new Error('the installer is Windows only')
  if (!existsSync(installer)) throw new Error(`no installer at ${installer}; run pnpm dist`)
  // A real install shares the uninstall entry; uninstalling at the end would remove it.
  if (registeredInstalls()) throw new Error(`${productName} is already installed for this user; uninstall it first`)

  // NSIS takes /D= last and unquoted, spaces and all, and Node would quote a path with a space, so
  // the command line is passed verbatim, with only the installer's own path quoted.
  const install = spawnSync(installer, ['/S', `/D=${installDir}`], {
    argv0: `"${installer}"`,
    windowsVerbatimArguments: true,
    stdio: 'ignore',
    timeout: 180_000,
  })
  installed = true
  if (install.status !== 0) throw new Error(`the installer exited with ${install.status ?? install.error?.message}`)
  const exe = join(installDir, `${productName}.exe`)
  const engineBin = join(installDir, 'resources', 'td-server.exe')
  for (const file of [exe, engineBin]) if (!existsSync(file)) throw new Error(`the install has no ${file}`)
  step(`installed into ${installDir}`)

  await checkEngine(engineBin)

  const dataDir = join(temp, 'data')
  const profileDir = join(temp, 'profile')
  const { code, output } = await launch(exe, dataDir, profileDir)
  for (const line of output) step(`app: ${line}`)
  passNote = launchVerdict({ code, output, engineBin, dataDir })
  if (!existsSync(profileDir) || readdirSync(profileDir).length === 0) throw new Error(`the app did not use the fresh profile ${profileDir}`)
  step(`the installed app started its own engine, its page reached main through window.td, it stopped the engine cleanly and exited with code ${code}`)

  await killAll()
  await uninstall()
  installed = false
}

let failure = null
try {
  await main()
} catch (e) {
  failure = e instanceof Error ? e.message : String(e)
}
// Clean up whatever happened.
try {
  await killAll()
} catch (e) {
  failure ??= `cleaning up processes: ${e instanceof Error ? e.message : String(e)}`
}
if (installed && existsSync(installDir)) {
  try {
    await uninstall()
  } catch (e) {
    failure ??= e instanceof Error ? e.message : String(e)
  }
}
try {
  rmSync(temp, { recursive: true, force: true, maxRetries: 5, retryDelay: 500 })
} catch (e) {
  step(`could not delete ${temp}: ${e instanceof Error ? e.message : String(e)}`)
}
if (failure) {
  console.log(`INSTALL-CHECK FAIL: ${failure}`)
  process.exit(1)
}
console.log(`INSTALL-CHECK PASS${passNote}`)
process.exit(0)
