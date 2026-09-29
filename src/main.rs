mod mcp;
mod setup;
mod transports;

use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Result, bail};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::{
    mcp::{AccessSpec, LocalMachine, MachineConfig},
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
    #[arg(short = 's', long, conflicts_with_all = ["stdio", "http", "ngrok"])]
    setup: bool,

    /// Select a named config profile (config.<profile>.json).
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

    /// Acknowledge network access for an unsandboxed shell.
    #[arg(long, alias = "allow-network-dangerous")]
    allow_network_dangereous: bool,

    /// Allow filesystem read/write everywhere. Path denies still take precedence.
    #[arg(long)]
    allow_rw_all_dangerous: bool,

    /// Deny the unrestricted filesystem grant, overriding dangerous grant shortcuts.
    #[arg(long)]
    deny_rw_all_dangerous: bool,

    /// Disable the Linux Bubblewrap shell sandbox.
    #[arg(long)]
    no_sandbox: bool,

    /// Shortcut for unrestricted filesystem + unsandboxed shell + network.
    /// Explicit deny flags still take precedence.
    #[arg(long)]
    allow_all_dangerous: bool,

    /// Print the configured OpenAI Tunnel ID and exit.
    #[arg(long)]
    print_id: bool,

    /// Show concise incoming MCP requests and tool calls.
    #[arg(short, long)]
    verbose: bool,

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
        let cwd = args.cwd.clone().unwrap_or(launch_cwd);
        let allow_all = args.allow_all_dangerous;
        let unrestricted_fs =
            (args.allow_rw_all_dangerous || allow_all) && !args.deny_rw_all_dangerous;

        let requested_shell = defaults.allow_shell || args.allow_shell || allow_all;
        let allow_shell = requested_shell && !args.deny_shell;
        let requested_no_sandbox = args.no_sandbox || allow_all;
        let dangerous_network = args.allow_network_dangereous || allow_all;

        let has_filesystem_denies = !args.deny_read.is_empty()
            || !args.deny_write.is_empty()
            || !args.deny_rw.is_empty()
            || !args.deny.is_empty();
        let deny_requires_sandbox =
            args.deny_network || has_filesystem_denies || args.deny_rw_all_dangerous;

        #[cfg(not(target_os = "linux"))]
        if allow_shell && deny_requires_sandbox {
            bail!(
                "filesystem/network deny rules cannot be enforced for shell execution on this OS; use --deny-shell or remove the deny rules"
            );
        }

        // Denies win over --no-sandbox/--allow-all-dangerous on Linux because
        // Bubblewrap is the subprocess enforcement boundary.
        let effective_no_sandbox = requested_no_sandbox
            && !(allow_shell && deny_requires_sandbox && cfg!(target_os = "linux"));

        #[cfg(not(target_os = "linux"))]
        if allow_shell && !effective_no_sandbox {
            bail!(
                "this OS has no Bubblewrap sandbox; shell access requires --no-sandbox plus the dangerous filesystem/network opt-ins, or --allow-all-dangerous"
            );
        }

        let sandbox_shell = cfg!(target_os = "linux") && allow_shell && !effective_no_sandbox;

        let allow_network = if !allow_shell || args.deny_network {
            false
        } else if sandbox_shell {
            args.allow_network
        } else {
            if !unrestricted_fs {
                bail!(
                    "unsandboxed shell access requires --allow-rw-all-dangerous (or --allow-all-dangerous)"
                );
            }
            if !dangerous_network {
                bail!(
                    "unsandboxed shell access inherently has network access; add --allow-network-dangereous or use --allow-all-dangerous"
                );
            }
            true
        };

        let mut read_roots = vec![cwd.clone()];
        let mut write_roots = Vec::new();

        if defaults.allow_rw {
            write_roots.push(cwd.clone());
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
                write_roots.push(path.clone());
            }
        }
        for path in &args.allow_rw {
            let path = cwd_or_path(path, &cwd);
            read_roots.push(path.clone());
            write_roots.push(path);
        }

        let mut deny_read_roots = Vec::new();
        let mut deny_write_roots = Vec::new();

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
    if path == &PathBuf::from(".") {
        cwd.to_path_buf()
    } else {
        path.clone()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let launch_cwd = std::env::current_dir()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("abird_link=warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(true)
        .init();

    let setup = load_or_setup(args.setup, args.profile.as_deref()).await?;

    if args.setup {
        return Ok(());
    }

    let policy = Policy::from_args(&args, launch_cwd, &setup.config.permissions)?;

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
            verbose: args.verbose,
        })
    } else {
        None
    };

    let (http_ephemeral_url, ngrok_ephemeral_url) = ephemeral_url_policy(&args);

    let active_transports = ActiveTransports {
        openai,
        stdio: args.stdio,
        http: http_bind.map(|bind| transports::http::Config {
            bind,
            ngrok: args.ngrok,
            http_ephemeral_url,
            ngrok_ephemeral_url,
        }),
    };

    if active_transports.is_empty() {
        bail!(
            "no MCP transport is active; enable OpenAI in setup or start a local transport with --stdio and/or --http"
        );
    }

    let machine = LocalMachine::new(MachineConfig {
        access: AccessSpec {
            cwd: policy.cwd.clone(),
            read_roots: policy.read_roots.clone(),
            write_roots: policy.write_roots.clone(),
            deny_read_roots: policy.deny_read_roots.clone(),
            deny_write_roots: policy.deny_write_roots.clone(),
            rw_all_dangerous: policy.unrestricted_fs,
        },
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
        args.verbose,
        args.allow_all_dangerous,
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
    verbose: bool,
    allow_all_dangerous: bool,
) {
    eprintln!();
    eprintln!("abird-link {}", env!("CARGO_PKG_VERSION"));
    eprintln!("────────────────────────────────────────────────────────");
    eprintln!("• Profile    {}", profile.unwrap_or("default"));
    eprintln!("• Transports {}", transports.names().join(", "));
    if let Some(openai) = &transports.openai {
        eprintln!("• Tunnel     {}", openai.tunnel_id);
    }
    if let Some(http) = transports.http {
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
    let access_summary = if allow_all_dangerous {
        format!(
            "{} (allow-all requested; explicit denies still win)",
            machine.access_summary()
        )
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
    if verbose {
        eprintln!("• Verbose    enabled");
    }
    eprintln!("• Status     starting…");
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
    fn default_is_read_only_cwd() {
        let (_, policy) = parse(&["abird-link"], no_defaults());
        assert_eq!(policy.read_roots, [PathBuf::from("/workspace")]);
        assert!(policy.write_roots.is_empty());
        assert!(!policy.allow_shell);
    }

    #[test]
    fn config_defaults_can_enable_rw_and_shell() {
        let defaults = PermissionConfig {
            allow_rw: true,
            allow_shell: true,
        };
        let (_, policy) = parse(&["abird-link"], defaults);
        assert!(policy.write_roots.contains(&PathBuf::from("/workspace")));
        assert!(policy.allow_shell);
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
    fn deny_flags_are_symmetric_and_additive() {
        let (_, policy) = parse(
            &[
                "abird-link",
                "--allow-rw=/both",
                "--deny-read=/read-secret",
                "--deny-write=/write-secret",
                "--deny-rw=/both/secret",
                "--deny=/legacy-secret",
            ],
            no_defaults(),
        );

        assert!(
            policy
                .deny_read_roots
                .contains(&PathBuf::from("/read-secret"))
        );
        assert!(
            policy
                .deny_write_roots
                .contains(&PathBuf::from("/write-secret"))
        );
        for path in ["/both/secret", "/legacy-secret"] {
            let path = PathBuf::from(path);
            assert!(policy.deny_read_roots.contains(&path));
            assert!(policy.deny_write_roots.contains(&path));
        }
    }

    #[test]
    fn deny_shell_overrides_config_and_dangerous_grants() {
        let defaults = PermissionConfig {
            allow_rw: false,
            allow_shell: true,
        };
        let (_, policy) = parse(&["abird-link", "--deny-shell"], defaults);
        assert!(!policy.allow_shell);

        let (_, policy) = parse(
            &["abird-link", "--allow-all-dangerous", "--deny-shell"],
            no_defaults(),
        );
        assert!(!policy.allow_shell);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deny_network_forces_sandbox_and_wins_over_dangerous_allow() {
        let (_, policy) = parse(
            &["abird-link", "--allow-all-dangerous", "--deny-network"],
            no_defaults(),
        );
        assert!(policy.allow_shell);
        assert!(policy.sandbox_shell);
        assert!(!policy.allow_network);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deny_rw_all_dangerous_overrides_allow_all_and_forces_sandbox() {
        let (_, policy) = parse(
            &[
                "abird-link",
                "--allow-all-dangerous",
                "--deny-rw-all-dangerous",
            ],
            no_defaults(),
        );
        assert!(!policy.unrestricted_fs);
        assert!(policy.allow_shell);
        assert!(policy.sandbox_shell);
        assert!(!policy.allow_network);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn filesystem_deny_forces_sandbox_and_wins_over_no_sandbox() {
        let (_, policy) = parse(
            &["abird-link", "--allow-all-dangerous", "--deny-rw=/secret"],
            no_defaults(),
        );
        assert!(policy.allow_shell);
        assert!(policy.sandbox_shell);
        assert!(policy.deny_read_roots.contains(&PathBuf::from("/secret")));
        assert!(policy.deny_write_roots.contains(&PathBuf::from("/secret")));
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

        let args = Args::try_parse_from(["abird-link", "-s", "-p", "work"]).unwrap();
        assert!(args.setup);
        assert_eq!(args.profile.as_deref(), Some("work"));
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
