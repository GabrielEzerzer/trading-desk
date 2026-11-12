//! A paper session's state and its files: the engine [`Session`] it feeds, the config it was made
//! from, and the append-only log of every bar fed and every manual order and cancel, which is what
//! a resume replays.
//!
//! Files live in `<data_dir>/paper/<session_id>/`: `config.json`, the config and the
//! [`ENGINE_REVISION`] the session was made with, written once and synced to disk before it takes
//! its name, and `events.jsonl`, one JSON object per line: a [`FedBar`] (`{"bar": ...}`), a manual
//! order (`{"manual_order": {"id", "request", "accepted_at"}}`: the request before rounding, the id
//! it got and when it was accepted) or a manual cancel (`{"manual_cancel": {"id"}}`). Logs written
//! before manual orders existed hold only bars and read as before. A line is appended after its
//! bar or order is applied, so the log never holds a bar the session refused or an order it
//! rejected, and a manual line sits between the bars it was placed between; with its time, a
//! replay places it exactly. Each append goes straight to the operating system, so it survives the
//! process dying. Each live bar's line and each manual line is also synced to disk, with the
//! warm-up lines before it, so a power cut loses at most the line being written and any warm-up
//! lines not yet followed by a live bar; a resume fetches those again. A manual order is answered
//! only once its line is on disk; a line that could not be written whole and synced is cut from
//! the file again where possible, so a refused order does not come back on a resume.
//!
//! A manual order takes effect from the first bar that opens at or after the moment it was
//! accepted by the local clock (see [`Session::submit`]): it never fills on a price from before
//! then, neither on the bar in progress when it was placed nor on a bar fed after it that closed
//! before it. Orders are refused while the session is catching up, more than one bar behind the
//! last closed bar (see [`PaperSession::place`]), so the strategy's checks (cash, holding,
//! leverage) count a pending manual order only at closes after it was accepted, and at the close
//! of at most one bar that closed before it but that a delayed feed delivered after it.
//!
//! A session runs a registered strategy, or none: strategy [`MANUAL_STRATEGY`] (`none`) makes a
//! manual session, where only manual orders trade. Manual orders are also allowed on a strategy
//! session, which shares its account with them; the strategy sees their fills in `on_fill` like
//! its own. Orders, fills and trades carry their [`OrderSource`].
//!
//! A session made by another engine revision still opens, so it can be listed and looked at, but
//! its log is replayed by this engine's rules, so its numbers may differ from the ones it showed.
//! It carries a warning saying so and cannot be resumed.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use td_core::{
    Candle, FeeSchedule, Fill, Instrument, InstrumentId, Millis, OrderId, OrderRequest, OrderType,
    Side, TimeInForce, Timeframe,
};
use td_engine::{
    Context, ENGINE_REVISION, EquityPoint, LIQUIDATION_TAG, Order, OrderStatus, Params, Portfolio,
    Session, SessionConfig, Strategy, Trade, build_strategy, build_trades_with_fills,
};
use tokio::sync::broadcast;
use ts_rs::TS;

use crate::{ManualError, PaperError};

const CONFIG_FILE: &str = "config.json";
const EVENTS_FILE: &str = "events.jsonl";

/// The strategy of a manual session: no strategy runs, and only manual orders trade. It takes no
/// parameters.
pub const MANUAL_STRATEGY: &str = "none";

/// The reason a manual cancel records on its order.
const CANCELLED_MANUALLY: &str = "cancelled manually";

/// What a paper session runs: a registered strategy with its parameters, or none for a manual
/// session, on one instrument and timeframe, with the same money settings as a backtest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PaperConfig {
    /// A registered strategy, or `none` ([`MANUAL_STRATEGY`]) for a manual session, which takes no
    /// parameters.
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
    /// The largest position allowed on a margined kind (perpetual, FX), as its notional over
    /// equity, as in a backtest: 1.0 means 1x, the default, which a config saved without it also
    /// gets. A kind without margin, such as spot, ignores it.
    #[serde(default = "default_max_leverage")]
    pub max_leverage: f64,
}

fn default_max_leverage() -> f64 {
    1.0
}

/// What `config.json` holds.
#[derive(Serialize, Deserialize)]
struct SavedConfig {
    #[serde(flatten)]
    config: PaperConfig,
    /// The [`ENGINE_REVISION`] the session was made with. A config saved before it was recorded
    /// reads as revision 1.
    #[serde(default = "first_engine_revision")]
    engine_revision: u32,
}

fn first_engine_revision() -> u32 {
    1
}

/// A funding rate fed with a bar.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, TS)]
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
    /// Fed during warm-up with [`Session::warm_up`]: trading is disabled and no funding is charged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub warm_up: bool,
}

