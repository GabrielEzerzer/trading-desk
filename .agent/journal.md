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

## Rounds

(none yet)

## Decisions made for Gabriel

- Targets above.
- Took the chart-history wiring from the frontend thread's queue, since it is the main cause of the slow paper view and that thread was waiting for a go.

## DECISION FOR GABRIEL

- Manual paper orders fill at the next bar open of the session's timeframe, so up to an hour on a 1h session. Filling on 1-minute bars or the live price while the strategy keeps its own timeframe would change engine behaviour and saved sessions. Not done in this run.
- Walk-forward in one pass per parameter set (slice metrics per window) would cut the work several times over but moves results at window edges. Not done in this run.

## Open

- Bench not built yet.
