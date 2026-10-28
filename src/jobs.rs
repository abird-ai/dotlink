//! Durable shell-job state shared by synchronous shell calls, MCP Tasks and
//! dotlink compatibility tools.
//!
//! The manager owns the complete execution lifetime: admission, request
//! deduplication, runner ownership, status notification, bounded disk spooling,
//! paging leases, retention and teardown. Protocol adapters only decide how long
//! to wait for an already-started job and how to expose its stable id.

use std::{
    collections::HashMap,
    fmt,
    fs::{self, File, OpenOptions},
    future::Future,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::{
    ErrorData as McpError,
    model::{
        CallToolResult, ContentBlock, DetailedTask, JsonObject, Task, TaskPayload, TaskStatus,
    },
};
use serde::Serialize;
use tokio::{
    sync::{Notify, RwLock},
    task::JoinHandle,
    time::Instant as TokioInstant,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg(test)]
use crate::limits::DEFAULT_JOB_SPOOL_BYTES;
use crate::limits::MAX_JOB_SPOOL_BYTES_PER_STREAM;

const MAX_ACTIVE_JOBS: usize = 32;
const MAX_RETAINED_JOBS: usize = 256;
const JOB_POLL_INTERVAL_MS: u64 = 1_000;
const JOB_OUTPUT_RESPONSE_RESERVE: usize = 2 * 1024;
const SESSION_PREFIX: &str = "session-";
const SESSION_LOCK_FILE: &str = ".session.lock";
const SESSION_COORDINATION_LOCK: &str = ".sessions.lock";
const SESSION_COORDINATION_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_JOB_ERROR_BYTES: usize = 16 * 1024;

fn bounded_job_error(message: impl Into<String>) -> String {
    let mut message = message.into();
    if message.len() <= MAX_JOB_ERROR_BYTES {
        return message;
    }
    let mut end = MAX_JOB_ERROR_BYTES.saturating_sub('…'.len_utf8());
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push('…');
    message
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobStatus {
    Working,
    Completed,
    TimedOut,
    Failed,
    Cancelled,
}

impl JobStatus {
    fn terminal(self) -> bool {
        self != Self::Working
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobPhase {
    Queued,
    Running,
    Terminal,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(crate) struct StreamSummary {
    pub(crate) total_bytes: u64,
    pub(crate) stored_bytes: u64,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct JobResultContext {
    pub(crate) dir: String,
    pub(crate) sandboxed: bool,
    pub(crate) network: bool,
    pub(crate) timeout_secs: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct JobCompletion {
    pub(crate) status: JobStatus,
    pub(crate) exit_code: Option<i32>,
    pub(crate) success: bool,
    pub(crate) error: Option<String>,
    pub(crate) stdout: StreamSummary,
    pub(crate) stderr: StreamSummary,
    pub(crate) context: Option<JobResultContext>,
    pub(crate) internal_error: Option<String>,
}

impl JobCompletion {
    pub(crate) fn completed(
        exit_code: Option<i32>,
        success: bool,
        stdout: StreamSummary,
        stderr: StreamSummary,
    ) -> Self {
        Self {
            status: JobStatus::Completed,
            exit_code,
            success,
            error: None,
            stdout,
            stderr,
            context: None,
            internal_error: None,
        }
    }

    pub(crate) fn timed_out(stdout: StreamSummary, stderr: StreamSummary) -> Self {
        Self {
            status: JobStatus::TimedOut,
            exit_code: None,
            success: false,
            error: Some("shell job reached its requested timeout".to_owned()),
            stdout,
            stderr,
            context: None,
            internal_error: None,
        }
    }

    pub(crate) fn cancelled(stdout: StreamSummary, stderr: StreamSummary) -> Self {
        Self {
            status: JobStatus::Cancelled,
            exit_code: None,
            success: false,
            error: Some("shell job was cancelled".to_owned()),
            stdout,
            stderr,
            context: None,
            internal_error: None,
        }
    }

    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Self::failed_with_output(message, StreamSummary::default(), StreamSummary::default())
    }

    pub(crate) fn failed_with_output(
        message: impl Into<String>,
        stdout: StreamSummary,
        stderr: StreamSummary,
    ) -> Self {
        Self {
            status: JobStatus::Failed,
            exit_code: None,
            success: false,
            error: Some(bounded_job_error(message)),
            stdout,
            stderr,
            context: None,
            internal_error: None,
        }
    }

    pub(crate) fn internal_failed(
        message: impl Into<String>,
        stdout: StreamSummary,
        stderr: StreamSummary,
    ) -> Self {
        let message = bounded_job_error(message);
        Self {
            status: JobStatus::Failed,
            exit_code: None,
            success: false,
            error: Some(message.clone()),
            stdout,
            stderr,
            context: None,
            internal_error: Some(message),
        }
    }

    pub(crate) fn with_context(mut self, context: JobResultContext) -> Self {
        self.context = Some(context);
        self
    }
}

#[derive(Debug)]
struct JobEntry {
    id: String,
    created: Instant,
    created_at: String,
    updated_at: String,
    timeout_secs: u64,
    result_limit: usize,
    phase: JobPhase,
    cancel: CancellationToken,
    runner: Option<JoinHandle<()>>,
    completion: Option<JobCompletion>,
    terminal_at: Option<Instant>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout_consumed: u64,
    stderr_consumed: u64,
    output_released: bool,
    readers: usize,
    output_cleanup_requested: bool,
    output_cleanup_pending: bool,
    expiration_pending: bool,
    externally_exposed: bool,
    native_task_exposed: bool,
    request_key: Option<String>,
    notify: Arc<Notify>,
}

impl JobEntry {
    fn status(&self) -> JobStatus {
        self.completion
            .as_ref()
            .map_or(JobStatus::Working, |completion| completion.status)
    }
}

#[derive(Default)]
struct JobState {
    closed: bool,
    jobs: HashMap<String, JobEntry>,
    request_jobs: HashMap<String, String>,
}

struct JobManagerInner {
    root: PathBuf,
    retention_secs: AtomicU64,
    spool_limit_bytes: AtomicU64,
    spool_used_bytes: AtomicU64,
    closed: AtomicBool,
    state: Mutex<JobState>,
    operation_gate: Arc<RwLock<()>>,
    session_lock: Mutex<Option<File>>,
    sweeper: Mutex<Option<JoinHandle<()>>>,
    intent_fingerprint: Mutex<Option<[u8; 32]>>,
    policy_fingerprint: Mutex<Option<[u8; 32]>>,
}

impl fmt::Debug for JobManagerInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobManagerInner")
            .field("root", &self.root)
            .field(
                "retention_secs",
                &self.retention_secs.load(Ordering::Relaxed),
            )
            .field(
                "spool_limit_bytes",
                &self.spool_limit_bytes.load(Ordering::Relaxed),
            )
            .field(
                "spool_used_bytes",
                &self.spool_used_bytes.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl Drop for JobManagerInner {
    fn drop(&mut self) {
        if let Some(handle) = self.sweeper.lock().ok().and_then(|mut value| value.take()) {
            handle.abort();
        }
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            for entry in state.jobs.values_mut() {
                entry.cancel.cancel();
                if let Some(handle) = entry.runner.take() {
                    handle.abort();
                }
            }
            state.jobs.clear();
            state.request_jobs.clear();
        }
        if let Ok(mut lock) = self.session_lock.lock() {
            lock.take();
        }
        let root = self.root.clone();
        if root.exists() && fs::remove_dir_all(&root).is_err() {
            let _ = std::thread::Builder::new()
                .name("dotlink-job-cleanup".into())
                .spawn(move || {
                    for _ in 0..40 {
                        if fs::remove_dir_all(&root).is_ok() || !root.exists() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    tracing::warn!(path = %root.display(), "could not remove job spool directory on shutdown");
                });
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct JobManager {
    inner: Arc<JobManagerInner>,
}

#[derive(Clone, Debug)]
pub(crate) struct JobFinisher {
    inner: Weak<JobManagerInner>,
}

#[derive(Clone, Debug)]
pub(crate) struct JobHandle {
    pub(crate) id: String,
    pub(crate) cancel: CancellationToken,
    pub(crate) stdout_path: PathBuf,
    pub(crate) stderr_path: PathBuf,
    inner: Weak<JobManagerInner>,
}

impl JobHandle {
    pub(crate) fn mark_running(&self) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        let mut state = inner.state.lock().expect("job manager lock");
        if state.closed {
            return false;
        }
        let Some(entry) = state.jobs.get_mut(&self.id) else {
            return false;
        };
        if entry.completion.is_some() {
            return false;
        }
        entry.phase = JobPhase::Running;
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        entry.notify.notify_waiters();
        true
    }

    /// Reserve up to `requested` bytes from the process-session spool budget.
    /// The caller may persist only the returned prefix and must keep draining
    /// the producer after the budget is exhausted.
    pub(crate) fn reserve_spool(&self, requested: u64) -> u64 {
        self.inner
            .upgrade()
            .map_or(0, |inner| reserve_spool_bytes(&inner, requested))
    }

    pub(crate) fn release_spool(&self, bytes: u64) {
        if let Some(inner) = self.inner.upgrade() {
            release_spool_bytes(&inner, bytes);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JobStream {
    Stdout,
    Stderr,
}

impl JobStream {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct JobStart {
    pub(crate) id: String,
    pub(crate) is_new: bool,
}

#[derive(Debug)]
pub(crate) enum JobWait {
    Terminal,
    Pending,
}

#[derive(Debug)]
pub(crate) enum JobError {
    Unknown(String),
    Internal(anyhow::Error),
}

impl JobError {
    pub(crate) fn into_mcp(self) -> McpError {
        match self {
            Self::Unknown(message) => McpError::invalid_params(message, None),
            Self::Internal(error) => McpError::internal_error(error.to_string(), None),
        }
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(message) => f.write_str(message),
            Self::Internal(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for JobError {}

impl From<anyhow::Error> for JobError {
    fn from(value: anyhow::Error) -> Self {
        Self::Internal(value)
    }
}

type JobResult<T> = std::result::Result<T, JobError>;

#[derive(Debug, Serialize)]
struct JobOutputPage<'a> {
    job_id: &'a str,
    stream: &'static str,
    offset: u64,
    next_offset: u64,
    stored_bytes: u64,
    total_bytes: Option<u64>,
    complete: bool,
    truncated: bool,
    released: bool,
    encoding: &'static str,
    data: String,
}

struct PendingJobFiles {
    paths: [PathBuf; 2],
    committed: bool,
}

impl PendingJobFiles {
    fn create(root: &Path, id: &str) -> Result<Self> {
        let paths = [
            root.join(format!("{id}.stdout")),
            root.join(format!("{id}.stderr")),
        ];
        let mut created = Vec::new();
        for path in &paths {
            if let Err(error) = create_private_file(path) {
                for created in created {
                    let _ = fs::remove_file(created);
                }
                return Err(error);
            }
            created.push(path.clone());
        }
        Ok(Self {
            paths,
            committed: false,
        })
    }

    fn commit(mut self) -> [PathBuf; 2] {
        self.committed = true;
        self.paths.clone()
    }
}

impl Drop for PendingJobFiles {
    fn drop(&mut self) {
        if !self.committed {
            for path in &self.paths {
                let _ = fs::remove_file(path);
            }
        }
    }
}

struct JobReadLease {
    inner: Weak<JobManagerInner>,
    id: String,
}

impl Drop for JobReadLease {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        release_reader(&inner, &self.id);
    }
}

impl JobFinisher {
    pub(crate) fn finish(&self, id: &str, completion: JobCompletion) {
        if let Some(inner) = self.inner.upgrade() {
            finish_inner(&inner, id, completion);
        }
    }
}

impl JobManager {
    #[cfg(test)]
    pub(crate) fn new(retention: Duration) -> Result<Self> {
        Self::with_spool_limit(retention, DEFAULT_JOB_SPOOL_BYTES)
    }

    pub(crate) fn with_spool_limit(retention: Duration, spool_limit_bytes: u64) -> Result<Self> {
        let root = crate::xdg::state_home()?
            .join("abird/dotlink/jobs")
            .join(format!(
                "{SESSION_PREFIX}{}-{}",
                std::process::id(),
                Uuid::new_v4().simple()
            ));
        let inner = Arc::new(JobManagerInner {
            root,
            retention_secs: AtomicU64::new(retention.as_secs()),
            spool_limit_bytes: AtomicU64::new(spool_limit_bytes),
            spool_used_bytes: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            state: Mutex::new(JobState::default()),
            operation_gate: Arc::new(RwLock::new(())),
            session_lock: Mutex::new(None),
            sweeper: Mutex::new(None),
            intent_fingerprint: Mutex::new(None),
            policy_fingerprint: Mutex::new(None),
        });
        Self::start_sweeper(&inner);
        Ok(Self { inner })
    }

    fn start_sweeper(inner: &Arc<JobManagerInner>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak: Weak<JobManagerInner> = Arc::downgrade(inner);
        let handle = runtime.spawn(async move {
            let interval = Duration::from_secs(30);
            loop {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                sweep_expired(&inner);
                let root = inner.root.clone();
                crate::blocking::run(|| cleanup_stale_sessions_coordinated(&root));
                drop(inner);
                tokio::time::sleep(interval).await;
            }
        });
        if let Ok(mut slot) = inner.sweeper.lock() {
            *slot = Some(handle);
        }
    }

    fn ensure_session_initialized(&self) -> Result<()> {
        if self.inner.closed.load(Ordering::Acquire) {
            bail!("job session is shutting down");
        }
        let mut live_lock = self.inner.session_lock.lock().expect("job session lock");
        // Recheck while holding the initialization mutex. Shutdown may have
        // closed and removed the session while this caller was waiting here;
        // never recreate control-plane state after that point.
        if self.inner.closed.load(Ordering::Acquire) {
            bail!("job session is shutting down");
        }
        if live_lock.is_some() {
            return Ok(());
        }
        let parent = self
            .inner
            .root
            .parent()
            .ok_or_else(|| anyhow!("job session root has no parent"))?;
        let _coordination = crate::private_state::acquire(
            &parent.join(SESSION_COORDINATION_LOCK),
            SESSION_COORDINATION_TIMEOUT,
        )?;
        crate::private_state::prepare_owned_dir(&self.inner.root)?;
        let lock_path = self.inner.root.join(SESSION_LOCK_FILE);
        let session_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("cannot open job session lock {}", lock_path.display()))?;
        crate::setup::set_private_file_permissions(&lock_path)?;
        match session_lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                bail!(
                    "job session lock is unexpectedly held: {}",
                    lock_path.display()
                )
            }
            Err(fs::TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("cannot lock job session {}", lock_path.display()));
            }
        }
        *live_lock = Some(session_lock);
        cleanup_stale_sessions(parent, &self.inner.root);
        Ok(())
    }

    /// Atomically admit, register and own one runner. If `request_key` was
    /// already admitted, the existing job id is returned and `runner` is never
    /// invoked. Holding the state lock across spawn/registration closes the
    /// create-vs-shutdown and spawn-vs-attach gaps.
    pub(crate) fn spawn<F, Fut>(
        &self,
        timeout_secs: u64,
        result_limit: usize,
        request_key: Option<String>,
        runner: F,
    ) -> Result<JobStart>
    where
        F: FnOnce(JobHandle) -> Fut + Send + 'static,
        Fut: Future<Output = JobCompletion> + Send + 'static,
    {
        self.ensure_session_initialized()?;
        sweep_expired(&self.inner);
        let mut state = self.inner.state.lock().expect("job manager lock");
        if state.closed || self.inner.closed.load(Ordering::Acquire) {
            bail!("job session is shutting down");
        }
        if let Some(key) = request_key.as_ref()
            && let Some(id) = state.request_jobs.get(key).cloned()
        {
            if state.jobs.contains_key(&id) {
                return Ok(JobStart { id, is_new: false });
            }
            state.request_jobs.remove(key);
        }
        let active = state
            .jobs
            .values()
            .filter(|entry| !entry.status().terminal())
            .count();
        if active >= MAX_ACTIVE_JOBS {
            bail!("too many active shell jobs ({MAX_ACTIVE_JOBS}); wait for or cancel one");
        }
        if state.jobs.len() >= MAX_RETAINED_JOBS {
            evict_oldest_reclaimable_terminal(&self.inner, &mut state);
        }
        if state.jobs.len() >= MAX_RETAINED_JOBS {
            bail!(
                "too many retained shell jobs ({MAX_RETAINED_JOBS}); acknowledge output or wait for retention cleanup"
            );
        }

        let id = Uuid::new_v4().to_string();
        let files = PendingJobFiles::create(&self.inner.root, &id)?;
        let [stdout_path, stderr_path] = files.paths.clone();
        let cancel = CancellationToken::new();
        let notify = Arc::new(Notify::new());
        let now = chrono::Utc::now().to_rfc3339();
        state.jobs.insert(
            id.clone(),
            JobEntry {
                id: id.clone(),
                created: Instant::now(),
                created_at: now.clone(),
                updated_at: now,
                timeout_secs,
                result_limit,
                phase: JobPhase::Queued,
                cancel: cancel.clone(),
                runner: None,
                completion: None,
                terminal_at: None,
                stdout_path: stdout_path.clone(),
                stderr_path: stderr_path.clone(),
                stdout_consumed: 0,
                stderr_consumed: 0,
                output_released: false,
                readers: 0,
                output_cleanup_requested: false,
                output_cleanup_pending: false,
                expiration_pending: false,
                externally_exposed: false,
                native_task_exposed: false,
                request_key: request_key.clone(),
                notify,
            },
        );
        if let Some(key) = request_key {
            state.request_jobs.insert(key, id.clone());
        }
        let handle = JobHandle {
            id: id.clone(),
            cancel,
            stdout_path,
            stderr_path,
            inner: Arc::downgrade(&self.inner),
        };
        let finisher = self.finisher();
        let id_for_task = id.clone();
        let future = runner(handle);
        let task = tokio::spawn(async move {
            let completion = future.await;
            finisher.finish(&id_for_task, completion);
        });
        state
            .jobs
            .get_mut(&id)
            .expect("job inserted before runner registration")
            .runner = Some(task);
        files.commit();
        Ok(JobStart { id, is_new: true })
    }

    #[cfg(test)]
    fn create_for_test(&self, timeout_secs: u64) -> Result<JobHandle> {
        self.ensure_session_initialized()?;
        sweep_expired(&self.inner);
        let mut state = self.inner.state.lock().expect("job manager lock");
        if state.closed {
            bail!("job session is shutting down");
        }
        if state.jobs.len() >= MAX_RETAINED_JOBS {
            evict_oldest_reclaimable_terminal(&self.inner, &mut state);
        }
        if state.jobs.len() >= MAX_RETAINED_JOBS {
            bail!(
                "too many retained shell jobs ({MAX_RETAINED_JOBS}); acknowledge output or wait for retention cleanup"
            );
        }
        let id = Uuid::new_v4().to_string();
        let files = PendingJobFiles::create(&self.inner.root, &id)?;
        let [stdout_path, stderr_path] = files.paths.clone();
        let cancel = CancellationToken::new();
        let now = chrono::Utc::now().to_rfc3339();
        state.jobs.insert(
            id.clone(),
            JobEntry {
                id: id.clone(),
                created: Instant::now(),
                created_at: now.clone(),
                updated_at: now,
                timeout_secs,
                result_limit: crate::limits::DEFAULT_TRANSFER_BYTES,
                phase: JobPhase::Running,
                cancel: cancel.clone(),
                runner: None,
                completion: None,
                terminal_at: None,
                stdout_path: stdout_path.clone(),
                stderr_path: stderr_path.clone(),
                stdout_consumed: 0,
                stderr_consumed: 0,
                output_released: false,
                readers: 0,
                output_cleanup_requested: false,
                output_cleanup_pending: false,
                expiration_pending: false,
                externally_exposed: false,
                native_task_exposed: false,
                request_key: None,
                notify: Arc::new(Notify::new()),
            },
        );
        files.commit();
        Ok(JobHandle {
            id,
            cancel,
            stdout_path,
            stderr_path,
            inner: Arc::downgrade(&self.inner),
        })
    }

    #[cfg(test)]
    pub(crate) fn finish(&self, id: &str, completion: JobCompletion) {
        finish_inner(&self.inner, id, completion);
    }

    pub(crate) fn finisher(&self) -> JobFinisher {
        JobFinisher {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub(crate) fn set_limits(&self, retention: Duration, spool_limit_bytes: u64) {
        self.inner
            .retention_secs
            .store(retention.as_secs(), Ordering::Relaxed);
        self.inner
            .spool_limit_bytes
            .store(spool_limit_bytes, Ordering::Relaxed);
        sweep_expired(&self.inner);
    }

    pub(crate) fn bind_intent(&self, fingerprint: [u8; 32]) -> bool {
        let mut current = self
            .inner
            .intent_fingerprint
            .lock()
            .expect("job intent fingerprint lock");
        match *current {
            Some(existing) => existing == fingerprint,
            None => {
                *current = Some(fingerprint);
                true
            }
        }
    }

    pub(crate) fn bind_policy(&self, fingerprint: [u8; 32]) -> bool {
        let mut current = self
            .inner
            .policy_fingerprint
            .lock()
            .expect("job policy fingerprint lock");
        match *current {
            Some(existing) => existing == fingerprint,
            None => {
                *current = Some(fingerprint);
                true
            }
        }
    }

    pub(crate) fn operation_gate(&self) -> Arc<RwLock<()>> {
        self.inner.operation_gate.clone()
    }

    pub(crate) fn protected_path(&self) -> &Path {
        &self.inner.root
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.inner.closed.store(true, Ordering::Release);
        let sweeper = self.inner.sweeper.lock().expect("job sweeper lock").take();
        if let Some(sweeper) = sweeper {
            sweeper.abort();
            let _ = sweeper.await;
        }

        let runners = {
            let mut state = self.inner.state.lock().expect("job manager lock");
            state.closed = true;
            state.request_jobs.clear();
            let mut runners = Vec::new();
            for entry in state.jobs.values_mut() {
                entry.cancel.cancel();
                if let Some(handle) = entry.runner.take() {
                    handle.abort();
                    runners.push(handle);
                }
            }
            runners
        };
        for runner in runners {
            let _ = runner.await;
        }

        let entries = {
            let mut state = self.inner.state.lock().expect("job manager lock");
            state
                .jobs
                .drain()
                .map(|(_, entry)| entry)
                .collect::<Vec<_>>()
        };
        for entry in &entries {
            cleanup_entry(&self.inner, entry);
        }
        self.inner
            .session_lock
            .lock()
            .expect("job session lock")
            .take();
        match tokio::fs::remove_dir_all(&self.inner.root).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot remove job spool session {}",
                        self.inner.root.display()
                    )
                });
            }
        }
        Ok(())
    }

    fn retention(&self) -> Duration {
        Duration::from_secs(self.inner.retention_secs.load(Ordering::Relaxed))
    }

    pub(crate) fn cancel(&self, id: &str) -> JobResult<String> {
        sweep_expired(&self.inner);
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .jobs
            .get_mut(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        if !entry.status().terminal() {
            entry.cancel.cancel();
            entry.updated_at = chrono::Utc::now().to_rfc3339();
        }
        Ok(render_status_locked(&self.inner, entry))
    }

    pub(crate) fn status(&self, id: &str) -> JobResult<String> {
        sweep_expired(&self.inner);
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .jobs
            .get(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        Ok(render_status_locked(&self.inner, entry))
    }

    pub(crate) fn list(&self, limit: usize) -> String {
        sweep_expired(&self.inner);
        let state = self.inner.state.lock().expect("job manager lock");
        let mut jobs = state
            .jobs
            .values()
            .filter(|entry| entry.externally_exposed)
            .collect::<Vec<_>>();
        jobs.sort_by_key(|entry| std::cmp::Reverse(entry.created));
        serde_json::json!({
            "jobs": jobs.into_iter().take(limit).map(|entry| serde_json::json!({
                "job_id": entry.id,
                "status": entry.status(),
                "phase": entry.phase,
                "created_at": entry.created_at,
                "elapsed_ms": entry.created.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            })).collect::<Vec<_>>(),
            "spool_used_bytes": self.inner.spool_used_bytes.load(Ordering::Relaxed),
            "spool_limit_bytes": self.inner.spool_limit_bytes.load(Ordering::Relaxed),
        })
        .to_string()
    }

    pub(crate) fn expose_compatibility(&self, id: &str) -> JobResult<CallToolResult> {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .jobs
            .get_mut(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        entry.externally_exposed = true;
        Ok(compatibility_receipt(&self.inner, entry))
    }

    /// Expose a job as a native MCP Task only when its exact eventual
    /// CallToolResult is guaranteed to fit the tightest task-result budget for the current request and every active transport sharing the runtime.
    /// An already-native task remains native on redelivery.
    pub(crate) fn expose_native_task(
        &self,
        id: &str,
        budget: Option<usize>,
    ) -> JobResult<Option<Task>> {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .jobs
            .get_mut(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        if !entry.native_task_exposed
            && budget.is_some_and(|budget| {
                !crate::mcp::shell_result_fits_budget(entry.result_limit, budget)
            })
        {
            return Ok(None);
        }
        entry.externally_exposed = true;
        entry.native_task_exposed = true;
        Ok(Some(task_from_entry(entry, self.retention())))
    }

    pub(crate) fn is_exposed(&self, id: &str) -> JobResult<(bool, bool)> {
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .jobs
            .get(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        Ok((entry.externally_exposed, entry.native_task_exposed))
    }

    pub(crate) fn ensure_native_task(&self, id: &str) -> JobResult<()> {
        let state = self.inner.state.lock().expect("job manager lock");
        match state.jobs.get(id) {
            Some(entry) if entry.native_task_exposed => Ok(()),
            _ => Err(JobError::Unknown(format!("unknown task {id}"))),
        }
    }

    /// Wait until the job becomes terminal or `deadline` is reached. The same
    /// stable job can be awaited repeatedly after a redelivery.
    pub(crate) async fn wait_until(
        &self,
        id: &str,
        deadline: Option<TokioInstant>,
    ) -> JobResult<JobWait> {
        loop {
            let notify = {
                let state = self.inner.state.lock().expect("job manager lock");
                let entry = state
                    .jobs
                    .get(id)
                    .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
                if entry.status().terminal() {
                    return Ok(JobWait::Terminal);
                }
                entry.notify.clone().notified_owned()
            };
            match deadline {
                Some(deadline) => {
                    if TokioInstant::now() >= deadline {
                        return Ok(JobWait::Pending);
                    }
                    tokio::select! {
                        _ = notify => {}
                        _ = tokio::time::sleep_until(deadline) => return Ok(JobWait::Pending),
                    }
                }
                None => notify.await,
            }
        }
    }

    /// Render the ordinary shell result from durable execution metadata and
    /// leased stdout/stderr spools. `budget=None` is used by local transports;
    /// a bounded transport gets `None` when the complete serialized result does
    /// not fit and should expose the same durable job/task instead.
    pub(crate) fn tool_result(
        &self,
        id: &str,
        budget: Option<usize>,
    ) -> JobResult<Option<CallToolResult>> {
        sweep_expired(&self.inner);
        let snapshot = {
            let mut state = self.inner.state.lock().expect("job manager lock");
            let entry = state
                .jobs
                .get_mut(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            if !entry.status().terminal() {
                return Err(JobError::Internal(anyhow!("job {id} is still running")));
            }
            if entry.output_released {
                return Ok(None);
            }
            entry.readers += 1;
            ToolResultSnapshot {
                completion: entry
                    .completion
                    .clone()
                    .ok_or_else(|| JobError::Internal(anyhow!("job {id} has no completion")))?,
                timeout_secs: entry.timeout_secs,
                result_limit: entry.result_limit,
                stdout_path: entry.stdout_path.clone(),
                stderr_path: entry.stderr_path.clone(),
                _lease: JobReadLease {
                    inner: Arc::downgrade(&self.inner),
                    id: id.to_owned(),
                },
            }
        };
        let result =
            crate::blocking::run(|| render_tool_result(&snapshot)).map_err(JobError::Internal)?;
        if budget.is_some_and(|limit| serialized_call_tool_result_len(&result) > limit) {
            return Ok(None);
        }
        Ok(Some(result))
    }

    /// A synchronous result can release its stream spools immediately on
    /// transports that have no durable redelivery identity. OpenAI request-key
    /// jobs retain the spools until terminal delivery is positively acknowledged;
    /// the ordinary result is reconstructed from those same bytes on redelivery.
    pub(crate) fn mark_synchronous_result(&self, id: &str) -> JobResult<()> {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .jobs
            .get_mut(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        if entry.request_key.is_none()
            && let Some(completion) = entry.completion.as_mut()
        {
            entry.stdout_consumed = completion.stdout.stored_bytes;
            entry.stderr_consumed = completion.stderr.stored_bytes;
            entry.output_cleanup_requested = true;
            maybe_release_output(&self.inner, entry);
        }
        Ok(())
    }

    /// Confirm that the transport accepted the terminal response for a durable
    /// request identity. This is the only point where OpenAI redelivery state
    /// becomes unnecessary. Hidden synchronous jobs are compacted; exposed jobs
    /// keep their task/job state but no longer occupy the dedupe ledger.
    pub(crate) fn acknowledge_request_delivery(&self, request_key: &str) {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let Some(id) = state.request_jobs.remove(request_key) else {
            return;
        };
        let Some(entry) = state.jobs.get_mut(&id) else {
            return;
        };
        if entry.request_key.as_deref() == Some(request_key) {
            entry.request_key = None;
        }
        if !entry.externally_exposed
            && entry.status().terminal()
            && let Some(completion) = entry.completion.as_mut()
        {
            entry.stdout_consumed = completion.stdout.stored_bytes;
            entry.stderr_consumed = completion.stderr.stored_bytes;
            entry.output_cleanup_requested = true;
            maybe_release_output(&self.inner, entry);
        }
    }

    pub(crate) fn output_page(
        &self,
        id: &str,
        stream: JobStream,
        offset: u64,
        requested_bytes: usize,
        response_ceiling: usize,
        base64: bool,
    ) -> JobResult<String> {
        sweep_expired(&self.inner);
        let payload_ceiling =
            job_output_payload_ceiling(response_ceiling).map_err(JobError::Internal)?;
        let (snapshot, _lease) = {
            let mut state = self.inner.state.lock().expect("job manager lock");
            let entry = state
                .jobs
                .get_mut(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            let summary = entry.completion.as_ref().map(|completion| match stream {
                JobStream::Stdout => completion.stdout,
                JobStream::Stderr => completion.stderr,
            });
            let stored = summary.map_or_else(
                || stored_len(stream_path(entry, stream), None),
                |summary| summary.stored_bytes,
            );
            if offset > stored {
                return Err(JobError::Internal(anyhow!(
                    "job {id} {} offset {offset} is beyond stored output ({stored} bytes)",
                    stream.label()
                )));
            }
            if entry.output_released {
                if offset == stored {
                    return OutputPageRender {
                        id,
                        stream,
                        offset,
                        stored_bytes: stored,
                        total_bytes: summary.map(|value| value.total_bytes),
                        truncated: summary.is_some_and(|value| value.truncated),
                        released: true,
                        base64,
                    }
                    .render(offset, true, &[])
                    .map_err(JobError::Internal);
                }
                return Err(JobError::Internal(anyhow!(
                    "job {id} output was already acknowledged and released"
                )));
            }
            entry.readers += 1;
            (
                OutputSnapshot {
                    path: stream_path(entry, stream).to_path_buf(),
                    terminal: entry.status().terminal(),
                    stored,
                    total: summary.map(|value| value.total_bytes),
                    truncated: summary.is_some_and(|value| value.truncated),
                    released: false,
                },
                JobReadLease {
                    inner: Arc::downgrade(&self.inner),
                    id: id.to_owned(),
                },
            )
        };

        let read_cap = requested_bytes.min(payload_ceiling).max(1);
        let mut file = OpenOptions::new()
            .read(true)
            .open(&snapshot.path)
            .with_context(|| format!("cannot open job {} output", stream.label()))
            .map_err(JobError::Internal)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| JobError::Internal(error.into()))?;
        let available = snapshot.stored.saturating_sub(offset);
        let to_read = usize::try_from(available.min(read_cap as u64)).unwrap_or(read_cap);
        let mut bytes = vec![0_u8; to_read];
        file.read_exact(&mut bytes)
            .map_err(|error| JobError::Internal(error.into()))?;
        fit_output_page(
            id,
            stream,
            offset,
            &bytes,
            &snapshot,
            payload_ceiling,
            base64,
        )
        .map_err(JobError::Internal)
    }

    /// Explicit, idempotent acknowledgement. Reads stay pure/replayable; only
    /// this control-plane operation advances deletion watermarks.
    pub(crate) fn acknowledge_output(
        &self,
        id: &str,
        stream: JobStream,
        offset: u64,
    ) -> JobResult<String> {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .jobs
            .get_mut(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        let stored = entry.completion.as_ref().map_or_else(
            || stored_len(stream_path(entry, stream), None),
            |completion| match stream {
                JobStream::Stdout => completion.stdout.stored_bytes,
                JobStream::Stderr => completion.stderr.stored_bytes,
            },
        );
        if offset > stored {
            return Err(JobError::Internal(anyhow!(
                "job {id} {} acknowledgement {offset} is beyond stored output ({stored} bytes)",
                stream.label()
            )));
        }
        match stream {
            JobStream::Stdout => entry.stdout_consumed = entry.stdout_consumed.max(offset),
            JobStream::Stderr => entry.stderr_consumed = entry.stderr_consumed.max(offset),
        }
        entry.output_cleanup_requested = true;
        maybe_release_output(&self.inner, entry);
        Ok(render_status_locked(&self.inner, entry))
    }

    pub(crate) fn detailed_task(&self, id: &str) -> JobResult<DetailedTask> {
        sweep_expired(&self.inner);
        let (task, status, internal_error, snapshot) = {
            let mut state = self.inner.state.lock().expect("job manager lock");
            let entry = state
                .jobs
                .get_mut(id)
                .filter(|entry| entry.native_task_exposed)
                .ok_or_else(|| JobError::Unknown(format!("unknown task {id}")))?;
            let task = task_from_entry(entry, self.retention());
            let status = entry.status();
            let internal_error = entry
                .completion
                .as_ref()
                .and_then(|completion| completion.internal_error.clone());
            let snapshot = if matches!(
                status,
                JobStatus::Completed | JobStatus::TimedOut | JobStatus::Failed
            ) && internal_error.is_none()
            {
                entry.readers += 1;
                Some(ToolResultSnapshot {
                    completion: entry.completion.clone().ok_or_else(|| {
                        JobError::Internal(anyhow!("task {id} has no completion"))
                    })?,
                    timeout_secs: entry.timeout_secs,
                    result_limit: entry.result_limit,
                    stdout_path: entry.stdout_path.clone(),
                    stderr_path: entry.stderr_path.clone(),
                    _lease: JobReadLease {
                        inner: Arc::downgrade(&self.inner),
                        id: id.to_owned(),
                    },
                })
            } else {
                None
            };
            (task, status, internal_error, snapshot)
        };
        let payload = match status {
            JobStatus::Working => TaskPayload::Working,
            JobStatus::Cancelled => TaskPayload::Cancelled,
            JobStatus::Completed | JobStatus::TimedOut | JobStatus::Failed => {
                if let Some(message) = internal_error {
                    TaskPayload::Failed {
                        error: error_object(&message),
                    }
                } else {
                    let result = crate::blocking::run(|| {
                        render_tool_result(snapshot.as_ref().expect("terminal task snapshot"))
                    })
                    .map_err(JobError::Internal)?;
                    TaskPayload::Completed {
                        result: result_object(result),
                    }
                }
            }
        };
        Ok(DetailedTask::new(task, payload))
    }
}

#[derive(Debug)]
struct OutputSnapshot {
    path: PathBuf,
    terminal: bool,
    stored: u64,
    total: Option<u64>,
    truncated: bool,
    released: bool,
}

struct ToolResultSnapshot {
    completion: JobCompletion,
    timeout_secs: u64,
    result_limit: usize,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    _lease: JobReadLease,
}

fn read_prefix(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("cannot open job output {}", path.display()))?;
    let max = u64::try_from(limit).unwrap_or(u64::MAX);
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    file.take(max)
        .read_to_end(&mut bytes)
        .context("cannot read job output")?;
    Ok(bytes)
}

fn render_tool_result(snapshot: &ToolResultSnapshot) -> Result<CallToolResult> {
    let completion = &snapshot.completion;
    if let Some(message) = completion.internal_error.as_deref() {
        return Ok(CallToolResult::error(vec![ContentBlock::text(
            message.to_owned(),
        )]));
    }
    let stdout = read_prefix(&snapshot.stdout_path, snapshot.result_limit)?;
    let stderr = read_prefix(&snapshot.stderr_path, snapshot.result_limit)?;
    let stdout_truncated =
        completion.stdout.truncated || completion.stdout.total_bytes > snapshot.result_limit as u64;
    let stderr_truncated =
        completion.stderr.truncated || completion.stderr.total_bytes > snapshot.result_limit as u64;

    let Some(context) = completion.context.as_ref() else {
        if completion.status != JobStatus::Completed {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                completion
                    .error
                    .clone()
                    .unwrap_or_else(|| "shell command failed before execution".to_owned()),
            )]));
        }
        let text = serde_json::json!({
            "exit_code": completion.exit_code,
            "success": completion.success,
            "timed_out": false,
            "stdout": String::from_utf8_lossy(&stdout),
            "stderr": String::from_utf8_lossy(&stderr),
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
        })
        .to_string();
        return Ok(CallToolResult::success(vec![ContentBlock::text(text)]));
    };

    let error = (completion.status == JobStatus::TimedOut).then(|| {
        format!(
            "command timed out after {} seconds and was terminated; use job_start for explicitly background execution or retry with a different timeout when the caller and transport permit it",
            snapshot.timeout_secs
        )
    });
    let text = serde_json::json!({
        "exit_code": completion.exit_code,
        "success": completion.success,
        "timed_out": completion.status == JobStatus::TimedOut,
        "dir": context.dir,
        "sandboxed": context.sandboxed,
        "network": context.network,
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
        "error": error.as_deref().or(completion.error.as_deref()),
    })
    .to_string();
    Ok(if completion.status == JobStatus::Completed {
        CallToolResult::success(vec![ContentBlock::text(text)])
    } else {
        CallToolResult::error(vec![ContentBlock::text(text)])
    })
}

fn serialized_call_tool_result_len(result: &CallToolResult) -> usize {
    crate::mcp::serialized_json_len(result).unwrap_or(usize::MAX)
}

fn error_object(message: &str) -> JsonObject {
    serde_json::to_value(McpError::internal_error(message.to_owned(), None))
        .expect("MCP error serializes")
        .as_object()
        .expect("MCP error is an object")
        .clone()
}

fn result_object(result: CallToolResult) -> JsonObject {
    serde_json::to_value(result)
        .expect("call tool result serializes")
        .as_object()
        .expect("call tool result is an object")
        .clone()
}

fn job_output_payload_ceiling(response_ceiling: usize) -> Result<usize> {
    if response_ceiling < crate::mcp::MIN_BYTE_LIMIT {
        bail!("job output response ceiling is below the supported minimum");
    }
    let reserve = JOB_OUTPUT_RESPONSE_RESERVE.min(response_ceiling / 4);
    let payload = response_ceiling.saturating_sub(reserve);
    if payload == 0 {
        bail!("job output response ceiling leaves no payload room");
    }
    Ok(payload)
}

fn double_json_data_len(text: &str) -> usize {
    text.chars()
        .map(|ch| match ch {
            '"' | '\\' => 4,
            '\u{08}' | '\t' | '\n' | '\u{0c}' | '\r' => 3,
            ch if ch <= '\u{1f}' => 7,
            ch => ch.len_utf8(),
        })
        .sum()
}

fn serialized_tool_result_len(page: &str) -> Result<usize> {
    Ok(crate::mcp::serialized_text_tool_result_len(page))
}

fn fit_output_page(
    id: &str,
    stream: JobStream,
    offset: u64,
    bytes: &[u8],
    snapshot: &OutputSnapshot,
    payload_ceiling: usize,
    base64: bool,
) -> Result<String> {
    let render = OutputPageRender {
        id,
        stream,
        offset,
        stored_bytes: snapshot.stored,
        total_bytes: snapshot.total,
        truncated: snapshot.truncated,
        released: snapshot.released,
        base64,
    };
    // Measure the largest metadata form once. `next_offset <= stored_bytes`, and
    // `false` is one byte longer than `true`, so every real page has at least
    // this much room left for data.
    let empty = render.render(snapshot.stored, false, &[])?;
    let fixed = serialized_tool_result_len(&empty)?;
    if fixed > payload_ceiling {
        bail!("job output metadata leaves no room under the response ceiling");
    }
    let data_budget = payload_ceiling - fixed;

    let take = if base64 {
        // Base64 characters need no JSON escaping at either nesting level.
        let groups = data_budget / 4;
        bytes.len().min(groups.saturating_mul(3))
    } else if let Ok(text) = std::str::from_utf8(bytes) {
        // Valid UTF-8 is the common shell-output path. Count the exact extra
        // bytes after the inner page JSON and outer CallToolResult JSON escapes,
        // preserving a source-byte boundary without repeated serialization.
        let mut used = 0_usize;
        let mut take = 0_usize;
        for (index, ch) in text.char_indices() {
            let cost = double_json_data_len(ch.encode_utf8(&mut [0; 4]));
            let Some(next) = used.checked_add(cost) else {
                break;
            };
            if next > data_budget {
                break;
            }
            used = next;
            take = index + ch.len_utf8();
        }
        take
    } else {
        // Invalid UTF-8 is uncommon and `from_utf8_lossy` changes byte-to-text
        // correspondence. Keep the exact, simple fallback for that case only.
        let fallback = |take: usize| {
            let next = offset + take as u64;
            render.render(
                next,
                snapshot.terminal && next >= snapshot.stored,
                &bytes[..take],
            )
        };
        let mut low = 0_usize;
        let mut high = bytes.len();
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            if serialized_tool_result_len(&fallback(middle)?)? <= payload_ceiling {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        text_page_boundary(bytes, low)
    };

    if take == 0 && !bytes.is_empty() {
        bail!("job output metadata leaves no room under the response ceiling");
    }
    let next = offset + take as u64;
    let rendered = render.render(
        next,
        snapshot.terminal && next >= snapshot.stored,
        &bytes[..take],
    )?;
    if serialized_tool_result_len(&rendered)? > payload_ceiling {
        bail!("job output MCP result exceeded its reserved response budget");
    }
    Ok(rendered)
}

fn stream_path(entry: &JobEntry, stream: JobStream) -> &Path {
    match stream {
        JobStream::Stdout => &entry.stdout_path,
        JobStream::Stderr => &entry.stderr_path,
    }
}

fn text_page_boundary(bytes: &[u8], take: usize) -> usize {
    let take = take.min(bytes.len());
    match std::str::from_utf8(&bytes[..take]) {
        Ok(_) => take,
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => take,
    }
}

struct OutputPageRender<'a> {
    id: &'a str,
    stream: JobStream,
    offset: u64,
    stored_bytes: u64,
    total_bytes: Option<u64>,
    truncated: bool,
    released: bool,
    base64: bool,
}

impl OutputPageRender<'_> {
    fn render(&self, next_offset: u64, complete: bool, bytes: &[u8]) -> Result<String> {
        let data = if self.base64 {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        };
        serde_json::to_string(&JobOutputPage {
            job_id: self.id,
            stream: self.stream.label(),
            offset: self.offset,
            next_offset,
            stored_bytes: self.stored_bytes,
            total_bytes: self.total_bytes,
            complete,
            truncated: self.truncated,
            released: self.released,
            encoding: if self.base64 { "base64" } else { "utf8_lossy" },
            data,
        })
        .context("cannot serialize job output page")
    }
}

fn render_status_locked(inner: &JobManagerInner, entry: &JobEntry) -> String {
    let stdout_stored = stored_len(&entry.stdout_path, None);
    let stderr_stored = stored_len(&entry.stderr_path, None);
    let completion = entry.completion.as_ref();
    serde_json::json!({
        "job_id": entry.id,
        "task_id": entry.id,
        "status": entry.status(),
        "phase": entry.phase,
        "elapsed_ms": entry.created.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        "timeout_secs": entry.timeout_secs,
        "spool_limit_bytes_per_stream": MAX_JOB_SPOOL_BYTES_PER_STREAM,
        "spool_used_bytes": inner.spool_used_bytes.load(Ordering::Relaxed),
        "spool_session_limit_bytes": inner.spool_limit_bytes.load(Ordering::Relaxed),
        "cancel_requested": entry.cancel.is_cancelled(),
        "exit_code": completion.and_then(|value| value.exit_code),
        "success": completion.map(|value| value.success),
        "error": completion.and_then(|value| value.error.as_deref()),
        "stdout": {
            "stored_bytes": stdout_stored,
            "acknowledged_bytes": entry.stdout_consumed,
            "total_bytes": completion.map(|value| value.stdout.total_bytes),
            "truncated": completion.is_some_and(|value| value.stdout.truncated),
            "released": entry.output_released,
        },
        "stderr": {
            "stored_bytes": stderr_stored,
            "acknowledged_bytes": entry.stderr_consumed,
            "total_bytes": completion.map(|value| value.stderr.total_bytes),
            "truncated": completion.is_some_and(|value| value.stderr.truncated),
            "released": entry.output_released,
        },
        "output": "use job_output to read; job_ack explicitly advances cleanup watermarks",
        "cancel": "use job_cancel while status=working",
    })
    .to_string()
}

fn compatibility_receipt(inner: &JobManagerInner, entry: &JobEntry) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        serde_json::json!({
            "job_id": entry.id,
            "task_id": entry.id,
            "status": entry.status(),
            "phase": entry.phase,
            "timeout_secs": entry.timeout_secs,
            "spool_limit_bytes_per_stream": MAX_JOB_SPOOL_BYTES_PER_STREAM,
            "spool_session_limit_bytes": inner.spool_limit_bytes.load(Ordering::Relaxed),
            "async": true,
            "poll": "job_get",
            "output": "job_output",
            "ack": "job_ack",
            "cancel": "job_cancel",
        })
        .to_string(),
    )])
}

fn finish_inner(inner: &JobManagerInner, id: &str, completion: JobCompletion) {
    let mut state = inner.state.lock().expect("job manager lock");
    if let Some(entry) = state.jobs.get_mut(id) {
        entry.runner = None;
        entry.phase = JobPhase::Terminal;
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        entry.terminal_at = Some(Instant::now());
        entry.completion = Some(completion);
        if entry.output_cleanup_requested {
            maybe_release_output(inner, entry);
        }
        entry.notify.notify_waiters();
    }
}

fn reserve_spool_bytes(inner: &JobManagerInner, requested: u64) -> u64 {
    if requested == 0 {
        return 0;
    }
    loop {
        let used = inner.spool_used_bytes.load(Ordering::Acquire);
        let limit = inner.spool_limit_bytes.load(Ordering::Relaxed);
        let available = limit.saturating_sub(used);
        let reserved = requested.min(available);
        if reserved == 0 {
            return 0;
        }
        match inner.spool_used_bytes.compare_exchange_weak(
            used,
            used.saturating_add(reserved),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return reserved,
            Err(_) => continue,
        }
    }
}

fn release_spool_bytes(inner: &JobManagerInner, bytes: u64) {
    if bytes == 0 {
        return;
    }
    let mut current = inner.spool_used_bytes.load(Ordering::Acquire);
    loop {
        let next = current.saturating_sub(bytes);
        match inner.spool_used_bytes.compare_exchange_weak(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

fn maybe_release_output(inner: &JobManagerInner, entry: &mut JobEntry) {
    if !entry.output_cleanup_requested {
        return;
    }
    let Some(completion) = entry.completion.as_ref() else {
        return;
    };
    if entry.output_released
        || entry.stdout_consumed < completion.stdout.stored_bytes
        || entry.stderr_consumed < completion.stderr.stored_bytes
    {
        return;
    }
    if entry.readers != 0 {
        entry.output_cleanup_pending = true;
        return;
    }
    release_acknowledged_output(inner, entry);
}

fn release_acknowledged_output(inner: &JobManagerInner, entry: &mut JobEntry) {
    if entry.native_task_exposed {
        let _ = compact_native_task_output(inner, entry);
    } else {
        let _ = release_output_files(inner, entry);
    }
}

fn truncate_file_releasing(inner: &JobManagerInner, path: &Path, keep: u64) -> bool {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return keep == 0,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not inspect native task spool for compaction");
            return false;
        }
    };
    let len = metadata.len();
    if len <= keep {
        return true;
    }
    match OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_len(keep))
    {
        Ok(()) => {
            release_spool_bytes(inner, len - keep);
            true
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not compact native task spool");
            false
        }
    }
}

fn compact_native_task_output(inner: &JobManagerInner, entry: &mut JobEntry) -> bool {
    if entry.output_released {
        return true;
    }
    let Some(completion) = entry.completion.as_ref() else {
        return false;
    };
    let keep_result =
        completion.internal_error.is_none() && completion.status != JobStatus::Cancelled;
    let stdout_keep = if keep_result {
        completion
            .stdout
            .stored_bytes
            .min(entry.result_limit as u64)
    } else {
        0
    };
    let stderr_keep = if keep_result {
        completion
            .stderr
            .stored_bytes
            .min(entry.result_limit as u64)
    } else {
        0
    };
    let stdout_ok = truncate_file_releasing(inner, &entry.stdout_path, stdout_keep);
    let stderr_ok = truncate_file_releasing(inner, &entry.stderr_path, stderr_keep);
    let success = stdout_ok && stderr_ok;
    entry.output_released = success;
    entry.output_cleanup_pending = !success;
    success
}

fn release_output_files(inner: &JobManagerInner, entry: &mut JobEntry) -> bool {
    if entry.output_released {
        return true;
    }
    let stdout_ok = remove_file_releasing(inner, &entry.stdout_path);
    let stderr_ok = remove_file_releasing(inner, &entry.stderr_path);
    let success = stdout_ok && stderr_ok;
    entry.output_released = success;
    entry.output_cleanup_pending = !success;
    success
}

fn release_reader(inner: &Arc<JobManagerInner>, id: &str) {
    let mut expired = None;
    {
        let mut state = inner.state.lock().expect("job manager lock");
        let Some(entry) = state.jobs.get_mut(id) else {
            return;
        };
        entry.readers = entry.readers.saturating_sub(1);
        if entry.readers == 0 {
            if entry.expiration_pending {
                let cleanup_ok = state
                    .jobs
                    .get(id)
                    .is_some_and(|entry| cleanup_entry(inner, entry));
                if cleanup_ok {
                    expired = remove_job_locked(&mut state, id);
                }
            } else if entry.output_cleanup_pending {
                release_acknowledged_output(inner, entry);
            }
        }
    }
    drop(expired);
}

fn cleanup_stale_sessions_coordinated(current: &Path) {
    let Some(parent) = current.parent() else {
        return;
    };
    if !parent.exists() {
        return;
    }
    match crate::private_state::acquire(
        &parent.join(SESSION_COORDINATION_LOCK),
        SESSION_COORDINATION_TIMEOUT,
    ) {
        Ok(_coordination) => cleanup_stale_sessions(parent, current),
        Err(error) => {
            tracing::warn!(%error, path = %parent.display(), "could not coordinate stale job cleanup")
        }
    }
}

fn cleanup_stale_sessions(parent: &Path, current: &Path) {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, path = %parent.display(), "could not scan stale job sessions");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == current {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(SESSION_PREFIX) {
            continue;
        }
        if !fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_dir()) {
            continue;
        }
        let lock_path = path.join(SESSION_LOCK_FILE);
        let stale = match OpenOptions::new().read(true).write(true).open(&lock_path) {
            Ok(lock) => match lock.try_lock() {
                Ok(()) => true,
                Err(fs::TryLockError::WouldBlock) => false,
                Err(fs::TryLockError::Error(error)) => {
                    tracing::warn!(%error, path = %lock_path.display(), "could not inspect job session lock");
                    false
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                tracing::warn!(%error, path = %lock_path.display(), "could not open job session lock");
                false
            }
        };
        if stale
            && let Err(error) = fs::remove_dir_all(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, path = %path.display(), "could not remove stale job session");
        }
    }
}

fn create_private_file(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("cannot create private job spool {}", path.display()))?;
    drop(file);
    if let Err(error) = crate::setup::set_private_file_permissions(path) {
        // `create_new` made this pathname ours. A hardening failure must roll it
        // back here because the caller cannot know whether creation happened
        // before the permission operation failed.
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

fn stored_len(path: &Path, hint: Option<u64>) -> u64 {
    hint.unwrap_or_else(|| fs::metadata(path).map_or(0, |metadata| metadata.len()))
}

fn remove_file_releasing(inner: &JobManagerInner, path: &Path) -> bool {
    let bytes = fs::metadata(path).map_or(0, |metadata| metadata.len());
    match fs::remove_file(path) {
        Ok(()) => {
            release_spool_bytes(inner, bytes);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            release_spool_bytes(inner, bytes);
            true
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not remove job spool file");
            false
        }
    }
}

fn cleanup_entry(inner: &JobManagerInner, entry: &JobEntry) -> bool {
    let stdout_ok = remove_file_releasing(inner, &entry.stdout_path);
    let stderr_ok = remove_file_releasing(inner, &entry.stderr_path);
    stdout_ok && stderr_ok
}

fn remove_job_locked(state: &mut JobState, id: &str) -> Option<JobEntry> {
    let entry = state.jobs.remove(id)?;
    if let Some(key) = entry.request_key.as_ref()
        && state
            .request_jobs
            .get(key)
            .is_some_and(|mapped| mapped == id)
    {
        state.request_jobs.remove(key);
    }
    Some(entry)
}

fn reclaimable_terminal(entry: &JobEntry) -> bool {
    if !entry.status().terminal()
        || entry.request_key.is_some()
        || entry.readers != 0
        || entry.native_task_exposed
    {
        return false;
    }
    let empty = entry.completion.as_ref().is_some_and(|completion| {
        completion.stdout.stored_bytes == 0 && completion.stderr.stored_bytes == 0
    });
    entry.output_released || (!entry.externally_exposed && empty)
}

fn evict_oldest_reclaimable_terminal(inner: &JobManagerInner, state: &mut JobState) {
    let candidate = state
        .jobs
        .iter()
        .filter(|(_, entry)| reclaimable_terminal(entry))
        .min_by_key(|(_, entry)| entry.terminal_at.unwrap_or(entry.created))
        .map(|(id, _)| id.clone());
    if let Some(id) = candidate {
        let cleanup_ok = state
            .jobs
            .get(&id)
            .is_some_and(|entry| cleanup_entry(inner, entry));
        if cleanup_ok {
            let _ = remove_job_locked(state, &id);
        }
    }
}

fn sweep_expired(inner: &JobManagerInner) {
    let retention = Duration::from_secs(inner.retention_secs.load(Ordering::Relaxed));
    {
        let Ok(mut state) = inner.state.lock() else {
            return;
        };
        for entry in state.jobs.values_mut() {
            if entry.output_cleanup_pending && entry.readers == 0 {
                release_acknowledged_output(inner, entry);
            }
        }
        let expired = state
            .jobs
            .iter()
            .filter_map(|(id, entry)| {
                entry
                    .terminal_at
                    .filter(|terminal| terminal.elapsed() >= retention)
                    .map(|_| id.clone())
            })
            .collect::<Vec<_>>();
        for id in expired {
            if state.jobs.get(&id).is_some_and(|entry| entry.readers != 0) {
                if let Some(entry) = state.jobs.get_mut(&id) {
                    entry.expiration_pending = true;
                }
                continue;
            }
            let cleanup_ok = state
                .jobs
                .get(&id)
                .is_some_and(|entry| cleanup_entry(inner, entry));
            if cleanup_ok {
                let _ = remove_job_locked(&mut state, &id);
            } else if let Some(entry) = state.jobs.get_mut(&id) {
                entry.expiration_pending = true;
            }
        }
    }
}

fn task_from_entry(entry: &JobEntry, retention: Duration) -> Task {
    let status = match entry.status() {
        JobStatus::Working => TaskStatus::Working,
        JobStatus::Cancelled => TaskStatus::Cancelled,
        JobStatus::Completed | JobStatus::TimedOut => TaskStatus::Completed,
        JobStatus::Failed
            if entry
                .completion
                .as_ref()
                .and_then(|value| value.internal_error.as_ref())
                .is_some() =>
        {
            TaskStatus::Failed
        }
        JobStatus::Failed => TaskStatus::Completed,
    };
    let retention_ms = u64::try_from(retention.as_millis()).unwrap_or(u64::MAX);
    let mut task = Task::new(
        entry.id.clone(),
        status,
        entry.created_at.clone(),
        entry.updated_at.clone(),
    );
    if let Some(terminal_at) = entry.terminal_at {
        let terminal_age = terminal_at.saturating_duration_since(entry.created);
        let ttl = u64::try_from(terminal_age.as_millis())
            .unwrap_or(u64::MAX)
            .saturating_add(retention_ms);
        task = task.with_ttl_ms(ttl);
    }
    task.with_poll_interval_ms(JOB_POLL_INTERVAL_MS)
        .with_status_message(match entry.phase {
            JobPhase::Queued => "shell job is queued",
            JobPhase::Running => {
                "shell job is running; job_output provides durable paged stdout/stderr"
            }
            JobPhase::Terminal => {
                "shell job finished; job_output provides durable paged stdout/stderr"
            }
        })
}

pub(crate) fn spool_limit() -> u64 {
    MAX_JOB_SPOOL_BYTES_PER_STREAM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_task_ttl_is_unlimited_until_queue_and_execution_finish() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(120).unwrap();
        let task = manager.expose_native_task(&job.id, None).unwrap().unwrap();
        assert_eq!(
            task.ttl_ms, None,
            "queue time is intentionally outside execution timeout, so a working task has no creation-relative expiry"
        );
    }

    #[test]
    fn terminal_task_ttl_matches_terminal_retention_from_creation() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(120).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary::default(),
                StreamSummary::default(),
            ),
        );
        let task = manager.expose_native_task(&job.id, None).unwrap().unwrap();
        let ttl = task.ttl_ms.unwrap();
        assert!(ttl >= 60_000, "{ttl}");
        assert!(
            ttl < 61_000,
            "terminal TTL must not retain the old execution timeout: {ttl}"
        );
    }

    #[tokio::test]
    async fn new_session_reaps_only_unlocked_stale_siblings() {
        let first = JobManager::new(Duration::from_secs(60)).unwrap();
        let first_job = first.create_for_test(30).unwrap();
        let first_root = first.protected_path().to_path_buf();
        let parent = first_root.parent().unwrap().to_path_buf();

        let stale = parent.join(format!(
            "{SESSION_PREFIX}stale-test-{}",
            Uuid::new_v4().simple()
        ));
        crate::private_state::prepare_owned_dir(&stale).unwrap();
        let stale_lock = stale.join(SESSION_LOCK_FILE);
        create_private_file(&stale_lock).unwrap();
        fs::write(stale.join("orphan.stdout"), b"orphan").unwrap();

        let second = JobManager::new(Duration::from_secs(60)).unwrap();
        let second_job = second.create_for_test(30).unwrap();
        assert!(
            first_root.exists(),
            "a live sibling session lock must protect its spool root"
        );
        assert!(
            !stale.exists(),
            "an unlocked crash-leftover session must be reaped on next use"
        );
        assert!(second.protected_path().exists());

        first.cancel(&first_job.id).unwrap();
        second.cancel(&second_job.id).unwrap();
        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn spool_directory_is_lazy_and_shutdown_is_deterministic() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let root = manager.protected_path().to_path_buf();
        assert!(
            !root.exists(),
            "idle runtimes must not create job spool state"
        );

        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                None,
                |_job| async { std::future::pending::<JobCompletion>().await },
            )
            .unwrap();
        assert!(root.exists());
        assert!(manager.status(&started.id).is_ok());
        manager.shutdown().await.unwrap();
        assert!(
            !root.exists(),
            "normal shutdown must synchronously remove the session root"
        );
        assert!(
            manager.status(&started.id).is_err(),
            "shutdown must clear task metadata"
        );
        assert!(
            manager.create_for_test(30).is_err(),
            "a closed process job session must never be resurrected"
        );
    }

    #[test]
    fn output_is_released_only_after_both_streams_are_consumed() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        fs::write(&job.stdout_path, b"hello").unwrap();
        fs::write(&job.stderr_path, b"err").unwrap();
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary {
                    total_bytes: 5,
                    stored_bytes: 5,
                    truncated: false,
                },
                StreamSummary {
                    total_bytes: 3,
                    stored_bytes: 3,
                    truncated: false,
                },
            ),
        );
        let first = manager
            .output_page(&job.id, JobStream::Stdout, 0, 100, 4096, false)
            .unwrap();
        assert!(first.contains("hello"));
        assert!(job.stdout_path.exists());
        manager
            .output_page(&job.id, JobStream::Stderr, 0, 100, 4096, false)
            .unwrap();
        // Reading is pure/replayable. Only explicit acknowledgement advances
        // cleanup watermarks.
        assert!(job.stdout_path.exists() && job.stderr_path.exists());
        manager
            .acknowledge_output(&job.id, JobStream::Stdout, 5)
            .unwrap();
        let released: serde_json::Value = serde_json::from_str(
            &manager
                .acknowledge_output(&job.id, JobStream::Stderr, 3)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(released["stdout"]["acknowledged_bytes"], 5);
        assert_eq!(released["stderr"]["acknowledged_bytes"], 3);
        assert!(!job.stdout_path.exists() && !job.stderr_path.exists());
        assert!(
            manager
                .status(&job.id)
                .unwrap()
                .contains("\"released\":true")
        );
    }

    #[test]
    fn output_consumed_while_running_advances_cleanup_watermark() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        fs::write(&job.stdout_path, b"one").unwrap();
        let first: serde_json::Value = serde_json::from_str(
            &manager
                .output_page(&job.id, JobStream::Stdout, 0, 64, 4096, false)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first["data"], "one");
        assert_eq!(first["complete"], false);
        manager
            .acknowledge_output(&job.id, JobStream::Stdout, 3)
            .unwrap();

        use std::io::Write as _;
        let mut out = OpenOptions::new()
            .append(true)
            .open(&job.stdout_path)
            .unwrap();
        out.write_all(b"two").unwrap();
        out.flush().unwrap();
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary {
                    total_bytes: 6,
                    stored_bytes: 6,
                    truncated: false,
                },
                StreamSummary::default(),
            ),
        );
        assert!(
            job.stdout_path.exists(),
            "unread tail must retain the spool"
        );
        let tail: serde_json::Value = serde_json::from_str(
            &manager
                .output_page(&job.id, JobStream::Stdout, 3, 64, 4096, false)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(tail["data"], "two");
        assert_eq!(tail["complete"], true);
        assert!(
            job.stdout_path.exists(),
            "final page is not acknowledged yet"
        );
        let ack: serde_json::Value = serde_json::from_str(
            &manager
                .acknowledge_output(&job.id, JobStream::Stdout, 6)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(ack["stdout"]["acknowledged_bytes"], 6);
        assert!(!job.stdout_path.exists() && !job.stderr_path.exists());
    }

    #[test]
    fn sequential_text_pages_do_not_split_valid_utf8() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let text = "a🙂b";
        fs::write(&job.stdout_path, text.as_bytes()).unwrap();
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary {
                    total_bytes: text.len() as u64,
                    stored_bytes: text.len() as u64,
                    truncated: false,
                },
                StreamSummary::default(),
            ),
        );
        let first: serde_json::Value = serde_json::from_str(
            &manager
                .output_page(&job.id, JobStream::Stdout, 0, 2, 4096, false)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first["data"], "a");
        assert_eq!(first["next_offset"], 1);
        let second: serde_json::Value = serde_json::from_str(
            &manager
                .output_page(&job.id, JobStream::Stdout, 1, 8, 4096, false)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(second["data"], "🙂b");
        assert_eq!(second["complete"], true);
    }

    #[test]
    fn lost_job_output_response_is_replayable_without_advancing_cleanup() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        fs::write(&job.stdout_path, b"hello").unwrap();
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary {
                    total_bytes: 5,
                    stored_bytes: 5,
                    truncated: false,
                },
                StreamSummary::default(),
            ),
        );
        let first = manager
            .output_page(&job.id, JobStream::Stdout, 0, 100, 4096, false)
            .unwrap();
        let replay = manager
            .output_page(&job.id, JobStream::Stdout, 0, 100, 4096, false)
            .unwrap();
        assert_eq!(
            first, replay,
            "retrying an unacknowledged page must be stable"
        );
        let status: serde_json::Value =
            serde_json::from_str(&manager.status(&job.id).unwrap()).unwrap();
        assert_eq!(status["stdout"]["acknowledged_bytes"], 0);
        assert!(job.stdout_path.exists());

        let ack: serde_json::Value = serde_json::from_str(
            &manager
                .acknowledge_output(&job.id, JobStream::Stdout, 5)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(ack["stdout"]["acknowledged_bytes"], 5);
        assert!(!job.stdout_path.exists());
    }

    #[test]
    fn fast_serialized_length_matches_serde_for_escaped_unicode_text() {
        let samples = [
            "",
            "plain ascii",
            "quote: \" and slash: \\",
            "line\nfeed\ttab\rcarriage",
            "control \u{0001} \u{001f}",
            "emoji 🙂 café 한글",
        ];
        for sample in samples {
            let result = CallToolResult::success(vec![ContentBlock::text(sample.to_owned())]);
            let expected = serde_json::to_vec(&result).unwrap().len();
            assert_eq!(
                serialized_tool_result_len(sample).unwrap(),
                expected,
                "{sample:?}"
            );
        }
    }

    #[test]
    fn double_escape_cost_matches_rendered_job_page_data_delta() {
        let samples = [
            "abc",
            "\"\\",
            "a\nb\tc\r",
            "\u{0001}x\u{001f}",
            "🙂 café 한글",
        ];
        for sample in samples {
            let render = OutputPageRender {
                id: "job",
                stream: JobStream::Stdout,
                offset: 0,
                stored_bytes: sample.len() as u64,
                total_bytes: Some(sample.len() as u64),
                truncated: false,
                released: false,
                base64: false,
            };
            let empty = render.render(sample.len() as u64, false, &[]).unwrap();
            let page = render
                .render(sample.len() as u64, false, sample.as_bytes())
                .unwrap();
            let fixed = serialized_tool_result_len(&empty).unwrap();
            let full = serialized_tool_result_len(&page).unwrap();
            assert_eq!(full - fixed, double_json_data_len(sample), "{sample:?}");
        }
    }

    #[test]
    fn job_output_page_reserves_response_envelope_space() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let data = b"\\\"".repeat(4096);
        fs::write(&job.stdout_path, &data).unwrap();
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary {
                    total_bytes: data.len() as u64,
                    stored_bytes: data.len() as u64,
                    truncated: false,
                },
                StreamSummary::default(),
            ),
        );
        let ceiling = 4096;
        let payload_ceiling = job_output_payload_ceiling(ceiling).unwrap();
        let page = manager
            .output_page(&job.id, JobStream::Stdout, 0, usize::MAX, ceiling, false)
            .unwrap();
        let serialized = serialized_tool_result_len(&page).unwrap();
        assert!(
            serialized <= payload_ceiling,
            "serialized MCP result {serialized} > {payload_ceiling}"
        );
        let value: serde_json::Value = serde_json::from_str(&page).unwrap();
        assert!(value["next_offset"].as_u64().unwrap() < data.len() as u64);
    }

    #[test]
    fn expired_unread_output_and_manager_drop_both_remove_spools() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let root = job.stdout_path.parent().unwrap().to_path_buf();
        fs::write(&job.stdout_path, b"unread").unwrap();
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary {
                    total_bytes: 6,
                    stored_bytes: 6,
                    truncated: false,
                },
                StreamSummary::default(),
            ),
        );
        manager.set_limits(Duration::ZERO, DEFAULT_JOB_SPOOL_BYTES);
        assert!(
            manager.status(&job.id).is_err(),
            "expired metadata must be evicted"
        );
        assert!(!job.stdout_path.exists() && !job.stderr_path.exists());
        assert!(
            root.exists(),
            "the live manager keeps its session directory"
        );
        drop(manager);
        assert!(
            !root.exists(),
            "manager drop must remove the session directory"
        );
    }

    #[tokio::test]
    async fn retained_job_capacity_never_evicts_unread_output_early() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let mut first_stdout = None;
        for index in 0..MAX_RETAINED_JOBS {
            let job = manager.create_for_test(30).unwrap();
            fs::write(&job.stdout_path, b"x").unwrap();
            if index == 0 {
                first_stdout = Some(job.stdout_path.clone());
            }
            manager.finish(
                &job.id,
                JobCompletion::completed(
                    Some(0),
                    true,
                    StreamSummary {
                        total_bytes: 1,
                        stored_bytes: 1,
                        truncated: false,
                    },
                    StreamSummary::default(),
                ),
            );
            manager.expose_compatibility(&job.id).unwrap();
        }
        let first_stdout = first_stdout.unwrap();
        let error = manager.create_for_test(30).unwrap_err().to_string();
        assert!(
            error.contains("too many retained shell jobs"),
            "capacity pressure must reject instead of deleting unread output: {error}"
        );
        assert!(
            first_stdout.exists(),
            "unread terminal output must survive until acknowledgement/retention"
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn released_terminal_metadata_is_reclaimable_at_capacity() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        for _ in 0..MAX_RETAINED_JOBS {
            let job = manager.create_for_test(30).unwrap();
            manager.finish(
                &job.id,
                JobCompletion::completed(
                    Some(0),
                    true,
                    StreamSummary::default(),
                    StreamSummary::default(),
                ),
            );
        }
        // Empty terminal streams are immediately released, so their metadata
        // may be reclaimed before TTL without losing user-visible output.
        let extra = manager.create_for_test(30).unwrap();
        assert!(manager.status(&extra.id).is_ok());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn acknowledged_exposed_compatibility_jobs_are_reclaimable_at_capacity() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        for _ in 0..MAX_RETAINED_JOBS {
            let job = manager.create_for_test(30).unwrap();
            fs::write(&job.stdout_path, b"x").unwrap();
            manager.finish(
                &job.id,
                JobCompletion::completed(
                    Some(0),
                    true,
                    StreamSummary {
                        total_bytes: 1,
                        stored_bytes: 1,
                        truncated: false,
                    },
                    StreamSummary::default(),
                ),
            );
            manager.expose_compatibility(&job.id).unwrap();
            manager
                .acknowledge_output(&job.id, JobStream::Stdout, 1)
                .unwrap();
            manager
                .acknowledge_output(&job.id, JobStream::Stderr, 0)
                .unwrap();
        }
        let extra = manager.create_for_test(30).unwrap();
        assert!(manager.status(&extra.id).is_ok());
        manager.shutdown().await.unwrap();
    }

    #[test]
    fn failed_ack_cleanup_remains_pending_and_retries_successfully() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        fs::write(&job.stdout_path, b"x").unwrap();
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary {
                    total_bytes: 1,
                    stored_bytes: 1,
                    truncated: false,
                },
                StreamSummary::default(),
            ),
        );
        manager.expose_compatibility(&job.id).unwrap();

        // Replace stderr spool with a directory so remove_file fails after
        // stdout cleanup has succeeded. The entry must remain pending/retryable.
        fs::remove_file(&job.stderr_path).unwrap();
        fs::create_dir(&job.stderr_path).unwrap();
        manager
            .acknowledge_output(&job.id, JobStream::Stdout, 1)
            .unwrap();
        let status = manager
            .acknowledge_output(&job.id, JobStream::Stderr, 0)
            .unwrap();
        let status: serde_json::Value = serde_json::from_str(&status).unwrap();
        assert_eq!(status["stdout"]["released"], false);
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&job.id];
            assert!(entry.output_cleanup_pending);
            assert!(!entry.output_released);
        }

        fs::remove_dir(&job.stderr_path).unwrap();
        // Missing file is already cleaned; the sweeper retry finalizes state.
        sweep_expired(&manager.inner);
        let status: serde_json::Value =
            serde_json::from_str(&manager.status(&job.id).unwrap()).unwrap();
        assert_eq!(status["stdout"]["released"], true);
        assert_eq!(status["stderr"]["released"], true);
    }

    #[test]
    fn cleanup_helpers_report_failure_instead_of_marking_output_released() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        // A directory cannot be removed with remove_file or truncated as a
        // regular spool. This deterministically exercises the retryable-failure
        // contract without relying on platform-specific sharing violations.
        let bad = tempfile::tempdir().unwrap();
        assert!(!remove_file_releasing(&manager.inner, bad.path()));
        assert!(!truncate_file_releasing(&manager.inner, bad.path(), 0));
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary::default(),
                StreamSummary::default(),
            ),
        );
    }

    #[tokio::test]
    async fn synchronous_request_replays_from_spools_until_delivery_then_compacts() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("request-key".to_owned()),
                move |job| async move {
                    std::fs::write(&job.stdout_path, b"exact-output").unwrap();
                    JobCompletion::completed(
                        Some(0),
                        true,
                        StreamSummary {
                            total_bytes: 12,
                            stored_bytes: 12,
                            truncated: false,
                        },
                        StreamSummary::default(),
                    )
                    .with_context(JobResultContext {
                        dir: ".".to_owned(),
                        sandboxed: true,
                        network: false,
                        timeout_secs: 30,
                    })
                },
            )
            .unwrap();
        assert!(matches!(
            manager.wait_until(&started.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        let first = manager.tool_result(&started.id, None).unwrap().unwrap();
        manager.mark_synchronous_result(&started.id).unwrap();
        assert!(
            manager
                .inner
                .state
                .lock()
                .unwrap()
                .request_jobs
                .contains_key("request-key")
        );
        assert!(
            manager.inner.state.lock().unwrap().jobs[&started.id]
                .stdout_path
                .exists()
        );

        let duplicate = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("request-key".to_owned()),
                |_job| async { panic!("redelivery must not launch another runner") },
            )
            .unwrap();
        assert!(!duplicate.is_new);
        assert_eq!(duplicate.id, started.id);
        assert!(matches!(
            manager.wait_until(&duplicate.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        let replay = manager.tool_result(&duplicate.id, None).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(replay).unwrap(),
            serde_json::to_value(first).unwrap()
        );

        manager.acknowledge_request_delivery("request-key");
        let state = manager.inner.state.lock().unwrap();
        let entry = &state.jobs[&started.id];
        assert!(entry.request_key.is_none());
        assert!(entry.output_released);
        assert!(!state.request_jobs.contains_key("request-key"));
        assert!(!entry.stdout_path.exists() && !entry.stderr_path.exists());
    }

    #[tokio::test]
    async fn delivered_fast_requests_do_not_exhaust_retained_job_capacity() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        for index in 0..(MAX_RETAINED_JOBS + 32) {
            let key = format!("request-{index}");
            let started = manager
                .spawn(
                    30,
                    crate::limits::DEFAULT_TRANSFER_BYTES,
                    Some(key.clone()),
                    |_job| async {
                        JobCompletion::completed(
                            Some(0),
                            true,
                            StreamSummary::default(),
                            StreamSummary::default(),
                        )
                    },
                )
                .unwrap();
            let _ = manager.wait_until(&started.id, None).await.unwrap();
            manager.mark_synchronous_result(&started.id).unwrap();
            manager.acknowledge_request_delivery(&key);
        }
        assert!(manager.inner.state.lock().unwrap().jobs.len() <= MAX_RETAINED_JOBS);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn request_key_deduplicates_runner_creation_and_list_recovers_exposed_job() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let first_starts = starts.clone();
        let first = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("request-1".to_owned()),
                move |_job| async move {
                    first_starts.fetch_add(1, AtomicOrdering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    JobCompletion::completed(
                        Some(0),
                        true,
                        StreamSummary::default(),
                        StreamSummary::default(),
                    )
                },
            )
            .unwrap();
        assert!(first.is_new);

        let duplicate_starts = starts.clone();
        let duplicate = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("request-1".to_owned()),
                move |_job| async move {
                    duplicate_starts.fetch_add(100, AtomicOrdering::SeqCst);
                    JobCompletion::failed("duplicate runner must never execute")
                },
            )
            .unwrap();
        assert!(!duplicate.is_new);
        assert_eq!(duplicate.id, first.id);
        manager.expose_compatibility(&first.id).unwrap();
        let listed: serde_json::Value = serde_json::from_str(&manager.list(64)).unwrap();
        assert_eq!(listed["jobs"][0]["job_id"], first.id);
        for _ in 0..100 {
            let status: serde_json::Value =
                serde_json::from_str(&manager.status(&first.id).unwrap()).unwrap();
            if status["status"] != "working" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 1);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn spawn_racing_shutdown_never_detaches_a_runner() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        for _ in 0..24 {
            let manager = JobManager::new(Duration::from_secs(60)).unwrap();
            let root = manager.protected_path().to_path_buf();
            let barrier = Arc::new(tokio::sync::Barrier::new(3));
            let side_effect = Arc::new(AtomicBool::new(false));
            let creator_manager = manager.clone();
            let creator_barrier = barrier.clone();
            let creator_effect = side_effect.clone();
            let creator = tokio::spawn(async move {
                creator_barrier.wait().await;
                creator_manager.spawn(
                    30,
                    crate::limits::DEFAULT_TRANSFER_BYTES,
                    None,
                    move |_job| async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        creator_effect.store(true, AtomicOrdering::SeqCst);
                        JobCompletion::completed(
                            Some(0),
                            true,
                            StreamSummary::default(),
                            StreamSummary::default(),
                        )
                    },
                )
            });
            let shutdown_manager = manager.clone();
            let shutdown_barrier = barrier.clone();
            let shutdown = tokio::spawn(async move {
                shutdown_barrier.wait().await;
                shutdown_manager.shutdown().await
            });
            barrier.wait().await;
            let _ = creator.await.unwrap();
            shutdown.await.unwrap().unwrap();
            tokio::time::sleep(Duration::from_millis(130)).await;
            assert!(
                !side_effect.load(AtomicOrdering::SeqCst),
                "a runner escaped session shutdown"
            );
            assert!(
                !root.exists(),
                "a creator waiting on session initialization recreated spool state after shutdown"
            );
            assert!(
                manager
                    .spawn(
                        30,
                        crate::limits::DEFAULT_TRANSFER_BYTES,
                        None,
                        |_job| async {
                            JobCompletion::completed(
                                Some(0),
                                true,
                                StreamSummary::default(),
                                StreamSummary::default(),
                            )
                        },
                    )
                    .is_err()
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wait_until_never_misses_immediate_completion() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        for _ in 0..128 {
            let started = manager
                .spawn(
                    30,
                    crate::limits::DEFAULT_TRANSFER_BYTES,
                    None,
                    |_job| async {
                        JobCompletion::completed(
                            Some(0),
                            true,
                            StreamSummary::default(),
                            StreamSummary::default(),
                        )
                    },
                )
                .unwrap();
            let outcome = tokio::time::timeout(
                Duration::from_secs(1),
                manager.wait_until(&started.id, None),
            )
            .await
            .expect("immediate completion notification was lost")
            .unwrap();
            assert!(matches!(outcome, JobWait::Terminal));
        }
        manager.shutdown().await.unwrap();
    }

    #[test]
    fn aggregate_spool_quota_is_atomic_reclaimable_and_never_overcommits() {
        let manager = JobManager::with_spool_limit(Duration::from_secs(60), 10).unwrap();
        assert_eq!(reserve_spool_bytes(&manager.inner, 8), 8);
        assert_eq!(reserve_spool_bytes(&manager.inner, 8), 2);
        assert_eq!(reserve_spool_bytes(&manager.inner, 1), 0);
        assert_eq!(manager.inner.spool_used_bytes.load(Ordering::Relaxed), 10);
        release_spool_bytes(&manager.inner, 6);
        assert_eq!(reserve_spool_bytes(&manager.inner, 5), 5);
        assert_eq!(manager.inner.spool_used_bytes.load(Ordering::Relaxed), 9);
    }

    #[test]
    fn cleanup_waits_for_active_reader_lease() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        fs::write(&job.stdout_path, b"hello").unwrap();
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary {
                    total_bytes: 5,
                    stored_bytes: 5,
                    truncated: false,
                },
                StreamSummary::default(),
            ),
        );
        {
            let mut state = manager.inner.state.lock().unwrap();
            state.jobs.get_mut(&job.id).unwrap().readers += 1;
        }
        manager
            .acknowledge_output(&job.id, JobStream::Stdout, 5)
            .unwrap();
        assert!(job.stdout_path.exists(), "reader lease must defer deletion");
        release_reader(&manager.inner, &job.id);
        assert!(!job.stdout_path.exists() && !job.stderr_path.exists());
    }

    #[tokio::test]
    async fn request_delivery_waits_for_active_stream_reader_before_deleting_spools() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("request-reader".to_owned()),
                |job| async move {
                    std::fs::write(&job.stdout_path, b"exact").unwrap();
                    JobCompletion::completed(
                        Some(0),
                        true,
                        StreamSummary {
                            total_bytes: 5,
                            stored_bytes: 5,
                            truncated: false,
                        },
                        StreamSummary::default(),
                    )
                },
            )
            .unwrap();
        let _ = manager.wait_until(&started.id, None).await.unwrap();
        let stdout_path = {
            let mut state = manager.inner.state.lock().unwrap();
            let entry = state.jobs.get_mut(&started.id).unwrap();
            entry.readers += 1;
            entry.stdout_path.clone()
        };
        manager.acknowledge_request_delivery("request-reader");
        assert!(
            stdout_path.exists(),
            "active reader must defer spool deletion"
        );
        release_reader(&manager.inner, &started.id);
        assert!(!stdout_path.exists());
    }

    #[test]
    fn job_errors_are_bounded_once_for_every_status_surface() {
        let completion = JobCompletion::failed("x".repeat(MAX_JOB_ERROR_BYTES * 8));
        let error = completion.error.as_deref().unwrap();
        assert!(error.len() <= MAX_JOB_ERROR_BYTES);
        assert!(error.ends_with('…'));

        let internal = JobCompletion::internal_failed(
            "y".repeat(MAX_JOB_ERROR_BYTES * 8),
            StreamSummary::default(),
            StreamSummary::default(),
        );
        assert!(internal.error.as_ref().unwrap().len() <= MAX_JOB_ERROR_BYTES);
        assert!(internal.internal_error.as_ref().unwrap().len() <= MAX_JOB_ERROR_BYTES);
    }

    #[test]
    fn infrastructure_failure_is_a_failed_mcp_task() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        manager.finish(
            &job.id,
            JobCompletion::internal_failed(
                "spool failed",
                StreamSummary::default(),
                StreamSummary::default(),
            ),
        );
        manager.expose_native_task(&job.id, None).unwrap().unwrap();
        let task = manager.detailed_task(&job.id).unwrap();
        assert_eq!(task.status(), TaskStatus::Failed);
        let TaskPayload::Failed { error } = task.payload else {
            panic!("internal job failure must use MCP failed-task payload")
        };
        assert!(
            serde_json::Value::Object(error)
                .to_string()
                .contains("spool failed")
        );
    }

    #[test]
    fn pending_job_files_roll_back_partial_creation() {
        let root = tempfile::tempdir().unwrap();
        let id = "rollback";
        let conflict = root.path().join(format!("{id}.stderr"));
        create_private_file(&conflict).unwrap();
        assert!(PendingJobFiles::create(root.path(), id).is_err());
        assert!(
            !root.path().join(format!("{id}.stdout")).exists(),
            "failed admission must remove files it created before the error"
        );
        assert!(
            conflict.exists(),
            "pre-existing conflicting file is not ours to remove"
        );
    }

    #[test]
    fn policy_fingerprint_is_sticky_for_one_job_session() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        assert!(manager.bind_intent([7; 32]));
        assert!(manager.bind_intent([7; 32]));
        assert!(!manager.bind_intent([8; 32]));
        assert!(manager.bind_policy([1; 32]));
        assert!(manager.bind_policy([1; 32]));
        assert!(!manager.bind_policy([2; 32]));
    }

    #[test]
    fn one_job_session_shares_its_operation_gate() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let first = manager.operation_gate();
        let second = manager.clone().operation_gate();
        assert!(Arc::ptr_eq(&first, &second));
    }
}
