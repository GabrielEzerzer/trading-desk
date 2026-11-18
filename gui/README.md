# Trading Desk GUI

One Electron app with two sections, Strategy Lab and Trading. The toggle in the title bar switches between them, and so do Ctrl+1 and Ctrl+2.

- `shared/`: the package both processes use: the API types generated from Rust (`src/api-types.ts`, never edited by hand), the IPC contract and the typed API client.
- `app/`: the Electron app (electron-vite, React, TypeScript). `src/main` starts the td-server engine as a sidecar and holds its token; `src/preload` exposes `window.td`; `src/renderer` is the UI.

The renderer never sees the engine's token. Every HTTP call goes renderer → IPC → main, which checks it against the route list and sends it with the token; main also holds the one WebSocket and forwards its messages over IPC.

## Commands

Run these from `gui/`, with pnpm 10 and Node 24.

- `pnpm install`: install the workspace.
- `pnpm dev`: build the engine optimised (`pnpm engine`, which runs `cargo build --release -p td-server --bin td-server`) and open the app with hot reload. The first build takes several minutes; later ones rebuild only what changed. `pnpm dev:hush` does the same but starts the app through `hush run`, so the engine gets the repo's secrets from the vault; only the app is started through hush, so the secrets stay out of the environment cargo's build scripts see.
- `pnpm test`: the vitest tests. The test that starts the real engine skips itself unless `target/release/td-server` exists.
- `pnpm typecheck` and `pnpm build`: TypeScript checks and the production bundle in `app/out`.
- `pnpm serve`: serves the last build (run `pnpm build` first) on http://127.0.0.1:4319/ (`--port <n>` to change it) with `app/shots/bridge-stub.js` standing in for `window.td`, so the renderer opens in an ordinary browser tab without Electron or the engine. See "Looking at the renderer" below.
- `pnpm dist`: builds the engine, the app and the Windows installer with electron-builder (`app/electron-builder.yml`) into `app/dist/` (git-ignored). The package holds `out/` and `package.json` only, since vite bundles every dependency; `td-server.exe` goes into the installed app's `resources/` folder, where main looks for it in a packaged app. There is no app icon yet, so it uses Electron's default.
- `pnpm install-check`: after `pnpm dist`, installs the installer silently into a temporary folder, starts the installed engine on an empty data folder and checks it answers what the Lab loads first, launches the installed app once on a fresh data folder and profile, uninstalls it and deletes the temporary folders. It prints `INSTALL-CHECK PASS` or `INSTALL-CHECK FAIL: <reason>`. The launch sets `TD_SMOKE=1`, which makes main quit with exit code 0 once the engine's handshake has arrived and the page has called main through `window.td` (so the preload and the renderer run), and exit 2 if the engine or the page fails or 30 s pass first, and `TD_USER_DATA_DIR`, which sets Electron's profile folder. The launch passes when the app reported the smoke passed, started the engine from the install folder and the engine stopped cleanly. If the app then exits with 1, the quit watchdog ended a slow shutdown, and the line reads `INSTALL-CHECK PASS (the quit watchdog ended the app after the engine stopped cleanly; see #71)`. It refuses to run if the app is already installed for this user, since uninstalling would remove that install. It starts a real Electron window, so run it by hand, once, never in a loop.
- `node scripts/quit-check.mjs [lab|trading]`: launches the built app once with the real engine, opens the Lab (the default) or Trading with its live chart, closes the window, and checks that Electron, its helpers and td-server all exit within 5 s. Check both sections. It starts a real Electron window, so run it by hand, once per section, never in a loop.

The engine reads `OANDA_API_TOKEN`, the API token of an OANDA practice account, from its environment when an FX download starts, and the app passes its own environment to the engine it starts. `pnpm dev:hush` takes the token from the vault. Otherwise, in `pnpm dev` or a built app, set `OANDA_API_TOKEN` in the environment the app starts from; without it, an FX download from the Lab fails and the Lab says the token is missing. The token never reaches the renderer, and the app neither logs it nor writes it to a file.

