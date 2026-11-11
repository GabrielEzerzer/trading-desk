# Trading Desk

A trading workspace for my own trading, in two desktop apps on one shared Rust engine. Strategy Lab backtests and tests strategies on historical data; Trading View shows live charts and paper-trades the same strategy code. It grows out of Tipping Point's paper-trading view, but it is built for applied trading rather than for the paper.

## FX history from OANDA

FX pairs (`--venue oanda --kind fx`) download bid and ask minutes from OANDA's REST API on a free practice account, from 2005 on. The download reads the account's API token from `OANDA_API_TOKEN`, so it runs through hush, for example:

```
hush run -- cargo run --release -p td-data -- download --venue oanda --kind fx --symbol USDJPY --from 2016-01-01 --to 2026-11-01
```

A request returns at most 5,000 minutes, so a month of one pair takes six or seven requests. Minutes with no price are filled at the last close, and nothing is stored for the weekend, when the market is closed.

## Writing a strategy

A strategy is one Rust file with three parts. `crates/td-engine/src/strategies/sma_cross.rs` is the example to copy.

1. **The strategy.** An `entry()` function gives its name, a description and its parameters, and a struct implements the `Strategy` trait.
2. **The test plan** (`TestPlan`). It names the instruments, the timeframe and the period to test on, the backtest settings (starting cash, leverage cap, a slippage for any instrument that should not take the backtest's default of 2 bps on BTC and ETH and 5 bps elsewhere, and the account currency, which is the quote currency unless the plan names the base, as a dollar account trading USD/JPY does), the parameter grid to sweep, the walk-forward window lengths and an optional single out-of-sample split. The `evidence` field holds the common evidence tests from #35. Leave it at `EvidenceSettings::default()` unless the strategy needs stricter values. A plan can make these tests stricter. The one value it can lower is the permutation count, down to 49 for a quicker run. It cannot switch any test off or loosen any other value, and the plan is refused if it tries.
3. **The verdict rule** (`VerdictRule`). It is one of three kinds. `PropFirm` takes the firm's daily-loss and drawdown limits and its profit target. `Portfolio` is judged against `BuyAndHold` or `Cash`. `HighRisk` is judged on expectancy and risk of ruin. Each kind's limits default to #35's values. To change a single limit, name that field and fill the rest from the default, for example `PortfolioLimits { min_sharpe: 1.5, ..PortfolioLimits::default() }`.

A private strategy goes in `crates/td-engine/src/strategies/private/`. That folder is a checkout of the private `trading-desk-strategies` repo, and git ignores it here. Clone it with `git clone <url> crates/td-engine/src/strategies/private`. Every `*.rs` file in it is compiled in by `crates/td-engine/build.rs` and listed after the examples. Nothing else needs editing, and the engine builds without the folder. A file name must be a Rust module name, such as `my_strategy.rs`, and not a Rust keyword such as `mod.rs` or a bare `_.rs`. An example strategy in this repo also needs a `pub mod` line in `strategies/mod.rs` and an entry in `registry.rs`.

`cargo test -p td-engine` checks every registered strategy's plan and rule, including the private ones when the folder is present.
