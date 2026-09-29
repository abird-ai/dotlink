use std::{
    collections::BTreeSet,
    env,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rmcp::{
    ErrorData as McpError,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ResourceContents},
    schemars, tool, tool_router,
};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

const MAX_PATCH_FILE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct AccessSpec {
    pub cwd: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub deny_read_roots: Vec<PathBuf>,
    pub deny_write_roots: Vec<PathBuf>,
    pub rw_all_dangerous: bool,
}

#[derive(Clone, Debug)]
pub struct MachineConfig {
    pub access: AccessSpec,
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
struct AccessPolicy {
    cwd: PathBuf,
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
    deny_read_roots: Vec<PathBuf>,
    deny_write_roots: Vec<PathBuf>,
    rw_all_dangerous: bool,
}

#[derive(Clone, Debug)]
struct RuntimeConfig {
    access: AccessPolicy,
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
}

#[derive(Debug, Clone, Copy)]
enum AccessNeed {
    Read,
    Write,
    ReadWrite,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListArgs {
    /// Directory path. Relative paths resolve from --cwd; absolute paths are allowed only when permitted.
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
    /// UTF-8 text file path. Relative paths resolve from --cwd.
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
    /// File path. Relative paths resolve from --cwd.
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

    /// Working directory. Relative paths resolve from --cwd.
    #[serde(default = "dot")]
    cwd: String,

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

async fn canonicalize_grant(cwd: &Path, path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let canonical = fs::canonicalize(&joined)
        .await
        .with_context(|| format!("grant path does not exist: {}", joined.display()))?;
    if !fs::metadata(&canonical).await?.is_dir() {
        bail!("grant path is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

async fn canonicalize_deny(cwd: &Path, path: &Path) -> Result<PathBuf> {
    let target = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
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
        if !spec.cwd.is_absolute()
            || spec.read_roots.iter().any(|path| !path.is_absolute())
            || spec.write_roots.iter().any(|path| !path.is_absolute())
            || spec.deny_read_roots.iter().any(|path| !path.is_absolute())
            || spec.deny_write_roots.iter().any(|path| !path.is_absolute())
        {
            bail!("internal error: access policy paths must be canonical absolute paths");
        }

        Ok(Self {
            cwd: spec.cwd,
            read_roots: spec.read_roots,
            write_roots: spec.write_roots,
            deny_read_roots: spec.deny_read_roots,
            deny_write_roots: spec.deny_write_roots,
            rw_all_dangerous: spec.rw_all_dangerous,
        })
    }

    fn lexical_target(&self, input: &str) -> Result<PathBuf> {
        let path = Path::new(input);
        if contains_parent_component(path) {
            bail!("parent traversal ('..') is not accepted; use a canonical path instead");
        }
        if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(self.cwd.join(path))
        }
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
        self.rw_all_dangerous || self.read_roots.iter().any(|root| path.starts_with(root))
    }

    fn grant_write(&self, path: &Path) -> bool {
        self.rw_all_dangerous || self.write_roots.iter().any(|root| path.starts_with(root))
    }

    fn can_read(&self, path: &Path) -> bool {
        !self.denied_read(path) && self.grant_read(path)
    }

    fn can_write(&self, path: &Path) -> bool {
        !self.denied_write(path) && self.grant_write(path)
    }

    fn check(&self, path: &Path, need: AccessNeed) -> Result<()> {
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
        let canonical = fs::canonicalize(&target)
            .await
            .with_context(|| format!("path does not exist or cannot be resolved: {input}"))?;
        self.check(&canonical, need)?;
        Ok(canonical)
    }

    async fn resolve_for_create(&self, input: &str, need: AccessNeed) -> Result<PathBuf> {
        let target = self.lexical_target(input)?;

        match fs::symlink_metadata(&target).await {
            Ok(_) => {
                let canonical = fs::canonicalize(&target)
                    .await
                    .with_context(|| format!("existing path cannot be safely resolved: {input}"))?;
                self.check(&canonical, need)?;
                return Ok(target);
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
        path.strip_prefix(&self.cwd)
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
        if self.rw_all_dangerous {
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
}

impl LocalMachine {
    pub async fn new(args: MachineConfig) -> Result<Self> {
        let cwd = fs::canonicalize(&args.access.cwd)
            .await
            .with_context(|| format!("cwd does not exist: {}", args.access.cwd.display()))?;
        if !fs::metadata(&cwd).await?.is_dir() {
            bail!("cwd is not a directory: {}", cwd.display());
        }

        let mut read_roots = Vec::with_capacity(args.access.read_roots.len());
        for path in &args.access.read_roots {
            read_roots.push(canonicalize_grant(&cwd, path).await?);
        }
        let mut write_roots = Vec::with_capacity(args.access.write_roots.len());
        for path in &args.access.write_roots {
            write_roots.push(canonicalize_grant(&cwd, path).await?);
        }
        let mut deny_read_roots = Vec::with_capacity(args.access.deny_read_roots.len());
        for path in &args.access.deny_read_roots {
            deny_read_roots.push(canonicalize_deny(&cwd, path).await?);
        }
        let mut deny_write_roots = Vec::with_capacity(args.access.deny_write_roots.len());
        for path in &args.access.deny_write_roots {
            deny_write_roots.push(canonicalize_deny(&cwd, path).await?);
        }

        let access = AccessPolicy::from_spec(AccessSpec {
            cwd,
            read_roots,
            write_roots,
            deny_read_roots,
            deny_write_roots,
            rw_all_dangerous: args.access.rw_all_dangerous,
        })?;

        let mut protected_paths = Vec::with_capacity(args.protected_paths.len());
        for path in args.protected_paths {
            protected_paths.push(fs::canonicalize(&path).await.unwrap_or(path));
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
        })
    }

    pub fn cwd(&self) -> &Path {
        &self.config.access.cwd
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
        if self.config.access.rw_all_dangerous {
            return "unrestricted read+write".to_owned();
        }
        format!(
            "read:{} write:{} deny-read:{} deny-write:{}{}",
            self.config.access.read_roots.len(),
            self.config.access.write_roots.len(),
            self.config.access.deny_read_roots.len(),
            self.config.access.deny_write_roots.len(),
            if self.config.allow_shell {
                " + shell"
            } else {
                ""
            }
        )
    }

    pub fn tool_router_for_policy(any_write: bool, allow_shell: bool) -> ToolRouter<Self> {
        let mut router = Self::tool_router();

        if !any_write {
            for name in ["write", "edit", "write_binary", "patch_binary"] {
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
            self.config.access.rw_all_dangerous || !self.config.access.write_roots.is_empty(),
            self.config.allow_shell,
        )
    }

    fn ensure_not_protected(&self, path: &Path) -> Result<()> {
        for protected in &self.config.protected_paths {
            if path == protected || path.starts_with(protected) {
                bail!(
                    "path is protected by abird-link and cannot be accessed through filesystem tools"
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

        let cwd = match self.resolve_existing(&args.cwd, AccessNeed::Read).await {
            Ok(path) => path,
            Err(error) => return Ok(tool_error(error)),
        };
        if !fs::metadata(&cwd)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            return Ok(tool_error("cwd is not a directory"));
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
            self.bubblewrap_command(shell_program, &cwd, &args.command)
        } else {
            direct_shell_command(shell_program, powershell, &cwd, &args.command)
        };
        #[cfg(not(target_os = "linux"))]
        let mut command = direct_shell_command(shell_program, powershell, &cwd, &args.command);

        command
            .stdin(if args.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("ABIRD_LINK_API_KEY")
            .env_remove("CONTROL_PLANE_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_ADMIN_KEY")
            .env_remove("NGROK_AUTHTOKEN")
            .kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return Ok(tool_error(error)),
        };

        if let Some(stdin_text) = args.stdin
            && let Some(mut stdin) = child.stdin.take()
        {
            match timeout(
                Duration::from_secs(timeout_secs),
                stdin.write_all(stdin_text.as_bytes()),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    let _ = child.kill().await;
                    return Ok(tool_error(format!("failed writing stdin: {error}")));
                }
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return Ok(tool_error(format!(
                        "writing command stdin timed out after {timeout_secs} seconds"
                    )));
                }
            }
        }

        let output_limit = self.output_limit(args.max_output_bytes);
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => return Ok(tool_error("failed to capture child stdout")),
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => return Ok(tool_error("failed to capture child stderr")),
        };
        let stdout_task = tokio::spawn(drain_capped(stdout, output_limit));
        let stderr_task = tokio::spawn(drain_capped(stderr, output_limit));

        let status = match timeout(Duration::from_secs(timeout_secs), child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                stdout_task.abort();
                stderr_task.abort();
                return Ok(tool_error(error));
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                stdout_task.abort();
                stderr_task.abort();
                return Ok(tool_error(format!(
                    "command timed out after {timeout_secs} seconds and was terminated"
                )));
            }
        };

        let (stdout_bytes, stdout_truncated) = match stdout_task.await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Ok(tool_error(format!("failed reading stdout: {error}"))),
            Err(error) => return Ok(tool_error(format!("stdout reader task failed: {error}"))),
        };
        let (stderr_bytes, stderr_truncated) = match stderr_task.await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Ok(tool_error(format!("failed reading stderr: {error}"))),
            Err(error) => return Ok(tool_error(format!("stderr reader task failed: {error}"))),
        };

        Ok(ok_text(
            serde_json::json!({
                "exit_code": status.code(),
                "success": status.success(),
                "cwd": self.config.access.relative_display(&cwd),
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
    fn bubblewrap_command(&self, shell: &Path, cwd: &Path, script: &str) -> Command {
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

        if self.config.access.rw_all_dangerous {
            command.arg("--bind").arg("/").arg("/");
        }

        if !self.config.allow_network {
            for daemon_socket in [
                Path::new("/nix/var/nix/daemon-socket/socket"),
                Path::new("/run/nix-daemon/socket"),
            ] {
                if daemon_socket.exists() {
                    add_parent_dirs(&mut command, daemon_socket);
                    command.arg("--ro-bind").arg("/dev/null").arg(daemon_socket);
                }
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
        if !self.config.access.rw_all_dangerous {
            command.arg("--dir").arg("/etc");
        }

        let mounts = self.config.access.sandbox_mounts();
        if !self.config.access.rw_all_dangerous {
            for path in sandbox_runtime_mounts() {
                add_parent_dirs(&mut command, &path);
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
                        add_parent_dirs(&mut command, path);
                        command.arg("--ro-bind").arg(path).arg(path);
                    }
                }
            }

            for (path, writable) in mounts {
                add_parent_dirs(&mut command, &path);
                if writable {
                    command.arg("--bind");
                } else {
                    command.arg("--ro-bind");
                }
                command.arg(&path).arg(&path);
            }
        }

        for denied in self.config.access.deny_write_roots() {
            if self.config.access.denied_read(denied)
                || !self.config.access.grant_read(denied)
                || !self.config.access.grant_write(denied)
            {
                continue;
            }
            add_parent_dirs(&mut command, denied);
            if denied.exists() {
                command.arg("--ro-bind").arg(denied).arg(denied);
            }
        }

        for denied in self.config.access.deny_read_roots() {
            if !(self.config.access.grant_read(denied) || self.config.access.grant_write(denied)) {
                continue;
            }
            add_parent_dirs(&mut command, denied);
            match std::fs::symlink_metadata(denied) {
                Ok(metadata) if metadata.is_dir() => {
                    command
                        .arg("--tmpfs")
                        .arg(denied)
                        .arg("--chmod")
                        .arg("000")
                        .arg(denied);
                }
                Ok(_) => {
                    command.arg("--ro-bind").arg("/dev/null").arg(denied);
                }
                Err(_) => {
                    // Startup validation rejects this case for sandboxed granted trees.
                }
            }
        }

        for protected in &self.config.protected_paths {
            if self.config.access.grant_read(protected)
                && !self.config.access.denied_read(protected)
            {
                add_parent_dirs(&mut command, protected);
                command.arg("--ro-bind").arg("/dev/null").arg(protected);
            }
        }

        command
            .arg("--chdir")
            .arg(cwd)
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
fn add_parent_dirs(command: &mut Command, path: &Path) {
    if path == Path::new("/") {
        return;
    }

    let mut current = PathBuf::from("/");
    let components = path.components().collect::<Vec<_>>();
    let final_is_file = path.is_file();

    for (index, component) in components.iter().enumerate() {
        match component {
            Component::RootDir | Component::Prefix(_) => continue,
            Component::CurDir | Component::ParentDir => continue,
            Component::Normal(part) => current.push(part),
        }
        if final_is_file && index + 1 == components.len() {
            break;
        }
        command.arg("--dir").arg(&current);
    }
}

fn direct_shell_command(shell: &Path, powershell: bool, cwd: &Path, script: &str) -> Command {
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
    command.current_dir(cwd);
    command
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
        description = "Run Bash. On Linux it is Bubblewrap-sandboxed unless dangerous unsandboxed access was explicitly enabled.",
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
        description = "Run PowerShell on Windows. Unsandboxed use requires explicit dangerous access.",
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
    name = "abird-link",
    instructions = "Private local-machine bridge. Filesystem access follows additive allow-read/allow-write/allow-rw grants; deny rules take precedence. Shell is hidden unless enabled. Linux shell execution is Bubblewrap-sandboxed by default with network disabled unless explicitly allowed."
)]
impl rmcp::ServerHandler for LocalMachine {}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(root: &Path) -> AccessPolicy {
        AccessPolicy::from_spec(AccessSpec {
            cwd: root.to_path_buf(),
            read_roots: vec![root.to_path_buf()],
            write_roots: Vec::new(),
            deny_read_roots: Vec::new(),
            deny_write_roots: Vec::new(),
            rw_all_dangerous: false,
        })
        .unwrap()
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
            cwd: root.clone(),
            read_roots: vec![root],
            write_roots: vec![child.clone()],
            deny_read_roots: Vec::new(),
            deny_write_roots: Vec::new(),
            rw_all_dangerous: false,
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
            cwd: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root],
            deny_read_roots: vec![denied.clone()],
            deny_write_roots: vec![denied.clone()],
            rw_all_dangerous: false,
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
            cwd: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root],
            deny_read_roots: vec![read_denied.clone()],
            deny_write_roots: vec![write_denied.clone()],
            rw_all_dangerous: false,
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
            cwd: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root],
            deny_read_roots: Vec::new(),
            deny_write_roots: Vec::new(),
            rw_all_dangerous: false,
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

    #[tokio::test]
    async fn protected_credentials_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let secret = root.join("runtime.key");
        std::fs::write(&secret, b"secret").unwrap();

        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                cwd: root.clone(),
                read_roots: vec![root],
                write_roots: Vec::new(),
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                rw_all_dangerous: false,
            },
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

    #[test]
    fn default_tool_policy_is_read_only() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(false, false)
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(names, ["ls", "read", "read_binary"]);
    }

    #[test]
    fn write_policy_adds_mutators() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(true, false)
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
    fn shell_policy_is_platform_specific() {
        let names: Vec<_> = LocalMachine::tool_router_for_policy(false, true)
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
            cwd: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![writable.clone()],
            deny_read_roots: vec![denied.clone()],
            deny_write_roots: vec![denied.clone()],
            rw_all_dangerous: false,
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
            cwd: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root.clone()],
            deny_read_roots: Vec::new(),
            deny_write_roots: vec![denied.clone()],
            rw_all_dangerous: false,
        })
        .unwrap();

        let mounts = access.sandbox_mounts();
        assert!(mounts.contains(&(root, true)));
        assert!(mounts.contains(&(denied, false)));
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
                    cwd: root.clone(),
                    read_roots: vec![root.clone()],
                    write_roots: Vec::new(),
                    deny_read_roots: vec![denied.clone()],
                    deny_write_roots: vec![denied.clone()],
                    rw_all_dangerous: false,
                })
                .unwrap(),
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
        };

