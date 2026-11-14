//! [`LiveSession`]: a strategy on a [`VenueBroker`], saved under `<data_dir>/live/<id>/` so it
//! survives a restart.
//!
//! The folder holds these files:
//! - `config.json`, written once at creation: the [`LiveConfig`], the starting balance read from
//!   the venue, the client-id prefix, the engine revision and the creation time ([`SavedConfig`]).
//! - `events.jsonl`, one [`Line`] per bar fed: the bar, the funding charged with it, whether it was
//!   a warm-up bar, and for a live bar the broker's record ([`VenueHandle::take_record`]). A live
//!   bar's line is synced to disk before the next bar is fed.
//! - `status.json`, rewritten after each live bar and on every change of state ([`LiveStatus`]).
//! - `lock`, held locked by the process running the session, so two cannot run it at once.
//! - `faulted`, written once when the broker faults: the fault and the venue's time. A session
//!   whose folder holds it is never opened or run again, whatever `status.json` says; nothing in
//!   this crate removes it.
//!
//! **The folder is input, not trusted.** A session is refused when `live`, its folder or any of
//! its files is a link (on Windows, any reparse point). Every complete line of the log must read
//! before a torn last line is cut, the log must replay to the state it records, and a replay that
//! panics is refused like any other bad log; none of it sends anything.
//!
//! **Write-ahead.** A live bar on which the session sends or cancels an order writes two lines.
//! Before the first send or cancel, a `sending` line holds the bar and the broker's record so far
//! (the bar's own reads and fills); after the bar, the bar's line holds the rest. A process that
//! dies between the two leaves the `sending` line last. The resume replays it, the session makes
//! the same orders again past the end of the record, and [`VenueHandle::go_live`] adopts each one
//! the venue already holds instead of sending it twice. The broker's own sends inside its bar
//! (the stop it places for a filled entry, a resize, a close at market) are not covered: a crash
//! right after one of them leaves an order the record does not know, which the resume's reconcile
//! reports as a fault, so the session halts rather than doubling an order.
//!
//! Nothing secret is written: route errors are stored by their `Display` text, which holds no key,
//! secret or signature.

use std::any::Any;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use td_core::{AccountCurrency, Candle, FeeSchedule, Instrument, Millis, OrderId, Timeframe};
use td_engine::{
    Broker, BrokerEvent, ENGINE_REVISION, EngineError, Order, Params, Portfolio, Session,
    SessionConfig, Strategy,
};
use td_orders::Client;

use crate::{BrokerRecord, VenueBroker, VenueHandle};

const CONFIG_FILE: &str = "config.json";
const EVENTS_FILE: &str = "events.jsonl";
const STATUS_FILE: &str = "status.json";
const LOCK_FILE: &str = "lock";
const FAULTED_FILE: &str = "faulted";
/// Every file a session folder holds, the temporary ones [`write_synced`] makes included.
const FILES: [&str; 7] = [
    CONFIG_FILE,
    EVENTS_FILE,
    STATUS_FILE,
    LOCK_FILE,
    FAULTED_FILE,
    "config.json.tmp",
    "status.json.tmp",
];
/// The most warnings the status keeps.
const KEEP_WARNINGS: usize = 100;
/// The longest warning, fault or panic text kept, in characters.
const MAX_TEXT: usize = 1000;
/// The pause before a failed rename of a rewritten file is tried once more.
const RENAME_RETRY: Duration = Duration::from_millis(200);
const NOT_STARTED: &str = "created; not started";
const ENDED_UNSTOPPED: &str = "the process running the session ended without stopping it; what \
                               it left at the venue is as last read";
/// The reason an order kept back after the log broke is cancelled with.
const NOT_SENT: &str = "not sent: the log could not be written, so the session sends nothing more";

/// Builds a strategy by name from its parameters: `td_engine::build_strategy` for the registered
/// strategies, or a test's own.
pub type BuildStrategy = fn(&str, &Params) -> Result<Box<dyn Strategy>, EngineError>;

/// What a live session runs: a strategy with its parameters on one instrument and timeframe, with
/// the session settings a paper session has. The starting balance is not here: it is the venue's
/// account equity, read when the session is created.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveConfig {
    pub strategy: String,
    pub params: Params,
    /// The instrument with the trading rules it had when the session was made, fixed for the
    /// session's life so a resume replays against the same rules.
    pub instrument: Instrument,
    pub timeframe: Timeframe,
    /// Prices the strategy's checks at submission; fills come from the venue.
    pub slippage_bps: f64,
    /// Replaces the instrument's fee schedule in the strategy's checks when set.
    pub fees: Option<FeeSchedule>,
    /// The largest position allowed, as its notional over equity: 1.0 means 1x.
    pub max_leverage: f64,
}

/// What `config.json` holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedConfig {
    #[serde(flatten)]
    pub config: LiveConfig,
    /// The account's equity in the settle coin when the session was created.
    pub starting_balance: f64,
    /// The client-id prefix of every order the session sends (see [`crate::ids`]).
    pub prefix: String,
    /// The [`ENGINE_REVISION`] the session was made with.
    pub engine_revision: u32,
    /// The venue's time when the session was created.
    pub created_at: Millis,
}

/// A funding rate fed with a bar.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Funding {
    /// The time the engine was given, inside the bar's window.
    pub time: Millis,
    pub rate: f64,
    /// The venue's funding time; earlier than `time` when the rate was published late.
    pub due: Millis,
}

/// One line of `events.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Line {
    bar: Candle,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    funding: Vec<Funding>,
    /// Fed with `Session::warm_up`: no trading, no broker record.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    warm_up: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    record: Option<BrokerRecord>,
    /// Written before the bar's first send or cancel; the bar's own line follows it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    sending: bool,
}

/// Where a session stands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    WarmingUp,
    Running,
    /// Stopped for `reason`; it can be resumed.
    Halted {
        reason: String,
    },
    /// The broker faulted ([`crate::BrokerFault`]). The session stays halted: a resume is refused.
    Faulted {
        fault: String,
    },
    /// The run panicked, in the strategy, the engine or the broker. The session stays halted: a
    /// resume would replay into the same panic, so it is refused.
    Panicked {
        panic: String,
    },
    /// Not running: not started yet, or stopped by its owner. What it left open at the venue is in
    /// the note and the status.
    Stopped {
        note: String,
    },
}

