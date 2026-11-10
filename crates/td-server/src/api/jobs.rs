//! Background jobs: anything that takes longer than a request. A job is queued, runs on a blocking
//! thread once a slot is free, reports progress, and ends done (with a result), failed (with an
//! error) or cancelled. Every change is published to the `jobs` WebSocket topic.
//!
//! Kinds: downloads ([`super::data`]) and runs: backtests, sweeps, out-of-sample splits and
//! walk-forwards ([`super::runs`]). A new kind plugs in by adding a variant to [`JobKind`],
//! [`JobRequest`] and [`JobResult`] and a route that calls [`Jobs::submit`] with its work.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use td_core::{InstrumentId, Millis};
use td_data::time::now_ms;
use tokio::sync::{Semaphore, broadcast};
use ts_rs::TS;

use td_engine::{BacktestConfig, OosSpec, SweepSpec, WalkForwardSpec};

use super::data::{DownloadRequest, DownloadResult};
use super::error::{ApiError, ErrorBody};
use super::runs::RunDone;

/// How many downloads run at once, for the venues' rate limits; the rest wait as `queued`.
const MAX_DOWNLOADS: usize = 2;

/// How many runs (backtests and tester runs) go at once, apart from downloads, so a long sweep
/// never holds up a download. A sweep already spreads over every core through rayon; two at once
/// share them.
const MAX_RUNS: usize = 2;

/// Events buffered per WebSocket subscriber before it is told to resync.
const EVENT_BUFFER: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Download,
    Backtest,
    Sweep,
    Oos,
    WalkForward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            JobStatus::Done | JobStatus::Failed | JobStatus::Cancelled
        )
    }
}

/// What a job was asked to do, tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobRequest {
    Download(DownloadRequest),
    Backtest(BacktestConfig),
    Sweep(SweepSpec),
    Oos(OosSpec),
    WalkForward(WalkForwardSpec),
}

impl JobRequest {
    pub fn kind(&self) -> JobKind {
        match self {
            JobRequest::Download(_) => JobKind::Download,
            JobRequest::Backtest(_) => JobKind::Backtest,
            JobRequest::Sweep(_) => JobKind::Sweep,
            JobRequest::Oos(_) => JobKind::Oos,
            JobRequest::WalkForward(_) => JobKind::WalkForward,
        }
    }

    /// The instrument whose store files this job writes, if it downloads.
    pub fn downloads(&self) -> Option<&InstrumentId> {
        match self {
            JobRequest::Download(d) => Some(&d.instrument),
            _ => None,
        }
    }
}

/// What a finished job produced, tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobResult {
    Download(DownloadResult),
    Backtest(RunDone),
    Sweep(RunDone),
    Oos(RunDone),
    WalkForward(RunDone),
}

impl JobResult {
    /// The saved run, for a run job.
    pub fn run(&self) -> Option<&RunDone> {
        match self {
            JobResult::Download(_) => None,
            JobResult::Backtest(r)
            | JobResult::Sweep(r)
            | JobResult::Oos(r)
            | JobResult::WalkForward(r) => Some(r),
        }
    }
}

/// A job as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct JobInfo {
    pub id: u64,
    pub kind: JobKind,
    pub status: JobStatus,
    /// From 0 to 1.
    pub progress: f64,
    /// The latest progress line, for people.
    pub message: String,
    pub request: JobRequest,
    /// Set when `status` is `done`.
    pub result: Option<JobResult>,
    /// Set when `status` is `failed`.
    pub error: Option<ErrorBody>,
    /// True once a cancel was asked for; a running job stops at its next checkpoint.
    pub cancel_requested: bool,
    pub created_at: Millis,
    pub started_at: Option<Millis>,
    pub finished_at: Option<Millis>,
}

/// Returned when a job is accepted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct JobCreated {
    pub job_id: u64,
}

/// How a job's work ended other than with a result.
#[derive(Debug, Clone, PartialEq)]
pub enum JobError {
    Cancelled,
    Failed(ErrorBody),
}

impl From<ApiError> for JobError {
    fn from(e: ApiError) -> Self {
        JobError::Failed(e.body)
    }
}

