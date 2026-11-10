# Journal: d1-data-sources (issue #42)

## Goal
Adding a market today means editing a `match` on `Venue` in several td-data files. Put everything that differs per data source behind one trait, so a new source is one new module plus one registration line, and a new ticker on an existing source needs no code. This is the first block of the multi-market base: sources (#42, this), stored data (#43), market types (#44), FX and the gotobi strategy (#45).

## State when offloaded (2026-11-10, laptop closed)
A local Opus worker was cancelled mid-work. Its uncommitted tree is the `wip` commit on this branch: a new `crates/td-data/src/source.rs`, `live/parse.rs` removed with its parsing moved, and edits across `binance.rs`, `bybit.rs`, `download.rs`, `instruments.rs`, `live/mod.rs`, `live/ws.rs`, `lib.rs`, `error.rs`, two test files and `td-server/src/api/error.rs`. Nothing in it has been verified: it may not compile. First read the diff against `origin/main`, run `cargo check -p td-data -p td-server`, and decide what is done and what is left against the success condition. Keep what is sound; do not restart from scratch without a reason written here.

## Success condition
1. One trait in td-data covers: which instrument kinds the source lists, the instrument list, 1-minute candle history for a range, funding (optional), the live stream (optional: URL, greeting, frame parsing), and default fees. Binance and Bybit each implement it in their own module.
2. One lookup maps a `Venue` to its source. No `match` on `Venue` remains in td-data or td-server non-test code outside `Display`, argument/text parsing and that lookup. Record the grep.
3. A source without funding or without a live stream reports it with a clear `DataError`; nothing assumes every source is a crypto exchange with a WebSocket.
4. The download logic (month loop, retain, store write, gap report) and the live-feed logic run against a fake source in tests with no network: at least one test drives the download end to end through a fake source into a temp store, and one shows the "no funding" / "no live stream" error path.
5. Binance and Bybit behave exactly as before. Existing tests pass with no change to their expectations; a test is touched only for a renamed function, and each such edit is listed.

Shape the trait so the next source fits without being built now: an FX history provider that publishes one compressed file per day, has no instrument-list endpoint (a fixed list), no funding, no WebSocket, and a market that closes at weekends. Do not add that source, a new `Venue` variant, or any FX code.

## Stay out of
`crates/td-engine`, `crates/td-server/src/run.rs`, `paper.rs`, `api/runs.rs`, `api/sessions.rs`, `api/jobs.rs`, `api/data.rs`, `crates/td-data/src/store.rs`, the td-data CLI commands, and `gui/` (branch `d2-stored-data` is changing `store.rs`, `api/data.rs`, the CLI and the generated `gui/shared/src/api-types.ts`). If `api-types.ts` would change, note it instead of regenerating it. The server's test seams in `crates/td-server/src/api/source.rs` (`MarketSource`, `LiveSource`) keep working; simplify them only if the trait makes part redundant.

## Review gate
Stakes: feature. `reviewer` until it approves, then `sol-loop` on the final diff. Reviewer briefs carry the success condition above.

## Rules for this run
- Work on this branch only. Never touch `main`, never merge the PR, never touch production. The PR stays a draft.
- `gh pr` and `gh issue` do not work in the cloud. Do not try them. The issue text you need is in this journal.
- You are the orchestrator, on Fable 5.2. Code-writing workers run through the Agent tool on Opus (`model: "opus"`), one at a time on this branch. Reviews go through the `reviewer` agent and `sol-loop`; if the GPT seat returns SEAT UNAVAILABLE or TIMEOUT, run a second, separate `reviewer` with the stand-in brief from CLAUDE.md and record it here.
- CI on GitHub is the gate (`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, on every push to a branch other than main). After each push, read the run for the exact head. If the CI result cannot be read from the cloud, say so in the summary; do not replace it with a local full-suite run. Focused local checks (one test, `cargo check -p <crate>`) are fine while building.
- Tests never touch the network and never touch a real data folder: temp directories and fakes only. Never commit market data. No secrets exist in the cloud; do not invent any.
- Send no personal or identifying data to any external service.
- Other lanes are live on this repo (GUI branches, `g1-candles`, `g2-manual-orders`, `e2-tester`). Run `git fetch origin && git merge origin/main` at the start and before the final review, and resolve conflicts on this branch.
- Code rules: smallest complete change; finish callers, tests, fixtures and doc comments; no speculative abstractions; new logic carries a test; every new check must be seen to fail once; a known limit gets `shortcut: <limit>, <when to upgrade>`.
- Commit and push after every round, and add a dated entry under "Log" each time.

## End state
Stop at Waiting (reviews clean, CI green on the head, ready for the laptop session to merge) or Blocked (say why). Write `.agent/summary.md`: what changed, the review results per seat and round, the CI run for the final head, open decisions, and one line on what was skipped or not checked. Push it.

## Log
- 2026-11-10 19:40 UTC: offloaded from the laptop with the wip commit; nothing verified yet.
