# Cross-check against NautilusTrader

This harness runs the same strategy on the same bars in our engine and in [NautilusTrader](https://nautilustrader.io), then compares fills, fees, funding, round-trip trades, the equity curve and the headline metrics under the agreement rule of issue #29. Nautilus is only an independent reference for this harness. It is not a dependency of the engine or the apps, and the harness is not part of CI.

Lane 1 (this folder as it stands) covers cases 1 to 4 of the issue's table. Cases 5 to 10 (stops, limits, stop-limits, ambiguous bars, gaps) are lane 2; see "Adding a case" below.

## Layout

- `cases/caseNN.json`: one case each: strategy and parameters, venue, kind, symbol, timeframe, period, starting cash, maker and taker fees, and the export folder.
- `../../crates/td-engine/examples/crosscheck.rs`: our side, a Rust example with three commands. `fetch` downloads a case's candles and funding with td-data into this folder's own store (`data/store`, never the user's default store) and exports the bars, the gapless bars and the funding to `data/export/caseNN/`. `run` runs a case through `td_engine::run` (or `run_strategy`, for the two strategies only the cross-check uses) and writes JSON. `metrics` computes our metrics report for any equity curve.
- `nautilus_side.py`: the Nautilus side: the same case on the same exported bars, with the strategies written again in Python.
- `compare.py`: the agreement rule. `tests/test_compare.py` tests it offline on small hand-made results.
- `run.py`: runs everything and writes `reports/caseNN.md`, which are committed.
- `data/` (market data) and `out/` (both engines' raw results, curves and metrics) are git-ignored.

## Setup

You need the repo's Rust toolchain and [uv](https://docs.astral.sh/uv/). From this folder:

```
uv sync
```

That creates `.venv` with Python 3.12 and the pinned `nautilus_trader==1.231.0` (about 500 MB).

## Running

One command runs every case: it builds our example, fetches any case whose export is missing, runs both engines on the gapless and the real bars, compares them and writes the reports.

```
uv run python run.py
```

- One case, or a few: `uv run python run.py --case case03 --case case04`.
- Download and export the data again: add `--fetch`. The first fetch of all four cases takes a few minutes and stores about 45 MB under `data/`.
- The comparison logic's tests: `uv run pytest`.

The exit code is 1 if any case fails.

The downloads use the venues' public endpoints only (instrument lists, Binance's candle archive, Bybit's klines, both venues' funding history) through td-data's own client, which sends the plain `trading-desk` user agent and nothing that identifies anyone.

## Reading a report

Each report starts with the verdict, PASS or FAIL, then the setup (data, dropped buckets, funding stamps) and two sections:

- **Gapless bars.** A copy of the bars with every open set to the previous close, so a market order gets the same price in both engines. Fills must pair one to one with the same decision bar, side and quantity; prices within one tick; fees within 1e-9 relative; funding per bar within 1e-9 relative; the equity at every bar and at the end within 1e-6 relative. Round-trip trades must pair one to one by the decision bar of their entry fill and their side, close on the same decision bar (or both still be open at the end), and have the same quantity; entry and exit prices within one tick; fees within 1e-9 relative; P&L net of fees and before funding within 1e-6 relative; funding within 1e-9 of the total size of the charges that make it up. Trade fees and P&L may also differ by up to four units of the 8th decimal, the floor for a P&L near zero: Nautilus keeps money to 8 decimals and a trade's amounts pass through at most four roundings of half a unit (a flip's commission share, the rest of it, the exit commission, the realized P&L), so the floor is twice the worst case.
- **Real bars.** Fills must pair one to one, each price difference must stay within `|open[N+1] - close[N]|` of the bar the order was decided on, and the final equity difference within the sum of bound 1, `qty * |open[N+1] - close[N]| * (1 + taker)` over the fills. The `(1 + taker)` factor is the fee charged on that price difference. Trades must pair one to one as on the gapless bars, and each trade's P&L (net of fees, before funding) must stay within the bound 1 of its entry and exit fills at our quantity. Nautilus's P&L is scaled to our quantity first: the engines size from their own equity, so paired trades can differ by a lot, and that sizing difference (explained under the fills) is not a price difference bound 1 covers. The scaling is exact for a trade with one entry fill and one exit fill, which is every trade in cases 1 to 4; a Nautilus trade with more fills fails the check until the bound is summed per fill.

A case with fills but no paired trades fails, as a case with no paired fills does.

On both series every trade's exported net P&L must equal its `gross_pnl - fees - funding` (funding paid is positive) within 1e-6 relative or the money floor, in both engines' exports. The other trade checks compare P&L before funding, so they would not see a wrong net figure.

Both sections end with the metrics computed with our formulas from each engine's equity curve. "Differences and their reasons" lists every difference the report found with its explanation; anything listed there as unexplained fails the case.

## How the two engines are lined up

- **Bars.** Both engines read the bars exported from our store. Nautilus gets them as external LAST bars stamped at their close and matches each one open, high, low, close (its default ordering) before the strategy sees it. Funding stamps are snapped to the nearest bar boundary when exported.
- **Fills.** Slippage is 0 in both, and Nautilus runs with `FillModel(prob_fill_on_limit=1.0, prob_slippage=0.0)`. A market order decided at bar N's close fills at bar N's close in Nautilus and at bar N+1's open in ours, which is expected difference 1. Fills are therefore paired by the bar they were decided on, not by their time. An order decided on the last bar never fills in our engine, so the Nautilus strategies make no decision on the last bar.
- **Fees.** The case's maker and taker rates are set on the Nautilus instrument and charged by its default maker/taker fee model. Nautilus rounds a commission to the 8 decimals of its money type; a fee that differs by less than half of that unit is reported as explained.
- **Accounts.** Spot runs use a Nautilus cash account, perpetuals a margin account at 10x, which the strategies never come near. Equity on the Nautilus side is the account's USDT balance plus the base asset at the close (spot), or plus the open position's unrealised P&L from Nautilus's own position (perpetual).
- **Funding.** Nautilus 1.231 has no funding settlement for perpetuals in a backtest, so the harness charges it into the Nautilus account through `SimulatedExchange.adjust_account`, the hook Nautilus's own FX rollover module uses, at our engine's stamps and on our convention: a stamp at or before a bar's open is charged on the position held going into the bar at the open price, and a stamp inside the bar on the position after the bar's fills at the close. The position is Nautilus's own.
- **Trades.** Ours are the run's `trades` (`td_engine::trades::build_trades`). Nautilus's are its own positions, the rows of its positions report: the positions in the cache plus the snapshots a NETTING account takes of each closed position before it reuses the position ID. A position's entry and exit time are its opening and closing fills' times, its quantity the peak quantity, its prices Nautilus's average open and close prices, its fees its commissions, and its P&L Nautilus's realized P&L, which is already net of those commissions.
- **Flips.** A fill that takes the position through zero is one closed trade and one new trade in both engines: the open quantity closes the old trade at the fill's price, the rest opens the new one at the same price, and the fill's fee is shared between them in proportion to those quantities. Nautilus's execution engine splits the fill that way on a NETTING account (`_flip_position`), and our trade builder does the same, so no conversion is needed; the comparison pairs the new trade by the flip fill's decision bar.
- **Funding in trades.** Nautilus has no funding of its own, so its realized P&L excludes it, and the trade P&L is compared before funding on both sides (`gross_pnl - fees`). Funding is compared per trade on its own, on the gapless bars: ours is the trade builder's (funding at a bar's open to the trade held into the bar, later funding to the trade open after the bar's fills); on the Nautilus side the strategy records which Nautilus position each harness charge was made on, by the position's opening time, and a trade's funding is the sum of those charges. On the real bars funding per trade is not compared, since the positions differ in size; the totals are reported.
- **The end of the run.** Our engine runs here without the end-of-run close-out (`close_at_end: false`), so no trade is force-closed. A position still open after the last bar is reported by our engine as an open trade marked at the last close, without an exit fee, and Nautilus leaves its position open too. The Nautilus side reports that open position the same way: exit at the last bar, at its close, with P&L equal to its realized P&L (the entry commission) plus its unrealized P&L at that close. A trade open at the end in one engine but closed in the other fails the pairing.
- **Strategies.** `sma_cross` is our registered strategy. Buy and hold and short and hold (`crosscheck_buy_and_hold`, `crosscheck_short_and_hold`) exist only in the example, so they never appear in the apps' strategy list: each sends one market order at the first bar's close for `fraction` of equity net of the taker fee, then holds. The Python versions repeat the Rust decisions step by step, including the rounding down to the lot and the order checks our engine makes at submission (minimum size and notional). They sum moving averages in a plain loop because Python's `sum` compensates rounding and can differ in the last bit.

## Adding a case

Write `cases/caseNN.json` like the others and run `uv run python run.py --case caseNN`. A new strategy needs a Rust version (registered, or in the example like the hold strategies) and a Python one in `CrosscheckStrategy.decide`.

Lane 2's cases need three things the comparison already takes as arguments: the bars where a stop and a target (or an entry and its stop) are both reachable, which `compare_gapless` leaves out of the exact comparison (`ambiguous_bars`, with the trades entered or closed on them), with a check still to write that our P&L on them is at most Nautilus's; bound 2 and bound 3 (`bound2`, `bound3` in `compare_real`); and the stop, limit and stop-limit orders on both sides, with limit fills priced against `|limit - open|` for difference 3.
