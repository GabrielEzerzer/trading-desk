# Trading Desk

Trading Desk is the workspace I use for my own trading: a desktop app that backtests trading strategies on historical market data and paper-trades the same strategy code on live prices. It is one Electron app with two sections on a shared Rust engine. Strategy Lab backtests and tests strategies on historical data; Trading shows live charts and paper-trades the same strategy code. It covers crypto spot and perpetual futures on Binance and Bybit, live and historical, and historical FX pairs from OANDA.

![Strategy Lab showing the result of an SMA cross backtest on BTCUSDT from January 2023 to October 2025: the return, CAGR, Sharpe and drawdown, the equity curve against buy and hold, and the metrics table](assets/lab-result.png)

![Trading showing the live one-minute BTCUSDT chart on Binance spot with the last few hours of history, a running Manual paper session, and the paper order panel with a 0.01 BTC market order ready to send](assets/trading.png)

Choosing a strategy in Strategy Lab and running it, through to the result. The wait while the run loads the data is shown at double speed.

![Choosing SMA cross in Strategy Lab, starting the run and watching it go through its steps until the result appears](assets/lab-run.gif)

## Status

Backtesting and paper trading only. The app and its sidecar never send an order; the `td-live` command sends orders only to a venue's demo-trading environment, never to a real-money endpoint, and that is fixed in code. Nothing in it is financial advice. It is a personal project that changes often.

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

