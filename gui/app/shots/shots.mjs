// Headless screenshots of the renderer, for looking at the design: every state at three window
// sizes, into gui/shots/ (git-ignored). Run `pnpm shots` from gui/, which builds the app first.
//
// It never launches Electron. It serves the built renderer from app/out/renderer on a local port
// for the length of the run, opens one headless Chromium, injects bridge-stub.js as `window.td`,
// and closes both in `finally`. The states and the server live in harness.mjs, shared with the
// Playwright UI gates in gui/e2e.

import { mkdirSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { chromium } from 'playwright'
import { SIZES, STATES, openApp, serve, settle } from './harness.mjs'

const outDir = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', 'shots')

const only = process.argv[2] ? new RegExp(process.argv[2]) : null
const { server, url } = await serve()
let browser
let failures = 0
try {
  browser = await chromium.launch({ headless: true })
  for (const size of SIZES) {
    mkdirSync(join(outDir, size.name), { recursive: true })
    for (const state of STATES) {
      if (only && !only.test(state.name)) continue
      const context = await browser.newContext({ viewport: { width: size.width, height: size.height } })
      const page = await context.newPage()
      const errors = []
      page.on('pageerror', (e) => errors.push(e.message))
      page.on('console', (m) => m.type() === 'error' && errors.push(m.text()))
      try {
        await openApp(page, url, state.config, { clock: state.clock })
        await state.steps(page)
        await settle(page)
        const file = join(outDir, size.name, `${state.name}.png`)
        await page.screenshot({ path: file })
        console.log(`${size.name}/${state.name}.png${errors.length ? `  errors: ${errors.join(' | ')}` : ''}`)
        if (errors.length) failures += 1
      } catch (e) {
        failures += 1
        console.error(`${size.name}/${state.name}: FAILED ${e instanceof Error ? e.message : e}`)
      } finally {
        await context.close()
      }
    }
  }
} finally {
  await browser?.close()
  await new Promise((r) => server.close(r))
}
console.log(failures ? `shots: ${failures} with errors` : 'shots: done')
process.exit(failures ? 1 : 0)
