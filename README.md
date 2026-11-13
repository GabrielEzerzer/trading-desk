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

### Installing

There is an installer for Windows. A GitHub release will attach it later; until then, build it from `gui/` with `pnpm dist`, which builds the engine, the app and the installer and writes `gui/app/dist/Trading Desk Setup <version>.exe`. The installer ships the engine built from your checkout, so if `crates/td-engine/src/strategies/private/` exists, your private strategies are compiled into it: do not share an installer built that way. Release installers are built from a clean checkout with no private folder.

The installer installs the app for the current Windows user only, so it needs no administrator rights. It puts the app, with the engine beside it, in `%LOCALAPPDATA%\Programs\trading-desk`, and adds a Start menu entry and a desktop shortcut. The installer is not signed, so the first time it runs Windows SmartScreen says it protected your PC; choose "More info", then "Run anyway". The app does not update itself: to update, run a newer installer.

The installed app keeps downloaded market data and saved runs in the data folder described under "Market data" below, `%LOCALAPPDATA%\trading-desk` unless `TD_DATA_DIR` is set, and creates what it needs there on first use. Its browser profile (caches and local storage) is in `%APPDATA%\Trading Desk`. For FX downloads, set `OANDA_API_TOKEN` as a user environment variable before starting the app.

Uninstall it from Settings > Apps > Installed apps. Uninstalling removes the app but leaves both folders, so your data is still there after a reinstall. To remove the data as well, delete `%LOCALAPPDATA%\trading-desk` and `%APPDATA%\Trading Desk` after uninstalling.

### Market data

The app needs candles before it can backtest. Download them with the `td-data` command line tool, for example two years of one-minute BTCUSDT spot candles from Binance:

```
cargo run --release -p td-data -- download --venue binance --kind spot --symbol BTCUSDT --from 2023-01-01 --to 2025-01-01
```

The `instruments` subcommand lists what a venue has open for trading, `stored` lists what you have downloaded and how much space it takes, and `delete` removes it. Strategy Lab can also download the history a strategy needs from inside the app.

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

### Holding back recent data

To keep the latest months unseen while you work on a strategy, start the engine with `TD_DATA_CUTOFF` set to a UTC date written `YYYY-MM-DD`, for example `TD_DATA_CUTOFF=2026-06-30`. While it is set, every backtest, sweep, out-of-sample run, walk-forward run and test battery whose range ends after the end of that date is refused with an error that names the cutoff, before any bar is read. A range that ends at midnight UTC at the end of the cutoff date is allowed. Paper trading and the live chart are not affected.

The cutoff is shown in one place on the server, `GET /health` (`data_cutoff`), and the app shows it while it is in force. Each run saved under a cutoff records it, and `GET /runs/{id}` returns it as `data_cutoff`. A value that is not a real date, an empty one included, stops the engine at start with an error. For the final check on the held-back months, start the engine without the variable.

## Writing a strategy

A strategy is one Rust file with three parts. `crates/td-engine/src/strategies/sma_cross.rs` is the example to copy.

