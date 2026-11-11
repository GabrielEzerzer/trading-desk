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

## Review of the measurement (reviewer, round 1, head `73bfe86`): CHANGES REQUESTED

Verified and accepted:

1. **The metric left out what the Lab does today.** The Lab can start only single backtests (`bridge.ts` passes on `POST /runs/backtest` only), over the whole stored range, with default parameters. A real job is check, load, run, save, then the result is read back, and opening a saved run reads the data a third time to fingerprint it. The bench timed none of the save and read steps, and half of its dev total was sweep and walk-forward, which the Lab cannot start yet. A round that halves the sweep would have been kept while a Lab backtest took as long as before.
2. The cached dataset directory was shared by all worktrees and not keyed on the store's file format.
3. The checksum includes the crate version string (`engine_version`). The version stays fixed during this run.
4. `pnpm dev` linked every td-server binary in release.
5. Only the backtest mode had a test; nothing enforced the freeze of the workload file.

**Decision made for Gabriel (eval set changed once, before any engine round):** the hillclimb rule is not to edit the eval after the baseline. I changed it here because the review of the measurement is the step that is meant to catch a wrong metric, no engine change had been scored yet, and the only kept round so far (release build) is re-measured on the new set. The eight existing workloads are untouched and must print the same checksums; two are added, `lab_sma_1h_60mo` (dev) and `lab_donchian_15m_48mo` (test), which time one Lab backtest end to end. Sweep and walk-forward stay in the total because the tester is the Lab's stated purpose and the server already serves it; that weighting is Gabriel's to change. From the commit that adds them, the file is frozen and a test asserts its hash.

## Baselines on the merged head `73bfe86` (main with #65 and #67), eight workloads

#65 and #67 added metrics to every result, so checksums changed (dev `232c001b45c95b12`, test `6eecea8b74475979`) and runs got heavier. Debug dev 14,921.4 and 21,303.9 ms; debug test 17,133.8 ms. Release dev 3,448.2 and 2,988.9 ms; release test 2,039.7 ms. The spread between two unchanged runs was 15% (release) and 43% (debug) while two other agents were compiling. These are replaced by the baseline on the ten-workload set.

## Final baseline: ten workloads, head `682a9aa` (2026-11-11 12:44 to 12:55 UTC, machine quieter)

The eight original workloads print the same checksums as before the two Lab workloads were added. Checksums: dev `30170cbf866f6c97`, test `5d549e7dc7c03cb8`, the same in debug and release.

| Build | Split | total_ms | load | backtest | sweep | walk-forward | Lab backtest |
|---|---|---|---|---|---|---|---|
| A debug, reps 3 | dev | 37,738.7 | 4,342.8 | 840.2 | 2,645.5 | 5,404.7 | 24,505.5 |
| A debug, reps 3 | test | 49,958.1 | 6,381.7 | 1,144.5 | 5,968.2 | 1,560.7 | 34,903.1 |
| B release, reps 5 | dev, run 1 | 4,226.1 | 655.8 | 98.7 | 243.3 | 454.6 | 2,773.7 |
| B release, reps 5 | dev, run 2 | 4,407.1 | 975.5 | 106.4 | 273.4 | 457.3 | 2,594.6 |
| B release, reps 5 | test | 3,354.0 | 491.6 | 69.3 | 342.2 | 86.1 | 2,364.9 |

Phases of the dev Lab backtest (five years of 1-minute candles, 1h bars), fastest repetition, release: check 664 ms, load 920 ms, run 34 ms, save 63 ms, result read 83 ms, reopen 1,009 ms. In debug: 6,363, 6,877, 314, 1,203, 1,213, 8,536 ms. The strategy run is about 1% of what the user waits for; reading the 1-minute data three times is about 93%.

Noise floor on a quiet machine: 4% between two unchanged release runs. The keep rule stays at more than 10% on the lower of two runs, because other agents start compiling without notice.

Round 1 restated on this set: dev 37,738.7 ms to 4,226.1 ms (8.9 times), test 49,958.1 ms to 3,354.0 ms (14.9 times). Target was 5 times. Kept.

**Baseline B for PR 2 is 4,226.1 ms on dev. Target: at most 2,113 ms with identical checksums.**

One more change to the bench before the freeze: each repetition of the load and Lab workloads opens its own `Store`, so anything a store later keeps in memory is not carried between repetitions. A repetition then stands for the first run after an app start. Checksums unchanged.

## Review of the measurement, round 2 (reviewer, head `919ab51`): CHANGES REQUESTED

