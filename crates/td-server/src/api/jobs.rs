//! Background jobs: anything that takes longer than a request. A job is queued, runs on a blocking
//! thread once a slot is free, reports progress, and ends done (with a result), failed (with an
//! error) or cancelled. Every change is published to the `jobs` WebSocket topic.
//!
//! Downloads are the only kind so far. A backtest, sweep or walk-forward run plugs in by adding a
//! variant to [`JobKind`], [`JobRequest`] and [`JobResult`] and a route that calls [`Jobs::submit`]
//! with its work.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use td_core::{InstrumentId, Millis};
use td_data::time::now_ms;
use tokio::sync::{Semaphore, broadcast};
use ts_rs::TS;

use super::data::{DownloadRequest, DownloadResult};
use super::error::{ApiError, ErrorBody};

/// How many jobs run at once; the rest wait as `queued`.
// shortcut: one shared limit for every kind, sized for downloads (venue rate limits); when
// backtests and sweeps land (#13, #14), give CPU-bound kinds their own pool sized to the cores.
const MAX_RUNNING: usize = 2;

/// Events buffered per WebSocket subscriber before it is told to resync.
const EVENT_BUFFER: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Download,
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
}

impl JobRequest {
    pub fn kind(&self) -> JobKind {
        match self {
            JobRequest::Download(_) => JobKind::Download,
        }
    }

    /// The instrument whose store files this job writes, if it downloads.
    pub fn downloads(&self) -> Option<&InstrumentId> {
        match self {
            JobRequest::Download(d) => Some(&d.instrument),
        }
    }
}

/// What a finished job produced, tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobResult {
    Download(DownloadResult),
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
    slots: Arc<Semaphore>,
}

impl Default for Jobs {
    fn default() -> Self {
        Jobs {
            inner: Mutex::new(Inner::default()),
            events: broadcast::channel(EVENT_BUFFER).0,
            slots: Arc::new(Semaphore::new(MAX_RUNNING)),
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
    /// 409 `conflict`: two jobs would fetch the same data twice and race on the same files.
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

        let jobs = self.clone();
        let id = info.id;
        tokio::spawn(async move {
            // The semaphore is never closed, so acquiring only waits.
            let Ok(_permit) = jobs.slots.clone().acquire_owned().await else {
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
        let mut inner = self.lock();
        if let Some(entry) = inner.jobs.get_mut(&id) {
            change(&mut entry.info);
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
        let busy: Vec<u64> = (0..MAX_RUNNING)
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
        assert_eq!(ran.load(Ordering::SeqCst), MAX_RUNNING);
        assert_eq!(
            ran_queued.load(Ordering::SeqCst),
            0,
            "the queued job never ran"
        );
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
