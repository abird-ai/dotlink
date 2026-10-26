//! `bash` / `powershell`: the sandboxed shell, its mounts and its heartbeat.

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

use anyhow::Result;
use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, ProgressNotificationParam, ProgressToken, Tool},
    schemars,
    service::{Peer, RequestContext, RoleServer},
    tool, tool_router,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::{Instant, timeout_at},
};
use tokio_util::task::AbortOnDropHandle;

use super::{LocalMachine, dot, ok_text, resolve_capped, tool_error};
use crate::mcp::{AccessNeed, AccessPolicy};

/// The client picks `timeout_secs`; the profile ceiling only bounds it and is
/// the fallback when the caller omits it.
fn resolve_shell_timeout(requested: Option<u64>, ceiling: u64) -> u64 {
    resolve_capped(requested, ceiling)
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

impl LocalMachine {
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

        crate::shell_process::configure(&mut command);
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
        let mut process_tree = match crate::shell_process::Guard::attach(&child) {
            Ok(group) => group,
            Err(error) => {
                Self::terminate_child(&mut child).await;
                return Ok(tool_error(format!(
                    "could not contain command tree: {error}"
                )));
            }
        };

        // A configured ceiling can exceed what the clock can represent; fail
        // the call instead of panicking on the addition.
        let Some(deadline) = Instant::now().checked_add(Duration::from_secs(timeout_secs)) else {
            return Ok(tool_error(format!(
                "timeout_secs {timeout_secs} is too large to schedule"
            )));
        };
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
        // Start draining before stdin: a child that writes before reading its
        // stdin must never be able to fill the stdout pipe and deadlock the
        // writer.
        // Spawning pipe workers is safe only when their handles are owned by
        // the request: dropping a cancelled tool call must abort all readers.
        let stdout_task = AbortOnDropHandle::new(tokio::spawn(drain_capped(stdout, output_limit)));
        let stderr_task = AbortOnDropHandle::new(tokio::spawn(drain_capped(stderr, output_limit)));

        let (mut stdin_task, mut stdin_rx) = match args.stdin {
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
                    Self::terminate_child(&mut child).await;
                    stdout_task.abort();
                    stderr_task.abort();
                    return Ok(tool_error("failed to capture child stdin"));
                }
            },
            None => (None, None),
        };

        enum WaitOutcome {
            Child(std::result::Result<std::process::ExitStatus, std::io::Error>),
            Timeout,
            StdinFailure(String),
        }
        let outcome = {
            let wait = timeout_at(deadline, child.wait());
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    result = &mut wait => break match result {
                        Ok(result) => WaitOutcome::Child(result),
                        Err(_) => WaitOutcome::Timeout,
                    },
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
                            Ok(Err(error)) => {
                                break WaitOutcome::StdinFailure(format!(
                                    "failed writing stdin: {error}"
                                ));
                            }
                            Err(_) => {
                                break WaitOutcome::StdinFailure(
                                    "stdin writer task failed".to_owned(),
                                );
                            }
                        }
                    }
                    () = heartbeat_tick(heartbeat.as_ref()), if heartbeat.is_some() => {
                        if let Some(heartbeat) = heartbeat.as_ref() {
                            heartbeat.report().await;
                        }
                    }
                }
            }
        };

        let status = match outcome {
            WaitOutcome::StdinFailure(message) => {
                Self::terminate_child(&mut child).await;
                if let Some(task) = stdin_task.take() {
                    task.abort();
                }
                stdout_task.abort();
                stderr_task.abort();
                return Ok(tool_error(message));
            }
            WaitOutcome::Timeout => {
                process_tree.terminate();
                Self::terminate_child(&mut child).await;
                if let Some(task) = stdin_task.take() {
                    task.abort();
                }
                None
            }
            WaitOutcome::Child(result) => match result {
                Ok(status) => {
                    // The child is done. A still-running stdin task has no
                    // reader; abort it rather than letting a descendant-held
                    // pipe delay the result.
                    if let Some(task) = stdin_task.take() {
                        task.abort();
                    }
                    Some(status)
                }
                Err(error) => {
                    Self::terminate_child(&mut child).await;
                    if let Some(task) = stdin_task.take() {
                        task.abort();
                    }
                    stdout_task.abort();
                    stderr_task.abort();
                    return Ok(tool_error(error));
                }
            },
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
        // A normal exit is allowed to start independent background work.
        // Error/cancellation paths retain the guard and kill the tree.
        process_tree.disarm();
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
            if self.config.access.grant_read(protected) || self.config.access.grant_write(protected)
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

    /// Output ceiling for one stream: the caller picks, the profile bounds.
    fn output_limit(&self, requested: Option<usize>) -> usize {
        resolve_capped(requested, self.config.max_output_bytes)
    }

    async fn terminate_child(child: &mut tokio::process::Child) {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

#[cfg(target_os = "linux")]
fn mask_path(command: &mut Command, path: &Path) {
    let metadata = std::fs::symlink_metadata(path);
    prepare_mount_target_dirs(command, path);

    // A protected path need not exist on the host yet. Reserve an inaccessible
    // directory in the sandbox so even a writable parent cannot create it.
    // Existing non-directories retain the /dev/null file mask.
    if metadata.as_ref().is_ok_and(|metadata| !metadata.is_dir()) {
        command.arg("--ro-bind").arg("/dev/null").arg(path);
    } else {
        if metadata.is_err() {
            command.arg("--dir").arg(path);
        }
        command
            .arg("--tmpfs")
            .arg(path)
            .arg("--chmod")
            .arg("000")
            .arg(path);
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
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.execute_shell(args, true, ProgressHeartbeat::for_request(&context))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::RwLock;

    use crate::logging::LogConfig;
    use crate::mcp::{
        AccessPolicy, AccessSpec, LocalMachine, MachineConfig, RuntimeConfig, SandboxCacheMount,
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
                shell_program: Some(bash),
                bwrap_program: None,
                max_shell_timeout_secs: 5,
                max_output_bytes: max_bytes,
                max_read_bytes: max_bytes,
                max_write_bytes: max_bytes,
                protected_paths: Vec::new(),
            },
            operation_gate: Arc::new(RwLock::new(())),
            router: std::sync::OnceLock::new(),
            serialize: false,
            #[cfg(target_os = "linux")]
            secure_fs,
        }
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

    #[test]
    fn shell_tool_schema_reports_configured_ceilings() {
        let bash = LocalMachine::all_tools().get("bash").cloned().unwrap();
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
        let read = LocalMachine::all_tools().get("read").cloned().unwrap();
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
    fn missing_protected_path_is_masked_without_creating_it_on_host() {
        use std::ffi::OsStr;
        let temp = tempfile::tempdir().unwrap();
        let absent = temp.path().join("private-state");
        let mut command = Command::new("true");
        mask_path(&mut command, &absent);
        assert!(!absent.exists());
        let args = command.as_std().get_args().collect::<Vec<_>>();
        for op in ["--dir", "--tmpfs", "--chmod"] {
            assert!(args.windows(2).any(|pair| pair[0] == OsStr::new(op)
                && pair[1]
                    == (if op == "--chmod" {
                        OsStr::new("000")
                    } else {
                        absent.as_os_str()
                    })));
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
                shell_program: Some(PathBuf::from("/bin/bash")),
                bwrap_program: Some(PathBuf::from("/bin/bwrap")),
                max_shell_timeout_secs: 5,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,
                protected_paths: Vec::new(),
            },
            operation_gate: Arc::new(RwLock::new(())),
            router: std::sync::OnceLock::new(),
            serialize: false,
            secure_fs: Arc::new(
                crate::secure_fs::SecureFs::new(&[root.to_path_buf()], &[root.to_path_buf()])
                    .unwrap(),
            ),
        }
    }

    /// A deny-write root may name one file. It is enforced by the read-only
    /// rebind pass, never by the directory-mount list: opening a file as a
    /// directory used to make every sandboxed shell call fail.
    #[cfg(target_os = "linux")]
    #[test]
    fn deny_write_roots_are_rebound_read_only_whether_file_or_directory() {
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
            .expect("a file deny-write must not break the sandbox command");
        let args: Vec<_> = command.as_std().get_args().collect();

        for path in [&locked_dir, &locked_file] {
            assert!(
                args.windows(3).any(|window| {
                    window[0] == OsStr::new("--ro-bind")
                        && window[1] == path.as_os_str()
                        && window[2] == path.as_os_str()
                }),
                "{} must be rebound read-only: {args:?}",
                path.display()
            );
            assert!(
                !machine
                    .config
                    .access
                    .sandbox_mounts()
                    .iter()
                    .any(|(mount, _)| mount == path),
                "a deny-write root must not reach the directory-mount list"
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
            router: std::sync::OnceLock::new(),
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
            router: std::sync::OnceLock::new(),
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
            router: std::sync::OnceLock::new(),
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
