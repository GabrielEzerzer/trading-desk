//! The `td-live` command: starts a live session on a venue's demo-trading environment with its
//! stop rules, and keeps it running until it halts or the owner stops it.
//!
//! The parser takes no host, endpoint, key or secret: the binary builds the order route's client
//! from `td_orders::DemoKeys` in the environment and the demo endpoint only. [`execute`] runs one
//! parsed command against an [`Env`] the binary (or a test) builds, writes one line per event to
//! `out`, and returns the exit code.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use td_core::{Instrument, InstrumentId, InstrumentKind, Timeframe, Venue};
use td_data::live::{Rest, SharedStream};
use td_engine::{DayRollover, Params, StopRules, default_slippage_bps};
use td_orders::{Client, SETTLE_COIN};

use crate::guard::{
    GuardTiming, Outcome, StopHandle, flatten_session, flatten_unfinished, run_guarded, saved_files,
};
use crate::session::{BuildStrategy, LiveConfig, LiveError, LiveSession, refuse_unended};

/// Runs a strategy on a venue's demo-trading account with loss limits the engine enforces.
#[derive(Debug, Parser)]
#[command(name = "td-live", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Creates a session with its stop rules and runs it until it halts or Ctrl-C.
    Start(StartArgs),
    /// Runs a saved session again, with the stop rules it was created with.
    Resume(SessionArgs),
    /// Prints a session's saved status and stop state; touches no venue.
    Status(SessionArgs),
    /// Cancels a session's resting orders and closes its position, reduce-only, and ends it.
    Flatten(SessionArgs),
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// The session's id: its folder name under `<data-dir>/live/`.
    #[arg(long)]
    pub id: String,
    /// The strategy's registered name.
    #[arg(long)]
    pub strategy: String,
    /// The strategy's parameters as a JSON object, for example '{"fast":10,"slow":30}'.
    #[arg(long)]
    pub params: String,
    /// The venue's symbol of a linear perpetual settled in USDT, for example BTCUSDT.
    #[arg(long)]
    pub symbol: String,
    /// The bar length the strategy runs on, for example 1h.
    #[arg(long, value_parser = parse_timeframe)]
    pub timeframe: Timeframe,
    /// The most the account may lose in one trading day, as a share of the starting balance:
    /// 0.05 is 5%.
    #[arg(long)]
    pub daily_loss: f64,
    /// The most the account may lose in the whole session, as a share of the starting balance.
    #[arg(long)]
    pub max_loss: f64,
    /// The largest open notional allowed, in USDT.
    #[arg(long)]
    pub notional_cap: f64,
    /// When the trading day ends: `utc` (midnight UTC), `utc:<minutes after midnight UTC>`, or
    /// `new-york-17` (17:00 in New York).
    #[arg(long, value_parser = parse_rollover, default_value = "utc")]
    pub rollover: DayRollover,
    /// The data folder; the default is the desk's data folder.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SessionArgs {
    /// The session's id.
    #[arg(long)]
    pub id: String,
    /// The data folder; the default is the desk's data folder.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

fn parse_timeframe(s: &str) -> Result<Timeframe, String> {
    s.parse().map_err(|e| format!("{e}"))
}

fn parse_rollover(s: &str) -> Result<DayRollover, String> {
    match s {
        "utc" => Ok(DayRollover::Utc { offset_minutes: 0 }),
        "new-york-17" => Ok(DayRollover::NewYork1700),
        _ => s
            .strip_prefix("utc:")
            .and_then(|m| m.parse::<i32>().ok())
            .map(|offset_minutes| DayRollover::Utc { offset_minutes })
            .ok_or_else(|| "expected utc, utc:<minutes> or new-york-17".to_string()),
    }
}

/// A stream of an instrument's live bars, opened once the session has warmed up.
pub type OpenStream = Arc<dyn Fn(InstrumentId) -> Arc<SharedStream> + Send + Sync>;

