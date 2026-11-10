//! A paper session's state and its files: the engine [`Session`] it feeds, the config it was made
//! from, and the append-only log of every bar fed, which is what a resume replays.
//!
//! Files live in `<data_dir>/paper/<session_id>/`: `config.json`, written once, and
//! `events.jsonl`, one [`FedBar`] per line. A line is appended and flushed after its bar is
//! applied, so the log never holds a bar the session refused.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use td_core::{Candle, FeeSchedule, Fill, Instrument, Millis, Timeframe};
use td_engine::{EquityPoint, Order, Params, Portfolio, Session, SessionConfig, build_strategy};

use crate::PaperError;

const CONFIG_FILE: &str = "config.json";
const EVENTS_FILE: &str = "events.jsonl";

/// What a paper session runs: a registered strategy with its parameters, on one instrument and
/// timeframe, with the same money settings as a backtest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperConfig {
    pub strategy: String,
    pub params: Params,
    /// The instrument with the trading rules it had when the session was made. Fixed for the
    /// session's life, so a resume replays against the same rules.
    pub instrument: Instrument,
    pub timeframe: Timeframe,
    pub starting_cash: f64,
    pub slippage_bps: f64,
    /// Replaces the instrument's fee schedule when set.
    pub fees: Option<FeeSchedule>,
}

/// A funding rate fed with a bar.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Funding {
    /// The time the engine was given, inside the bar's window.
    pub time: Millis,
    pub rate: f64,
    /// The venue's funding time. Earlier than `time` when the rate was not yet published when its
    /// own bar closed and was charged on this later bar.
    pub due: Millis,
}

/// One line of `events.jsonl`: a bar and the funding fed with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FedBar {
    pub bar: Candle,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub funding: Vec<Funding>,
    /// Fed during warm-up, with trading disabled.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub warm_up: bool,
}

/// Where a paper session is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "message", rename_all = "snake_case")]
pub enum Status {
    /// Feeding recent history to the strategy before trading.
    WarmingUp,
    /// Connected to the stream and trading on each closed bar.
    Live,
    /// Not connected to the stream, before the first connection or after a drop, or waiting on a
    /// REST backfill. Bars resume, missed ones first, once it is back.
    Reconnecting,
    Stopped,
    /// Stopped by an error.
    Error(String),
}

/// One strategy paper trading one instrument, and its files.
pub struct PaperSession {
    id: String,
    data_dir: PathBuf,
    config: PaperConfig,
    session: Session,
    log: File,
    log_path: PathBuf,
    status: Status,
    funding: Vec<Funding>,
    /// Open time of the first bar fed live, after warm-up.
    live_from: Option<Millis>,
    dropped: Vec<Millis>,
    warnings: Vec<String>,
}

/// The folder of a paper session.
pub fn paper_dir(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join("paper").join(id)
}

