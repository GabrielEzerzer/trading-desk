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
- `node scripts/quit-check.mjs`: launches the built app once with the real engine, closes the window, and checks that Electron, its helpers and td-server all exit. It starts a real Electron window, so run it by hand, once, never in a loop.

In development the app starts `<repo>/target/debug/td-server` (`.exe` on Windows). Set `TD_SERVER_BIN` to use another binary, and `TD_DATA_DIR` to choose the engine's data folder. Set `TD_MOCK=1` to show the Strategy Lab on the fixtures in `app/mock/` (made by `app/mock/generate.mjs`) instead of the engine's data; the engine still starts.

## Design

The look follows the project's design direction (`docs/design/direction.md`, outside git). Every colour, size and duration is a token in `shared/src/theme.css`; components never name a raw value. The Lab reads its data through the `LabSource` seam in `app/src/renderer/src/lab/source.ts`, so mock data never enters the components.
