# Full-send journal: Trading Desk backend half

Goal: build issues #10-#16 (map #1, closed) merged, one PR each, on green after reviewer + sol-loop.
Reading: away · done · feature · logic. Started 2026-11-10. Gabriel asleep; standing instruction: if an issue blocks a merge and gates the rest of the build, go with my recommendation and list it in the final executive summary.

Order: #10 → (#11 ∥ #12) → (#13 ∥ #15 once #11+#12) → #14 → #16. Progress = open/closed build issues on the tracker.

## Decisions made for Gabriel
- Repo name `trading-desk` (umbrella for both apps).
- Prices and quantities are f64, rounded to tick and lot size when an order is submitted. Timestamps are i64 milliseconds UTC. Reason: speed in sweeps; venue data is in ms.
- Crates under `crates/`: td-core, td-engine, td-data, td-server. Rust stable 1.99, edition 2024.
- Workers are native Opus subagents in worktrees under D:\code\.worktrees (T3 delegation was declined at the capabilities call).

## Rounds
- #10: worker started in D:\code\.worktrees\trading-desk-b1-workspace (branch b1-workspace, base 6c8823c).

## Open
