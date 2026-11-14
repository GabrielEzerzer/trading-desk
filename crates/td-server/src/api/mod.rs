//! The localhost API the desktop apps, tests and scripts use: HTTP and one WebSocket, bound to
//! 127.0.0.1, every request guarded by the per-launch token (see [`auth`]).
//!
//! Routes (JSON in and out; errors are `{"error": {"code", "message"}}` with a matching status):
//!
//! - `GET /health`: the server version, and the data cutoff when one is in force (`data_cutoff`,
//!   the date `TD_DATA_CUTOFF` set when the engine started; see [`runs::DataCutoff`]). With a
//!   cutoff, every run, sweep, walk-forward and test battery whose range ends after the end of
//!   that date is refused with a 422 `after_cutoff`, when it is requested and again when its job
//!   loads the data. Paper sessions and `GET /candles` are not affected. This route is where the
//!   API says a cutoff is in force; each saved run also records the cutoff it was made under
//!   (`data_cutoff` on `GET /runs/{id}`).
//! - `GET /instruments?venue=&kind=`: the instruments a venue lists, cached per server run.
//! - `POST /data/download` `{instrument, from, to}`: starts a download job, answers `{job_id}`.
//!   `from` may not be before 2017-01-01 UTC for binance and bybit ([`data::EARLIEST_FROM`]), or
//!   before 2007-01-01 UTC for oanda ([`data::OANDA_EARLIEST_FROM`]); an instrument that
//!   already has an unfinished download in this server is a 409 `conflict` (a duplicate from
//!   another server on the same data folder is safe, since the store locks across processes, just
//!   redundant).
//! - `GET /data/coverage?instrument=venue:symbol:kind`: which months and minutes the store holds,
//!   and each month file's bytes. A closure recorded at download time is closed time, neither a
//!   gap nor counted in `month_minutes`.
//! - `GET /data/stored`: the data folder's path, every instrument the store holds, with each month
//!   file's bytes and the funding file's, and any paths in the data folder the store did not write.
//! - `POST /data/delete` `{instrument, from?, to?}`: deletes the candles of the months `from` to
//!   `to` (`YYYY-MM`, both included), or with neither the whole instrument including a
//!   perpetual's funding file, and answers `{files, bytes}` removed. An instrument with a queued
//!   download in this server, or a running download or delete here or in another process on the
//!   same data folder, is a 409 `conflict`.
//! - `POST /data/delete-all`: deletes every stored instrument as `/data/delete` does, and answers
//!   `{files, bytes}` removed; saved runs and paper sessions stay. A 409 `conflict`, deleting
//!   nothing, while any stored instrument is held by a download or delete.
//! - `GET /candles?instrument=venue:symbol:kind&timeframe=&from=&to=&limit=`: closed bars of the
//!   timeframe with open times in `[from, to)`, oldest first, for a chart (see [`candles`]). Bars
//!   come from the store, resampled from its minutes; the slots it does not cover are fetched from
//!   the venue's REST API, as a paper session's warm-up does, and are not stored, so an instrument
//!   with no stored data works too. Oanda has no live stream and so no REST bars: there the slots
//!   the store does not cover are reported missing. A bar still open is never returned. `to`
//!   defaults to now and `from` to the venue's earliest download date (2017-01-01, or 2007-01-01
//!   for oanda), which is also the earliest bar served; either one between bar boundaries is
//!   rounded up to the next. `limit` defaults to and is capped at [`candles::MAX_CANDLES`]; a range
//!   holding more bar slots than `limit` is cut to the latest `limit` slots ending at `to`, so the
//!   chart gets the latest history (`truncated` says so when `from` was given). The response
//!   names the spans served by the venue (`from_venue`) and the slots neither had (`missing`). A
//!   malformed instrument, an unknown venue, kind or timeframe, a `limit` of 0 or `from >= to` is
//!   a 400 `bad_request`, and so is the venue refusing the request with a 4xx (other than its
//!   rate limit), as Binance does for a symbol it does not list, with the venue's message. Any
//!   other venue failure while filling in is a 502 `venue` with its message.
//! - `GET /strategies`: the registered strategies, examples first and then the private ones when
//!   their folder is checked out, each with its parameter specs, its rule's kind, its test plan
//!   and its verdict rule.
//! - `GET /jobs`, `GET /jobs/{id}`, `POST /jobs/{id}/cancel`: background jobs (see [`jobs`]).
//! - `POST /runs/backtest` (a `BacktestConfig`), `POST /runs/sweep` (a `SweepSpec`),
//!   `POST /runs/out-of-sample` (an `OosSpec`), `POST /runs/walk-forward` (a `WalkForwardSpec`):
//!   start a run job, answer `{job_id}`. An out-of-sample run's job kind is `oos`, the same as its
//!   saved run's kind; only the route spells it out. `from`, `to` and `warmup_from` must each be
//!   where a bar of the timeframe opens. The store lacks part of the range when it is missing a
//!   day or more of open time in one stretch, counted across the weekends inside it, apart from
//!   closures recorded at download time; for FX in a month downloaded before closures were
//!   recorded, two days. The job then downloads that part first, reporting
//!   `downloading the data: ...`, and runs once the store holds it; a failed download fails the
//!   job with the source's error, and one that does not fill the range fails it with a
//!   `data_missing` naming the missing spans. A range nothing can be downloaded for (after now,
//!   or before the venue's earliest data) is a 422 `data_missing` when it is requested. Shorter
//!   gaps are run and listed in the result's data report (`gaps`, `dropped`), with the recorded
//!   closures in its `closures`. A range that ends after the data cutoff is a 422 `after_cutoff`
//!   (see `GET /health`) when it is requested, before anything is downloaded. A bad spec is a 400
//!   (`invalid_config`, which includes an unknown strategy and range edges off a bar boundary, or
//!   `invalid_spec`). At most
//!   [`jobs::MAX_RUN_JOBS`] run jobs are queued or running at once; one more is a 429
//!   `too_many_runs`. A queued run holds only its request and loads its data when it gets one of
//!   the run slots, downloading then what the store lacks. Each finished run is saved (see
//!   [`runs`]).
//! - `GET /jobs/{id}/result`: the saved run of a finished run job (`RunDetail`); 409
//!   `job_not_done` while it is queued or running (ask again later), 409 `no_result` once it has
//!   finished without a saved run (failed, cancelled, or not a run job).
//! - `GET /runs`: the saved runs, newest first, one summary row each. `GET /runs/{id}`: one saved
//!   run with its data report (`data_error` says why when that report is damaged), a check of
//!   whether its data and engine still match, the hash of the strategy's source file it was made
//!   with and whether the file is still the same (`code_hash`, `code_matches`, `null` when
//!   unknown) and the data cutoff it was made under (`data_cutoff`).
//! - `POST /runs/plan` `{strategy, instruments?}` (a `PlanRequest`): runs the strategy's test
//!   battery on each instrument of its plan, or on the given ones, as one job of kind `plan`, and
//!   answers `{job_id}` (see [`plans`]). 400 `invalid_config` for an unknown strategy or an
//!   instrument the venue does not list, `invalid_plan` for a plan or rule that fails its checks,
//!   `instrument_not_in_plan` for an instrument the plan does not test, `invalid_request` for an
//!   empty or repeated list; 422 `data_missing`, naming the instrument and the span, when the store
//!   lacks part of the plan's period (nothing is downloaded); 422 `after_cutoff` when the period
//!   ends after the data cutoff; 429 `too_many_runs` as for the other
//!   runs. The job's result (`PlanDone`) holds one verdict and saved run id per instrument; a
//!   cancelled plan job keeps the instruments it finished, and the one it stopped during the luck
//!   test, in its result; a failed one keeps those it saved before the error. Each instrument's run
//!   (kind `plan`, a `PlanRun`) is at `GET /runs/{id}`.
//! - `GET /verdicts?strategy=`: the latest verdict of each strategy on each instrument
//!   (`VerdictSummary`; the newest complete one, so a luck test stopped by a cancel does not
//!   replace a finished battery), with how many batteries that pair has saved, the run id, its time
//!   and the engine revision, whether that revision is this engine's (`engine_matches`), whether
//!   the strategy's plan and rule are still the ones it ran under (`plan_matches`), whether the
//!   strategy's source file is still the one it ran with (`code_matches`, `null` when unknown,
//!   as for a battery saved before code hashes were recorded), `stale` when any of those three is
//!   not true, and whether its luck test finished (`complete`). Each row also lists the limits of
//!   the rule it was judged under that are looser than its kind's defaults (`loosened`), and how
//!   many distinct versions of the strategy's code have been through the battery on that
//!   instrument (`code_versions`). The run and version counts are keyed by strategy name, so
//!   renaming a strategy starts both again from zero. Only the given strategy's with `strategy`.
//! - Paper sessions (see [`sessions`]):
//!   - `POST /sessions` `{strategy, params, instrument, timeframe, starting_cash, slippage_bps,
//!     fees?, verdict_run_id?}`: creates a session and starts it, 201 with its summary.
//!     `instrument` is an instrument id; its trading rules come from `GET /instruments`. Strategy
//!     `none`, with no parameters, makes a manual session: no strategy runs and only manual
//!     orders trade. A strategy session is linked to its code hash, plan and rule, and to the
//!     verdict `verdict_run_id` names, or else the latest verdict for its strategy and instrument;
//!     400 `invalid_verdict` if the named verdict is missing, is for another strategy or
//!     instrument, or the session is manual.
//!   - `GET /sessions`: every session, oldest first, with its status and equity.
//!   - `GET /sessions/{id}`: a session's full state: status, position, cash, equity curve, open
//!     orders, fills and trades so far, and its `link` when it has one, with `stale` set when its
//!     code, plan or engine no longer matches the linked verdict or, without a verdict, when its
//!     code or plan has changed since the session started.
//!   - `GET /sessions/{id}/comparison`: a backtest of the session's strategy and parameters on
//!     the session's own logged bars, compared with what the session recorded: fills paired one
//!     to one, the fills and orders only one side has, fees, funding and result per side, the
//!     ratio of the session's P&L to the backtest's, and each difference named. 409
//!     `nothing_to_compare` for a manual session or one with no live bar yet.
//!   - `POST /sessions/{id}/stop`: 200 once stopped, 409 `not_running` if it was not running.
//!   - `POST /sessions/{id}/resume`: replays a stopped session and starts it again, backfilling
//!     what it missed; 409 `already_running` if it is running. A session that ended on its own
//!     with an error counts as stopped. Its `session:` subscribers get the resumed state. 409
//!     `engine_changed` if the session was created by another engine revision: it still lists
//!     and shows, replayed by this engine with a warning saying so, but cannot go on; start a new
//!     session instead.
//!   - `POST /sessions/{id}/orders` `{side, qty, type: "market"|"limit", limit_price?,
//!     reduce_only?}`: places a manual order (a `ManualOrder`), on a strategy session too. It goes
//!     through the same rounding, checks (lot, tick and minimums, the spot cash and holding, the
//!     leverage cap) and broker as a strategy's order, checked at the close of the last bar fed.
//!     It takes effect from the first bar that opens at or after the moment it is accepted (by
//!     the local clock, assumed in step with the venue), so it
//!     never fills on a price from before then: a market order fills at that bar's open, and a
//!     limit is first checked against that bar. The bar in progress when it is placed cannot fill
//!     it, nor can the bars a backfill feeds after it; one placed exactly as a bar closes takes
//!     effect from the next bar, like a strategy's. Its `submitted_at` is the open of the bar
//!     before the one it takes effect from (at the earliest, the last bar fed), and it can be
//!     cancelled before it takes effect. 201 with the order (a `PaperOrder`,
//!     `source: "manual"`). 400 `invalid_order` with the engine's reason when it is refused, 4xx
//!     `bad_request` for a body that does not read, 409 `not_running` on a session that is not
//!     running, 409 `not_live` before its first live bar, 409 `catching_up` while the session is
//!     still replaying bars it missed: its last bar fed opens more than one bar before the last
//!     bar closed by the same clock, as during a resume's backfill or behind a lagging feed. The
//!     strategy's checks at each bar's close count pending orders, so this keeps an order from
//!     changing its decisions on bars from before the order, apart from at most one bar that
//!     closed before it but was delivered after it. Its fills and trades are marked manual too,
//!     and the order is in the session's log, so a resume replays it.
//!   - `POST /sessions/{id}/orders/{order_id}/cancel`: cancels an open manual order, 200 with the
//!     order, while catching up too. 404 for an unknown order, 409 `not_manual` for one the
//!     strategy placed, 409 `order_done` for one already filled or cancelled, 409 `not_running`
//!     as above.
//!
//!   An unknown session id is a 404.
//! - `GET /ws`: topic subscriptions: `jobs`, `candles:...` and `session:<id>` (see [`ws`]).
//!
//! On start the binary prints one line to stdout, `{"port":<n>,"token":"<hex>"}`
//! ([`Handshake`]), and logs only to stderr.

