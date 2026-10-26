//! The local machine: access policy, pinned descriptors, and the tool surface.
//!
//! Tool implementations live in [`tools`], one module per family. This module
//! owns what they share: who may read or write what, the descriptor-rooted file
//! primitives, the advertised-tool table and the MCP server handler.

mod tools;

use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::{
    ErrorData as McpError,
    handler::server::router::tool::ToolRouter,
    handler::server::tool::ToolCallContext,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ResultType, Tool,
    },
    service::{RequestContext, RoleServer},
    tool_handler,
};

use serde_json::Value;
use tokio::{fs, io::AsyncReadExt, sync::RwLock};

use crate::logging::LogConfig;

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
    /// The advertised surface, built once. Visibility depends only on the
    /// immutable access policy, so every request can share one router instead
    /// of merging the per-family routers again.
    router: OnceLock<Arc<ToolRouter<Self>>>,
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

/// Worst-case JSON escaping of one payload byte: a control byte becomes
/// `\u00XX`. Payloads bounded by `max_write_bytes` therefore never need a body
/// larger than this multiple of it.
const MAX_JSON_ESCAPE: usize = 6;
/// Room for what surrounds a payload: paths, patterns, edit anchors, framing.
const MAX_REQUEST_FRAMING_BYTES: usize = 64 * 1024;

/// A configured limit is a hard ceiling: reject a value the runtime cannot
/// honour instead of quietly raising it.
fn checked_limit<T: PartialOrd + std::fmt::Display>(name: &str, value: T, minimum: T) -> Result<T> {
    if value < minimum {
        bail!("{name} must be at least {minimum}, got {value}; raise it in the profile or CLI");
    }
    Ok(value)
}

/// True for the shell tool this platform advertises. Tool visibility, the
/// schema-limit rewrite, the tunnel's delivery reserve and the TOOL log all ask
/// this one question, so a new shell variant cannot be missed in one of them.
pub fn is_shell_tool(name: &str) -> bool {
    let shell = Capabilities {
        shell: true,
        ..Capabilities::default()
    };
    TOOL_SPECS
        .iter()
        .any(|spec| spec.name == name && shell.allows(spec.capability))
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
];

impl Capabilities {
    pub fn allows(self, capability: Capability) -> bool {
        match capability {
            Capability::Read => self.read,
            Capability::Write => self.write,
            Capability::ReadWrite => self.read_write,
            Capability::ShellUnix => self.shell && !cfg!(windows),
            Capability::ShellWindows => self.shell && cfg!(windows),
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

/// Deny and protected roots may name paths that do not exist yet, so the nearest
/// existing ancestor is canonicalized and the remainder is appended. The target
/// is folded lexically first: a `..` after a missing component would otherwise
/// survive into the comparison and quietly fail to match the path it denies.
async fn canonicalize_deny(base_dir: &Path, path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };
    let target = lexical_normalize(&joined);

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
                return Ok(lexical_normalize(&canonical_ancestor.join(remainder)));
            }
            Err(_) => {
                if !ancestor.pop() {
                    bail!("could not resolve deny path {}", target.display());
                }
            }
        }
    }
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

        #[cfg(any(target_os = "linux", target_os = "macos"))]
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
                max_shell_timeout_secs: checked_limit(
                    "max_shell_timeout_secs",
                    args.max_shell_timeout_secs,
                    MIN_SHELL_TIMEOUT_SECS,
                )?,
                max_output_bytes: checked_limit(
                    "max_output_bytes",
                    args.max_output_bytes,
                    MIN_BYTE_LIMIT,
                )?,
                max_read_bytes: checked_limit(
                    "max_read_bytes",
                    args.max_read_bytes,
                    MIN_BYTE_LIMIT,
                )?,
                max_write_bytes: checked_limit(
                    "max_write_bytes",
                    args.max_write_bytes,
                    MIN_BYTE_LIMIT,
                )?,
                protected_paths,
            },
            operation_gate: Arc::new(RwLock::new(())),
            serialize: args.serialize,
            router: OnceLock::new(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
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

    /// The largest request body a tool call can need, for transports that cap
    /// bodies independently of the tools (the streamable-HTTP server and the
    /// tunnel). It follows the write ceiling, so raising `max_write_bytes`
    /// raises the transport ceiling with it instead of leaving a hidden 4 MiB
    /// limit that refuses writes the tools accept.
    pub(crate) fn max_request_body_bytes(&self) -> usize {
        self.config
            .max_write_bytes
            .saturating_mul(MAX_JSON_ESCAPE)
            .saturating_add(MAX_REQUEST_FRAMING_BYTES)
    }

    /// Bound the embedded MCP response at the *same* configured read/output
    /// ceilings as stdio and HTTP. Serialized JSON can expand a source byte to
    /// six ASCII bytes (`\\u00XX`), so the ceiling must include that expansion
    /// as well as protocol framing. There is no hidden 16 MiB tunnel limit:
    /// users choosing larger read limits intentionally allow larger responses.
    pub(crate) fn max_response_body_bytes(&self) -> usize {
        self.config
            .max_read_bytes
            .max(self.config.max_output_bytes)
            .saturating_mul(MAX_JSON_ESCAPE)
            .saturating_add(MAX_REQUEST_FRAMING_BYTES)
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
        let path = self.config.access.resolve_existing(input, need).await?;
        self.ensure_not_protected(&path)?;
        Ok(path)
    }

    async fn resolve_for_create(&self, input: &str, need: AccessNeed) -> Result<PathBuf> {
        let path = self.config.access.resolve_for_create(input, need).await?;
        self.ensure_not_protected(&path)?;
        Ok(path)
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
        Capabilities {
            read: self.config.access.has_read_capability(),
            write: self.config.access.has_write_capability(),
            read_write: self.config.access.has_read_write_capability(),
            shell: self.config.allow_shell,
        }
    }

    fn policy_tool_router(&self) -> &ToolRouter<Self> {
        self.router
            .get_or_init(|| Arc::new(Self::tool_router_for_policy(self.capabilities())))
    }

    /// Pure reads may overlap each other; everything else takes the operation
    /// gate on platforms without descriptor-rooted IO.
    #[cfg(any(test, windows))]
    fn is_concurrent_read_tool(name: &str) -> bool {
        TOOL_SPECS
            .iter()
            .any(|spec| spec.name == name && spec.concurrent_read)
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
        crate::logging::summarize_arguments(arguments, names.iter().map(String::as_str))
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
        // Building the router to read the schema is not free, so only do it when
        // the detail line will actually be printed.
        let detail = match self.config.log.activity_enabled() {
            true => self.summarize_tool_call_arguments(&name, request.arguments.as_ref()),
            false => String::new(),
        };
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
    use tokio::io::AsyncSeekExt;

    use super::*;
    use crate::mcp::tools::test_support::access;

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
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();

        assert!(machine.require_regular_file(&regular).await.is_ok());
        assert!(machine.require_regular_file(&missing).await.is_err());
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
            assert_eq!(names, ["bread", "grep", "ls", "powershell", "read"]);
        } else {
            assert_eq!(names, ["bash", "bread", "grep", "ls", "read"]);
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
            ["ls", "grep", "read", "bread"]
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
            write_ceiling * 6 + 64 * 1024,
            "one encoded byte is at most six, plus framing"
        );
        assert!(
            config.max_request_body_bytes > 4 * 1024 * 1024,
            "the transport must not keep rmcp's smaller default"
        );
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
}