impl PaperSession {
    /// Makes a new session in `<data_dir>/paper/<id>/` and writes its config. The strategy and
    /// money settings are checked before anything is written.
    pub fn create(data_dir: &Path, id: &str, config: PaperConfig) -> Result<Self, PaperError> {
        check_id(id)?;
        let session = build_session(&config)?;
        let dir = paper_dir(data_dir, id);
        let config_path = dir.join(CONFIG_FILE);
        if config_path.exists() {
            return Err(PaperError::Exists(dir));
        }
        fs::create_dir_all(&dir).map_err(PaperError::io(&dir))?;
        let json = serde_json::to_vec_pretty(&config).map_err(|e| PaperError::BadFile {
            path: config_path.clone(),
            reason: e.to_string(),
        })?;
        let temp = dir.join(format!("{CONFIG_FILE}.tmp"));
        fs::write(&temp, json).map_err(PaperError::io(&temp))?;
        fs::rename(&temp, &config_path).map_err(PaperError::io(&config_path))?;
        let log_path = dir.join(EVENTS_FILE);
        let log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&log_path)
            .map_err(PaperError::io(&log_path))?;
        Ok(PaperSession {
            id: id.to_string(),
            data_dir: data_dir.to_path_buf(),
            config,
            session,
            log,
            log_path,
            status: Status::Stopped,
            funding: Vec::new(),
            live_from: None,
            dropped: Vec::new(),
            warnings: Vec::new(),
        })
    }

    /// Opens a saved session: builds a fresh engine session from its config and replays its log,
    /// which gives exactly the state it had when it stopped.
    ///
    /// A last line without its newline was torn by a crash mid-write: it is ignored, cut from the
    /// file so later lines append cleanly, and noted in [`PaperSession::warnings`]. Its bar is
    /// fetched again when the session goes live. Any other line that does not read is an error.
    pub fn open(data_dir: &Path, id: &str) -> Result<Self, PaperError> {
        check_id(id)?;
        let dir = paper_dir(data_dir, id);
        let config_path = dir.join(CONFIG_FILE);
        let text = fs::read_to_string(&config_path).map_err(PaperError::io(&config_path))?;
        let config: PaperConfig = serde_json::from_str(&text).map_err(|e| PaperError::BadFile {
            path: config_path.clone(),
            reason: e.to_string(),
        })?;
        let log_path = dir.join(EVENTS_FILE);
        let bytes = fs::read(&log_path).map_err(PaperError::io(&log_path))?;
        let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let mut warnings = Vec::new();
        if complete < bytes.len() {
            warnings.push(format!(
                "{}: ignored a torn last line of {} bytes",
                log_path.display(),
                bytes.len() - complete
            ));
            let file = OpenOptions::new()
                .write(true)
                .open(&log_path)
                .map_err(PaperError::io(&log_path))?;
            file.set_len(complete as u64)
                .map_err(PaperError::io(&log_path))?;
        }
        let log = OpenOptions::new()
            .append(true)
            .open(&log_path)
            .map_err(PaperError::io(&log_path))?;
        let mut paper = PaperSession {
            id: id.to_string(),
            data_dir: data_dir.to_path_buf(),
            session: build_session(&config)?,
            config,
            log,
            log_path: log_path.clone(),
            status: Status::Stopped,
            funding: Vec::new(),
            live_from: None,
            dropped: Vec::new(),
            warnings,
        };
        let text = String::from_utf8_lossy(&bytes[..complete]);
        for (index, line) in text.lines().enumerate() {
            let bad = |reason: String| PaperError::BadFile {
                path: log_path.clone(),
                reason: format!("line {}: {reason}", index + 1),
            };
            let fed: FedBar = serde_json::from_str(line).map_err(|e| bad(e.to_string()))?;
            paper.apply(&fed).map_err(|e| bad(e.to_string()))?;
        }
        Ok(paper)
    }

    /// Feeds one bar with its funding to the engine session, then appends it to the log. A
    /// warm-up bar goes through [`Session::warm_up`], with trading disabled. On an engine error
    /// nothing changes; on a log error the bar has been applied but not saved, so stop the session.
    pub fn feed(
        &mut self,
        bar: Candle,
        funding: Vec<Funding>,
        warm_up: bool,
    ) -> Result<(), PaperError> {
        let fed = FedBar {
            bar,
            funding,
            warm_up,
        };
        self.apply(&fed)?;
        let mut line = serde_json::to_string(&fed).map_err(|e| PaperError::BadFile {
            path: self.log_path.clone(),
            reason: e.to_string(),
        })?;
        line.push('\n');
        self.log
            .write_all(line.as_bytes())
            .and_then(|()| self.log.flush())
            .map_err(PaperError::io(&self.log_path))
    }

    fn apply(&mut self, fed: &FedBar) -> Result<(), PaperError> {
        let funding: Vec<(Millis, f64)> = fed.funding.iter().map(|f| (f.time, f.rate)).collect();
        if fed.warm_up {
            self.session.warm_up(fed.bar, &funding)?;
        } else {
            self.session.on_bar(fed.bar, &funding)?;
            self.live_from.get_or_insert(fed.bar.open_time);
        }
        self.funding.extend(&fed.funding);
        Ok(())
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn config(&self) -> &PaperConfig {
        &self.config
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub(crate) fn set_status(&mut self, status: Status) {
        self.status = status;
    }

    /// The engine session, for anything the accessors below do not cover.
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn portfolio(&self) -> &Portfolio {
        self.session.portfolio()
    }

    pub fn open_orders(&self) -> &[Order] {
        self.session.open_orders()
    }

    /// Every order submitted, rejected ones included.
    pub fn orders(&self) -> &[Order] {
        self.session.orders()
    }

    pub fn fills(&self) -> &[Fill] {
        self.session.fills()
    }

    /// Equity at each bar's close, warm-up bars included.
    pub fn equity(&self) -> &[EquityPoint] {
        self.session.equity()
    }

    pub fn last_bar(&self) -> Option<&Candle> {
        self.session.bars().last()
    }

    /// Every funding rate charged, in order.
    pub fn funding(&self) -> &[Funding] {
        &self.funding
    }

    /// Open time of the first bar fed after warm-up.
    pub fn live_from(&self) -> Option<Millis> {
        self.live_from
    }

    /// Open time of the next bar the session expects: the end of the last bar fed.
    pub fn next_bar_time(&self) -> Option<Millis> {
        self.last_bar()
            .map(|bar| bar.open_time + self.config.timeframe.duration_ms())
    }

    /// Buckets the live feed left out because too many of their minutes were missing.
    pub fn dropped(&self) -> &[Millis] {
        &self.dropped
    }

    pub(crate) fn push_dropped(&mut self, open_time: Millis) {
        self.dropped.push(open_time);
    }

    /// Things that went wrong without stopping the session: a torn log line, a frame that did not
    /// read, a funding rate that could not be fetched.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub(crate) fn warn(&mut self, warning: String) {
        // shortcut: only the latest warnings are kept, so a noisy stream cannot grow memory
        // without bound; persist them if the history matters.
        const KEEP: usize = 100;
        if self.warnings.len() == KEEP {
            self.warnings.remove(0);
        }
        self.warnings.push(warning);
    }
}

// TEMP: stands in for td-engine's `Session::warm_up` until the engine review fix lands.
trait WarmUp {
    fn warm_up(
        &mut self,
        bar: Candle,
        funding: &[(Millis, f64)],
    ) -> Result<(), td_engine::EngineError>;
}

impl WarmUp for Session {
    fn warm_up(
        &mut self,
        bar: Candle,
        funding: &[(Millis, f64)],
    ) -> Result<(), td_engine::EngineError> {
        self.on_bar(bar, funding)
    }
}

fn build_session(config: &PaperConfig) -> Result<Session, PaperError> {
    let strategy = build_strategy(&config.strategy, &config.params)?;
    let money = SessionConfig {
        starting_cash: config.starting_cash,
        slippage_bps: config.slippage_bps,
        fees: config.fees,
    };
    Ok(Session::new(
        strategy,
        config.instrument.clone(),
        config.timeframe,
        money,
    )?)
}

fn check_id(id: &str) -> Result<(), PaperError> {
    let ok = (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(PaperError::InvalidId(id.to_string()))
    }
}
