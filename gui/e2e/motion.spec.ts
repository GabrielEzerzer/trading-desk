// The motion invariants, for the Lab and Trading: no animated layout properties, no `transition:
// all`, no infinite animation on content, and reduced motion honoured. Canvas and
// requestAnimationFrame motion (the count-up of a fresh result, the live chart) does not show in
// getAnimations(), so it is not tested here.
import { STATES, settle } from '../app/shots/harness.mjs'
import { LAYOUT_PROPERTY_SOURCE, infiniteAudit, motionAudit, reducedMotionAudit } from './audit'
import { expect, test } from './fixtures'

test.use({ viewport: { width: 1100, height: 720 } })

const MOTION_STATES = [
  'engine-starting',
  'lab-list',
  'lab-strategy-menu',
  'lab-running',
  'lab-result-trade',
  'lab-engine-cannot-run',
  'trading',
  'trading-instruments-loading',
  'trading-ticker-menu',
  'trading-start-session',
  'trading-engine-start',
  'trading-session-running',
  'trading-position-panel',
  'trading-dropped',
]

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

test('reduced motion: nothing moves or loops through Trading', async ({ page, open }) => {
  await page.emulateMedia({ reducedMotion: 'reduce' })
  const findings: unknown[] = []
  const check = async (step: string) => {
    const found = await page.evaluate(reducedMotionAudit)
    if (found.length) findings.push({ step, found })
  }

  await open({ engine: 'running', mock: true })
  await page.getByRole('heading', { name: 'Strategies' }).waitFor()
  await page.keyboard.press('Control+2')
  await page.getByText('Start a paper session to place orders.').waitFor()
  await check('Trading ready')
  await page.keyboard.press('Control+k')
  await page.getByRole('listbox', { name: 'Instruments' }).waitFor()
  await check('ticker menu open')
  await page.keyboard.press('Escape')
  await page.getByRole('listbox').waitFor({ state: 'detached' })
  await page.getByRole('button', { name: 'Timeframe: 1m' }).click()
  await page.getByRole('menu').waitFor()
  await check('timeframe menu open')
  await page.keyboard.press('Escape')
  await page.getByRole('menu').waitFor({ state: 'detached' })
  await page.getByRole('button', { name: 'Start a session' }).click()
  await page.getByRole('dialog').waitFor()
  await check('start panel open')
  await page.getByRole('button', { name: 'Start', exact: true }).click()
  await page.locator('.position-summary').waitFor()
  await page.getByPlaceholder('Size').fill('0.05')
  await page.getByRole('button', { name: 'Buy', exact: true }).click()
  await page.locator('.position-summary').filter({ hasText: /0\.05000 BTC/ }).waitFor({ timeout: 10_000 })
  await check('order filled')
  await page.locator('.position-summary').click()
  await page.getByRole('dialog').waitFor()
  await check('session panel open')

  expect(findings, JSON.stringify(findings, null, 1)).toEqual([])
})

test('the live price tint swaps colour only: no transition, no animation, no layout change', async ({ page, open }) => {
  await open({ engine: 'running', mock: true })
  await page.getByRole('heading', { name: 'Strategies' }).waitFor()
  await page.keyboard.press('Control+2')
  // The mock closes a bar every 1.5 s; the tint shows for 700 ms after the price changes.
  const price = page.locator('.ticker-price[data-tint]')
  await price.waitFor({ timeout: 10_000 })
  const motion = await page.evaluate(motionAudit, LAYOUT_PROPERTY_SOURCE)
  const report = await price.evaluate((el) => {
    const layout = ['width', 'height', 'padding', 'margin', 'font-size', 'font-weight', 'letter-spacing', 'line-height']
    const read = () => {
      const s = getComputedStyle(el)
      return Object.fromEntries(layout.map((p) => [p, s.getPropertyValue(p)]))
    }
    const tinted = read()
    const tint = el.getAttribute('data-tint')
    const durations = getComputedStyle(el).transitionDuration
    const animations = el.getAnimations().length
    el.removeAttribute('data-tint')
    const plain = read()
    if (tint) el.setAttribute('data-tint', tint)
    return { tint, durations, animations, sameLayout: JSON.stringify(tinted) === JSON.stringify(plain), tinted, plain }
  })
  expect(motion, JSON.stringify(motion, null, 1)).toEqual({ layoutTransitions: [], transitionAll: [], layoutKeyframes: [] })
  expect(report.tint).toMatch(/^(gain|loss)$/)
  expect(report.durations.split(',').every((d) => parseFloat(d) === 0), `transition-duration ${report.durations}`).toBe(true)
  expect(report.animations).toBe(0)
  expect(report.sameLayout, JSON.stringify(report, null, 1)).toBe(true)
})
