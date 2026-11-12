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
    await expect(page.getByText('Run it to backtest it on the chosen data.')).toBeVisible()
    await page.getByRole('button', { name: 'Overrides' }).click()
    await expect(page.getByRole('menuitemradio', { name: 'BTCUSDT perpetual, Binance' })).toHaveAttribute('aria-checked', 'true')
    await page.getByRole('menuitemradio', { name: '1d' }).click()
    await page.getByRole('button', { name: 'Run', exact: true }).click()
    await expect(page.getByText('Step 1 of 1 · Running the backtest')).toBeVisible()
    await expect(page.locator('#verdict')).toHaveText('No verdict rule yet', { timeout: 10_000 })

    const sent = (await calls(page)).filter((c) => c.method === 'POST' && c.path === '/runs/backtest')
    expect(sent).toHaveLength(1)
    // The whole stored range of BTCUSDT perpetual (2024-01-01 to 2025-07-01), in whole days.
    expect(sent[0]?.body).toMatchObject({
      strategy: 'donchian_breakout',
      params: { channel: 20 },
      instrument: { id: { venue: 'binance', symbol: 'BTCUSDT', kind: 'perpetual' } },
      timeframe: '1d',
      from: Date.UTC(2024, 0, 1),
      to: Date.UTC(2025, 6, 1),
    })
    // The run's last trade is still open at the end: no close date.
    const lastTrade = page.locator('.table-trades tbody tr').last()
    await expect(lastTrade.locator('td').nth(3)).toHaveText('Open')
    await expect(page.locator('.table-trades tbody tr').first().locator('td').nth(3)).not.toHaveText('Open')
    await page.locator('.pill').click()
    await page.getByRole('menuitem', { name: 'All strategies' }).click()
    await expect(page.getByRole('button', { name: /Donchian breakout/ })).toContainText('10 Nov 2026 ·')
  })

  test('Cancel asks the engine to stop the job', async ({ page, open }) => {
    await open(engine)
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await page.getByRole('button', { name: 'Run', exact: true }).click()
    // Cancel before "Saving the result": past that, the job saves the run anyway.
    await expect(page.getByText(/^Step 1 of 1 · (Loading the data|Running the backtest)$/)).toBeVisible()
    await page.getByRole('button', { name: 'Cancel the run' }).click()
    await expect(page.getByText('Not run', { exact: true })).toBeVisible()
    await expect(page.getByRole('alert')).toHaveCount(0)
    await expect.poll(() => cancelSent(page)).toBe(true)
    // A job that ignored the cancel would show its result within its three 700 ms steps.
    await page.waitForTimeout(3 * 700 + 300)
    await expect(page.locator('#verdict')).toHaveCount(0)
    await expect(page.getByText('Not run', { exact: true })).toBeVisible()
  })

  test('a cancel that lands after the last checkpoint still shows the saved result', async ({ page, open }) => {
    // Slow steps, so the job can't finish between finding Cancel and clicking it: Cancel and Run
    // are one button, and a late click would start a second run instead.
    await open({ ...engine, stepMs: 2_500 })
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await page.getByRole('button', { name: 'Run', exact: true }).click()
    // The stub, like the engine, saves the run once its last step has started, cancel or not.
    await expect(page.getByText('Step 1 of 1 · Saving the result')).toBeVisible({ timeout: 10_000 })
    await page.getByRole('button', { name: 'Cancel the run' }).click()
    await expect(page.getByRole('progressbar')).toHaveCount(0)
    await expect(page.locator('#verdict')).toHaveText('No verdict rule yet', { timeout: 15_000 })
    await expect(page.getByRole('alert')).toHaveCount(0)
    expect(await cancelSent(page)).toBe(true)
  })

  test('a run refused for missing data offers the download, then runs', async ({ page, open }) => {
    await open({ ...engine, backtest: 'data_missing' })
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await page.getByRole('button', { name: 'Run', exact: true }).click()
    await expect(page.getByText('Data missing', { exact: true })).toBeVisible()
    await expect(page.getByText('No data downloaded')).toHaveCount(0)
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

  test('with nothing stored, a strategy with a saved run still offers the download', async ({ page, open }) => {
    await open({ ...engine, noData: true })
    await page.getByRole('button', { name: /SMA cross/ }).click()
    await expect(page.getByText('No data downloaded')).toBeVisible()
    await page.getByRole('button', { name: 'Download BTCUSDT history' }).click()
    await expect(page.getByRole('progressbar', { name: 'Download progress' })).toBeVisible()
    // Once the data is stored, the saved result shows again.
    await expect(page.locator('#verdict')).toHaveText('No verdict rule yet', { timeout: 10_000 })
  })

  test('a download can be cancelled', async ({ page, open }) => {
    await open({ ...engine, noData: true })
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await page.getByRole('button', { name: 'Download BTCUSDT history' }).click()
    await page.getByRole('button', { name: 'Cancel the download' }).click()
    // The engine stops the job at its next step; until then the view says so and Cancel is spent.
    await expect(page.getByText('Cancelling the download')).toBeVisible()
    await expect(page.getByRole('button', { name: 'Cancel the download' })).toHaveAttribute('aria-disabled', 'true')
    await expect(page.getByRole('button', { name: 'Download BTCUSDT history' })).toBeVisible()
    await expect(page.getByRole('alert')).toHaveCount(0)
    await expect.poll(() => cancelSent(page)).toBe(true)
  })

  test('a busy engine says so in one line where the verdict goes, in its own words', async ({ page, open }) => {
    await open({ ...engine, backtest: 'too_many_runs' })
    await page.getByRole('button', { name: /Donchian breakout/ }).click()
    await page.getByRole('button', { name: 'Run', exact: true }).click()
    await expect(
      page.getByText('8 runs are already queued or running, the most this server takes; wait for one to finish or cancel one.'),
    ).toBeVisible()
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

  /** A Manual session past its two bars of warm-up, so it takes orders, as the engine's does. */
  const startLive = async (page: Page) => {
    await startManual(page)
    await expect(page.locator('.position-summary')).toContainText('Manual · Live', BARS)
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
    await startLive(page)
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

  test('a refused order says why in the engine’s words', async ({ page, open }) => {
    await openTrading(page, open)
    await startManual(page)
    await page.getByPlaceholder('Size').fill('0.05')
    await page.getByRole('button', { name: 'Sell', exact: true }).click()
    await expect(page.getByRole('alert')).toHaveText(
      'The session has not fed its first live bar yet; orders can be placed once it has.',
    )
    await expect(page.locator('.position-summary')).toContainText('Manual · Live', BARS)
    await page.getByRole('button', { name: 'Sell', exact: true }).click()
    await expect(page.getByRole('alert')).toHaveText('Spot sessions cannot sell more than they hold (0 BTC).')
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

  test('a manual session sends a limit order to the engine, lists it and cancels it', async ({ page, open }) => {
    await openTrading(page, open, engineTrading)
    await page.getByRole('button', { name: 'Start a session' }).click()
    const start = page.getByRole('dialog')
    await expect(start.getByRole('button', { name: 'Strategy: Manual' })).toBeVisible()
    await start.getByRole('button', { name: 'Start', exact: true }).click()
    await expect(start).toBeHidden()
    await expect(page.locator('.position-summary')).toContainText('Manual · Warming up')

    await page.getByPlaceholder('Size').fill('0.05')
    await page.getByRole('button', { name: 'Order type: Market' }).click()
    await page.getByRole('menuitemradio', { name: 'Limit' }).click()
    await page.getByPlaceholder('Price').fill('60000')
    const buy = page.getByRole('button', { name: 'Buy', exact: true })
    // The engine takes no order before the session's first live bar, and says so.
    await buy.click()
    await expect(page.getByRole('alert')).toHaveText(
      'The session has not fed its first live bar yet; orders can be placed once it has.',
    )

    await page.evaluate(() => (window as { __stubGoLive?: () => void }).__stubGoLive?.())
    await expect(page.locator('.position-summary')).toContainText('Manual · Live')
    await buy.click()
    await expect(page.getByRole('status').filter({ hasText: 'Buy 0.05000 BTC at 60,000.00 sent.' })).toBeVisible()

    await page.locator('.position-summary').click()
    const panel = page.getByRole('dialog')
    const row = panel.getByRole('row').nth(1)
    await expect(row).toContainText('Limit')
    await expect(row).toContainText('60,000.00')
    await panel.getByRole('button', { name: 'Cancel the buy order of 0.05000 BTC' }).click()
    await expect(panel.getByText('No open orders.')).toBeVisible()

    const calls = await page.evaluate(
      () => (window as { __tdCalls?: { method: string; path: string; body?: Record<string, unknown> }[] }).__tdCalls ?? [],
    )
    // td-server runs no strategy for "none"; any other name is looked up as a strategy.
    expect(calls.find((c) => c.method === 'POST' && c.path === '/sessions')?.body?.strategy).toBe('none')
    const order = { side: 'buy', qty: 0.05, type: 'limit', limit_price: 60000 }
    expect(calls.filter((c) => c.path.includes('/orders'))).toEqual([
      { method: 'POST', path: '/sessions/stub-1/orders', body: order },
      { method: 'POST', path: '/sessions/stub-1/orders', body: order },
      { method: 'POST', path: '/sessions/stub-1/orders/1/cancel' },
    ])
  })

  const subscribed = (page: Page) => page.evaluate(() => (window as { __stubSubscribed?: string[] }).__stubSubscribed ?? [])
  const candleCalls = (page: Page) =>
    page.evaluate(() =>
      ((window as { __tdCalls?: { path: string; query?: Record<string, string> }[] }).__tdCalls ?? [])
        .filter((c) => c.path === '/candles')
        .map((c) => c.query),
    )

  test('draws the candle history before any live bar', async ({ page, open }) => {
    // No live bars: the stub's stream is off, so whatever the chart shows came from GET /candles.
    await openTrading(page, open, { ...engineTrading, candles: { ...engineTrading.candles!, everyMs: 3_600_000 } })
    await expect(page.locator('.ticker-price')).toHaveText('64,210.00')
    await expect(page.locator('.chart-empty')).toHaveCount(0)
    await expect(page.getByText(/Waiting for the first/)).toHaveCount(0)
    expect(await candleCalls(page)).toEqual([{ instrument: 'binance:BTCUSDT:spot', timeframe: '1m', limit: '500' }])
  })

  test('says the history is loading while live bars already fill the chart', async ({ page, open }) => {
    await openTrading(page, open, { ...engineTrading, history: { '*': 'hang' } })
    const loading = page.locator('.chart-empty[role="status"]')
    await expect(loading).toHaveText('Loading candle history')
    await expect(loading.locator('.state-quiet.shimmer')).toHaveCount(1)
    // The stub's live bars walk on from 64,210: the price moves while the history still loads.
    await expect(page.locator('.ticker-price')).toHaveText(/\d/)
    await expect(page.locator('.ticker-price')).not.toHaveText('64,210.00')
    await expect(loading).toBeVisible()
  })

  test('says in the engine’s words when the history did not load, and Retry loads it', async ({ page, open }) => {
    await openTrading(page, open, { ...engineTrading, history: { '*': 'fail' } })
    const alert = page.locator('.chart-empty[role="alert"]')
    await expect(alert).toContainText('The candle history did not load.')
    await expect(alert.locator('.chart-empty-note')).toHaveText(
      'Fetching binance:BTCUSDT:spot 1m bars from the venue failed: connection timed out.',
    )
    // The live stream still fills the chart.
    await expect(page.locator('.ticker-price')).toHaveText(/\d/)
    // The venue answers again.
    await page.evaluate(() => {
      delete (window as { __stubHistory?: Record<string, unknown> }).__stubHistory?.['*']
    })
    await alert.getByRole('button', { name: 'Retry' }).click()
    await expect(page.locator('.chart-empty')).toHaveCount(0)
    expect(await candleCalls(page)).toHaveLength(2)
  })

  test('moves the last candle with the bar forming, then the closed bar replaces it', async ({ page, open }) => {
    // On a held clock: the stub shows the bar forming half way to each close, here at 500 ms.
    await open({ ...engineTrading, candles: { ...engineTrading.candles!, everyMs: 1_000 } }, { clock: true })
    await page.getByRole('heading', { name: 'Strategies' }).waitFor()
    await page.clock.pauseAt((await page.evaluate(() => Date.now())) + 1_000)
    await page.keyboard.press('Control+2')
    const price = page.locator('.ticker-price')
    await expect(price).toHaveText('64,210.00')

    // The first live bar opens at 64,210 and closes 38.53 higher (the stub's walk); half way, the
    // forming bar is the last candle, and its close the price.
    await page.clock.runFor(600)
    await expect(price).toHaveText('64,229.26')
    await page.clock.runFor(500)
    await expect(price).toHaveText('64,248.53')
    expect(await page.evaluate(() => (window as { __stubCandles?: number }).__stubCandles)).toBe(1)
  })

  test('a switch stops the old stream and drops the old history’s late answer', async ({ page, open }) => {
    // BTCUSDT's history answers after 800 ms, ETHUSDT's never.
    await openTrading(page, open, {
      ...engineTrading,
      candles: { ...engineTrading.candles!, everyMs: 3_600_000 },
      history: { 'binance:BTCUSDT:spot': 800, '*': 'hang' },
    })
    await expect(page.locator('.chart-empty')).toHaveText('Loading candle history')
    expect(await subscribed(page)).toEqual(['candles:binance:BTCUSDT:spot:1m'])

    await page.locator('.ticker').click()
    await page.getByRole('option', { name: /^ETHUSDT\s*Binance spot/ }).first().click()
    await expect(page.locator('.ticker')).toHaveAccessibleName(/^ETHUSDT, Binance spot\./)
    await expect.poll(() => subscribed(page)).toEqual(['candles:binance:ETHUSDT:spot:1m'])

    // Past the BTCUSDT answer: its bars never reach the ETHUSDT chart.
    await page.waitForTimeout(1_000)
    await expect(page.locator('.chart-empty')).toHaveText('Loading candle history')
    await expect(page.locator('.ticker-price')).not.toHaveText(/\d/)

    await page.getByRole('button', { name: 'Timeframe: 1m' }).click()
    await page.getByRole('menuitemradio', { name: '5m', exact: true }).click()
    await expect.poll(() => subscribed(page)).toEqual(['candles:binance:ETHUSDT:spot:5m'])
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
