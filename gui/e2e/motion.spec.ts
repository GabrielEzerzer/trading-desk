// The motion invariants: no animated layout properties, no `transition: all`, no infinite animation
// on content, and reduced motion honoured. Canvas and requestAnimationFrame motion (the count-up of
// a fresh result) does not show in getAnimations(), so it is not tested here.
import { STATES, settle } from '../app/shots/harness.mjs'
import { LAYOUT_PROPERTY_SOURCE, infiniteAudit, motionAudit, reducedMotionAudit } from './audit'
import { expect, test } from './fixtures'

test.use({ viewport: { width: 1100, height: 720 } })

const MOTION_STATES = ['engine-starting', 'lab-list', 'lab-strategy-menu', 'lab-running', 'lab-result-trade', 'lab-engine-cannot-run']

for (const name of MOTION_STATES) {
  const state = STATES.find((s) => s.name === name)
  if (!state) throw new Error(`no state named ${name}`)
  test(`${name}: no layout animation, no transition all, no infinite motion on content`, async ({ page, open }) => {
    await open(state.config, { clock: state.clock })
    await state.steps(page)
    // Read while the state's own animations still run, then again once they settle.
    const during = await page.evaluate(motionAudit, LAYOUT_PROPERTY_SOURCE)
    await settle(page)
    const after = await page.evaluate(motionAudit, LAYOUT_PROPERTY_SOURCE)
    expect.soft(during, JSON.stringify(during, null, 1)).toEqual({ layoutTransitions: [], transitionAll: [], layoutKeyframes: [] })
    expect.soft(after, JSON.stringify(after, null, 1)).toEqual({ layoutTransitions: [], transitionAll: [], layoutKeyframes: [] })
    const infinite = await page.evaluate(infiniteAudit)
    expect.soft(infinite, JSON.stringify(infinite, null, 1)).toEqual([])
  })
}

test('reduced motion: nothing moves or loops through the main flow', async ({ page, open }) => {
  await page.emulateMedia({ reducedMotion: 'reduce' })
  const findings: unknown[] = []
  const check = async (step: string) => {
    const found = await page.evaluate(reducedMotionAudit)
    if (found.length) findings.push({ step, found })
  }

  await open({ engine: 'running', mock: true })
  await page.getByRole('heading', { name: 'Strategies' }).waitFor()
  await check('strategy list')
  await page.keyboard.press('Control+2')
  await check('switch to Trading')
  await page.keyboard.press('Control+1')
  await page.locator('.pill').click()
  await page.getByRole('menu').waitFor()
  await check('strategy menu open')
  await page.getByRole('menuitemradio', { name: /Donchian breakout/ }).click()
  await page.getByText('Not run', { exact: true }).waitFor()
  await check('strategy chosen')
  await page.getByRole('button', { name: 'Run', exact: true }).click()
  await page.getByText(/^Step 2 of 4/).first().waitFor()
  await check('running')
  await page.getByRole('heading', { name: 'Trade', exact: true }).waitFor({ timeout: 15_000 })
  await check('result')

  expect(findings, JSON.stringify(findings, null, 1)).toEqual([])
})

test('reduced motion: the engine-starting cue stands still', async ({ page, open }) => {
  await page.emulateMedia({ reducedMotion: 'reduce' })
  await open({ engine: 'starting', mock: true })
  await page.getByText('Starting the engine').waitFor()
  const found = await page.evaluate(reducedMotionAudit)
  expect(found, JSON.stringify(found, null, 1)).toEqual([])
})
