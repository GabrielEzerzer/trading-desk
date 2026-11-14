//! Paper sessions over the API: start, list, inspect, stop and resume, place and cancel manual
//! orders, with each session's events on the `session:<id>` WebSocket topic.
//!
//! Every session in the data directory is loaded when the server starts, by replaying its log, and
//! listed as stopped: one that was running when the server last stopped resumes with
//! `POST /sessions/{id}/resume`, which replays the log again and backfills the bars it missed (see
//! [`crate::start`]). A session made by another engine revision is listed with a warning and its
//! resume is refused. The server stops every running session when it shuts down.
//!
//! Sessions run on the server's shared live streams ([`super::live`]), so a session and the
//! `candles:` topics of its instrument use one upstream connection.
//!
//! Each run of a session reports its events on a channel of its own. A resume opens a new one and
//! drops the old, so a `session:` subscriber that stays through a stop and resume sees its channel
//! close, takes the resumed session's full state and follows the new channel from there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use td_core::{AccountCurrency, Candle, FeeSchedule, InstrumentId, Millis, OrderId, Timeframe};
use td_data::time::now_ms;
use td_engine::{
    BacktestConfig, EquityPoint, Params, Recorded, RunId, SessionComparison, StrategyEntry,
    compare_with_backtest,
};
use tokio::sync::{broadcast, oneshot, watch};
use tokio::task::JoinHandle;
use ts_rs::TS;

use super::AppState;
use super::data;
use super::error::ApiError;
use super::live::Streams;
use super::plans::{LinkState, SessionLink, session_link};
use crate::{
    Funding, MANUAL_STRATEGY, ManualError, ManualOrder, OrderSource, PaperConfig, PaperError,
    PaperFill, PaperOrder, PaperSession, PaperTrade, SessionEvent, Status,
};

/// Events buffered per `session:` subscriber before it is sent the full state again.
pub(super) const EVENT_BUFFER: usize = 1024;

/// `POST /sessions`: what to paper trade. The instrument's trading rules come from the venue's
/// instrument list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct StartSessionRequest {
    /// A registered strategy, as `GET /strategies` lists it, or `none` for a manual session, where
    /// only manual orders trade; it takes no parameters.
    pub strategy: String,
    pub params: Params,
    pub instrument: InstrumentId,
    pub timeframe: Timeframe,
    pub starting_cash: f64,
    pub slippage_bps: f64,
    /// Replaces the instrument's fee schedule when set.
    #[serde(default)]
    #[ts(optional)]
    pub fees: Option<FeeSchedule>,
    /// The saved verdict (a plan run's id) the session is started from: a verdict of the same
    /// strategy on the same instrument. Left out, the strategy's latest verdict on the instrument
    /// as `GET /verdicts` gives it, if it has one. Refused on a manual session.
    #[serde(default)]
    #[ts(optional)]
    pub verdict_run_id: Option<RunId>,
}

/// A session as `GET /sessions` lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SessionSummary {
    pub id: String,
    pub strategy: String,
    pub instrument: InstrumentId,
    pub timeframe: Timeframe,
    pub status: Status,
    /// Equity now, at the last live bar's close.
    pub equity: f64,
}

/// The open position, marked at the last live bar's close.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PositionInfo {
    /// Signed quantity in the base asset: negative is short.
    pub qty: f64,
    pub avg_entry: f64,
    pub mark_price: f64,
    pub unrealised_pnl: f64,
    pub realised_pnl: f64,
    pub fees_paid: f64,
    /// Net funding paid; negative when the position received funding.
    pub funding_paid: f64,
}

/// Everything about one session: `GET /sessions/{id}`, and the first message of a `session:`
/// subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SessionState {
    pub id: String,
    pub config: PaperConfig,
    pub status: Status,
    pub cash: f64,
    /// Equity now, at the last live bar's close.
    pub equity: f64,
    pub position: PositionInfo,
    /// Equity at each live bar's close.
    pub equity_curve: Vec<EquityPoint>,
    pub open_orders: Vec<PaperOrder>,
    pub fills: Vec<PaperFill>,
    /// Round trips so far; one still open is marked at the last close.
    pub trades: Vec<PaperTrade>,
    /// Funding rates charged.
    pub funding: Vec<Funding>,
    /// Open time of the first live bar, after warm-up.
    pub live_from: Option<Millis>,
    pub last_bar: Option<Candle>,
    /// Buckets the live feed left out because too many of their minutes were missing.
    pub dropped: Vec<Millis>,
    /// The latest problems that did not stop the session.
    pub warnings: Vec<String>,
    /// The strategy's code, plan and rule when the session started and the verdict it was started
    /// from, with whether the strategy as it is now still matches that verdict or, without one,
    /// what the session started with (`stale`). Left out for a manual session and for one started
    /// before links were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub link: Option<LinkState>,
}

