mod controls;
mod logging;
mod mcp;
mod oauth;
mod setup;
mod transports;

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow, bail};
use clap::{
    ArgAction, ColorChoice, CommandFactory, FromArgMatches, Parser, Subcommand,
    builder::styling::{AnsiColor, Styles},
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::{
    controls::{RuntimeControl, RuntimeControls, StdioInterruptGuard},
    logging::{ColorMode, LogConfig},
    mcp::{AccessSpec, LocalMachine, MachineConfig, SandboxCacheMount},
    setup::{
        PermissionConfig, ProfileBool, ProfileRuleKind, TransportConfig, create_profile,
        delete_profile, edit_profile, list_profiles, load_or_setup, mutate_profile_rule,
        set_profile_bool, show_profile, validate_ngrok_domain,
    },
    transports::ActiveTransports,
};

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Cyan.on_default().bold())
    .usage(AnsiColor::Cyan.on_default().bold())
    .literal(AnsiColor::Green.on_default().bold())
    .placeholder(AnsiColor::Yellow.on_default());

#[derive(Debug, Parser)]
#[command(
    name = "dotlink",
    version = concat!("v", env!("CARGO_PKG_VERSION")),
    about = "Connect ChatGPT and other MCP clients to permission-scoped files and tools on this machine",
    args_conflicts_with_subcommands = true,
    subcommand_help_heading = "Management commands",
    after_help = "Quick start:\n  dotlink --setup\n  dotlink --allow-rw --allow-shell\n  dotlink profile --help\n  dotlink oauth --help",
    styles = HELP_STYLES
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
            "ngrok_domain",
            "no_ngrok_domain",
            "oauth",
            "no_oauth",
            "public_url",
            "allow_public_no_auth",
            "ephemeral_url",
            "http_ephemeral_url",
            "ngrok_ephemeral_url"
        ]
    )]
    #[arg(help_heading = "Setup & profiles")]
    setup: bool,

    /// Select a named config profile (config.<profile>.jsonc).
    #[arg(short = 'p', long, value_name = "NAME")]
    #[arg(help_heading = "Setup & profiles")]
    profile: Option<String>,

    /// Add the stdio MCP server for this run.
    #[arg(long, conflicts_with = "no_stdio")]
    #[arg(help_heading = "Transports")]
    stdio: bool,

    /// Disable stdio even if enabled in the selected profile.
    #[arg(long, conflicts_with = "stdio")]
    #[arg(help_heading = "Transports")]
    no_stdio: bool,

    /// Add the HTTP MCP server for this run.
    #[arg(long, conflicts_with = "no_http")]
    #[arg(help_heading = "Transports")]
    http: bool,

    /// Disable HTTP (and ngrok) even if enabled in the selected profile.
    #[arg(long, conflicts_with = "http")]
    #[arg(help_heading = "Transports")]
    no_http: bool,

    /// Override the configured HTTP listen address for this run.
    #[arg(long, value_name = "ADDR")]
    #[arg(help_heading = "Transports")]
    http_bind: Option<SocketAddr>,

    /// Publish the effective HTTP MCP server through ngrok for this run.
    #[arg(long, conflicts_with = "no_ngrok")]
    #[arg(help_heading = "Remote HTTP & OAuth")]
    ngrok: bool,

    /// Disable ngrok even if enabled in the selected profile.
    #[arg(long, conflicts_with = "ngrok")]
    #[arg(help_heading = "Remote HTTP & OAuth")]
    no_ngrok: bool,

    /// Override the ngrok domain for this run. The domain must already be available to the ngrok account.
    #[arg(long, value_name = "DOMAIN", conflicts_with_all = ["no_ngrok", "no_ngrok_domain"])]
    #[arg(help_heading = "Remote HTTP & OAuth")]
    ngrok_domain: Option<String>,

    /// Ignore a persisted ngrok domain for this run and let ngrok choose one.
    #[arg(long, conflicts_with = "ngrok_domain")]
    #[arg(help_heading = "Remote HTTP & OAuth")]
    no_ngrok_domain: bool,

    /// Protect local/reverse-proxied HTTP with embedded OAuth. Public ngrok is protected independently.
    #[arg(long, conflicts_with = "no_oauth")]
    #[arg(help_heading = "Remote HTTP & OAuth")]
    oauth: bool,

    /// Disable profile OAuth for local HTTP. Public ngrok still requires OAuth.
    #[arg(long, conflicts_with = "oauth")]
    #[arg(help_heading = "Remote HTTP & OAuth")]
    no_oauth: bool,

    /// Canonical public OAuth origin for reverse-proxied HTTP (for example https://mcp.example.com).
    #[arg(long, value_name = "URL")]
    #[arg(help_heading = "Remote HTTP & OAuth")]
    public_url: Option<String>,

    /// Allow externally reachable HTTP/ngrok without OAuth for this run. This is intentionally unsafe.
    #[arg(long)]
    #[arg(help_heading = "Remote HTTP & OAuth")]
    allow_public_no_auth: bool,

    /// Use fresh hard-to-guess URL paths for both local HTTP and ngrok.
    #[arg(long)]
    #[arg(help_heading = "Remote HTTP & OAuth")]
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
    #[arg(help_heading = "Remote HTTP & OAuth")]
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
    #[arg(help_heading = "Remote HTTP & OAuth")]
    ngrok_ephemeral_url: Option<bool>,

    /// Add readable access. Bare --allow-read means the launch directory.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    #[arg(help_heading = "Filesystem access")]
    allow_read: Vec<PathBuf>,

    /// Add writable access. Bare --allow-write means read+write on the launch directory.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    #[arg(help_heading = "Filesystem access")]
    allow_write: Vec<PathBuf>,

    /// Add read+write access. Bare --allow-rw means the launch directory.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    #[arg(help_heading = "Filesystem access")]
    allow_rw: Vec<PathBuf>,

    /// Do not implicitly grant read access to the launch directory for this run.
    #[arg(long)]
    #[arg(help_heading = "Filesystem access")]
    no_default_allow: bool,

    /// Deny reads. Bare --deny-read means the launch directory. Denies override allows.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    #[arg(help_heading = "Filesystem access")]
    deny_read: Vec<PathBuf>,

    /// Deny writes. Bare --deny-write means the launch directory. Denies override allows.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    #[arg(help_heading = "Filesystem access")]
    deny_write: Vec<PathBuf>,

    /// Deny both reads and writes. Bare --deny-rw means the launch directory.
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = ".",
        require_equals = true
    )]
    #[arg(help_heading = "Filesystem access")]
    deny_rw: Vec<PathBuf>,

    /// Legacy synonym for --deny-rw=<PATH>.
    #[arg(long, value_name = "PATH")]
    #[arg(help_heading = "Filesystem access")]
    deny: Vec<PathBuf>,

    /// Enable the platform shell (Bash on Unix, PowerShell on Windows).
    #[arg(long)]
    #[arg(help_heading = "Shell & network")]
    allow_shell: bool,

    /// Disable shell even if config or another flag enables it.
    #[arg(long)]
    #[arg(help_heading = "Shell & network")]
    deny_shell: bool,

    /// Allow network access from a Bubblewrap-sandboxed shell.
    #[arg(long)]
    #[arg(help_heading = "Shell & network")]
    allow_network: bool,

    /// Deny shell network access. On Linux this forces sandboxing when needed.
    #[arg(long)]
    #[arg(help_heading = "Shell & network")]
    deny_network: bool,

    /// Disable the shell sandbox. Requires --allow-all.
    #[arg(long, requires = "allow_all")]
    #[arg(help_heading = "Shell & network")]
    no_sandbox: bool,

    /// Grant unrestricted filesystem, shell, and network access. Requires --no-sandbox.
    #[arg(long, requires = "no_sandbox")]
    #[arg(help_heading = "Shell & network")]
    allow_all: bool,

    /// Print the configured OpenAI Tunnel ID and exit.
    #[arg(long)]
    #[arg(help_heading = "Output & diagnostics")]
    print_id: bool,

    /// Suppress TOOL activity even when verbosity enables it.
    #[arg(short = 'q', long)]
    #[arg(help_heading = "Output & diagnostics")]
    quiet: bool,

    /// Increase logging verbosity: -v shows TOOL activity; -vv also shows REQ diagnostics.
    #[arg(short = 'v', long, action = ArgAction::Count)]
    #[arg(help_heading = "Output & diagnostics")]
    verbose: u8,

    /// Control ANSI colors in human-facing output, including help.
    #[arg(long, value_enum, default_value_t = ColorMode::Auto, global = true)]
    #[arg(help_heading = "Output & diagnostics")]
    color: ColorMode,

    /// List the MCP tools exposed under the effective policy and exit.
    #[arg(long)]
    #[arg(help_heading = "Output & diagnostics")]
    list_tools: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage persisted profiles, permissions, transports, and runtime defaults.
    #[command(
        subcommand_help_heading = "Profile commands",
        after_help = "Examples:\n  dotlink profile list\n  dotlink profile show work\n  dotlink profile allow work rw /shared\n  dotlink profile enable work shell"
    )]
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Inspect OAuth state and revoke approved remote clients or refresh grants.
    #[command(
        subcommand_help_heading = "OAuth commands",
        after_help = "Examples:\n  dotlink oauth status\n  dotlink oauth clients -p work\n  dotlink oauth revoke -p work <CLIENT_ID>\n  dotlink oauth revoke-all -p work"
    )]
    Oauth {
        #[command(subcommand)]
        command: OAuthCommand,
    },
}

