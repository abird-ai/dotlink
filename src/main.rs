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
    setup::load_or_setup,
    transports::ActiveTransports,
};

#[derive(Debug, Parser)]
#[command(
    name = "abird-tunnel",
    version,
    about = "Permission-scoped local MCP bridge over OpenAI Tunnel, stdio, or HTTP"
)]
struct Args {
    /// Re-run the interactive first-use setup and replace the saved configuration.
    #[arg(long, conflicts_with_all = ["stdio", "http", "ngrok"])]
    setup: bool,

    /// Default working directory. Defaults to the directory where abird-tunnel is launched.
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// Start the stdio MCP server. Must be enabled in persisted transport config.
    #[arg(long)]
    stdio: bool,

    /// Start the HTTP MCP server. Must be enabled in persisted transport config.
    #[arg(long)]
    http: bool,

    /// Override the configured HTTP listen address for this run.
    #[arg(long, value_name = "ADDR", requires = "http")]
    http_bind: Option<SocketAddr>,

    /// Publish the HTTP MCP server through ngrok. Requires --http and NGROK_AUTHTOKEN.
    #[arg(long, requires = "http")]
    ngrok: bool,

    /// Add a readable directory. May be repeated. The cwd is readable by default.
    #[arg(long, value_name = "DIR")]
    allow_read: Vec<PathBuf>,

    /// Add a writable directory. May be repeated. Bare --allow-write means read+write cwd.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    allow_write: Vec<PathBuf>,

    /// Add a directory with both read and write permission. May be repeated.
    #[arg(long, value_name = "DIR")]
    allow_rw: Vec<PathBuf>,

    /// Deny a path even if another allow rule covers it. May be repeated.
    #[arg(long, value_name = "PATH")]
    deny: Vec<PathBuf>,

    /// Enable the platform shell tool (bash on Unix, PowerShell on Windows).
    #[arg(long)]
    allow_shell: bool,

    /// Allow network access from a Bubblewrap-sandboxed shell.
    #[arg(long)]
    allow_network: bool,

    /// Allow network access for an unsandboxed shell. Deliberately dangerous.
    #[arg(long, alias = "allow-network-dangerous")]
    allow_network_dangereous: bool,

    /// Allow filesystem read/write everywhere. Deliberately dangerous.
    #[arg(long)]
    allow_rw_all_dangerous: bool,

    /// Disable the Linux Bubblewrap shell sandbox.
    #[arg(long)]
    no_sandbox: bool,

    /// Allow unrestricted filesystem, unsandboxed shell, and network access.
    #[arg(long)]
    allow_all_dangerous: bool,

    /// Print the saved tunnel id and exit.
    #[arg(long)]
    print_id: bool,

    /// Show concise incoming MCP requests and tool calls.
    #[arg(short, long)]
    verbose: bool,

    /// List the MCP tools exposed under the selected capability flags and exit.
    #[arg(long)]
    list_tools: bool,
}

#[derive(Debug, Clone)]
struct Policy {
    cwd: PathBuf,
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
    deny_roots: Vec<PathBuf>,
    unrestricted_fs: bool,
    allow_shell: bool,
    sandbox_shell: bool,
    allow_network: bool,
}

