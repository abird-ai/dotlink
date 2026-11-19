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
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use aw_tools::budget::{RawPageBytes, SerializedResultBytes};
use aw_tools::diagnostic::bounded_diagnostic;
use aw_tools::execution_ref::{Handle as SharedHandle, JobRef, NodeCoordinate};
use aw_tools::execution_stop::{
    StopReason as SharedProcessStopReason, StopRecord as SharedProcessStopRecord,
};
use aw_tools::filesystem::{
    batch::{Graph as SharedGraph, NodeTool as SharedNodeTool, Status as SharedStatus},
    footprint::Footprint as SharedFootprint,
    job::{
        JournalError as SharedJournalError, Lifecycle as SharedLifecycle,
        NodeState as SharedNodeState, StopReason as SharedStopReason,
    },
    job_manager::{
        JobManager as SharedJobManager, Limits as SharedJobLimits, NodePlan as SharedNodePlan,
        NodePolicy as SharedNodePolicy, OutputAck as SharedOutputAck,
        OutputOwner as SharedOutputOwner, OutputStream as SharedOutputStream,
        OutputWriter as SharedOutputWriter, RuntimeAdmission as SharedRuntimeAdmission,
        SemanticBudget as SharedSemanticBudget, ShellOutputFiles as SharedShellOutputFiles,
        Snapshot as SharedSnapshot, StartRequest as SharedStartRequest, plan_graph,
    },
    runner::{
        ProcessExit as SharedProcessExit, ProcessOutputMeta as SharedProcessOutputMeta,
        ProcessTerminal as SharedProcessTerminal,
    },
    status::Primary as SharedPrimary,
};
use aw_tools::{
    OperationId,
    receipt::{Receipt, render_handoff, render_plain},
    task_ops::TaskTerminalDetail as SharedTaskTerminalDetail,
    toolset::JobProfile,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::FutureExt as _;
use rmcp::{
    ErrorData as McpError,
    model::{
        CallToolResult, ContentBlock, DetailedTask, JsonObject, Task, TaskPayload, TaskStatus,
    },
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::{
    sync::{Notify, RwLock, mpsc, oneshot},
    task::JoinHandle,
    time::Instant as TokioInstant,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg(test)]
use crate::limits::DEFAULT_JOB_SPOOL_BYTES;
use crate::limits::MAX_JOB_SPOOL_BYTES_PER_STREAM;

const MAX_ACTIVE_JOBS: usize = 32;
pub(crate) const MAX_RETAINED_JOBS: usize = 256;
/// Dotlink's model-facing finite Job defaults. The same values drive the
/// shared Toolset metadata and the runtime guards below.
pub(crate) const DOTLINK_JOB_PROFILE: JobProfile = JobProfile {
    get_tail_default: 512,
    get_tail_max: 1024,
    get_wait_max_ms: 30_000,
    read_page_default_bytes: 256 * 1024,
    read_page_max_bytes: aw_tools::toolset::MAX_JOB_READ_PAGE_BYTES,
    read_wait_max_ms: 30_000,
    list_page_max: 256,
};

pub(crate) fn dotlink_job_profile(max_output_bytes: usize) -> JobProfile {
    let read_page_max_bytes = max_output_bytes.min(DOTLINK_JOB_PROFILE.read_page_max_bytes);
    JobProfile {
        read_page_default_bytes: DOTLINK_JOB_PROFILE
            .read_page_default_bytes
            .min(read_page_max_bytes),
        read_page_max_bytes,
        ..DOTLINK_JOB_PROFILE
    }
}
const SHARED_MAX_RETAINED_JOBS: usize = MAX_RETAINED_JOBS + MAX_REQUEST_REPLAYS;
/// Transport-owned hidden state is separate from ordinary user-visible job
/// retention. Request-key replay mappings plus positive-delivery cleanup-only
/// retries share this bound; the spool quota independently bounds their bytes.
const MAX_REQUEST_REPLAYS: usize = 4_096;
const JOB_POLL_INTERVAL_MS: u64 = 1_000;
const JOB_OUTPUT_RESPONSE_RESERVE: usize = 2 * 1024;
const SESSION_PREFIX: &str = "session-";
const SESSION_LOCK_FILE: &str = ".session.lock";
// A failed shutdown must not be treated as a safe-to-reap crash orphan.
const SESSION_UNRESOLVED_MARKER: &str = ".unresolved-output";
const SESSION_COORDINATION_LOCK: &str = ".sessions.lock";
const SESSION_COORDINATION_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn next_shared_sequence(manager: &SharedJobManager, id: &str) -> Result<u64> {
    manager
        .current_sequence(id)
        .map_err(|error| anyhow!("cannot read shared Job transition sequence: {error:?}"))?
        .checked_add(1)
        .ok_or_else(|| anyhow!("shared Job transition sequence exhausted"))
}
#[cfg(windows)]
const SESSION_REMOVE_RETRIES: usize = 40;
#[cfg(windows)]
const SESSION_REMOVE_RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobStatus {
    Working,
    Completed,
    TimedOut,
    Failed,
    Cancelled,
    Unknown,
}

impl JobStatus {
    fn terminal(self) -> bool {
        self != Self::Working
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Completed => "completed",
            Self::TimedOut => "timed_out",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
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
    /// Physical process exit, independent from product/Job lifecycle status.
    pub(crate) process_exit: SharedProcessExit,
    /// Final factual logical stop record, when a process stop sequence occurred.
    pub(crate) stop_record: Option<SharedProcessStopRecord>,
    /// Product-only provenance for a shared `Cancelled` reason.
    pub(crate) cancel_origin: Option<CancelOrigin>,
    pub(crate) success: bool,
    pub(crate) error: Option<String>,
    pub(crate) stdout: StreamSummary,
    pub(crate) stderr: StreamSummary,
    pub(crate) context: Option<JobResultContext>,
    pub(crate) internal_error: Option<String>,
    /// Host-observed terminal event retained when canonical shared settlement
    /// must conservatively publish `unknown` (for example an unprovable Unix
    /// timeout/cancel stop). This is diagnostic evidence, never proof.
    pub(crate) observed_status: Option<JobStatus>,
    /// Whether the host proved stdout/stderr readers quiesced after process settlement.
    pub(crate) output_complete: bool,
    /// The canonical Start checkpoint committed, but `Command::spawn` itself
    /// definitively failed, so no process/effect was created.
    pub(crate) spawn_failed: bool,
}

impl JobCompletion {
    fn base(
        status: JobStatus,
        success: bool,
        error: Option<String>,
        stdout: StreamSummary,
        stderr: StreamSummary,
    ) -> Self {
        Self {
            status,
            process_exit: SharedProcessExit::Unknown,
            stop_record: None,
            cancel_origin: None,
            success,
            error,
            stdout,
            stderr,
            context: None,
            internal_error: None,
            observed_status: None,
            output_complete: true,
            spawn_failed: false,
        }
    }

    /// Compatibility constructor for tests and legacy non-process callers that
    /// only have an exit code. Real process execution uses completed_with_exit
    /// so Unix signal identity is never collapsed into None.
    #[cfg(test)]
    pub(crate) fn completed(
        exit_code: Option<i32>,
        success: bool,
        stdout: StreamSummary,
        stderr: StreamSummary,
    ) -> Self {
        let exit = match exit_code {
            Some(code) => SharedProcessExit::Code(code),
            None if success => SharedProcessExit::Code(0),
            None => SharedProcessExit::Unknown,
        };
        Self::completed_with_exit(exit, success, stdout, stderr)
    }

    pub(crate) fn completed_with_exit(
        process_exit: SharedProcessExit,
        success: bool,
        stdout: StreamSummary,
        stderr: StreamSummary,
    ) -> Self {
        let mut completion = Self::base(JobStatus::Completed, success, None, stdout, stderr);
        completion.process_exit = process_exit;
        completion
    }

    pub(crate) fn timed_out(stdout: StreamSummary, stderr: StreamSummary) -> Self {
        Self::base(
            JobStatus::TimedOut,
            false,
            Some("shell job reached its requested timeout".to_owned()),
            stdout,
            stderr,
        )
    }

    pub(crate) fn cancelled(stdout: StreamSummary, stderr: StreamSummary) -> Self {
        Self::base(
            JobStatus::Cancelled,
            false,
            Some("shell job was cancelled".to_owned()),
            stdout,
            stderr,
        )
    }

    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Self::failed_with_output(message, StreamSummary::default(), StreamSummary::default())
    }

    pub(crate) fn unknown_with_output(
        message: impl Into<String>,
        stdout: StreamSummary,
        stderr: StreamSummary,
    ) -> Self {
        Self::base(
            JobStatus::Unknown,
            false,
            Some(bounded_diagnostic(message.into())),
            stdout,
            stderr,
        )
    }

    pub(crate) fn failed_with_output(
        message: impl Into<String>,
        stdout: StreamSummary,
        stderr: StreamSummary,
    ) -> Self {
        Self::base(
            JobStatus::Failed,
            false,
            Some(bounded_diagnostic(message.into())),
            stdout,
            stderr,
        )
    }

    pub(crate) fn spawn_failed(message: impl Into<String>) -> Self {
        let mut completion = Self::failed(message);
        completion.spawn_failed = true;
        completion
    }

    pub(crate) fn internal_failed(
        message: impl Into<String>,
        stdout: StreamSummary,
        stderr: StreamSummary,
    ) -> Self {
        let message = bounded_diagnostic(message.into());
        let mut completion = Self::base(
            JobStatus::Failed,
            false,
            Some(message.clone()),
            stdout,
            stderr,
        );
        completion.internal_error = Some(message);
        completion.output_complete = false;
        completion
    }

    pub(crate) fn with_context(mut self, context: JobResultContext) -> Self {
        self.context = Some(context);
        self
    }

    pub(crate) fn with_incomplete_output(mut self) -> Self {
        self.output_complete = false;
        self
    }

    pub(crate) fn with_observed_status(mut self, status: JobStatus) -> Self {
        self.observed_status = Some(status);
        self
    }

    pub(crate) fn with_process_facts(
        mut self,
        process_exit: SharedProcessExit,
        stop_record: Option<SharedProcessStopRecord>,
    ) -> Self {
        self.process_exit = process_exit;
        self.stop_record = stop_record;
        self
    }

    pub(crate) fn with_cancel_origin(mut self, origin: CancelOrigin) -> Self {
        self.cancel_origin = Some(origin);
        self
    }

    pub(crate) const fn exit_code(&self) -> Option<i32> {
        match self.process_exit {
            SharedProcessExit::Code(code) => Some(code),
            SharedProcessExit::Signal(_) | SharedProcessExit::Unknown => None,
        }
    }

    pub(crate) const fn signal(&self) -> Option<i32> {
        match self.process_exit {
            SharedProcessExit::Signal(signal) => Some(signal),
            SharedProcessExit::Code(_) | SharedProcessExit::Unknown => None,
        }
    }

    pub(crate) const fn terminal_stop_reason(&self) -> Option<SharedProcessStopReason> {
        match self.stop_record {
            Some(record) => record.terminal_reason(),
            None => None,
        }
    }

    pub(crate) fn from_shared_graph(snapshot: &SharedSnapshot) -> Self {
        let uncertain = snapshot.journal_unknown
            || matches!(
                snapshot.summary,
                Some((SharedPrimary::Unknown, _, _)) | None
            );
        let (status, success, error) = if uncertain {
            (
                JobStatus::Unknown,
                false,
                Some(
                    "Graph terminal outcome is uncertain; inspect node results before retrying"
                        .to_owned(),
                ),
            )
        } else {
            match snapshot.stop_reason {
                Some(SharedStopReason::Cancelled) => (
                    JobStatus::Cancelled,
                    false,
                    Some("Graph execution was cancelled".to_owned()),
                ),
                Some(SharedStopReason::Revoked) => (
                    JobStatus::Failed,
                    false,
                    Some("Graph execution authority was revoked".to_owned()),
                ),
                None => match snapshot.summary.expect("uncertain handled above") {
                    (SharedPrimary::Ok | SharedPrimary::Unchanged, _, _) => {
                        (JobStatus::Completed, true, None)
                    }
                    (SharedPrimary::Fail | SharedPrimary::Mixed, _, _) => (
                        JobStatus::Failed,
                        false,
                        Some("Graph completed with failed nodes".to_owned()),
                    ),
                    (SharedPrimary::Skip, _, _) => (
                        JobStatus::Failed,
                        false,
                        Some("Graph completed without launching a successful node".to_owned()),
                    ),
                    (SharedPrimary::Unknown, _, _) => unreachable!("uncertain handled above"),
                },
            }
        };
        Self::base(
            status,
            success,
            error,
            StreamSummary::default(),
            StreamSummary::default(),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetentionClass {
    /// Active jobs and state intentionally visible through the job/task API.
    Ordinary,
    /// Hidden request-key state owned only by transport replay/deduplication.
    /// This class does not consume an ordinary retained-job slot and may be
    /// running or terminal; the request key and spool quotas bound it instead.
    TransportOnly,
    /// Delivery is confirmed; only retryable spool cleanup remains.
    CleanupOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestDeliveryState {
    /// The execution exists, but no successful wire representation has been committed yet.
    Pending,
    /// The exact synchronous shell result is the committed response.
    Synchronous,
    /// A compatibility receipt or native Task is committed and still depends on this job session.
    Durable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestDeliveryAck {
    Acknowledged,
    Pending,
    Missing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestRetirement {
    Retired,
    Busy,
    Missing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JobDisposition {
    /// No response representation has been committed yet. A redelivery may
    /// still choose synchronous replay or one durable exposure form.
    Pending,
    /// The exact ordinary shell result is the committed response.
    Synchronous,
    /// The compatibility job receipt is the committed response.
    Compatibility,
    /// A native MCP Task is the committed response.
    NativeTask,
}

impl JobDisposition {
    fn exposed(self) -> bool {
        matches!(self, Self::Compatibility | Self::NativeTask)
    }

    fn native_task(self) -> bool {
        self == Self::NativeTask
    }
}

#[derive(Debug)]
pub(crate) enum JobHandoff {
    Synchronous,
    Compatibility(CallToolResult),
    NativeTask(Task),
}

/// Product-only cancellation provenance. AW owns the semantic process reason;
/// dotlink keeps who asked so the same shared `Cancelled` fact can render
/// `cancel` for the caller or `shutdown` for connector teardown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CancelOrigin {
    Caller,
    Connector,
}

#[derive(Clone, Debug)]
pub(crate) struct JobCancel {
    token: CancellationToken,
    connector: Arc<AtomicBool>,
}

impl JobCancel {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            connector: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn origin(&self) -> CancelOrigin {
        if self.connector.load(Ordering::Acquire) {
            CancelOrigin::Connector
        } else {
            CancelOrigin::Caller
        }
    }

    pub(crate) fn cancelled(&self) -> impl Future<Output = ()> + '_ {
        self.token.cancelled()
    }

    pub(crate) fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub(crate) fn cancel_as(&self, origin: CancelOrigin) {
        self.connector
            .store(origin == CancelOrigin::Connector, Ordering::Release);
        self.token.cancel();
    }

    pub(crate) fn cancel(&self) {
        self.cancel_as(CancelOrigin::Caller);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

struct SharedJobLease {
    manager: Arc<SharedJobManager>,
    id: String,
}

impl fmt::Debug for SharedJobLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedJobLease")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for SharedJobLease {
    fn drop(&mut self) {
        match self.manager.expire_terminal(&self.id) {
            Ok(()) | Err(aw_tools::filesystem::job_manager::Error::NotFound) => {}
            Err(error) => tracing::warn!(
                id = %self.id,
                ?error,
                "shared Job projection outlived dotlink host retention entry"
            ),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ProcessInput {
    Text(Vec<u8>),
    Eof,
}

pub(crate) struct ProcessInputRequest {
    pub(crate) input: ProcessInput,
    pub(crate) response: oneshot::Sender<std::result::Result<(), String>>,
}

struct JobEntry {
    id: String,
    created: Instant,
    created_at: String,
    updated_at: String,
    timeout_secs: u64,
    result_limit: usize,
    phase: JobPhase,
    cancel: JobCancel,
    runner: Option<JoinHandle<()>>,
    completion: Option<JobCompletion>,
    /// First successful materialization time of the immutable native MCP Task
    /// result. Exact Task replay reuses this timestamp.
    task_result_ts: Option<u64>,
    terminal_at: Option<Instant>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    drain_writers: Arc<DrainWriters>,
    legacy_spool: Option<Arc<LegacySpoolAccount>>,
    // Legacy/test-only stream mirrors. Shared production entries derive all
    // retained/ACK/released state directly from SharedJobManager.
    stdout_retained: u64,
    stderr_retained: u64,
    stdout_consumed: u64,
    stderr_consumed: u64,
    /// The user-facing job stream is no longer pageable after its Job owner is
    /// fully acknowledged. A native Task may still retain the same immutable
    /// shared spool under its separate owner until Task/job retention expires.
    stdout_released: bool,
    stderr_released: bool,
    readers: usize,
    output_cleanup_requested: bool,
    output_cleanup_pending: bool,
    expiration_pending: bool,
    disposition: JobDisposition,
    /// Live callers currently deciding the response representation. This is
    /// orthogonal to `disposition`: transport-only replay may remain Pending
    /// after every active attempt has gone away.
    response_attempts: usize,
    retention: RetentionClass,
    request_key: Option<String>,
    /// Exact live Linux sandbox guard generation used to prepare this command.
    /// None for unsandboxed/non-Linux commands and pre-execution failures.
    shell_guard_generation: Option<[u8; 32]>,
    /// Runner-owned live stdin command channels keyed by exact Process/Graph
    /// coordinate. The ChildStdin itself never leaves the runner.
    process_inputs: HashMap<usize, mpsc::Sender<ProcessInputRequest>>,
    /// Legacy/test-only delivery fence for Jobs without a shared output owner.
    /// Shared Jobs keep exact delivery proof in aw-tools.
    delivered_through: HashMap<(usize, JobOutputStream), u64>,
    /// RAII owner of the canonical shared lifecycle projection.
    shared_job: Option<SharedJobLease>,
    /// Serializes only sequence allocation + canonical shared lifecycle
    /// transitions for this Job. Effects, output I/O and unrelated Jobs remain
    /// fully concurrent.
    shared_transition_gate: Arc<Mutex<()>>,
    /// Multi-node/native Graph runner already owns every shared node transition.
    /// The legacy local finisher must not settle node 1 again.
    shared_graph_authoritative: bool,
    /// When user-visible retention began. For request-key responses this is
    /// rebased on positive delivery acknowledgement; execution completion stays
    /// independently represented by `terminal_at`.
    retention_anchor: Option<Instant>,
    /// Immutable standalone list-exposure sequence. Assigned exactly once when
    /// this execution first becomes user-visible; lifecycle activity never
    /// changes it.
    exposure_sequence: Option<u64>,
    /// Monotonic public compare-and-act/observation token. Canonical lifecycle
    /// and output truth stay in SharedJobManager; this projection also advances
    /// for accepted Process input/control events.
    public_revision: u64,
    notify: Arc<Notify>,
}

impl JobEntry {
    fn status(&self) -> JobStatus {
        self.completion
            .as_ref()
            .map_or(JobStatus::Working, |completion| completion.status)
    }

    fn all_output_released(&self) -> bool {
        if self.shared_graph_authoritative {
            return authoritative_graph_outputs_released(self);
        }
        [JobStream::Stdout, JobStream::Stderr]
            .into_iter()
            .all(|stream| job_owner_projection(self, stream).is_ok_and(|state| state.released))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RemoteStartReceipt {
    Mcp(CallToolResult),
    JobMcp {
        reference: JobRef,
        result: CallToolResult,
    },
}

#[derive(Clone, Debug)]
enum RemoteStartDisposition {
    Claimed,
    Ready(RemoteStartReceipt),
    Unknown,
}

#[derive(Clone, Debug)]
struct RemoteStartEntry {
    semantic_hash: [u8; 32],
    expires_at_unix_ms: u64,
    disposition: RemoteStartDisposition,
}

#[derive(Default)]
struct JobState {
    closed: bool,
    jobs: HashMap<String, JobEntry>,
    request_jobs: HashMap<String, String>,
    remote_starts: HashMap<[u8; 32], RemoteStartEntry>,
    next_exposure_sequence: u64,
}

impl JobState {
    fn available(&self, id: &str) -> Option<&JobEntry> {
        self.jobs.get(id).filter(|entry| !entry.expiration_pending)
    }

    fn available_mut(&mut self, id: &str) -> Option<&mut JobEntry> {
        self.jobs
            .get_mut(id)
            .filter(|entry| !entry.expiration_pending)
    }
}

pub(crate) enum RemoteStartDecision {
    New(RemoteStartGuard),
    Existing(RemoteStartReceipt),
    Unknown,
}

pub(crate) struct RemoteStartGuard {
    inner: Weak<JobManagerInner>,
    key: [u8; 32],
    settled: bool,
}

impl RemoteStartGuard {
    pub(crate) fn mark_ready(mut self, receipt: RemoteStartReceipt) -> Result<()> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| anyhow!("remote Start owner is unavailable"))?;
        let mut state = inner
            .state
            .lock()
            .map_err(|_| anyhow!("job manager lock poisoned"))?;
        let entry = state
            .remote_starts
            .get_mut(&self.key)
            .ok_or_else(|| anyhow!("remote Start claim disappeared"))?;
        if !matches!(entry.disposition, RemoteStartDisposition::Claimed) {
            bail!("remote Start claim is no longer pending");
        }
        entry.disposition = RemoteStartDisposition::Ready(receipt);
        self.settled = true;
        Ok(())
    }

    pub(crate) fn release_before_effect(mut self) {
        if let Some(inner) = self.inner.upgrade()
            && let Ok(mut state) = inner.state.lock()
            && state
                .remote_starts
                .get(&self.key)
                .is_some_and(|entry| matches!(entry.disposition, RemoteStartDisposition::Claimed))
        {
            state.remote_starts.remove(&self.key);
        }
        self.settled = true;
    }

    pub(crate) fn mark_unknown(mut self) {
        if let Some(inner) = self.inner.upgrade()
            && let Ok(mut state) = inner.state.lock()
            && let Some(entry) = state.remote_starts.get_mut(&self.key)
            && matches!(entry.disposition, RemoteStartDisposition::Claimed)
        {
            entry.disposition = RemoteStartDisposition::Unknown;
        }
        self.settled = true;
    }
}

impl Drop for RemoteStartGuard {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        if let Some(inner) = self.inner.upgrade()
            && let Ok(mut state) = inner.state.lock()
            && let Some(entry) = state.remote_starts.get_mut(&self.key)
            && matches!(entry.disposition, RemoteStartDisposition::Claimed)
        {
            entry.disposition = RemoteStartDisposition::Unknown;
        }
    }
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
    remote_target: Mutex<Option<crate::remote_start::BridgeTarget>>,
    session_fingerprint: [u8; 32],
    shared_jobs: Arc<SharedJobManager>,
}

const SPOOL_WRITERS_SEALED: usize = 1usize << (usize::BITS - 1);
const SPOOL_WRITER_COUNT_MASK: usize = !SPOOL_WRITERS_SEALED;

/// Host-only lifetime fence for async stdout/stderr drain tasks. Byte quota,
/// retained prefixes, paging and acknowledgements belong to shared JobManager.
#[derive(Debug, Default)]
struct DrainWriters {
    writers: AtomicUsize,
    writers_idle: Notify,
}

/// Legacy/test-only local byte account for entries that never attach shared
/// output. Production shared jobs use SharedJobManager as the sole byte owner.
#[derive(Debug)]
struct LegacySpoolAccount {
    inner: Weak<JobManagerInner>,
    charged: AtomicU64,
}

#[derive(Debug)]
pub(crate) struct SpoolWriterGuard {
    drain: Arc<DrainWriters>,
}

impl DrainWriters {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn begin<const N: usize>(self: &Arc<Self>) -> Option<[SpoolWriterGuard; N]> {
        if N == 0 {
            return Some(std::array::from_fn(|_| unreachable!()));
        }
        let mut state = self.writers.load(Ordering::Acquire);
        loop {
            if state & SPOOL_WRITERS_SEALED != 0 {
                return None;
            }
            let count = state & SPOOL_WRITER_COUNT_MASK;
            let next_count = count.checked_add(N)?;
            if next_count > SPOOL_WRITER_COUNT_MASK {
                return None;
            }
            match self.writers.compare_exchange_weak(
                state,
                next_count,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(std::array::from_fn(|_| SpoolWriterGuard {
                        drain: self.clone(),
                    }));
                }
                Err(actual) => state = actual,
            }
        }
    }

    #[cfg(test)]
    fn begin_one(self: &Arc<Self>) -> Option<SpoolWriterGuard> {
        self.begin::<1>().map(|[writer]| writer)
    }

    fn seal(&self) {
        self.writers
            .fetch_or(SPOOL_WRITERS_SEALED, Ordering::AcqRel);
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.writers_idle.notified();
            tokio::pin!(notified);
            // Register before observing the count: `notify_waiters` does not store
            // a permit for a future waiter, so checking first could miss the final
            // writer transition and sleep forever.
            notified.as_mut().enable();
            if self.writers.load(Ordering::Acquire) & SPOOL_WRITER_COUNT_MASK == 0 {
                return;
            }
            notified.await;
        }
    }

    async fn seal_and_wait_idle(&self) {
        self.seal();
        self.wait_idle().await;
    }
}

impl LegacySpoolAccount {
    #[cfg(test)]
    fn new(inner: &Arc<JobManagerInner>) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::downgrade(inner),
            charged: AtomicU64::new(0),
        })
    }

    #[cfg(test)]
    fn reserve(&self, requested: u64) -> u64 {
        let Some(inner) = self.inner.upgrade() else {
            return 0;
        };
        let reserved = reserve_spool_bytes(&inner, requested);
        self.charged.fetch_add(reserved, Ordering::AcqRel);
        reserved
    }

    fn release(&self, requested: u64) {
        if requested == 0 {
            return;
        }
        let mut current = self.charged.load(Ordering::Acquire);
        loop {
            let released = requested.min(current);
            let next = current - released;
            match self.charged.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if let Some(inner) = self.inner.upgrade() {
                        release_spool_bytes(&inner, released);
                    }
                    return;
                }
                Err(actual) => current = actual,
            }
        }
    }

    fn reconcile_to(&self, retained: u64) {
        loop {
            let charged = self.charged.load(Ordering::Acquire);
            if charged <= retained {
                return;
            }
            self.release(charged - retained);
        }
    }

    #[cfg(test)]
    fn charged(&self) -> u64 {
        self.charged.load(Ordering::Relaxed)
    }
}

impl Drop for SpoolWriterGuard {
    fn drop(&mut self) {
        let previous = self.drain.writers.fetch_sub(1, Ordering::AcqRel);
        let previous_count = previous & SPOOL_WRITER_COUNT_MASK;
        debug_assert!(previous_count != 0, "spool writer count underflow");
        if previous_count == 1 {
            self.drain.writers_idle.notify_waiters();
        }
    }
}

impl Drop for LegacySpoolAccount {
    fn drop(&mut self) {
        let residual = self.charged.swap(0, Ordering::AcqRel);
        if residual == 0 {
            return;
        }
        if let Some(inner) = self.inner.upgrade() {
            release_spool_bytes(&inner, residual);
        }
    }
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

#[cfg(not(windows))]
fn remove_session_directory(root: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn remove_session_directory(root: &Path) -> std::io::Result<()> {
    for attempt in 0..=SESSION_REMOVE_RETRIES {
        match fs::remove_dir_all(root) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error)
                if attempt < SESSION_REMOVE_RETRIES
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::PermissionDenied
                            | std::io::ErrorKind::DirectoryNotEmpty
                    ) =>
            {
                // Windows may keep a just-closed file/lock handle temporarily
                // non-deletable. All dotlink-owned runners, spool writers and
                // the session lock are already closed before this helper is
                // called; retry only this bounded delete-on-close window.
                std::thread::sleep(SESSION_REMOVE_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("session removal loop always returns")
}

fn mark_session_unresolved(root: &Path) -> std::io::Result<()> {
    let marker = root.join(SESSION_UNRESOLVED_MARKER);
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&marker)?;
            if !metadata.file_type().is_file() {
                return Err(std::io::Error::other(
                    "unresolved Job marker is not a regular file",
                ));
            }
            OpenOptions::new().read(true).write(true).open(&marker)?
        }
        Err(error) => return Err(error),
    };
    // An existing marker may be the residue of an earlier failed sync. Existence
    // alone is not durability evidence: harden and flush it again before the
    // session lock can be released.
    crate::setup::set_private_file_permissions(&marker).map_err(std::io::Error::other)?;
    file.sync_all()?;
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    Ok(())
}

impl Drop for JobManagerInner {
    fn drop(&mut self) {
        if let Some(handle) = self.sweeper.lock().ok().and_then(|mut value| value.take()) {
            handle.abort();
        }
        // Drop cannot await runners or prove shared-owned output was released.
        // A failed explicit shutdown deliberately retains its evidence; never
        // undo that decision from this last-resort destructor.
        let safe_to_remove = if let Ok(mut state) = self.state.lock() {
            let safe = state.jobs.is_empty() && state.request_jobs.is_empty();
            state.closed = true;
            for entry in state.jobs.values_mut() {
                entry.drain_writers.seal();
                entry.cancel.cancel_as(CancelOrigin::Connector);
                if let Some(handle) = entry.runner.take() {
                    handle.abort();
                }
            }
            state.jobs.clear();
            state.request_jobs.clear();
            safe
        } else {
            false
        };
        if !safe_to_remove && let Err(error) = mark_session_unresolved(&self.root) {
            tracing::error!(%error, path = %self.root.display(), "could not mark unresolved Job evidence for retention");
        }
        if let Ok(mut lock) = self.session_lock.lock() {
            lock.take();
        }
        if !safe_to_remove {
            tracing::warn!(
                path = %self.root.display(),
                "retaining job spool session on drop: Job/output cleanup was not proven safe"
            );
            return;
        }
        let root = self.root.clone();
        if root.exists() {
            match fs::remove_dir_all(&root) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    let _ = std::thread::Builder::new()
                        .name("dotlink-job-cleanup".into())
                        .spawn(move || {
                            if let Err(error) = remove_session_directory(&root) {
                                tracing::warn!(%error, path = %root.display(), "could not remove job spool directory on shutdown");
                            }
                        });
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct JobManager {
    inner: Arc<JobManagerInner>,
}

#[derive(Clone, Debug)]
struct JobFinisher {
    inner: Weak<JobManagerInner>,
}

#[derive(Clone, Debug)]
pub(crate) struct JobHandle {
    pub(crate) id: String,
    pub(crate) cancel: JobCancel,
    pub(crate) stdout_path: PathBuf,
    pub(crate) stderr_path: PathBuf,
    stdout_file: Arc<File>,
    stderr_file: Arc<File>,
    drain_writers: Arc<DrainWriters>,
    shared_transition_gate: Arc<Mutex<()>>,
    #[cfg(test)]
    legacy_spool: Option<Arc<LegacySpoolAccount>>,
    inner: Weak<JobManagerInner>,
}

#[derive(Clone)]
pub(crate) struct SharedShellWriters {
    pub(crate) stdout: SharedOutputWriter,
    pub(crate) stderr: SharedOutputWriter,
}

fn shared_task_output_owner(manager: &SharedJobManager, id: &str) -> Result<SharedOutputOwner> {
    manager.task_result_owner(id, 1).map_err(|error| {
        anyhow!("canonical shared native-task output owner unavailable: {error:?}")
    })
}

fn rollback_shared_claim(inner: &JobManagerInner, id: &str, transition_gate: &Arc<Mutex<()>>) {
    let _transition = match transition_gate.lock() {
        Ok(guard) => guard,
        Err(_) => {
            tracing::warn!(%id, "shared Job transition gate poisoned during rollback");
            return;
        }
    };
    let claimed = match inner.shared_jobs.node_state(id, 1) {
        Ok(SharedNodeState::Claimed { sequence }) => sequence,
        Ok(SharedNodeState::Terminal { .. })
        | Err(aw_tools::filesystem::job_manager::Error::NotFound) => {
            return;
        }
        Ok(state) => {
            tracing::warn!(%id, ?state, "shared shell rollback found unexpected pre-launch state");
            return;
        }
        Err(error) => {
            tracing::warn!(%id, ?error, "could not inspect shared shell claim for rollback");
            return;
        }
    };
    let sequence = match next_shared_sequence(&inner.shared_jobs, id) {
        Ok(sequence) => sequence,
        Err(error) => {
            tracing::warn!(%id, %error, "could not advance shared shell sequence for rollback");
            return;
        }
    };
    match inner.shared_jobs.settle_unlaunched(
        id,
        1,
        claimed,
        sequence,
        |_, _| Ok(()),
        |_, _| Ok::<_, SharedJournalError>(()),
    ) {
        Ok(()) => {
            if let Err(error) = inner.shared_jobs.expire_terminal(id)
                && !matches!(error, aw_tools::filesystem::job_manager::Error::NotFound)
            {
                tracing::warn!(%id, ?error, "could not expire rolled-back shared shell Job");
            }
        }
        Err(error) => {
            tracing::warn!(%id, ?error, "could not roll back claimed shared shell Job");
        }
    }
}

fn rollback_shared_process_attachment(
    inner: &JobManagerInner,
    id: &str,
    transition_gate: &Arc<Mutex<()>>,
    owners: &[SharedOutputOwner],
    stdout: &SharedOutputWriter,
    stderr: &SharedOutputWriter,
) {
    let _ = stdout.finish();
    let _ = stderr.finish();
    for stream in [SharedOutputStream::Stdout, SharedOutputStream::Stderr] {
        for owner in owners {
            let _ = inner
                .shared_jobs
                .acknowledge_output(id, 1, stream, *owner, 0);
        }
    }
    rollback_shared_claim(inner, id, transition_gate);
}

fn reconcile_prelaunch_shared_exposure(
    manager: &SharedJobManager,
    id: &str,
    context: &str,
) -> Result<()> {
    let exposure = manager
        .reserve_exposed(id)
        .and_then(|()| manager.expose(id));
    if let Err(error) = exposure {
        let cleanup = manager.discard_unstarted(id);
        bail!("{context}: {error:?}; cleanup={cleanup:?}");
    }
    Ok(())
}

impl JobHandle {
    pub(crate) fn admit_prepared_graph(
        &self,
        graph: SharedGraph,
        plans: Vec<SharedNodePlan>,
        width: usize,
        shell_guard_generation: Option<[u8; 32]>,
        authoritative: bool,
    ) -> Result<()> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| anyhow!("job session disappeared before Graph admission"))?;
        if plans.len() != graph.steps().len() || plans.is_empty() {
            bail!("prepared Graph plans do not match Graph shape");
        }
        let runtime_session = inner
            .intent_fingerprint
            .lock()
            .map_err(|_| anyhow!("job intent fingerprint lock poisoned"))?
            .unwrap_or(inner.session_fingerprint);
        let runtime_session = hex::encode(runtime_session);
        let _transition = self
            .shared_transition_gate
            .lock()
            .map_err(|_| anyhow!("shared Job transition gate poisoned"))?;
        inner
            .shared_jobs
            .admit_runtime(
                graph,
                plans,
                SharedRuntimeAdmission {
                    id: &self.id,
                    runtime_session: &runtime_session,
                    epoch: 1,
                    admission_sequence: 1,
                    width,
                    exposed: false,
                },
            )
            .map_err(|error| anyhow!("shared Graph admission failed: {error:?}"))?;
        let mut state = inner.state.lock().expect("job manager lock");
        let Some(entry) = state.jobs.get_mut(&self.id) else {
            drop(state);
            let _ = inner.shared_jobs.discard_unstarted(&self.id);
            bail!("job disappeared during shared Graph admission");
        };
        if entry.completion.is_some() || entry.shared_job.is_some() {
            drop(state);
            let _ = inner.shared_jobs.discard_unstarted(&self.id);
            bail!("job is no longer eligible for shared Graph admission");
        }
        // A local response can be abandoned or handed off before this async
        // runner reaches shared Graph admission. In that ordering the local
        // wire disposition is already durable while the shared Job did not yet
        // exist, so the earlier exposure attempt necessarily observed NotFound.
        // Reconcile visibility here, still before claim/process launch, so the
        // canonical JobManager never trails an already-committed host surface.
        if entry.disposition.exposed()
            && let Err(error) = reconcile_prelaunch_shared_exposure(
                &inner.shared_jobs,
                &self.id,
                "shared Graph could not reconcile pre-admission exposure before launch",
            )
        {
            drop(state);
            return Err(error);
        }
        entry.shell_guard_generation = shell_guard_generation;
        entry.shared_graph_authoritative = authoritative;
        entry.shared_job = Some(SharedJobLease {
            manager: inner.shared_jobs.clone(),
            id: self.id.clone(),
        });
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        entry.notify.notify_waiters();
        Ok(())
    }

    pub(crate) fn restart_prepared_graph(
        &self,
        source_id: &str,
        graph: SharedGraph,
        plans: Vec<SharedNodePlan>,
        width: usize,
        shell_guard_generation: Option<[u8; 32]>,
    ) -> Result<()> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| anyhow!("job session disappeared before Graph restart"))?;
        if plans.len() != graph.steps().len() || plans.is_empty() {
            bail!("prepared restart plans do not match Graph shape");
        }
        let runtime_session = inner
            .intent_fingerprint
            .lock()
            .map_err(|_| anyhow!("job intent fingerprint lock poisoned"))?
            .unwrap_or(inner.session_fingerprint);
        let runtime_session = hex::encode(runtime_session);
        let _transition = self
            .shared_transition_gate
            .lock()
            .map_err(|_| anyhow!("shared Job transition gate poisoned"))?;
        inner
            .shared_jobs
            .restart_runtime(
                source_id,
                graph,
                plans,
                SharedRuntimeAdmission {
                    id: &self.id,
                    runtime_session: &runtime_session,
                    epoch: 1,
                    admission_sequence: 1,
                    width,
                    exposed: false,
                },
            )
            .map_err(|error| anyhow!("shared Graph restart admission failed: {error:?}"))?;
        let mut state = inner.state.lock().expect("job manager lock");
        let Some(entry) = state.jobs.get_mut(&self.id) else {
            drop(state);
            let _ = inner.shared_jobs.discard_unstarted(&self.id);
            bail!("restarted job disappeared during shared Graph admission");
        };
        if entry.completion.is_some() || entry.shared_job.is_some() {
            drop(state);
            let _ = inner.shared_jobs.discard_unstarted(&self.id);
            bail!("job is no longer eligible for shared Graph restart");
        }
        if entry.disposition.exposed()
            && let Err(error) = reconcile_prelaunch_shared_exposure(
                &inner.shared_jobs,
                &self.id,
                "shared restarted Graph exposure failed before launch",
            )
        {
            drop(state);
            return Err(error);
        }
        entry.shell_guard_generation = shell_guard_generation;
        entry.shared_graph_authoritative = true;
        entry.shared_job = Some(SharedJobLease {
            manager: inner.shared_jobs.clone(),
            id: self.id.clone(),
        });
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        entry.notify.notify_waiters();
        Ok(())
    }

    pub(crate) fn claim_prepared_shell(
        &self,
        tool: SharedNodeTool,
        arguments: serde_json::Value,
        shell_guard_generation: Option<[u8; 32]>,
        result_limit: usize,
        stream_capacity: u64,
    ) -> Result<SharedShellWriters> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| anyhow!("job session disappeared before shell claim"))?;
        {
            let state = inner
                .state
                .lock()
                .map_err(|_| anyhow!("job manager lock poisoned"))?;
            let entry = state
                .jobs
                .get(&self.id)
                .ok_or_else(|| anyhow!("job disappeared before shared shell claim"))?;
            if entry.result_limit != result_limit {
                bail!("shared shell result limit differs from admitted host Job");
            }
        }
        let authority = inner
            .policy_fingerprint
            .lock()
            .map_err(|_| anyhow!("job policy fingerprint lock poisoned"))?
            .ok_or_else(|| anyhow!("shell execution policy is not bound"))?;
        if authority == [0; 32] {
            bail!("shell execution policy fingerprint is invalid");
        }
        let graph_input = serde_json::json!({"ops":[[tool.name(), arguments]]});
        let graph = SharedGraph::compile_with(
            &graph_input,
            |name| (name == tool.name()).then_some(tool),
            |node, args| node == tool && args.is_object(),
        )
        .map_err(|error| anyhow!("cannot compile shared shell Job: {error:?}"))?;
        let work_bytes = stream_capacity.saturating_mul(2).max(1);
        let plans = plan_graph(&graph, |_, _| {
            Ok::<_, ()>(SharedNodePolicy {
                budget: SharedSemanticBudget {
                    work_bytes,
                    result_bytes: 4 * 1024,
                },
                // A standalone shell Job has no sibling Graph node to order.
                // Keep its shared footprint conservative: `writes` remains an
                // advisory scheduling hint, never authority or correctness input.
                footprint: SharedFootprint::unknown(true, true),
                registry_generation: 1,
                authority_binding: authority,
                runtime_binding: authority,
                // A standalone shell Process can later become the exact native
                // MCP Task for this same immutable execution. Declare that
                // eligibility in the sealed shared plan instead of inventing a
                // host-private output owner after admission.
                task_result_eligible: true,
            })
        })
        .map_err(|error| anyhow!("cannot plan shared shell Job: {error:?}"))?;
        self.admit_prepared_graph(graph, plans, 1, shell_guard_generation, false)?;
        let claim = {
            let _transition = self
                .shared_transition_gate
                .lock()
                .map_err(|_| anyhow!("shared Job transition gate poisoned"))?;
            inner
                .shared_jobs
                .claim(&self.id, 1, true, 2, |_, _| Ok::<_, SharedJournalError>(()))
        };
        if let Err(error) = claim {
            let _ = inner.shared_jobs.discard_unstarted(&self.id);
            return Err(anyhow!("shared shell Job claim failed: {error:?}"));
        }
        let job_owner = inner
            .shared_jobs
            .job_output_owner(&self.id)
            .map_err(|error| anyhow!("cannot derive shared shell output owner: {error:?}"))?;
        let task_owner = shared_task_output_owner(&inner.shared_jobs, &self.id)?;
        let owners = [job_owner, task_owner];
        let stdout_file = self
            .stdout_file
            .try_clone()
            .context("cannot clone exact stdout spool capability")?;
        let stderr_file = match self.stderr_file.try_clone() {
            Ok(file) => file,
            Err(error) => {
                rollback_shared_claim(&inner, &self.id, &self.shared_transition_gate);
                return Err(error).context("cannot clone exact stderr spool capability");
            }
        };
        let (stdout, stderr) = match inner.shared_jobs.attach_process_output_files(
            &self.id,
            1,
            owners,
            SharedShellOutputFiles {
                stdout: stdout_file,
                stderr: stderr_file,
                stream_capacity,
            },
        ) {
            Ok(writers) => writers,
            Err(error) => {
                rollback_shared_claim(&inner, &self.id, &self.shared_transition_gate);
                return Err(anyhow!(
                    "shared process output attachment failed: {error:?}"
                ));
            }
        };
        let state = inner.state.lock().expect("job manager lock");
        let Some(entry) = state.jobs.get(&self.id) else {
            drop(state);
            rollback_shared_process_attachment(
                &inner,
                &self.id,
                &self.shared_transition_gate,
                &owners,
                &stdout,
                &stderr,
            );
            bail!("job disappeared during shared shell claim");
        };
        if entry.completion.is_some() || entry.shared_job.is_none() {
            drop(state);
            rollback_shared_process_attachment(
                &inner,
                &self.id,
                &self.shared_transition_gate,
                &owners,
                &stdout,
                &stderr,
            );
            bail!("job is no longer eligible for shared shell claim");
        }
        drop(state);
        Ok(SharedShellWriters { stdout, stderr })
    }

    pub(crate) fn shared_manager(&self) -> Result<Arc<SharedJobManager>> {
        self.inner
            .upgrade()
            .map(|inner| inner.shared_jobs.clone())
            .ok_or_else(|| anyhow!("job session disappeared"))
    }

    pub(crate) fn with_shared_transition<T>(
        &self,
        transition: impl FnOnce(&SharedJobManager) -> Result<T>,
    ) -> Result<T> {
        let _gate = self
            .shared_transition_gate
            .lock()
            .map_err(|_| anyhow!("shared Job transition gate poisoned"))?;
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| anyhow!("job session disappeared"))?;
        let outcome = transition(&inner.shared_jobs);
        if outcome.is_ok() {
            let mut state = inner.state.lock().expect("job manager lock");
            if let Some(entry) = state.jobs.get_mut(&self.id) {
                entry.public_revision = entry.public_revision.saturating_add(1);
                entry.updated_at = chrono::Utc::now().to_rfc3339();
                entry.notify.notify_waiters();
            }
        }
        outcome
    }

    #[cfg(test)]
    pub(crate) fn mark_launched(&self) -> Result<()> {
        self.mark_node_launched(1)
    }

    /// Commit the canonical Start checkpoint before the OS is allowed to spawn
    /// the process. Every production shell Job must already own a shared claim;
    /// a missing shared entry is split-brain state, never a compatibility path.
    pub(crate) fn mark_node_launched(&self, coordinate: usize) -> Result<()> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| anyhow!("job session disappeared before process launch"))?;
        self.with_shared_transition(|shared| {
            let claimed_sequence = match shared.node_state(&self.id, coordinate) {
                Ok(SharedNodeState::Claimed { sequence }) => sequence,
                Err(aw_tools::filesystem::job_manager::Error::NotFound) => {
                    bail!("shared shell Job is missing before process launch")
                }
                Ok(state) => bail!("shared shell Job has invalid pre-launch state: {state:?}"),
                Err(error) => bail!("cannot inspect shared shell Job before start: {error:?}"),
            };
            let sequence = next_shared_sequence(shared, &self.id)?;
            shared
                .start(
                    &self.id,
                    SharedStartRequest {
                        coordinate,
                        claimed_sequence,
                        source_live: true,
                        launch_authorized: true,
                        sequence,
                    },
                    |_, _| Ok::<_, SharedJournalError>(()),
                )
                .map_err(|error| anyhow!("shared shell start checkpoint failed: {error:?}"))
        })?;

        let mut state = inner.state.lock().expect("job manager lock");
        if state.closed {
            bail!("job session closed before process launch");
        }
        let Some(entry) = state.jobs.get_mut(&self.id) else {
            bail!("job disappeared before process launch");
        };
        if entry.completion.is_some() {
            bail!("job became terminal before process launch");
        }
        entry.phase = JobPhase::Running;
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        entry.notify.notify_waiters();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn settle_unlaunched(&self) {
        self.settle_node_unlaunched(1);
    }

    pub(crate) fn settle_node_unlaunched(&self, coordinate: usize) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        for stream in [SharedOutputStream::Stdout, SharedOutputStream::Stderr] {
            if let Err(error) = inner
                .shared_jobs
                .finish_output(&self.id, coordinate, stream)
            {
                tracing::warn!(id = %self.id, ?error, ?stream, "could not finish unlaunched shared shell output");
            }
        }
        if let Err(error) = self.with_shared_transition(|shared| {
            let SharedNodeState::Claimed { sequence: claimed } = shared
                .node_state(&self.id, coordinate)
                .map_err(|error| anyhow!("cannot inspect shared no-launch state: {error:?}"))?
            else {
                return Ok(());
            };
            let sequence = next_shared_sequence(shared, &self.id)?;
            shared
                .settle_unlaunched(
                    &self.id,
                    coordinate,
                    claimed,
                    sequence,
                    |_, _| Ok(()),
                    |_, _| Ok::<_, SharedJournalError>(()),
                )
                .map_err(|error| anyhow!("shared no-launch settlement failed: {error:?}"))
        }) {
            tracing::warn!(id = %self.id, ?error, "could not settle shared shell Job as unlaunched");
        }
    }

    pub(crate) fn install_process_input(
        &self,
        coordinate: usize,
        sender: mpsc::Sender<ProcessInputRequest>,
    ) -> Result<()> {
        if coordinate == 0 {
            bail!("Process input coordinate must be positive");
        }
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| anyhow!("job session disappeared before stdin custody"))?;
        let mut state = inner.state.lock().expect("job manager lock");
        let entry = state
            .available_mut(&self.id)
            .ok_or_else(|| anyhow!("job disappeared before stdin custody"))?;
        if entry.completion.is_some() {
            bail!("job settled before stdin custody");
        }
        if entry.process_inputs.insert(coordinate, sender).is_some() {
            bail!("Process stdin custody already installed for node {coordinate}");
        }
        entry.public_revision = entry.public_revision.saturating_add(1);
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        entry.notify.notify_waiters();
        Ok(())
    }

    pub(crate) fn notify_output_changed(&self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if let Ok(state) = inner.state.lock()
            && let Some(entry) = state.jobs.get(&self.id)
        {
            entry.notify.notify_waiters();
        }
    }

    pub(crate) fn retire_process_input(&self, coordinate: usize) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut state = inner.state.lock().expect("job manager lock");
        if let Some(entry) = state.jobs.get_mut(&self.id)
            && entry.process_inputs.remove(&coordinate).is_some()
        {
            entry.public_revision = entry.public_revision.saturating_add(1);
            entry.updated_at = chrono::Utc::now().to_rfc3339();
            entry.notify.notify_waiters();
        }
    }

    #[cfg(test)]
    pub(crate) fn mark_running(&self, shell_guard_generation: Option<[u8; 32]>) -> bool {
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
        entry.shell_guard_generation = shell_guard_generation;
        entry.public_revision = entry.public_revision.saturating_add(1);
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        entry.notify.notify_waiters();
        true
    }

    /// Reserve up to `requested` bytes from the process-session spool budget.
    /// The caller may persist only the returned prefix and must keep draining
    /// the producer after the budget is exhausted.
    #[cfg(test)]
    pub(crate) fn reserve_spool(&self, requested: u64) -> u64 {
        self.legacy_spool
            .as_ref()
            .map_or(0, |account| account.reserve(requested))
    }

    #[cfg(test)]
    pub(crate) fn release_spool(&self, bytes: u64) {
        if let Some(account) = &self.legacy_spool {
            account.release(bytes);
        }
    }

    #[cfg(test)]
    pub(crate) fn shared_stream_summary(
        &self,
        stream: JobStream,
        force_truncated: bool,
    ) -> StreamSummary {
        self.shared_node_stream_summary(1, stream, force_truncated)
    }

    pub(crate) fn shared_node_stream_summary(
        &self,
        coordinate: usize,
        stream: JobStream,
        force_truncated: bool,
    ) -> StreamSummary {
        let Some(inner) = self.inner.upgrade() else {
            return fallback_stream_summary(stream_path_handle(self, stream), true);
        };
        let shared_stream = match stream {
            JobStream::Stdout => SharedOutputStream::Stdout,
            JobStream::Stderr => SharedOutputStream::Stderr,
        };
        match inner
            .shared_jobs
            .output_summary(&self.id, coordinate, shared_stream)
        {
            Ok((summary, _)) => StreamSummary {
                total_bytes: summary.total_bytes,
                stored_bytes: summary.retained_bytes,
                truncated: summary.truncated || force_truncated,
            },
            Err(_) => fallback_stream_summary(stream_path_handle(self, stream), true),
        }
    }

    #[cfg(test)]
    pub(crate) fn begin_spool_writer(&self) -> Option<SpoolWriterGuard> {
        self.drain_writers.begin_one()
    }

    pub(crate) fn begin_spool_writers(&self) -> Option<[SpoolWriterGuard; 2]> {
        self.drain_writers.begin::<2>()
    }

    pub(crate) fn seal_spool_writers(&self) {
        self.drain_writers.seal();
    }

    pub(crate) async fn wait_spool_writers_idle(&self) {
        self.drain_writers.wait_idle().await;
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum JobOutputStream {
    Result,
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputPageRequest {
    pub node: Option<usize>,
    pub stream: Option<JobOutputStream>,
    pub offset: u64,
    pub requested_bytes: RawPageBytes,
    pub response_ceiling: SerializedResultBytes,
    pub base64: bool,
}

impl JobOutputStream {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Result => "result",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }

    fn shared(self) -> SharedOutputStream {
        match self {
            Self::Result => SharedOutputStream::Result,
            Self::Stdout => SharedOutputStream::Stdout,
            Self::Stderr => SharedOutputStream::Stderr,
        }
    }

    fn legacy(self) -> Option<JobStream> {
        match self {
            Self::Result => None,
            Self::Stdout => Some(JobStream::Stdout),
            Self::Stderr => Some(JobStream::Stderr),
        }
    }
}

fn shared_output_stream(stream: JobStream) -> SharedOutputStream {
    match stream {
        JobStream::Stdout => SharedOutputStream::Stdout,
        JobStream::Stderr => SharedOutputStream::Stderr,
    }
}

fn fallback_stream_summary(path: &Path, truncated: bool) -> StreamSummary {
    let stored = fs::metadata(path).map_or(0, |metadata| metadata.len());
    StreamSummary {
        total_bytes: stored,
        stored_bytes: stored,
        truncated,
    }
}

fn stream_path_handle(handle: &JobHandle, stream: JobStream) -> &Path {
    match stream {
        JobStream::Stdout => &handle.stdout_path,
        JobStream::Stderr => &handle.stderr_path,
    }
}

#[derive(Debug)]
struct ResponseAttemptGuard {
    inner: Weak<JobManagerInner>,
    id: String,
}

impl Drop for ResponseAttemptGuard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        abandon_response_attempt(&inner, &self.id);
    }
}

