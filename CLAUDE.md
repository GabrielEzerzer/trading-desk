# Trading Desk

A private workspace for my own trading: two Electron desktop apps on one Rust engine. Strategy Lab backtests strategies on historical data and runs the strategy tester; Trading View shows live charts and paper-trades the same strategy code. The engine runs as a sidecar process that the apps talk to over localhost. Markets are crypto spot and perpetuals on Binance and Bybit. The plan and its decisions are on the map issue, #1.

## Layout

- `crates/td-core`: the shared types every crate builds against (venue, instrument, timeframe, candle, order, fill) and the rounding of prices and quantities to tick and lot size.
- `crates/td-engine`: the `Strategy` trait, the event-driven simulation on candles, portfolio, metrics and the strategy tester.
- `crates/td-data`: venue clients, history download, the candle store and live feeds.
- `crates/td-server`: the localhost API binary the apps start as a sidecar.

## Conventions

- Prices and quantities are `f64`. They are rounded to the instrument's tick and lot size when an order is submitted, with `Instrument::round_price` and `Instrument::round_qty`, then checked with `Instrument::validate_order`.
- Timestamps are `i64` milliseconds since the Unix epoch, UTC (`td_core::Millis`).
- Strategies are Rust, behind the one `Strategy` trait.
- Tests never touch the network. Use fixtures.
- New logic gets at least one small test.
- Never commit market data. The data directory lives outside the repo.
- Secrets live in the hush vault (`hush.toml`), never in the repo.

## Checks

CI runs `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` on GitHub for every push to a branch other than main. That run is the gate; don't treat a local run as one. Running a single test locally while debugging is fine.