In development the app starts `<repo>/target/release/td-server` (`.exe` on Windows). It is the optimised build because the debug build runs backtests and loads data about twelve times slower. Set `TD_SERVER_BIN` to use another binary, and `TD_DATA_DIR` to choose the engine's data folder. Set `TD_MOCK=1` to run both sections on the fixtures in `app/mock/` instead of the engine's data; the engine still starts. The Lab's fixtures come from `app/mock/generate.mjs` and Trading's from `app/mock/generate-trading.mjs` (instruments for both venues and a few thousand 1m and 1h bars). The Trading mock continues those bars live, one closed bar every 1.5 seconds with the bar in progress shown twice before it closes, and fills manual orders at the next bar's open.

The app's `productName`, `Trading Desk`, names its profile folder (`%APPDATA%\Trading Desk`), so `pnpm dev`, `quit-check` and the installed app share that profile and Electron's single-instance lock: while the installed app is open, `pnpm dev` or `quit-check` exits at once.

## Looking at the renderer

`pnpm serve` serves `app/out/renderer` and puts the bridge stub's script in front of the app's own, configured from the page's query string. `preset` picks a starting configuration: `mock` (the default: the engine running and both sections on the fixtures in `app/mock/`), `engine` (the Lab on the stub's fake engine, `app/mock/engine.json`), `engine-trading` (Trading on the fake engine, with a closed bar every 250 ms) or `engine-session` (the same with a running strategy session). Every other parameter sets one field of the stub's `config` (listed at the top of `bridge-stub.js`), read as JSON when it parses and as text otherwise. Rebuild and reload to see a change. For example:

- `/`: the Lab on the mock; Ctrl+2 opens Trading on the mock.
- `/?engine=starting` and `/?engine=failed`: the engine starting and failed.
- `/?preset=engine`: the Lab on the fake engine, with saved runs and stored data. Add `&hangStrategies=true` for the strategy list loading, `&noData=true` for nothing stored, `&dataCutoff=2025-03-31&codeChanged=true` for a data cutoff and a changed strategy, `&backtest=data_missing` or `&backtest=too_many_runs` for a refused run, `&download=no_token` for an FX download without a token, and `&deleteFails=true` for a refused delete.
- `/?preset=engine-trading`, then Ctrl+2: Trading on the fake engine. Add `&history={"*":"hang"}` or `&history={"*":"fail"}` for the candle history loading or failing, and `&hangInstruments=true` or `&failInstruments=true` for the instrument lists.
- `/?preset=engine-session`, then Ctrl+2 and Binance spot BTCUSDT: Trading with a running strategy session.

The rest of a state is reached by using the page: choose a strategy, run it, open a menu. Time runs as it does in the app, so a run or a live chart keeps moving. On the mock, `window.tdMockTrading.reconnect()` and `window.tdMockTrading.dropBucket()` in the browser console show the feed reconnecting and a dropped bucket.

## Design

The look follows the project's design direction (`docs/design/direction.md`, outside git). Every colour and motion duration is a token in `shared/src/theme.css`, and components never name a raw one; the one exception is the reduced-motion override in `styles.css`, which cuts every duration to 0.01 ms. Sizes and spacing are still written as raw pixel values in `styles.css`. The Lab reads its data through the `LabSource` seam in `app/src/renderer/src/lab/source.ts`, so mock data never enters the components.

Trading reads its data through the `TradingSource` seam in `app/src/renderer/src/trading/source.ts`, chosen the same way. With the real engine the chart loads the last 500 closed bars from `GET /candles`, and the live `candles:` topic continues them: each closed bar, and the bar still forming as the venue updates it, drawn as the last candle until its closed bar replaces it. The order composer places manual paper orders with `POST /sessions/{id}/orders`, and the open orders list cancels them with `POST /sessions/{id}/orders/{order}/cancel`.

The chart is TradingView Lightweight Charts, under the Apache 2.0 licence, which asks for its attribution notice and a link to TradingView on a page users see. The library's own logo injects a `<style>` element, which the app's content security policy blocks, so the chart turns it off and shows a plain "Charting by TradingView" link instead, with the attribution notice from the library's NOTICE file under it. Main opens that one URL in the browser and denies every other window.