/// What a command runs against. The binary builds the real one; tests point it at the mock.
pub struct Env {
    /// Builds the order route's client. Called only by the commands that touch the venue.
    pub connect: Box<dyn Fn() -> Result<Arc<Client>, String> + Send + Sync>,
    /// The venue's REST data for warm-up and backfill.
    pub rest: Arc<dyn Rest>,
    pub stream: OpenStream,
    pub build: BuildStrategy,
    /// The data folder when `--data-dir` is not given.
    pub data_dir: Box<dyn Fn() -> Result<PathBuf, String> + Send + Sync>,
    pub timing: GuardTiming,
    pub stop: StopHandle,
}

/// Runs `cli` and returns the exit code: 0 for a stop by the owner or a flatten that ended flat,
/// 1 for a halt, a fault or an error, 2 for a fired stop flattened, 3 for anything a flatten left
/// open at the venue.
pub fn execute(cli: Cli, env: &Env, out: &mut dyn FnMut(&str)) -> u8 {
    match run(cli, env, out) {
        Ok(code) => code,
        Err(e) => {
            out(&format!("error: {e}"));
            1
        }
    }
}

fn run(cli: Cli, env: &Env, out: &mut dyn FnMut(&str)) -> Result<u8, String> {
    let data_dir = |given: Option<PathBuf>| given.map_or_else(|| (env.data_dir)(), Ok);
    match cli.command {
        Command::Start(args) => {
            let data = data_dir(args.data_dir.clone())?;
            refuse_unended(&data).map_err(|e| e.to_string())?;
            let client = (env.connect)()?;
            let config = start_config(&args, &client)?;
            let live = LiveSession::create(&data, &args.id, config, client, env.build)
                .map_err(|e| e.to_string())?;
            out(&format!(
                "created session {} at {}",
                args.id,
                live.dir().display()
            ));
            Ok(guarded(live, env, out))
        }
        Command::Resume(args) => {
            let data = data_dir(args.data_dir)?;
            let client = (env.connect)()?;
            match LiveSession::open(&data, &args.id, client.clone(), env.build) {
                Ok(live) => {
                    out(&format!("resumed session {}", args.id));
                    Ok(guarded(live, env, out))
                }
                Err(LiveError::StopFired(fired)) => {
                    out(&format!(
                        "a stop fired, so the session never trades again: {fired}"
                    ));
                    if !flatten_unfinished(&data, &args.id).map_err(|e| e.to_string())? {
                        out("its flatten had finished flat; the session stays halted");
                        return Ok(2);
                    }
                    out("its flatten did not finish; finishing it");
                    flatten(&data, &args.id, &client, env, out)
                }
                Err(e) => Err(e.to_string()),
            }
        }
        Command::Status(args) => {
            let data = data_dir(args.data_dir)?;
            let (status, stops) = saved_files(&data, &args.id).map_err(|e| e.to_string())?;
            let pretty = |v| serde_json::to_string_pretty(&v).unwrap_or_default();
            out(&format!("status: {}", pretty(status)));
            out(&format!("stops: {}", pretty(stops)));
            Ok(0)
        }
        Command::Flatten(args) => {
            let data = data_dir(args.data_dir)?;
            let client = (env.connect)()?;
            flatten(&data, &args.id, &client, env, out)
        }
    }
}

fn guarded(live: LiveSession, env: &Env, out: &mut dyn FnMut(&str)) -> u8 {
    let (stream, id) = (
        env.stream.clone(),
        live.config().config.instrument.id.clone(),
    );
    let outcome = run_guarded(
        live,
        env.rest.clone(),
        move || stream(id),
        env.timing,
        &env.stop,
        out,
    );
    outcome.exit_code()
}

fn flatten(
    data: &Path,
    id: &str,
    client: &Client,
    env: &Env,
    out: &mut dyn FnMut(&str),
) -> Result<u8, String> {
    let outcome: Outcome =
        flatten_session(data, id, client, env.timing, out).map_err(|e| e.to_string())?;
    Ok(outcome.exit_code())
}

