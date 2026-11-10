# Lane B (e2-tester) journal — Part of #34, Closes #33

## Goal
Lane B of the Fable engine review (#34, see `docs/fable-review.md` on main if present, and issue #34): tester/metrics fixes. Reviewer round 1 requested changes; this round answers them.

## State at offload (2026-11-10)
- Commit 449d144: walk-forward windows anchored to the end, annualised metrics left out of short runs (MIN_DAILY_RETURNS 30 daily returns, 90 days), benchmark over the stitched span, `min_trades` default 1.
- aea7fda: merged origin/main (90fa44b, which includes #41 lane A at ENGINE_REVISION 3).
- WIP commit after that: unfinished edits to metrics.rs, runner.rs, tester.rs, tests/tester.rs by the previous worker (stopped mid-edit, it was about to write test changes). Check it compiles; finish or redo.

## Remaining work for this round (reviewer round 1 findings + nits)
1. `ENGINE_REVISION` = 4 in `crates/td-engine/src/runner.rs`, and the golden/fixture that pins the revision expects 4.
2. Walk-forward per-window benchmark: compute the benchmark per out-of-sample window and stitch it exactly like the strategy's windows, using the existing `stitch()` (decision for Gabriel: option one, stitched like the strategy).
3. Doc comment on `MIN_DAILY_RETURNS` explaining the floor.
4. A correlation test that checks the actual value (hand-worked), not just presence.
5. `WalkForwardSpec::check` rejects up front a train window too short for the metric floors (clear error), with a test.
6. A load test: a saved WalkForwardResult with no benchmark still loads.
Each new test must fail on the code before its fix.

## Then
- Gate: `cargo +stable fmt --all --check`; `cargo +stable clippy --workspace --all-targets --locked -- -D warnings`; `cargo +stable test --workspace --locked` (read every `test result:` line); `cargo +stable run -p td-server --bin gen-api-types --locked` then `git diff --exit-code gui/shared/src/api-types.ts`.
- Review gate: reviewer round 2 on the change since 449d144 plus the full diff vs merge base with origin/main; then Sol (or second reviewer if Sol unavailable) on the final diff.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Pathspec commits.

## Needs the laptop / skip in cloud
Nothing needs secrets; tests are offline. PR body + merge are done from the laptop.

## Log