        let command = machine.bubblewrap_command(Path::new("/bin/bash"), &root, "true");
        let args: Vec<_> = command.as_std().get_args().collect();

        assert!(args.iter().any(|arg| *arg == OsStr::new("--unshare-net")));

        for socket in [
            Path::new("/nix/var/nix/daemon-socket/socket"),
            Path::new("/run/nix-daemon/socket"),
        ] {
            if socket.exists() {
                assert!(
                    args.windows(3).any(|window| {
                        window[0] == OsStr::new("--ro-bind")
                            && window[1] == OsStr::new("/dev/null")
                            && window[2] == socket.as_os_str()
                    }),
                    "Nix daemon socket should be masked when network is denied"
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bubblewrap_runtime_smoke_when_requested() {
        if std::env::var_os("ABIRD_TEST_BWRAP").is_none() {
            return;
        }

        let Some(bwrap) = resolve_executable("bwrap") else {
            panic!("ABIRD_TEST_BWRAP requested but bwrap is not installed");
        };
        let Some(bash) = resolve_executable("bash") else {
            panic!("ABIRD_TEST_BWRAP requested but bash is not installed");
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
                    cwd: root.clone(),
                    read_roots: vec![root.clone()],
                    write_roots: vec![writable.clone()],
                    deny_read_roots: vec![denied.clone()],
                    deny_write_roots: vec![denied],
                    rw_all_dangerous: false,
                })
                .unwrap(),
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
                    cwd: ".".to_owned(),
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
    }
}
