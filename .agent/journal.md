# Journal: f4-trading

## Goal
The Trading section of the app: a live chart for one ticker, a ticker switcher, and a paper order composer. Built (f4), and brought under the Playwright UI gates (f7). What's left is the review gate, then stopping at Waiting.

## Context
- The app is one Electron app in `gui/` (see gui/README.md). This branch stacks on f1-gui-shell (PR #40), which is being merged into main now. Once #40 is on main: `git fetch origin && git merge origin/main`. Take main's version for files this branch didn't change. Push, then read CI for the new head.
- Never launch Electron (a past agent crashed the machine). CI's headless Chromium gates are the gate. Never edit crates/**, gui/shared/src/api-types.ts or .github/workflows/ci.yml.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Head at offload: 8b12b61. ci passed (run 42886355077). gui was in progress (run 42886355198); read it.

## What was built (from the f4 and f7 reports)
- `gui/app/src/renderer/src/trading/**`:
  - the chart (lightweight-charts 5.2.1): gain/loss candles, fill markers, a dotted average-entry line, dashed resting limits; disposed on unmount;
  - the ticker switcher: a pill with symbol, venue and last price, plus a frosted searchable menu (Ctrl+K) with All/Spot/Perpetual and recents;
  - a timeframe pill (1m–4h);
  - the order composer: size, Market/Limit, a limit price, Sell/Buy, and one note line;
  - the position line and a panel with orders, fills and trades, plus Stop/Resume/New session.
- `TradingSource` seam: engine and mock (TD_MOCK). Main bridge routes for /sessions, and topic patterns for `candles:` and `session:`, with tests.
- App.tsx: the Lab stays mounted (a run keeps going), and Trading unmounts when hidden, which disposes the chart and ends its subscriptions.
- Licence: the TradingView logo is off (it injects a `<style>`, which the CSP blocks). A plain "Charting by TradingView" link is shown instead; main opens only that URL. The NOTICE text is quoted in Chart.tsx.
- Waiting on #36:
  - no candle history, so the engine chart starts empty;
  - no manual order route, so the composer says so;
  - no Manual sessions.
- f7: Trading added to layout, axe, flows (including that the chart is gone from the DOM while the Lab shows), motion, and screenshot baselines. Fixes in fba7917: Retry and the attribution link take clicks; the ticker search field keeps its full height.

## Next steps
1. Read gui run 42886355198 on 8b12b61. Fix it if it's red.
2. Merge origin/main once #40 has merged (see Context).
3. Review gate: run a `reviewer` on the diff against origin/main (after the merge).
   Success condition:
   - the Trading behaviour above;
   - no leaked subscriptions or charts across ticker, timeframe and section switches;
   - the bridge routes and topics are exact and anchored, and the token never reaches the renderer;
   - order validation rounds to tick and lot like the engine (Instrument::round_price / round_qty in td-core);
   - the stub's shapes match crates/td-server;
   - the licence attribution meets lightweight-charts' NOTICE;
   - tests can fail;
   - plain copy; colour only on controls and gain/loss.
4. Fix verified findings, re-review until approve, then `sol-loop` (if the sol seat is unavailable, a second reviewer attacks intent and assumptions).
5. Push, and wait for gui and ci on the exact head.
6. Write .agent/summary.md and stop at Waiting. Never Ready, never merge.

## Log
- 2026-11-10 19:40 UTC: offloaded from the laptop. Built and gated; review not yet done.
