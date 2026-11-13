// Visual baselines for the key Lab states and every Trading state at each window size. Trading's
// states run on a held clock (harness.mjs), so the live chart shows the same bars on every run.
// Font rendering differs between operating systems, so the baselines are made and compared on CI's
// Linux runner only; elsewhere this spec skips (README, "UI gates").
import { SIZES, STATES, settle } from '../app/shots/harness.mjs'
import { expect, test } from './fixtures'

const KEY_STATES = [
  'engine-failed',
  'lab-list',
  'lab-strategy-menu',
  'lab-never-run',
  'lab-running',
  'lab-result-trade',
  'lab-result-evidence',
  'lab-run-failed',
  'lab-list-engine',
  'lab-engine-past-result',
  'lab-engine-past-result-no-data',
  'lab-engine-null-metrics',
  'lab-engine-no-data',
  'lab-engine-downloading',
  'lab-engine-too-many-runs',
  'lab-data',
  'lab-data-empty',
  'lab-data-confirm',
  'lab-data-delete-failed',
  ...STATES.filter((s) => s.section === 'trading').map((s) => s.name),
]

test.skip(process.platform !== 'linux', "Baselines are made and compared on CI's Linux runner")

for (const size of SIZES) {
  test.describe(`${size.name}`, () => {
    test.use({ viewport: { width: size.width, height: size.height } })

    for (const name of KEY_STATES) {
      const state = STATES.find((s) => s.name === name)
      if (!state) throw new Error(`no state named ${name}`)
      test(name, async ({ page, open }) => {
        await open(state.config, { clock: state.clock })
        await state.steps(page)
        await page.mouse.move(0, 0)
        await settle(page)
        await expect(page).toHaveScreenshot(`${size.name}-${name}.png`)
      })
    }
  })
}