#[derive(Debug)]
pub(crate) struct JobStart {
    pub(crate) id: String,
    pub(crate) is_new: bool,
    _response_attempt: Option<ResponseAttemptGuard>,
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
            Self::Unknown(message) => McpError::invalid_params(bounded_diagnostic(message), None),
            Self::Internal(error) => {
                McpError::internal_error(bounded_diagnostic(error.to_string()), None)
            }
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

pub(crate) type JobResult<T> = std::result::Result<T, JobError>;

#[derive(Debug, Serialize)]
struct JobOutputPage<'a> {
    job_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<usize>,
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
    files: Option<[Arc<File>; 2]>,
    committed: bool,
}

impl PendingJobFiles {
    fn create(root: &Path, id: &str) -> Result<Self> {
        let paths = [
            root.join(format!("{id}.stdout")),
            root.join(format!("{id}.stderr")),
        ];
        let stdout = Arc::new(create_private_file(&paths[0])?);
        let stderr = match create_private_file(&paths[1]) {
            Ok(file) => Arc::new(file),
            Err(error) => {
                drop(stdout);
                let _ = fs::remove_file(&paths[0]);
                return Err(error);
            }
        };
        Ok(Self {
            paths,
            files: Some([stdout, stderr]),
            committed: false,
        })
    }

