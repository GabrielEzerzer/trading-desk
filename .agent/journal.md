# Speed hillclimb: journal

Started 2026-11-11, thread "Improve Backtesting and Paper View Speed". Gabriel is away (class, gym); this is a `full-send` run. Limits: 6 hours of wall clock, 20 rounds, or two rounds in a row without improvement.

## Goal and reading

`away · metric (wall time of the benchmark's dev workloads) · feature · logic, plus one UI lane`

Gabriel: the app is slow, in backtesting and in the paper view. Make it faster by structural means (build, algorithm), not by hunting single lines.

Three pull requests, in this order:

1. `perf-bench`: the benchmark, and the app starting an optimised engine build. No overlap with other open work.
2. `perf-engine`: engine hot path (sweep shared preparation, data load, indicators). Starts only after #66 merges, on the main that contains it, because #66 rewrites `runner.rs`, `tester.rs` and the strategies and adds an ask series to `run`, `sweep` and `walk_forward`.
3. `live-chart`: the Trading chart loads history from `GET /candles` and shows the forming bar. This is not metric-shaped; it is judged on acceptance criteria and proven by looking at it.

## Coordination (2026-11-11)

- Gotobi / market-types thread (e1344837): #64 merged (main `6eb2b47`). #66 one to two hours out; it will send one line here when it merges. After #66 it has a small `runs.rs` slice (load stored asks) and an engine slice (`portfolio.rs`, `strategy.rs`, `trades.rs`, `metrics.rs`, td-core `instrument.rs`). I stay out of `session.rs`, `broker.rs`, `fill.rs`, `paper.rs`, `sessions.rs`, `binance.rs`, `bybit.rs`, `source.rs` until #66 is in.
- Frontend thread (dac0fd99): nothing in flight. It handed over the chart-history wiring with design constraints (reuse `.chart-empty` overlay, no spinner, attribution stays, baselines rendered on CI Linux only, one hidden Electron instance at a time). Manual-order displays and #62 stay with it.
- PR #65 (p3-stats) has no owner among running threads: parked. It touches `tester.rs`; I keep my `tester.rs` changes small.

## Measurement setup

- **Command:** `cargo run [--release] -p td-server --bin td-bench -- --split dev --reps N`. It prints one line per workload and a last line `BENCH split=dev total_ms=<n> checksum=<hex>`.
- **Metric:** `total_ms` on the dev split: the sum over dev workloads of the fastest of N repetitions. The fastest repetition is used because this machine runs near 90% CPU and other agents are compiling; the minimum is the least load-sensitive statistic.
- **Eval set:** `harness/evals/perf-workloads.json`, frozen once the baseline is taken. Synthetic 1-minute candles from a fixed seed, written through the real store, so the load path is the real one. Dev workloads drive the loop. Test workloads (other seed, other strategy and timeframe pairings) are scored at baseline and at the end only.
- **Guardrails, every round:**
  - `checksum` on dev must equal the baseline's: results are bit-identical, so `ENGINE_REVISION` stays 4.
  - CI (`fmt`, `clippy`, `cargo test --workspace`, golden fixtures) green on the pushed head.
  - Clean release build time of `td-server`, recorded when the build profile changes.
- **Two baselines:** A is the debug build, which is what the app starts today. B is the release build of untouched engine code.
- **Targets (my defaults, Gabriel gave no number):** PR 1: the app's engine build at most 1/5 of baseline A on dev. PR 2: at most 1/2 of baseline B on dev, same checksums.
- **No gaming:** the workloads file, the generator seed and the checksum code do not change after the baseline. The one planned harness edit is passing `&[]` for asks when #66 changes the engine signatures; the checksum must not move across that edit.

## Baselines (commit `c4eb9a9`, 2026-11-11 11:49 to 12:03 UTC)

Checksums are the same in debug and release: dev `1d66746fdcd697cb`, test `a866581ba9e3c049`.

| Build | Split | Run | total_ms | load | backtest | sweep | walk-forward |
|---|---|---|---|---|---|---|---|
| A debug, reps 3 | dev | 1 | 21,802.0 | 10,048.4 | 1,304.1 | 3,895.3 | 6,554.2 |
| A debug, reps 3 | dev | 2 | 19,966.2 | 5,604.9 | 797.2 | 3,977.4 | 9,586.6 |
| A debug, reps 3 | test | 1 | 13,301.5 | 5,397.0 | 1,011.2 | 5,491.0 | 1,402.4 |
| B release, reps 5 | dev | 1 | 2,250.7 | 1,028.6 | 115.5 | 316.3 | 790.2 |
| B release, reps 5 | dev | 2 | 2,316.6 | 1,243.1 | 137.6 | 370.7 | 565.2 |
| B release, reps 5 | test | 1 | 1,840.9 | 1,050.3 | 126.9 | 516.6 | 147.1 |

Noise floor: the dev total moved 3% (release) to 9% (debug) between two unchanged runs; single workloads moved up to 45% because other agents were compiling. A round is kept only when the dev total, taken as the lower of two runs, improves by more than 10%.

Baseline A for the target is the lower debug run, 19,966.2 ms. Baseline B is the lower release run, 2,250.7 ms.

## Rounds

### Round 1: the app starts the release build (kept)

- **Hypothesis:** most of the slowness Gabriel sees is the build, not the algorithm. `pnpm dev` builds and starts `target/debug/td-server`, which has no optimisation.
- **Change:** `pnpm dev` builds with `--release`, and the app, `quit-check` and the integration test look for `target/release/td-server`. No engine code changes; no `[profile.release]` section added, the defaults are used.
- **Dev:** 19,966.2 ms to 2,250.7 ms, 8.9 times faster (target was 5). Test: 13,301.5 ms to 1,840.9 ms, 7.2 times.
- **Guardrails:** checksums identical on both splits. Clean release build of `td-server` 570 s on this machine under load (one time). Rebuild after touching `crates/td-engine/src/runner.rs`: release 55 s, debug 36 s.
- **Taught:** in release, loading data is 46% of the dev total (it reads every month of 1-minute candles and resamples them, twice per run in the app). That is the first target for PR 2, ahead of the sweep.

## Decisions made for Gabriel

- Targets above.
- Took the chart-history wiring from the frontend thread's queue, since it is the main cause of the slow paper view and that thread was waiting for a go.

## DECISION FOR GABRIEL

- Manual paper orders fill at the next bar open of the session's timeframe, so up to an hour on a 1h session. Filling on 1-minute bars or the live price while the strategy keeps its own timeframe would change engine behaviour and saved sessions. Not done in this run.
- Walk-forward in one pass per parameter set (slice metrics per window) would cut the work several times over but moves results at window edges. Not done in this run.

## Open

- PR 1 (`perf-bench`): push, CI, review gate, merge.
- PR 2 (`perf-engine`): waits for #66. Rounds planned: load caching, shared sweep preparation, incremental indicators.
- PR 3 (`live-chart`): worker running in `D:/code/.worktrees/trading-desk-live-chart`.
- The bench worker did not check that checksums match between Windows and Linux; CI does not run the bench, so this only matters if someone compares across machines.
