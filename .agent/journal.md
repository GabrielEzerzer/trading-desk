# Journal: d2-stored-data (issue #43)

## Goal
The disk this runs on is small, so market data has to come and go: see what is stored and how big it is, and delete it. The store already lives outside the repo (`TD_DATA_DIR`, else the platform data folder), but nothing can list sizes or delete. This is the second block of the multi-market base: sources (#42), stored data (#43, this), market types (#44), FX and the gotobi strategy (#45).

## State when offloaded (2026-11-10, laptop closed)
A local Opus worker was cancelled mid-work. Its uncommitted tree is the `wip` commit on this branch: about 600 new lines in `crates/td-data/src/store.rs`, 320 in `crates/td-data/tests/store.rs`, and exports in `lib.rs`. So the store half (criteria 1 to 3 and their tests) is probably well along, and the API, the CLI and the generated types (4 to 6) are not started. Nothing has been verified: it may not compile. First read the diff against `origin/main`, run `cargo check -p td-data`, and decide what is done and what is left.

## Where things are
- `crates/td-data/src/store.rs`: read its module doc first (file layout, atomic rename, the per-path lock in-process and across processes through a `<name>.parquet.lock` sidecar).
- `crates/td-server/src/api/data.rs` and the route table in `api/mod.rs`: `/instruments`, `/data/download`, `/data/coverage`. `api/jobs.rs` holds the running jobs.
- `crates/td-data/src/main.rs`: the `td-data` CLI.
- `gui/shared/src/api-types.ts`: generated with `cargo run -p td-server --bin gen-api-types`; never edited by hand.

## Success condition
1. The store lists every instrument it holds, with its months and bytes on disk per month, plus the funding file's bytes for a perpetual. A stray or unparseable folder is skipped or reported, never a panic.
2. The store deletes an instrument's candles for a month range, or the whole instrument including its funding file. A delete takes the same path lock a writer takes. It removes the lock sidecar of a file it deletes when that is safe, and folders it empties. Deleting something that is not there is not an error. It reports what it removed (files, bytes).
3. A delete can never leave the data folder: an instrument id cannot make a path escape the store root. There is a test for a hostile symbol.
4. API: a listing of stored data with sizes, and a delete (instrument, optional month range). `/data/coverage` also reports bytes per month. A delete is refused with a conflict error while a download job for the same instrument is running. New routes follow the existing auth, error and naming conventions and sit behind the same token guard.
5. The `td-data` CLI gets the same two commands, in the style of its existing ones.
6. `gui/shared/src/api-types.ts` is regenerated and committed.
7. Tests, on temp directories with no network: listing with sizes; delete of a month range leaves the other months readable; delete of a whole perpetual removes candles and funding; empty folders are gone; the hostile symbol; a writer and a delete on the same file do not corrupt it; the API conflict while a download job runs; the new routes reject a missing token like their neighbours.

Not in this change: downloading missing candles when a run starts. There is no automatic size cap; data is deleted by hand.

## Stay out of
`crates/td-engine`, `crates/td-server/src/run.rs`, `paper.rs`, `api/runs.rs`, `api/sessions.rs`, `crates/td-data/src/download.rs`, `instruments.rs`, `live/`, `binance.rs`, `bybit.rs`, `crates/td-server/src/api/source.rs` (branch `d1-data-sources` is refactoring those), and `gui/` except the generated `api-types.ts`. In `api/mod.rs` only add routes.

## Review gate
Stakes: feature, and delete is the one destructive operation in this codebase, so the `reviewer` stage includes a security pass on path handling (criterion 3) and the lock. Then `sol-loop` on the final diff. Reviewer briefs carry the success condition above. The reviewer should also answer: how does a delete behave against a concurrent download, and against a running paper session reading the same instrument?

## Rules for this run
- Work on this branch only. Never touch `main`, never merge the PR, never touch production. The PR stays a draft.
- `gh pr` and `gh issue` do not work in the cloud. Do not try them. The issue text you need is in this journal.
- You are the orchestrator, on Fable 5.2. Code-writing workers run through the Agent tool on Opus (`model: "opus"`), one at a time on this branch. Reviews go through the `reviewer` agent and `sol-loop`; if the GPT seat returns SEAT UNAVAILABLE or TIMEOUT, run a second, separate `reviewer` with the stand-in brief from CLAUDE.md and record it here.
- CI on GitHub is the gate (`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, on every push to a branch other than main). After each push, read the run for the exact head. If the CI result cannot be read from the cloud, say so in the summary; do not replace it with a local full-suite run. Focused local checks (one test, `cargo check -p <crate>`) are fine while building.
- Tests never touch the network and never touch a real data folder: temp directories and fakes only. Never run a delete against a real data folder. Never commit market data. No secrets exist in the cloud; do not invent any.
- Send no personal or identifying data to any external service.
- Other lanes are live on this repo (GUI branches, `g1-candles`, `g2-manual-orders`, `e2-tester`). `g1-candles` adds a candle-history route, so expect a small conflict in `api/mod.rs` or `api/data.rs`. Run `git fetch origin && git merge origin/main` at the start and before the final review, and resolve conflicts on this branch.
- Code rules: smallest complete change; finish callers, tests, fixtures and doc comments (the store's module doc says the lock sidecar stays on disk; keep it true); no speculative abstractions; new logic carries a test; every new check must be seen to fail once; a known limit gets `shortcut: <limit>, <when to upgrade>`.
- Commit and push after every round, and add a dated entry under "Log" each time.

## End state
Stop at Waiting (reviews clean, CI green on the head, ready for the laptop session to merge) or Blocked (say why). Write `.agent/summary.md`: what changed, the review results per seat and round, the CI run for the final head, open decisions, and one line on what was skipped or not checked. Push it.

## Log
- 2026-11-10 19:40 UTC: offloaded from the laptop with the wip commit; nothing verified yet.
