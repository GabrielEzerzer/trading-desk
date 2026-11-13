# Trading Desk

Trading Desk is the workspace I use for my own trading: a desktop app that backtests trading strategies on historical market data and paper-trades the same strategy code on live prices. It is one Electron app with two sections on a shared Rust engine. Strategy Lab backtests and tests strategies on historical data; Trading shows live charts and paper-trades the same strategy code. It covers crypto spot and perpetual futures on Binance and Bybit, live and historical, and historical FX pairs from OANDA.

![Strategy Lab showing the result of an SMA cross backtest on BTCUSDT from January 2023 to October 2025: the return, CAGR, Sharpe and drawdown, the equity curve against buy and hold, and the metrics table](assets/lab-result.png)

![Trading showing the live one-minute BTCUSDT chart on Binance spot, with the last few hours of history and the paper order panel](assets/trading.png)

Choosing a strategy in Strategy Lab and running it, through to the result. The wait while the run loads the data is shown at double speed.

![Choosing SMA cross in Strategy Lab, starting the run and watching it go through its steps until the result appears](assets/lab-run.gif)

## Status

Backtesting and paper trading only. The app never sends a real order to any exchange, and nothing in it is financial advice. It is a personal project that changes often.

## Building and running

You need:

- the Rust toolchain named in `rust-toolchain.toml` (rustup installs it on the first build),
- Node 24 and pnpm 10.

Then, from `gui/`:

- `pnpm install` installs the GUI workspace.
- `pnpm dev` builds the engine optimised (`cargo build --release -p td-server --bin td-server`) and opens the app with hot reload. The first build takes several minutes; later ones rebuild only what changed.
- `pnpm build` makes the production bundle in `app/out`, and `pnpm test` runs the GUI tests.

The engine's own tests run with `cargo test --workspace` from the repository root. `gui/README.md` has the rest of the GUI commands, including the screenshot and UI checks.

### Market data

The app needs candles before it can backtest. Download them with the `td-data` command line tool, for example two years of one-minute BTCUSDT spot candles from Binance:

```
cargo run --release -p td-data -- download --venue binance --kind spot --symbol BTCUSDT --from 2023-01-01 --to 2025-01-01
```

The `instruments` subcommand lists what a venue has open for trading, `stored` lists what you have downloaded and how much space it takes, and `delete` removes it. Strategy Lab can also download the history a strategy needs from inside the app.

In the app, Strategy Lab's "Stored data" view, at the foot of the strategy list, shows the data folder, its size and each instrument's months, and deletes one instrument, a range of its months or all market data; saved runs and paper sessions stay.

The data folder is `TD_DATA_DIR` when that environment variable is set. Otherwise it is `%LOCALAPPDATA%\trading-desk` on Windows, and `$XDG_DATA_HOME/trading-desk` or `~/.local/share/trading-desk` elsewhere. It lives outside the repository, and market data is never committed.

### Data sources

Crypto prices come from Binance and Bybit public market data, which needs no account: Binance's monthly archives on data.binance.vision, both venues' public REST APIs and their public live streams. FX pairs come from OANDA, which needs the API token of a free OANDA practice account in the `OANDA_API_TOKEN` environment variable. I keep mine in my secrets vault and start the tools through it, but any way of setting the environment variable works.

### FX history from OANDA

FX pairs (`--venue oanda --kind fx`) download bid and ask minutes from OANDA's REST API on a free practice account, from 2007 on. The desk's FX market hours follow the US daylight-saving rule in force since 2007, so the app refuses an earlier start; the CLI does not check it, so keep `--from` at 2007-01-01 or later. The download reads the account's API token from `OANDA_API_TOKEN`. Build first, then run the built program with the variable set, so the token is not in the environment of the build scripts cargo runs:

```
cargo build --release -p td-data
OANDA_API_TOKEN=<your token> target/release/td-data download --venue oanda --kind fx --symbol USDJPY --from 2016-01-01 --to 2026-11-01
```

A request returns at most 5,000 minutes, so a month of one pair takes six or seven requests. A minute with no price is filled at the last close, with volume 0; most such minutes fall around the New York close. A run of an hour or more with no price, such as a market holiday, is left missing and shows as a gap, and nothing is stored for the weekend, when the market is closed.

## Writing a strategy

A strategy is one Rust file with three parts. `crates/td-engine/src/strategies/sma_cross.rs` is the example to copy.

1. **The strategy.** An `entry()` function gives its name, a description and its parameters, and a struct implements the `Strategy` trait.
2. **The test plan** (`TestPlan`). It names the instruments, the timeframe and the period to test on, the backtest settings (starting cash, leverage cap, a slippage for any instrument that should not take the backtest's default of 2 bps on BTC and ETH and 5 bps elsewhere, and the account currency, which is the quote currency unless the plan names the base, as a dollar account trading USD/JPY does), the parameter grid to sweep, the walk-forward window lengths and an optional single out-of-sample split. The `evidence` field holds the common evidence tests from #35. Leave it at `EvidenceSettings::default()` unless the strategy needs stricter values. A plan can make these tests stricter. The one value it can lower is the permutation count, down to 49 for a quicker run. It cannot switch any test off or loosen any other value, and the plan is refused if it tries.
3. **The verdict rule** (`VerdictRule`). It is one of three kinds. `PropFirm` takes the firm's daily-loss and drawdown limits and its profit target. `Portfolio` is judged against `BuyAndHold` or `Cash`. `HighRisk` is judged on expectancy and risk of ruin. Each kind's limits default to #35's values. To change a single limit, name that field and fill the rest from the default, for example `PortfolioLimits { min_sharpe: 1.5, ..PortfolioLimits::default() }`.

A private strategy goes in `crates/td-engine/src/strategies/private/`. Git ignores that folder, so it can hold your own strategies, or a checkout of a separate private repo of them (`git clone <url> crates/td-engine/src/strategies/private`). Every `*.rs` file in it is compiled in by `crates/td-engine/build.rs` and listed after the examples. Nothing else needs editing, and the engine builds without the folder. A file name must be a Rust module name, such as `my_strategy.rs`, and not a Rust keyword such as `mod.rs` or a bare `_.rs`. An example strategy in this repo also needs a `pub mod` line in `strategies/mod.rs` and an entry in `registry.rs`.

`cargo test -p td-engine` checks every registered strategy's plan and rule, including the private ones when the folder is present.

## Checks

CI runs rustfmt, clippy with warnings denied, the workspace tests, a check that the generated TypeScript API types are up to date, and the benchmark. The benchmark step runs `td-bench` in release mode on its synthetic data and compares each split's results checksum with the value in `harness/evals/perf-checksums.txt`; it ignores the timings, which are too noisy on shared runners. The checksum leaves out the engine revision, so a revision bump alone does not move it. A change that means to move results updates that file in the same PR, beside the `ENGINE_REVISION` bump in `crates/td-engine/src/runner.rs`. To measure the values, run `cargo run --release -p td-server --bin td-bench -- --split dev --reps 1` and read `checksum=` on the `BENCH` line.

## License

Trading Desk is licensed under the GNU General Public License, version 3 only (GPL-3.0-only). See [LICENSE](LICENSE).