impl SessionState {
    /// The state of a paper session, as the API shows it, its link checked against `strategies`.
    pub fn of(paper: &PaperSession, strategies: &[StrategyEntry]) -> SessionState {
        let portfolio = paper.portfolio();
        SessionState {
            id: paper.id().to_string(),
            config: paper.config().clone(),
            status: paper.status().clone(),
            cash: portfolio.cash(),
            equity: portfolio.equity(),
            position: PositionInfo {
                qty: portfolio.position(),
                avg_entry: portfolio.avg_entry(),
                mark_price: portfolio.mark_price(),
                unrealised_pnl: portfolio.unrealised_pnl(),
                realised_pnl: portfolio.realised_pnl(),
                fees_paid: portfolio.fees_paid(),
                funding_paid: portfolio.funding_paid(),
            },
            equity_curve: paper.equity().to_vec(),
            open_orders: paper.open_orders_with_source(),
            fills: paper.fills_with_source(),
            trades: paper.trades(),
            funding: paper.funding().to_vec(),
            live_from: paper.live_from(),
            last_bar: paper.last_bar().copied(),
            dropped: paper.dropped().to_vec(),
            warnings: paper.warnings().to_vec(),
            link: paper.link().map(|link| {
                let config = paper.config();
                LinkState::of(link, &config.strategy, &config.instrument.id, strategies)
            }),
        }
    }
}

/// `GET /sessions/{id}/comparison`: a session beside a backtest of the same strategy, parameters,
/// instrument, timeframe and money settings over the same bars.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SessionComparisonResponse {
    /// The backtest that was run: from the session's first live bar (`from`) to the end of its
    /// last bar (`to`), warmed up from its first warm-up bar (`warmup_from`).
    pub backtest: BacktestConfig,
    pub bars: ComparedBars,
    /// Manual orders placed on the session. The backtest has none, so they, their fills and what
    /// they changed show as differences.
    pub manual_orders: usize,
    pub comparison: SessionComparison,
}

/// The bars the backtest ran on: the session's own, as its log holds them, warm-up bars included.
/// Nothing is downloaded and nothing is read from the store, so both sides saw exactly the same
/// prices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ComparedBars {
    pub warm_up_bars: usize,
    pub live_bars: usize,
    /// Buckets the session's live feed left out; the backtest lacks them too.
    pub dropped: Vec<Millis>,
    /// The funding rates the session was charged. The backtest is given each at the venue's
    /// funding time (`due`), as a backtest from stored data would be, so a rate the session was
    /// charged on a later bar, because it was not yet published when its own bar closed, lands
    /// on another bar there; one due before the first live bar is not charged there at all.
    pub funding_events: usize,
    /// How many of those rates the session charged on a later bar than the one they were due in.
    /// Each moves funding, and the equity after it, to another bar in the backtest, so it shows as
    /// a funding and a result difference and, for a strategy that sizes its orders on equity, as
    /// order and fill quantity differences: none of these is then a difference in how orders were
    /// filled.
    pub late_funding: usize,
}

