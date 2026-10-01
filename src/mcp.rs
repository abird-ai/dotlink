use std::{
    collections::BTreeSet,
    env,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use crate::logging::{LogConfig, truncate};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rmcp::{
    ErrorData as McpError,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ResourceContents,
    },
    schemars,
    service::{RequestContext, RoleServer},
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
    pub log: LogConfig,
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
}

#[derive(Debug, Clone, Copy)]
enum AccessNeed {
    Read,
    Write,
    ReadWrite,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListArgs {
    /// Directory path. Relative paths resolve from the directory where dotlink was launched; absolute paths are allowed only when permitted.
    #[serde(default = "dot")]
    path: String,

    /// Recurse into subdirectories. Symlinked directories are never traversed.
    #[serde(default)]
    recursive: bool,

    /// Maximum number of entries to return. Defaults to 500 and is capped at 5000.
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
enum ReadBinaryFormat {
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
struct ReadBinaryArgs {
    /// Binary file path.
    path: String,

    /// "mcp" returns typed MCP image/audio/blob content; base64/hex return encoded text.
    #[serde(default)]
    format: ReadBinaryFormat,

    /// Byte offset for base64/hex reads. MCP format requires offset 0.
    #[serde(default)]
    offset: Option<u64>,

    /// Maximum bytes to read. MCP format requires the complete file.
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WriteBinaryArgs {
    /// File path.
    path: String,

    /// Encoded binary payload.
    data: String,

    /// Encoding of data.
    #[serde(default)]
    encoding: BinaryEncoding,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PatchBinaryArgs {
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

    /// Timeout for this command in seconds. Defaults to 30 and is capped by server policy.
    #[serde(default)]
    timeout_secs: Option<u64>,

    /// Optional output byte limit per stream, capped by server policy.
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

fn resolve_executable(name: &str) -> Option<PathBuf> {
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

        Ok(Self {
            config: RuntimeConfig {
                access,
                cache_mounts,
                shell_env: args.shell_env,
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
            for name in ["ls", "read", "read_binary"] {
                router.disable_route(name.to_owned());
            }
        }
        if !any_write {
            for name in ["write", "write_binary"] {
                router.disable_route(name.to_owned());
            }
        }
        if !any_read_write {
            for name in ["edit", "patch_binary"] {
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

    async fn require_regular_or_missing(&self, path: &Path) -> Result<()> {
        match fs::metadata(path).await {
            Ok(metadata) if metadata.is_file() => Ok(()),
            Ok(_) => bail!("path is not a regular file: {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn require_regular_file(&self, path: &Path) -> Result<()> {
        let metadata = fs::metadata(path).await?;
        if !metadata.is_file() {
            bail!("path is not a regular file: {}", path.display());
        }
        Ok(())
    }

    async fn list_impl(&self, args: ListArgs) -> Result<Vec<FsEntry>> {
        let root = self.resolve_existing(&args.path, AccessNeed::Read).await?;
        if !fs::metadata(&root).await?.is_dir() {
            bail!("not a directory: {}", args.path);
        }

        let max_entries = args.max_entries.unwrap_or(500).clamp(1, 5000);
        let mut output = Vec::new();
        let mut pending = vec![root];

        while let Some(dir) = pending.pop() {
            let mut reader = fs::read_dir(&dir).await?;
            while let Some(entry) = reader.next_entry().await? {
                let path = entry.path();

                if self.config.access.denied_read(&path)
                    || self.ensure_not_protected(&path).is_err()
                {
                    continue;
                }

                let metadata = fs::symlink_metadata(&path).await?;
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

                output.push(FsEntry {
                    path: self.config.access.relative_display(&path),
                    kind,
                    size: metadata.len(),
                });
                if output.len() >= max_entries {
                    return Ok(output);
                }
                if args.recursive && file_type.is_dir() && self.config.access.can_read(&path) {
                    pending.push(path);
                }
            }
        }

        output.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(output)
    }

    fn output_limit(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.config.max_output_bytes)
            .clamp(1, self.config.max_output_bytes)
    }

    async fn terminate_child(child: &mut tokio::process::Child) {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    async fn execute_shell(
        &self,
        args: ShellArgs,
        powershell: bool,
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

        let timeout_secs = args
            .timeout_secs
            .unwrap_or(30)
            .clamp(1, self.config.max_shell_timeout_secs);
        let shell_program = match &self.config.shell_program {
            Some(path) => path,
            None => return Ok(tool_error("shell execution is disabled")),
        };

        #[cfg(target_os = "linux")]
        let mut command = if self.config.sandbox_shell && !powershell {
            self.bubblewrap_command(shell_program, &working_dir, &args.command)
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

        let status = match timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                Self::terminate_child(&mut child).await;
                stdout_task.abort();
                stderr_task.abort();
                return Ok(tool_error(error));
            }
            Err(_) => {
                Self::terminate_child(&mut child).await;
                stdout_task.abort();
                stderr_task.abort();
                return Ok(tool_error(format!(
                    "command timed out after {timeout_secs} seconds and was terminated"
                )));
            }
        };

        let mut stdout_task = stdout_task;
        let mut stderr_task = stderr_task;
        let collected = timeout_at(deadline, async {
            let stdout = (&mut stdout_task).await;
            let stderr = (&mut stderr_task).await;
            (stdout, stderr)
        })
        .await;
        let (stdout_result, stderr_result) = match collected {
            Ok(results) => results,
            Err(_) => {
                stdout_task.abort();
                stderr_task.abort();
                return Ok(tool_error(format!(
                    "collecting command output timed out after {timeout_secs} seconds"
                )));
            }
        };

        let (stdout_bytes, stdout_truncated) = match stdout_result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Ok(tool_error(format!("failed reading stdout: {error}"))),
            Err(error) => return Ok(tool_error(format!("stdout reader task failed: {error}"))),
        };
        let (stderr_bytes, stderr_truncated) = match stderr_result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Ok(tool_error(format!("failed reading stderr: {error}"))),
            Err(error) => return Ok(tool_error(format!("stderr reader task failed: {error}"))),
        };

        Ok(ok_text(
            serde_json::json!({
                "exit_code": status.code(),
                "success": status.success(),
                "dir": self.config.access.relative_display(&working_dir),
                "sandboxed": self.config.sandbox_shell,
                "network": self.config.allow_network,
                "stdout": String::from_utf8_lossy(&stdout_bytes),
                "stderr": String::from_utf8_lossy(&stderr_bytes),
                "stdout_truncated": stdout_truncated,
                "stderr_truncated": stderr_truncated,
            })
            .to_string(),
        ))
    }

    #[cfg(target_os = "linux")]
    fn bubblewrap_command(&self, shell: &Path, working_dir: &Path, script: &str) -> Command {
        let bwrap = self
            .config
            .bwrap_program
            .as_ref()
            .expect("bwrap validated at startup");
        let mut command = Command::new(bwrap);

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

        if !self.config.allow_network {
            for daemon_endpoint in [
                Path::new("/nix/var/nix/daemon-socket"),
                Path::new("/run/nix-daemon"),
            ] {
                mask_path(&mut command, daemon_endpoint);
            }
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
                if writable {
                    command.arg("--bind");
                } else {
                    command.arg("--ro-bind");
                }
                command.arg(&path).arg(&path);
            }
        }

        for cache in &self.config.cache_mounts {
            prepare_directory_mount_target(&mut command, &cache.target);
            if cache.writable {
                command.arg("--bind");
            } else {
                command.arg("--ro-bind");
            }
            command.arg(&cache.source).arg(&cache.target);
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

        command
            .arg("--")
            .arg(shell)
            .arg("--noprofile")
            .arg("--norc")
            .arg("-lc")
            .arg(script);
        command
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

fn is_concurrent_read_tool(name: &str) -> bool {
    matches!(name, "ls" | "read" | "read_binary")
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
        "recursive",
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
        description = "List files and directories allowed by the active read policy.",
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
            Ok(entries) => match serde_json::to_string_pretty(&entries) {
                Ok(json) => ok_text(json),
                Err(error) => tool_error(error),
            },
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Read an allowed UTF-8 text file. Use read_binary for non-text files.",
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
        let metadata = match fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return tool_error("path is not a regular file"),
            Err(error) => return tool_error(error),
        };
        if metadata.len() > self.config.max_read_bytes as u64 {
            return tool_error(format!(
                "file is {} bytes, above the {} byte text-read limit",
                metadata.len(),
                self.config.max_read_bytes
            ));
        }

        let bytes = match fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) => return tool_error(error),
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return tool_error("binary file; use read_binary"),
        };

        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(usize::MAX);
        ok_text(
            text.lines()
                .skip(offset - 1)
                .take(limit)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    #[tool(
        description = "Create or replace an allowed UTF-8 text file. Missing parent directories are created automatically.",
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
        if let Err(error) = self.require_regular_or_missing(&path).await {
            return tool_error(error);
        }
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent).await
        {
            return tool_error(error);
        }
        match fs::write(&path, args.content.as_bytes()).await {
            Ok(()) => ok_text(format!(
                "wrote {} bytes to {}",
                args.content.len(),
                args.path
            )),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Replace exact text inside an existing file. Requires both read and write permission.",
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
        if let Err(error) = self.require_regular_file(&path).await {
            return tool_error(error);
        }
        let bytes = match fs::read(&path).await {
            Ok(bytes) if bytes.len() <= self.config.max_read_bytes => bytes,
            Ok(bytes) => {
                return tool_error(format!(
                    "file is {} bytes, above the {} byte edit limit",
                    bytes.len(),
                    self.config.max_read_bytes
                ));
            }
            Err(error) => return tool_error(error),
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return tool_error("binary file; use patch_binary"),
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

        match fs::write(&path, edited.as_bytes()).await {
            Ok(()) => ok_text(format!("edited {}", args.path)),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Read an allowed binary file. format=mcp returns typed MCP image/audio/blob content; base64 or hex returns encoded bytes.",
        annotations(
            title = "Read binary file",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn read_binary(&self, Parameters(args): Parameters<ReadBinaryArgs>) -> CallToolResult {
        let path = match self.resolve_existing(&args.path, AccessNeed::Read).await {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        let metadata = match fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return tool_error("path is not a regular file"),
            Err(error) => return tool_error(error),
        };
        let total_size = metadata.len();

        match args.format {
            ReadBinaryFormat::Mcp => {
                if args.offset.unwrap_or(0) != 0 {
                    return tool_error("format=mcp requires offset=0");
                }
                let limit = args.limit.unwrap_or(self.config.max_read_bytes);
                if total_size > limit.min(self.config.max_read_bytes) as u64 {
                    return tool_error(
                        "format=mcp requires the complete file within the read limit; use base64 or hex for chunked reads",
                    );
                }
                let bytes = match fs::read(&path).await {
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
            ReadBinaryFormat::Base64 | ReadBinaryFormat::Hex => {
                let offset = args.offset.unwrap_or(0);
                if offset > total_size {
                    return tool_error("offset is beyond end of file");
                }
                let limit = args
                    .limit
                    .unwrap_or(self.config.max_read_bytes)
                    .clamp(1, self.config.max_read_bytes);
                let mut file = match fs::File::open(&path).await {
                    Ok(file) => file,
                    Err(error) => return tool_error(error),
                };
                if let Err(error) = file.seek(std::io::SeekFrom::Start(offset)).await {
                    return tool_error(error);
                }
                let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
                if let Err(error) = file.take(limit as u64).read_to_end(&mut bytes).await {
                    return tool_error(error);
                }
                let (format, encoded) = match args.format {
                    ReadBinaryFormat::Base64 => ("base64", BASE64.encode(&bytes)),
                    ReadBinaryFormat::Hex => ("hex", hex::encode(&bytes)),
                    ReadBinaryFormat::Mcp => unreachable!(),
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
        description = "Create or replace an allowed binary file using base64 or hex data.",
        annotations(
            title = "Write binary file",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn write_binary(&self, Parameters(args): Parameters<WriteBinaryArgs>) -> CallToolResult {
        let bytes = match decode_binary(&args.data, args.encoding, self.config.max_write_bytes) {
            Ok(bytes) => bytes,
            Err(error) => return tool_error(error),
        };
        let path = match self.resolve_for_create(&args.path, AccessNeed::Write).await {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        if let Err(error) = self.require_regular_or_missing(&path).await {
            return tool_error(error);
        }
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent).await
        {
            return tool_error(error);
        }
        match fs::write(&path, &bytes).await {
            Ok(()) => ok_text(format!("wrote {} bytes to {}", bytes.len(), args.path)),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Patch an existing binary file by replacing a byte range. Requires read+write permission.",
        annotations(
            title = "Patch binary file",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn patch_binary(&self, Parameters(args): Parameters<PatchBinaryArgs>) -> CallToolResult {
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
        let metadata = match fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return tool_error("path is not a regular file"),
            Err(error) => return tool_error(error),
        };
        if metadata.len() > MAX_PATCH_FILE_BYTES as u64 {
            return tool_error(format!(
                "patch target is {} bytes; maximum patchable file size is {} bytes",
                metadata.len(),
                MAX_PATCH_FILE_BYTES
            ));
        }

        let mut bytes = match fs::read(&path).await {
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
        match fs::write(&path, &bytes).await {
            Ok(()) => ok_text(format!(
                "patched {} at offset {} (replaced {} bytes)",
                args.path, args.offset, length
            )),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Run Bash. On Linux it is Bubblewrap-sandboxed unless --no-sandbox --allow-all was explicitly selected.",
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
    ) -> Result<CallToolResult, McpError> {
        self.execute_shell(args, false).await
    }

    #[tool(
        description = "Run PowerShell on Windows. Unsandboxed shell access requires --no-sandbox --allow-all.",
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
    ) -> Result<CallToolResult, McpError> {
        self.execute_shell(args, true).await
    }
}

#[rmcp::tool_handler(
    router = self.policy_tool_router(),
    name = "dotlink",
    instructions = "Private local-machine bridge. Filesystem access follows additive allow-read/allow-write/allow-rw grants; deny rules take precedence. Shell is hidden unless enabled. Linux shell execution is Bubblewrap-sandboxed by default with network disabled unless explicitly allowed."
)]
impl rmcp::ServerHandler for LocalMachine {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        let detail = summarize_tool_call_arguments(request.arguments.as_ref());
        let started = self.config.log.activity_start(&name, detail);

        let context = ToolCallContext::new(self, request, context);

        // Path authorization canonicalizes before the actual host operation.
        // Keep dotlink-originated mutations/shell execution exclusive with all
        // filesystem calls so another concurrent tool cannot swap a symlink
        // between authorization and use. Unknown/future tools default to the
        // exclusive side until explicitly reviewed as pure reads.
        let result = if is_concurrent_read_tool(&name) {
            let _guard = self.operation_gate.read().await;
            self.policy_tool_router().call(context).await
        } else {
            let _guard = self.operation_gate.write().await;
            self.policy_tool_router().call(context).await
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
        for name in ["ls", "read", "read_binary"] {
            assert!(is_concurrent_read_tool(name), "{name}");
        }
        for name in [
            "write",
            "edit",
            "write_binary",
            "patch_binary",
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
            log: LogConfig::default(),
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
            log: LogConfig::default(),
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
            log: LogConfig::default(),
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
        assert_eq!(names, ["ls", "read", "read_binary"]);
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
            [
                "edit",
                "ls",
                "patch_binary",
                "read",
                "read_binary",
                "write",
                "write_binary",
            ]
        );
    }

    #[test]
    fn write_only_policy_advertises_only_write_tools() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(false, true, false, false)
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(names, ["write", "write_binary"]);
    }

    #[test]
    fn split_read_write_without_overlap_does_not_advertise_edit_tools() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(true, true, false, false)
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(
            names,
            ["ls", "read", "read_binary", "write", "write_binary"]
        );
    }

    #[test]
    fn shell_policy_is_platform_specific() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(true, false, false, true)
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        if cfg!(windows) {
            assert_eq!(names, ["ls", "powershell", "read", "read_binary"]);
        } else {
            assert_eq!(names, ["bash", "ls", "read", "read_binary"]);
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
            log: LogConfig::default(),
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
            log: LogConfig::default(),
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
        };

        let command = machine.bubblewrap_command(Path::new("/bin/bash"), &root, "true");
        let args: Vec<_> = command.as_std().get_args().collect();

        assert!(args.windows(3).any(|window| {
            window[0] == OsStr::new("--ro-bind")
                && window[1] == read_only.as_os_str()
                && window[2] == OsStr::new("/tmp/home/.cargo/registry")
        }));
        assert!(args.windows(3).any(|window| {
            window[0] == OsStr::new("--bind")
                && window[1] == read_write.as_os_str()
                && window[2] == OsStr::new("/tmp/home/.cargo/git")
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
        };

        let command = machine.bubblewrap_command(Path::new("/bin/bash"), &root, "true");
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
        let command = networked.bubblewrap_command(Path::new("/bin/bash"), &root, "true");
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
            log: LogConfig::default(),
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
                false,
            )
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
        };

        let result = machine
            .execute_shell(
                ShellArgs {
                    command: concat!(
                        "set -eu; ",
                        "cat visible.txt >/dev/null; ",
                        "if cat denied/secret.txt >/dev/null 2>&1; then exit 11; fi; ",
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
                false,
            )
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
