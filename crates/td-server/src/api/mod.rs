//! The localhost API the desktop apps, tests and scripts use: HTTP and one WebSocket, bound to
//! 127.0.0.1, every request guarded by the per-launch token (see [`auth`]).
//!
//! Routes (JSON in and out; errors are `{"error": {"code", "message"}}` with a matching status):
//!
//! - `GET /health`: the server version.
//! - `GET /instruments?venue=&kind=`: the instruments a venue lists, cached per server run.
//! - `POST /data/download` `{instrument, from, to}`: starts a download job, answers `{job_id}`.
//!   `from` may not be before 2017-01-01 UTC ([`data::EARLIEST_FROM`]); an instrument that already
//!   has an unfinished download in this server is a 409 `conflict` (a duplicate from another
//!   server on the same data folder is safe, since the store locks across processes, just
//!   redundant).
//! - `GET /data/coverage?instrument=venue:symbol:kind`: which months and minutes the store holds.
//! - `GET /strategies`: the registered strategies and their parameter specs.
//! - `GET /jobs`, `GET /jobs/{id}`, `POST /jobs/{id}/cancel`: background jobs (see [`jobs`]).
//! - Paper sessions (see [`sessions`]):
//!   - `POST /sessions` `{strategy, params, instrument, timeframe, starting_cash, slippage_bps,
//!     fees?}`: creates a session and starts it, 201 with its summary. `instrument` is an
//!     instrument id; its trading rules come from `GET /instruments`.
//!   - `GET /sessions`: every session, oldest first, with its status and equity.
//!   - `GET /sessions/{id}`: a session's full state: status, position, cash, equity curve, open
//!     orders, fills and trades so far.
//!   - `POST /sessions/{id}/stop`: 200 once stopped, 409 `not_running` if it was not running.
//!   - `POST /sessions/{id}/resume`: replays a stopped session and starts it again, backfilling
//!     what it missed; 409 `already_running` if it is running.
//!
//!   An unknown session id is a 404.
//! - `GET /ws`: topic subscriptions: `jobs`, `candles:...` and `session:<id>` (see [`ws`]).
//!
//! On start the binary prints one line to stdout, `{"port":<n>,"token":"<hex>"}`
//! ([`Handshake`]), and logs only to stderr.

pub mod auth;
pub mod data;
pub mod error;
pub mod jobs;
mod live;
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
use td_engine::ParamSpec;
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
    /// Turns true when the server is stopping; WebSocket connections close on it.
    shutdown: watch::Receiver<bool>,
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
    /// starts serving in the background.
    pub async fn start(config: ApiConfig) -> io::Result<Server> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.port)).await?;
        let addr = listener.local_addr()?;
        let (shutdown, shutdown_rx) = watch::channel(false);
        let jobs = Arc::new(Jobs::default());
        let streams = Arc::new(Streams::new(config.live));
        let (data_dir, loading) = (config.store.root().to_path_buf(), streams.clone());
        let sessions = tokio::task::spawn_blocking(move || Sessions::load(&data_dir, loading))
            .await
            .map(Arc::new)
            .map_err(|e| io::Error::other(format!("loading the paper sessions stopped: {e}")))?;
        let state = Arc::new(AppState {
            token: config.token.clone(),
            port: addr.port(),
            store: config.store,
            source: config.source,
            jobs: jobs.clone(),
            sessions: sessions.clone(),
            streams,
            instruments: Mutex::new(HashMap::new()),
            shutdown: shutdown_rx.clone(),
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
        .route("/strategies", get(strategies))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/cancel", post(cancel_job))
        .route("/sessions", get(sessions::list).post(sessions::start))
        .route("/sessions/{id}", get(sessions::get))
        .route("/sessions/{id}/stop", post(sessions::stop))
        .route("/sessions/{id}/resume", post(sessions::resume))
        .route("/ws", get(ws::upgrade))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(state.clone(), auth::guard))
        .with_state(state)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct HealthResponse {
    pub version: String,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// A strategy the apps can run, with the parameters it takes.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct StrategyInfo {
    pub name: String,
    pub description: String,
    pub params: Vec<ParamSpec>,
}

async fn strategies() -> Json<Vec<StrategyInfo>> {
    Json(
        td_engine::strategies()
            .into_iter()
            .map(|e| StrategyInfo {
                name: e.name.to_string(),
                description: e.description.to_string(),
                params: e.params,
            })
            .collect(),
    )
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
        let state = Arc::new(AppState {
            token: token.clone(),
            port,
            store: Store::new(&unused),
            source: Arc::new(NoSource),
            jobs: Arc::new(Jobs::default()),
            sessions: Arc::new(Sessions::load(&unused, streams.clone())),
            streams,
            instruments: Mutex::new(HashMap::new()),
            shutdown: watch::channel(false).1,
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_peer_that_is_not_loopback_is_403_even_with_the_token() {
        let (status, body) = status_from_peer(SocketAddr::from(([192, 168, 1, 20], 40000))).await;
        assert_eq!(status, 403);
        assert!(body.contains("forbidden_peer"), "{body}");
        let (status, _) = status_from_peer(SocketAddr::from(([127, 0, 0, 1], 40000))).await;
        assert_eq!(status, 200);
    }
}