/// The venue's position and equity as last read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VenueRead {
    /// The venue's time of the read.
    pub at: Millis,
    /// Signed size on the instrument.
    pub position: f64,
    pub avg_price: f64,
    /// The account's equity in `coin`.
    pub equity: f64,
    pub coin: String,
}

/// What `status.json` holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveStatus {
    #[serde(flatten)]
    pub state: State,
    /// Open time of the last bar fed live.
    pub last_bar: Option<Millis>,
    /// The session's own position, from the fills it was reported.
    pub position: f64,
    /// The session's open orders.
    pub open_orders: Vec<Order>,
    pub venue: Option<VenueRead>,
    /// Things that went wrong without stopping the session, newest last.
    pub warnings: Vec<String>,
}

impl LiveStatus {
    fn not_started(venue: Option<VenueRead>) -> LiveStatus {
        LiveStatus {
            state: State::Stopped {
                note: NOT_STARTED.into(),
            },
            last_bar: None,
            position: 0.0,
            open_orders: Vec::new(),
            venue,
            warnings: Vec::new(),
        }
    }
}

/// What the `faulted` marker holds.
#[derive(Serialize, Deserialize)]
struct FaultMarker {
    /// The venue's time when the fault was recorded.
    at: Millis,
    fault: String,
}

/// Why a live session could not be made, opened or run.
#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error(
        "invalid session id {0:?}: use 1 to 64 lowercase letters, digits, '-' or '_', and not a \
         name Windows keeps for a device (con, prn, aux, nul, com0 to com9, lpt0 to lpt9)"
    )]
    InvalidId(String),
    #[error(
        "{} is a link or reparse point; a session's folder and files must be plain ones, so the \
         session is refused",
        .0.display()
    )]
    Link(PathBuf),
    #[error("a live session already exists at {}", .0.display())]
    Exists(PathBuf),
    #[error("no live session at {}", .0.display())]
    NotFound(PathBuf),
    #[error("the live session at {} is already running in another process or run", .0.display())]
    Locked(PathBuf),
    #[error("the venue could not be read: {0}")]
    VenueUnreadable(String),
    #[error(
        "a fresh session must start flat: the account holds a position of {position} and \
         {open_orders} open orders on the instrument"
    )]
    NotFlat { position: f64, open_orders: usize },
    #[error(
        "this session was created with engine revision {created} and this engine is revision \
         {current}; it cannot be resumed: start a new session"
    )]
    EngineChanged { created: u32, current: u32 },
    #[error("the session halted on a broker fault; nothing was sent or cancelled after it: {0}")]
    Faulted(String),
    #[error("the session halted on a broker fault and stays halted, so it cannot be resumed: {0}")]
    StillFaulted(String),
    #[error(
        "the session's run panicked and a resume would replay into the same panic, so it cannot \
         be resumed: {0}"
    )]
    StillPanicked(String),
    #[error("the session halted: {0}")]
    Halted(String),
    #[error("{}: {reason}; nothing was sent", path.display())]
    BadLog { path: PathBuf, reason: String },
    #[error("{}: {reason}", path.display())]
    BadFile { path: PathBuf, reason: String },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("the log could not be written, so the session takes no more bars: {0}")]
    LogFailed(String),
    #[error("the run panicked: {0}")]
    Panicked(String),
}

impl LiveError {
    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> LiveError + '_ {
        move |source| LiveError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// The folder of the live session `id`.
pub fn live_dir(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join("live").join(id)
}

/// A strategy on a [`VenueBroker`], with its saved files. See the module documentation.
pub struct LiveSession {
    id: String,
    data_dir: PathBuf,
    dir: PathBuf,
    saved: SavedConfig,
    client: Arc<Client>,
    session: Session,
    handle: VenueHandle,
    log: Arc<Mutex<LogFile>>,
    status: LiveStatus,
    /// Open time of the first live bar.
    live_from: Option<Millis>,
    /// Open time of the bar after the last one fed, warm-up included.
    next_bar: Option<Millis>,
    /// The funding charged on live bars.
    funding: Vec<Funding>,
    /// A `sending` line the log ends with: its bar's line is written after `go_live`.
    pending: Option<(Candle, Vec<Funding>)>,
    /// Held locked while the session is open.
    _lock: File,
}

impl LiveSession {
    /// Makes a new session in `<data_dir>/live/<id>/`. Nothing is written or sent when the id is
    /// taken, the strategy or settings do not check, the venue cannot be read, or the account
    /// holds a position or open orders on the instrument. The starting balance is the account's
    /// equity read here, and the client-id prefix is made from the creation time. When writing
    /// the folder fails part way, what was written is removed, so the id can be used again.
    pub fn create(
        data_dir: &Path,
        id: &str,
        config: LiveConfig,
        client: Arc<Client>,
        build: BuildStrategy,
    ) -> Result<LiveSession, LiveError> {
        check_id(id)?;
        let root = data_dir.join("live");
        refuse_link(&root)?;
        let dir = root.join(id);
        if fs::symlink_metadata(&dir).is_ok() {
            return Err(LiveError::Exists(dir));
        }
        let strategy = build(&config.strategy, &config.params)?;
        let unreadable = |e: td_orders::Error| LiveError::VenueUnreadable(e.to_string());
        let equity = client.equity().map_err(unreadable)?;
        let position = client.position(&config.instrument).map_err(unreadable)?;
        let open = client.open_orders(&config.instrument).map_err(unreadable)?;
        if position.size.abs() >= config.instrument.lot_size / 2.0 || !open.is_empty() {
            return Err(LiveError::NotFlat {
                position: position.size,
                open_orders: open.len(),
            });
        }
        let created_at = client.server_now();
        let saved = SavedConfig {
            config,
            starting_balance: equity.equity,
            prefix: base36(created_at),
            engine_revision: ENGINE_REVISION,
            created_at,
        };
        let config_path = dir.join(CONFIG_FILE);
        let log_path = dir.join(EVENTS_FILE);
        let parts = Parts::build(&saved, client.clone(), strategy, &config_path)?;
        let venue = VenueRead {
            at: created_at,
            position: position.size,
            avg_price: position.avg_price,
            equity: equity.equity,
            coin: equity.coin,
        };

        fs::create_dir_all(&root).map_err(LiveError::io(&root))?;
        refuse_link(&root)?;
        make_folder(&dir, || {
            let lock = take_lock(&dir)?;
            let json = serde_json::to_vec_pretty(&saved).map_err(|e| LiveError::BadFile {
                path: config_path.clone(),
                reason: e.to_string(),
            })?;
            write_synced(&dir, CONFIG_FILE, &json)?;
            let file = OpenOptions::new()
                .append(true)
                .create_new(true)
                .open(&log_path)
                .map_err(LiveError::io(&log_path))?;
            *lock_log(&parts.log) = LogFile::new(file, log_path, 0);
            let mut live = LiveSession {
                id: id.to_string(),
                data_dir: data_dir.to_path_buf(),
                dir: dir.clone(),
                saved,
                client,
                session: parts.session,
                handle: parts.handle,
                log: parts.log,
                status: LiveStatus::not_started(Some(venue)),
                live_from: None,
                next_bar: None,
                funding: Vec::new(),
                pending: None,
                _lock: lock,
            };
            live.save_status()?;
            Ok(live)
        })
    }

