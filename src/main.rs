mod controls;
mod logging;
mod mcp;
mod setup;
mod transports;

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow, bail};
use clap::{ArgAction, Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::{
    controls::{RuntimeControl, RuntimeControls},
    logging::{ColorMode, LogConfig},
    mcp::{AccessSpec, LocalMachine, MachineConfig, SandboxCacheMount},
    setup::{
        PermissionConfig, ProfileBool, ProfileRuleKind, TransportConfig, create_profile,
        delete_profile, edit_profile, list_profiles, load_or_setup, mutate_profile_rule,
        set_profile_bool, show_profile,
    },
    transports::ActiveTransports,
};

#[derive(Debug, Parser)]
#[command(
    name = "dotlink",
    version,
    about = "Permission-scoped local MCP bridge over OpenAI Tunnel, stdio, or HTTP"
)]
struct Args {
    /// Manage persisted profiles.
    #[command(subcommand)]
    command: Option<Command>,

    /// Run interactive setup for the selected profile.
    #[arg(
        short = 'S',
        long,
        conflicts_with_all = [
            "stdio",
            "no_stdio",
            "http",
            "no_http",
            "ngrok",
            "no_ngrok",
            "ephemeral_url",
            "http_ephemeral_url",
            "ngrok_ephemeral_url"
        ]
    )]
    setup: bool,

    /// Select a named config profile (config.<profile>.jsonc).
    #[arg(short = 'p', long, value_name = "NAME")]
    profile: Option<String>,

    /// Add the stdio MCP server for this run.
    #[arg(long, conflicts_with = "no_stdio")]
    stdio: bool,

    /// Disable stdio even if enabled in the selected profile.
    #[arg(long, conflicts_with = "stdio")]
    no_stdio: bool,

    /// Add the HTTP MCP server for this run.
    #[arg(long, conflicts_with = "no_http")]
    http: bool,

    /// Disable HTTP (and ngrok) even if enabled in the selected profile.
    #[arg(long, conflicts_with = "http")]
    no_http: bool,

    /// Override the configured HTTP listen address for this run.
    #[arg(long, value_name = "ADDR")]
    http_bind: Option<SocketAddr>,

    /// Publish the effective HTTP MCP server through ngrok for this run.
    #[arg(long, conflicts_with = "no_ngrok")]
    ngrok: bool,

    /// Disable ngrok even if enabled in the selected profile.
    #[arg(long, conflicts_with = "ngrok")]
    no_ngrok: bool,

    /// Use fresh hard-to-guess URL paths for both local HTTP and ngrok.
    #[arg(long)]
    ephemeral_url: bool,

    /// Override local HTTP ephemeral-path behavior. Bare flag means true.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true,
        alias = "http-emphemeral-url"
    )]
    http_ephemeral_url: Option<bool>,

    /// Override ngrok ephemeral-path behavior. Bare flag means true.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true,
        alias = "ngrok-emphemeral-url"
    )]
    ngrok_ephemeral_url: Option<bool>,

    /// Add readable access. Bare --allow-read means the launch directory.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    allow_read: Vec<PathBuf>,

    /// Add writable access. Bare --allow-write means read+write on the launch directory.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    allow_write: Vec<PathBuf>,

    /// Add read+write access. Bare --allow-rw means the launch directory.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    allow_rw: Vec<PathBuf>,

    /// Do not implicitly grant read access to the launch directory for this run.
    #[arg(long)]
    no_default_allow: bool,

    /// Deny reads. Bare --deny-read means the launch directory. Denies override allows.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    deny_read: Vec<PathBuf>,

    /// Deny writes. Bare --deny-write means the launch directory. Denies override allows.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    deny_write: Vec<PathBuf>,

    /// Deny both reads and writes. Bare --deny-rw means the launch directory.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    deny_rw: Vec<PathBuf>,

    /// Legacy synonym for --deny-rw=<PATH>.
    #[arg(long, value_name = "PATH")]
    deny: Vec<PathBuf>,

    /// Enable the platform shell (Bash on Unix, PowerShell on Windows).
    #[arg(long)]
    allow_shell: bool,

    /// Disable shell even if config or another flag enables it.
    #[arg(long)]
    deny_shell: bool,

    /// Allow network access from a Bubblewrap-sandboxed shell.
    #[arg(long)]
    allow_network: bool,

    /// Deny shell network access. On Linux this forces sandboxing when needed.
    #[arg(long)]
    deny_network: bool,

    /// Disable the shell sandbox. Requires --allow-all.
    #[arg(long, requires = "allow_all")]
    no_sandbox: bool,

    /// Grant unrestricted filesystem, shell, and network access. Requires --no-sandbox.
    #[arg(long, requires = "no_sandbox")]
    allow_all: bool,

    /// Print the configured OpenAI Tunnel ID and exit.
    #[arg(long)]
    print_id: bool,

    /// Suppress TOOL activity even when verbosity enables it.
    #[arg(short = 's', long)]
    silent: bool,

    /// Increase logging verbosity: -v shows TOOL activity; -vv also shows REQ diagnostics.
    #[arg(short = 'v', long, action = ArgAction::Count)]
    verbose: u8,

    /// Control ANSI colors in human-facing stderr output.
    #[arg(long, value_enum, default_value_t = ColorMode::Auto, global = true)]
    color: ColorMode,

    /// List the MCP tools exposed under the effective policy and exit.
    #[arg(long)]
    list_tools: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List, create, edit, delete, or mutate profiles.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ProfileCommand {
    /// List available profiles.
    List,
    /// Show a profile without exposing secret key material.
    Show { name: String },
    /// Create a new profile with the interactive setup editor.
    Create { name: String },
    /// Edit an existing profile using its current values as defaults.
    Edit { name: String },
    /// Delete a profile and its saved runtime key.
    Delete { name: String },
    /// Add an allow rule.
    Allow {
        name: String,
        #[arg(value_enum)]
        kind: ProfileRuleKind,
        path: PathBuf,
    },
    /// Remove an allow rule.
    RemoveAllow {
        name: String,
        #[arg(value_enum)]
        kind: ProfileRuleKind,
        path: PathBuf,
    },
    /// Add a deny rule.
    Deny {
        name: String,
        #[arg(value_enum)]
        kind: ProfileRuleKind,
        path: PathBuf,
    },
    /// Remove a deny rule.
    RemoveDeny {
        name: String,
        #[arg(value_enum)]
        kind: ProfileRuleKind,
        path: PathBuf,
    },
    /// Enable a persisted boolean setting.
    Enable {
        name: String,
        #[arg(value_enum)]
        setting: ProfileBool,
    },
    /// Disable a persisted boolean setting.
    Disable {
        name: String,
        #[arg(value_enum)]
        setting: ProfileBool,
    },
}

