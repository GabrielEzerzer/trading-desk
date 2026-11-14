//! The `td-live` command: starts a live session on a venue's demo-trading environment with its
//! stop rules, and keeps it running until it halts or the owner stops it.
//!
//! The parser takes no host, endpoint, key or secret: the binary builds the order route's client
//! from `td_orders::DemoKeys` in the environment and the demo endpoint only. [`execute`] runs one
//! parsed command against an [`Env`] the binary (or a test) builds, writes one line per event to
//! `out`, and returns the exit code.
//!
//! `start` links the session to what it was started from, as a paper session is linked
//! ([`td_engine::session_link`]): the strategy's code hash, plan and rule, and the saved verdict
//! `--verdict` names, or else the strategy's latest verdict on the instrument, read from the runs
//! folder under the data folder. A verdict judged with reference instruments is refused before
//! anything is written or sent: a live session feeds its strategy none. `status` shows the link
//! with its checks against the strategy as this `td-live` build has it ([`td_engine::LinkState`]),
//! and, with a verdict, a check of what the session started with against that verdict.
//!
//! Text read from the data folder (a saved session's config, a run record) is printed through
//! `clean`, so a control character in a file cannot reach the owner's terminal.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use td_core::{Instrument, InstrumentId, InstrumentKind, Timeframe, Venue};
use td_data::live::{Rest, SharedStream};
use td_engine::{
    DayRollover, ENGINE_REVISION, LinkState, LinkedVerdict, Params, RUNS_DIR, SessionLink,
    StopRules, default_slippage_bps, instrument_view,
};
use td_orders::{Client, SETTLE_COIN};

use crate::guard::{
    GuardTiming, Outcome, StopHandle, flatten_session, flatten_unfinished, run_guarded, saved_files,
};
use crate::session::{
    BuildStrategy, CONFIG_FILE, LiveConfig, LiveError, LiveSession, MAX_TEXT, SavedConfig, clean,
    read_json, refuse_unended, session_dir,
};

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
    /// Compares a session's fills with a backtest over its own saved bars; touches no venue.
    Compare(SessionArgs),
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
    /// The saved verdict the session is started from, by its run id; the default is the
    /// strategy's latest verdict on the instrument, or none when it has none there.
    #[arg(long)]
    pub verdict: Option<String>,
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

