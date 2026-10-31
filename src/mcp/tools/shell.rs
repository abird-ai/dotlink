//! `bash` / `powershell`: the sandboxed shell, its mounts and its heartbeat.

use std::{
    env,
    io::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, CreateTaskResult,
        ProgressNotificationParam, ProgressToken, Tool,
    },
    schemars,
    service::{Peer, RequestContext, RoleServer},
    tool, tool_router,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::{Instant, sleep_until, timeout_at},
};
use tokio_util::task::AbortOnDropHandle;

use super::{LocalMachine, dot, resolve_capped, tool_error};
#[cfg(target_os = "linux")]
use crate::mcp::{
    AccessPolicy, ShellGuards, guard_identities, validate_host_mount_policy, validate_shell_guards,
};
use crate::{
    jobs::{
        JobCompletion, JobDisposition, JobHandle, JobHandoff, JobResultContext, JobStatus, JobWait,
        SpoolWriterGuard, StreamSummary,
    },
    mcp::RuntimeConfig,
};
#[cfg(target_os = "linux")]
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read as _, Seek as _, SeekFrom},
    os::fd::{AsRawFd as _, FromRawFd as _},
    path::Component,
};
#[cfg(unix)]
use std::{ffi::CString, os::unix::ffi::OsStrExt as _};

/// The client picks the command execution timeout; the profile ceiling bounds
/// it and is the fallback when omitted. Transport response windows never
/// rewrite this execution lifetime.
fn resolve_shell_timeout(requested: Option<u64>, ceiling: u64) -> u64 {
    resolve_capped(requested, ceiling)
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShellArgs {
    /// Command string to execute.
    pub(crate) command: String,

    /// Working directory. Relative paths resolve from the directory where dotlink was launched.
    #[serde(default = "dot")]
    pub(crate) dir: String,

    /// Optional stdin text passed to the process.
    #[serde(default)]
    pub(crate) stdin: Option<String>,

    /// Command execution timeout in seconds. Defaults to the profile max_shell_timeout_secs ceiling and is capped there. Transport response windows only control how long the caller waits before the same durable execution is handed off.
    #[serde(default)]
    pub(crate) timeout_secs: Option<u64>,

    /// Output byte limit per stream. Defaults to the profile ceiling (max_output_bytes); clamped to it.
    #[serde(default)]
    pub(crate) max_output_bytes: Option<usize>,
}

/// Shell progress is a bare heartbeat: the caller cannot know how much of an
/// arbitrary command is done, so no percentages or messages are reported.
#[cfg(not(test))]
const SHELL_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
const SHELL_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);

/// Progress is advisory; a stalled transport must never stall command timeout
/// enforcement or child reaping. No detached heartbeat tasks or queues.
const PROGRESS_SEND_BUDGET: Duration = Duration::from_millis(100);

/// Salvage window for partial output after a killed child.
const TIMEOUT_OUTPUT_GRACE: Duration = Duration::from_millis(250);

/// Cancellation drops a pending notification; no progress worker outlives its
/// command and no transport backpressure can cross the command's deadline.
async fn deliver_heartbeat_bounded<F: std::future::Future<Output = ()>>(
    command_deadline: Instant,
    delivery: F,
) {
    let send_deadline = command_deadline.min(Instant::now() + PROGRESS_SEND_BUDGET);
    tokio::select! {
        biased;
        () = sleep_until(send_deadline) => {}
        () = delivery => {}
    }
}

/// Heartbeat sink for one `tools/call` that carried a progress token.
struct ProgressHeartbeat {
    peer: Peer<RoleServer>,
    token: ProgressToken,
    ticks: AtomicU64,
}

impl ProgressHeartbeat {
    fn for_request(context: &RequestContext<RoleServer>) -> Option<Self> {
        let token = context.meta.get_progress_token()?;
        Some(Self {
            peer: context.peer.clone(),
            token,
            ticks: AtomicU64::new(0),
        })
    }

    /// Monotonic progress counter with no total and no message.
    async fn report(&self, command_deadline: Instant) {
        let progress = self.ticks.fetch_add(1, Ordering::Relaxed) + 1;
        let param = ProgressNotificationParam::new(self.token.clone(), progress as f64);
        deliver_heartbeat_bounded(command_deadline, async {
            let _ = self.peer.notify_progress(param).await;
        })
        .await;
    }
}

async fn heartbeat_tick(heartbeat: Option<&ProgressHeartbeat>) {
    match heartbeat {
        Some(_) => tokio::time::sleep(SHELL_HEARTBEAT_INTERVAL).await,
        None => std::future::pending().await,
    }
}

fn shell_timeout_description(max_secs: u64) -> String {
    format!(
        "Execution timeout in seconds. Defaults to {max_secs} and is capped there. Transport response windows do not shorten execution: a command that is still running when the caller's synchronous window closes continues as the same durable job/task."
    )
}

fn shell_output_description(max_bytes: usize) -> String {
    format!(
        "Ordinary shell-result byte limit per stream. Defaults to {max_bytes} and is capped there. Every shell execution is durably spooled; if the response window closes first or the complete ordinary result cannot fit the transport response budget, dotlink returns the same job/task handle and its retained output is paged with job_output."
    )
}

/// Tool schemas are static, the policy ceilings are not. Replace the limit
/// arguments' descriptions with the numbers this profile actually enforces.
pub(crate) fn with_profile_limits(mut tool: Tool, max_secs: u64, max_bytes: usize) -> Tool {
    if !crate::mcp::is_shell_tool(tool.name.as_ref()) {
        return tool;
    }
    let mut schema = (*tool.input_schema).clone();
    let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return tool;
    };
    for (key, description) in [
        ("timeout_secs", shell_timeout_description(max_secs)),
        ("max_output_bytes", shell_output_description(max_bytes)),
    ] {
        if let Some(Value::Object(property)) = properties.get_mut(key) {
            property.insert("description".to_owned(), Value::String(description));
            property.insert(
                "maximum".to_owned(),
                Value::from(if key == "timeout_secs" {
                    max_secs
                } else {
                    max_bytes as u64
                }),
            );
        }
    }
    tool.input_schema = Arc::new(schema);
    tool
}

/// Drain a long-running job stream directly into its private spool file. The
/// stored prefix is bounded independently of response paging, while the pipe is
/// always fully drained so a noisy child can never deadlock on a full pipe.
type SpoolTask = AbortOnDropHandle<std::io::Result<StreamSummary>>;

async fn abort_and_quiesce_spool_tasks(job: &JobHandle, stdout: &SpoolTask, stderr: &SpoolTask) {
    // Seal registration before requesting cancellation. This makes the transition
    // one-way even if a future drain implementation grows detached helper tasks.
    job.seal_spool_writers();
    stdout.abort();
    stderr.abort();
    // A collection timeout may already have consumed one JoinHandle result while
    // waiting for the other, so task handles are not a reusable completion barrier.
    // Writer lifetime is registered before spawn and remains authoritative even
    // after a JoinHandle result was consumed or its owner was dropped.
    job.wait_spool_writers_idle().await;
}

async fn spool_job_output<R>(
    mut reader: R,
    path: PathBuf,
    writer_guard: SpoolWriterGuard,
) -> std::io::Result<StreamSummary>
where
    R: AsyncRead + Unpin,
{
    let cap = crate::jobs::spool_limit();
    // Tokio's async file writer may detach a blocking write when its owning task
    // is aborted. Job terminal metadata must never race bytes that can still land
    // afterward, so spool-file mutations use the shared cancellation-safe
    // synchronous OS-operation boundary while child-pipe reads stay async.
    let mut file = crate::blocking::run(|| {
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
    })?;
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0_u64;
    let mut stored = 0_u64;
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if truncated {
            // Once any byte is omitted, the durable file must remain an exact
            // prefix forever. Aggregate quota can become available later, but
            // appending a later suffix would corrupt byte offsets and Task replay.
            continue;
        }
        let remaining = cap.saturating_sub(stored);
        let wanted = remaining.min(read as u64);
        let reserved = writer_guard.reserve(wanted);
        let keep = usize::try_from(reserved).unwrap_or(read);
        if keep > 0 {
            let (written, error) = crate::blocking::run(|| {
                let mut written = 0_usize;
                while written < keep {
                    match file.write(&buffer[written..keep]) {
                        Ok(0) => {
                            return (
                                written,
                                Some(std::io::Error::new(
                                    std::io::ErrorKind::WriteZero,
                                    "job spool write made no progress",
                                )),
                            );
                        }
                        Ok(count) => written += count,
                        Err(error) => return (written, Some(error)),
                    }
                }
                (written, None)
            });
            if let Some(error) = error {
                // Successful prefix writes remain charged; release only the
                // reservation that never reached the spool file.
                writer_guard.release(reserved.saturating_sub(written as u64));
                return Err(error);
            }
            stored = stored.saturating_add(written as u64);
        }
        if reserved < read as u64 {
            truncated = true;
        }
    }
    let summary = StreamSummary {
        total_bytes: total,
        stored_bytes: stored,
        truncated,
    };
    drop(writer_guard);
    Ok(summary)
}

fn stream_summary_from_file(path: &Path, truncated: bool) -> StreamSummary {
    let stored = std::fs::metadata(path).map_or(0, |metadata| metadata.len());
    StreamSummary {
        total_bytes: stored,
        stored_bytes: stored,
        truncated,
    }
}

#[cfg(unix)]
fn effective_access(path: &Path, mode: libc::c_int) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is NUL-terminated and points to a live CString. AT_EACCESS
    // asks the kernel for this process's effective-credential access decision,
    // including ACLs and mount policy.
    unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), mode, libc::AT_EACCESS) == 0 }
}

#[cfg(unix)]
fn effectively_executable(path: &Path) -> bool {
    effective_access(path, libc::X_OK)
}

#[cfg(target_os = "linux")]
fn descriptor_filesystem_read_only(file: &File) -> Result<bool> {
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `stats` points to valid writable storage and `file` owns a live FD.
    if unsafe { libc::fstatvfs(file.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot inspect launcher filesystem");
    }
    // SAFETY: successful `fstatvfs` initialized the complete structure.
    let stats = unsafe { stats.assume_init() };
    Ok((stats.f_flag & libc::ST_RDONLY as libc::c_ulong) != 0)
}

#[cfg(target_os = "linux")]
fn descriptor_filesystem_type(file: &File) -> Result<u64> {
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `stats` points to valid writable storage and `file` owns a live FD.
    if unsafe { libc::fstatfs(file.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot inspect launcher filesystem type");
    }
    // SAFETY: successful `fstatfs` initialized the complete structure.
    Ok(unsafe { stats.assume_init() }.f_type as u64)
}

#[cfg(target_os = "linux")]
fn descriptor_mount_id(file: &File) -> Result<u64> {
    // `/proc/self/fdinfo` exposes the kernel mount id for this exact live
    // descriptor. Using the proc ABI directly avoids libc/statx target-surface
    // differences (notably musl) while keeping the check descriptor-bound and
    // fail-closed when proc metadata is unavailable.
    let path = format!("/proc/self/fdinfo/{}", file.as_raw_fd());
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read launcher descriptor metadata {path}"))?;
    text.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .map(str::trim)
        .ok_or_else(|| anyhow::anyhow!("launcher descriptor metadata has no mount id"))?
        .parse::<u64>()
        .context("launcher descriptor mount id is invalid")
}

#[cfg(target_os = "linux")]
fn nix_store_item_root(path: &Path) -> Option<PathBuf> {
    const NIX_BASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    let relative = path.strip_prefix("/nix/store").ok()?;
    let Component::Normal(item) = relative.components().next()? else {
        return None;
    };
    let bytes = item.as_bytes();
    if bytes.len() <= 33
        || bytes[32] != b'-'
        || !bytes[..32].iter().all(|byte| NIX_BASE32.contains(byte))
    {
        return None;
    }
    Some(Path::new("/nix/store").join(item))
}

#[cfg(target_os = "linux")]
fn root_owned_launcher_path(path: &Path, source: &std::fs::Metadata) -> Result<bool> {
    use std::os::unix::fs::MetadataExt as _;

    if source.uid() != 0 || source.mode() & 0o022 != 0 {
        return Ok(false);
    }
    let mut ancestor = path.parent();
    while let Some(path) = ancestor {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("cannot inspect launcher ancestor {}", path.display()))?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Ok(false);
        }
        ancestor = path.parent();
    }
    Ok(true)
}

#[cfg(target_os = "linux")]
fn traditional_nix_store_trusted(
    store_uid: libc::uid_t,
    store_mode: libc::mode_t,
    all_item_objects_root_owned: bool,
) -> bool {
    store_uid == 0
        && all_item_objects_root_owned
        && (store_mode & 0o022 == 0 || store_mode & 0o1000 != 0)
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NixStoreTrust {
    RootOwned,
    ReadOnlyMapped,
}

#[cfg(target_os = "linux")]
fn trusted_nix_store_launcher(
    path: &Path,
    source: &File,
    source_metadata: &std::fs::Metadata,
) -> Result<Option<NixStoreTrust>> {
    use std::os::unix::fs::MetadataExt as _;

    let Some(item_root) = nix_store_item_root(path) else {
        return Ok(None);
    };
    if source_metadata.mode() & 0o022 != 0 {
        return Ok(None);
    }

    let mut all_item_objects_root_owned = source_metadata.uid() == 0;
    let mut ancestor = path.parent();
    let mut reached_item_root = false;
    while let Some(path) = ancestor {
        if !path.starts_with(&item_root) {
            break;
        }
        let metadata = std::fs::symlink_metadata(path).with_context(|| {
            format!(
                "cannot inspect Nix store launcher ancestor {}",
                path.display()
            )
        })?;
        if !metadata.is_dir() || metadata.mode() & 0o022 != 0 {
            return Ok(None);
        }
        all_item_objects_root_owned &= metadata.uid() == 0;
        if path == item_root {
            reached_item_root = true;
            break;
        }
        ancestor = path.parent();
    }
    if !reached_item_root {
        return Ok(None);
    }

    let store = crate::secure_fs::SecureFs::pin_canonical(Path::new("/nix/store"))?;
    let store_metadata = store.metadata()?;
    if !store_metadata.is_dir()
        || descriptor_filesystem_type(&store)? == libc::FUSE_SUPER_MAGIC as u64
        || descriptor_mount_id(&store)? != descriptor_mount_id(source)?
    {
        return Ok(None);
    }

    // Traditional daemon-managed stores may have a writable sticky store root,
    // while the immutable item itself and every descendant are root-owned.
    if traditional_nix_store_trusted(
        store_metadata.uid(),
        store_metadata.mode(),
        all_item_objects_root_owned,
    ) {
        return Ok(Some(NixStoreTrust::RootOwned));
    }

    // UID-remapped/rootless stores have no usable ownership provenance from this
    // namespace. A read-only same-mount view is sufficient only for unprivileged
    // same-UID execution; root rejects this trust mode entirely because the
    // launcher snapshot cannot authenticate its loader/shared-library closure.
    if descriptor_filesystem_read_only(&store)? {
        return Ok(Some(NixStoreTrust::ReadOnlyMapped));
    }

    Ok(None)
}

#[cfg(target_os = "linux")]
fn build_pinned_bubblewrap(path: &Path) -> bool {
    option_env!("DOTLINK_BUILD_BWRAP")
        .and_then(|value| std::fs::canonicalize(value).ok())
        .is_some_and(|trusted| trusted == path)
}

#[cfg(target_os = "linux")]
fn nix_store_selection_authorized(build_pinned: bool, root_managed_selector: bool) -> bool {
    build_pinned || root_managed_selector
}

#[cfg(target_os = "linux")]
fn read_only_mapped_nix_allowed_for_effective_uid(
    euid: libc::uid_t,
    selection_authorized: bool,
) -> bool {
    selection_authorized && euid != 0
}

#[cfg(target_os = "linux")]
fn managed_selector_applies(candidate: &Path) -> bool {
    !candidate.starts_with("/nix/store")
}

#[cfg(target_os = "linux")]
fn root_managed_launcher_selector(path: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt as _;

    if !path.is_absolute() {
        return Ok(false);
    }

    // Selection authority belongs to the final PATH entry, not to any ancestor
    // symlink that happened to participate in path traversal. Requiring the final
    // candidate itself to be root-owned means only the host administrator can
    // choose which immutable Nix object dotlink may execute host-side.
    let selector = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect launcher selector {}", path.display()))?;
    if !selector.file_type().is_symlink() || selector.uid() != 0 {
        return Ok(false);
    }

    // Follow each parent as the kernel would during candidate lookup. An ancestor
    // symlink into a user-controlled tree therefore resolves to a non-root-owned
    // or writable directory and cannot become a shortcut to trusted selection.
    let mut parent = path.parent();
    while let Some(path) = parent {
        let metadata = std::fs::metadata(path).with_context(|| {
            format!("cannot inspect launcher selector parent {}", path.display())
        })?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Ok(false);
        }
        parent = path.parent();
    }
    Ok(true)
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct UntrustedSandboxLauncher {
    path: PathBuf,
}

#[cfg(target_os = "linux")]
impl std::fmt::Display for UntrustedSandboxLauncher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Bubblewrap launcher {} is not from a trusted host-managed source; use a root-owned native executable under a root-owned non-writable path, a root-managed selector such as the system profile, or dotlink's Nix package so the exact Bubblewrap derivation is build-pinned (Nix-store immutability alone is not selection authority)",
            self.path.display()
        )
    }
}

#[cfg(target_os = "linux")]
impl std::error::Error for UntrustedSandboxLauncher {}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SandboxLauncherTrust {
    RootOwnedPath,
    NixStoreRootOwned,
    NixStoreReadOnlyMapped,
}