pub mod auth;
pub mod candles;
pub mod data;
pub mod error;
pub mod jobs;
mod live;
pub mod plans;
pub mod runs;
pub mod sessions;
pub mod source;
pub mod ts;
pub mod ws;

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde::{Deserialize, Serialize};
use td_core::{InstrumentKind, Venue};
use td_data::Store;
use td_engine::{ParamSpec, RuleKind, StrategyEntry, TestPlan, VerdictRule};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use ts_rs::TS;

use self::auth::Token;
use self::data::InstrumentsResponse;
use self::error::ApiError;
use self::jobs::{JobInfo, Jobs};
use self::live::Streams;
use self::sessions::Sessions;
use self::source::{LiveSource, MarketSource};

/// How long the binary waits on shutdown, first for open requests to finish and paper sessions to
/// stop, and then for running jobs to reach a checkpoint, before it exits anyway.
pub const JOB_GRACE: Duration = Duration::from_secs(5);

/// What a server needs to start.
pub struct ApiConfig {
    /// The candle store; the binary uses td-data's data directory, tests a temporary one.
    pub store: Store,
    pub source: Arc<dyn MarketSource>,
    /// Live candles for paper sessions and the `candles:` topic.
    pub live: Arc<dyn LiveSource>,
    pub token: Token,
    /// The port on 127.0.0.1; 0 lets the operating system choose.
    pub port: u16,
    /// The date after which runs read no bars (`TD_DATA_CUTOFF`); `None` for no limit.
    pub data_cutoff: Option<runs::DataCutoff>,
}

