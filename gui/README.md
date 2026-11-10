# Trading Desk GUI

One Electron app with two sections, Strategy Lab and Trading. The toggle in the title bar switches between them, and so do Ctrl+1 and Ctrl+2.

- `shared/`: the package both processes use: the API types generated from Rust (`src/api-types.ts`, never edited by hand), the IPC contract and the typed API client.
- `app/`: the Electron app (electron-vite, React, TypeScript). `src/main` starts the td-server engine as a sidecar and holds its token; `src/preload` exposes `window.td`; `src/renderer` is the UI.

The renderer never sees the engine's token. Every HTTP call goes renderer → IPC → main, which checks it against the route list and sends it with the token; main also holds the one WebSocket and forwards its messages over IPC.

## Commands

Run these from `gui/`, with pnpm 10 and Node 24.

- `pnpm install`: install the workspace.
- `pnpm dev`: build the engine (`cargo build -p td-server`) and open the app with hot reload. `pnpm --filter @td/app dev` opens it without building the engine.
- `pnpm test`: the vitest tests. The test that starts the real engine skips itself unless `target/debug/td-server` exists.
- `pnpm typecheck` and `pnpm build`: TypeScript checks and the production bundle in `app/out`.
- `pnpm shots`: builds the app and takes headless screenshots of every renderer state at three window sizes into `gui/shots/` (git-ignored). It never starts Electron: it serves `app/out/renderer` on a local port for the length of the run, opens one headless Chromium, and stands in for `window.td` with `app/shots/bridge-stub.js`. Pass a pattern to take only some states, for example `node app/shots/shots.mjs lab-result`. Needs `npx playwright install --only-shell chromium` once.
- `pnpm gates`: the Playwright UI gates in `e2e/`, against the last build (run `pnpm build` first). See "UI gates" below.
- `node scripts/quit-check.mjs`: launches the built app once with the real engine, closes the window, and checks that Electron, its helpers and td-server all exit. It starts a real Electron window, so run it by hand, once, never in a loop.

In development the app starts `<repo>/target/debug/td-server` (`.exe` on Windows). Set `TD_SERVER_BIN` to use another binary, and `TD_DATA_DIR` to choose the engine's data folder. Set `TD_MOCK=1` to show the Strategy Lab on the fixtures in `app/mock/` (made by `app/mock/generate.mjs`) instead of the engine's data; the engine still starts.

## UI gates

`e2e/` holds the Playwright specs that check the renderer the same way every time. Like `pnpm shots`, they serve `app/out/renderer` on a local port, stand in for `window.td` with the bridge stub and use headless Chromium; they never start Electron. The states, the window sizes and the server live in `app/shots/harness.mjs`, shared by both.

- `layout.spec.ts`: at 900x600, 1100x720 and 1600x1000, for every Strategy Lab state: no horizontal overflow, no overlapping text or controls, no clipped text, no broken images, hit areas of at least 24 px (44 px in the composer), and axe at WCAG 2.2 AA, contrast included. It also checks that the end of a long result scrolls fully above the composer and its fade.
- `flows.spec.ts`: the section toggle by click, Ctrl+1/2 and the arrow keys; choosing a strategy from the list and from the menu; Run through its steps to the result; a failed run; Cancel; Run with nothing chosen; and Retry after the engine failed.
- `motion.spec.ts`: no animated layout properties, no `transition: all`, no infinite animation on content (the shimmer on work in progress is the one allowed), and nothing moving or looping under reduced motion. The count-up of a fresh result runs on `requestAnimationFrame`, which these checks cannot see.
- `screenshots.spec.ts`: `toHaveScreenshot` baselines of the key states at the three sizes, in `e2e/__screenshots__`.

Every gate fails on a console error. CI runs them all after the build (`.github/workflows/gui.yml`).

Fonts render differently on each operating system, so the screenshot baselines are made and compared on CI's Linux runner only, and `screenshots.spec.ts` skips on Windows and macOS. To add or change a baseline, push: the run writes each missing baseline and fails, and the `ui-gates` artifact of that run holds the new files under `e2e/__screenshots__`. Look at them, commit them, and push again. Never update a baseline only to turn the run green; a changed screenshot is a design decision.

## Design

The look follows the project's design direction (`docs/design/direction.md`, outside git). Every colour and motion duration is a token in `shared/src/theme.css`, and components never name a raw one; the one exception is the reduced-motion override in `styles.css`, which cuts every duration to 0.01 ms. Sizes and spacing are still written as raw pixel values in `styles.css`. The Lab reads its data through the `LabSource` seam in `app/src/renderer/src/lab/source.ts`, so mock data never enters the components.
