// The key flows, driven with real clicks and keys, each asserting the state the page must reach.
import type { Page } from '@playwright/test'
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

// The engine source, on the bridge stub's fake engine (app/shots/bridge-stub.js, app/mock/engine.json).
test.describe('running on the engine', () => {
  const engine = { engine: 'running', mock: false } as const
  type Call = { method: string; path: string; body?: Record<string, unknown> }
  const calls = (page: Page) => page.evaluate(() => (window as unknown as { __tdCalls: Call[] }).__tdCalls)
  const cancelSent = async (page: Page) => (await calls(page)).some((c) => c.method === 'POST' && /^\/jobs\/\d+\/cancel$/.test(c.path))

  test('the list shows the latest run of each strategy, and a past result opens settled', async ({ page, open }) => {
    await open(engine)
    await expect(page.getByRole('button', { name: /SMA cross/ })).toContainText('9 Nov 2026 · +20.5%')
    await expect(page.getByRole('button', { name: /Donchian breakout/ })).toContainText('Not run')
    await page.getByRole('button', { name: /SMA cross/ }).click()
    // Read the return in the frame the result first shows: a count-up would start near zero.
    const first = await page.waitForFunction(() => document.getElementById('verdict') && document.querySelector('.key-number dd')?.textContent)
    expect(await first.jsonValue()).toBe('+20.5%')
    await expect(page.locator('#verdict')).toHaveText('No verdict rule yet')
    await expect(page.locator('.criteria')).toHaveCount(0)
    expect((await calls(page)).some((c) => c.path === '/runs/1794242520000-9e4b7a10')).toBe(true)
  })

  test('metrics the engine could not compute show a dash in the ink', async ({ page, open }) => {
    await open(engine)
    await page.getByRole('button', { name: /RSI reversion/ }).click()
    const cagr = page.locator('.key-number').filter({ hasText: 'CAGR' }).locator('dd')
    await expect(cagr).toHaveText('–')
    await expect(cagr).not.toHaveAttribute('data-tone')
    await expect(page.getByRole('row', { name: /Sharpe/ })).toContainText('–')
  })

  test('Run sends one backtest on the defaults over the chosen data, then shows the result', async ({ page, open }) => {
    await open(engine)
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await page.getByRole('button', { name: 'Overrides' }).click()
    await expect(page.getByRole('menuitemradio', { name: 'BTCUSDT perpetual, Binance' })).toHaveAttribute('aria-checked', 'true')
    await page.getByRole('menuitemradio', { name: '1d' }).click()
    await page.getByRole('button', { name: 'Run', exact: true }).click()
    await expect(page.getByText('Step 1 of 1 · Running the backtest')).toBeVisible()
    await expect(page.locator('#verdict')).toHaveText('No verdict rule yet', { timeout: 10_000 })

    const sent = (await calls(page)).find((c) => c.method === 'POST' && c.path === '/runs/backtest')
    // The whole stored range of BTCUSDT perpetual (2024-01-01 to 2025-07-01), in whole days.
    expect(sent?.body).toMatchObject({
      strategy: 'donchian_breakout',
      params: { channel: 20 },
      instrument: { id: { venue: 'binance', symbol: 'BTCUSDT', kind: 'perpetual' } },
      timeframe: '1d',
      from: Date.UTC(2024, 0, 1),
      to: Date.UTC(2025, 6, 1),
    })
    await page.locator('.pill').click()
    await page.getByRole('menuitem', { name: 'All strategies' }).click()
    await expect(page.getByRole('button', { name: /Donchian breakout/ })).toContainText('10 Nov 2026 ·')
  })

  test('Cancel asks the engine to stop the job', async ({ page, open }) => {
    await open(engine)
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await page.getByRole('button', { name: 'Run', exact: true }).click()
    await expect(page.getByText(/^Step 1 of 1/)).toBeVisible()
    await page.getByRole('button', { name: 'Cancel the run' }).click()
    await expect(page.getByText('Not run', { exact: true })).toBeVisible()
    await expect(page.getByRole('alert')).toHaveCount(0)
    await expect.poll(() => cancelSent(page)).toBe(true)
  })

  test('a run refused for missing data offers the download, then runs', async ({ page, open }) => {
    await open({ ...engine, backtest: 'data_missing' })
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await page.getByRole('button', { name: 'Run', exact: true }).click()
    await expect(page.getByText('No data downloaded')).toBeVisible()
    await expect(page.getByText(/store has no candles for binance:BTCUSDT:perpetual/i)).toBeVisible()
    await page.getByRole('button', { name: 'Download BTCUSDT history' }).click()
    await expect(page.getByRole('progressbar', { name: 'Download progress' })).toBeVisible()
    await expect(page.getByRole('button', { name: 'Run', exact: true })).toHaveAttribute('aria-disabled', 'true')
    await expect(page.getByText('Not run', { exact: true })).toBeVisible({ timeout: 10_000 })
    const download = (await calls(page)).find((c) => c.path === '/data/download')
    expect(download?.body).toMatchObject({
      instrument: { venue: 'binance', symbol: 'BTCUSDT', kind: 'perpetual' },
      from: Date.UTC(2017, 0, 1),
    })

    await page.getByRole('button', { name: 'Run', exact: true }).click()
    await expect(page.locator('#verdict')).toHaveText('No verdict rule yet', { timeout: 10_000 })
  })

  test('with nothing stored, Run waits for a download of the default instrument', async ({ page, open }) => {
    await open({ ...engine, noData: true })
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await expect(page.getByText('No data downloaded')).toBeVisible()
    const run = page.getByRole('button', { name: 'Run', exact: true })
    await expect(run).toHaveAttribute('aria-disabled', 'true')
    await run.hover()
    await expect(page.getByRole('tooltip')).toHaveText('Download data first.')
    await page.mouse.move(0, 0)
    await page.getByRole('button', { name: 'Download BTCUSDT history' }).click()
    await expect(page.getByText('Not run', { exact: true })).toBeVisible({ timeout: 10_000 })
    await expect(run).not.toHaveAttribute('aria-disabled', 'true')
    await page.getByRole('button', { name: 'Overrides' }).click()
    await expect(page.getByRole('menuitemradio', { name: 'BTCUSDT spot, Binance' })).toHaveAttribute('aria-checked', 'true')
  })

  test('a download can be cancelled', async ({ page, open }) => {
    await open({ ...engine, noData: true })
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await page.getByRole('button', { name: 'Download BTCUSDT history' }).click()
    await page.getByRole('button', { name: 'Cancel the download' }).click()
    await expect(page.getByRole('button', { name: 'Download BTCUSDT history' })).toBeVisible()
    await expect(page.getByRole('alert')).toHaveCount(0)
    await expect.poll(() => cancelSent(page)).toBe(true)
  })

  test('a busy engine says so in one line where the verdict goes', async ({ page, open }) => {
    await open({ ...engine, backtest: 'too_many_runs' })
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await page.getByRole('button', { name: 'Run', exact: true }).click()
    await expect(page.getByText('Another run is in progress. Try again when it finishes.')).toBeVisible()
    await expect(page.getByRole('alert')).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'Run', exact: true })).not.toHaveAttribute('aria-disabled', 'true')
  })

  test('a job that fails shows the message from the server', async ({ page, open }) => {
    await open({ ...engine, backtest: 'fails' })
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await page.getByRole('button', { name: 'Run', exact: true }).click()
    await expect(page.getByRole('alert')).toContainText('Run failed', { timeout: 10_000 })
    await expect(page.getByRole('alert')).toContainText(/strategy refused its parameters: slow must be above fast/i)
  })
})
