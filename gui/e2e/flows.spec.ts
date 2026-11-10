// The key flows, driven with real clicks and keys, each asserting the state the page must reach.
import { expect, test } from './fixtures'

test.use({ viewport: { width: 1100, height: 720 } })

const lab = { engine: 'running', mock: true } as const

test.describe('section toggle', () => {
  test('switches by click', async ({ page, open }) => {
    await open(lab)
    await page.getByRole('radio', { name: 'Trading' }).click()
    await expect(page.getByRole('radio', { name: 'Trading' })).toHaveAttribute('aria-checked', 'true')
    await expect(page.locator('main')).toHaveAttribute('aria-label', 'Trading')
    await page.getByRole('radio', { name: 'Strategy Lab' }).click()
    await expect(page.locator('main')).toHaveAttribute('aria-label', 'Strategy Lab')
    await expect(page.getByRole('heading', { name: 'Strategies' })).toBeVisible()
  })

  test('switches with Ctrl+2 and Ctrl+1', async ({ page, open }) => {
    await open(lab)
    await page.getByRole('heading', { name: 'Strategies' }).waitFor()
    await page.keyboard.press('Control+2')
    await expect(page.locator('main')).toHaveAttribute('aria-label', 'Trading')
    await expect(page.getByRole('heading', { name: 'Strategies' })).toBeHidden()
    await page.keyboard.press('Control+1')
    await expect(page.locator('main')).toHaveAttribute('aria-label', 'Strategy Lab')
    await expect(page.getByRole('heading', { name: 'Strategies' })).toBeVisible()
  })

  test('moves with the arrow keys, focus following the choice', async ({ page, open }) => {
    await open(lab)
    await page.getByRole('radio', { name: 'Strategy Lab' }).focus()
    await page.keyboard.press('ArrowRight')
    await expect(page.getByRole('radio', { name: 'Trading' })).toBeFocused()
    await expect(page.getByRole('radio', { name: 'Trading' })).toHaveAttribute('aria-checked', 'true')
    await expect(page.locator('main')).toHaveAttribute('aria-label', 'Trading')
    await page.keyboard.press('ArrowLeft')
    await expect(page.getByRole('radio', { name: 'Strategy Lab' })).toBeFocused()
    await expect(page.locator('main')).toHaveAttribute('aria-label', 'Strategy Lab')
  })
})

test.describe('choosing a strategy', () => {
  test('from the list shows its result and moves focus to the pill', async ({ page, open }) => {
    await open(lab)
    await page.getByRole('button', { name: /SMA cross/ }).click()
    await expect(page.getByRole('heading', { name: 'Trade', exact: true })).toBeVisible()
    await expect(page.locator('.pill')).toHaveText('SMA cross')
    await expect(page.locator('.pill')).toBeFocused()
  })

  test('from the menu, then back to all strategies', async ({ page, open }) => {
    await open(lab)
    await page.getByRole('heading', { name: 'Strategies' }).waitFor()
    await page.locator('.pill').click()
    await page.getByRole('menuitemradio', { name: /RSI reversion/ }).click()
    await expect(page.getByRole('menu')).toBeHidden()
    await expect(page.getByRole('heading', { name: 'No trade', exact: true })).toBeVisible()
    await expect(page.locator('.pill')).toHaveText('RSI reversion')

    await page.locator('.pill').click()
    await expect(page.getByRole('menuitemradio', { name: /RSI reversion/ })).toHaveAttribute('aria-checked', 'true')
    await page.getByRole('menuitem', { name: 'All strategies' }).click()
    await expect(page.getByRole('heading', { name: 'Strategies' })).toBeVisible()
    await expect(page.locator('.pill')).toHaveText('Choose a strategy')
  })
})

test.describe('running', () => {
  test('Run goes through its steps to the result, label and bar agreeing', async ({ page, open }) => {
    await open(lab)
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await expect(page.getByText('Not run', { exact: true })).toBeVisible()
    await page.getByRole('button', { name: 'Run', exact: true }).click()

    await expect(page.getByRole('progressbar', { name: 'Run progress' })).toBeVisible()
    await expect(page.getByRole('button', { name: 'Cancel the run' })).toBeVisible()
    await expect(page.locator('.pill')).toBeDisabled()
    // Read the label and the bar in one go, so both come from the same render.
    const at = await page.waitForFunction(() => {
      const label = document.querySelector('.run-status')?.textContent
      if (label !== 'Step 2 of 4 · Sweeping parameters, 48 of 48') return false
      return document.querySelector('[role=progressbar]')?.getAttribute('aria-valuenow')
    })
    expect(await at.jsonValue()).toBe('50')

    await expect(page.getByRole('heading', { name: 'Trade', exact: true })).toBeVisible({ timeout: 15_000 })
    await expect(page.getByRole('progressbar')).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'Run', exact: true })).toBeVisible()
  })

  test('a failed run says why', async ({ page, open }) => {
    await open(lab)
    await page.getByRole('button', { name: /Funding carry/ }).click()
    await page.getByRole('button', { name: 'Run', exact: true }).click()
    await expect(page.getByRole('alert')).toContainText('Run failed', { timeout: 10_000 })
    await expect(page.getByRole('alert')).toContainText('No funding rates are stored for ETHUSDT perpetual on Bybit')
    await expect(page.getByRole('button', { name: 'Run', exact: true })).toBeVisible()
  })

  test('Cancel stops the run and returns to the strategy', async ({ page, open }) => {
    await open(lab)
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await page.getByRole('button', { name: 'Run', exact: true }).click()
    await expect(page.getByText(/^Step 1 of 4/)).toBeVisible()
    await page.getByRole('button', { name: 'Cancel the run' }).click()
    await expect(page.getByRole('progressbar')).toHaveCount(0)
    await expect(page.getByText('Not run', { exact: true })).toBeVisible()
    await expect(page.getByRole('button', { name: 'Run', exact: true })).toBeVisible()
    await expect(page.getByRole('alert')).toHaveCount(0)
  })

  test('Run without a strategy does nothing and says why', async ({ page, open }) => {
    await open(lab)
    await page.getByRole('heading', { name: 'Strategies' }).waitFor()
    const run = page.getByRole('button', { name: 'Run', exact: true })
    await expect(run).toHaveAttribute('aria-disabled', 'true')
    // aria-disabled keeps Run focusable and clickable, so its tooltip can say why; Playwright counts
    // it as disabled, so the click is forced.
    await run.click({ force: true })
    await expect(page.getByRole('progressbar')).toHaveCount(0)
    // Radix closes a tooltip on press until the pointer leaves, as any tooltip does.
    await page.mouse.move(0, 0)
    await run.hover()
    await expect(page.getByRole('tooltip')).toHaveText('Choose a strategy first.')
  })
})

test('a failed engine starts again with Retry', async ({ page, open }) => {
  await open({ engine: 'failed', mock: true })
  await expect(page.getByRole('alert')).toContainText('The engine failed')
  await page.getByRole('button', { name: 'Retry' }).click()
  await expect(page.getByText('Starting the engine')).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Strategies' })).toBeVisible()
  await expect(page.locator('.engine-dot')).toHaveCount(0)
})