/// `rollover` as `--rollover` takes it.
fn rollover_flag(rollover: DayRollover) -> String {
    match rollover {
        DayRollover::Utc { offset_minutes: 0 } => "utc".into(),
        DayRollover::Utc { offset_minutes } => format!("utc:{offset_minutes}"),
        DayRollover::NewYork1700 => "new-york-17".into(),
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
            // An error can carry text from the data folder, such as a run record's.
            out(&format!("error: {}", clean(&e, MAX_TEXT)));
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
            // Before the venue is asked anything, so a refused verdict sends nothing.
            let linked_on = InstrumentId {
                venue: Venue::Bybit,
                symbol: args.symbol.clone(),
                kind: InstrumentKind::Perpetual,
            };
            let link = start_link(&data, &args, &linked_on)?;
            let client = (env.connect)()?;
            let config = start_config(&args, &client)?;
            // `Client::instrument_info` already refuses an answer for another symbol; this keeps
            // the link and the session on one instrument should that change.
            if config.instrument.id != linked_on {
                return Err(format!(
                    "the venue lists {} as {}; give the symbol as the venue writes it",
                    args.symbol, config.instrument.id.symbol
                ));
            }
            let live = LiveSession::create(&data, &args.id, config, link, client, env.build)
                .map_err(|e| e.to_string())?;
            out(&format!(
                "created session {} at {}",
                args.id,
                live.dir().display()
            ));
            out(&limits(&live));
            Ok(guarded(live, env, out))
        }
        Command::Resume(args) => {
            let data = data_dir(args.data_dir)?;
            let client = (env.connect)()?;
            match LiveSession::open(&data, &args.id, client.clone(), env.build) {
                Ok(live) => {
                    out(&format!("resumed session {}", args.id));
                    out(&limits(&live));
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
            let dir = session_dir(&data, &args.id).map_err(|e| e.to_string())?;
            let saved: SavedConfig =
                read_json(&dir.join(CONFIG_FILE)).map_err(|e| e.to_string())?;
            let pretty = |v| serde_json::to_string_pretty(&v).unwrap_or_default();
            out(&format!("status: {}", pretty(status)));
            out(&format!("stops: {}", pretty(stops)));
            let config = &saved.config;
            let link = saved.link.as_ref();
            for line in link_lines(
                &config.strategy,
                &config.instrument.id,
                link,
                saved.engine_revision,
            ) {
                out(&line);
            }
            Ok(0)
        }
        Command::Flatten(args) => {
            let data = data_dir(args.data_dir)?;
            let client = (env.connect)()?;
            flatten(&data, &args.id, &client, env, out)
        }
        Command::Compare(args) => {
            let data = data_dir(args.data_dir)?;
            crate::compare::compare(&data, &args.id, env.build, out)?;
            Ok(0)
        }
    }
}

/// The line `start` and `resume` print with the limits the session runs with, so an edit of its
/// saved files is seen. It reads the stop state the guard checks, which `resume` restores only
/// when `config.json` gives the same rules and starting balance. It holds no key and no equity
/// but the starting balance.
fn limits(live: &LiveSession) -> String {
    let state = &live.stops.state;
    let rules = state.rules();
    format!(
        "limits: daily loss {} and maximum loss {} of the starting balance of {} {SETTLE_COIN}, \
         notional cap {} {SETTLE_COIN}, day rollover {}",
        rules.daily_loss_share,
        rules.max_loss_share,
        state.starting_balance(),
        rules.open_notional_cap,
        rollover_flag(rules.rollover),
    )
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

/// The link of a session started with `args` on `instrument`: from the verdict `--verdict` names,
/// or the strategy's latest on the instrument. `None` for a strategy that is not registered, which
/// has no saved verdicts; naming a verdict for one is refused.
fn start_link(
    data: &Path,
    args: &StartArgs,
    instrument: &InstrumentId,
) -> Result<Option<SessionLink>, String> {
    let Some(entry) = td_engine::strategies()
        .into_iter()
        .find(|e| e.name == args.strategy)
    else {
        return match &args.verdict {
            Some(_) => Err(format!(
                "--verdict names a verdict, but {} is not a registered strategy, so it has none",
                args.strategy
            )),
            None => Ok(None),
        };
    };
    td_engine::session_link(
        &data.join(RUNS_DIR),
        &entry,
        instrument,
        args.verdict.as_deref(),
        None,
        "with --verdict",
    )
    .map(Some)
    .map_err(|e| e.to_string())
}

/// What a stale link's status line adds: the checks against the strategy are of this `td-live`
/// build, which need not be the one running the session.
const KEEPS_ITS_CODE: &str =
    "a running session keeps the code it was started with until it is restarted";

/// The status lines on what a session of `strategy` on `instrument` was started from. The checks
/// against the strategy ([`LinkState`]) are of the strategy as this `td-live` build has it, which
/// need not be the build running the session. With a verdict, a further check compares what the
/// session started with, from the saved link and `engine_revision` (the one saved in the
/// session's config) alone, with what that verdict judged ([`started_as_judged`]). Without one,
/// `engine_revision` is checked against this build's. The link is current only when every check
/// matches.
fn link_lines(
    strategy: &str,
    instrument: &InstrumentId,
    link: Option<&SessionLink>,
    engine_revision: u32,
) -> Vec<String> {
    let Some(link) = link else {
        return vec![
            "link: none recorded; the session was created before links were recorded, or its \
             strategy is not registered"
                .into(),
        ];
    };
    let state = LinkState::of(link, strategy, instrument, &td_engine::strategies());
    let check = |m: Option<bool>| match m {
        Some(true) => "matches",
        Some(false) => "does not match",
        None => "unknown",
    };
    // Read from config.json, which a person or program other than the owner may have written.
    let (shown_strategy, shown_instrument) = (
        clean(strategy, MAX_TEXT),
        clean(&instrument.to_string(), MAX_TEXT),
    );
    let code = clean(link.code_hash.as_deref().unwrap_or("none"), MAX_TEXT);
    let mut lines = Vec::new();
    match &link.verdict {
        Some(v) => {
            let state_name = serde_json::to_value(v.state)
                .ok()
                .and_then(|s| s.as_str().map(String::from))
                .unwrap_or_default();
            let stopped = if v.complete {
                ""
            } else {
                " (its luck test was stopped)"
            };
            lines.push(format!(
                "link: started from verdict {} of {shown_strategy} on {shown_instrument}: \
                 {state_name}{stopped}, judged by engine revision {}; the strategy's code hash \
                 was {code}",
                clean(v.run_id.as_str(), MAX_TEXT),
                v.engine_revision,
            ));
            let (started_plan, started_code) = started_as_judged(link, v, instrument);
            let started_engine = engine_revision == v.engine_revision;
            lines.push(format!(
                "link checks of what the session started with against the verdict: plan and \
                 rule {}, code {}, engine revision {}",
                check(started_plan),
                check(started_code),
                check(Some(started_engine)),
            ));
            lines.push(format!(
                "link checks of the verdict against the strategy as this td-live build has it: \
                 engine revision {}, plan and rule {}, code {}",
                check(state.engine_matches),
                check(state.plan_matches),
                check(state.code_matches),
            ));
            let current = !state.stale
                && started_plan == Some(true)
                && started_code == Some(true)
                && started_engine;
            lines.push(if current {
                "link: current".into()
            } else {
                format!(
                    "link: stale; what the session started with, or the code, plan or engine of \
                     the strategy as this td-live build has it, may not be what that verdict \
                     judged; {KEEPS_ITS_CODE}"
                )
            });
        }
        None => {
            // The link saves no engine revision; the session's config holds the one it was made
            // with.
            let engine = engine_revision == ENGINE_REVISION;
            lines.push(format!(
                "link: started with no saved verdict of {shown_strategy} on {shown_instrument}; \
                 the strategy's code hash was {code}"
            ));
            lines.push(format!(
                "link checks of what the session started with against the strategy as this \
                 td-live build has it: plan and rule {}, code {}, engine revision {}",
                check(state.plan_matches),
                check(state.code_matches),
                check(Some(engine)),
            ));
            lines.push(if state.stale || !engine {
                format!(
                    "link: stale; the code, plan or engine of the strategy as this td-live build \
                     has it may not be what the session started with; {KEEPS_ITS_CODE}"
                )
            } else {
                "link: current".into()
            });
        }
    }
    lines
}

/// Whether the session started with what `verdict` judged, from the saved `link` alone, so the
/// answer does not depend on the build that runs `status`: its plan and rule, as far as they
/// concern `instrument`, with the plan it started with still testing `instrument` (as
/// [`LinkState`] compares them), and its code hash, `None` when either hash is missing.
fn started_as_judged(
    link: &SessionLink,
    verdict: &LinkedVerdict,
    instrument: &InstrumentId,
) -> (Option<bool>, Option<bool>) {
    let plan = serde_json::to_value(instrument).ok().map(|id| {
        let tests_it = link
            .plan
            .get("instruments")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|tested| tested.contains(&id));
        link.rule == verdict.rule
            && tests_it
            && instrument_view(link.plan.clone(), &id) == instrument_view(verdict.plan.clone(), &id)
    });
    let code = link
        .code_hash
        .as_deref()
        .zip(verdict.code_hash.as_deref())
        .map(|(started, judged)| started == judged);
    (plan, code)
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

    #[test]
    fn rollover_prints_as_the_flag_takes_it() {
        for flag in ["utc", "utc:-60", "utc:1380", "new-york-17"] {
            assert_eq!(rollover_flag(parse_rollover(flag).unwrap()), flag);
        }
    }

    /// A session on an instrument the strategy's plan tests, linked to a verdict that this build
    /// matches in every check, started with exactly what the verdict judged.
    fn judged_link() -> (InstrumentId, SessionLink) {
        let entry = td_engine::find_strategy("sma_cross").expect("registered");
        let on = entry.plan.instruments[0].clone();
        let plan = serde_json::to_value(&entry.plan).unwrap();
        let rule = serde_json::to_value(entry.rule).unwrap();
        let hash = td_engine::code_hash(entry.name).map(String::from);
        let verdict = LinkedVerdict {
            run_id: td_engine::RunId::parse("1700000000000-0000000a").unwrap(),
            state: td_engine::VerdictState::Trade,
            complete: true,
            engine_revision: td_engine::ENGINE_REVISION,
            code_hash: hash.clone(),
            plan: plan.clone(),
            rule: rule.clone(),
        };
        let link = SessionLink {
            code_hash: hash,
            plan,
            rule,
            verdict: Some(verdict),
        };
        (on, link)
    }

    /// The engine revision a session made by this build saves.
    const ENGINE: u32 = td_engine::ENGINE_REVISION;

    /// `td-live status` may be another build than the one running the session, so its checks
    /// against the strategy do not show what the session started with; the saved link does.
    #[test]
    fn a_link_is_stale_when_the_session_started_with_other_code_or_plan_than_its_verdict_judged() {
        let (on, link) = judged_link();
        let lines = link_lines("sma_cross", &on, Some(&link), ENGINE);
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[3], "link: current");

        // This build still matches the verdict in every check.
        let started_elsewhere = SessionLink {
            code_hash: Some("0000000000000000".into()),
            ..link.clone()
        };
        let lines = link_lines("sma_cross", &on, Some(&started_elsewhere), ENGINE);
        assert_eq!(
            lines[1],
            "link checks of what the session started with against the verdict: plan and rule \
             matches, code does not match, engine revision matches"
        );
        assert_eq!(
            lines[2],
            "link checks of the verdict against the strategy as this td-live build has it: \
             engine revision matches, plan and rule matches, code matches"
        );
        assert!(lines[3].starts_with("link: stale; "), "{lines:?}");

        let mut replanned = link.clone();
        replanned.plan["period"]["from"] = 1.into();
        let lines = link_lines("sma_cross", &on, Some(&replanned), ENGINE);
        assert!(
            lines[1]
                .ends_with("plan and rule does not match, code matches, engine revision matches"),
            "{lines:?}"
        );
        assert!(lines[3].starts_with("link: stale; "), "{lines:?}");

        // A plan that no longer tests the session's instrument, though its view of it is the
        // verdict's, is not what the verdict judged for this session.
        let mut untested = link.clone();
        untested.plan["instruments"] = serde_json::json!([]);
        let lines = link_lines("sma_cross", &on, Some(&untested), ENGINE);
        assert!(
            lines[1].contains("plan and rule does not match, code matches"),
            "{lines:?}"
        );
        assert!(lines[3].starts_with("link: stale; "), "{lines:?}");

        // A session made by another engine revision than the verdict was judged by.
        let lines = link_lines("sma_cross", &on, Some(&link), ENGINE + 1);
        assert!(
            lines[1]
                .ends_with("plan and rule matches, code matches, engine revision does not match"),
            "{lines:?}"
        );
        assert!(lines[3].starts_with("link: stale; "), "{lines:?}");

        // A link saved without a code hash cannot show the code it started with.
        let unhashed = SessionLink {
            code_hash: None,
            ..link
        };
        let lines = link_lines("sma_cross", &on, Some(&unhashed), ENGINE);
        assert!(
            lines[1].ends_with("code unknown, engine revision matches"),
            "{lines:?}"
        );
        assert!(lines[3].starts_with("link: stale; "), "{lines:?}");
    }

    /// The link saves no engine revision without a verdict; the session's config holds the one it
    /// was made with, and that is checked against this build's.
    #[test]
    fn a_link_without_a_verdict_checks_the_engine_revision_the_session_was_made_with() {
        let (on, link) = judged_link();
        let unjudged = SessionLink {
            verdict: None,
            ..link
        };
        let lines = link_lines("sma_cross", &on, Some(&unjudged), ENGINE);
        assert_eq!(
            lines[1],
            "link checks of what the session started with against the strategy as this td-live \
             build has it: plan and rule matches, code matches, engine revision matches"
        );
        assert_eq!(lines[2], "link: current");

        let lines = link_lines("sma_cross", &on, Some(&unjudged), ENGINE + 1);
        assert!(
            lines[1].ends_with("engine revision does not match"),
            "{lines:?}"
        );
        assert!(
            lines[2].starts_with(
                "link: stale; the code, plan or engine of the strategy as this td-live build has \
                 it may not be what the session started with"
            ),
            "{lines:?}"
        );
    }
}
