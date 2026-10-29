//! The local machine: access policy, pinned descriptors, and the tool surface.
//!
//! Tool implementations live in [`tools`], one module per family. This module
//! owns what they share: who may read or write what, the descriptor-rooted file
//! primitives, the advertised-tool table and the MCP server handler.

mod tools;
#[cfg(test)]
pub(crate) use tools::test_support::test_machine;
pub(crate) use tools::{MAX_EDIT_FILES, MAX_EDIT_OPS};

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::{
    ErrorData as McpError,
    handler::server::router::tool::ToolRouter,
    handler::server::tool::ToolCallContext,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
        ContentBlock, GetTaskParams, GetTaskResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ResultType, ServerCapabilities, ServerConfig,
        Tool, UpdateTaskParams,
    },
    service::{RequestContext, RoleServer},
    tool_handler,
};

use serde_json::Value;
use tokio::{fs, io::AsyncReadExt, sync::RwLock};
use uuid::Uuid;

use crate::logging::{LogConfig, LogOutcome};

pub(crate) use tools::shell::{resolve_executable, with_profile_limits};

#[derive(Clone, Debug)]
pub struct AccessSpec {
    pub base_dir: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub deny_read_roots: Vec<PathBuf>,
    pub deny_write_roots: Vec<PathBuf>,
    pub unrestricted_fs: bool,
}

#[derive(Clone, Debug)]
pub struct MachineConfig {
    pub access: AccessSpec,
    pub cache_mounts: Vec<SandboxCacheMount>,
    pub shell_env: Vec<(String, String)>,
    #[cfg(target_os = "linux")]
    pub host_shell: crate::host_services::HostShell,
    pub log: LogConfig,
    pub serialize: bool,
    pub allow_shell: bool,
    pub sandbox_shell: bool,
    pub allow_network: bool,
    pub max_shell_timeout_secs: u64,
    pub max_output_bytes: usize,
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
    /// Smallest serialized native-Task result budget across all active
    /// transports sharing this runtime. None means every active transport is
    /// effectively unbounded for task results.
    pub task_result_budget: Option<usize>,
    pub job_retention_secs: u64,
    pub job_spool_bytes: u64,
    pub protected_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct SandboxCacheMount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub writable: bool,
}

#[derive(Clone, Debug)]
struct PreparedCacheMount {
    source: PathBuf,
    target: PathBuf,
    writable: bool,
    #[cfg(target_os = "linux")]
    pinned: Arc<std::fs::File>,
}

#[cfg(target_os = "linux")]
impl PreparedCacheMount {
    fn verify_source_identity(&self) -> Result<()> {
        use std::os::unix::fs::MetadataExt as _;
        let pinned = self.pinned.metadata().with_context(|| {
            format!(
                "cannot inspect prepared cache source {}",
                self.source.display()
            )
        })?;
        let current_path = std::fs::canonicalize(&self.source).with_context(|| {
            format!(
                "shell cache source changed or disappeared since preparation: {}",
                self.source.display()
            )
        })?;
        let current = std::fs::metadata(&current_path).with_context(|| {
            format!(
                "cannot inspect shell cache source {}",
                current_path.display()
            )
        })?;
        if current_path != self.source
            || pinned.dev() != current.dev()
            || pinned.ino() != current.ino()
        {
            bail!(
                "shell cache source changed since preparation: {}; restart dotlink to re-authorize the new directory",
                self.source.display()
            );
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
impl PreparedCacheMount {
    fn for_test(source: PathBuf, target: PathBuf, writable: bool) -> Self {
        let source = std::fs::canonicalize(source).expect("test cache source canonicalizes");
        let pinned = Arc::new(
            crate::secure_fs::SecureFs::open_canonical_directory(&source)
                .expect("test cache source pins"),
        );
        Self {
            source,
            target,
            writable,
            pinned,
        }
    }
}

#[derive(Clone, Debug)]
struct AccessPolicy {
    base_dir: PathBuf,
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
    /// Every name the deny rules guard; see [`guard_identities`].
    deny_read_roots: Vec<PathBuf>,
    deny_write_roots: Vec<PathBuf>,
    /// Configured deny-read and deny-write rules, as the user counts them.
    deny_rules: [usize; 2],
    unrestricted_fs: bool,
}

#[derive(Clone, Debug)]
struct RuntimeConfig {
    access: AccessPolicy,
    cache_mounts: Vec<PreparedCacheMount>,
    shell_env: Vec<(String, String)>,
    #[cfg(target_os = "linux")]
    host_shell: crate::host_services::HostShell,
    log: LogConfig,
    allow_shell: bool,
    sandbox_shell: bool,
    allow_network: bool,
    shell_program: Option<PathBuf>,
    bwrap_program: Option<PathBuf>,
    max_shell_timeout_secs: u64,
    max_output_bytes: usize,
    max_read_bytes: usize,
    max_write_bytes: usize,
    task_result_budget: Option<usize>,
    protected_paths: Vec<PathBuf>,
}

#[derive(Clone)]
pub(crate) struct TransportContextToken(Arc<str>);

impl TransportContextToken {
    fn random() -> Self {
        Self(Arc::from(Uuid::new_v4().to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl From<&str> for TransportContextToken {
    fn from(value: &str) -> Self {
        Self(Arc::from(value))
    }
}

impl std::fmt::Debug for TransportContextToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TransportContextToken([redacted])")
    }
}

#[derive(Clone, Debug)]
pub struct LocalMachine {
    config: RuntimeConfig,
    jobs: crate::jobs::JobManager,
    operation_gate: Arc<RwLock<()>>,
    serialize: bool,
    /// Capability token authenticating transport-injected request constraints.
    /// It never leaves the process except inside the private in-process OpenAI
    /// dispatch envelope and makes client-supplied lookalike `_meta` inert.
    transport_context_token: TransportContextToken,
    /// The advertised surface, built once. Visibility depends only on the
    /// immutable access policy, so every request can share one router instead
    /// of merging the per-family routers again.
    router: OnceLock<Arc<ToolRouter<Self>>>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    secure_fs: Arc<crate::secure_fs::SecureFs>,
}

#[derive(Debug)]
pub(crate) struct PreparedMachine {
    config: RuntimeConfig,
    authority: serde_json::Value,
    job_root: PathBuf,
    serialize: bool,
    retention: std::time::Duration,
    spool_limit: u64,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    secure_fs: Arc<crate::secure_fs::SecureFs>,
}

/// A write that is staged but not yet visible; see `LocalMachine::stage_write`.
/// Both backends stage beside the target and install in one step; the shared
/// mode and naming policy lives in [`crate::staging`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
pub(crate) struct PendingWrite(crate::secure_fs::StagedWrite);

#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct PendingWrite(crate::staging::StagedWrite);

/// Smallest accepted profile byte limit. Below this the fixed JSON reply framing
/// and a truncation marker no longer fit, so a configured ceiling could not be
/// honoured; the value is rejected rather than silently widened.
pub const MIN_BYTE_LIMIT: usize = 1024;

/// Smallest accepted shell timeout.
pub const MIN_SHELL_TIMEOUT_SECS: u64 = 1;

/// Private request metadata carried through rmcp by the in-process OpenAI
/// adapter. These fields are trusted only when the opaque per-runtime token
/// matches; public HTTP/stdio clients may send the same names but cannot affect
/// execution semantics.
pub(crate) const TRANSPORT_META_PREFIX: &str = "io.abird.dotlink/";
pub(crate) const TRANSPORT_CONTEXT_TOKEN_META_KEY: &str = "io.abird.dotlink/transportContextToken";
pub(crate) const SYNC_BUDGET_META_KEY: &str = "io.abird.dotlink/syncBudgetMs";
pub(crate) const REQUEST_KEY_META_KEY: &str = "io.abird.dotlink/requestKey";
pub(crate) const RESULT_BUDGET_META_KEY: &str = "io.abird.dotlink/resultBudgetBytes";

pub(crate) fn serialized_text_content_len(text: &str) -> usize {
    text.chars()
        .map(|ch| match ch {
            '"' | '\\' => 2,
            '\u{08}' | '\t' | '\n' | '\u{0c}' | '\r' => 2,
            ch if ch <= '\u{1f}' => 6,
            ch => ch.len_utf8(),
        })
        .sum()
}

pub(crate) fn serialized_text_tool_result_fixed_len() -> usize {
    static EMPTY_RESULT_BYTES: OnceLock<usize> = OnceLock::new();
    *EMPTY_RESULT_BYTES.get_or_init(|| {
        serde_json::to_vec(&CallToolResult::success(vec![ContentBlock::text("")]))
            .expect("empty text tool result serializes")
            .len()
    })
}

pub(crate) fn serialized_text_tool_result_len(text: &str) -> usize {
    serialized_text_tool_result_fixed_len().saturating_add(serialized_text_content_len(text))
}

pub(crate) fn serialized_text_tool_result_parts_len(parts: &[&str]) -> usize {
    parts
        .iter()
        .fold(serialized_text_tool_result_fixed_len(), |total, part| {
            total.saturating_add(serialized_text_content_len(part))
        })
}

fn serialized_error_text_tool_result_fixed_len() -> usize {
    static EMPTY_ERROR_BYTES: OnceLock<usize> = OnceLock::new();
    *EMPTY_ERROR_BYTES.get_or_init(|| {
        serde_json::to_vec(&CallToolResult::error(vec![ContentBlock::text("")]))
            .expect("empty error tool result serializes")
            .len()
    })
}

fn fitted_tool_result_text(text: &str, budget: usize, is_error: bool, suffix: &str) -> String {
    let fixed = if is_error {
        serialized_error_text_tool_result_fixed_len()
    } else {
        serialized_text_tool_result_fixed_len()
    };
    if fixed.saturating_add(serialized_text_content_len(text)) <= budget {
        return text.to_owned();
    }
    let suffix_cost = serialized_text_content_len(suffix);
    let available = budget.saturating_sub(fixed).saturating_sub(suffix_cost);
    let mut used = 0_usize;
    let mut end = 0_usize;
    let mut utf8 = [0_u8; 4];
    for (index, ch) in text.char_indices() {
        let cost = serialized_text_content_len(ch.encode_utf8(&mut utf8));
        let Some(next) = used.checked_add(cost) else {
            break;
        };
        if next > available {
            break;
        }
        used = next;
        end = index + ch.len_utf8();
    }
    if end == 0 && suffix_cost > budget.saturating_sub(fixed) {
        return String::new();
    }
    let mut output = String::with_capacity(end.saturating_add(suffix.len()));
    output.push_str(&text[..end]);
    output.push_str(suffix);
    output
}

fn fit_call_tool_response_to_budget(
    tool: &str,
    response: CallToolResponse,
    budget: Option<usize>,
) -> CallToolResponse {
    let Some(budget) = budget else {
        return response;
    };
    let CallToolResponse::Complete(result) = response else {
        return response;
    };
    if serialized_json_len(&result).is_some_and(|size| size <= budget) {
        return CallToolResponse::Complete(result);
    }

    let is_error = result.is_error.unwrap_or(false);
    let original_text = result
        .content
        .iter()
        .find_map(|block| block.as_text().map(|text| text.text.as_str()))
        .unwrap_or(if is_error {
            "tool returned an oversized error response"
        } else {
            "tool completed successfully"
        });
    let suffix = if is_error {
        "\n… error details truncated to fit the transport response budget"
    } else {
        "\n… detailed result omitted because it exceeded the transport response budget"
    };
    let text = fitted_tool_result_text(original_text, budget, is_error, suffix);
    if is_error {
        CallToolResponse::Complete(CallToolResult::error(vec![ContentBlock::text(text)]))
    } else {
        tracing::error!(
            tool,
            budget,
            "tool success result exceeded its transport response budget"
        );
        CallToolResponse::Complete(CallToolResult::success(vec![ContentBlock::text(text)]))
    }
}

fn fit_mcp_error_to_budget(error: McpError, budget: Option<usize>) -> McpError {
    let Some(budget) = budget else {
        return error;
    };
    if serialized_json_len(&error).is_some_and(|size| size <= budget) {
        return error;
    }

    let code = error.code;
    let message = error.message.into_owned();
    let without_data = McpError::new(code, message.clone(), None);
    if serialized_json_len(&without_data).is_some_and(|size| size <= budget) {
        return without_data;
    }

    let empty = McpError::new(code, "", None);
    let fixed = serialized_json_len(&empty).unwrap_or(usize::MAX);
    let suffix = " … error details truncated to fit the transport response budget";
    let suffix_cost = serialized_text_content_len(suffix);
    let available = budget.saturating_sub(fixed).saturating_sub(suffix_cost);
    let mut used = 0_usize;
    let mut end = 0_usize;
    let mut utf8 = [0_u8; 4];
    for (index, ch) in message.char_indices() {
        let cost = serialized_text_content_len(ch.encode_utf8(&mut utf8));
        let Some(next) = used.checked_add(cost) else {
            break;
        };
        if next > available {
            break;
        }
        used = next;
        end = index + ch.len_utf8();
    }
    let fitted = if suffix_cost <= budget.saturating_sub(fixed) {
        let mut fitted = String::with_capacity(end.saturating_add(suffix.len()));
        fitted.push_str(&message[..end]);
        fitted.push_str(suffix);
        fitted
    } else {
        String::new()
    };
    McpError::new(code, fitted, None)
}

struct CountingWriter(usize);

impl std::io::Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("serialized JSON size overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Exact serialized JSON length without allocating the serialized bytes.
pub(crate) fn serialized_json_len(value: &impl serde::Serialize) -> Option<usize> {
    let mut writer = CountingWriter(0);
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(writer.0)
}

/// Worst-case JSON escaping of one payload byte: a control byte becomes
/// `\u00XX`. Payloads bounded by `max_write_bytes` therefore never need a body
/// larger than this multiple of it.
const MAX_JSON_ESCAPE: usize = 6;
/// A control byte encoded in an inner JSON string needs one more outer escape.
const MAX_NESTED_JSON_ESCAPE: usize = 7;
const SHELL_OUTPUT_STREAMS: usize = 2;
/// Base JSON-RPC framing, independent of payload and edit path strings.
const MAX_REQUEST_FRAMING_BYTES: usize = 64 * 1024;

/// Conservative upper bound for a complete ordinary shell CallToolResult at a
/// given per-stream raw byte limit. Shell output is first embedded in an inner
/// JSON string and then in the outer MCP text block, so one source byte can
/// occupy up to seven serialized bytes. The fixed reserve covers status/path
/// metadata and CallToolResult framing.
pub(crate) fn shell_result_worst_case_bytes(per_stream: usize) -> usize {
    per_stream
        .saturating_mul(SHELL_OUTPUT_STREAMS)
        .saturating_mul(MAX_NESTED_JSON_ESCAPE)
        .saturating_add(MAX_REQUEST_FRAMING_BYTES)
}

pub(crate) fn shell_result_fits_budget(per_stream: usize, budget: usize) -> bool {
    shell_result_worst_case_bytes(per_stream) <= budget
}
/// Separate headroom for every edit operation's possible pathname: the 512-op
/// tool must remain reachable even with a small `max_write_bytes`. Budget a
/// practical 4 KiB path with worst-case JSON escaping for each operation.
const EDIT_PATH_ENVELOPE_BYTES: usize = 4 * 1024 * MAX_JSON_ESCAPE;

/// A configured limit is a hard ceiling: reject a value the runtime cannot
/// honour instead of quietly raising it.
fn checked_limit<T: PartialOrd + std::fmt::Display>(name: &str, value: T, minimum: T) -> Result<T> {
    if value < minimum {
        return Err(crate::recovery::operator(anyhow!(
            "{name} must be at least {minimum}, got {value}; raise it in the profile or CLI"
        )));
    }
    Ok(value)
}

fn checked_byte_limit(name: &str, value: usize) -> Result<usize> {
    let value = checked_limit(name, value, MIN_BYTE_LIMIT)?;
    if value > crate::limits::OPENAI_TUNNEL_TRANSFER_CEILING_BYTES {
        return Err(crate::recovery::operator(anyhow!(
            "{name} must not exceed {} bytes, the configured transfer ceiling below the OpenAI tunnel's {} byte serialized-message limit",
            crate::limits::OPENAI_TUNNEL_TRANSFER_CEILING_BYTES,
            crate::limits::OPENAI_TUNNEL_SERIALIZED_MESSAGE_BYTES,
        )));
    }
    Ok(value)
}

fn checked_job_retention(value: u64) -> Result<std::time::Duration> {
    if !(crate::limits::MIN_JOB_RETENTION_SECS..=crate::limits::MAX_JOB_RETENTION_SECS)
        .contains(&value)
    {
        return Err(crate::recovery::operator(anyhow!(
            "job_retention_secs must be between {} and {} seconds, got {value}",
            crate::limits::MIN_JOB_RETENTION_SECS,
            crate::limits::MAX_JOB_RETENTION_SECS,
        )));
    }
    Ok(std::time::Duration::from_secs(value))
}

/// Name of the shell tool this platform advertises. Derived from `TOOL_SPECS`
/// through [`Capabilities::allows`], so a new shell variant cannot be added to
/// the table and missed here.
pub fn shell_tool_name() -> &'static str {
    let shell = Capabilities {
        shell: true,
        ..Capabilities::default()
    };
    TOOL_SPECS
        .iter()
        .find(|spec| shell.allows(spec.capability))
        .map_or("bash", |spec| spec.name)
}

/// True for the shell tool this platform advertises. Tool visibility, the
/// schema-limit rewrite, the tunnel's delivery reserve and the TOOL log all ask
/// this one question, so a new shell variant cannot be missed in one of them.
pub fn is_shell_tool(name: &str) -> bool {
    name == shell_tool_name()
}

#[derive(Debug, Clone, Copy)]
enum AccessNeed {
    Read,
    Write,
    ReadWrite,
}

/// What a runtime may advertise. Named fields, not positional booleans, so a
/// new capability cannot be passed in the wrong slot.
#[derive(Clone, Copy, Debug, Default)]
pub struct Capabilities {
    pub read: bool,
    pub write: bool,
    pub read_write: bool,
    pub shell: bool,
}

/// What one tool needs. `ShellUnix`/`ShellWindows` are the same grant with a
/// platform-dependent tool name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    Read,
    Write,
    ReadWrite,
    ShellUnix,
    ShellWindows,
    Job,
}

/// One row per advertised tool: the single place that records a tool's
/// capability and whether it may run concurrently. Visibility, the concurrency
/// gate, `--list-tools` and log labels all read this table.
pub struct ToolSpec {
    pub name: &'static str,
    pub capability: Capability,
    /// Pure reads that may overlap each other without an operation gate. Read
    /// only on platforms that still serialize mutation.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub concurrent_read: bool,
}

pub const TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "ls",
        capability: Capability::Read,
        concurrent_read: true,
    },
    ToolSpec {
        name: "grep",
        capability: Capability::Read,
        concurrent_read: true,
    },
    ToolSpec {
        name: "read",
        capability: Capability::Read,
        concurrent_read: true,
    },
    ToolSpec {
        name: "bread",
        capability: Capability::Read,
        concurrent_read: true,
    },
    ToolSpec {
        name: "write",
        capability: Capability::Write,
        concurrent_read: false,
    },
    ToolSpec {
        name: "bwrite",
        capability: Capability::Write,
        concurrent_read: false,
    },
    ToolSpec {
        name: "edit",
        capability: Capability::ReadWrite,
        concurrent_read: false,
    },
    ToolSpec {
        name: "patch",
        capability: Capability::ReadWrite,
        concurrent_read: false,
    },
    ToolSpec {
        name: "bedit",
        capability: Capability::ReadWrite,
        concurrent_read: false,
    },
    ToolSpec {
        name: "bash",
        capability: Capability::ShellUnix,
        concurrent_read: false,
    },
    ToolSpec {
        name: "powershell",
        capability: Capability::ShellWindows,
        concurrent_read: false,
    },
    ToolSpec {
        name: "job_start",
        capability: Capability::Job,
        concurrent_read: false,
    },
    ToolSpec {
        name: "job_list",
        capability: Capability::Job,
        concurrent_read: true,
    },
    ToolSpec {
        name: "job_get",
        capability: Capability::Job,
        concurrent_read: true,
    },
    ToolSpec {
        name: "job_output",
        capability: Capability::Job,
        concurrent_read: true,
    },
    ToolSpec {
        name: "job_ack",
        capability: Capability::Job,
        concurrent_read: false,
    },
    ToolSpec {
        name: "job_cancel",
        capability: Capability::Job,
        concurrent_read: false,
    },
];

impl Capabilities {
    pub fn allows(self, capability: Capability) -> bool {
        match capability {
            Capability::Read => self.read,
            Capability::Write => self.write,
            Capability::ReadWrite => self.read_write,
            Capability::ShellUnix => self.shell && !cfg!(windows),
            Capability::ShellWindows => self.shell && cfg!(windows),
            Capability::Job => self.shell,
        }
    }
}

fn contains_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| component == Component::ParentDir)
}

