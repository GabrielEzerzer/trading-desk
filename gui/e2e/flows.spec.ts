// The key flows, driven with real clicks and keys, each asserting the state the page must reach.
import type { Page } from '@playwright/test'
import { engineTrading } from '../app/shots/harness.mjs'
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

test.describe('trading', () => {
  // Real time on the mock source: a closed bar every 1.5 s, and a market order fills at the next
  // bar's open, so the waits for a bar allow a few of them.
  const BARS = { timeout: 10_000 }

  const openTrading = async (page: Page, open: (config: typeof lab) => Promise<void>) => {
    await open(lab)
    await page.getByRole('heading', { name: 'Strategies' }).waitFor()
    await page.keyboard.press('Control+2')
    await expect(page.locator('main')).toHaveAttribute('aria-label', 'Trading')
    await expect(page.getByText('Start a paper session to place orders.')).toBeVisible()
    await expect(page.locator('.ticker-price')).toHaveText(/^\d{2},\d{3}\.\d{2}$/)
  }

  const startManual = async (page: Page) => {
    await page.getByRole('button', { name: 'Start a session' }).click()
    const panel = page.getByRole('dialog')
    await expect(panel.getByText('Start a paper session', { exact: true })).toBeVisible()
    await expect(panel.getByRole('button', { name: 'Strategy: Manual' })).toBeVisible()
    await panel.getByRole('button', { name: 'Start', exact: true }).click()
    await expect(panel).toBeHidden()
    await expect(page.locator('.position-summary')).toContainText('Manual · Warming up')
  }

  test('switches instrument from the menu and with Ctrl+K search', async ({ page, open }) => {
    await openTrading(page, open)
    const ticker = page.locator('.ticker')

    await ticker.click()
    await page.getByRole('listbox', { name: 'Instruments' }).waitFor()
    await page.getByRole('option', { name: /^ETHUSDT\s*Binance spot/ }).first().click()
    await expect(page.getByRole('listbox')).toBeHidden()
    await expect(ticker).toHaveAccessibleName(/^ETHUSDT, Binance spot\./)
    await expect(page.locator('.ticker-price')).toHaveText(/^\d,\d{3}\.\d{2}$/)
    await expect(page.getByText('No paper session on ETHUSDT')).toBeVisible()

    await page.keyboard.press('Control+k')
    const search = page.getByRole('combobox', { name: 'Search instruments' })
    await expect(search).toBeFocused()
    await search.fill('sol')
    await expect(page.getByRole('option', { name: /^SOLUSDT\s*Binance spot/, selected: true })).toBeVisible()
    await page.keyboard.press('ArrowDown')
    await expect(page.getByRole('option', { name: /^SOLUSDT\s*Binance perpetual/, selected: true })).toBeVisible()
    await page.keyboard.press('Enter')
    await expect(page.getByRole('listbox')).toBeHidden()
    await expect(ticker).toHaveAccessibleName(/^SOLUSDT, Binance perpetual\./)
    await expect(page.getByText('No paper session on SOLUSDT')).toBeVisible()

    // The latest choices come first next time.
    await page.keyboard.press('Control+k')
    const options = page.getByRole('option')
    await expect(options.nth(0)).toHaveAccessibleName(/^SOLUSDT\s*Binance perpetual/)
    await expect(options.nth(1)).toHaveAccessibleName(/^ETHUSDT\s*Binance spot/)
    await page.keyboard.press('Escape')
    await expect(page.getByRole('listbox')).toBeHidden()
  })

  test('changes the timeframe, and a new session runs on it', async ({ page, open }) => {
    await openTrading(page, open)
    await page.getByRole('button', { name: 'Timeframe: 1m' }).click()
    await expect(page.getByRole('menuitemradio', { name: '1m' })).toHaveAttribute('aria-checked', 'true')
    await page.getByRole('menuitemradio', { name: '15m' }).click()
    await expect(page.getByRole('menu')).toBeHidden()
    await expect(page.getByRole('button', { name: 'Timeframe: 15m' })).toBeVisible()
    await page.getByRole('button', { name: 'Start a session' }).click()
    await expect(page.getByRole('dialog')).toContainText('Runs on BTCUSDT 15m bars from now on.')
  })

  test('a session: start, a market order sent and filled, the panel, stop', async ({ page, open }) => {
    await openTrading(page, open)
    await startManual(page)
    const summary = page.locator('.position-summary')
    await expect(summary).toContainText('Manual · Live', BARS)

    await page.getByPlaceholder('Size').fill('0.05')
    await page.getByRole('button', { name: 'Buy', exact: true }).click()
    await expect(page.getByRole('status').filter({ hasText: 'Buy 0.05000 BTC at market sent.' })).toBeVisible()
    await expect(summary).toContainText('0.05000 BTC', BARS)

    await summary.click()
    const panel = page.getByRole('dialog')
    await expect(panel).toContainText('Manual on BTCUSDT · 1m')
    await expect(panel).toContainText(/Live since \d\d:\d\d UTC/)
    await expect(panel.getByRole('tab', { name: /Open orders/ })).toHaveAttribute('aria-selected', 'true')
    await expect(panel.getByText('No open orders.')).toBeVisible()
    await panel.getByRole('tab', { name: /Fills/ }).click()
    await expect(panel.getByRole('tab', { name: /Fills/ })).toHaveAttribute('aria-selected', 'true')
    await expect(panel.getByRole('row')).toHaveCount(2)
    await expect(panel.getByRole('row').nth(1)).toContainText('Buy')
    await expect(panel.getByRole('row').nth(1)).toContainText('0.05000')
    await panel.getByRole('tab', { name: /Trades/ }).click()
    await expect(panel.getByRole('row').nth(1)).toContainText('Long · open')

    await panel.getByRole('button', { name: 'Stop', exact: true }).click()
    await expect(summary).toContainText('Manual · Stopped')
    await expect(panel.getByRole('button', { name: 'Resume' })).toBeVisible()
    await page.keyboard.press('Escape')
    await expect(panel).toBeHidden()
    await expect(page.getByText('The paper session is stopped. Resume it to place orders.')).toBeVisible()
    await expect(page.getByRole('button', { name: 'Buy', exact: true })).toHaveAttribute('aria-disabled', 'true')
  })

  test('an invalid size says why and sends nothing', async ({ page, open }) => {
    await openTrading(page, open)
    await startManual(page)
    const size = page.getByPlaceholder('Size')
    await size.fill('0.00005')
    await expect(page.getByText(/^The order value is [\d.]+ USDT; the venue's minimum is 5\.00 USDT\.$/)).toBeVisible()
    await expect(size).toHaveAttribute('aria-invalid', 'true')
    const buy = page.getByRole('button', { name: 'Buy', exact: true })
    await expect(buy).toHaveAttribute('aria-disabled', 'true')
    // aria-disabled keeps Buy focusable; Playwright counts it as disabled, so the click is forced.
    await buy.click({ force: true })
    await expect(page.getByText(/ sent\.$/)).toHaveCount(0)
    await size.fill('abc')
    await expect(page.getByText('Enter the size as a number, for example 0.5.')).toBeVisible()
    await size.fill('0.05')
    await expect(size).not.toHaveAttribute('aria-invalid', 'true')
    await expect(buy).toHaveAttribute('aria-disabled', 'false')
  })

  test('a limit order is sent and rests in the open orders', async ({ page, open }) => {
    await openTrading(page, open)
    await startManual(page)
    await page.getByPlaceholder('Size').fill('0.05')
    await page.getByRole('button', { name: 'Order type: Market' }).click()
    await page.getByRole('menuitemradio', { name: 'Limit' }).click()
    await expect(page.getByRole('button', { name: 'Order type: Limit' })).toBeVisible()
    const price = page.getByPlaceholder('Price')
    // Choosing Limit starts the price at the last close.
    await expect(price).toHaveValue(/^\d{5}\.\d{2}$/)
    // Far under the market, so it rests for the length of the test.
    await price.fill('60000')
    await page.getByRole('button', { name: 'Buy', exact: true }).click()
    await expect(page.getByRole('status').filter({ hasText: 'Buy 0.05000 BTC at 60,000.00 sent.' })).toBeVisible()

    await page.locator('.position-summary').click()
    const panel = page.getByRole('dialog')
    await expect(panel.getByRole('tab', { name: /Open orders/ })).toContainText('1')
    const row = panel.getByRole('row').nth(1)
    await expect(row).toContainText('Buy')
    await expect(row).toContainText('Limit')
    await expect(row).toContainText('60,000.00')
    await expect(page.locator('.position-summary')).toContainText('Flat')
  })

  test('a rejected order says why', async ({ page, open }) => {
    await openTrading(page, open)
    await startManual(page)
    await page.getByPlaceholder('Size').fill('0.05')
    await page.getByRole('button', { name: 'Sell', exact: true }).click()
    await expect(page.getByRole('alert')).toHaveText(/^Order rejected: Spot sessions cannot sell more than they hold \(0 BTC\)/)
    // The rejection shows until the next edit.
    await page.getByPlaceholder('Size').fill('0.04')
    await expect(page.getByRole('alert')).toHaveCount(0)
  })

  test('Ctrl+1 takes the chart out of the page and Ctrl+2 brings it back with the session', async ({ page, open }) => {
    await openTrading(page, open)
    await startManual(page)
    await expect(page.locator('.trading canvas').first()).toBeVisible()

    await page.keyboard.press('Control+1')
    await expect(page.locator('main')).toHaveAttribute('aria-label', 'Strategy Lab')
    await expect(page.getByRole('heading', { name: 'Strategies' })).toBeVisible()
    await expect(page.locator('.trading')).toHaveCount(0)
    await expect(page.locator('.chart-canvas')).toHaveCount(0)
    await expect(page.locator('canvas')).toHaveCount(0)

    await page.keyboard.press('Control+2')
    await expect(page.locator('main')).toHaveAttribute('aria-label', 'Trading')
    await expect(page.locator('.trading canvas').first()).toBeVisible()
    await expect(page.getByRole('heading', { name: 'Strategies' })).toBeHidden()
    // The paper session kept running while the Lab was shown.
    await expect(page.locator('.position-summary')).toContainText(/Manual · (Warming up|Live)/)
  })
})

test.describe('trading on the engine', () => {
  const openTrading = async (page: Page, open: (config: typeof engineTrading) => Promise<void>, config: typeof engineTrading) => {
    await open(config)
    await page.getByRole('heading', { name: 'Strategies' }).waitFor()
    await page.keyboard.press('Control+2')
    await expect(page.locator('main')).toHaveAttribute('aria-label', 'Trading')
  }

  test('a double click on Start opens one session', async ({ page, open }) => {
    await openTrading(page, open, engineTrading)
    await page.getByRole('button', { name: 'Start a session' }).click()
    const panel = page.getByRole('dialog')
    const start = panel.getByRole('button', { name: 'Start', exact: true })
    await expect(start).toHaveAttribute('aria-disabled', 'false')
    // The stub answers 300 ms later, so the second click lands while the first start is pending.
    await start.dblclick()
    await expect(panel).toBeHidden()
    await expect(page.locator('.position-summary')).toContainText('Warming up')
    expect(await page.evaluate(() => (window as { __stubStarts?: number }).__stubStarts)).toBe(1)
  })

  test('a session list that did not load says so with Retry, and offers no new session', async ({ page, open }) => {
    await openTrading(page, open, { ...engineTrading, failSessions: 1 })
    const line = page.locator('.position-line')
    await expect(line.getByRole('alert')).toHaveText('The paper sessions did not load. The session store did not answer.')
    await expect(page.getByText(/^No paper session/)).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'Start a session' })).toHaveCount(0)
    await expect(page.getByText('Start a paper session to place orders.')).toHaveCount(0)
    await line.getByRole('button', { name: 'Retry' }).click()
    await expect(page.getByText('No paper session on BTCUSDT')).toBeVisible()
    await expect(line.getByRole('alert')).toHaveCount(0)
  })
})