/// The session's config: the instrument from the venue's own listing, which must be a linear
/// perpetual that trades and settles in the route's settle coin, and the stop rules from the
/// flags. The engine's default slippage and the venue's default fees apply.
fn start_config(args: &StartArgs, client: &Client) -> Result<LiveConfig, String> {
    let params: Params = serde_json::from_str(&args.params)
        .map_err(|e| format!("--params is not a JSON object of parameter values: {e}"))?;
    let info = client
        .instrument_info(&args.symbol)
        .map_err(|e| e.to_string())?;
    if info.contract_type != "LinearPerpetual"
        || info.status != "Trading"
        || info.settle_coin != SETTLE_COIN
    {
        return Err(format!(
            "{} is not a trading linear perpetual settled in {SETTLE_COIN}",
            args.symbol
        ));
    }
    let fees = td_data::default_fees(Venue::Bybit, InstrumentKind::Perpetual)
        .map_err(|e| e.to_string())?;
    let instrument = Instrument {
        id: InstrumentId {
            venue: Venue::Bybit,
            symbol: info.symbol,
            kind: InstrumentKind::Perpetual,
        },
        base: info.base_coin,
        quote: info.quote_coin,
        tick_size: info.tick_size,
        lot_size: info.qty_step,
        min_qty: info.min_order_qty,
        min_notional: info.min_notional,
        fees,
    };
    Ok(LiveConfig {
        strategy: args.strategy.clone(),
        params,
        slippage_bps: default_slippage_bps(&instrument),
        instrument,
        timeframe: args.timeframe,
        fees: None,
        max_leverage: 1.0,
        stops: StopRules {
            daily_loss_share: args.daily_loss,
            max_loss_share: args.max_loss,
            open_notional_cap: args.notional_cap,
            rollover: args.rollover,
        },
    })
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    const START: [&str; 18] = [
        "td-live",
        "start",
        "--id",
        "demo-1",
        "--strategy",
        "sma_cross",
        "--params",
        r#"{"fast":10,"slow":30}"#,
        "--symbol",
        "BTCUSDT",
        "--timeframe",
        "1h",
        "--daily-loss",
        "0.03",
        "--max-loss",
        "0.08",
        "--notional-cap",
        "5000",
    ];

    #[test]
    fn start_parses_with_all_three_limits() {
        let cli = Cli::try_parse_from(START).expect("parses");
        let Command::Start(args) = cli.command else {
            panic!("not start");
        };
        assert_eq!(
            (args.daily_loss, args.max_loss, args.notional_cap),
            (0.03, 0.08, 5000.0)
        );
        assert_eq!(args.timeframe, Timeframe::H1);
        assert_eq!(args.rollover, DayRollover::Utc { offset_minutes: 0 });
    }

    #[test]
    fn start_is_refused_without_any_one_limit() {
        for flag in ["--daily-loss", "--max-loss", "--notional-cap"] {
            let at = START.iter().position(|a| *a == flag).unwrap();
            let args: Vec<_> = START
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != at && *i != at + 1)
                .map(|(_, a)| *a)
                .collect();
            let err = Cli::try_parse_from(args).expect_err(flag);
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{flag}"
            );
        }
    }

    #[test]
    fn resume_status_and_flatten_take_no_limits() {
        for command in ["resume", "status", "flatten"] {
            assert!(Cli::try_parse_from(["td-live", command, "--id", "demo-1"]).is_ok());
            assert!(
                Cli::try_parse_from(["td-live", command, "--id", "x", "--max-loss", "0.1"])
                    .is_err(),
                "{command}"
            );
        }
    }

    #[test]
    fn no_argument_names_a_host_an_endpoint_or_a_key() {
        let cli = Cli::command();
        let mut names = Vec::new();
        for sub in cli.get_subcommands() {
            for arg in sub.get_arguments() {
                names.push(arg.get_id().to_string());
                names.extend(arg.get_long().map(str::to_string));
            }
        }
        assert!(names.contains(&"symbol".to_string()));
        for name in names {
            for word in ["host", "url", "endpoint", "key", "secret", "base"] {
                assert!(!name.contains(word), "{name} names a {word}");
            }
        }
    }

    #[test]
    fn rollover_parses_the_three_forms() {
        assert_eq!(
            parse_rollover("utc:-60"),
            Ok(DayRollover::Utc {
                offset_minutes: -60
            })
        );
        assert_eq!(parse_rollover("new-york-17"), Ok(DayRollover::NewYork1700));
        assert!(parse_rollover("noon").is_err());
    }
}