fn path_depth(path: &Path) -> usize {
    path.components().count()
}

/// Fold `.` and `..` without touching the filesystem, so a policy path with a
/// not-yet-existing component cannot smuggle `..` into a comparison.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// SHA-256 of a byte buffer, used for snapshot conflict detection and the
/// `bedit` precondition.
pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

async fn canonicalize_grant(base_dir: &Path, path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };
    let canonical = fs::canonicalize(&joined)
        .await
        .with_context(|| format!("grant path does not exist: {}", joined.display()))?;
    if !fs::metadata(&canonical).await?.is_dir() {
        bail!("grant path is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

fn text_path_label(value: &str) -> String {
    if value.is_empty() {
        return ".".to_owned();
    }
    if value
        .chars()
        .any(|ch| ch == ':' || ch == '"' || ch == '\\' || ch.is_control())
    {
        return serde_json::to_string(value).expect("path text serializes as JSON string");
    }
    value.to_owned()
}

#[cfg(unix)]
fn byte_path_label(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len().saturating_add(3));
    output.push_str("b\"");
    for &byte in bytes {
        match byte {
            b'"' => output.push_str("\\\""),
            b'\\' => output.push_str("\\\\"),
            0x20..=0x7e => output.push(char::from(byte)),
            _ => {
                write!(&mut output, "\\x{byte:02X}").expect("write to String cannot fail");
            }
        }
    }
    output.push('"');
    output
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Absent, or below a non-directory: nothing exists at the path yet.
pub(crate) fn is_missing(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Resolve a policy path the way the kernel would reach it, without requiring
/// it to exist. Symlinks (dangling ones included) expand in place and `..`
/// applies to the already-resolved prefix, so `link -> pivot/../x` follows
/// `pivot` first. Below a missing component nothing can be a link yet; there
/// `..` folds lexically, as it would once the directory is created.
pub(crate) fn resolve_policy_path(base_dir: &Path, path: &Path) -> Result<PathBuf> {
    const MAX_SYMLINKS: usize = 40;
    let joined = base_dir.join(path);
    if let Ok(canonical) = std::fs::canonicalize(&joined) {
        return Ok(canonical);
    }

    let parts = |path: &Path| {
        path.components()
            .rev()
            .map(|part| part.as_os_str().to_owned())
            .collect::<Vec<_>>()
    };
    // Pending components, next one last.
    let mut pending = parts(&joined);
    let mut resolved = PathBuf::new();
    let mut links = 0_usize;
    while let Some(part) = pending.pop() {
        match Path::new(&part).components().next() {
            None | Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                resolved.pop();
            }
            // `push` replaces the path for a root, keeping a Windows prefix.
            Some(Component::Prefix(_) | Component::RootDir) => resolved.push(&part),
            Some(Component::Normal(name)) => {
                let candidate = resolved.join(name);
                match std::fs::symlink_metadata(&candidate) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        links += 1;
                        if links > MAX_SYMLINKS {
                            bail!("too many symlinks while resolving {}", joined.display());
                        }
                        let link = std::fs::read_link(&candidate).with_context(|| {
                            format!("could not read policy symlink {}", candidate.display())
                        })?;
                        // Relative targets continue from the link's parent.
                        pending.extend(parts(&link));
                    }
                    Ok(_) => resolved = candidate,
                    Err(error) if is_missing(&error) => resolved = candidate,
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("could not inspect policy path {}", candidate.display())
                        });
                    }
                }
            }
        }
    }

    // Spell the existing prefix as `canonicalize` does (a verbatim prefix on
    // Windows) so the result compares with canonical grants.
    let mut existing = resolved.as_path();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(canonical) => {
                let missing = resolved
                    .strip_prefix(existing)
                    .map_err(|_| anyhow!("failed to resolve {}", joined.display()))?;
                return Ok(canonical.join(missing));
            }
            Err(error) if is_missing(&error) => {
                existing = existing
                    .parent()
                    .ok_or_else(|| anyhow!("could not resolve {}", joined.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not resolve {}", existing.display()));
            }
        }
    }
}

/// Every name a deny or protected rule guards: its lexical spelling, which
/// keeps the directory entry itself, and what that entry and the configured
/// spelling resolve to now. Feeding the result back in re-resolves each
/// identity, so the startup names persist alongside any later retarget.
pub(crate) fn guard_identities(base_dir: &Path, configured: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut names = BTreeSet::new();
    for path in configured {
        let joined = base_dir.join(path);
        let lexical = lexical_normalize(&joined);
        names.insert(resolve_policy_path(base_dir, &joined)?);
        if lexical != joined {
            names.insert(resolve_policy_path(base_dir, &lexical)?);
        }
        names.insert(lexical);
    }
    Ok(names.into_iter().collect())
}

/// [`guard_identities`] off the async runtime.
async fn resolve_guards(base_dir: &Path, configured: &[PathBuf]) -> Result<Vec<PathBuf>> {
    crate::blocking::run(|| guard_identities(base_dir, configured))
}

impl AccessPolicy {
    fn from_spec(spec: AccessSpec) -> Result<Self> {
        if !spec.base_dir.is_absolute()
            || spec.read_roots.iter().any(|path| !path.is_absolute())
            || spec.write_roots.iter().any(|path| !path.is_absolute())
            || spec.deny_read_roots.iter().any(|path| !path.is_absolute())
            || spec.deny_write_roots.iter().any(|path| !path.is_absolute())
        {
            bail!("internal error: access policy paths must be canonical absolute paths");
        }

        Ok(Self {
            base_dir: spec.base_dir,
            read_roots: spec.read_roots,
            write_roots: spec.write_roots,
            deny_rules: [spec.deny_read_roots.len(), spec.deny_write_roots.len()],
            deny_read_roots: spec.deny_read_roots,
            deny_write_roots: spec.deny_write_roots,
            unrestricted_fs: spec.unrestricted_fs,
        })
    }