type InstrumentCache = HashMap<(Venue, InstrumentKind), InstrumentsResponse>;

/// Shared by every handler.
pub(crate) struct AppState {
    token: Token,
    port: u16,
    store: Store,
    source: Arc<dyn MarketSource>,
    jobs: Arc<Jobs>,
    sessions: Arc<Sessions>,
    streams: Arc<Streams>,
    instruments: Mutex<InstrumentCache>,
    /// The strategies `GET /strategies` lists and `POST /runs/plan` runs plans of.
    strategies: Arc<Vec<StrategyEntry>>,
    /// Turns true when the server is stopping; WebSocket connections close on it.
    shutdown: watch::Receiver<bool>,
    /// The data cutoff every run is checked against.
    data_cutoff: Option<runs::DataCutoff>,
}

impl AppState {
    fn instrument_cache(&self) -> MutexGuard<'_, InstrumentCache> {
        self.instruments.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The line the binary prints to stdout once it listens, for the app that started it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Handshake {
    pub port: u16,
    pub token: String,
}

/// A running server.
pub struct Server {
    addr: SocketAddr,
    token: Token,
    jobs: Arc<Jobs>,
    sessions: Arc<Sessions>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<io::Result<()>>,
}

impl Server {
    /// Binds 127.0.0.1 on the configured port, loads the saved paper sessions (stopped), and
    /// starts serving in the background, with every registered strategy.
    pub async fn start(config: ApiConfig) -> io::Result<Server> {
        Self::start_with_strategies(config, td_engine::strategies()).await
    }