#[derive(Debug, Subcommand)]
enum OAuthCommand {
    /// Show owner/client/refresh-grant OAuth state for a profile.
    Status {
        /// Profile name; use "default" for the unnamed default profile.
        #[arg(short = 'p', long, default_value = "default", value_name = "NAME")]
        profile: String,
    },
    /// List approved dynamically registered OAuth clients.
    Clients {
        /// Profile name; use "default" for the unnamed default profile.
        #[arg(short = 'p', long, default_value = "default", value_name = "NAME")]
        profile: String,
    },
    /// Revoke an approved DCR client and all of its persisted refresh grants.
    Revoke {
        /// Client ID shown by `dotlink oauth clients`.
        client_id: String,
        /// Profile name; use "default" for the unnamed default profile.
        #[arg(short = 'p', long, default_value = "default", value_name = "NAME")]
        profile: String,
    },
    /// Revoke every persisted OAuth refresh grant for a profile.
    RevokeAll {
        /// Profile name; use "default" for the unnamed default profile.
        #[arg(short = 'p', long, default_value = "default", value_name = "NAME")]
        profile: String,
    },
}

#[derive(Debug, Subcommand)]
enum ProfileCommand {
    /// List configured profiles.
    List,
    /// Show a profile without exposing secret key material.
    Show {
        /// Profile name; use "default" for the unnamed default profile.
        name: String,
    },
    /// Create a profile with the interactive setup editor.
    Create {
        /// New profile name.
        name: String,
    },
    /// Edit a profile using its current values as defaults.
    Edit {
        /// Profile name; use "default" for the unnamed default profile.
        name: String,
    },
    /// Delete a profile plus its saved Runtime key and OAuth state.
    Delete {
        /// Profile name; use "default" for the unnamed default profile.
        name: String,
    },
    /// Add a persisted filesystem allow rule.
    Allow {
        /// Profile name; use "default" for the unnamed default profile.
        name: String,
        /// Permission kind to allow.
        #[arg(value_enum, value_name = "KIND")]
        kind: ProfileRuleKind,
        /// Path to add; relative paths resolve from the launch directory.
        path: PathBuf,
    },
    /// Remove a persisted filesystem allow rule.
    RemoveAllow {
        /// Profile name; use "default" for the unnamed default profile.
        name: String,
        /// Permission kind to remove.
        #[arg(value_enum, value_name = "KIND")]
        kind: ProfileRuleKind,
        /// Path to remove.
        path: PathBuf,
    },
    /// Add a persisted filesystem deny rule. Denies override allows.
    Deny {
        /// Profile name; use "default" for the unnamed default profile.
        name: String,
        /// Permission kind to deny.
        #[arg(value_enum, value_name = "KIND")]
        kind: ProfileRuleKind,
        /// Path to deny; relative paths resolve from the launch directory.
        path: PathBuf,
    },
    /// Remove a persisted filesystem deny rule.
    RemoveDeny {
        /// Profile name; use "default" for the unnamed default profile.
        name: String,
        /// Permission kind to remove.
        #[arg(value_enum, value_name = "KIND")]
        kind: ProfileRuleKind,
        /// Path to remove.
        path: PathBuf,
    },
    /// Enable a persisted transport or capability setting.
    Enable {
        /// Profile name; use "default" for the unnamed default profile.
        name: String,
        /// Setting to enable.
        #[arg(value_enum, value_name = "SETTING")]
        setting: ProfileBool,
    },
    /// Disable a persisted transport or capability setting.
    Disable {
        /// Profile name; use "default" for the unnamed default profile.
        name: String,
        /// Setting to disable.
        #[arg(value_enum, value_name = "SETTING")]
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

        let effective_read = capability_available(&read_roots, &deny_read_roots, unrestricted_fs);
        let effective_write =
            capability_available(&write_roots, &deny_write_roots, unrestricted_fs);
        if allow_shell && !effective_read {
            bail!(
                "shell access requires at least one effectively readable directory after deny rules"
            );
        }
        if !effective_read && !effective_write && !allow_shell {
            bail!("no effective local capability remains after applying deny rules");
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
}

fn root_fully_denied(root: &Path, denies: &[PathBuf]) -> bool {
    denies.iter().any(|deny| root.starts_with(deny))
}

fn capability_available(grants: &[PathBuf], denies: &[PathBuf], unrestricted: bool) -> bool {
    if unrestricted && !root_fully_denied(Path::new("/"), denies) {
        return true;
    }
    grants.iter().any(|root| !root_fully_denied(root, denies))
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

fn help_color_choice<I>(args: I) -> ColorChoice
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else {
            continue;
        };
        if arg == "--" {
            break;
        }

        let value = if let Some(value) = arg.strip_prefix("--color=") {
            Some(value.to_owned())
        } else if arg == "--color" {
            args.next()
                .and_then(|value| value.to_str().map(str::to_owned))
        } else {
            None
        };

        match value.as_deref() {
            Some("always") => return ColorChoice::Always,
            Some("never") => return ColorChoice::Never,
            Some("auto") => return ColorChoice::Auto,
            _ => {}
        }
    }
    ColorChoice::Auto
}

fn parse_args() -> Args {
    let color = help_color_choice(std::env::args_os().skip(1));
    let matches = Args::command().color(color).get_matches();
    Args::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();
    let launch_dir = std::env::current_dir()?;

    let log = LogConfig::new(args.verbose, args.quiet, args.color);
    let setup_color = args.color.stdout_enabled();
    let default_filter = "dotlink=warn";
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_writer(std::io::stderr)
        .with_ansi(log.color_enabled())
        .init();

    if let Some(command) = &args.command {
        run_command(command, setup_color).await?;
        return Ok(());
    }

    let mut runtime_restarts = 0_u32;
    loop {
        match run_runtime(&args, &launch_dir, &log, setup_color).await {
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
                log.developer("runtime", format!("restart reason: {error:#}"));
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
        Command::Oauth { command } => match command {
            OAuthCommand::Status { profile } => {
                print!("{}", oauth::status(oauth_profile_target(profile))?);
            }
            OAuthCommand::Clients { profile } => {
                let clients = oauth::clients(oauth_profile_target(profile))?;
                if clients.is_empty() {
                    println!("No dynamically registered OAuth clients.");
                } else {
                    for client in clients {
                        println!("{client}");
                    }
                }
            }
            OAuthCommand::Revoke { profile, client_id } => {
                if oauth::revoke_client(oauth_profile_target(profile), client_id)? {
                    println!("OAuth client {client_id:?} revoked.");
                    println!(
                        "Restart dotlink to invalidate any short-lived access token immediately."
                    );
                } else {
                    bail!("OAuth client {client_id:?} was not found");
                }
            }
            OAuthCommand::RevokeAll { profile } => {
                let count = oauth::revoke_all(oauth_profile_target(profile))?;
                println!("Revoked {count} persisted OAuth refresh grant(s).");
                println!("Restart dotlink to invalidate any short-lived access token immediately.");
            }
        },
    }
    Ok(())
}

fn oauth_profile_target(name: &str) -> Option<&str> {
    if name.eq_ignore_ascii_case("default") {
        None
    } else {
        Some(name)
    }
}

async fn run_runtime(
    args: &Args,
    launch_dir: &Path,
    log: &LogConfig,
    setup_color: bool,
) -> Result<RuntimeOutcome> {
    let Some(setup) = load_or_setup(args.setup, args.profile.as_deref(), setup_color).await? else {
        return Ok(RuntimeOutcome::Exit);
    };

    if args.setup {
        return Ok(RuntimeOutcome::Exit);
    }

    let policy = Policy::from_args(args, launch_dir.to_path_buf(), &setup.config.permissions)?;

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

    let http_auth = resolve_http_auth(args, &setup.config.oauth, &local_transports, http_bind)?;
    let oauth_runtime = if http_auth.local_oauth || http_auth.ngrok_oauth {
        Some(oauth::Runtime::load(args.profile.as_deref())?)
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
            ngrok_domain: local_transports.ngrok_domain.clone(),
            http_ephemeral_url: local_transports.http_ephemeral_url,
            ngrok_ephemeral_url: local_transports.ngrok_ephemeral_url,
            oauth: oauth_runtime,
            local_oauth: http_auth.local_oauth,
            ngrok_oauth: http_auth.ngrok_oauth,
            oauth_public_url: http_auth.public_url,
            ngrok_no_auth: http_auth.ngrok_no_auth,
            log: log.clone(),
        }),
    };

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

