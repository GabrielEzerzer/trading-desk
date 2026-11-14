# Trading Desk

A workspace for my own trading: one Electron desktop app with two sections on one Rust engine. Strategy Lab backtests strategies on historical data and runs the strategy tester; Trading shows live charts and paper-trades the same strategy code. The engine runs as a sidecar process that the app talks to over localhost. Markets are crypto spot and perpetuals on Binance and Bybit, and FX history from OANDA. The plan and its decisions are on the map issue, #1.

## Layout

- `crates/td-core`: the shared types every crate builds against (venue, instrument, timeframe, candle, order, fill) and the rounding of prices and quantities to tick and lot size.
- `crates/td-engine`: the `Strategy` trait, the event-driven simulation on candles, portfolio, metrics and the strategy tester.
- `crates/td-data`: venue clients, history download, the candle store and live feeds.
- `crates/td-server`: the localhost API binary the app starts as a sidecar.
- `crates/td-orders`: the order route to Bybit's demo-trading REST API for USDT perpetuals: market, reduce-only market and resting reduce-only stop orders, cancel and amend, and the reads a broker reconciles from. It can reach only the demo host and 127.0.0.1; the crate header lists its guards. `examples/demo-smoke.rs` sends real demo orders and runs only with `--confirm-send-orders`.
- `crates/td-live`: `VenueBroker`, the engine `Broker` that sends a session's orders through `td-orders` and reports the venue's fills, with a per-bar record for replay and a fault on any disagreement with the venue; and `LiveSession`, a strategy run on live closed bars through it, saved under `<data_dir>/live/<id>/` and resumed after a restart by replaying its log without touching the venue. A library for now: nothing starts it yet.
- `mock/bybit` (`td-mock-bybit`): a stateful mock of Bybit's v5 REST API on 127.0.0.1 that `td-orders` and `td-live` test against, with fault injection and the documented response examples in `fixtures/`. Test harness only.
- `gui/shared`: the shared GUI package. For now it holds `src/api-types.ts`, generated from td-server's types with `cargo run -p td-server --bin gen-api-types`; never edit it by hand.

## Conventions

- Prices and quantities are `f64`. They are rounded to the instrument's tick and lot size when an order is submitted, with `Instrument::round_price` and `Instrument::round_qty`, then checked with `Instrument::validate_order`.
- Timestamps are `i64` milliseconds since the Unix epoch, UTC (`td_core::Millis`).
- Strategies are Rust, behind the one `Strategy` trait. Each is one file with its test plan and verdict rule; private ones go in the git-ignored `crates/td-engine/src/strategies/private/` (README, "Writing a strategy").
- Tests never touch the network. Use fixtures.
- New logic gets at least one small test.
- Never commit market data. The data directory lives outside the repo.
- Secrets live in the hush vault (`hush.toml`), never in the repo.

## Checks

CI runs `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` on GitHub for every push to a branch other than main. That run is the gate; don't treat a local run as one. Running a single test locally while debugging is fine.