#[derive(Debug, Clone)]
struct Policy {
    base_dir: PathBuf,
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
    deny_read_roots: Vec<PathBuf>,
    deny_write_roots: Vec<PathBuf>,
    unrestricted_fs: bool,
    allow_shell: bool,
    sandbox_shell: bool,
    allow_network: bool,
}

impl Policy {
    fn from_args(args: &Args, launch_dir: PathBuf, defaults: &PermissionConfig) -> Result<Self> {
        let base_dir = launch_dir;

        let has_filesystem_denies = !defaults.deny_read.is_empty()
            || !defaults.deny_write.is_empty()
            || !defaults.deny_rw.is_empty()
            || !args.deny_read.is_empty()
            || !args.deny_write.is_empty()
            || !args.deny_rw.is_empty()
            || !args.deny.is_empty();

        if args.allow_all && has_filesystem_denies {
            bail!("--allow-all cannot be combined with filesystem deny rules");
        }
        if args.allow_all && (args.deny_shell || args.deny_network) {
            bail!("--allow-all cannot be combined with --deny-shell or --deny-network");
        }

        let mut read_roots = Vec::new();
        let mut write_roots = Vec::new();

        if defaults.default_allow && !args.no_default_allow {
            read_roots.push(base_dir.clone());
        }

        for path in defaults.allow_read.iter().chain(defaults.allow_rw.iter()) {
            read_roots.push(base_or_path(path, &base_dir));
        }
        for path in defaults.allow_write.iter().chain(defaults.allow_rw.iter()) {
            write_roots.push(base_or_path(path, &base_dir));
        }

        for path in &args.allow_read {
            read_roots.push(base_or_path(path, &base_dir));
        }
        for path in &args.allow_write {
            if path == &PathBuf::from(".") {
                // Bare --allow-write remains ergonomic shorthand for rw on the launch/base directory.
                read_roots.push(base_dir.clone());
                write_roots.push(base_dir.clone());
            } else {
                write_roots.push(base_or_path(path, &base_dir));
            }
        }
        for path in &args.allow_rw {
            let path = base_or_path(path, &base_dir);
            read_roots.push(path.clone());
            write_roots.push(path);
        }

        if args.allow_all {
            read_roots.push(PathBuf::from("/"));
            write_roots.push(PathBuf::from("/"));
        }

        let mut deny_read_roots = Vec::new();
        let mut deny_write_roots = Vec::new();

        for path in defaults.deny_read.iter().chain(defaults.deny_rw.iter()) {
            deny_read_roots.push(base_or_path(path, &base_dir));
        }
        for path in defaults.deny_write.iter().chain(defaults.deny_rw.iter()) {
            deny_write_roots.push(base_or_path(path, &base_dir));
        }

        for path in &args.deny_read {
            deny_read_roots.push(base_or_path(path, &base_dir));
        }
        for path in &args.deny_write {
            deny_write_roots.push(base_or_path(path, &base_dir));
        }
        for path in args.deny_rw.iter().chain(args.deny.iter()) {
            let path = base_or_path(path, &base_dir);
            deny_read_roots.push(path.clone());
            deny_write_roots.push(path);
        }

        let unrestricted_fs = read_roots.iter().any(|path| path == Path::new("/"))
            && write_roots.iter().any(|path| path == Path::new("/"));

        let requested_shell = defaults.allow_shell || args.allow_shell || args.allow_all;
        if args.allow_network && !requested_shell && !args.deny_shell {
            bail!("--allow-network requires shell permission");
        }
        let allow_shell = requested_shell && !args.deny_shell;

        #[cfg(not(target_os = "linux"))]
        if allow_shell && !args.no_sandbox {
            bail!(
                "sandboxed shell execution is unavailable on this OS; use --no-sandbox --allow-all, or use Linux/WSL2 with Bubblewrap"
            );
        }

        let sandbox_shell = cfg!(target_os = "linux") && allow_shell && !args.no_sandbox;
        let requested_sandbox_network = defaults.allow_network || args.allow_network;

        let allow_network = if !allow_shell || args.deny_network {
            false
        } else if sandbox_shell {
            requested_sandbox_network
        } else {
            // --no-sandbox requires --allow-all at the CLI layer.
            true
        };

        if allow_shell && read_roots.is_empty() {
            bail!(
                "shell access requires at least one readable directory; omit --no-default-allow or add --allow-read/--allow-rw"
            );
        }
        if read_roots.is_empty() && write_roots.is_empty() && !allow_shell {
            bail!(
                "no local capability is enabled; allow a path (or omit --no-default-allow) or enable shell access"
            );
        }

        Ok(Self {
            base_dir,
            read_roots,
            write_roots,
            deny_read_roots,
            deny_write_roots,
            unrestricted_fs,
            allow_shell,
            sandbox_shell,
            allow_network,
        })
    }