CI on `919ab51` (main with #70 merged in): `ci` and `gui` green.

Both findings verified against the app and accepted. Neither touches the workload file, whose hash test still passes; both change what a Lab repetition times, so the Lab baseline is taken again. No engine or store change has been scored yet.

1. **The reopen ran on the store the same repetition had just read.** The Lab opens a saved run only when its result is not already on screen (`StrategyLab.tsx`, the effect on `last`), which means a later session with a new server process. A cache inside `Store` would have made the bench's reopen free while the user's reopen stayed as slow. Fixed: the open phase reads through a `Store` of its own.
2. **The Lab reads the coverage of every stored month before each backtest (`engine-source.ts`, `api.coverage` in `run`), and the bench did not time it.** That is a fourth full read of the 1-minute data. Fixed: a `coverage` phase first, through the server's `read_coverage` (now `pub`), on the same store as check and load, as in the app; its output is part of the Lab checksum.
3. Nit, taken: the `WORKLOAD` line prints `first_ms` beside `min_ms`, so a cache that outlives a repetition shows as a gap between them. Rule for PR 2: a cache is a field of `Store`, never a static.

What one Lab repetition now stands for: the first backtest after the app starts (coverage, check, load, run, save, result read on one store), then the reopen of that run in a later session (a cold store).

## Baseline with the coverage phase and the later-session reopen

Head `f89f7cf`, 2026-11-11 13:17 to 13:32 UTC (`.agent/logs/baseline4.log`). Checksums equal in debug and release.

| Build | Split | total_ms | load | backtest | sweep | walk-forward | Lab backtest |
|---|---|---|---|---|---|---|---|
| A debug, reps 3 | dev | 58,713.2 | 5,583.9 | 1,082.1 | 3,262.5 | 7,964.8 | 40,819.9 |
| A debug, reps 3 | test | 52,179.5 | 6,392.2 | 1,006.0 | 4,070.9 | 1,361.0 | 39,349.4 |
| B release, reps 5 | dev, run 1 | 4,872.2 | 789.2 | 109.0 | 292.1 | 504.5 | 3,177.5 |
| B release, reps 5 | dev, run 2 | 7,647.7 | 993.7 | 136.5 | 483.7 | 608.5 | 5,425.4 |
| B release, reps 5 | test | 4,581.2 | 808.5 | 111.2 | 535.2 | 137.4 | 2,988.8 |

Phases of the dev Lab backtest, fastest repetition: release coverage 674, check 656, load 813, run 23, save 50, result read 65, reopen 897 ms; debug 8,025, 8,627, 8,930, 363, 1,360, 1,222, 12,294 ms. Four reads of the 1-minute data are 96% of a Lab backtest; the strategy run is under 1%.

**Round 1 on the final metric: dev 58,713.2 ms to 4,872.2 ms (12.0 times), test 52,179.5 ms to 4,581.2 ms (11.4 times). Target was 5 times. Kept.** A five-year Lab backtest goes from 40.8 s to 3.2 s.

Noise: run 2 was 57% above run 1 while other threads ran `cargo test --workspace`; CPU load was 100%. A single run cannot decide a round. PR 2 measures base and candidate interleaved, several runs each, and compares the lowest of each.

## After #66 (main `99a7743`) and review round 3

- #66 added an `asks` argument to the engine entry points; the bench passes none, as the server does for crypto. Release re-run on `0e352dd` (`baseline5.log`): every checksum equal to `f89f7cf`'s (dev `0ee29ceab903b31a`, test `2fa9814ee9eeb6b6`), so #66 changed no crypto result. Totals 7,011 to 9,408 ms dev under full load: not a baseline.
- Reviewer round 3 (head `0e352dd`): round 2's two findings fixed as the app behaves. One new should, accepted: the Lab checksum hashed each month file's size on disk, so a store change that keeps every candle would have looked like a changed result. Fixed: sizes are zeroed before hashing (still serialized in the timed phase). The nit (parse `first_ms` in the test) taken.
- Final checksums on `64f11b5` (`baseline6.log`): dev `18c3f65986696a8c`, test `4e8892bfdc155145`; the eight non-Lab workloads unchanged, `lab_sma_1h_60mo` `360b189bdc2fdcb4`, `lab_donchian_15m_48mo` `bfa8c14a6243bd0a`. Release totals on this head with a worker compiling: dev 5,009.9 and 5,531.7 ms, test 5,180.7 ms.

**Baseline B for PR 2: 4,872.2 ms on dev (the quietest run of the same timed code). Target: at most 2,436 ms with these checksums.**

## PR 2 plan (`perf-read`, worktree `trading-desk-perf-read`, branched from `64f11b5`)

The read path does not overlap #66 or the engine. Rounds, one hypothesis each:

- Round 2, hypothesis: coverage, check and load run seconds apart in one server process and each decode the same month files; keeping decoded months in the `Store` (validated by file length and time, bounded at 256 MB) makes check and load nearly free. Expect check 656 to under 100 ms, load 813 to about 300 ms, the `load` workload about 40% faster; the reopen and coverage stay cold. About 25 to 30% off the dev total.
- Round 3, hypothesis: the cold reads (coverage, the reopen, the first read of the `load` workload) decode 24 to 60 files one after another on one core; decoding a range's months in parallel cuts them several times on a free machine. About another 20 to 25%.
- Then, if time remains: shared preparation across a sweep's parameter sets, and incremental indicators.

## PR 1 review gate closed (2026-11-11 14:15 UTC)

- reviewer round 4 on `2491236`: APPROVE, no findings.
- Sol round 1: one should, accepted. The Lab checksum left out the data report (`RunDetail.data`, `data_error`) of both reads, so a read path that lost or skipped the report would have kept the checksum. Fixed in `510af3d`: the checksum covers the coverage (without file sizes), and the run, report and report error of both the job-result read and the reopen, and the reopen's check. A test asserts both reports are in the hashed JSON. Sol round 2: APPROVE, S1 resolved.
- **Final checksums, head `510af3d` (`baseline7.log`): dev `1bc2d4ff4b411ddc`, test `5ecd3911e0a8641c`.** Per workload: load_1h_24mo `3852b7945ccec859`, backtest_sma_1m_3mo `75ffe6edbb8ad741`, sweep_sma_1h_24mo `0a98024072a85554`, walkforward_donchian_1h_24mo `98212a17978b0b68`, lab_sma_1h_60mo `8cea35df3dbf82f9`; test: load_15m_18mo `334ea9e6970cea33`, backtest_donchian_5m_12mo `3f828b35d7f2aba6`, sweep_donchian_15m_12mo `917724d45265e03f`, walkforward_sma_4h_36mo `c0d922779bbc6a15`, lab_donchian_15m_48mo `64b19b670067da6d`. The timed work is the same as on `f89f7cf`; only what is hashed changed. Totals in that run (10.4 s dev) were taken with three other builds running and are not a baseline.
- Proof that the app's default engine path is the release build: `cargo build --release -p td-server --bin td-server --manifest-path ../Cargo.toml` (the new `pnpm dev` build step) from `gui/`, then `pnpm vitest run app/src/main/engine.integration.test.ts`: 1 passed, not skipped, on `510af3d`.
- The comment and README said "about nine times"; the final measurement is twelve, corrected.

## PR 2 (`perf-read`): rounds 2 and 3, measured 2026-11-11 14:26 to 14:37 UTC

Worker commits `b0abc6f` (round 2) and `ed844b4` (round 3), with the final bench merged in (`14585d1`). Three binaries, run one after another three times (`.agent/logs/round23.log`): the base (`510af3d`, the store as on main), round 2 alone (`ed844b4` reverted in the tree for the build), and rounds 2 and 3. Dev split, reps 5. The machine was under heavy load from other threads the whole time (the base took 9.7 to 14.3 s against 4.9 s on a quiet machine), so only the comparison inside each pass counts.

| Pass | Base | Round 2 | Rounds 2 and 3 |
|---|---|---|---|
| 1 | 14,150.9 | 5,654.6 | 4,423.7 |
| 2 | 14,306.8 | 7,144.4 | 3,333.9 |
| 3 | 9,660.2 | 5,614.9 | 3,414.9 |
| lowest | 9,660.2 | 5,614.9 | 3,333.9 |

Checksum `1bc2d4ff4b411ddc` in all nine runs: no result changed.

### Round 2: a month file is decoded once per process while it is unchanged (kept)

- **Hypothesis (journaled above before the change):** coverage, check and load decode the same files seconds apart; keeping decoded months in the `Store` makes check and load nearly free.
- **Change:** `Store` holds an `Arc<MonthCache>`: entries keyed by file path and column set, valid while the file's length and modified time are those of the decoded file, dropped on every write or delete through the store, least recently used evicted past 256 MB. A field of `Store`, not a static.
- **Dev:** lowest 9,660.2 to 5,614.9 ms, 42% less; 60%, 50% and 42% less in the three passes. Far above the noise rule (10%).
- **Phases of the Lab backtest:** check 1,025 to 2,877 ms became 100 to 131 ms; load 1,128 to 3,412 ms became 320 to 497 ms. Coverage and the reopen stayed cold, as predicted.
- **Guardrails:** checksums identical. CI on the pushed head.

### Round 3: a range's months are decoded in parallel (kept)

- **Hypothesis:** the cold reads decode 24 to 60 files one after another on one core.
- **Change:** `Store::read_months` decodes the months not in memory with rayon, in month order, one result per month so the first error is the one the old loop gave. Used by `coverage`, `load_candles`, `load_asks`, and the month loops of `read_data` and `check_run_data` in td-server.
- **Dev:** lowest 5,614.9 to 3,333.9 ms, 41% less; 22%, 53% and 39% less in the three passes.
- **Phases:** coverage 917 to 1,699 ms became 158 to 201 ms; the reopen 1,402 to 1,789 ms became 353 to 1,084 ms. The best Lab repetition was 935 ms against 9,781 ms for the base in the same pass.
- **Guardrails:** checksums identical. The run check now holds a whole range in memory at once instead of one month at a time (marked `shortcut:`).
- **Taught:** even with every core busy, parallel decoding still gained a third or more.

Against the target (at most 2,436 ms, half of the quiet baseline 4,872.2 ms): load only adds time, so any run of rounds 2 and 3 at or under 2,436 ms meets it. Not yet seen under this load; more runs follow when the machine is quieter.