impl From<td_data::DataError> for JobError {
    fn from(e: td_data::DataError) -> Self {
        ApiError::from(e).into()
    }
}

/// What a job's work sees: where to report progress and whether to stop.
pub struct JobCtx {
    id: u64,
    jobs: Arc<Jobs>,
    cancel: Arc<AtomicBool>,
}

impl JobCtx {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Reports progress: `fraction` from 0 to 1 and a line for people.
    pub fn progress(&self, fraction: f64, message: impl Into<String>) {
        let message = message.into();
        self.jobs.update(self.id, |job| {
            if fraction.is_finite() {
                job.progress = fraction.clamp(0.0, 1.0);
            }
            job.message = message;
        });
    }

    /// Like [`JobCtx::progress`], but ignored when `fraction` is below the progress already shown,
    /// so reports from several threads that arrive out of order never move it backwards.
    pub fn advance(&self, fraction: f64, message: impl FnOnce() -> String) {
        self.jobs.update_if(self.id, |job| {
            if !(fraction.is_finite() && fraction >= job.progress) {
                return false;
            }
            job.progress = fraction.clamp(0.0, 1.0);
            job.message = message();
            true
        });
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// `Err(JobError::Cancelled)` once a cancel was asked for; call it between steps.
    pub fn checkpoint(&self) -> Result<(), JobError> {
        if self.is_cancelled() {
            Err(JobError::Cancelled)
        } else {
            Ok(())
        }
    }
}

struct Entry {
    info: JobInfo,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    jobs: BTreeMap<u64, Entry>,
}

/// The job registry.
// shortcut: jobs stay in memory for the server's lifetime and are lost when it exits; persist
// them (with their results) once runs are long enough that losing one on restart hurts.
pub struct Jobs {
    inner: Mutex<Inner>,
    events: broadcast::Sender<JobInfo>,
    download_slots: Arc<Semaphore>,
    run_slots: Arc<Semaphore>,
}

impl Default for Jobs {
    fn default() -> Self {
        Jobs {
            inner: Mutex::new(Inner::default()),
            events: broadcast::channel(EVENT_BUFFER).0,
            download_slots: Arc::new(Semaphore::new(MAX_DOWNLOADS)),
            run_slots: Arc::new(Semaphore::new(MAX_RUNS)),
        }
    }
}

impl Jobs {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves plain data behind, still consistent enough to read.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues `work` and returns the new job. `work` runs on a blocking thread, so it may block on
    /// I/O or compute for as long as it needs; it should call [`JobCtx::checkpoint`] between steps.
    /// Must be called from inside the Tokio runtime.
    ///
    /// A download of an instrument that already has an unfinished download job is refused with
    /// 409 `conflict`: two jobs would fetch the same data twice. This is a courtesy within one
    /// server process only; the store's per-file locks hold across processes, so a duplicate
    /// download started by another server on the same data folder (the other app's sidecar) is
    /// safe, just redundant.
    pub fn submit<F>(self: &Arc<Self>, request: JobRequest, work: F) -> Result<JobInfo, ApiError>
    where
        F: FnOnce(&JobCtx) -> Result<JobResult, JobError> + Send + 'static,
    {
        let cancel = Arc::new(AtomicBool::new(false));
        let info = {
            let mut inner = self.lock();
            if let Some(wanted) = request.downloads()
                && let Some(running) = inner.jobs.values().find(|e| {
                    !e.info.status.is_finished() && e.info.request.downloads() == Some(wanted)
                })
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "conflict",
                    format!(
                        "{wanted} already has a download in progress (job {}); wait for it or \
                         cancel it",
                        running.info.id
                    ),
                ));
            }
            inner.next_id += 1;
            let info = JobInfo {
                id: inner.next_id,
                kind: request.kind(),
                status: JobStatus::Queued,
                progress: 0.0,
                message: "queued".into(),
                request,
                result: None,
                error: None,
                cancel_requested: false,
                created_at: now_ms(),
                started_at: None,
                finished_at: None,
            };
            inner.jobs.insert(
                info.id,
                Entry {
                    info: info.clone(),
                    cancel: cancel.clone(),
                },
            );
            let _ = self.events.send(info.clone());
            info
        };

        let slots = match info.kind {
            JobKind::Download => self.download_slots.clone(),
            _ => self.run_slots.clone(),
        };
        let jobs = self.clone();
        let id = info.id;
        tokio::spawn(async move {
            // The semaphore is never closed, so acquiring only waits.
            let Ok(_permit) = slots.acquire_owned().await else {
                return;
            };
            if !jobs.start(id) {
                return; // cancelled while queued
            }
            let ctx = JobCtx {
                id,
                jobs: jobs.clone(),
                cancel,
            };
            let outcome = tokio::task::spawn_blocking(move || work(&ctx)).await;
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(e) => Err(JobError::Failed(
                    ApiError::internal(format!("the job stopped unexpectedly: {e}")).body,
                )),
            };
            jobs.finish(id, outcome);
        });
        Ok(info)
    }

    pub fn list(&self) -> Vec<JobInfo> {
        self.lock().jobs.values().map(|e| e.info.clone()).collect()
    }

    pub fn get(&self, id: u64) -> Option<JobInfo> {
        self.lock().jobs.get(&id).map(|e| e.info.clone())
    }

    /// Asks a job to stop. A queued job is cancelled at once; a running one stops at its next
    /// checkpoint. Cancelling a finished job is a 409.
    pub fn cancel(&self, id: u64) -> Result<JobInfo, ApiError> {
        let info = {
            let mut inner = self.lock();
            let entry = inner
                .jobs
                .get_mut(&id)
                .ok_or_else(|| ApiError::not_found(format!("no job {id}")))?;
            if entry.info.status.is_finished() {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "job_finished",
                    format!("job {id} has already finished"),
                ));
            }
            entry.cancel.store(true, Ordering::SeqCst);
            entry.info.cancel_requested = true;
            if entry.info.status == JobStatus::Queued {
                entry.info.status = JobStatus::Cancelled;
                entry.info.message = "cancelled".into();
                entry.info.finished_at = Some(now_ms());
            }
            let _ = self.events.send(entry.info.clone());
            entry.info.clone()
        };
        Ok(info)
    }

    /// Asks every unfinished job to stop, for shutdown.
    pub fn cancel_all(&self) {
        let ids: Vec<u64> = self
            .lock()
            .jobs
            .values()
            .filter(|e| !e.info.status.is_finished())
            .map(|e| e.info.id)
            .collect();
        for id in ids {
            let _ = self.cancel(id);
        }
    }

    /// Every change to every job, as the job's new state.
    pub fn subscribe(&self) -> broadcast::Receiver<JobInfo> {
        self.events.subscribe()
    }

    /// Moves a queued job to running; false if it was cancelled while it waited.
    fn start(&self, id: u64) -> bool {
        let mut started = false;
        self.update(id, |job| {
            if job.status == JobStatus::Queued {
                job.status = JobStatus::Running;
                job.message = "running".into();
                job.started_at = Some(now_ms());
                started = true;
            }
        });
        started
    }

    fn finish(&self, id: u64, outcome: Result<JobResult, JobError>) {
        self.update(id, |job| {
            job.finished_at = Some(now_ms());
            match outcome {
                Ok(result) => {
                    job.status = JobStatus::Done;
                    job.progress = 1.0;
                    job.message = "done".into();
                    job.result = Some(result);
                }
                Err(JobError::Cancelled) => {
                    job.status = JobStatus::Cancelled;
                    job.message = "cancelled".into();
                }
                Err(JobError::Failed(error)) => {
                    job.status = JobStatus::Failed;
                    job.message = error.message.clone();
                    job.error = Some(error);
                }
            }
        });
    }

    /// Changes a job and publishes its new state. Events are sent under the lock, so subscribers
    /// see a job's states in the order they happened.
    fn update(&self, id: u64, change: impl FnOnce(&mut JobInfo)) {
        self.update_if(id, |job| {
            change(job);
            true
        });
    }

    /// [`Jobs::update`], publishing only when `change` returns true.
    fn update_if(&self, id: u64, change: impl FnOnce(&mut JobInfo) -> bool) {
        let mut inner = self.lock();
        if let Some(entry) = inner.jobs.get_mut(&id)
            && change(&mut entry.info)
        {
            let _ = self.events.send(entry.info.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use td_core::{InstrumentKind, Venue};

    /// A download request; each `symbol` is its own instrument, so jobs for different symbols do
    /// not conflict.
    fn request(symbol: &str) -> JobRequest {
        JobRequest::Download(DownloadRequest {
            instrument: InstrumentId {
                venue: Venue::Binance,
                symbol: symbol.into(),
                kind: InstrumentKind::Spot,
            },
            from: 0,
            to: 1,
        })
    }

    async fn wait_for(jobs: &Jobs, id: u64, status: JobStatus) -> JobInfo {
        for _ in 0..400 {
            if let Some(job) = jobs.get(id).filter(|j| j.status == status) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id} never reached {status:?}: {:?}", jobs.get(id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn jobs_beyond_the_limit_queue_and_a_queued_job_cancels_without_running() {
        let jobs = Arc::new(Jobs::default());
        let ran = Arc::new(AtomicUsize::new(0));
        // Fill every slot with a job that runs until cancelled.
        let busy: Vec<u64> = (0..MAX_DOWNLOADS)
            .map(|i| {
                let ran = ran.clone();
                jobs.submit(request(&format!("BUSY{i}")), move |ctx| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    loop {
                        ctx.checkpoint()?;
                        std::thread::sleep(Duration::from_millis(5));
                    }
                })
                .expect("accepted")
                .id
            })
            .collect();
        for &id in &busy {
            wait_for(&jobs, id, JobStatus::Running).await;
        }
        let ran_queued = Arc::new(AtomicUsize::new(0));
        let flag = ran_queued.clone();
        let queued = jobs
            .submit(request("QUEUED"), move |_| {
                flag.fetch_add(1, Ordering::SeqCst);
                Err(JobError::Cancelled)
            })
            .expect("accepted")
            .id;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(jobs.get(queued).unwrap().status, JobStatus::Queued);

        let cancelled = jobs.cancel(queued).unwrap();
        assert_eq!(cancelled.status, JobStatus::Cancelled);
        for &id in &busy {
            jobs.cancel(id).unwrap();
            wait_for(&jobs, id, JobStatus::Cancelled).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(ran.load(Ordering::SeqCst), MAX_DOWNLOADS);
        assert_eq!(
            ran_queued.load(Ordering::SeqCst),
            0,
            "the queued job never ran"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn progress_reported_out_of_order_never_moves_backwards() {
        let jobs = Arc::new(Jobs::default());
        let mut events = jobs.subscribe();
        let id = jobs
            .submit(request("ORDER"), |ctx| {
                ctx.advance(0.5, || "half".into());
                ctx.advance(0.4, || "late".into());
                ctx.advance(0.6, || "more".into());
                Err(JobError::Cancelled)
            })
            .expect("accepted")
            .id;
        wait_for(&jobs, id, JobStatus::Cancelled).await;
        let mut shown = Vec::new();
        while let Ok(job) = events.try_recv() {
            if job.status == JobStatus::Running && job.progress > 0.0 {
                shown.push((job.progress, job.message));
            }
        }
        assert_eq!(shown, [(0.5, "half".into()), (0.6, "more".into())]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failures_and_panics_end_as_failed_with_an_error() {
        let jobs = Arc::new(Jobs::default());
        let failed = jobs
            .submit(request("FAILS"), |_| {
                Err(ApiError::bad_request("nope").into())
            })
            .expect("accepted")
            .id;
        let job = wait_for(&jobs, failed, JobStatus::Failed).await;
        assert_eq!(job.error.map(|e| e.message), Some("nope".to_string()));
        let panicked = jobs
            .submit(request("PANICS"), |_| panic!("boom"))
            .expect("accepted")
            .id;
        let job = wait_for(&jobs, panicked, JobStatus::Failed).await;
        assert_eq!(job.error.map(|e| e.code), Some("internal".to_string()));
    }
}