    if args.list_tools {
        print_tools(&machine);
        return Ok(RuntimeOutcome::Exit);
    }

    if active_transports.is_empty() {
        bail!("no MCP transport is active; enable one in setup or add --stdio/--http for this run");
    }

    let cancellation = CancellationToken::new();
    let _stdio_interrupt = StdioInterruptGuard::start(active_transports.stdio);
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
        controls.shutdown();
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct EffectiveLocalTransports {
    stdio: bool,
    http: bool,
    ngrok: bool,
    ngrok_domain: Option<String>,
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
        || args.ngrok_domain.is_some()
        || args.no_ngrok_domain
        || args.oauth
        || args.no_oauth
        || args.public_url.is_some()
        || args.allow_public_no_auth
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
    if (args.ngrok_domain.is_some() || args.no_ngrok_domain) && !ngrok {
        bail!("ngrok domain overrides require ngrok to be enabled in the profile or with --ngrok");
    }

    let ngrok_domain = if ngrok {
        if args.no_ngrok_domain {
            None
        } else if let Some(domain) = args.ngrok_domain.as_deref() {
            Some(validate_ngrok_domain(domain)?)
        } else {
            defaults.ngrok_domain.clone()
        }
    } else {
        None
    };

    let (http_ephemeral_url, ngrok_ephemeral_url) = ephemeral_url_policy(args, defaults);

    Ok(EffectiveLocalTransports {
        stdio,
        http,
        ngrok,
        ngrok_domain,
        http_ephemeral_url,
        ngrok_ephemeral_url,
    })
}

#[derive(Debug, Clone)]
struct EffectiveHttpAuth {
    local_oauth: bool,
    ngrok_oauth: bool,
    public_url: Option<url::Url>,
    ngrok_no_auth: bool,
}

fn resolve_http_auth(
    args: &Args,
    defaults: &oauth::OAuthConfig,
    transports: &EffectiveLocalTransports,
    http_bind: Option<SocketAddr>,
) -> Result<EffectiveHttpAuth> {
    if !transports.http {
        if args.oauth || args.no_oauth || args.public_url.is_some() || args.allow_public_no_auth {
            bail!("OAuth/public HTTP options require the HTTP transport");
        }
        return Ok(EffectiveHttpAuth {
            local_oauth: false,
            ngrok_oauth: false,
            public_url: None,
            ngrok_no_auth: false,
        });
    }

    let bind = http_bind.ok_or_else(|| anyhow::anyhow!("HTTP bind is missing"))?;
    let non_loopback_http = !bind.ip().is_loopback();
    if args.allow_public_no_auth && !transports.ngrok && !non_loopback_http {
        bail!("--allow-public-no-auth is valid only for ngrok or a non-loopback HTTP listener");
    }

    let requested_local_oauth = (defaults.enabled || args.oauth) && !args.no_oauth;
    let local_oauth = if non_loopback_http && args.allow_public_no_auth {
        false
    } else {
        requested_local_oauth
    };
    let ngrok_no_auth = transports.ngrok && args.allow_public_no_auth;
    let ngrok_oauth = transports.ngrok && !ngrok_no_auth;

    if args.public_url.is_some() && !local_oauth {
        bail!("--public-url requires local HTTP OAuth to be enabled");
    }

    let public_url = if local_oauth {
        args.public_url
            .as_deref()
            .or(defaults.public_url.as_deref())
            .map(oauth::validate_public_base_url)
            .transpose()?
    } else {
        None
    };

    if non_loopback_http {
        if !local_oauth && !args.allow_public_no_auth {
            bail!(
                "non-loopback HTTP requires OAuth; use --oauth with --public-url=https://... or explicitly opt out with --allow-public-no-auth"
            );
        }
        if local_oauth {
            let public_url = public_url.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "OAuth on non-loopback HTTP requires --public-url=https://... so issuer/resource identity is explicit"
                )
            })?;
            if public_url.scheme() != "https" {
                bail!("OAuth on non-loopback HTTP requires an HTTPS --public-url");
            }
        }
    }

    Ok(EffectiveHttpAuth {
        local_oauth,
        ngrok_oauth,
        public_url,
        ngrok_no_auth,
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
        if http.bind.port() == 0 {
            eprintln!("• HTTP       dynamic port; exact URL will be printed after bind");
        } else if http.http_ephemeral_url {
            eprintln!("• HTTP       ephemeral URL; exact path will be printed after bind");
        } else {
            eprintln!("• HTTP       http://{}/mcp", http.bind);
        }
        if http.local_oauth {
            eprintln!("• OAuth      local HTTP protected");
        } else if !http.bind.ip().is_loopback() {
            eprintln!("• OAuth      WARNING: non-loopback HTTP is unauthenticated");
        }
        if http.ngrok {
            let domain = http.ngrok_domain.as_deref().unwrap_or("automatic hostname");
            if http.ngrok_ephemeral_url {
                eprintln!("• ngrok      {domain}; OAuth resource path is ephemeral");
            } else {
                eprintln!("• ngrok      {domain}; public URL will be printed after connection");
            }
            if http.ngrok_no_auth {
                eprintln!("• OAuth      WARNING: public ngrok authentication disabled");
            } else if http.ngrok_oauth {
                eprintln!("• OAuth      public ngrok protected");
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

fn print_tools(machine: &LocalMachine) {
    println!("abird dotlink tools");
    println!("────────────────────────────────────────────────────────");
    for (name, title) in machine.visible_tool_descriptions() {
        println!("• {name:<16} {title}");
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

    fn loopback_http_bind() -> Option<SocketAddr> {
        Some("127.0.0.1:3000".parse().unwrap())
    }

    #[test]
    fn help_is_grouped_and_styled() {
        let mut plain = Vec::new();
        Args::command()
            .color(ColorChoice::Never)
            .write_long_help(&mut plain)
            .unwrap();
        let plain = String::from_utf8(plain).unwrap();
        for heading in [
            "Management commands",
            "Setup & profiles",
            "Transports",
            "Remote HTTP & OAuth",
            "Filesystem access",
            "Shell & network",
            "Output & diagnostics",
        ] {
            assert!(
                plain.contains(&format!("{heading}:")),
                "missing {heading:?}"
            );
        }
    }

    #[test]
    fn cli_version_uses_v_prefix() {
        assert_eq!(
            Args::command().get_version(),
            Some(concat!("v", env!("CARGO_PKG_VERSION")))
        );
    }

    #[test]
    fn management_help_documents_current_commands() {
        let mut command = Args::command();

        let profile = command.find_subcommand_mut("profile").unwrap();
        let mut profile_help = Vec::new();
        profile.write_long_help(&mut profile_help).unwrap();
        let profile_help = String::from_utf8(profile_help).unwrap();
        assert!(profile_help.contains("Profile commands:"));
        assert!(profile_help.contains("dotlink profile allow work rw /shared"));
        assert!(
            profile_help.contains("Delete a profile plus its saved Runtime key and OAuth state")
        );

        let oauth = command.find_subcommand_mut("oauth").unwrap();
        let mut oauth_help = Vec::new();
        oauth.write_long_help(&mut oauth_help).unwrap();
        let oauth_help = String::from_utf8(oauth_help).unwrap();
        assert!(oauth_help.contains("OAuth commands:"));
        assert!(oauth_help.contains("dotlink oauth revoke -p work <CLIENT_ID>"));
    }

    #[test]
    fn help_color_follows_global_color_flag() {
        use std::ffi::OsString;

        assert_eq!(
            help_color_choice(["--color=always", "--help"].map(OsString::from)),
            ColorChoice::Always
        );
        assert_eq!(
            help_color_choice(["--color", "never", "profile", "--help"].map(OsString::from)),
            ColorChoice::Never
        );
        assert_eq!(
            help_color_choice(["--help"].map(OsString::from)),
            ColorChoice::Auto
        );
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
    fn deny_can_remove_the_last_effective_capability() {
        let args = Args::try_parse_from(["dotlink", "--deny-read"]).unwrap();
        let error =
            Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).unwrap_err();
        assert!(error.to_string().contains("no effective local capability"));
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
        assert!(error.to_string().contains(
            "shell access requires at least one effectively readable directory after deny rules"
        ));

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
    fn public_ngrok_is_oauth_protected_by_default() {
        let args = Args::try_parse_from(["dotlink"]).unwrap();
        let defaults = TransportConfig {
            openai: false,
            http: true,
            ngrok: true,
            ..TransportConfig::default()
        };
        let transports = resolve_local_transports(&args, &defaults).unwrap();
        let auth = resolve_http_auth(
            &args,
            &oauth::OAuthConfig::default(),
            &transports,
            loopback_http_bind(),
        )
        .unwrap();
        assert!(!auth.local_oauth);
        assert!(auth.ngrok_oauth);
        assert!(!auth.ngrok_no_auth);
    }

    #[test]
    fn public_ngrok_requires_explicit_unsafe_opt_out() {
        let args = Args::try_parse_from(["dotlink", "--allow-public-no-auth"]).unwrap();
        let defaults = TransportConfig {
            openai: false,
            http: true,
            ngrok: true,
            ..TransportConfig::default()
        };
        let transports = resolve_local_transports(&args, &defaults).unwrap();
        let auth = resolve_http_auth(
            &args,
            &oauth::OAuthConfig::default(),
            &transports,
            loopback_http_bind(),
        )
        .unwrap();
        assert!(!auth.ngrok_oauth);
        assert!(auth.ngrok_no_auth);
    }

    #[test]
    fn local_oauth_can_coexist_with_ngrok_no_auth_override() {
        let args = Args::try_parse_from(["dotlink", "--oauth", "--allow-public-no-auth"]).unwrap();
        let defaults = TransportConfig {
            openai: false,
            http: true,
            ngrok: true,
            ..TransportConfig::default()
        };
        let transports = resolve_local_transports(&args, &defaults).unwrap();
        let auth = resolve_http_auth(
            &args,
            &oauth::OAuthConfig::default(),
            &transports,
            loopback_http_bind(),
        )
        .unwrap();
        assert!(auth.local_oauth);
        assert!(!auth.ngrok_oauth);
        assert!(auth.ngrok_no_auth);
    }

    #[test]
    fn no_oauth_only_disables_local_http_auth() {
        let args = Args::try_parse_from(["dotlink", "--no-oauth"]).unwrap();
        let defaults = TransportConfig {
            openai: false,
            http: true,
            ngrok: true,
            ..TransportConfig::default()
        };
        let transports = resolve_local_transports(&args, &defaults).unwrap();
        let auth = resolve_http_auth(
            &args,
            &oauth::OAuthConfig {
                enabled: true,
                public_url: None,
            },
            &transports,
            loopback_http_bind(),
        )
        .unwrap();
        assert!(!auth.local_oauth);
        assert!(auth.ngrok_oauth);
    }

    #[test]
    fn public_url_requires_local_oauth() {
        let args = Args::try_parse_from([
            "dotlink",
            "--http",
            "--no-oauth",
            "--public-url=https://mcp.example.com",
        ])
        .unwrap();
        let transports = resolve_local_transports(&args, &TransportConfig::default()).unwrap();
        assert!(
            resolve_http_auth(
                &args,
                &oauth::OAuthConfig::default(),
                &transports,
                loopback_http_bind()
            )
            .is_err()
        );

        let args = Args::try_parse_from([
            "dotlink",
            "--http",
            "--oauth",
            "--public-url=https://mcp.example.com",
        ])
        .unwrap();
        let transports = resolve_local_transports(&args, &TransportConfig::default()).unwrap();
        let auth = resolve_http_auth(
            &args,
            &oauth::OAuthConfig::default(),
            &transports,
            loopback_http_bind(),
        )
        .unwrap();
        assert!(auth.local_oauth);
        assert_eq!(
            auth.public_url.unwrap().as_str(),
            "https://mcp.example.com/"
        );
    }

    #[test]
    fn non_loopback_http_requires_oauth_or_explicit_opt_out() {
        let bind = Some("0.0.0.0:3000".parse().unwrap());

        let args = Args::try_parse_from(["dotlink", "--http"]).unwrap();
        let transports = resolve_local_transports(&args, &TransportConfig::default()).unwrap();
        assert!(
            resolve_http_auth(&args, &oauth::OAuthConfig::default(), &transports, bind).is_err()
        );

        let args = Args::try_parse_from(["dotlink", "--http", "--allow-public-no-auth"]).unwrap();
        let transports = resolve_local_transports(&args, &TransportConfig::default()).unwrap();
        let auth =
            resolve_http_auth(&args, &oauth::OAuthConfig::default(), &transports, bind).unwrap();
        assert!(!auth.local_oauth);

        let auth = resolve_http_auth(
            &args,
            &oauth::OAuthConfig {
                enabled: true,
                public_url: Some("https://mcp.example.com".to_owned()),
            },
            &transports,
            bind,
        )
        .unwrap();
        assert!(!auth.local_oauth);

        let args = Args::try_parse_from(["dotlink", "--http", "--oauth"]).unwrap();
        let transports = resolve_local_transports(&args, &TransportConfig::default()).unwrap();
        assert!(
            resolve_http_auth(&args, &oauth::OAuthConfig::default(), &transports, bind).is_err()
        );

        let args = Args::try_parse_from([
            "dotlink",
            "--http",
            "--oauth",
            "--public-url=https://mcp.example.com",
        ])
        .unwrap();
        let transports = resolve_local_transports(&args, &TransportConfig::default()).unwrap();
        let auth =
            resolve_http_auth(&args, &oauth::OAuthConfig::default(), &transports, bind).unwrap();
        assert!(auth.local_oauth);
    }

    #[test]
    fn ngrok_domain_uses_profile_or_cli_override() {
        let defaults = TransportConfig {
            openai: false,
            http: true,
            ngrok: true,
            ngrok_domain: Some("profile.ngrok.app".to_owned()),
            ..TransportConfig::default()
        };

        let args = Args::try_parse_from(["dotlink"]).unwrap();
        let transports = resolve_local_transports(&args, &defaults).unwrap();
        assert_eq!(
            transports.ngrok_domain.as_deref(),
            Some("profile.ngrok.app")
        );

        let args = Args::try_parse_from(["dotlink", "--ngrok-domain=CLI.NGROK.APP"]).unwrap();
        let transports = resolve_local_transports(&args, &defaults).unwrap();
        assert_eq!(transports.ngrok_domain.as_deref(), Some("cli.ngrok.app"));

        let args = Args::try_parse_from(["dotlink", "--no-ngrok-domain"]).unwrap();
        let transports = resolve_local_transports(&args, &defaults).unwrap();
        assert!(transports.ngrok_domain.is_none());
    }

    #[test]
    fn ngrok_domain_override_requires_effective_ngrok() {
        let args =
            Args::try_parse_from(["dotlink", "--http", "--ngrok-domain=x.ngrok.app"]).unwrap();
        assert!(
            resolve_local_transports(
                &args,
                &TransportConfig {
                    openai: false,
                    ..TransportConfig::default()
                }
            )
            .is_err()
        );
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
        let quiet = Args::try_parse_from(["dotlink", "-q"]).unwrap();
        assert!(quiet.quiet);
        assert_eq!(quiet.verbose, 0);
        assert!(!quiet.setup);

        let verbose = Args::try_parse_from(["dotlink", "-v"]).unwrap();
        assert_eq!(verbose.verbose, 1);
        assert!(!verbose.quiet);

        let developer = Args::try_parse_from(["dotlink", "-vv"]).unwrap();
        assert_eq!(developer.verbose, 2);

        let both = Args::try_parse_from(["dotlink", "--quiet", "-vv", "--color=never"]).unwrap();
        assert!(both.quiet);
        assert_eq!(both.verbose, 2);
        assert_eq!(both.color, ColorMode::Never);
    }

    #[test]
    fn setup_uses_uppercase_s_short_flag() {
        let args = Args::try_parse_from(["dotlink", "-S"]).unwrap();
        assert!(args.setup);
        assert!(!args.quiet);
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
    fn profile_commands_reject_runtime_flags_but_allow_global_color() {
        assert!(Args::try_parse_from(["dotlink", "--allow-shell", "profile", "list"]).is_err());
        assert!(Args::try_parse_from(["dotlink", "-p", "work", "profile", "list"]).is_err());
        assert!(Args::try_parse_from(["dotlink", "--setup", "profile", "list"]).is_err());

        let args = Args::try_parse_from(["dotlink", "profile", "list", "--color=never"]).unwrap();
        assert_eq!(args.color, ColorMode::Never);
    }

    #[test]
    fn setup_is_not_mixed_with_protocol_stdio_or_http() {
        assert!(Args::try_parse_from(["dotlink", "--setup", "--stdio"]).is_err());
        assert!(Args::try_parse_from(["dotlink", "--setup", "--http"]).is_err());
    }
}
