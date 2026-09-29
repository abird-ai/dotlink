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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("abird_tunnel=info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(true)
        .init();

    let args = Args::parse();
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
        cancellation.child_token(),
    )?;

    print_banner(
        &setup.config.tunnel_id,
        machine.workspace_root(),
        allow_shell,
        setup.new_tunnel,
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

fn print_banner(tunnel_id: &str, workspace_root: &std::path::Path, shell: bool, new_tunnel: bool) {
    println!();
    println!("abird-tunnel {}", env!("CARGO_PKG_VERSION"));
    println!("────────────────────────────────────────────────────────");
    println!("Tunnel ID   {tunnel_id}");
    println!("Workspace   {}", workspace_root.display());
    println!("Bash        {}", if shell { "enabled" } else { "disabled" });
    if new_tunnel {
        println!("Status      tunnel created; connecting…");
    } else {
        println!("Status      connecting…");
    }
    println!("────────────────────────────────────────────────────────");
    println!("Paste the Tunnel ID into the ChatGPT plugin's Tunnel connection.");
    println!("Keep this process running. Press Ctrl-C to stop.");
    println!();
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
}