    /// [`Server::start`] with `strategies` in place of the registered ones, for tests that run a
    /// plan smaller than a registered strategy's. Each entry's name must still be a registered
    /// strategy: the engine builds strategies by name.
    pub async fn start_with_strategies(
        config: ApiConfig,
        strategies: Vec<StrategyEntry>,
    ) -> io::Result<Server> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.port)).await?;
        let addr = listener.local_addr()?;
        let (shutdown, shutdown_rx) = watch::channel(false);
        let jobs = Arc::new(Jobs::default());
        let streams = Arc::new(Streams::new(config.live));
        // The sessions check their links against the same list `GET /verdicts` reads.
        let strategies = Arc::new(strategies);
        let (data_dir, loading, checked) = (
            config.store.root().to_path_buf(),
            streams.clone(),
            strategies.clone(),
        );
        let sessions =
            tokio::task::spawn_blocking(move || Sessions::load(&data_dir, loading, checked))
                .await
                .map(Arc::new)
                .map_err(|e| {
                    io::Error::other(format!("loading the paper sessions stopped: {e}"))
                })?;
        let state = Arc::new(AppState {
            token: config.token.clone(),
            port: addr.port(),
            store: config.store,
            source: config.source,
            jobs: jobs.clone(),
            sessions: sessions.clone(),
            streams,
            instruments: Mutex::new(HashMap::new()),
            strategies,
            shutdown: shutdown_rx.clone(),
            data_cutoff: config.data_cutoff,
        });
        let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
        let mut stop = shutdown_rx;
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stop.wait_for(|stopping| *stopping).await;
                })
                .await
        });
        Ok(Server {
            addr,
            token: config.token,
            jobs,
            sessions,
            shutdown,
            task,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn token(&self) -> &Token {
        &self.token
    }

    /// The job registry the routes queue on, for code that queues jobs of its own beside them
    /// (the tests hold run slots with jobs whose end they control).
    pub fn jobs(&self) -> &Arc<Jobs> {
        &self.jobs
    }

    /// The handshake line, without a newline.
    pub fn handshake_line(&self) -> String {
        let handshake = Handshake {
            port: self.port(),
            token: self.token.as_str().to_string(),
        };
        serde_json::to_string(&handshake).expect("a struct of a number and a string serializes")
    }

    /// Stops accepting connections, closes WebSockets, asks running jobs to stop, stops every
    /// running paper session, and waits for open requests to finish. Jobs stop at their next
    /// checkpoint on their own threads. The wait has no limit of its own; the binary bounds it
    /// with [`JOB_GRACE`].
    pub async fn shutdown(self) -> io::Result<()> {
        let _ = self.shutdown.send(true);
        self.jobs.cancel_all();
        self.sessions.stop_all().await;
        self.task
            .await
            .map_err(|e| io::Error::other(format!("the server task stopped: {e}")))?
    }
}

/// Every route, behind the guard. The guard also covers the fallbacks, so an unknown path without
/// the token is a 401, not a 404.
pub(crate) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/instruments", get(data::instruments))
        .route("/data/download", post(data::download))
        .route("/data/coverage", get(data::coverage))
        .route("/data/stored", get(data::stored))
        .route("/data/delete", post(data::delete))
        .route("/data/delete-all", post(data::delete_all))
        .route("/candles", get(candles::candles))
        .route("/strategies", get(strategies))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/cancel", post(cancel_job))
        .route("/jobs/{id}/result", get(runs::job_result))
        .route("/runs", get(runs::list))
        .route("/runs/backtest", post(runs::backtest))
        .route("/runs/sweep", post(runs::sweep))
        .route("/runs/out-of-sample", post(runs::out_of_sample))
        .route("/runs/walk-forward", post(runs::walk_forward))
        .route("/runs/plan", post(plans::start))
        .route("/verdicts", get(plans::verdicts))
        .route("/runs/{id}", get(runs::get))
        .route("/sessions", get(sessions::list).post(sessions::start))
        .route("/sessions/{id}", get(sessions::get))
        .route("/sessions/{id}/stop", post(sessions::stop))
        .route("/sessions/{id}/resume", post(sessions::resume))
        .route("/sessions/{id}/comparison", get(sessions::comparison))
        .route("/sessions/{id}/orders", post(sessions::place_order))
        .route(
            "/sessions/{id}/orders/{order_id}/cancel",
            post(sessions::cancel_order),
        )
        .route("/ws", get(ws::upgrade))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(state.clone(), auth::guard))
        .with_state(state)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct HealthResponse {
    pub version: String,
    /// The data cutoff date (`TD_DATA_CUTOFF`, `YYYY-MM-DD` UTC) when one is in force: every run,
    /// sweep, walk-forward and test battery whose range ends after that date is refused. `null`
    /// when there is none.
    pub data_cutoff: Option<String>,
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        data_cutoff: state.data_cutoff.as_ref().map(|c| c.date().to_string()),
    })
}