fn summary(paper: &PaperSession) -> SessionSummary {
    let config = paper.config();
    SessionSummary {
        id: paper.id().to_string(),
        strategy: config.strategy.clone(),
        instrument: config.instrument.id.clone(),
        timeframe: config.timeframe,
        status: paper.status().clone(),
        equity: paper.portfolio().equity(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A session's lock is poisoned only if its run loop panicked mid-bar; the loop has ended and
    // its status says so, and the state is still read for display.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running session's supervisor: it stops the session when told to, or notices when it ends on
/// its own.
struct Run {
    /// Tells a supervisor apart from a later one of the same session.
    generation: u64,
    stop: oneshot::Sender<()>,
    supervisor: JoinHandle<()>,
    /// Turns true once the session has ended, before the supervisor clears the run.
    ended: watch::Receiver<bool>,
}

impl Run {
    /// The session has ended, on its own or by a stop; the supervisor may not have noticed yet.
    fn is_over(&self) -> bool {
        *self.ended.borrow()
    }
}

struct Entry {
    session: Arc<Mutex<PaperSession>>,
    /// The channel of the session's current run, or of its last one. A resume replaces it.
    events: broadcast::Sender<SessionEvent>,
    run: Option<Run>,
}

/// Every paper session in the data directory.
pub(crate) struct Sessions {
    data_dir: PathBuf,
    streams: Arc<Streams>,
    /// The server's strategies, the list `GET /verdicts` reads, that each session's link is checked
    /// against.
    strategies: Arc<Vec<StrategyEntry>>,
    entries: Mutex<BTreeMap<String, Entry>>,
    /// Starts, stops and resumes go one at a time, so two cannot race on one session.
    ops: tokio::sync::Mutex<Ops>,
    generations: AtomicU64,
}

#[derive(Default)]
struct Ops {
    /// Set at shutdown: no session starts after it.
    closed: bool,
}

fn session_not_found(id: &str) -> ApiError {
    ApiError::not_found(format!("no paper session {id:?}"))
}

fn not_running(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "not_running",
        format!("paper session {id:?} is not running"),
    )
}

impl Sessions {
    /// Opens every session saved under `<data_dir>/paper`, each stopped. Blocks: it replays each
    /// session's log.
    ///
    /// shortcut: a session whose files do not read is left out with a line on stderr, and every
    /// log is replayed at start; list broken sessions and replay lazily if either starts to matter.
    pub(crate) fn load(
        data_dir: &Path,
        streams: Arc<Streams>,
        strategies: Arc<Vec<StrategyEntry>>,
    ) -> Sessions {
        let mut entries = BTreeMap::new();
        let folder = data_dir.join("paper");
        let names: Vec<String> = std::fs::read_dir(&folder)
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        for name in names {
            match PaperSession::open(data_dir, &name) {
                Ok(paper) => {
                    entries.insert(
                        name,
                        Entry {
                            session: Arc::new(Mutex::new(paper)),
                            events: broadcast::channel(EVENT_BUFFER).0,
                            run: None,
                        },
                    );
                }
                Err(e) => eprintln!("td-server: left out paper session {name:?}: {e}"),
            }
        }
        Sessions {
            data_dir: data_dir.to_path_buf(),
            streams,
            strategies,
            entries: Mutex::new(entries),
            ops: tokio::sync::Mutex::new(Ops::default()),
            generations: AtomicU64::new(0),
        }
    }

    fn entries(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        lock(&self.entries)
    }

    pub(crate) fn list(&self) -> Vec<SessionSummary> {
        self.entries()
            .values()
            .map(|e| summary(&lock(&e.session)))
            .collect()
    }

    pub(crate) fn state(&self, id: &str) -> Option<SessionState> {
        self.entries()
            .get(id)
            .map(|e| SessionState::of(&lock(&e.session), &self.strategies))
    }

    fn summary_of(&self, id: &str) -> Result<SessionSummary, ApiError> {
        self.entries()
            .get(id)
            .map(|e| summary(&lock(&e.session)))
            .ok_or_else(|| session_not_found(id))
    }

    /// Subscribes to a session's events and takes its state under the session's lock, so every
    /// change after the state arrives as an event, exactly once.
    pub(crate) fn subscribe(
        &self,
        id: &str,
    ) -> Option<(broadcast::Receiver<SessionEvent>, SessionState)> {
        let entries = self.entries();
        let entry = entries.get(id)?;
        let paper = lock(&entry.session);
        Some((
            entry.events.subscribe(),
            SessionState::of(&paper, &self.strategies),
        ))
    }

    /// The sender of a session's current channel, to put events on it in tests.
    #[cfg(test)]
    pub(crate) fn events(&self, id: &str) -> Option<broadcast::Sender<SessionEvent>> {
        self.entries().get(id).map(|e| e.events.clone())
    }

    /// Creates a session with a new id and starts it, saving `link` with its config.
    pub(crate) async fn start(
        self: &Arc<Self>,
        config: PaperConfig,
        link: Option<SessionLink>,
    ) -> Result<SessionSummary, ApiError> {
        let ops = self.ops.lock().await;
        refuse_if_closed(&ops)?;
        let id = new_id()?;
        let (dir, new) = (self.data_dir.clone(), id.clone());
        let paper = tokio::task::spawn_blocking(move || {
            PaperSession::create_linked(&dir, &new, config, link)
        })
        .await
        .map_err(|e| ApiError::internal(format!("creating the session stopped: {e}")))??;
        let events = broadcast::channel(EVENT_BUFFER).0;
        {
            // Launched under the lock, so a session that ends at once finds its entry.
            let mut entries = self.entries();
            let (session, run) = self.launch(&id, paper, events.clone());
            entries.insert(
                id.clone(),
                Entry {
                    session,
                    events,
                    run: Some(run),
                },
            );
        }
        drop(ops);
        self.summary_of(&id)
    }

    /// Stops a running session and waits for it to end.
    pub(crate) async fn stop(&self, id: &str) -> Result<SessionSummary, ApiError> {
        let _ops = self.ops.lock().await;
        let run = {
            let mut entries = self.entries();
            let entry = entries.get_mut(id).ok_or_else(|| session_not_found(id))?;
            entry.run.take().ok_or_else(|| not_running(id))?
        };
        let _ = run.stop.send(());
        let _ = run.supervisor.await;
        self.summary_of(id)
    }

    /// Replays a stopped session's log and starts it again; it backfills the bars it missed. A
    /// session that ended on its own counts as stopped, even before its supervisor has noticed. A
    /// session made by another engine revision is refused.
    pub(crate) async fn resume(self: &Arc<Self>, id: &str) -> Result<SessionSummary, ApiError> {
        let ops = self.ops.lock().await;
        refuse_if_closed(&ops)?;
        match self.entries().get(id) {
            None => return Err(session_not_found(id)),
            Some(entry) if entry.run.as_ref().is_some_and(|run| !run.is_over()) => {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "already_running",
                    format!("paper session {id:?} is already running"),
                ));
            }
            Some(entry) => {
                // The barrier: the old session's lock, taken after the run is seen to be over, so
                // a manual order being written as it ended is in the log before it is read (see
                // `Sessions::manual`). Keep this lock even if the check below moves.
                let barrier = lock(&entry.session);
                barrier.check_resumable()?;
            }
        }
        let (dir, name) = (self.data_dir.clone(), id.to_string());
        let paper = tokio::task::spawn_blocking(move || PaperSession::open(&dir, &name))
            .await
            .map_err(|e| ApiError::internal(format!("opening the session stopped: {e}")))??;
        {
            let mut entries = self.entries();
            let entry = entries.get_mut(id).ok_or_else(|| session_not_found(id))?;
            // A new channel: once the old run's session is gone too, the old channel closes and its
            // subscribers take the resumed session's state (see `ws::forward_session`).
            entry.events = broadcast::channel(EVENT_BUFFER).0;
            let (session, run) = self.launch(id, paper, entry.events.clone());
            // The finished run's supervisor, if it is still winding down, no longer matches this
            // generation and leaves the new run alone.
            entry.session = session;
            entry.run = Some(run);
        }
        drop(ops);
        self.summary_of(id)
    }

    /// Places a manual order on a running session, or cancels one, with `act`. Like the bar feed,
    /// it writes the session's log under the session's lock only, so the write's sync does not
    /// hold up the list. A resume still cannot reopen the log between the check that the session
    /// runs and the write: the check is made again under the session's lock, and a resume, once
    /// it sees the run is over, takes that lock before it reads the log (see [`Sessions::resume`]).
    /// When the log cannot be written, the session is stopped.
    fn manual(
        &self,
        id: &str,
        act: impl FnOnce(&mut PaperSession) -> Result<PaperOrder, ManualError>,
    ) -> Result<PaperOrder, ApiError> {
        let (session, ended, generation) = {
            let entries = self.entries();
            let entry = entries.get(id).ok_or_else(|| session_not_found(id))?;
            match &entry.run {
                Some(run) if !run.is_over() => {
                    (entry.session.clone(), run.ended.clone(), run.generation)
                }
                _ => return Err(not_running(id)),
            }
        };
        let mut paper = lock(&session);
        if *ended.borrow() {
            return Err(not_running(id));
        }
        let done = act(&mut paper);
        if let Err(ManualError::Paper(e)) = &done {
            paper.warn(format!("stopped: {e}"));
            drop(paper);
            // Its supervisor stops it and lets the run go; it is not waited for here.
            let mut entries = self.entries();
            if let Some(entry) = entries.get_mut(id)
                && entry
                    .run
                    .as_ref()
                    .is_some_and(|r| r.generation == generation)
                && let Some(run) = entry.run.take()
            {
                let _ = run.stop.send(());
            }
        }
        done.map_err(ApiError::from)
    }

    /// [`Sessions::manual`] on a blocking thread: the order's line is synced to disk under the
    /// session's lock, which should hold up no async thread. A panic is passed on as if `act` had
    /// run inline.
    async fn manual_off_runtime(
        self: &Arc<Self>,
        id: &str,
        act: impl FnOnce(&mut PaperSession) -> Result<PaperOrder, ManualError> + Send + 'static,
    ) -> Result<PaperOrder, ApiError> {
        let (sessions, id) = (Arc::clone(self), id.to_string());
        match tokio::task::spawn_blocking(move || sessions.manual(&id, act)).await {
            Ok(done) => done,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => Err(ApiError::internal(format!("the order stopped: {e}"))),
        }
    }

    /// Places a manual order on a running session, accepted at the time the session's live
    /// source gives as the order is applied.
    pub(crate) async fn place(
        self: &Arc<Self>,
        id: &str,
        order: ManualOrder,
    ) -> Result<PaperOrder, ApiError> {
        // shortcut: the acceptance time is the local clock, assumed in step with the venue's.
        // A clock behind the venue by s seconds lets an order placed within s seconds after a bar
        // opened fill at that bar's open. Taking the newest in-progress kline time from the
        // stream as a floor would close it.
        // The same clock decides whether the session has caught up (see `PaperSession::place`):
        // an order is refused while the feed is more than one bar behind it. Within that bound
        // the strategy can still see a pending manual order at the close of one bar that closed
        // before the order was accepted but was delivered after it, a feed delayed by up to one
        // bar; its checks on that bar (cash, holding, leverage) count the order. It cannot fill
        // there.
        let rest = self.streams.live().rest();
        self.manual_off_runtime(id, move |paper| paper.place(&order, rest.now()))
            .await
    }

    /// Cancels a manual order on a running session.
    pub(crate) async fn cancel(
        self: &Arc<Self>,
        id: &str,
        order: OrderId,
    ) -> Result<PaperOrder, ApiError> {
        self.manual_off_runtime(id, move |paper| paper.cancel(order))
            .await
    }

    /// Compares a session with a backtest of the same strategy over the bars it has been fed so
    /// far (see [`SessionComparisonResponse`]). Blocks: it runs the backtest. The session's lock is
    /// held only while its records are copied. 409 `nothing_to_compare` for a manual session, which
    /// has no strategy to backtest, and for one with no live bar yet.
    pub(crate) fn comparison(&self, id: &str) -> Result<SessionComparisonResponse, ApiError> {
        let nothing = |why: &str| {
            ApiError::new(
                StatusCode::CONFLICT,
                "nothing_to_compare",
                format!("paper session {id:?} {why}"),
            )
        };
        let session = self
            .entries()
            .get(id)
            .map(|e| e.session.clone())
            .ok_or_else(|| session_not_found(id))?;
        let (
            config,
            bars,
            live_from,
            funding,
            orders,
            fills,
            equity,
            funding_paid,
            manual,
            dropped,
        ) = {
            let paper = lock(&session);
            let config = paper.config().clone();
            if config.strategy == MANUAL_STRATEGY {
                return Err(nothing(
                    "is a manual session: there is no strategy to backtest",
                ));
            }
            let Some(live_from) = paper.live_from() else {
                return Err(nothing("has not fed a live bar yet"));
            };
            let s = paper.session();
            let manual = s
                .orders()
                .iter()
                .filter(|o| paper.source(o.id) == OrderSource::Manual)
                .count();
            (
                config,
                s.bars().to_vec(),
                live_from,
                paper.funding().to_vec(),
                s.orders().to_vec(),
                s.fills().to_vec(),
                s.equity().to_vec(),
                s.portfolio().funding_paid(),
                manual,
                paper.dropped().to_vec(),
            )
        };
        let (first, last) = match (bars.first(), bars.last()) {
            (Some(first), Some(last)) => (first.open_time, last.open_time),
            _ => return Err(nothing("has not fed a live bar yet")),
        };
        let backtest = BacktestConfig {
            strategy: config.strategy.clone(),
            params: config.params.clone(),
            instrument: config.instrument.clone(),
            timeframe: config.timeframe,
            from: live_from,
            to: last + config.timeframe.duration_ms(),
            starting_cash: config.starting_cash,
            slippage_bps: config.slippage_bps,
            fee_override: config.fees,
            max_leverage: config.max_leverage,
            warmup_from: (first < live_from).then_some(first),
            close_at_end: false,
            // Paper sessions are on a quote-currency account (see `paper::build_session`).
            account_currency: AccountCurrency::Quote,
        };
        let due: Vec<(Millis, f64)> = funding.iter().map(|f| (f.due, f.rate)).collect();
        let result = td_engine::run(&backtest, &bars, &[], &due).map_err(|e| {
            ApiError::internal(format!("the backtest beside the session failed: {e}"))
        })?;
        let recorded = Recorded {
            orders: &orders,
            fills: &fills,
            equity: &equity,
            starting_cash: config.starting_cash,
            funding_paid,
        };
        let comparison = compare_with_backtest(recorded, Recorded::of_backtest(&result));
        let warm_up_bars = bars.iter().filter(|b| b.open_time < live_from).count();
        let bar_of = |t| config.timeframe.floor(t);
        let late_funding = funding
            .iter()
            .filter(|f| bar_of(f.due) < bar_of(f.time))
            .count();
        Ok(SessionComparisonResponse {
            backtest,
            bars: ComparedBars {
                warm_up_bars,
                live_bars: bars.len() - warm_up_bars,
                dropped,
                funding_events: funding.len(),
                late_funding,
            },
            manual_orders: manual,
            comparison,
        })
    }

    /// Stops every running session, for shutdown; none starts after this.
    pub(crate) async fn stop_all(&self) {
        let mut ops = self.ops.lock().await;
        ops.closed = true;
        let runs: Vec<Run> = self
            .entries()
            .values_mut()
            .filter_map(|e| e.run.take())
            .collect();
        let mut supervisors = Vec::with_capacity(runs.len());
        for run in runs {
            let _ = run.stop.send(());
            supervisors.push(run.supervisor);
        }
        for supervisor in supervisors {
            let _ = supervisor.await;
        }
    }

    /// Starts `paper` on the shared streams, with a supervisor that clears the entry's run when
    /// the session ends on its own.
    fn launch(
        self: &Arc<Self>,
        id: &str,
        mut paper: PaperSession,
        events: broadcast::Sender<SessionEvent>,
    ) -> (Arc<Mutex<PaperSession>>, Run) {
        paper.set_events(events);
        let instrument = paper.config().instrument.id.clone();
        let streams = self.streams.clone();
        let rest = streams.live().rest();
        let handle = crate::start_on(paper, rest, move || streams.stream(&instrument));
        let session = handle.session().clone();
        let ended = handle.ended();
        let generation = self.generations.fetch_add(1, Ordering::SeqCst);
        let (stop, stopped) = oneshot::channel::<()>();
        let (sessions, id) = (Arc::clone(self), id.to_string());
        let supervisor = tokio::spawn(async move {
            tokio::select! {
                _ = stopped => {}
                () = handle.finished() => {}
            }
            handle.stop().await;
            let mut entries = sessions.entries();
            if let Some(entry) = entries.get_mut(&id)
                && entry
                    .run
                    .as_ref()
                    .is_some_and(|r| r.generation == generation)
            {
                entry.run = None;
            }
        });
        let run = Run {
            generation,
            stop,
            supervisor,
            ended,
        };
        (session, run)
    }
}

