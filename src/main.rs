mod mcp;
mod setup;
mod tunnel;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::{
    mcp::{LocalMachine, MachineConfig},
    setup::load_or_setup,
    tunnel::{EmbeddedMcp, TunnelClient},
};

#[derive(Debug, Parser)]
#[command(
    name = "abird-tunnel",
    version,
    about = "Connect ChatGPT to this machine over OpenAI Secure MCP Tunnel"
)]
struct Args {
    /// Re-run the interactive first-use setup and replace the saved configuration.
    #[arg(long)]
    setup: bool,

    /// Filesystem workspace root for this run. Defaults to the directory where abird-tunnel is launched.
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// Disable shell_exec for this run only.
    #[arg(long)]
    no_shell: bool,

    /// Print the saved tunnel id and exit.
    #[arg(long)]
    print_id: bool,

    /// Show concise incoming MCP requests and tool calls.
    #[arg(short, long)]
    verbose: bool,

    /// List the MCP tools exposed by abird-tunnel and exit.
    #[arg(long)]
    list_tools: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.list_tools {
        print_tools();
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
    // Capture the launch directory before any setup work so the default workspace is
    // exactly the directory from which the user invoked `abird-tunnel`.
    let launch_cwd = std::env::current_dir()?;
    let setup = load_or_setup(args.setup).await?;

    if args.print_id {
        println!("{}", setup.config.tunnel_id);
        return Ok(());
    }

    let workspace_root = args.cwd.unwrap_or(launch_cwd);
    let allow_shell = setup.config.allow_shell && !args.no_shell;

    let machine = LocalMachine::new(MachineConfig {
        workspace_root,
        allow_shell,
        shell: setup.config.shell.clone(),
        max_shell_timeout_secs: setup.config.max_shell_timeout_secs,
        max_output_bytes: setup.config.max_output_bytes,
        max_read_bytes: setup.config.max_read_bytes,
        max_write_bytes: setup.config.max_write_bytes,
        protected_paths: setup.protected_paths.clone(),
    })
    .await?;

    let cancellation = CancellationToken::new();
    let embedded_mcp = EmbeddedMcp::new(machine.clone(), cancellation.child_token());
    let tunnel = TunnelClient::new(
        setup.config.base_url.clone(),
        setup.config.tunnel_id.clone(),
        setup.config.runtime_api_key.clone(),
        setup.config.organization_id.clone(),
        setup.new_tunnel,
        args.verbose,
        cancellation.child_token(),
    )?;

    print_banner(
        &setup.config.tunnel_id,
        machine.workspace_root(),
        allow_shell,
        setup.new_tunnel,
        args.verbose,
    );

    tokio::select! {
        result = tunnel.run(embedded_mcp) => result?,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            cancellation.cancel();
            eprintln!("\nabird-tunnel stopped.");
        }
    }

    Ok(())
}

fn print_banner(
    tunnel_id: &str,
    workspace_root: &std::path::Path,
    shell: bool,
    new_tunnel: bool,
    verbose: bool,
) {
    println!();
    println!("abird-tunnel {}", env!("CARGO_PKG_VERSION"));
    println!("────────────────────────────────────────────────────────");
    println!("• Tunnel     {tunnel_id}");
    println!("• Workspace  {}", workspace_root.display());
    println!(
        "• Bash       {}",
        if shell { "enabled" } else { "disabled" }
    );
    println!(
        "• Status     {}",
        if new_tunnel {
            "new tunnel; connecting…"
        } else {
            "connecting…"
        }
    );
    if verbose {
        println!("• Verbose    enabled");
    }
    println!();
    println!("ChatGPT → Plugins → Tunnel → paste the Tunnel ID above.");
    println!("Ctrl-C to stop.");
    println!();
}

fn print_tools() {
    let mut tools = LocalMachine::tool_router().list_all();
    tools.sort_by(|a, b| a.name.cmp(&b.name));

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

    #[test]
    fn parses_cwd_equals_form() {
        let args = Args::try_parse_from(["abird-tunnel", "--cwd=/tmp/project"]).unwrap();
        assert_eq!(args.cwd, Some(PathBuf::from("/tmp/project")));
    }

    #[test]
    fn cwd_is_optional() {
        let args = Args::try_parse_from(["abird-tunnel"]).unwrap();
        assert!(args.cwd.is_none());
    }

    #[test]
    fn parses_verbose_and_list_tools() {
        let verbose = Args::try_parse_from(["abird-tunnel", "-v"]).unwrap();
        assert!(verbose.verbose);

        let list = Args::try_parse_from(["abird-tunnel", "--list-tools"]).unwrap();
        assert!(list.list_tools);
    }

    #[test]
    fn tool_list_comes_from_actual_router() {
        let mut names: Vec<_> = LocalMachine::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "fs_list",
                "fs_mkdir",
                "fs_read_text",
                "fs_remove",
                "fs_stat",
                "fs_write_text",
                "machine_info",
                "shell_exec",
            ]
        );
    }
}
