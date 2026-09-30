mod logging;
mod mcp;
mod setup;
mod transports;

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::{
    logging::{ColorMode, LogConfig},
    mcp::{AccessSpec, LocalMachine, MachineConfig, SandboxCacheMount},
    setup::{PermissionConfig, load_or_setup},
    transports::ActiveTransports,
};

#[derive(Debug, Parser)]
#[command(
    name = "abird-link",
    version,
    about = "Permission-scoped local MCP bridge over OpenAI Tunnel, stdio, or HTTP"
)]
struct Args {
    /// Run interactive setup for the selected profile.
    #[arg(short = 'S', long, conflicts_with_all = ["stdio", "http", "ngrok"])]
    setup: bool,

    /// Select a named config profile (config.<profile>.jsonc).
    #[arg(short = 'p', long, value_name = "NAME")]
    profile: Option<String>,

    /// Default working directory. Defaults to the directory where abird-link is launched.
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// Start the stdio MCP server for this run.
    #[arg(long)]
    stdio: bool,

    /// Start the HTTP MCP server for this run.
    #[arg(long)]
    http: bool,

    /// Override the configured HTTP listen address for this run.
    #[arg(long, value_name = "ADDR", requires = "http")]
    http_bind: Option<SocketAddr>,

    /// Publish the HTTP MCP server through ngrok. Requires --http and NGROK_AUTHTOKEN.
    #[arg(long, requires = "http")]
    ngrok: bool,

    /// Use fresh hard-to-guess URL paths for both local HTTP and ngrok.
    #[arg(long, requires = "http")]
    ephemeral_url: bool,

    /// Override local HTTP ephemeral-path behavior. Bare flag means true.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true,
        requires = "http",
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
        requires = "ngrok",
        alias = "ngrok-emphemeral-url"
    )]
    ngrok_ephemeral_url: Option<bool>,

    /// Add readable access. Bare --allow-read means cwd.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    allow_read: Vec<PathBuf>,

    /// Add writable access. Bare --allow-write means read+write cwd.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    allow_write: Vec<PathBuf>,

    /// Add read+write access. Bare --allow-rw means cwd.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    allow_rw: Vec<PathBuf>,

    /// Deny reads. Bare --deny-read means cwd. Denies override allows.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    deny_read: Vec<PathBuf>,

    /// Deny writes. Bare --deny-write means cwd. Denies override allows.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    deny_write: Vec<PathBuf>,

    /// Deny both reads and writes. Bare --deny-rw means cwd.
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

    /// Hide normal tool-attempt activity logging.
    #[arg(short = 's', long)]
    silent: bool,

    /// Enable developer request logging in addition to normal tool activity.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Control ANSI colors in human-facing stderr output.
    #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
    color: ColorMode,

    /// List the MCP tools exposed under the effective policy and exit.
    #[arg(long)]
    list_tools: bool,
}

