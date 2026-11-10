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

In development the app starts `<repo>/target/debug/td-server` (`.exe` on Windows). Set `TD_SERVER_BIN` to use another binary, and `TD_DATA_DIR` to choose the engine's data folder.