    /// Opens a saved session to resume it: checks the folder, takes the lock, checks the engine
    /// revision and the client-id prefix, and replays the log through a fresh session and broker
    /// without touching the venue. A session that halted on a broker fault (its `faulted` marker,
    /// or its status) or on a panic is refused. A status saved as running or warming up, left by
    /// a process that ended without stopping, reads as stopped. A folder with its config and no
    /// status, left by a crash inside [`LiveSession::create`], opens as not started when its log
    /// is empty or missing.
    ///
    /// A last line without its newline was torn by a crash mid-write: once every line before it
    /// has read and the log has replayed, it is cut from the file and noted in the warnings, and
    /// its bar is fetched again when the session runs. Any other line that does not read, or a
    /// log that does not replay to the state it records, is refused and the file left as it is.
    pub fn open(
        data_dir: &Path,
        id: &str,
        client: Arc<Client>,
        build: BuildStrategy,
    ) -> Result<LiveSession, LiveError> {
        check_id(id)?;
        let dir = session_dir(data_dir, id)?;
        let config_path = dir.join(CONFIG_FILE);
        if !config_path.exists() {
            return Err(LiveError::NotFound(dir));
        }
        let lock = take_lock(&dir)?;
        // Under the lock, so a run in another process cannot fault after the check.
        if let Some(fault) = fault_marker(&dir) {
            return Err(LiveError::StillFaulted(fault));
        }
        let saved: SavedConfig = read_json(&config_path)?;
        if saved.engine_revision != ENGINE_REVISION {
            return Err(LiveError::EngineChanged {
                created: saved.engine_revision,
                current: ENGINE_REVISION,
            });
        }
        if saved.prefix != base36(saved.created_at) {
            return Err(LiveError::BadFile {
                path: config_path,
                reason: "the client-id prefix is not the one made from the creation time".into(),
            });
        }

        let log_path = dir.join(EVENTS_FILE);
        let status_path = dir.join(STATUS_FILE);
        let bytes = match fs::read(&log_path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound && !status_path.exists() => Vec::new(),
            Err(e) => return Err(LiveError::io(&log_path)(e)),
        };
        let mut status = if status_path.exists() {
            read_json::<LiveStatus>(&status_path)?
        } else if bytes.is_empty() {
            LiveStatus::not_started(None)
        } else {
            return Err(LiveError::BadFile {
                path: status_path,
                reason: "it is missing, while the log holds bars".into(),
            });
        };
        if let Some(refused) = refusal(&dir, &status.state) {
            return Err(refused);
        }
        if matches!(status.state, State::Running | State::WarmingUp) {
            status.state = State::Stopped {
                note: ENDED_UNSTOPPED.into(),
            };
        }
        trim_warnings(&mut status.warnings);
        // Worked out again by the replay, so a status edited by hand cannot misstate them.
        status.last_bar = None;
        status.position = 0.0;
        status.open_orders.clear();

        let strategy = build(&saved.config.strategy, &saved.config.params)?;
        let parts = Parts::build(&saved, client.clone(), strategy, &config_path)?;
        let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let bad = |index: usize, reason: String| LiveError::BadLog {
            path: log_path.clone(),
            reason: format!("line {}: {reason}", index + 1),
        };
        let text = std::str::from_utf8(&bytes[..complete]).map_err(|e| LiveError::BadLog {
            path: log_path.clone(),
            reason: format!("it is not UTF-8: {e}"),
        })?;
        let lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| serde_json::from_str::<Line>(l).map_err(|e| bad(i, e.to_string())))
            .collect::<Result<Vec<Line>, LiveError>>()?;