impl Policy {
    fn from_args(args: &Args, launch_cwd: PathBuf) -> Result<Self> {
        let cwd = args.cwd.clone().unwrap_or(launch_cwd);
        let allow_all = args.allow_all_dangerous;
        let unrestricted_fs = args.allow_rw_all_dangerous || allow_all;
        let allow_shell = args.allow_shell || allow_all;
        let no_sandbox = args.no_sandbox || allow_all;
        let dangerous_network = args.allow_network_dangereous || allow_all;

        if args.allow_network && !allow_shell {
            bail!("--allow-network requires --allow-shell");
        }
        if args.no_sandbox && !allow_shell && !allow_all {
            bail!("--no-sandbox requires --allow-shell");
        }
        if args.allow_network_dangereous && !allow_shell && !allow_all {
            bail!("--allow-network-dangereous requires --allow-shell");
        }

        if args.allow_network && no_sandbox {
            bail!(
                "--allow-network is only for the sandbox; unsandboxed network requires --allow-network-dangereous or --allow-all-dangerous"
            );
        }

        if args.allow_network_dangereous && !no_sandbox && !allow_all {
            bail!(
                "--allow-network-dangereous is only for an unsandboxed shell; use --allow-network with Bubblewrap"
            );
        }

        if no_sandbox && allow_shell && !args.deny.is_empty() {
            bail!(
                "--deny cannot constrain an unsandboxed shell; remove --deny or keep the Bubblewrap sandbox enabled"
            );
        }

        if no_sandbox && allow_shell && !unrestricted_fs {
            bail!(
                "unsandboxed shell access requires --allow-rw-all-dangerous (or --allow-all-dangerous)"
            );
        }
        if no_sandbox && allow_shell && !dangerous_network {
            bail!(
                "unsandboxed shell access inherently has network access; add --allow-network-dangereous or use --allow-all-dangerous"
            );
        }

        #[cfg(not(target_os = "linux"))]
        if allow_shell && !no_sandbox {
            bail!(
                "this OS has no Bubblewrap sandbox; shell access requires --no-sandbox plus the dangerous filesystem/network opt-ins, or --allow-all-dangerous"
            );
        }

        let sandbox_shell = cfg!(target_os = "linux") && allow_shell && !no_sandbox;
        let allow_network = if sandbox_shell {
            args.allow_network
        } else {
            allow_shell && dangerous_network
        };

        // The cwd is always readable unless unrestricted access supersedes path grants.
        let mut read_roots = vec![cwd.clone()];
        read_roots.extend(args.allow_read.iter().cloned());

        let mut write_roots = Vec::new();
        for path in &args.allow_write {
            if path.as_path() == PathBuf::from(".").as_path() {
                // Bare --allow-write is the ergonomic shorthand for --allow-rw=<cwd>.
                read_roots.push(cwd.clone());
                write_roots.push(cwd.clone());
            } else {
                write_roots.push(path.clone());
            }
        }
        for path in &args.allow_rw {
            read_roots.push(path.clone());
            write_roots.push(path.clone());
        }

        Ok(Self {
            cwd,
            read_roots,
            write_roots,
            deny_roots: args.deny.clone(),
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let launch_cwd = std::env::current_dir()?;
    let policy = Policy::from_args(&args, launch_cwd)?;

    if args.list_tools {
        print_tools(&policy);
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("abird_tunnel=warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(true)
        .init();

    let setup = load_or_setup(args.setup).await?;

    if args.setup {
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

    if args.stdio && !setup.config.transports.stdio {
        bail!("stdio transport is disabled in config; run 'abird-tunnel --setup' to enable it");
    }
    if args.http && !setup.config.transports.http {
        bail!("HTTP transport is disabled in config; run 'abird-tunnel --setup' to enable it");
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

    let active_transports = ActiveTransports {
        openai,
        stdio: args.stdio,
        http: http_bind.map(|bind| transports::http::Config {
            bind,
            ngrok: args.ngrok,
        }),
    };

    if active_transports.is_empty() {
        bail!(
            "no MCP transport is active; enable OpenAI in setup or start a configured local transport with --stdio and/or --http"
        );
    }

    let machine = LocalMachine::new(MachineConfig {
        access: AccessSpec {
            cwd: policy.cwd.clone(),
            read_roots: policy.read_roots.clone(),
            write_roots: policy.write_roots.clone(),
            deny_roots: policy.deny_roots.clone(),
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
        args.verbose,
        args.allow_all_dangerous,
    );

    tokio::select! {
        result = active_transports.run(machine, cancellation.child_token()) => result?,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            cancellation.cancel();
            eprintln!("\nabird-tunnel stopped.");
        }
    }

    Ok(())
}

fn print_banner(
    transports: &ActiveTransports,
    machine: &LocalMachine,
    verbose: bool,
    allow_all_dangerous: bool,
) {
    eprintln!();
    eprintln!("abird-tunnel {}", env!("CARGO_PKG_VERSION"));
    eprintln!("────────────────────────────────────────────────────────");
    eprintln!("• Transports {}", transports.names().join(", "));
    if let Some(openai) = &transports.openai {
        eprintln!("• Tunnel     {}", openai.tunnel_id);
    }
    if let Some(http) = transports.http {
        eprintln!("• HTTP       http://{}/mcp", http.bind);
        if http.ngrok {
            eprintln!("• ngrok      enabled; public URL will be printed after connection");
        }
    }
    eprintln!("• Cwd        {}", machine.cwd().display());
    let access_summary = if allow_all_dangerous {
        "ALL DANGEROUS (unrestricted fs + shell + network)".to_owned()
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

    println!("abird-tunnel tools");
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

    fn parse(args: &[&str]) -> (Args, Policy) {
        let args = Args::try_parse_from(args).unwrap();
        let policy = Policy::from_args(&args, PathBuf::from("/workspace")).unwrap();
        (args, policy)
    }

    #[test]
    fn default_is_read_only_cwd() {
        let (_, policy) = parse(&["abird-tunnel"]);
        assert_eq!(policy.read_roots, [PathBuf::from("/workspace")]);
        assert!(policy.write_roots.is_empty());
        assert!(!policy.allow_shell);
    }

    #[test]
    fn bare_allow_write_means_rw_cwd() {
        let (_, policy) = parse(&["abird-tunnel", "--allow-write"]);
        assert!(policy.read_roots.contains(&PathBuf::from("/workspace")));
        assert!(policy.write_roots.contains(&PathBuf::from("/workspace")));
    }

    #[test]
    fn path_grants_are_additive() {
        let (_, policy) = parse(&[
            "abird-tunnel",
            "--allow-read=/read",
            "--allow-write=/write",
            "--allow-rw=/both",
            "--deny=/both/secret",
        ]);
        assert!(policy.read_roots.contains(&PathBuf::from("/read")));
        assert!(policy.read_roots.contains(&PathBuf::from("/both")));
        assert!(policy.write_roots.contains(&PathBuf::from("/write")));
        assert!(policy.write_roots.contains(&PathBuf::from("/both")));
        assert_eq!(policy.deny_roots, [PathBuf::from("/both/secret")]);
    }

    #[test]
    fn sandbox_network_is_explicit() {
        let args =
            Args::try_parse_from(["abird-tunnel", "--allow-shell", "--allow-network"]).unwrap();
        let policy = Policy::from_args(&args, PathBuf::from("/workspace")).unwrap();
        assert_eq!(policy.sandbox_shell, cfg!(target_os = "linux"));
        if cfg!(target_os = "linux") {
            assert!(policy.allow_network);
        }
    }

    #[test]
    fn allow_all_dangerous_is_the_full_escape_hatch() {
        let (_, policy) = parse(&["abird-tunnel", "--allow-all-dangerous"]);
        assert!(policy.unrestricted_fs);
        assert!(policy.allow_shell);
        assert!(!policy.sandbox_shell);
        assert!(policy.allow_network);
    }

    #[test]
    fn unsandboxed_shell_requires_both_dangerous_opt_ins() {
        let args = Args::try_parse_from(["abird-tunnel", "--allow-shell", "--no-sandbox"]).unwrap();
        assert!(Policy::from_args(&args, PathBuf::from("/workspace")).is_err());
    }

    #[test]
    fn corrected_network_dangerous_alias_is_accepted() {
        let args = Args::try_parse_from([
            "abird-tunnel",
            "--allow-shell",
            "--no-sandbox",
            "--allow-rw-all-dangerous",
            "--allow-network-dangerous",
        ])
        .unwrap();
        let policy = Policy::from_args(&args, PathBuf::from("/workspace")).unwrap();
        assert!(policy.allow_network);
    }

    #[test]
    fn network_flag_requires_shell() {
        let args = Args::try_parse_from(["abird-tunnel", "--allow-network"]).unwrap();
        assert!(Policy::from_args(&args, PathBuf::from("/workspace")).is_err());
    }

    #[test]
    fn no_sandbox_requires_shell() {
        let args = Args::try_parse_from(["abird-tunnel", "--no-sandbox"]).unwrap();
        assert!(Policy::from_args(&args, PathBuf::from("/workspace")).is_err());
    }

    #[test]
    fn ngrok_requires_http() {
        assert!(Args::try_parse_from(["abird-tunnel", "--ngrok"]).is_err());
        let args = Args::try_parse_from(["abird-tunnel", "--http", "--ngrok"]).unwrap();
        assert!(args.http);
        assert!(args.ngrok);
    }

    #[test]
    fn setup_is_not_mixed_with_protocol_stdio_or_http() {
        assert!(Args::try_parse_from(["abird-tunnel", "--setup", "--stdio"]).is_err());
        assert!(Args::try_parse_from(["abird-tunnel", "--setup", "--http"]).is_err());
    }
}