    fn has_write(&self) -> bool {
        self.unrestricted_fs || !self.write_roots.is_empty()
    }
}

fn base_or_path(path: &PathBuf, base_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path.clone()
    } else {
        base_dir.join(path)
    }
}

fn next_runtime_restart_attempt(current: u32, reset_backoff: bool) -> u32 {
    if reset_backoff {
        1
    } else {
        current.saturating_add(1)
    }
}

fn runtime_restart_delay(attempt: u32) -> std::time::Duration {
    let exponent = attempt.saturating_sub(1).min(5);
    let seconds = (1_u64 << exponent).min(30);
    std::time::Duration::from_secs(seconds)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeOutcome {
    Exit,
    Restart,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let launch_dir = std::env::current_dir()?;

    let log = LogConfig::new(args.verbose, args.silent, args.color);
    let default_filter = "dotlink=warn";
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_writer(std::io::stderr)
        .with_ansi(log.color_enabled())
        .init();

    if let Some(command) = &args.command {
        run_command(command, log.color_enabled()).await?;
        return Ok(());
    }

    let mut runtime_restarts = 0_u32;
    loop {
        match run_runtime(&args, &launch_dir, &log).await {
            Ok(RuntimeOutcome::Exit) => return Ok(()),
            Ok(RuntimeOutcome::Restart) => {
                runtime_restarts = 0;
                log.notice("runtime", "restarting");
            }
            Err(error) => {
                let Some(restart) = transports::runtime_restart_request(&error) else {
                    return Err(error);
                };

                runtime_restarts =
                    next_runtime_restart_attempt(runtime_restarts, restart.reset_backoff());
                let delay = runtime_restart_delay(runtime_restarts);

                tracing::warn!(
                    restart = runtime_restarts,
                    delay_secs = delay.as_secs(),
                    "OpenAI transport remained unhealthy; restarting dotlink runtime"
                );
                tracing::debug!(%error, "runtime restart reason");
                eprintln!(
                    "\nOpenAI tunnel unhealthy — restarting dotlink runtime in {}s…",
                    delay.as_secs()
                );

                tokio::select! {
                    signal = tokio::signal::ctrl_c() => {
                        signal?;
                        eprintln!("\ndotlink stopped.");
                        return Ok(());
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

async fn run_command(command: &Command, color: bool) -> Result<()> {
    match command {
        Command::Profile { command } => match command {
            ProfileCommand::List => {
                let profiles = list_profiles()?;
                if profiles.is_empty() {
                    println!("No profiles configured.");
                } else {
                    for profile in profiles {
                        println!("{profile}");
                    }
                }
            }
            ProfileCommand::Show { name } => print!("{}", show_profile(name)?),
            ProfileCommand::Create { name } => {
                if create_profile(name, color).await? {
                    println!("Profile {name:?} created.");
                }
            }
            ProfileCommand::Edit { name } => {
                if edit_profile(name, color).await? {
                    println!("Profile {name:?} updated.");
                }
            }
            ProfileCommand::Delete { name } => {
                delete_profile(name)?;
                println!("Profile {name:?} deleted.");
            }
            ProfileCommand::Allow { name, kind, path } => {
                mutate_profile_rule(name, true, *kind, path.clone(), false)?;
                println!("Profile {name:?} updated.");
            }
            ProfileCommand::RemoveAllow { name, kind, path } => {
                mutate_profile_rule(name, true, *kind, path.clone(), true)?;
                println!("Profile {name:?} updated.");
            }
            ProfileCommand::Deny { name, kind, path } => {
                mutate_profile_rule(name, false, *kind, path.clone(), false)?;
                println!("Profile {name:?} updated.");
            }
            ProfileCommand::RemoveDeny { name, kind, path } => {
                mutate_profile_rule(name, false, *kind, path.clone(), true)?;
                println!("Profile {name:?} updated.");
            }
            ProfileCommand::Enable { name, setting } => {
                set_profile_bool(name, *setting, true)?;
                println!("Profile {name:?} updated.");
            }
            ProfileCommand::Disable { name, setting } => {
                set_profile_bool(name, *setting, false)?;
                println!("Profile {name:?} updated.");
            }
        },
    }
    Ok(())
}

async fn run_runtime(args: &Args, launch_dir: &Path, log: &LogConfig) -> Result<RuntimeOutcome> {
    let Some(setup) =
        load_or_setup(args.setup, args.profile.as_deref(), log.color_enabled()).await?
    else {
        return Ok(RuntimeOutcome::Exit);
    };

    if args.setup {
        return Ok(RuntimeOutcome::Exit);
    }

    let policy = Policy::from_args(args, launch_dir.to_path_buf(), &setup.config.permissions)?;

    if args.list_tools {
        print_tools(&policy);
        return Ok(RuntimeOutcome::Exit);
    }

    if args.print_id {
        let tunnel_id = setup
            .config
            .tunnel_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("OpenAI transport is not configured"))?;
        println!("{tunnel_id}");
        return Ok(RuntimeOutcome::Exit);
    }

    let local_transports = resolve_local_transports(args, &setup.config.transports)?;

    let http_bind = if local_transports.http {
        Some(match args.http_bind {
            Some(bind) => bind,
            None => setup
                .config
                .transports
                .http_bind
                .parse::<SocketAddr>()
                .map_err(|error| {
                    anyhow::anyhow!("invalid configured HTTP bind address: {error}")
                })?,
        })
    } else {
        None
    };

    let openai = if setup.config.transports.openai {
        let tunnel_id = setup.config.tunnel_id.clone().ok_or_else(|| {
            anyhow::anyhow!("OpenAI transport is enabled but tunnel_id is missing")
        })?;
        Some(transports::openai::Config {
            base_url: setup.config.base_url.clone(),
            tunnel_id,
            runtime_api_key: setup.config.runtime_api_key.clone(),
            organization_id: setup.config.organization_id.clone(),
            new_tunnel: setup.new_tunnel,
            log: log.clone(),
        })
    } else {
        None
    };

    let active_transports = ActiveTransports {
        openai,
        stdio: local_transports.stdio,
        http: http_bind.map(|bind| transports::http::Config {
            bind,
            ngrok: local_transports.ngrok,
            http_ephemeral_url: local_transports.http_ephemeral_url,
            ngrok_ephemeral_url: local_transports.ngrok_ephemeral_url,
            log: log.clone(),
        }),
    };

    if active_transports.is_empty() {
        bail!("no MCP transport is active; enable one in setup or add --stdio/--http for this run");
    }

    let mut cache_mounts = Vec::new();
    let mut shell_env = BTreeMap::new();
    if policy.allow_shell && policy.sandbox_shell {
        for cache in &setup.config.caches {
            if !cache.path.is_dir() {
                continue;
            }
            let target = cache.sandbox_path()?;
            if let Some((name, value)) = cache.shell_env(&target) {
                shell_env.insert(name, value);
            }
            cache_mounts.push(SandboxCacheMount {
                source: cache.path.clone(),
                target,
                writable: cache.mode.writable(),
            });
        }
    }

    let machine = LocalMachine::new(MachineConfig {
        access: AccessSpec {
            base_dir: policy.base_dir.clone(),
            read_roots: policy.read_roots.clone(),
            write_roots: policy.write_roots.clone(),
            deny_read_roots: policy.deny_read_roots.clone(),
            deny_write_roots: policy.deny_write_roots.clone(),
            unrestricted_fs: policy.unrestricted_fs,
        },
        cache_mounts,
        shell_env: shell_env.into_iter().collect(),
        log: log.clone(),
        allow_shell: policy.allow_shell,
        sandbox_shell: policy.sandbox_shell,
        allow_network: policy.allow_network,
        max_shell_timeout_secs: setup.config.max_shell_timeout_secs,
        max_output_bytes: setup.config.max_output_bytes,
        max_read_bytes: setup.config.max_read_bytes,
        max_write_bytes: setup.config.max_write_bytes,
        protected_paths: setup.protected_paths.clone(),
    })
    .await?;

    let cancellation = CancellationToken::new();
    let mut controls = RuntimeControls::start(log.clone(), active_transports.stdio);
    let controls_available = controls.is_some();

    print_banner(
        &active_transports,
        &machine,
        args.profile.as_deref(),
        log,
        args.allow_all,
        controls_available,
    );

    let mut transport_task =
        tokio::spawn(active_transports.run(machine, cancellation.child_token()));

    enum RuntimeEvent {
        Transport(Result<()>),
        Exit,
        Restart,
    }

    let event = tokio::select! {
        result = &mut transport_task => {
            RuntimeEvent::Transport(match result {
                Ok(result) => result,
                Err(error) => Err(anyhow!("transport supervisor task failed: {error}")),
            })
        }
        signal = tokio::signal::ctrl_c() => {
            signal?;
            RuntimeEvent::Exit
        }
        control = async {
            match controls.as_mut() {
                Some(controls) => controls.recv().await,
                None => std::future::pending::<Option<RuntimeControl>>().await,
            }
        } => {
            match control.unwrap_or(RuntimeControl::Exit) {
                RuntimeControl::Exit => RuntimeEvent::Exit,
                RuntimeControl::Restart => RuntimeEvent::Restart,
            }
        }
    };

    if let Some(controls) = controls.take() {
        controls.shutdown().await;
    }

    match event {
        RuntimeEvent::Transport(result) => {
            result?;
            Ok(RuntimeOutcome::Exit)
        }
        RuntimeEvent::Exit => {
            stop_runtime_transports(&mut transport_task, &cancellation).await;
            eprintln!("\ndotlink stopped.");
            Ok(RuntimeOutcome::Exit)
        }
        RuntimeEvent::Restart => {
            stop_runtime_transports(&mut transport_task, &cancellation).await;
            Ok(RuntimeOutcome::Restart)
        }
    }
}

async fn stop_runtime_transports(
    task: &mut tokio::task::JoinHandle<Result<()>>,
    cancellation: &CancellationToken,
) {
    cancellation.cancel();
    if tokio::time::timeout(std::time::Duration::from_secs(6), &mut *task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EffectiveLocalTransports {
    stdio: bool,
    http: bool,
    ngrok: bool,
    http_ephemeral_url: bool,
    ngrok_ephemeral_url: bool,
}

fn resolve_local_transports(
    args: &Args,
    defaults: &TransportConfig,
) -> Result<EffectiveLocalTransports> {
    let stdio = (defaults.stdio || args.stdio) && !args.no_stdio;
    let http = (defaults.http || args.http) && !args.no_http;

    let http_options_requested = args.http_bind.is_some()
        || args.ngrok
        || args.no_ngrok
        || args.ephemeral_url
        || args.http_ephemeral_url.is_some()
        || args.ngrok_ephemeral_url.is_some();
    if !http && http_options_requested {
        bail!(
            "HTTP options were requested, but HTTP is disabled; enable it in the profile or add --http"
        );
    }

    let ngrok = http && (defaults.ngrok || args.ngrok) && !args.no_ngrok;
    if args.ngrok_ephemeral_url.is_some() && !ngrok {
        bail!("--ngrok-ephemeral-url requires ngrok to be enabled in the profile or with --ngrok");
    }

    let (http_ephemeral_url, ngrok_ephemeral_url) = ephemeral_url_policy(args, defaults);

    Ok(EffectiveLocalTransports {
        stdio,
        http,
        ngrok,
        http_ephemeral_url,
        ngrok_ephemeral_url,
    })
}

fn ephemeral_url_policy(args: &Args, defaults: &TransportConfig) -> (bool, bool) {
    let http = args
        .http_ephemeral_url
        .unwrap_or(args.ephemeral_url || defaults.http_ephemeral_url);
    let ngrok = args
        .ngrok_ephemeral_url
        .unwrap_or(args.ephemeral_url || defaults.ngrok_ephemeral_url);
    (http, ngrok)
}

fn print_banner(
    transports: &ActiveTransports,
    machine: &LocalMachine,
    profile: Option<&str>,
    log: &LogConfig,
    allow_all: bool,
    controls_available: bool,
) {
    eprintln!();
    eprintln!(
        "{}",
        log.style(
            "1;36",
            format!("abird dotlink {}", env!("CARGO_PKG_VERSION"))
        )
    );
    eprintln!(
        "{}",
        log.style(
            "2",
            "────────────────────────────────────────────────────────"
        )
    );
    eprintln!("• Profile    {}", profile.unwrap_or("default"));
    eprintln!("• Transports {}", transports.names().join(", "));
    if let Some(openai) = &transports.openai {
        eprintln!("• Tunnel     {}", openai.tunnel_id);
    }
    if let Some(http) = &transports.http {
        if http.http_ephemeral_url {
            eprintln!("• HTTP       ephemeral URL; exact path will be printed after bind");
        } else {
            eprintln!("• HTTP       http://{}/mcp", http.bind);
        }
        if http.ngrok {
            if http.ngrok_ephemeral_url {
                eprintln!("• ngrok      enabled with independent ephemeral MCP path");
            } else {
                eprintln!("• ngrok      enabled; public URL will be printed after connection");
            }
        }
    }
    let access_summary = if allow_all {
        format!("{} (allow-all requested)", machine.access_summary())
    } else {
        machine.access_summary()
    };
    eprintln!("• Access     {access_summary}");
    if machine.shell_enabled() {
        eprintln!(
            "• Sandbox    {}",
            if machine.shell_sandboxed() {
                "Bubblewrap"
            } else {
                "disabled"
            }
        );
        eprintln!(
            "• Network    {}",
            if machine.network_enabled() {
                "enabled"
            } else {
                "disabled"
            }
        );
    }
    eprintln!("• Logging    {}", log.verbosity_label());
    eprintln!();
    if transports.openai.is_some() {
        eprintln!("OpenAI Secure MCP Tunnel connecting…");
    }
    if transports.stdio {
        eprintln!("stdio MCP server active; stdout is reserved for MCP.");
    }
    if controls_available {
        eprintln!(
            "{}",
            log.style(
                "2",
                "Keys        Ctrl-C: exit · Ctrl-R: restart · v: verbosity"
            )
        );
    } else {
        eprintln!("{}", log.style("2", "Keys        Ctrl-C: exit"));
    }
    eprintln!();
}

fn print_tools(policy: &Policy) {
    let tools =
        LocalMachine::tool_router_for_policy(policy.has_write(), policy.allow_shell).list_all();

    println!("abird dotlink tools");
    println!("────────────────────────────────────────────────────────");
    for tool in tools {
        let title = tool
            .title
            .as_deref()
            .or_else(|| tool.annotations.as_ref().and_then(|a| a.title.as_deref()))
            .or(tool.description.as_deref())
            .unwrap_or("");
        println!("• {:<16} {}", tool.name, title);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str], defaults: PermissionConfig) -> (Args, Policy) {
        let args = Args::try_parse_from(args).unwrap();
        let policy = Policy::from_args(&args, PathBuf::from("/workspace"), &defaults).unwrap();
        (args, policy)
    }

    fn no_defaults() -> PermissionConfig {
        PermissionConfig::default()
    }

    #[test]
    fn successful_runtime_resets_restart_backoff() {
        assert_eq!(next_runtime_restart_attempt(0, false), 1);
        assert_eq!(next_runtime_restart_attempt(1, false), 2);
        assert_eq!(next_runtime_restart_attempt(5, true), 1);
    }

    #[test]
    fn runtime_restart_backoff_is_bounded() {
        assert_eq!(runtime_restart_delay(1), std::time::Duration::from_secs(1));
        assert_eq!(runtime_restart_delay(2), std::time::Duration::from_secs(2));
        assert_eq!(runtime_restart_delay(5), std::time::Duration::from_secs(16));
        assert_eq!(runtime_restart_delay(6), std::time::Duration::from_secs(30));
        assert_eq!(
            runtime_restart_delay(100),
            std::time::Duration::from_secs(30)
        );
    }

    #[test]
    fn default_is_read_only_launch_directory() {
        let (_, policy) = parse(&["dotlink"], no_defaults());
        assert_eq!(policy.base_dir, PathBuf::from("/workspace"));
        assert_eq!(policy.read_roots, [PathBuf::from("/workspace")]);
        assert!(policy.write_roots.is_empty());
        assert!(!policy.allow_shell);
    }

    #[test]
    fn no_default_allow_removes_implicit_launch_read() {
        let args =
            Args::try_parse_from(["dotlink", "--no-default-allow", "--allow-read=/ref"]).unwrap();
        let policy = Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).unwrap();
        assert_eq!(policy.base_dir, PathBuf::from("/workspace"));
        assert_eq!(policy.read_roots, [PathBuf::from("/ref")]);
    }

    #[test]
    fn no_filesystem_or_shell_capability_is_rejected() {
        let args = Args::try_parse_from(["dotlink", "--no-default-allow"]).unwrap();
        assert!(Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).is_err());
    }

    #[test]
    fn shell_requires_a_readable_directory() {
        let args =
            Args::try_parse_from(["dotlink", "--no-default-allow", "--allow-shell"]).unwrap();
        let error =
            Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("shell access requires at least one readable directory")
        );

        let args = Args::try_parse_from([
            "dotlink",
            "--no-default-allow",
            "--allow-read=/project",
            "--allow-shell",
        ])
        .unwrap();
        let policy = Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).unwrap();
        assert!(policy.allow_shell);
        assert_eq!(policy.read_roots, [PathBuf::from("/project")]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deny_network_overrides_profile_network_default() {
        let defaults = PermissionConfig {
            allow_shell: true,
            allow_network: true,
            ..PermissionConfig::default()
        };
        let (_, policy) = parse(&["dotlink", "--deny-network"], defaults);
        assert!(policy.allow_shell);
        assert!(policy.sandbox_shell);
        assert!(!policy.allow_network);
    }

    #[test]
    fn allow_network_requires_shell_permission() {
        let args = Args::try_parse_from(["dotlink", "--allow-network"]).unwrap();
        assert!(Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).is_err());
    }

    #[test]
    fn bare_allow_write_and_allow_rw_mean_rw_launch_directory() {
        for flag in ["--allow-write", "--allow-rw"] {
            let (_, policy) = parse(&["dotlink", flag], no_defaults());
            assert!(policy.read_roots.contains(&PathBuf::from("/workspace")));
            assert!(policy.write_roots.contains(&PathBuf::from("/workspace")));
        }
    }

    #[test]
    fn allow_rw_root_naturally_means_unrestricted_filesystem() {
        let (_, policy) = parse(&["dotlink", "--allow-rw=/"], no_defaults());
        assert!(policy.unrestricted_fs);
        assert!(policy.read_roots.contains(&PathBuf::from("/")));
        assert!(policy.write_roots.contains(&PathBuf::from("/")));
        assert!(!policy.allow_shell);
    }

    #[test]
    fn deny_flags_are_symmetric_and_additive() {
        let defaults = PermissionConfig {
            deny_read: vec![PathBuf::from("/profile-read-secret")],
            deny_write: vec![PathBuf::from("/profile-write-secret")],
            deny_rw: vec![PathBuf::from("/profile-secret")],
            ..PermissionConfig::default()
        };
        let (_, policy) = parse(
            &[
                "dotlink",
                "--allow-rw=/both",
                "--deny-read=/read-secret",
                "--deny-write=/write-secret",
                "--deny-rw=/both/secret",
                "--deny=/legacy-secret",
            ],
            defaults,
        );

        for path in [
            "/read-secret",
            "/both/secret",
            "/legacy-secret",
            "/profile-read-secret",
            "/profile-secret",
        ] {
            assert!(policy.deny_read_roots.contains(&PathBuf::from(path)));
        }
        for path in [
            "/write-secret",
            "/both/secret",
            "/legacy-secret",
            "/profile-write-secret",
            "/profile-secret",
        ] {
            assert!(policy.deny_write_roots.contains(&PathBuf::from(path)));
        }
    }

    #[test]
    fn deny_shell_overrides_profile_shell() {
        let defaults = PermissionConfig {
            allow_shell: true,
            ..PermissionConfig::default()
        };
        let (_, policy) = parse(&["dotlink", "--deny-shell"], defaults);
        assert!(!policy.allow_shell);
    }

    #[test]
    fn allow_all_and_no_sandbox_require_each_other() {
        assert!(Args::try_parse_from(["dotlink", "--allow-all"]).is_err());
        assert!(Args::try_parse_from(["dotlink", "--no-sandbox"]).is_err());
        assert!(Args::try_parse_from(["dotlink", "--allow-all", "--no-sandbox"]).is_ok());
    }

    #[test]
    fn allow_all_no_sandbox_is_full_authority() {
        let (_, policy) = parse(&["dotlink", "--allow-all", "--no-sandbox"], no_defaults());
        assert!(policy.unrestricted_fs);
        assert!(policy.allow_shell);
        assert!(!policy.sandbox_shell);
        assert!(policy.allow_network);
    }

    #[test]
    fn allow_all_rejects_deny_rules() {
        let args = Args::try_parse_from([
            "dotlink",
            "--allow-all",
            "--no-sandbox",
            "--deny-rw=/secret",
        ])
        .unwrap();
        assert!(Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).is_err());
    }

    #[test]
    fn profile_local_transports_start_without_cli_flags() {
        let args = Args::try_parse_from(["dotlink"]).unwrap();
        let defaults = TransportConfig {
            openai: false,
            stdio: true,
            http: true,
            ..TransportConfig::default()
        };
        let effective = resolve_local_transports(&args, &defaults).unwrap();
        assert!(effective.stdio);
        assert!(effective.http);
    }

    #[test]
    fn cli_can_add_or_remove_profile_local_transports() {
        let defaults = TransportConfig {
            openai: false,
            stdio: true,
            http: true,
            ..TransportConfig::default()
        };

        let args = Args::try_parse_from(["dotlink", "--no-stdio", "--no-http"]).unwrap();
        let effective = resolve_local_transports(&args, &defaults).unwrap();
        assert!(!effective.stdio);
        assert!(!effective.http);

        let args = Args::try_parse_from(["dotlink", "--stdio", "--http"]).unwrap();
        let effective = resolve_local_transports(
            &args,
            &TransportConfig {
                openai: false,
                ..TransportConfig::default()
            },
        )
        .unwrap();
        assert!(effective.stdio);
        assert!(effective.http);
    }

    #[test]
    fn profile_and_setup_can_be_combined() {
        let args = Args::try_parse_from(["dotlink", "--setup", "-p", "work"]).unwrap();
        assert!(args.setup);
        assert_eq!(args.profile.as_deref(), Some("work"));

        let args = Args::try_parse_from(["dotlink", "-S", "-p", "work"]).unwrap();
        assert!(args.setup);
        assert_eq!(args.profile.as_deref(), Some("work"));
    }

    #[test]
    fn logging_flags_have_distinct_cli_meanings() {
        let silent = Args::try_parse_from(["dotlink", "-s"]).unwrap();
        assert!(silent.silent);
        assert_eq!(silent.verbose, 0);
        assert!(!silent.setup);

        let verbose = Args::try_parse_from(["dotlink", "-v"]).unwrap();
        assert_eq!(verbose.verbose, 1);
        assert!(!verbose.silent);

        let developer = Args::try_parse_from(["dotlink", "-vv"]).unwrap();
        assert_eq!(developer.verbose, 2);

        let both = Args::try_parse_from(["dotlink", "--silent", "-vv", "--color=never"]).unwrap();
        assert!(both.silent);
        assert_eq!(both.verbose, 2);
        assert_eq!(both.color, ColorMode::Never);
    }

    #[test]
    fn setup_uses_uppercase_s_short_flag() {
        let args = Args::try_parse_from(["dotlink", "-S"]).unwrap();
        assert!(args.setup);
        assert!(!args.silent);
    }

    #[test]
    fn profile_http_accepts_http_overrides_without_repeating_http_flag() {
        let defaults = TransportConfig {
            openai: false,
            http: true,
            ngrok: true,
            ..TransportConfig::default()
        };
        let args =
            Args::try_parse_from(["dotlink", "--http-ephemeral-url", "--ngrok-ephemeral-url"])
                .unwrap();
        let effective = resolve_local_transports(&args, &defaults).unwrap();
        assert!(effective.http);
        assert!(effective.ngrok);
        assert!(effective.http_ephemeral_url);
        assert!(effective.ngrok_ephemeral_url);
    }

    #[test]
    fn http_specific_flags_require_effective_http() {
        let defaults = TransportConfig {
            openai: false,
            ..TransportConfig::default()
        };
        for argv in [
            vec!["dotlink", "--ngrok"],
            vec!["dotlink", "--ephemeral-url"],
            vec!["dotlink", "--http-bind=127.0.0.1:4000"],
        ] {
            let args = Args::try_parse_from(argv).unwrap();
            assert!(resolve_local_transports(&args, &defaults).is_err());
        }
    }

    #[test]
    fn transport_ephemeral_overrides_profile_and_global_defaults() {
        let defaults = TransportConfig {
            openai: false,
            http: true,
            ngrok: true,
            http_ephemeral_url: true,
            ngrok_ephemeral_url: false,
            ..TransportConfig::default()
        };

        let args =
            Args::try_parse_from(["dotlink", "--ephemeral-url", "--http-ephemeral-url=false"])
                .unwrap();
        let effective = resolve_local_transports(&args, &defaults).unwrap();
        assert!(!effective.http_ephemeral_url);
        assert!(effective.ngrok_ephemeral_url);

        let args = Args::try_parse_from(["dotlink", "--ngrok-ephemeral-url=false"]).unwrap();
        let effective = resolve_local_transports(&args, &defaults).unwrap();
        assert!(effective.http_ephemeral_url);
        assert!(!effective.ngrok_ephemeral_url);
    }

    #[test]
    fn no_ngrok_disables_profile_ngrok_without_disabling_http() {
        let defaults = TransportConfig {
            openai: false,
            http: true,
            ngrok: true,
            ngrok_ephemeral_url: true,
            ..TransportConfig::default()
        };
        let args = Args::try_parse_from(["dotlink", "--no-ngrok"]).unwrap();
        let effective = resolve_local_transports(&args, &defaults).unwrap();
        assert!(effective.http);
        assert!(!effective.ngrok);
    }

    #[test]
    fn ngrok_ephemeral_override_requires_effective_ngrok() {
        let defaults = TransportConfig {
            openai: false,
            http: true,
            ngrok: false,
            ..TransportConfig::default()
        };
        let args = Args::try_parse_from(["dotlink", "--ngrok-ephemeral-url"]).unwrap();
        assert!(resolve_local_transports(&args, &defaults).is_err());
    }

    #[test]
    fn profile_subcommands_parse_cleanly() {
        let args = Args::try_parse_from(["dotlink", "profile", "list"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Profile {
                command: ProfileCommand::List
            })
        ));

        let args =
            Args::try_parse_from(["dotlink", "profile", "allow", "work", "rw", "/shared"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Profile {
                command: ProfileCommand::Allow {
                    kind: ProfileRuleKind::Rw,
                    ..
                }
            })
        ));

        let args =
            Args::try_parse_from(["dotlink", "profile", "enable", "work", "http-ephemeral-url"])
                .unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Profile {
                command: ProfileCommand::Enable {
                    setting: ProfileBool::HttpEphemeral,
                    ..
                }
            })
        ));

        let args =
            Args::try_parse_from(["dotlink", "profile", "disable", "work", "network"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Profile {
                command: ProfileCommand::Disable {
                    setting: ProfileBool::Network,
                    ..
                }
            })
        ));
    }

    #[test]
    fn setup_is_not_mixed_with_protocol_stdio_or_http() {
        assert!(Args::try_parse_from(["dotlink", "--setup", "--stdio"]).is_err());
        assert!(Args::try_parse_from(["dotlink", "--setup", "--http"]).is_err());
    }
}