There is an installer for Windows: download `Trading.Desk.Setup.<version>.exe` from the latest [GitHub release](https://github.com/GabrielEzerzer/trading-desk/releases/latest). To build it yourself, run `pnpm dist` from `gui/`, which builds the engine, the app and the installer and writes `gui/app/dist/Trading Desk Setup <version>.exe`. The installer ships the engine built from your checkout, so if `crates/td-engine/src/strategies/private/` exists, your private strategies are compiled into it: do not share an installer built that way. Release installers are built from a clean checkout with no private folder.

The installer installs the app for the current Windows user only, so it needs no administrator rights. It puts the app, with the engine beside it, in `%LOCALAPPDATA%\Programs\trading-desk`, and adds a Start menu entry and a desktop shortcut. The installer is not signed, so the first time it runs Windows SmartScreen says it protected your PC; choose "More info", then "Run anyway". The app does not update itself: to update, run a newer installer.

The installed app keeps downloaded market data and saved runs in the data folder described under "Market data" below, `%LOCALAPPDATA%\trading-desk` unless `TD_DATA_DIR` is set, and creates what it needs there on first use. Its browser profile (caches and local storage) is in `%APPDATA%\Trading Desk`. For FX downloads, the app needs `OANDA_API_TOKEN` in its own environment. Set it only for the app's process. In PowerShell, run `$env:OANDA_API_TOKEN = [Net.NetworkCredential]::new('', (Read-Host 'OANDA token' -AsSecureString)).Password` and paste the token, which is not shown. Then start the app from that same window with `& "$env:LOCALAPPDATA\Programs\trading-desk\Trading Desk.exe"`, or start it through your secrets manager. Do not save the token as a persistent user environment variable (with `setx` or the Settings dialog): every program you start afterwards would get it, including the build scripts of every crate and npm package you build.

Uninstall it from Settings > Apps > Installed apps. Uninstalling removes the app but leaves both folders, so your data is still there after a reinstall. To remove the data as well, delete `%LOCALAPPDATA%\trading-desk` and `%APPDATA%\Trading Desk` after uninstalling.

### Market data

The app needs candles before it can backtest. Download them with the `td-data` command line tool, for example two years of one-minute BTCUSDT spot candles from Binance:

```
cargo run --release -p td-data -- download --venue binance --kind spot --symbol BTCUSDT --from 2023-01-01 --to 2025-01-01
```

A download skips a month that is already stored with no minute missing, so running it again over the same range fetches only what is missing. The `instruments` subcommand lists what a venue has open for trading, and keeps the list in the data folder for a day; `stored` lists what you have downloaded and how much space it takes, and `delete` removes it. Strategy Lab can also download the history a strategy needs from inside the app, and a backtest whose range is not all stored downloads the missing part first, as a step of the same run.

In the app, Strategy Lab's "Stored data" view, at the foot of the strategy list, shows the data folder, its size and each instrument's months, and deletes one instrument, a range of its months or all market data; saved runs and paper sessions stay.

The data folder is `TD_DATA_DIR` when that environment variable is set. Otherwise it is `%LOCALAPPDATA%\trading-desk` on Windows, and `$XDG_DATA_HOME/trading-desk` or `~/.local/share/trading-desk` elsewhere. It lives outside the repository, and market data is never committed.

### Data sources

Crypto prices come from Binance and Bybit public market data, which needs no account: Binance's monthly archives on data.binance.vision, both venues' public REST APIs and their public live streams. FX pairs come from OANDA, which needs the API token of a free OANDA practice account in the `OANDA_API_TOKEN` environment variable. I keep mine in my secrets vault and start the tools through it, but any way of setting the environment variable works.

### FX history from OANDA

FX pairs (`--venue oanda --kind fx`) download bid and ask minutes from OANDA's REST API on a free practice account, from 2007 on. The desk's FX market hours follow the US daylight-saving rule in force since 2007, so the app refuses an earlier start; the CLI does not check it, so keep `--from` at 2007-01-01 or later. The download reads the account's API token from `OANDA_API_TOKEN`. Build first, then run the built program with the variable set, so the token is not in the environment of the build scripts cargo runs. Reading the token from a hidden prompt, as below, keeps it out of your shell history:

```
cargo build --release -p td-data
read -rs -p 'OANDA token: ' OANDA_API_TOKEN && echo
OANDA_API_TOKEN="$OANDA_API_TOKEN" target/release/td-data download --venue oanda --kind fx --symbol USDJPY --from 2016-01-01 --to 2026-11-01
```

A request returns at most 5,000 minutes, so a month of one pair takes six or seven requests. A minute with no price is filled at the last close, with volume 0; most such minutes fall around the New York close. A run of an hour or more with no price is left missing, and nothing is stored for the weekend, when the market is closed. The download records the spans OANDA returned nothing for in a `.closed.json` file beside the month; the first download of a month fetches all of it, so its record covers the whole month. A recorded span counts as closed if it holds at most 4 hours of open time, such as a late Sunday open, or at most 27 hours and overlaps the year-end holidays, 24 December to 2 January UTC, such as the 24 to 25 December closure, which runs a little over a day. A closed span is not a gap in coverage or in a run's data report, which lists it under `closures`. Any other recorded span is an outage. It stays a gap, and the month is not complete, so the next download over it fetches it again. A backtest is refused over a day or more of missing open time in a month with a record. A month downloaded before these records existed has none, so its holidays still show as gaps, and a backtest is refused only at two days of missing open time there. Download the month again to record its closures.

### Holding back recent data

To keep the latest months unseen while you work on a strategy, start the engine with `TD_DATA_CUTOFF` set to a UTC date written `YYYY-MM-DD`, for example `TD_DATA_CUTOFF=2026-06-30`. While it is set, every backtest, sweep, out-of-sample run, walk-forward run and test battery whose range ends after the end of that date is refused with an error that names the cutoff, before any bar is read. A range that ends at midnight UTC at the end of the cutoff date is allowed. Paper trading and the live chart are not affected.

The cutoff is shown in one place on the server, `GET /health` (`data_cutoff`), and the app shows it while it is in force. Each run saved under a cutoff records it, and `GET /runs/{id}` returns it as `data_cutoff`. A value that is not a real date, an empty one included, stops the engine at start with an error. Runs saved before the cutoff was set still open in full, held-back months included. For the final check on the held-back months, start the engine without the variable.

## Live sessions on a demo account

The `td-live` command runs a strategy on a venue's demo-trading account: it trades the same strategy code on live closed bars and sends its orders to the venue's demo-trading environment. It is the only way to start a live session. It reads its keys from the environment, so it runs through `hush run`:

```
hush run -e dev -- cargo run -p td-live --bin td-live -- start --id demo-1 --strategy sma_cross --params '{"fast":10,"slow":30}' --symbol BTCUSDT --timeframe 1h --daily-loss 0.03 --max-loss 0.08 --notional-cap 5000
```

The symbol must be a linear perpetual settled in USDT. The session is saved under `live/<id>/` in the data folder, or in the folder `--data-dir` names. The command prints one line per event and runs until the session ends or halts, or you press Ctrl-C. Run only one session on an account, and nothing else on it: `start` refuses while another session in the same data folder has not ended, names it, and says to end it with `td-live flatten`.

**The limits.** All three are required and have no defaults.

- `--daily-loss` is the most the account may lose in one trading day, as a share of the starting balance: 0.03 is 3%. The loss is measured from the equity at the start of the day. `--rollover` sets when the day ends: `utc` (the default, midnight UTC), `utc:<minutes>` for a time that many minutes after midnight UTC, or `new-york-17` for 17:00 in New York.
- `--max-loss` is the most the account may lose over the whole session, from the starting balance, as a share of it.
- `--notional-cap` is the largest open position allowed, as its value in USDT.

The starting balance is the account's equity when the session is created. Losses are measured from the equity the venue reports, open profit and loss included, never from the strategy's own figures. The limits and the starting balance are written into the session folder when the session is created, in `config.json` and in the stop file, `stops.json`, and `resume` takes no limits. A session is refused when the two files give different limits or a different starting balance, when `stops.json` is missing or cannot be read, when it is a link rather than a file, when it shows no fired stop but records a flatten, or when the day's starting equity it holds is already at or below the maximum-loss line. Some edits are not detected. Changing only the day's figures in `stops.json` can loosen the daily limit until the next rollover, and editing `stops.json` and the session's status file together can bring back a session whose stop fired. The session folder is the owner's, and a person or program that can rewrite its files can change a saved session.

The limits are checked against the venue before the first bar each time the session starts or resumes, after every bar, and every 15 seconds between bars. If the equity can be read but the open position cannot be valued, the loss limits are still checked, and the check counts as one that could not be read.

An order is judged by the position it would leave. It is refused before it is sent when that position's value would be above the cap and above the open position's value now, and the strategy sees an ordinary rejection with the reason. An order that only reduces the position is never refused by the cap, whether or not it is marked reduce-only. Two orders in one bar are counted together: since the first may or may not have filled by then, the second must pass both with the first added to the position and without it. Near the cap this can refuse an order that would fit, until the next bar begins, but it never lets through an order that takes the position past the cap. An order that would add to the position is also refused when the position cannot be read.

If the venue cannot be read for four checks in a row, the session halts and nothing is flattened, since the engine cannot know the position. From then on every order that is not reduce-only is refused; a protective stop for an entry that has already filled still goes out. After a halt or a fault with a position open, the command keeps running and checking the limits until a check reads the venue position as flat or you press Ctrl-C, and a stop that fires meanwhile is acted on as below.

**When a limit is reached**, the stop fires, and that is final. The engine saves the fired stop, halts the strategy and stops it sending, cancels every resting order of the session on the instrument, and closes the position with a reduce-only market order. The close is sized from the position the venue reports, so it closes the account's whole position on that instrument, not only what the session opened; only the cancels are limited to the session's own orders. The engine then reads the venue again and repeats this, a few seconds apart and for at most ten rounds, until the position is flat and no order of the session rests. If something is still open after that, the session is recorded as fired, not flat, with what is left, and the command exits with an error. If the fired stop cannot be saved, the flatten still goes ahead, the save is tried again during it, and the command reports a save that never went through. `resume` refuses a session whose stop fired, so it does not trade again unless its files are edited as described above.

**The other commands.**

- `resume --id <id>` runs a saved session again with the limits it was created with. If its stop fired and the flatten did not finish, it finishes the flatten instead, and the session stays halted.
- `status --id <id>` prints the saved status and stop state. It does not contact the venue.
- `flatten --id <id>` cancels the session's resting orders, closes the account's position on the session's instrument with reduce-only orders and ends the session, which cannot be resumed afterwards. A session that faulted stays halted: `resume` refuses it, and `flatten` is how to close what it holds. If the session's `stops.json` is missing or cannot be read, the flatten still runs and writes a new `stops.json` that records it and says the old one could not be read. The new file does not record whether a stop had fired, and the session's status then says it was flattened by the owner. A link in place of `stops.json` is refused.

**Ctrl-C** stops the strategy cleanly and leaves the position and its resting orders, the protective stop included, at the venue. The limits are not enforced while the command is not running; `flatten` closes the position.

The command exits with 0 when Ctrl-C stops a running session or `flatten` ends flat on a session whose stop had not fired, 1 for a halt (also when Ctrl-C ends the checks that follow one), a fault or an error, 2 when a stop fired and the position was closed, and 3 when a flatten left something open at the venue or `stops.json` could not be saved when it ended.

## Writing a strategy

A strategy is one Rust file with three parts. `crates/td-engine/src/strategies/sma_cross.rs` is the example to copy.

1. **The strategy.** An `entry()` function gives its name, a description and its parameters, and a struct implements the `Strategy` trait.
2. **The test plan** (`TestPlan`). It names the instruments, the timeframe and the period to test on, the backtest settings (starting cash, leverage cap, a slippage for any instrument that should not take the backtest's default of 2 bps on BTC and ETH and 5 bps elsewhere, the account currency, which is the quote currency unless the plan names the base, as a dollar account trading USD/JPY does, and an optional `fee_override` that every run pays in place of the instrument's own fees, such as a broker's commission per unit of an FX pair), the parameter grid to sweep, the walk-forward window lengths and an optional single out-of-sample split. The `evidence` field holds the common evidence tests from #35. Leave it at `EvidenceSettings::default()` unless the strategy needs stricter values. A plan can make these tests stricter. The one value it can lower is the permutation count, down to 49 for a quicker run. It cannot switch any test off or loosen any other value, and the plan is refused if it tries.

   The battery runs the same tests on every market, keyed on what the market's rules say it can do rather than on its name. On a market with quotes, such as FX, the luck test rebuilds each shuffled bar's ask with the spread of the bar that stood at that time, so a strategy that trades only in a tight-spread hour pays that hour's spread on every shuffled copy too, and the cost stress widens each bar's spread by the same multiple it applies to fees, the per-unit commission and slippage. The parameter plateau is judged out of sample: in each walk-forward window, the neighbours on the grid of the set the window chose run on that window's test bars, and each is compared with the walk-forward's own runs in the windows where it ran. When a market charges a financing cost the engine does not model yet, such as the FX overnight swap, and a trade holds a position through the point where it is charged, the verdict carries a warning naming that cost.
3. **The verdict rule** (`VerdictRule`). It is one of three kinds. `PropFirm` takes the firm's daily-loss and drawdown limits and its profit target, and optionally a second phase's target (`second_target`), a minimum number of trading days per phase (`min_trading_days`), a best-day rule (`best_day_share`), a day cap (`day_target_share`) and the level a trailing floor stops rising at (`trailing_floor_lock`). The day cap limits what one day counts toward an evaluation phase's target to a share of that target: a winning day counts its profit up to the cap, a losing day counts its whole loss, and the phase passes once the counted profit reaches the target, while the loss limits still act on the account's real balance. A trading day is a day with a fill, unless the firm's own test (`trading_day`) is set: it can ask for a fill that closes a trade (`needs_close`), for the closed part to be worth at least a share of the starting balance (`min_trade_size`, the closed quantity at the average entry price, judged at the size the strategy was tested at, so it cannot be set with `attempt` terms traded at another size), and for it to have moved at least a share of that value before fees (`min_trade_move`). Each closing fill is judged on its own, and the same days count toward the minimum per phase and the wait before the first payout. Its limits set how many days the pass odds and the breach odds are simulated over (`pass_horizon_days` and `breach_horizon_days`, 90 each by default), so a challenge with no time limit can set a longer pass horizon. Each simulated day brings its close-to-close return and its lowest equity inside the day, both from the same past day (UTC days, as the daily returns use), and the daily loss limit and the maximum loss are checked on that low, since firms usually watch equity at every moment: a day that dips through a limit and closes back above it ends the attempt. A day's low is checked against the trailing floor that the closes before it set, as a firm that trails its floor on daily closes checks it. When the run has no equity inside its bars, the limits are checked at daily closes instead, and the odds lines of the verdict say so. The rule's optional `attempt` terms (`AttemptTerms`: account size, fees, the funded account's payout rules and horizon, the size traded in each, and a reduced-edge factor) make the battery price one attempt in money, and add a line that asks for the attempt to be worth more than zero with the mean daily return scaled by that factor. `Portfolio` is judged against `BuyAndHold` or `Cash`. `HighRisk` is judged on expectancy and risk of ruin. Each kind's limits default to #35's values. To change a single limit, name that field and fill the rest from the default, for example `PortfolioLimits { min_sharpe: 1.5, ..PortfolioLimits::default() }`.

Each saved verdict says what it was judged on, so a strategy tested over many runs cannot look better than it is. `GET /verdicts` lists, for each strategy and instrument:

- `code_matches`: whether the strategy's source file is the same as when the battery ran. The build hashes each strategy's own file, examples and private ones alike, and every run saves that hash. A run saved before hashes were recorded shows `null`, meaning unknown. The hash covers the strategy's own file only. Any other file it uses, a shared helper or a file of its own pulled in with `#[path]` or `include!`, is not hashed, so a change to that file alone leaves `code_matches` true.
- `stale`: true unless the engine, the plan and rule, and the code all still match.
- `loosened`: every limit of the verdict's rule that is looser than its kind's default, with both values. A lower minimum or a higher maximum is looser; a longer pass horizon and a shorter breach horizon are looser too. Tightened limits are not listed.
- `runs` and `code_versions`: how many batteries have run on the pair, and how many distinct versions of the code they ran. Both are keyed by the strategy's name, so a renamed strategy starts again from zero.

A private strategy goes in `crates/td-engine/src/strategies/private/`. Git ignores that folder, so it can hold your own strategies, or a checkout of a separate private repo of them (`git clone <url> crates/td-engine/src/strategies/private`). Every `*.rs` file in it is compiled in by `crates/td-engine/build.rs` and listed after the examples. Nothing else needs editing, and the engine builds without the folder. A file name must be a Rust module name, such as `my_strategy.rs`, and not a Rust keyword such as `mod.rs` or a bare `_.rs`. An example strategy in this repo also needs a `pub mod` line in `strategies/mod.rs` and an entry in `registry.rs`.

`cargo test -p td-engine` checks every registered strategy's plan and rule, including the private ones when the folder is present.

## Checks

CI runs rustfmt, clippy with warnings denied, the workspace tests, a check that the generated TypeScript API types are up to date, and the benchmark. The benchmark step runs `td-bench` in release mode on its synthetic data and compares each split's results checksum with the value in `harness/evals/perf-checksums.txt`; it ignores the timings, which are too noisy on shared runners. The checksum leaves out the engine revision, so a revision bump alone does not move it. A change that means to move results updates that file in the same PR, beside the `ENGINE_REVISION` bump in `crates/td-engine/src/runner.rs`. To measure the values, run `cargo run --release -p td-server --bin td-bench -- --split dev --reps 1` and read `checksum=` on the `BENCH` line.

## License

Trading Desk is licensed under the GNU General Public License, version 3 only (GPL-3.0-only). See [LICENSE](LICENSE).