fn refuse_if_closed(ops: &Ops) -> Result<(), ApiError> {
    if ops.closed {
        Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "the server is shutting down",
        ))
    } else {
        Ok(())
    }
}

/// A new session id: the time in milliseconds, so ids sort in the order sessions were made, and 32
/// random bits.
fn new_id() -> Result<String, ApiError> {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes)
        .map_err(|e| ApiError::internal(format!("cannot draw a session id: {e}")))?;
    Ok(format!("{}-{:08x}", now_ms(), u32::from_be_bytes(bytes)))
}

impl From<PaperError> for ApiError {
    fn from(e: PaperError) -> Self {
        let message = e.to_string();
        match e {
            // At creation these are the strategy, its parameters and the money settings.
            PaperError::Engine(_) | PaperError::InvalidId(_) => ApiError::bad_request(message),
            PaperError::Data(e) => e.into(),
            PaperError::Exists(_) => ApiError::new(StatusCode::CONFLICT, "conflict", message),
            PaperError::EngineChanged { .. } => {
                ApiError::new(StatusCode::CONFLICT, "engine_changed", message)
            }
            PaperError::Io { .. }
            | PaperError::BadFile { .. }
            | PaperError::FeedEnded
            | PaperError::Task(_)
            | PaperError::LogFailed(_) => ApiError::internal(message),
        }
    }
}