/// A manual order or cancel, as a line of `events.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManualLine {
    /// The request as the API built it, before rounding, the id it was given and when it was
    /// accepted, which sets the first bar it can fill on.
    ManualOrder {
        id: OrderId,
        request: OrderRequest,
        accepted_at: Millis,
    },
    ManualCancel {
        id: OrderId,
    },
}

/// One line of `events.jsonl`.
enum Line {
    Bar(FedBar),
    Manual(ManualLine),
}

impl Line {
    fn parse(text: &str) -> Result<Line, String> {
        // Nearly every line is a bar, so bars are tried first.
        match serde_json::from_str::<FedBar>(text) {
            Ok(fed) => Ok(Line::Bar(fed)),
            Err(bar_error) => match serde_json::from_str::<ManualLine>(text) {
                Ok(manual) => Ok(Line::Manual(manual)),
                Err(e) if text.trim_start().starts_with(r#"{"manual_"#) => Err(e.to_string()),
                Err(_) => Err(bar_error.to_string()),
            },
        }
    }
}

/// `POST /sessions/{id}/orders`: an order placed by hand. It goes through the same rounding,
/// checks and broker as a strategy's order, checked at the close of the last bar fed, and takes
/// effect from the first bar that opens at or after the moment it is accepted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ManualOrder {
    pub side: Side,
    /// In the base asset; rounded down to the lot size.
    pub qty: f64,
    #[serde(rename = "type")]
    pub order_type: ManualOrderType,
    /// Required for a limit order and refused on a market one. Rounded to the tick in the order's
    /// favour: a buy down, a sell up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub limit_price: Option<f64>,
    /// Only reduce the open position; false when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reduce_only: Option<bool>,
}

/// A manual order's type. A market order fills at the open of the first bar that opens at or after
/// it is accepted; a limit order rests, good till cancelled, until a bar from then on reaches its
/// price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ManualOrderType {
    Market,
    Limit,
}

impl ManualOrder {
    /// The engine's request for this order on `instrument`, before rounding.
    fn request(&self, instrument: &InstrumentId) -> Result<OrderRequest, String> {
        let order_type = match (self.order_type, self.limit_price) {
            (ManualOrderType::Market, None) => OrderType::Market,
            (ManualOrderType::Limit, Some(price)) => OrderType::Limit { price },
            (ManualOrderType::Market, Some(_)) => {
                return Err("a market order takes no limit_price".into());
            }
            (ManualOrderType::Limit, None) => {
                return Err("a limit order needs a limit_price".into());
            }
        };
        Ok(OrderRequest {
            instrument: instrument.clone(),
            side: self.side,
            qty: self.qty,
            order_type,
            tif: TimeInForce::Gtc,
            reduce_only: self.reduce_only.unwrap_or(false),
            client_tag: None,
        })
    }
}

/// Who placed an order, and so whose its fills are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum OrderSource {
    Strategy,
    /// Placed by hand with `POST /sessions/{id}/orders`.
    Manual,
    /// The engine's close of a liquidated position (client tag `liquidation`).
    Liquidation,
}

/// An order with who placed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PaperOrder {
    #[serde(flatten)]
    pub order: Order,
    pub source: OrderSource,
}

/// A fill with who placed its order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PaperFill {
    #[serde(flatten)]
    pub fill: Fill,
    pub source: OrderSource,
}

/// Whose fills make up a trade. A liquidation's fill does not count: it only closes a trade that
/// the strategy or a person opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TradeSource {
    Strategy,
    Manual,
    /// Fills of both, such as a strategy's position closed by hand.
    Mixed,
}

impl TradeSource {
    fn of(sources: impl IntoIterator<Item = OrderSource>) -> TradeSource {
        let (mut strategy, mut manual) = (false, false);
        for source in sources {
            match source {
                OrderSource::Strategy => strategy = true,
                OrderSource::Manual => manual = true,
                OrderSource::Liquidation => {}
            }
        }
        match (strategy, manual) {
            (true, true) => TradeSource::Mixed,
            (false, true) => TradeSource::Manual,
            _ => TradeSource::Strategy,
        }
    }
}

/// A round trip with whose fills it is made of.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PaperTrade {
    #[serde(flatten)]
    pub trade: Trade,
    pub source: TradeSource,
}

/// Where a paper session is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
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

/// What a session reports as it runs, once [`PaperSession::set_events`] gives it a channel. Sent
/// while the session is locked, so a reader that subscribes and takes the state under the same lock
/// sees each change exactly once. A bar's events are sent once its line is in the log, so a
/// subscriber never sees a bar that a resume would fetch and report again. Replaying the log
/// reports nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SessionEvent {
    /// A funding rate charged with the bar that follows.
    Funding {
        funding: Funding,
    },
    /// A fill on the bar that follows.
    Fill {
        fill: PaperFill,
    },
    /// An order submitted, or one that changed state, on the bar that follows; or a manual order
    /// placed or cancelled between bars, sent on its own once its line is in the log.
    Order {
        order: PaperOrder,
    },
    /// A bar the strategy has processed, after the events it caused. `equity` is the equity at its
    /// close; warm-up bars have none.
    Bar {
        bar: Candle,
        warm_up: bool,
        equity: Option<f64>,
    },
    Status {
        status: Status,
    },
    /// The live feed left out the bucket opening at this time: too many of its minutes were
    /// missing.
    Dropped {
        open_time: Millis,
    },
    /// A problem that did not stop the session, as [`PaperSession::warnings`] keeps it.
    Warning {
        message: String,
    },
}

