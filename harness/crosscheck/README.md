# Cross-check against NautilusTrader

This harness runs the same strategy on the same bars in our engine and in [NautilusTrader](https://nautilustrader.io), then compares fills, fees, funding, the equity curve and the headline metrics under the agreement rule of issue #29. Nautilus is only an independent reference for this harness. It is not a dependency of the engine or the apps, and the harness is not part of CI.

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

- **Gapless bars.** A copy of the bars with every open set to the previous close, so a market order gets the same price in both engines. Fills must pair one to one with the same decision bar, side and quantity; prices within one tick; fees within 1e-9 relative; funding per bar within 1e-9 relative; the equity at every bar and at the end within 1e-6 relative.
- **Real bars.** Fills must pair one to one, each price difference must stay within `|open[N+1] - close[N]|` of the bar the order was decided on, and the final equity difference within the sum of bound 1, `qty * |open[N+1] - close[N]| * (1 + taker)` over the fills. The `(1 + taker)` factor is the fee charged on that price difference.

Both sections end with the metrics computed with our formulas from each engine's equity curve. "Differences and their reasons" lists every difference the report found with its explanation; anything listed there as unexplained fails the case.

## How the two engines are lined up

- **Bars.** Both engines read the bars exported from our store. Nautilus gets them as external LAST bars stamped at their close and matches each one open, high, low, close (its default ordering) before the strategy sees it. Funding stamps are snapped to the nearest bar boundary when exported.
- **Fills.** Slippage is 0 in both, and Nautilus runs with `FillModel(prob_fill_on_limit=1.0, prob_slippage=0.0)`. A market order decided at bar N's close fills at bar N's close in Nautilus and at bar N+1's open in ours, which is expected difference 1. Fills are therefore paired by the bar they were decided on, not by their time. An order decided on the last bar never fills in our engine, so the Nautilus strategies make no decision on the last bar.
- **Fees.** The case's maker and taker rates are set on the Nautilus instrument and charged by its default maker/taker fee model. Nautilus rounds a commission to the 8 decimals of its money type; a fee that differs by less than half of that unit is reported as explained.
- **Accounts.** Spot runs use a Nautilus cash account, perpetuals a margin account at 10x, which the strategies never come near. Equity on the Nautilus side is the account's USDT balance plus the base asset at the close (spot), or plus the open position's unrealised P&L from Nautilus's own position (perpetual).
- **Funding.** Nautilus 1.231 has no funding settlement for perpetuals in a backtest, so the harness charges it into the Nautilus account through `SimulatedExchange.adjust_account`, the hook Nautilus's own FX rollover module uses, at our engine's stamps and on our convention: a stamp at or before a bar's open is charged on the position held going into the bar at the open price, and a stamp inside the bar on the position after the bar's fills at the close. The position is Nautilus's own.
- **Strategies.** `sma_cross` is our registered strategy. Buy and hold and short and hold (`crosscheck_buy_and_hold`, `crosscheck_short_and_hold`) exist only in the example, so they never appear in the apps' strategy list: each sends one market order at the first bar's close for `fraction` of equity net of the taker fee, then holds. The Python versions repeat the Rust decisions step by step, including the rounding down to the lot and the order checks our engine makes at submission (minimum size and notional). They sum moving averages in a plain loop because Python's `sum` compensates rounding and can differ in the last bit.

## Adding a case

Write `cases/caseNN.json` like the others and run `uv run python run.py --case caseNN`. A new strategy needs a Rust version (registered, or in the example like the hold strategies) and a Python one in `CrosscheckStrategy.decide`.

Lane 2's cases need three things the comparison already takes as arguments: the bars where a stop and a target (or an entry and its stop) are both reachable, which `compare_gapless` leaves out of the exact comparison (`ambiguous_bars`), with a check still to write that our P&L on them is at most Nautilus's; bound 2 and bound 3 (`bound2`, `bound3` in `compare_real`); and the stop, limit and stop-limit orders on both sides, with limit fills priced against `|limit - open|` for difference 3.