impl From<ManualError> for ApiError {
    fn from(e: ManualError) -> Self {
        let message = e.to_string();
        match e {
            ManualError::NotLive => ApiError::new(StatusCode::CONFLICT, "not_live", message),
            ManualError::CatchingUp => ApiError::new(StatusCode::CONFLICT, "catching_up", message),
            ManualError::Rejected(_) => {
                ApiError::new(StatusCode::BAD_REQUEST, "invalid_order", message)
            }
            ManualError::NoSuchOrder(_) => ApiError::not_found(message),
            ManualError::NotManual(_) => ApiError::new(StatusCode::CONFLICT, "not_manual", message),
            ManualError::Done { .. } => ApiError::new(StatusCode::CONFLICT, "order_done", message),
            // Only a log that could not be written.
            ManualError::Paper(e) => ApiError::internal(e.to_string()),
        }
    }
}

/// `POST /sessions`: 201 with the new session, already warming up.
pub(super) async fn start(
    State(state): State<Arc<AppState>>,
    body: Result<Json<StartSessionRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<SessionSummary>), ApiError> {
    let Json(request) = body?;
    let id = &request.instrument;
    // The paper path feeds bars with no asks, and no live feed gives them, so a quoted market
    // would fail on its first bar.
    // shortcut: refused outright; lift this when a live FX feed gives the asks.
    if id.kind.rules().quotes {
        return Err(ApiError::bad_request(format!(
            "{id} is quoted as a bid and an ask, and there is no live ask feed yet, so it cannot \
             be paper traded"
        )));
    }
    let list = data::instrument_list(&state, id.venue, id.kind).await?;
    let instrument = list
        .instruments
        .into_iter()
        .find(|i| i.id == *id)
        .ok_or_else(|| {
            ApiError::bad_request(format!("{id} is not an instrument the venue lists"))
        })?;
    let config = PaperConfig {
        strategy: request.strategy,
        params: request.params,
        instrument,
        timeframe: request.timeframe,
        starting_cash: request.starting_cash,
        slippage_bps: request.slippage_bps,
        fees: request.fees,
        // shortcut: a session started over the API trades at 1x, since the request has no
        // leverage yet; add the cap to the request when the apps get a leverage setting.
        max_leverage: 1.0,
    };
    let link = if config.strategy == MANUAL_STRATEGY {
        if request.verdict_run_id.is_some() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_verdict",
                "a manual session runs no strategy, so it is not started from a verdict",
            ));
        }
        None
    } else {
        // An unknown strategy has no link; creating the session refuses it.
        match state
            .strategies
            .iter()
            .find(|e| e.name == config.strategy)
            .cloned()
        {
            None => None,
            Some(entry) => {
                let (runs_dir, instrument) = (state.runs_dir(), config.instrument.id.clone());
                let verdict = request.verdict_run_id;
                let link = tokio::task::spawn_blocking(move || {
                    session_link(&runs_dir, &entry, &instrument, verdict.as_ref())
                })
                .await
                .map_err(|e| ApiError::internal(format!("reading the verdict stopped: {e}")))??;
                Some(link)
            }
        }
    };
    let created = state.sessions.start(config, link).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /sessions/{id}/comparison`.
pub(super) async fn comparison(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<SessionComparisonResponse>, ApiError> {
    let sessions = state.sessions.clone();
    let compared = tokio::task::spawn_blocking(move || sessions.comparison(&id))
        .await
        .map_err(|e| ApiError::internal(format!("the comparison stopped: {e}")))??;
    Ok(Json(compared))
}

/// `GET /sessions`, oldest first.
pub(super) async fn list(State(state): State<Arc<AppState>>) -> Json<Vec<SessionSummary>> {
    Json(state.sessions.list())
}

/// `GET /sessions/{id}`.
pub(super) async fn get(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<SessionState>, ApiError> {
    state
        .sessions
        .state(&id)
        .map(Json)
        .ok_or_else(|| session_not_found(&id))
}

/// `POST /sessions/{id}/stop`: 200 once it has stopped, 409 if it was not running.
pub(super) async fn stop(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<SessionSummary>, ApiError> {
    Ok(Json(state.sessions.stop(&id).await?))
}

/// `POST /sessions/{id}/resume`: 200 once it is running again, 409 if it already was or if it was
/// made by another engine revision.
pub(super) async fn resume(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<SessionSummary>, ApiError> {
    Ok(Json(state.sessions.resume(&id).await?))
}

/// `POST /sessions/{id}/orders`: 201 with the order, open and marked manual.
pub(super) async fn place_order(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    body: Result<Json<ManualOrder>, JsonRejection>,
) -> Result<(StatusCode, Json<PaperOrder>), ApiError> {
    let Json(order) = body?;
    let placed = state.sessions.place(&id, order).await?;
    Ok((StatusCode::CREATED, Json(placed)))
}

/// `POST /sessions/{id}/orders/{order_id}/cancel`: 200 with the cancelled order.
pub(super) async fn cancel_order(
    State(state): State<Arc<AppState>>,
    path: Result<UrlPath<(String, u64)>, PathRejection>,
) -> Result<Json<PaperOrder>, ApiError> {
    let UrlPath((id, order)) =
        path.map_err(|_| ApiError::not_found("order ids are whole numbers"))?;
    Ok(Json(state.sessions.cancel(&id, OrderId(order)).await?))
}

#[cfg(test)]
pub(crate) mod tests {
    use td_core::{Instrument, InstrumentKind, Venue};
    use td_data::DataError;
    use td_data::live::Rest;
    use td_engine::ParamValue;

    use super::*;
    use crate::api::source::LiveSource;

    /// No venue: REST has nothing and the stream URL refuses connections, so a session started on
    /// it warms up on nothing and keeps reconnecting.
    struct Nowhere;

    impl Rest for Nowhere {
        fn klines(
            &self,
            _: &InstrumentId,
            _: Timeframe,
            _: Millis,
            _: Millis,
        ) -> Result<Vec<Candle>, DataError> {
            Ok(Vec::new())
        }

        fn funding(
            &self,
            _: &InstrumentId,
            _: Millis,
            _: Millis,
        ) -> Result<Vec<(Millis, f64)>, DataError> {
            Ok(Vec::new())
        }

        fn now(&self) -> Millis {
            1_704_067_200_000
        }
    }

    impl LiveSource for Nowhere {
        fn rest(&self) -> Arc<dyn Rest> {
            Arc::new(Nowhere)
        }

        fn stream_url(&self, _: &InstrumentId) -> String {
            // Port 9 on loopback: nothing listens there, so the connection is refused at once.
            "ws://127.0.0.1:9/".into()
        }
    }

    /// The sessions of a temporary data directory holding one stopped session, `s`, on no venue.
    pub(crate) fn one_session() -> (tempfile::TempDir, Arc<Sessions>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = PaperConfig {
            strategy: "sma_cross".into(),
            params: Params::new()
                .with("fast", ParamValue::Int(2))
                .with("slow", ParamValue::Int(5)),
            instrument: Instrument {
                id: InstrumentId {
                    venue: Venue::Binance,
                    symbol: "TESTUSDT".into(),
                    kind: InstrumentKind::Spot,
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
        };
        PaperSession::create(dir.path(), "s", config).expect("creates");
        let streams = Arc::new(Streams::new(Arc::new(Nowhere)));
        let sessions = Arc::new(Sessions::load(
            dir.path(),
            streams,
            Arc::new(td_engine::strategies()),
        ));
        (dir, sessions)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_that_ended_resumes_before_its_supervisor_has_noticed() {
        let (_dir, sessions) = one_session();
        // A run that has ended, with a supervisor that has not got round to clearing it.
        let (stop, stopped) = oneshot::channel();
        let stuck = tokio::spawn(async move {
            let _ = stopped.await;
        });
        let (_ended_tx, ended) = watch::channel(true);
        sessions.entries().get_mut("s").expect("loaded").run = Some(Run {
            generation: u64::MAX,
            stop,
            supervisor: stuck,
            ended,
        });

        let resumed = sessions.resume("s").await.expect("resumes");
        assert_eq!(resumed.id, "s");
        // A run that has not ended still refuses a resume.
        let (_running_tx, running) = watch::channel(false);
        if let Some(run) = sessions.entries().get_mut("s").and_then(|e| e.run.as_mut()) {
            run.ended = running;
        }
        let refused = sessions.resume("s").await.expect_err("already running");
        assert_eq!(refused.body.code, "already_running");
        sessions.stop_all().await;
    }

    /// A manual order's line is synced off the async thread: on this single-threaded runtime, a
    /// sync made inline would run on the test's own thread, where it is set to fail.
    #[tokio::test]
    async fn a_manual_order_is_synced_off_the_async_thread() {
        let (_dir, sessions) = one_session();
        let now = Nowhere.now();
        {
            let entries = sessions.entries();
            let mut paper = lock(&entries["s"].session);
            let bar = Candle {
                open_time: now - 60_000,
                open: 100.0,
                high: 100.0,
                low: 100.0,
                close: 100.0,
                volume: 1.0,
                quote_volume: 100.0,
            };
            paper.feed(bar, Vec::new(), false).expect("a live bar");
        }
        // A run that is going, so the session takes orders.
        let (stop, stopped) = oneshot::channel();
        let supervisor = tokio::spawn(async move {
            let _ = stopped.await;
        });
        let (_running_tx, running) = watch::channel(false);
        sessions.entries().get_mut("s").expect("loaded").run = Some(Run {
            generation: u64::MAX,
            stop,
            supervisor,
            ended: running,
        });
        let order = ManualOrder {
            side: td_core::Side::Buy,
            qty: 1.0,
            order_type: crate::ManualOrderType::Limit,
            limit_price: Some(50.0),
            reduce_only: None,
        };

        crate::paper::FAIL_SYNC.set(true);
        let placed = sessions.place("s", order).await;
        let cancelled = match &placed {
            Ok(order) => Some(sessions.cancel("s", order.order.id).await),
            Err(_) => None,
        };
        crate::paper::FAIL_SYNC.set(false);
        let placed = placed.expect("placed, synced on a blocking thread");
        cancelled
            .expect("placed")
            .expect("cancelled, synced on a blocking thread");
        assert_eq!(placed.source, crate::OrderSource::Manual);
        sessions.stop_all().await;
    }

    /// A bar of a wave that crosses its moving averages every few bars.
    fn wave(i: i64) -> Candle {
        let price = |i: i64| 100.0 + 10.0 * (i as f64 * std::f64::consts::TAU / 16.0).sin();
        let (open, close) = (price(i - 1), price(i));
        Candle {
            open_time: i * 60_000,
            open,
            high: open.max(close) + 0.5,
            low: open.min(close) - 0.5,
            close,
            volume: 1.0,
            quote_volume: close,
        }
    }

    /// Writes a perpetual sma_cross session `id` fed 5 warm-up bars and 60 live ones, with a
    /// funding rate every 8 bars, `late` bars after its due time.
    fn fed_session(dir: &Path, id: &str, late: i64) {
        let config = PaperConfig {
            params: Params::new()
                .with("fast", ParamValue::Int(2))
                .with("slow", ParamValue::Int(5))
                .with("allow_short", ParamValue::Bool(true)),
            ..crate::paper::tests::config()
        };
        let mut paper = PaperSession::create(dir, id, config).expect("creates");
        for i in 0..65 {
            let funding = if i >= 5 && i % 8 == 0 {
                vec![Funding {
                    time: i * 60_000,
                    rate: 0.001,
                    due: (i - late) * 60_000,
                }]
            } else {
                Vec::new()
            };
            paper.feed(wave(i), funding, i < 5).expect("feeds");
        }
    }

    fn loaded(dir: &Path) -> Arc<Sessions> {
        Arc::new(Sessions::load(
            dir,
            Arc::new(Streams::new(Arc::new(Nowhere))),
            Arc::new(td_engine::strategies()),
        ))
    }

    #[test]
    fn a_paper_session_matches_a_backtest_on_its_own_bars() {
        let dir = tempfile::tempdir().expect("temp dir");
        fed_session(dir.path(), "on-time", 0);
        let compared = loaded(dir.path()).comparison("on-time").expect("compares");
        let c = &compared.comparison;
        assert_eq!(c.differences, [], "{:#?}", c.differences);
        assert!(c.fills.len() >= 4, "the strategy trades: {}", c.fills.len());
        assert!(c.session.funding != 0.0, "funding was charged");
        assert_eq!(c.pnl_ratio, Some(1.0));
        let bars = &compared.bars;
        assert_eq!((bars.warm_up_bars, bars.live_bars), (5, 60));
        assert_eq!((bars.funding_events, bars.late_funding), (8, 0));
        let backtest = &compared.backtest;
        assert_eq!(backtest.warmup_from, Some(0));
        assert_eq!((backtest.from, backtest.to), (5 * 60_000, 65 * 60_000));
        assert_eq!(compared.manual_orders, 0);
    }

    #[test]
    fn funding_charged_late_in_the_session_is_a_named_difference() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Each rate reaches the session one bar after it was due, as when the venue had not yet
        // published it when its bar closed; the backtest charges it on the bar it was due in.
        fed_session(dir.path(), "late", 1);
        let compared = loaded(dir.path()).comparison("late").expect("compares");
        let c = &compared.comparison;
        let kinds: Vec<_> = c.differences.iter().map(|d| d.kind).collect();
        assert!(
            kinds.contains(&td_engine::DifferenceKind::Funding),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&td_engine::DifferenceKind::Result),
            "{kinds:?}"
        );
        assert!(c.session_only_fills.is_empty() && c.backtest_only_fills.is_empty());
        let bars = &compared.bars;
        assert_eq!((bars.funding_events, bars.late_funding), (8, 8));
    }

    #[test]
    fn a_manual_order_shows_in_the_comparison_and_a_manual_session_has_none() {
        let (dir, _) = one_session();
        {
            let mut paper = PaperSession::open(dir.path(), "s").expect("opens");
            paper.feed(wave(0), Vec::new(), false).expect("feeds");
            let order = ManualOrder {
                side: td_core::Side::Buy,
                qty: 1.0,
                order_type: crate::ManualOrderType::Market,
                limit_price: None,
                reduce_only: None,
            };
            paper.place(&order, 60_000).expect("placed");
            paper.feed(wave(1), Vec::new(), false).expect("feeds");
        }
        let manual = PaperConfig {
            strategy: MANUAL_STRATEGY.into(),
            params: Params::new(),
            ..crate::paper::tests::config()
        };
        PaperSession::create(dir.path(), "by-hand", manual).expect("creates");
        PaperSession::create(dir.path(), "fresh", crate::paper::tests::config()).expect("creates");
        let sessions = loaded(dir.path());

        let compared = sessions.comparison("s").expect("compares");
        assert_eq!(compared.manual_orders, 1);
        let kinds: Vec<_> = compared
            .comparison
            .differences
            .iter()
            .map(|d| d.kind)
            .collect();
        assert!(
            kinds.contains(&td_engine::DifferenceKind::SessionOnlyOrder)
                && kinds.contains(&td_engine::DifferenceKind::SessionOnlyFill),
            "{kinds:?}"
        );
        for id in ["by-hand", "fresh"] {
            let refused = sessions.comparison(id).expect_err("nothing to compare");
            assert_eq!(refused.body.code, "nothing_to_compare", "{id}");
        }
    }

    #[test]
    fn a_session_without_a_link_serialises_as_before() {
        let (_dir, sessions) = one_session();
        let state = serde_json::to_value(sessions.state("s").expect("loaded")).expect("json");
        assert!(state.get("link").is_none(), "{state}");
    }

    #[test]
    fn new_ids_are_valid_session_ids_that_sort_by_time() {
        let a = new_id().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_id().unwrap();
        assert!(a < b, "{a} then {b}");
        assert!(a.len() <= 64 && a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
