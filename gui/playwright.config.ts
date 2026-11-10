// The UI gates: Playwright specs in e2e/ against the built renderer (app/out/renderer), served on
// a local port with the bridge stub as `window.td`. They never launch Electron. Build first:
// `pnpm build && pnpm gates`.
import { defineConfig } from '@playwright/test'

export default defineConfig({
  testDir: 'e2e',
  fullyParallel: true,
  workers: 2,
  retries: 0,
  forbidOnly: !!process.env.CI,
  timeout: 30_000,
  reporter: process.env.CI ? [['list'], ['github']] : 'list',
  outputDir: 'e2e/.results',
  // Baselines are made on CI's Linux runner (README, "UI gates"). A missing one is written (and the
  // test passes), so the gui workflow fails any run that leaves an uncommitted baseline behind.
  updateSnapshots: 'missing',
  snapshotPathTemplate: '{testDir}/__screenshots__/{arg}{ext}',
  expect: {
    toHaveScreenshot: { maxDiffPixelRatio: 0.002, animations: 'disabled', caret: 'hide' },
  },
  use: {
    browserName: 'chromium',
    headless: true,
    trace: 'retain-on-failure',
  },
})
