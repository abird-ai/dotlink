use std::{
    collections::BTreeSet,
    env,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::logging::{LogConfig, truncate};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rmcp::{
    ErrorData as McpError,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, ProgressToken,
        ProtocolVersion, ResourceContents, ResultType, Tool,
    },
    schemars,
    service::{Peer, RequestContext, RoleServer},
    tool, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    process::Command,
    sync::RwLock,
    time::{Instant, timeout_at},
};

const MAX_PATCH_FILE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_LIST_ENTRIES: usize = 4 * 1024;
const MAX_LIST_ENTRIES: usize = 16 * 1024;

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
    pub protected_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct SandboxCacheMount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub writable: bool,
}

#[derive(Clone, Debug)]
struct AccessPolicy {
    base_dir: PathBuf,
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
    deny_read_roots: Vec<PathBuf>,
    deny_write_roots: Vec<PathBuf>,
    unrestricted_fs: bool,
}

#[derive(Clone, Debug)]
struct RuntimeConfig {
    access: AccessPolicy,
    cache_mounts: Vec<SandboxCacheMount>,
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
    protected_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct LocalMachine {
    config: RuntimeConfig,
    operation_gate: Arc<RwLock<()>>,
    serialize: bool,
    #[cfg(target_os = "linux")]
    secure_fs: Arc<crate::secure_fs::SecureFs>,
}

#[derive(Debug, Clone, Copy)]
enum AccessNeed {
    Read,
    Write,
    ReadWrite,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListArgs {
    /// Directory path. Relative paths resolve from the directory where dotlink was launched.
    #[serde(default = "dot")]
    path: String,

    /// Glob over entry paths relative to `path`, and over the bare entry name.
    /// `*`, `?`, `[...]`, `{a,b}` stay within one path segment; `**` recurses.
    #[serde(default)]
    pattern: Option<String>,

    /// Directory levels to descend. 1 (default) lists this directory only. Symlinked directories are never traversed.
    #[serde(default)]
    depth: Option<usize>,

    /// Maximum number of entries to return. Defaults to 4096 and is capped at 16384.
    #[serde(default)]
    max_entries: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadArgs {
    /// UTF-8 text file path. Relative paths resolve from the directory where dotlink was launched.
    path: String,

    /// First 1-based line to return. Defaults to 1.
    #[serde(default)]
    offset: Option<usize>,

    /// Maximum number of lines to return. Omit to return all remaining lines.
    #[serde(default)]
    limit: Option<usize>,

    /// Prefix each line with its 1-based line number and a tab. Defaults to true.
    #[serde(default)]
    numbered: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WriteArgs {
    /// File path. Relative paths resolve from the directory where dotlink was launched.
    path: String,

    /// UTF-8 content. Missing parent directories are created automatically.
    content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EditArgs {
    /// Existing UTF-8 text file path.
    path: String,

    /// Exact text to find.
    old_text: String,

    /// Replacement text. May be empty.
    new_text: String,

    /// Replace every exact occurrence. Without this flag, the match must be unique.
    #[serde(default)]
    replace_all: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BreadFormat {
    #[default]
    Mcp,
    Base64,
    Hex,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BinaryEncoding {
    #[default]
    Base64,
    Hex,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BreadArgs {
    /// Binary file path.
    path: String,

    /// "mcp" returns typed MCP image/audio/blob content; base64/hex return encoded text.
    #[serde(default)]
    format: BreadFormat,

    /// Byte offset for base64/hex reads. MCP format requires offset 0.
    #[serde(default)]
    offset: Option<u64>,

    /// Maximum bytes to read. MCP format requires the complete file.
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BwriteArgs {
    /// File path.
    path: String,

    /// Encoded binary payload.
    data: String,

    /// Encoding of data.
    #[serde(default)]
    encoding: BinaryEncoding,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BeditArgs {
    /// Existing binary file path.
    path: String,

    /// Zero-based byte offset where replacement begins.
    offset: usize,

    /// Number of existing bytes to replace. Defaults to the decoded payload length.
    /// Use 0 to insert. Use empty data with a positive length to delete.
    #[serde(default)]
    length: Option<usize>,

    /// Replacement bytes encoded as base64 or hex. May be empty.
    data: String,

    /// Encoding of data.
    #[serde(default)]
    encoding: BinaryEncoding,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ShellArgs {
    /// Command string to execute.
    command: String,

    /// Working directory. Relative paths resolve from the directory where dotlink was launched.
    #[serde(default = "dot")]
    dir: String,

    /// Optional stdin text passed to the process.
    #[serde(default)]
    stdin: Option<String>,

    /// Timeout in seconds. Defaults to the client's response window when the transport provides one, else the profile ceiling (max_shell_timeout_secs); clamped to that ceiling.
    #[serde(default)]
    timeout_secs: Option<u64>,

    /// Output byte limit per stream. Defaults to the profile ceiling (max_output_bytes); clamped to it.
    #[serde(default)]
    max_output_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
struct FsEntry {
    path: String,
    kind: &'static str,
    size: u64,
}

fn dot() -> String {
    ".".to_owned()
}

fn ok_text(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.into())])
}

fn tool_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

fn contains_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| component == Component::ParentDir)
}

fn path_depth(path: &Path) -> usize {
    path.components().count()
}

/// Client-controlled shell timeout. The caller picks it; the profile ceiling
/// (`max_shell_timeout_secs`) only bounds it, and is the fallback when omitted.
fn resolve_shell_timeout(requested: Option<u64>, ceiling: u64) -> u64 {
    resolve_capped(requested, ceiling)
}

/// Client-controlled limit under a policy ceiling. Both the shell timeout and
/// the per-stream output limit follow this shape: the caller picks any value up
/// to the configured ceiling, which is also the fallback when omitted.
fn resolve_capped<T>(requested: Option<T>, ceiling: T) -> T
where
    T: Ord + From<u8> + Copy,
{
    let ceiling = ceiling.max(T::from(1));
    requested.unwrap_or(ceiling).clamp(T::from(1), ceiling)
}

/// Shell progress is a bare heartbeat: the caller cannot know how much of an
/// arbitrary command is done, so no percentages or messages are reported.
#[cfg(not(test))]
const SHELL_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
const SHELL_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);

/// Salvage window for partial output after a killed child.
const TIMEOUT_OUTPUT_GRACE: Duration = Duration::from_millis(250);

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
    async fn report(&self) {
        let progress = self.ticks.fetch_add(1, Ordering::Relaxed) + 1;
        let param = ProgressNotificationParam::new(self.token.clone(), progress as f64);
        let _ = self.peer.notify_progress(param).await;
    }
}

async fn heartbeat_tick(heartbeat: Option<&ProgressHeartbeat>) {
    match heartbeat {
        Some(_) => tokio::time::sleep(SHELL_HEARTBEAT_INTERVAL).await,
        None => std::future::pending().await,
    }
}

/// One glob grammar for every tool: `globset` with `literal_separator`, so
/// `*`/`?`/`[...]` never cross `/` and `**` is the only recursive form.
/// `{a,b}` alternation comes from the same builder. Candidates are always a
/// path already held in memory; nothing here touches the filesystem.
struct GlobFilter {
    matcher: globset::GlobMatcher,
}

impl GlobFilter {
    fn new(pattern: &str) -> Result<Self> {
        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid glob pattern: {pattern}"))?
            .compile_matcher();
        Ok(Self { matcher })
    }

    fn matches(&self, candidate: &Path) -> bool {
        self.matcher.is_match(candidate)
    }
}

fn list_entry_limit(requested: Option<usize>) -> usize {
    requested
        .unwrap_or(DEFAULT_MAX_LIST_ENTRIES)
        .clamp(1, MAX_LIST_ENTRIES)
}

fn shell_timeout_description(max_secs: u64) -> String {
    format!(
        "Timeout in seconds. Defaults to the client's response window when the transport provides one, else {max_secs}; capped at {max_secs}."
    )
}

fn shell_output_description(max_bytes: usize) -> String {
    format!("Output byte limit per stream. Defaults to {max_bytes} and is capped there.")
}

/// Tool schemas are static, the policy ceilings are not. Replace the limit
/// arguments' descriptions with the numbers this profile actually enforces.
fn with_profile_limits(mut tool: Tool, max_secs: u64, max_bytes: usize) -> Tool {
    if !matches!(tool.name.as_ref(), "bash" | "powershell") {
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
        }
    }
    tool.input_schema = Arc::new(schema);
    tool
}

async fn drain_capped<R>(mut reader: R, limit: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut kept = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        let remaining = limit.saturating_sub(kept.len());
        if remaining > 0 {
            let copy_len = remaining.min(read);
            kept.extend_from_slice(&buffer[..copy_len]);
        }
        if read > remaining {
            truncated = true;
        }
    }

    Ok((kept, truncated))
}

pub(crate) fn resolve_executable(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    let found = if candidate.components().count() > 1 {
        candidate.exists().then(|| candidate.to_path_buf())
    } else {
        env::var_os("PATH").and_then(|path| {
            env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|path| path.is_file())
        })
    }?;

    Some(std::fs::canonicalize(&found).unwrap_or(found))
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

#[cfg(target_os = "linux")]
fn sandbox_runtime_mounts() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("/nix/store"),
        PathBuf::from("/run/current-system"),
        PathBuf::from("/etc/profiles"),
        PathBuf::from("/nix/var/nix/profiles"),
        PathBuf::from("/usr"),
        PathBuf::from("/bin"),
        PathBuf::from("/sbin"),
        PathBuf::from("/lib"),
        PathBuf::from("/lib64"),
    ];