    fn handles(&self) -> [Arc<File>; 2] {
        let files = self.files.as_ref().expect("pending job files stay open");
        [files[0].clone(), files[1].clone()]
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for PendingJobFiles {
    fn drop(&mut self) {
        if !self.committed {
            // Windows cannot remove an open file reliably. Close the exact
            // admission capabilities before rolling their private names back.
            drop(self.files.take());
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
    async fn finish(&self, id: &str, completion: JobCompletion) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let drain_writers = {
            let state = inner.state.lock().expect("job manager lock");
            state.jobs.get(id).map(|entry| entry.drain_writers.clone())
        };
        if let Some(drain) = drain_writers {
            drain.seal_and_wait_idle().await;
        }
        finish_inner(&inner, id, completion);
    }
}

impl JobManager {
    #[cfg(test)]
    pub(crate) fn new(retention: Duration) -> Result<Self> {
        Self::with_spool_limit(retention, DEFAULT_JOB_SPOOL_BYTES)
    }

    pub(crate) fn with_spool_limit(retention: Duration, spool_limit_bytes: u64) -> Result<Self> {
        let session_uuid = Uuid::new_v4();
        let root = crate::private_state::state_dir()?
            .join("jobs")
            .join(format!(
                "{SESSION_PREFIX}{}-{}",
                std::process::id(),
                session_uuid.simple()
            ));
        let session_fingerprint = crate::mcp::sha256(session_uuid.as_bytes());
        let shared_jobs = Arc::new(
            SharedJobManager::new(SharedJobLimits {
                max_retained_jobs: SHARED_MAX_RETAINED_JOBS,
                max_exposed_jobs: MAX_RETAINED_JOBS,
                max_hidden_jobs: SHARED_MAX_RETAINED_JOBS,
                max_active_nodes: MAX_ACTIVE_JOBS,
                max_hidden_replays: MAX_REQUEST_REPLAYS,
                max_spool_bytes: spool_limit_bytes,
            })
            .map_err(|error| anyhow!("invalid shared JobManager limits: {error:?}"))?,
        );
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
            remote_target: Mutex::new(None),
            session_fingerprint,
            shared_jobs,
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
        // Job admission creates async ownership. Reject synchronous/internal
        // misuse before materializing session state rather than letting
        // `tokio::spawn` panic after a Job/request identity was published.
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| anyhow!("shell job execution requires an active Tokio runtime"))?;
        self.ensure_session_initialized()?;
        sweep_expired(&self.inner);
        let mut state = self.inner.state.lock().expect("job manager lock");
        if state.closed || self.inner.closed.load(Ordering::Acquire) {
            bail!("job session is shutting down");
        }
        if let Some(key) = request_key.as_ref()
            && let Some(id) = state.request_jobs.get(key).cloned()
        {
            let needs_slot = state.jobs.get(&id).is_some_and(|entry| {
                entry.disposition == JobDisposition::Pending
                    && entry.response_attempts == 0
                    && entry.retention == RetentionClass::TransportOnly
            });
            if needs_slot && !ensure_retained_capacity(&mut state, 1) {
                bail!(
                    "too many retained shell jobs ({MAX_RETAINED_JOBS}); wait for retained jobs to expire or acknowledge output before retrying this delivery"
                );
            }
            if let Some(entry) = state.jobs.get_mut(&id) {
                if needs_slot {
                    entry.retention = RetentionClass::Ordinary;
                }
                let response_attempt = (entry.disposition == JobDisposition::Pending).then(|| {
                    entry.response_attempts = entry.response_attempts.saturating_add(1);
                    ResponseAttemptGuard {
                        inner: Arc::downgrade(&self.inner),
                        id: id.clone(),
                    }
                });
                return Ok(JobStart {
                    id,
                    is_new: false,
                    _response_attempt: response_attempt,
                });
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
        if request_key.is_some() && transport_retention_slots(&state) >= MAX_REQUEST_REPLAYS {
            bail!(
                "too many pending shell delivery replays/cleanups ({MAX_REQUEST_REPLAYS}); wait for delivery acknowledgement or retention cleanup"
            );
        }
        if !ensure_retained_capacity(&mut state, 1) {
            bail!(
                "too many retained shell jobs ({MAX_RETAINED_JOBS}); acknowledge output or wait for retention cleanup"
            );
        }

        let id = Uuid::new_v4().to_string();
        let files = PendingJobFiles::create(&self.inner.root, &id)?;
        let [stdout_path, stderr_path] = files.paths.clone();
        let [stdout_file, stderr_file] = files.handles();
        let cancel = JobCancel::new();
        let notify = Arc::new(Notify::new());
        let drain_writers = DrainWriters::new();
        let shared_transition_gate = Arc::new(Mutex::new(()));
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
                task_result_ts: None,
                terminal_at: None,
                stdout_path: stdout_path.clone(),
                stderr_path: stderr_path.clone(),
                drain_writers: drain_writers.clone(),
                legacy_spool: None,
                stdout_retained: 0,
                stderr_retained: 0,
                stdout_consumed: 0,
                stderr_consumed: 0,
                stdout_released: false,
                stderr_released: false,
                readers: 0,
                output_cleanup_requested: false,
                output_cleanup_pending: false,
                expiration_pending: false,
                disposition: JobDisposition::Pending,
                response_attempts: 1,
                retention: RetentionClass::Ordinary,
                request_key: request_key.clone(),
                shell_guard_generation: None,
                process_inputs: HashMap::new(),
                delivered_through: HashMap::new(),
                shared_job: None,
                shared_transition_gate: shared_transition_gate.clone(),
                shared_graph_authoritative: false,
                retention_anchor: None,
                exposure_sequence: None,
                public_revision: 1,
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
            stdout_file,
            stderr_file,
            drain_writers,
            shared_transition_gate,
            #[cfg(test)]
            legacy_spool: None,
            inner: Arc::downgrade(&self.inner),
        };
        let panic_stdout = handle.stdout_path.clone();
        let panic_stderr = handle.stderr_path.clone();
        let panic_drain = handle.drain_writers.clone();
        let finisher = self.finisher();
        let id_for_task = id.clone();
        // Future construction is trusted host code, but it still must not be
        // able to poison the manager or strand a replay identity before a
        // managed effect exists. Product runners return `async move` directly;
        // actual shell preparation/launch happens only when that Future polls.
        let future = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runner(handle)))
        {
            Ok(future) => future,
            Err(_) => {
                let removed = remove_job_locked(&mut state, &id);
                debug_assert!(
                    removed.is_some(),
                    "new Job must exist during runner rollback"
                );
                return Err(anyhow!(
                    "shell job runner construction panicked before managed execution"
                ));
            }
        };
        let task = runtime.spawn(async move {
            let completion = match std::panic::AssertUnwindSafe(future).catch_unwind().await {
                Ok(completion) => completion,
                Err(_) => {
                    // A panic drops the runner future (and therefore its task
                    // handles), but blocking spool writes may still be finishing.
                    // Seal/quiesce before freezing the retained prefixes so a
                    // programmer bug cannot strand a permanently-working job.
                    panic_drain.seal_and_wait_idle().await;
                    JobCompletion::internal_failed(
                        "shell job runner panicked",
                        incomplete_stream_summary(&panic_stdout),
                        incomplete_stream_summary(&panic_stderr),
                    )
                }
            };
            finisher.finish(&id_for_task, completion).await;
        });
        state
            .jobs
            .get_mut(&id)
            .expect("job inserted before runner registration")
            .runner = Some(task);
        files.commit();
        Ok(JobStart {
            _response_attempt: Some(ResponseAttemptGuard {
                inner: Arc::downgrade(&self.inner),
                id: id.clone(),
            }),
            id,
            is_new: true,
        })
    }

    #[cfg(test)]
    pub(crate) fn create_for_test(&self, timeout_secs: u64) -> Result<JobHandle> {
        self.ensure_session_initialized()?;
        sweep_expired(&self.inner);
        let mut state = self.inner.state.lock().expect("job manager lock");
        if state.closed {
            bail!("job session is shutting down");
        }
        if !ensure_retained_capacity(&mut state, 1) {
            bail!(
                "too many retained shell jobs ({MAX_RETAINED_JOBS}); acknowledge output or wait for retention cleanup"
            );
        }
        let id = Uuid::new_v4().to_string();
        let files = PendingJobFiles::create(&self.inner.root, &id)?;
        let [stdout_path, stderr_path] = files.paths.clone();
        let [stdout_file, stderr_file] = files.handles();
        let cancel = JobCancel::new();
        let drain_writers = DrainWriters::new();
        let shared_transition_gate = Arc::new(Mutex::new(()));
        let legacy_spool = LegacySpoolAccount::new(&self.inner);
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
                task_result_ts: None,
                terminal_at: None,
                stdout_path: stdout_path.clone(),
                stderr_path: stderr_path.clone(),
                drain_writers: drain_writers.clone(),
                legacy_spool: Some(legacy_spool.clone()),
                stdout_retained: 0,
                stderr_retained: 0,
                stdout_consumed: 0,
                stderr_consumed: 0,
                stdout_released: false,
                stderr_released: false,
                readers: 0,
                output_cleanup_requested: false,
                output_cleanup_pending: false,
                expiration_pending: false,
                disposition: JobDisposition::Pending,
                response_attempts: 0,
                retention: RetentionClass::Ordinary,
                request_key: None,
                shell_guard_generation: None,
                process_inputs: HashMap::new(),
                delivered_through: HashMap::new(),
                shared_job: None,
                shared_transition_gate: shared_transition_gate.clone(),
                shared_graph_authoritative: false,
                retention_anchor: None,
                exposure_sequence: None,
                public_revision: 1,
                notify: Arc::new(Notify::new()),
            },
        );
        files.commit();
        Ok(JobHandle {
            id,
            cancel,
            stdout_path,
            stderr_path,
            stdout_file,
            stderr_file,
            drain_writers,
            shared_transition_gate,
            legacy_spool: Some(legacy_spool),
            inner: Arc::downgrade(&self.inner),
        })
    }

    #[cfg(test)]
    pub(crate) fn finish(&self, id: &str, completion: JobCompletion) {
        finish_inner(&self.inner, id, completion);
    }

    fn finisher(&self) -> JobFinisher {
        JobFinisher {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub(crate) fn resource_policy_matches(
        &self,
        retention: Duration,
        spool_limit_bytes: u64,
    ) -> bool {
        self.inner.retention_secs.load(Ordering::Relaxed) == retention.as_secs()
            && self.inner.spool_limit_bytes.load(Ordering::Relaxed) == spool_limit_bytes
            && self
                .inner
                .shared_jobs
                .spool_usage()
                .is_ok_and(|(_, limit)| limit == spool_limit_bytes)
    }

    #[cfg(test)]
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

    pub(crate) fn policy_fingerprint(&self) -> Result<[u8; 32]> {
        self.inner
            .policy_fingerprint
            .lock()
            .map_err(|_| anyhow!("job policy fingerprint lock poisoned"))?
            .ok_or_else(|| anyhow!("job execution policy is not bound"))
    }

    pub(crate) fn bind_remote_target(
        &self,
        target: crate::remote_start::BridgeTarget,
    ) -> Result<()> {
        let mut current = self
            .inner
            .remote_target
            .lock()
            .map_err(|_| anyhow!("remote bridge target lock poisoned"))?;
        match current.as_ref() {
            None => {
                *current = Some(target);
                Ok(())
            }
            Some(existing) if existing == &target => Ok(()),
            Some(_) => bail!("remote bridge target differs from live Job runtime"),
        }
    }

    pub(crate) fn remote_target(&self) -> Result<crate::remote_start::BridgeTarget> {
        self.inner
            .remote_target
            .lock()
            .map_err(|_| anyhow!("remote bridge target lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow!("remote bridge target is not configured"))
    }

    pub(crate) fn claim_remote_start(
        &self,
        start: &crate::remote_start::ValidatedRemoteStart,
    ) -> Result<RemoteStartDecision> {
        let now = crate::remote_start::now_unix_ms()?;
        if start.expires_at_unix_ms() <= now {
            bail!("remote Start authorization expired before replay claim");
        }
        let key = start.claim_key();
        let semantic_hash = start.semantic_hash();
        if key == [0; 32] || semantic_hash == [0; 32] {
            bail!("invalid remote Start replay identity");
        }
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| anyhow!("job manager lock poisoned"))?;
        state.remote_starts.retain(|_, entry| {
            entry.expires_at_unix_ms > now
                || matches!(entry.disposition, RemoteStartDisposition::Claimed)
        });
        if let Some(existing) = state.remote_starts.get(&key) {
            if existing.semantic_hash != semantic_hash {
                bail!("remote Start replay identity was reused for different semantics");
            }
            return Ok(match &existing.disposition {
                RemoteStartDisposition::Claimed | RemoteStartDisposition::Unknown => {
                    RemoteStartDecision::Unknown
                }
                RemoteStartDisposition::Ready(receipt) => {
                    RemoteStartDecision::Existing(receipt.clone())
                }
            });
        }
        if state.remote_starts.len() >= MAX_REQUEST_REPLAYS {
            bail!(
                "too many retained remote Start replay claims ({MAX_REQUEST_REPLAYS}); wait for proof expiry or restart the runtime"
            );
        }
        state.remote_starts.insert(
            key,
            RemoteStartEntry {
                semantic_hash,
                expires_at_unix_ms: start.expires_at_unix_ms(),
                disposition: RemoteStartDisposition::Claimed,
            },
        );
        Ok(RemoteStartDecision::New(RemoteStartGuard {
            inner: Arc::downgrade(&self.inner),
            key,
            settled: false,
        }))
    }

    #[cfg(test)]
    fn ensure_exposed_graph(&self, id: &str) -> Result<()> {
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .jobs
            .get(id)
            .ok_or_else(|| anyhow!("unknown Job {id}"))?;
        if !entry.disposition.exposed() || entry.shared_job.is_none() {
            bail!("Job {id} is not an exposed Graph");
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn shared_graph(&self, id: &str) -> Result<SharedGraph> {
        self.ensure_exposed_graph(id)?;
        self.inner
            .shared_jobs
            .graph(id)
            .map_err(|error| anyhow!("shared Graph is unavailable: {error:?}"))
    }

    #[cfg(test)]
    pub(crate) fn shared_snapshot(&self, id: &str) -> Result<SharedSnapshot> {
        self.ensure_exposed_graph(id)?;
        self.inner
            .shared_jobs
            .runtime_snapshot(id)
            .map_err(|error| anyhow!("shared Graph status is unavailable: {error:?}"))
    }

    pub(crate) fn render_graph_result(&self, id: &str, max_bytes: usize) -> Result<Option<String>> {
        self.inner
            .shared_jobs
            .render_graph_result(id, max_bytes)
            .map_err(|error| anyhow!("shared Graph result is unavailable: {error:?}"))
    }

    #[cfg(test)]
    pub(crate) fn shared_spool_usage(&self) -> Result<(u64, u64)> {
        self.inner
            .shared_jobs
            .spool_usage()
            .map_err(|error| anyhow!("shared spool usage is unavailable: {error:?}"))
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

        let (runners, drain_sets) = {
            let mut state = self.inner.state.lock().expect("job manager lock");
            state.closed = true;
            state.request_jobs.clear();
            let mut runners = Vec::new();
            let mut drain_sets = Vec::with_capacity(state.jobs.len());
            for entry in state.jobs.values_mut() {
                // Seal before cancellation so a detached runner subtask cannot
                // register a new writer after shutdown has observed an idle count.
                entry.drain_writers.seal();
                drain_sets.push(entry.drain_writers.clone());
                entry.cancel.cancel_as(CancelOrigin::Connector);
                if let Some(handle) = entry.runner.take() {
                    handle.abort();
                    runners.push(handle);
                }
            }
            (runners, drain_sets)
        };
        for runner in runners {
            let _ = runner.await;
        }

        // Runner abort drops its AbortOnDrop spool handles, but Tokio abort is a
        // request: a writer already inside cancellation-safe blocking I/O may
        // still be running. Registrations are already sealed; now wait until all
        // existing writers have actually stopped before unlinking spool state.
        for drain in &drain_sets {
            drain.wait_idle().await;
        }

        // Aborting the host runner can bypass its normal finisher. Close the
        // canonical shared lifecycle before unlinking any file-backed output.
        // Unknown is intentional: process-tree stop/side-effect completion is
        // not proven merely because the Tokio owner was aborted.
        let shared_ids = {
            let state = self.inner.state.lock().expect("job manager lock");
            state
                .jobs
                .values()
                .filter(|entry| entry.shared_job.is_some())
                .map(|entry| entry.id.clone())
                .collect::<Vec<_>>()
        };
        for id in shared_ids {
            let completion = JobCompletion::internal_failed(
                "job session shut down before runner settlement",
                StreamSummary::default(),
                StreamSummary::default(),
            );
            let _ = settle_shared_completion(&self.inner, &id, &completion, true);
        }

        let cleanup_ok = {
            let mut state = self.inner.state.lock().expect("job manager lock");
            let mut cleanup_ok = true;
            for entry in state.jobs.values_mut() {
                cleanup_ok &= cleanup_entry(entry);
            }
            if cleanup_ok {
                state.jobs.clear();
                state.request_jobs.clear();
            }
            cleanup_ok
        };
        if !cleanup_ok {
            if let Err(error) = mark_session_unresolved(&self.inner.root) {
                tracing::error!(%error, path = %self.inner.root.display(), "could not durably mark unresolved Job evidence");
            }
            return Err(anyhow!(
                "job shutdown could not safely release all shared/output state; host metadata and spool session retained for inspection"
            ));
        }
        self.inner
            .session_lock
            .lock()
            .expect("job session lock")
            .take();
        let root = self.inner.root.clone();
        crate::blocking::run(|| remove_session_directory(&root)).with_context(|| {
            format!(
                "cannot remove job spool session {}",
                self.inner.root.display()
            )
        })?;
        Ok(())
    }

    fn retention(&self) -> Duration {
        Duration::from_secs(self.inner.retention_secs.load(Ordering::Relaxed))
    }

    /// Authenticated live-runtime generation consumed by the D2 remote Start bridge.
    #[allow(dead_code)]
    pub(crate) fn runtime_generation_fingerprint(&self) -> [u8; 32] {
        self.inner.session_fingerprint
    }

    fn standalone_list_cursor(&self, before_exposure: u64) -> String {
        let mut bytes = before_exposure.to_be_bytes().to_vec();
        let mut hash = Sha256::new();
        hash.update(b"dotlink-jobs-list-cursor-v1\0");
        hash.update(self.inner.session_fingerprint);
        hash.update(before_exposure.to_be_bytes());
        bytes.extend_from_slice(&hash.finalize()[..16]);
        URL_SAFE_NO_PAD.encode(bytes)
    }

    fn parse_standalone_list_cursor(&self, cursor: &str) -> JobResult<u64> {
        if cursor.is_empty() || cursor.len() > 128 {
            return Err(JobError::Unknown("invalid jobs_list cursor".into()));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(cursor)
            .map_err(|_| JobError::Unknown("invalid jobs_list cursor".into()))?;
        if bytes.len() != 24 {
            return Err(JobError::Unknown("invalid jobs_list cursor".into()));
        }
        let mut sequence = [0u8; 8];
        sequence.copy_from_slice(&bytes[..8]);
        let sequence = u64::from_be_bytes(sequence);
        let mut hash = Sha256::new();
        hash.update(b"dotlink-jobs-list-cursor-v1\0");
        hash.update(self.inner.session_fingerprint);
        hash.update(sequence.to_be_bytes());
        if hash.finalize()[..16] != bytes[8..] {
            return Err(JobError::Unknown(
                "stale or unauthorized jobs_list cursor".into(),
            ));
        }
        Ok(sequence)
    }

    pub(crate) fn standalone_ref(id: &str, node: Option<usize>) -> JobResult<JobRef> {
        let job = SharedHandle::new(id.to_owned())
            .map_err(|_| JobError::Internal(anyhow!("invalid trusted Job handle")))?;
        let node = node
            .map(|node| {
                u32::try_from(node)
                    .ok()
                    .and_then(|node| NodeCoordinate::new(node).ok())
                    .ok_or_else(|| JobError::Unknown("Job node coordinate is invalid".into()))
            })
            .transpose()?;
        Ok(JobRef::new(job, node))
    }

    fn compact_root_status(&self, entry: &JobEntry) -> JobResult<(u64, String)> {
        let reference = Self::standalone_ref(&entry.id, None)?;
        let kind = if entry.shared_graph_authoritative {
            "graph"
        } else {
            "process"
        };
        match self.inner.shared_jobs.runtime_snapshot(&entry.id) {
            Ok(snapshot) => {
                let terminal =
                    if snapshot.lifecycle == SharedLifecycle::Terminal && snapshot.total == 1 {
                        self.inner
                            .shared_jobs
                            .process_terminal(&entry.id, 1)
                            .map_err(|error| {
                                JobError::Internal(anyhow!(
                                    "cannot inspect shared Job terminal facts: {error:?}"
                                ))
                            })?
                    } else {
                        None
                    };
                let rendered = render_plain(
                    Receipt::JobStatus {
                        reference: &reference,
                        snapshot: &snapshot,
                        terminal,
                        next: None,
                    },
                    aw_tools::filesystem::job_manager::MAX_COMPACT_JOB_STATUS_BYTES,
                )
                .map_err(|error| {
                    JobError::Internal(anyhow!("cannot render shared Job status: {error:?}"))
                })?;
                Ok((snapshot.sequence, rendered))
            }
            Err(aw_tools::filesystem::job_manager::Error::NotFound)
                if entry.shared_job.is_none() =>
            {
                let state = match (entry.phase, entry.status()) {
                    (JobPhase::Queued, JobStatus::Working) => "waiting",
                    (JobPhase::Running, JobStatus::Working) => {
                        if entry.cancel.is_cancelled() {
                            "cancelling"
                        } else {
                            "running"
                        }
                    }
                    (_, JobStatus::Cancelled) => "cancelled",
                    (_, _) => "settled",
                };
                let outcome = if entry.status().terminal() {
                    Some(match entry.status() {
                        JobStatus::Completed
                            if entry.completion.as_ref().is_some_and(|c| c.success) =>
                        {
                            "ok"
                        }
                        JobStatus::Cancelled => "cancelled",
                        JobStatus::Unknown => "unknown",
                        JobStatus::Working => unreachable!(),
                        _ => "fail",
                    })
                } else {
                    None
                };
                let mut row = format!("{reference} {kind} {state} rev={}", entry.public_revision);
                if let Some(outcome) = outcome {
                    row.push_str(&format!(" outcome={outcome}"));
                }
                Ok((entry.public_revision, row))
            }
            Err(error) => Err(JobError::Internal(anyhow!(
                "cannot inspect shared Job status: {error:?}"
            ))),
        }
    }

    fn standalone_status_locked(
        &self,
        entry: &JobEntry,
        node: Option<usize>,
    ) -> JobResult<(u64, String)> {
        let id = entry.id.as_str();
        if let Some(node) = node {
            if !entry.shared_graph_authoritative {
                return Err(JobError::Unknown(
                    "Process Job ref cannot contain a Graph node".into(),
                ));
            }
            let snapshot = self
                .inner
                .shared_jobs
                .runtime_snapshot(id)
                .map_err(|error| {
                    JobError::Internal(anyhow!("cannot inspect shared Graph status: {error:?}"))
                })?;
            let node_state = self
                .inner
                .shared_jobs
                .node_state(id, node)
                .map_err(|error| JobError::Unknown(format!("unavailable Graph node: {error:?}")))?;
            let graph = self.inner.shared_jobs.graph(id).map_err(|error| {
                JobError::Internal(anyhow!("cannot inspect shared Job Graph: {error:?}"))
            })?;
            let tool = graph
                .steps()
                .get(node.saturating_sub(1))
                .ok_or_else(|| JobError::Unknown(format!("unknown Graph node {node}")))?
                .tool
                .name();
            let terminal = if matches!(node_state, SharedNodeState::Terminal { .. }) {
                self.inner
                    .shared_jobs
                    .process_terminal(id, node)
                    .map_err(|error| {
                        JobError::Internal(anyhow!(
                            "cannot inspect shared Graph node terminal facts: {error:?}"
                        ))
                    })?
            } else {
                None
            };
            let reference = Self::standalone_ref(id, Some(node))?;
            let rendered = render_plain(
                Receipt::NodeStatus {
                    reference: &reference,
                    tool,
                    revision: snapshot.sequence,
                    state: node_state,
                    journal_unknown: snapshot.journal_unknown,
                    terminal,
                },
                aw_tools::filesystem::job_manager::MAX_COMPACT_JOB_STATUS_BYTES,
            )
            .map_err(|error| {
                JobError::Internal(anyhow!("cannot render shared Graph node status: {error:?}"))
            })?;
            return Ok((snapshot.sequence, rendered));
        }
        self.compact_root_status(entry)
    }

    pub(crate) fn standalone_status(
        &self,
        id: &str,
        node: Option<usize>,
    ) -> JobResult<(u64, String)> {
        sweep_expired(&self.inner);
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        self.standalone_status_locked(entry, node)
    }

    /// Render the richest shared factual handoff that fits the host projection,
    /// falling back only to the shared typed no-rerun receipt for the same Job.
    pub(crate) fn standalone_handoff_fitted(
        &self,
        id: &str,
        fits: impl Fn(&str) -> bool,
    ) -> JobResult<String> {
        sweep_expired(&self.inner);
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        let reference = Self::standalone_ref(id, None)?;
        match self.inner.shared_jobs.runtime_snapshot(id) {
            Ok(snapshot) => {
                let terminal =
                    if snapshot.lifecycle == SharedLifecycle::Terminal && snapshot.total == 1 {
                        self.inner
                            .shared_jobs
                            .process_terminal(id, 1)
                            .map_err(|error| {
                                JobError::Internal(anyhow!(
                                    "cannot inspect shared Job terminal facts: {error:?}"
                                ))
                            })?
                    } else {
                        None
                    };
                let full = render_handoff(
                    &reference,
                    &snapshot,
                    terminal,
                    aw_tools::filesystem::job_manager::MAX_COMPACT_JOB_STATUS_BYTES,
                )
                .map_err(|error| {
                    JobError::Internal(anyhow!("cannot render shared Job handoff: {error:?}"))
                })?;
                if fits(&full) {
                    return Ok(full);
                }
                let concise = render_plain(
                    Receipt::Handoff {
                        reference: &reference,
                        revision: snapshot.sequence,
                    },
                    aw_tools::filesystem::job_manager::MAX_COMPACT_JOB_STATUS_BYTES,
                )
                .map_err(|error| {
                    JobError::Internal(anyhow!(
                        "cannot render shared compact Job handoff: {error:?}"
                    ))
                })?;
                fits(&concise).then_some(concise).ok_or_else(|| {
                    JobError::Unknown(
                        "Job is already admitted, but its handoff exceeds the response budget; list Jobs to recover it without rerunning"
                            .into(),
                    )
                })
            }
            Err(aw_tools::filesystem::job_manager::Error::NotFound)
                if entry.shared_job.is_none() =>
            {
                let (_, full) = self.compact_root_status(entry)?;
                if fits(&full) {
                    return Ok(full);
                }
                let concise = render_plain(
                    Receipt::Handoff {
                        reference: &reference,
                        revision: entry.public_revision,
                    },
                    aw_tools::filesystem::job_manager::MAX_COMPACT_JOB_STATUS_BYTES,
                )
                .map_err(|error| {
                    JobError::Internal(anyhow!(
                        "cannot render compatibility Job handoff: {error:?}"
                    ))
                })?;
                fits(&concise).then_some(concise).ok_or_else(|| {
                    JobError::Unknown(
                        "Job is already admitted, but its handoff exceeds the response budget; list Jobs to recover it without rerunning"
                            .into(),
                    )
                })
            }
            Err(error) => Err(JobError::Internal(anyhow!(
                "cannot inspect shared Job status: {error:?}"
            ))),
        }
    }

    /// Observe an exact Job or Graph node until settlement or the caller's
    /// deadline. rev is only a status token: stdout, sibling progress and
    /// revision changes do not complete this wait. Execution keeps its own
    /// independent timeout.
    // The existing Job notify is the sole observation signal; state and node
    // settlement are always read from their authoritative owners under the
    // original lock. No public revision is a completion condition.
    fn standalone_wait_snapshot(
        &self,
        id: &str,
        node: Option<usize>,
    ) -> JobResult<(String, Arc<Notify>, bool)> {
        sweep_expired(&self.inner);
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        let (_, rendered) = self.standalone_status_locked(entry, node)?;
        let settled = if let Some(coordinate) = node {
            matches!(
                self.inner
                    .shared_jobs
                    .node_state(id, coordinate)
                    .map_err(|error| {
                        JobError::Unknown(format!("unavailable Graph node: {error:?}"))
                    })?,
                SharedNodeState::Terminal { .. }
            )
        } else {
            entry.status().terminal()
        };
        Ok((rendered, entry.notify.clone(), settled))
    }

    pub(crate) async fn standalone_get(
        &self,
        id: &str,
        node: Option<usize>,
        _observed_revision: Option<u64>,
        wait_ms: u64,
    ) -> JobResult<String> {
        if wait_ms > DOTLINK_JOB_PROFILE.get_wait_max_ms {
            return Err(JobError::Unknown(format!(
                "jobs_get wait_ms exceeds {}",
                DOTLINK_JOB_PROFILE.get_wait_max_ms
            )));
        }
        let deadline = TokioInstant::now()
            .checked_add(Duration::from_millis(wait_ms))
            .ok_or_else(|| JobError::Unknown("jobs_get wait window overflow".into()))?;
        loop {
            let (rendered, notify, settled) = self.standalone_wait_snapshot(id, node)?;
            if settled || wait_ms == 0 || TokioInstant::now() >= deadline {
                return Ok(rendered);
            }
            // Register before rechecking. A committed transition between the
            // first snapshot and this point can neither be missed nor mistaken
            // for an early return on an unrelated revision/output event.
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (current, _, settled) = self.standalone_wait_snapshot(id, node)?;
            if settled || TokioInstant::now() >= deadline {
                return Ok(current);
            }
            if tokio::time::timeout_at(deadline, notified.as_mut())
                .await
                .is_err()
            {
                return self.standalone_status(id, node).map(|(_, status)| status);
            }
        }
    }

    fn standalone_process_tail_coordinate(
        &self,
        id: &str,
        node: Option<usize>,
    ) -> JobResult<Option<(Arc<SharedJobManager>, usize)>> {
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        let Some(shared) = entry.shared_job.as_ref() else {
            return Ok(None);
        };
        if !entry.shared_graph_authoritative && node.is_none() {
            return Ok(Some((shared.manager.clone(), 1)));
        }
        let Some(coordinate) = node else {
            return Ok(None);
        };
        let graph = shared.manager.graph(id).map_err(|error| {
            JobError::Internal(anyhow!(
                "cannot inspect shared Job Graph for {id}: {error:?}"
            ))
        })?;
        let Some(step) = graph.steps().get(coordinate.saturating_sub(1)) else {
            return Err(JobError::Unknown(format!(
                "job {id} node is outside 1..={}",
                graph.steps().len()
            )));
        };
        Ok(step
            .tool
            .native()
            .is_none()
            .then(|| (shared.manager.clone(), coordinate)))
    }

    pub(crate) fn standalone_process_tail(
        &self,
        id: &str,
        node: Option<usize>,
        requested: Option<usize>,
        status: String,
        response_ceiling: usize,
    ) -> JobResult<String> {
        let tail = requested.unwrap_or(DOTLINK_JOB_PROFILE.get_tail_default);
        if tail > DOTLINK_JOB_PROFILE.get_tail_max {
            return Err(JobError::Unknown(format!(
                "jobs_get tail exceeds {} combined bytes",
                DOTLINK_JOB_PROFILE.get_tail_max
            )));
        }
        if tail == 0 {
            return Ok(status);
        }
        let Some((manager, coordinate)) = self.standalone_process_tail_coordinate(id, node)? else {
            return if requested.is_some() {
                Err(JobError::Unknown(
                    "jobs_get tail is supported only by Process Job streams".into(),
                ))
            } else {
                Ok(status)
            };
        };
        let owner = manager.job_output_owner(id).map_err(|error| {
            JobError::Internal(anyhow!(
                "cannot derive shared Job output owner for {id}: {error:?}"
            ))
        })?;
        let project = |limit: usize, capped: bool| -> JobResult<String> {
            let pages = manager.process_tail_for_owner(id, coordinate, owner, limit);
            let mut rendered = status.clone();
            match pages {
                Ok((stdout, stderr)) => {
                    let mut any = false;
                    for (stream, page) in [("stdout", stdout), ("stderr", stderr)] {
                        if let Some(page) = page {
                            any = true;
                            let end = page.offset + page.bytes.len() as u64;
                            rendered.push_str(&format!(
                                "\n{stream} [{}..{}]{}:\n",
                                page.offset,
                                end,
                                if page.writer_finished { " eof" } else { "" }
                            ));
                            rendered.push_str(&String::from_utf8_lossy(&page.bytes));
                        }
                    }
                    if any && capped {
                        rendered.push_str("\ntail=capped");
                    }
                }
                Err(aw_tools::filesystem::job_manager::Error::OutputReleased) => {
                    rendered.push_str("\ntail=released");
                }
                Err(aw_tools::filesystem::job_manager::Error::OutputMissing) => {
                    rendered.push_str("\ntail=unavailable");
                }
                Err(error) => {
                    return Err(JobError::Internal(anyhow!(
                        "job {id} Process tail unavailable: {error:?}"
                    )));
                }
            }
            Ok(rendered)
        };
        let full = project(tail, false)?;
        if serialized_tool_result_len(&full).is_ok_and(|size| size <= response_ceiling) {
            return Ok(full);
        }
        let budget_marker = format!("{status}\ntail=budget");
        if match serialized_tool_result_len(&budget_marker) {
            Ok(size) => size > response_ceiling,
            Err(_) => true,
        } {
            return Ok(status);
        }
        let mut low = 0usize;
        let mut high = tail;
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            let candidate = project(middle, middle < tail)?;
            if serialized_tool_result_len(&candidate).is_ok_and(|size| size <= response_ceiling) {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        if low == 0 {
            Ok(budget_marker)
        } else {
            project(low, low < tail)
        }
    }

    pub(crate) fn standalone_list_fitted(
        &self,
        cursor: Option<&str>,
        limit: usize,
        fits: impl Fn(&str) -> bool,
    ) -> JobResult<String> {
        if !(1..=256).contains(&limit) {
            return Err(JobError::Unknown(
                "jobs_list limit must be within 1..=256".into(),
            ));
        }
        sweep_expired(&self.inner);
        let before = cursor
            .map(|cursor| self.parse_standalone_list_cursor(cursor))
            .transpose()?;
        let state = self.inner.state.lock().expect("job manager lock");
        let mut jobs = state
            .jobs
            .values()
            .filter(|entry| {
                entry.exposure_sequence.is_some()
                    && !entry.expiration_pending
                    && before.is_none_or(|before| entry.exposure_sequence.unwrap() < before)
            })
            .collect::<Vec<_>>();
        jobs.sort_by(|left, right| {
            right
                .exposure_sequence
                .cmp(&left.exposure_sequence)
                .then_with(|| left.id.cmp(&right.id))
        });
        let available = jobs.len();
        let selected = jobs.into_iter().take(limit).collect::<Vec<_>>();
        let render = |count: usize| -> JobResult<String> {
            let mut rows = Vec::with_capacity(count + 1);
            for entry in selected.iter().take(count) {
                rows.push(self.compact_root_status(entry)?.1);
            }
            if count < available {
                let boundary = selected
                    .get(count.saturating_sub(1))
                    .and_then(|entry| entry.exposure_sequence)
                    .or(before)
                    .unwrap_or(u64::MAX);
                rows.push(format!(
                    "more cursor={}",
                    self.standalone_list_cursor(boundary)
                ));
            }
            Ok(rows.join("\n"))
        };
        if selected.is_empty() {
            let text = render(0)?;
            return fits(&text).then_some(text).ok_or_else(|| {
                JobError::Unknown("jobs_list response budget is too small for page metadata".into())
            });
        }
        let mut best = None;
        for count in 1..=selected.len() {
            let candidate = render(count)?;
            if fits(&candidate) {
                best = Some(candidate);
            } else {
                break;
            }
        }
        best.ok_or_else(|| {
            JobError::Unknown(
                "jobs_list response budget is too small for one complete Job row".into(),
            )
        })
    }

    pub(crate) fn standalone_process_tool(
        &self,
        id: &str,
        node: Option<usize>,
    ) -> JobResult<SharedNodeTool> {
        sweep_expired(&self.inner);
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        let coordinate = match (entry.shared_graph_authoritative, node) {
            (false, None) => 1,
            (false, Some(_)) => {
                return Err(JobError::Unknown(
                    "Process Job ref cannot contain a Graph node".into(),
                ));
            }
            (true, Some(node)) if node > 0 => node,
            (true, None) => {
                return Err(JobError::Unknown(
                    "Graph Process input requires an exact node ref".into(),
                ));
            }
            (true, Some(_)) => {
                return Err(JobError::Unknown(
                    "Graph node coordinate must be positive".into(),
                ));
            }
        };
        let shared = entry.shared_job.as_ref().ok_or_else(|| {
            JobError::Unknown(
                "Process stdin is not interactive, not ready, closed, or already settled".into(),
            )
        })?;
        let graph = shared
            .manager
            .graph(id)
            .map_err(|error| JobError::Unknown(format!("Process graph unavailable: {error:?}")))?;
        let tool = graph
            .steps()
            .get(coordinate.saturating_sub(1))
            .map(|step| step.tool)
            .ok_or_else(|| JobError::Unknown(format!("unknown Graph node {coordinate}")))?;
        if !tool.is_shell() {
            return Err(JobError::Unknown(
                "jobs_send target is not an interactive Process node".into(),
            ));
        }
        Ok(tool)
    }

    pub(crate) fn standalone_shell_guard_generation(
        &self,
        id: &str,
    ) -> JobResult<Option<[u8; 32]>> {
        sweep_expired(&self.inner);
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        Ok(entry.shell_guard_generation)
    }

    #[cfg(test)]
    pub(crate) fn process_input_ready_for_test(&self, id: &str, node: Option<usize>) -> bool {
        let state = self.inner.state.lock().expect("job manager lock");
        let Some(entry) = state.jobs.get(id) else {
            return false;
        };
        let coordinate = match (entry.shared_graph_authoritative, node) {
            (false, None) => 1,
            (true, Some(node)) if node > 0 => node,
            _ => return false,
        };
        entry.process_inputs.contains_key(&coordinate)
    }

    #[cfg(test)]
    pub(crate) fn set_shell_guard_generation_for_test(
        &self,
        id: &str,
        generation: Option<[u8; 32]>,
    ) {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state.jobs.get_mut(id).expect("test Job exists");
        entry.shell_guard_generation = generation;
        entry.public_revision = entry.public_revision.saturating_add(1);
        entry.notify.notify_waiters();
    }

    pub(crate) async fn standalone_send_process_input(
        &self,
        id: &str,
        node: Option<usize>,
        expected_revision: Option<u64>,
        input: ProcessInput,
    ) -> JobResult<u64> {
        const MAX_INPUT_BYTES: usize = 64 * 1024;
        if let ProcessInput::Text(bytes) = &input
            && bytes.len() > MAX_INPUT_BYTES
        {
            return Err(JobError::Unknown(format!(
                "jobs_send text exceeds {MAX_INPUT_BYTES} bytes"
            )));
        }
        let (sender, revision, shared_owned) = {
            sweep_expired(&self.inner);
            let state = self.inner.state.lock().expect("job manager lock");
            let entry = state
                .available(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            if entry.status().terminal() {
                return Err(JobError::Unknown(format!("job {id} is already terminal")));
            }
            let coordinate = match (entry.shared_graph_authoritative, node) {
                (false, None) => 1,
                (false, Some(_)) => {
                    return Err(JobError::Unknown(
                        "Process Job ref cannot contain a Graph node".into(),
                    ));
                }
                (true, Some(node)) if node > 0 => node,
                (true, None) => {
                    return Err(JobError::Unknown(
                        "Graph jobs_send requires an exact interactive node ref".into(),
                    ));
                }
                (true, Some(_)) => {
                    return Err(JobError::Unknown(
                        "Graph node coordinate must be positive".into(),
                    ));
                }
            };
            let (revision, _) = self.standalone_status_locked(entry, node)?;
            if expected_revision.is_some_and(|expected| expected != revision) {
                return Err(JobError::Unknown(format!(
                    "stale Job revision: expected {expected_revision:?}, current {revision}"
                )));
            }
            let sender = entry
                .process_inputs
                .get(&coordinate)
                .cloned()
                .ok_or_else(|| {
                    JobError::Unknown(
                        "Process stdin is not interactive, not ready, closed, or already settled"
                            .into(),
                    )
                })?;
            (sender, revision, entry.shared_job.is_some())
        };
        let effect_revision = if shared_owned {
            self.inner
                .shared_jobs
                .compare_and_bump_revision(id, expected_revision)
                .map_err(|error| {
                    JobError::Unknown(format!(
                        "stale Job revision or unavailable stdin target: {error:?}"
                    ))
                })?
        } else {
            revision.saturating_add(1)
        };
        let (response, received) = oneshot::channel();
        sender
            .send(ProcessInputRequest { input, response })
            .await
            .map_err(|_| {
                JobError::Unknown(
                    "Process stdin owner disappeared before delivery; inspect before retry".into(),
                )
            })?;
        match received.await {
            Ok(Ok(())) => {
                let mut state = self.inner.state.lock().expect("job manager lock");
                let entry = state
                    .available_mut(id)
                    .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
                entry.public_revision = entry.public_revision.saturating_add(1);
                entry.updated_at = chrono::Utc::now().to_rfc3339();
                entry.notify.notify_waiters();
                Ok(if shared_owned {
                    effect_revision
                } else {
                    entry.public_revision.max(effect_revision)
                })
            }
            Ok(Err(message)) => Err(JobError::Unknown(message)),
            Err(_) => Err(JobError::Unknown(
                "Process stdin outcome is unknown; do not retry blindly".into(),
            )),
        }
    }

    pub(crate) fn cancel(&self, id: &str) -> JobResult<String> {
        sweep_expired(&self.inner);
        let (should_cancel, transition_gate) = {
            let state = self.inner.state.lock().expect("job manager lock");
            let entry = state
                .available(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            (
                !entry.status().terminal(),
                entry.shared_transition_gate.clone(),
            )
        };
        if should_cancel {
            let _transition = transition_gate
                .lock()
                .map_err(|_| JobError::Internal(anyhow!("shared Job transition gate poisoned")))?;
            let shared_owned = self
                .inner
                .state
                .lock()
                .expect("job manager lock")
                .available(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?
                .shared_job
                .is_some();
            match self.inner.shared_jobs.runtime_snapshot(id) {
                Ok(snapshot)
                    if snapshot.lifecycle != aw_tools::filesystem::job::Lifecycle::Terminal =>
                {
                    let sequence = next_shared_sequence(&self.inner.shared_jobs, id)
                        .map_err(JobError::Internal)?;
                    self.inner
                        .shared_jobs
                        .cancel(id, sequence, |_, _| Ok::<_, SharedJournalError>(()))
                        .map_err(|error| {
                            JobError::Internal(anyhow!(
                                "shared Job cancellation checkpoint failed: {error:?}"
                            ))
                        })?;
                }
                Ok(_) => {}
                Err(aw_tools::filesystem::job_manager::Error::NotFound) if !shared_owned => {}
                Err(aw_tools::filesystem::job_manager::Error::NotFound) => {
                    return Err(JobError::Internal(anyhow!(
                        "shared-owned Job {id} disappeared before cancellation"
                    )));
                }
                Err(error) => {
                    return Err(JobError::Internal(anyhow!(
                        "cannot inspect shared Job before cancellation: {error:?}"
                    )));
                }
            }
        }
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available_mut(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        if !entry.status().terminal() {
            entry.cancel.cancel();
            entry.public_revision = entry.public_revision.saturating_add(1);
            entry.updated_at = chrono::Utc::now().to_rfc3339();
            entry.notify.notify_waiters();
        }
        Ok(render_status_locked(&self.inner, entry))
    }

    /// Compare-and-act cancellation for the finite Jobs control surface.
    ///
    /// The shared public revision is reserved immediately before cancellation
    /// intent leaves the owner. Remote replay is resolved above this boundary.
    pub(crate) fn standalone_cancel_checked(
        &self,
        id: &str,
        expected_revision: Option<u64>,
    ) -> JobResult<(bool, u64)> {
        sweep_expired(&self.inner);
        let (shared_owned, transition_gate) = {
            let state = self.inner.state.lock().expect("job manager lock");
            let entry = state
                .available(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            (
                entry.shared_job.is_some(),
                entry.shared_transition_gate.clone(),
            )
        };

        if !shared_owned {
            let (current, _) = self.standalone_status(id, None)?;
            if expected_revision.is_some_and(|expected| expected != current) {
                return Err(JobError::Unknown(format!(
                    "stale Job revision: expected {expected_revision:?}, current {current}"
                )));
            }
            let changed = {
                let state = self.inner.state.lock().expect("job manager lock");
                !state
                    .available(id)
                    .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?
                    .status()
                    .terminal()
            };
            self.cancel(id)?;
            let (revision, _) = self.standalone_status(id, None)?;
            return Ok((changed, revision));
        }

        let _transition = transition_gate
            .lock()
            .map_err(|_| JobError::Internal(anyhow!("shared Job transition gate poisoned")))?;
        let before = self
            .inner
            .shared_jobs
            .runtime_snapshot(id)
            .map_err(|error| {
                JobError::Internal(anyhow!(
                    "cannot inspect shared Job before cancellation: {error:?}"
                ))
            })?;
        if expected_revision.is_some_and(|expected| expected != before.sequence) {
            return Err(JobError::Unknown(format!(
                "stale Job revision: expected {expected_revision:?}, current {}",
                before.sequence
            )));
        }
        if before.lifecycle != SharedLifecycle::Running {
            return Ok((false, before.sequence));
        }

        self.inner
            .shared_jobs
            .compare_and_bump_revision(id, expected_revision)
            .map_err(|error| {
                JobError::Unknown(format!(
                    "stale Job revision or unavailable cancellation target: {error:?}"
                ))
            })?;
        let sequence =
            next_shared_sequence(&self.inner.shared_jobs, id).map_err(JobError::Internal)?;
        self.inner
            .shared_jobs
            .cancel(id, sequence, |_, _| Ok::<_, SharedJournalError>(()))
            .map_err(|error| {
                JobError::Internal(anyhow!(
                    "shared Job cancellation checkpoint failed: {error:?}"
                ))
            })?;
        {
            let mut state = self.inner.state.lock().expect("job manager lock");
            let entry = state
                .available_mut(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            entry.cancel.cancel();
            entry.public_revision = entry.public_revision.saturating_add(1);
            entry.updated_at = chrono::Utc::now().to_rfc3339();
            entry.notify.notify_waiters();
        }
        let after = self
            .inner
            .shared_jobs
            .runtime_snapshot(id)
            .map_err(|error| {
                JobError::Internal(anyhow!(
                    "cannot inspect shared Job after cancellation: {error:?}"
                ))
            })?;
        Ok((true, after.sequence))
    }

    #[cfg(test)]
    pub(crate) fn status(&self, id: &str) -> JobResult<String> {
        sweep_expired(&self.inner);
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        Ok(render_status_locked(&self.inner, entry))
    }

    /// Snapshot newest-first exposed Jobs, then admit only complete rows whose
    /// rendered page fits the host projection. The fit callback is deliberately
    /// transport-neutral: MCP can measure its exact outer wire representation
    /// while other hosts can apply their own response envelope.
    #[cfg(test)]
    pub(crate) fn list_fitted(
        &self,
        offset: usize,
        limit: usize,
        fits: impl Fn(&str) -> bool,
    ) -> JobResult<String> {
        sweep_expired(&self.inner);
        let (rows, total, spool_used_bytes, spool_limit_bytes) = {
            let state = self.inner.state.lock().expect("job manager lock");
            let mut jobs = state
                .jobs
                .values()
                .filter(|entry| {
                    (entry.disposition.exposed()
                        || (entry.disposition == JobDisposition::Synchronous
                            && entry.status() == JobStatus::Unknown))
                        && !entry.expiration_pending
                })
                .collect::<Vec<_>>();
            jobs.sort_by(|left, right| {
                right
                    .created
                    .cmp(&left.created)
                    .then_with(|| left.id.cmp(&right.id))
            });
            let total = jobs.len();
            let rows = jobs
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|entry| {
                    serde_json::json!({
                        "job_id": entry.id,
                        "status": projected_status(&self.inner, entry),
                        "phase": entry.phase,
                        "created_at": entry.created_at,
                        "elapsed_ms": entry.created.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    })
                })
                .collect::<Vec<_>>();
            let (spool_used_bytes, spool_limit_bytes) = physical_spool_usage(&self.inner);
            (rows, total, spool_used_bytes, spool_limit_bytes)
        };

        let render = |count: usize| {
            let next_offset = (offset + count < total).then_some(offset + count);
            serde_json::json!({
                "jobs": &rows[..count],
                "offset": offset,
                "next_offset": next_offset,
                "spool_used_bytes": spool_used_bytes,
                "spool_limit_bytes": spool_limit_bytes,
            })
            .to_string()
        };

        if rows.is_empty() {
            let page = render(0);
            return fits(&page).then_some(page).ok_or_else(|| {
                JobError::Unknown(
                    "jobs_list response budget is too small for page metadata; raise the response limit"
                        .to_owned(),
                )
            });
        }

        let mut best = None;
        for count in 1..=rows.len() {
            let candidate = render(count);
            if fits(&candidate) {
                best = Some(candidate);
            } else {
                break;
            }
        }
        best.ok_or_else(|| {
            JobError::Unknown(
                "jobs_list response budget is too small for one complete Job row; raise the response limit"
                    .to_owned(),
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn list(&self, offset: usize, limit: usize) -> String {
        self.list_fitted(offset, limit, |_| true)
            .expect("unbounded test Job list projection")
    }

    /// Reserve canonical shared user-visible capacity while the optimistic
    /// shell Job remains hidden. `NotFound` is retained only for pre-shared/test
    /// legacy entries; every current shell execution owns a shared projection.
    fn reserve_shared_exposure(&self, id: &str, shared_owned: bool) -> JobResult<()> {
        match self.inner.shared_jobs.reserve_exposed(id) {
            Ok(()) => Ok(()),
            Err(aw_tools::filesystem::job_manager::Error::NotFound) if !shared_owned => Ok(()),
            Err(aw_tools::filesystem::job_manager::Error::NotFound) => Err(JobError::Internal(
                anyhow!("shared-owned Job {id} disappeared before exposure reservation"),
            )),
            Err(aw_tools::filesystem::job_manager::Error::Capacity) => {
                Err(JobError::Internal(anyhow!(
                    "too many retained shell jobs ({MAX_RETAINED_JOBS}); acknowledge output or wait for retention cleanup"
                )))
            }
            Err(error) => Err(JobError::Internal(anyhow!(
                "shared Job exposure reservation failed for {id}: {error:?}"
            ))),
        }
    }

    fn expose_shared_job(&self, id: &str, shared_owned: bool) -> JobResult<()> {
        match self.inner.shared_jobs.expose(id) {
            Ok(()) => Ok(()),
            Err(aw_tools::filesystem::job_manager::Error::NotFound) if !shared_owned => Ok(()),
            Err(aw_tools::filesystem::job_manager::Error::NotFound) => Err(JobError::Internal(
                anyhow!("shared-owned Job {id} disappeared before exposure"),
            )),
            Err(error) => Err(JobError::Internal(anyhow!(
                "shared Job exposure failed for {id}: {error:?}"
            ))),
        }
    }

    pub(crate) fn expose_compatibility(&self, id: &str) -> JobResult<CallToolResult> {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let (disposition, shared_owned) = {
            let entry = state
                .jobs
                .get(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            (entry.disposition, entry.shared_job.is_some())
        };
        match disposition {
            JobDisposition::Pending => {
                reserve_exposure_slot(&mut state, id)?;
                self.reserve_shared_exposure(id, shared_owned)?;
                self.expose_shared_job(id, shared_owned)?;
                commit_exposure(&mut state, id, JobDisposition::Compatibility)?;
                let entry = state
                    .jobs
                    .get(id)
                    .expect("job validated before exposure capacity reservation");
                Ok(compatibility_receipt(&self.inner, entry))
            }
            JobDisposition::Compatibility => Ok(compatibility_receipt(
                &self.inner,
                state.jobs.get(id).expect("job validated above"),
            )),
            JobDisposition::Synchronous | JobDisposition::NativeTask => Err(JobError::Internal(
                anyhow!("job {id} already committed a different response representation"),
            )),
        }
    }

    /// Expose a job as a native MCP Task only when its exact eventual
    /// CallToolResult is guaranteed to fit the tightest task-result budget for the current request and every active transport sharing the runtime.
    /// An already-native task remains native on redelivery.
    #[cfg(test)]
    pub(crate) fn expose_native_task(
        &self,
        id: &str,
        budget: Option<usize>,
    ) -> JobResult<Option<Task>> {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .jobs
            .get(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        let shared_owned = entry.shared_job.is_some();
        match entry.disposition {
            JobDisposition::NativeTask => {
                return Ok(Some(task_from_entry(entry, self.retention())));
            }
            JobDisposition::Pending => {}
            JobDisposition::Synchronous | JobDisposition::Compatibility => return Ok(None),
        }
        if budget
            .is_some_and(|budget| !crate::mcp::shell_result_fits_budget(entry.result_limit, budget))
        {
            return Ok(None);
        }
        reserve_exposure_slot(&mut state, id)?;
        self.reserve_shared_exposure(id, shared_owned)?;
        self.expose_shared_job(id, shared_owned)?;
        commit_exposure(&mut state, id, JobDisposition::NativeTask)?;
        let entry = state
            .jobs
            .get(id)
            .expect("job validated before exposure capacity reservation");
        Ok(Some(task_from_entry(entry, self.retention())))
    }

    /// Atomically choose a durable response representation, or replay the one
    /// already committed by a concurrent/redelivered attempt. The first winner
    /// owns the wire contract for this execution; later callers cannot switch
    /// between synchronous, compatibility-job and native-task responses.
    pub(crate) fn handoff(
        &self,
        id: &str,
        native_tasks: bool,
        budget: Option<usize>,
    ) -> JobResult<JobHandoff> {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let (disposition, result_limit, shared_owned) = {
            let entry = state
                .jobs
                .get(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            (
                entry.disposition,
                entry.result_limit,
                entry.shared_job.is_some(),
            )
        };
        if disposition == JobDisposition::Pending {
            let native = native_tasks
                && !budget.is_some_and(|budget| {
                    !crate::mcp::shell_result_fits_budget(result_limit, budget)
                });
            reserve_exposure_slot(&mut state, id)?;
            self.reserve_shared_exposure(id, shared_owned)?;
            self.expose_shared_job(id, shared_owned)?;
            commit_exposure(
                &mut state,
                id,
                if native {
                    JobDisposition::NativeTask
                } else {
                    JobDisposition::Compatibility
                },
            )?;
        }
        let entry = state
            .jobs
            .get(id)
            .expect("job remains present after response disposition");
        Ok(handoff_from_entry(&self.inner, entry, self.retention()))
    }

    pub(crate) fn replay_handoff(&self, id: &str) -> JobResult<Option<JobHandoff>> {
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .jobs
            .get(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        if entry.disposition == JobDisposition::Pending {
            return Ok(None);
        }
        Ok(Some(handoff_from_entry(
            &self.inner,
            entry,
            self.retention(),
        )))
    }

    pub(crate) fn ensure_native_task(&self, id: &str) -> JobResult<()> {
        sweep_expired(&self.inner);
        let state = self.inner.state.lock().expect("job manager lock");
        match state.available(id) {
            Some(entry) if entry.disposition.native_task() => Ok(()),
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
                .available_mut(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            if !entry.status().terminal() {
                return Err(JobError::Internal(anyhow!("job {id} is still running")));
            }
            if job_owner_projection(entry, JobStream::Stdout)?.released
                || job_owner_projection(entry, JobStream::Stderr)?.released
            {
                return Ok(None);
            }
            entry.readers += 1;
            ToolResultSnapshot {
                id: entry.id.clone(),
                completion: entry
                    .completion
                    .clone()
                    .ok_or_else(|| JobError::Internal(anyhow!("job {id} has no completion")))?,
                timeout_secs: entry.timeout_secs,
                result_limit: entry.result_limit,
                output: tool_result_output(entry),
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
    pub(crate) fn mark_synchronous_result(&self, id: &str) -> JobResult<JobDisposition> {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let (status, disposition, shared_owned) = {
            let entry = state
                .jobs
                .get(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            (
                entry.status(),
                entry.disposition,
                entry.shared_job.is_some(),
            )
        };
        if !status.terminal() {
            return Err(JobError::Internal(anyhow!("job {id} is still running")));
        }
        if disposition != JobDisposition::Pending {
            return Ok(disposition);
        }
        if status == JobStatus::Unknown {
            // Preserve the exact first wire representation, but keep uncertain
            // effects discoverable and pageable for inspection. Unknown is
            // status evidence, not disposable synchronous output.
            reserve_exposure_slot(&mut state, id)?;
            self.reserve_shared_exposure(id, shared_owned)?;
            self.expose_shared_job(id, shared_owned)?;
            let entry = state
                .jobs
                .get_mut(id)
                .expect("job validated before unknown-result publication");
            entry.disposition = JobDisposition::Synchronous;
            if !release_shared_task_owner(entry) {
                entry.output_cleanup_pending = true;
            }
            entry.retention = RetentionClass::Ordinary;
            entry.retention_anchor = Some(Instant::now());
            entry.expiration_pending = false;
            return Ok(entry.disposition);
        }
        let entry = state
            .jobs
            .get_mut(id)
            .expect("job validated before synchronous publication");
        entry.disposition = JobDisposition::Synchronous;
        if !release_shared_task_owner(entry) {
            entry.output_cleanup_pending = true;
        }
        if entry.request_key.is_some() {
            entry.retention = RetentionClass::TransportOnly;
        } else {
            let _ = consume_complete_job_output(entry);
        }
        Ok(entry.disposition)
    }

    pub(crate) fn owns_request_key(&self, request_key: &str) -> bool {
        self.request_job_id(request_key).is_some()
    }

    pub(crate) fn request_job_id(&self, request_key: &str) -> Option<String> {
        let state = self.inner.state.lock().expect("job manager lock");
        let id = state.request_jobs.get(request_key)?;
        state
            .jobs
            .get(id)
            .is_some_and(|entry| entry.request_key.as_deref() == Some(request_key))
            .then(|| id.clone())
    }

    pub(crate) fn request_shell_guard_generation(&self, request_key: &str) -> Option<[u8; 32]> {
        let state = self.inner.state.lock().expect("job manager lock");
        let id = state.request_jobs.get(request_key)?;
        let entry = state.jobs.get(id)?;
        (entry.request_key.as_deref() == Some(request_key))
            .then_some(entry.shell_guard_generation)
            .flatten()
    }

    pub(crate) fn request_delivery_state(&self, request_key: &str) -> Option<RequestDeliveryState> {
        let state = self.inner.state.lock().expect("job manager lock");
        let id = state.request_jobs.get(request_key)?;
        let entry = state.jobs.get(id)?;
        if entry.request_key.as_deref() != Some(request_key) {
            return None;
        }
        Some(match entry.disposition {
            JobDisposition::Pending => RequestDeliveryState::Pending,
            JobDisposition::Synchronous => RequestDeliveryState::Synchronous,
            JobDisposition::Compatibility | JobDisposition::NativeTask => {
                RequestDeliveryState::Durable
            }
        })
    }

    /// Confirm that the transport accepted the terminal response for a durable
    /// request identity. This is the only point where OpenAI redelivery state
    /// becomes unnecessary. Hidden synchronous jobs release their spools and
    /// metadata once cleanup succeeds; exposed jobs keep their task/job state
    /// but no longer occupy the dedupe ledger.
    pub(crate) fn acknowledge_request_delivery(&self, request_key: &str) -> RequestDeliveryAck {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let Some(id) = state.request_jobs.get(request_key).cloned() else {
            return RequestDeliveryAck::Missing;
        };
        let Some(disposition) = state.jobs.get(&id).and_then(|entry| {
            (entry.request_key.as_deref() == Some(request_key)).then_some(entry.disposition)
        }) else {
            return RequestDeliveryAck::Missing;
        };
        if disposition == JobDisposition::Pending {
            // Delivery acknowledgement is valid only for a response representation
            // that was already committed under the manager lock. Never let a
            // transport caller turn a pending execution into an unkeyed hidden job.
            return RequestDeliveryAck::Pending;
        }

        state.request_jobs.remove(request_key);
        let mut remove_hidden = false;
        if let Some(entry) = state.jobs.get_mut(&id) {
            entry.request_key = None;
            if entry.disposition.exposed()
                || (entry.disposition == JobDisposition::Synchronous
                    && entry.status() == JobStatus::Unknown)
            {
                // Transport replay may have kept this handle alive long past its
                // ordinary TTL. Positive delivery starts a fresh user-visible
                // retention window. Unknown synchronous results remain an
                // inspection surface instead of becoming cleanup-only state.
                entry.retention = RetentionClass::Ordinary;
                entry.retention_anchor = Some(Instant::now());
                entry.expiration_pending = false;
            } else if entry.status().terminal() {
                entry.retention = RetentionClass::CleanupOnly;
                if consume_complete_job_output(entry) {
                    remove_hidden = entry.all_output_released() && entry.readers == 0;
                }
            }
        }
        if remove_hidden {
            let _ = cleanup_and_remove_job_locked(&mut state, &id);
        }
        RequestDeliveryAck::Acknowledged
    }

    /// Revoke an undelivered hidden execution after its live shell guard
    /// generation is no longer authorized. Already-exposed jobs are never
    /// affected. A still-running hidden job is cancellation-requested and keeps
    /// its request key until it becomes terminal, so cleanup cannot race output.
    pub(crate) fn discard_hidden_request_delivery(&self, request_key: &str) -> RequestRetirement {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let Some(id) = state.request_jobs.get(request_key).cloned() else {
            return RequestRetirement::Missing;
        };
        let Some(entry) = state.jobs.get_mut(&id) else {
            return RequestRetirement::Missing;
        };
        if entry.request_key.as_deref() != Some(request_key) || entry.disposition.exposed() {
            return RequestRetirement::Missing;
        }
        if entry.response_attempts != 0 {
            return RequestRetirement::Busy;
        }
        if !entry.status().terminal() {
            entry.cancel.cancel_as(CancelOrigin::Connector);
            return RequestRetirement::Busy;
        }

        state.request_jobs.remove(request_key);
        let entry = state
            .jobs
            .get_mut(&id)
            .expect("request mapping validated above");
        entry.request_key = None;
        entry.retention = RetentionClass::CleanupOnly;
        let _ = consume_complete_job_output(entry);
        let remove_hidden = entry.all_output_released() && entry.readers == 0;
        if remove_hidden {
            let _ = cleanup_and_remove_job_locked(&mut state, &id);
        }
        RequestRetirement::Retired
    }

    /// The hosted transport can no longer redeliver this request identity. Make
    /// any still-hidden execution user-discoverable, then release the transport
    /// dedupe key so ordinary retention can bound its remaining lifetime.
    pub(crate) fn retire_request_delivery(&self, request_key: &str) -> RequestRetirement {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let Some(id) = state.request_jobs.get(request_key).cloned() else {
            return RequestRetirement::Missing;
        };
        let Some(entry) = state.jobs.get(&id) else {
            return RequestRetirement::Missing;
        };
        if entry.request_key.as_deref() != Some(request_key) {
            return RequestRetirement::Missing;
        }
        if entry.response_attempts != 0 {
            return RequestRetirement::Busy;
        }
        let disposition = entry.disposition;
        let shared_owned = entry.shared_job.is_some();

        let needs_ordinary_slot = state.jobs.get(&id).is_some_and(|entry| {
            matches!(
                entry.disposition,
                JobDisposition::Pending | JobDisposition::Synchronous
            ) && entry.retention != RetentionClass::Ordinary
        });
        if needs_ordinary_slot && !ensure_retained_capacity(&mut state, 1) {
            return RequestRetirement::Busy;
        }
        if matches!(
            disposition,
            JobDisposition::Pending | JobDisposition::Synchronous
        ) && (self.reserve_shared_exposure(&id, shared_owned).is_err()
            || self.expose_shared_job(&id, shared_owned).is_err())
        {
            return RequestRetirement::Busy;
        }

        let newly_exposed = matches!(
            disposition,
            JobDisposition::Pending | JobDisposition::Synchronous
        );
        if newly_exposed && commit_exposure(&mut state, &id, JobDisposition::Compatibility).is_err()
        {
            return RequestRetirement::Busy;
        }

        state.request_jobs.remove(request_key);
        let entry = state
            .jobs
            .get_mut(&id)
            .expect("request mapping validated above");
        entry.request_key = None;
        if !newly_exposed {
            // The handle already existed; retirement merely starts a fresh
            // user-visible retention window from the point transport replay
            // ceased to be possible.
            entry.retention_anchor = Some(Instant::now());
            entry.expiration_pending = false;
        }
        RequestRetirement::Retired
    }

    pub(crate) fn output_page(
        &self,
        id: &str,
        stream: JobStream,
        offset: u64,
        requested_bytes: RawPageBytes,
        response_ceiling: SerializedResultBytes,
        base64: bool,
    ) -> JobResult<String> {
        sweep_expired(&self.inner);
        enum Source {
            Shared {
                manager: Arc<SharedJobManager>,
                terminal: bool,
                _lease: JobReadLease,
            },
            Legacy {
                snapshot: OutputSnapshot,
                _lease: JobReadLease,
            },
        }
        let source = {
            let mut state = self.inner.state.lock().expect("job manager lock");
            let entry = state
                .available_mut(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            let summary = entry.completion.as_ref().map(|completion| match stream {
                JobStream::Stdout => completion.stdout,
                JobStream::Stderr => completion.stderr,
            });
            let owner_projection = job_owner_projection(entry, stream)?;
            let mirrored_stored = if entry.shared_job.is_some() {
                owner_projection.retained
            } else {
                summary.map_or_else(
                    || stored_len(stream_path(entry, stream), None),
                    |_| owner_projection.retained,
                )
            };
            if owner_projection.released {
                if offset > mirrored_stored {
                    return Err(JobError::Internal(anyhow!(
                        "job {id} {} offset {offset} is beyond stored output ({mirrored_stored} bytes)",
                        stream.label()
                    )));
                }
                if offset == mirrored_stored {
                    return OutputPageRender {
                        id,
                        node: None,
                        stream: stream.label(),
                        offset,
                        stored_bytes: mirrored_stored,
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
            let lease = JobReadLease {
                inner: Arc::downgrade(&self.inner),
                id: id.to_owned(),
            };
            if let Some(shared) = entry.shared_job.as_ref() {
                Source::Shared {
                    manager: shared.manager.clone(),
                    terminal: entry.status().terminal(),
                    _lease: lease,
                }
            } else {
                if offset > mirrored_stored {
                    return Err(JobError::Internal(anyhow!(
                        "job {id} {} offset {offset} is beyond stored output ({mirrored_stored} bytes)",
                        stream.label()
                    )));
                }
                Source::Legacy {
                    snapshot: OutputSnapshot {
                        path: stream_path(entry, stream).to_path_buf(),
                        terminal: entry.status().terminal(),
                        stored: mirrored_stored,
                        total: summary.map(|value| value.total_bytes),
                        truncated: summary.is_some_and(|value| value.truncated),
                        released: false,
                    },
                    _lease: lease,
                }
            }
        };

        let read_cap = requested_bytes.get();
        let (bytes, snapshot) = match source {
            Source::Shared {
                manager,
                terminal,
                _lease,
            } => {
                let page = manager
                    .output_page(id, 1, shared_output_stream(stream), offset, read_cap)
                    .map_err(|error| {
                        JobError::Internal(anyhow!(
                            "shared job {id} {} page failed: {error:?}",
                            stream.label()
                        ))
                    })?;
                (
                    page.bytes,
                    OutputSnapshot {
                        path: PathBuf::new(),
                        terminal,
                        stored: page.retained_bytes,
                        total: Some(page.total_bytes),
                        truncated: page.truncated,
                        released: false,
                    },
                )
            }
            Source::Legacy { snapshot, _lease } => {
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
                (bytes, snapshot)
            }
        };
        fit_output_page(
            id,
            stream,
            offset,
            &bytes,
            &snapshot,
            response_ceiling,
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
        sweep_expired(&self.inner);
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available_mut(id)
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
        if let Some(shared) = entry.shared_job.as_ref() {
            let owner = shared.manager.job_output_owner(id).map_err(|error| {
                JobError::Internal(anyhow!(
                    "cannot derive shared Job output owner for {id}: {error:?}"
                ))
            })?;
            match shared.manager.acknowledge_output(
                id,
                1,
                shared_output_stream(stream),
                owner,
                offset,
            ) {
                Ok(_) | Err(aw_tools::filesystem::job_manager::Error::OutputReleased) => {}
                Err(error) => {
                    return Err(JobError::Internal(anyhow!(
                        "shared Job output acknowledgement failed for {id}: {error:?}"
                    )));
                }
            }
        } else {
            match stream {
                JobStream::Stdout => entry.stdout_consumed = entry.stdout_consumed.max(offset),
                JobStream::Stderr => entry.stderr_consumed = entry.stderr_consumed.max(offset),
            }
        }
        entry.output_cleanup_requested = true;
        maybe_release_output(entry);
        Ok(render_status_locked(&self.inner, entry))
    }

    fn resolve_output_target(
        &self,
        id: &str,
        node: Option<usize>,
        stream: Option<JobOutputStream>,
    ) -> JobResult<(Option<Arc<SharedJobManager>>, usize, JobOutputStream)> {
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        let Some(shared) = entry.shared_job.as_ref() else {
            let coordinate = node.unwrap_or(1);
            if coordinate != 1 {
                return Err(JobError::Unknown(format!(
                    "legacy shell job {id} has only node 1"
                )));
            }
            let selected = stream.unwrap_or(JobOutputStream::Stdout);
            if selected.legacy().is_none() {
                return Err(JobError::Unknown(format!(
                    "legacy shell job {id} has no retained result stream"
                )));
            }
            return Ok((None, 1, selected));
        };
        let graph = shared.manager.graph(id).map_err(|error| {
            JobError::Internal(anyhow!(
                "cannot inspect shared Job Graph for {id}: {error:?}"
            ))
        })?;
        let total = graph.steps().len();
        let coordinate = match node {
            Some(value) if (1..=total).contains(&value) => value,
            Some(_) => {
                return Err(JobError::Unknown(format!(
                    "job {id} node is outside 1..={total}"
                )));
            }
            None if total == 1 => 1,
            None => {
                return Err(JobError::Unknown(format!(
                    "job {id} has {total} nodes; specify node"
                )));
            }
        };
        let tool = graph.steps()[coordinate - 1].tool;
        let shell = tool.native().is_none();
        let selected = stream.unwrap_or(if shell {
            JobOutputStream::Stdout
        } else {
            JobOutputStream::Result
        });
        if !shell && selected != JobOutputStream::Result {
            return Err(JobError::Unknown(format!(
                "native job node {coordinate} exposes only result"
            )));
        }
        Ok((Some(shared.manager.clone()), coordinate, selected))
    }

    /// Graph-aware output projection. Existing one-shell compatibility entries
    /// retain their old stdout/stderr behavior; shared Jobs use structural node
    /// selection and the canonical shared output streams.
    fn standalone_output_ready(
        &self,
        id: &str,
        node: Option<usize>,
        stream: Option<JobOutputStream>,
        offset: u64,
    ) -> JobResult<bool> {
        let (shared, coordinate, selected) = self.resolve_output_target(id, node, stream)?;
        if let Some(manager) = shared {
            let settled = || -> JobResult<bool> {
                if let Some(node) = node {
                    return manager
                        .node_state(id, node)
                        .map(|state| matches!(state, SharedNodeState::Terminal { .. }))
                        .map_err(|error| {
                            JobError::Unknown(format!("unavailable Graph node: {error:?}"))
                        });
                }
                let state = self.inner.state.lock().expect("job manager lock");
                let entry = state
                    .available(id)
                    .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
                Ok(entry.status().terminal())
            };
            return match manager.output_summary(id, coordinate, selected.shared()) {
                Ok((summary, writer_finished)) => {
                    if summary.retained_bytes > offset || summary.truncated || writer_finished {
                        Ok(true)
                    } else {
                        settled()
                    }
                }
                // A skipped/failed Graph node may have no stream at all.
                // Its own terminal state ends the wait, not a slow sibling.
                Err(aw_tools::filesystem::job_manager::Error::OutputMissing) => settled(),
                Err(aw_tools::filesystem::job_manager::Error::OutputReleased) => Ok(true),
                Err(error) => Err(JobError::Internal(anyhow!(
                    "cannot observe job {id} output before wait: {error:?}"
                ))),
            };
        }

        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        let legacy = selected
            .legacy()
            .ok_or_else(|| JobError::Unknown("legacy Process has no result stream".into()))?;
        let stored = entry.completion.as_ref().map_or_else(
            || stored_len(stream_path(entry, legacy), None),
            |completion| match legacy {
                JobStream::Stdout => completion.stdout.stored_bytes,
                JobStream::Stderr => completion.stderr.stored_bytes,
            },
        );
        let truncated = entry
            .completion
            .as_ref()
            .is_some_and(|completion| match legacy {
                JobStream::Stdout => completion.stdout.truncated,
                JobStream::Stderr => completion.stderr.truncated,
            });
        Ok(stored > offset || truncated || entry.status().terminal())
    }

    pub(crate) fn standalone_is_graph(&self, id: &str) -> JobResult<bool> {
        sweep_expired(&self.inner);
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        Ok(entry.shared_graph_authoritative)
    }

    fn record_standalone_delivery(
        &self,
        id: &str,
        coordinate: usize,
        stream: JobOutputStream,
        rendered: &str,
    ) -> JobResult<()> {
        let page: serde_json::Value = serde_json::from_str(rendered).map_err(|error| {
            JobError::Internal(anyhow!(
                "cannot inspect rendered jobs_read page for delivery fence: {error}"
            ))
        })?;
        let start = page
            .get("offset")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| JobError::Internal(anyhow!("jobs_read page is missing raw offset")))?;
        let through = page
            .get("next_offset")
            .and_then(serde_json::Value::as_u64)
            .filter(|through| *through >= start)
            .ok_or_else(|| {
                JobError::Internal(anyhow!(
                    "jobs_read page has invalid next_offset delivery evidence"
                ))
            })?;
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available_mut(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        use std::collections::hash_map::Entry;
        // Only a contiguous, *observed* prefix is ACKable. An out-of-order
        // page does not certify any prefix, including an empty offset-0 ACK.
        // Retire this host-side fence after adopting AW's reviewed owner proof.
        match entry.delivered_through.entry((coordinate, stream)) {
            Entry::Vacant(vacant) if start == 0 => {
                vacant.insert(through);
            }
            Entry::Occupied(mut present) if start <= *present.get() => {
                *present.get_mut() = (*present.get()).max(through);
            }
            _ => {}
        }
        Ok(())
    }

    fn require_standalone_delivery(
        &self,
        id: &str,
        coordinate: usize,
        stream: JobOutputStream,
        through: u64,
    ) -> JobResult<()> {
        let state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        let delivered = entry
            .delivered_through
            .get(&(coordinate, stream))
            .copied()
            .ok_or_else(|| {
                if through == 0 {
                    JobError::Unknown(
                        "jobs_ack offset 0 requires a projected jobs_read page at offset 0".into(),
                    )
                } else {
                    JobError::Unknown(format!(
                        "jobs_ack offset {through} exceeds bytes delivered by jobs_read (0)"
                    ))
                }
            })?;
        if through > delivered {
            return Err(JobError::Unknown(format!(
                "jobs_ack offset {through} exceeds bytes delivered by jobs_read ({delivered})"
            )));
        }
        Ok(())
    }

    pub(crate) fn standalone_graph_result_page(
        &self,
        id: &str,
        offset: u64,
        requested_bytes: RawPageBytes,
        response_ceiling: SerializedResultBytes,
        base64: bool,
    ) -> JobResult<String> {
        sweep_expired(&self.inner);
        {
            let state = self.inner.state.lock().expect("job manager lock");
            let entry = state
                .available(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            if !entry.shared_graph_authoritative {
                return Err(JobError::Unknown(
                    "Process Job root has no synthesized Graph result".into(),
                ));
            }
        }
        let render_limit = 16 * 1024 * 1024;
        let rendered = self
            .inner
            .shared_jobs
            .render_graph_result(id, render_limit)
            .map_err(|error| match error {
                aw_tools::filesystem::job_manager::Error::Busy => {
                    JobError::Unknown("Graph result is not terminal".into())
                }
                other => JobError::Internal(anyhow!("Graph result projection failed: {other:?}")),
            })?
            .ok_or_else(|| {
                JobError::Unknown(
                    "Graph aggregate result is too large; read exact n:<coordinate> refs".into(),
                )
            })?;
        let bytes = rendered.as_bytes();
        let start = usize::try_from(offset)
            .ok()
            .filter(|offset| *offset <= bytes.len())
            .ok_or_else(|| JobError::Unknown("jobs_read offset is past Graph result".into()))?;
        let end = start.saturating_add(requested_bytes.get()).min(bytes.len());
        let snapshot = OutputSnapshot {
            path: PathBuf::new(),
            terminal: true,
            stored: bytes.len() as u64,
            total: Some(bytes.len() as u64),
            truncated: false,
            released: false,
        };
        let render = OutputPageRender {
            id,
            node: None,
            stream: JobOutputStream::Result.label(),
            offset,
            stored_bytes: snapshot.stored,
            total_bytes: snapshot.total,
            truncated: false,
            released: false,
            base64,
        };
        fit_shared_output_page(&render, &bytes[start..end], &snapshot, response_ceiling)
            .map(|(rendered, _)| rendered)
            .map_err(JobError::Internal)
    }

    pub(crate) async fn standalone_graph_result_page_wait(
        &self,
        id: &str,
        offset: u64,
        requested_bytes: RawPageBytes,
        response_ceiling: SerializedResultBytes,
        base64: bool,
        wait_ms: u64,
    ) -> JobResult<String> {
        if wait_ms > DOTLINK_JOB_PROFILE.read_wait_max_ms {
            return Err(JobError::Unknown(format!(
                "jobs_read wait_ms exceeds {}",
                DOTLINK_JOB_PROFILE.read_wait_max_ms
            )));
        }
        if wait_ms == 0 {
            return self.standalone_graph_result_page(
                id,
                offset,
                requested_bytes,
                response_ceiling,
                base64,
            );
        }
        let deadline = TokioInstant::now()
            .checked_add(Duration::from_millis(wait_ms))
            .ok_or_else(|| JobError::Unknown("jobs_read wait window overflow".into()))?;
        loop {
            let (notify, terminal) = {
                sweep_expired(&self.inner);
                let state = self.inner.state.lock().expect("job manager lock");
                let entry = state
                    .available(id)
                    .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
                if !entry.shared_graph_authoritative {
                    return Err(JobError::Unknown(
                        "Process Job root has no synthesized Graph result".into(),
                    ));
                }
                (entry.notify.clone(), entry.status().terminal())
            };
            if terminal {
                return self.standalone_graph_result_page(
                    id,
                    offset,
                    requested_bytes,
                    response_ceiling,
                    base64,
                );
            }
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let terminal_after_subscribe = {
                let state = self.inner.state.lock().expect("job manager lock");
                state
                    .available(id)
                    .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?
                    .status()
                    .terminal()
            };
            if terminal_after_subscribe {
                continue;
            }
            if TokioInstant::now() >= deadline
                || tokio::time::timeout_at(deadline, notified.as_mut())
                    .await
                    .is_err()
            {
                return self.standalone_graph_result_page(
                    id,
                    offset,
                    requested_bytes,
                    response_ceiling,
                    base64,
                );
            }
        }
    }

    pub(crate) async fn output_node_page_wait(
        &self,
        id: &str,
        request: OutputPageRequest,
        wait_ms: u64,
    ) -> JobResult<String> {
        if wait_ms > DOTLINK_JOB_PROFILE.read_wait_max_ms {
            return Err(JobError::Unknown(format!(
                "jobs_read wait_ms exceeds {}",
                DOTLINK_JOB_PROFILE.read_wait_max_ms
            )));
        }
        if wait_ms == 0 {
            return self.output_node_page(id, request);
        }
        let deadline = TokioInstant::now()
            .checked_add(Duration::from_millis(wait_ms))
            .ok_or_else(|| JobError::Unknown("jobs_read wait window overflow".into()))?;
        loop {
            let notify = {
                sweep_expired(&self.inner);
                let state = self.inner.state.lock().expect("job manager lock");
                state
                    .available(id)
                    .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?
                    .notify
                    .clone()
            };
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.standalone_output_ready(id, request.node, request.stream, request.offset)? {
                return self.output_node_page(id, request);
            }
            if TokioInstant::now() >= deadline {
                return self.output_node_page(id, request);
            }
            if tokio::time::timeout_at(deadline, notified.as_mut())
                .await
                .is_err()
            {
                return self.output_node_page(id, request);
            }
        }
    }

    pub(crate) fn output_node_page(
        &self,
        id: &str,
        request: OutputPageRequest,
    ) -> JobResult<String> {
        let OutputPageRequest {
            node,
            stream,
            offset,
            requested_bytes,
            response_ceiling,
            base64,
        } = request;
        sweep_expired(&self.inner);
        let (shared, coordinate, selected) = self.resolve_output_target(id, node, stream)?;
        let Some(manager) = shared else {
            let rendered = self.output_page(
                id,
                selected.legacy().expect("legacy target validated"),
                offset,
                requested_bytes,
                response_ceiling,
                base64,
            )?;
            self.record_standalone_delivery(id, coordinate, selected, &rendered)?;
            return Ok(rendered);
        };
        let owner = manager.job_output_owner(id).map_err(|error| {
            JobError::Internal(anyhow!(
                "cannot derive shared Job output owner for {id}: {error:?}"
            ))
        })?;
        let owner_state = match manager.output_owner_state(id, coordinate, selected.shared(), owner)
        {
            Ok(state) => state,
            Err(aw_tools::filesystem::job_manager::Error::OutputMissing) => {
                let absent_after_settlement = matches!(
                    manager.node_state(id, coordinate),
                    Ok(SharedNodeState::Terminal { .. })
                );
                let detail = if absent_after_settlement {
                    "has no retained output after settlement"
                } else {
                    "output is not available yet"
                };
                return Err(JobError::Unknown(format!(
                    "job {id} node {coordinate} {} {detail}",
                    selected.label()
                )));
            }
            Err(error) => {
                return Err(JobError::Internal(anyhow!(
                    "cannot inspect job {id} node {coordinate} {} output owner: {error:?}",
                    selected.label()
                )));
            }
        };
        let (summary, writer_finished) = match manager.output_summary(
            id,
            coordinate,
            selected.shared(),
        ) {
            Ok(value) => value,
            Err(aw_tools::filesystem::job_manager::Error::OutputReleased) => {
                if offset == owner_state.through {
                    return OutputPageRender {
                        id,
                        node: Some(coordinate),
                        stream: selected.label(),
                        offset,
                        stored_bytes: owner_state.through,
                        total_bytes: None,
                        truncated: false,
                        released: true,
                        base64,
                    }
                    .render(offset, true, &[])
                    .map_err(JobError::Internal);
                }
                return Err(JobError::Unknown(format!(
                    "job {id} node {coordinate} {} output was already acknowledged and released",
                    selected.label()
                )));
            }
            Err(error) => {
                return Err(JobError::Internal(anyhow!(
                    "cannot inspect job {id} node {coordinate} {} output: {error:?}",
                    selected.label()
                )));
            }
        };
        if offset > summary.retained_bytes {
            return Err(JobError::Unknown(format!(
                "job {id} node {coordinate} {} offset {offset} exceeds retained output {}",
                selected.label(),
                summary.retained_bytes
            )));
        }
        let page = manager
            .output_page_for_owner(
                id,
                coordinate,
                selected.shared(),
                owner,
                offset,
                requested_bytes.get(),
            )
            .map_err(|error| {
                JobError::Internal(anyhow!(
                    "job {id} node {coordinate} {} page failed: {error:?}",
                    selected.label()
                ))
            })?;
        let snapshot = OutputSnapshot {
            path: PathBuf::new(),
            terminal: writer_finished,
            stored: page.retained_bytes,
            total: Some(page.total_bytes),
            truncated: page.truncated,
            released: owner_state.released,
        };
        let render = OutputPageRender {
            id,
            node: Some(coordinate),
            stream: selected.label(),
            offset,
            stored_bytes: snapshot.stored,
            total_bytes: snapshot.total,
            truncated: snapshot.truncated,
            released: snapshot.released,
            base64,
        };
        let (rendered, credited) =
            fit_shared_output_page(&render, &page.bytes, &snapshot, response_ceiling)
                .map_err(JobError::Internal)?;
        manager
            .record_delivered_output(
                id,
                coordinate,
                selected.shared(),
                owner,
                page.offset,
                credited,
            )
            .map_err(|error| {
                JobError::Internal(anyhow!(
                    "cannot record delivered job {id} node {coordinate} {} prefix: {error:?}",
                    selected.label()
                ))
            })?;
        Ok(rendered)
    }

    pub(crate) fn acknowledge_node_output(
        &self,
        id: &str,
        node: Option<usize>,
        stream: Option<JobOutputStream>,
        offset: u64,
    ) -> JobResult<SharedOutputAck> {
        sweep_expired(&self.inner);
        if node.is_none() && self.standalone_is_graph(id)? {
            return Err(JobError::Unknown(
                "Graph root result is a synthesized aggregate; ACK an exact j:<handle>/n:<coordinate> retained result instead".into(),
            ));
        }
        let (shared, coordinate, selected) = self.resolve_output_target(id, node, stream)?;
        let Some(manager) = shared else {
            self.require_standalone_delivery(id, coordinate, selected, offset)?;
            let legacy = selected.legacy().expect("legacy target validated");
            let before = {
                let state = self.inner.state.lock().expect("job manager lock");
                let entry = state
                    .available(id)
                    .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
                match legacy {
                    JobStream::Stdout => entry.stdout_consumed,
                    JobStream::Stderr => entry.stderr_consumed,
                }
            };
            self.acknowledge_output(id, legacy, offset)?;
            let released = {
                let state = self.inner.state.lock().expect("job manager lock");
                let entry = state
                    .available(id)
                    .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
                match legacy {
                    JobStream::Stdout => entry.stdout_released,
                    JobStream::Stderr => entry.stderr_released,
                }
            };
            return Ok(SharedOutputAck {
                changed: offset > before,
                released,
            });
        };
        let owner = manager.job_output_owner(id).map_err(|error| {
            JobError::Internal(anyhow!(
                "cannot derive shared Job output owner for {id}: {error:?}"
            ))
        })?;
        let ack = match manager.acknowledge_delivered_output(
            id,
            coordinate,
            selected.shared(),
            owner,
            offset,
        ) {
            Ok(ack) => ack,
            Err(aw_tools::filesystem::job_manager::Error::OutputReleased) => SharedOutputAck {
                changed: false,
                released: true,
            },
            Err(aw_tools::filesystem::job_manager::Error::InvalidOutput) => {
                return Err(JobError::Unknown(format!(
                    "jobs_ack offset {offset} exceeds bytes delivered by jobs_read for this exact output owner"
                )));
            }
            Err(aw_tools::filesystem::job_manager::Error::Busy) => {
                return Err(JobError::Unknown(
                    "Job output acknowledgement is blocked by an unresolved persistence transition"
                        .into(),
                ));
            }
            Err(aw_tools::filesystem::job_manager::Error::ReplayUnknown) => {
                return Err(JobError::Unknown(
                    "Job effect is unknown; reconcile before acknowledging output".into(),
                ));
            }
            Err(error) => {
                return Err(JobError::Internal(anyhow!(
                    "job {id} node {coordinate} {} acknowledgement failed: {error:?}",
                    selected.label()
                )));
            }
        };
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available_mut(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        entry.output_cleanup_requested = true;
        maybe_release_output(entry);
        Ok(ack)
    }

    pub(crate) fn finish_node_output(
        &self,
        id: &str,
        node: Option<usize>,
        stream: Option<JobOutputStream>,
    ) -> JobResult<SharedOutputAck> {
        sweep_expired(&self.inner);
        let (manager, graph) = {
            let state = self.inner.state.lock().expect("job manager lock");
            let entry = state
                .available(id)
                .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
            let shared = entry.shared_job.as_ref().ok_or_else(|| {
                JobError::Unknown("terminal Finish requires canonical shared Job output".into())
            })?;
            (shared.manager.clone(), entry.shared_graph_authoritative)
        };
        if graph && node.is_none() && stream.is_some() {
            return Err(JobError::Unknown(
                "Graph root Finish cannot select one stream; finish the root or an exact node"
                    .into(),
            ));
        }
        let owner = manager.job_output_owner(id).map_err(|error| {
            JobError::Internal(anyhow!(
                "cannot derive shared Job output owner for {id}: {error:?}"
            ))
        })?;
        let ack = match manager.finish_output_for_owner(
            id,
            node,
            stream.map(JobOutputStream::shared),
            owner,
        ) {
            Ok(ack) => ack,
            Err(aw_tools::filesystem::job_manager::Error::Busy) => {
                return Err(JobError::Unknown(
                    "Job output is not yet terminal; inspect with jobs_get before finishing".into(),
                ));
            }
            Err(aw_tools::filesystem::job_manager::Error::ReplayUnknown) => {
                return Err(JobError::Unknown(
                    "Job effect is unknown; reconcile before finishing output".into(),
                ));
            }
            Err(error) => {
                return Err(JobError::Internal(anyhow!(
                    "job {id} output Finish failed: {error:?}"
                )));
            }
        };
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available_mut(id)
            .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
        entry.output_cleanup_requested = true;
        maybe_release_output(entry);
        Ok(ack)
    }

    fn finalize_task_result_ts(&self, id: &str) -> JobResult<u64> {
        let mut state = self.inner.state.lock().expect("job manager lock");
        let entry = state
            .available_mut(id)
            .filter(|entry| entry.disposition.native_task())
            .ok_or_else(|| JobError::Unknown(format!("unknown task {id}")))?;
        if let Some(ts) = entry.task_result_ts {
            return Ok(ts);
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                JobError::Internal(anyhow!(
                    "cannot finalize native Task result timestamp: {error}"
                ))
            })?
            .as_secs();
        entry.task_result_ts = Some(ts);
        Ok(ts)
    }

    fn task_receipt_operation(&self, id: &str) -> JobResult<OperationId> {
        let shared_owned = {
            let state = self.inner.state.lock().expect("job manager lock");
            state
                .available(id)
                .filter(|entry| entry.disposition.native_task())
                .ok_or_else(|| JobError::Unknown(format!("unknown task {id}")))?
                .shared_job
                .is_some()
        };
        if !shared_owned {
            // Legacy/test-only native Tasks predate the shared Process Graph.
            return Ok(OperationId::Bash);
        }
        let graph = self.inner.shared_jobs.graph(id).map_err(|error| {
            JobError::Internal(anyhow!(
                "cannot resolve native Task receipt operation for {id}: {error:?}"
            ))
        })?;
        let operation = graph
            .steps()
            .first()
            .map(|step| step.tool.operation())
            .ok_or_else(|| JobError::Internal(anyhow!("native Task {id} has no Process node")))?;
        if !matches!(
            operation,
            OperationId::Bash | OperationId::Pwsh | OperationId::Powershell
        ) {
            return Err(JobError::Internal(anyhow!(
                "native Task {id} receipt operation is not a Process tool"
            )));
        }
        Ok(operation)
    }

    pub(crate) fn detailed_task(&self, id: &str) -> JobResult<DetailedTask> {
        sweep_expired(&self.inner);
        let (task, status, internal_error, completion, snapshot) = {
            let mut state = self.inner.state.lock().expect("job manager lock");
            let entry = state
                .available_mut(id)
                .filter(|entry| entry.disposition.native_task())
                .ok_or_else(|| JobError::Unknown(format!("unknown task {id}")))?;
            let task = task_from_entry(entry, self.retention());
            let status = entry.status();
            let completion = entry.completion.clone();
            let internal_error = completion
                .as_ref()
                .and_then(|completion| completion.internal_error.clone());
            let snapshot = if matches!(
                status,
                JobStatus::Completed | JobStatus::TimedOut | JobStatus::Failed
            ) && internal_error.is_none()
            {
                entry.readers += 1;
                Some(ToolResultSnapshot {
                    id: entry.id.clone(),
                    completion: entry.completion.clone().ok_or_else(|| {
                        JobError::Internal(anyhow!("task {id} has no completion"))
                    })?,
                    timeout_secs: entry.timeout_secs,
                    result_limit: entry.result_limit,
                    output: tool_result_output(entry),
                    _lease: JobReadLease {
                        inner: Arc::downgrade(&self.inner),
                        id: id.to_owned(),
                    },
                })
            } else {
                None
            };
            (task, status, internal_error, completion, snapshot)
        };
        let payload = match status {
            JobStatus::Working => TaskPayload::Working,
            JobStatus::Cancelled => TaskPayload::Cancelled,
            JobStatus::Unknown => {
                let message = internal_error
                    .as_deref()
                    .or_else(|| completion.as_ref().and_then(|value| value.error.as_deref()))
                    .unwrap_or("shell effect state is unknown; inspect before retrying");
                TaskPayload::Failed {
                    error: completion.as_ref().map_or_else(
                        || error_object(message),
                        |completion| failure_error_object(message, completion),
                    ),
                }
            }
            JobStatus::Completed | JobStatus::TimedOut | JobStatus::Failed => {
                if let Some(message) = internal_error {
                    TaskPayload::Failed {
                        error: completion.as_ref().map_or_else(
                            || error_object(&message),
                            |completion| failure_error_object(&message, completion),
                        ),
                    }
                } else {
                    let result = crate::blocking::run(|| {
                        render_tool_result(snapshot.as_ref().expect("terminal task snapshot"))
                    })
                    .map_err(JobError::Internal)?;
                    let ts = self.finalize_task_result_ts(id)?;
                    let operation = self.task_receipt_operation(id)?;
                    let result = crate::mcp::decorate_native_task_result(operation, result, ts)
                        .map_err(|error| JobError::Internal(anyhow!(error.to_string())))?;
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

enum ToolResultOutput {
    Shared {
        manager: Arc<SharedJobManager>,
        id: String,
    },
    Legacy {
        stdout_path: PathBuf,
        stderr_path: PathBuf,
    },
}

struct ToolResultSnapshot {
    id: String,
    completion: JobCompletion,
    timeout_secs: u64,
    result_limit: usize,
    output: ToolResultOutput,
    _lease: JobReadLease,
}

fn tool_result_output(entry: &JobEntry) -> ToolResultOutput {
    match entry.shared_job.as_ref() {
        Some(shared) => ToolResultOutput::Shared {
            manager: shared.manager.clone(),
            id: entry.id.clone(),
        },
        None => ToolResultOutput::Legacy {
            stdout_path: entry.stdout_path.clone(),
            stderr_path: entry.stderr_path.clone(),
        },
    }
}

pub(crate) fn product_stopped_by(completion: &JobCompletion) -> Option<&'static str> {
    let reason = completion.stop_record?.terminal_reason()?;
    Some(match reason {
        SharedProcessStopReason::Timeout => "timeout",
        SharedProcessStopReason::RuntimeError => "failed",
        SharedProcessStopReason::Cancelled => {
            match completion.cancel_origin.unwrap_or(CancelOrigin::Caller) {
                CancelOrigin::Caller => "cancel",
                CancelOrigin::Connector => "shutdown",
            }
        }
    })
}

pub(crate) fn external_signal(completion: &JobCompletion) -> Option<i32> {
    let signal = completion.signal()?;
    completion
        .stop_record
        .is_none_or(|record| record.reason.is_none())
        .then_some(signal)
}

pub(crate) fn signal_label(signal: i32) -> String {
    match aw_platform::signal_name(signal) {
        Some(name) => format!("{name} (signal {signal})"),
        None => format!("signal {signal}"),
    }
}

pub(crate) fn signal_token(signal: i32) -> String {
    aw_platform::signal_name(signal)
        .map(str::to_owned)
        .unwrap_or_else(|| signal.to_string())
}

fn external_stop_note(completion: &JobCompletion) -> Option<String> {
    let signal = external_signal(completion)?;
    Some(format!(
        "terminated by {} from outside dotlink; check what signalled it before retrying",
        signal_label(signal)
    ))
}

fn with_termination(
    mut payload: serde_json::Value,
    completion: &JobCompletion,
) -> serde_json::Value {
    if let Some(signal) = completion.signal() {
        payload["signal"] = serde_json::Value::from(signal);
    }
    if let Some(stopped_by) = product_stopped_by(completion) {
        payload["stopped_by"] = serde_json::Value::from(stopped_by);
    }
    payload
}

pub(crate) fn process_lifecycle_message(id: &str, completion: &JobCompletion) -> Option<String> {
    if let Some(record) = completion.stop_record
        && let Some(reason) = record.reason
    {
        let reason = match reason {
            SharedProcessStopReason::Timeout => "timeout",
            SharedProcessStopReason::RuntimeError => "failed",
            SharedProcessStopReason::Cancelled => {
                match completion.cancel_origin.unwrap_or(CancelOrigin::Caller) {
                    CancelOrigin::Caller => "cancel",
                    CancelOrigin::Connector => "shutdown",
                }
            }
        };
        let outcome = match record.evidence {
            aw_tools::execution_stop::StopEvidence::Stopped => "delivered",
            aw_tools::execution_stop::StopEvidence::NoTarget => "gone",
            aw_tools::execution_stop::StopEvidence::Unknown => "failed",
        };
        return Some(format!("stop id={id} reason={reason} outcome={outcome}"));
    }
    external_signal(completion)
        .map(|signal| format!("exit id={id} signal={} from=outside", signal_token(signal)))
}

pub(crate) fn log_process_lifecycle(
    log: &crate::logging::LogConfig,
    id: &str,
    completion: &JobCompletion,
) {
    if let Some(message) = process_lifecycle_message(id, completion) {
        log.lifecycle("job", message);
    }
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

fn read_shared_prefix(
    manager: &SharedJobManager,
    id: &str,
    stream: SharedOutputStream,
    limit: usize,
) -> Result<Vec<u8>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let (summary, _) = manager
        .output_summary(id, 1, stream)
        .map_err(|error| anyhow!("cannot inspect shared retained output: {error:?}"))?;
    let target = summary.retained_bytes.min(limit as u64);
    let capacity = usize::try_from(target).unwrap_or(limit);
    let mut bytes = Vec::with_capacity(capacity.min(64 * 1024));
    let mut offset = 0u64;
    while offset < target {
        let remaining = usize::try_from(target - offset).unwrap_or(usize::MAX);
        let page_limit = remaining.min(MAX_JOB_SPOOL_BYTES_PER_STREAM as usize);
        let page = manager
            .output_page(id, 1, stream, offset, page_limit)
            .map_err(|error| anyhow!("cannot read shared retained output: {error:?}"))?;
        if page.bytes.is_empty() {
            bail!("shared retained output made no paging progress");
        }
        let take = page.bytes.len().min(remaining);
        bytes.extend_from_slice(&page.bytes[..take]);
        offset = offset
            .checked_add(take as u64)
            .ok_or_else(|| anyhow!("shared retained output offset overflow"))?;
    }
    Ok(bytes)
}

fn internal_failure_payload(completion: &JobCompletion) -> String {
    let observed = completion.observed_status.unwrap_or(completion.status);
    let mut payload = serde_json::json!({
        "exit_code": completion.exit_code(),
        "success": false,
        "timed_out": observed == JobStatus::TimedOut,
        "streams_omitted": true,
        "error": completion.internal_error.as_deref().or(completion.error.as_deref()),
    });
    if let Some(context) = completion.context.as_ref() {
        payload["dir"] = serde_json::Value::from(context.dir.as_str());
        payload["sandboxed"] = serde_json::Value::from(context.sandboxed);
        payload["network"] = serde_json::Value::from(context.network);
    }
    if let Some(note) = external_stop_note(completion) {
        payload["next"] = serde_json::Value::from(note);
    }
    with_termination(payload, completion).to_string()
}

fn internal_failure_result(completion: &JobCompletion) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(internal_failure_payload(
        completion,
    ))])
}

fn render_tool_result(snapshot: &ToolResultSnapshot) -> Result<CallToolResult> {
    let completion = &snapshot.completion;
    if completion.internal_error.is_some() {
        return Ok(internal_failure_result(completion));
    }
    let stdout_limit = usize::try_from(completion.stdout.stored_bytes)
        .unwrap_or(usize::MAX)
        .min(snapshot.result_limit);
    let stderr_limit = usize::try_from(completion.stderr.stored_bytes)
        .unwrap_or(usize::MAX)
        .min(snapshot.result_limit);
    let (stdout, stderr) = match &snapshot.output {
        ToolResultOutput::Shared { manager, id } => (
            read_shared_prefix(manager, id, SharedOutputStream::Stdout, stdout_limit)?,
            read_shared_prefix(manager, id, SharedOutputStream::Stderr, stderr_limit)?,
        ),
        ToolResultOutput::Legacy {
            stdout_path,
            stderr_path,
        } => (
            read_prefix(stdout_path, stdout_limit)?,
            read_prefix(stderr_path, stderr_limit)?,
        ),
    };
    let stdout_truncated =
        completion.stdout.truncated || completion.stdout.total_bytes > snapshot.result_limit as u64;
    let stderr_truncated =
        completion.stderr.truncated || completion.stderr.total_bytes > snapshot.result_limit as u64;

    let Some(context) = completion.context.as_ref() else {
        if completion.status == JobStatus::Unknown {
            let text = serde_json::json!({
                "job_id": snapshot.id,
                "status": "unknown",
                "observed_status": completion.observed_status.map(JobStatus::as_str),
                "success": false,
                "stdout": String::from_utf8_lossy(&stdout),
                "stderr": String::from_utf8_lossy(&stderr),
                "stdout_truncated": stdout_truncated,
                "stderr_truncated": stderr_truncated,
                "error": completion.error.as_deref().or(completion.internal_error.as_deref()),
            })
            .to_string();
            return Ok(CallToolResult::error(vec![ContentBlock::text(text)]));
        }
        if completion.status != JobStatus::Completed {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                completion
                    .error
                    .clone()
                    .unwrap_or_else(|| "shell command failed before execution".to_owned()),
            )]));
        }
        let text = with_termination(
            serde_json::json!({
                "exit_code": completion.exit_code(),
                "success": completion.success,
                "timed_out": false,
                "stdout": String::from_utf8_lossy(&stdout),
                "stderr": String::from_utf8_lossy(&stderr),
                "stdout_truncated": stdout_truncated,
                "stderr_truncated": stderr_truncated,
            }),
            completion,
        )
        .to_string();
        return Ok(CallToolResult::success(vec![ContentBlock::text(text)]));
    };

    let observed = completion.observed_status.unwrap_or(completion.status);
    let error = if completion.status == JobStatus::Unknown {
        completion.error.clone()
    } else if completion.status == JobStatus::TimedOut {
        Some(format!(
            "command timed out after {} seconds and was terminated; use the shell constructor with background:true for explicit durable background execution or retry with a different timeout when the caller and transport permit it",
            snapshot.timeout_secs
        ))
    } else {
        None
    };
    let mut payload = serde_json::json!({
        "status": completion.status.as_str(),
        "observed_status": completion.observed_status.map(JobStatus::as_str),
        "exit_code": completion.exit_code(),
        "success": completion.success,
        "timed_out": observed == JobStatus::TimedOut,
        "dir": context.dir,
        "sandboxed": context.sandboxed,
        "network": context.network,
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
        "error": error.as_deref().or(completion.error.as_deref()),
    });
    if completion.status == JobStatus::Unknown {
        payload["job_id"] = serde_json::Value::String(snapshot.id.clone());
    }
    if let Some(note) = external_stop_note(completion) {
        payload["next"] = serde_json::Value::from(note);
    }
    let text = with_termination(payload, completion).to_string();
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

fn failure_error_object(message: &str, completion: &JobCompletion) -> JsonObject {
    let mut error = error_object(message);
    let terminal = SharedTaskTerminalDetail::from(shared_process_terminal(completion));
    let mut data = serde_json::Map::new();
    data.insert("streams_omitted".to_owned(), serde_json::Value::from(true));
    match terminal.exit {
        SharedProcessExit::Code(code) => {
            data.insert("exit_code".to_owned(), serde_json::Value::from(code));
        }
        SharedProcessExit::Signal(signal) => {
            data.insert("signal".to_owned(), serde_json::Value::from(signal));
            data.insert(
                "signal_name".to_owned(),
                serde_json::Value::from(signal_label(signal)),
            );
        }
        SharedProcessExit::Unknown => {}
    }
    if let Some(stop) = terminal.stop {
        data.insert("stop".to_owned(), serde_json::Value::from(stop.as_str()));
    }
    if completion.observed_status.unwrap_or(completion.status) == JobStatus::TimedOut {
        data.insert("timed_out".to_owned(), serde_json::Value::from(true));
    }
    if let Some(stopped_by) = product_stopped_by(completion) {
        data.insert("stopped_by".to_owned(), serde_json::Value::from(stopped_by));
    }
    error.insert("data".to_owned(), serde_json::Value::Object(data));
    error
}

fn result_object(result: CallToolResult) -> JsonObject {
    serde_json::to_value(result)
        .expect("call tool result serializes")
        .as_object()
        .expect("call tool result is an object")
        .clone()
}

fn job_output_payload_ceiling(response_ceiling: SerializedResultBytes) -> Result<usize> {
    let response_ceiling = response_ceiling.get();
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
    response_ceiling: SerializedResultBytes,
    base64: bool,
) -> Result<String> {
    let payload_ceiling = job_output_payload_ceiling(response_ceiling)?;
    let render = OutputPageRender {
        id,
        node: None,
        stream: stream.label(),
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

fn fit_shared_output_page(
    render: &OutputPageRender<'_>,
    bytes: &[u8],
    snapshot: &OutputSnapshot,
    response_ceiling: SerializedResultBytes,
) -> Result<(String, usize)> {
    let payload_ceiling = job_output_payload_ceiling(response_ceiling)?;
    let offset = render.offset;
    let base64 = render.base64;
    let empty = render.render(snapshot.stored, false, &[])?;
    let fixed = serialized_tool_result_len(&empty)?;
    if fixed > payload_ceiling {
        bail!("job output metadata leaves no room under the response ceiling");
    }
    let data_budget = payload_ceiling - fixed;
    let take = if base64 {
        bytes.len().min((data_budget / 4).saturating_mul(3))
    } else if let Ok(text) = std::str::from_utf8(bytes) {
        let mut used = 0usize;
        let mut take = 0usize;
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
        let mut low = 0usize;
        let mut high = bytes.len();
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            let next = offset + middle as u64;
            let page = render.render(
                next,
                snapshot.terminal && next >= snapshot.stored,
                &bytes[..middle],
            )?;
            if serialized_tool_result_len(&page)? <= payload_ceiling {
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
    Ok((rendered, take))
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
    node: Option<usize>,
    stream: &'static str,
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
            node: self.node,
            stream: self.stream,
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

#[derive(Clone, Copy, Debug, Default)]
struct SharedOwnerProjection {
    through: u64,
    retained: u64,
    released: bool,
}

fn shared_owner_projection(
    entry: &JobEntry,
    stream: JobStream,
) -> Option<JobResult<SharedOwnerProjection>> {
    let shared = entry.shared_job.as_ref()?;
    let owner = match shared.manager.job_output_owner(&entry.id) {
        Ok(owner) => owner,
        Err(error) => {
            return Some(Err(JobError::Internal(anyhow!(
                "cannot derive shared Job output owner for {}: {error:?}",
                entry.id
            ))));
        }
    };
    let owner_state =
        match shared
            .manager
            .output_owner_state(&entry.id, 1, shared_output_stream(stream), owner)
        {
            Ok(state) => state,
            Err(aw_tools::filesystem::job_manager::Error::OutputMissing) => {
                return Some(Ok(SharedOwnerProjection::default()));
            }
            Err(error) => {
                return Some(Err(JobError::Internal(anyhow!(
                    "cannot inspect shared Job owner state for {} {}: {error:?}",
                    entry.id,
                    stream.label()
                ))));
            }
        };
    let retained = match shared
        .manager
        .output_summary(&entry.id, 1, shared_output_stream(stream))
    {
        Ok((summary, _)) => summary.retained_bytes,
        Err(aw_tools::filesystem::job_manager::Error::OutputReleased) => 0,
        Err(aw_tools::filesystem::job_manager::Error::OutputMissing) => 0,
        Err(error) => {
            return Some(Err(JobError::Internal(anyhow!(
                "cannot inspect shared Job retained output for {} {}: {error:?}",
                entry.id,
                stream.label()
            ))));
        }
    };
    Some(Ok(SharedOwnerProjection {
        through: owner_state.through,
        retained,
        released: owner_state.released
            || (owner_state.acknowledged && owner_state.through >= retained),
    }))
}

fn job_owner_projection(entry: &JobEntry, stream: JobStream) -> JobResult<SharedOwnerProjection> {
    if let Some(projection) = shared_owner_projection(entry, stream) {
        return projection;
    }
    Ok(match stream {
        JobStream::Stdout => SharedOwnerProjection {
            through: entry.stdout_consumed,
            retained: entry.stdout_retained,
            released: entry.stdout_released,
        },
        JobStream::Stderr => SharedOwnerProjection {
            through: entry.stderr_consumed,
            retained: entry.stderr_retained,
            released: entry.stderr_released,
        },
    })
}

fn shared_snapshot_status(snapshot: &SharedSnapshot) -> &'static str {
    if snapshot.lifecycle != SharedLifecycle::Terminal {
        // Keep dotlink's established job-control wire contract: every live
        // phase is "working". Precise running/cancelling/revoking state is
        // exposed separately under graph.lifecycle.
        return "working";
    }
    match snapshot.status_label() {
        // Preserve dotlink's existing terminal vocabulary while retaining the
        // canonical Graph detail under graph.summary/graph.compact.
        "ok" | "unchanged" => "completed",
        "fail" | "mixed" | "revoked" => "failed",
        "unknown" => "unknown",
        "cancelled" => "cancelled",
        // Dotlink previously collapsed this into cancellation. Keep the
        // canonical no-effect distinction visible instead.
        "skip" => "skip",
        _ => "unknown",
    }
}

fn shared_lifecycle_label(lifecycle: SharedLifecycle) -> &'static str {
    match lifecycle {
        SharedLifecycle::Running => "running",
        SharedLifecycle::Cancelling => "cancelling",
        SharedLifecycle::Revoking => "revoking",
        SharedLifecycle::Terminal => "terminal",
    }
}

fn shared_stop_reason_label(reason: SharedStopReason) -> &'static str {
    match reason {
        SharedStopReason::Cancelled => "cancelled",
        SharedStopReason::Revoked => "revoked",
    }
}

fn authoritative_graph_snapshot(
    inner: &JobManagerInner,
    entry: &JobEntry,
) -> Option<SharedSnapshot> {
    if !entry.shared_graph_authoritative {
        return None;
    }
    match inner.shared_jobs.runtime_snapshot(&entry.id) {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            tracing::warn!(id = %entry.id, ?error, "authoritative shared Graph status unavailable");
            None
        }
    }
}

fn projected_status(inner: &JobManagerInner, entry: &JobEntry) -> String {
    if !entry.shared_graph_authoritative {
        return entry.status().as_str().to_owned();
    }
    authoritative_graph_snapshot(inner, entry)
        .as_ref()
        .map_or("unknown", shared_snapshot_status)
        .to_owned()
}

fn physical_spool_usage(inner: &JobManagerInner) -> (u64, u64) {
    let legacy = inner.spool_used_bytes.load(Ordering::Relaxed);
    match inner.shared_jobs.spool_usage() {
        Ok((shared, limit)) => (shared.saturating_add(legacy), limit),
        Err(error) => {
            tracing::warn!(?error, "cannot inspect shared Job spool usage");
            (legacy, inner.spool_limit_bytes.load(Ordering::Relaxed))
        }
    }
}

fn render_status_locked(inner: &JobManagerInner, entry: &JobEntry) -> String {
    let completion = entry.completion.as_ref();
    let (spool_used_bytes, spool_session_limit_bytes) = physical_spool_usage(inner);
    let shared_snapshot = authoritative_graph_snapshot(inner, entry);
    let status = if entry.shared_graph_authoritative {
        shared_snapshot
            .as_ref()
            .map_or("unknown", shared_snapshot_status)
            .to_owned()
    } else {
        entry.status().as_str().to_owned()
    };
    let graph = if entry.shared_graph_authoritative {
        let compact = inner.shared_jobs.render_job_status(&entry.id).ok();
        Some(match shared_snapshot.as_ref() {
            Some(snapshot) => serde_json::json!({
                "lifecycle": shared_lifecycle_label(snapshot.lifecycle),
                "stop_reason": snapshot.stop_reason.map(shared_stop_reason_label),
                "finished": snapshot.finished,
                "total": snapshot.total,
                "summary": snapshot.summary.map(|(primary, more, incomplete)| serde_json::json!({
                    "primary": primary.as_str(),
                    "more": more,
                    "incomplete": incomplete,
                })),
                "compact": compact,
            }),
            None => serde_json::json!({
                "lifecycle": "unknown",
                "finished": serde_json::Value::Null,
                "total": serde_json::Value::Null,
                "summary": serde_json::Value::Null,
                "compact": compact,
            }),
        })
    } else {
        None
    };
    let stdout_owner =
        job_owner_projection(entry, JobStream::Stdout).unwrap_or_else(|_| SharedOwnerProjection {
            retained: completion.map_or_else(
                || stored_len(&entry.stdout_path, None),
                |_| entry.stdout_retained,
            ),
            ..Default::default()
        });
    let stderr_owner =
        job_owner_projection(entry, JobStream::Stderr).unwrap_or_else(|_| SharedOwnerProjection {
            retained: completion.map_or_else(
                || stored_len(&entry.stderr_path, None),
                |_| entry.stderr_retained,
            ),
            ..Default::default()
        });
    let stdout_stored = stdout_owner.retained;
    let stderr_stored = stderr_owner.retained;
    let external_note = completion.and_then(external_stop_note);
    let mut payload = serde_json::json!({
        "job_id": entry.id,
        "task_id": entry.id,
        "status": status,
        "phase": entry.phase,
        "graph": graph,
        "elapsed_ms": entry.created.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        "timeout_secs": entry.timeout_secs,
        "spool_limit_bytes_per_stream": MAX_JOB_SPOOL_BYTES_PER_STREAM,
        "spool_used_bytes": spool_used_bytes,
        "spool_session_limit_bytes": spool_session_limit_bytes,
        "cancel_requested": entry.cancel.is_cancelled(),
        "exit_code": completion.and_then(JobCompletion::exit_code),
        "success": completion.map(|value| value.success),
        "error": completion
            .and_then(|value| value.error.as_deref())
            .or(external_note.as_deref()),
        "observed_status": completion.and_then(|value| value.observed_status.map(JobStatus::as_str)),
        "stdout": {
            "stored_bytes": stdout_stored,
            "acknowledged_bytes": stdout_owner.through,
            "total_bytes": completion.map(|value| value.stdout.total_bytes),
            "truncated": completion.is_some_and(|value| value.stdout.truncated),
            "released": stdout_owner.released,
        },
        "stderr": {
            "stored_bytes": stderr_stored,
            "acknowledged_bytes": stderr_owner.through,
            "total_bytes": completion.map(|value| value.stderr.total_bytes),
            "truncated": completion.is_some_and(|value| value.stderr.truncated),
            "released": stderr_owner.released,
        },
        "output": "use jobs_read to read; jobs_ack explicitly advances cleanup watermarks",
        "cancel": "use jobs_cancel while status=working",
    });
    if let Some(completion) = completion {
        payload = with_termination(payload, completion);
    }
    payload.to_string()
}

fn abandon_response_attempt(inner: &JobManagerInner, id: &str) {
    let Ok(mut state) = inner.state.lock() else {
        return;
    };
    let Some(entry) = state.jobs.get_mut(id) else {
        return;
    };
    entry.response_attempts = entry.response_attempts.saturating_sub(1);
    if entry.disposition != JobDisposition::Pending {
        return;
    }
    if entry.request_key.is_some() && entry.response_attempts == 0 {
        // The transport can redeliver this exact request later. Once the last
        // undecided caller disappears, keep execution/replay state but release
        // the scarce ordinary retained-job slot. A surviving concurrent attempt
        // keeps the slot it is still actively deciding how to represent.
        entry.retention = RetentionClass::TransportOnly;
    } else if entry.request_key.is_none() && entry.response_attempts == 0 {
        // Local transports have no durable request identity to replay. Make an
        // abandoned execution discoverable through jobs_list instead of hiding a
        // side effect behind an irrecoverable lost response. The local retained
        // slot was reserved at admission, so shared exposure capacity is not an
        // independent admission decision here.
        if let Some(shared) = entry.shared_job.as_ref() {
            let reserved = match shared.manager.reserve_exposed(&entry.id) {
                Ok(()) => true,
                Err(aw_tools::filesystem::job_manager::Error::NotFound) => {
                    tracing::error!(id = %entry.id, "shared-owned Job disappeared during abandoned-response exposure reservation");
                    false
                }
                Err(error) => {
                    tracing::error!(id = %entry.id, ?error, "shared Job exposure reservation failed during abandoned-response recovery");
                    false
                }
            };
            if reserved {
                match shared.manager.expose(&entry.id) {
                    Ok(()) => {}
                    Err(aw_tools::filesystem::job_manager::Error::NotFound) => {
                        tracing::error!(id = %entry.id, "shared-owned Job disappeared during abandoned-response publication");
                        return;
                    }
                    Err(error) => {
                        tracing::error!(id = %entry.id, ?error, "reserved shared Job publication failed during abandoned-response recovery");
                        return;
                    }
                }
            } else {
                return;
            }
        }
        let id = entry.id.clone();
        let _ = entry;
        let _ = commit_exposure(&mut state, &id, JobDisposition::Compatibility);
    }
}

fn commit_exposure(state: &mut JobState, id: &str, disposition: JobDisposition) -> JobResult<()> {
    debug_assert!(matches!(
        disposition,
        JobDisposition::Compatibility | JobDisposition::NativeTask
    ));
    let needs_sequence = state
        .jobs
        .get(id)
        .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?
        .exposure_sequence
        .is_none();
    let sequence = if needs_sequence {
        state.next_exposure_sequence = state
            .next_exposure_sequence
            .checked_add(1)
            .ok_or_else(|| JobError::Internal(anyhow!("job exposure sequence exhausted")))?;
        Some(state.next_exposure_sequence)
    } else {
        None
    };
    let entry = state
        .jobs
        .get_mut(id)
        .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
    if let Some(sequence) = sequence {
        entry.exposure_sequence = Some(sequence);
    }
    entry.disposition = disposition;
    if disposition == JobDisposition::Compatibility && !release_shared_task_owner(entry) {
        entry.output_cleanup_pending = true;
    }
    entry.retention = RetentionClass::Ordinary;
    entry.retention_anchor = Some(Instant::now());
    entry.expiration_pending = false;
    Ok(())
}

fn handoff_from_entry(
    inner: &JobManagerInner,
    entry: &JobEntry,
    retention: Duration,
) -> JobHandoff {
    match entry.disposition {
        JobDisposition::Synchronous => JobHandoff::Synchronous,
        JobDisposition::Compatibility => {
            JobHandoff::Compatibility(compatibility_receipt(inner, entry))
        }
        JobDisposition::NativeTask => JobHandoff::NativeTask(task_from_entry(entry, retention)),
        JobDisposition::Pending => unreachable!("pending job has no committed response"),
    }
}

fn terminal_retention_anchor(entry: &JobEntry) -> Option<Instant> {
    let terminal = entry.terminal_at?;
    Some(
        entry
            .retention_anchor
            .map_or(terminal, |anchor| anchor.max(terminal)),
    )
}

fn task_retention_anchor(entry: &JobEntry) -> Option<Instant> {
    let anchor = terminal_retention_anchor(entry)?;
    if entry.request_key.is_some() && entry.disposition.exposed() {
        // A replayed receipt may be delivered long after the first attempt. Its
        // advertised TTL must match the full window the server will start when
        // positive delivery is acknowledged.
        Some(anchor.max(Instant::now()))
    } else {
        Some(anchor)
    }
}

fn compatibility_receipt(inner: &JobManagerInner, entry: &JobEntry) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        serde_json::json!({
            "job_id": entry.id,
            "task_id": entry.id,
            "status": projected_status(inner, entry),
            "phase": entry.phase,
            "timeout_secs": entry.timeout_secs,
            "spool_limit_bytes_per_stream": MAX_JOB_SPOOL_BYTES_PER_STREAM,
            "spool_session_limit_bytes": inner.spool_limit_bytes.load(Ordering::Relaxed),
            "async": true,
            "poll": "jobs_get",
            "output": "jobs_read",
            "ack": "jobs_ack",
            "cancel": "jobs_cancel",
        })
        .to_string(),
    )])
}

pub(crate) fn shared_process_terminal(completion: &JobCompletion) -> SharedProcessTerminal {
    let output = SharedProcessOutputMeta {
        stdout_bytes: completion.stdout.total_bytes,
        stderr_bytes: completion.stderr.total_bytes,
        stdout_truncated: completion.stdout.truncated,
        stderr_truncated: completion.stderr.truncated,
        complete: completion.output_complete,
    };
    SharedProcessTerminal {
        exit: completion.process_exit,
        stop: completion.terminal_stop_reason(),
        output,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SharedSettlement {
    /// This host entry predates/does not own a canonical shared projection.
    LegacyOnly,
    /// The canonical shared lifecycle is terminal with a known outcome.
    Terminal,
    /// The shared lifecycle is terminal-unknown or could not be checkpointed.
    Uncertain,
}

fn shared_terminal_settlement(state: SharedNodeState) -> Option<SharedSettlement> {
    match state {
        SharedNodeState::Terminal { status, .. } => {
            Some(if status.primary == SharedPrimary::Unknown {
                SharedSettlement::Uncertain
            } else {
                SharedSettlement::Terminal
            })
        }
        _ => None,
    }
}

fn settle_shared_completion(
    inner: &JobManagerInner,
    id: &str,
    completion: &JobCompletion,
    shared_owned: bool,
) -> SharedSettlement {
    let transition_gate = {
        let state = inner.state.lock().expect("job manager lock");
        let Some(entry) = state.jobs.get(id) else {
            return if shared_owned {
                tracing::warn!(%id, "shared-owned shell Job disappeared before settlement");
                SharedSettlement::Uncertain
            } else {
                SharedSettlement::LegacyOnly
            };
        };
        entry.shared_transition_gate.clone()
    };
    let _transition = match transition_gate.lock() {
        Ok(guard) => guard,
        Err(_) => {
            tracing::warn!(%id, "shared Job transition gate poisoned during settlement");
            return SharedSettlement::Uncertain;
        }
    };
    let terminal = (!completion.spawn_failed).then(|| shared_process_terminal(completion));
    let state = match inner.shared_jobs.node_state(id, 1) {
        Ok(state) => state,
        Err(aw_tools::filesystem::job_manager::Error::NotFound) => {
            return if shared_owned {
                tracing::warn!(%id, "shared-owned shell Job disappeared during settlement");
                SharedSettlement::Uncertain
            } else {
                SharedSettlement::LegacyOnly
            };
        }
        Err(error) => {
            tracing::warn!(%id, ?error, "cannot inspect shared shell Job during settlement");
            return SharedSettlement::Uncertain;
        }
    };
    if let Some(settlement) = shared_terminal_settlement(state) {
        return settlement;
    }
    match state {
        SharedNodeState::Pending => {
            tracing::warn!(%id, "shared shell Job remained pending at host completion");
            SharedSettlement::Uncertain
        }
        SharedNodeState::Claimed { sequence: claimed } => {
            // Host completion without a canonical Start checkpoint is an
            // invariant violation. Fail closed: never project it as a
            // successful shell or infer no-effect from host state alone.
            let Ok(sequence) = next_shared_sequence(&inner.shared_jobs, id) else {
                return SharedSettlement::Uncertain;
            };
            let status = SharedStatus {
                primary: SharedPrimary::Unknown,
                more: false,
                incomplete: false,
            };
            match inner.shared_jobs.settle(
                id,
                1,
                SharedNodeState::Claimed { sequence: claimed },
                status,
                sequence,
                |_, _| Ok::<_, SharedJournalError>(()),
            ) {
                Ok(()) => SharedSettlement::Uncertain,
                Err(error) => {
                    tracing::warn!(%id, ?error, "shared claimed settlement failed under the Job transition gate");
                    SharedSettlement::Uncertain
                }
            }
        }
        SharedNodeState::Running { sequence: running } => {
            let Ok(sequence) = next_shared_sequence(&inner.shared_jobs, id) else {
                return SharedSettlement::Uncertain;
            };
            let settled = if completion.spawn_failed {
                inner
                    .shared_jobs
                    .settle(
                        id,
                        1,
                        SharedNodeState::Running { sequence: running },
                        SharedStatus {
                            primary: SharedPrimary::Fail,
                            more: false,
                            incomplete: false,
                        },
                        sequence,
                        |_, _| Ok::<_, SharedJournalError>(()),
                    )
                    .map(|()| SharedStatus {
                        primary: SharedPrimary::Fail,
                        more: false,
                        incomplete: false,
                    })
            } else {
                inner.shared_jobs.settle_process(
                    id,
                    1,
                    SharedNodeState::Running { sequence: running },
                    terminal.expect("non-spawn-failure process terminal"),
                    sequence,
                    |_, _| Ok::<_, SharedJournalError>(()),
                )
            };
            match settled {
                Ok(status) if status.primary == SharedPrimary::Unknown => {
                    SharedSettlement::Uncertain
                }
                Ok(_) => SharedSettlement::Terminal,
                Err(error) => {
                    tracing::warn!(%id, ?error, "shared process settlement failed under the Job transition gate");
                    SharedSettlement::Uncertain
                }
            }
        }
        SharedNodeState::Terminal { .. } => unreachable!("handled above"),
    }
}

fn fail_closed_shared_completion(mut completion: JobCompletion) -> JobCompletion {
    // Canonical shared uncertainty dominates the host's local terminal class.
    // Preserve an infrastructure error as diagnostic evidence, but never let a
    // post-admission failure look like a proven no-effect failure/retry signal.
    let observed = completion.observed_status.unwrap_or(completion.status);
    let detail = completion
        .error
        .as_deref()
        .or(completion.internal_error.as_deref())
        .unwrap_or("terminal host outcome could not be certified");
    if completion.status != JobStatus::Unknown && completion.observed_status.is_none() {
        completion.observed_status = Some(completion.status);
    }
    completion.status = JobStatus::Unknown;
    completion.success = false;
    // Fail-closed product status must not erase already observed physical exit
    // or final stop evidence.
    completion.error = Some(bounded_diagnostic(format!(
        "shared shell lifecycle outcome is uncertain after observed {}: {}; inspect the Job/effects before retrying",
        observed.as_str(),
        detail
    )));
    completion
}

fn finish_inner(inner: &JobManagerInner, id: &str, completion: JobCompletion) {
    let (authoritative, shared_owned) = inner
        .state
        .lock()
        .expect("job manager lock")
        .jobs
        .get(id)
        .map_or((false, false), |entry| {
            (entry.shared_graph_authoritative, entry.shared_job.is_some())
        });
    let completion = if authoritative {
        completion
    } else {
        match settle_shared_completion(inner, id, &completion, shared_owned) {
            SharedSettlement::LegacyOnly | SharedSettlement::Terminal => completion,
            SharedSettlement::Uncertain => fail_closed_shared_completion(completion),
        }
    };
    let mut state = inner.state.lock().expect("job manager lock");
    if let Some(entry) = state.jobs.get_mut(id) {
        entry.runner = None;
        entry.phase = JobPhase::Terminal;
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        entry.terminal_at = Some(Instant::now());
        if entry.shared_job.is_none() {
            entry.stdout_retained = completion.stdout.stored_bytes;
            entry.stderr_retained = completion.stderr.stored_bytes;
        }
        entry.completion = Some(completion);
        entry.public_revision = entry.public_revision.saturating_add(1);
        if !release_shared_task_owner(entry) {
            entry.output_cleanup_pending = true;
        }
        if entry.output_cleanup_requested || entry.output_cleanup_pending {
            maybe_release_output(entry);
        }
        entry.notify.notify_waiters();
    }
}

#[cfg(test)]
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

fn acknowledge_shared_owner(
    entry: &JobEntry,
    owner: SharedOutputOwner,
    stream: JobStream,
    through: u64,
) -> bool {
    let Some(shared) = entry.shared_job.as_ref() else {
        return false;
    };
    match shared.manager.acknowledge_output(
        &entry.id,
        1,
        shared_output_stream(stream),
        owner,
        through,
    ) {
        Ok(_) | Err(aw_tools::filesystem::job_manager::Error::OutputReleased) => true,
        Err(error) => {
            tracing::warn!(id = %entry.id, ?stream, ?error, "shared Job output acknowledgement failed");
            false
        }
    }
}

/// Once the response is permanently non-Task, the prospective Task owner no
/// longer needs to pin raw output. This is called only after terminal metadata
/// fixes the complete retained prefix, so future bytes cannot appear beyond the
/// acknowledgement watermark.
fn release_shared_task_owner(entry: &JobEntry) -> bool {
    if entry.shared_graph_authoritative {
        return true;
    }
    if !(entry.retention == RetentionClass::CleanupOnly
        || matches!(
            entry.disposition,
            JobDisposition::Synchronous | JobDisposition::Compatibility
        ))
        || !entry.status().terminal()
    {
        return true;
    }
    let Some(shared) = entry.shared_job.as_ref() else {
        return true;
    };
    let Some(completion) = entry.completion.as_ref() else {
        return false;
    };
    let owner = match shared_task_output_owner(&shared.manager, &entry.id) {
        Ok(owner) => owner,
        Err(error) => {
            tracing::warn!(id = %entry.id, %error, "cannot derive shared non-Task output owner");
            return false;
        }
    };
    let mut ready = true;
    for (stream, through) in [
        (JobStream::Stdout, completion.stdout.stored_bytes),
        (JobStream::Stderr, completion.stderr.stored_bytes),
    ] {
        if !acknowledge_shared_owner(entry, owner, stream, through) {
            ready = false;
        }
    }
    ready
}

/// Once the compatibility Job owner has released both streams, retain only the
/// exact prefix still needed by a committed native Task. The shared JobManager
/// remains the sole physical spool/quota owner; dotlink mirrors its retained
/// lengths only after each shared transition succeeds.
fn compact_shared_native_task_stream(entry: &mut JobEntry, stream: JobStream) -> bool {
    if !entry.disposition.native_task() {
        return true;
    }
    let Some(shared) = entry.shared_job.as_ref() else {
        return false;
    };
    let Some(completion) = entry.completion.as_ref() else {
        return false;
    };
    let owner = match shared_task_output_owner(&shared.manager, &entry.id) {
        Ok(owner) => owner,
        Err(error) => {
            tracing::warn!(id = %entry.id, %error, "cannot derive shared native-Task output owner");
            return false;
        }
    };
    let keep_result =
        completion.internal_error.is_none() && completion.status != JobStatus::Cancelled;
    let retained = match shared
        .manager
        .output_summary(&entry.id, 1, shared_output_stream(stream))
    {
        Ok((summary, _)) => summary.retained_bytes,
        Err(aw_tools::filesystem::job_manager::Error::OutputReleased) => 0,
        Err(error) => {
            tracing::warn!(id = %entry.id, ?stream, ?error, "cannot inspect shared native-Task output before compaction");
            return false;
        }
    };
    let keep = if keep_result {
        retained.min(entry.result_limit as u64)
    } else {
        0
    };
    let retained = match shared.manager.compact_output_for_owner(
        &entry.id,
        1,
        shared_output_stream(stream),
        owner,
        keep,
    ) {
        Ok(retained) => retained,
        Err(aw_tools::filesystem::job_manager::Error::OutputReleased) => 0,
        Err(error) => {
            tracing::warn!(
                id = %entry.id,
                ?stream,
                ?error,
                "shared native-Task output compaction failed"
            );
            return false;
        }
    };
    let _ = retained;
    true
}

fn graph_output_streams() -> [SharedOutputStream; 3] {
    [
        SharedOutputStream::Result,
        SharedOutputStream::Stdout,
        SharedOutputStream::Stderr,
    ]
}

fn authoritative_graph_outputs_released(entry: &JobEntry) -> bool {
    let Some(shared) = entry.shared_job.as_ref() else {
        return false;
    };
    let Ok(graph) = shared.manager.graph(&entry.id) else {
        return false;
    };
    for coordinate in 1..=graph.steps().len() {
        for stream in graph_output_streams() {
            match shared.manager.output_summary(&entry.id, coordinate, stream) {
                Ok((summary, _)) if summary.retained_bytes == 0 => {}
                Err(aw_tools::filesystem::job_manager::Error::OutputMissing)
                | Err(aw_tools::filesystem::job_manager::Error::OutputReleased) => {}
                Ok(_) | Err(_) => return false,
            }
        }
    }
    true
}

fn acknowledge_authoritative_graph_outputs(entry: &JobEntry, owner: SharedOutputOwner) -> bool {
    let Some(shared) = entry.shared_job.as_ref() else {
        return false;
    };
    let Ok(graph) = shared.manager.graph(&entry.id) else {
        return false;
    };
    let mut ready = true;
    for coordinate in 1..=graph.steps().len() {
        for stream in graph_output_streams() {
            let through = match shared.manager.output_summary(&entry.id, coordinate, stream) {
                Ok((summary, _)) => summary.retained_bytes,
                Err(aw_tools::filesystem::job_manager::Error::OutputMissing)
                | Err(aw_tools::filesystem::job_manager::Error::OutputReleased) => continue,
                Err(error) => {
                    tracing::warn!(id = %entry.id, coordinate, ?stream, ?error, "cannot inspect Graph output before complete consumption");
                    ready = false;
                    continue;
                }
            };
            match shared
                .manager
                .acknowledge_output(&entry.id, coordinate, stream, owner, through)
            {
                Ok(_) | Err(aw_tools::filesystem::job_manager::Error::OutputReleased) => {}
                Err(error) => {
                    tracing::warn!(id = %entry.id, coordinate, ?stream, ?error, "cannot acknowledge complete Graph output");
                    ready = false;
                }
            }
        }
    }
    ready
}

fn consume_complete_job_output(entry: &mut JobEntry) -> bool {
    let Some(completion) = entry.completion.as_ref() else {
        return false;
    };
    if let Some(shared) = entry.shared_job.as_ref() {
        let owner = match shared.manager.job_output_owner(&entry.id) {
            Ok(owner) => owner,
            Err(error) => {
                tracing::warn!(id = %entry.id, ?error, "cannot derive shared Job output owner for complete consumption");
                return false;
            }
        };
        let mut acknowledged = true;
        if entry.shared_graph_authoritative {
            acknowledged = acknowledge_authoritative_graph_outputs(entry, owner);
        } else {
            for (stream, through) in [
                (JobStream::Stdout, completion.stdout.stored_bytes),
                (JobStream::Stderr, completion.stderr.stored_bytes),
            ] {
                if !acknowledge_shared_owner(entry, owner, stream, through) {
                    acknowledged = false;
                }
            }
        }
        if !acknowledged {
            return false;
        }
    } else {
        entry.stdout_consumed = completion.stdout.stored_bytes;
        entry.stderr_consumed = completion.stderr.stored_bytes;
    }
    entry.output_cleanup_requested = true;
    maybe_release_output(entry);
    true
}

fn maybe_release_output(entry: &mut JobEntry) {
    if !entry.output_cleanup_requested {
        return;
    }
    if entry.shared_job.is_some() {
        // Shared ownership is stream-local. Reconcile after every ACK so one
        // fully released stream can compact independently of the other; an
        // active host reader still fences physical spool mutation globally.
        if entry.readers != 0 {
            entry.output_cleanup_pending = true;
            return;
        }
        release_acknowledged_output(entry);
        return;
    }
    let Some(completion) = entry.completion.as_ref() else {
        return;
    };
    if (entry.all_output_released() && !entry.output_cleanup_pending)
        || entry.stdout_consumed < completion.stdout.stored_bytes
        || entry.stderr_consumed < completion.stderr.stored_bytes
    {
        return;
    }
    if entry.readers != 0 {
        entry.output_cleanup_pending = true;
        return;
    }
    release_acknowledged_output(entry);
}

fn release_acknowledged_output(entry: &mut JobEntry) {
    if entry.shared_job.is_some() {
        if entry.shared_graph_authoritative {
            entry.output_cleanup_pending = !authoritative_graph_outputs_released(entry);
            return;
        }
        let non_task_owner_ready = release_shared_task_owner(entry);
        let stdout_ready =
            job_owner_projection(entry, JobStream::Stdout).is_ok_and(|state| state.released);
        let stderr_ready =
            job_owner_projection(entry, JobStream::Stderr).is_ok_and(|state| state.released);
        let stdout_retention_ready =
            !stdout_ready || compact_shared_native_task_stream(entry, JobStream::Stdout);
        let stderr_retention_ready =
            !stderr_ready || compact_shared_native_task_stream(entry, JobStream::Stderr);
        entry.output_cleanup_pending = !stdout_ready
            || !stderr_ready
            || !non_task_owner_ready
            || !stdout_retention_ready
            || !stderr_retention_ready;
        return;
    }
    // Legacy/test-only entries that never joined the shared lifecycle retain the
    // historical local file cleanup behavior.
    if entry.disposition.native_task() {
        let _ = compact_native_task_output(entry);
    } else {
        let _ = release_output_files(entry);
    }
}

fn truncate_file_releasing(
    account: Option<&LegacySpoolAccount>,
    path: &Path,
    keep: u64,
) -> Option<u64> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (keep == 0).then_some(0);
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not inspect native task spool for compaction");
            return None;
        }
    };
    if !metadata.is_file() {
        tracing::warn!(
            path = %path.display(),
            "could not compact native task spool because it is not a regular file"
        );
        return None;
    }
    let len = metadata.len();
    if len <= keep {
        return Some(len);
    }
    match OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_len(keep))
    {
        Ok(()) => {
            if let Some(account) = account {
                account.release(len - keep);
            }
            Some(keep)
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not compact native task spool");
            None
        }
    }
}

fn compact_native_task_output(entry: &mut JobEntry) -> bool {
    if entry.all_output_released() {
        return true;
    }
    let Some(completion) = entry.completion.as_ref() else {
        return false;
    };
    let keep_result =
        completion.internal_error.is_none() && completion.status != JobStatus::Cancelled;
    if !keep_result {
        return release_output_files(entry);
    }
    let stdout_keep = completion
        .stdout
        .stored_bytes
        .min(entry.result_limit as u64);
    let stderr_keep = completion
        .stderr
        .stored_bytes
        .min(entry.result_limit as u64);
    let stdout_retained = if entry.stdout_released {
        Some(entry.stdout_retained)
    } else {
        truncate_file_releasing(
            entry.legacy_spool.as_deref(),
            &entry.stdout_path,
            stdout_keep,
        )
    };
    let stderr_retained = if entry.stderr_released {
        Some(entry.stderr_retained)
    } else {
        truncate_file_releasing(
            entry.legacy_spool.as_deref(),
            &entry.stderr_path,
            stderr_keep,
        )
    };
    if let Some(retained) = stdout_retained {
        entry.stdout_retained = retained;
        entry.stdout_released = true;
    }
    if let Some(retained) = stderr_retained {
        entry.stderr_retained = retained;
        entry.stderr_released = true;
    }
    let success = entry.all_output_released();
    if success && let Some(account) = &entry.legacy_spool {
        account.reconcile_to(entry.stdout_retained.saturating_add(entry.stderr_retained));
    }
    entry.output_cleanup_pending = !success;
    success
}

fn release_output_files(entry: &mut JobEntry) -> bool {
    if entry.all_output_released() {
        return true;
    }
    let stdout_ok = entry.stdout_released
        || remove_file_releasing(entry.legacy_spool.as_deref(), &entry.stdout_path);
    if stdout_ok {
        entry.stdout_retained = 0;
        entry.stdout_released = true;
    }
    let stderr_ok = entry.stderr_released
        || remove_file_releasing(entry.legacy_spool.as_deref(), &entry.stderr_path);
    if stderr_ok {
        entry.stderr_retained = 0;
        entry.stderr_released = true;
    }
    let success = entry.all_output_released();
    if success && let Some(account) = &entry.legacy_spool {
        account.reconcile_to(0);
    }
    entry.output_cleanup_pending = !success;
    success
}

fn release_reader(inner: &Arc<JobManagerInner>, id: &str) {
    let mut removed = None;
    {
        let mut state = inner.state.lock().expect("job manager lock");
        let Some(entry) = state.jobs.get_mut(id) else {
            return;
        };
        entry.readers = entry.readers.saturating_sub(1);
        if entry.readers == 0 {
            if entry.expiration_pending {
                let cleanup_ok = state.jobs.get_mut(id).is_some_and(cleanup_entry);
                if cleanup_ok {
                    removed = remove_job_locked(&mut state, id);
                }
            } else if entry.retention == RetentionClass::CleanupOnly {
                release_acknowledged_output(entry);
                if entry.all_output_released() {
                    removed = cleanup_and_remove_job_locked(&mut state, id);
                }
            } else if entry.output_cleanup_pending {
                release_acknowledged_output(entry);
            }
        }
    }
    drop(removed);
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
        let session_lock = match OpenOptions::new().read(true).write(true).open(&lock_path) {
            Ok(lock) => match lock.try_lock() {
                Ok(()) => Some(lock),
                Err(fs::TryLockError::WouldBlock) => continue,
                Err(fs::TryLockError::Error(error)) => {
                    tracing::warn!(%error, path = %lock_path.display(), "could not inspect job session lock");
                    continue;
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::warn!(%error, path = %lock_path.display(), "could not open job session lock");
                continue;
            }
        };
        // Inspect retention only after the prior owner has released the lock:
        // it creates this marker before releasing that lock on failed shutdown.
        match fs::symlink_metadata(path.join(SESSION_UNRESOLVED_MARKER)) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "cannot inspect unresolved session marker");
                continue;
            }
        }
        // Windows cannot delete an open session lock file. There cannot be a
        // new writer for this unique session ID once its owner has exited.
        #[cfg(windows)]
        drop(session_lock);
        #[cfg(not(windows))]
        let _session_lock = session_lock;
        if let Err(error) = fs::remove_dir_all(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, path = %path.display(), "could not remove stale job session");
        }
    }
}

fn create_private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("cannot create private job spool {}", path.display()))?;
    if let Err(error) = crate::setup::set_private_file_permissions(path) {
        // `create_new` made this pathname ours. Close the capability before
        // removing its name so rollback is reliable on Windows too.
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(file)
}

fn incomplete_stream_summary(path: &Path) -> StreamSummary {
    let stored = fs::metadata(path).map_or(0, |metadata| metadata.len());
    StreamSummary {
        total_bytes: stored,
        stored_bytes: stored,
        truncated: true,
    }
}

fn stored_len(path: &Path, hint: Option<u64>) -> u64 {
    hint.unwrap_or_else(|| fs::metadata(path).map_or(0, |metadata| metadata.len()))
}

fn remove_file_releasing(account: Option<&LegacySpoolAccount>, path: &Path) -> bool {
    let bytes = fs::metadata(path).map_or(0, |metadata| metadata.len());
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(account) = account {
                account.release(bytes);
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(account) = account {
                account.release(bytes);
            }
            true
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not remove job spool file");
            false
        }
    }
}

fn cleanup_entry(entry: &mut JobEntry) -> bool {
    if let Some(shared) = entry.shared_job.take() {
        match shared.manager.expire_terminal(&entry.id) {
            Ok(()) => drop(shared),
            Err(error) => {
                tracing::warn!(id = %entry.id, ?error, "shared-owned Job is missing or not ready for host spool cleanup");
                entry.shared_job = Some(shared);
                return false;
            }
        }
    }
    let stdout_ok = remove_file_releasing(entry.legacy_spool.as_deref(), &entry.stdout_path);
    if stdout_ok {
        entry.stdout_retained = 0;
        entry.stdout_released = true;
    }
    let stderr_ok = remove_file_releasing(entry.legacy_spool.as_deref(), &entry.stderr_path);
    if stderr_ok {
        entry.stderr_retained = 0;
        entry.stderr_released = true;
    }
    let success = stdout_ok && stderr_ok;
    if success && let Some(account) = &entry.legacy_spool {
        account.reconcile_to(0);
    }
    success
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

fn cleanup_and_remove_job_locked(state: &mut JobState, id: &str) -> Option<JobEntry> {
    if !state.jobs.get_mut(id).is_some_and(cleanup_entry) {
        if let Some(entry) = state.jobs.get_mut(id) {
            entry.output_cleanup_pending = true;
        }
        return None;
    }
    remove_job_locked(state, id)
}

fn retained_job_slots(state: &JobState) -> usize {
    state
        .jobs
        .values()
        .filter(|entry| entry.retention == RetentionClass::Ordinary)
        .count()
}

fn transport_retention_slots(state: &JobState) -> usize {
    state.request_jobs.len().saturating_add(
        state
            .jobs
            .values()
            .filter(|entry| entry.retention == RetentionClass::CleanupOnly)
            .count(),
    )
}

fn reclaimable_terminal(entry: &JobEntry) -> bool {
    if entry.retention != RetentionClass::Ordinary
        || !entry.status().terminal()
        || entry.request_key.is_some()
        || entry.response_attempts != 0
        || entry.readers != 0
        || entry.disposition.native_task()
        || entry.status() == JobStatus::Unknown
    {
        return false;
    }
    let empty = entry.completion.as_ref().is_some_and(|completion| {
        completion.stdout.stored_bytes == 0 && completion.stderr.stored_bytes == 0
    });
    entry.all_output_released() || (!entry.disposition.exposed() && empty)
}

fn evict_one_reclaimable_terminal(state: &mut JobState) -> bool {
    let mut candidates = state
        .jobs
        .iter()
        .filter(|(_, entry)| reclaimable_terminal(entry))
        .map(|(id, entry)| {
            (
                terminal_retention_anchor(entry).unwrap_or(entry.created),
                id.clone(),
            )
        })
        .collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|(terminal_at, _)| *terminal_at);
    for (_, id) in candidates {
        let cleanup_ok = state.jobs.get_mut(&id).is_some_and(cleanup_entry);
        if cleanup_ok {
            let _ = remove_job_locked(state, &id);
            return true;
        }
    }
    false
}

fn ensure_retained_capacity(state: &mut JobState, additional: usize) -> bool {
    while retained_job_slots(state).saturating_add(additional) > MAX_RETAINED_JOBS {
        if !evict_one_reclaimable_terminal(state) {
            return false;
        }
    }
    true
}

fn reserve_exposure_slot(state: &mut JobState, id: &str) -> JobResult<()> {
    let entry = state
        .jobs
        .get(id)
        .ok_or_else(|| JobError::Unknown(format!("unknown job {id}")))?;
    if entry.retention == RetentionClass::Ordinary {
        return Ok(());
    }
    if entry.retention == RetentionClass::CleanupOnly {
        return Err(JobError::Unknown(format!("unknown job {id}")));
    }
    if ensure_retained_capacity(state, 1) {
        Ok(())
    } else {
        Err(JobError::Internal(anyhow!(
            "too many retained shell jobs ({MAX_RETAINED_JOBS}); acknowledge output or wait for retention cleanup"
        )))
    }
}

fn sweep_expired(inner: &JobManagerInner) {
    let retention = Duration::from_secs(inner.retention_secs.load(Ordering::Relaxed));
    {
        let Ok(mut state) = inner.state.lock() else {
            return;
        };
        let cleanup_only = state
            .jobs
            .iter()
            .filter(|(_, entry)| {
                entry.retention == RetentionClass::CleanupOnly && entry.readers == 0
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in cleanup_only {
            let released = if let Some(entry) = state.jobs.get_mut(&id) {
                release_acknowledged_output(entry);
                entry.all_output_released()
            } else {
                false
            };
            if released {
                let _ = cleanup_and_remove_job_locked(&mut state, &id);
            }
        }
        for entry in state.jobs.values_mut() {
            if entry.retention != RetentionClass::CleanupOnly
                && entry.output_cleanup_pending
                && entry.readers == 0
            {
                release_acknowledged_output(entry);
            }
        }
        let expired = state
            .jobs
            .iter()
            .filter_map(|(id, entry)| {
                // An outstanding transport request key is the dedupe authority,
                // not ordinary job TTL. Late tunnel redelivery must keep finding
                // the same execution until positive response delivery clears the
                // key; the separate transport-entry and spool quotas bound this
                // state without weakening exactly-once side-effect semantics.
                if entry.request_key.is_some() || entry.response_attempts != 0 {
                    return None;
                }
                terminal_retention_anchor(entry)
                    .filter(|anchor| anchor.elapsed() >= retention)
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
            let cleanup_ok = state.jobs.get_mut(&id).is_some_and(cleanup_entry);
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
        JobStatus::Unknown => TaskStatus::Failed,
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
    if let Some(anchor) = task_retention_anchor(entry) {
        let retention_age = anchor.saturating_duration_since(entry.created);
        let ttl = u64::try_from(retention_age.as_millis())
            .unwrap_or(u64::MAX)
            .saturating_add(retention_ms);
        task = task.with_ttl_ms(ttl);
    }
    task.with_poll_interval_ms(JOB_POLL_INTERVAL_MS)
        .with_status_message(match entry.phase {
            JobPhase::Queued => "shell job is queued",
            JobPhase::Running => {
                "shell job is running; jobs_read provides durable paged stdout/stderr"
            }
            JobPhase::Terminal => {
                "shell job finished; jobs_read provides durable paged stdout/stderr"
            }
        })
}

#[cfg(test)]
pub(crate) fn spool_limit() -> u64 {
    MAX_JOB_SPOOL_BYTES_PER_STREAM
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(bytes: usize) -> RawPageBytes {
        RawPageBytes::new(bytes).expect("test raw page budget")
    }

    fn serialized(bytes: usize) -> SerializedResultBytes {
        SerializedResultBytes::new(bytes).expect("test serialized result budget")
    }

    #[test]
    fn process_projection_keeps_physical_exit_stop_evidence_and_product_context_orthogonal() {
        let stream = StreamSummary::default();
        let external =
            JobCompletion::completed_with_exit(SharedProcessExit::Signal(9), false, stream, stream);
        assert_eq!(external_signal(&external), Some(9));
        assert_eq!(product_stopped_by(&external), None);
        assert_eq!(
            process_lifecycle_message("j:1", &external).as_deref(),
            Some("exit id=j:1 signal=SIGKILL from=outside")
        );

        let timeout = JobCompletion::timed_out(stream, stream).with_process_facts(
            SharedProcessExit::Signal(9),
            Some(SharedProcessStopRecord::stopped(
                SharedProcessStopReason::Timeout,
            )),
        );
        assert_eq!(external_signal(&timeout), None);
        assert_eq!(product_stopped_by(&timeout), Some("timeout"));
        assert_eq!(
            process_lifecycle_message("j:2", &timeout).as_deref(),
            Some("stop id=j:2 reason=timeout outcome=delivered")
        );

        let unresolved = JobCompletion::timed_out(stream, stream).with_process_facts(
            SharedProcessExit::Signal(9),
            Some(SharedProcessStopRecord::unknown(
                SharedProcessStopReason::Timeout,
            )),
        );
        assert_eq!(external_signal(&unresolved), None);
        assert_eq!(product_stopped_by(&unresolved), None);
        assert_eq!(
            process_lifecycle_message("j:3", &unresolved).as_deref(),
            Some("stop id=j:3 reason=timeout outcome=failed")
        );

        let gone = JobCompletion::failed("cleanup").with_process_facts(
            SharedProcessExit::Unknown,
            Some(SharedProcessStopRecord::no_target(
                SharedProcessStopReason::RuntimeError,
            )),
        );
        assert_eq!(product_stopped_by(&gone), None);
        assert_eq!(
            process_lifecycle_message("j:4", &gone).as_deref(),
            Some("stop id=j:4 reason=failed outcome=gone")
        );

        let failed = JobCompletion::failed("runtime failure").with_process_facts(
            SharedProcessExit::Signal(9),
            Some(SharedProcessStopRecord::stopped(
                SharedProcessStopReason::RuntimeError,
            )),
        );
        assert_eq!(product_stopped_by(&failed), Some("failed"));
        assert_eq!(
            process_lifecycle_message("j:failed", &failed).as_deref(),
            Some("stop id=j:failed reason=failed outcome=delivered")
        );

        let housekeeping = JobCompletion::failed("cleanup").with_process_facts(
            SharedProcessExit::Unknown,
            Some(SharedProcessStopRecord::housekeeping(
                aw_tools::execution_stop::StopEvidence::NoTarget,
            )),
        );
        assert_eq!(product_stopped_by(&housekeeping), None);
        assert_eq!(process_lifecycle_message("j:cleanup", &housekeeping), None);

        for (origin, stopped_by, line_reason) in [
            (CancelOrigin::Caller, "cancel", "cancel"),
            (CancelOrigin::Connector, "shutdown", "shutdown"),
        ] {
            let cancelled = JobCompletion::cancelled(stream, stream)
                .with_process_facts(
                    SharedProcessExit::Signal(9),
                    Some(SharedProcessStopRecord::stopped(
                        SharedProcessStopReason::Cancelled,
                    )),
                )
                .with_cancel_origin(origin);
            assert_eq!(product_stopped_by(&cancelled), Some(stopped_by));
            assert_eq!(external_signal(&cancelled), None);
            let expected = format!("stop id=j:5 reason={line_reason} outcome=delivered");
            assert_eq!(
                process_lifecycle_message("j:5", &cancelled).as_deref(),
                Some(expected.as_str())
            );
        }
    }

    #[test]
    fn task_failure_data_preserves_prior_timeout_and_signal() {
        let completion = JobCompletion::internal_failed(
            "spool failed",
            StreamSummary::default(),
            StreamSummary::default(),
        )
        .with_observed_status(JobStatus::TimedOut)
        .with_process_facts(
            SharedProcessExit::Signal(9),
            Some(SharedProcessStopRecord::stopped(
                SharedProcessStopReason::Timeout,
            )),
        );
        let error = failure_error_object("spool failed", &completion);
        let data = error
            .get("data")
            .and_then(serde_json::Value::as_object)
            .expect("Task failure data");
        assert_eq!(data.get("signal"), Some(&serde_json::Value::from(9)));
        assert_eq!(data.get("stop"), Some(&serde_json::Value::from("timeout")));
        assert_eq!(
            data.get("stopped_by"),
            Some(&serde_json::Value::from("timeout"))
        );
        assert_eq!(data.get("timed_out"), Some(&serde_json::Value::from(true)));
    }

    #[test]
    fn cancellation_origin_is_recorded_before_token_delivery() {
        let caller = JobCancel::new();
        caller.cancel();
        assert_eq!(caller.origin(), CancelOrigin::Caller);
        assert!(caller.is_cancelled());

        let connector = JobCancel::new();
        connector.cancel_as(CancelOrigin::Connector);
        assert_eq!(connector.origin(), CancelOrigin::Connector);
        assert!(connector.is_cancelled());
    }

    #[test]
    fn explicit_unknown_completion_preserves_fail_closed_process_evidence() {
        let stdout = StreamSummary {
            total_bytes: 7,
            stored_bytes: 7,
            truncated: false,
        };
        let completion = JobCompletion::unknown_with_output(
            "process custody became uncertain",
            stdout,
            StreamSummary::default(),
        );
        assert_eq!(completion.status, JobStatus::Unknown);
        assert!(!completion.success);
        assert_eq!(completion.process_exit, SharedProcessExit::Unknown);
        assert_eq!(completion.stop_record, None);
        assert!(completion.internal_error.is_none());
        assert_eq!(completion.stdout.stored_bytes, 7);
        assert_eq!(
            shared_process_terminal(&completion).status().primary,
            SharedPrimary::Unknown
        );
    }

    #[test]
    fn shared_graph_skip_is_not_host_cancellation_without_cancel_stop_reason() {
        let skipped = SharedSnapshot {
            id: "skip".into(),
            label: "graph".into(),
            lifecycle: SharedLifecycle::Terminal,
            stop_reason: None,
            journal_unknown: false,
            sequence: 1,
            finished: 2,
            total: 2,
            summary: Some((SharedPrimary::Skip, false, false)),
            detail: None,
        };
        let completion = JobCompletion::from_shared_graph(&skipped);
        assert_eq!(completion.status, JobStatus::Failed);
        assert!(!completion.success);
        assert_eq!(completion.stop_record, None);

        let cancelled = SharedSnapshot {
            id: "cancelled".into(),
            stop_reason: Some(SharedStopReason::Cancelled),
            ..skipped.clone()
        };
        let completion = JobCompletion::from_shared_graph(&cancelled);
        assert_eq!(completion.status, JobStatus::Cancelled);
        assert!(!completion.success);
        assert_eq!(completion.stop_record, None);

        let cancelled_unknown = SharedSnapshot {
            id: "cancelled-unknown".into(),
            summary: Some((SharedPrimary::Unknown, false, false)),
            ..cancelled
        };
        let completion = JobCompletion::from_shared_graph(&cancelled_unknown);
        assert_eq!(completion.status, JobStatus::Unknown);
        assert!(!completion.success);
        assert_eq!(completion.stop_record, None);

        let journal_unknown = SharedSnapshot {
            id: "journal-unknown".into(),
            journal_unknown: true,
            summary: Some((SharedPrimary::Ok, false, false)),
            ..skipped
        };
        let completion = JobCompletion::from_shared_graph(&journal_unknown);
        assert_eq!(completion.status, JobStatus::Unknown);
        assert!(!completion.success);
        assert_eq!(completion.stop_record, None);
    }

    #[test]
    fn authoritative_graph_status_and_list_preserve_canonical_skip_ranges() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let graph = SharedGraph::compile(
            &serde_json::json!({"ops":[
                ["read", {"path":"a"}, "*"],
                ["read", {"path":"b"}, "*"]
            ]}),
            |_, _| true,
        )
        .unwrap();
        let plans = plan_graph(&graph, |coordinate, _| {
            Ok::<_, ()>(SharedNodePolicy {
                budget: SharedSemanticBudget {
                    work_bytes: 1,
                    result_bytes: 128,
                },
                footprint: SharedFootprint::none(),
                registry_generation: 1,
                authority_binding: [coordinate as u8; 32],
                runtime_binding: [9; 32],
                task_result_eligible: false,
            })
        })
        .unwrap();
        job.admit_prepared_graph(graph, plans, 2, None, true)
            .unwrap();
        manager.expose_compatibility(&job.id).unwrap();
        let shared = manager.inner.shared_jobs.clone();
        for coordinate in 1..=2 {
            let claimed = next_shared_sequence(&shared, &job.id).unwrap();
            shared
                .claim(&job.id, coordinate, true, claimed, |_, _| {
                    Ok::<_, SharedJournalError>(())
                })
                .unwrap();
            let settle = next_shared_sequence(&shared, &job.id).unwrap();
            shared
                .settle_unlaunched(
                    &job.id,
                    coordinate,
                    claimed,
                    settle,
                    |_, sequence| {
                        if sequence == claimed {
                            Ok(())
                        } else {
                            Err(aw_tools::filesystem::job::Error::StaleGeneration)
                        }
                    },
                    |_, _| Ok::<_, SharedJournalError>(()),
                )
                .unwrap();
        }
        let snapshot = shared.runtime_snapshot(&job.id).unwrap();
        assert_eq!(snapshot.stop_reason, None);
        assert_eq!(snapshot.summary, Some((SharedPrimary::Skip, false, false)));
        manager.finish(&job.id, JobCompletion::from_shared_graph(&snapshot));

        let status: serde_json::Value =
            serde_json::from_str(&manager.status(&job.id).unwrap()).unwrap();
        assert_eq!(status["status"], "skip");
        assert_eq!(status["graph"]["finished"], 2);
        assert_eq!(status["graph"]["total"], 2);
        assert_eq!(status["graph"]["summary"]["primary"], "skip");
        assert!(
            status["graph"]["compact"]
                .as_str()
                .is_some_and(|value| value.contains("1-2 skip"))
        );

        let listed: serde_json::Value = serde_json::from_str(&manager.list(0, 64)).unwrap();
        let row = listed["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["job_id"] == job.id)
            .unwrap();
        assert_eq!(row["status"], "skip");
    }

    #[test]
    fn unproven_cancel_or_timeout_maps_to_shared_unknown() {
        for (completion, reason) in [
            (
                JobCompletion::cancelled(StreamSummary::default(), StreamSummary::default()),
                SharedProcessStopReason::Cancelled,
            ),
            (
                JobCompletion::timed_out(StreamSummary::default(), StreamSummary::default()),
                SharedProcessStopReason::Timeout,
            ),
        ] {
            let completion = completion.with_process_facts(
                SharedProcessExit::Unknown,
                Some(SharedProcessStopRecord::unknown(reason)),
            );
            let terminal = shared_process_terminal(&completion);
            assert_eq!(terminal.exit, SharedProcessExit::Unknown);
            assert_eq!(terminal.stop, None);
            assert_eq!(terminal.status().primary, SharedPrimary::Unknown);
        }
    }

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

        let retained = parent.join(format!(
            "{SESSION_PREFIX}retained-test-{}",
            Uuid::new_v4().simple()
        ));
        // Build this synthetic crash-leftover outside the session namespace.
        // Concurrent JobManager tests legitimately sweep unlocked session dirs,
        // so publishing the fixture before its unresolved marker exists makes
        // the test itself racy.
        let retained_staging = parent.join(format!(
            ".retained-test-staging-{}",
            Uuid::new_v4().simple()
        ));
        crate::private_state::prepare_owned_dir(&retained_staging).unwrap();
        create_private_file(&retained_staging.join(SESSION_LOCK_FILE)).unwrap();
        mark_session_unresolved(&retained_staging).unwrap();
        fs::write(retained_staging.join("uncertain.stdout"), b"evidence").unwrap();
        fs::rename(&retained_staging, &retained).unwrap();

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
        assert!(
            retained.join("uncertain.stdout").exists(),
            "explicitly retained unresolved evidence must survive the stale-session sweep"
        );
        assert!(second.protected_path().exists());

        first.cancel(&first_job.id).unwrap();
        second.cancel(&second_job.id).unwrap();
        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();
        remove_session_directory(&retained).unwrap();
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
    fn spawn_without_tokio_runtime_fails_before_job_or_session_admission() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let root = manager.protected_path();
        let error = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("no-runtime".to_owned()),
                |_job| {
                    std::future::ready(JobCompletion::completed(
                        Some(0),
                        true,
                        StreamSummary::default(),
                        StreamSummary::default(),
                    ))
                },
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires an active Tokio runtime")
        );
        assert!(manager.inner.state.lock().unwrap().jobs.is_empty());
        assert!(manager.request_job_id("no-runtime").is_none());
        assert!(
            !root.exists(),
            "runtime preflight must precede session materialization"
        );
    }

    #[tokio::test]
    async fn panicking_runner_constructor_rolls_back_admission_without_poisoning_manager() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let request_key = "constructor-panic".to_owned();
        let error = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some(request_key.clone()),
                |_job| -> std::future::Ready<JobCompletion> { panic!("runner constructor bug") },
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("runner construction panicked before managed execution")
        );
        assert!(manager.request_job_id(&request_key).is_none());
        assert_eq!(manager.inner.state.lock().unwrap().jobs.len(), 0);
        assert!(
            std::fs::read_dir(manager.protected_path())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    !name.ends_with(".stdout") && !name.ends_with(".stderr")
                }),
            "failed admission must not leave spool files"
        );

        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some(request_key.clone()),
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
        assert!(started.is_new);
        manager.wait_until(&started.id, None).await.unwrap();
        assert_eq!(
            manager.request_job_id(&request_key),
            Some(started.id.clone())
        );
        drop(started);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn panicking_runner_becomes_terminal_internal_failure() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                None,
                |_job| async move { panic!("runner bug") },
            )
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(2),
            manager.wait_until(&started.id, None),
        )
        .await
        .expect("panicking runner must still reach a terminal job state")
        .unwrap();
        let status: serde_json::Value =
            serde_json::from_str(&manager.status(&started.id).unwrap()).unwrap();
        assert_eq!(status["status"], "failed");
        assert!(
            status["error"]
                .as_str()
                .is_some_and(|error| error.contains("runner panicked"))
        );
        drop(started);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn runner_cannot_publish_terminal_state_before_spool_writers_are_idle() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let (writer_tx, writer_rx) = tokio::sync::oneshot::channel();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                None,
                move |job| async move {
                    writer_tx
                        .send(
                            job.begin_spool_writer()
                                .expect("runner writer registration is open"),
                        )
                        .unwrap();
                    JobCompletion::completed(
                        Some(0),
                        true,
                        StreamSummary::default(),
                        StreamSummary::default(),
                    )
                },
            )
            .unwrap();
        let writer = writer_rx.await.unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            !manager.inner.state.lock().unwrap().jobs[&started.id]
                .status()
                .terminal(),
            "JobManager must enforce writer quiescence before terminal publication"
        );
        drop(writer);
        assert!(matches!(
            manager.wait_until(&started.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        drop(started);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn sealing_spool_writers_blocks_late_registration() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let account = DrainWriters::new();
        let writer = account.begin_one().expect("writer registration is open");
        account.seal();
        assert!(
            account.begin_one().is_none(),
            "sealed terminal/shutdown state must reject every later writer registration"
        );
        drop(writer);
        account.wait_idle().await;
        drop(account);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_idle_wait_cannot_miss_the_final_drop() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let account = DrainWriters::new();
        for _ in 0..256 {
            let writer = account.begin_one().expect("writer registration is open");
            let waiter_account = account.clone();
            let waiter = tokio::spawn(async move { waiter_account.wait_idle().await });
            tokio::task::yield_now().await;
            drop(writer);
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("final writer notification must never be lost")
                .unwrap();
        }
        drop(account);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_waits_for_registered_spool_writers_to_quiesce() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let root = manager.protected_path().to_path_buf();
        let writer = job
            .begin_spool_writer()
            .expect("writer registration is open before shutdown");
        let shutdown_manager = manager.clone();
        let shutdown = tokio::spawn(async move { shutdown_manager.shutdown().await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !shutdown.is_finished(),
            "shutdown must not unlink spool state while a registered writer is live"
        );
        drop(writer);
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .expect("shutdown should complete after the writer becomes idle")
            .unwrap()
            .unwrap();
        assert!(!root.exists());
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
            .output_page(
                &job.id,
                JobStream::Stdout,
                0,
                raw(100),
                serialized(4096),
                false,
            )
            .unwrap();
        assert!(first.contains("hello"));
        assert!(job.stdout_path.exists());
        manager
            .output_page(
                &job.id,
                JobStream::Stderr,
                0,
                raw(100),
                serialized(4096),
                false,
            )
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
                .output_page(
                    &job.id,
                    JobStream::Stdout,
                    0,
                    raw(64),
                    serialized(4096),
                    false,
                )
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
                .output_page(
                    &job.id,
                    JobStream::Stdout,
                    3,
                    raw(64),
                    serialized(4096),
                    false,
                )
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
                .output_page(
                    &job.id,
                    JobStream::Stdout,
                    0,
                    raw(2),
                    serialized(4096),
                    false,
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first["data"], "a");
        assert_eq!(first["next_offset"], 1);
        let second: serde_json::Value = serde_json::from_str(
            &manager
                .output_page(
                    &job.id,
                    JobStream::Stdout,
                    1,
                    raw(8),
                    serialized(4096),
                    false,
                )
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
            .output_page(
                &job.id,
                JobStream::Stdout,
                0,
                raw(100),
                serialized(4096),
                false,
            )
            .unwrap();
        let replay = manager
            .output_page(
                &job.id,
                JobStream::Stdout,
                0,
                raw(100),
                serialized(4096),
                false,
            )
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
                node: None,
                stream: "stdout",
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
        let payload_ceiling = job_output_payload_ceiling(serialized(ceiling)).unwrap();
        let page = manager
            .output_page(
                &job.id,
                JobStream::Stdout,
                0,
                raw(usize::MAX),
                serialized(ceiling),
                false,
            )
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
    async fn capacity_skips_reclaimable_job_with_stuck_cleanup() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let stuck = manager.create_for_test(30).unwrap();
        manager.finish(
            &stuck.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary::default(),
                StreamSummary::default(),
            ),
        );
        fs::remove_file(&stuck.stderr_path).unwrap();
        fs::create_dir(&stuck.stderr_path).unwrap();

        for _ in 1..MAX_RETAINED_JOBS {
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
        let extra = manager.create_for_test(30).unwrap();
        assert!(manager.status(&extra.id).is_ok());
        assert!(
            manager
                .inner
                .state
                .lock()
                .unwrap()
                .jobs
                .contains_key(&stuck.id),
            "failed cleanup stays retryable while a later safe candidate is reclaimed"
        );
        // Repair the deliberate test-only filesystem corruption before asking
        // ordinary shutdown to prove complete cleanup.
        fs::remove_dir(&stuck.stderr_path).unwrap();
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

    #[tokio::test]
    async fn hidden_delivery_cleanup_failure_is_bounded_and_retries_to_removal() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("cleanup-only".to_owned()),
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
        assert!(matches!(
            manager.wait_until(&started.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        let stderr = {
            let state = manager.inner.state.lock().unwrap();
            state.jobs[&started.id].stderr_path.clone()
        };
        fs::remove_file(&stderr).unwrap();
        fs::create_dir(&stderr).unwrap();
        manager.mark_synchronous_result(&started.id).unwrap();
        assert_eq!(
            manager.acknowledge_request_delivery("cleanup-only"),
            RequestDeliveryAck::Acknowledged
        );
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&started.id];
            assert_eq!(entry.retention, RetentionClass::CleanupOnly);
            assert_eq!(retained_job_slots(&state), 0);
            assert_eq!(transport_retention_slots(&state), 1);
            assert!(entry.output_cleanup_pending);
            assert!(!state.request_jobs.contains_key("cleanup-only"));
        }
        fs::remove_dir(&stderr).unwrap();
        sweep_expired(&manager.inner);
        assert!(
            !manager
                .inner
                .state
                .lock()
                .unwrap()
                .jobs
                .contains_key(&started.id)
        );
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
        assert_eq!(status["stdout"]["released"], true);
        assert_eq!(status["stderr"]["released"], false);
        assert_eq!(
            status["stdout"]["stored_bytes"], 0,
            "a successfully deleted stream must report zero retained bytes even while sibling cleanup is retryable"
        );
        let released_page: serde_json::Value = serde_json::from_str(
            &manager
                .output_page(
                    &job.id,
                    JobStream::Stdout,
                    0,
                    raw(64),
                    serialized(4096),
                    false,
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(released_page["stored_bytes"], 0);
        assert_eq!(released_page["complete"], true);
        assert_eq!(released_page["released"], true);
        assert_eq!(released_page["data"], "");
        let page_error = manager
            .output_page(
                &job.id,
                JobStream::Stdout,
                1,
                raw(64),
                serialized(4096),
                false,
            )
            .unwrap_err()
            .to_string();
        assert!(
            page_error.contains("beyond stored output (0 bytes)"),
            "jobs_read must validate against current retained bytes after partial cleanup: {page_error}"
        );
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&job.id];
            assert!(entry.output_cleanup_pending);
            assert!(entry.stdout_released);
            assert!(!entry.stderr_released);
            assert!(!entry.all_output_released());
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
        assert!(!remove_file_releasing(
            job.legacy_spool.as_deref(),
            bad.path()
        ));
        assert!(truncate_file_releasing(job.legacy_spool.as_deref(), bad.path(), 0).is_none());
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
    async fn revoked_hidden_request_discards_output_without_exposing_job() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let request_key = "revoked-sync".to_owned();
        let started = manager
            .spawn(30, 1024, Some(request_key.clone()), |job| async move {
                assert!(job.mark_running(Some([7; 32])));
                JobCompletion::completed(
                    Some(0),
                    true,
                    StreamSummary::default(),
                    StreamSummary::default(),
                )
            })
            .unwrap();
        let id = started.id.clone();
        drop(started);
        assert!(matches!(
            manager.wait_until(&id, None).await.unwrap(),
            JobWait::Terminal
        ));
        assert_eq!(
            manager.mark_synchronous_result(&id).unwrap(),
            JobDisposition::Synchronous
        );
        assert_eq!(
            manager.request_shell_guard_generation(&request_key),
            Some([7; 32])
        );

        assert_eq!(
            manager.discard_hidden_request_delivery(&request_key),
            RequestRetirement::Retired
        );
        assert!(manager.request_job_id(&request_key).is_none());
        assert!(
            !manager.inner.state.lock().unwrap().jobs.contains_key(&id),
            "revoked hidden synchronous result must be cleaned rather than exposed"
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn revoked_running_hidden_request_cancels_then_discards_without_exposure() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let request_key = "revoked-running".to_owned();
        let started = manager
            .spawn(30, 1024, Some(request_key.clone()), |job| async move {
                assert!(job.mark_running(Some([8; 32])));
                job.cancel.cancelled().await;
                JobCompletion::cancelled(StreamSummary::default(), StreamSummary::default())
            })
            .unwrap();
        let id = started.id.clone();
        drop(started);

        for _ in 0..100 {
            if manager.request_shell_guard_generation(&request_key) == Some([8; 32]) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            manager.request_shell_guard_generation(&request_key),
            Some([8; 32])
        );
        assert_eq!(
            manager.discard_hidden_request_delivery(&request_key),
            RequestRetirement::Busy,
            "running revoked work must be cancelled before its hidden state is cleaned"
        );
        assert!(manager.owns_request_key(&request_key));

        assert!(matches!(
            manager.wait_until(&id, None).await.unwrap(),
            JobWait::Terminal
        ));
        assert_eq!(
            manager.discard_hidden_request_delivery(&request_key),
            RequestRetirement::Retired
        );
        assert!(!manager.owns_request_key(&request_key));
        assert!(manager.status(&id).is_err());
        let listed: serde_json::Value = serde_json::from_str(&manager.list(0, 64)).unwrap();
        assert!(listed["jobs"].as_array().unwrap().is_empty());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn synchronous_request_replays_from_spools_until_delivery_then_releases() {
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
        assert!(!state.request_jobs.contains_key("request-key"));
        assert!(
            !state.jobs.contains_key(&started.id),
            "positively delivered hidden requests need no retained metadata"
        );
        assert!(!started.id.is_empty());
        drop(state);
        assert!(
            !manager
                .inner
                .root
                .join(format!("{}.stdout", started.id))
                .exists()
        );
        assert!(
            !manager
                .inner
                .root
                .join(format!("{}.stderr", started.id))
                .exists()
        );
    }

    #[tokio::test]
    async fn abandoned_keyed_response_releases_ordinary_capacity_but_preserves_replay() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("abandoned-request".to_owned()),
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
        let id = started.id.clone();
        drop(started);
        assert!(matches!(
            manager.wait_until(&id, None).await.unwrap(),
            JobWait::Terminal
        ));
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&id];
            assert_eq!(entry.disposition, JobDisposition::Pending);
            assert_eq!(entry.retention, RetentionClass::TransportOnly);
            assert_eq!(retained_job_slots(&state), 0);
            assert_eq!(state.request_jobs.get("abandoned-request"), Some(&id));
        }

        let replay = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("abandoned-request".to_owned()),
                |_job| async { panic!("redelivery must reuse the abandoned execution") },
            )
            .unwrap();
        assert!(!replay.is_new);
        assert_eq!(replay.id, id);
        let _ = manager.tool_result(&id, None).unwrap().unwrap();
        assert_eq!(
            manager.mark_synchronous_result(&id).unwrap(),
            JobDisposition::Synchronous
        );
        assert!(matches!(
            manager.replay_handoff(&id).unwrap(),
            Some(JobHandoff::Synchronous)
        ));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn one_abandoned_redelivery_does_not_release_a_surviving_attempts_slot() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let first = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("concurrent-attempts".to_owned()),
                |_job| async { std::future::pending::<JobCompletion>().await },
            )
            .unwrap();
        let duplicate = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("concurrent-attempts".to_owned()),
                |_job| async { panic!("duplicate runner must never launch") },
            )
            .unwrap();
        assert_eq!(first.id, duplicate.id);
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&first.id];
            assert_eq!(entry.response_attempts, 2);
            assert_eq!(entry.retention, RetentionClass::Ordinary);
        }
        let id = first.id.clone();
        drop(first);
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&id];
            assert_eq!(entry.response_attempts, 1);
            assert_eq!(entry.retention, RetentionClass::Ordinary);
            assert_eq!(retained_job_slots(&state), 1);
        }
        drop(duplicate);
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&id];
            assert_eq!(entry.response_attempts, 0);
            assert_eq!(entry.retention, RetentionClass::TransportOnly);
            assert_eq!(retained_job_slots(&state), 0);
        }
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn many_abandoned_keyed_responses_cannot_exhaust_ordinary_capacity() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let count = MAX_RETAINED_JOBS + 32;
        for index in 0..count {
            let started = manager
                .spawn(
                    30,
                    crate::limits::DEFAULT_TRANSFER_BYTES,
                    Some(format!("abandoned-{index}")),
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
            let id = started.id.clone();
            drop(started);
            assert!(matches!(
                manager.wait_until(&id, None).await.unwrap(),
                JobWait::Terminal
            ));
        }
        {
            let state = manager.inner.state.lock().unwrap();
            assert_eq!(retained_job_slots(&state), 0);
            assert_eq!(state.request_jobs.len(), count);
            assert!(state.jobs.values().all(|entry| {
                entry.disposition == JobDisposition::Pending
                    && entry.retention == RetentionClass::TransportOnly
                    && entry.response_attempts == 0
            }));
        }
        let ordinary = manager.create_for_test(30).unwrap();
        assert!(manager.status(&ordinary.id).is_ok());
        manager.shutdown().await.unwrap();
    }

    #[test]
    fn failed_prelaunch_shared_exposure_discards_provisional_job() {
        let manager = SharedJobManager::new(SharedJobLimits {
            max_retained_jobs: 2,
            max_exposed_jobs: 1,
            max_hidden_jobs: 2,
            max_active_nodes: 1,
            max_hidden_replays: 2,
            max_spool_bytes: 1024,
        })
        .unwrap();
        let graph = SharedGraph::compile(
            &serde_json::json!({"ops":[["read", {"path":"a"}]]}),
            |_, _| true,
        )
        .unwrap();
        let plans = plan_graph(&graph, |_, _| {
            Ok::<_, ()>(SharedNodePolicy {
                budget: SharedSemanticBudget {
                    work_bytes: 1,
                    result_bytes: 128,
                },
                footprint: SharedFootprint::none(),
                registry_generation: 1,
                authority_binding: [1; 32],
                runtime_binding: [2; 32],
                task_result_eligible: false,
            })
        })
        .unwrap();
        for (id, exposed) in [("visible", true), ("candidate", false)] {
            manager
                .admit_runtime(
                    graph.clone(),
                    plans.clone(),
                    SharedRuntimeAdmission {
                        id,
                        runtime_session: "s",
                        epoch: 1,
                        admission_sequence: 1,
                        width: 1,
                        exposed,
                    },
                )
                .unwrap();
        }
        let error = reconcile_prelaunch_shared_exposure(
            &manager,
            "candidate",
            "fixture prelaunch exposure failed",
        )
        .unwrap_err();
        assert!(error.to_string().contains("Capacity"), "{error}");
        assert!(matches!(
            manager.runtime_snapshot("candidate"),
            Err(aw_tools::filesystem::job_manager::Error::NotFound)
        ));
        assert_eq!(manager.get("visible").unwrap().id, "visible");
    }

    #[tokio::test]
    async fn exposure_committed_before_shared_admission_is_reconciled_before_launch() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let release = Arc::new(Notify::new());
        let claimed = Arc::new(Notify::new());
        let runner_manager = manager.clone();
        let runner_release = release.clone();
        let runner_claimed = claimed.clone();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                None,
                move |job| {
                    let runner_manager = runner_manager.clone();
                    async move {
                        runner_release.notified().await;
                        let writers = claim_shared_shell_writers(&runner_manager, &job);
                        // The local compatibility representation was committed
                        // before this shared admission. The shared Job must be
                        // visible before any process/effect can be marked live.
                        assert!(runner_manager.inner.shared_jobs.get(&job.id).is_ok());
                        runner_claimed.notify_one();
                        job.mark_launched().unwrap();
                        writers.stdout.finish().unwrap();
                        writers.stderr.finish().unwrap();
                        JobCompletion::completed(
                            Some(0),
                            true,
                            job.shared_stream_summary(JobStream::Stdout, false),
                            job.shared_stream_summary(JobStream::Stderr, false),
                        )
                    }
                },
            )
            .unwrap();
        let id = started.id.clone();
        drop(started);
        {
            let state = manager.inner.state.lock().unwrap();
            assert_eq!(state.jobs[&id].disposition, JobDisposition::Compatibility);
            assert!(state.jobs[&id].shared_job.is_none());
        }
        assert!(matches!(
            manager.inner.shared_jobs.runtime_snapshot(&id),
            Err(aw_tools::filesystem::job_manager::Error::NotFound)
        ));
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), claimed.notified())
            .await
            .expect("shared admission must reconcile exposure");
        assert_eq!(manager.inner.shared_jobs.get(&id).unwrap().id, id);
        assert!(matches!(
            manager
                .wait_until(&id, Some(TokioInstant::now() + Duration::from_secs(2)))
                .await
                .unwrap(),
            JobWait::Terminal
        ));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn abandoned_local_response_becomes_recoverable_compatibility_job() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                None,
                |_job| async { std::future::pending::<JobCompletion>().await },
            )
            .unwrap();
        let id = started.id.clone();
        drop(started);
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&id];
            assert_eq!(entry.disposition, JobDisposition::Compatibility);
            assert_eq!(entry.retention, RetentionClass::Ordinary);
            assert!(entry.retention_anchor.is_some());
        }
        let listed: serde_json::Value = serde_json::from_str(&manager.list(0, 64)).unwrap();
        assert_eq!(listed["jobs"][0]["job_id"], id);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn first_committed_response_disposition_wins_concurrent_redelivery_races() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let exposed = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("exposure-wins".to_owned()),
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
        assert!(matches!(
            manager.wait_until(&exposed.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        let _rendered = manager.tool_result(&exposed.id, None).unwrap().unwrap();
        assert!(matches!(
            manager.handoff(&exposed.id, false, None).unwrap(),
            JobHandoff::Compatibility(_)
        ));
        assert_eq!(
            manager.mark_synchronous_result(&exposed.id).unwrap(),
            JobDisposition::Compatibility,
            "a late synchronous contender must replay the already-committed receipt"
        );
        assert!(matches!(
            manager.replay_handoff(&exposed.id).unwrap(),
            Some(JobHandoff::Compatibility(_))
        ));

        let synchronous = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("sync-wins".to_owned()),
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
        assert!(matches!(
            manager.wait_until(&synchronous.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        let _ = manager.tool_result(&synchronous.id, None).unwrap().unwrap();
        assert_eq!(
            manager.mark_synchronous_result(&synchronous.id).unwrap(),
            JobDisposition::Synchronous
        );
        assert!(matches!(
            manager.handoff(&synchronous.id, true, None).unwrap(),
            JobHandoff::Synchronous
        ));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delivered_exposed_request_gets_a_fresh_user_visible_retention_window() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("delayed-receipt".to_owned()),
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
        assert!(matches!(
            manager.wait_until(&started.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        assert!(matches!(
            manager.handoff(&started.id, false, None).unwrap(),
            JobHandoff::Compatibility(_)
        ));
        {
            let mut state = manager.inner.state.lock().unwrap();
            let entry = state.jobs.get_mut(&started.id).unwrap();
            let old = Instant::now() - Duration::from_secs(61);
            entry.terminal_at = Some(old);
            entry.retention_anchor = Some(old);
        }
        sweep_expired(&manager.inner);
        assert!(
            manager
                .inner
                .state
                .lock()
                .unwrap()
                .jobs
                .contains_key(&started.id)
        );

        manager.acknowledge_request_delivery("delayed-receipt");
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&started.id];
            assert!(entry.request_key.is_none());
            assert!(
                entry
                    .retention_anchor
                    .is_some_and(|anchor| anchor.elapsed() < Duration::from_secs(1))
            );
        }
        sweep_expired(&manager.inner);
        assert!(manager.status(&started.id).is_ok());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn replayed_native_task_advertises_retention_from_the_delivery_attempt() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("delayed-task".to_owned()),
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
        assert!(matches!(
            manager.wait_until(&started.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        assert!(matches!(
            manager.handoff(&started.id, true, None).unwrap(),
            JobHandoff::NativeTask(_)
        ));
        {
            let mut state = manager.inner.state.lock().unwrap();
            let entry = state.jobs.get_mut(&started.id).unwrap();
            let now = Instant::now();
            entry.created = now - Duration::from_secs(62);
            entry.terminal_at = Some(now - Duration::from_secs(61));
            entry.retention_anchor = Some(now - Duration::from_secs(61));
        }
        let Some(JobHandoff::NativeTask(task)) = manager.replay_handoff(&started.id).unwrap()
        else {
            panic!("native task disposition must replay identically")
        };
        assert!(
            task.ttl_ms.is_some_and(|ttl| ttl >= 121_000),
            "replayed task must promise a full retention window from this delivery attempt: {task:?}"
        );
        manager.acknowledge_request_delivery("delayed-task");
        assert!(manager.detailed_task(&started.id).is_ok());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unacknowledged_request_key_survives_generic_job_ttl() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("late-redelivery".to_owned()),
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
        assert!(matches!(
            manager.wait_until(&started.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        let _ = manager.tool_result(&started.id, None).unwrap().unwrap();
        manager.mark_synchronous_result(&started.id).unwrap();
        {
            let mut state = manager.inner.state.lock().unwrap();
            let entry = state.jobs.get_mut(&started.id).unwrap();
            entry.terminal_at = Some(Instant::now() - Duration::from_secs(61));
            assert_eq!(entry.retention, RetentionClass::TransportOnly);
        }

        sweep_expired(&manager.inner);
        {
            let state = manager.inner.state.lock().unwrap();
            assert_eq!(
                state.request_jobs.get("late-redelivery"),
                Some(&started.id),
                "ordinary job TTL must not retire transport dedupe authority"
            );
            assert!(state.jobs.contains_key(&started.id));
        }

        let duplicate = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("late-redelivery".to_owned()),
                |_job| async { panic!("late redelivery must not launch another runner") },
            )
            .unwrap();
        assert!(!duplicate.is_new);
        assert_eq!(duplicate.id, started.id);
        manager.acknowledge_request_delivery("late-redelivery");
        assert!(
            !manager
                .inner
                .state
                .lock()
                .unwrap()
                .jobs
                .contains_key(&started.id)
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn synchronous_disposition_rejects_a_running_job_without_releasing_it() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                None,
                |_job| async { std::future::pending::<JobCompletion>().await },
            )
            .unwrap();

        let error = manager.mark_synchronous_result(&started.id).unwrap_err();
        assert!(error.to_string().contains("still running"));
        assert!(
            manager.inner.state.lock().unwrap().jobs[&started.id].disposition
                == JobDisposition::Pending,
            "an invalid early transition must not release the pressure-eviction guard"
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn pending_response_disposition_is_not_capacity_evicted() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
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
        assert!(matches!(
            manager.wait_until(&started.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        assert_eq!(
            manager.inner.state.lock().unwrap().jobs[&started.id].disposition,
            JobDisposition::Pending
        );

        for _ in 1..MAX_RETAINED_JOBS {
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
        }
        let error = manager.create_for_test(30).unwrap_err();
        assert!(error.to_string().contains("too many retained shell jobs"));
        assert!(
            manager
                .inner
                .state
                .lock()
                .unwrap()
                .jobs
                .contains_key(&started.id),
            "capacity pressure must not evict an execution before its initiating response commits"
        );

        let _ = manager.tool_result(&started.id, None).unwrap().unwrap();
        manager.mark_synchronous_result(&started.id).unwrap();
        assert!(
            manager.inner.state.lock().unwrap().jobs[&started.id].disposition
                != JobDisposition::Pending
        );
        let started_id = started.id.clone();
        drop(started);
        let extra = manager.create_for_test(30).unwrap();
        {
            let state = manager.inner.state.lock().unwrap();
            assert!(state.jobs.contains_key(&extra.id));
            assert!(
                !state.jobs.contains_key(&started_id),
                "resolved empty synchronous metadata may be reclaimed after its response attempt ends"
            );
        }
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ambiguous_hidden_replays_do_not_consume_ordinary_retained_job_capacity() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        for index in 0..(MAX_RETAINED_JOBS + 32) {
            let key = format!("ambiguous-{index}");
            let started = manager
                .spawn(
                    30,
                    crate::limits::DEFAULT_TRANSFER_BYTES,
                    Some(key),
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
            assert!(matches!(
                manager.wait_until(&started.id, None).await.unwrap(),
                JobWait::Terminal
            ));
            manager.mark_synchronous_result(&started.id).unwrap();
        }
        {
            let state = manager.inner.state.lock().unwrap();
            assert!(state.jobs.len() > MAX_RETAINED_JOBS);
            assert_eq!(retained_job_slots(&state), 0);
            assert_eq!(state.request_jobs.len(), MAX_RETAINED_JOBS + 32);
        }

        let ordinary = manager.create_for_test(30).unwrap();
        assert!(manager.status(&ordinary.id).is_ok());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn pending_delivery_cannot_be_acknowledged_and_final_retirement_exposes_it() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("pending-delivery".to_owned()),
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
        let id = started.id.clone();
        assert!(matches!(
            manager.wait_until(&id, None).await.unwrap(),
            JobWait::Terminal
        ));
        assert_eq!(
            manager.acknowledge_request_delivery("pending-delivery"),
            RequestDeliveryAck::Pending
        );
        assert!(manager.owns_request_key("pending-delivery"));
        drop(started);
        assert_eq!(
            manager.retire_request_delivery("pending-delivery"),
            RequestRetirement::Retired
        );
        assert!(!manager.owns_request_key("pending-delivery"));
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&id];
            assert_eq!(entry.disposition, JobDisposition::Compatibility);
            assert_eq!(entry.retention, RetentionClass::Ordinary);
            assert_eq!(retained_job_slots(&state), 1);
        }
        let list: serde_json::Value = serde_json::from_str(&manager.list(0, 16)).unwrap();
        assert!(
            list["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|job| job["job_id"] == id),
            "retired transport-only execution must be recoverable through jobs_list"
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delivery_acknowledgement_requires_a_consistent_request_mapping() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("original-key".to_owned()),
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
        {
            let mut state = manager.inner.state.lock().unwrap();
            state.jobs.get_mut(&started.id).unwrap().request_key = Some("other-key".to_owned());
        }
        manager.acknowledge_request_delivery("original-key");
        let state = manager.inner.state.lock().unwrap();
        assert_eq!(
            state.request_jobs.get("original-key"),
            Some(&started.id),
            "an inconsistent mapping must not be destructively half-acknowledged"
        );
        assert_eq!(
            state.jobs[&started.id].request_key.as_deref(),
            Some("other-key")
        );
    }

    #[tokio::test]
    async fn shared_hidden_synchronous_delivery_releases_owners_unlinks_spools_and_metadata() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let runner_manager = manager.clone();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("shared-hidden-delivery".to_owned()),
                move |job| {
                    let runner_manager = runner_manager.clone();
                    async move {
                        let writers = claim_shared_shell_writers(&runner_manager, &job);
                        job.mark_launched().unwrap();
                        assert_eq!(writers.stdout.append(b"exact").unwrap(), 5);
                        writers.stdout.finish().unwrap();
                        writers.stderr.finish().unwrap();
                        JobCompletion::completed(
                            Some(0),
                            true,
                            job.shared_stream_summary(JobStream::Stdout, false),
                            job.shared_stream_summary(JobStream::Stderr, false),
                        )
                    }
                },
            )
            .unwrap();
        assert!(matches!(
            manager.wait_until(&started.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        let (stdout_path, stderr_path) = {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&started.id];
            (entry.stdout_path.clone(), entry.stderr_path.clone())
        };
        let rendered = manager.tool_result(&started.id, None).unwrap().unwrap();
        let rendered: serde_json::Value =
            serde_json::from_str(&rendered.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(rendered["stdout"], "exact");
        manager.mark_synchronous_result(&started.id).unwrap();
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&started.id];
            assert_eq!(entry.retention, RetentionClass::TransportOnly);
            assert!(!entry.all_output_released());
            assert!(stdout_path.exists() && stderr_path.exists());
        }
        assert_eq!(
            manager.acknowledge_request_delivery("shared-hidden-delivery"),
            RequestDeliveryAck::Acknowledged
        );
        assert!(
            !manager
                .inner
                .state
                .lock()
                .unwrap()
                .jobs
                .contains_key(&started.id),
            "positive delivery must not leave shared hidden metadata in CleanupOnly"
        );
        assert!(!stdout_path.exists() && !stderr_path.exists());
        assert_eq!(manager.inner.shared_jobs.spool_usage().unwrap().0, 0);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn acknowledging_replay_backlog_removes_hidden_metadata_without_slot_burst() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let count = MAX_RETAINED_JOBS + 32;
        for index in 0..count {
            let key = format!("recover-{index}");
            let started = manager
                .spawn(
                    30,
                    crate::limits::DEFAULT_TRANSFER_BYTES,
                    Some(key),
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
            assert!(matches!(
                manager.wait_until(&started.id, None).await.unwrap(),
                JobWait::Terminal
            ));
            manager.mark_synchronous_result(&started.id).unwrap();
        }
        {
            let state = manager.inner.state.lock().unwrap();
            assert_eq!(retained_job_slots(&state), 0);
            assert_eq!(state.request_jobs.len(), count);
            assert_eq!(transport_retention_slots(&state), count);
            assert!(
                state
                    .jobs
                    .values()
                    .all(|entry| entry.retention == RetentionClass::TransportOnly)
            );
        }

        for index in 0..count {
            manager.acknowledge_request_delivery(&format!("recover-{index}"));
        }
        {
            let state = manager.inner.state.lock().unwrap();
            assert!(
                state.jobs.is_empty(),
                "hidden delivered jobs need no retained metadata"
            );
            assert!(state.request_jobs.is_empty());
            assert_eq!(retained_job_slots(&state), 0);
            assert_eq!(transport_retention_slots(&state), 0);
        }
        let ordinary = manager.create_for_test(30).unwrap();
        assert!(manager.status(&ordinary.id).is_ok());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn terminal_request_keeps_its_reserved_slot_until_response_representation_is_chosen() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("handoff-slot".to_owned()),
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
        assert!(matches!(
            manager.wait_until(&started.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&started.id];
            assert_eq!(entry.retention, RetentionClass::Ordinary);
            assert_eq!(retained_job_slots(&state), 1);
        }

        for _ in 1..MAX_RETAINED_JOBS {
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
        }
        assert_eq!(
            retained_job_slots(&manager.inner.state.lock().unwrap()),
            MAX_RETAINED_JOBS
        );
        manager
            .expose_compatibility(&started.id)
            .expect("the terminal request already owns its retained slot");
        {
            let state = manager.inner.state.lock().unwrap();
            assert!(state.jobs[&started.id].disposition.exposed());
            assert_eq!(state.jobs[&started.id].retention, RetentionClass::Ordinary);
            assert_eq!(retained_job_slots(&state), MAX_RETAINED_JOBS);
        }
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn transport_only_redelivery_reserves_ordinary_capacity_before_waiting() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let replay = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("replay-transition".to_owned()),
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
        let replay_id = replay.id.clone();
        drop(replay);
        assert!(matches!(
            manager.wait_until(&replay_id, None).await.unwrap(),
            JobWait::Terminal
        ));

        let mut reclaimable = None;
        for index in 0..MAX_RETAINED_JOBS {
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
            if index == 0 {
                reclaimable = Some(job.id);
            }
        }

        let error = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("replay-transition".to_owned()),
                |_job| async { panic!("redelivery must never launch a second runner") },
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("too many retained shell jobs"), "{error}");
        {
            let state = manager.inner.state.lock().unwrap();
            let replay_entry = &state.jobs[&replay_id];
            assert_eq!(replay_entry.retention, RetentionClass::TransportOnly);
            assert_eq!(replay_entry.response_attempts, 0);
            assert_eq!(retained_job_slots(&state), MAX_RETAINED_JOBS);
        }

        let reclaimable = reclaimable.unwrap();
        manager
            .acknowledge_output(&reclaimable, JobStream::Stdout, 1)
            .unwrap();
        manager
            .acknowledge_output(&reclaimable, JobStream::Stderr, 0)
            .unwrap();
        let duplicate = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("replay-transition".to_owned()),
                |_job| async { panic!("redelivery must reuse the original execution") },
            )
            .unwrap();
        assert!(!duplicate.is_new);
        assert_eq!(duplicate.id, replay_id);
        {
            let state = manager.inner.state.lock().unwrap();
            let replay_entry = &state.jobs[&replay_id];
            assert_eq!(replay_entry.retention, RetentionClass::Ordinary);
            assert_eq!(replay_entry.response_attempts, 1);
            assert_eq!(retained_job_slots(&state), MAX_RETAINED_JOBS);
        }
        drop(duplicate);
        assert_eq!(
            manager.inner.state.lock().unwrap().jobs[&replay_id].retention,
            RetentionClass::TransportOnly
        );
        manager.shutdown().await.unwrap();
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

    #[tokio::test]
    async fn job_list_pages_newest_first_with_advancing_offset() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let mut ids = Vec::new();
        for _ in 0..3 {
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
            manager.expose_compatibility(&job.id).unwrap();
            ids.push(job.id.clone());
            std::thread::sleep(Duration::from_millis(1));
        }
        let first: serde_json::Value = serde_json::from_str(&manager.list(0, 2)).unwrap();
        assert_eq!(first["offset"], 0);
        assert_eq!(first["next_offset"], 2);
        let first_ids = first["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|job| job["job_id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(first_ids, vec![ids[2].as_str(), ids[1].as_str()]);

        let second: serde_json::Value = serde_json::from_str(&manager.list(2, 2)).unwrap();
        assert_eq!(second["offset"], 2);
        assert!(second["next_offset"].is_null());
        assert_eq!(second["jobs"][0]["job_id"], ids[0]);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn job_list_budget_keeps_rows_atomic_and_cursor_advancing() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        for _ in 0..3 {
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
            manager.expose_compatibility(&job.id).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }

        let one = manager.list(0, 1);
        let ceiling = one.len();
        let fitted = manager
            .list_fitted(0, 3, |page| page.len() <= ceiling)
            .unwrap();
        let fitted: serde_json::Value = serde_json::from_str(&fitted).unwrap();
        assert_eq!(fitted["jobs"].as_array().unwrap().len(), 1);
        assert_eq!(fitted["offset"], 0);
        assert_eq!(fitted["next_offset"], 1);

        let error = manager.list_fitted(0, 3, |_| false).unwrap_err();
        assert!(error.to_string().contains("one complete Job row"));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn job_list_breaks_equal_creation_times_by_job_id() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let mut ids = Vec::new();
        for _ in 0..3 {
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
            manager.expose_compatibility(&job.id).unwrap();
            ids.push(job.id.clone());
        }
        let tied = Instant::now();
        {
            let mut state = manager.inner.state.lock().unwrap();
            for id in &ids {
                state.jobs.get_mut(id).unwrap().created = tied;
            }
        }
        ids.sort();
        let listed: serde_json::Value = serde_json::from_str(&manager.list(0, 3)).unwrap();
        let actual = listed["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|job| job["job_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(actual, ids);
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
        let listed: serde_json::Value = serde_json::from_str(&manager.list(0, 64)).unwrap();
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
    fn resource_policy_match_proves_shared_physical_spool_limit() {
        let manager = JobManager::with_spool_limit(Duration::from_secs(77), 4096).unwrap();
        assert!(manager.resource_policy_matches(Duration::from_secs(77), 4096));
        assert!(!manager.resource_policy_matches(Duration::from_secs(78), 4096));
        assert!(!manager.resource_policy_matches(Duration::from_secs(77), 8192));
        manager
            .inner
            .spool_limit_bytes
            .store(8192, Ordering::Relaxed);
        assert!(
            !manager.resource_policy_matches(Duration::from_secs(77), 8192),
            "legacy mirror alone must not widen the shared physical quota"
        );
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
    fn cleanup_reconciles_phantom_spool_reservations() {
        let manager = JobManager::with_spool_limit(Duration::from_secs(60), 64).unwrap();
        let job = manager.create_for_test(30).unwrap();
        assert_eq!(job.reserve_spool(16), 16);
        assert_eq!(job.legacy_spool.as_ref().unwrap().charged(), 16);
        assert_eq!(manager.inner.spool_used_bytes.load(Ordering::Relaxed), 16);

        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary::default(),
                StreamSummary::default(),
            ),
        );
        manager.expose_compatibility(&job.id).unwrap();
        manager
            .acknowledge_output(&job.id, JobStream::Stdout, 0)
            .unwrap();

        assert_eq!(job.legacy_spool.as_ref().unwrap().charged(), 0);
        assert_eq!(manager.inner.spool_used_bytes.load(Ordering::Relaxed), 0);
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
        manager.mark_synchronous_result(&started.id).unwrap();
        assert_eq!(
            manager.acknowledge_request_delivery("request-reader"),
            RequestDeliveryAck::Acknowledged
        );
        assert!(
            stdout_path.exists(),
            "active reader must defer spool deletion"
        );
        release_reader(&manager.inner, &started.id);
        assert!(!stdout_path.exists());
        assert!(
            !manager
                .inner
                .state
                .lock()
                .unwrap()
                .jobs
                .contains_key(&started.id),
            "cleanup-only metadata must disappear with the last reader"
        );
    }

    #[test]
    fn job_errors_are_bounded_once_for_every_status_surface() {
        let completion =
            JobCompletion::failed("x".repeat(aw_tools::diagnostic::MAX_DIAGNOSTIC_BYTES * 8));
        let error = completion.error.as_deref().unwrap();
        assert!(error.len() <= aw_tools::diagnostic::MAX_DIAGNOSTIC_BYTES);
        assert!(error.ends_with('…'));

        let internal = JobCompletion::internal_failed(
            "y".repeat(aw_tools::diagnostic::MAX_DIAGNOSTIC_BYTES * 8),
            StreamSummary::default(),
            StreamSummary::default(),
        );
        assert!(
            internal.error.as_ref().unwrap().len() <= aw_tools::diagnostic::MAX_DIAGNOSTIC_BYTES
        );
        assert!(
            internal.internal_error.as_ref().unwrap().len()
                <= aw_tools::diagnostic::MAX_DIAGNOSTIC_BYTES
        );
    }

    #[test]
    fn terminal_surfaces_never_read_past_frozen_output_prefix() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        fs::write(&job.stdout_path, b"stableEXTRA").unwrap();
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

        let status: serde_json::Value =
            serde_json::from_str(&manager.status(&job.id).unwrap()).unwrap();
        assert_eq!(status["stdout"]["stored_bytes"], 6);

        let page: serde_json::Value = serde_json::from_str(
            &manager
                .output_page(
                    &job.id,
                    JobStream::Stdout,
                    0,
                    raw(100),
                    serialized(4096),
                    false,
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(page["stored_bytes"], 6);
        assert_eq!(page["next_offset"], 6);
        assert_eq!(page["data"], "stable");
        assert_eq!(page["complete"], true);

        manager.expose_native_task(&job.id, None).unwrap().unwrap();
        let task = manager.detailed_task(&job.id).unwrap();
        let TaskPayload::Completed { result } = task.payload else {
            panic!("terminal native task must return a completed payload")
        };
        let rendered = serde_json::Value::Object(result).to_string();
        assert!(rendered.contains("stable"), "{rendered}");
        assert!(!rendered.contains("EXTRA"), "{rendered}");
    }

    #[test]
    fn native_task_compaction_tracks_retained_bytes_without_rewriting_ack_history() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        fs::write(&job.stdout_path, b"abcdefgh").unwrap();
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary {
                    total_bytes: 8,
                    stored_bytes: 8,
                    truncated: false,
                },
                StreamSummary::default(),
            ),
        );
        {
            let mut state = manager.inner.state.lock().unwrap();
            state.jobs.get_mut(&job.id).unwrap().result_limit = 4;
        }
        manager.expose_native_task(&job.id, None).unwrap().unwrap();

        let first: serde_json::Value = serde_json::from_str(
            &manager
                .acknowledge_output(&job.id, JobStream::Stdout, 8)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first["stdout"]["stored_bytes"], 4);
        assert_eq!(first["stdout"]["acknowledged_bytes"], 8);
        assert_eq!(first["stdout"]["released"], true);
        assert_eq!(fs::metadata(&job.stdout_path).unwrap().len(), 4);
        let released_page: serde_json::Value = serde_json::from_str(
            &manager
                .output_page(
                    &job.id,
                    JobStream::Stdout,
                    4,
                    raw(64),
                    serialized(4096),
                    false,
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(released_page["stored_bytes"], 4);
        assert_eq!(released_page["complete"], true);
        assert_eq!(released_page["released"], true);
        assert_eq!(released_page["data"], "");
        let released_error = manager
            .output_page(
                &job.id,
                JobStream::Stdout,
                0,
                raw(64),
                serialized(4096),
                false,
            )
            .unwrap_err()
            .to_string();
        assert!(released_error.contains("already acknowledged and released"));

        // The historical terminal length remains the acknowledgement ceiling, so
        // a lost ACK response can be retried after compaction without becoming an
        // invalid offset. Current retained bytes remain the compacted Task prefix.
        let replay: serde_json::Value = serde_json::from_str(
            &manager
                .acknowledge_output(&job.id, JobStream::Stdout, 8)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(replay["stdout"]["stored_bytes"], 4);
        assert_eq!(replay["stdout"]["acknowledged_bytes"], 8);
        let tail: serde_json::Value = serde_json::from_str(
            &manager
                .output_page(
                    &job.id,
                    JobStream::Stdout,
                    4,
                    raw(64),
                    serialized(4096),
                    false,
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(tail["stored_bytes"], 4);
        assert_eq!(tail["next_offset"], 4);
        assert_eq!(tail["complete"], true);
        assert_eq!(tail["released"], true);
        let page_error = manager
            .output_page(
                &job.id,
                JobStream::Stdout,
                8,
                raw(64),
                serialized(4096),
                false,
            )
            .unwrap_err()
            .to_string();
        assert!(
            page_error.contains("beyond stored output (4 bytes)"),
            "jobs_read must not expose the pre-compaction terminal length: {page_error}"
        );

        let detailed = manager.detailed_task(&job.id).unwrap();
        let TaskPayload::Completed { result } = detailed.payload else {
            panic!("compacted native task must remain complete")
        };
        let rendered = serde_json::Value::Object(result).to_string();
        assert!(rendered.contains("abcd"), "{rendered}");
        assert!(!rendered.contains("efgh"), "{rendered}");
    }

    #[test]
    fn expired_task_is_unavailable_while_cleanup_retry_remains_internal() {
        let manager = JobManager::new(Duration::ZERO).unwrap();
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
        manager.expose_native_task(&job.id, None).unwrap().unwrap();

        // Force TTL cleanup to succeed for stdout but remain retryable for stderr.
        fs::remove_file(&job.stderr_path).unwrap();
        fs::create_dir(&job.stderr_path).unwrap();

        assert!(matches!(manager.status(&job.id), Err(JobError::Unknown(_))));
        let listed: serde_json::Value = serde_json::from_str(&manager.list(0, 64)).unwrap();
        assert!(
            listed["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["job_id"] != job.id),
            "expired cleanup-retry state must not remain user-discoverable"
        );
        assert!(matches!(
            manager.detailed_task(&job.id),
            Err(JobError::Unknown(_))
        ));
        assert!(matches!(
            manager.ensure_native_task(&job.id),
            Err(JobError::Unknown(_))
        ));
        assert!(matches!(
            manager.output_page(
                &job.id,
                JobStream::Stdout,
                0,
                raw(64),
                serialized(4096),
                false
            ),
            Err(JobError::Unknown(_))
        ));
        assert!(matches!(
            manager.acknowledge_output(&job.id, JobStream::Stdout, 1),
            Err(JobError::Unknown(_))
        ));
        assert!(matches!(manager.cancel(&job.id), Err(JobError::Unknown(_))));

        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&job.id];
            assert!(entry.expiration_pending);
            assert!(entry.stdout_released);
            assert!(!entry.stderr_released);
            assert!(!entry.all_output_released());
        }

        fs::remove_dir(&job.stderr_path).unwrap();
        sweep_expired(&manager.inner);
        assert!(
            !manager
                .inner
                .state
                .lock()
                .unwrap()
                .jobs
                .contains_key(&job.id),
            "cleanup retry should remove internal metadata once physical cleanup succeeds"
        );
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
    fn unknown_job_errors_bound_caller_controlled_identifiers() {
        let huge = "x".repeat(aw_tools::diagnostic::MAX_DIAGNOSTIC_BYTES * 4);
        let error = JobError::Unknown(format!("unknown task {huge}")).into_mcp();
        assert!(error.message.len() <= aw_tools::diagnostic::MAX_DIAGNOSTIC_BYTES);
        assert!(error.message.ends_with('…'));
    }

    fn claim_shared_shell_writers(manager: &JobManager, job: &JobHandle) -> SharedShellWriters {
        assert!(manager.bind_policy([9; 32]));
        let result_limit = manager
            .inner
            .state
            .lock()
            .unwrap()
            .jobs
            .get(&job.id)
            .expect("test Job remains admitted")
            .result_limit;
        job.claim_prepared_shell(
            SharedNodeTool::Bash,
            serde_json::json!({
                "command":"printf ok",
                "dir":".",
                "stdin":null,
                "timeout_secs":30,
                "max_output_bytes":result_limit
            }),
            None,
            result_limit,
            MAX_JOB_SPOOL_BYTES_PER_STREAM,
        )
        .unwrap()
    }

    fn claim_shared_shell(manager: &JobManager, job: &JobHandle) {
        let _ = claim_shared_shell_writers(manager, job);
    }

    #[tokio::test]
    async fn shutdown_retains_host_metadata_when_shared_owned_state_is_missing() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let root = manager.protected_path().to_path_buf();
        let job = manager.create_for_test(30).unwrap();
        claim_shared_shell(&manager, &job);
        job.settle_unlaunched();
        manager.inner.shared_jobs.expire_terminal(&job.id).unwrap();
        drop(job);

        let error = manager.shutdown().await.unwrap_err().to_string();
        assert!(error.contains("retained for inspection"), "{error}");
        assert!(root.exists());
        let state = manager.inner.state.lock().unwrap();
        assert_eq!(state.jobs.len(), 1);
        let entry = state.jobs.values().next().unwrap();
        assert!(entry.shared_job.is_some());
        assert!(entry.stdout_path.exists());
        assert!(entry.stderr_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn existing_unresolved_marker_is_rehardened_and_resynced() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("session-marker-resync");
        crate::private_state::prepare_owned_dir(&root).unwrap();
        mark_session_unresolved(&root).unwrap();
        let marker = root.join(SESSION_UNRESOLVED_MARKER);
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o666)).unwrap();

        mark_session_unresolved(&root).unwrap();

        assert_eq!(
            fs::metadata(&marker).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn failed_shutdown_does_not_erase_evidence_on_manager_drop() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let root = manager.protected_path().to_path_buf();
        let job = manager.create_for_test(30).unwrap();
        claim_shared_shell(&manager, &job);
        job.settle_unlaunched();
        manager.inner.shared_jobs.expire_terminal(&job.id).unwrap();
        let stdout = job.stdout_path.clone();
        let stderr = job.stderr_path.clone();
        drop(job);

        assert!(manager.shutdown().await.is_err());
        assert!(root.join(SESSION_UNRESOLVED_MARKER).exists());
        drop(manager);
        assert!(root.exists(), "failed shutdown must retain the session");
        assert!(
            stdout.exists(),
            "failed shutdown must retain stdout evidence"
        );
        assert!(
            stderr.exists(),
            "failed shutdown must retain stderr evidence"
        );

        // The test intentionally injects inconsistent shared-owned state.
        // Remove its isolated fixture only after verifying retention.
        remove_session_directory(&root).unwrap();
    }

    #[tokio::test]
    async fn standalone_delivery_cannot_ack_unread_gaps_or_another_stream() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let writers = claim_shared_shell_writers(&manager, &job);
        job.mark_launched().unwrap();
        manager.expose_compatibility(&job.id).unwrap();
        let content = b"abcdefghijklmnop";
        assert_eq!(writers.stdout.append(content).unwrap(), content.len());
        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        let page_at = |offset| {
            serde_json::from_str::<serde_json::Value>(
                &manager
                    .output_node_page(
                        &job.id,
                        OutputPageRequest {
                            node: None,
                            stream: Some(JobOutputStream::Stdout),
                            offset,
                            requested_bytes: raw(8),
                            response_ceiling: serialized(4096),
                            base64: false,
                        },
                    )
                    .unwrap(),
            )
            .unwrap()
        };

        // Before any projected response, even an empty-prefix ACK has no
        // delivery proof. One out-of-order page cannot create that proof.
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), 0,)
                .is_err()
        );
        let late = page_at(8);
        assert_eq!(late["offset"], 8);
        assert_eq!(late["next_offset"], 16);
        assert_eq!(late["data"], "ijklmnop");
        assert!(
            manager.inner.state.lock().unwrap().jobs[&job.id]
                .delivered_through
                .is_empty(),
            "shared delivery proof must live only in aw-tools"
        );
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), 0,)
                .is_err(),
            "late page may not mint a zero-prefix ACK"
        );
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), 16,)
                .is_err(),
            "late page must not ACK bytes [0,8)"
        );
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stderr), 1,)
                .is_err(),
            "output evidence must be stream-scoped"
        );

        let first = page_at(0);
        assert_eq!(first["next_offset"], 8);
        assert!(
            manager.inner.state.lock().unwrap().jobs[&job.id]
                .delivered_through
                .is_empty(),
            "shared first-page delivery must not repopulate the legacy host ledger"
        );
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), 0,)
                .is_ok(),
            "a projected first page certifies its zero prefix"
        );
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), 8,)
                .is_ok()
        );
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), 16,)
                .is_err(),
            "a previously viewed late page must not bridge the gap"
        );

        let continuation = page_at(8);
        assert_eq!(continuation["next_offset"], 16);
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), 16,)
                .is_ok()
        );

        let owner = manager.inner.shared_jobs.job_output_owner(&job.id).unwrap();
        let state = manager
            .inner
            .shared_jobs
            .output_owner_state(&job.id, 1, SharedOutputStream::Stdout, owner)
            .unwrap();
        assert_eq!(state.through, 16);
        drop(job);
        manager.shutdown().await.unwrap();
    }

    #[test]
    fn shared_process_get_tail_is_aggregate_budgeted_and_never_delivery_credit() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let writers = claim_shared_shell_writers(&manager, &job);
        job.mark_launched().unwrap();
        manager.expose_compatibility(&job.id).unwrap();
        let stdout = vec![b'X'; 700];
        let stderr = vec![b'Y'; 100];
        assert_eq!(writers.stdout.append(&stdout).unwrap(), stdout.len());
        assert_eq!(writers.stderr.append(&stderr).unwrap(), stderr.len());

        let (_, running) = manager.standalone_status(&job.id, None).unwrap();
        let tail = manager
            .standalone_process_tail(&job.id, None, None, running, 16 * 1024)
            .unwrap();
        assert!(tail.contains("stdout [288..700]"), "{tail}");
        assert!(tail.contains("stderr [0..100]"), "{tail}");
        assert_eq!(tail.matches('X').count(), 412, "{tail}");
        assert_eq!(tail.matches('Y').count(), 100, "{tail}");
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), 700)
                .is_err(),
            "jobs_get tail is a pure peek and must not create ACK credit"
        );

        let (_, running) = manager.standalone_status(&job.id, None).unwrap();
        let capped = manager
            .standalone_process_tail(&job.id, None, Some(1024), running, 520)
            .unwrap();
        assert!(serialized_tool_result_len(&capped).unwrap() <= 520);
        assert!(capped.contains("tail=capped") || capped.contains("tail=budget"));

        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        let stdout_summary = job.shared_stream_summary(JobStream::Stdout, false);
        let stderr_summary = job.shared_stream_summary(JobStream::Stderr, false);
        manager.finish(
            &job.id,
            JobCompletion::completed(Some(0), true, stdout_summary, stderr_summary),
        );
        let (_, terminal) = manager.standalone_status(&job.id, None).unwrap();
        let tail = manager
            .standalone_process_tail(&job.id, None, Some(1024), terminal.clone(), 16 * 1024)
            .unwrap();
        assert!(tail.contains("stdout [0..700] eof"), "{tail}");
        assert!(tail.contains("stderr [0..100] eof"), "{tail}");
        assert_eq!(
            manager
                .standalone_process_tail(&job.id, None, Some(0), terminal.clone(), 16 * 1024)
                .unwrap(),
            terminal,
            "tail=0 disables output"
        );
    }

    #[tokio::test]
    async fn shutdown_expires_unacknowledged_shared_output_before_removing_session_dir() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let root = manager.inner.root.clone();
        let job = manager.create_for_test(30).unwrap();
        let writers = claim_shared_shell_writers(&manager, &job);
        job.mark_launched().unwrap();
        assert_eq!(writers.stdout.append(b"retained-without-ack").unwrap(), 20);
        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        let stdout = job.shared_stream_summary(JobStream::Stdout, false);
        let stderr = job.shared_stream_summary(JobStream::Stderr, false);
        manager.finish(
            &job.id,
            JobCompletion::completed(Some(0), true, stdout, stderr),
        );
        assert!(root.exists());
        assert!(job.stdout_path.exists());
        let owner = manager.inner.shared_jobs.job_output_owner(&job.id).unwrap();
        let owner_state = manager
            .inner
            .shared_jobs
            .output_owner_state(&job.id, 1, SharedOutputStream::Stdout, owner)
            .unwrap();
        assert!(!owner_state.acknowledged);
        drop(job);

        manager.shutdown().await.unwrap();
        assert!(
            !root.exists(),
            "shutdown must expire terminal shared output and close file handles before removing the session directory"
        );
    }

    #[test]
    fn bounded_mcp_page_only_credits_serialized_source_prefix() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let writers = claim_shared_shell_writers(&manager, &job);
        job.mark_launched().unwrap();
        let bytes = b"quoted\" slash\\ newline\n	";
        let data = bytes.repeat(160);
        assert_eq!(writers.stdout.append(&data).unwrap(), data.len());
        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        let request = |offset, ceiling, base64| OutputPageRequest {
            node: None,
            stream: Some(JobOutputStream::Stdout),
            offset,
            requested_bytes: raw(data.len()),
            response_ceiling: serialized(ceiling),
            base64,
        };
        assert!(
            manager
                .output_node_page(&job.id, request(0, 1, false))
                .is_err()
        );
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), 1)
                .is_err(),
            "failed response preflight cannot mint delivered proof"
        );

        let rendered = manager
            .output_node_page(&job.id, request(0, 1024, false))
            .unwrap();
        assert!(serialized_tool_result_len(&rendered).unwrap() <= 1024);
        let decorated = aw_mcp_protocol::decorate_and_fit_complete_result(
            crate::mcp::shared_receipt_profile(),
            OperationId::JobsRead,
            CallToolResult::success(vec![ContentBlock::text(rendered.clone())]),
            Some(1_794_737_000),
            serialized(1024),
        )
        .unwrap();
        assert!(
            aw_mcp_protocol::serialized_result_len(&decorated).unwrap() <= 1024,
            "D5 response reserve must leave room for the D6 receipt envelope"
        );
        assert_eq!(
            decorated.content[0].as_text().unwrap().text,
            rendered,
            "D6 decoration must not truncate a jobs_read page after delivery credit is recorded"
        );
        let page: serde_json::Value =
            serde_json::from_str(&decorated.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(page["offset"], 0);
        let through = page["next_offset"].as_u64().unwrap();
        assert!(through > 0 && through < data.len() as u64);
        assert_eq!(
            page["data"],
            String::from_utf8_lossy(&data[..through as usize]).as_ref()
        );
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), through + 1)
                .is_err(),
            "fetching a large spool does not credit unrendered bytes"
        );
        manager
            .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), through)
            .unwrap();
        let late_offset = (through + 40).min(data.len() as u64 - 1);
        let late = manager
            .output_node_page(&job.id, request(late_offset, 1024, true))
            .unwrap();
        assert!(serialized_tool_result_len(&late).unwrap() <= 1024);
        let late: serde_json::Value = serde_json::from_str(&late).unwrap();
        let late_end = late["next_offset"].as_u64().unwrap();
        assert!(late_end > late_offset);
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), late_end)
                .is_err(),
            "base64 page past a gap cannot advance the ACK prefix"
        );

        let bridge = manager
            .output_node_page(&job.id, request(through, 1024, true))
            .unwrap();
        let bridge: serde_json::Value = serde_json::from_str(&bridge).unwrap();
        let bridged = bridge["next_offset"].as_u64().unwrap();
        assert!(bridged > through);
        manager
            .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), bridged)
            .unwrap();
    }

    #[test]
    fn model_ack_preserves_independent_native_task_result_and_replay() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let writers = claim_shared_shell_writers(&manager, &job);
        job.mark_launched().unwrap();
        assert_eq!(writers.stdout.append(b"abcdefgh").unwrap(), 8);
        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        let stdout = job.shared_stream_summary(JobStream::Stdout, false);
        let stderr = job.shared_stream_summary(JobStream::Stderr, false);
        manager.finish(
            &job.id,
            JobCompletion::completed(Some(0), true, stdout, stderr),
        );
        {
            let mut state = manager.inner.state.lock().unwrap();
            state.jobs.get_mut(&job.id).unwrap().result_limit = 4;
        }
        manager.expose_native_task(&job.id, None).unwrap().unwrap();
        let owner = manager.inner.shared_jobs.job_output_owner(&job.id).unwrap();
        let task_owner = shared_task_output_owner(&manager.inner.shared_jobs, &job.id).unwrap();
        assert_ne!(
            owner, task_owner,
            "MCP Task and Job leases must be independent"
        );
        assert!(
            manager.inner.shared_jobs.plans(&job.id).unwrap()[0].task_result_eligible(),
            "native Task retention must be declared in the sealed shared plan"
        );
        assert_eq!(
            task_owner,
            manager
                .inner
                .shared_jobs
                .task_result_owner(&job.id, 1)
                .unwrap(),
            "dotlink must use the canonical AW Task-result owner"
        );

        // The model has not received any output, even though the Task
        // already owns the retained source. Task ownership is not ACK proof.
        assert!(
            manager
                .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), 8,)
                .is_err()
        );
        let page = manager
            .output_node_page(
                &job.id,
                OutputPageRequest {
                    node: None,
                    stream: Some(JobOutputStream::Stdout),
                    offset: 0,
                    requested_bytes: raw(8),
                    response_ceiling: serialized(4096),
                    base64: false,
                },
            )
            .unwrap();
        let page: serde_json::Value = serde_json::from_str(&page).unwrap();
        assert_eq!(page["data"], "abcdefgh");
        assert_eq!(page["next_offset"], 8);
        manager
            .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), 8)
            .unwrap();
        let (still_retained, finished) = manager
            .inner
            .shared_jobs
            .output_summary(&job.id, 1, SharedOutputStream::Stdout)
            .unwrap();
        assert!(finished);
        assert_eq!(still_retained.retained_bytes, 4);
        let task_state = manager
            .inner
            .shared_jobs
            .output_owner_state(&job.id, 1, SharedOutputStream::Stdout, task_owner)
            .unwrap();
        assert!(
            !task_state.released,
            "Job ACK must not release native Task bytes"
        );

        // Replaying an ACK cannot widen the Task lease or corrupt its prefix.
        manager
            .acknowledge_node_output(&job.id, None, Some(JobOutputStream::Stdout), 8)
            .unwrap();
        assert_eq!(
            manager
                .inner
                .shared_jobs
                .output_summary(&job.id, 1, SharedOutputStream::Stdout,)
                .unwrap()
                .0
                .retained_bytes,
            4
        );
        let detail = manager.detailed_task(&job.id).unwrap();
        let TaskPayload::Completed { result } = detail.payload else {
            panic!("native Task result disappeared after model ACK");
        };
        let rendered = serde_json::Value::Object(result).to_string();
        assert!(rendered.contains("abcd"), "{rendered}");
        assert!(!rendered.contains("efgh"), "{rendered}");
    }

    #[test]
    fn terminal_finish_acks_only_job_owner_and_preserves_native_task_output() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let writers = claim_shared_shell_writers(&manager, &job);
        job.mark_launched().unwrap();
        assert_eq!(writers.stdout.append(b"abcdefgh").unwrap(), 8);
        assert_eq!(writers.stderr.append(b"err").unwrap(), 3);
        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        let stdout = job.shared_stream_summary(JobStream::Stdout, false);
        let stderr = job.shared_stream_summary(JobStream::Stderr, false);
        manager.finish(
            &job.id,
            JobCompletion::completed(Some(0), true, stdout, stderr),
        );
        manager.expose_native_task(&job.id, None).unwrap().unwrap();

        manager.finish_node_output(&job.id, None, None).unwrap();
        let job_owner = manager.inner.shared_jobs.job_output_owner(&job.id).unwrap();
        let task_owner = shared_task_output_owner(&manager.inner.shared_jobs, &job.id).unwrap();
        for (stream, through) in [
            (SharedOutputStream::Stdout, 8),
            (SharedOutputStream::Stderr, 3),
        ] {
            let job_state = manager
                .inner
                .shared_jobs
                .output_owner_state(&job.id, 1, stream, job_owner)
                .unwrap();
            assert!(job_state.acknowledged);
            assert_eq!(job_state.through, through);
            assert!(!job_state.released);
            let task_state = manager
                .inner
                .shared_jobs
                .output_owner_state(&job.id, 1, stream, task_owner)
                .unwrap();
            assert!(!task_state.acknowledged);
            assert_eq!(task_state.through, 0);
            assert!(!task_state.released);
            assert_eq!(
                manager
                    .inner
                    .shared_jobs
                    .output_summary(&job.id, 1, stream)
                    .unwrap()
                    .0
                    .retained_bytes,
                through,
                "Finish must not discard an independent Task owner's retained bytes"
            );
        }
        manager.finish_node_output(&job.id, None, None).unwrap();
    }

    #[test]
    fn shared_native_task_keeps_pending_owner_then_compacts_exact_task_prefix() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let writers = claim_shared_shell_writers(&manager, &job);
        job.mark_launched().unwrap();
        assert_eq!(writers.stdout.append(b"abcdefgh").unwrap(), 8);
        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        let stdout = job.shared_stream_summary(JobStream::Stdout, false);
        let stderr = job.shared_stream_summary(JobStream::Stderr, false);
        assert_eq!(stdout.stored_bytes, 8);
        manager.finish(
            &job.id,
            JobCompletion::completed(Some(0), true, stdout, stderr),
        );
        {
            let state = manager.inner.state.lock().unwrap();
            let entry = &state.jobs[&job.id];
            assert_eq!(entry.disposition, JobDisposition::Pending);
            let (summary, _) = manager
                .inner
                .shared_jobs
                .output_summary(&job.id, 1, SharedOutputStream::Stdout)
                .unwrap();
            assert_eq!(
                summary.retained_bytes, 8,
                "Pending must preserve Task owner"
            );
        }
        {
            let mut state = manager.inner.state.lock().unwrap();
            state.jobs.get_mut(&job.id).unwrap().result_limit = 4;
        }
        manager.expose_native_task(&job.id, None).unwrap().unwrap();
        let first: serde_json::Value = serde_json::from_str(
            &manager
                .acknowledge_output(&job.id, JobStream::Stdout, 8)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first["stdout"]["stored_bytes"], 4);
        assert_eq!(first["stdout"]["acknowledged_bytes"], 8);
        assert_eq!(first["stdout"]["released"], true);
        let (shared, _) = manager
            .inner
            .shared_jobs
            .output_summary(&job.id, 1, SharedOutputStream::Stdout)
            .unwrap();
        assert_eq!(shared.retained_bytes, 4);
        assert_eq!(manager.inner.shared_jobs.spool_usage().unwrap().0, 4);
        assert_eq!(fs::metadata(&job.stdout_path).unwrap().len(), 4);

        // Lost Job-owner ACK response remains replay-safe after Task compaction.
        let replay: serde_json::Value = serde_json::from_str(
            &manager
                .acknowledge_output(&job.id, JobStream::Stdout, 8)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(replay["stdout"]["stored_bytes"], 4);
        assert_eq!(replay["stdout"]["acknowledged_bytes"], 8);

        let detailed = manager.detailed_task(&job.id).unwrap();
        let TaskPayload::Completed { result } = detailed.payload else {
            panic!("shared compacted native task must remain complete")
        };
        let rendered = serde_json::Value::Object(result).to_string();
        assert!(rendered.contains("abcd"), "{rendered}");
        assert!(!rendered.contains("efgh"), "{rendered}");
    }

    #[test]
    fn shared_shell_claim_rolls_back_if_host_entry_became_terminal_before_binding() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        manager.finish(&job.id, JobCompletion::failed("pre-launch host failure"));
        assert!(manager.bind_policy([9; 32]));
        assert!(
            job.claim_prepared_shell(
                SharedNodeTool::Bash,
                serde_json::json!({
                    "command":"printf ok",
                    "dir":".",
                    "stdin":null,
                    "timeout_secs":30,
                    "max_output_bytes":1024
                }),
                None,
                1024,
                1024,
            )
            .is_err()
        );
        assert!(matches!(
            manager.inner.shared_jobs.runtime_snapshot(&job.id),
            Err(aw_tools::filesystem::job_manager::Error::NotFound)
        ));
    }

    #[test]
    fn shared_owned_cancel_fails_closed_if_canonical_job_disappears() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        claim_shared_shell(&manager, &job);
        job.settle_unlaunched();
        manager.inner.shared_jobs.expire_terminal(&job.id).unwrap();
        let error = manager.cancel(&job.id).unwrap_err().to_string();
        assert!(error.contains("shared-owned Job"), "{error}");
        assert!(
            manager.inner.state.lock().unwrap().jobs[&job.id]
                .shared_job
                .is_some()
        );
    }

    #[test]
    fn shared_owned_completion_becomes_unknown_if_canonical_job_disappears() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        claim_shared_shell(&manager, &job);
        job.settle_unlaunched();
        manager.inner.shared_jobs.expire_terminal(&job.id).unwrap();
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary::default(),
                StreamSummary::default(),
            ),
        );
        let state = manager.inner.state.lock().unwrap();
        let completion = state.jobs[&job.id].completion.as_ref().unwrap();
        assert_eq!(completion.status, JobStatus::Unknown);
        assert_eq!(completion.observed_status, Some(JobStatus::Completed));
        assert!(!completion.success);
    }

    #[test]
    fn shared_owned_cleanup_retains_spools_if_canonical_job_disappears() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        claim_shared_shell(&manager, &job);
        job.settle_unlaunched();
        manager.inner.shared_jobs.expire_terminal(&job.id).unwrap();
        let mut state = manager.inner.state.lock().unwrap();
        let entry = state.jobs.get_mut(&job.id).unwrap();
        assert!(!cleanup_entry(entry));
        assert!(entry.shared_job.is_some());
        assert!(entry.stdout_path.exists());
        assert!(entry.stderr_path.exists());
    }

    #[test]
    fn shell_start_requires_canonical_shared_claim() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let error = job.mark_launched().unwrap_err().to_string();
        assert!(
            error.contains("shared shell Job is missing before process launch"),
            "{error}"
        );
        assert!(matches!(
            manager.inner.shared_jobs.runtime_snapshot(&job.id),
            Err(aw_tools::filesystem::job_manager::Error::NotFound)
        ));
    }

    #[test]
    fn proven_spawn_refusal_after_start_is_canonical_fail() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let writers = claim_shared_shell_writers(&manager, &job);
        job.mark_launched().unwrap();
        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        manager.finish(&job.id, JobCompletion::spawn_failed("spawn refused"));
        let snapshot = manager.inner.shared_jobs.runtime_snapshot(&job.id).unwrap();
        assert_eq!(
            snapshot.lifecycle,
            aw_tools::filesystem::job::Lifecycle::Terminal
        );
        assert_eq!(snapshot.summary, Some((SharedPrimary::Fail, false, false)));
        let state = manager.inner.state.lock().unwrap();
        assert_eq!(
            state.jobs[&job.id].completion.as_ref().unwrap().status,
            JobStatus::Failed
        );
    }

    #[test]
    fn shared_shell_claim_settles_proven_unlaunched_without_effect() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        claim_shared_shell(&manager, &job);
        assert!(matches!(
            manager.inner.shared_jobs.node_state(&job.id, 1).unwrap(),
            SharedNodeState::Claimed { .. }
        ));

        job.settle_unlaunched();
        manager.finish(&job.id, JobCompletion::failed("spawn refused"));
        let snapshot = manager.inner.shared_jobs.runtime_snapshot(&job.id).unwrap();
        assert_eq!(
            snapshot.lifecycle,
            aw_tools::filesystem::job::Lifecycle::Terminal
        );
        assert_eq!(snapshot.summary, Some((SharedPrimary::Skip, false, false)));
    }

    #[test]
    fn claimed_shared_shell_completion_is_never_projected_as_success() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        claim_shared_shell(&manager, &job);
        assert!(matches!(
            manager.inner.shared_jobs.node_state(&job.id, 1).unwrap(),
            SharedNodeState::Claimed { .. }
        ));
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary::default(),
                StreamSummary::default(),
            ),
        );
        let state = manager.inner.state.lock().unwrap();
        let entry = state.jobs.get(&job.id).unwrap();
        let completion = entry.completion.as_ref().unwrap();
        assert_eq!(completion.status, JobStatus::Unknown);
        assert_eq!(completion.observed_status, Some(JobStatus::Completed));
        assert!(completion.internal_error.is_none());
        assert!(
            completion
                .error
                .as_deref()
                .is_some_and(|value| value.contains("uncertain"))
        );
        assert!(!completion.success);
        drop(state);
        assert!(matches!(
            manager.inner.shared_jobs.node_state(&job.id, 1).unwrap(),
            SharedNodeState::Terminal {
                status: SharedStatus {
                    primary: SharedPrimary::Unknown,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn shared_internal_failure_with_uncertain_effect_projects_unknown_and_keeps_evidence() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let writers = claim_shared_shell_writers(&manager, &job);
        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        manager.finish(
            &job.id,
            JobCompletion::internal_failed(
                "post-spawn setup failed after process launch",
                StreamSummary::default(),
                StreamSummary::default(),
            ),
        );
        let state = manager.inner.state.lock().unwrap();
        let entry = state.jobs.get(&job.id).unwrap();
        let completion = entry.completion.as_ref().unwrap();
        assert_eq!(completion.status, JobStatus::Unknown);
        assert_eq!(completion.observed_status, Some(JobStatus::Failed));
        assert!(
            completion
                .internal_error
                .as_deref()
                .is_some_and(|value| value.contains("post-spawn setup failed"))
        );
        assert!(
            completion
                .error
                .as_deref()
                .is_some_and(|value| value.contains("uncertain") && value.contains("inspect"))
        );
        drop(state);
        assert!(matches!(
            manager.inner.shared_jobs.node_state(&job.id, 1).unwrap(),
            SharedNodeState::Terminal {
                status: SharedStatus {
                    primary: SharedPrimary::Unknown,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn terminal_unknown_synchronous_result_remains_inspectable() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let writers = claim_shared_shell_writers(&manager, &job);
        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        manager.finish(
            &job.id,
            JobCompletion::internal_failed(
                "post-launch host failure",
                StreamSummary::default(),
                StreamSummary::default(),
            ),
        );
        assert_eq!(
            manager.mark_synchronous_result(&job.id).unwrap(),
            JobDisposition::Synchronous
        );
        let state = manager.inner.state.lock().unwrap();
        let entry = state.jobs.get(&job.id).unwrap();
        assert_eq!(entry.retention, RetentionClass::Ordinary);
        assert_eq!(entry.disposition, JobDisposition::Synchronous);
        assert_eq!(entry.status(), JobStatus::Unknown);
        drop(state);
        assert!(matches!(
            manager.replay_handoff(&job.id).unwrap(),
            Some(JobHandoff::Synchronous)
        ));
        let listed: serde_json::Value = serde_json::from_str(&manager.list(0, 64)).unwrap();
        assert!(
            listed["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["job_id"] == job.id)
        );
    }

    #[tokio::test]
    async fn delivered_unknown_synchronous_request_keeps_inspection_job() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let runner = manager.clone();
        let started = manager
            .spawn(
                30,
                crate::limits::DEFAULT_TRANSFER_BYTES,
                Some("unknown-sync-delivery".to_owned()),
                move |job| {
                    let runner = runner.clone();
                    async move {
                        let writers = claim_shared_shell_writers(&runner, &job);
                        writers.stdout.finish().unwrap();
                        writers.stderr.finish().unwrap();
                        JobCompletion::internal_failed(
                            "post-launch host failure",
                            StreamSummary::default(),
                            StreamSummary::default(),
                        )
                    }
                },
            )
            .unwrap();
        manager.wait_until(&started.id, None).await.unwrap();
        assert_eq!(
            manager.mark_synchronous_result(&started.id).unwrap(),
            JobDisposition::Synchronous
        );
        assert_eq!(
            manager.acknowledge_request_delivery("unknown-sync-delivery"),
            RequestDeliveryAck::Acknowledged
        );
        assert!(manager.request_job_id("unknown-sync-delivery").is_none());
        let status: serde_json::Value =
            serde_json::from_str(&manager.status(&started.id).unwrap()).unwrap();
        assert_eq!(status["status"], "unknown");
        assert!(manager.list(0, 64).contains(&started.id));
    }

    #[test]
    fn acknowledged_unknown_remains_inspectable_until_ordinary_ttl() {
        let manager = JobManager::new(Duration::from_secs(1)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let writers = claim_shared_shell_writers(&manager, &job);
        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        manager.finish(
            &job.id,
            JobCompletion::internal_failed(
                "post-launch host failure",
                StreamSummary::default(),
                StreamSummary::default(),
            ),
        );
        assert_eq!(
            manager.mark_synchronous_result(&job.id).unwrap(),
            JobDisposition::Synchronous
        );
        manager
            .acknowledge_output(&job.id, JobStream::Stdout, 0)
            .unwrap();
        manager
            .acknowledge_output(&job.id, JobStream::Stderr, 0)
            .unwrap();
        let status: serde_json::Value =
            serde_json::from_str(&manager.status(&job.id).unwrap()).unwrap();
        assert_eq!(status["status"], "unknown");
        assert!(manager.list(0, 64).contains(&job.id));

        {
            let mut state = manager.inner.state.lock().unwrap();
            let entry = state.jobs.get_mut(&job.id).unwrap();
            let old = Instant::now() - Duration::from_secs(2);
            entry.terminal_at = Some(old);
            entry.retention_anchor = Some(old);
        }
        assert!(matches!(manager.status(&job.id), Err(JobError::Unknown(_))));
        assert!(!manager.list(0, 64).contains(&job.id));
    }

    #[test]
    fn optimistic_shell_reserves_shared_exposure_without_publishing_until_handoff() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        claim_shared_shell(&manager, &job);
        job.mark_launched().unwrap();
        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary::default(),
                StreamSummary::default(),
            ),
        );
        assert!(matches!(
            manager.inner.shared_jobs.get(&job.id),
            Err(aw_tools::filesystem::job_manager::Error::NotExposed)
        ));
        manager.reserve_shared_exposure(&job.id, true).unwrap();
        assert!(matches!(
            manager.inner.shared_jobs.get(&job.id),
            Err(aw_tools::filesystem::job_manager::Error::NotExposed)
        ));
        assert!(
            manager
                .inner
                .shared_jobs
                .list(0, 16)
                .unwrap()
                .jobs
                .is_empty()
        );

        manager.expose_compatibility(&job.id).unwrap();
        assert_eq!(manager.inner.shared_jobs.get(&job.id).unwrap().id, job.id);
        assert_eq!(
            manager.inner.state.lock().unwrap().jobs[&job.id].disposition,
            JobDisposition::Compatibility
        );
    }

    #[test]
    fn shared_shell_launch_and_terminal_exit_are_canonical_job_state() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        claim_shared_shell(&manager, &job);
        job.mark_launched().unwrap();
        assert!(matches!(
            manager.inner.shared_jobs.node_state(&job.id, 1).unwrap(),
            SharedNodeState::Running { .. }
        ));

        manager.finish(
            &job.id,
            JobCompletion::completed(
                Some(0),
                true,
                StreamSummary::default(),
                StreamSummary::default(),
            ),
        );
        let snapshot = manager.inner.shared_jobs.runtime_snapshot(&job.id).unwrap();
        assert_eq!(
            snapshot.lifecycle,
            aw_tools::filesystem::job::Lifecycle::Terminal
        );
        assert_eq!(snapshot.summary, Some((SharedPrimary::Ok, false, false)));

        manager.expose_compatibility(&job.id).unwrap();
        assert_eq!(manager.inner.shared_jobs.get(&job.id).unwrap().id, job.id);
    }

    #[test]
    fn job_cancel_checkpoints_shared_cancellation_before_signalling_runner() {
        let manager = JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        claim_shared_shell(&manager, &job);
        job.mark_launched().unwrap();
        assert!(!job.cancel.is_cancelled());

        manager.cancel(&job.id).unwrap();
        let snapshot = manager.inner.shared_jobs.runtime_snapshot(&job.id).unwrap();
        assert_eq!(
            snapshot.lifecycle,
            aw_tools::filesystem::job::Lifecycle::Cancelling
        );
        assert_eq!(
            snapshot.stop_reason,
            Some(aw_tools::filesystem::job::StopReason::Cancelled)
        );
        assert!(job.cancel.is_cancelled());

        manager.finish(
            &job.id,
            JobCompletion::cancelled(StreamSummary::default(), StreamSummary::default()),
        );
        let terminal = manager.inner.shared_jobs.runtime_snapshot(&job.id).unwrap();
        assert_eq!(
            terminal.lifecycle,
            aw_tools::filesystem::job::Lifecycle::Terminal
        );
        assert_eq!(
            terminal.stop_reason,
            Some(aw_tools::filesystem::job::StopReason::Cancelled)
        );
    }

    #[tokio::test]
    async fn standalone_cursor_is_runtime_generation_bound_and_job_ids_are_not_reused() {
        let first = JobManager::new(Duration::from_secs(60)).unwrap();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let job = first.create_for_test(30).unwrap();
            first.finish(
                &job.id,
                JobCompletion::completed(
                    Some(0),
                    true,
                    StreamSummary::default(),
                    StreamSummary::default(),
                ),
            );
            first.expose_compatibility(&job.id).unwrap();
            ids.push(job.id.clone());
        }
        let page = first.standalone_list_fitted(None, 1, |_| true).unwrap();
        let cursor = page
            .lines()
            .find_map(|line| line.strip_prefix("more cursor="))
            .expect("strictly older cursor")
            .to_owned();

        let second = JobManager::new(Duration::from_secs(60)).unwrap();
        let fresh = second.create_for_test(30).unwrap();
        assert!(
            !ids.contains(&fresh.id),
            "new live runtime generation must never reuse a prior Job handle"
        );
        assert!(
            second
                .standalone_list_fitted(Some(&cursor), 1, |_| true)
                .is_err(),
            "cursor from another runtime generation must fail closed"
        );
        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();
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