/// A strategy the apps can run, with the parameters it takes, the plan the battery tests it on
/// and the rule its verdict is judged by.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct StrategyInfo {
    pub name: String,
    pub description: String,
    pub params: Vec<ParamSpec>,
    /// The rule's kind, for the Lab's picker; the same as `rule.kind`.
    pub kind: RuleKind,
    pub plan: TestPlan,
    pub rule: VerdictRule,
}

async fn strategies(State(state): State<Arc<AppState>>) -> Json<Vec<StrategyInfo>> {
    Json(strategy_infos(state.strategies.as_ref().clone()))
}

/// What `GET /strategies` returns for `entries`. Public only so tests/gui_fixtures.rs can check
/// the GUI's fake engine against it.
pub fn strategy_infos(entries: Vec<StrategyEntry>) -> Vec<StrategyInfo> {
    entries
        .into_iter()
        .map(|e| StrategyInfo {
            name: e.name.to_string(),
            description: e.description.to_string(),
            params: e.params,
            kind: e.rule.kind(),
            plan: e.plan,
            rule: e.rule,
        })
        .collect()
}

async fn list_jobs(State(state): State<Arc<AppState>>) -> Json<Vec<JobInfo>> {
    Json(state.jobs.list())
}

fn job_id(path: Result<Path<u64>, PathRejection>) -> Result<u64, ApiError> {
    path.map(|Path(id)| id)
        .map_err(|_| ApiError::not_found("job ids are whole numbers"))
}