/// One strategy, or a person placing orders by hand, paper trading one instrument, and its files.
pub struct PaperSession {
    id: String,
    data_dir: PathBuf,
    config: PaperConfig,
    /// The [`ENGINE_REVISION`] the session was made with.
    engine_revision: u32,
    session: Session,
    log: File,
    log_path: PathBuf,
    /// The length of the log: the lines written whole, where a line that fails is cut back to.
    log_len: u64,
    status: Status,
    funding: Vec<Funding>,
    /// Open time of the first bar fed live, after warm-up.
    live_from: Option<Millis>,
    dropped: Vec<Millis>,
    warnings: Vec<String>,
    events: Option<broadcast::Sender<SessionEvent>>,
    /// The ids of the manual orders.
    manual: BTreeSet<OrderId>,
    /// Why a line could not be written, once one could not: the log no longer matches the
    /// session, which takes no more bars or orders.
    broken: Option<String>,
}

/// The folder of a paper session.
pub fn paper_dir(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join("paper").join(id)
}

impl PaperSession {
    /// Makes a new session in `<data_dir>/paper/<id>/` and writes its config, with this engine's
    /// [`ENGINE_REVISION`]. The strategy and money settings are checked before anything is
    /// written.
    pub fn create(data_dir: &Path, id: &str, config: PaperConfig) -> Result<Self, PaperError> {
        check_id(id)?;
        let session = build_session(&config)?;
        let dir = paper_dir(data_dir, id);
        let config_path = dir.join(CONFIG_FILE);
        if config_path.exists() {
            return Err(PaperError::Exists(dir));
        }
        fs::create_dir_all(&dir).map_err(PaperError::io(&dir))?;
        let saved = SavedConfig {
            config,
            engine_revision: ENGINE_REVISION,
        };
        let json = serde_json::to_vec_pretty(&saved).map_err(|e| PaperError::BadFile {
            path: config_path.clone(),
            reason: e.to_string(),
        })?;
        let temp = dir.join(format!("{CONFIG_FILE}.tmp"));
        // Synced before the rename, so a crash cannot leave an empty or partial config in place.
        File::create(&temp)
            .and_then(|mut file| file.write_all(&json).and_then(|()| file.sync_all()))
            .map_err(PaperError::io(&temp))?;
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
            config: saved.config,
            engine_revision: saved.engine_revision,
            session,
            log,
            log_path,
            log_len: 0,
            status: Status::Stopped,
            funding: Vec::new(),
            live_from: None,
            dropped: Vec::new(),
            warnings: Vec::new(),
            events: None,
            manual: BTreeSet::new(),
            broken: None,
        })
    }

    /// Opens a saved session: builds a fresh engine session from its config and replays its log,
    /// which gives exactly the state it had when it stopped.
    ///
    /// A last line without its newline was torn by a crash mid-write: it is ignored, cut from the
    /// file so later lines append cleanly, and noted in [`PaperSession::warnings`]. Its bar is
    /// fetched again when the session goes live. Any other line that does not read is an error.
    ///
    /// A session made by another [`ENGINE_REVISION`] opens with a warning, and
    /// [`PaperSession::check_resumable`] refuses it.
    pub fn open(data_dir: &Path, id: &str) -> Result<Self, PaperError> {
        check_id(id)?;
        let dir = paper_dir(data_dir, id);
        let config_path = dir.join(CONFIG_FILE);
        let text = fs::read_to_string(&config_path).map_err(PaperError::io(&config_path))?;
        let saved: SavedConfig = serde_json::from_str(&text).map_err(|e| PaperError::BadFile {
            path: config_path.clone(),
            reason: e.to_string(),
        })?;
        let log_path = dir.join(EVENTS_FILE);
        let bytes = fs::read(&log_path).map_err(PaperError::io(&log_path))?;
        let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let mut warnings = Vec::new();
        if saved.engine_revision != ENGINE_REVISION {
            warnings.push(format!(
                "this session was created with engine revision {} and its numbers are replayed \
                 with engine revision {ENGINE_REVISION}, so they may differ from what it showed; \
                 it cannot be resumed: start a new session",
                saved.engine_revision
            ));
        }
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
            session: build_session(&saved.config)?,
            config: saved.config,
            engine_revision: saved.engine_revision,
            log,
            log_path: log_path.clone(),
            log_len: complete as u64,
            status: Status::Stopped,
            funding: Vec::new(),
            live_from: None,
            dropped: Vec::new(),
            warnings,
            events: None,
            manual: BTreeSet::new(),
            broken: None,
        };
        let text = String::from_utf8_lossy(&bytes[..complete]);
        for (index, line) in text.lines().enumerate() {
            let bad = |reason: String| PaperError::BadFile {
                path: log_path.clone(),
                reason: format!("line {}: {reason}", index + 1),
            };
            match Line::parse(line).map_err(bad)? {
                Line::Bar(fed) => paper.apply(&fed).map_err(|e| bad(e.to_string()))?,
                Line::Manual(manual) => {
                    if let Err(reason) = paper.replay(manual) {
                        // Another engine's rules may refuse what this session accepted; the
                        // session still opens to be looked at.
                        if paper.engine_revision == ENGINE_REVISION {
                            return Err(bad(reason));
                        }
                        let skipped = format!("line {}: {reason}; skipped", index + 1);
                        paper.warnings.push(skipped);
                    }
                }
            }
        }
        Ok(paper)
    }

    /// Applies a manual line from the log as it was applied when it was written.
    fn replay(&mut self, line: ManualLine) -> Result<(), String> {
        match line {
            ManualLine::ManualOrder {
                id,
                request,
                accepted_at,
            } => {
                // Checked before it is submitted: an order placed under another id stays placed,
                // and would count as the strategy's.
                let next = self.session.orders().len() as u64 + 1;
                if id.0 != next {
                    return Err(format!(
                        "manual order {} would replay as order {next}",
                        id.0
                    ));
                }
                self.session
                    .submit(request, accepted_at)
                    .map_err(|reason| format!("manual order {} is now rejected: {reason}", id.0))?;
                self.manual.insert(id);
            }
            ManualLine::ManualCancel { id } => {
                if !self.manual.contains(&id) || !self.session.cancel(id, CANCELLED_MANUALLY) {
                    return Err(format!("manual order {} is not open to cancel", id.0));
                }
            }
        }
        Ok(())
    }

    /// Feeds one bar with its funding to the engine session, then appends it to the log and
    /// reports its events. A warm-up bar goes through [`Session::warm_up`], with trading disabled.
    /// A live bar's line is synced to disk before this returns; warm-up lines are synced with the
    /// first live one. On an engine error nothing changes; on a log error the bar has been applied
    /// but may not be saved and is not reported, so stop the session.
    pub fn feed(
        &mut self,
        bar: Candle,
        funding: Vec<Funding>,
        warm_up: bool,
    ) -> Result<(), PaperError> {
        self.check_log()?;
        let fed = FedBar {
            bar,
            funding,
            warm_up,
        };
        let before = self.events.as_ref().map(|_| self.mark());
        self.apply(&fed)?;
        let events = before.map(|before| self.report(&fed, before));
        self.append(&fed, !warm_up)?;
        for event in events.into_iter().flatten() {
            self.emit(event);
        }
        Ok(())
    }

    /// Places a manual order between bars, accepted at time `accepted_at`: the engine rounds and
    /// checks it at the close of the last bar fed, as it does a strategy's, and it takes effect
    /// from the first bar that opens at or after `accepted_at` (see [`Session::submit`]). A market
    /// order fills at that bar's open, and no bar before it can fill a limit, so the order never
    /// fills on a price from before it was placed. An accepted order is appended to the log with
    /// `accepted_at` and synced, then reported and returned. A rejected one changes nothing.
    ///
    /// Refused before the first live bar, and while the session is catching up: when the last
    /// bar fed opens more than one bar before the last bar closed at `accepted_at`, as during a
    /// resume's backfill or behind a lagging feed. The strategy's checks at the close of each bar
    /// fed count a pending order, so an order accepted then would change its decisions on the bars
    /// still to be fed from before the order. One bar of delay is allowed for a feed that has not
    /// delivered the bar that just closed yet; that bar is the only one from before the order
    /// whose checks can see it. On a log error the order has been placed but is not saved, and
    /// the session takes no more bars or orders: stop it.
    pub fn place(
        &mut self,
        order: &ManualOrder,
        accepted_at: Millis,
    ) -> Result<PaperOrder, ManualError> {
        self.check_log()?;
        let Some(last) = self.last_bar().filter(|_| self.live_from.is_some()) else {
            return Err(ManualError::NotLive);
        };
        let timeframe = self.config.timeframe;
        if last.open_time < timeframe.floor(accepted_at) - 2 * timeframe.duration_ms() {
            return Err(ManualError::CatchingUp);
        }
        let request = order
            .request(&self.config.instrument.id)
            .map_err(ManualError::Rejected)?;
        let placed = self
            .session
            .submit(request.clone(), accepted_at)
            .map_err(ManualError::Rejected)?;
        let id = placed.id;
        self.manual.insert(id);
        let line = ManualLine::ManualOrder {
            id,
            request,
            accepted_at,
        };
        self.append(&line, true)?;
        let order = self.with_source(&placed);
        self.emit(SessionEvent::Order {
            order: order.clone(),
        });
        Ok(order)
    }

    /// Cancels an open manual order between bars, then appends the cancel to the log, syncs it,
    /// reports the order and returns it. An order that does not exist, was not placed by hand or
    /// is no longer open is refused and nothing changes. On a log error, as for
    /// [`PaperSession::place`].
    pub fn cancel(&mut self, id: OrderId) -> Result<PaperOrder, ManualError> {
        self.check_log()?;
        let index = (id.0 as usize).wrapping_sub(1);
        let status = match self.session.orders().get(index) {
            None => return Err(ManualError::NoSuchOrder(id.0)),
            Some(_) if !self.manual.contains(&id) => return Err(ManualError::NotManual(id.0)),
            Some(order) => order.status.clone(),
        };
        if status != OrderStatus::Open || !self.session.cancel(id, CANCELLED_MANUALLY) {
            let state = match status {
                OrderStatus::Attached => "attached to an entry that has not filled",
                OrderStatus::Open => "not open at the broker",
                OrderStatus::Filled => "filled",
                OrderStatus::Cancelled(_) => "cancelled",
                OrderStatus::Rejected(_) => "rejected",
            };
            return Err(ManualError::Done { id: id.0, state });
        }
        self.append(&ManualLine::ManualCancel { id }, true)?;
        let order = self.with_source(&self.session.orders()[index]);
        self.emit(SessionEvent::Order {
            order: order.clone(),
        });
        Ok(order)
    }

    /// Refuses to go on once a line could not be written.
    fn check_log(&self) -> Result<(), PaperError> {
        match &self.broken {
            Some(reason) => Err(PaperError::LogFailed(reason.clone())),
            None => Ok(()),
        }
    }

    /// Appends one line to the log, synced to disk if `sync`. A failure breaks the session (see
    /// [`PaperSession::check_log`]) and cuts from the file whatever part of the line reached it,
    /// so a line the caller is told failed does not replay.
    fn append(&mut self, line: &impl Serialize, sync: bool) -> Result<(), PaperError> {
        let mut text = serde_json::to_string(line).map_err(|e| PaperError::BadFile {
            path: self.log_path.clone(),
            reason: e.to_string(),
        })?;
        text.push('\n');
        // A `File` has no buffer: the write reaches the operating system at once.
        let written = self
            .log
            .write_all(text.as_bytes())
            .and_then(|()| if sync { sync_data(&self.log) } else { Ok(()) });
        let Err(e) = written else {
            self.log_len += text.len() as u64;
            return Ok(());
        };
        let e = PaperError::io(&self.log_path)(e);
        let mut reason = e.to_string();
        // Through a handle of its own: the log's is append-only, which cannot truncate.
        let cut = OpenOptions::new()
            .write(true)
            .open(&self.log_path)
            .and_then(|file| file.set_len(self.log_len).and_then(|()| file.sync_data()));
        if let Err(cut) = cut {
            reason.push_str(&format!(
                "; the line could not be cut from the log either, so a resume replays it: {cut}"
            ));
        }
        self.broken = Some(reason);
        Err(e)
    }

    /// Where the orders and fills stand, to report what a bar changes.
    fn mark(&self) -> Mark {
        let orders = self.session.orders();
        Mark {
            orders: orders.len(),
            fills: self.session.fills().len(),
            pending: orders
                .iter()
                .enumerate()
                .filter(|(_, o)| matches!(o.status, OrderStatus::Open | OrderStatus::Attached))
                .map(|(i, o)| (i, o.status.clone()))
                .collect(),
        }
    }

    /// The events of the bar just applied.
    fn report(&self, fed: &FedBar, before: Mark) -> Vec<SessionEvent> {
        let orders = self.session.orders();
        let changed = before
            .pending
            .iter()
            .filter(|(i, status)| orders.get(*i).is_some_and(|o| o.status != *status))
            .map(|(i, _)| *i)
            .chain(before.orders..orders.len());
        let mut events: Vec<SessionEvent> = Vec::new();
        if !fed.warm_up {
            events.extend(
                fed.funding
                    .iter()
                    .map(|&funding| SessionEvent::Funding { funding }),
            );
        }
        events.extend(
            self.session.fills()[before.fills..]
                .iter()
                .map(|fill| SessionEvent::Fill {
                    fill: self.fill_with_source(fill),
                }),
        );
        events.extend(changed.map(|i| SessionEvent::Order {
            order: self.with_source(&orders[i]),
        }));
        events.push(SessionEvent::Bar {
            bar: fed.bar,
            warm_up: fed.warm_up,
            equity: (!fed.warm_up).then(|| self.session.portfolio().equity()),
        });
        events
    }

    /// Sends an event to the channel, if there is one. No one listening is fine.
    fn emit(&self, event: SessionEvent) {
        if let Some(events) = &self.events {
            let _ = events.send(event);
        }
    }

    /// Gives the session a channel to report its events on, from now on.
    pub fn set_events(&mut self, events: broadcast::Sender<SessionEvent>) {
        self.events = Some(events);
    }

    fn apply(&mut self, fed: &FedBar) -> Result<(), PaperError> {
        let funding: Vec<(Millis, f64)> = fed.funding.iter().map(|f| (f.time, f.rate)).collect();
        if fed.warm_up {
            self.session.warm_up(fed.bar, &funding)?;
        } else {
            self.session.on_bar(fed.bar, &funding)?;
            self.live_from.get_or_insert(fed.bar.open_time);
            // The engine charges no funding on a warm-up bar.
            self.funding.extend(&fed.funding);
        }
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

    /// The [`ENGINE_REVISION`] the session was made with.
    pub fn engine_revision(&self) -> u32 {
        self.engine_revision
    }

    /// Refuses to run a session made by another [`ENGINE_REVISION`]: its log replays by this
    /// engine's rules, so going on from it would trade from numbers it never had.
    pub fn check_resumable(&self) -> Result<(), PaperError> {
        if self.engine_revision == ENGINE_REVISION {
            Ok(())
        } else {
            Err(PaperError::EngineChanged {
                created: self.engine_revision,
                current: ENGINE_REVISION,
            })
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub(crate) fn set_status(&mut self, status: Status) {
        if self.status != status {
            self.status = status.clone();
            self.emit(SessionEvent::Status { status });
        }
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

    /// Who placed the order with this id.
    pub fn source(&self, id: OrderId) -> OrderSource {
        let liquidation = || {
            let order = self.session.orders().get((id.0 as usize).wrapping_sub(1));
            order.is_some_and(|o| o.request.client_tag.as_deref() == Some(LIQUIDATION_TAG))
        };
        if self.manual.contains(&id) {
            OrderSource::Manual
        } else if liquidation() {
            OrderSource::Liquidation
        } else {
            OrderSource::Strategy
        }
    }

    fn with_source(&self, order: &Order) -> PaperOrder {
        PaperOrder {
            order: order.clone(),
            source: self.source(order.id),
        }
    }

    fn fill_with_source(&self, fill: &Fill) -> PaperFill {
        PaperFill {
            fill: fill.clone(),
            source: self.source(fill.order_id),
        }
    }

    /// [`PaperSession::open_orders`], each with who placed it.
    pub fn open_orders_with_source(&self) -> Vec<PaperOrder> {
        self.open_orders()
            .iter()
            .map(|o| self.with_source(o))
            .collect()
    }

    /// [`PaperSession::fills`], each with who placed its order.
    pub fn fills_with_source(&self) -> Vec<PaperFill> {
        self.fills()
            .iter()
            .map(|f| self.fill_with_source(f))
            .collect()
    }

    /// Round trips so far over the live bars, each with whose fills it is made of; one still open
    /// is marked at the last close.
    pub fn trades(&self) -> Vec<PaperTrade> {
        let session = &self.session;
        // One bar funding entry per live bar, after the warm-up bars.
        let bars = session.bars();
        let live = &bars[bars.len().saturating_sub(session.bar_funding().len())..];
        let fills = session.fills();
        build_trades_with_fills(
            fills,
            live,
            &[],
            session.bar_funding(),
            session.portfolio().account(),
            session.instrument().pip_size(),
        )
        .into_iter()
        .map(|(trade, range)| PaperTrade {
            trade,
            source: TradeSource::of(fills[range].iter().map(|f| self.source(f.order_id))),
        })
        .collect()
    }

    /// Equity at each live bar's close. Warm-up bars record none, so the series starts at
    /// [`PaperSession::live_from`] and is shorter than the bars by the warm-up bars.
    pub fn equity(&self) -> &[EquityPoint] {
        self.session.equity()
    }

    pub fn last_bar(&self) -> Option<&Candle> {
        self.session.bars().last()
    }

    /// Every funding rate charged, in order. Funding fed with a warm-up bar is not charged.
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
        self.emit(SessionEvent::Dropped { open_time });
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
        self.warnings.push(warning.clone());
        self.emit(SessionEvent::Warning { message: warning });
    }
}

/// Orders and fills before a bar: how many there were, and the orders that could still change, by
/// index, with their status.
struct Mark {
    orders: usize,
    fills: usize,
    pending: Vec<(usize, OrderStatus)>,
}

#[cfg(test)]
thread_local! {
    /// Makes [`sync_data`] fail on this thread, to test a line that was written but not synced.
    static FAIL_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// [`File::sync_data`]; tests can make it fail.
fn sync_data(file: &File) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_SYNC.get() {
        return Err(std::io::Error::other("sync failed (test)"));
    }
    file.sync_data()
}

/// The strategy of a manual session.
struct NoStrategy;

impl Strategy for NoStrategy {
    fn on_bar(&mut self, _ctx: &mut Context, _bar: &Candle) {}
}

fn build_session(config: &PaperConfig) -> Result<Session, PaperError> {
    let strategy: Box<dyn Strategy> = if config.strategy == MANUAL_STRATEGY {
        // It takes no parameters, so any given is unknown.
        config.params.resolve(&[])?;
        Box::new(NoStrategy)
    } else {
        build_strategy(&config.strategy, &config.params)?
    };
    let money = SessionConfig {
        starting_cash: config.starting_cash,
        slippage_bps: config.slippage_bps,
        fees: config.fees,
        max_leverage: config.max_leverage,
        // shortcut: paper sessions are on a quote-currency account; add the account currency to
        // `PaperConfig` when paper trading takes a market with quotes.
        account_currency: td_core::AccountCurrency::Quote,
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

#[cfg(test)]
mod tests {
    use td_core::{
        InstrumentId, InstrumentKind, OrderRequest, OrderType, Side, TimeInForce, Venue,
    };
    use td_engine::{Context, OrderStatus, Strategy};

    use super::*;

    const MIN: Millis = 60_000;

    /// Tries to buy on every bar, warm-up bars included.
    struct AlwaysBuy;

    impl Strategy for AlwaysBuy {
        fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
            let _ = ctx.submit(OrderRequest {
                instrument: ctx.instrument().id.clone(),
                side: Side::Buy,
                qty: 1.0,
                order_type: OrderType::Market,
                tif: TimeInForce::Gtc,
                reduce_only: false,
                client_tag: None,
            });
        }
    }

    fn bar(i: i64) -> Candle {
        Candle {
            open_time: i * MIN,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.0,
            volume: 1.0,
            quote_volume: 100.0,
        }
    }

    fn config() -> PaperConfig {
        PaperConfig {
            strategy: "sma_cross".into(),
            params: Params::new(),
            instrument: Instrument {
                id: InstrumentId {
                    venue: Venue::Binance,
                    symbol: "TESTUSDT".into(),
                    kind: InstrumentKind::Perpetual,
                },
                base: "TEST".into(),
                quote: "USDT".into(),
                tick_size: 0.01,
                lot_size: 0.001,
                min_qty: 0.001,
                min_notional: 1.0,
                fees: FeeSchedule {
                    maker: 0.001,
                    taker: 0.002,
                    per_unit: 0.0,
                },
            },
            timeframe: Timeframe::M1,
            starting_cash: 10_000.0,
            slippage_bps: 0.0,
            fees: None,
            max_leverage: 1.0,
        }
    }

    #[test]
    fn a_strategy_cannot_trade_on_warm_up_bars() {
        let config = config();
        let dir = tempfile::tempdir().expect("temp dir");
        let mut paper = PaperSession::create(dir.path(), "warm", config.clone()).expect("creates");
        // The registry has no strategy that trades while warming up, so swap one in.
        paper.session = Session::new(
            Box::new(AlwaysBuy),
            config.instrument.clone(),
            config.timeframe,
            SessionConfig {
                starting_cash: config.starting_cash,
                slippage_bps: config.slippage_bps,
                fees: config.fees,
                max_leverage: config.max_leverage,
                account_currency: td_core::AccountCurrency::Quote,
            },
        )
        .expect("session");

        let rate = Funding {
            time: 0,
            rate: 0.01,
            due: 0,
        };
        paper.feed(bar(0), vec![rate], true).expect("warm-up bar");
        for i in 1..5 {
            paper.feed(bar(i), Vec::new(), true).expect("warm-up bar");
        }
        assert_eq!(paper.orders().len(), 5, "the strategy tried on every bar");
        for order in paper.orders() {
            assert_eq!(
                order.status,
                OrderStatus::Rejected("warm-up: trading is disabled".into())
            );
        }
        assert!(paper.fills().is_empty() && paper.open_orders().is_empty());
        assert!(paper.equity().is_empty() && paper.funding().is_empty());
        assert_eq!(
            paper.portfolio(),
            &Portfolio::new(InstrumentKind::Perpetual, 10_000.0)
        );
        assert_eq!(paper.live_from(), None);

        // Live, its orders go through and fill on the next bar.
        paper.feed(bar(5), Vec::new(), false).expect("live bar");
        paper.feed(bar(6), Vec::new(), false).expect("live bar");
        assert_eq!(paper.fills().len(), 1);
        assert_eq!(paper.equity().len(), 2);
        assert_eq!(paper.live_from(), Some(5 * MIN));

        // A warm-up bar after a live one is refused and never reaches the log.
        assert!(paper.feed(bar(7), Vec::new(), true).is_err());
        let log = fs::read_to_string(&paper.log_path).expect("reads");
        assert_eq!(log.lines().count(), 7);
        assert_eq!(
            log.lines()
                .filter(|l| l.contains(r#""warm_up":true"#))
                .count(),
            5
        );
    }

    #[test]
    fn a_bar_whose_line_cannot_be_written_is_not_reported() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut paper = PaperSession::create(dir.path(), "torn", config()).expect("creates");
        let (events, mut rx) = broadcast::channel(16);
        paper.set_events(events);
        paper.feed(bar(0), Vec::new(), false).expect("live bar");
        assert!(matches!(rx.try_recv(), Ok(SessionEvent::Bar { .. })));

        // A handle that cannot write: the next line fails.
        paper.log = File::open(&paper.log_path).expect("opens read-only");
        assert!(matches!(
            paper.feed(bar(1), Vec::new(), false),
            Err(PaperError::Io { .. })
        ));
        assert_eq!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty),
            "a bar that is not in the log was reported"
        );
    }

    #[test]
    fn a_manual_order_written_but_not_synced_is_cut_from_the_log_and_does_not_replay() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = PaperConfig {
            strategy: MANUAL_STRATEGY.into(),
            ..config()
        };
        let mut paper = PaperSession::create(dir.path(), "unsynced", config).expect("creates");
        paper.feed(bar(0), Vec::new(), false).expect("live bar");
        let before = fs::read(&paper.log_path).expect("reads");
        let order = ManualOrder {
            side: Side::Buy,
            qty: 1.0,
            order_type: ManualOrderType::Market,
            limit_price: None,
            reduce_only: None,
        };

        // The line reaches the file, but syncing it fails: the caller is told the order failed.
        FAIL_SYNC.set(true);
        let placed = paper.place(&order, MIN);
        FAIL_SYNC.set(false);
        assert!(
            matches!(placed, Err(ManualError::Paper(PaperError::Io { .. }))),
            "{placed:?}"
        );
        assert_eq!(
            fs::read(&paper.log_path).expect("reads"),
            before,
            "the failed line is still in the log"
        );
        drop(paper);
        let reopened = PaperSession::open(dir.path(), "unsynced").expect("opens");
        assert!(reopened.orders().is_empty(), "{:?}", reopened.orders());
        assert_eq!(reopened.session().bars(), [bar(0)]);
    }

    #[test]
    fn a_manual_order_whose_line_cannot_be_written_is_not_reported_and_stops_the_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = PaperConfig {
            strategy: MANUAL_STRATEGY.into(),
            ..config()
        };
        let mut paper = PaperSession::create(dir.path(), "unsaved", config).expect("creates");
        let (events, mut rx) = broadcast::channel(16);
        paper.set_events(events);
        paper.feed(bar(0), Vec::new(), false).expect("live bar");
        while rx.try_recv().is_ok() {}
        let order = ManualOrder {
            side: Side::Buy,
            qty: 1.0,
            order_type: ManualOrderType::Market,
            limit_price: None,
            reduce_only: None,
        };

        // A handle that cannot write: the order's line fails.
        paper.log = File::open(&paper.log_path).expect("opens read-only");
        assert!(matches!(
            paper.place(&order, MIN),
            Err(ManualError::Paper(PaperError::Io { .. }))
        ));
        assert_eq!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty),
            "an order that is not in the log was reported"
        );
        // The log no longer matches the session, which takes nothing more, even once it could
        // write again.
        paper.log = OpenOptions::new()
            .append(true)
            .open(&paper.log_path)
            .expect("opens");
        assert!(matches!(
            paper.place(&order, MIN),
            Err(ManualError::Paper(PaperError::LogFailed(_)))
        ));
        assert!(matches!(
            paper.feed(bar(1), Vec::new(), false),
            Err(PaperError::LogFailed(_))
        ));
        assert_eq!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty));
    }

    #[test]
    fn dropped_buckets_and_warnings_are_reported() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut paper = PaperSession::create(dir.path(), "notes", config()).expect("creates");
        let (events, mut rx) = broadcast::channel(16);
        paper.set_events(events);
        paper.push_dropped(5 * MIN);
        paper.warn("a frame did not read".into());
        assert_eq!(
            rx.try_recv(),
            Ok(SessionEvent::Dropped { open_time: 5 * MIN })
        );
        assert_eq!(
            rx.try_recv(),
            Ok(SessionEvent::Warning {
                message: "a frame did not read".into()
            })
        );
        assert_eq!(paper.dropped(), [5 * MIN]);
        assert_eq!(paper.warnings(), ["a frame did not read"]);
    }
}