        let mut live = LiveSession {
            id: id.to_string(),
            data_dir: data_dir.to_path_buf(),
            dir,
            saved,
            client,
            session: parts.session,
            handle: parts.handle,
            log: parts.log,
            status,
            live_from: None,
            next_bar: None,
            funding: Vec::new(),
            pending: None,
            _lock: lock,
        };
        // The replay runs strategy and engine code on the file's contents.
        match catch_unwind(AssertUnwindSafe(|| live.replay(&lines))) {
            Ok(Ok(())) => {}
            Ok(Err((i, reason))) => return Err(bad(i, reason)),
            Err(panic) => {
                return Err(LiveError::BadLog {
                    path: log_path,
                    reason: format!("replaying it panicked: {}", panic_message(&*panic)),
                });
            }
        }
        if complete < bytes.len() {
            live.warn(format!(
                "{}: cut a torn last line of {} bytes",
                log_path.display(),
                bytes.len() - complete
            ));
            OpenOptions::new()
                .write(true)
                .open(&log_path)
                .and_then(|file| {
                    file.set_len(complete as u64)
                        .and_then(|()| file.sync_data())
                })
                .map_err(LiveError::io(&log_path))?;
        }
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&log_path)
            .map_err(LiveError::io(&log_path))?;
        *lock_log(&live.log) = LogFile::new(file, log_path, complete as u64);
        live.save_status()?;
        Ok(live)
    }

    /// Feeds the log's lines to the session, each live bar with its broker record queued first.
    /// Returns the index of the line that failed and why.
    fn replay(&mut self, lines: &[Line]) -> Result<(), (usize, String)> {
        let mut i = 0;
        while i < lines.len() {
            let line = &lines[i];
            let funding = pairs(&line.funding);
            if line.warm_up {
                self.session
                    .warm_up(line.bar, &funding)
                    .map_err(|e| (i, e.to_string()))?;
                self.next_bar =
                    Some(line.bar.open_time + self.saved.config.timeframe.duration_ms());
                i += 1;
                continue;
            }
            let Some(record) = &line.record else {
                return Err((i, "a live bar without its broker record".into()));
            };
            self.handle.replay(record.clone());
            let mut next = i + 1;
            if line.sending {
                match lines.get(i + 1) {
                    Some(rest) if rest.bar == line.bar && !rest.warm_up && !rest.sending => {
                        let Some(record) = &rest.record else {
                            return Err((i + 1, "a live bar without its broker record".into()));
                        };
                        self.handle.replay(record.clone());
                        next = i + 2;
                    }
                    Some(_) => {
                        return Err((i + 1, "a sending line is not followed by its bar".into()));
                    }
                    None => self.pending = Some((line.bar, line.funding.clone())),
                }
            }
            match self.session.on_bar(line.bar, &funding) {
                // The bar was fed in full, as when it was logged.
                Ok(()) | Err(EngineError::StrategyFailed(_)) => {}
                Err(e) => return Err((i, e.to_string())),
            }
            if let Some(fault) = self.handle.fault() {
                return Err((
                    i,
                    format!("the log does not replay to the state it records: {fault}"),
                ));
            }
            self.mark_live(&line.bar, &line.funding);
            i = next;
        }
        Ok(())
    }

    /// Feeds one bar with its funding. A warm-up bar goes through `Session::warm_up` and its line
    /// is not synced. A live bar's line, with the broker's record, is synced before this returns.
    /// After `EngineError::StrategyFailed` the bar was fed in full and is logged, and the error is
    /// returned so the run halts. On a log error the run must halt too: an order the strategy
    /// made after the log broke was not sent, and is cancelled in the engine with the reason.
    pub(crate) fn feed(
        &mut self,
        bar: Candle,
        funding: Vec<Funding>,
        warm_up: bool,
    ) -> Result<(), LiveError> {
        lock_log(&self.log).check()?;
        let engine_funding = pairs(&funding);
        if warm_up {
            self.session.warm_up(bar, &engine_funding)?;
            self.next_bar = Some(bar.open_time + self.saved.config.timeframe.duration_ms());
            let line = Line {
                bar,
                funding,
                warm_up: true,
                record: None,
                sending: false,
            };
            return lock_log(&self.log).append(&line, false);
        }
        lock_log(&self.log).armed = Some((bar, funding.clone()));
        let fed = self.session.on_bar(bar, &engine_funding);
        let kept_back = {
            let mut log = lock_log(&self.log);
            log.armed = None;
            log.refused.clone()
        };
        for id in kept_back {
            self.session.cancel(id, NOT_SENT);
        }
        let mut log = lock_log(&self.log);
        // A bar the engine refused never reached the broker.
        if fed
            .as_ref()
            .is_err_and(|e| !matches!(e, EngineError::StrategyFailed(_)))
        {
            return fed.map_err(LiveError::from);
        }
        // A sending line that could not be written was cut; this line alone would not replay.
        log.check()?;
        let line = Line {
            bar,
            funding: funding.clone(),
            warm_up: false,
            record: Some(self.handle.take_record()),
            sending: false,
        };
        log.append(&line, true)?;
        drop(log);
        self.mark_live(&bar, &funding);
        Ok(fed?)
    }

    fn mark_live(&mut self, bar: &Candle, funding: &[Funding]) {
        self.live_from.get_or_insert(bar.open_time);
        self.next_bar = Some(bar.open_time + self.saved.config.timeframe.duration_ms());
        self.funding.extend_from_slice(funding);
        self.status.last_bar = Some(bar.open_time);
        self.status.position = self.session.portfolio().position();
        self.status.open_orders = self.session.open_orders().to_vec();
    }

    /// Ends the replay at the venue ([`VenueHandle::go_live`]), then writes the bar's line of a
    /// `sending` line the log ended with.
    pub(crate) fn go_live(&mut self) -> Result<(), LiveError> {
        let result = self.handle.go_live();
        let mut logged = Ok(());
        if let Some((bar, funding)) = self.pending.take() {
            let line = Line {
                bar,
                funding,
                warm_up: false,
                record: Some(self.handle.take_record()),
                sending: false,
            };
            logged = lock_log(&self.log).append(&line, true);
            self.status.open_orders = self.session.open_orders().to_vec();
        }
        // The fault comes first, whatever the line did: it is what keeps the session halted.
        if let Err(fault) = result {
            if let Err(e) = logged {
                self.warn(e.to_string());
            }
            return Err(LiveError::Faulted(fault.to_string()));
        }
        logged
    }

    /// Writes the `faulted` marker, unless there is one: the first fault stays. A marker that
    /// cannot be written is a warning; the status still says the session faulted.
    pub(crate) fn mark_faulted(&mut self, fault: &str) {
        let path = self.dir.join(FAULTED_FILE);
        let marker = FaultMarker {
            at: self.client.server_now(),
            fault: clean(fault, MAX_TEXT),
        };
        let written = serde_json::to_vec(&marker)
            .map_err(std::io::Error::other)
            .and_then(|json| {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                file.write_all(&json)?;
                file.sync_all()
            });
        match written {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => self.warn(format!(
                "{}: the fault marker could not be written: {e}",
                path.display()
            )),
        }
    }

    /// Why the session must not run again: its `faulted` marker, or a state of faulted or
    /// panicked.
    pub(crate) fn refusal(&self) -> Option<LiveError> {
        refusal(&self.dir, &self.status.state)
    }

    /// Reads the venue's position and equity into the status; a failed read is a warning.
    pub(crate) fn read_venue(&mut self) {
        let read = self
            .handle
            .venue_position()
            .and_then(|p| self.handle.equity().map(|e| (p, e)));
        match read {
            Ok((position, equity)) => {
                self.status.venue = Some(VenueRead {
                    at: self.client.server_now(),
                    position: position.size,
                    avg_price: position.avg_price,
                    equity: equity.equity,
                    coin: equity.coin,
                });
            }
            Err(e) => self.warn(format!("the venue could not be read: {e}")),
        }
    }

    /// Sets the state and rewrites the status file.
    pub(crate) fn set_state(&mut self, state: State) -> Result<(), LiveError> {
        self.status.state = state;
        self.save_status()
    }

    /// Rewrites `status.json`: written to a temporary file, synced and renamed over the old one.
    pub(crate) fn save_status(&mut self) -> Result<(), LiveError> {
        self.status.position = self.session.portfolio().position();
        self.status.open_orders = self.session.open_orders().to_vec();
        let json = serde_json::to_vec_pretty(&self.status).map_err(|e| LiveError::BadFile {
            path: self.dir.join(STATUS_FILE),
            reason: e.to_string(),
        })?;
        write_synced(&self.dir, STATUS_FILE, &json)
    }

    pub(crate) fn warn(&mut self, warning: String) {
        push_warning(&mut self.status.warnings, warning);
    }

    pub(crate) fn live_from(&self) -> Option<Millis> {
        self.live_from
    }

    pub(crate) fn next_bar_time(&self) -> Option<Millis> {
        self.next_bar
    }

    pub(crate) fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// The session's folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn config(&self) -> &SavedConfig {
        &self.saved
    }

    pub fn status(&self) -> &LiveStatus {
        &self.status
    }

    /// The engine session: orders, fills, portfolio and bars.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The broker's handle: the venue's position, equity and the session's resting orders.
    pub fn handle(&self) -> &VenueHandle {
        &self.handle
    }

    /// The funding charged on live bars.
    pub fn funding(&self) -> &[Funding] {
        &self.funding
    }
}