async fn get_job(
    State(state): State<Arc<AppState>>,
    path: Result<Path<u64>, PathRejection>,
) -> Result<Json<JobInfo>, ApiError> {
    let id = job_id(path)?;
    state
        .jobs
        .get(id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no job {id}")))
}

/// Asks a job to stop: 202 with the job's state, 404 for an unknown job, 409 if it has finished.
async fn cancel_job(
    State(state): State<Arc<AppState>>,
    path: Result<Path<u64>, PathRejection>,
) -> Result<(StatusCode, Json<JobInfo>), ApiError> {
    let id = job_id(path)?;
    Ok((StatusCode::ACCEPTED, Json(state.jobs.cancel(id)?)))
}

async fn not_found() -> ApiError {
    ApiError::not_found("no such route")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "this route does not take that method",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::connect_info::MockConnectInfo;
    use td_core::{InstrumentId, Millis};
    use td_data::{DataError, DownloadReport, InstrumentList};

    struct NoSource;

    impl MarketSource for NoSource {
        fn instruments(&self, _: Venue, _: InstrumentKind) -> Result<InstrumentList, DataError> {
            Err(DataError::NoDataDir)
        }
        fn candles(
            &self,
            _: &Store,
            _: &InstrumentId,
            _: Millis,
            _: Millis,
        ) -> Result<DownloadReport, DataError> {
            Err(DataError::NoDataDir)
        }
        fn funding(
            &self,
            _: &Store,
            _: &InstrumentId,
            _: Millis,
            _: Millis,
        ) -> Result<usize, DataError> {
            Err(DataError::NoDataDir)
        }
    }

    /// Serves the real router over real HTTP, but with the peer address the guard sees replaced
    /// by `peer`, the way a connection from another machine would look.
    async fn status_from_peer(peer: SocketAddr) -> (u16, String) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = Token::generate().unwrap();
        let unused = std::env::temp_dir().join("td-server-unused");
        let streams = Arc::new(Streams::new(Arc::new(source::VenueLive)));
        let strategies = Arc::new(td_engine::strategies());
        let state = Arc::new(AppState {
            token: token.clone(),
            port,
            store: Store::new(&unused),
            source: Arc::new(NoSource),
            jobs: Arc::new(Jobs::default()),
            sessions: Arc::new(Sessions::load(&unused, streams.clone(), strategies.clone())),
            streams,
            instruments: Mutex::new(HashMap::new()),
            strategies,
            shutdown: watch::channel(false).1,
            data_cutoff: None,
        });
        let app = router(state).layer(MockConnectInfo(peer));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let response = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/health"))
            .header("authorization", format!("Bearer {}", token.as_str()))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.text().await.unwrap();
        server.abort();
        (status, body)
    }

    /// The listing maps every registered entry, so a private strategy added after the examples
    /// is listed with its kind, plan and rule like they are.
    #[test]
    fn every_registered_strategy_is_listed_with_its_kind_plan_and_rule() {
        let mut entries = td_engine::strategies();
        let mut private = entries[0].clone();
        private.name = "private_example";
        private.rule = VerdictRule::HighRisk {
            limits: td_engine::HighRiskLimits::default(),
        };
        entries.push(private.clone());
        let infos = strategy_infos(entries.clone());
        assert_eq!(infos.len(), entries.len());
        for (info, entry) in infos.iter().zip(&entries) {
            assert_eq!(info.name, entry.name);
            assert_eq!(info.kind, entry.rule.kind());
            assert_eq!(info.plan, entry.plan);
            assert_eq!(info.rule, entry.rule);
        }
        assert_eq!(infos.last().unwrap().kind, RuleKind::HighRisk);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_peer_that_is_not_loopback_is_403_even_with_the_token() {
        let (status, body) = status_from_peer(SocketAddr::from(([192, 168, 1, 20], 40000))).await;
        assert_eq!(status, 403);
        assert!(body.contains("forbidden_peer"), "{body}");
        let (status, _) = status_from_peer(SocketAddr::from(([127, 0, 0, 1], 40000))).await;
        assert_eq!(status, 200);
    }
}