    fn lexical_target(&self, input: &str) -> Result<PathBuf> {
        let path = Path::new(input);
        if contains_parent_component(path) {
            bail!("parent traversal ('..') is not accepted; use a canonical path instead");
        }

        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_dir.join(path)
        };
        let mut normalized = PathBuf::new();
        for component in joined.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => unreachable!("parent components were rejected above"),
                other => normalized.push(other.as_os_str()),
            }
        }
        Ok(normalized)
    }

    fn denied_read(&self, path: &Path) -> bool {
        self.deny_read_roots
            .iter()
            .any(|root| path.starts_with(root))
    }

    fn denied_write(&self, path: &Path) -> bool {
        self.deny_write_roots
            .iter()
            .any(|root| path.starts_with(root))
    }

    fn overlaps_deny_read(&self, path: &Path) -> bool {
        self.deny_read_roots
            .iter()
            .any(|root| paths_overlap(path, root))
    }

    fn overlaps_deny_write(&self, path: &Path) -> bool {
        self.deny_write_roots
            .iter()
            .any(|root| paths_overlap(path, root))
    }

    /// Explicit `--allow-rw=/` is an intentional filesystem opt-out for the
    /// sandboxed shell. It is *not* `--allow-all --no-sandbox`: network, device
    /// and process isolation stay independent, and explicit user denies still
    /// take precedence. Only the mandatory private-path overlap rule is waived.
    #[cfg(target_os = "linux")]
    fn explicit_root_rw_grant(&self) -> bool {
        self.read_roots.iter().any(|root| root == Path::new("/"))
            && self.write_roots.iter().any(|root| root == Path::new("/"))
    }

    fn grant_read(&self, path: &Path) -> bool {
        self.unrestricted_fs || self.read_roots.iter().any(|root| path.starts_with(root))
    }

    fn grant_write(&self, path: &Path) -> bool {
        self.unrestricted_fs || self.write_roots.iter().any(|root| path.starts_with(root))
    }

    fn can_read(&self, path: &Path) -> bool {
        !self.denied_read(path) && self.grant_read(path)
    }

    fn can_write(&self, path: &Path) -> bool {
        !self.denied_write(path) && self.grant_write(path)
    }

    fn check_denies(&self, path: &Path, need: AccessNeed) -> Result<()> {
        match need {
            AccessNeed::Read if self.denied_read(path) => {
                bail!("read access is denied by policy: {}", path.display())
            }
            AccessNeed::Write if self.denied_write(path) => {
                bail!("write access is denied by policy: {}", path.display())
            }
            AccessNeed::ReadWrite if self.denied_read(path) || self.denied_write(path) => {
                bail!("read+write access is denied by policy: {}", path.display())
            }
            _ => Ok(()),
        }
    }

    fn check(&self, path: &Path, need: AccessNeed) -> Result<()> {
        self.check_denies(path, need)?;
        match need {
            AccessNeed::Read if !self.can_read(path) => {
                bail!("read access is not allowed: {}", path.display())
            }
            AccessNeed::Write if !self.can_write(path) => {
                bail!("write access is not allowed: {}", path.display())
            }
            AccessNeed::ReadWrite if !(self.can_read(path) && self.can_write(path)) => {
                bail!("read+write access is not allowed: {}", path.display())
            }
            _ => Ok(()),
        }
    }

    async fn resolve_existing(&self, input: &str, need: AccessNeed) -> Result<PathBuf> {
        let target = self.lexical_target(input)?;
        self.check_denies(&target, need)?;
        let canonical = fs::canonicalize(&target)
            .await
            .with_context(|| format!("path does not exist or cannot be resolved: {input}"))?;
        self.check(&canonical, need)?;
        Ok(canonical)
    }

    async fn resolve_for_create(&self, input: &str, need: AccessNeed) -> Result<PathBuf> {
        let target = self.lexical_target(input)?;
        self.check_denies(&target, need)?;

        match fs::symlink_metadata(&target).await {
            Ok(_) => {
                let canonical = fs::canonicalize(&target)
                    .await
                    .with_context(|| format!("existing path cannot be safely resolved: {input}"))?;
                self.check(&canonical, need)?;
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let mut ancestor = target.clone();
        loop {
            match fs::symlink_metadata(&ancestor).await {
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if !ancestor.pop() {
                        bail!("could not find an existing ancestor for {input}");
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }

        let canonical_ancestor = fs::canonicalize(&ancestor)
            .await
            .with_context(|| format!("existing ancestor cannot be safely resolved: {input}"))?;
        let remainder = target
            .strip_prefix(&ancestor)
            .map_err(|_| anyhow!("failed to resolve path below existing ancestor: {input}"))?;
        let resolved = canonical_ancestor.join(remainder);
        self.check(&resolved, need)?;
        Ok(resolved)
    }

    /// Tool output names paths with `/` on every platform, so one request, one
    /// reply and one transcript read the same on Linux, macOS and Windows. The
    /// runtime still resolves the host spelling internally.
    fn relative_display(&self, path: &Path) -> String {
        let value = path
            .strip_prefix(&self.base_dir)
            .map(|relative| {
                let value = relative.to_string_lossy().to_string();
                if value.is_empty() {
                    ".".to_owned()
                } else {
                    value
                }
            })
            .unwrap_or_else(|_| path.to_string_lossy().to_string());
        if std::path::MAIN_SEPARATOR == '/' {
            return value;
        }
        value.replace(std::path::MAIN_SEPARATOR, "/")
    }

    /// Path label for line-oriented text protocols such as grep content mode.
    /// Ordinary UTF-8 paths retain the familiar `path:line:` shape. Paths that
    /// could inject delimiters/newlines are JSON-quoted; invalid UTF-8 Unix
    /// names use an explicit byte string so distinct files never collapse via
    /// lossy conversion.
    fn relative_text_display(&self, path: &Path) -> String {
        let relative = path.strip_prefix(&self.base_dir).unwrap_or(path);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            let bytes = relative.as_os_str().as_bytes();
            if bytes.is_empty() {
                return ".".to_owned();
            }
            if let Ok(text) = std::str::from_utf8(bytes) {
                return text_path_label(text);
            }
            byte_path_label(bytes)
        }
        #[cfg(not(unix))]
        {
            text_path_label(&self.relative_display(path))
        }
    }

    fn sandbox_mounts(&self) -> Vec<(PathBuf, bool)> {
        if self.unrestricted_fs {
            return vec![(PathBuf::from("/"), true)];
        }

        // Grants are directories, and deny-write roots are mounted read-only. A
        // deny-write root may name a single file, so it is enforced by the
        // shell's dedicated `--ro-bind` pass instead of this directory list.
        let mut candidates = BTreeSet::new();
        candidates.extend(self.read_roots.iter().cloned());
        candidates.extend(self.write_roots.iter().cloned());

        let mut mounts = candidates
            .into_iter()
            .filter_map(|path| {
                if !self.can_read(&path) {
                    return None;
                }
                Some((path.clone(), self.can_write(&path)))
            })
            .collect::<Vec<_>>();
        mounts.sort_by_key(|(path, _)| path_depth(path));
        mounts
    }

    fn has_read_capability(&self) -> bool {
        self.unrestricted_fs && !self.denied_read(Path::new("/"))
            || self.read_roots.iter().any(|root| !self.denied_read(root))
    }

    fn has_write_capability(&self) -> bool {
        self.unrestricted_fs && !self.denied_write(Path::new("/"))
            || self.write_roots.iter().any(|root| !self.denied_write(root))
    }

    fn has_read_write_capability(&self) -> bool {
        if self.unrestricted_fs
            && !self.denied_read(Path::new("/"))
            && !self.denied_write(Path::new("/"))
        {
            return true;
        }

        self.read_roots.iter().any(|read| {
            self.write_roots.iter().any(|write| {
                let overlap = if read.starts_with(write) {
                    read.as_path()
                } else if write.starts_with(read) {
                    write.as_path()
                } else {
                    return false;
                };
                !self.denied_read(overlap) && !self.denied_write(overlap)
            })
        })
    }
}

impl AccessPolicy {
    /// Resolve exactly the same filesystem policy for execution and inspection.
    /// Only filesystem names are resolved here: no secrets, executable discovery,
    /// sandbox initialization or host integrations are involved.
    async fn resolve(spec: AccessSpec) -> Result<Self> {
        let base_dir = fs::canonicalize(&spec.base_dir).await.with_context(|| {
            format!(
                "launch/base directory does not exist: {}",
                spec.base_dir.display()
            )
        })?;
        if !fs::metadata(&base_dir).await?.is_dir() {
            bail!(
                "launch/base directory is not a directory: {}",
                base_dir.display()
            );
        }

        let mut read_roots = Vec::with_capacity(spec.read_roots.len());
        for path in &spec.read_roots {
            read_roots.push(canonicalize_grant(&base_dir, path).await?);
        }
        let mut write_roots = Vec::with_capacity(spec.write_roots.len());
        for path in &spec.write_roots {
            write_roots.push(canonicalize_grant(&base_dir, path).await?);
        }
        // A deny keeps the entry it names as well as its target: retargeting
        // or unlinking a configured alias must not free its lexical name.
        let deny_read_roots = resolve_guards(&base_dir, &spec.deny_read_roots).await?;
        let deny_write_roots = resolve_guards(&base_dir, &spec.deny_write_roots).await?;
        for roots in [&mut read_roots, &mut write_roots] {
            roots.sort();
            roots.dedup();
        }
        let rules = |configured: &[PathBuf]| {
            configured
                .iter()
                .map(|path| lexical_normalize(&base_dir.join(path)))
                .collect::<BTreeSet<_>>()
                .len()
        };
        let deny_rules = [rules(&spec.deny_read_roots), rules(&spec.deny_write_roots)];

        let access = Self::from_spec(AccessSpec {
            base_dir,
            read_roots,
            write_roots,
            deny_read_roots,
            deny_write_roots,
            unrestricted_fs: spec.unrestricted_fs,
        })?;

        Ok(Self {
            deny_rules,
            ..access
        })
    }

    /// The same policy with every deny alias re-resolved now. A sandbox is
    /// assembled from pathnames, so each command masks what an alias names at
    /// that moment as well as what it named at startup.
    #[cfg(target_os = "linux")]
    fn refreshed(&self) -> Result<Self> {
        Ok(Self {
            deny_read_roots: guard_identities(&self.base_dir, &self.deny_read_roots)?,
            deny_write_roots: guard_identities(&self.base_dir, &self.deny_write_roots)?,
            ..self.clone()
        })
    }

    fn capabilities(&self, allow_shell: bool) -> Capabilities {
        Capabilities {
            read: self.has_read_capability(),
            write: self.has_write_capability(),
            read_write: self.has_read_write_capability(),
            shell: allow_shell,
        }
    }
}

/// The guarded names one sandbox command must enforce, resolved when the
/// command is built: deny aliases and protected paths may have been retargeted
/// since startup, so both their startup and current identities apply.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
struct GuardAnchor {
    path: PathBuf,
    writable: bool,
}

#[cfg(target_os = "linux")]
struct ShellGuards {
    access: AccessPolicy,
    protected: Vec<PathBuf>,
    runtime: tools::shell::RuntimeLayout,
    anchors: Vec<GuardAnchor>,
}

/// A sandbox policy the shell cannot enforce as configured. Retrying with the
/// same configuration and host layout fails again: exit `CONFIG`.
#[cfg(target_os = "linux")]
fn shell_policy_conflict(message: String) -> anyhow::Error {
    crate::recovery::operator(anyhow!(message))
}

/// A path mask is lifetime-safe beneath a broad user bind when every mutable
/// directory entry between the bind root and the guarded directory is itself a
/// mountpoint in the sandbox. The shell may still write *inside* those anchored
/// directories, but cannot rename/unlink the namespace chain (`EBUSY`) and make
/// a later host-side recreation of the guarded name appear outside the mask.
///
/// Any existing canonical object can use this mechanism: its eventual mask or
/// read-only rebind makes the leaf itself a mountpoint, while ancestor anchors
/// stabilize the namespace above it. Missing names and symlink spellings remain
/// fail-closed because there is no stable leaf mountpoint at the configured name.
/// Cache/host/runtime integrations keep their stricter overlap rules because
/// they are independent remappings rather than the user's ordinary grant tree.
#[cfg(target_os = "linux")]
fn validate_shell_guards(
    access: &AccessPolicy,
    cache_mounts: &[PreparedCacheMount],
    protected_paths: &[PathBuf],
    runtime: &tools::shell::RuntimeLayout,
) -> Result<Vec<GuardAnchor>> {
    for cache in cache_mounts {
        cache.verify_source_identity()?;
    }
    let guards = protected_paths
        .iter()
        // An explicit root rw grant is a deliberate opt-out of private-file
        // isolation for this shell, but does not waive user-configured denies.
        .filter(|_| !access.explicit_root_rw_grant())
        .map(|path| ("protected path", path, true))
        .chain(
            access
                .deny_read_roots
                .iter()
                .map(|path| ("deny-read path", path, true)),
        )
        .chain(
            access
                .deny_write_roots
                .iter()
                .map(|path| ("deny-write path", path, false)),
        );
    let mounts = access.sandbox_mounts();
    // One hidden canonical directory makes every guarded descendant invisible
    // as well. Standard dotlink state deliberately uses this shape (private
    // directory plus sometimes a redundant active credential file beneath it),
    // so do not reject a child file that is already covered by the parent mask.
    let hidden_directory_ancestors = protected_paths
        .iter()
        .filter(|_| !access.explicit_root_rw_grant())
        .chain(access.deny_read_roots.iter())
        .filter_map(|path| {
            let metadata = std::fs::symlink_metadata(path).ok()?;
            if !metadata.is_dir() {
                return None;
            }
            let canonical = std::fs::canonicalize(path).ok()?;
            (canonical == *path).then_some(canonical)
        })
        .collect::<Vec<_>>();
    let mut anchors = BTreeMap::<PathBuf, bool>::new();
    for (label, guarded, hide) in guards {
        if hidden_directory_ancestors
            .iter()
            .any(|ancestor| ancestor != guarded && guarded.starts_with(ancestor))
        {
            continue;
        }
        for cache in cache_mounts.iter().filter(|cache| hide || cache.writable) {
            if paths_overlap(guarded, &cache.source) || paths_overlap(guarded, &cache.target) {
                return Err(shell_policy_conflict(format!(
                    "cache mount {} -> {} overlaps {label} {}; remove that cache grant",
                    cache.source.display(),
                    cache.target.display(),
                    guarded.display()
                )));
            }
        }
        for (mount, writable) in &mounts {
            if !(hide || *writable) || !paths_overlap(guarded, mount) {
                continue;
            }
            if !guarded.starts_with(mount) || guarded == mount {
                return Err(shell_policy_conflict(format!(
                    "{label} {} contains or equals exposed shell mount {}; narrow the grant to exclude this guarded path",
                    guarded.display(),
                    mount.display()
                )));
            }
            let metadata = std::fs::symlink_metadata(guarded).map_err(|error| {
                shell_policy_conflict(format!(
                    "{label} {} overlaps {} shell mount {} but cannot be lifetime-anchored: {error}",
                    guarded.display(),
                    if *writable { "writable" } else { "readable" },
                    mount.display()
                ))
            })?;
            let canonical = std::fs::canonicalize(guarded).map_err(|error| {
                shell_policy_conflict(format!(
                    "{label} {} overlaps shell mount {} but cannot be resolved for lifetime anchoring: {error}",
                    guarded.display(),
                    mount.display()
                ))
            })?;
            if metadata.file_type().is_symlink() || canonical != *guarded {
                return Err(shell_policy_conflict(format!(
                    "{label} {} overlaps {} shell mount {} but is not an existing canonical object; missing paths and symlink spellings cannot be safely guarded beneath a live broad bind",
                    guarded.display(),
                    if *writable { "writable" } else { "readable" },
                    mount.display()
                )));
            }

            let mut ancestor = guarded.parent();
            while let Some(path) = ancestor {
                if path == mount {
                    break;
                }
                if !path.starts_with(mount) {
                    return Err(shell_policy_conflict(format!(
                        "internal error: guard anchor {} escaped shell mount {}",
                        path.display(),
                        mount.display()
                    )));
                }
                anchors
                    .entry(path.to_path_buf())
                    .or_insert_with(|| access.can_write(path));
                ancestor = path.parent();
            }
        }
        // Runtime directories are live host-backed binds too. Keep their
        // overlap policy strict: they are implicit remappings, not user grant
        // ancestors for which we can preserve the original write semantics.
        if hide {
            for bind in &runtime.binds {
                if paths_overlap(guarded, bind) {
                    return Err(shell_policy_conflict(format!(
                        "runtime mount {} overlaps {label} {}; move the guarded file outside the shell runtime mount or disable shell in this profile",
                        bind.display(),
                        guarded.display()
                    )));
                }
            }
            for (alias, canonical) in &runtime.links {
                if paths_overlap(guarded, alias) || paths_overlap(guarded, canonical) {
                    return Err(shell_policy_conflict(format!(
                        "runtime alias {} -> {} overlaps {label} {}; move the guarded file outside the runtime mount or disable shell in this profile",
                        alias.display(),
                        canonical.display(),
                        guarded.display()
                    )));
                }
            }
        }
        // Procfs and devtmpfs are separately exposed by Bubblewrap even when
        // no user grant mentions them. Their dynamic objects cannot be
        // protected by namespace anchors or pathname masks.
        if hide
            && ["/proc", "/dev"]
                .iter()
                .any(|root| guarded.starts_with(root))
        {
            return Err(shell_policy_conflict(format!(
                "{label} {} is exposed by the sandbox's virtual /proc or /dev mount; this guard cannot be enforced by a path mask",
                guarded.display()
            )));
        }
    }
    let mut anchors = anchors
        .into_iter()
        .map(|(path, writable)| GuardAnchor { path, writable })
        .collect::<Vec<_>>();
    anchors.sort_by_key(|anchor| path_depth(&anchor.path));
    Ok(anchors)
}

#[cfg(target_os = "linux")]
fn validate_host_mount_policy(
    access: &AccessPolicy,
    host_shell: &crate::host_services::HostShell,
    protected_paths: &[PathBuf],
) -> Result<()> {
    for mount in &host_shell.mounts {
        // Check both names captured when the mount was pinned, and the
        // sandbox target. A directory source can contain a guarded
        // descendant; a source below a guarded root is equally disallowed.
        // Sockets and devices act rather than hold data: a read-only bind
        // still connects or issues ioctls, so deny-write blocks them too.
        let blocked = [
            mount.source.as_path(),
            mount.resolved.as_path(),
            mount.target.as_path(),
        ]
        .into_iter()
        .any(|path| {
            access.overlaps_deny_read(path)
                || (mount.kind.is_operational() && access.overlaps_deny_write(path))
                || (!access.explicit_root_rw_grant()
                    && protected_paths
                        .iter()
                        .any(|protected| paths_overlap(path, protected)))
        });
        if blocked {
            return Err(shell_policy_conflict(format!(
                "host integration source or target is denied or protected: {} -> {}",
                mount.source.display(),
                mount.target.display()
            )));
        }
    }
    Ok(())
}

pub(crate) fn authority_path_identity(path: &Path) -> serde_json::Value {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = std::fs::metadata(&resolved).ok();
        serde_json::json!({
            "path": resolved,
            "dev": metadata.as_ref().map(|value| value.dev()),
            "ino": metadata.as_ref().map(|value| value.ino()),
            "rdev": metadata.as_ref().map(|value| value.rdev()),
        })
    }
    #[cfg(windows)]
    {
        let identity = crate::windows_fs::path_identity(&resolved).ok();
        serde_json::json!({
            "path": resolved,
            "volume": identity.map(|value| value.0),
            "file_index": identity.map(|value| value.1),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        serde_json::json!({ "path": resolved })
    }
}

fn prepared_authority_value(
    config: &RuntimeConfig,
    #[cfg(any(target_os = "linux", target_os = "macos"))] secure_fs: &crate::secure_fs::SecureFs,
) -> Result<serde_json::Value> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let (read_roots, write_roots) = {
        let (read, write) = secure_fs.authority_roots()?;
        let render = |values: Vec<crate::secure_fs::RootIdentity>| {
            values
                .into_iter()
                .map(|value| {
                    serde_json::json!({
                        "path": value.path,
                        "dev": value.dev,
                        "ino": value.ino,
                        "rdev": value.rdev,
                    })
                })
                .collect::<Vec<_>>()
        };
        (render(read), render(write))
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let (read_roots, write_roots) = (
        config
            .access
            .read_roots
            .iter()
            .map(|path| authority_path_identity(path))
            .collect::<Vec<_>>(),
        config
            .access
            .write_roots
            .iter()
            .map(|path| authority_path_identity(path))
            .collect::<Vec<_>>(),
    );
    let caches = config
        .cache_mounts
        .iter()
        .map(|cache| -> Result<serde_json::Value> {
            #[cfg(target_os = "linux")]
            {
                let identity =
                    crate::secure_fs::file_identity(&cache.pinned).with_context(|| {
                        format!(
                            "cannot fingerprint prepared cache source {}",
                            cache.source.display()
                        )
                    })?;
                Ok(serde_json::json!({
                    "source": cache.source,
                    "target": cache.target,
                    "writable": cache.writable,
                    "dev": identity.dev,
                    "ino": identity.ino,
                    "rdev": identity.rdev,
                }))
            }
            #[cfg(not(target_os = "linux"))]
            {
                Ok(serde_json::json!({
                    "source": authority_path_identity(&cache.source),
                    "target": cache.target,
                    "writable": cache.writable,
                }))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let mut authority = serde_json::json!({
        "base_dir": authority_path_identity(&config.access.base_dir),
        "read_roots": read_roots,
        "write_roots": write_roots,
        "deny_read_roots": config.access.deny_read_roots.iter().map(|path| authority_path_identity(path)).collect::<Vec<_>>(),
        "deny_write_roots": config.access.deny_write_roots.iter().map(|path| authority_path_identity(path)).collect::<Vec<_>>(),
        "protected_paths": config.protected_paths.iter().map(|path| authority_path_identity(path)).collect::<Vec<_>>(),
        "cache_mounts": caches,
    });
    #[cfg(target_os = "linux")]
    {
        let mounts = config
            .host_shell
            .mounts
            .iter()
            .map(|mount| -> Result<serde_json::Value> {
                let identity =
                    crate::secure_fs::file_identity(&mount.pinned).with_context(|| {
                        format!(
                            "cannot fingerprint pinned host integration {}",
                            mount.source.display()
                        )
                    })?;
                Ok(serde_json::json!({
                    "source": mount.source,
                    "resolved": mount.resolved,
                    "target": mount.target,
                    "kind": format!("{:?}", mount.kind),
                    "dev": identity.dev,
                    "ino": identity.ino,
                    "rdev": identity.rdev,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        authority["host_shell"] = serde_json::json!({
            "mounts": mounts,
            "environment": config.host_shell.environment,
            "nix_daemon": config.host_shell.nix_daemon,
            "ssh_agent": config.host_shell.ssh_agent,
            "kvm": config.host_shell.kvm,
            "gpu": config.host_shell.gpu,
        });
    }
    Ok(authority)
}

impl PreparedMachine {
    pub(crate) fn authority_value(&self) -> &serde_json::Value {
        &self.authority
    }
}

impl LocalMachine {
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn new(args: MachineConfig) -> Result<Self> {
        let retention = checked_job_retention(args.job_retention_secs)?;
        let jobs = crate::jobs::JobManager::with_spool_limit(retention, args.job_spool_bytes)?;
        Self::new_with_jobs(args, jobs).await
    }

    pub(crate) async fn prepare_with_job_root(
        args: MachineConfig,
        job_root: &Path,
    ) -> Result<PreparedMachine> {
        let max_shell_timeout_secs = checked_limit(
            "max_shell_timeout_secs",
            args.max_shell_timeout_secs,
            MIN_SHELL_TIMEOUT_SECS,
        )?;
        let max_output_bytes = checked_byte_limit("max_output_bytes", args.max_output_bytes)?;
        let max_read_bytes = checked_byte_limit("max_read_bytes", args.max_read_bytes)?;
        let max_write_bytes = checked_byte_limit("max_write_bytes", args.max_write_bytes)?;
        let retention = checked_job_retention(args.job_retention_secs)?;

        let access = AccessPolicy::resolve(args.access).await?;
        let mut configured_protected = args.protected_paths.clone();
        configured_protected.push(job_root.to_path_buf());
        #[cfg(target_os = "linux")]
        if args.allow_shell && args.sandbox_shell {
            for private_root in [
                crate::setup::dotlink_config_dir()?,
                crate::oauth::state_dir()?,
            ] {
                if configured_protected
                    .iter()
                    .any(|path| path == &private_root)
                {
                    crate::private_state::prepare_owned_dir(&private_root)?;
                }
            }
        }
        let protected_paths = resolve_guards(&access.base_dir, &configured_protected).await?;

        let mut cache_mounts = Vec::with_capacity(args.cache_mounts.len());
        let mut cache_targets = BTreeSet::new();
        for mount in args.cache_mounts {
            if !mount.target.has_root()
                || !mount.target.starts_with("/tmp/home")
                || mount
                    .target
                    .components()
                    .any(|component| component == Component::ParentDir)
            {
                bail!(
                    "internal error: cache sandbox target must stay below /tmp/home: {}",
                    mount.target.display()
                );
            }
            let target = lexical_normalize(&mount.target);
            let source = match fs::canonicalize(&mount.source).await {
                Ok(source) => source,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !fs::metadata(&source).await?.is_dir() {
                continue;
            }
            if access.overlaps_deny_read(&source)
                || access.overlaps_deny_read(&target)
                || protected_paths.iter().any(|protected| {
                    paths_overlap(&source, protected) || paths_overlap(&target, protected)
                })
            {
                continue;
            }
            let writable = mount.writable
                && !access.overlaps_deny_write(&source)
                && !access.overlaps_deny_write(&target);
            if !cache_targets.insert(target.clone()) {
                bail!(
                    "internal error: duplicate cache sandbox target: {}",
                    target.display()
                );
            }
            #[cfg(target_os = "linux")]
            let pinned = Arc::new(crate::secure_fs::SecureFs::open_canonical_directory(
                &source,
            )?);
            cache_mounts.push(PreparedCacheMount {
                source,
                target,
                writable,
                #[cfg(target_os = "linux")]
                pinned,
            });
        }

        #[cfg(target_os = "linux")]
        if args.allow_shell && args.sandbox_shell {
            let runtime = tools::shell::RuntimeLayout::for_shell(
                args.allow_network,
                args.host_shell.ssh_agent,
            )?;
            let _ = validate_shell_guards(&access, &cache_mounts, &protected_paths, &runtime)?;
        }
        #[cfg(target_os = "linux")]
        validate_host_mount_policy(&access, &args.host_shell, &protected_paths)?;

        let shell_program = if args.allow_shell {
            #[cfg(windows)]
            {
                Some(
                    resolve_executable("pwsh.exe")
                        .or_else(|| resolve_executable("powershell.exe"))
                        .ok_or_else(|| anyhow!("PowerShell was not found in PATH"))?,
                )
            }
            #[cfg(not(windows))]
            {
                Some(
                    resolve_executable("bash")
                        .ok_or_else(|| anyhow!("bash was not found in PATH"))?,
                )
            }
        } else {
            None
        };

        #[cfg(target_os = "linux")]
        let bwrap_program = if args.allow_shell && args.sandbox_shell {
            let program = resolve_executable("bwrap").ok_or_else(|| {
                anyhow!("Bubblewrap (bwrap) is required for sandboxed shell access")
            })?;
            tools::shell::validate_bubblewrap(&program).await?;
            Some(program)
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        let bwrap_program = None;

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let secure_fs = Arc::new(crate::secure_fs::SecureFs::new(
            &access.read_roots,
            &access.write_roots,
        )?);

        let config = RuntimeConfig {
            access,
            cache_mounts,
            shell_env: args.shell_env,
            #[cfg(target_os = "linux")]
            host_shell: args.host_shell,
            log: args.log,
            allow_shell: args.allow_shell,
            sandbox_shell: args.sandbox_shell,
            allow_network: args.allow_network,
            shell_program,
            bwrap_program,
            max_shell_timeout_secs,
            max_output_bytes,
            max_read_bytes,
            max_write_bytes,
            task_result_budget: args.task_result_budget,
            protected_paths,
        };
        let authority = prepared_authority_value(
            &config,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            &secure_fs,
        )?;
        Ok(PreparedMachine {
            config,
            authority,
            job_root: job_root.to_path_buf(),
            serialize: args.serialize,
            retention,
            spool_limit: args.job_spool_bytes,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            secure_fs,
        })
    }

    pub(crate) fn from_prepared(
        prepared: PreparedMachine,
        jobs: crate::jobs::JobManager,
    ) -> Result<Self> {
        if prepared.job_root != jobs.protected_path() {
            bail!("internal error: prepared machine belongs to a different job session");
        }
        jobs.set_limits(prepared.retention, prepared.spool_limit);
        let operation_gate = jobs.operation_gate();
        Ok(Self {
            config: prepared.config,
            jobs,
            operation_gate,
            serialize: prepared.serialize,
            transport_context_token: TransportContextToken::random(),
            router: OnceLock::new(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            secure_fs: prepared.secure_fs,
        })
    }

    pub(crate) async fn new_with_jobs(
        args: MachineConfig,
        jobs: crate::jobs::JobManager,
    ) -> Result<Self> {
        let prepared = Self::prepare_with_job_root(args, jobs.protected_path()).await?;
        Self::from_prepared(prepared, jobs)
    }

    /// Re-resolve every guarded name and rerun the startup checks, so one
    /// sandbox command enforces exactly what the host names now.
    #[cfg(all(test, target_os = "linux"))]
    fn shell_guards(&self) -> Result<ShellGuards> {
        let access = self.config.access.refreshed()?;
        let protected = guard_identities(&access.base_dir, &self.config.protected_paths)?;
        let runtime = tools::shell::RuntimeLayout::for_shell(
            self.config.allow_network,
            self.config.host_shell.ssh_agent,
        )?;
        let anchors =
            validate_shell_guards(&access, &self.config.cache_mounts, &protected, &runtime)?;
        validate_host_mount_policy(&access, &self.config.host_shell, &protected)?;
        Ok(ShellGuards {
            access,
            protected,
            runtime,
            anchors,
        })
    }

    pub fn log(&self) -> &LogConfig {
        &self.config.log
    }

    pub fn shell_enabled(&self) -> bool {
        self.config.allow_shell
    }

    pub fn shell_sandboxed(&self) -> bool {
        self.config.sandbox_shell
    }

    pub fn network_enabled(&self) -> bool {
        self.config.allow_network
    }

    #[cfg(target_os = "linux")]
    pub fn host_access_summary(&self) -> Option<String> {
        let shell = &self.config.host_shell;
        let mut enabled = Vec::new();
        if shell.nix_daemon {
            enabled.push("nix daemon");
        }
        if shell
            .mounts
            .iter()
            .any(|mount| mount.target == Path::new("/tmp/home/.config/nix/nix.conf"))
        {
            enabled.push("nix config");
        }
        if shell.mounts.iter().any(|mount| {
            mount.target == Path::new("/tmp/home/.gitconfig")
                || mount.target == Path::new("/tmp/home/.config/git/config")
        }) {
            enabled.push("git config");
        }
        if shell.ssh_agent {
            enabled.push("ssh agent");
        }
        if shell.kvm {
            enabled.push("kvm");
        }
        for gpu in &shell.gpu {
            enabled.push(gpu.label());
        }
        if enabled.is_empty() {
            None
        } else {
            Some(enabled.join(", "))
        }
    }

    /// The largest request body a tool call can need for profile-derived local
    /// Streamable HTTP. It follows the write ceiling so local HTTP never keeps
    /// rmcp's smaller hidden default. The OpenAI embedded endpoint deliberately
    /// clamps this further to its fixed 9 MiB embedded-message budget.
    pub(crate) fn max_request_body_bytes(&self) -> usize {
        self.config
            .max_write_bytes
            .saturating_mul(MAX_JSON_ESCAPE)
            .saturating_add(tools::MAX_EDIT_OPS.saturating_mul(EDIT_PATH_ENVELOPE_BYTES))
            .saturating_add(MAX_REQUEST_FRAMING_BYTES)
    }

    /// Profile-derived local response ceiling. Ordinary tool text can expand
    /// sixfold in JSON; shell has two independently capped streams nested in a
    /// JSON string. Local HTTP can use this full bound. The OpenAI embedded MCP
    /// endpoint clamps it to the fixed 9 MiB embedded-message budget.
    pub(crate) fn max_response_body_bytes(&self) -> usize {
        let read = self
            .config
            .max_read_bytes
            .saturating_mul(MAX_JSON_ESCAPE)
            .saturating_add(MAX_REQUEST_FRAMING_BYTES);
        let shell = shell_result_worst_case_bytes(self.config.max_output_bytes);
        read.max(shell)
    }

    pub fn access_summary(&self) -> String {
        if self.config.access.unrestricted_fs {
            return "unrestricted read+write".to_owned();
        }
        format!(
            "read:{} write:{} deny-read:{} deny-write:{} caches:{}{}",
            self.config.access.read_roots.len(),
            self.config.access.write_roots.len(),
            self.config.access.deny_rules[0],
            self.config.access.deny_rules[1],
            self.config.cache_mounts.len(),
            if self.config.allow_shell {
                " + shell"
            } else {
                ""
            }
        )
    }

    fn ensure_not_protected(&self, path: &Path) -> Result<()> {
        for protected in &self.config.protected_paths {
            if path == protected || path.starts_with(protected) {
                bail!(
                    "path is protected by dotlink and cannot be accessed through filesystem tools"
                );
            }
        }
        Ok(())
    }

    /// One guard spans planning through commit, shared by every mutating tool.
    pub(crate) async fn lock_mutations<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
    ) -> Result<crate::mutation::MutationGuard> {
        crate::mutation::lock(paths).await
    }

    async fn resolve_existing(&self, input: &str, need: AccessNeed) -> Result<PathBuf> {
        let lexical = self.config.access.lexical_target(input)?;
        self.ensure_not_protected(&lexical)?;
        let resolved = self.config.access.resolve_existing(input, need).await?;
        self.ensure_not_protected(&resolved)?;
        Ok(resolved)
    }

    async fn resolve_for_create(&self, input: &str, need: AccessNeed) -> Result<PathBuf> {
        let lexical = self.config.access.lexical_target(input)?;
        self.ensure_not_protected(&lexical)?;
        let resolved = self.config.access.resolve_for_create(input, need).await?;
        self.ensure_not_protected(&resolved)?;
        Ok(resolved)
    }

    #[cfg(any(test, windows))]
    async fn require_regular_file(&self, path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path).await?;
        if !metadata.is_file() {
            bail!("path is not a regular file: {}", path.display());
        }
        Ok(())
    }

    // Tokio file writes may finish on a background blocking task even after
    // write_all returns. Flush before acknowledging a mutating MCP call.
    // Size checks before reading are not sufficient: a concurrent shell can
    // extend a file while it is being read. Bound actual I/O to one byte over
    // the limit so even adversarial growth cannot cause unbounded allocation.
    async fn read_bounded(file: &mut fs::File, limit: usize) -> Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
        let mut bounded = file.take(limit.saturating_add(1) as u64);
        bounded.read_to_end(&mut bytes).await?;
        if bytes.len() > limit {
            bail!("file exceeded the {} byte read limit", limit);
        }
        Ok(bytes)
    }

    async fn open_read_file(&self, path: &Path) -> Result<fs::File> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            Ok(fs::File::from_std(self.secure_fs.open_read(path)?))
        }
        #[cfg(windows)]
        {
            Ok(fs::File::from_std(crate::windows_fs::open_file(
                path, false,
            )?))
        }
    }

    async fn open_edit_file(&self, path: &Path) -> Result<fs::File> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            Ok(fs::File::from_std(self.secure_fs.open_edit(path)?))
        }
        #[cfg(windows)]
        {
            Ok(fs::File::from_std(crate::windows_fs::open_file(
                path, true,
            )?))
        }
    }

    /// Replace a file's content without ever exposing a partial write, and
    /// without truncating the original: the bytes are staged beside the target
    /// and renamed over it, preserving the existing mode.
    pub(crate) async fn write_file_atomic(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        self.commit_write(self.stage_replace_write(path, bytes).await?)
            .await
    }

    /// Prepare an in-place update: the bytes are flushed beside the target and
    /// removed again if the value is dropped without being committed. The
    /// expected length/digest is re-checked at commit.
    pub(crate) async fn stage_update_write(
        &self,
        path: &Path,
        bytes: &[u8],
        expected: (u64, [u8; 32]),
    ) -> Result<PendingWrite> {
        self.stage_write_mode(path, bytes, false, false, Some(expected))
            .await
    }

    /// Descriptor-pinned streamed replacement. Unlike `stage_update_write`,
    /// the caller produces output in bounded chunks instead of allocating a
    /// full replacement. Both backends reuse their existing temp ownership,
    /// mode/ACL handling, digest precondition and atomic commit path.
    pub(crate) async fn stage_update_with(
        &self,
        path: &Path,
        expected: (u64, [u8; 32]),
        write: impl FnOnce(&mut std::fs::File) -> Result<()>,
    ) -> Result<PendingWrite> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            Ok(PendingWrite(crate::blocking::run(|| {
                self.secure_fs.stage_update_with(path, expected, write)
            })?))
        }
        #[cfg(windows)]
        {
            Ok(PendingWrite(
                crate::staging::stage_with_path(path, false, Some(expected), write).await?,
            ))
        }
    }

    /// Prepare a create-or-replace write that needs only write authority.
    pub(crate) async fn stage_replace_write(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> Result<PendingWrite> {
        self.stage_write_mode(path, bytes, false, true, None).await
    }

    /// Prepare a create-only write. The target is installed only when the
    /// staged value is committed, so a reader never observes an empty file and
    /// an existing path cannot be replaced.
    pub(crate) async fn stage_create_write(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> Result<PendingWrite> {
        self.stage_write_mode(path, bytes, true, true, None).await
    }

    async fn stage_write_mode(
        &self,
        path: &Path,
        bytes: &[u8],
        create_only: bool,
        write_only: bool,
        expected: Option<(u64, [u8; 32])>,
    ) -> Result<PendingWrite> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            // Run descriptor-rooted staging as one synchronous segment:
            // cancellation cannot outlive its caller's mutation lock.
            let staged = crate::blocking::run(|| match (create_only, write_only) {
                (true, _) => self.secure_fs.stage_create(path, bytes),
                (false, true) => self.secure_fs.stage(path, bytes),
                (false, false) => {
                    let expected = expected.ok_or_else(|| {
                        anyhow!("internal error: an update requires an expected snapshot")
                    })?;
                    self.secure_fs.stage_update(path, bytes, expected)
                }
            })?;
            Ok(PendingWrite(staged))
        }
        #[cfg(windows)]
        {
            let _ = write_only;
            Ok(PendingWrite(
                crate::staging::stage_path(path, bytes, create_only, expected).await?,
            ))
        }
    }

    /// Make a staged write visible.
    pub(crate) async fn commit_write(&self, pending: PendingWrite) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            crate::blocking::run(|| pending.0.commit())
        }
        #[cfg(windows)]
        {
            pending.0.commit().await
        }
    }
}

impl LocalMachine {
    /// Every advertised tool, merged from the per-family routers. Each family
    /// names its own `#[tool_router]`, so tools stay with their implementation.
    fn all_tools() -> ToolRouter<Self> {
        let mut router = Self::discovery_router();
        router.merge(Self::text_router());
        router.merge(Self::grep_router());
        router.merge(Self::edit_router());
        router.merge(Self::patch_router());
        router.merge(Self::binary_router());
        router.merge(Self::shell_router());
        router.merge(Self::jobs_router());
        if let Some(edit) = router.map.get_mut("edit") {
            tools::edit::with_edit_limits(&mut edit.attr);
        }
        if let Some(bedit) = router.map.get_mut("bedit") {
            tools::binary::with_bedit_limits(&mut bedit.attr);
        }
        router
    }

    pub fn tool_router_for_policy(capabilities: Capabilities) -> ToolRouter<Self> {
        let mut router = Self::all_tools();
        for spec in TOOL_SPECS {
            if !capabilities.allows(spec.capability) {
                router.disable_route(spec.name);
            }
        }
        router
    }

    fn capabilities(&self) -> Capabilities {
        self.config.access.capabilities(self.config.allow_shell)
    }

    fn trusted_transport_meta<'a>(
        &self,
        context: &'a RequestContext<RoleServer>,
    ) -> Option<&'a serde_json::Map<String, Value>> {
        let meta = &context.meta.0;
        (meta
            .get(TRANSPORT_CONTEXT_TOKEN_META_KEY)
            .and_then(Value::as_str)
            == Some(self.transport_context_token.as_str()))
        .then_some(meta)
    }

    pub(crate) fn request_result_budget(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Option<usize> {
        self.trusted_transport_meta(context)?
            .get(RESULT_BUDGET_META_KEY)?
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
    }

    pub(crate) fn request_sync_budget_millis(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Option<u64> {
        self.trusted_transport_meta(context)?
            .get(SYNC_BUDGET_META_KEY)?
            .as_u64()
    }

    pub(crate) fn request_job_key(&self, context: &RequestContext<RoleServer>) -> Option<String> {
        self.trusted_transport_meta(context)?
            .get(REQUEST_KEY_META_KEY)?
            .as_str()
            .map(str::to_owned)
    }

    pub(crate) fn transport_context_token(&self) -> &str {
        self.transport_context_token.as_str()
    }

    pub(crate) fn task_result_budget(&self, request_budget: Option<usize>) -> Option<usize> {
        match (self.config.task_result_budget, request_budget) {
            (Some(runtime), Some(request)) => Some(runtime.min(request)),
            (Some(runtime), None) => Some(runtime),
            (None, request) => request,
        }
    }

    pub(crate) fn acknowledge_job_request_delivery(&self, request_key: &str) {
        self.jobs.acknowledge_request_delivery(request_key);
    }

    fn policy_tool_router(&self) -> &ToolRouter<Self> {
        self.router
            .get_or_init(|| Arc::new(Self::tool_router_for_policy(self.capabilities())))
    }

    /// Non-caching policy lookup used by HTTP transports before rmcp's
    /// SEP-2243 schema cache sees an untrusted tool name.
    pub(crate) fn has_tool_for_policy(&self, name: &str) -> bool {
        self.policy_tool_router().get(name).is_some()
    }

    /// Whether a SEP-2243 schema-routed argument could be reflected by rmcp
    /// into a pre-handler mismatch diagnostic above the routing ceiling.
    pub(crate) fn has_oversized_routing_param(
        &self,
        name: &str,
        arguments: Option<&serde_json::Map<String, Value>>,
        limit: usize,
    ) -> bool {
        let Some(arguments) = arguments else {
            return false;
        };
        let Some(properties) = self
            .policy_tool_router()
            .get(name)
            .and_then(|tool| tool.input_schema.get("properties"))
            .and_then(Value::as_object)
        else {
            return false;
        };
        properties.iter().any(|(property, schema)| {
            schema.get("x-mcp-header").and_then(Value::as_str).is_some()
                && arguments
                    .get(property)
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.len() > limit)
        })
    }

    /// Pure reads may overlap each other; everything else takes the operation
    /// gate on platforms without descriptor-rooted IO.
    #[cfg(any(test, windows))]
    fn is_concurrent_read_tool(name: &str) -> bool {
        TOOL_SPECS
            .iter()
            .any(|spec| spec.name == name && spec.concurrent_read)
    }

    /// Job observation/cancellation is control-plane traffic for work that may
    /// itself own the execution gate. Putting these calls behind that same gate
    /// would make a serialized job impossible to poll or cancel. Starting a job
    /// is deliberately excluded: it acquires and transfers the gate to the job.
    fn is_job_control_tool(name: &str) -> bool {
        matches!(
            name,
            "job_list" | "job_get" | "job_output" | "job_ack" | "job_cancel"
        )
    }

    fn describe_tools(tools: impl IntoIterator<Item = Tool>) -> Vec<(String, String)> {
        tools
            .into_iter()
            .map(|tool| {
                let title = tool
                    .title
                    .as_deref()
                    .or_else(|| tool.annotations.as_ref().and_then(|a| a.title.as_deref()))
                    .or(tool.description.as_deref())
                    .unwrap_or("")
                    .to_owned();
                (tool.name.to_string(), title)
            })
            .collect()
    }

    /// An inspection-only path: same canonical policy, same tool router, but
    /// no machine/runtime or transport side effects.
    pub async fn inspect_tool_descriptions(
        access: AccessSpec,
        allow_shell: bool,
    ) -> Result<Vec<(String, String)>> {
        let capabilities = AccessPolicy::resolve(access)
            .await?
            .capabilities(allow_shell);
        Ok(Self::describe_tools(
            Self::tool_router_for_policy(capabilities).list_all(),
        ))
    }

    /// The tool's own argument names, in schema order. The TOOL log renders
    /// exactly these keys, so a new argument is logged without a second list.
    fn argument_names(&self, name: &str) -> Vec<String> {
        self.policy_tool_router()
            .get(name)
            .and_then(|tool| tool.input_schema.get("properties"))
            .and_then(Value::as_object)
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn summarize_tool_call_arguments(
        &self,
        name: &str,
        arguments: Option<&serde_json::Map<String, Value>>,
    ) -> String {
        let Some(arguments) = arguments else {
            return String::new();
        };
        let names = self.argument_names(name);
        self.config.log.summarize_arguments(
            name,
            arguments,
            names.iter().map(String::as_str),
            Some(&self.config.access.base_dir),
        )
    }
}

#[tool_handler(
    router = self.policy_tool_router(),
    name = "dotlink",
    instructions = "Private local-machine bridge. Filesystem access follows additive allow-read/allow-write/allow-rw grants; deny rules take precedence. Shell is hidden unless enabled. Linux shell execution is Bubblewrap-sandboxed by default with network disabled unless explicitly allowed."
)]
impl rmcp::ServerHandler for LocalMachine {
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools: self
                .policy_tool_router()
                .list_all()
                .into_iter()
                .map(|tool| {
                    let mut tool = with_profile_limits(
                        tool,
                        self.config.max_shell_timeout_secs,
                        self.config.max_output_bytes,
                    );
                    tools::text::with_read_limit(&mut tool, self.config.max_read_bytes);
                    tools::binary::with_read_limit(&mut tool, self.config.max_read_bytes);
                    tools::jobs::with_job_limits(
                        &mut tool,
                        self.config.max_shell_timeout_secs,
                        self.config.max_output_bytes,
                    );
                    tool
                })
                .collect(),
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(CacheScope::Public),
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.policy_tool_router().get(name).cloned().map(|tool| {
            let mut tool = with_profile_limits(
                tool,
                self.config.max_shell_timeout_secs,
                self.config.max_output_bytes,
            );
            tools::text::with_read_limit(&mut tool, self.config.max_read_bytes);
            tools::binary::with_read_limit(&mut tool, self.config.max_read_bytes);
            tools::jobs::with_job_limits(
                &mut tool,
                self.config.max_shell_timeout_secs,
                self.config.max_output_bytes,
            );
            tool
        })
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tasks()
                .build(),
        )
        .with_server_info(Implementation::new("dotlink", env!("CARGO_PKG_VERSION")))
        .with_instructions("Private local-machine bridge. Filesystem access follows additive allow-read/allow-write/allow-rw grants; deny rules take precedence. Shell is hidden unless enabled. Linux shell execution is Bubblewrap-sandboxed by default with network disabled unless explicitly allowed. Long shell calls may become durable MCP tasks/jobs instead of being shortened to a transport deadline.".to_owned())
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        let budget = self.request_result_budget(&context);
        let result = self
            .jobs
            .detailed_task(&request.task_id)
            .map(GetTaskResult::new)
            .map_err(|error| fit_mcp_error_to_budget(error.into_mcp(), budget))?;
        if budget.is_some_and(|limit| serialized_json_len(&result).is_none_or(|size| size > limit))
        {
            return Err(fit_mcp_error_to_budget(
                McpError::internal_error(
                    "native task result exceeded its transport response budget",
                    None,
                ),
                budget,
            ));
        }
        Ok(result)
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        // Shell jobs never enter input_required. Validate the shared task id so
        // unknown-task behavior stays precise, then reject an update that has no
        // outstanding server request instead of silently accepting unused input.
        self.jobs
            .ensure_native_task(&request.task_id)
            .map_err(|error| {
                fit_mcp_error_to_budget(error.into_mcp(), self.request_result_budget(&context))
            })?;
        Err(fit_mcp_error_to_budget(
            McpError::invalid_params("shell tasks do not request mid-task input", None),
            self.request_result_budget(&context),
        ))
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let budget = self.request_result_budget(&context);
        self.jobs
            .ensure_native_task(&request.task_id)
            .map_err(|error| fit_mcp_error_to_budget(error.into_mcp(), budget))?;
        self.jobs
            .cancel(&request.task_id)
            .map(|_| ())
            .map_err(|error| fit_mcp_error_to_budget(error.into_mcp(), budget))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        let result_budget = self.request_result_budget(&context);
        // Building the router to read the schema is not free, so only do it when
        // the detail line will actually be printed.
        let detail = match self.config.log.activity_enabled() {
            true => self.summarize_tool_call_arguments(&name, request.arguments.as_ref()),
            false => String::new(),
        };
        let started = self.config.log.activity_start(&name, detail);

        let shell_result = self
            .handle_shell_call(&request, &context)
            .await
            .map_err(|error| fit_mcp_error_to_budget(error, result_budget))?;
        if let Some(result) = shell_result {
            let result = fit_call_tool_response_to_budget(&name, result, result_budget);
            let outcome = match &result {
                CallToolResponse::Complete(result) if result.is_error.unwrap_or(false) => {
                    LogOutcome::error("error")
                }
                CallToolResponse::Task(_) => LogOutcome::neutral("task"),
                _ => LogOutcome::success("ok"),
            };
            self.config.log.activity_done(&name, started, outcome);
            return Ok(result);
        }

        let context = ToolCallContext::new(self, request, context);

        // Linux native filesystem tools use descriptor-rooted operations;
        // independent MCP requests may execute concurrently by default.
        // --serialize opts into exclusive execution for predictable ordering.
        // Other platforms retain their conservative operation gate until
        // equivalent secure handle-based filesystem access is available.
        let result = if Self::is_job_control_tool(&name) {
            self.policy_tool_router().call(context).await
        } else if self.serialize {
            let _guard = self.operation_gate.write().await;
            self.policy_tool_router().call(context).await
        } else {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                // Native file operations use checked directory handles, and
                // Bubblewrap gives each shell its own process/filesystem view.
                self.policy_tool_router().call(context).await
            }
            #[cfg(windows)]
            {
                // Until Windows gets descriptor-relative tools,
                // preserve their conservative filesystem topology gate.
                if Self::is_concurrent_read_tool(&name) {
                    let _guard = self.operation_gate.read().await;
                    self.policy_tool_router().call(context).await
                } else {
                    let _guard = self.operation_gate.write().await;
                    self.policy_tool_router().call(context).await
                }
            }
        };

        let result = result
            .map(|response| fit_call_tool_response_to_budget(&name, response, result_budget))
            .map_err(|error| fit_mcp_error_to_budget(error, result_budget));
        let outcome = match &result {
            Ok(CallToolResponse::Complete(result)) if result.is_error.unwrap_or(false) => {
                LogOutcome::error("error")
            }
            Ok(CallToolResponse::Complete(_)) => LogOutcome::success("ok"),
            Ok(CallToolResponse::InputRequired(_)) => LogOutcome::neutral("input-required"),
            Ok(CallToolResponse::Task(_)) => LogOutcome::neutral("task"),
            Ok(_) => LogOutcome::success("ok"),
            Err(_) => LogOutcome::error("protocol-error"),
        };
        self.config.log.activity_done(&name, started, outcome);
        result
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncSeekExt;

    use super::*;

    use crate::mcp::tools::test_support::access;

    #[tokio::test]
    async fn inspection_and_runtime_advertise_the_same_policy_tool_surface() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = crate::mcp::tools::test_support::test_machine(root.clone());
        let inspect = LocalMachine::inspect_tool_descriptions(
            AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root.clone()],
                write_roots: vec![root],
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            inspect,
            LocalMachine::describe_tools(machine.policy_tool_router().list_all())
        );
    }

    #[test]
    fn default_access_is_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let policy = access(&root);
        assert!(policy.can_read(&root));
        assert!(!policy.can_write(&root));
    }

    #[test]
    fn additive_read_and_write_combine_to_rw() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let child = root.join("child");
        std::fs::create_dir(&child).unwrap();

        let policy = AccessPolicy::from_spec(AccessSpec {
            base_dir: root.clone(),
            read_roots: vec![root],
            write_roots: vec![child.clone()],
            deny_read_roots: Vec::new(),
            deny_write_roots: Vec::new(),
            unrestricted_fs: false,
        })
        .unwrap();

        assert!(policy.can_read(&child));
        assert!(policy.can_write(&child));
    }

    #[test]
    fn deny_takes_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let denied = root.join("denied");
        std::fs::create_dir(&denied).unwrap();

        let policy = AccessPolicy::from_spec(AccessSpec {
            base_dir: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root],
            deny_read_roots: vec![denied.clone()],
            deny_write_roots: vec![denied.clone()],
            unrestricted_fs: false,
        })
        .unwrap();

        assert!(!policy.can_read(&denied));
        assert!(!policy.can_write(&denied));
    }

    #[test]
    fn read_and_write_denies_are_independent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let read_denied = root.join("read-denied");
        let write_denied = root.join("write-denied");
        std::fs::create_dir(&read_denied).unwrap();
        std::fs::create_dir(&write_denied).unwrap();

        let policy = AccessPolicy::from_spec(AccessSpec {
            base_dir: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root],
            deny_read_roots: vec![read_denied.clone()],
            deny_write_roots: vec![write_denied.clone()],
            unrestricted_fs: false,
        })
        .unwrap();

        assert!(!policy.can_read(&read_denied));
        assert!(policy.can_write(&read_denied));
        assert!(policy.can_read(&write_denied));
        assert!(!policy.can_write(&write_denied));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_executable_canonicalizes_symlink_paths() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real-bash");
        let link = temp.path().join("bash");
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();
        symlink(&real, &link).unwrap();

        assert_eq!(
            resolve_executable(link.to_str().unwrap()).unwrap(),
            real.canonicalize().unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"secret").unwrap();
        symlink(outside.path(), allowed.path().join("escape")).unwrap();

        let root = allowed.path().canonicalize().unwrap();
        let policy = AccessPolicy::from_spec(AccessSpec {
            base_dir: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root],
            deny_read_roots: Vec::new(),
            deny_write_roots: Vec::new(),
            unrestricted_fs: false,
        })
        .unwrap();

        assert!(
            policy
                .resolve_existing("escape/secret", AccessNeed::Read)
                .await
                .is_err()
        );
        assert!(
            policy
                .resolve_for_create("escape/new-file", AccessNeed::Write)
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lexical_deny_cannot_be_bypassed_by_later_symlink_alias() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let other = root.join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("file"), b"visible").unwrap();

        let denied = root.join("secret");
        let policy = AccessPolicy::from_spec(AccessSpec {
            base_dir: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root],
            deny_read_roots: vec![denied.clone()],
            deny_write_roots: vec![denied.clone()],
            unrestricted_fs: false,
        })
        .unwrap();

        symlink(&other, &denied).unwrap();

        assert!(
            policy
                .resolve_existing("secret/file", AccessNeed::Read)
                .await
                .is_err()
        );
        assert!(
            policy
                .resolve_for_create("secret/new-file", AccessNeed::Write)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn write_targets_accept_regular_files_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let regular = root.join("regular");
        let directory = root.join("directory");
        let missing = root.join("missing");
        std::fs::write(&regular, b"x").unwrap();
        std::fs::create_dir(&directory).unwrap();

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

        assert!(machine.require_regular_file(&regular).await.is_ok());
        assert!(machine.require_regular_file(&missing).await.is_err());
        assert!(machine.require_regular_file(&directory).await.is_err());
    }

    #[tokio::test]
    async fn job_spool_root_is_implicitly_protected_from_filesystem_tools() {
        let temp_root = std::env::temp_dir().canonicalize().unwrap();
        let base = tempfile::tempdir_in(&temp_root).unwrap();
        let base = base.path().canonicalize().unwrap();
        let jobs = crate::jobs::JobManager::new(std::time::Duration::from_secs(60)).unwrap();
        let spool = jobs.protected_path().to_path_buf();
        let machine = LocalMachine::new_with_jobs(
            MachineConfig {
                access: AccessSpec {
                    base_dir: base,
                    read_roots: vec![temp_root.clone()],
                    write_roots: vec![temp_root],
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
                allow_shell: false,
                sandbox_shell: false,
                allow_network: false,
                max_shell_timeout_secs: 60,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,

                task_result_budget: None,
                job_retention_secs: 60,

                job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
                protected_paths: Vec::new(),
            },
            jobs,
        )
        .await
        .unwrap();

        assert!(machine.ensure_not_protected(&spool).is_err());
        let error = machine
            .resolve_existing(spool.to_str().unwrap(), AccessNeed::Read)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("protected by dotlink"), "{error}");
    }

    #[tokio::test]
    async fn protected_credentials_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let secret = root.join("runtime.key");
        std::fs::write(&secret, b"secret").unwrap();

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
            protected_paths: vec![secret],
        })
        .await
        .unwrap();

        assert!(
            machine
                .resolve_existing("runtime.key", AccessNeed::Read)
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn protected_symlink_keeps_both_names_and_cannot_be_replaced_after_unlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let target = root.join("runtime.key");
        let alias = root.join("credential-link");
        std::fs::write(&target, b"secret").unwrap();
        symlink(&target, &alias).unwrap();

        let machine = LocalMachine::new(MachineConfig {
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
            // Exercise normalization as well as the lexical/resolved pair.
            protected_paths: vec![root.join("unused/../credential-link")],
        })
        .await
        .unwrap();

        assert!(machine.config.protected_paths.contains(&alias));
        assert!(machine.config.protected_paths.contains(&target));
        assert!(
            machine
                .resolve_for_create("credential-link", AccessNeed::Write)
                .await
                .is_err()
        );

        // If the alias disappears after startup, an authorized native write
        // still cannot install a replacement at its lexical name.
        std::fs::remove_file(&alias).unwrap();
        let result: Result<()> = async {
            let path = machine
                .resolve_for_create("credential-link", AccessNeed::Write)
                .await?;
            machine.write_file_atomic(&path, b"replacement").await
        }
        .await;
        assert!(result.is_err());
        assert!(!alias.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"secret");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dangling_protected_symlink_retains_its_missing_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let alias = root.join("credential-link");
        let target = root.join("future").join("runtime.key");
        symlink("future/runtime.key", &alias).unwrap();

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
            protected_paths: vec![alias.clone()],
        })
        .await
        .unwrap();

        assert!(machine.config.protected_paths.contains(&alias));
        assert!(
            machine.config.protected_paths.contains(&target),
            "a dangling final symlink must not fall back to protecting only its lexical name"
        );

        std::fs::create_dir(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"secret").unwrap();
        assert!(
            machine
                .resolve_existing("future/runtime.key", AccessNeed::Read)
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn protected_write_target_cannot_be_reached_through_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let secret = root.join("config.jsonc");
        let link = root.join("config-link");
        std::fs::write(&secret, b"secret").unwrap();
        symlink(&secret, &link).unwrap();

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
            protected_paths: vec![secret],
        })
        .await
        .unwrap();

        assert!(
            machine
                .resolve_for_create("config-link", AccessNeed::Write)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn bounded_read_enforces_real_io_limit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("growing.txt");
        std::fs::write(&path, b"abcdefghij").unwrap();
        let mut file = fs::File::open(&path).await.unwrap();
        assert!(LocalMachine::read_bounded(&mut file, 4).await.is_err());
        file.rewind().await.unwrap();
        assert_eq!(
            LocalMachine::read_bounded(&mut file, 10).await.unwrap(),
            b"abcdefghij"
        );
        file.rewind().await.unwrap();
        assert_eq!(
            LocalMachine::read_bounded(&mut file, 11).await.unwrap(),
            b"abcdefghij"
        );
    }

    #[test]
    fn only_explicit_pure_reads_share_the_operation_gate() {
        for name in ["ls", "read", "bread"] {
            assert!(LocalMachine::is_concurrent_read_tool(name), "{name}");
        }
        for name in [
            "write",
            "edit",
            "bwrite",
            "bedit",
            "bash",
            "powershell",
            "future_tool",
        ] {
            assert!(!LocalMachine::is_concurrent_read_tool(name), "{name}");
        }
    }

    #[test]
    fn default_tool_policy_is_read_only() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(Capabilities {
            read: true,
            ..Default::default()
        })
        .list_all()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
        assert_eq!(names, ["bread", "grep", "ls", "read"]);
    }

    #[test]
    fn write_policy_adds_mutators() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(Capabilities {
            read: true,
            write: true,
            read_write: true,
            ..Default::default()
        })
        .list_all()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
        assert_eq!(
            names,
            [
                "bedit", "bread", "bwrite", "edit", "grep", "ls", "patch", "read", "write"
            ]
        );
    }

    #[test]
    fn write_only_policy_advertises_only_write_tools() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(Capabilities {
            write: true,
            ..Default::default()
        })
        .list_all()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
        assert_eq!(names, ["bwrite", "write"]);
    }

    #[test]
    fn split_read_write_without_overlap_does_not_advertise_edit_tools() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(Capabilities {
            read: true,
            write: true,
            ..Default::default()
        })
        .list_all()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
        assert_eq!(names, ["bread", "bwrite", "grep", "ls", "read", "write"]);
    }

    #[test]
    fn shell_policy_is_platform_specific() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(Capabilities {
            read: true,
            shell: true,
            ..Default::default()
        })
        .list_all()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
        if cfg!(windows) {
            assert_eq!(
                names,
                [
                    "bread",
                    "grep",
                    "job_ack",
                    "job_cancel",
                    "job_get",
                    "job_list",
                    "job_output",
                    "job_start",
                    "ls",
                    "powershell",
                    "read"
                ]
            );
        } else {
            assert_eq!(
                names,
                [
                    "bash",
                    "bread",
                    "grep",
                    "job_ack",
                    "job_cancel",
                    "job_get",
                    "job_list",
                    "job_output",
                    "job_start",
                    "ls",
                    "read"
                ]
            );
        }
    }

    #[test]
    fn tool_table_matches_the_advertised_router() {
        // One table drives visibility; prove it covers every route exactly once.
        let all: Vec<String> = LocalMachine::all_tools()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        let mut from_table: Vec<String> =
            TOOL_SPECS.iter().map(|spec| spec.name.to_owned()).collect();
        from_table.sort();
        assert_eq!(all, from_table, "table and routers disagree");

        // Every capability the table names is grantable, and the concurrent-read
        // class is what the operation gate expects.
        assert_eq!(
            TOOL_SPECS
                .iter()
                .filter(|spec| spec.concurrent_read)
                .map(|spec| spec.name)
                .collect::<Vec<_>>(),
            [
                "ls",
                "grep",
                "read",
                "bread",
                "job_list",
                "job_get",
                "job_output"
            ]
        );
    }

    /// Both HTTP entry points must use the ceiling derived from the profile, not
    /// rmcp's fixed 4 MiB default: the tools accept a write up to
    /// `max_write_bytes`, its JSON encoding can be six times that, and the
    /// transport must not refuse what the tool would have accepted.
    #[tokio::test]
    async fn streamable_transports_follow_the_configured_write_ceiling() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let write_ceiling = 8 * 1024 * 1024;
        let machine = crate::mcp::tools::test_support::test_machine_with(root, |config| {
            config.max_write_bytes = write_ceiling;
        });

        let config = crate::transports::streamable_config(
            &machine,
            tokio_util::sync::CancellationToken::new(),
        );
        assert_eq!(
            config.max_request_body_bytes,
            machine.max_request_body_bytes()
        );
        assert_eq!(
            config.max_request_body_bytes,
            write_ceiling * 6 + MAX_EDIT_OPS * EDIT_PATH_ENVELOPE_BYTES + 64 * 1024,
            "write payload and all edit path strings have independent JSON headroom"
        );
        assert!(
            config.max_request_body_bytes > 4 * 1024 * 1024,
            "the transport must not keep rmcp's smaller default"
        );
        assert!(
            config.sse_keep_alive.is_some(),
            "network HTTP needs keepalives"
        );
        let embedded = crate::transports::embedded_streamable_config(
            &machine,
            tokio_util::sync::CancellationToken::new(),
        );
        assert!(embedded.sse_keep_alive.is_none());
        assert!(embedded.sse_retry.is_none());
    }

    #[test]
    fn embedded_response_ceiling_covers_both_nested_shell_streams() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let output_ceiling = 2 * 1024 * 1024;
        let machine = crate::mcp::tools::test_support::test_machine_with(root, |config| {
            config.max_read_bytes = 1024;
            config.max_output_bytes = output_ceiling;
        });

        assert_eq!(
            machine.max_response_body_bytes(),
            output_ceiling * 2 * 7 + 64 * 1024,
            "stdout and stderr are independently capped and nested in JSON"
        );
    }

    #[test]
    fn transport_postcondition_bounds_error_and_unexpected_success_results() {
        let budget = 512;
        let bulk = format!("{}{}", "\"\\\n".repeat(512), "tail");
        for (is_error, response) in [
            (
                true,
                CallToolResponse::Complete(CallToolResult::error(vec![ContentBlock::text(
                    bulk.clone(),
                )])),
            ),
            (
                false,
                CallToolResponse::Complete(CallToolResult::success(vec![ContentBlock::text(
                    bulk.clone(),
                )])),
            ),
        ] {
            let fitted = fit_call_tool_response_to_budget("test", response, Some(budget));
            let CallToolResponse::Complete(result) = fitted else {
                panic!("complete result changed response kind")
            };
            assert_eq!(result.is_error.unwrap_or(false), is_error);
            assert!(serialized_json_len(&result).unwrap() <= budget);
            let text = result.content[0].as_text().unwrap().text.as_str();
            assert!(
                text.contains("transport response budget") || text.is_empty(),
                "{text:?}"
            );
        }
    }

    #[test]
    fn transport_postcondition_bounds_protocol_errors_and_preserves_code() {
        let budget = 512;
        let error = McpError::new(
            rmcp::model::ErrorCode::INVALID_PARAMS,
            format!("bad {}", "\"\\\n".repeat(512)),
            Some(serde_json::json!({"bulk": "x".repeat(4096)})),
        );
        let fitted = fit_mcp_error_to_budget(error, Some(budget));
        assert_eq!(fitted.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(fitted.data.is_none());
        assert!(serialized_json_len(&fitted).unwrap() <= budget);
        assert!(fitted.message.contains("transport response budget"));
    }

    #[test]
    fn transport_context_capability_debug_is_redacted() {
        let token = TransportContextToken::from("secret-test-capability");
        let rendered = format!("{token:?}");
        assert_eq!(rendered, "TransportContextToken([redacted])");
        assert!(!rendered.contains("secret-test-capability"));
    }

    #[tokio::test]
    async fn configured_resource_ceilings_are_rejected_not_widened() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine_config = |max_write_bytes: usize| MachineConfig {
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
            serialize: false,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 60,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes,
            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        };

        // A ceiling below the supported minimum is an error naming the value,
        // not a silent promotion to the minimum.
        let error = LocalMachine::new(machine_config(32)).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("max_write_bytes must be at least 1024")
                && error.to_string().contains("got 32"),
            "{error}"
        );
        // Restarting with the same profile cannot help.
        assert_eq!(
            crate::recovery::Recovery::of(&error),
            crate::recovery::Recovery::Operator
        );

        // An accepted value is used exactly as configured.
        let machine = LocalMachine::new(machine_config(2048)).await.unwrap();
        assert_eq!(machine.config.max_write_bytes, 2048);
    }

    #[test]
    fn the_shell_predicate_is_derived_from_the_tool_table() {
        // Exactly one advertised tool is the platform's shell: every other
        // consumer (schema limits, delivery reserve, logging) asks this.
        let platform_shell = match cfg!(windows) {
            true => "powershell",
            false => "bash",
        };
        let accepted = TOOL_SPECS
            .iter()
            .filter(|spec| is_shell_tool(spec.name))
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(accepted, [platform_shell]);
        assert!(!is_shell_tool("read"));
        assert!(!is_shell_tool("future_tool"));
    }

    #[tokio::test]
    async fn deny_paths_are_normalized_before_they_are_matched() {
        // A deny that names a not-yet-existing component and then walks back out
        // of it must still deny the real path. Without lexical folding the
        // stored root keeps `..` and silently matches nothing.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("secret")).unwrap();
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root.clone()],
                write_roots: vec![root.clone()],
                deny_read_roots: vec![PathBuf::from("missing/../secret")],
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: true,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 60,
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

        let error = machine
            .resolve_existing("secret", AccessNeed::Read)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("denied by policy"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn policy_paths_resolve_each_symlink_before_its_parent_component() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        symlink("a/b", root.join("pivot")).unwrap();
        // Dangling: the kernel follows `pivot` first, so `..` leaves `a/b`
        // for `a`. Folding `pivot/..` lexically would guard `root/x` instead.
        symlink("pivot/../x", root.join("link")).unwrap();
        assert_eq!(
            resolve_policy_path(&root, Path::new("link")).unwrap(),
            root.join("a/x")
        );
        // Below a missing component `..` folds as it will once created, and
        // a symlink reached again after it is still followed.
        assert_eq!(
            resolve_policy_path(&root, Path::new("missing/../pivot/new")).unwrap(),
            root.join("a/b/new")
        );

        symlink("loop", root.join("loop")).unwrap();
        assert!(resolve_policy_path(&root, Path::new("loop/x")).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deny_alias_keeps_its_name_and_startup_target_after_a_retarget() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let secret = root.join("secret");
        let other = root.join("other");
        let alias = root.join("alias");
        for directory in [&secret, &other] {
            std::fs::create_dir(directory).unwrap();
            std::fs::write(directory.join("file"), b"data").unwrap();
        }
        symlink(&secret, &alias).unwrap();

        let policy = AccessPolicy::resolve(AccessSpec {
            base_dir: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root.clone()],
            deny_read_roots: vec![alias.clone()],
            deny_write_roots: vec![alias.clone()],
            unrestricted_fs: false,
        })
        .await
        .unwrap();
        assert_eq!(policy.deny_read_roots, [alias.clone(), secret.clone()]);

        std::fs::remove_file(&alias).unwrap();
        symlink(&other, &alias).unwrap();
        for input in ["alias/file", "secret/file"] {
            assert!(
                policy
                    .resolve_existing(input, AccessNeed::Read)
                    .await
                    .is_err(),
                "{input} escaped the deny"
            );
            assert!(
                policy
                    .resolve_for_create(input, AccessNeed::Write)
                    .await
                    .is_err(),
                "{input} escaped the deny"
            );
        }
        std::fs::remove_file(&alias).unwrap();
        assert!(
            policy
                .resolve_for_create("alias", AccessNeed::Write)
                .await
                .is_err(),
            "an unlinked alias freed its name"
        );

        // The sandbox also masks what the alias names when a command starts.
        #[cfg(target_os = "linux")]
        {
            symlink(&other, &alias).unwrap();
            let current = policy.refreshed().unwrap();
            assert!(current.denied_read(&other.join("file")));
            assert!(current.denied_read(&secret.join("file")));
        }
    }

    #[cfg(target_os = "linux")]
    fn sandboxed_config(root: &Path, deny_read: Vec<PathBuf>) -> MachineConfig {
        MachineConfig {
            access: AccessSpec {
                base_dir: root.to_path_buf(),
                read_roots: vec![root.to_path_buf()],
                write_roots: vec![root.to_path_buf()],
                deny_read_roots: deny_read,
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
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
        }
    }

    /// Existing canonical file/directory guards can be made lifetime-stable
    /// beneath a broad live bind: the leaf mask/read-only rebind is its own
    /// mountpoint and mutable ancestors become namespace anchors. Missing names
    /// and symlink spellings remain fail-closed.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn broad_writable_home_accepts_standard_dotlink_private_directory() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let dotlink = home.join(".config/abird/dotlink");
        std::fs::create_dir_all(&dotlink).unwrap();
        std::fs::write(dotlink.join("runtime.key"), b"secret").unwrap();

        let mut config = sandboxed_config(&home, Vec::new());
        config.protected_paths = vec![dotlink.clone(), dotlink.join("runtime.key")];
        let machine = LocalMachine::new(config)
            .await
            .expect("a broad writable home must coexist with dotlink's private directory");
        let guards = machine.shell_guards().unwrap();
        assert_eq!(
            guards
                .anchors
                .iter()
                .map(|anchor| anchor.path.clone())
                .collect::<Vec<_>>(),
            [home.join(".config"), home.join(".config/abird")]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn broad_writable_home_accepts_control_plane_and_job_spool_private_trees() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let config_dir = home.join(".config/abird/dotlink");
        let job_root = home.join(".local/state/abird/dotlink/jobs/session-test");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&job_root).unwrap();
        std::fs::write(config_dir.join("runtime.key"), b"secret").unwrap();

        let mut config = sandboxed_config(&home, Vec::new());
        config.protected_paths = vec![config_dir.clone()];
        let prepared = LocalMachine::prepare_with_job_root(config, &job_root)
            .await
            .expect("broad writable home must coexist with both private trees");
        let runtime = tools::shell::RuntimeLayout::for_shell(false, false).unwrap();
        let anchors = validate_shell_guards(
            &prepared.config.access,
            &prepared.config.cache_mounts,
            &prepared.config.protected_paths,
            &runtime,
        )
        .unwrap();
        let anchors = anchors
            .into_iter()
            .map(|anchor| anchor.path)
            .collect::<BTreeSet<_>>();

        for expected in [
            home.join(".config"),
            home.join(".config/abird"),
            home.join(".local"),
            home.join(".local/state"),
            home.join(".local/state/abird"),
            home.join(".local/state/abird/dotlink"),
            home.join(".local/state/abird/dotlink/jobs"),
        ] {
            assert!(
                anchors.contains(&expected),
                "missing anchor {}",
                expected.display()
            );
        }
        assert!(prepared.config.protected_paths.contains(&config_dir));
        assert!(prepared.config.protected_paths.contains(&job_root));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn canonical_file_and_directory_guards_are_anchorable_but_symlinks_fail_closed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let project = root.join("project");
        let nested = project.join(".env");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(&nested, b"secret").unwrap();

        let file_machine = LocalMachine::new(sandboxed_config(&root, vec![nested.clone()]))
            .await
            .expect("an existing canonical file guard is lifetime-anchorable");
        let file_guards = file_machine.shell_guards().unwrap();
        assert_eq!(
            file_guards
                .anchors
                .iter()
                .map(|anchor| anchor.path.as_path())
                .collect::<Vec<_>>(),
            std::slice::from_ref(&project.as_path())
        );

        let alias = root.join("alias");
        symlink(&project, &alias).unwrap();
        let error = LocalMachine::new(sandboxed_config(&root, vec![alias]))
            .await
            .expect_err("a symlink spelling cannot become a stable namespace anchor");
        assert_eq!(
            crate::recovery::Recovery::of(&error),
            crate::recovery::Recovery::Operator
        );
        assert!(
            error
                .to_string()
                .contains("not an existing canonical object"),
            "{error}"
        );

        let access = AccessPolicy::resolve(sandboxed_config(&root, vec![project, nested]).access)
            .await
            .unwrap();
        let anchors =
            validate_shell_guards(&access, &[], &[], &tools::shell::RuntimeLayout::default())
                .unwrap();
        assert!(
            anchors.is_empty(),
            "a directly-mounted guarded directory is itself the stable mask mountpoint"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn explicit_root_rw_is_unrestricted_for_shell_but_explicit_denies_still_apply() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let protected = base.join("config.jsonc");
        std::fs::write(&protected, b"private").unwrap();
        let mut spec = AccessSpec {
            base_dir: base.clone(),
            read_roots: vec![PathBuf::from("/")],
            write_roots: vec![PathBuf::from("/")],
            deny_read_roots: Vec::new(),
            deny_write_roots: Vec::new(),
            unrestricted_fs: false,
        };
        let runtime = tools::shell::RuntimeLayout::default();
        let policy = AccessPolicy::resolve(spec.clone()).await.unwrap();
        assert!(policy.explicit_root_rw_grant());
        validate_shell_guards(&policy, &[], std::slice::from_ref(&protected), &runtime).unwrap();
        // Explicit denies are not opted out. Existing canonical objects can
        // still be enforced under the root bind by anchoring their ancestor
        // namespace and masking the leaf.
        spec.deny_read_roots.push(protected);
        let denied = AccessPolicy::resolve(spec).await.unwrap();
        let anchors = validate_shell_guards(&denied, &[], &[], &runtime).unwrap();
        assert!(anchors.iter().any(|anchor| anchor.path == base));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn regular_file_guard_is_anchorable_but_missing_name_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let guard = root.join(".env");
        std::fs::write(&guard, b"protected").unwrap();
        let access = AccessPolicy::resolve(sandboxed_config(&root, vec![guard.clone()]).access)
            .await
            .unwrap();

        assert!(
            validate_shell_guards(&access, &[], &[], &tools::shell::RuntimeLayout::default())
                .unwrap()
                .is_empty()
        );

        std::fs::remove_file(&guard).unwrap();
        let error =
            validate_shell_guards(&access, &[], &[], &tools::shell::RuntimeLayout::default())
                .unwrap_err();
        assert_eq!(
            crate::recovery::Recovery::of(&error),
            crate::recovery::Recovery::Operator
        );
        assert!(
            error.to_string().contains("cannot be lifetime-anchored"),
            "{error}"
        );

        std::fs::write(&guard, b"restored").unwrap();
        assert!(
            validate_shell_guards(&access, &[], &[], &tools::shell::RuntimeLayout::default())
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn readable_mount_masks_existing_guard_but_missing_name_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let secret = root.join(".secret");
        let mut config = sandboxed_config(&root, vec![secret.clone()]);
        config.access.write_roots.clear();
        let access = AccessPolicy::resolve(config.access).await.unwrap();

        let error =
            validate_shell_guards(&access, &[], &[], &tools::shell::RuntimeLayout::default())
                .unwrap_err();
        assert_eq!(
            crate::recovery::Recovery::of(&error),
            crate::recovery::Recovery::Operator
        );
        assert!(
            error.to_string().contains("cannot be lifetime-anchored"),
            "{error}"
        );

        std::fs::write(&secret, b"secret").unwrap();
        assert!(
            validate_shell_guards(&access, &[], &[], &tools::shell::RuntimeLayout::default())
                .unwrap()
                .is_empty(),
            "a direct-child existing file is stabilized by its own mask mountpoint"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn writable_mount_only_conflicts_with_write_denies_when_exposed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let denied = root.join("no-write");
        let mut config = sandboxed_config(&root, vec![]);
        config.access.write_roots.clear();
        config.access.deny_write_roots.push(denied);
        let access = AccessPolicy::resolve(config.access).await.unwrap();
        validate_shell_guards(&access, &[], &[], &tools::shell::RuntimeLayout::default()).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn deny_write_blocks_operational_host_mounts() {
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let agent = root.join("agent.sock");
        let _listener = UnixListener::bind(&agent).unwrap();
        let host_shell = crate::host_services::HostShell {
            mounts: vec![
                crate::host_services::HostMount::new(
                    agent.clone(),
                    PathBuf::from("/tmp/ssh-agent.sock"),
                    crate::host_services::HostMountKind::Socket,
                )
                .unwrap(),
            ],
            ..Default::default()
        };
        let access = AccessPolicy::resolve(AccessSpec {
            base_dir: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: Vec::new(),
            deny_read_roots: Vec::new(),
            deny_write_roots: vec![agent],
            unrestricted_fs: false,
        })
        .await
        .unwrap();

        // A read-only bind of a socket still connects; deny-write must hold.
        let error = validate_host_mount_policy(&access, &host_shell, &[]).unwrap_err();
        assert!(error.to_string().contains("denied or protected"), "{error}");
        assert_eq!(
            crate::recovery::Recovery::of(&error),
            crate::recovery::Recovery::Operator
        );
    }
}