/// The session and broker built from a saved config.
struct Parts {
    session: Session,
    handle: VenueHandle,
    log: Arc<Mutex<LogFile>>,
}

impl Parts {
    fn build(
        saved: &SavedConfig,
        client: Arc<Client>,
        strategy: Box<dyn Strategy>,
        config_path: &Path,
    ) -> Result<Parts, LiveError> {
        let config = &saved.config;
        let (inner, handle) = VenueBroker::new(
            client,
            config.instrument.clone(),
            config.timeframe,
            &saved.prefix,
        )
        .map_err(|e| LiveError::BadFile {
            path: config_path.to_path_buf(),
            reason: e.to_string(),
        })?;
        let log = Arc::new(Mutex::new(LogFile::closed()));
        let broker = LoggedBroker {
            inner,
            handle: handle.clone(),
            log: log.clone(),
        };
        let money = SessionConfig {
            starting_cash: saved.starting_balance,
            slippage_bps: config.slippage_bps,
            fees: config.fees,
            max_leverage: config.max_leverage,
            // shortcut: a quote-currency account, as paper sessions have; add the account
            // currency to `LiveConfig` when a live session trades a market with quotes.
            account_currency: AccountCurrency::Quote,
        };
        let session = Session::with_broker(
            strategy,
            config.instrument.clone(),
            config.timeframe,
            money,
            Box::new(broker),
        )?;
        Ok(Parts {
            session,
            handle,
            log,
        })
    }
}

/// The log file, shared by the session and its broker for the write-ahead line.
struct LogFile {
    file: Option<File>,
    path: PathBuf,
    len: u64,
    broken: Option<String>,
    /// The live bar being fed, until its first send or cancel writes the `sending` line.
    armed: Option<(Candle, Vec<Funding>)>,
    /// Orders the broker kept back because the log broke; [`LiveSession::feed`] cancels them.
    refused: Vec<OrderId>,
}

fn lock_log(log: &Mutex<LogFile>) -> MutexGuard<'_, LogFile> {
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl LogFile {
    fn closed() -> LogFile {
        LogFile {
            file: None,
            path: PathBuf::new(),
            len: 0,
            broken: None,
            armed: None,
            refused: Vec::new(),
        }
    }

    fn new(file: File, path: PathBuf, len: u64) -> LogFile {
        LogFile {
            file: Some(file),
            path,
            len,
            broken: None,
            armed: None,
            refused: Vec::new(),
        }
    }

    fn check(&self) -> Result<(), LiveError> {
        match &self.broken {
            Some(reason) => Err(LiveError::LogFailed(reason.clone())),
            None => Ok(()),
        }
    }

    /// Appends one line, synced if `sync`. A failure breaks the log and cuts back whatever part of
    /// the line reached the file, so a line reported as failed does not replay.
    ///
    /// shortcut: a copy of `PaperSession::append` in td-server; share one once the paper and live
    /// logs settle on one line format.
    fn append(&mut self, line: &Line, sync: bool) -> Result<(), LiveError> {
        self.check()?;
        let mut text = serde_json::to_string(line).map_err(|e| LiveError::BadFile {
            path: self.path.clone(),
            reason: e.to_string(),
        })?;
        text.push('\n');
        let Some(file) = self.file.as_mut() else {
            return Err(LiveError::LogFailed("the log is not open".into()));
        };
        let written = file
            .write_all(text.as_bytes())
            .and_then(|()| if sync { file.sync_data() } else { Ok(()) });
        let Err(e) = written else {
            self.len += text.len() as u64;
            return Ok(());
        };
        let mut reason = format!("{}: {e}", self.path.display());
        // Through a handle of its own: the log's is append-only, which cannot truncate. Never
        // through a link put in the log's place since it was opened.
        let cut = match fs::symlink_metadata(&self.path) {
            Ok(meta) if is_link(&meta) => Err(std::io::Error::other("it is now a link")),
            _ => OpenOptions::new()
                .write(true)
                .open(&self.path)
                .and_then(|file| file.set_len(self.len).and_then(|()| file.sync_data())),
        };
        if let Err(cut) = cut {
            reason.push_str(&format!(
                "; the line could not be cut from the log either, so a resume replays it: {cut}"
            ));
        }
        self.broken = Some(reason.clone());
        Err(LiveError::LogFailed(reason))
    }
}

