# Trading Desk

A trading workspace for my own trading, in two desktop apps on one shared Rust engine. Strategy Lab backtests and tests strategies on historical data; Trading View shows live charts and paper-trades the same strategy code. It grows out of Tipping Point's paper-trading view, but it is built for applied trading rather than for the paper.

## FX history from OANDA

FX pairs (`--venue oanda --kind fx`) download bid and ask minutes from OANDA's REST API on a free practice account, from 2007 on. The desk's FX market hours follow the US daylight-saving rule in force since 2007, so the app refuses an earlier start; the CLI does not check it, so keep `--from` at 2007-01-01 or later. The download reads the account's API token from `OANDA_API_TOKEN`, so it runs through hush. Build first and give hush the built program, so the token is not in the environment of the build scripts cargo runs:

```
cargo build --release -p td-data
hush run -- target/release/td-data download --venue oanda --kind fx --symbol USDJPY --from 2016-01-01 --to 2026-11-01
```

A request returns at most 5,000 minutes, so a month of one pair takes six or seven requests. A minute with no price is filled at the last close, with volume 0; most such minutes fall around the New York close. A run of an hour or more with no price, such as a market holiday, is left missing and shows as a gap, and nothing is stored for the weekend, when the market is closed.

## Writing a strategy

A strategy is one Rust file with three parts. `crates/td-engine/src/strategies/sma_cross.rs` is the example to copy.

1. **The strategy.** An `entry()` function gives its name, a description and its parameters, and a struct implements the `Strategy` trait.
2. **The test plan** (`TestPlan`). It names the instruments, the timeframe and the period to test on, the backtest settings (starting cash, leverage cap, a slippage for any instrument that should not take the backtest's default of 2 bps on BTC and ETH and 5 bps elsewhere, the account currency, which is the quote currency unless the plan names the base, as a dollar account trading USD/JPY does, and an optional `fee_override` that every run pays in place of the instrument's own fees, such as a broker's commission per unit of an FX pair), the parameter grid to sweep, the walk-forward window lengths and an optional single out-of-sample split. The `evidence` field holds the common evidence tests from #35. Leave it at `EvidenceSettings::default()` unless the strategy needs stricter values. A plan can make these tests stricter. The one value it can lower is the permutation count, down to 49 for a quicker run. It cannot switch any test off or loosen any other value, and the plan is refused if it tries.

   The battery runs the same tests on every market, keyed on what the market's rules say it can do rather than on its name. On a market with quotes, such as FX, the luck test shuffles each bar's ask with it, and the cost stress widens each bar's spread by the same multiple it applies to fees, the per-unit commission and slippage. The parameter plateau is judged out of sample: in each walk-forward window, the neighbours on the grid of the set the window chose run on that window's test bars, and are compared with the walk-forward's own result. When a market charges a financing cost the engine does not model yet, such as the FX overnight swap, and a trade holds a position through the point where it is charged, the verdict carries a warning naming that cost.
3. **The verdict rule** (`VerdictRule`). It is one of three kinds. `PropFirm` takes the firm's daily-loss and drawdown limits and its profit target; its limits set how many days the pass odds and the breach odds are simulated over (`pass_horizon_days` and `breach_horizon_days`, 90 each by default), so a challenge with no time limit can set a longer pass horizon. `Portfolio` is judged against `BuyAndHold` or `Cash`. `HighRisk` is judged on expectancy and risk of ruin. Each kind's limits default to #35's values. To change a single limit, name that field and fill the rest from the default, for example `PortfolioLimits { min_sharpe: 1.5, ..PortfolioLimits::default() }`.

A private strategy goes in `crates/td-engine/src/strategies/private/`. That folder is a checkout of the private `trading-desk-strategies` repo, and git ignores it here. Clone it with `git clone <url> crates/td-engine/src/strategies/private`. Every `*.rs` file in it is compiled in by `crates/td-engine/build.rs` and listed after the examples. Nothing else needs editing, and the engine builds without the folder. A file name must be a Rust module name, such as `my_strategy.rs`, and not a Rust keyword such as `mod.rs` or a bare `_.rs`. An example strategy in this repo also needs a `pub mod` line in `strategies/mod.rs` and an entry in `registry.rs`.

`cargo test -p td-engine` checks every registered strategy's plan and rule, including the private ones when the folder is present.
