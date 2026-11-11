# Trading Desk GUI

One Electron app with two sections, Strategy Lab and Trading. The toggle in the title bar switches between them, and so do Ctrl+1 and Ctrl+2.

- `shared/`: the package both processes use: the API types generated from Rust (`src/api-types.ts`, never edited by hand), the IPC contract and the typed API client.
- `app/`: the Electron app (electron-vite, React, TypeScript). `src/main` starts the td-server engine as a sidecar and holds its token; `src/preload` exposes `window.td`; `src/renderer` is the UI.

The renderer never sees the engine's token. Every HTTP call goes renderer → IPC → main, which checks it against the route list and sends it with the token; main also holds the one WebSocket and forwards its messages over IPC.

## Commands

Run these from `gui/`, with pnpm 10 and Node 24.

- `pnpm install`: install the workspace.
- `pnpm dev`: build the engine optimised (`cargo build --release -p td-server --bin td-server`) and open the app with hot reload. The first build takes several minutes; later ones rebuild only what changed. `pnpm --filter @td/app dev` opens it without building the engine.
- `pnpm test`: the vitest tests. The test that starts the real engine skips itself unless `target/release/td-server` exists.
- `pnpm typecheck` and `pnpm build`: TypeScript checks and the production bundle in `app/out`.
- `pnpm shots`: builds the app and takes headless screenshots of every renderer state at three window sizes into `gui/shots/` (git-ignored). It never starts Electron: it serves `app/out/renderer` on a local port for the length of the run, opens one headless Chromium, and stands in for `window.td` with `app/shots/bridge-stub.js`. Pass a pattern to take only some states, for example `node app/shots/shots.mjs lab-result`. Needs `npx playwright install --only-shell chromium` once.
- `pnpm gates`: the Playwright UI gates in `e2e/`, against the last build (run `pnpm build` first). See "UI gates" below.
- `node scripts/quit-check.mjs`: launches the built app once with the real engine, closes the window, and checks that Electron, its helpers and td-server all exit. It starts a real Electron window, so run it by hand, once, never in a loop.

In development the app starts `<repo>/target/release/td-server` (`.exe` on Windows). It is the optimised build because the debug build runs backtests and loads data about twelve times slower. Set `TD_SERVER_BIN` to use another binary, and `TD_DATA_DIR` to choose the engine's data folder. Set `TD_MOCK=1` to run both sections on the fixtures in `app/mock/` instead of the engine's data; the engine still starts. The Lab's fixtures come from `app/mock/generate.mjs` and Trading's from `app/mock/generate-trading.mjs` (instruments for both venues and a few thousand 1m and 1h bars). The Trading mock continues those bars live, one closed bar every 1.5 seconds with the bar in progress shown twice before it closes, and fills manual orders at the next bar's open.

## UI gates

`e2e/` holds the Playwright specs that check the renderer the same way every time. Like `pnpm shots`, they serve `app/out/renderer` on a local port, stand in for `window.td` with the bridge stub and use headless Chromium; they never start Electron. The states, the window sizes and the server live in `app/shots/harness.mjs`, shared by both.

- `layout.spec.ts`: at 900x600, 1100x720 and 1600x1000, for every Strategy Lab and Trading state: no horizontal overflow, no overlapping text or controls, no clipped text, no broken images, hit areas of at least 24 px (44 px in the Lab's composer and Trading's order composer), and axe at WCAG 2.2 AA, contrast included. Layers that cover the page on purpose are declared in `audit.ts`: menus, popovers and tooltips, the Lab's composer and sticky table headers, and Trading's chart canvas, which the switcher, the position line, the order composer and the attribution link float over. It also checks that the end of a long result scrolls fully above the composer and its fade.
- `flows.spec.ts`: the section toggle by click, Ctrl+1/2 and the arrow keys; choosing a strategy from the list and from the menu; Run through its steps to the result; a failed run; Cancel; Run with nothing chosen; and Retry after the engine failed. In Trading, on the mock in real time: switching instrument from the menu and with Ctrl+K search, changing the timeframe, starting a session, a market order sent and filled, an invalid size, a limit order, a rejected order, the session panel, stopping the session, and leaving for the Lab with Ctrl+1, which takes the chart out of the page, and coming back with Ctrl+2. On the engine stub: the candle history drawn before any live bar, the loading line while it loads, the engine's error with Retry when it fails, the forming bar moving the last candle until its closed bar replaces it, and a switch of instrument or timeframe that stops the old stream and drops the old history's late answer.
- `motion.spec.ts`: no animated layout properties, no `transition: all`, no infinite animation on content (the shimmer on work in progress is the one allowed), and nothing moving or looping under reduced motion, through the Lab and through Trading. The live price's tint swaps colour only, with no transition and no change to layout. The count-up of a fresh result and the chart run on `requestAnimationFrame` and canvas, which these checks cannot see.
- `screenshots.spec.ts`: `toHaveScreenshot` baselines of the key Lab states and every Trading state at the three sizes, in `e2e/__screenshots__`. Trading's states run on Playwright's clock, held still before Trading opens and moved on only by the steps (`hold` and `tick` in `harness.mjs`), so the live chart shows the same bars, prices and fills on every run.

Every gate fails on a console error. CI runs them all after the build (`.github/workflows/gui.yml`).

Fonts render differently on each operating system, so the screenshot baselines are made and compared on CI's Linux runner only, and `screenshots.spec.ts` skips on Windows and macOS. To add or change a baseline, push: the run writes each missing baseline and fails, and the `ui-gates` artifact of that run holds the new files under `e2e/__screenshots__`. Look at them, commit them, and push again. Never update a baseline only to turn the run green; a changed screenshot is a design decision.

## Design

The look follows the project's design direction (`docs/design/direction.md`, outside git). Every colour and motion duration is a token in `shared/src/theme.css`, and components never name a raw one; the one exception is the reduced-motion override in `styles.css`, which cuts every duration to 0.01 ms. Sizes and spacing are still written as raw pixel values in `styles.css`. The Lab reads its data through the `LabSource` seam in `app/src/renderer/src/lab/source.ts`, so mock data never enters the components.

Trading reads its data through the `TradingSource` seam in `app/src/renderer/src/trading/source.ts`, chosen the same way. With the real engine the chart loads the last 500 closed bars from `GET /candles`, and the live `candles:` topic continues them: each closed bar, and the bar still forming as the venue updates it, drawn as the last candle until its closed bar replaces it. The engine has no manual order endpoint yet (#36), so the order composer says orders arrive with #36.

The chart is TradingView Lightweight Charts, under the Apache 2.0 licence, which asks for its attribution notice and a link to TradingView on a page users see. The library's own logo injects a `<style>` element, which the app's content security policy blocks, so the chart turns it off and shows a plain "Charting by TradingView" link instead, with the attribution notice from the library's NOTICE file under it. Main opens that one URL in the browser and denies every other window.