/// The session's broker: a [`VenueBroker`] that writes the `sending` line before a live bar's
/// first send or cancel reaches it, and sends nothing more once a line has failed.
struct LoggedBroker {
    inner: VenueBroker,
    handle: VenueHandle,
    log: Arc<Mutex<LogFile>>,
}

impl LoggedBroker {
    /// Whether a send or cancel may reach the venue: writes the bar's `sending` line before its
    /// first one. Once a line has failed nothing more goes out, since a resume would not know of
    /// an order sent without its line.
    fn may_send(&self) -> bool {
        let mut log = lock_log(&self.log);
        if log.broken.is_some() {
            return false;
        }
        let Some((bar, funding)) = log.armed.take() else {
            return true;
        };
        let line = Line {
            bar,
            funding,
            warm_up: false,
            record: Some(self.handle.take_record()),
            sending: true,
        };
        log.append(&line, true).is_ok()
    }
}

impl Broker for LoggedBroker {
    fn submit(&mut self, order: Order) {
        if self.may_send() {
            self.inner.submit(order);
        } else {
            lock_log(&self.log).refused.push(order.id);
        }
    }

    fn cancel(&mut self, id: OrderId) -> bool {
        if self.may_send() {
            return self.inner.cancel(id);
        }
        // Nothing reaches the venue: only an order kept back here can be cancelled.
        let mut log = lock_log(&self.log);
        let kept = log.refused.len();
        log.refused.retain(|&refused| refused != id);
        log.refused.len() < kept
    }

    fn on_bar(
        &mut self,
        bar: &Candle,
        ask: &Candle,
        portfolio: &Portfolio,
        breached: &dyn Fn(&Portfolio) -> Option<f64>,
    ) -> Vec<BrokerEvent> {
        self.inner.on_bar(bar, ask, portfolio, breached)
    }

    fn open_orders(&self) -> &[Order] {
        self.inner.open_orders()
    }
}

fn pairs(funding: &[Funding]) -> Vec<(Millis, f64)> {
    funding.iter().map(|f| (f.time, f.rate)).collect()
}

fn push_warning(warnings: &mut Vec<String>, warning: String) {
    warnings.push(warning);
    trim_warnings(warnings);
}

/// Keeps the newest [`KEEP_WARNINGS`] warnings, each cleaned and cut to [`MAX_TEXT`].
///
/// shortcut: only the latest warnings are kept, so a noisy stream cannot grow the status file
/// without bound; keep a separate history if it matters.
fn trim_warnings(warnings: &mut Vec<String>) {
    let extra = warnings.len().saturating_sub(KEEP_WARNINGS);
    warnings.drain(..extra);
    for warning in warnings.iter_mut() {
        *warning = clean(warning, MAX_TEXT);
    }
}

/// `text` with control characters replaced by spaces, cut to `max` characters.
fn clean(text: &str, max: usize) -> String {
    let mut out: String = text
        .chars()
        .take(max)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if text.chars().nth(max).is_some() {
        out.push('…');
    }
    out
}

/// The text of a caught panic, cleaned and cut to [`MAX_TEXT`].
pub(crate) fn panic_message(panic: &(dyn Any + Send)) -> String {
    let text = panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".into());
    clean(&text, MAX_TEXT)
}

/// The refusal for a session whose folder holds the `faulted` marker or whose state is faulted
/// or panicked.
fn refusal(dir: &Path, state: &State) -> Option<LiveError> {
    if let Some(fault) = fault_marker(dir) {
        return Some(LiveError::StillFaulted(fault));
    }
    match state {
        State::Faulted { fault } => Some(LiveError::StillFaulted(clean(fault, MAX_TEXT))),
        State::Panicked { panic } => Some(LiveError::StillPanicked(clean(panic, MAX_TEXT))),
        _ => None,
    }
}

/// The fault the `faulted` marker in `dir` records, when the marker is there in any form. One that
/// cannot be read still counts.
fn fault_marker(dir: &Path) -> Option<String> {
    let path = dir.join(FAULTED_FILE);
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => return Some(format!("its fault marker could not be checked: {e}")),
    }
    let mut bytes = Vec::new();
    let read =
        File::open(&path).and_then(|file| file.take(4 * MAX_TEXT as u64).read_to_end(&mut bytes));
    let text = match read {
        Ok(_) => serde_json::from_slice::<FaultMarker>(&bytes)
            .map(|marker| marker.fault)
            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) => format!("its fault marker could not be read: {e}"),
    };
    Some(if text.trim().is_empty() {
        "its fault marker holds no text".into()
    } else {
        clean(&text, MAX_TEXT)
    })
}

/// The folder of session `id`, once neither `live`, the folder nor any file a session folder
/// holds is a link.
fn session_dir(data_dir: &Path, id: &str) -> Result<PathBuf, LiveError> {
    let root = data_dir.join("live");
    refuse_link(&root)?;
    let dir = root.join(id);
    refuse_link(&dir)?;
    for name in FILES {
        refuse_link(&dir.join(name))?;
    }
    Ok(dir)
}

/// Refuses a link at `path`; nothing there passes.
fn refuse_link(path: &Path) -> Result<(), LiveError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if is_link(&meta) => Err(LiveError::Link(path.to_path_buf())),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(LiveError::io(path)(e)),
    }
}

/// A symbolic link, or on Windows any reparse point (a junction among them).
fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

