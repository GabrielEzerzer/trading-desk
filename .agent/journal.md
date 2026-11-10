# Journal: f6-lab-engine-runs

## Goal
Run in the Strategy Lab starts one real backtest on the engine through the run-job routes from #39, and shows the saved result. Built, CI green on cbf9900 (gui run 42883916249, ci run 42883916192). What's left is the review gate, then stopping at Waiting.

## Context
- The app is one Electron app in `gui/` (see gui/README.md). This branch stacks on f1-gui-shell (PR #40), which is being merged into main now. Once #40 is on main: `git fetch origin && git merge origin/main`. Squash-merge duplicates may conflict; take main's version for files this branch didn't change. Push, then read CI for the new head.
- The design spec lives outside the repo and isn't available here. Judge copy and visuals against the existing Lab screens and the baselines in gui/e2e/__screenshots__.
- Never launch Electron (a past agent crashed the machine). CI's headless Chromium gates are the gate. Never edit crates/**, gui/shared/src/api-types.ts or .github/workflows/*.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## What f6 built (from its report)
- `gui/app/src/renderer/src/lab/engine-source.ts`, `runs.ts`, `Composer.tsx`, `StrategyLab.tsx`, `Result.tsx`, `EquityChart.tsx`, `progress.ts`.
- `gui/shared/src/client.ts`: startBacktest, jobResult, runs, run.
- `gui/app/src/main/bridge.ts`: four routes with tests.
- A run = one backtest on default params over the full stored range of the dataset from the overrides menu (`shortcut:` until #32).
- Progress comes from the `jobs` topic; cancel is POST /jobs/{id}/cancel.
- The verdict slot reads "No verdict rule yet" (grey). Null metrics show "–".
- The list shows the latest backtest from GET /runs, and opening a past result shows it settled.
- States:
  - 422/no data: download offer, progress, cancel, then run.
  - 429: "Another run is in progress. Try again when it finishes."
  - otherwise: the server's message.
- Stored-data listing doesn't exist yet. The workaround asks /data/coverage for 8 candidates plus instruments of saved runs (`shortcut:` until #43). There is no download size.
- Tests: lab/runs.test.ts, 9 engine flows in gui/e2e/flows.spec.ts via gui/app/shots/bridge-stub.js, 18 new baselines.

## Next steps
1. Merge origin/main once #40 has merged (see Context).
2. Review gate: run a `reviewer` on `git diff 65a2a8b..HEAD` (65a2a8b = f1 + main before f6; after the main merge, diff against origin/main).
   Success condition:
   - exactly one backtest per Run, with the right body;
   - no progress/result race (stuck on running, stale result, or a result applied to another strategy after a switch mid-run);
   - correct RunDetail mapping with null-safe metrics;
   - no dead-end states after a failed or cancelled download;
   - the bridge adds only the used routes, anchored;
   - the stub's responses match the real routes' shapes and status codes in crates/td-server;
   - tests can fail;
   - plain, accurate copy.
   A first review was stopped before it reported. Known suspect: the never-run text still says "Run its test plan to get a verdict", which isn't true against the engine; fix it.
3. Fix verified findings, re-review until approve, then `sol-loop` (if the sol seat is unavailable, a second reviewer attacks intent and assumptions).
4. Push, and wait for gui and ci on the exact head.
5. Write .agent/summary.md and stop at Waiting. Never Ready, never merge.

## Log
- 2026-11-10 19:40 UTC: offloaded from the laptop. Built and green; review not yet done.