#[cfg(target_os = "linux")]
fn sandbox_launcher_source_trust(
    path: &Path,
    source: &File,
    source_metadata: &std::fs::Metadata,
    allow_nix_store: bool,
    allow_read_only_mapped_nix: bool,
) -> Result<Option<SandboxLauncherTrust>> {
    if descriptor_filesystem_type(source)? == libc::FUSE_SUPER_MAGIC as u64 {
        return Ok(None);
    }

    // Nix store objects are root-owned even when an unprivileged user asked the
    // daemon to build them. Never let a canonical store path fall through the
    // generic root-owned-file rule: every runtime uid must additionally prove
    // that selection came from the build pin or a root-owned final PATH symlink.
    if path.starts_with("/nix/store") {
        if !allow_nix_store {
            return Ok(None);
        }
        return Ok(
            match trusted_nix_store_launcher(path, source, source_metadata)? {
                Some(NixStoreTrust::RootOwned) => Some(SandboxLauncherTrust::NixStoreRootOwned),
                Some(NixStoreTrust::ReadOnlyMapped) if allow_read_only_mapped_nix => {
                    Some(SandboxLauncherTrust::NixStoreReadOnlyMapped)
                }
                Some(NixStoreTrust::ReadOnlyMapped) => None,
                None => None,
            },
        );
    }

    if root_owned_launcher_path(path, source_metadata)? {
        return Ok(Some(SandboxLauncherTrust::RootOwnedPath));
    }
    Ok(None)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        effectively_executable(path)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

const MAX_PREPARED_EXECUTABLE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct PreparedExecutable {
    path: PathBuf,
    #[cfg(unix)]
    identity: ExecutableIdentity,
    digest: [u8; 32],
    #[cfg(target_os = "linux")]
    snapshot: Arc<File>,
    #[cfg(target_os = "linux")]
    exec_path: PathBuf,
    #[cfg(target_os = "linux")]
    script: bool,
    #[cfg(target_os = "linux")]
    privileged_source: bool,
    #[cfg(target_os = "linux")]
    sandbox_trust: Option<SandboxLauncherTrust>,
}

#[cfg(target_os = "linux")]
fn validate_sandbox_launcher(prepared: &PreparedExecutable) -> Result<()> {
    if prepared.privileged_source {
        anyhow::bail!(
            "Bubblewrap launcher {} uses setuid/setgid bits or Linux file capabilities; sealed launcher snapshots intentionally drop kernel-granted executable privileges, so install an unprivileged modern Bubblewrap (0.12+) in a system-managed path",
            prepared.path.display()
        );
    }
    if prepared.script {
        anyhow::bail!(
            "Bubblewrap launcher {} is a #! wrapper; sandbox launchers must be native executables so the host-side interpreter is part of the sealed trust boundary",
            prepared.path.display()
        );
    }
    if prepared.sandbox_trust.is_none() {
        anyhow::bail!(
            "Bubblewrap launcher {} is not from a trusted host-managed source; use a root-owned native executable under a root-owned non-writable path, a root-managed selector such as the system profile, or dotlink's Nix package so the exact Bubblewrap derivation is build-pinned (Nix-store immutability alone is not selection authority)",
            prepared.path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExecutableIdentity {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) rdev: u64,
}

#[cfg(target_os = "linux")]
fn file_has_security_capability(file: &File) -> Result<bool> {
    let name = CString::new("security.capability").expect("static xattr name");
    // SAFETY: `name` is a live NUL-terminated CString; a null value with size 0
    // asks only for the attribute length and does not expose Rust memory.
    let length =
        unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if length >= 0 {
        return Ok(length > 0);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == libc::ENODATA || code == libc::ENOTSUP => Ok(false),
        _ => Err(error).context("cannot inspect executable file capabilities"),
    }
}

#[cfg(target_os = "linux")]
fn create_executable_memfd() -> Result<File> {
    let name = CString::new("dotlink-executable").expect("static memfd name");
    let create = |flags| {
        // SAFETY: `name` is a live NUL-terminated CString and memfd_create
        // returns either an owned descriptor or -1.
        unsafe { libc::memfd_create(name.as_ptr(), flags) }
    };
    let base = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
    let raw = create(base | libc::MFD_EXEC);
    if raw >= 0 {
        // SAFETY: `raw` is uniquely owned after successful memfd_create.
        return Ok(unsafe { File::from_raw_fd(raw) });
    }
    let first = std::io::Error::last_os_error();
    if first.raw_os_error() == Some(libc::EINVAL) {
        // Linux before MFD_EXEC existed rejects the unknown flag. Those kernels
        // also predate vm.memfd_noexec, so retain the historical executable
        // memfd behavior as a compatibility fallback.
        let raw = create(base);
        if raw >= 0 {
            // SAFETY: `raw` is uniquely owned after successful memfd_create.
            return Ok(unsafe { File::from_raw_fd(raw) });
        }
        return Err(std::io::Error::last_os_error())
            .context("cannot create sealed executable snapshot");
    }
    if matches!(first.raw_os_error(), Some(libc::EACCES | libc::EPERM)) {
        return Err(crate::recovery::operator(anyhow::anyhow!(
            "kernel policy forbids executable memfd snapshots; allow MFD_EXEC (vm.memfd_noexec must not prohibit executable memfds) or disable shell access"
        )));
    }
    Err(first).context("cannot create sealed executable snapshot")
}

#[cfg(target_os = "linux")]
fn sealed_executable_snapshot(
    source: &File,
    display: &Path,
) -> Result<(File, [u8; 32], bool, bool)> {
    use std::os::unix::fs::PermissionsExt as _;

    let source_path = PathBuf::from(format!("/proc/self/fd/{}", source.as_raw_fd()));
    let mut input = File::open(&source_path)
        .with_context(|| format!("cannot read prepared executable {}", display.display()))?;
    let metadata = input.metadata()?;
    let privileged_source =
        metadata.permissions().mode() & 0o6000 != 0 || file_has_security_capability(&input)?;
    if metadata.len() > MAX_PREPARED_EXECUTABLE_BYTES {
        anyhow::bail!(
            "executable {} is {} bytes, above the {} byte preparation limit",
            display.display(),
            metadata.len(),
            MAX_PREPARED_EXECUTABLE_BYTES
        );
    }
    let mut snapshot = create_executable_memfd()?;
    snapshot.set_permissions(std::fs::Permissions::from_mode(0o500))?;

    let mut digest = Sha256::new();
    let mut prefix = [0_u8; 2];
    let mut prefix_len = 0_usize;
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if prefix_len < prefix.len() {
            let take = (prefix.len() - prefix_len).min(read);
            prefix[prefix_len..prefix_len + take].copy_from_slice(&buffer[..take]);
            prefix_len += take;
        }
        copied = copied.saturating_add(read as u64);
        if copied > MAX_PREPARED_EXECUTABLE_BYTES {
            anyhow::bail!(
                "executable {} grew above the {} byte preparation limit",
                display.display(),
                MAX_PREPARED_EXECUTABLE_BYTES
            );
        }
        digest.update(&buffer[..read]);
        snapshot.write_all(&buffer[..read])?;
    }
    snapshot.flush()?;
    snapshot.seek(SeekFrom::Start(0))?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
    // SAFETY: fcntl operates on the live memfd and does not alias Rust memory.
    if unsafe { libc::fcntl(snapshot.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot seal executable snapshot");
    }
    let digest: [u8; 32] = digest.finalize().into();
    Ok((
        snapshot,
        digest,
        prefix_len == 2 && prefix == *b"#!",
        privileged_source,
    ))
}

#[cfg(not(target_os = "linux"))]
fn executable_digest(path: &Path) -> Result<[u8; 32]> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > MAX_PREPARED_EXECUTABLE_BYTES {
        anyhow::bail!(
            "executable {} is {} bytes, above the {} byte preparation limit",
            path.display(),
            metadata.len(),
            MAX_PREPARED_EXECUTABLE_BYTES
        );
    }
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_PREPARED_EXECUTABLE_BYTES {
            anyhow::bail!(
                "executable {} grew above the {} byte preparation limit",
                path.display(),
                MAX_PREPARED_EXECUTABLE_BYTES
            );
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}

impl PreparedExecutable {
    pub(crate) fn prepare(path: PathBuf) -> Result<Self> {
        Self::prepare_inner(path, false, false, false)
    }

    fn prepare_inner(
        path: PathBuf,
        require_sandbox_trust: bool,
        allow_nix_store: bool,
        allow_read_only_mapped_nix: bool,
    ) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        let _ = (
            require_sandbox_trust,
            allow_nix_store,
            allow_read_only_mapped_nix,
        );
        let path = std::fs::canonicalize(&path)
            .with_context(|| format!("cannot resolve executable {}", path.display()))?;
        #[cfg(target_os = "linux")]
        {
            let pinned = crate::secure_fs::SecureFs::pin_canonical(&path)?;
            let source_metadata = pinned.metadata()?;
            if !source_metadata.is_file() {
                anyhow::bail!("executable is not a regular file: {}", path.display());
            }
            let pinned_path = PathBuf::from(format!("/proc/self/fd/{}", pinned.as_raw_fd()));
            if !effectively_executable(&pinned_path) {
                anyhow::bail!(
                    "pinned executable is not executable by this process: {}",
                    path.display()
                );
            }
            let sandbox_trust = if require_sandbox_trust {
                let trust = sandbox_launcher_source_trust(
                    &path,
                    &pinned,
                    &source_metadata,
                    allow_nix_store,
                    allow_read_only_mapped_nix,
                )?;
                if trust.is_none() {
                    return Err(anyhow::Error::new(UntrustedSandboxLauncher {
                        path: path.clone(),
                    }));
                }
                trust
            } else {
                None
            };
            let identity = crate::secure_fs::file_identity(&pinned)?;
            let (snapshot, digest, script, privileged_source) =
                sealed_executable_snapshot(&pinned, &path)?;
            let snapshot = Arc::new(snapshot);
            let exec_path = PathBuf::from(format!("/proc/self/fd/{}", snapshot.as_raw_fd()));
            if !effectively_executable(&exec_path) {
                return Err(crate::recovery::operator(anyhow::anyhow!(
                    "sealed executable snapshot is not executable by this process: {}; kernel executable-memfd policy may prohibit shell launchers",
                    path.display()
                )));
            }
            Ok(Self {
                path,
                identity: ExecutableIdentity {
                    dev: identity.dev,
                    ino: identity.ino,
                    rdev: identity.rdev,
                },
                digest,
                snapshot,
                exec_path,
                script,
                privileged_source,
                sandbox_trust,
            })
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            use std::os::unix::fs::MetadataExt as _;
            if !is_executable_file(&path) {
                anyhow::bail!("path is not executable by this process: {}", path.display());
            }
            let metadata = std::fs::metadata(&path)?;
            let digest = executable_digest(&path)?;
            Ok(Self {
                path,
                identity: ExecutableIdentity {
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                    rdev: metadata.rdev(),
                },
                digest,
            })
        }
        #[cfg(not(unix))]
        {
            if !is_executable_file(&path) {
                anyhow::bail!("path is not executable by this process: {}", path.display());
            }
            let digest = executable_digest(&path)?;
            Ok(Self { path, digest })
        }
    }

    #[cfg(all(target_os = "linux", test))]
    pub(crate) fn prepare_sandbox_launcher(path: PathBuf) -> Result<Self> {
        Self::prepare_sandbox_launcher_selected(path, false)
    }

    #[cfg(target_os = "linux")]
    fn prepare_sandbox_launcher_selected(
        path: PathBuf,
        root_managed_selector: bool,
    ) -> Result<Self> {
        let canonical = std::fs::canonicalize(&path)
            .with_context(|| format!("cannot resolve executable {}", path.display()))?;
        let build_pinned = build_pinned_bubblewrap(&canonical);
        let euid = unsafe { libc::geteuid() };
        // Nix-store immutability proves only that the selected bytes cannot
        // change; it does not prove who selected them. Unprivileged users can
        // ask the daemon to build arbitrary root-owned derivations, so direct
        // store paths are trusted only when this exact Bubblewrap was compiled
        // into Dotlink or a root-managed host selector chose it.
        let selection_authorized =
            nix_store_selection_authorized(build_pinned, root_managed_selector);
        let prepared = Self::prepare_inner(
            canonical,
            true,
            selection_authorized,
            read_only_mapped_nix_allowed_for_effective_uid(euid, selection_authorized),
        )?;
        validate_sandbox_launcher(&prepared)?;
        Ok(prepared)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(unix)]
    pub(crate) fn identity(&self) -> ExecutableIdentity {
        self.identity
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        self.digest
    }

    #[cfg(not(target_os = "linux"))]
    fn revalidate_source(&self) -> Result<()> {
        if !is_executable_file(&self.path) {
            anyhow::bail!(
                "prepared executable is no longer runnable: {}",
                self.path.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = std::fs::metadata(&self.path)?;
            let current = ExecutableIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
                rdev: metadata.rdev(),
            };
            if current != self.identity {
                anyhow::bail!(
                    "prepared executable identity changed before launch: {}",
                    self.path.display()
                );
            }
        }
        if executable_digest(&self.path)? != self.digest {
            anyhow::bail!(
                "prepared executable contents changed before launch: {}",
                self.path.display()
            );
        }
        Ok(())
    }

    pub(crate) fn command(&self) -> Result<Command> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt as _;
            let mut command = Command::new(&self.exec_path);
            command.as_std_mut().arg0(&self.path);
            if self.script {
                let descriptor = self.snapshot.as_raw_fd();
                // A shebang interpreter reopens /proc/self/fd/N after exec. Keep
                // the sealed snapshot descriptor alive in this child only for
                // scripts; native ELF launchers retain CLOEXEC and leak no extra FD.
                unsafe {
                    command.as_std_mut().pre_exec(move || {
                        if libc::fcntl(descriptor, libc::F_SETFD, 0) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            Ok(command)
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.revalidate_source()?;
            Ok(Command::new(&self.path))
        }
    }

    #[cfg(target_os = "linux")]
    fn sandbox_host_command(&self) -> Result<Command> {
        if self.sandbox_trust.is_none() {
            anyhow::bail!(
                "internal error: host-side sandbox launcher execution requires trusted provenance"
            );
        }
        let mut command = self.command()?;
        // Bubblewrap is itself host-side code. Start it with no inherited
        // environment so dynamic-loader controls (LD_PRELOAD, LD_LIBRARY_PATH,
        // LD_AUDIT, GLIBC_TUNABLES, etc.) cannot run attacker-controlled code
        // before Bubblewrap establishes the sandbox. The child environment is
        // built separately through Bubblewrap --clearenv/--setenv arguments.
        command.env_clear();
        Ok(command)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn open_snapshot_reader(&self) -> Result<File> {
        File::open(&self.exec_path).with_context(|| {
            format!(
                "cannot reopen sealed executable snapshot for {}",
                self.path.display()
            )
        })
    }

    #[cfg(all(target_os = "linux", test))]
    pub(crate) fn try_clone_snapshot(&self) -> Result<File> {
        self.snapshot.try_clone().map_err(Into::into)
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        if let Ok(prepared) = Self::prepare(path.clone()) {
            return prepared;
        }
        let fallback = resolve_executable("bash")
            .or_else(|| resolve_executable("sh"))
            .expect("a small shell executable is available for command-shape tests");
        let mut prepared = Self::prepare(fallback).expect("test shell executable prepares");
        prepared.path = path;
        prepared
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn for_test_sandbox_launcher(path: PathBuf) -> Self {
        let mut prepared = Self::for_test(path);
        prepared.sandbox_trust = Some(SandboxLauncherTrust::RootOwnedPath);
        prepared
    }
}

fn executable_candidate_in_path(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn resolve_executable_in_path(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    executable_candidate_in_path(name, path)
        .map(|found| std::fs::canonicalize(&found).unwrap_or(found))
}

#[cfg(target_os = "linux")]
fn prepare_bubblewrap_in_path(path: &std::ffi::OsStr) -> Result<Option<PreparedExecutable>> {
    let mut first_preparation_error = None;
    let mut first_selector_error = None;
    let mut first_rejection = None;
    for dir in env::split_paths(path) {
        let candidate = dir.join("bwrap");
        if !is_executable_file(&candidate) {
            continue;
        }
        let selector = if managed_selector_applies(&candidate) {
            match root_managed_launcher_selector(&candidate) {
                Ok(selector) => selector,
                Err(error) => {
                    first_selector_error.get_or_insert(error);
                    continue;
                }
            }
        } else {
            false
        };
        match PreparedExecutable::prepare_sandbox_launcher_selected(candidate, selector) {
            Ok(prepared) => return Ok(Some(prepared)),
            Err(error) => {
                if crate::recovery::Recovery::of(&error) == crate::recovery::Recovery::Operator {
                    return Err(error);
                }
                if error.downcast_ref::<UntrustedSandboxLauncher>().is_some() {
                    first_rejection.get_or_insert(error);
                } else {
                    first_preparation_error.get_or_insert(error);
                }
            }
        }
    }
    match first_preparation_error
        .or(first_selector_error)
        .or(first_rejection)
    {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn prepare_bubblewrap_from_path() -> Result<Option<PreparedExecutable>> {
    let Some(path) = env::var_os("PATH") else {
        return Ok(None);
    };
    prepare_bubblewrap_in_path(&path)
}

pub(crate) fn resolve_executable(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return is_executable_file(candidate)
            .then(|| std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf()));
    }
    env::var_os("PATH").and_then(|path| resolve_executable_in_path(name, &path))
}

#[cfg(target_os = "linux")]
fn sandbox_visible_path(access: &AccessPolicy) -> std::ffi::OsString {
    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();

    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let canonical = std::fs::canonicalize(&dir).unwrap_or(dir);
            let system_visible = [
                Path::new("/nix/store"),
                Path::new("/run/current-system"),
                Path::new("/etc/profiles"),
                Path::new("/nix/var/nix/profiles"),
                Path::new("/usr"),
                Path::new("/bin"),
                Path::new("/sbin"),
            ]
            .iter()
            .any(|root| canonical.starts_with(root));

            if (system_visible || access.can_read(&canonical)) && seen.insert(canonical.clone()) {
                paths.push(canonical);
            }
        }
    }

    for fallback in [
        PathBuf::from("/run/current-system/sw/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ] {
        if fallback.exists() && seen.insert(fallback.clone()) {
            paths.push(fallback);
        }
    }

    env::join_paths(paths)
        .unwrap_or_else(|_| std::ffi::OsString::from("/run/current-system/sw/bin:/usr/bin:/bin"))
}

/// Check the actual mount primitives used by the shell backend at runtime
/// preparation. The probe has both a time bound and a byte bound; a broken
/// executable cannot consume unbounded memory while advertising capabilities.
#[cfg(target_os = "linux")]
const MAX_BUBBLEWRAP_HELP_BYTES: u64 = 64 * 1024;

#[cfg(target_os = "linux")]
async fn read_bubblewrap_help(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_BUBBLEWRAP_HELP_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > MAX_BUBBLEWRAP_HELP_BYTES {
        return Err(crate::recovery::operator(anyhow::anyhow!(
            "Bubblewrap --help exceeded {MAX_BUBBLEWRAP_HELP_BYTES} bytes"
        )));
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
pub(crate) async fn validate_bubblewrap(program: &PreparedExecutable) -> Result<()> {
    const REQUIRED: &[&str] = &[
        "--bind-fd",
        "--ro-bind-fd",
        "--ro-bind-data",
        "--perms",
        "--dev-bind",
    ];
    let mut command = program.sandbox_host_command()?;
    let mut child = command
        .arg("--help")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            crate::recovery::operator(anyhow::anyhow!(
                "cannot execute Bubblewrap {} for capability inspection: {error}",
                program.path().display()
            ))
        })?;
    let stdout = child
        .stdout
        .take()
        .context("Bubblewrap help stdout was not piped")?;
    let help = tokio::time::timeout(Duration::from_secs(5), async {
        let bytes = read_bubblewrap_help(stdout).await?;
        let status = child.wait().await?;
        if !status.success() {
            return Err(crate::recovery::operator(anyhow::anyhow!(
                "Bubblewrap {} could not report its supported options ({status})",
                program.path().display()
            )));
        }
        Ok::<_, anyhow::Error>(bytes)
    })
    .await
    .with_context(|| {
        format!(
            "timed out inspecting Bubblewrap {}",
            program.path().display()
        )
    })??;
    let missing = missing_bubblewrap_options(&String::from_utf8_lossy(&help), REQUIRED);
    if !missing.is_empty() {
        return Err(crate::recovery::operator(anyhow::anyhow!(
            "Bubblewrap {} is missing required options: {}; install a newer Bubblewrap with descriptor-backed mount support",
            program.path().display(),
            missing.join(", ")
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn missing_bubblewrap_options<'a>(help: &str, required: &'a [&str]) -> Vec<&'a str> {
    required
        .iter()
        .copied()
        .filter(|option| {
            !help
                .lines()
                .any(|line| line.split_whitespace().next() == Some(*option))
        })
        .collect()
}

/// Read-only host paths the shell needs without a grant: loader, libraries and
/// Nix profiles; resolver and CA files with network; account lookup files with
/// the SSH agent (`ssh-keygen -Y sign` calls getpwuid, and `/etc` stays
/// private otherwise).
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
enum RuntimeTargetPolicy {
    Any,
    Beneath(PathBuf),
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
struct RuntimeSource {
    path: PathBuf,
    target: RuntimeTargetPolicy,
}

#[cfg(target_os = "linux")]
impl RuntimeSource {
    fn system(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            target: RuntimeTargetPolicy::Any,
        }
    }

    fn constrained(path: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            target: RuntimeTargetPolicy::Beneath(root.into()),
        }
    }

    fn validate_target(&self, canonical: &Path) -> Result<()> {
        match &self.target {
            RuntimeTargetPolicy::Any => Ok(()),
            RuntimeTargetPolicy::Beneath(root) if canonical.starts_with(root) => Ok(()),
            RuntimeTargetPolicy::Beneath(root) => anyhow::bail!(
                "refusing implicit runtime alias {} -> {} because it escapes trusted root {}",
                self.path.display(),
                canonical.display(),
                root.display()
            ),
        }
    }
}

#[cfg(target_os = "linux")]
fn sandbox_runtime_sources(network: bool, accounts: bool) -> Vec<RuntimeSource> {
    let mut paths = [
        "/nix/store",
        "/run/current-system",
        "/etc/profiles",
        "/nix/var/nix/profiles",
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
    ]
    .map(RuntimeSource::system)
    .to_vec();
    if let Some(home) = env::var_os("HOME") {
        paths.push(RuntimeSource::constrained(
            PathBuf::from(home).join(".nix-profile"),
            PathBuf::from("/nix/store"),
        ));
    }
    if network {
        paths.extend(
            [
                "/etc/resolv.conf",
                "/etc/hosts",
                "/etc/nsswitch.conf",
                "/etc/ssl",
                "/etc/ca-certificates",
                "/etc/pki",
            ]
            .map(RuntimeSource::system),
        );
    }
    if accounts {
        paths
            .extend(["/etc/passwd", "/etc/group", "/etc/nsswitch.conf"].map(RuntimeSource::system));
    }
    paths.retain(|source| source.path.exists());
    paths
}

/// Runtime paths as the sandbox sees them. Each host object is bound once,
/// read-only, at its canonical path, with protected/read-denied overlap
/// rejected up front. Any other spelling becomes a sandbox symlink. Binding an
/// alias such as `/bin -> usr/bin` by name would expose the same content under
/// a second name that no mask covers.
#[cfg(target_os = "linux")]
#[derive(Debug, Default, PartialEq)]
pub(crate) struct RuntimeLayout {
    /// Canonical paths to bind; none lies below another.
    pub(crate) binds: Vec<PathBuf>,
    /// `(alias, canonical)` symlinks for spellings no bind already reaches.
    pub(crate) links: Vec<(PathBuf, PathBuf)>,
}

#[cfg(target_os = "linux")]
impl RuntimeLayout {
    pub(crate) fn for_shell(network: bool, accounts: bool) -> Result<Self> {
        Self::resolve_sources(&sandbox_runtime_sources(network, accounts))
    }

    fn resolve_sources(sources: &[RuntimeSource]) -> Result<Self> {
        let mut canonical_paths = BTreeSet::new();
        let mut aliases = Vec::new();
        for source in sources {
            let path = &source.path;
            let canonical = match std::fs::canonicalize(path) {
                Ok(canonical) => canonical,
                // Optional; it disappeared after discovery.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(anyhow::Error::new(error).context(format!(
                        "cannot resolve sandbox runtime path {}",
                        path.display()
                    )));
                }
            };
            source.validate_target(&canonical)?;
            if canonical != *path {
                aliases.push((path.clone(), canonical.clone()));
            }
            canonical_paths.insert(canonical);
        }

        // Path order visits an ancestor before its descendants.
        let mut layout = Self::default();
        for path in canonical_paths {
            if !layout.binds.iter().any(|bind| path.starts_with(bind)) {
                layout.binds.push(path);
            }
        }
        aliases.sort_by_key(|(alias, _)| alias.components().count());
        for (alias, canonical) in aliases {
            let reachable = layout.binds.iter().any(|bind| alias.starts_with(bind))
                || layout.links.iter().any(|(link, _)| alias.starts_with(link));
            if !reachable {
                layout.links.push((alias, canonical));
            }
        }
        Ok(layout)
    }

    #[cfg(test)]
    fn resolve(paths: &[PathBuf]) -> Result<Self> {
        let sources = paths
            .iter()
            .cloned()
            .map(RuntimeSource::system)
            .collect::<Vec<_>>();
        Self::resolve_sources(&sources)
    }
}

#[cfg(target_os = "linux")]
fn resolve_shell_guards(config: &RuntimeConfig) -> Result<ShellGuards> {
    let access = config.access.refreshed()?;
    let protected = guard_identities(&access.base_dir, &config.protected_paths)?;
    let runtime = RuntimeLayout::for_shell(config.allow_network, config.host_shell.ssh_agent)?;
    let anchors = validate_shell_guards(&access, &config.cache_mounts, &protected, &runtime)?;
    validate_host_mount_policy(&access, &config.host_shell, &protected)?;
    Ok(ShellGuards {
        access,
        protected,
        runtime,
        anchors,
    })
}

#[cfg(target_os = "linux")]
fn shell_guard_generation(guards: &ShellGuards) -> [u8; 32] {
    fn paths<'a>(digest: &mut Sha256, tag: u8, values: impl IntoIterator<Item = &'a Path>) {
        digest.update([tag]);
        for path in values {
            let bytes = path.as_os_str().as_bytes();
            digest.update((bytes.len() as u64).to_be_bytes());
            digest.update(bytes);
        }
        digest.update(0_u64.to_be_bytes());
    }

    let mut digest = Sha256::new();
    digest.update(b"dotlink-shell-guards-v1\0");
    paths(
        &mut digest,
        1,
        guards.access.deny_read_roots.iter().map(PathBuf::as_path),
    );
    paths(
        &mut digest,
        2,
        guards.access.deny_write_roots.iter().map(PathBuf::as_path),
    );
    if guards.access.explicit_root_rw_grant() {
        paths(&mut digest, 3, std::iter::empty());
    } else {
        paths(
            &mut digest,
            3,
            guards.protected.iter().map(PathBuf::as_path),
        );
    }
    digest.update([4]);
    for anchor in &guards.anchors {
        let bytes = anchor.path.as_os_str().as_bytes();
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
        digest.update([u8::from(anchor.writable)]);
    }
    digest.update(0_u64.to_be_bytes());
    digest.finalize().into()
}

#[derive(Clone, Debug)]
struct ShellRuntime {
    config: RuntimeConfig,
    launchers: crate::mcp::PreparedLaunchers,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    secure_fs: Arc<crate::secure_fs::SecureFs>,
}

impl ShellRuntime {
    async fn prepare_shell_command(
        &self,
        args: &ShellArgs,
        powershell: bool,
    ) -> Result<(Command, PathBuf, Option<[u8; 32]>)> {
        if let Some(stdin_text) = args.stdin.as_ref()
            && stdin_text.len() > self.config.max_write_bytes
        {
            anyhow::bail!(
                "stdin is {} bytes, above the {} byte limit",
                stdin_text.len(),
                self.config.max_write_bytes
            );
        }
        let working_dir = resolve_existing_runtime(&self.config, &args.dir).await?;
        if !fs::metadata(&working_dir).await?.is_dir() {
            anyhow::bail!("dir is not a directory");
        }
        let shell_launcher = self
            .launchers
            .shell
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("shell execution is disabled"))?;

        #[cfg(target_os = "linux")]
        let (command, shell_guard_generation) = if self.config.sandbox_shell && !powershell {
            let guards = self.shell_guards()?;
            let generation = shell_guard_generation(&guards);
            (
                self.bubblewrap_command_with_guards(
                    shell_launcher,
                    &working_dir,
                    &args.command,
                    guards,
                )?,
                Some(generation),
            )
        } else {
            (
                direct_shell_command(shell_launcher, powershell, &working_dir, &args.command)?,
                None,
            )
        };
        #[cfg(not(target_os = "linux"))]
        let (command, shell_guard_generation) = (
            direct_shell_command(shell_launcher, powershell, &working_dir, &args.command)?,
            None,
        );
        Ok((command, working_dir, shell_guard_generation))
    }

    #[cfg(target_os = "linux")]
    fn shell_guards(&self) -> Result<ShellGuards> {
        resolve_shell_guards(&self.config)
    }

    fn output_limit(&self, requested: Option<usize>) -> usize {
        resolve_capped(requested, self.config.max_output_bytes)
    }

    #[cfg(target_os = "linux")]
    #[cfg_attr(not(test), allow(dead_code))]
    fn bubblewrap_command(
        &self,
        shell: &PreparedExecutable,
        working_dir: &Path,
        script: &str,
    ) -> Result<Command> {
        let guards = self.shell_guards()?;
        self.bubblewrap_command_with_guards(shell, working_dir, script, guards)
    }

    #[cfg(target_os = "linux")]
    fn bubblewrap_command_with_guards(
        &self,
        shell: &PreparedExecutable,
        working_dir: &Path,
        script: &str,
        guards: ShellGuards,
    ) -> Result<Command> {
        let bwrap = self
            .launchers
            .bwrap
            .as_ref()
            .expect("bwrap validated and pinned at startup");
        let access = &guards.access;
        let mut command = bwrap.sandbox_host_command()?;
        let mut mount_fds: Vec<std::fs::File> = Vec::new();
        #[derive(Clone, Copy)]
        enum BindFdKind {
            ReadOnly,
            ReadWrite,
            Device,
            ExecutableData,
        }
        let mut bind_fd =
            |command: &mut Command, source: std::fs::File, dest: &Path, kind: BindFdKind| {
                let descriptor = std::os::fd::AsRawFd::as_raw_fd(&source);
                match kind {
                    BindFdKind::Device => {
                        // Bubblewrap's regular binds set MS_NODEV, making opened
                        // device nodes unusable. --dev-bind preserves character
                        // device access; the source is the already-pinned FD,
                        // never a mutable host pathname.
                        command
                            .arg("--dev-bind")
                            .arg(format!("/proc/self/fd/{descriptor}"))
                            .arg(dest);
                    }
                    BindFdKind::ExecutableData => {
                        command
                            .arg("--perms")
                            .arg("0500")
                            .arg("--ro-bind-data")
                            .arg(descriptor.to_string())
                            .arg(dest);
                    }
                    BindFdKind::ReadOnly | BindFdKind::ReadWrite => {
                        command
                            .arg(if matches!(kind, BindFdKind::ReadWrite) {
                                "--bind-fd"
                            } else {
                                "--ro-bind-fd"
                            })
                            .arg(descriptor.to_string())
                            .arg(dest);
                    }
                }
                mount_fds.push(source);
            };

        command
            .arg("--die-with-parent")
            .arg("--new-session")
            .arg("--unshare-pid")
            .arg("--unshare-ipc")
            .arg("--unshare-uts");
        if !self.config.allow_network {
            command.arg("--unshare-net");
        }

        if access.unrestricted_fs {
            command.arg("--bind").arg("/").arg("/");
        }

        command
            .arg("--proc")
            .arg("/proc")
            .arg("--dev")
            .arg("/dev")
            .arg("--tmpfs")
            .arg("/tmp")
            .arg("--dir")
            .arg("/tmp/home");
        if !access.unrestricted_fs {
            command.arg("--dir").arg("/etc");
        }

        if !access.unrestricted_fs {
            // Runtime links go in before any policy bind: created later, a
            // link inside a writable grant would land on the host.
            for path in &guards.runtime.binds {
                prepare_mount_target_dirs(&mut command, path);
                let source = crate::secure_fs::SecureFs::pin_canonical(path)?;
                bind_fd(&mut command, source, path, BindFdKind::ReadOnly);
            }
            for (alias, canonical) in &guards.runtime.links {
                prepare_parent_dirs(&mut command, alias);
                command.arg("--symlink").arg(canonical).arg(alias);
            }
        }

        // User grants and namespace anchors are one ordered mount layer. A
        // deeper explicit grant must remain able to override a broader grant's
        // access mode even when an anchored protected directory shares an
        // ancestor with it, so sort the union shallow -> deep and bind each
        // path exactly once.
        let mut policy_mounts = BTreeMap::<PathBuf, bool>::new();
        if !access.unrestricted_fs {
            for (path, writable) in access.sandbox_mounts() {
                policy_mounts.insert(path, writable);
            }
        }
        for anchor in &guards.anchors {
            policy_mounts
                .entry(anchor.path.clone())
                .or_insert(anchor.writable);
        }
        let mut policy_mounts = policy_mounts.into_iter().collect::<Vec<_>>();
        policy_mounts.sort_by_key(|(path, _)| path.components().count());
        for (path, writable) in policy_mounts {
            prepare_mount_target_dirs(&mut command, &path);
            let source = self.secure_fs.open_directory(&path)?;
            bind_fd(
                &mut command,
                source,
                &path,
                if writable {
                    BindFdKind::ReadWrite
                } else {
                    BindFdKind::ReadOnly
                },
            );
        }

        for cache in &self.config.cache_mounts {
            prepare_directory_mount_target(&mut command, &cache.target);
            let source = cache.pinned.try_clone()?;
            bind_fd(
                &mut command,
                source,
                &cache.target,
                if cache.writable {
                    BindFdKind::ReadWrite
                } else {
                    BindFdKind::ReadOnly
                },
            );
        }

        for mount in &self.config.host_shell.mounts {
            prepare_mount_target_dirs(&mut command, &mount.target);
            let source = mount.pinned.try_clone()?;
            bind_fd(
                &mut command,
                source,
                &mount.target,
                if mount.kind == crate::host_services::HostMountKind::CharacterDevice {
                    BindFdKind::Device
                } else {
                    BindFdKind::ReadOnly
                },
            );
        }

        // The command interpreter is part of prepared execution authority too.
        // Materialize the sealed snapshot through Bubblewrap's data-FD surface
        // rather than descriptor bind mounting an anonymous memfd: this keeps the
        // immutable bytes while avoiding any dependency on realpath() support for
        // `/proc/self/fd/<memfd>` mount sources.
        prepare_mount_target_dirs(&mut command, shell.path());
        bind_fd(
            &mut command,
            shell.open_snapshot_reader()?,
            shell.path(),
            BindFdKind::ExecutableData,
        );

        // Guards act on canonical objects. Shared admission already rejects
        // guard overlap with user and implicit runtime mounts, regardless of
        // whether a host path is momentarily present. The following masks
        // remain defense in depth for other sandbox-visible sources.
        for denied in existing_canonical(&access.deny_write_roots)? {
            if access.denied_read(&denied)
                || !access.grant_read(&denied)
                || !access.grant_write(&denied)
            {
                continue;
            }
            prepare_mount_target_dirs(&mut command, &denied);
            let source = crate::secure_fs::SecureFs::pin_canonical(&denied)?;
            bind_fd(&mut command, source, &denied, BindFdKind::ReadOnly);
        }

        // Masks follow every mount above: runtime, grant, cache and host
        // mounts can each make a path visible, so they are unconditional.
        for guarded in existing_canonical(&access.deny_read_roots)? {
            mask_path(&mut command, &guarded)?;
        }
        // An explicit root RW grant is the user's filesystem opt-out, not a
        // promise of partial isolation. Do not add ineffective private-file
        // masks that would contradict the unrestricted filesystem choice.
        if !access.explicit_root_rw_grant() {
            for guarded in existing_canonical(&guards.protected)? {
                mask_path(&mut command, &guarded)?;
            }
        }

        if !self.config.host_shell.nix_daemon {
            // Nix daemon access is a separate shell capability, not a side
            // effect of --allow-network or filesystem grants. Mask this last
            // so a broad user grant, a writable cache or a pre-existing root
            // bind cannot reintroduce a daemon socket after the restriction.
            for daemon_endpoint in [
                Path::new("/nix/var/nix/daemon-socket"),
                Path::new("/run/nix-daemon"),
            ] {
                mask_path(&mut command, daemon_endpoint)?;
            }
        }

        command
            .arg("--chdir")
            .arg(working_dir)
            .arg("--clearenv")
            .arg("--setenv")
            .arg("HOME")
            .arg("/tmp/home")
            .arg("--setenv")
            .arg("TMPDIR")
            .arg("/tmp")
            .arg("--setenv")
            .arg("PATH")
            .arg(sandbox_visible_path(access));

        for name in ["LANG", "LC_ALL", "TERM", "USER"] {
            if let Ok(value) = env::var(name) {
                command.arg("--setenv").arg(name).arg(value);
            }
        }

        for (name, value) in &self.config.shell_env {
            command.arg("--setenv").arg(name).arg(value);
        }
        for (name, value) in &self.config.host_shell.environment {
            command.arg("--setenv").arg(name).arg(value);
        }

        command
            .arg("--")
            .arg(shell.path())
            .arg("--noprofile")
            .arg("--norc")
            .arg("-lc")
            .arg(script);

        if !mount_fds.is_empty() {
            use std::os::{fd::AsRawFd, unix::process::CommandExt};
            // SAFETY: only async-signal-safe fcntl calls run in the child
            // before exec. Parent FDs retain CLOEXEC, so concurrent child
            // processes cannot inherit another request's mount capabilities.
            unsafe {
                command.as_std_mut().pre_exec(move || {
                    for source in &mount_fds {
                        if libc::fcntl(source.as_raw_fd(), libc::F_SETFD, 0) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
        }
        Ok(command)
    }
}

async fn resolve_existing_runtime(config: &RuntimeConfig, raw: &str) -> Result<PathBuf> {
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        config.access.base_dir.join(raw)
    };
    let canonical = fs::canonicalize(&candidate)
        .await
        .with_context(|| format!("path does not exist: {}", candidate.display()))?;
    if !config.access.grant_read(&canonical) || config.access.denied_read(&canonical) {
        anyhow::bail!("read access is not allowed: {}", canonical.display());
    }
    Ok(canonical)
}

impl LocalMachine {
    fn shell_runtime(&self) -> ShellRuntime {
        ShellRuntime {
            config: self.config.clone(),
            launchers: self.launchers.clone(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            secure_fs: self.secure_fs.clone(),
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    fn bubblewrap_command(
        &self,
        shell: &Path,
        working_dir: &Path,
        script: &str,
    ) -> Result<Command> {
        let shell = PreparedExecutable::for_test(shell.to_path_buf());
        self.shell_runtime()
            .bubblewrap_command(&shell, working_dir, script)
    }

    async fn run_shell_to_completion(
        &self,
        args: ShellArgs,
        powershell: bool,
        heartbeat: Option<ProgressHeartbeat>,
    ) -> Result<CallToolResult, McpError> {
        let started = match self.start_shell_job(args, powershell, None) {
            Ok(started) => started,
            Err(error) => return Ok(tool_error(error)),
        };
        let wait = self.jobs.wait_until(&started.id, None);
        tokio::pin!(wait);
        loop {
            tokio::select! {
                biased;
                result = &mut wait => {
                    result.map_err(crate::jobs::JobError::into_mcp)?;
                    break;
                }
                () = heartbeat_tick(heartbeat.as_ref()), if heartbeat.is_some() => {
                    if let Some(heartbeat) = heartbeat.as_ref() {
                        heartbeat
                            .report(Instant::now() + Duration::from_secs(self.config.max_shell_timeout_secs))
                            .await;
                    }
                }
            }
        }
        let result = self
            .jobs
            .tool_result(&started.id, None)
            .map_err(crate::jobs::JobError::into_mcp)?
            .ok_or_else(|| {
                McpError::internal_error("local shell result unexpectedly unavailable", None)
            })?;
        self.jobs
            .mark_synchronous_result(&started.id)
            .map_err(crate::jobs::JobError::into_mcp)?;
        Ok(result)
    }

    #[cfg(test)]
    async fn execute_shell(
        &self,
        args: ShellArgs,
        powershell: bool,
        heartbeat: Option<ProgressHeartbeat>,
    ) -> Result<CallToolResult, McpError> {
        self.run_shell_to_completion(args, powershell, heartbeat)
            .await
    }

    pub(crate) fn response_from_handoff(
        &self,
        id: &str,
        handoff: JobHandoff,
        result_budget: Option<usize>,
    ) -> Result<CallToolResponse, McpError> {
        match handoff {
            JobHandoff::Synchronous => {
                let result = self
                    .jobs
                    .tool_result(id, result_budget)
                    .map_err(crate::jobs::JobError::into_mcp)?
                    .ok_or_else(|| {
                        McpError::internal_error(
                            "committed synchronous shell result no longer fits the request budget",
                            None,
                        )
                    })?;
                Ok(CallToolResponse::Complete(result))
            }
            JobHandoff::Compatibility(receipt) => Ok(CallToolResponse::Complete(receipt)),
            JobHandoff::NativeTask(task) => Ok(CallToolResponse::Task(CreateTaskResult::new(task))),
        }
    }

    pub(crate) fn recover_job_request_result(
        &self,
        request_key: &str,
        result_budget: Option<usize>,
    ) -> Result<Option<(Value, bool)>, McpError> {
        let Some(id) = self.jobs.request_job_id(request_key) else {
            return Ok(None);
        };
        let handoff = match self
            .jobs
            .replay_handoff(&id)
            .map_err(crate::jobs::JobError::into_mcp)?
        {
            Some(handoff) => handoff,
            None => self
                .jobs
                .handoff(&id, false, self.task_result_budget(result_budget))
                .map_err(crate::jobs::JobError::into_mcp)?,
        };
        let requires_owner = !matches!(handoff, JobHandoff::Synchronous);
        let value = match handoff {
            JobHandoff::Synchronous => {
                let result = self
                    .jobs
                    .tool_result(&id, result_budget)
                    .map_err(crate::jobs::JobError::into_mcp)?
                    .ok_or_else(|| {
                        McpError::internal_error(
                            "recovered synchronous shell result no longer fits the request budget",
                            None,
                        )
                    })?;
                serde_json::to_value(result).map_err(|error| {
                    McpError::internal_error(
                        format!("failed to serialize recovered shell result: {error}"),
                        None,
                    )
                })?
            }
            JobHandoff::Compatibility(receipt) => {
                serde_json::to_value(receipt).map_err(|error| {
                    McpError::internal_error(
                        format!("failed to serialize recovered job receipt: {error}"),
                        None,
                    )
                })?
            }
            JobHandoff::NativeTask(task) => serde_json::to_value(CreateTaskResult::new(task))
                .map_err(|error| {
                    McpError::internal_error(
                        format!("failed to serialize recovered task receipt: {error}"),
                        None,
                    )
                })?,
        };
        Ok(Some((value, requires_owner)))
    }

    pub(crate) fn job_request_shell_guard_generation(&self, request_key: &str) -> Option<[u8; 32]> {
        self.jobs.request_shell_guard_generation(request_key)
    }

    pub(crate) fn current_shell_guard_generation(&self) -> Result<Option<[u8; 32]>> {
        #[cfg(target_os = "linux")]
        if self.config.allow_shell && self.config.sandbox_shell {
            let guards = resolve_shell_guards(&self.config)?;
            return Ok(Some(shell_guard_generation(&guards)));
        }
        Ok(None)
    }

    pub(crate) fn shell_guard_generation_is_current(&self, expected: Option<[u8; 32]>) -> bool {
        expected.is_none_or(|expected| {
            matches!(
                self.current_shell_guard_generation(),
                Ok(Some(current)) if current == expected
            )
        })
    }

    pub(crate) fn job_request_delivery_state(
        &self,
        request_key: &str,
    ) -> Option<crate::jobs::RequestDeliveryState> {
        self.jobs.request_delivery_state(request_key)
    }

    fn handoff_shell_job(
        &self,
        id: &str,
        native_tasks: bool,
        result_budget: Option<usize>,
    ) -> Result<CallToolResponse, McpError> {
        let handoff = self
            .jobs
            .handoff(id, native_tasks, self.task_result_budget(result_budget))
            .map_err(crate::jobs::JobError::into_mcp)?;
        self.response_from_handoff(id, handoff, result_budget)
    }

    pub(crate) async fn handle_shell_call(
        &self,
        request: &CallToolRequestParams,
        context: &RequestContext<RoleServer>,
    ) -> Result<Option<CallToolResponse>, McpError> {
        self.handle_shell_call_with(
            request,
            self.request_sync_budget_millis(context),
            context
                .client_capabilities()
                .is_some_and(|capabilities| capabilities.supports_tasks()),
            self.request_job_key(context),
            self.request_result_budget(context),
            ProgressHeartbeat::for_request(context),
        )
        .await
    }

    async fn handle_shell_call_with(
        &self,
        request: &CallToolRequestParams,
        sync_budget_millis: Option<u64>,
        native_tasks: bool,
        request_key: Option<String>,
        result_budget: Option<usize>,
        heartbeat: Option<ProgressHeartbeat>,
    ) -> Result<Option<CallToolResponse>, McpError> {
        let name = request.name.as_ref();
        let explicit_background = name == "job_start";
        if !explicit_background && !crate::mcp::is_shell_tool(name) {
            return Ok(None);
        }
        if !self.has_tool_for_policy(name) {
            return Ok(None);
        }
        let arguments = request.arguments.clone().unwrap_or_default();
        let args = if explicit_background {
            super::jobs::decode_job_start(arguments)
        } else {
            serde_json::from_value::<ShellArgs>(Value::Object(arguments))
        }
        .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let powershell = if explicit_background {
            cfg!(windows)
        } else {
            name == "powershell"
        };

        if explicit_background {
            let started = self
                .start_shell_job(args, powershell, request_key)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            let receipt = self
                .jobs
                .expose_compatibility(&started.id)
                .map_err(crate::jobs::JobError::into_mcp)?;
            return Ok(Some(CallToolResponse::Complete(receipt)));
        }

        // Every ordinary shell call starts exactly one durable execution. The
        // transport decides only how long to wait for that handle. A fast
        // command therefore remains a normal synchronous tools/call even when
        // its requested timeout is long; a slow command keeps running after the
        // synchronous response window closes.
        let started = self
            .start_shell_job(args, powershell, request_key)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;

        if !started.is_new
            && let Some(handoff) = self
                .jobs
                .replay_handoff(&started.id)
                .map_err(crate::jobs::JobError::into_mcp)?
        {
            return self
                .response_from_handoff(&started.id, handoff, result_budget)
                .map(Some);
        }

        let deadline = sync_budget_millis
            .and_then(|millis| Instant::now().checked_add(Duration::from_millis(millis)));
        let wait = self.jobs.wait_until(&started.id, deadline);
        tokio::pin!(wait);
        let outcome = loop {
            tokio::select! {
                biased;
                result = &mut wait => break result.map_err(crate::jobs::JobError::into_mcp)?,
                () = heartbeat_tick(heartbeat.as_ref()), if heartbeat.is_some() => {
                    if let Some(heartbeat) = heartbeat.as_ref() {
                        let report_deadline = deadline.unwrap_or_else(|| {
                            Instant::now() + Duration::from_secs(self.config.max_shell_timeout_secs)
                        });
                        heartbeat.report(report_deadline).await;
                    }
                }
            }
        };
        match outcome {
            JobWait::Terminal => {
                if let Some(result) = self
                    .jobs
                    .tool_result(&started.id, result_budget)
                    .map_err(crate::jobs::JobError::into_mcp)?
                {
                    let disposition = self
                        .jobs
                        .mark_synchronous_result(&started.id)
                        .map_err(crate::jobs::JobError::into_mcp)?;
                    if disposition == JobDisposition::Synchronous {
                        return Ok(Some(CallToolResponse::Complete(result)));
                    }
                    let handoff = self
                        .jobs
                        .replay_handoff(&started.id)
                        .map_err(crate::jobs::JobError::into_mcp)?
                        .ok_or_else(|| {
                            McpError::internal_error(
                                "shell response disposition was not committed",
                                None,
                            )
                        })?;
                    return self
                        .response_from_handoff(&started.id, handoff, result_budget)
                        .map(Some);
                }
                // Execution finished, but the ordinary result cannot fit this
                // transport response. Expose the same terminal execution for
                // compact status + paged stdout/stderr rather than failing after
                // side effects have already happened.
                self.handoff_shell_job(&started.id, native_tasks, result_budget)
                    .map(Some)
            }
            JobWait::Pending => self
                .handoff_shell_job(&started.id, native_tasks, result_budget)
                .map(Some),
        }
    }

    pub(crate) fn start_shell_job(
        &self,
        args: ShellArgs,
        powershell: bool,
        request_key: Option<String>,
    ) -> Result<crate::jobs::JobStart> {
        if args.command.trim().is_empty() {
            anyhow::bail!("command must not be empty");
        }
        let timeout_secs =
            resolve_shell_timeout(args.timeout_secs, self.config.max_shell_timeout_secs);
        let runtime = self.shell_runtime();
        let operation_gate = (self.serialize || cfg!(windows)).then(|| self.operation_gate.clone());
        let output_limit = runtime.output_limit(args.max_output_bytes);
        crate::blocking::run(|| {
            self.jobs.spawn(
                timeout_secs,
                output_limit,
                request_key,
                move |job| async move {
                    // Queueing is not execution time. Acquire the topology/ordering
                    // gate first, then resolve paths and pin mounts under that gate.
                    let operation_guard = match operation_gate {
                        Some(gate) => {
                            tokio::select! {
                                biased;
                                _ = job.cancel.cancelled() => {
                                    return JobCompletion::cancelled(
                                        StreamSummary::default(),
                                        StreamSummary::default(),
                                    );
                                }
                                guard = gate.write_owned() => Some(guard),
                            }
                        }
                        None => None,
                    };
                    if job.cancel.is_cancelled() {
                        return JobCompletion::cancelled(
                            StreamSummary::default(),
                            StreamSummary::default(),
                        );
                    }

                    let (mut command, working_dir, shell_guard_generation) =
                        match runtime.prepare_shell_command(&args, powershell).await {
                            Ok(prepared) => prepared,
                            Err(error) => {
                                let message = error.to_string();
                                return JobCompletion::failed(message);
                            }
                        };
                    if job.cancel.is_cancelled() {
                        return JobCompletion::cancelled(
                            StreamSummary::default(),
                            StreamSummary::default(),
                        );
                    }
                    crate::shell_process::configure(&mut command);
                    crate::credentials::hide_from_child(&mut command)
                        .stdin(if args.stdin.is_some() {
                            Stdio::piped()
                        } else {
                            Stdio::null()
                        })
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .kill_on_drop(true);
                    if !job.mark_running(shell_guard_generation) {
                        return JobCompletion::cancelled(
                            StreamSummary::default(),
                            StreamSummary::default(),
                        );
                    }
                    let Some(deadline) =
                        Instant::now().checked_add(Duration::from_secs(timeout_secs))
                    else {
                        let message =
                            format!("timeout_secs {timeout_secs} is too large to schedule");
                        return JobCompletion::failed(message);
                    };
                    let result_context = JobResultContext {
                        dir: runtime.config.access.relative_display(&working_dir),
                        sandboxed: runtime.config.sandbox_shell,
                        network: runtime.config.allow_network,
                        timeout_secs,
                    };
                    let completion = LocalMachine::run_shell_job(
                        command,
                        args.stdin,
                        deadline,
                        job,
                        result_context,
                    )
                    .await;
                    drop(operation_guard);
                    completion
                },
            )
        })
    }

    async fn run_shell_job(
        mut command: Command,
        stdin_text: Option<String>,
        deadline: Instant,
        job: JobHandle,
        result_context: JobResultContext,
    ) -> JobCompletion {
        if Instant::now() >= deadline {
            return JobCompletion::timed_out(StreamSummary::default(), StreamSummary::default())
                .with_context(result_context);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return JobCompletion::failed(error.to_string()).with_context(result_context);
            }
        };
        let mut process_tree = match crate::shell_process::Guard::attach(&child) {
            Ok(group) => group,
            Err(error) => {
                Self::terminate_child(&mut child).await;
                return JobCompletion::failed(format!("could not contain command tree: {error}"))
                    .with_context(result_context);
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                process_tree.terminate();
                Self::terminate_child(&mut child).await;
                return JobCompletion::failed("failed to capture child stdout")
                    .with_context(result_context);
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                process_tree.terminate();
                Self::terminate_child(&mut child).await;
                return JobCompletion::failed("failed to capture child stderr")
                    .with_context(result_context);
            }
        };
        let Some([stdout_writer, stderr_writer]) = job.begin_spool_writers() else {
            process_tree.terminate();
            Self::terminate_child(&mut child).await;
            return JobCompletion::cancelled(StreamSummary::default(), StreamSummary::default())
                .with_context(result_context);
        };
        let mut stdout_task = AbortOnDropHandle::new(tokio::spawn(spool_job_output(
            stdout,
            job.stdout_path.clone(),
            stdout_writer,
        )));
        let mut stderr_task = AbortOnDropHandle::new(tokio::spawn(spool_job_output(
            stderr,
            job.stderr_path.clone(),
            stderr_writer,
        )));

        let (mut stdin_task, mut stdin_rx) = match stdin_text {
            Some(stdin_text) => match child.stdin.take() {
                Some(mut stdin) => {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let task = AbortOnDropHandle::new(tokio::spawn(async move {
                        let result = stdin.write_all(stdin_text.as_bytes()).await;
                        let _ = tx.send(result);
                    }));
                    (Some(task), Some(rx))
                }
                None => {
                    process_tree.terminate();
                    Self::terminate_child(&mut child).await;
                    abort_and_quiesce_spool_tasks(&job, &stdout_task, &stderr_task).await;
                    return JobCompletion::failed("failed to capture child stdin")
                        .with_context(result_context);
                }
            },
            None => (None, None),
        };

        enum WaitOutcome {
            Child(std::result::Result<std::process::ExitStatus, std::io::Error>),
            Timeout,
            Cancelled,
            StdinFailure(String),
        }
        let outcome = {
            let wait = child.wait();
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    biased;
                    _ = job.cancel.cancelled() => break WaitOutcome::Cancelled,
                    _ = sleep_until(deadline) => break WaitOutcome::Timeout,
                    result = &mut wait => break WaitOutcome::Child(result),
                    result = async {
                        match stdin_rx.as_mut() {
                            Some(rx) => rx.await,
                            None => std::future::pending().await,
                        }
                    }, if stdin_rx.is_some() => {
                        match result {
                            Ok(Ok(())) => {
                                stdin_rx = None;
                                stdin_task.take();
                            }
                            Ok(Err(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                                // Closing stdin early is a valid child behavior: the
                                // supplied bytes are input, not a delivery contract.
                                // Stop feeding the pipe and let the command's own exit
                                // status decide success/failure.
                                stdin_rx = None;
                                stdin_task.take();
                            }
                            Ok(Err(error)) => break WaitOutcome::StdinFailure(format!("failed writing stdin: {error}")),
                            Err(_) => break WaitOutcome::StdinFailure("stdin writer task failed".to_owned()),
                        }
                    }
                }
            }
        };

        let mut exit_status = None;
        let mut terminal = JobStatus::Completed;
        let mut terminal_error = None;
        match outcome {
            WaitOutcome::Child(Ok(status)) => exit_status = Some(status),
            WaitOutcome::Child(Err(error)) => {
                terminal = JobStatus::Failed;
                terminal_error = Some(error.to_string());
                process_tree.terminate();
                Self::terminate_child(&mut child).await;
            }
            WaitOutcome::Timeout => {
                terminal = JobStatus::TimedOut;
                process_tree.terminate();
                Self::terminate_child(&mut child).await;
            }
            WaitOutcome::Cancelled => {
                terminal = JobStatus::Cancelled;
                process_tree.terminate();
                Self::terminate_child(&mut child).await;
            }
            WaitOutcome::StdinFailure(message) => {
                terminal = JobStatus::Failed;
                terminal_error = Some(message);
                process_tree.terminate();
                Self::terminate_child(&mut child).await;
            }
        }
        if let Some(task) = stdin_task.take() {
            task.abort();
        }

        let collect_deadline = if terminal == JobStatus::Completed {
            deadline
        } else {
            Instant::now() + TIMEOUT_OUTPUT_GRACE
        };
        let collected = timeout_at(collect_deadline, async {
            let stdout = (&mut stdout_task).await;
            let stderr = (&mut stderr_task).await;
            (stdout, stderr)
        })
        .await;
        let (stdout, stderr) = match collected {
            Ok((stdout_result, stderr_result)) => {
                let stdout = match stdout_result {
                    Ok(Ok(summary)) => summary,
                    Ok(Err(error)) => {
                        process_tree.terminate();
                        return JobCompletion::internal_failed(
                            format!("failed spooling stdout: {error}"),
                            stream_summary_from_file(&job.stdout_path, true),
                            stream_summary_from_file(&job.stderr_path, true),
                        )
                        .with_context(result_context);
                    }
                    Err(error) => {
                        process_tree.terminate();
                        return JobCompletion::internal_failed(
                            format!("stdout spool task failed: {error}"),
                            stream_summary_from_file(&job.stdout_path, true),
                            stream_summary_from_file(&job.stderr_path, true),
                        )
                        .with_context(result_context);
                    }
                };
                let stderr = match stderr_result {
                    Ok(Ok(summary)) => summary,
                    Ok(Err(error)) => {
                        process_tree.terminate();
                        return JobCompletion::internal_failed(
                            format!("failed spooling stderr: {error}"),
                            stream_summary_from_file(&job.stdout_path, true),
                            stream_summary_from_file(&job.stderr_path, true),
                        )
                        .with_context(result_context);
                    }
                    Err(error) => {
                        process_tree.terminate();
                        return JobCompletion::internal_failed(
                            format!("stderr spool task failed: {error}"),
                            stream_summary_from_file(&job.stdout_path, true),
                            stream_summary_from_file(&job.stderr_path, true),
                        )
                        .with_context(result_context);
                    }
                };
                (stdout, stderr)
            }
            Err(_) if terminal == JobStatus::Completed => {
                abort_and_quiesce_spool_tasks(&job, &stdout_task, &stderr_task).await;
                process_tree.terminate();
                return JobCompletion::internal_failed(
                    format!(
                        "collecting command output timed out after {} seconds",
                        result_context.timeout_secs
                    ),
                    stream_summary_from_file(&job.stdout_path, true),
                    stream_summary_from_file(&job.stderr_path, true),
                )
                .with_context(result_context);
            }
            Err(_) => {
                abort_and_quiesce_spool_tasks(&job, &stdout_task, &stderr_task).await;
                (
                    stream_summary_from_file(&job.stdout_path, true),
                    stream_summary_from_file(&job.stderr_path, true),
                )
            }
        };

        match terminal {
            JobStatus::Completed => {
                let status = exit_status.expect("completed job has an exit status");
                // A normal exit may intentionally launch independent background
                // work. Keep containment armed until all inherited output has
                // been collected successfully, matching synchronous shell.
                process_tree.disarm();
                JobCompletion::completed(status.code(), status.success(), stdout, stderr)
                    .with_context(result_context)
            }
            JobStatus::TimedOut => {
                JobCompletion::timed_out(stdout, stderr).with_context(result_context)
            }
            JobStatus::Cancelled => {
                JobCompletion::cancelled(stdout, stderr).with_context(result_context)
            }
            JobStatus::Failed => JobCompletion::failed_with_output(
                terminal_error.unwrap_or_else(|| "shell job failed".to_owned()),
                stdout,
                stderr,
            )
            .with_context(result_context),
            JobStatus::Working => unreachable!(),
        }
    }

    async fn terminate_child(child: &mut tokio::process::Child) {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

/// The canonical object `path` names, if it exists. An unresolvable path
/// fails the command rather than going unmasked.
#[cfg(target_os = "linux")]
fn canonical_object(path: &Path) -> Result<Option<PathBuf>> {
    match std::fs::canonicalize(path) {
        Ok(object) => Ok(Some(object)),
        Err(error) if crate::mcp::is_missing(&error) => Ok(None),
        Err(error) => Err(anyhow::Error::new(error)
            .context(format!("cannot resolve guarded path {}", path.display()))),
    }
}

/// The existing objects `paths` name, canonical and once each. A mount on any
/// other spelling would follow its symlinks to one of these anyway.
#[cfg(target_os = "linux")]
fn existing_canonical(paths: &[PathBuf]) -> Result<BTreeSet<PathBuf>> {
    let mut objects = BTreeSet::new();
    for path in paths {
        objects.extend(canonical_object(path)?);
    }
    Ok(objects)
}

/// Hide what `path` names: a mode-000 tmpfs over a directory, `/dev/null`
/// over anything else. A missing path has nothing to hide.
#[cfg(target_os = "linux")]
fn mask_path(command: &mut Command, path: &Path) -> Result<()> {
    let Some(object) = canonical_object(path)? else {
        return Ok(());
    };
    prepare_mount_target_dirs(command, &object);
    if object.is_dir() {
        command
            .arg("--tmpfs")
            .arg(&object)
            .arg("--chmod")
            .arg("000")
            .arg(&object);
    } else {
        command.arg("--ro-bind").arg("/dev/null").arg(&object);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn prepare_directory_mount_target(command: &mut Command, path: &Path) {
    prepare_mount_target_dirs(command, path);
    command.arg("--dir").arg(path);
}

/// Create the sandbox directories a mount at `path` needs: the path itself
/// for a directory, else only its parents.
#[cfg(target_os = "linux")]
fn prepare_mount_target_dirs(command: &mut Command, path: &Path) {
    if path.is_dir() {
        prepare_dirs(command, path);
    } else {
        prepare_parent_dirs(command, path);
    }
}

#[cfg(target_os = "linux")]
fn prepare_parent_dirs(command: &mut Command, path: &Path) {
    if let Some(parent) = path.parent() {
        prepare_dirs(command, parent);
    }
}

#[cfg(target_os = "linux")]
fn prepare_dirs(command: &mut Command, directory: &Path) {
    let mut current = PathBuf::from("/");
    for component in directory.components() {
        if let Component::Normal(part) = component {
            current.push(part);
            command.arg("--dir").arg(&current);
        }
    }
}

fn direct_shell_command(
    shell: &PreparedExecutable,
    powershell: bool,
    working_dir: &Path,
    script: &str,
) -> Result<Command> {
    let mut command = shell.command()?;
    if powershell {
        command
            .arg("-NoLogo")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(script);
    } else {
        command
            .arg("--noprofile")
            .arg("--norc")
            .arg("-lc")
            .arg(script);
    }
    command.current_dir(working_dir);
    Ok(command)
}
#[tool_router(router = "shell_router", vis = "pub(crate)")]
impl LocalMachine {
    #[tool(
        description = "Run a Bash command.",
        annotations(
            title = "Run Bash command",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn bash(
        &self,
        Parameters(args): Parameters<ShellArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.run_shell_to_completion(args, false, ProgressHeartbeat::for_request(&context))
            .await
    }

    #[tool(
        description = "Run a PowerShell command.",
        annotations(
            title = "Run PowerShell command",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn powershell(
        &self,
        Parameters(args): Parameters<ShellArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.run_shell_to_completion(args, true, ProgressHeartbeat::for_request(&context))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::RwLock;

    use crate::logging::LogConfig;
    #[cfg(target_os = "linux")]
    use crate::mcp::PreparedCacheMount;
    use crate::mcp::{
        AccessNeed, AccessPolicy, AccessSpec, LocalMachine, MachineConfig, PreparedLaunchers,
        RuntimeConfig, SandboxCacheMount, TransportContextToken,
    };

    #[cfg(unix)]
    fn direct_shell_machine(root: PathBuf, bash: PathBuf, max_bytes: usize) -> LocalMachine {
        let access = AccessPolicy::from_spec(AccessSpec {
            base_dir: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root.clone()],
            deny_read_roots: Vec::new(),
            deny_write_roots: Vec::new(),
            unrestricted_fs: false,
        })
        .unwrap();
        let secure_fs = Arc::new(
            crate::secure_fs::SecureFs::new(
                std::slice::from_ref(&root),
                std::slice::from_ref(&root),
            )
            .unwrap(),
        );
        LocalMachine {
            config: RuntimeConfig {
                access,
                cache_mounts: Vec::new(),
                shell_env: Vec::new(),
                #[cfg(target_os = "linux")]
                host_shell: crate::host_services::HostShell::default(),
                log: LogConfig::default(),
                allow_shell: true,
                sandbox_shell: false,
                allow_network: true,
                max_shell_timeout_secs: 5,
                max_output_bytes: max_bytes,
                max_read_bytes: max_bytes,
                max_write_bytes: max_bytes,

                task_result_budget: None,
                protected_paths: Vec::new(),
            },
            launchers: PreparedLaunchers::for_test(Some(bash), None),

            jobs: crate::jobs::JobManager::new(std::time::Duration::from_secs(
                crate::limits::DEFAULT_JOB_RETENTION_SECS,
            ))
            .unwrap(),
            operation_gate: Arc::new(RwLock::new(())),
            transport_context_token: TransportContextToken::from("test-transport-context"),
            router: std::sync::OnceLock::new(),
            serialize: false,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            secure_fs,
        }
    }

    #[tokio::test]
    async fn aggregate_quota_truncation_never_resumes_after_capacity_returns() {
        struct ReleasingReader {
            step: u8,
            job: crate::jobs::JobHandle,
        }

        impl AsyncRead for ReleasingReader {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                let chunk: &[u8] = match self.step {
                    0 => b"AAAA",
                    1 => b"BBBB",
                    2 => {
                        // Simulate another job releasing quota after this stream
                        // has already dropped a middle chunk.
                        self.job.release_spool(4);
                        b"CCCC"
                    }
                    _ => return std::task::Poll::Ready(Ok(())),
                };
                self.step += 1;
                buf.put_slice(chunk);
                std::task::Poll::Ready(Ok(()))
            }
        }

        let manager =
            crate::jobs::JobManager::with_spool_limit(std::time::Duration::from_secs(60), 8)
                .unwrap();
        let (path_tx, path_rx) = tokio::sync::oneshot::channel();
        let started = manager
            .spawn(30, 4096, None, move |job| async move {
                let _ = path_tx.send(job.stdout_path.clone());
                // Four bytes model retained state owned by another stream/job.
                assert_eq!(job.reserve_spool(4), 4);
                let summary = spool_job_output(
                    ReleasingReader {
                        step: 0,
                        job: job.clone(),
                    },
                    job.stdout_path.clone(),
                    job.begin_spool_writer()
                        .expect("test spool writer registration is open"),
                )
                .await
                .unwrap();
                assert_eq!(summary.total_bytes, 12);
                assert_eq!(summary.stored_bytes, 4);
                assert!(summary.truncated);
                crate::jobs::JobCompletion::completed(
                    Some(0),
                    true,
                    summary,
                    crate::jobs::StreamSummary::default(),
                )
            })
            .unwrap();
        let id = started.id.clone();
        let path = path_rx.await.unwrap();
        assert!(matches!(
            manager.wait_until(&id, None).await.unwrap(),
            crate::jobs::JobWait::Terminal
        ));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"AAAA",
            "retained output must stay a contiguous prefix after the first dropped byte"
        );
        drop(started);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborting_spool_tasks_uses_writer_lifetime_after_one_join_result_was_consumed() {
        let manager = crate::jobs::JobManager::new(Duration::from_secs(60)).unwrap();
        let job = manager.create_for_test(30).unwrap();
        let [stdout_writer, stderr_writer] = job
            .begin_spool_writers()
            .expect("test spool writer registration is open");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut stdout = AbortOnDropHandle::new(tokio::spawn(async move {
            drop(stdout_writer);
            Ok(StreamSummary::default())
        }));
        let stderr = AbortOnDropHandle::new(tokio::spawn(async move {
            let _writer = stderr_writer;
            crate::blocking::run(|| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            Ok(StreamSummary::default())
        }));
        (&mut stdout)
            .await
            .expect("stdout task joins")
            .expect("stdout task succeeds");
        crate::blocking::run(|| entered_rx.recv_timeout(Duration::from_secs(2))).unwrap();

        {
            let quiesce = abort_and_quiesce_spool_tasks(&job, &stdout, &stderr);
            tokio::pin!(quiesce);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut quiesce)
                    .await
                    .is_err(),
                "abort must not report quiescence while blocking spool I/O is still running"
            );
            release_tx.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(2), &mut quiesce)
                .await
                .expect("quiescence must complete once the blocking write returns");
        }
        drop(stderr);
        drop(stdout);
        drop(job);
        manager.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn executable_discovery_skips_non_executable_path_entries() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let shadow = first.join("bash");
        let executable = second.join("bash");
        std::fs::write(&shadow, b"#!/bin/sh\nexit 1\n").unwrap();
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&shadow, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = env::join_paths([&first, &second]).unwrap();

        assert_eq!(
            resolve_executable_in_path("bash", &path),
            Some(executable.canonicalize().unwrap()),
            "a non-executable PATH entry must not shadow a later runnable binary"
        );
        assert_eq!(
            resolve_executable(shadow.to_str().unwrap()),
            None,
            "an explicit non-executable file is not a usable command"
        );

        // The old mode-bit test accepted this because *some* execute bit is
        // present. For a file owned by this user, the owner class is decisive:
        // other-execute must not make it runnable for the owner.
        if unsafe { libc::geteuid() } != 0 {
            let misleading = first.join("owner-no-exec");
            let fallback = second.join("owner-no-exec");
            std::fs::write(&misleading, b"#!/bin/sh\nexit 9\n").unwrap();
            std::fs::write(&fallback, b"#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&misleading, std::fs::Permissions::from_mode(0o001)).unwrap();
            std::fs::set_permissions(&fallback, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(
                resolve_executable_in_path("owner-no-exec", &path),
                Some(fallback.canonicalize().unwrap()),
                "effective owner permissions must beat an unrelated execute bit"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn prepared_executable_keeps_the_validated_inode_after_path_replacement() {
        use std::os::unix::fs::PermissionsExt as _;

        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let launcher = temp.path().join("launcher");
        std::fs::copy(bash, &launcher).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let prepared = PreparedExecutable::prepare(launcher.clone()).unwrap();
        let prepared_identity = prepared.identity();

        let replacement = temp.path().join("replacement");
        std::fs::write(&replacement, b"#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&replacement, &launcher).unwrap();

        let replacement_prepared = PreparedExecutable::prepare(launcher).unwrap();
        assert_ne!(replacement_prepared.identity(), prepared_identity);
        let status = prepared
            .command()
            .unwrap()
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg("exit 0")
            .status()
            .await
            .unwrap();
        assert!(
            status.success(),
            "execution must use the pinned validated inode, not the replacement pathname"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn prepared_executable_snapshot_survives_in_place_source_mutation() {
        use std::os::unix::fs::PermissionsExt as _;

        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let launcher = temp.path().join("launcher");
        std::fs::copy(bash, &launcher).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let prepared = PreparedExecutable::prepare(launcher.clone()).unwrap();
        let digest = prepared.digest();

        // Mutate the same source inode rather than replacing its pathname.
        std::fs::write(&launcher, b"#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let reparsed = PreparedExecutable::prepare(launcher).unwrap();
        assert_ne!(reparsed.digest(), digest);

        let status = prepared
            .command()
            .unwrap()
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg("exit 0")
            .status()
            .await
            .unwrap();
        assert!(
            status.success(),
            "prepared execution changed with source bytes"
        );

        let mut snapshot = prepared.try_clone_snapshot().unwrap();
        let error = std::io::Write::write_all(&mut snapshot, b"tamper").unwrap_err();
        assert!(
            matches!(error.raw_os_error(), Some(libc::EPERM | libc::EBUSY)),
            "sealed snapshot unexpectedly writable: {error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn prepared_shell_shebang_wrapper_keeps_snapshot_fd_through_exec() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let wrapper = temp.path().join("wrapper");
        std::fs::write(&wrapper, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let prepared = PreparedExecutable::prepare(wrapper).unwrap();
        assert!(
            prepared
                .command()
                .unwrap()
                .status()
                .await
                .unwrap()
                .success()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_launcher_rejects_shebang_wrappers_explicitly() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let wrapper = temp.path().join("bwrap");
        std::fs::write(&wrapper, b"#!/bin/sh\nexec /bin/false \"$@\"\n").unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let prepared = PreparedExecutable::prepare(wrapper).unwrap();
        let error = validate_sandbox_launcher(&prepared)
            .unwrap_err()
            .to_string();
        assert!(error.contains("#! wrapper"), "{error}");
        assert!(error.contains("native executables"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_launcher_rejects_privilege_bearing_source() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let mut prepared = PreparedExecutable::prepare(bash).unwrap();
        // The rejection logic consumes privilege provenance recorded during
        // preparation. Set that recorded fact directly so the test does not
        // depend on the surrounding filesystem allowing setuid chmod (Nix
        // builders intentionally prohibit creating privilege-bearing files).
        prepared.privileged_source = true;
        let error = validate_sandbox_launcher(&prepared)
            .unwrap_err()
            .to_string();
        assert!(error.contains("setuid/setgid"), "{error}");
        assert!(error.contains("unprivileged modern Bubblewrap"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nix_launcher_requires_trusted_selection_and_root_owned_store_bytes() {
        assert!(!nix_store_selection_authorized(false, false));
        assert!(nix_store_selection_authorized(true, false));
        assert!(nix_store_selection_authorized(false, true));
        assert!(!read_only_mapped_nix_allowed_for_effective_uid(1000, false));
        assert!(read_only_mapped_nix_allowed_for_effective_uid(1000, true));
        assert!(!read_only_mapped_nix_allowed_for_effective_uid(0, true));
        assert!(managed_selector_applies(Path::new(
            "/run/current-system/sw/bin/bwrap"
        )));
        assert!(!managed_selector_applies(Path::new(
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-bubblewrap/bin/bwrap"
        )));
        assert!(managed_selector_applies(Path::new("/usr/bin/bwrap")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nix_store_trust_accepts_traditional_root_owned_items_only() {
        assert!(traditional_nix_store_trusted(0, 0o755, true));
        assert!(traditional_nix_store_trusted(0, 0o1775, true));
        assert!(!traditional_nix_store_trusted(0, 0o775, true));
        assert!(!traditional_nix_store_trusted(1000, 0o755, true));
        assert!(!traditional_nix_store_trusted(0, 0o755, false));
        assert!(
            nix_store_item_root(Path::new(
                "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-bubblewrap/bin/bwrap"
            ))
            .is_some()
        );
        assert!(nix_store_item_root(Path::new("/nix/store/not-a-store-item/bin/bwrap")).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_discovery_skips_untrusted_executable_shadow() {
        use std::os::unix::fs::PermissionsExt as _;

        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let Some(path_env) = env::var_os("PATH") else {
            return;
        };
        let trusted = env::split_paths(&path_env)
            .map(|dir| dir.join("bwrap"))
            .find(|candidate| {
                if !is_executable_file(candidate) {
                    return false;
                }
                let selector = managed_selector_applies(candidate)
                    && root_managed_launcher_selector(candidate).unwrap_or(false);
                PreparedExecutable::prepare_sandbox_launcher_selected(candidate.clone(), selector)
                    .is_ok()
            });
        let Some(trusted) = trusted else {
            return;
        };
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let shadow = temp.path().join("bwrap");
        std::fs::copy(bash, &shadow).unwrap();
        std::fs::set_permissions(&shadow, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = env::join_paths([temp.path(), trusted.parent().unwrap()]).unwrap();

        let prepared = prepare_bubblewrap_in_path(&path)
            .unwrap()
            .expect("later trusted Bubblewrap should not be shadowed by an untrusted executable");
        assert_eq!(prepared.path(), trusted.canonicalize().unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_launcher_rejects_user_owned_source_even_outside_sandbox_write_authority() {
        use std::os::unix::fs::PermissionsExt as _;

        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let launcher_root = tempfile::tempdir().unwrap();
        let launcher = launcher_root.path().join("bwrap");
        std::fs::copy(bash, &launcher).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o555)).unwrap();
        let error = PreparedExecutable::prepare_sandbox_launcher(launcher).unwrap_err();
        assert!(
            error.downcast_ref::<UntrustedSandboxLauncher>().is_some(),
            "provenance rejection must remain typed for PATH-discovery error priority"
        );
        let error = error.to_string();
        assert!(
            error.contains("not from a trusted host-managed source"),
            "{error}"
        );
        assert!(error.contains("root-owned non-writable path"), "{error}");
        assert!(error.contains("dotlink's Nix package"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn direct_nix_store_launcher_requires_explicit_selection_authority() {
        let Some(bwrap) = resolve_executable("bwrap") else {
            return;
        };
        let Ok(canonical) = std::fs::canonicalize(&bwrap) else {
            return;
        };
        if nix_store_item_root(&canonical).is_none() {
            return;
        }
        let build_pinned = build_pinned_bubblewrap(&canonical);
        let prepared = PreparedExecutable::prepare_sandbox_launcher(canonical);
        if build_pinned {
            let prepared = prepared.expect("the build-pinned Nix launcher must be trusted");
            assert!(matches!(
                prepared.sandbox_trust,
                Some(
                    SandboxLauncherTrust::NixStoreRootOwned
                        | SandboxLauncherTrust::NixStoreReadOnlyMapped
                )
            ));
        } else {
            let error = prepared.expect_err(
                "a directly selected Nix store item must not gain trust from immutability alone",
            );
            assert!(error.downcast_ref::<UntrustedSandboxLauncher>().is_some());
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandbox_host_launcher_inherits_no_process_environment() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        if std::env::var_os("HOME").is_none() {
            return;
        }
        let ordinary = PreparedExecutable::prepare(bash.clone()).unwrap();
        let error = ordinary.sandbox_host_command().unwrap_err().to_string();
        assert!(error.contains("requires trusted provenance"), "{error}");

        // The environment-clearing behavior is tested with an explicit test-only
        // trusted marker; production can obtain that marker only through the
        // provenance-aware Bubblewrap preparation path.
        let prepared = PreparedExecutable::for_test_sandbox_launcher(bash);
        let status = prepared
            .sandbox_host_command()
            .unwrap()
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg("test -z \"${HOME+x}\" && test -z \"${SSH_AUTH_SOCK+x}\" && test -z \"${LD_PRELOAD+x}\" && test -z \"${LD_LIBRARY_PATH+x}\"")
            .status()
            .await
            .unwrap();
        assert!(
            status.success(),
            "host-side sandbox launcher inherited process environment"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sealed_host_executable_probes_only_when_validation_is_requested() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let prepared = PreparedExecutable::for_test_sandbox_launcher(bash);
        let error = validate_bubblewrap(&prepared)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing required options"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_executable_bubblewrap_is_rejected_before_capability_probe() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("bwrap");
        std::fs::write(&fake, b"not executable").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = PreparedExecutable::prepare(fake).unwrap_err();
        assert!(error.to_string().contains("not executable"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn durable_jobs_survive_machine_rebuild_but_die_with_the_process_session() {
        let Some(_bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let jobs = crate::jobs::JobManager::new(Duration::from_secs(60)).unwrap();
        let config = || crate::mcp::MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root.clone()],
                write_roots: vec![root.clone()],
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: true,
            allow_shell: true,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 5,
            max_output_bytes: 16 * 1024,
            max_read_bytes: 16 * 1024,
            max_write_bytes: 16 * 1024,

            task_result_budget: None,
            job_retention_secs: 60,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        };

        let first = LocalMachine::new_with_jobs(config(), jobs.clone())
            .await
            .unwrap();
        let gate = first.operation_gate.clone();
        let id = first
            .start_shell_job(
                ShellArgs {
                    command: "sleep 0.08; printf survived".to_owned(),
                    dir: ".".to_owned(),
                    stdin: None,
                    timeout_secs: Some(3),
                    max_output_bytes: None,
                },
                false,
                None,
            )
            .unwrap()
            .id;
        drop(first);

        let second = LocalMachine::new_with_jobs(config(), jobs.clone())
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&gate, &second.operation_gate));
        for _ in 0..100 {
            let status: Value = serde_json::from_str(&second.jobs.status(&id).unwrap()).unwrap();
            if status["status"] != "working" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let output = second
            .jobs
            .output_page(&id, crate::jobs::JobStream::Stdout, 0, 4096, 4096, false)
            .unwrap();
        assert!(output.contains("survived"), "{output}");
        drop(second);

        // The final process-session owner is authoritative for cleanup. A job
        // cannot escape by retaining the LocalMachine/JobManager in its task.
        let marker = root.join("must-not-appear");
        let third = LocalMachine::new_with_jobs(config(), jobs.clone())
            .await
            .unwrap();
        third
            .start_shell_job(
                ShellArgs {
                    command: format!("sleep 0.25; printf leaked > '{}'", marker.display()),
                    dir: ".".to_owned(),
                    stdin: None,
                    timeout_secs: Some(3),
                    max_output_bytes: None,
                },
                false,
                None,
            )
            .unwrap();
        drop(third);
        drop(jobs);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !marker.exists(),
            "session drop must kill the contained process tree"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serialized_jobs_return_ids_while_queued_without_spending_execution_timeout() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut machine = direct_shell_machine(root.clone(), bash, 16 * 1024);
        machine.serialize = true;
        let first_started = root.join("first-started");
        let late_dir = root.join("late-dir");
        let second_ran = root.join("second-ran");

        let first = machine
            .start_shell_job(
                ShellArgs {
                    command: format!("touch '{}'; sleep 0.35", first_started.display()),
                    dir: ".".to_owned(),
                    stdin: None,
                    timeout_secs: Some(2),
                    max_output_bytes: None,
                },
                false,
                None,
            )
            .unwrap()
            .id;
        for _ in 0..100 {
            if first_started.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            first_started.exists(),
            "first job never acquired the serialize gate"
        );

        let started = Instant::now();
        let second = machine
            .start_shell_job(
                ShellArgs {
                    command: format!("touch '{}'", second_ran.display()),
                    dir: "late-dir".to_owned(),
                    stdin: None,
                    timeout_secs: Some(1),
                    max_output_bytes: None,
                },
                false,
                None,
            )
            .unwrap()
            .id;
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "queued job creation waited on the serialize gate"
        );
        let queued: Value = serde_json::from_str(&machine.jobs.status(&second).unwrap()).unwrap();
        assert_eq!(queued["phase"], "queued", "{queued}");
        assert!(
            !late_dir.exists(),
            "test precondition: queued working directory must not exist yet"
        );
        std::fs::create_dir(&late_dir).unwrap();

        for _ in 0..200 {
            let status: Value =
                serde_json::from_str(&machine.jobs.status(&second).unwrap()).unwrap();
            if status["status"] != "working" {
                assert_eq!(status["status"], "completed", "{status}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            second_ran.exists(),
            "queued job must resolve its working directory only after acquiring the gate"
        );

        machine.jobs.cancel(&first).unwrap();
        machine.jobs.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_stdin_and_stdout_pressure_do_not_deadlock() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = direct_shell_machine(root, bash, 256 * 1024);
        let started = Instant::now();
        let result = machine
            .execute_shell(
                ShellArgs {
                    // Child writes more than a pipe buffer before it reads
                    // stdin; both directions must make progress concurrently.
                    command: "head -c 160000 /dev/zero; cat >/dev/null".to_owned(),
                    dir: ".".to_owned(),
                    stdin: Some("x".repeat(160 * 1024)),
                    timeout_secs: Some(3),
                    max_output_bytes: Some(256 * 1024),
                },
                false,
                None,
            )
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "bidirectional pipe pressure timed out: {result:?}"
        );
        assert_ne!(result.is_error, Some(true), "{result:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_closing_stdin_early_does_not_fail_or_kill_the_command() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = direct_shell_machine(root, bash, 256 * 1024);
        let result = machine
            .execute_shell(
                ShellArgs {
                    command: "exec 0<&-; sleep 0.05; printf survived".to_owned(),
                    dir: ".".to_owned(),
                    stdin: Some("x".repeat(160 * 1024)),
                    timeout_secs: Some(3),
                    max_output_bytes: Some(16 * 1024),
                },
                false,
                None,
            )
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let value = serde_json::to_value(result).unwrap();
        let payload: Value = serde_json::from_str(
            value["content"][0]["text"]
                .as_str()
                .expect("shell result text"),
        )
        .unwrap();
        assert_eq!(payload["stdout"], "survived");
        assert_eq!(payload["success"], true);
    }

    #[test]
    fn shell_tool_schema_reports_configured_ceilings() {
        let shell = LocalMachine::all_tools()
            .get(crate::mcp::shell_tool_name())
            .cloned()
            .unwrap();
        let patched = with_profile_limits(shell.clone(), 42, 2048);
        let properties = patched.input_schema["properties"].as_object().unwrap();
        let timeout = properties["timeout_secs"]["description"].as_str().unwrap();
        let output = properties["max_output_bytes"]["description"]
            .as_str()
            .unwrap();
        assert!(timeout.contains("42"), "{timeout}");
        assert!(!timeout.contains("max_shell_timeout_secs"), "{timeout}");
        assert!(output.contains("2048"), "{output}");
        // Non-shell tools keep their static schemas.
        let read = LocalMachine::all_tools().get("read").cloned().unwrap();
        assert_eq!(
            with_profile_limits(read.clone(), 42, 2048).input_schema,
            read.input_schema
        );
    }

    #[tokio::test]
    async fn heartbeat_delivery_never_crosses_the_command_deadline() {
        let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let delivery_polled = polled.clone();
        let delivery = async move {
            delivery_polled.store(true, Ordering::Relaxed);
            std::future::pending::<()>().await;
        };

        deliver_heartbeat_bounded(Instant::now() - Duration::from_secs(1), delivery).await;
        assert!(
            !polled.load(Ordering::Relaxed),
            "an expired command deadline must win before delivery is polled"
        );
    }

    #[test]
    fn output_limits_are_client_controlled_under_a_policy_ceiling() {
        // Omitted -> the profile ceiling.
        assert_eq!(resolve_capped(None, 1_048_576_usize), 1_048_576);
        // Caller lowers it freely.
        assert_eq!(resolve_capped(Some(4_096_usize), 1_048_576), 4_096);
        // Caller cannot exceed the ceiling.
        assert_eq!(resolve_capped(Some(8_388_608_usize), 1_048_576), 1_048_576);
        assert_eq!(resolve_capped(Some(0_usize), 1_048_576), 1);
    }

    #[test]
    fn shell_timeout_is_client_controlled_under_a_policy_ceiling() {
        // Omitted -> the profile ceiling, not a hidden 30 s default.
        assert_eq!(resolve_shell_timeout(None, 3_600), 3_600);
        // Explicit caller values are honored inside the ceiling.
        assert_eq!(resolve_shell_timeout(Some(600), 3_600), 600);
        assert_eq!(resolve_shell_timeout(Some(1), 3_600), 1);
        // The caller cannot exceed the ceiling.
        assert_eq!(resolve_shell_timeout(Some(7_200), 3_600), 3_600);
        // Zero is clamped up; a degenerate ceiling still yields a positive deadline.
        assert_eq!(resolve_shell_timeout(Some(0), 3_600), 1);
        assert_eq!(resolve_shell_timeout(None, 0), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_waits_synchronously_then_hands_off_the_same_durable_job() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = direct_shell_machine(root, bash, 16 * 1024);
        let request = |command: &str, timeout: Option<u64>| {
            let mut arguments = serde_json::Map::new();
            arguments.insert("command".to_owned(), Value::String(command.to_owned()));
            if let Some(timeout) = timeout {
                arguments.insert("timeout_secs".to_owned(), Value::from(timeout));
            }
            CallToolRequestParams::new("bash").with_arguments(arguments)
        };

        // A generous execution timeout never forces an early handoff. If the
        // command actually finishes inside the transport window it is the
        // ordinary synchronous bash result.
        let fast = machine
            .handle_shell_call_with(
                &request("printf fast", Some(3)),
                Some(500),
                false,
                Some("fast-request".to_owned()),
                None,
                None,
            )
            .await
            .unwrap()
            .expect("shell is handled by the durable layer");
        let CallToolResponse::Complete(fast) = fast else {
            panic!("fast command should stay synchronous")
        };
        let fast_text = serde_json::to_value(fast).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(fast_text.contains("fast"), "{fast_text}");
        assert!(!fast_text.contains("job_id"), "{fast_text}");

        // Omitted execution timeout still uses the profile ceiling; only the
        // response wait is short. The command continues and compatibility
        // clients get its stable job id.
        let slow_request = request("sleep 0.12; printf compat", None);
        let first = machine
            .handle_shell_call_with(
                &slow_request,
                Some(10),
                false,
                Some("compat-request".to_owned()),
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let compat_id = match first {
            CallToolResponse::Complete(result) => {
                let value = serde_json::to_value(result).unwrap();
                let text = value["content"][0]["text"].as_str().unwrap();
                serde_json::from_str::<Value>(text).unwrap()["job_id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            }
            other => panic!("expected compatibility job receipt, got {other:?}"),
        };
        // Redelivery of the same tunnel request must never launch a duplicate.
        let replay = machine
            .handle_shell_call_with(
                &slow_request,
                Some(10),
                false,
                Some("compat-request".to_owned()),
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let replay_id = match replay {
            CallToolResponse::Complete(result) => {
                let value = serde_json::to_value(result).unwrap();
                let text = value["content"][0]["text"].as_str().unwrap();
                serde_json::from_str::<Value>(text).unwrap()["job_id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            }
            other => panic!("expected replayed compatibility receipt, got {other:?}"),
        };
        assert_eq!(replay_id, compat_id);

        // Tasks-capable clients expose the same durable execution through MCP
        // Tasks when it outlives the synchronous response window.
        let native = machine
            .handle_shell_call_with(
                &request("sleep 0.12; printf native", Some(3)),
                Some(10),
                true,
                Some("native-request".to_owned()),
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let native_id = match native {
            CallToolResponse::Task(task) => task.task.task_id,
            other => panic!("expected native task, got {other:?}"),
        };

        for _ in 0..100 {
            let status: Value =
                serde_json::from_str(&machine.jobs.status(&compat_id).unwrap()).unwrap();
            if status["status"] != "working" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let compat_status: Value =
            serde_json::from_str(&machine.jobs.status(&compat_id).unwrap()).unwrap();
        assert_ne!(compat_status["status"], "working");
        for _ in 0..100 {
            if machine
                .jobs
                .detailed_task(&native_id)
                .unwrap()
                .status()
                .is_terminal()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            machine
                .jobs
                .detailed_task(&native_id)
                .unwrap()
                .status()
                .is_terminal()
        );
        assert!(
            machine
                .jobs
                .output_page(
                    &compat_id,
                    crate::jobs::JobStream::Stdout,
                    0,
                    4096,
                    4096,
                    false,
                )
                .unwrap()
                .contains("compat")
        );
        assert!(
            machine
                .jobs
                .output_page(
                    &native_id,
                    crate::jobs::JobStream::Stdout,
                    0,
                    4096,
                    4096,
                    false,
                )
                .unwrap()
                .contains("native")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_execution_shell_failure_keeps_plain_tool_error_contract() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = direct_shell_machine(root, bash, 16 * 1024);
        let result = machine
            .execute_shell(
                ShellArgs {
                    command: "printf never-runs".to_owned(),
                    dir: "missing-directory".to_owned(),
                    stdin: None,
                    timeout_secs: Some(3),
                    max_output_bytes: None,
                },
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("path does not exist"), "{text}");
        assert!(
            !text.starts_with('{'),
            "pre-execution errors should stay direct: {text}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_shell_result_that_cannot_fit_hands_off_the_same_job() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = direct_shell_machine(root, bash, 16 * 1024);
        let request = CallToolRequestParams::new("bash").with_arguments(
            serde_json::from_value::<serde_json::Map<String, Value>>(serde_json::json!({
                "command": "printf '%4096s' x",
                "dir": ".",
                "timeout_secs": 3,
                "max_output_bytes": 8192
            }))
            .unwrap(),
        );
        let response = machine
            .handle_shell_call_with(
                &request,
                Some(2_000),
                false,
                Some("oversized-complete".to_owned()),
                Some(512),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let CallToolResponse::Complete(receipt) = response else {
            panic!("compatibility client should receive a compact job receipt")
        };
        let receipt_text = receipt.content[0].as_text().unwrap().text.as_str();
        let receipt: Value = serde_json::from_str(receipt_text).unwrap();
        assert_eq!(receipt["status"], "completed", "{receipt}");
        assert_eq!(receipt["async"], true, "{receipt}");
        let job_id = receipt["job_id"].as_str().unwrap();
        let page: Value = serde_json::from_str(
            &machine
                .jobs
                .output_page(job_id, crate::jobs::JobStream::Stdout, 0, 8192, 8192, false)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(page["stored_bytes"], 4096);
        assert_eq!(page["data"].as_str().unwrap().len(), 4096);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_oversized_shell_result_falls_back_to_compatibility_job() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = direct_shell_machine(root, bash, 64 * 1024);
        let request = CallToolRequestParams::new("bash").with_arguments(
            serde_json::from_value::<serde_json::Map<String, Value>>(serde_json::json!({
                "command": "for ((i=0; i<8192; i++)); do printf x; done",
                "timeout_secs": 3,
                "max_output_bytes": 16384
            }))
            .unwrap(),
        );
        let response = machine
            .handle_shell_call_with(
                &request,
                Some(5_000),
                true,
                Some("oversized-complete".to_owned()),
                Some(1024),
                None,
            )
            .await
            .unwrap()
            .expect("shell interception");
        let CallToolResponse::Complete(receipt) = response else {
            panic!("oversized completed result must use compatibility receipt")
        };
        let value = serde_json::to_value(receipt).unwrap();
        assert_ne!(value["isError"], true);
        let text = value["content"][0]["text"].as_str().unwrap();
        let receipt: Value = serde_json::from_str(text).unwrap();
        assert_eq!(receipt["async"], true, "{receipt}");
        let job_id = receipt["job_id"].as_str().unwrap();
        let status: Value = serde_json::from_str(&machine.jobs.status(job_id).unwrap()).unwrap();
        assert_eq!(status["status"], "completed");
        let output: Value = serde_json::from_str(
            &machine
                .jobs
                .output_page(
                    job_id,
                    crate::jobs::JobStream::Stdout,
                    0,
                    16_384,
                    16_384,
                    false,
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(output["stored_bytes"], 8192);
        assert_eq!(output["data"].as_str().unwrap().len(), 8192);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn promoted_native_task_returns_the_exact_original_shell_result() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = direct_shell_machine(root, bash, 16 * 1024);
        let expected = machine
            .execute_shell(
                ShellArgs {
                    command: "printf abcdefgh".to_owned(),
                    dir: ".".to_owned(),
                    stdin: None,
                    timeout_secs: Some(3),
                    max_output_bytes: Some(4),
                },
                false,
                None,
            )
            .await
            .unwrap();
        let request = CallToolRequestParams::new("bash").with_arguments(
            serde_json::from_value::<serde_json::Map<String, Value>>(serde_json::json!({
                "command": "sleep 0.05; printf abcdefgh",
                "dir": ".",
                "timeout_secs": 3,
                "max_output_bytes": 4
            }))
            .unwrap(),
        );
        let task_id = match machine
            .handle_shell_call_with(
                &request,
                Some(1),
                true,
                Some("native-exact".to_owned()),
                Some(crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES),
                None,
            )
            .await
            .unwrap()
            .unwrap()
        {
            CallToolResponse::Task(task) => task.task.task_id,
            other => panic!("expected native task, got {other:?}"),
        };
        let completed = loop {
            let detailed = machine.jobs.detailed_task(&task_id).unwrap();
            if detailed.status().is_terminal() {
                break detailed;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let rmcp::model::TaskPayload::Completed { result } = completed.payload else {
            panic!("native task did not complete with a tool result")
        };
        let task_result: CallToolResult = serde_json::from_value(Value::Object(result)).unwrap();
        assert_eq!(
            serde_json::to_value(&task_result).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "SEP-2663 tasks/get must inline the original tools/call result"
        );

        // Full durable output remains pageable beyond the ordinary result's
        // per-stream limit, and acknowledgement cannot delete the source needed
        // to reproduce the native Task result until task retention expires.
        let stdout_page: Value = serde_json::from_str(
            &machine
                .jobs
                .output_page(
                    &task_id,
                    crate::jobs::JobStream::Stdout,
                    0,
                    4096,
                    4096,
                    false,
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stdout_page["data"], "abcdefgh");
        let stdout_end = stdout_page["next_offset"].as_u64().unwrap();
        let acknowledged: Value = serde_json::from_str(
            &machine
                .jobs
                .acknowledge_output(&task_id, crate::jobs::JobStream::Stdout, stdout_end)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(acknowledged["stdout"]["released"], true);
        let compacted: Value =
            serde_json::from_str(&machine.jobs.status(&task_id).unwrap()).unwrap();
        assert_eq!(
            compacted["stdout"]["stored_bytes"], 4,
            "job_get must report the retained native-task prefix, not historical spool bytes"
        );
        let replay = machine.jobs.detailed_task(&task_id).unwrap();
        let rmcp::model::TaskPayload::Completed { result } = replay.payload else {
            panic!("acknowledgement changed native task status")
        };
        let replayed: CallToolResult = serde_json::from_value(Value::Object(result)).unwrap();
        assert_eq!(
            serde_json::to_value(replayed).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_task_falls_back_to_compat_when_exact_result_cannot_be_guaranteed_to_fit() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = direct_shell_machine(root, bash, 2 * 1024 * 1024);
        let request = CallToolRequestParams::new("bash").with_arguments(
            serde_json::from_value::<serde_json::Map<String, Value>>(serde_json::json!({
                "command": "sleep 0.05; printf done",
                "timeout_secs": 3,
                "max_output_bytes": 1024 * 1024
            }))
            .unwrap(),
        );
        let response = machine
            .handle_shell_call_with(
                &request,
                Some(1),
                true,
                Some("native-unsafe".to_owned()),
                Some(crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let CallToolResponse::Complete(receipt) = response else {
            panic!("unsafe native task result ceiling must use compatibility handoff")
        };
        let text = receipt.content[0].as_text().unwrap().text.as_str();
        let receipt: Value = serde_json::from_str(text).unwrap();
        assert_eq!(receipt["async"], true);
        assert!(receipt["job_id"].as_str().is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn machine_wide_task_budget_protects_cross_transport_tasks() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut machine = direct_shell_machine(root, bash, 2 * 1024 * 1024);
        // Model a runtime shared with a bounded transport such as OpenAI. The
        // local caller itself supplies no result budget, but any native Task it
        // creates must still be fetchable through every active transport.
        machine.config.task_result_budget = Some(1024);
        let request = CallToolRequestParams::new("bash").with_arguments(
            serde_json::from_value::<serde_json::Map<String, Value>>(serde_json::json!({
                "command": "sleep 0.05; printf done",
                "timeout_secs": 3,
                "max_output_bytes": 4096
            }))
            .unwrap(),
        );
        let response = machine
            .handle_shell_call_with(&request, Some(1), true, None, None, None)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(response, CallToolResponse::Complete(_)),
            "machine-wide bounded transport must prevent an unsafe native Task"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn durable_shell_preflight_rejects_arguments_the_tool_schema_rejects() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = direct_shell_machine(root, bash, 16 * 1024);
        let shell = CallToolRequestParams::new("bash").with_arguments(
            serde_json::from_value::<serde_json::Map<String, Value>>(serde_json::json!({
                "command": "true",
                "unknown": true
            }))
            .unwrap(),
        );
        let error = machine
            .handle_shell_call_with(&shell, Some(1_000), false, None, None, None)
            .await
            .unwrap_err();
        assert!(error.message.contains("unknown field"), "{error:?}");

        let job_start = CallToolRequestParams::new("job_start").with_arguments(
            serde_json::from_value::<serde_json::Map<String, Value>>(serde_json::json!({
                "command": "true",
                "max_output_bytes": 4096
            }))
            .unwrap(),
        );
        let error = machine
            .handle_shell_call_with(&job_start, Some(1_000), true, None, None, None)
            .await
            .unwrap_err();
        assert!(error.message.contains("unknown field"), "{error:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_keeps_containment_until_inherited_pipes_finish() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let marker = root.join("descendant-must-not-run");
        let machine = direct_shell_machine(root, bash, 16 * 1024);
        let id = machine
            .start_shell_job(
                ShellArgs {
                    command: format!("(sleep 2; touch '{}') & printf done", marker.display()),
                    dir: ".".to_owned(),
                    stdin: None,
                    timeout_secs: Some(1),
                    max_output_bytes: Some(4096),
                },
                false,
                None,
            )
            .unwrap()
            .id;

        let status = loop {
            let status: Value = serde_json::from_str(&machine.jobs.status(&id).unwrap()).unwrap();
            if status["status"] != "working" {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(status["status"], "failed", "{status}");
        assert!(
            status["error"]
                .as_str()
                .is_some_and(|error| error.contains("collecting command output timed out")),
            "{status}"
        );
        assert_eq!(status["stdout"]["stored_bytes"], 4, "{status}");
        let partial = machine
            .jobs
            .output_page(&id, crate::jobs::JobStream::Stdout, 0, 4096, 4096, false)
            .unwrap();
        assert!(partial.contains("done"), "{partial}");
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(
            !marker.exists(),
            "descendant holding inherited pipes must remain contained through output collection"
        );
        machine.jobs.shutdown().await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepare_mount_target_dirs_treats_unix_socket_as_leaf() {
        use std::{ffi::OsStr, os::unix::net::UnixListener};

        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("daemon.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        let mut command = Command::new("true");
        prepare_mount_target_dirs(&mut command, &socket);
        let args: Vec<_> = command.as_std().get_args().collect();

        assert!(
            !args.windows(2).any(|window| {
                window[0] == OsStr::new("--dir") && window[1] == socket.as_os_str()
            }),
            "socket leaf itself must not be created as a directory"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mask_path_is_type_aware() {
        use std::{ffi::OsStr, os::unix::net::UnixListener};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let directory = root.join("directory");
        let socket = root.join("daemon.sock");
        std::fs::create_dir(&directory).unwrap();
        let _listener = UnixListener::bind(&socket).unwrap();

        let mut command = Command::new("true");
        mask_path(&mut command, &directory).unwrap();
        mask_path(&mut command, &socket).unwrap();
        let args: Vec<_> = command.as_std().get_args().collect();

        assert!(
            args.windows(2).any(|window| {
                window[0] == OsStr::new("--tmpfs") && window[1] == directory.as_os_str()
            }),
            "directories should be hidden with an inaccessible tmpfs mount"
        );
        assert!(
            args.windows(3).any(|window| {
                window[0] == OsStr::new("--ro-bind")
                    && window[1] == OsStr::new("/dev/null")
                    && window[2] == socket.as_os_str()
            }),
            "non-directories should be hidden behind /dev/null"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn masks_never_create_a_missing_path_or_follow_a_symlink_spelling() {
        use std::{ffi::OsStr, os::unix::fs::symlink};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let absent = root.join("private-state");
        let mut command = Command::new("true");
        mask_path(&mut command, &absent).unwrap();
        assert!(!absent.exists());
        assert_eq!(command.as_std().get_args().count(), 0);

        // A directory alias masks its target as a directory; a /dev/null bind
        // on the alias name would fail on the directory it resolves to.
        let target = root.join("target");
        let alias = root.join("alias");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &alias).unwrap();
        mask_path(&mut command, &alias).unwrap();
        let args: Vec<_> = command.as_std().get_args().collect();
        assert!(args.windows(2).any(|window| {
            window[0] == OsStr::new("--tmpfs") && window[1] == target.as_os_str()
        }));
        assert!(!args.contains(&alias.as_os_str()), "{args:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_aliases_become_links_to_one_canonical_bind() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let usr = root.join("usr");
        let usr_bin = usr.join("bin");
        let bin = root.join("bin");
        let profile = root.join("profile");
        std::fs::create_dir_all(&usr_bin).unwrap();
        symlink("usr/bin", &bin).unwrap();
        symlink(&bin, &profile).unwrap();

        let layout = RuntimeLayout::resolve(&[
            usr_bin.clone(),
            bin.clone(),
            usr.clone(),
            profile.clone(),
            root.join("missing"),
        ])
        .unwrap();
        // One bind per host object: `/usr` covers `/usr/bin`, and no alias is
        // bound under its own name, where masks on `/usr/bin` would not apply.
        assert_eq!(layout.binds, [usr]);
        assert_eq!(
            layout.links,
            [(bin, usr_bin.clone()), (profile, usr_bin.clone())]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bubblewrap_help_read_is_bounded_even_without_eof() {
        let exact = std::io::Cursor::new(vec![b'x'; MAX_BUBBLEWRAP_HELP_BYTES as usize]);
        assert_eq!(
            read_bubblewrap_help(exact).await.unwrap().len(),
            MAX_BUBBLEWRAP_HELP_BYTES as usize
        );
        let oversized = std::io::Cursor::new(vec![b'x'; MAX_BUBBLEWRAP_HELP_BYTES as usize + 1]);
        let error = read_bubblewrap_help(oversized).await.unwrap_err();
        assert_eq!(
            crate::recovery::Recovery::of(&error),
            crate::recovery::Recovery::Operator
        );
        assert!(error.to_string().contains("exceeded"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_feature_probe_matches_exact_option_tokens() {
        let help = "  --bind-fd FD DEST  Bind descriptor\n  --ro-bind-fd FD DEST  Read only\n  --dev-bind SRC DEST  Device\n";
        assert!(
            missing_bubblewrap_options(help, &["--bind-fd", "--ro-bind-fd", "--dev-bind"])
                .is_empty()
        );
        assert_eq!(
            missing_bubblewrap_options("  --bind-fd-extra FD DEST\n", &["--bind-fd"]),
            vec!["--bind-fd"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn constrained_runtime_alias_cannot_escape_its_trusted_root() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let trusted = root.join("trusted");
        let outside = root.join("outside");
        let alias = root.join("profile");
        std::fs::create_dir(&trusted).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, &alias).unwrap();
        let source = RuntimeSource::constrained(alias.clone(), trusted.clone());
        let error = RuntimeLayout::resolve_sources(std::slice::from_ref(&source)).unwrap_err();
        assert!(
            error.to_string().contains("escapes trusted root"),
            "{error}"
        );

        std::fs::remove_file(&alias).unwrap();
        let profile = trusted.join("profile");
        std::fs::create_dir(&profile).unwrap();
        symlink(&profile, &alias).unwrap();
        let layout =
            RuntimeLayout::resolve_sources(&[RuntimeSource::constrained(alias.clone(), trusted)])
                .unwrap();
        assert!(layout.links.contains(&(alias, profile)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_runtime_paths_include_available_nix_profiles_without_home_root() {
        let mounts = sandbox_runtime_sources(false, false)
            .into_iter()
            .map(|source| source.path)
            .collect::<Vec<_>>();

        for expected in [
            PathBuf::from("/nix/store"),
            PathBuf::from("/run/current-system"),
            PathBuf::from("/etc/profiles"),
            PathBuf::from("/nix/var/nix/profiles"),
        ] {
            if expected.exists() {
                assert!(
                    mounts.contains(&expected),
                    "expected runtime mount {}",
                    expected.display()
                );
            }
        }

        if let Some(home) = env::var_os("HOME") {
            let home = PathBuf::from(home);
            let profile = home.join(".nix-profile");
            if profile.exists() {
                assert!(mounts.contains(&profile));
                let source = sandbox_runtime_sources(false, false)
                    .into_iter()
                    .find(|source| source.path == profile)
                    .unwrap();
                assert_eq!(
                    source.target,
                    RuntimeTargetPolicy::Beneath(PathBuf::from("/nix/store"))
                );
            }
            assert!(
                !mounts.contains(&home),
                "sandbox runtime mounts must not expose the whole home directory"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_mounts_follow_additive_policy_and_deny() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let writable = root.join("writable");
        let denied = root.join("denied");
        std::fs::create_dir(&writable).unwrap();
        std::fs::create_dir(&denied).unwrap();

        let access = AccessPolicy::from_spec(AccessSpec {
            base_dir: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![writable.clone()],
            deny_read_roots: vec![denied.clone()],
            deny_write_roots: vec![denied.clone()],
            unrestricted_fs: false,
        })
        .unwrap();

        let mounts = access.sandbox_mounts();
        assert!(mounts.contains(&(root.clone(), false)));
        assert!(mounts.contains(&(writable, true)));
        assert!(!mounts.iter().any(|(path, _)| path == &denied));
    }

    /// A sandboxed machine whose only grants are `root`, for command-shape
    /// assertions that must not depend on the host's bwrap.
    #[cfg(target_os = "linux")]
    fn sandbox_machine(root: &Path, deny_write: Vec<PathBuf>) -> LocalMachine {
        LocalMachine {
            config: RuntimeConfig {
                access: AccessPolicy::from_spec(AccessSpec {
                    base_dir: root.to_path_buf(),
                    read_roots: vec![root.to_path_buf()],
                    write_roots: vec![root.to_path_buf()],
                    deny_read_roots: Vec::new(),
                    deny_write_roots: deny_write,
                    unrestricted_fs: false,
                })
                .unwrap(),
                cache_mounts: Vec::new(),
                shell_env: Vec::new(),
                host_shell: crate::host_services::HostShell::default(),
                log: LogConfig::default(),
                allow_shell: true,
                sandbox_shell: true,
                allow_network: false,
                max_shell_timeout_secs: 5,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,

                task_result_budget: None,
                protected_paths: Vec::new(),
            },
            launchers: PreparedLaunchers::for_test(
                Some(PathBuf::from("/bin/bash")),
                Some(PathBuf::from("/bin/bwrap")),
            ),

            jobs: crate::jobs::JobManager::new(std::time::Duration::from_secs(
                crate::limits::DEFAULT_JOB_RETENTION_SECS,
            ))
            .unwrap(),
            operation_gate: Arc::new(RwLock::new(())),
            transport_context_token: TransportContextToken::from("test-transport-context"),
            router: std::sync::OnceLock::new(),
            serialize: false,
            secure_fs: Arc::new(
                crate::secure_fs::SecureFs::new(&[root.to_path_buf()], &[root.to_path_buf()])
                    .unwrap(),
            ),
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn final_retirement_discards_hidden_synchronous_output_after_guard_retarget() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let root = workspace.canonicalize().unwrap();
        let first = root.join("first");
        let second = root.join("second");
        let alias = temp.path().join("deny-link");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        symlink(&first, &alias).unwrap();

        let mut machine = sandbox_machine(&root, Vec::new());
        machine.config.access.deny_read_roots =
            crate::mcp::guard_identities(&root, std::slice::from_ref(&alias)).unwrap();
        let generation = machine
            .current_shell_guard_generation()
            .unwrap()
            .expect("sandboxed Linux shell has a live guard generation");

        let request_key = "retirement-guard-revocation".to_owned();
        let started = machine
            .jobs
            .spawn(30, 4096, Some(request_key.clone()), move |job| async move {
                assert!(job.mark_running(Some(generation)));
                std::fs::write(&job.stdout_path, b"secret").unwrap();
                crate::jobs::JobCompletion::completed(
                    Some(0),
                    true,
                    crate::jobs::StreamSummary {
                        total_bytes: 6,
                        stored_bytes: 6,
                        truncated: false,
                    },
                    crate::jobs::StreamSummary::default(),
                )
            })
            .unwrap();
        let id = started.id.clone();
        assert!(matches!(
            machine.jobs.wait_until(&id, None).await.unwrap(),
            crate::jobs::JobWait::Terminal
        ));
        assert_eq!(
            machine.jobs.mark_synchronous_result(&id).unwrap(),
            crate::jobs::JobDisposition::Synchronous
        );
        drop(started);

        std::fs::remove_file(&alias).unwrap();
        symlink(&second, &alias).unwrap();
        assert!(
            !machine.shell_guard_generation_is_current(Some(generation)),
            "retargeting a live deny alias must revoke the launch generation"
        );

        assert_eq!(
            machine.retire_job_request_delivery(&request_key),
            crate::jobs::RequestRetirement::Retired
        );
        assert!(machine.jobs.request_job_id(&request_key).is_none());
        assert!(
            machine.jobs.status(&id).is_err(),
            "revoked hidden synchronous output must be discarded, not promoted to compatibility"
        );
        let listed: serde_json::Value = serde_json::from_str(&machine.jobs.list(64)).unwrap();
        assert!(
            listed["jobs"].as_array().unwrap().is_empty(),
            "revoked hidden output must never become discoverable through job_list"
        );
        machine.jobs.shutdown().await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_materializes_sealed_shell_with_ro_bind_data() {
        use std::ffi::OsStr;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = sandbox_machine(&root, Vec::new());
        let command = machine
            .bubblewrap_command(Path::new("/bin/bash"), &root, "true")
            .unwrap();
        let args = command.as_std().get_args().collect::<Vec<_>>();
        assert!(args.windows(5).any(|window| {
            window[0] == OsStr::new("--perms")
                && window[1] == OsStr::new("0500")
                && window[2] == OsStr::new("--ro-bind-data")
                && window[3].to_string_lossy().parse::<i32>().is_ok()
                && window[4] == OsStr::new("/bin/bash")
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_refuses_a_writable_protected_symlink_entry() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let target = root.join("runtime.key");
        let alias = root.join("credential-link");
        std::fs::write(&target, b"secret").unwrap();
        symlink(&target, &alias).unwrap();

        let mut machine = sandbox_machine(&root, Vec::new());
        machine.config.protected_paths = vec![alias.clone(), target.clone()];
        let error = machine
            .bubblewrap_command(Path::new("/bin/bash"), &root, "true")
            .expect_err("a writable protected symlink must stop sandbox startup");
        assert!(
            error
                .to_string()
                .contains("not an existing canonical object"),
            "{error}"
        );
        assert!(alias.is_symlink());
        assert_eq!(std::fs::read(target).unwrap(), b"secret");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn broad_writable_home_uses_namespace_anchors_for_private_directory() {
        use std::ffi::OsStr;

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let config = home.join(".config");
        let abird = config.join("abird");
        let protected = abird.join("dotlink");
        let runtime_key = protected.join("runtime.key");
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::write(&runtime_key, b"secret").unwrap();

        let mut machine = sandbox_machine(&home, Vec::new());
        // Mirror normal setup: the stable private directory plus a redundant
        // active credential file beneath it are both protected.
        machine.config.protected_paths = vec![protected.clone(), runtime_key];
        let command = machine
            .bubblewrap_command(Path::new("/bin/bash"), &home, "true")
            .expect("a broad writable home should be anchorable");
        let args = command.as_std().get_args().collect::<Vec<_>>();

        for anchor in [&config, &abird] {
            assert!(
                args.windows(3).any(|window| {
                    window[0] == OsStr::new("--bind-fd") && window[2] == anchor.as_os_str()
                }),
                "missing writable namespace anchor for {}: {args:?}",
                anchor.display()
            );
        }
        assert!(
            args.windows(2).any(|window| {
                window[0] == OsStr::new("--tmpfs") && window[1] == protected.as_os_str()
            }),
            "protected directory must remain masked: {args:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn anchored_private_directory_stays_hidden_while_broad_home_remains_writable() {
        if std::env::var_os("DOTLINK_TEST_BWRAP").is_none() {
            return;
        }
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let Some(bwrap) = resolve_executable("bwrap") else {
            return;
        };

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let config = home.join(".config");
        let abird = config.join("abird");
        let protected = abird.join("dotlink");
        let runtime_key = protected.join("runtime.key");
        let safe = abird.join("safe.txt");
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::write(&runtime_key, b"TOP-SECRET\n").unwrap();

        let mut machine = sandbox_machine(&home, Vec::new());
        machine.launchers = PreparedLaunchers::for_test(Some(bash.clone()), Some(bwrap));
        machine.config.protected_paths = vec![protected.clone(), runtime_key.clone()];
        let script = format!(
            "set +e; printf safe > '{}'; \
             mv '{}' '{}-moved' >/dev/null 2>&1; echo abird_rc=$?; \
             mv '{}' '{}-moved' >/dev/null 2>&1; echo config_rc=$?; \
             cat '{}' 2>/dev/null; echo secret_rc=$?",
            safe.display(),
            abird.display(),
            abird.display(),
            config.display(),
            config.display(),
            runtime_key.display(),
        );
        let mut command = machine
            .bubblewrap_command(&bash, &home, &script)
            .expect("anchored broad-home sandbox should build");
        let output = command.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("abird_rc=1"), "{stdout}");
        assert!(stdout.contains("config_rc=1"), "{stdout}");
        assert!(stdout.contains("secret_rc=1"), "{stdout}");
        assert!(!stdout.contains("TOP-SECRET"), "{stdout}");
        assert_eq!(std::fs::read(&safe).unwrap(), b"safe");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_anchors_writable_ancestor_of_protected_file() {
        use std::ffi::OsStr;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let container = root.join("container");
        let protected = container.join("runtime.key");
        std::fs::create_dir(&container).unwrap();
        std::fs::write(&protected, b"secret").unwrap();

        let mut machine = sandbox_machine(&root, Vec::new());
        machine.config.protected_paths = vec![protected.clone()];
        let command = machine
            .bubblewrap_command(Path::new("/bin/bash"), &root, "true")
            .expect("an existing protected file can be stabilized by its parent anchor");
        let args = command.as_std().get_args().collect::<Vec<_>>();
        assert!(args.windows(3).any(|window| {
            window[0] == OsStr::new("--bind-fd") && window[2] == container.as_os_str()
        }));
        assert!(args.windows(3).any(|window| {
            window[0] == OsStr::new("--ro-bind")
                && window[1] == OsStr::new("/dev/null")
                && window[2] == protected.as_os_str()
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_anchors_nested_boundary_below_writable_ancestor() {
        use std::ffi::OsStr;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let container = root.join("container");
        let locked = container.join("locked");
        let protected = locked.join("runtime.key");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(&protected, b"secret").unwrap();

        let mut machine = sandbox_machine(&root, vec![locked.clone()]);
        machine.config.protected_paths = vec![protected.clone()];
        let command = machine
            .bubblewrap_command(Path::new("/bin/bash"), &root, "true")
            .expect("nested canonical guards should be lifetime-anchorable");
        let args = command.as_std().get_args().collect::<Vec<_>>();
        assert!(args.windows(3).any(|window| {
            window[0] == OsStr::new("--bind-fd") && window[2] == container.as_os_str()
        }));
        assert!(args.windows(3).any(|window| {
            window[0] == OsStr::new("--ro-bind-fd") && window[2] == locked.as_os_str()
        }));
        assert!(args.windows(3).any(|window| {
            window[0] == OsStr::new("--ro-bind")
                && window[1] == OsStr::new("/dev/null")
                && window[2] == protected.as_os_str()
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_rejects_missing_protected_child_in_host_visible_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let locked = root.join("locked");
        let missing = locked.join("runtime.key");
        std::fs::create_dir(&locked).unwrap();

        let mut machine = sandbox_machine(&root, vec![locked]);
        machine.config.protected_paths = vec![missing];
        let error = machine
            .bubblewrap_command(Path::new("/bin/bash"), &root, "true")
            .expect_err("a host process can populate a missing guard while a shell runs");
        assert!(
            error.to_string().contains("cannot be lifetime-anchored"),
            "{error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_rejects_protected_symlink_even_under_read_only_rebind() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let locked = root.join("locked");
        let target = locked.join("runtime.key");
        let alias = locked.join("credential-link");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(&target, b"secret").unwrap();
        symlink(&target, &alias).unwrap();

        let mut machine = sandbox_machine(&root, vec![locked]);
        machine.config.protected_paths = vec![alias, target];
        let error = machine
            .bubblewrap_command(Path::new("/bin/bash"), &root, "true")
            .expect_err("read-only mounts remain mutable by external host processes");
        assert!(
            error
                .to_string()
                .contains("not an existing canonical object"),
            "{error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_revalidates_a_protected_symlink_after_startup() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let cache = temp.path().join("cache");
        let safe = root.join("safe");
        let alias = root.join("protected-link");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&cache).unwrap();
        std::fs::create_dir(&safe).unwrap();
        symlink(&safe, &alias).unwrap();

        let mut machine = sandbox_machine(&root, Vec::new());
        machine.config.access.write_roots.clear();
        machine.config.cache_mounts = vec![PreparedCacheMount::for_test(
            cache.clone(),
            PathBuf::from("/tmp/home/.cache/shared"),
            true,
        )];
        machine.config.protected_paths = vec![alias.clone(), safe];
        let initial = machine
            .shell_guards()
            .err()
            .expect("the protected path is visible under the read-only grant");
        assert!(
            initial
                .to_string()
                .contains("overlaps readable shell mount"),
            "{initial}"
        );

        std::fs::remove_file(&alias).unwrap();
        symlink(&cache, &alias).unwrap();
        let error = machine
            .bubblewrap_command(Path::new("/bin/bash"), &root, "true")
            .expect_err("the current symlink target must be checked on every command");
        assert!(
            error.to_string().contains("cache mount")
                || error.to_string().contains("readable shell mount"),
            "{error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_mount_rejects_protected_content_even_without_a_user_grant() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let protected = sandbox_runtime_sources(false, false)
            .into_iter()
            .map(|source| source.path)
            .collect::<Vec<_>>()
            .into_iter()
            .find(|path| {
                path.canonicalize()
                    .is_ok_and(|canonical| canonical == *path)
            })
            .expect("the shell requires at least one canonical runtime path");
        let mut machine = sandbox_machine(&root, Vec::new());
        machine.config.protected_paths = vec![protected.clone()];
        assert!(!machine.config.access.grant_read(&protected));
        assert!(!machine.config.access.grant_write(&protected));
        let error = machine
            .bubblewrap_command(Path::new("/bin/bash"), &root, "true")
            .unwrap_err();
        assert!(error.to_string().contains("runtime mount"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn implicit_runtime_mount_cannot_expose_a_recreated_private_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let allowed = root.join("allowed");
        let profile = root.join("profile");
        std::fs::create_dir(&allowed).unwrap();
        std::fs::create_dir(&profile).unwrap();
        let guard = profile.join("config.jsonc");
        std::fs::write(&guard, b"private").unwrap();
        let mut machine = sandbox_machine(&allowed, Vec::new());
        machine.config.access.write_roots.clear();
        let runtime = RuntimeLayout::resolve(std::slice::from_ref(&profile)).unwrap();
        for exists in [true, false, true] {
            if exists {
                std::fs::write(&guard, b"private").unwrap();
            } else {
                std::fs::remove_file(&guard).unwrap();
            }
            let error = crate::mcp::validate_shell_guards(
                &machine.config.access,
                &[],
                std::slice::from_ref(&guard),
                &runtime,
            )
            .unwrap_err();
            assert!(error.to_string().contains("runtime mount"), "{error}");
        }
        // A mount that is disjoint from the protected object remains usable.
        let safe = RuntimeLayout::resolve(std::slice::from_ref(&allowed)).unwrap();
        crate::mcp::validate_shell_guards(&machine.config.access, &[], &[guard], &safe).unwrap();
    }

    /// Existing canonical deny-write files/directories become stable read-only
    /// mountpoints beneath broad writable grants; missing/symlink names still fail.
    #[cfg(target_os = "linux")]
    #[test]
    fn deny_write_files_and_directories_are_anchored_read_only() {
        use std::ffi::OsStr;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let locked_dir = root.join("locked-dir");
        let locked_file = root.join(".env");
        std::fs::create_dir(&locked_dir).unwrap();
        std::fs::write(&locked_file, b"secret").unwrap();

        let machine = sandbox_machine(&root, vec![locked_dir.clone(), locked_file.clone()]);
        let command = machine
            .bubblewrap_command(Path::new("/bin/bash"), &root, "true")
            .expect("existing deny-write objects should be safely re-bound read-only");
        let args = command.as_std().get_args().collect::<Vec<_>>();
        for guarded in [&locked_dir, &locked_file] {
            assert!(
                args.windows(3).any(|window| {
                    window[0] == OsStr::new("--ro-bind-fd") && window[2] == guarded.as_os_str()
                }),
                "missing read-only guard bind for {}: {args:?}",
                guarded.display()
            );
        }
    }

    #[tokio::test]
    async fn filesystem_denies_override_cache_grants() {
        let project = tempfile::tempdir().unwrap();
        let caches = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        let denied_read = caches.path().join("deny-read");
        let denied_write = caches.path().join("deny-write");
        let read_cache_ancestor = caches.path().join("read-cache-ancestor");
        let read_denied_descendant = read_cache_ancestor.join("private");
        let read_denied_ancestor = caches.path().join("read-denied-ancestor");
        let read_cache_descendant = read_denied_ancestor.join("cache");
        let write_cache_ancestor = caches.path().join("write-cache-ancestor");
        let write_denied_descendant = write_cache_ancestor.join("locked");
        let write_denied_ancestor = caches.path().join("write-denied-ancestor");
        let write_cache_descendant = write_denied_ancestor.join("cache");
        for path in [
            &denied_read,
            &denied_write,
            &read_denied_descendant,
            &read_cache_descendant,
            &write_denied_descendant,
            &write_cache_descendant,
        ] {
            std::fs::create_dir_all(path).unwrap();
        }

        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root],
                write_roots: Vec::new(),
                deny_read_roots: vec![
                    denied_read.clone(),
                    read_denied_descendant,
                    read_denied_ancestor,
                ],
                deny_write_roots: vec![
                    denied_write.clone(),
                    write_denied_descendant,
                    write_denied_ancestor,
                ],
                unrestricted_fs: false,
            },
            cache_mounts: vec![
                SandboxCacheMount {
                    source: denied_read,
                    target: PathBuf::from("/tmp/home/.cache/read-denied"),
                    writable: true,
                },
                SandboxCacheMount {
                    source: read_cache_ancestor,
                    target: PathBuf::from("/tmp/home/.cache/read-ancestor"),
                    writable: true,
                },
                SandboxCacheMount {
                    source: read_cache_descendant,
                    target: PathBuf::from("/tmp/home/.cache/read-descendant"),
                    writable: true,
                },
                SandboxCacheMount {
                    source: denied_write,
                    target: PathBuf::from("/tmp/home/.cache/write-denied"),
                    writable: true,
                },
                SandboxCacheMount {
                    source: write_cache_ancestor,
                    target: PathBuf::from("/tmp/home/.cache/write-ancestor"),
                    writable: true,
                },
                SandboxCacheMount {
                    source: write_cache_descendant,
                    target: PathBuf::from("/tmp/home/.cache/write-descendant"),
                    writable: true,
                },
            ],
            shell_env: Vec::new(),
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 5,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();

        assert_eq!(machine.config.cache_mounts.len(), 3);
        assert_eq!(
            machine
                .config
                .cache_mounts
                .iter()
                .map(|mount| mount.target.clone())
                .collect::<Vec<_>>(),
            [
                PathBuf::from("/tmp/home/.cache/write-denied"),
                PathBuf::from("/tmp/home/.cache/write-ancestor"),
                PathBuf::from("/tmp/home/.cache/write-descendant"),
            ]
        );
        assert!(
            machine
                .config
                .cache_mounts
                .iter()
                .all(|mount| !mount.writable),
            "deny-write overlap in either direction must downgrade caches to read-only"
        );
    }

    #[tokio::test]
    async fn protected_cache_source_is_not_exposed_at_a_remapped_target() {
        let project = tempfile::tempdir().unwrap();
        let caches = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        let cache_root = caches.path().canonicalize().unwrap();
        let protected = cache_root.join("credentials.json");
        std::fs::write(&protected, b"secret").unwrap();

        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root],
                write_roots: Vec::new(),
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: vec![SandboxCacheMount {
                source: cache_root,
                target: PathBuf::from("/tmp/home/.cache/remapped"),
                writable: false,
            }],
            shell_env: Vec::new(),
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 5,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: vec![protected],
        })
        .await
        .unwrap();

        assert!(
            machine.config.cache_mounts.is_empty(),
            "a cache ancestor must not remap a protected descendant"
        );
    }

    #[tokio::test]
    async fn cache_mounts_do_not_expand_mcp_filesystem_access() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        let cache_root = cache.path().canonicalize().unwrap();

        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root],
                write_roots: Vec::new(),
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: vec![SandboxCacheMount {
                source: cache_root.clone(),
                target: PathBuf::from("/tmp/home/.cargo/registry"),
                writable: true,
            }],
            shell_env: vec![("CARGO_HOME".to_owned(), "/tmp/home/.cargo".to_owned())],
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 5,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();

        assert!(
            machine
                .resolve_existing(cache_root.to_str().unwrap(), AccessNeed::Read)
                .await
                .is_err(),
            "cache sharing must not grant MCP read access to the host cache"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn prepared_cache_mount_keeps_the_original_directory_after_path_replacement() {
        use std::os::unix::fs::MetadataExt as _;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let cache = root.join("cache");
        let old = root.join("cache-old");
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(cache.join("original"), b"old").unwrap();
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root.clone()],
                write_roots: Vec::new(),
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: vec![SandboxCacheMount {
                source: cache.clone(),
                target: PathBuf::from("/tmp/home/.cache/test"),
                writable: true,
            }],
            shell_env: Vec::new(),
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 5,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,
            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,
            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let pinned_before = machine.config.cache_mounts[0].pinned.metadata().unwrap();
        std::fs::rename(&cache, &old).unwrap();
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(cache.join("replacement"), b"new").unwrap();
        let replacement = std::fs::metadata(&cache).unwrap();
        let pinned_after = machine.config.cache_mounts[0].pinned.metadata().unwrap();
        assert_eq!(pinned_before.dev(), pinned_after.dev());
        assert_eq!(pinned_before.ino(), pinned_after.ino());
        assert_ne!(pinned_after.ino(), replacement.ino());
        let error = machine
            .shell_guards()
            .err()
            .expect("replaced cache source must fail closed");
        assert!(
            error.to_string().contains("cache source changed"),
            "{error}"
        );

        std::fs::remove_dir_all(&cache).unwrap();
        std::os::unix::fs::symlink(&old, &cache).unwrap();
        let error = machine
            .shell_guards()
            .err()
            .expect("symlink-back cache source must also fail closed");
        assert!(
            error.to_string().contains("cache source changed"),
            "{error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_cache_mounts_preserve_ro_rw_and_cache_env() {
        use std::ffi::OsStr;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let read_only = root.join("cache-ro");
        let read_write = root.join("cache-rw");
        std::fs::create_dir(&read_only).unwrap();
        std::fs::create_dir(&read_write).unwrap();

        let machine = LocalMachine {
            config: RuntimeConfig {
                access: AccessPolicy::from_spec(AccessSpec {
                    base_dir: root.clone(),
                    read_roots: vec![root.clone()],
                    write_roots: Vec::new(),
                    deny_read_roots: Vec::new(),
                    deny_write_roots: Vec::new(),
                    unrestricted_fs: false,
                })
                .unwrap(),
                cache_mounts: vec![
                    PreparedCacheMount::for_test(
                        read_only.clone(),
                        PathBuf::from("/tmp/home/.cargo/registry"),
                        false,
                    ),
                    PreparedCacheMount::for_test(
                        read_write.clone(),
                        PathBuf::from("/tmp/home/.cargo/git"),
                        true,
                    ),
                ],
                shell_env: vec![("CARGO_HOME".to_owned(), "/tmp/home/.cargo".to_owned())],
                #[cfg(target_os = "linux")]
                host_shell: crate::host_services::HostShell::default(),
                log: LogConfig::default(),
                allow_shell: true,
                sandbox_shell: true,
                allow_network: false,
                max_shell_timeout_secs: 5,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,

                task_result_budget: None,
                protected_paths: Vec::new(),
            },
            launchers: PreparedLaunchers::for_test(
                Some(PathBuf::from("/bin/bash")),
                Some(PathBuf::from("/bin/bwrap")),
            ),

            jobs: crate::jobs::JobManager::new(std::time::Duration::from_secs(
                crate::limits::DEFAULT_JOB_RETENTION_SECS,
            ))
            .unwrap(),
            operation_gate: Arc::new(RwLock::new(())),
            transport_context_token: TransportContextToken::from("test-transport-context"),
            router: std::sync::OnceLock::new(),
            serialize: false,
            #[cfg(target_os = "linux")]
            secure_fs: Arc::new(
                crate::secure_fs::SecureFs::new(std::slice::from_ref(&root), &[]).unwrap(),
            ),
        };

        let command = machine
            .bubblewrap_command(Path::new("/bin/bash"), &root, "true")
            .unwrap();
        let args: Vec<_> = command.as_std().get_args().collect();

        assert!(args.windows(3).any(|window| {
            window[0] == OsStr::new("--ro-bind-fd")
                && window[2] == OsStr::new("/tmp/home/.cargo/registry")
        }));
        assert!(args.windows(3).any(|window| {
            window[0] == OsStr::new("--bind-fd") && window[2] == OsStr::new("/tmp/home/.cargo/git")
        }));
        assert!(args.windows(3).any(|window| {
            window[0] == OsStr::new("--setenv")
                && window[1] == OsStr::new("CARGO_HOME")
                && window[2] == OsStr::new("/tmp/home/.cargo")
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_blocks_network_by_default_and_masks_denies() {
        use std::ffi::OsStr;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let denied = root.join("denied");
        let allowed = root.join("allowed");
        std::fs::create_dir(&denied).unwrap();
        std::fs::create_dir(&allowed).unwrap();

        let machine = LocalMachine {
            config: RuntimeConfig {
                access: AccessPolicy::from_spec(AccessSpec {
                    base_dir: root.clone(),
                    read_roots: vec![allowed.clone()],
                    write_roots: Vec::new(),
                    deny_read_roots: vec![denied.clone()],
                    deny_write_roots: vec![denied.clone()],
                    unrestricted_fs: false,
                })
                .unwrap(),
                cache_mounts: Vec::new(),
                shell_env: Vec::new(),
                #[cfg(target_os = "linux")]
                host_shell: crate::host_services::HostShell::default(),
                log: LogConfig::default(),
                allow_shell: true,
                sandbox_shell: true,
                allow_network: false,
                max_shell_timeout_secs: 5,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,

                task_result_budget: None,
                protected_paths: Vec::new(),
            },
            launchers: PreparedLaunchers::for_test(
                Some(PathBuf::from("/bin/bash")),
                Some(PathBuf::from("/bin/bwrap")),
            ),

            jobs: crate::jobs::JobManager::new(std::time::Duration::from_secs(
                crate::limits::DEFAULT_JOB_RETENTION_SECS,
            ))
            .unwrap(),
            operation_gate: Arc::new(RwLock::new(())),
            transport_context_token: TransportContextToken::from("test-transport-context"),
            router: std::sync::OnceLock::new(),
            serialize: false,
            #[cfg(target_os = "linux")]
            secure_fs: Arc::new(
                crate::secure_fs::SecureFs::new(std::slice::from_ref(&allowed), &[]).unwrap(),
            ),
        };

        let command = machine
            .bubblewrap_command(Path::new("/bin/bash"), &allowed, "true")
            .unwrap();
        let args: Vec<_> = command.as_std().get_args().collect();

        assert!(args.iter().any(|arg| *arg == OsStr::new("--unshare-net")));

        for endpoint in [
            Path::new("/nix/var/nix/daemon-socket"),
            Path::new("/run/nix-daemon"),
        ] {
            // Masks land on the canonical object a spelling names.
            let Ok(endpoint) = endpoint.canonicalize() else {
                continue;
            };
            let metadata = std::fs::metadata(&endpoint).unwrap();

            if metadata.is_dir() {
                assert!(
                    args.windows(2).any(|window| {
                        window[0] == OsStr::new("--tmpfs") && window[1] == endpoint.as_os_str()
                    }),
                    "Nix daemon endpoint directory should be hidden when network is denied"
                );
            } else {
                assert!(
                    args.windows(3).any(|window| {
                        window[0] == OsStr::new("--ro-bind")
                            && window[1] == OsStr::new("/dev/null")
                            && window[2] == endpoint.as_os_str()
                    }),
                    "Nix daemon endpoint should be hidden when network is denied"
                );
            }
        }

        assert!(
            args.windows(2).any(|window| {
                window[0] == OsStr::new("--tmpfs") && window[1] == denied.as_os_str()
            }),
            "denied directory should be hidden behind a tmpfs mount"
        );
        assert!(
            args.windows(3).any(|window| {
                window[0] == OsStr::new("--chmod")
                    && window[1] == OsStr::new("000")
                    && window[2] == denied.as_os_str()
            }),
            "denied directory mask should be mode 000"
        );

        let mut networked = machine.clone();
        networked.config.allow_network = true;
        let command = networked
            .bubblewrap_command(Path::new("/bin/bash"), &allowed, "true")
            .unwrap();
        assert!(
            !command
                .as_std()
                .get_args()
                .any(|arg| arg == OsStr::new("--unshare-net"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_timeout_includes_output_collection_from_descendants() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = LocalMachine {
            config: RuntimeConfig {
                access: AccessPolicy::from_spec(AccessSpec {
                    base_dir: root.clone(),
                    read_roots: vec![root],
                    write_roots: Vec::new(),
                    deny_read_roots: Vec::new(),
                    deny_write_roots: Vec::new(),
                    unrestricted_fs: false,
                })
                .unwrap(),
                cache_mounts: Vec::new(),
                shell_env: Vec::new(),
                #[cfg(target_os = "linux")]
                host_shell: crate::host_services::HostShell::default(),
                log: LogConfig::default(),
                allow_shell: true,
                sandbox_shell: false,
                allow_network: true,
                max_shell_timeout_secs: 2,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,

                task_result_budget: None,
                protected_paths: Vec::new(),
            },
            launchers: PreparedLaunchers::for_test(Some(bash), None),

            jobs: crate::jobs::JobManager::new(std::time::Duration::from_secs(
                crate::limits::DEFAULT_JOB_RETENTION_SECS,
            ))
            .unwrap(),
            operation_gate: Arc::new(RwLock::new(())),
            transport_context_token: TransportContextToken::from("test-transport-context"),
            router: std::sync::OnceLock::new(),
            serialize: false,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            secure_fs: Arc::new(crate::secure_fs::SecureFs::new(&[], &[]).unwrap()),
        };

        let started = Instant::now();
        let result = machine
            .execute_shell(
                ShellArgs {
                    // The background process inherits the captured pipes after
                    // Bash exits, so output collection must share the same
                    // absolute deadline as the command itself.
                    command: "sleep 2 & printf done".to_owned(),
                    dir: ".".to_owned(),
                    stdin: None,
                    timeout_secs: Some(1),
                    max_output_bytes: Some(4096),
                },
                false,
                None,
            )
            .await
            .unwrap();

        assert!(started.elapsed() < Duration::from_millis(1800));
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(
            serialized.contains("collecting command output timed out after 1 seconds"),
            "unexpected shell result: {serialized}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_timeout_returns_tool_error_with_partial_output() {
        let Some(bash) = resolve_executable("bash") else {
            return;
        };

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = LocalMachine {
            config: RuntimeConfig {
                access: AccessPolicy::from_spec(AccessSpec {
                    base_dir: root.clone(),
                    read_roots: vec![root],
                    write_roots: Vec::new(),
                    deny_read_roots: Vec::new(),
                    deny_write_roots: Vec::new(),
                    unrestricted_fs: false,
                })
                .unwrap(),
                cache_mounts: Vec::new(),
                shell_env: Vec::new(),
                #[cfg(target_os = "linux")]
                host_shell: crate::host_services::HostShell::default(),
                log: LogConfig::default(),
                allow_shell: true,
                sandbox_shell: false,
                allow_network: true,
                max_shell_timeout_secs: 2,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,

                task_result_budget: None,
                protected_paths: Vec::new(),
            },
            launchers: PreparedLaunchers::for_test(Some(bash), None),

            jobs: crate::jobs::JobManager::new(std::time::Duration::from_secs(
                crate::limits::DEFAULT_JOB_RETENTION_SECS,
            ))
            .unwrap(),
            operation_gate: Arc::new(RwLock::new(())),
            transport_context_token: TransportContextToken::from("test-transport-context"),
            router: std::sync::OnceLock::new(),
            serialize: false,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            secure_fs: Arc::new(crate::secure_fs::SecureFs::new(&[], &[]).unwrap()),
        };

        let result = machine
            .execute_shell(
                ShellArgs {
                    command: "printf 'partial output\\n'; sleep 30".to_owned(),
                    dir: ".".to_owned(),
                    stdin: None,
                    timeout_secs: Some(1),
                    max_output_bytes: Some(4096),
                },
                false,
                None,
            )
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(true));
        let serialized = serde_json::to_value(&result).unwrap();
        let text = serialized["content"][0]["text"].as_str().unwrap();
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["timed_out"], Value::Bool(true));
        assert_eq!(payload["success"], Value::Bool(false));
        assert_eq!(payload["exit_code"], Value::Null);
        assert!(
            payload["error"]
                .as_str()
                .unwrap()
                .contains("timed out after 1 seconds"),
            "{payload}"
        );
        assert!(
            payload["stdout"]
                .as_str()
                .unwrap()
                .contains("partial output"),
            "{payload}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bubblewrap_cache_runtime_smoke_when_requested() {
        if std::env::var_os("DOTLINK_TEST_BWRAP").is_none() {
            return;
        }

        let Some(bwrap) = resolve_executable("bwrap") else {
            panic!("DOTLINK_TEST_BWRAP requested but bwrap is not installed");
        };
        let Some(bash) = resolve_executable("bash") else {
            panic!("DOTLINK_TEST_BWRAP requested but bash is not installed");
        };

        let project = tempfile::tempdir().unwrap();
        let caches = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        let read_only = caches.path().join("registry");
        let read_write = caches.path().join("git");
        std::fs::create_dir(&read_only).unwrap();
        std::fs::create_dir(&read_write).unwrap();
        std::fs::write(read_only.join("cached.txt"), b"cached").unwrap();

        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root],
                write_roots: Vec::new(),
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: vec![
                SandboxCacheMount {
                    source: read_only.clone(),
                    target: PathBuf::from("/tmp/home/.cargo/registry"),
                    writable: false,
                },
                SandboxCacheMount {
                    source: read_write.clone(),
                    target: PathBuf::from("/tmp/home/.cargo/git"),
                    writable: true,
                },
            ],
            shell_env: vec![("CARGO_HOME".to_owned(), "/tmp/home/.cargo".to_owned())],
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: true,
            allow_network: false,
            max_shell_timeout_secs: 10,
            max_output_bytes: 16 * 1024,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();

        // Use the executable paths resolved by LocalMachine::new.
        assert_eq!(
            machine.launchers.shell.as_ref().map(|value| value.path()),
            Some(bash.as_path())
        );
        assert_eq!(
            machine.launchers.bwrap.as_ref().map(|value| value.path()),
            Some(bwrap.as_path())
        );

        let result = machine
            .execute_shell(
                ShellArgs {
                    command: concat!(
                        "set -eu; ",
                        "test \"$CARGO_HOME\" = /tmp/home/.cargo; ",
                        "cat \"$CARGO_HOME/registry/cached.txt\" >/dev/null; ",
                        "if touch \"$CARGO_HOME/registry/blocked\" >/dev/null 2>&1; then exit 21; fi; ",
                        "touch \"$CARGO_HOME/git/works\""
                    )
                    .to_owned(),
                    dir: ".".to_owned(),
                    stdin: None,
                    timeout_secs: Some(10),
                    max_output_bytes: Some(16 * 1024),
                },
                false, None)
            .await
            .unwrap();

        let serialized = serde_json::to_string(&result).unwrap();
        assert!(
            serialized.contains("\\\"exit_code\\\":0"),
            "cache sandbox smoke failed: {serialized}"
        );
        assert!(!read_only.join("blocked").exists());
        assert!(read_write.join("works").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bubblewrap_uses_pinned_mount_sources_after_symlink_swap() {
        use std::os::unix::fs::symlink;
        if std::env::var_os("DOTLINK_TEST_BWRAP").is_none() {
            return;
        }
        let bash = resolve_executable("bash").expect("bash required for sandbox test");
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("granted");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(root.join("marker"), "allowed").unwrap();
        std::fs::write(outside.join("marker"), "not-allowed").unwrap();
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root.clone()],
                write_roots: Vec::new(),
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: true,
            allow_network: false,
            max_shell_timeout_secs: 5,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let mut command = machine
            .bubblewrap_command(&bash, &root, "cat marker")
            .unwrap();
        std::fs::rename(&root, temp.path().join("moved")).unwrap();
        symlink(&outside, &root).unwrap();
        let output = command.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"allowed");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn opted_in_host_services_mount_files_and_ssh_agent_without_network() {
        use crate::host_services::{Discovery, HostAccess, HostShell};
        use std::io::{Read as _, Write as _};
        if std::env::var_os("DOTLINK_TEST_BWRAP").is_none() {
            return;
        }
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let Some(python) = resolve_executable("python3") else {
            return;
        };
        let Some(git) = resolve_executable("git") else {
            return;
        };
        let bwrap = resolve_executable("bwrap").expect("requested Bubblewrap validation");

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let home = temp.path().join("private-home");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let legacy_git = home.join(".gitconfig");
        let xdg_git = home.join("git-config");
        let nix_config = home.join("nix.conf");
        std::fs::write(&legacy_git, "[user]\n name = Dotlink Fixture\n").unwrap();
        std::fs::write(&xdg_git, "[dotlink]\n probe = visible\n").unwrap();
        std::fs::write(&nix_config, "experimental-features = nix-command flakes\n").unwrap();
        let sock = temp.path().join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();

        let host_shell = HostShell::prepare_discovered(
            HostAccess {
                nix_daemon: false,
                nix_config: true,
                ssh_agent: true,
                git_config: true,
            },
            Discovery {
                nix_daemon: None,
                nix_config: Some(nix_config.clone()),
                ssh_agent: Some(sock.clone()),
                git_configs: vec![legacy_git, xdg_git],
            },
        )
        .unwrap();
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root],
                write_roots: Vec::new(),
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            host_shell,
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: true,
            allow_network: false,
            max_shell_timeout_secs: 8,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();

        let acceptor = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut ping = [0_u8; 4];
            conn.read_exact(&mut ping).unwrap();
            assert_eq!(&ping, b"ping");
            conn.write_all(b"pong").unwrap();
        });
        let command = format!(
            r#"set -eu
                test -S "$SSH_AUTH_SOCK"
                test -r /tmp/home/.config/nix/nix.conf
                test ! -e /tmp/home/.ssh
                test -r /etc/passwd
                test -r /etc/group
                {git} config --global --get user.name | grep -q 'Dotlink Fixture'
                test -r /tmp/home/.config/git/config
                {git} config --get dotlink.probe | grep -q visible
                {python} -c 'import socket; s=socket.socket(socket.AF_UNIX); s.settimeout(3); s.connect("/tmp/ssh-agent.sock"); s.sendall(b"ping"); assert s.recv(4)==b"pong"'
            "#,
            git = git.display(),
            python = python.display(),
        );
        let response = machine
            .execute_shell(
                ShellArgs {
                    command,
                    dir: ".".to_owned(),
                    stdin: None,
                    timeout_secs: Some(8),
                    max_output_bytes: None,
                },
                false,
                None,
            )
            .await
            .unwrap();
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains(r#"\"exit_code\":0"#), "{json}");
        acceptor.join().unwrap();
        // Host config sources were never native filesystem grants.
        assert!(
            machine
                .resolve_existing(&nix_config.to_string_lossy(), AccessNeed::Read)
                .await
                .is_err()
        );
        let _ = bash;
        let _ = bwrap;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn nix_daemon_stays_masked_without_opt_in_even_with_network_and_broad_grants() {
        if std::env::var_os("DOTLINK_TEST_BWRAP").is_none() {
            return;
        }
        let daemon_socket = Path::new("/nix/var/nix/daemon-socket/socket");
        if !daemon_socket.exists() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        for unrestricted_fs in [false, true] {
            let machine = LocalMachine::new(MachineConfig {
                access: AccessSpec {
                    base_dir: root.clone(),
                    read_roots: if unrestricted_fs {
                        vec![PathBuf::from("/")]
                    } else {
                        vec![root.clone(), PathBuf::from("/nix/var/nix/daemon-socket")]
                    },
                    write_roots: if unrestricted_fs {
                        vec![PathBuf::from("/")]
                    } else {
                        Vec::new()
                    },
                    deny_read_roots: Vec::new(),
                    deny_write_roots: Vec::new(),
                    unrestricted_fs,
                },
                cache_mounts: Vec::new(),
                shell_env: Vec::new(),
                host_shell: crate::host_services::HostShell::default(),
                log: LogConfig::default(),
                serialize: false,
                allow_shell: true,
                sandbox_shell: true,
                // Network alone must not grant the independent Nix daemon capability.
                allow_network: true,
                max_shell_timeout_secs: 8,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,

                task_result_budget: None,
                job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

                job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
                protected_paths: Vec::new(),
            })
            .await
            .unwrap();
            let result = machine
                .execute_shell(
                    ShellArgs {
                        command: concat!(
                            "set -eu; test ! -e /nix/var/nix/daemon-socket/socket; ",
                            "for node in /dev/kvm /dev/kfd /dev/nvidia0 /dev/nvidiactl /dev/dri/renderD128; do ",
                            r#"if test -e "$node" && ( : <> "$node" ) 2>/dev/null; then exit 61; fi; "#,
                            "done; printf masked"
                        ).into(),
                        dir: if unrestricted_fs {
                            "/".into()
                        } else {
                            ".".into()
                        },
                        stdin: None,
                        timeout_secs: Some(8),
                        max_output_bytes: None,
                    },
                    false, None)
                .await
                .unwrap();
            let json = serde_json::to_string(&result).unwrap();
            assert!(
                json.contains("masked") && json.contains(r#"\"exit_code\":0"#),
                "Nix daemon unexpectedly visible (unrestricted={unrestricted_fs}): {json}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn pinned_host_mount_denies_are_not_changed_by_alias_swap() {
        use crate::host_services::{HostMount, HostMountKind, HostShell};
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let private = temp.path().join("private");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&private).unwrap();
        let denied = private.join("secret.conf");
        let public = root.join("visible.conf");
        std::fs::write(&denied, "secret").unwrap();
        std::fs::write(&public, "public").unwrap();
        let alias = root.join("mutable-alias");
        symlink(&denied, &alias).unwrap();
        let mount = HostMount::new(
            alias.clone(),
            PathBuf::from("/tmp/home/.gitconfig"),
            HostMountKind::RegularFile,
        )
        .unwrap();
        assert_eq!(mount.resolved, denied);
        std::fs::remove_file(&alias).unwrap();
        symlink(&public, &alias).unwrap();
        let result = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root],
                write_roots: Vec::new(),
                deny_read_roots: vec![private],
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            host_shell: HostShell {
                mounts: vec![mount],
                ..HostShell::default()
            },
            log: LogConfig::default(),
            serialize: false,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 5,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await;
        let error = result.expect_err("canonical denied target must remain denied");
        assert!(error.to_string().contains("denied or protected"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn host_mount_sources_cannot_be_ancestors_or_descendants_of_protected_paths() {
        use crate::host_services::{HostMount, HostMountKind, HostShell};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let private = temp.path().join("private");
        let protected_child = private.join("nested").join("secret");
        let mounted_file = private.join("mounted.conf");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir_all(protected_child.parent().unwrap()).unwrap();
        std::fs::write(&protected_child, b"secret").unwrap();
        std::fs::write(&mounted_file, b"config").unwrap();

        for (index, source, protected, kind) in [
            (
                0,
                private.clone(),
                protected_child,
                HostMountKind::Directory,
            ),
            (1, mounted_file, private.clone(), HostMountKind::RegularFile),
        ] {
            let mount = HostMount::new(
                source,
                PathBuf::from(format!("/tmp/home/host-{index}")),
                kind,
            )
            .unwrap();
            let outcome = LocalMachine::new(MachineConfig {
                access: AccessSpec {
                    base_dir: root.clone(),
                    read_roots: vec![root.clone()],
                    write_roots: Vec::new(),
                    deny_read_roots: Vec::new(),
                    deny_write_roots: Vec::new(),
                    unrestricted_fs: false,
                },
                cache_mounts: Vec::new(),
                shell_env: Vec::new(),
                host_shell: HostShell {
                    mounts: vec![mount],
                    ..HostShell::default()
                },
                log: LogConfig::default(),
                serialize: false,
                allow_shell: false,
                sandbox_shell: false,
                allow_network: false,
                max_shell_timeout_secs: 5,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,

                task_result_budget: None,
                job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

                job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
                protected_paths: vec![protected],
            })
            .await;
            let error = outcome.expect_err("host remapping must not bypass protection");
            assert!(error.to_string().contains("denied or protected"), "{error}");
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn git_ssh_signing_uses_only_opted_in_agent_without_private_key_mounts() {
        use crate::host_services::{Discovery, HostAccess, HostShell};
        use std::os::unix::fs::FileTypeExt;
        if std::env::var_os("DOTLINK_TEST_BWRAP").is_none() {
            return;
        }
        let tools = ["ssh-agent", "ssh-add", "ssh-keygen", "git", "bash", "bwrap"];
        let Some(paths) = tools
            .iter()
            .map(|tool| resolve_executable(tool))
            .collect::<Option<Vec<_>>>()
        else {
            // Not all developer images include OpenSSH and Git tools.
            return;
        };
        let [ssh_agent, ssh_add, ssh_keygen, git, _bash, _bwrap] =
            paths.try_into().expect("exactly six executable paths");
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repository");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("README.md"), b"signed fixture").unwrap();
        let key = temp.path().join("disposable-signing-key");
        let agent_socket = temp.path().join("disposable-agent.sock");
        let mut agent = tokio::process::Command::new(&ssh_agent)
            .arg("-D")
            .arg("-a")
            .arg(&agent_socket)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if std::fs::metadata(&agent_socket).is_ok_and(|meta| meta.file_type().is_socket()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(agent_socket.exists(), "disposable agent did not start");
        let generate = std::process::Command::new(&ssh_keygen)
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .output()
            .unwrap();
        assert!(
            generate.status.success(),
            "{}",
            String::from_utf8_lossy(&generate.stderr)
        );
        let add_key = std::process::Command::new(&ssh_add)
            .arg(&key)
            .env("SSH_AUTH_SOCK", &agent_socket)
            .output()
            .unwrap();
        assert!(
            add_key.status.success(),
            "{}",
            String::from_utf8_lossy(&add_key.stderr)
        );
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        let config = temp.path().join(".gitconfig");
        std::fs::write(&config, format!(
            "[user]\n name = Disposable Fixture\n email = fixture@example.invalid\n signingkey = key::{}\n[gpg]\n format = ssh\n[gpg \"ssh\"]\n program = {}\n[commit]\n gpgsign = true\n",
            public.trim(), ssh_keygen.display()
        )).unwrap();
        let git_init = std::process::Command::new(&git)
            .args(["init", "-q"])
            .arg(&root)
            .output()
            .unwrap();
        assert!(git_init.status.success());
        let host_shell = HostShell::prepare_discovered(
            HostAccess {
                ssh_agent: true,
                git_config: true,
                ..HostAccess::default()
            },
            Discovery {
                ssh_agent: Some(agent_socket),
                git_configs: vec![config],
                ..Discovery::default()
            },
        )
        .unwrap();
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root.clone()],
                write_roots: vec![root],
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            host_shell,
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: true,
            allow_network: false,
            max_shell_timeout_secs: 15,
            max_output_bytes: 16 * 1024,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let result = machine.execute_shell(ShellArgs {
            command: format!(
                "set -eu; {} add README.md; {} commit -qm 'Signed in Bubblewrap'; {} cat-file commit HEAD; test ! -e /tmp/home/.ssh",
                git.display(), git.display(), git.display(),
            ),
            dir: ".".into(), stdin: None,
            timeout_secs: Some(15), max_output_bytes: None,
        }, false, None).await.unwrap();
        let json = serde_json::to_string(&result).unwrap();
        assert!(
            json.contains("BEGIN SSH SIGNATURE"),
            "commit was not signed: {json}"
        );
        assert!(
            json.contains(r#"\"exit_code\":0"#),
            "sandboxed Git commit failed: {json}"
        );
        let _ = agent.kill().await;
        let _ = agent.wait().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandboxed_root_rw_preparation_accepts_system_managed_bubblewrap() {
        if resolve_executable("bash").is_none() || !crate::mcp::trusted_bubblewrap_available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: temp.path().to_path_buf(),
                read_roots: vec![PathBuf::from("/")],
                write_roots: vec![PathBuf::from("/")],
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: true,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: true,
            allow_network: false,
            max_shell_timeout_secs: 8,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,
            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,
            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .expect("sandboxed root-RW must not reject a system-managed Bubblewrap launcher");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn unrestricted_filesystem_grant_does_not_grant_device_access() {
        if std::env::var_os("DOTLINK_TEST_BWRAP").is_none() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: temp.path().to_path_buf(),
                read_roots: vec![PathBuf::from("/")],
                write_roots: vec![PathBuf::from("/")],
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: true,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: true,
            allow_network: false,
            max_shell_timeout_secs: 8,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let result = machine.execute_shell(ShellArgs {
            command: concat!(
                "set -eu; ",
                "for node in /dev/kvm /dev/kfd /dev/nvidia0 /dev/nvidiactl /dev/dri/renderD128; do ",
                r#"if test -e "$node" && ( : <> "$node" ) 2>/dev/null; then exit 61; fi; "#,
                "done; echo isolated"
            ).into(),
            dir: "/".into(), stdin: None,
            timeout_secs: Some(8), max_output_bytes: None,
        }, false, None).await.unwrap();
        let json = serde_json::to_string(&result).unwrap();
        assert!(
            json.contains("isolated") && json.contains(r#"\"exit_code\":0"#),
            "{json}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn deny_write_revokes_explicit_character_device_access() {
        use crate::host_services::{HostMount, HostMountKind, HostShell};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let source = PathBuf::from("/dev/null");
        let mount = HostMount::new(
            source.clone(),
            PathBuf::from("/dev/kvm"),
            HostMountKind::CharacterDevice,
        )
        .unwrap();
        let outcome = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root],
                write_roots: Vec::new(),
                deny_read_roots: Vec::new(),
                deny_write_roots: vec![source],
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            host_shell: HostShell {
                mounts: vec![mount],
                kvm: true,
                ..HostShell::default()
            },
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: true,
            allow_network: false,
            max_shell_timeout_secs: 5,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await;
        let error = outcome.expect_err("deny-write must override the device grant");
        assert!(error.to_string().contains("denied or protected"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn discovered_gpu_and_kvm_nodes_are_visible_only_when_granted() {
        if std::env::var_os("DOTLINK_TEST_BWRAP").is_none() {
            return;
        }
        let available = crate::hardware::available();
        let vendors = [
            (crate::setup::GpuVendor::Amd, available.amd),
            (crate::setup::GpuVendor::Nvidia, available.nvidia),
        ]
        .into_iter()
        .filter_map(|(vendor, present)| present.then_some(vendor))
        .collect::<Vec<_>>();
        if !available.kvm && vendors.is_empty() {
            return;
        }

        let mut host_shell = crate::host_services::HostShell::default();
        crate::hardware::add_to_shell(&mut host_shell, available.kvm, &vendors).unwrap();
        assert_eq!(host_shell.gpu, vendors);
        if available.kvm {
            assert!(host_shell.kvm);
        }
        let mut checked = String::new();
        for mount in &host_shell.mounts {
            if mount.kind == crate::host_services::HostMountKind::CharacterDevice {
                checked.push_str(&format!(
                    "test -c '{}' || exit 47; ",
                    mount.target.display()
                ));
            }
        }
        if !vendors.is_empty() {
            checked.push_str("test -d /sys/class/drm || exit 48; ");
        }
        if std::env::var_os("DOTLINK_TEST_GPU_SMOKE").is_some() {
            if available.nvidia
                && let Some(smi) = resolve_executable("nvidia-smi")
            {
                checked.push_str(&format!("'{}' -L >/dev/null || exit 49; ", smi.display()));
            }
            if !vendors.is_empty()
                && let Some(vulkan) = resolve_executable("vulkaninfo")
            {
                checked.push_str(&format!(
                    "'{}' --summary >/dev/null || exit 50; ",
                    vulkan.display()
                ));
            }
        }
        checked.push_str("echo hardware-visible");
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root],
                write_roots: Vec::new(),
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            host_shell,
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: true,
            allow_network: false,
            max_shell_timeout_secs: 12,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let result = machine
            .execute_shell(
                ShellArgs {
                    command: checked,
                    dir: ".".into(),
                    stdin: None,
                    timeout_secs: Some(12),
                    max_output_bytes: None,
                },
                false,
                None,
            )
            .await
            .unwrap();
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(serialized.contains("hardware-visible"), "{serialized}");
        assert!(serialized.contains(r#"\"exit_code\":0"#), "{serialized}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn pinned_character_device_mounts_are_usable_in_bubblewrap() {
        use crate::host_services::{HostMount, HostMountKind, HostShell};
        use std::os::unix::fs::{MetadataExt, symlink};
        if std::env::var_os("DOTLINK_TEST_BWRAP").is_none() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let alias = root.join("device-alias");
        symlink("/dev/null", &alias).unwrap();
        let first = HostMount::new(
            alias.clone(),
            PathBuf::from("/dev/kvm"),
            HostMountKind::CharacterDevice,
        )
        .unwrap();
        std::fs::remove_file(&alias).unwrap();
        symlink("/dev/zero", &alias).unwrap();
        // The pin cannot switch to the new device inode or identity.
        assert_eq!(
            first.pinned.metadata().unwrap().rdev(),
            std::fs::metadata("/dev/null").unwrap().rdev()
        );
        assert_ne!(
            first.pinned.metadata().unwrap().rdev(),
            std::fs::metadata(&alias).unwrap().rdev()
        );

        let second = HostMount::new(
            PathBuf::from("/dev/zero"),
            PathBuf::from("/dev/dri/renderD128"),
            HostMountKind::CharacterDevice,
        )
        .unwrap();
        for expose in [false, true] {
            let host_shell = if expose {
                HostShell {
                    mounts: vec![first.clone(), second.clone()],
                    kvm: true,
                    gpu: vec![crate::setup::GpuVendor::Amd],
                    ..HostShell::default()
                }
            } else {
                HostShell::default()
            };
            let machine = LocalMachine::new(MachineConfig {
                access: AccessSpec {
                    base_dir: root.clone(),
                    read_roots: vec![root.clone()],
                    write_roots: Vec::new(),
                    deny_read_roots: Vec::new(),
                    deny_write_roots: Vec::new(),
                    unrestricted_fs: false,
                },
                cache_mounts: Vec::new(),
                shell_env: Vec::new(),
                host_shell,
                log: LogConfig::default(),
                serialize: false,
                allow_shell: true,
                sandbox_shell: true,
                allow_network: false,
                max_shell_timeout_secs: 10,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,

                task_result_budget: None,
                job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

                job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
                protected_paths: Vec::new(),
            })
            .await
            .unwrap();
            let command = if expose {
                "set -eu; test -c /dev/kvm; test -c /dev/dri/renderD128; exec 3</dev/kvm; exec 4</dev/dri/renderD128; echo mounted"
            } else {
                "set -eu; test ! -e /dev/kvm; test ! -e /dev/dri/renderD128; echo isolated"
            };
            let result = machine
                .execute_shell(
                    ShellArgs {
                        command: command.to_owned(),
                        dir: ".".to_owned(),
                        stdin: None,
                        timeout_secs: Some(10),
                        max_output_bytes: None,
                    },
                    false,
                    None,
                )
                .await
                .unwrap();
            let json = serde_json::to_string(&result).unwrap();
            assert!(json.contains(r#"\"exit_code\":0"#), "{json}");
            assert!(
                json.contains(if expose { "mounted" } else { "isolated" }),
                "{json}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn opted_in_nix_daemon_works_without_shell_network() {
        use crate::host_services::{Discovery, HostAccess, HostShell};
        if std::env::var_os("DOTLINK_TEST_BWRAP").is_none() {
            return;
        }
        let Some(nix) = resolve_executable("nix") else {
            return;
        };
        let Some(daemon) = crate::host_services::discover().nix_daemon else {
            return;
        };
        let Some(bash) = resolve_executable("bash") else {
            return;
        };
        let Some(bwrap) = resolve_executable("bwrap") else {
            return;
        };

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let host_shell = HostShell::prepare_discovered(
            HostAccess {
                nix_daemon: true,
                ..HostAccess::default()
            },
            Discovery {
                nix_daemon: Some(daemon),
                ..Discovery::default()
            },
        )
        .unwrap();
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root],
                write_roots: Vec::new(),
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            host_shell,
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: true,
            allow_network: false,
            max_shell_timeout_secs: 8,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let result = machine
            .execute_shell(
                ShellArgs {
                    command: format!(
                        "{} --extra-experimental-features 'nix-command flakes' store info",
                        nix.display()
                    ),
                    dir: ".".into(),
                    stdin: None,
                    timeout_secs: Some(8),
                    max_output_bytes: None,
                },
                false,
                None,
            )
            .await
            .unwrap();
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("Store URL: daemon"), "{json}");
        assert!(json.contains(r#"\"exit_code\":0"#), "{json}");
        let _ = bash;
        let _ = bwrap;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bubblewrap_runtime_smoke_when_requested() {
        if std::env::var_os("DOTLINK_TEST_BWRAP").is_none() {
            return;
        }

        let Some(bwrap) = resolve_executable("bwrap") else {
            panic!("DOTLINK_TEST_BWRAP requested but bwrap is not installed");
        };
        let Some(bash) = resolve_executable("bash") else {
            panic!("DOTLINK_TEST_BWRAP requested but bash is not installed");
        };

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let writable = root.join("writable");
        let readable = root.join("readable");
        let denied = root.join("denied");
        std::fs::create_dir(&writable).unwrap();
        std::fs::create_dir(&readable).unwrap();
        std::fs::create_dir(&denied).unwrap();
        std::fs::write(readable.join("visible.txt"), b"visible").unwrap();
        std::fs::write(denied.join("secret.txt"), b"secret").unwrap();

        let machine = LocalMachine {
            config: RuntimeConfig {
                access: AccessPolicy::from_spec(AccessSpec {
                    base_dir: root.clone(),
                    // Denied content is outside both mounted grants, so
                    // external host rename/recreation cannot expose it.
                    read_roots: vec![readable.clone(), writable.clone()],
                    write_roots: vec![writable.clone()],
                    deny_read_roots: vec![denied.clone()],
                    deny_write_roots: vec![denied],
                    unrestricted_fs: false,
                })
                .unwrap(),
                cache_mounts: Vec::new(),
                shell_env: Vec::new(),
                #[cfg(target_os = "linux")]
                host_shell: crate::host_services::HostShell::default(),
                log: LogConfig::default(),
                allow_shell: true,
                sandbox_shell: true,
                allow_network: false,
                max_shell_timeout_secs: 10,
                max_output_bytes: 16 * 1024,
                max_read_bytes: 4096,
                max_write_bytes: 4096,

                task_result_budget: None,
                protected_paths: Vec::new(),
            },
            launchers: PreparedLaunchers::for_test(Some(bash), Some(bwrap)),

            jobs: crate::jobs::JobManager::new(std::time::Duration::from_secs(
                crate::limits::DEFAULT_JOB_RETENTION_SECS,
            ))
            .unwrap(),
            operation_gate: Arc::new(RwLock::new(())),
            transport_context_token: TransportContextToken::from("test-transport-context"),
            router: std::sync::OnceLock::new(),
            serialize: false,
            #[cfg(target_os = "linux")]
            secure_fs: Arc::new(
                crate::secure_fs::SecureFs::new(
                    &[readable.clone(), writable.clone()],
                    std::slice::from_ref(&writable),
                )
                .unwrap(),
            ),
        };

        let result = machine
            .execute_shell(
                ShellArgs {
                    command: concat!(
                        "set -eu; ",
                        "cat ../readable/visible.txt >/dev/null; ",
                        "if cat ../denied/secret.txt >/dev/null 2>&1; then exit 11; fi; ",
                        "for d in /proc/self/fd/*; do if cat \"$d/denied/secret.txt\" >/dev/null 2>&1; then exit 14; fi; done; ",
                        "if touch ../readable/should-not-work >/dev/null 2>&1; then exit 12; fi; ",
                        "touch works; ",
                        "test ! -e /etc/resolv.conf"
                    )
                    .to_owned(),
                    dir: "writable".to_owned(),
                    stdin: None,
                    timeout_secs: Some(10),
                    max_output_bytes: Some(16 * 1024),
                },
                false, None)
            .await
            .unwrap();

        let serialized = serde_json::to_string(&result).unwrap();
        assert!(
            serialized.contains("\\\"exit_code\\\":0"),
            "sandbox smoke failed: {serialized}"
        );
        assert!(writable.join("works").exists());
        assert!(!readable.join("should-not-work").exists());

        let late = writable.join("late");
        let background = machine
            .execute_shell(
                ShellArgs {
                    command: "(sleep 2; touch late) >/dev/null 2>&1 &".to_owned(),
                    dir: "writable".to_owned(),
                    stdin: None,
                    timeout_secs: Some(10),
                    max_output_bytes: Some(16 * 1024),
                },
                false,
                None,
            )
            .await
            .unwrap();
        let serialized = serde_json::to_string(&background).unwrap();
        assert!(
            serialized.contains("\\\"exit_code\\\":0"),
            "background sandbox smoke failed: {serialized}"
        );

        if !late.exists() {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            assert!(
                !late.exists(),
                "sandbox descendant mutated a host mount after the shell tool returned"
            );
        }
    }

    /// A configured deny whose parent is visible through a host bind must be
    /// rejected before Bubblewrap is invoked, regardless of alias retargets.
    /// Rebinding a pathname mask is not stable against host-side rename.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandbox_rejects_retargeted_alias_under_readable_grant() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let secret = root.join("secret");
        let other = root.join("other");
        let alias = root.join("alias");
        for dir in [&secret, &other] {
            std::fs::create_dir(dir).unwrap();
            std::fs::write(dir.join("file"), b"secret").unwrap();
        }
        symlink(&secret, &alias).unwrap();
        let access = AccessPolicy::resolve(AccessSpec {
            base_dir: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: Vec::new(),
            deny_read_roots: vec![alias.clone()],
            deny_write_roots: Vec::new(),
            unrestricted_fs: false,
        })
        .await
        .unwrap();
        let mut machine = sandbox_machine(&root, Vec::new());
        machine.config.access = access;
        machine.secure_fs =
            Arc::new(crate::secure_fs::SecureFs::new(std::slice::from_ref(&root), &[]).unwrap());
        for target in [&secret, &other] {
            if target == &other {
                std::fs::remove_file(&alias).unwrap();
                symlink(&other, &alias).unwrap();
            }
            let error = machine
                .bubblewrap_command(Path::new("/bin/bash"), &root, "true")
                .unwrap_err();
            assert!(
                error.to_string().contains("readable shell mount"),
                "{error}"
            );
        }
    }
}
