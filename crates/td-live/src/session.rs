//! [`LiveSession`]: a strategy on a [`VenueBroker`], saved under `<data_dir>/live/<id>/` so it
//! survives a restart.
//!
//! The folder holds four files:
//! - `config.json`, written once at creation: the [`LiveConfig`], the starting balance read from
//!   the venue, the client-id prefix, the engine revision and the creation time ([`SavedConfig`]).
//! - `events.jsonl`, one [`Line`] per bar fed: the bar, the funding charged with it, whether it was
//!   a warm-up bar, and for a live bar the broker's record ([`VenueHandle::take_record`]). A live
//!   bar's line is synced to disk before the next bar is fed.
//! - `status.json`, rewritten after each live bar and on every change of state ([`LiveStatus`]).
//! - `lock`, held locked by the process running the session, so two cannot run it at once.
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

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

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

/// Why a live session could not be made, opened or run.
#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error("invalid session id {0:?}: use 1 to 64 letters, digits, '-' or '_'")]
    InvalidId(String),
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
    /// equity read here, and the client-id prefix is made from the creation time.
    pub fn create(
        data_dir: &Path,
        id: &str,
        config: LiveConfig,
        client: Arc<Client>,
        build: BuildStrategy,
    ) -> Result<LiveSession, LiveError> {
        check_id(id)?;
        let dir = live_dir(data_dir, id);
        if dir.exists() {
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

        fs::create_dir_all(&dir).map_err(LiveError::io(&dir))?;
        let lock = take_lock(&dir)?;
        if config_path.exists() {
            return Err(LiveError::Exists(dir));
        }
        let json = serde_json::to_vec_pretty(&saved).map_err(|e| LiveError::BadFile {
            path: config_path.clone(),
            reason: e.to_string(),
        })?;
        write_synced(&dir, CONFIG_FILE, &json)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&log_path)
            .map_err(LiveError::io(&log_path))?;
        *lock_log(&parts.log) = LogFile::new(file, log_path, 0);
        let mut live = LiveSession {
            id: id.to_string(),
            data_dir: data_dir.to_path_buf(),
            dir,
            saved,
            client,
            session: parts.session,
            handle: parts.handle,
            log: parts.log,
            status: LiveStatus {
                state: State::Stopped {
                    note: "created; not started".into(),
                },
                last_bar: None,
                position: 0.0,
                open_orders: Vec::new(),
                venue: Some(VenueRead {
                    at: created_at,
                    position: position.size,
                    avg_price: position.avg_price,
                    equity: equity.equity,
                    coin: equity.coin,
                }),
                warnings: Vec::new(),
            },
            live_from: None,
            next_bar: None,
            funding: Vec::new(),
            pending: None,
            _lock: lock,
        };
        live.save_status()?;
        Ok(live)
    }

    /// Opens a saved session to resume it: checks the engine revision, takes the lock, and
    /// replays the log through a fresh session and broker without touching the venue. A session
    /// that halted on a broker fault is refused with that fault.
    ///
    /// A last line without its newline was torn by a crash mid-write: it is cut from the file and
    /// noted in the warnings, and its bar is fetched again when the session runs. Any other line
    /// that does not read, or a log that does not replay to the state it records, is refused.
    pub fn open(
        data_dir: &Path,
        id: &str,
        client: Arc<Client>,
        build: BuildStrategy,
    ) -> Result<LiveSession, LiveError> {
        check_id(id)?;
        let dir = live_dir(data_dir, id);
        let config_path = dir.join(CONFIG_FILE);
        if !config_path.exists() {
            return Err(LiveError::NotFound(dir));
        }
        let saved: SavedConfig = read_json(&config_path)?;
        if saved.engine_revision != ENGINE_REVISION {
            return Err(LiveError::EngineChanged {
                created: saved.engine_revision,
                current: ENGINE_REVISION,
            });
        }
        let lock = take_lock(&dir)?;
        let mut status: LiveStatus = read_json(&dir.join(STATUS_FILE))?;
        if let State::Faulted { fault } = &status.state {
            return Err(LiveError::StillFaulted(fault.clone()));
        }

        let strategy = build(&saved.config.strategy, &saved.config.params)?;
        let parts = Parts::build(&saved, client.clone(), strategy, &config_path)?;
        let log_path = dir.join(EVENTS_FILE);
        let bytes = fs::read(&log_path).map_err(LiveError::io(&log_path))?;
        let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        if complete < bytes.len() {
            push_warning(
                &mut status.warnings,
                format!(
                    "{}: cut a torn last line of {} bytes",
                    log_path.display(),
                    bytes.len() - complete
                ),
            );
            OpenOptions::new()
                .write(true)
                .open(&log_path)
                .and_then(|file| file.set_len(complete as u64))
                .map_err(LiveError::io(&log_path))?;
        }
        let bad = |index: usize, reason: String| LiveError::BadLog {
            path: log_path.clone(),
            reason: format!("line {}: {reason}", index + 1),
        };
        let text = String::from_utf8_lossy(&bytes[..complete]);
        let lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| serde_json::from_str::<Line>(l).map_err(|e| bad(i, e.to_string())))
            .collect::<Result<Vec<Line>, LiveError>>()?;

        let file = OpenOptions::new()
            .append(true)
            .open(&log_path)
            .map_err(LiveError::io(&log_path))?;
        *lock_log(&parts.log) = LogFile::new(file, log_path.clone(), complete as u64);
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
        live.replay(&lines).map_err(|(i, reason)| bad(i, reason))?;
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
    /// returned so the run halts. On a log error the run must halt too.
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
        let mut log = lock_log(&self.log);
        log.armed = None;
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
        if let Some((bar, funding)) = self.pending.take() {
            let line = Line {
                bar,
                funding,
                warm_up: false,
                record: Some(self.handle.take_record()),
                sending: false,
            };
            lock_log(&self.log).append(&line, true)?;
            self.status.open_orders = self.session.open_orders().to_vec();
        }
        result.map_err(|fault| LiveError::Faulted(fault.to_string()))
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
        }
    }

    fn new(file: File, path: PathBuf, len: u64) -> LogFile {
        LogFile {
            file: Some(file),
            path,
            len,
            broken: None,
            armed: None,
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
        // Through a handle of its own: the log's is append-only, which cannot truncate.
        let cut = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .and_then(|file| file.set_len(self.len).and_then(|()| file.sync_data()));
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
/// first send or cancel reaches it.
struct LoggedBroker {
    inner: VenueBroker,
    handle: VenueHandle,
    log: Arc<Mutex<LogFile>>,
}

impl LoggedBroker {
    fn write_ahead(&self) {
        let mut log = lock_log(&self.log);
        if let Some((bar, funding)) = log.armed.take() {
            let line = Line {
                bar,
                funding,
                warm_up: false,
                record: Some(self.handle.take_record()),
                sending: true,
            };
            // A failure breaks the log, and the session halts after this bar. The order still goes
            // through: the engine has no way to refuse it here.
            let _ = log.append(&line, true);
        }
    }
}

impl Broker for LoggedBroker {
    fn submit(&mut self, order: Order) {
        self.write_ahead();
        self.inner.submit(order);
    }

    fn cancel(&mut self, id: OrderId) -> bool {
        self.write_ahead();
        self.inner.cancel(id)
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
    // shortcut: only the latest warnings are kept, so a noisy stream cannot grow the status file
    // without bound; keep a separate history if it matters.
    const KEEP: usize = 100;
    if warnings.len() == KEEP {
        warnings.remove(0);
    }
    warnings.push(warning);
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
/// leave it empty or partial.
fn write_synced(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), LiveError> {
    let path = dir.join(name);
    let temp = dir.join(format!("{name}.tmp"));
    File::create(&temp)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
        .map_err(LiveError::io(&temp))?;
    fs::rename(&temp, &path).map_err(LiveError::io(&path))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, LiveError> {
    let text = fs::read_to_string(path).map_err(LiveError::io(path))?;
    serde_json::from_str(&text).map_err(|e| LiveError::BadFile {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

fn check_id(id: &str) -> Result<(), LiveError> {
    let ok = (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
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
    use super::*;

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
        assert!(check_id("demo-1_a").is_ok());
        for bad in ["", "a/b", "..", "a b", &"x".repeat(65)] {
            assert!(matches!(check_id(bad), Err(LiveError::InvalidId(_))));
        }
    }
}