/// Makes the folder `dir` and fills it with `fill`. When `fill` fails, the folder is removed
/// again, so its id can be used again.
fn make_folder<T>(dir: &Path, fill: impl FnOnce() -> Result<T, LiveError>) -> Result<T, LiveError> {
    match fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            return Err(LiveError::Exists(dir.to_path_buf()));
        }
        Err(e) => return Err(LiveError::io(dir)(e)),
    }
    // `fill` has returned, so the files it opened are closed and the folder can go.
    fill().inspect_err(|_| {
        let _ = fs::remove_dir_all(dir);
    })
}

fn take_lock(dir: &Path) -> Result<File, LiveError> {
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(LiveError::io(&path))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(LiveError::Locked(dir.to_path_buf())),
        Err(TryLockError::Error(e)) => Err(LiveError::io(&path)(e)),
    }
}

/// Writes `name` in `dir` through a synced temporary file renamed over it, so a crash cannot
/// leave it empty or partial. A temporary file left by a crash is removed first (a link there is
/// removed, never followed), and the new one is made fresh. A failed rename is tried once more
/// after [`RENAME_RETRY`]: on Windows a reader holding the file makes it fail.
fn write_synced(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), LiveError> {
    let path = dir.join(name);
    let temp = dir.join(format!("{name}.tmp"));
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(LiveError::io(&temp)(e)),
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
        .map_err(LiveError::io(&temp))?;
    fs::rename(&temp, &path)
        .or_else(|_| {
            std::thread::sleep(RENAME_RETRY);
            fs::rename(&temp, &path)
        })
        .map_err(LiveError::io(&path))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, LiveError> {
    let text = fs::read_to_string(path).map_err(LiveError::io(path))?;
    serde_json::from_str(&text).map_err(|e| LiveError::BadFile {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

/// An id is 1 to 64 lowercase ASCII letters, digits, `-` or `_`, and not a name Windows keeps for
/// a device. Lowercase is required, not made: Windows folder names ignore case, so `Demo` and
/// `demo` would be one folder.
fn check_id(id: &str) -> Result<(), LiveError> {
    let device = matches!(id, "con" | "prn" | "aux" | "nul")
        || (id.len() == 4
            && (id.starts_with("com") || id.starts_with("lpt"))
            && id.as_bytes()[3].is_ascii_digit());
    let ok = (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && !device;
    if ok {
        Ok(())
    } else {
        Err(LiveError::InvalidId(id.to_string()))
    }
}

/// `t` in base 36, lowercase: the client-id prefix of a session created at `t`.
///
/// shortcut: two sessions created in the same millisecond on one account would share a prefix;
/// the broker's id-collision check catches an order the other one placed. Add a random suffix if
/// sessions are ever created in bulk.
fn base36(t: Millis) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut n = t.unsigned_abs();
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII digits")
}

#[cfg(test)]
mod tests {
    use td_core::{
        InstrumentId, InstrumentKind, OrderRequest, OrderType, Side, TimeInForce, Venue,
    };
    use td_engine::{Context, OrderStatus, ParamValue};
    use td_mock_bybit::{MockVenue, TEST_KEY, TEST_SECRET};
    use td_orders::{DemoKeys, Endpoint, Timing};

    use super::*;

    const SYMBOL: &str = "BTCUSDT";
    const PRICE: f64 = 65000.0;
    const QTY: f64 = 0.004;
    const HOUR: Millis = 3_600_000;
    const CREATE: &str = "/v5/order/create";

    fn instrument() -> Instrument {
        Instrument {
            id: InstrumentId {
                venue: Venue::Bybit,
                symbol: SYMBOL.into(),
                kind: InstrumentKind::Perpetual,
            },
            base: "BTC".into(),
            quote: "USDT".into(),
            tick_size: 0.1,
            lot_size: 0.001,
            min_qty: 0.002,
            min_notional: 100.0,
            fees: FeeSchedule {
                maker: 0.0002,
                taker: 0.00055,
                per_unit: 0.0,
            },
        }
    }

    fn client_for(mock: &MockVenue) -> Arc<Client> {
        let timing = Timing {
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_millis(1500),
            recv_window: Duration::from_millis(1000),
            settle: Duration::from_millis(50),
        };
        let keys = DemoKeys::for_loopback(TEST_KEY, TEST_SECRET).unwrap();
        Arc::new(Client::with_timing(Endpoint::Loopback(mock.port()), keys, timing).unwrap())
    }

    /// Buys [`QTY`] at market on live bar `buy_on`, counting from 0.
    struct Buyer {
        buy_on: i64,
        bar: i64,
    }

    impl Strategy for Buyer {
        fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
            if self.bar == self.buy_on {
                let request = OrderRequest {
                    instrument: instrument().id,
                    side: Side::Buy,
                    qty: QTY,
                    order_type: OrderType::Market,
                    tif: TimeInForce::Gtc,
                    reduce_only: false,
                    client_tag: None,
                };
                ctx.submit(request).expect("buy");
            }
            self.bar += 1;
        }
    }

    fn build(_: &str, params: &Params) -> Result<Box<dyn Strategy>, EngineError> {
        Ok(Box::new(Buyer {
            buy_on: params.int("buy_on")?,
            bar: 0,
        }))
    }

    fn config(buy_on: i64) -> LiveConfig {
        LiveConfig {
            strategy: "buyer".into(),
            params: Params::new().with("buy_on", ParamValue::Int(buy_on)),
            instrument: instrument(),
            timeframe: Timeframe::H1,
            slippage_bps: 0.0,
            fees: None,
            max_leverage: 1.0,
        }
    }

    fn flat(open_time: Millis) -> Candle {
        Candle {
            open_time,
            open: PRICE,
            high: PRICE,
            low: PRICE,
            close: PRICE,
            volume: 1.0,
            quote_volume: PRICE,
        }
    }

    fn mock() -> MockVenue {
        let mock = MockVenue::start();
        mock.add_instrument(&instrument(), PRICE);
        mock
    }

    /// Gives the log a read-only handle, so every write to it fails from here.
    fn break_log(live: &LiveSession) {
        let mut log = lock_log(&live.log);
        log.file = Some(File::open(&log.path).expect("log"));
    }

    #[test]
    fn an_order_made_after_the_log_breaks_is_not_sent_and_is_cancelled_in_the_engine() {
        let mock = mock();
        let data = tempfile::tempdir().unwrap();
        let mut live =
            LiveSession::create(data.path(), "broken", config(1), client_for(&mock), build)
                .unwrap();
        let start = Timeframe::H1.floor(mock.now()) - HOUR;
        live.feed(flat(start), Vec::new(), false).expect("bar 0");
        let lines = fs::read_to_string(live.dir().join(EVENTS_FILE)).unwrap();

        break_log(&live);
        let error = live
            .feed(flat(start + HOUR), Vec::new(), false)
            .unwrap_err();
        assert!(matches!(error, LiveError::LogFailed(_)), "{error}");
        assert_eq!(mock.request_count(CREATE), 0, "nothing reached the venue");
        let orders = live.session().orders();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].status, OrderStatus::Cancelled(NOT_SENT.into()));
        assert!(live.session().open_orders().is_empty());
        assert_eq!(
            fs::read_to_string(live.dir().join(EVENTS_FILE)).unwrap(),
            lines,
            "the failed line was cut, so a resume feeds bar 1 afresh"
        );
    }

    #[test]
    fn a_fault_at_go_live_is_returned_even_when_the_pending_line_cannot_be_written() {
        let mock = mock();
        let data = tempfile::tempdir().unwrap();
        let client = client_for(&mock);
        let mut live =
            LiveSession::create(data.path(), "pending", config(0), client.clone(), build).unwrap();
        let start = Timeframe::H1.floor(mock.now()) - HOUR;
        live.feed(flat(start), Vec::new(), false).expect("bar 0");
        assert_eq!(mock.request_count(CREATE), 1);
        let path = live.dir().join(EVENTS_FILE);
        drop(live);

        // The process died after the buy went out, before bar 0's own line: the log ends with
        // its sending line. The venue's position moved while it was down.
        let text = fs::read_to_string(&path).unwrap();
        let sending = text.lines().next().unwrap();
        fs::write(&path, format!("{sending}\n")).unwrap();
        mock.set_position(SYMBOL, 0.01, PRICE);

        let mut live = LiveSession::open(data.path(), "pending", client, build).unwrap();
        assert!(live.pending.is_some());
        break_log(&live);
        let error = live.go_live().unwrap_err();
        assert!(matches!(&error, LiveError::Faulted(f) if f.starts_with("position mismatch")));
        assert!(
            live.status().warnings.iter().any(|w| w.contains("log")),
            "{:?}",
            live.status().warnings
        );
    }

    #[test]
    fn a_folder_that_fails_to_fill_is_removed_so_its_id_can_be_used_again() {
        let data = tempfile::tempdir().unwrap();
        let dir = data.path().join("taken");
        let failed = make_folder(&dir, || {
            let _lock = take_lock(&dir)?;
            write_synced(&dir, CONFIG_FILE, b"{}")?;
            Err::<(), _>(LiveError::LogFailed("the disk is full".into()))
        });
        assert!(matches!(failed, Err(LiveError::LogFailed(_))));
        assert!(!dir.exists());
        make_folder(&dir, || Ok(())).expect("the id is free again");
        assert!(matches!(
            make_folder(&dir, || Ok(())),
            Err(LiveError::Exists(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn a_rename_refused_while_a_reader_holds_the_file_is_tried_again() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATUS_FILE);
        fs::write(&path, b"old").unwrap();
        // A reader that lets nobody delete or replace the file while it holds it.
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .unwrap();
        let released = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            drop(reader);
        });
        write_synced(dir.path(), STATUS_FILE, b"new").expect("written on the second try");
        released.join().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn a_stale_temporary_file_is_replaced_not_written_through() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("status.json.tmp"), b"left by a crash").unwrap();
        write_synced(dir.path(), STATUS_FILE, b"new").unwrap();
        assert_eq!(fs::read(dir.path().join(STATUS_FILE)).unwrap(), b"new");
        assert!(!dir.path().join("status.json.tmp").exists());
    }

    #[test]
    fn a_stale_temporary_link_is_removed_not_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, b"not the session's").unwrap();
        let temp = dir.path().join("status.json.tmp");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&outside, &temp);
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&outside, &temp);
        if let Err(e) = made {
            println!("skipped: a link could not be made: {e}");
            return;
        }
        write_synced(dir.path(), STATUS_FILE, b"new").unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"not the session's");
        let status = dir.path().join(STATUS_FILE);
        assert!(!fs::symlink_metadata(&status).unwrap().is_symlink());
        assert_eq!(fs::read(&status).unwrap(), b"new");
    }

    #[test]
    fn warnings_are_trimmed_to_the_newest_and_cut_to_a_bounded_length() {
        let mut warnings: Vec<String> = (0..KEEP_WARNINGS + 20).map(|i| i.to_string()).collect();
        warnings.push(format!("a\nb{}", "x".repeat(5 * MAX_TEXT)));
        trim_warnings(&mut warnings);
        assert_eq!(warnings.len(), KEEP_WARNINGS);
        assert_eq!(warnings[0], "21");
        let last = warnings.last().unwrap();
        assert!(last.starts_with("a b"), "{last}");
        assert_eq!(last.chars().count(), MAX_TEXT + 1);
        push_warning(&mut warnings, "newest".into());
        assert_eq!(warnings.len(), KEEP_WARNINGS);
        assert_eq!(warnings[0], "22");
    }

    #[test]
    fn a_prefix_is_the_creation_time_in_base_36() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        let now = base36(1_767_225_600_000);
        assert_eq!(now.len(), 8);
        assert!(crate::ids::check_prefix(&now).is_ok());
    }

    #[test]
    fn ids_are_checked() {
        for good in ["demo-1_a", "con1", "com", "lpt10", "console", "nul_"] {
            assert!(check_id(good).is_ok(), "{good}");
        }
        let long = "x".repeat(65);
        let bad = [
            "", "a/b", "..", "a b", &long, "Demo", "CON", "con", "Nul", "aux", "prn", "com1",
            "COM9", "lpt0", "Lpt5",
        ];
        for bad in bad {
            assert!(
                matches!(check_id(bad), Err(LiveError::InvalidId(_))),
                "{bad}"
            );
        }
    }
}