    if let Some(home) = env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".nix-profile"));
    }

    paths.retain(|path| path.exists());
    paths
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

async fn canonicalize_deny(base_dir: &Path, path: &Path) -> Result<PathBuf> {
    let target = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };

    if let Ok(canonical) = fs::canonicalize(&target).await {
        return Ok(canonical);
    }

    let mut ancestor = target.clone();
    loop {
        match fs::canonicalize(&ancestor).await {
            Ok(canonical_ancestor) => {
                let remainder = target
                    .strip_prefix(&ancestor)
                    .map_err(|_| anyhow!("failed to resolve deny path {}", target.display()))?;
                return Ok(canonical_ancestor.join(remainder));
            }
            Err(_) => {
                if !ancestor.pop() {
                    bail!("could not resolve deny path {}", target.display());
                }
            }
        }
    }
}

fn decode_binary(data: &str, encoding: BinaryEncoding, max_bytes: usize) -> Result<Vec<u8>> {
    if data.len() > max_bytes.saturating_mul(2).saturating_add(16) {
        bail!("encoded payload is too large");
    }

    let bytes = match encoding {
        BinaryEncoding::Base64 => BASE64.decode(data).context("invalid base64 payload")?,
        BinaryEncoding::Hex => hex::decode(data).context("invalid hex payload")?,
    };
    if bytes.len() > max_bytes {
        bail!(
            "decoded payload is {} bytes, above the {} byte limit",
            bytes.len(),
            max_bytes
        );
    }
    Ok(bytes)
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

    fn relative_display(&self, path: &Path) -> String {
        path.strip_prefix(&self.base_dir)
            .map(|relative| {
                let value = relative.to_string_lossy().to_string();
                if value.is_empty() {
                    ".".to_owned()
                } else {
                    value
                }
            })
            .unwrap_or_else(|_| path.to_string_lossy().to_string())
    }

    fn sandbox_mounts(&self) -> Vec<(PathBuf, bool)> {
        if self.unrestricted_fs {
            return vec![(PathBuf::from("/"), true)];
        }

        let mut candidates = BTreeSet::new();
        candidates.extend(self.read_roots.iter().cloned());
        candidates.extend(self.write_roots.iter().cloned());
        candidates.extend(self.deny_write_roots.iter().cloned());

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

    fn deny_read_roots(&self) -> &[PathBuf] {
        &self.deny_read_roots
    }

    fn deny_write_roots(&self) -> &[PathBuf] {
        &self.deny_write_roots
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

impl LocalMachine {
    pub async fn new(args: MachineConfig) -> Result<Self> {
        let base_dir = fs::canonicalize(&args.access.base_dir)
            .await
            .with_context(|| {
                format!(
                    "launch/base directory does not exist: {}",
                    args.access.base_dir.display()
                )
            })?;
        if !fs::metadata(&base_dir).await?.is_dir() {
            bail!(
                "launch/base directory is not a directory: {}",
                base_dir.display()
            );
        }

        let mut read_roots = Vec::with_capacity(args.access.read_roots.len());
        for path in &args.access.read_roots {
            read_roots.push(canonicalize_grant(&base_dir, path).await?);
        }
        let mut write_roots = Vec::with_capacity(args.access.write_roots.len());
        for path in &args.access.write_roots {
            write_roots.push(canonicalize_grant(&base_dir, path).await?);
        }
        let mut deny_read_roots = Vec::with_capacity(args.access.deny_read_roots.len());
        for path in &args.access.deny_read_roots {
            deny_read_roots.push(canonicalize_deny(&base_dir, path).await?);
        }
        let mut deny_write_roots = Vec::with_capacity(args.access.deny_write_roots.len());
        for path in &args.access.deny_write_roots {
            deny_write_roots.push(canonicalize_deny(&base_dir, path).await?);
        }

        for roots in [
            &mut read_roots,
            &mut write_roots,
            &mut deny_read_roots,
            &mut deny_write_roots,
        ] {
            roots.sort();
            roots.dedup();
        }

        let access = AccessPolicy::from_spec(AccessSpec {
            base_dir,
            read_roots,
            write_roots,
            deny_read_roots,
            deny_write_roots,
            unrestricted_fs: args.access.unrestricted_fs,
        })?;

        let mut cache_mounts = Vec::with_capacity(args.cache_mounts.len());
        let mut cache_targets = BTreeSet::new();
        for mount in args.cache_mounts {
            if !mount.target.is_absolute()
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

            let source = match fs::canonicalize(&mount.source).await {
                Ok(source) => source,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !fs::metadata(&source).await?.is_dir() {
                continue;
            }
            if access.denied_read(&source) {
                continue;
            }
            let writable = mount.writable && !access.denied_write(&source);
            if !cache_targets.insert(mount.target.clone()) {
                bail!(
                    "internal error: duplicate cache sandbox target: {}",
                    mount.target.display()
                );
            }

            cache_mounts.push(SandboxCacheMount {
                source,
                target: mount.target,
                writable,
            });
        }

        let mut protected_paths = Vec::with_capacity(args.protected_paths.len());
        for path in args.protected_paths {
            protected_paths.push(
                canonicalize_deny(&access.base_dir, &path)
                    .await
                    .unwrap_or(path),
            );
        }

        // Deny/protected policy is validated before resolving any sandbox or
        // shell tooling so that a policy violation fails closed even when
        // bwrap/bash are absent from PATH (for example inside a Nix build).
        #[cfg(target_os = "linux")]
        for mount in &args.host_shell.mounts {
            // Denies/protected control-plane paths cover both the requested
            // lexical spelling and the resolved path captured when the file
            // descriptor was opened. Never re-resolve a mutable alias here.
            if [mount.source.as_path(), mount.resolved.as_path()]
                .iter()
                .any(|path| {
                    access.denied_read(path)
                        || (mount.kind == crate::host_services::HostMountKind::CharacterDevice
                            && access.denied_write(path))
                        || protected_paths
                            .iter()
                            .any(|protected| *path == protected || path.starts_with(protected))
                })
            {
                bail!(
                    "host integration source is denied or protected: {}",
                    mount.source.display()
                );
            }
        }

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
        if args.allow_shell && args.sandbox_shell {
            for denied in access.deny_read_roots() {
                if (access.grant_read(denied) || access.grant_write(denied)) && !denied.exists() {
                    bail!(
                        "sandboxed deny-read paths inside granted trees must already exist: {}",
                        denied.display()
                    );
                }
            }
            for denied in access.deny_write_roots() {
                if access.grant_read(denied) && access.grant_write(denied) && !denied.exists() {
                    bail!(
                        "sandboxed deny-write paths inside read+write trees must already exist: {}",
                        denied.display()
                    );
                }
            }
        }

        #[cfg(target_os = "linux")]
        let bwrap_program = if args.allow_shell && args.sandbox_shell {
            Some(resolve_executable("bwrap").ok_or_else(|| {
                anyhow!("Bubblewrap (bwrap) is required for sandboxed shell access")
            })?)
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        let bwrap_program = None;

        #[cfg(target_os = "linux")]
        let secure_fs = Arc::new(crate::secure_fs::SecureFs::new(
            &access.read_roots,
            &access.write_roots,
        )?);

        Ok(Self {
            config: RuntimeConfig {
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
                max_shell_timeout_secs: args.max_shell_timeout_secs.max(1),
                max_output_bytes: args.max_output_bytes.max(1024),
                max_read_bytes: args.max_read_bytes.max(1024),
                max_write_bytes: args.max_write_bytes.max(1024),
                protected_paths,
            },
            operation_gate: Arc::new(RwLock::new(())),
            serialize: args.serialize,
            #[cfg(target_os = "linux")]
            secure_fs,
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

    pub fn access_summary(&self) -> String {
        if self.config.access.unrestricted_fs {
            return "unrestricted read+write".to_owned();
        }
        format!(
            "read:{} write:{} deny-read:{} deny-write:{} caches:{}{}",
            self.config.access.read_roots.len(),
            self.config.access.write_roots.len(),
            self.config.access.deny_read_roots.len(),
            self.config.access.deny_write_roots.len(),
            self.config.cache_mounts.len(),
            if self.config.allow_shell {
                " + shell"
            } else {
                ""
            }
        )
    }

    pub fn tool_router_for_policy(
        any_read: bool,
        any_write: bool,
        any_read_write: bool,
        allow_shell: bool,
    ) -> ToolRouter<Self> {
        let mut router = Self::tool_router();

        if !any_read {
            for name in ["ls", "read", "bread"] {
                router.disable_route(name.to_owned());
            }
        }
        if !any_write {
            for name in ["write", "bwrite"] {
                router.disable_route(name.to_owned());
            }
        }
        if !any_read_write {
            for name in ["edit", "bedit"] {
                router.disable_route(name.to_owned());
            }
        }

        if !allow_shell {
            router.disable_route("bash".to_owned());
            router.disable_route("powershell".to_owned());
        } else if cfg!(windows) {
            router.disable_route("bash".to_owned());
        } else {
            router.disable_route("powershell".to_owned());
        }

        router
    }

    fn policy_tool_router(&self) -> ToolRouter<Self> {
        Self::tool_router_for_policy(
            self.config.access.has_read_capability(),
            self.config.access.has_write_capability(),
            self.config.access.has_read_write_capability(),
            self.config.allow_shell,
        )
    }

    pub fn visible_tool_descriptions(&self) -> Vec<(String, String)> {
        self.policy_tool_router()
            .list_all()
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

    async fn resolve_existing(&self, input: &str, need: AccessNeed) -> Result<PathBuf> {
        let path = self.config.access.resolve_existing(input, need).await?;
        self.ensure_not_protected(&path)?;
        Ok(path)
    }

    async fn resolve_for_create(&self, input: &str, need: AccessNeed) -> Result<PathBuf> {
        let path = self.config.access.resolve_for_create(input, need).await?;
        self.ensure_not_protected(&path)?;
        Ok(path)
    }

    #[cfg(any(test, not(target_os = "linux")))]
    async fn require_regular_or_missing(&self, path: &Path) -> Result<()> {
        match fs::metadata(path).await {
            Ok(metadata) if metadata.is_file() => Ok(()),
            Ok(_) => bail!("path is not a regular file: {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(any(test, not(target_os = "linux")))]
    async fn require_regular_file(&self, path: &Path) -> Result<()> {
        let metadata = fs::metadata(path).await?;
        if !metadata.is_file() {
            bail!("path is not a regular file: {}", path.display());
        }
        Ok(())
    }

    // Tokio file writes may finish on a background blocking task even after
    // write_all returns. Flush before acknowledging a mutating MCP call.
    async fn write_complete(file: &mut fs::File, bytes: &[u8]) -> Result<()> {
        file.write_all(bytes).await?;
        file.flush().await?;
        Ok(())
    }

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
        #[cfg(target_os = "linux")]
        {
            Ok(fs::File::from_std(self.secure_fs.open_read(path)?))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let file = fs::File::open(path).await?;
            if !file.metadata().await?.is_file() {
                bail!("path is not a regular file: {}", path.display());
            }
            Ok(file)
        }
    }

    async fn open_edit_file(&self, path: &Path) -> Result<fs::File> {
        #[cfg(target_os = "linux")]
        {
            Ok(fs::File::from_std(self.secure_fs.open_edit(path)?))
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.require_regular_file(path).await?;
            Ok(fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .await?)
        }
    }

    async fn open_write_file(&self, path: &Path) -> Result<fs::File> {
        #[cfg(target_os = "linux")]
        {
            Ok(fs::File::from_std(self.secure_fs.open_write(path)?))
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.require_regular_or_missing(path).await?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).await?;
            }
            Ok(fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
                .await?)
        }
    }

    async fn list_impl(&self, args: ListArgs) -> Result<(Vec<FsEntry>, bool)> {
        let root = self.resolve_existing(&args.path, AccessNeed::Read).await?;
        if !fs::metadata(&root).await?.is_dir() {
            bail!("not a directory: {}", args.path);
        }

        let depth = args.depth.unwrap_or(1).max(1);
        let pattern = args
            .pattern
            .as_deref()
            .filter(|pattern| !pattern.is_empty())
            .map(GlobFilter::new)
            .transpose()?;
        let max_entries = list_entry_limit(args.max_entries);
        let mut output = Vec::new();
        let mut truncated = false;
        // (directory, remaining levels below it)
        let mut pending = vec![(root.clone(), depth)];

        while let Some((dir, remaining)) = pending.pop() {
            #[cfg(target_os = "linux")]
            let directory_handle = self.secure_fs.open_directory(&dir)?;
            #[cfg(target_os = "linux")]
            let read_path = PathBuf::from(format!(
                "/proc/self/fd/{}",
                std::os::fd::AsRawFd::as_raw_fd(&directory_handle)
            ));
            #[cfg(not(target_os = "linux"))]
            let read_path = dir.clone();
            let mut reader = fs::read_dir(&read_path).await?;
            while let Some(entry) = reader.next_entry().await? {
                // The Linux enumerator reads via /proc/self/fd/<pinned-dir>.
                // Its DirEntry.path() is therefore an *internal* FD path,
                // not the logical authorized path. Use the original name for
                // denies, protected-path filtering, display and recursion.
                let path = dir.join(entry.file_name());

                if self.config.access.denied_read(&path)
                    || self.ensure_not_protected(&path).is_err()
                {
                    continue;
                }

                // Inspect the already opened directory instead of reopening
                // `dir` through a potentially replaced host pathname. Entries
                // may disappear while other tools work concurrently; skip
                // vanished entries rather than failing the entire listing.
                let metadata = match fs::symlink_metadata(entry.path()).await {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                let file_type = metadata.file_type();
                let kind = if file_type.is_symlink() {
                    "symlink"
                } else if file_type.is_dir() {
                    "dir"
                } else if file_type.is_file() {
                    "file"
                } else {
                    "other"
                };

                let relative = path.strip_prefix(&root).unwrap_or(&path);
                if pattern.as_ref().is_none_or(|pattern| {
                    // Entry names keep `*.rs` useful below the root; relative
                    // paths let `src/**/*.rs` address the same listing.
                    pattern.matches(relative) || pattern.matches(Path::new(&entry.file_name()))
                }) {
                    output.push(FsEntry {
                        path: self.config.access.relative_display(&path),
                        kind,
                        size: metadata.len(),
                    });
                }
                if output.len() >= max_entries {
                    // Keep the output ordering stable even when a listing is
                    // capped partway through a directory enumeration.
                    truncated = true;
                    output.sort_by(|a, b| a.path.cmp(&b.path));
                    return Ok((output, truncated));
                }
                if remaining > 1 && file_type.is_dir() && self.config.access.can_read(&path) {
                    pending.push((path, remaining - 1));
                }
            }
        }

        output.sort_by(|a, b| a.path.cmp(&b.path));
        Ok((output, truncated))
    }

    fn output_limit(&self, requested: Option<usize>) -> usize {
        resolve_capped(requested, self.config.max_output_bytes)
    }

    async fn terminate_child(child: &mut tokio::process::Child) {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    async fn execute_shell(
        &self,
        args: ShellArgs,
        powershell: bool,
        heartbeat: Option<ProgressHeartbeat>,
    ) -> Result<CallToolResult, McpError> {
        if args.command.trim().is_empty() {
            return Err(McpError::invalid_params("command must not be empty", None));
        }
        if let Some(stdin_text) = args.stdin.as_ref()
            && stdin_text.len() > self.config.max_write_bytes
        {
            return Ok(tool_error(format!(
                "stdin is {} bytes, above the {} byte limit",
                stdin_text.len(),
                self.config.max_write_bytes
            )));
        }

        let working_dir = match self.resolve_existing(&args.dir, AccessNeed::Read).await {
            Ok(path) => path,
            Err(error) => return Ok(tool_error(error)),
        };
        if !fs::metadata(&working_dir)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            return Ok(tool_error("dir is not a directory"));
        }

        let timeout_secs =
            resolve_shell_timeout(args.timeout_secs, self.config.max_shell_timeout_secs);
        let shell_program = match &self.config.shell_program {
            Some(path) => path,
            None => return Ok(tool_error("shell execution is disabled")),
        };

        #[cfg(target_os = "linux")]
        let mut command = if self.config.sandbox_shell && !powershell {
            match self.bubblewrap_command(shell_program, &working_dir, &args.command) {
                Ok(command) => command,
                Err(error) => return Ok(tool_error(error)),
            }
        } else {
            direct_shell_command(shell_program, powershell, &working_dir, &args.command)
        };
        #[cfg(not(target_os = "linux"))]
        let mut command =
            direct_shell_command(shell_program, powershell, &working_dir, &args.command);

        command
            .stdin(if args.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("DOTLINK_API_KEY")
            .env_remove("DOTLINK_API_KEY_FILE")
            .env_remove("NGROK_AUTHTOKEN_FILE")
            .env_remove("CONTROL_PLANE_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_ADMIN_KEY")
            .env_remove("NGROK_AUTHTOKEN")
            .kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return Ok(tool_error(error)),
        };

        let deadline = Instant::now() + Duration::from_secs(timeout_secs);

        if let Some(stdin_text) = args.stdin
            && let Some(mut stdin) = child.stdin.take()
        {
            match timeout_at(deadline, stdin.write_all(stdin_text.as_bytes())).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    Self::terminate_child(&mut child).await;
                    return Ok(tool_error(format!("failed writing stdin: {error}")));
                }
                Err(_) => {
                    Self::terminate_child(&mut child).await;
                    return Ok(tool_error(format!(
                        "writing command stdin timed out after {timeout_secs} seconds"
                    )));
                }
            }
        }

        let output_limit = self.output_limit(args.max_output_bytes);
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                Self::terminate_child(&mut child).await;
                return Ok(tool_error("failed to capture child stdout"));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                Self::terminate_child(&mut child).await;
                return Ok(tool_error("failed to capture child stderr"));
            }
        };
        let stdout_task = tokio::spawn(drain_capped(stdout, output_limit));
        let stderr_task = tokio::spawn(drain_capped(stderr, output_limit));

        let status = {
            let wait = timeout_at(deadline, child.wait());
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    result = &mut wait => break result,
                    () = heartbeat_tick(heartbeat.as_ref()), if heartbeat.is_some() => {
                        if let Some(heartbeat) = heartbeat.as_ref() {
                            heartbeat.report().await;
                        }
                    }
                }
            }
        };

        let status = match status {
            Ok(Ok(status)) => Some(status),
            Ok(Err(error)) => {
                Self::terminate_child(&mut child).await;
                stdout_task.abort();
                stderr_task.abort();
                return Ok(tool_error(error));
            }
            Err(_) => {
                Self::terminate_child(&mut child).await;
                None
            }
        };
        let timed_out = status.is_none();

        // A killed child closes its pipes; descendants may still hold them, so a
        // timed-out call gets a short grace to salvage partial output.
        let collect_deadline = match timed_out {
            false => deadline,
            true => Instant::now() + TIMEOUT_OUTPUT_GRACE,
        };
        let mut stdout_task = stdout_task;
        let mut stderr_task = stderr_task;
        let collected = timeout_at(collect_deadline, async {
            let stdout = (&mut stdout_task).await;
            let stderr = (&mut stderr_task).await;
            (stdout, stderr)
        })
        .await;
        let collected = match collected {
            Ok(results) => Some(results),
            Err(_) => {
                stdout_task.abort();
                stderr_task.abort();
                None
            }
        };

        let (stdout_bytes, stdout_truncated, stderr_bytes, stderr_truncated) = match collected {
            Some((stdout_result, stderr_result)) => {
                let (stdout_bytes, stdout_truncated) = match stdout_result {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => {
                        return Ok(tool_error(format!("failed reading stdout: {error}")));
                    }
                    Err(error) => {
                        return Ok(tool_error(format!("stdout reader task failed: {error}")));
                    }
                };
                let (stderr_bytes, stderr_truncated) = match stderr_result {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => {
                        return Ok(tool_error(format!("failed reading stderr: {error}")));
                    }
                    Err(error) => {
                        return Ok(tool_error(format!("stderr reader task failed: {error}")));
                    }
                };
                (
                    stdout_bytes,
                    stdout_truncated,
                    stderr_bytes,
                    stderr_truncated,
                )
            }
            None if timed_out => (Vec::new(), false, Vec::new(), false),
            None => {
                return Ok(tool_error(format!(
                    "collecting command output timed out after {timeout_secs} seconds"
                )));
            }
        };

        let mut payload = serde_json::json!({
            "exit_code": status.as_ref().and_then(|status| status.code()),
            "success": status.as_ref().is_some_and(|status| status.success()),
            "timed_out": timed_out,
            "dir": self.config.access.relative_display(&working_dir),
            "sandboxed": self.config.sandbox_shell,
            "network": self.config.allow_network,
            "stdout": String::from_utf8_lossy(&stdout_bytes),
            "stderr": String::from_utf8_lossy(&stderr_bytes),
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
        });
        if timed_out {
            // Report a real MCP error so the model knows why it has no result,
            // and keep whatever output arrived before the deadline.
            payload["error"] = Value::String(format!(
                "command timed out after {timeout_secs} seconds and was terminated; retry with a larger timeout_secs or run it in the background"
            ));
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                payload.to_string(),
            )]));
        }
        Ok(ok_text(payload.to_string()))
    }

    #[cfg(target_os = "linux")]
    fn bubblewrap_command(
        &self,
        shell: &Path,
        working_dir: &Path,
        script: &str,
    ) -> Result<Command> {
        let bwrap = self
            .config
            .bwrap_program
            .as_ref()
            .expect("bwrap validated at startup");
        let mut command = Command::new(bwrap);
        let mut mount_fds: Vec<std::fs::File> = Vec::new();
        let mut bind_fd = |command: &mut Command,
                           source: std::fs::File,
                           dest: &Path,
                           write: bool,
                           device: bool| {
            let descriptor = std::os::fd::AsRawFd::as_raw_fd(&source);
            if device {
                // Bubblewrap's regular binds set MS_NODEV, making opened
                // device nodes unusable. --dev-bind preserves character
                // device access; the source is the already-pinned FD,
                // never a mutable host pathname.
                command
                    .arg("--dev-bind")
                    .arg(format!("/proc/self/fd/{descriptor}"))
                    .arg(dest);
            } else {
                command
                    .arg(if write { "--bind-fd" } else { "--ro-bind-fd" })
                    .arg(descriptor.to_string())
                    .arg(dest);
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

        if self.config.access.unrestricted_fs {
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
        if !self.config.access.unrestricted_fs {
            command.arg("--dir").arg("/etc");
        }

        let mounts = self.config.access.sandbox_mounts();
        if !self.config.access.unrestricted_fs {
            for path in sandbox_runtime_mounts() {
                prepare_mount_target_dirs(&mut command, &path);
                command.arg("--ro-bind").arg(&path).arg(&path);
            }

            if self.config.allow_network {
                for network_path in [
                    "/etc/resolv.conf",
                    "/etc/hosts",
                    "/etc/nsswitch.conf",
                    "/etc/ssl",
                    "/etc/ca-certificates",
                    "/etc/pki",
                ] {
                    let path = Path::new(network_path);
                    if path.exists() {
                        prepare_mount_target_dirs(&mut command, path);
                        command.arg("--ro-bind").arg(path).arg(path);
                    }
                }
            }

            for (path, writable) in mounts {
                prepare_mount_target_dirs(&mut command, &path);
                let source = self.secure_fs.open_directory(&path)?;
                bind_fd(&mut command, source, &path, writable, false);
            }
        }

        for cache in &self.config.cache_mounts {
            prepare_directory_mount_target(&mut command, &cache.target);
            let source = crate::secure_fs::SecureFs::open_canonical_directory(&cache.source)?;
            bind_fd(&mut command, source, &cache.target, cache.writable, false);
        }

        for mount in &self.config.host_shell.mounts {
            prepare_mount_target_dirs(&mut command, &mount.target);
            let source = mount.pinned.try_clone()?;
            bind_fd(
                &mut command,
                source,
                &mount.target,
                false,
                mount.kind == crate::host_services::HostMountKind::CharacterDevice,
            );
        }

        if self.config.host_shell.ssh_agent {
            // OpenSSH's ssh-keygen -Y sign resolves the invoking Unix user
            // through getpwuid(). We intentionally keep /etc private by
            // default, so expose only the read-only account lookup files
            // when the owner explicitly opts into SSH agent integration.
            for path in ["/etc/passwd", "/etc/group", "/etc/nsswitch.conf"] {
                let path = Path::new(path);
                if path.is_file()
                    && !self.config.access.denied_read(path)
                    && !self
                        .config
                        .protected_paths
                        .iter()
                        .any(|protected| path == protected || path.starts_with(protected))
                {
                    prepare_mount_target_dirs(&mut command, path);
                    let (source, _) = crate::secure_fs::SecureFs::open_host_mount(
                        path,
                        crate::host_services::HostMountKind::RegularFile,
                    )?;
                    bind_fd(&mut command, source, path, false, false);
                }
            }
        }

        for denied in self.config.access.deny_write_roots() {
            if self.config.access.denied_read(denied)
                || !self.config.access.grant_read(denied)
                || !self.config.access.grant_write(denied)
            {
                continue;
            }
            prepare_mount_target_dirs(&mut command, denied);
            if denied.exists() {
                command.arg("--ro-bind").arg(denied).arg(denied);
            }
        }

        for denied in self.config.access.deny_read_roots() {
            if !(self.config.access.grant_read(denied) || self.config.access.grant_write(denied)) {
                continue;
            }
            mask_path(&mut command, denied);
        }

        for protected in &self.config.protected_paths {
            if self.config.access.grant_read(protected)
                && !self.config.access.denied_read(protected)
            {
                mask_path(&mut command, protected);
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
                mask_path(&mut command, daemon_endpoint);
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
            .arg(sandbox_visible_path(&self.config.access));

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
            .arg(shell)
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

#[cfg(target_os = "linux")]
fn mask_path(command: &mut Command, path: &Path) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };

    prepare_mount_target_dirs(command, path);

    if metadata.is_dir() {
        command
            .arg("--tmpfs")
            .arg(path)
            .arg("--chmod")
            .arg("000")
            .arg(path);
    } else {
        command.arg("--ro-bind").arg("/dev/null").arg(path);
    }
}

#[cfg(target_os = "linux")]
fn prepare_directory_mount_target(command: &mut Command, path: &Path) {
    prepare_mount_target_dirs(command, path);
    command.arg("--dir").arg(path);
}

#[cfg(target_os = "linux")]
fn prepare_mount_target_dirs(command: &mut Command, path: &Path) {
    if path == Path::new("/") {
        return;
    }

    let mut current = PathBuf::from("/");
    let components = path.components().collect::<Vec<_>>();
    let final_is_directory = path.is_dir();

    for (index, component) in components.iter().enumerate() {
        match component {
            Component::RootDir | Component::Prefix(_) => continue,
            Component::CurDir | Component::ParentDir => continue,
            Component::Normal(part) => current.push(part),
        }
        if !final_is_directory && index + 1 == components.len() {
            break;
        }
        command.arg("--dir").arg(&current);
    }
}

fn direct_shell_command(
    shell: &Path,
    powershell: bool,
    working_dir: &Path,
    script: &str,
) -> Command {
    let mut command = Command::new(shell);
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
    command
}

#[cfg(any(test, not(target_os = "linux")))]
fn is_concurrent_read_tool(name: &str) -> bool {
    matches!(name, "ls" | "read" | "bread")
}

fn summarize_tool_call_arguments(arguments: Option<&serde_json::Map<String, Value>>) -> String {
    let Some(arguments) = arguments else {
        return String::new();
    };

    let mut parts = Vec::new();
    for key in [
        "path",
        "dir",
        "command",
        "format",
        "encoding",
        "pattern",
        "depth",
        "replace_all",
        "offset",
        "limit",
        "length",
        "timeout_secs",
        "max_entries",
        "max_output_bytes",
    ] {
        let Some(value) = arguments.get(key) else {
            continue;
        };
        let rendered = match value {
            Value::String(value) if key == "command" => truncate(value, 180),
            Value::String(value) => truncate(value, 240),
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            Value::Null => "null".to_owned(),
            _ => continue,
        };
        parts.push(format!("{key}={rendered}"));
    }

    for (key, label) in [
        ("content", "content_bytes"),
        ("data", "encoded_chars"),
        ("old_text", "old_bytes"),
        ("new_text", "new_bytes"),
    ] {
        if let Some(Value::String(value)) = arguments.get(key) {
            parts.push(format!("{label}={}", value.len()));
        }
    }

    parts.join(" ")
}

#[tool_router(vis = "pub")]
impl LocalMachine {
    #[tool(
        description = "List files and directories.",
        annotations(
            title = "List files",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn ls(&self, Parameters(args): Parameters<ListArgs>) -> CallToolResult {
        match self.list_impl(args).await {
            Ok((entries, truncated)) => {
                let payload = serde_json::json!({ "entries": entries, "truncated": truncated });
                match serde_json::to_string_pretty(&payload) {
                    Ok(json) => ok_text(json),
                    Err(error) => tool_error(error),
                }
            }
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Read a UTF-8 text file. Use bread for non-text files.",
        annotations(
            title = "Read text file",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn read(&self, Parameters(args): Parameters<ReadArgs>) -> CallToolResult {
        let path = match self.resolve_existing(&args.path, AccessNeed::Read).await {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        let mut file = match self.open_read_file(&path).await {
            Ok(file) => file,
            Err(error) => return tool_error(error),
        };
        let metadata = match file.metadata().await {
            Ok(metadata) => metadata,
            Err(error) => return tool_error(error),
        };
        if metadata.len() > self.config.max_read_bytes as u64 {
            return tool_error(format!(
                "file is {} bytes, above the {} byte text-read limit",
                metadata.len(),
                self.config.max_read_bytes
            ));
        }

        let bytes = match Self::read_bounded(&mut file, self.config.max_read_bytes).await {
            Ok(bytes) => bytes,
            Err(error) => return tool_error(error),
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return tool_error("binary file; use bread"),
        };

        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(usize::MAX);
        let numbered = args.numbered.unwrap_or(true);
        let lines = text
            .lines()
            .skip(offset - 1)
            .take(limit)
            .enumerate()
            .map(|(index, line)| match numbered {
                true => format!("{}\t{line}", offset + index),
                false => line.to_owned(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        ok_text(lines)
    }

    #[tool(
        description = "Create or replace a UTF-8 text file. Missing parent directories are created automatically.",
        annotations(
            title = "Write text file",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn write(&self, Parameters(args): Parameters<WriteArgs>) -> CallToolResult {
        if args.content.len() > self.config.max_write_bytes {
            return tool_error(format!(
                "content is {} bytes, above the {} byte write limit",
                args.content.len(),
                self.config.max_write_bytes
            ));
        }
        let path = match self.resolve_for_create(&args.path, AccessNeed::Write).await {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        let mut file = match self.open_write_file(&path).await {
            Ok(file) => file,
            Err(error) => return tool_error(error),
        };
        match Self::write_complete(&mut file, args.content.as_bytes()).await {
            Ok(()) => ok_text(format!(
                "wrote {} bytes to {}",
                args.content.len(),
                args.path
            )),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Replace exact text inside an existing file.",
        annotations(
            title = "Edit text file",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn edit(&self, Parameters(args): Parameters<EditArgs>) -> CallToolResult {
        if args.old_text.is_empty() {
            return tool_error("old_text must not be empty");
        }
        let path = match self
            .resolve_existing(&args.path, AccessNeed::ReadWrite)
            .await
        {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        let mut file = match self.open_edit_file(&path).await {
            Ok(file) => file,
            Err(error) => return tool_error(error),
        };
        let bytes = match Self::read_bounded(&mut file, self.config.max_read_bytes).await {
            Ok(bytes) => bytes,
            Err(error) => return tool_error(error),
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return tool_error("binary file; use bedit"),
        };
        let matches = text.match_indices(&args.old_text).count();
        if matches == 0 {
            return tool_error("old_text was not found");
        }
        if !args.replace_all && matches != 1 {
            return tool_error(format!(
                "old_text occurs {matches} times; make the match unique or set replace_all=true"
            ));
        }

        let edited = if args.replace_all {
            text.replace(&args.old_text, &args.new_text)
        } else {
            text.replacen(&args.old_text, &args.new_text, 1)
        };
        if edited.len() > self.config.max_write_bytes {
            return tool_error(format!(
                "edited file would be {} bytes, above the {} byte write limit",
                edited.len(),
                self.config.max_write_bytes
            ));
        }

        if let Err(error) = file.rewind().await {
            return tool_error(error);
        }
        if let Err(error) = file.set_len(0).await {
            return tool_error(error);
        }
        match Self::write_complete(&mut file, edited.as_bytes()).await {
            Ok(()) => ok_text(format!("edited {}", args.path)),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Read a binary file. format=mcp returns typed MCP image/audio/blob content; base64 or hex returns encoded bytes.",
        annotations(
            title = "Read binary file",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn bread(&self, Parameters(args): Parameters<BreadArgs>) -> CallToolResult {
        let path = match self.resolve_existing(&args.path, AccessNeed::Read).await {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        let mut file = match self.open_read_file(&path).await {
            Ok(file) => file,
            Err(error) => return tool_error(error),
        };
        let metadata = match file.metadata().await {
            Ok(metadata) => metadata,
            Err(error) => return tool_error(error),
        };
        let total_size = metadata.len();

        match args.format {
            BreadFormat::Mcp => {
                if args.offset.unwrap_or(0) != 0 {
                    return tool_error("format=mcp requires offset=0");
                }
                let limit = args.limit.unwrap_or(self.config.max_read_bytes);
                if total_size > limit.min(self.config.max_read_bytes) as u64 {
                    return tool_error(
                        "format=mcp requires the complete file within the read limit; use base64 or hex for chunked reads",
                    );
                }
                let bytes = match Self::read_bounded(
                    &mut file,
                    limit.min(self.config.max_read_bytes),
                )
                .await
                {
                    Ok(bytes) => bytes,
                    Err(error) => return tool_error(error),
                };
                let encoded = BASE64.encode(bytes);
                let mime = mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_owned();
                let content = if mime.starts_with("image/") {
                    ContentBlock::image(encoded, mime)
                } else if mime.starts_with("audio/") {
                    ContentBlock::audio(encoded, mime)
                } else {
                    let uri = format!(
                        "abird://file/{}",
                        self.config.access.relative_display(&path)
                    );
                    ContentBlock::resource(
                        ResourceContents::blob(encoded, uri).with_mime_type(mime),
                    )
                };
                CallToolResult::success(vec![content])
            }
            BreadFormat::Base64 | BreadFormat::Hex => {
                let offset = args.offset.unwrap_or(0);
                if offset > total_size {
                    return tool_error("offset is beyond end of file");
                }
                let limit = args
                    .limit
                    .unwrap_or(self.config.max_read_bytes)
                    .clamp(1, self.config.max_read_bytes);
                if let Err(error) = file.seek(std::io::SeekFrom::Start(offset)).await {
                    return tool_error(error);
                }
                let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
                if let Err(error) = file.take(limit as u64).read_to_end(&mut bytes).await {
                    return tool_error(error);
                }
                let (format, encoded) = match args.format {
                    BreadFormat::Base64 => ("base64", BASE64.encode(&bytes)),
                    BreadFormat::Hex => ("hex", hex::encode(&bytes)),
                    BreadFormat::Mcp => unreachable!(),
                };
                let truncated = offset + (bytes.len() as u64) < total_size;
                ok_text(
                    serde_json::json!({
                        "path": args.path,
                        "format": format,
                        "offset": offset,
                        "bytes": bytes.len(),
                        "total_size": total_size,
                        "truncated": truncated,
                        "data": encoded,
                    })
                    .to_string(),
                )
            }
        }
    }

    #[tool(
        description = "Create or replace a binary file using base64 or hex data.",
        annotations(
            title = "Write binary file",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn bwrite(&self, Parameters(args): Parameters<BwriteArgs>) -> CallToolResult {
        let bytes = match decode_binary(&args.data, args.encoding, self.config.max_write_bytes) {
            Ok(bytes) => bytes,
            Err(error) => return tool_error(error),
        };
        let path = match self.resolve_for_create(&args.path, AccessNeed::Write).await {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        let mut file = match self.open_write_file(&path).await {
            Ok(file) => file,
            Err(error) => return tool_error(error),
        };
        match Self::write_complete(&mut file, &bytes).await {
            Ok(()) => ok_text(format!("wrote {} bytes to {}", bytes.len(), args.path)),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Patch an existing binary file by replacing a byte range.",
        annotations(
            title = "Patch binary file",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn bedit(&self, Parameters(args): Parameters<BeditArgs>) -> CallToolResult {
        let replacement =
            match decode_binary(&args.data, args.encoding, self.config.max_write_bytes) {
                Ok(bytes) => bytes,
                Err(error) => return tool_error(error),
            };
        let path = match self
            .resolve_existing(&args.path, AccessNeed::ReadWrite)
            .await
        {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        let mut file = match self.open_edit_file(&path).await {
            Ok(file) => file,
            Err(error) => return tool_error(error),
        };
        let metadata = match file.metadata().await {
            Ok(metadata) => metadata,
            Err(error) => return tool_error(error),
        };
        if metadata.len() > MAX_PATCH_FILE_BYTES as u64 {
            return tool_error(format!(
                "patch target is {} bytes; maximum patchable file size is {} bytes",
                metadata.len(),
                MAX_PATCH_FILE_BYTES
            ));
        }

        let mut bytes = match Self::read_bounded(&mut file, MAX_PATCH_FILE_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => return tool_error(error),
        };
        if args.offset > bytes.len() {
            return tool_error("offset is beyond end of file");
        }
        let length = args.length.unwrap_or(replacement.len());
        let end = match args.offset.checked_add(length) {
            Some(end) if end <= bytes.len() => end,
            _ => return tool_error("patch range extends beyond end of file"),
        };
        let resulting_size = bytes.len() - length + replacement.len();
        if resulting_size > MAX_PATCH_FILE_BYTES {
            return tool_error(format!(
                "patched file would exceed the {} byte limit",
                MAX_PATCH_FILE_BYTES
            ));
        }

        bytes.splice(args.offset..end, replacement);
        if let Err(error) = file.rewind().await {
            return tool_error(error);
        }
        if let Err(error) = file.set_len(0).await {
            return tool_error(error);
        }
        match Self::write_complete(&mut file, &bytes).await {
            Ok(()) => ok_text(format!(
                "patched {} at offset {} (replaced {} bytes)",
                args.path, args.offset, length
            )),
            Err(error) => tool_error(error),
        }
    }

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
    ) -> Result<CallToolResult, McpError> {
        self.execute_shell(args, false, ProgressHeartbeat::for_request(&context))
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
    ) -> Result<CallToolResult, McpError> {
        self.execute_shell(args, true, ProgressHeartbeat::for_request(&context))
            .await
    }
}

#[rmcp::tool_handler(
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
                    with_profile_limits(
                        tool,
                        self.config.max_shell_timeout_secs,
                        self.config.max_output_bytes,
                    )
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
            with_profile_limits(
                tool,
                self.config.max_shell_timeout_secs,
                self.config.max_output_bytes,
            )
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        let detail = summarize_tool_call_arguments(request.arguments.as_ref());
        let started = self.config.log.activity_start(&name, detail);

        let context = ToolCallContext::new(self, request, context);

        // Linux native filesystem tools use descriptor-rooted operations;
        // independent MCP requests may execute concurrently by default.
        // --serialize opts into exclusive execution for predictable ordering.
        // Other platforms retain their conservative operation gate until
        // equivalent secure handle-based filesystem access is available.
        let result = if self.serialize {
            let _guard = self.operation_gate.write().await;
            self.policy_tool_router().call(context).await
        } else {
            #[cfg(target_os = "linux")]
            {
                // Native file operations use checked directory handles, and
                // Bubblewrap gives each shell its own process/filesystem view.
                self.policy_tool_router().call(context).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                // Until the other platforms get descriptor-relative tools,
                // preserve their conservative filesystem topology gate.
                if is_concurrent_read_tool(&name) {
                    let _guard = self.operation_gate.read().await;
                    self.policy_tool_router().call(context).await
                } else {
                    let _guard = self.operation_gate.write().await;
                    self.policy_tool_router().call(context).await
                }
            }
        };

        let outcome = match &result {
            Ok(CallToolResponse::Complete(result)) if result.is_error.unwrap_or(false) => "error",
            Ok(CallToolResponse::Complete(_)) => "ok",
            Ok(CallToolResponse::InputRequired(_)) => "input-required",
            Ok(CallToolResponse::Task(_)) => "task",
            Ok(_) => "ok",
            Err(_) => "protocol-error",
        };
        self.config.log.activity_done(&name, started, outcome);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_grammar_covers_stars_classes_alternation_and_separators() {
        let matches = |pattern: &str, candidate: &str| {
            GlobFilter::new(pattern)
                .unwrap()
                .matches(Path::new(candidate))
        };

        assert!(matches("*.rs", "lib.rs"));
        assert!(matches("*", "anything"));
        assert!(matches("test_?.rs", "test_a.rs"));
        assert!(!matches("test_?.rs", "test_ab.rs"));
        assert!(matches("a*b*c", "axxbyyc"));
        assert!(matches("a*c", "ac"));
        assert!(!matches("*.rs", "lib.rs.bak"));
        assert!(matches("lib.rs", "lib.rs"));
        assert!(matches("[abc].rs", "b.rs"));
        assert!(!matches("[!abc].rs", "b.rs"));
        assert!(matches("*.{rs,md}", "notes.md"));
        assert!(!matches("*.{rs,md}", "notes.txt"));
        assert!(matches("**/*.rs", "src/nested/deep.rs"));
        assert!(matches("src/**", "src/nested/deep.rs"));
        // `literal_separator`: a bare `*` never crosses a separator.
        assert!(!matches("*.rs", "src/lib.rs"));
        assert!(!matches("src/*.rs", "src/nested/deep.rs"));
        assert!(GlobFilter::new("[unclosed").is_err());
    }

    fn test_machine(root: PathBuf) -> LocalMachine {
        #[cfg(target_os = "linux")]
        let root_for_secure_fs = root.clone();
        LocalMachine {
            config: RuntimeConfig {
                access: AccessPolicy::from_spec(AccessSpec {
                    base_dir: root.clone(),
                    read_roots: vec![root.clone()],
                    write_roots: vec![root.clone()],
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
                allow_shell: false,
                sandbox_shell: false,
                allow_network: false,
                shell_program: None,
                bwrap_program: None,
                max_shell_timeout_secs: 60,
                max_output_bytes: 65536,
                max_read_bytes: 65536,
                max_write_bytes: 65536,
                protected_paths: Vec::new(),
            },
            operation_gate: Arc::new(RwLock::new(())),
            serialize: false,
            #[cfg(target_os = "linux")]
            secure_fs: Arc::new(
                crate::secure_fs::SecureFs::new(
                    std::slice::from_ref(&root_for_secure_fs),
                    std::slice::from_ref(&root_for_secure_fs),
                )
                .unwrap(),
            ),
        }
    }

    #[tokio::test]
    async fn ls_globs_and_limits_depth() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("top.rs"), "x").unwrap();
        std::fs::write(root.join("src/lib.rs"), "x").unwrap();
        std::fs::write(root.join("src/nested/deep.rs"), "x").unwrap();
        std::fs::write(root.join("README.md"), "x").unwrap();
        let machine = test_machine(root);

        // Default depth 1: this directory only.
        let (entries, truncated) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                pattern: None,
                depth: None,
                max_entries: None,
            })
            .await
            .unwrap();
        assert!(!truncated);
        let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(names, ["README.md", "src", "top.rs"]);

        // Depth 2 descends one more level.
        let (entries, _) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                pattern: None,
                depth: Some(2),
                max_entries: None,
            })
            .await
            .unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert!(names.contains(&"src/lib.rs"), "{names:?}");
        assert!(!names.contains(&"src/nested/deep.rs"), "{names:?}");

        // Pattern filters every level it descends into.
        let (entries, _) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                pattern: Some("*.rs".to_owned()),
                depth: Some(3),
                max_entries: None,
            })
            .await
            .unwrap();
        let mut names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["src/lib.rs", "src/nested/deep.rs", "top.rs"]);

        // A capped listing reports truncation instead of silently dropping entries.
        let (entries, truncated) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                pattern: Some("*.rs".to_owned()),
                depth: Some(3),
                max_entries: Some(2),
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(truncated);
    }

    #[tokio::test]
    async fn read_numbers_lines_and_respects_ranges() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let machine = test_machine(root);

        let result = machine
            .read(Parameters(ReadArgs {
                path: "a.txt".to_owned(),
                offset: None,
                limit: None,
                numbered: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        assert_eq!(
            serialized["content"][0]["text"].as_str().unwrap(),
            "1\talpha\n2\tbeta\n3\tgamma"
        );

        let result = machine
            .read(Parameters(ReadArgs {
                path: "a.txt".to_owned(),
                offset: Some(2),
                limit: Some(1),
                numbered: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        assert_eq!(
            serialized["content"][0]["text"].as_str().unwrap(),
            "2\tbeta"
        );

        let result = machine
            .read(Parameters(ReadArgs {
                path: "a.txt".to_owned(),
                offset: Some(2),
                limit: None,
                numbered: Some(false),
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        assert_eq!(
            serialized["content"][0]["text"].as_str().unwrap(),
            "beta\ngamma"
        );
    }

    #[test]
    fn list_entry_limit_defaults_to_4096_and_caps_at_16384() {
        assert_eq!(list_entry_limit(None), 4 * 1024);
        assert_eq!(list_entry_limit(Some(10)), 10);
        assert_eq!(list_entry_limit(Some(1_000_000)), 16 * 1024);
        assert_eq!(list_entry_limit(Some(0)), 1);
    }

    #[test]
    fn shell_tool_schema_reports_configured_ceilings() {
        let bash = LocalMachine::tool_router().get("bash").cloned().unwrap();
        let patched = with_profile_limits(bash.clone(), 42, 2048);
        let properties = patched.input_schema["properties"].as_object().unwrap();
        let timeout = properties["timeout_secs"]["description"].as_str().unwrap();
        let output = properties["max_output_bytes"]["description"]
            .as_str()
            .unwrap();
        assert!(timeout.contains("42"), "{timeout}");
        assert!(!timeout.contains("max_shell_timeout_secs"), "{timeout}");
        assert!(output.contains("2048"), "{output}");
        // Non-shell tools keep their static schemas.
        let read = LocalMachine::tool_router().get("read").cloned().unwrap();
        assert_eq!(
            with_profile_limits(read.clone(), 42, 2048).input_schema,
            read.input_schema
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

    fn access(root: &Path) -> AccessPolicy {
        AccessPolicy::from_spec(AccessSpec {
            base_dir: root.to_path_buf(),
            read_roots: vec![root.to_path_buf()],
            write_roots: Vec::new(),
            deny_read_roots: Vec::new(),
            deny_write_roots: Vec::new(),
            unrestricted_fs: false,
        })
        .unwrap()
    }

    #[test]
    fn only_explicit_pure_reads_share_the_operation_gate() {
        for name in ["ls", "read", "bread"] {
            assert!(is_concurrent_read_tool(name), "{name}");
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
            assert!(!is_concurrent_read_tool(name), "{name}");
        }
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
    async fn write_targets_accept_regular_files_or_missing_paths_only() {
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
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();

        assert!(machine.require_regular_or_missing(&regular).await.is_ok());
        assert!(machine.require_regular_or_missing(&missing).await.is_ok());
        assert!(
            machine
                .require_regular_or_missing(&directory)
                .await
                .is_err()
        );
        assert!(machine.require_regular_file(&regular).await.is_ok());
        assert!(machine.require_regular_file(&directory).await.is_err());
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

    #[test]
    fn default_tool_policy_is_read_only() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(true, false, false, false)
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(names, ["bread", "ls", "read"]);
    }

    #[test]
    fn write_policy_adds_mutators() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(true, true, true, false)
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(
            names,
            ["bedit", "bread", "bwrite", "edit", "ls", "read", "write"]
        );
    }

    #[test]
    fn write_only_policy_advertises_only_write_tools() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(false, true, false, false)
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(names, ["bwrite", "write"]);
    }

    #[test]
    fn split_read_write_without_overlap_does_not_advertise_edit_tools() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(true, true, false, false)
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(names, ["bread", "bwrite", "ls", "read", "write"]);
    }

    #[test]
    fn shell_policy_is_platform_specific() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(true, false, false, true)
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        if cfg!(windows) {
            assert_eq!(names, ["bread", "ls", "powershell", "read"]);
        } else {
            assert_eq!(names, ["bash", "bread", "ls", "read"]);
        }
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
        let directory = temp.path().join("directory");
        let socket = temp.path().join("daemon.sock");
        std::fs::create_dir(&directory).unwrap();
        let _listener = UnixListener::bind(&socket).unwrap();

        let mut command = Command::new("true");
        mask_path(&mut command, &directory);
        mask_path(&mut command, &socket);
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
    fn sandbox_runtime_mounts_include_available_nix_profiles_without_home_root() {
        let mounts = sandbox_runtime_mounts();

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

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_write_deny_downgrades_rw_subtree_to_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let denied = root.join("readable-but-not-writable");
        std::fs::create_dir(&denied).unwrap();

        let access = AccessPolicy::from_spec(AccessSpec {
            base_dir: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root.clone()],
            deny_read_roots: Vec::new(),
            deny_write_roots: vec![denied.clone()],
            unrestricted_fs: false,
        })
        .unwrap();

        let mounts = access.sandbox_mounts();
        assert!(mounts.contains(&(root, true)));
        assert!(mounts.contains(&(denied, false)));
    }

    #[tokio::test]
    async fn filesystem_denies_override_cache_grants() {
        let project = tempfile::tempdir().unwrap();
        let caches = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        let denied_read = caches.path().join("deny-read");
        let denied_write = caches.path().join("deny-write");
        std::fs::create_dir(&denied_read).unwrap();
        std::fs::create_dir(&denied_write).unwrap();

        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root],
                write_roots: Vec::new(),
                deny_read_roots: vec![denied_read.clone()],
                deny_write_roots: vec![denied_write.clone()],
                unrestricted_fs: false,
            },
            cache_mounts: vec![
                SandboxCacheMount {
                    source: denied_read,
                    target: PathBuf::from("/tmp/home/.cache/read-denied"),
                    writable: true,
                },
                SandboxCacheMount {
                    source: denied_write,
                    target: PathBuf::from("/tmp/home/.cache/write-denied"),
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
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();

        assert_eq!(machine.config.cache_mounts.len(), 1);
        assert_eq!(
            machine.config.cache_mounts[0].target,
            PathBuf::from("/tmp/home/.cache/write-denied")
        );
        assert!(
            !machine.config.cache_mounts[0].writable,
            "deny-write must downgrade a shared cache to read-only"
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
                allow_shell: true,
                sandbox_shell: true,
                allow_network: false,
                shell_program: Some(PathBuf::from("/bin/bash")),
                bwrap_program: Some(PathBuf::from("/bin/bwrap")),
                max_shell_timeout_secs: 5,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,
                protected_paths: Vec::new(),
            },
            operation_gate: Arc::new(RwLock::new(())),
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
        std::fs::create_dir(&denied).unwrap();

        let machine = LocalMachine {
            config: RuntimeConfig {
                access: AccessPolicy::from_spec(AccessSpec {
                    base_dir: root.clone(),
                    read_roots: vec![root.clone()],
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
                shell_program: Some(PathBuf::from("/bin/bash")),
                bwrap_program: Some(PathBuf::from("/bin/bwrap")),
                max_shell_timeout_secs: 5,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,
                protected_paths: Vec::new(),
            },
            operation_gate: Arc::new(RwLock::new(())),
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

        assert!(args.iter().any(|arg| *arg == OsStr::new("--unshare-net")));

        for endpoint in [
            Path::new("/nix/var/nix/daemon-socket"),
            Path::new("/run/nix-daemon"),
        ] {
            let Ok(metadata) = std::fs::symlink_metadata(endpoint) else {
                continue;
            };

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
            .bubblewrap_command(Path::new("/bin/bash"), &root, "true")
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
                shell_program: Some(bash),
                bwrap_program: None,
                max_shell_timeout_secs: 2,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,
                protected_paths: Vec::new(),
            },
            operation_gate: Arc::new(RwLock::new(())),
            serialize: false,
            #[cfg(target_os = "linux")]
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
                shell_program: Some(bash),
                bwrap_program: None,
                max_shell_timeout_secs: 2,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,
                protected_paths: Vec::new(),
            },
            operation_gate: Arc::new(RwLock::new(())),
            serialize: false,
            #[cfg(target_os = "linux")]
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
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();

        // Use the executable paths resolved by LocalMachine::new.
        assert_eq!(machine.config.shell_program.as_ref(), Some(&bash));
        assert_eq!(machine.config.bwrap_program.as_ref(), Some(&bwrap));

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
            protected_paths: Vec::new(),
        })
        .await;
        let error = result.expect_err("canonical denied target must remain denied");
        assert!(error.to_string().contains("denied or protected"));
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
        let denied = root.join("denied");
        std::fs::create_dir(&writable).unwrap();
        std::fs::create_dir(&denied).unwrap();
        std::fs::write(root.join("visible.txt"), b"visible").unwrap();
        std::fs::write(denied.join("secret.txt"), b"secret").unwrap();

        let machine = LocalMachine {
            config: RuntimeConfig {
                access: AccessPolicy::from_spec(AccessSpec {
                    base_dir: root.clone(),
                    read_roots: vec![root.clone()],
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
                shell_program: Some(bash),
                bwrap_program: Some(bwrap),
                max_shell_timeout_secs: 10,
                max_output_bytes: 16 * 1024,
                max_read_bytes: 4096,
                max_write_bytes: 4096,
                protected_paths: Vec::new(),
            },
            operation_gate: Arc::new(RwLock::new(())),
            serialize: false,
            #[cfg(target_os = "linux")]
            secure_fs: Arc::new(
                crate::secure_fs::SecureFs::new(
                    std::slice::from_ref(&root),
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
                        "cat visible.txt >/dev/null; ",
                        "if cat denied/secret.txt >/dev/null 2>&1; then exit 11; fi; ",
                        "for d in /proc/self/fd/*; do if cat \"$d/denied/secret.txt\" >/dev/null 2>&1; then exit 14; fi; done; ",
                        "if touch should-not-work >/dev/null 2>&1; then exit 12; fi; ",
                        "touch writable/works; ",
                        "test ! -e /etc/resolv.conf"
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
            "sandbox smoke failed: {serialized}"
        );
        assert!(writable.join("works").exists());
        assert!(!root.join("should-not-work").exists());

        let late = writable.join("late");
        let background = machine
            .execute_shell(
                ShellArgs {
                    command: "(sleep 2; touch writable/late) >/dev/null 2>&1 &".to_owned(),
                    dir: ".".to_owned(),
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
}
