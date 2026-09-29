use std::{
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_router,
};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

#[derive(Clone, Debug)]
pub struct MachineConfig {
    pub workspace_root: PathBuf,
    pub allow_shell: bool,
    pub shell: String,
    pub max_shell_timeout_secs: u64,
    pub max_output_bytes: usize,
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
    pub protected_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct LocalMachine {
    config: MachineConfig,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PathArgs {
    /// Path relative to the workspace root selected by --cwd. Use "." for the workspace itself.
    path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListArgs {
    /// Directory path relative to the workspace root selected by --cwd. Defaults to ".".
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
struct ReadTextArgs {
    /// UTF-8 text file path relative to the workspace root selected by --cwd.
    path: String,

    /// Optional per-call byte limit, capped by the server's configured maximum.
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WriteTextArgs {
    /// File path relative to the workspace root selected by --cwd.
    path: String,

    /// UTF-8 content to write.
    content: String,

    /// Create missing parent directories before writing.
    #[serde(default)]
    create_parents: bool,

    /// Write mode. "overwrite" replaces/truncates; "append" appends or creates.
    #[serde(default)]
    mode: WriteMode,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WriteMode {
    #[default]
    Overwrite,
    Append,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MkdirArgs {
    /// Directory path relative to the workspace root selected by --cwd.
    path: String,

    /// Create missing parent directories as needed.
    #[serde(default = "yes")]
    parents: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RemoveArgs {
    /// File or directory path relative to the workspace root selected by --cwd.
    path: String,

    /// Required for removing a non-empty directory tree.
    #[serde(default)]
    recursive: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ShellArgs {
    /// Bash command string executed as: bash --noprofile --norc -lc <command>.
    command: String,

    /// Working directory relative to the workspace root selected by --cwd. Defaults to ".".
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

fn yes() -> bool {
    true
}

fn ok_text(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.into())])
}

fn tool_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
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

impl LocalMachine {
    pub fn workspace_root(&self) -> &Path {
        self.config.workspace_root.as_path()
    }
    pub async fn new(args: MachineConfig) -> Result<Self> {
        let root = fs::canonicalize(&args.workspace_root)
            .await
            .with_context(|| {
                format!(
                    "failed to canonicalize workspace {}",
                    args.workspace_root.display()
                )
            })?;
        let metadata = fs::metadata(&root).await?;
        if !metadata.is_dir() {
            bail!("workspace is not a directory: {}", root.display());
        }

        let mut protected_paths = Vec::with_capacity(args.protected_paths.len());
        for path in args.protected_paths {
            let canonical = fs::canonicalize(&path).await.unwrap_or(path);
            protected_paths.push(canonical);
        }

        Ok(Self {
            config: MachineConfig {
                workspace_root: root,
                allow_shell: args.allow_shell,
                shell: args.shell,
                max_shell_timeout_secs: args.max_shell_timeout_secs.max(1),
                max_output_bytes: args.max_output_bytes.max(1024),
                max_read_bytes: args.max_read_bytes.max(1024),
                max_write_bytes: args.max_write_bytes.max(1024),
                protected_paths,
            },
        })
    }

    fn ensure_not_protected(&self, path: &Path) -> Result<()> {
        for protected in &self.config.protected_paths {
            if path == protected || path.starts_with(protected) {
                bail!(
                    "path is protected by abird-tunnel and cannot be accessed through filesystem tools"
                );
            }
        }
        Ok(())
    }

    fn lexical_target(&self, input: &str) -> Result<PathBuf> {
        let rel = Path::new(input);
        if rel.is_absolute() {
            bail!(
                "absolute paths are not accepted; paths must be relative to the selected workspace"
            );
        }

        let mut clean = PathBuf::new();
        for component in rel.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => clean.push(part),
                Component::ParentDir => bail!("parent traversal ('..') is not allowed"),
                Component::RootDir | Component::Prefix(_) => {
                    bail!("absolute paths are not allowed")
                }
            }
        }

        Ok(self.config.workspace_root.join(clean))
    }

    async fn resolve_existing(&self, input: &str) -> Result<PathBuf> {
        let target = self.lexical_target(input)?;
        let canonical = fs::canonicalize(&target)
            .await
            .with_context(|| format!("path does not exist or cannot be resolved: {input}"))?;
        self.ensure_in_root(&canonical)?;
        self.ensure_not_protected(&canonical)?;
        Ok(canonical)
    }

    /// Validate every parent component against the selected workspace while preserving the
    /// final path component itself. This is useful for operations such as stat/remove,
    /// where following a final symlink would change which object is inspected or deleted.
    async fn resolve_existing_no_follow_final(&self, input: &str) -> Result<PathBuf> {
        let target = self.lexical_target(input)?;
        if target.as_path() == self.config.workspace_root.as_path() {
            return Ok(target);
        }

        fs::symlink_metadata(&target)
            .await
            .with_context(|| format!("path does not exist or cannot be resolved: {input}"))?;

        let parent = target
            .parent()
            .ok_or_else(|| anyhow!("path has no parent: {input}"))?;
        let final_name = target
            .file_name()
            .ok_or_else(|| anyhow!("path has no final component: {input}"))?;
        let canonical_parent = fs::canonicalize(parent)
            .await
            .with_context(|| format!("parent cannot be resolved: {input}"))?;
        self.ensure_in_root(&canonical_parent)?;

        // Follow parent symlinks, but deliberately preserve the final component so
        // stat/remove act on a final symlink itself rather than its target.
        let resolved = canonical_parent.join(final_name);
        self.ensure_not_protected(&resolved)?;
        Ok(resolved)
    }

    async fn resolve_for_create(&self, input: &str) -> Result<PathBuf> {
        let target = self.lexical_target(input)?;

        // symlink_metadata observes a final symlink even when it is dangling. Using
        // try_exists here would treat a dangling symlink as absent and could let a
        // subsequent open/create follow it outside the selected workspace.
        match fs::symlink_metadata(&target).await {
            Ok(_) => {
                let canonical = fs::canonicalize(&target)
                    .await
                    .with_context(|| format!("existing path cannot be safely resolved: {input}"))?;
                self.ensure_in_root(&canonical)?;
                self.ensure_not_protected(&canonical)?;
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
        self.ensure_in_root(&canonical_ancestor)?;

        let remainder = target
            .strip_prefix(&ancestor)
            .map_err(|_| anyhow!("failed to resolve path below existing ancestor: {input}"))?;
        let resolved = canonical_ancestor.join(remainder);
        self.ensure_not_protected(&resolved)?;
        Ok(resolved)
    }

    fn ensure_in_root(&self, path: &Path) -> Result<()> {
        if !path.starts_with(self.config.workspace_root.as_path()) {
            bail!("resolved path escapes the selected workspace");
        }
        Ok(())
    }

    fn relative_display(&self, path: &Path) -> String {
        path.strip_prefix(self.config.workspace_root.as_path())
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    }

    async fn list_dir_impl(&self, args: ListArgs) -> Result<Vec<FsEntry>> {
        let root = self.resolve_existing(&args.path).await?;
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
                // Do not even reveal protected credential filenames through fs_list.
                if self.ensure_not_protected(&path).is_err() {
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
                    path: self.relative_display(&path),
                    kind,
                    size: metadata.len(),
                });

                if output.len() >= max_entries {
                    return Ok(output);
                }

                if args.recursive && file_type.is_dir() {
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
}

#[tool_router]
impl LocalMachine {
    #[tool(
        description = "Show this MCP server's workspace directory, filesystem boundary, and execution policy without modifying anything.",
        annotations(
            title = "Machine bridge info",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn machine_info(&self) -> CallToolResult {
        ok_text(
            serde_json::json!({
                "cwd": self.config.workspace_root.to_string_lossy(),
                "filesystem_boundary": self.config.workspace_root.to_string_lossy(),
                "shell_enabled": self.config.allow_shell,
                "shell": self.config.shell.as_str(),
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "max_shell_timeout_secs": self.config.max_shell_timeout_secs,
                "max_read_bytes": self.config.max_read_bytes,
                "max_write_bytes": self.config.max_write_bytes,
            })
            .to_string(),
        )
    }

    #[tool(
        description = "List files and directories inside the workspace selected by --cwd. Paths are relative to that workspace and cannot escape it. Does not follow symlinked directories.",
        annotations(
            title = "List local files",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn fs_list(&self, Parameters(args): Parameters<ListArgs>) -> CallToolResult {
        match self.list_dir_impl(args).await {
            Ok(entries) => match serde_json::to_string_pretty(&entries) {
                Ok(json) => ok_text(json),
                Err(error) => tool_error(error),
            },
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Return metadata for one local filesystem path inside the workspace selected by --cwd. Paths cannot escape the workspace.",
        annotations(
            title = "Stat local path",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn fs_stat(&self, Parameters(args): Parameters<PathArgs>) -> CallToolResult {
        let path = match self.resolve_existing_no_follow_final(&args.path).await {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        match fs::symlink_metadata(&path).await {
            Ok(metadata) => {
                let ft = metadata.file_type();
                ok_text(
                    serde_json::json!({
                        "path": self.relative_display(&path),
                        "kind": if ft.is_symlink() { "symlink" } else if ft.is_dir() { "dir" } else if ft.is_file() { "file" } else { "other" },
                        "size": metadata.len(),
                        "readonly": metadata.permissions().readonly(),
                    })
                    .to_string(),
                )
            }
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Read a UTF-8 text file inside the workspace selected by --cwd. Refuses paths outside the workspace and files above the configured byte limit.",
        annotations(
            title = "Read local text file",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn fs_read_text(&self, Parameters(args): Parameters<ReadTextArgs>) -> CallToolResult {
        let path = match self.resolve_existing(&args.path).await {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        let limit = args
            .max_bytes
            .unwrap_or(self.config.max_read_bytes)
            .clamp(1, self.config.max_read_bytes);

        let file = match fs::File::open(&path).await {
            Ok(file) => file,
            Err(error) => return tool_error(error),
        };
        match file.metadata().await {
            Ok(metadata) if !metadata.is_file() => return tool_error("path is not a regular file"),
            Ok(metadata) if metadata.len() > limit as u64 => {
                return tool_error(format!(
                    "file is {} bytes, above the {} byte limit",
                    metadata.len(),
                    limit
                ));
            }
            Err(error) => return tool_error(error),
            _ => {}
        }

        // Read through a bounded handle as well as checking metadata, so a file that grows
        // between metadata() and read cannot force an unbounded allocation.
        let mut bounded = file.take(limit as u64 + 1);
        let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
        match bounded.read_to_end(&mut bytes).await {
            Ok(_) if bytes.len() > limit => tool_error(format!(
                "file grew above the {} byte limit while it was being read",
                limit
            )),
            Ok(_) => match String::from_utf8(bytes) {
                Ok(text) => ok_text(text),
                Err(_) => tool_error(
                    "file is not valid UTF-8; binary reads are intentionally disabled in v0.1",
                ),
            },
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Write UTF-8 text to a file inside the workspace selected by --cwd. Refuses paths outside the workspace. Can overwrite existing data, so treat as a destructive write action.",
        annotations(
            title = "Write local text file",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn fs_write_text(&self, Parameters(args): Parameters<WriteTextArgs>) -> CallToolResult {
        if args.content.len() > self.config.max_write_bytes {
            return tool_error(format!(
                "content is {} bytes, above the {} byte write limit",
                args.content.len(),
                self.config.max_write_bytes
            ));
        }

        let path = match self.resolve_for_create(&args.path).await {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };

        if args.create_parents
            && let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent).await
        {
            return tool_error(error);
        }

        let mut options = fs::OpenOptions::new();
        options.create(true).write(true);
        match args.mode {
            WriteMode::Overwrite => {
                options.truncate(true);
            }
            WriteMode::Append => {
                options.append(true);
            }
        }

        match options.open(&path).await {
            Ok(mut file) => {
                use tokio::io::AsyncWriteExt;
                match file.write_all(args.content.as_bytes()).await {
                    Ok(()) => ok_text(format!(
                        "wrote {} bytes to {}",
                        args.content.len(),
                        args.path
                    )),
                    Err(error) => tool_error(error),
                }
            }
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Create a directory inside the workspace selected by --cwd. Refuses paths outside the workspace.",
        annotations(
            title = "Create local directory",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn fs_mkdir(&self, Parameters(args): Parameters<MkdirArgs>) -> CallToolResult {
        let path = match self.resolve_for_create(&args.path).await {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        let result = if args.parents {
            fs::create_dir_all(&path).await
        } else {
            fs::create_dir(&path).await
        };
        match result {
            Ok(()) => ok_text(format!("created directory {}", args.path)),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Remove a file or directory inside the workspace selected by --cwd. Refuses paths outside the workspace. recursive=true can delete entire directory trees.",
        annotations(
            title = "Remove local path",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn fs_remove(&self, Parameters(args): Parameters<RemoveArgs>) -> CallToolResult {
        if args.path == "." || args.path.trim().is_empty() {
            return tool_error("refusing to remove the workspace root");
        }
        let path = match self.resolve_existing_no_follow_final(&args.path).await {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        if path.as_path() == self.config.workspace_root.as_path() {
            return tool_error("refusing to remove the workspace root");
        }

        let metadata = match fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) => return tool_error(error),
        };

        let result = if metadata.file_type().is_dir() {
            if args.recursive {
                fs::remove_dir_all(&path).await
            } else {
                fs::remove_dir(&path).await
            }
        } else {
            fs::remove_file(&path).await
        };

        match result {
            Ok(()) => ok_text(format!("removed {}", args.path)),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Execute an arbitrary Bash command on the local machine when shell access is enabled. It starts in a directory inside the --cwd workspace, but Bash itself is not sandboxed and may read/write outside that workspace via absolute paths or parent traversal. Treat it as fully privileged within the OS user's account.",
        annotations(
            title = "Run local Bash command",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn shell_exec(
        &self,
        Parameters(args): Parameters<ShellArgs>,
    ) -> Result<CallToolResult, McpError> {
        if !self.config.allow_shell {
            return Ok(tool_error(
                "shell execution is disabled; restart abird-tunnel with shell access enabled to enable it",
            ));
        }
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

        let cwd = match self.resolve_existing(&args.cwd).await {
            Ok(path) => path,
            Err(error) => return Ok(tool_error(error)),
        };
        match fs::metadata(&cwd).await {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(tool_error("cwd is not a directory")),
            Err(error) => return Ok(tool_error(error)),
        }

        let timeout_secs = args
            .timeout_secs
            .unwrap_or(30)
            .clamp(1, self.config.max_shell_timeout_secs);

        let mut command = Command::new(self.config.shell.as_str());
        command
            .arg("--noprofile")
            .arg("--norc")
            .arg("-lc")
            .arg(&args.command)
            .current_dir(&cwd)
            .stdin(if args.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("ABIRD_TUNNEL_API_KEY")
            .env_remove("CONTROL_PLANE_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_ADMIN_KEY")
            .kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return Ok(tool_error(error)),
        };

        if let Some(stdin_text) = args.stdin
            && let Some(mut stdin) = child.stdin.take()
        {
            use tokio::io::AsyncWriteExt;
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

        // Drain both pipes for the entire process lifetime while retaining only the bounded prefix.
        // This prevents an untrusted command from growing this server's memory without bound.
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
                "cwd": self.relative_display(&cwd),
                "stdout": String::from_utf8_lossy(&stdout_bytes),
                "stderr": String::from_utf8_lossy(&stderr_bytes),
                "stdout_truncated": stdout_truncated,
                "stderr_truncated": stderr_truncated,
            })
            .to_string(),
        ))
    }
}

#[rmcp::tool_handler(
    name = "abird-tunnel",
    instructions = "Private local-machine bridge for the workspace selected when abird-tunnel started. Prefer fs_* tools: they are hard-bounded to that workspace and reject escape attempts. Use shell_exec only when shell behavior is actually required; Bash starts in the workspace but is not sandboxed, can modify data outside it, and can access the network. Never request secrets unless the user explicitly asks."
)]
impl rmcp::ServerHandler for LocalMachine {}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_machine(root: PathBuf, protected_paths: Vec<PathBuf>) -> LocalMachine {
        LocalMachine::new(MachineConfig {
            workspace_root: root,
            allow_shell: true,
            shell: "bash".to_owned(),
            max_shell_timeout_secs: 5,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,
            protected_paths,
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn rejects_parent_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let machine = test_machine(temp.path().to_path_buf(), Vec::new()).await;
        assert!(machine.resolve_existing("../outside").await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn protected_path_stays_protected_through_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let secret_dir = root.join(".config/abird-tunnel");
        let secret = secret_dir.join("runtime.key");
        std::fs::create_dir_all(&secret_dir).unwrap();
        std::fs::write(&secret, b"secret").unwrap();
        symlink(&secret_dir, root.join("alias")).unwrap();

        let machine = test_machine(root, vec![secret]).await;
        assert!(
            machine
                .resolve_existing_no_follow_final("alias/runtime.key")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn protected_files_are_hidden_from_listings() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let secret_dir = root.join(".config/abird-tunnel");
        let secret = secret_dir.join("runtime.key");
        std::fs::create_dir_all(&secret_dir).unwrap();
        std::fs::write(&secret, b"secret").unwrap();
        std::fs::write(secret_dir.join("config.toml"), b"version = 1").unwrap();

        let machine = test_machine(root, vec![secret]).await;
        let entries = machine
            .list_dir_impl(ListArgs {
                path: ".config/abird-tunnel".to_owned(),
                recursive: false,
                max_entries: None,
            })
            .await
            .unwrap();

        assert!(
            entries
                .iter()
                .all(|entry| !entry.path.ends_with("runtime.key"))
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.path.ends_with("config.toml"))
        );
    }
}