1. **The strategy.** An `entry()` function gives its name, a description and its parameters, and a struct implements the `Strategy` trait.
2. **The test plan** (`TestPlan`). It names the instruments, the timeframe and the period to test on, the backtest settings (starting cash, leverage cap, a slippage for any instrument that should not take the backtest's default of 2 bps on BTC and ETH and 5 bps elsewhere, the account currency, which is the quote currency unless the plan names the base, as a dollar account trading USD/JPY does, and an optional `fee_override` that every run pays in place of the instrument's own fees, such as a broker's commission per unit of an FX pair), the parameter grid to sweep, the walk-forward window lengths and an optional single out-of-sample split. The `evidence` field holds the common evidence tests from #35. Leave it at `EvidenceSettings::default()` unless the strategy needs stricter values. A plan can make these tests stricter. The one value it can lower is the permutation count, down to 49 for a quicker run. It cannot switch any test off or loosen any other value, and the plan is refused if it tries.

   The battery runs the same tests on every market, keyed on what the market's rules say it can do rather than on its name. On a market with quotes, such as FX, the luck test rebuilds each shuffled bar's ask with the spread of the bar that stood at that time, so a strategy that trades only in a tight-spread hour pays that hour's spread on every shuffled copy too, and the cost stress widens each bar's spread by the same multiple it applies to fees, the per-unit commission and slippage. The parameter plateau is judged out of sample: in each walk-forward window, the neighbours on the grid of the set the window chose run on that window's test bars, and each is compared with the walk-forward's own runs in the windows where it ran. When a market charges a financing cost the engine does not model yet, such as the FX overnight swap, and a trade holds a position through the point where it is charged, the verdict carries a warning naming that cost.
3. **The verdict rule** (`VerdictRule`). It is one of three kinds. `PropFirm` takes the firm's daily-loss and drawdown limits and its profit target; its limits set how many days the pass odds and the breach odds are simulated over (`pass_horizon_days` and `breach_horizon_days`, 90 each by default), so a challenge with no time limit can set a longer pass horizon. `Portfolio` is judged against `BuyAndHold` or `Cash`. `HighRisk` is judged on expectancy and risk of ruin. Each kind's limits default to #35's values. To change a single limit, name that field and fill the rest from the default, for example `PortfolioLimits { min_sharpe: 1.5, ..PortfolioLimits::default() }`.

Each saved verdict says what it was judged on, so a strategy tested over many runs cannot look better than it is. `GET /verdicts` lists, for each strategy and instrument:

- `code_matches`: whether the strategy's source file is the same as when the battery ran. The build hashes each strategy's own file, examples and private ones alike, and every run saves that hash. A run saved before hashes were recorded shows `null`, meaning unknown. The hash covers the strategy's file only, not shared helpers it calls, so a change to a helper alone leaves `code_matches` true.
- `stale`: true unless the engine, the plan and rule, and the code all still match.
- `loosened`: every limit of the verdict's rule that is looser than its kind's default, with both values. A lower minimum or a higher maximum is looser; a longer pass horizon and a shorter breach horizon are looser too. Tightened limits are not listed.
- `runs` and `code_versions`: how many batteries have run on the pair, and how many distinct versions of the code they ran. Both are keyed by the strategy's name, so a renamed strategy starts again from zero.

A private strategy goes in `crates/td-engine/src/strategies/private/`. Git ignores that folder, so it can hold your own strategies, or a checkout of a separate private repo of them (`git clone <url> crates/td-engine/src/strategies/private`). Every `*.rs` file in it is compiled in by `crates/td-engine/build.rs` and listed after the examples. Nothing else needs editing, and the engine builds without the folder. A file name must be a Rust module name, such as `my_strategy.rs`, and not a Rust keyword such as `mod.rs` or a bare `_.rs`. An example strategy in this repo also needs a `pub mod` line in `strategies/mod.rs` and an entry in `registry.rs`.

`cargo test -p td-engine` checks every registered strategy's plan and rule, including the private ones when the folder is present.

## Checks

CI runs rustfmt, clippy with warnings denied, the workspace tests, a check that the generated TypeScript API types are up to date, and the benchmark. The benchmark step runs `td-bench` in release mode on its synthetic data and compares each split's results checksum with the value in `harness/evals/perf-checksums.txt`; it ignores the timings, which are too noisy on shared runners. The checksum leaves out the engine revision, so a revision bump alone does not move it. A change that means to move results updates that file in the same PR, beside the `ENGINE_REVISION` bump in `crates/td-engine/src/runner.rs`. To measure the values, run `cargo run --release -p td-server --bin td-bench -- --split dev --reps 1` and read `checksum=` on the `BENCH` line.

## License

Trading Desk is licensed under the GNU General Public License, version 3 only (GPL-3.0-only). See [LICENSE](LICENSE).