#[derive(Debug, Clone)]
struct Policy {
    cwd: PathBuf,
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
    fn from_args(args: &Args, launch_cwd: PathBuf, defaults: &PermissionConfig) -> Result<Self> {
        let configured_cwd = args
            .cwd
            .as_ref()
            .or(defaults.cwd.as_ref())
            .cloned()
            .unwrap_or_else(|| launch_cwd.clone());
        let cwd = if configured_cwd.is_absolute() {
            configured_cwd
        } else {
            launch_cwd.join(configured_cwd)
        };

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

        let mut read_roots = vec![cwd.clone()];
        let mut write_roots = Vec::new();

        for path in defaults.allow_read.iter().chain(defaults.allow_rw.iter()) {
            read_roots.push(cwd_or_path(path, &cwd));
        }
        for path in defaults.allow_write.iter().chain(defaults.allow_rw.iter()) {
            write_roots.push(cwd_or_path(path, &cwd));
        }

        for path in &args.allow_read {
            read_roots.push(cwd_or_path(path, &cwd));
        }
        for path in &args.allow_write {
            if path == &PathBuf::from(".") {
                // Historical ergonomic behavior: bare --allow-write means rw cwd.
                read_roots.push(cwd.clone());
                write_roots.push(cwd.clone());
            } else {
                write_roots.push(cwd_or_path(path, &cwd));
            }
        }
        for path in &args.allow_rw {
            let path = cwd_or_path(path, &cwd);
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
            deny_read_roots.push(cwd_or_path(path, &cwd));
        }
        for path in defaults.deny_write.iter().chain(defaults.deny_rw.iter()) {
            deny_write_roots.push(cwd_or_path(path, &cwd));
        }

        for path in &args.deny_read {
            deny_read_roots.push(cwd_or_path(path, &cwd));
        }
        for path in &args.deny_write {
            deny_write_roots.push(cwd_or_path(path, &cwd));
        }
        for path in args.deny_rw.iter().chain(args.deny.iter()) {
            let path = cwd_or_path(path, &cwd);
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

        Ok(Self {
            cwd,
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

fn cwd_or_path(path: &PathBuf, cwd: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.clone()
    } else {
        cwd.join(path)
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let launch_cwd = std::env::current_dir()?;

    let log = LogConfig::new(args.silent, args.verbose, args.color);
    let default_filter = if args.verbose {
        "abird_link=debug"
    } else {
        "abird_link=warn"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_writer(std::io::stderr)
        .with_ansi(log.color_enabled())
        .init();

    let mut runtime_restarts = 0_u32;
    loop {
        match run_runtime(&args, &launch_cwd, &log).await {
            Ok(()) => return Ok(()),
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
                    "OpenAI transport remained unhealthy; restarting abird-link runtime"
                );
                tracing::debug!(%error, "runtime restart reason");
                eprintln!(
                    "\nOpenAI tunnel unhealthy — restarting abird-link runtime in {}s…",
                    delay.as_secs()
                );

                tokio::select! {
                    signal = tokio::signal::ctrl_c() => {
                        signal?;
                        eprintln!("\nabird-link stopped.");
                        return Ok(());
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

async fn run_runtime(args: &Args, launch_cwd: &Path, log: &LogConfig) -> Result<()> {
    let setup = load_or_setup(args.setup, args.profile.as_deref()).await?;

    if args.setup {
        return Ok(());
    }

    let policy = Policy::from_args(args, launch_cwd.to_path_buf(), &setup.config.permissions)?;

    if args.list_tools {
        print_tools(&policy);
        return Ok(());
    }

    if args.print_id {
        let tunnel_id = setup
            .config
            .tunnel_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("OpenAI transport is not configured"))?;
        println!("{tunnel_id}");
        return Ok(());
    }

    let http_bind = if args.http {
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

    let (http_ephemeral_url, ngrok_ephemeral_url) = ephemeral_url_policy(args);

    let active_transports = ActiveTransports {
        openai,
        stdio: args.stdio,
        http: http_bind.map(|bind| transports::http::Config {
            bind,
            ngrok: args.ngrok,
            http_ephemeral_url,
            ngrok_ephemeral_url,
            log: log.clone(),
        }),
    };

    if active_transports.is_empty() {
        bail!(
            "no MCP transport is active; enable OpenAI in setup or start a local transport with --stdio and/or --http"
        );
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
            cwd: policy.cwd.clone(),
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

    print_banner(
        &active_transports,
        &machine,
        args.profile.as_deref(),
        log,
        args.allow_all,
    );

    tokio::select! {
        result = active_transports.run(machine, cancellation.child_token()) => result?,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            cancellation.cancel();
            eprintln!("\nabird-link stopped.");
        }
    }

    Ok(())
}

fn ephemeral_url_policy(args: &Args) -> (bool, bool) {
    (
        args.http_ephemeral_url.unwrap_or(args.ephemeral_url),
        args.ngrok_ephemeral_url.unwrap_or(args.ephemeral_url),
    )
}

fn print_banner(
    transports: &ActiveTransports,
    machine: &LocalMachine,
    profile: Option<&str>,
    log: &LogConfig,
    allow_all: bool,
) {
    eprintln!();
    eprintln!(
        "{}",
        log.style("1;36", format!("abird-link {}", env!("CARGO_PKG_VERSION")))
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
    eprintln!("• Cwd        {}", machine.cwd().display());
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
    if log.developer_enabled() {
        eprintln!("• Logging    verbose");
    }
    eprintln!("• Status     {}", log.style("33", "starting…"));
    eprintln!();
    if transports.openai.is_some() {
        eprintln!("OpenAI Secure MCP Tunnel active.");
    }
    if transports.stdio {
        eprintln!("stdio MCP server active; stdout is reserved for MCP.");
    }
    eprintln!("Ctrl-C to stop.");
    eprintln!();
}

fn print_tools(policy: &Policy) {
    let tools =
        LocalMachine::tool_router_for_policy(policy.has_write(), policy.allow_shell).list_all();

    println!("abird-link tools");
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
    fn default_is_read_only_cwd() {
        let (_, policy) = parse(&["abird-link"], no_defaults());
        assert_eq!(policy.read_roots, [PathBuf::from("/workspace")]);
        assert!(policy.write_roots.is_empty());
        assert!(!policy.allow_shell);
    }

    #[test]
    fn config_defaults_can_persist_cwd_paths_shell_and_network() {
        let defaults = PermissionConfig {
            cwd: Some(PathBuf::from("/project")),
            allow_read: vec![PathBuf::from("/reference")],
            allow_write: vec![PathBuf::from("generated")],
            allow_rw: vec![PathBuf::from(".")],
            deny_read: vec![PathBuf::from("private")],
            deny_write: vec![PathBuf::from("locked")],
            deny_rw: vec![PathBuf::from("secret")],
            allow_shell: true,
            allow_network: true,
        };
        let (_, policy) = parse(&["abird-link"], defaults);

        assert_eq!(policy.cwd, PathBuf::from("/project"));
        assert!(policy.read_roots.contains(&PathBuf::from("/project")));
        assert!(policy.read_roots.contains(&PathBuf::from("/reference")));
        assert!(policy.write_roots.contains(&PathBuf::from("/project")));
        assert!(
            policy
                .write_roots
                .contains(&PathBuf::from("/project/generated"))
        );
        assert!(
            policy
                .deny_read_roots
                .contains(&PathBuf::from("/project/private"))
        );
        assert!(
            policy
                .deny_write_roots
                .contains(&PathBuf::from("/project/locked"))
        );
        assert!(
            policy
                .deny_read_roots
                .contains(&PathBuf::from("/project/secret"))
        );
        assert!(
            policy
                .deny_write_roots
                .contains(&PathBuf::from("/project/secret"))
        );
        assert!(policy.allow_shell);
        if cfg!(target_os = "linux") {
            assert!(policy.allow_network);
        }
    }

    #[test]
    fn cli_cwd_overrides_profile_cwd() {
        let defaults = PermissionConfig {
            cwd: Some(PathBuf::from("/profile")),
            ..PermissionConfig::default()
        };
        let (_, policy) = parse(&["abird-link", "--cwd=/cli"], defaults);
        assert_eq!(policy.cwd, PathBuf::from("/cli"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deny_network_overrides_profile_network_default() {
        let defaults = PermissionConfig {
            allow_shell: true,
            allow_network: true,
            ..PermissionConfig::default()
        };
        let (_, policy) = parse(&["abird-link", "--deny-network"], defaults);
        assert!(policy.allow_shell);
        assert!(policy.sandbox_shell);
        assert!(!policy.allow_network);
    }

    #[test]
    fn allow_network_requires_shell_permission() {
        let args = Args::try_parse_from(["abird-link", "--allow-network"]).unwrap();
        assert!(Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).is_err());
    }

    #[test]
    fn bare_allow_write_and_allow_rw_mean_rw_cwd() {
        for flag in ["--allow-write", "--allow-rw"] {
            let (_, policy) = parse(&["abird-link", flag], no_defaults());
            assert!(policy.read_roots.contains(&PathBuf::from("/workspace")));
            assert!(policy.write_roots.contains(&PathBuf::from("/workspace")));
        }
    }

    #[test]
    fn allow_rw_root_naturally_means_unrestricted_filesystem() {
        let (_, policy) = parse(&["abird-link", "--allow-rw=/"], no_defaults());
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
                "abird-link",
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
        let (_, policy) = parse(&["abird-link", "--deny-shell"], defaults);
        assert!(!policy.allow_shell);
    }

    #[test]
    fn allow_all_and_no_sandbox_require_each_other() {
        assert!(Args::try_parse_from(["abird-link", "--allow-all"]).is_err());
        assert!(Args::try_parse_from(["abird-link", "--no-sandbox"]).is_err());
        assert!(Args::try_parse_from(["abird-link", "--allow-all", "--no-sandbox"]).is_ok());
    }

    #[test]
    fn allow_all_no_sandbox_is_full_authority() {
        let (_, policy) = parse(
            &["abird-link", "--allow-all", "--no-sandbox"],
            no_defaults(),
        );
        assert!(policy.unrestricted_fs);
        assert!(policy.allow_shell);
        assert!(!policy.sandbox_shell);
        assert!(policy.allow_network);
    }

    #[test]
    fn allow_all_rejects_deny_rules() {
        let args = Args::try_parse_from([
            "abird-link",
            "--allow-all",
            "--no-sandbox",
            "--deny-rw=/secret",
        ])
        .unwrap();
        assert!(Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).is_err());
    }

    #[test]
    fn explicit_local_transport_flags_parse_as_runtime_overrides() {
        let args = Args::try_parse_from(["abird-link", "--stdio", "--http"]).unwrap();
        assert!(args.stdio);
        assert!(args.http);
    }

    #[test]
    fn profile_and_setup_can_be_combined() {
        let args = Args::try_parse_from(["abird-link", "--setup", "-p", "work"]).unwrap();
        assert!(args.setup);
        assert_eq!(args.profile.as_deref(), Some("work"));

        let args = Args::try_parse_from(["abird-link", "-S", "-p", "work"]).unwrap();
        assert!(args.setup);
        assert_eq!(args.profile.as_deref(), Some("work"));
    }

    #[test]
    fn logging_flags_have_distinct_cli_meanings() {
        let silent = Args::try_parse_from(["abird-link", "-s"]).unwrap();
        assert!(silent.silent);
        assert!(!silent.verbose);
        assert!(!silent.setup);

        let verbose = Args::try_parse_from(["abird-link", "-v"]).unwrap();
        assert!(verbose.verbose);
        assert!(!verbose.silent);

        let both =
            Args::try_parse_from(["abird-link", "--silent", "--verbose", "--color=never"]).unwrap();
        assert!(both.silent);
        assert!(both.verbose);
        assert_eq!(both.color, ColorMode::Never);
    }

    #[test]
    fn setup_uses_uppercase_s_short_flag() {
        let args = Args::try_parse_from(["abird-link", "-S"]).unwrap();
        assert!(args.setup);
        assert!(!args.silent);
    }

    #[test]
    fn ngrok_requires_http() {
        assert!(Args::try_parse_from(["abird-link", "--ngrok"]).is_err());
        let args = Args::try_parse_from(["abird-link", "--http", "--ngrok"]).unwrap();
        assert!(args.http);
        assert!(args.ngrok);
    }

    #[test]
    fn ephemeral_url_requires_http() {
        assert!(Args::try_parse_from(["abird-link", "--ephemeral-url"]).is_err());
        let args = Args::try_parse_from(["abird-link", "--http", "--ephemeral-url"]).unwrap();
        assert!(args.http);
        assert!(args.ephemeral_url);
    }

    #[test]
    fn transport_ephemeral_overrides_parse_independently() {
        let args = Args::try_parse_from([
            "abird-link",
            "--http",
            "--ngrok",
            "--ephemeral-url",
            "--http-ephemeral-url=false",
            "--ngrok-ephemeral-url",
        ])
        .unwrap();
        assert_eq!(ephemeral_url_policy(&args), (false, true));
    }

    #[test]
    fn global_ephemeral_url_can_be_overridden_per_transport() {
        let args = Args::try_parse_from([
            "abird-link",
            "--http",
            "--ngrok",
            "--ephemeral-url",
            "--http-ephemeral-url=false",
        ])
        .unwrap();
        assert_eq!(ephemeral_url_policy(&args), (false, true));

        let args = Args::try_parse_from([
            "abird-link",
            "--http",
            "--ngrok",
            "--ephemeral-url",
            "--ngrok-ephemeral-url=false",
        ])
        .unwrap();
        assert_eq!(ephemeral_url_policy(&args), (true, false));
    }

    #[test]
    fn individual_ephemeral_flags_are_independent() {
        let args = Args::try_parse_from(["abird-link", "--http", "--http-ephemeral-url"]).unwrap();
        assert_eq!(ephemeral_url_policy(&args), (true, false));

        let args =
            Args::try_parse_from(["abird-link", "--http", "--ngrok", "--ngrok-ephemeral-url"])
                .unwrap();
        assert_eq!(ephemeral_url_policy(&args), (false, true));
    }

    #[test]
    fn ngrok_ephemeral_override_requires_ngrok() {
        assert!(Args::try_parse_from(["abird-link", "--http", "--ngrok-ephemeral-url"]).is_err());
    }

    #[test]
    fn setup_is_not_mixed_with_protocol_stdio_or_http() {
        assert!(Args::try_parse_from(["abird-link", "--setup", "--stdio"]).is_err());
        assert!(Args::try_parse_from(["abird-link", "--setup", "--http"]).is_err());
    }
}
