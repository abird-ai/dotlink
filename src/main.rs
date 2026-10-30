mod backoff;
mod blocking;
mod bounded;
mod controls;
mod credentials;
#[cfg(target_os = "linux")]
mod hardware;
#[cfg(target_os = "linux")]
mod host_services;
mod jobs;
mod limits;
mod logging;
mod mcp;
mod mutation;
mod oauth;
mod private_state;
mod recovery;
mod requester;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod secure_fs;
mod setup;
mod shell_process;
mod shutdown;
mod staging;
mod supervisor;
mod transports;
#[cfg(windows)]
mod windows_fs;
#[cfg(windows)]
mod windows_private;
mod xdg;

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{
    ArgAction, ColorChoice, CommandFactory, FromArgMatches, Parser, Subcommand,
    builder::styling::{AnsiColor, Styles},
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{
    EnvFilter, Layer,
    filter::{FilterExt, filter_fn},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

use crate::{
    backoff::Backoff,
    controls::{RuntimeControl, RuntimeControls, StdioInterruptGuard},
    logging::{ColorMode, LogConfig},
    mcp::{AccessSpec, LocalMachine, MachineConfig, PreparedMachine, SandboxCacheMount},
    recovery::Recovery,
    setup::{
        CacheKind, CacheMode, GpuVendor, PermissionConfig, ProfileBool, ProfileRuleKind,
        SetupResult, TransportConfig, cache_grants_for_cli, create_profile, delete_profile,
        edit_profile, list_profiles, load_or_setup, mutate_profile_rule, set_profile_bool,
        show_profile, validate_ngrok_domain,
    },
    shutdown::Shutdown,
    supervisor::Supervisor,
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
    after_help = "Quick start:\n  dotlink --setup\n  dotlink --allow-rw --allow-shell\n  dotlink --unrestricted\n  dotlink profile --help\n  dotlink oauth --help",
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

    /// Expose only the host Nix daemon Unix socket to sandboxed shell tools.
    #[arg(long, conflicts_with = "setup", help_heading = "Shell integrations")]
    allow_nix_daemon: bool,

    /// Share the user's Nix config file read-only into the private sandbox HOME.
    #[arg(long, conflicts_with = "setup", help_heading = "Shell integrations")]
    allow_nix_config: bool,

    /// Expose the SSH-agent socket (not keys); permits signing and authentication.
    #[arg(long, conflicts_with = "setup", help_heading = "Shell integrations")]
    allow_ssh_agent: bool,

    /// Share ~/.gitconfig and XDG Git config read-only, when present.
    #[arg(long, conflicts_with = "setup", help_heading = "Shell integrations")]
    allow_git_config: bool,

    /// Expose the host KVM acceleration device /dev/kvm to Bubblewrap.
    #[arg(long, conflicts_with = "setup", help_heading = "Hardware acceleration")]
    allow_kvm: bool,

    /// Expose a vendor's GPU devices and driver runtime to Bubblewrap (repeatable).
    #[arg(long, value_enum, value_name = "VENDOR", action = ArgAction::Append,
        conflicts_with = "setup", help_heading = "Hardware acceleration")]
    allow_gpu: Vec<GpuVendor>,

    /// Allow specific detected developer caches into the shell sandbox.
    #[command(flatten)]
    cache_flags: CacheFlags,

    /// Disable the shell sandbox. Requires --allow-all or --unrestricted.
    #[arg(long)]
    #[arg(help_heading = "Shell & network")]
    no_sandbox: bool,

    /// Legacy two-flag full-host mode; pair with --no-sandbox.
    #[arg(long)]
    #[arg(help_heading = "Shell & network")]
    allow_all: bool,

    /// Full unrestricted host access: root RW filesystem, shell, network, and no sandbox.
    #[arg(long)]
    #[arg(help_heading = "Shell & network")]
    unrestricted: bool,

    /// Print the configured OpenAI Tunnel ID and exit.
    #[arg(long)]
    #[arg(help_heading = "Output & diagnostics")]
    print_id: bool,

    /// Execute one MCP tool call at a time instead of allowing concurrent calls.
    #[arg(short = 's', long)]
    #[arg(help_heading = "Output & diagnostics")]
    serialize: bool,

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

#[derive(Debug, Default, clap::Args)]
struct CacheFlags {
    /// Share discovered Cargo registry cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_cargo_registry: Option<CacheMode>,

    /// Share discovered Cargo Git cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_cargo_git: Option<CacheMode>,

    /// Share discovered npm cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_npm: Option<CacheMode>,

    /// Share discovered pnpm cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_pnpm: Option<CacheMode>,

    /// Share discovered Yarn cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_yarn: Option<CacheMode>,

    /// Share discovered pip cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_pip: Option<CacheMode>,

    /// Share discovered uv cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_uv: Option<CacheMode>,

    /// Share discovered Go module cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_go_mod: Option<CacheMode>,

    /// Share discovered Go build cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_go_build: Option<CacheMode>,

    /// Share discovered Maven cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_maven: Option<CacheMode>,

    /// Share discovered Gradle cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_gradle: Option<CacheMode>,

    /// Share discovered sccache cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_sccache: Option<CacheMode>,

    /// Share discovered ccache cache; bare flag means read-only, =rw enables writes.
    #[arg(long, value_enum, value_name = "MODE", num_args=0..=1, default_missing_value="ro", require_equals=true, conflicts_with="setup", help_heading="Developer caches")]
    allow_cache_ccache: Option<CacheMode>,
}

impl CacheFlags {
    fn requested(&self) -> Vec<(CacheKind, CacheMode)> {
        let mut selected = Vec::new();
        if let Some(mode) = self.allow_cache_cargo_registry {
            selected.push((CacheKind::CargoRegistry, mode));
        }
        if let Some(mode) = self.allow_cache_cargo_git {
            selected.push((CacheKind::CargoGit, mode));
        }
        if let Some(mode) = self.allow_cache_npm {
            selected.push((CacheKind::Npm, mode));
        }
        if let Some(mode) = self.allow_cache_pnpm {
            selected.push((CacheKind::Pnpm, mode));
        }
        if let Some(mode) = self.allow_cache_yarn {
            selected.push((CacheKind::Yarn, mode));
        }
        if let Some(mode) = self.allow_cache_pip {
            selected.push((CacheKind::Pip, mode));
        }
        if let Some(mode) = self.allow_cache_uv {
            selected.push((CacheKind::Uv, mode));
        }
        if let Some(mode) = self.allow_cache_go_mod {
            selected.push((CacheKind::GoMod, mode));
        }
        if let Some(mode) = self.allow_cache_go_build {
            selected.push((CacheKind::GoBuild, mode));
        }
        if let Some(mode) = self.allow_cache_maven {
            selected.push((CacheKind::Maven, mode));
        }
        if let Some(mode) = self.allow_cache_gradle {
            selected.push((CacheKind::Gradle, mode));
        }
        if let Some(mode) = self.allow_cache_sccache {
            selected.push((CacheKind::Sccache, mode));
        }
        if let Some(mode) = self.allow_cache_ccache {
            selected.push((CacheKind::Ccache, mode));
        }
        selected
    }
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
    #[cfg(target_os = "linux")]
    host_access: host_services::HostAccess,
    #[cfg(target_os = "linux")]
    allow_kvm: bool,
    #[cfg(target_os = "linux")]
    allow_gpu: Vec<GpuVendor>,
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
        let unrestricted = args.unrestricted || args.allow_all;
        let no_sandbox = args.unrestricted || args.no_sandbox;

        if args.allow_all != args.no_sandbox && !args.unrestricted {
            bail!(
                "--allow-all and --no-sandbox must be used together; use --unrestricted for the one-flag full-host mode"
            );
        }
        if unrestricted && has_filesystem_denies {
            bail!("--unrestricted/--allow-all cannot be combined with filesystem deny rules");
        }
        if unrestricted && (args.deny_shell || args.deny_network) {
            bail!(
                "--unrestricted/--allow-all cannot be combined with --deny-shell or --deny-network"
            );
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

        if unrestricted {
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

        let requested_shell = defaults.allow_shell || args.allow_shell || unrestricted;
        if args.allow_network && !requested_shell && !args.deny_shell {
            bail!("--allow-network requires shell permission");
        }
        let allow_shell = requested_shell && !args.deny_shell;
        let sandbox_shell = cfg!(target_os = "linux") && allow_shell && !no_sandbox;

        #[cfg(not(target_os = "linux"))]
        if allow_shell && !no_sandbox {
            bail!(
                "sandboxed shell execution is unavailable on this OS; use --unrestricted, or use Linux/WSL2 with Bubblewrap"
            );
        }

        let host_integration_requested = args.allow_nix_daemon
            || args.allow_nix_config
            || args.allow_ssh_agent
            || args.allow_git_config
            || defaults.allow_nix_daemon
            || defaults.allow_nix_config
            || defaults.allow_ssh_agent
            || defaults.allow_git_config
            || args.allow_kvm
            || !args.allow_gpu.is_empty()
            || defaults.allow_kvm
            || !defaults.allow_gpu.is_empty();
        if (host_integration_requested || !args.cache_flags.requested().is_empty())
            && !sandbox_shell
            && !unrestricted
        {
            bail!(
                "host integrations and --allow-cache-* require a Linux Bubblewrap shell; enable --allow-shell without --no-sandbox"
            );
        }

        #[cfg(target_os = "linux")]
        // Full-host mode already uses the process's host environment/devices.
        // Sandbox-specific mounts and discovery are redundant, including saved
        // profile defaults; do not try to initialize them without Bubblewrap.
        let host_access = if sandbox_shell {
            host_services::HostAccess {
                nix_daemon: args.allow_nix_daemon || defaults.allow_nix_daemon,
                nix_config: args.allow_nix_config || defaults.allow_nix_config,
                ssh_agent: args.allow_ssh_agent || defaults.allow_ssh_agent,
                git_config: args.allow_git_config || defaults.allow_git_config,
            }
        } else {
            host_services::HostAccess::default()
        };
        #[cfg(target_os = "linux")]
        let allow_kvm = sandbox_shell && (args.allow_kvm || defaults.allow_kvm);
        #[cfg(target_os = "linux")]
        let allow_gpu = if sandbox_shell {
            args.allow_gpu
                .iter()
                .copied()
                .chain(defaults.allow_gpu.iter().copied())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let requested_sandbox_network =
            defaults.allow_network || args.allow_network || unrestricted;

        let allow_network = if !allow_shell || args.deny_network {
            false
        } else if sandbox_shell {
            requested_sandbox_network
        } else {
            // Unsandboxed mode is reached through --unrestricted or the legacy --allow-all --no-sandbox pair.
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
            #[cfg(target_os = "linux")]
            host_access,
            #[cfg(target_os = "linux")]
            allow_kvm,
            #[cfg(target_os = "linux")]
            allow_gpu,
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

/// Runtime restart schedule: 1 s doubling to a 30 s cap, no jitter.
const RUNTIME_RESTART_BACKOFF: Backoff = Backoff {
    base: Duration::from_secs(1),
    cap: Duration::from_secs(30),
    jitter: Duration::ZERO,
};

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

fn dependency_trace_is_safe(target: &str) -> bool {
    // rmcp debug/trace events render complete requests and responses, including
    // tool payloads. Keep RUST_LOG useful for dotlink without bypassing the
    // application's payload-redaction boundary.
    target != "rmcp" && !target.starts_with("rmcp::")
}

fn main() -> ExitCode {
    // SAFETY: no other thread exists before the async runtime starts.
    let supervisor = unsafe { Supervisor::take_from_env() };
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")
        .and_then(|runtime| runtime.block_on(run_main(&supervisor)));
    let Err(error) = result else {
        return ExitCode::SUCCESS;
    };

    let recovery = Recovery::of(&error);
    if recovery == Recovery::Unconfigured {
        // Setup is pending, not broken: report it as a notice and leave the
        // failure hint to the classes that need an operator to fix something.
        eprintln!("{error:?}");
        if let Some(hint) = supervisor.unconfigured_hint() {
            eprintln!("{hint}");
        }
    } else {
        eprintln!("Error: {error:?}");
        if recovery == Recovery::Operator
            && let Some(hint) = supervisor.operator_hint()
        {
            eprintln!("{hint}");
        }
    }
    recovery.exit_code()
}

async fn run_main(supervisor: &Supervisor) -> Result<()> {
    let args = parse_args();
    let launch_dir = std::env::current_dir()?;

    let log = LogConfig::new(args.verbose, args.quiet, args.color);
    let setup_color = args.color.stdout_enabled();
    let default_filter = "dotlink=warn";
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_filter))
        .and(filter_fn(|metadata| {
            dependency_trace_is_safe(metadata.target())
        }));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(log.color_enabled())
                .with_filter(filter),
        )
        .init();

    if let Some(command) = &args.command {
        return run_command(command, setup_color).await;
    }
    if args.setup || args.print_id || args.list_tools {
        return run_one_shot(&args, &launch_dir, setup_color).await;
    }

    supervisor.schedule_restart_reset();

    // Both outlive any one runtime: a signal or unresolved OpenAI request
    // replay/rejection state must survive an in-process restart.
    let mut shutdown = Shutdown::default();
    let openai_state = transports::openai::ProcessState::default();
    // Long-running jobs belong to the dotlink process session, not one OpenAI
    // transport runtime. Automatic tunnel recovery therefore preserves them.
    let mut jobs: Option<jobs::JobManager> = None;
    let mut runtime_restarts = 0_u32;
    loop {
        match run_runtime(
            &args,
            &launch_dir,
            &log,
            setup_color,
            &mut shutdown,
            &openai_state,
            &mut jobs,
        )
        .await
        {
            Ok(RuntimeOutcome::Exit) => {
                shutdown_job_session(&mut jobs).await?;
                return Ok(());
            }
            Ok(RuntimeOutcome::Restart) => {
                runtime_restarts = 0;
                // Ctrl+R is a policy reload, not transport recovery. Drop the
                // old session so revoked shell/filesystem authority also stops
                // work that was launched under it.
                shutdown_job_session(&mut jobs).await?;
                log.notice("runtime", "restarting");
            }
            Err(error) => {
                let Recovery::Runtime { reset_backoff } = Recovery::of(&error) else {
                    shutdown_job_session(&mut jobs).await?;
                    return Err(error);
                };

                runtime_restarts = next_runtime_restart_attempt(runtime_restarts, reset_backoff);
                let delay = RUNTIME_RESTART_BACKOFF.delay(runtime_restarts);

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

                let shutdown_requested = shutdown.requested()?;
                tokio::select! {
                    biased;
                    _ = shutdown_requested => {
                        shutdown_job_session(&mut jobs).await?;
                        eprintln!("\ndotlink stopped.");
                        return Ok(());
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

async fn shutdown_job_session(jobs: &mut Option<jobs::JobManager>) -> Result<()> {
    if let Some(manager) = jobs.take() {
        manager.shutdown().await?;
    }
    Ok(())
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

/// Inspection and setup modes never start transports, so their failures are
/// ordinary command failures.
async fn run_one_shot(args: &Args, launch_dir: &Path, setup_color: bool) -> Result<()> {
    if args.setup {
        load_or_setup(true, args.profile.as_deref(), setup_color).await?;
    } else if args.print_id {
        let tunnel_id = setup::configured_tunnel_id(args.profile.as_deref())?
            .ok_or_else(|| anyhow::anyhow!("OpenAI transport is not configured"))?;
        println!("{tunnel_id}");
    } else {
        let defaults = setup::inspection_permissions(args.profile.as_deref())?;
        let policy = Policy::from_args(args, launch_dir.to_path_buf(), &defaults)?;
        let descriptions = LocalMachine::inspect_tool_descriptions(
            AccessSpec {
                base_dir: policy.base_dir,
                read_roots: policy.read_roots,
                write_roots: policy.write_roots,
                deny_read_roots: policy.deny_read_roots,
                deny_write_roots: policy.deny_write_roots,
                unrestricted_fs: policy.unrestricted_fs,
            },
            policy.allow_shell,
        )
        .await?;
        print_tools(descriptions);
    }
    Ok(())
}

/// The runtime as determined by profile, environment, credentials and flags.
struct RuntimePlan {
    setup: SetupResult,
    policy: Policy,
    transports: ActiveTransports,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RuntimeAuthorityIntent {
    execution: [u8; 32],
    job_session: [u8; 32],
}

fn openai_job_namespace_value(transports: &ActiveTransports) -> serde_json::Value {
    transports
        .openai
        .as_ref()
        .map_or(serde_json::Value::Null, |config| {
            serde_json::json!({
                "base_url": transports::openai::canonical_base_url(&config.base_url),
                "tunnel_id": config.tunnel_id,
                "organization_id": config.organization_id,
            })
        })
}

fn runtime_authority_intent(
    args: &Args,
    setup: &SetupResult,
    policy: &Policy,
    transports: &ActiveTransports,
) -> Result<RuntimeAuthorityIntent> {
    let constraints = transports.constraints();
    let mut execution = serde_json::json!({
        "base_dir": policy.base_dir,
        "read_roots": policy.read_roots,
        "write_roots": policy.write_roots,
        "deny_read_roots": policy.deny_read_roots,
        "deny_write_roots": policy.deny_write_roots,
        "unrestricted_fs": policy.unrestricted_fs,
        "allow_shell": policy.allow_shell,
        "sandbox_shell": policy.sandbox_shell,
        "allow_network": policy.allow_network,
        "max_shell_timeout_secs": setup.config.max_shell_timeout_secs,
        "max_output_bytes": setup.config.max_output_bytes,
        "max_read_bytes": setup.config.max_read_bytes,
        "max_write_bytes": setup.config.max_write_bytes,
        "saved_caches": setup.config.caches,
        "cli_caches": args.cache_flags.requested(),
        "protected_paths": setup.protected_paths,
        "transport_constraints": {
            "task_result_budget": constraints.task_result_budget,
        },
    });
    #[cfg(target_os = "linux")]
    {
        execution["host_access"] = serde_json::json!({
            "nix_daemon": policy.host_access.nix_daemon,
            "nix_config": policy.host_access.nix_config,
            "ssh_agent": policy.host_access.ssh_agent,
            "git_config": policy.host_access.git_config,
            "kvm": policy.allow_kvm,
            "gpu": policy.allow_gpu,
        });
    }
    let execution = crate::mcp::sha256(&serde_json::to_vec(&execution)?);
    let job_session = crate::mcp::sha256(&serde_json::to_vec(&serde_json::json!({
        "execution": hex::encode(execution),
        // Hidden OpenAI request keys are namespace-scoped. A logical tunnel
        // change must retire the old JobManager even when machine authority is
        // otherwise identical.
        "openai_job_namespace": openai_job_namespace_value(transports),
    }))?);
    Ok(RuntimeAuthorityIntent {
        execution,
        job_session,
    })
}

fn execution_authority_fingerprint(
    intent: [u8; 32],
    prepared: &PreparedMachine,
) -> Result<[u8; 32]> {
    let authority = serde_json::json!({
        "intent": hex::encode(intent),
        "machine": prepared.authority_value(),
    });
    Ok(crate::mcp::sha256(&serde_json::to_vec(&authority)?))
}

/// Resolve configuration from local state, without host devices or the network
/// (first-run interactive setup aside). `None` means that setup was cancelled.
async fn plan_runtime(
    args: &Args,
    launch_dir: &Path,
    log: &LogConfig,
    setup_color: bool,
    openai_state: &transports::openai::ProcessState,
) -> Result<Option<RuntimePlan>> {
    let Some(mut setup) = load_or_setup(false, args.profile.as_deref(), setup_color).await? else {
        return Ok(None);
    };

    let policy = Policy::from_args(args, launch_dir.to_path_buf(), &setup.config.permissions)?;

    let local_transports = resolve_local_transports(args, &setup.config.transports)?;

    // An explicit ngrok credential is selected only when ngrok is active. Read
    // it once and carry both its value and descriptor-verified protected path
    // into the HTTP runtime and filesystem policy.
    let ngrok_credential = if local_transports.ngrok {
        credentials::from_env("NGROK_AUTHTOKEN_FILE")?
    } else {
        None
    };
    if let Some(source) = ngrok_credential.as_ref() {
        source.extend_protected_paths(&mut setup.protected_paths);
        setup.protected_paths.sort();
        setup.protected_paths.dedup();
    }

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
            base_url: transports::openai::canonical_base_url(&setup.config.base_url).to_owned(),
            tunnel_id,
            runtime_api_key: setup.config.runtime_api_key.clone(),
            organization_id: setup.config.organization_id.clone(),
            new_tunnel: setup.new_tunnel,
            execution_authority: None,
            log: log.clone(),
            process_state: openai_state.clone(),
        })
    } else {
        None
    };

    let transports = ActiveTransports {
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
            ngrok_authtoken: ngrok_credential
                .as_ref()
                .map(|source| source.secret().to_owned()),
            log: log.clone(),
        }),
    };
    if transports.is_empty() {
        bail!("no MCP transport is active; enable one in setup or add --stdio/--http for this run");
    }

    Ok(Some(RuntimePlan {
        setup,
        policy,
        transports,
    }))
}

async fn prepare_runtime(
    args: &Args,
    log: &LogConfig,
    plan: RuntimePlan,
    intent: RuntimeAuthorityIntent,
    jobs: &mut Option<jobs::JobManager>,
) -> Result<(ActiveTransports, LocalMachine)> {
    let RuntimePlan {
        setup,
        policy,
        mut transports,
    } = plan;

    // Startup depends on the host (devices, caches and filesystem state), so
    // its failures restart by default. Resolve/pin machine authority once into a
    // PreparedMachine; the fingerprint and final LocalMachine consume that exact
    // snapshot instead of independently traversing the same pathnames.
    let mut cache_mounts = Vec::new();
    let mut shell_env = BTreeMap::new();
    #[cfg(target_os = "linux")]
    let mut host_shell = host_services::HostShell::prepare(policy.host_access)?;
    #[cfg(target_os = "linux")]
    hardware::add_to_shell(&mut host_shell, policy.allow_kvm, &policy.allow_gpu)?;
    if policy.allow_shell && policy.sandbox_shell {
        let mut caches = setup.config.caches.clone();
        for override_cache in cache_grants_for_cli(&args.cache_flags.requested()).await? {
            caches.retain(|saved| saved.kind != override_cache.kind);
            caches.push(override_cache);
        }
        for cache in &caches {
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

    let retention = std::time::Duration::from_secs(setup.config.job_retention_secs);
    let spool_limit = setup.config.job_spool_bytes;
    let machine_config = MachineConfig {
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
        #[cfg(target_os = "linux")]
        host_shell,
        log: log.clone(),
        serialize: args.serialize,
        allow_shell: policy.allow_shell,
        sandbox_shell: policy.sandbox_shell,
        allow_network: policy.allow_network,
        max_shell_timeout_secs: setup.config.max_shell_timeout_secs,
        max_output_bytes: setup.config.max_output_bytes,
        max_read_bytes: setup.config.max_read_bytes,
        max_write_bytes: setup.config.max_write_bytes,
        task_result_budget: transports.constraints().task_result_budget,
        job_retention_secs: setup.config.job_retention_secs,
        job_spool_bytes: setup.config.job_spool_bytes,
        protected_paths: setup.protected_paths.clone(),
    };

    let (job_manager, prepared, authority) = if let Some(existing) = jobs.as_ref() {
        let prepared = match LocalMachine::prepare_with_job_root(
            machine_config.clone(),
            existing.protected_path(),
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                // Reuse is allowed only after the replacement authority is
                // successfully prepared and proven identical. A preparation
                // failure leaves equality unknown, so revoke the old session
                // before propagating the startup error.
                shutdown_job_session(jobs).await?;
                return Err(error);
            }
        };
        let authority = execution_authority_fingerprint(intent.execution, &prepared)?;
        if existing.bind_policy(authority) {
            (existing.clone(), prepared, authority)
        } else {
            shutdown_job_session(jobs).await?;
            let created = jobs::JobManager::with_spool_limit(retention, spool_limit)?;
            let bound_intent = created.bind_intent(intent.job_session);
            debug_assert!(bound_intent);
            let prepared =
                LocalMachine::prepare_with_job_root(machine_config, created.protected_path())
                    .await?;
            prepared.validate_launchers().await?;
            let authority = execution_authority_fingerprint(intent.execution, &prepared)?;
            let bound_policy = created.bind_policy(authority);
            debug_assert!(bound_policy);
            *jobs = Some(created.clone());
            (created, prepared, authority)
        }
    } else {
        let created = jobs::JobManager::with_spool_limit(retention, spool_limit)?;
        let bound_intent = created.bind_intent(intent.job_session);
        debug_assert!(bound_intent);
        let prepared =
            LocalMachine::prepare_with_job_root(machine_config, created.protected_path()).await?;
        prepared.validate_launchers().await?;
        let authority = execution_authority_fingerprint(intent.execution, &prepared)?;
        let bound_policy = created.bind_policy(authority);
        debug_assert!(bound_policy);
        *jobs = Some(created.clone());
        (created, prepared, authority)
    };

    if let Some(openai) = transports.openai.as_mut() {
        openai.execution_authority = Some(authority);
    }
    let machine = LocalMachine::from_prepared(prepared, job_manager)?;
    Ok((transports, machine))
}

async fn run_runtime(
    args: &Args,
    launch_dir: &Path,
    log: &LogConfig,
    setup_color: bool,
    shutdown: &mut Shutdown,
    openai_state: &transports::openai::ProcessState,
    jobs: &mut Option<jobs::JobManager>,
) -> Result<RuntimeOutcome> {
    // On the first pass this does not install a listener, preserving the OS's
    // default Ctrl+C behavior during interactive first-run setup. Every later
    // pass is raced from its first planning operation because the listener is
    // sticky for the rest of the process.
    let planned = shutdown
        .run_until_requested(plan_runtime(
            args,
            launch_dir,
            log,
            setup_color,
            openai_state,
        ))
        .await;
    let Some(planned) = planned else {
        eprintln!("\ndotlink stopped.");
        return Ok(RuntimeOutcome::Exit);
    };
    let Some(plan) = planned.map_err(recovery::config_failure)? else {
        return Ok(RuntimeOutcome::Exit);
    };
    let intent = runtime_authority_intent(args, &plan.setup, &plan.policy, &plan.transports)?;
    if jobs
        .as_ref()
        .is_some_and(|existing| !existing.bind_intent(intent.job_session))
    {
        // Config/CLI authority changed. Revoke the old session before any
        // potentially failing startup/discovery work for the replacement.
        shutdown_job_session(jobs).await?;
    }
    // Initial interactive planning is complete. From this exact point onward,
    // signal cancellation races startup as well as the running transports.
    shutdown.start()?;
    let prepared = shutdown
        .run_until_requested(prepare_runtime(args, log, plan, intent, jobs))
        .await;
    let Some(prepared) = prepared else {
        eprintln!("\ndotlink stopped.");
        return Ok(RuntimeOutcome::Exit);
    };
    let (active_transports, machine) = prepared?;

    let shutdown_requested = shutdown.requested()?;
    // Linking the runtime token directly to the process listener lets OpenAI's
    // cancellation-first selects observe the signal in the same scheduler turn.
    let cancellation = shutdown.child_token()?;
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
        biased;
        _ = shutdown_requested => RuntimeEvent::Exit,
        result = &mut transport_task => {
            RuntimeEvent::Transport(match result {
                Ok(result) => result,
                Err(error) => Err(anyhow!("transport supervisor task failed: {error}")),
            })
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
            stop_runtime_transports(&mut transport_task, &cancellation).await?;
            eprintln!("\ndotlink stopped.");
            Ok(RuntimeOutcome::Exit)
        }
        RuntimeEvent::Restart => {
            stop_runtime_transports(&mut transport_task, &cancellation).await?;
            Ok(RuntimeOutcome::Restart)
        }
    }
}

async fn stop_runtime_transports(
    task: &mut tokio::task::JoinHandle<Result<()>>,
    cancellation: &CancellationToken,
) -> Result<()> {
    cancellation.cancel();
    match tokio::time::timeout(std::time::Duration::from_secs(6), &mut *task).await {
        // Stopping anyway: a transport's concurrent restart request is moot.
        Ok(Ok(Err(error))) if Recovery::of(&error).restarts_runtime() => Ok(()),
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(anyhow!(
            "transport supervisor task failed during shutdown: {error}"
        )),
        Err(_) => {
            task.abort();
            let _ = task.await;
            Err(anyhow!(
                "transport supervisor exceeded the shutdown deadline"
            ))
        }
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
    let bind_ip = transports::http::normalize_ip(bind.ip());
    let non_loopback_http = !bind_ip.is_loopback();
    if args.allow_public_no_auth && bind_ip.is_unspecified() {
        bail!(
            "unauthenticated HTTP cannot use a wildcard bind: choose a concrete interface IP so the MCP Host allowlist can protect against DNS rebinding"
        );
    }
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
        log.style("1;36", format!("dotlink v{}", env!("CARGO_PKG_VERSION")))
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
        } else if !transports::http::normalize_ip(http.bind.ip()).is_loopback() {
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
    #[cfg(target_os = "linux")]
    if let Some(services) = machine.host_access_summary() {
        eprintln!("• Host       {services}");
    }
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
    let keys = if controls_available && log.activity_enabled() {
        "Keys        Ctrl-C: exit · Ctrl-R: restart · v: verbosity · V: full commands"
    } else if controls_available {
        "Keys        Ctrl-C: exit · Ctrl-R: restart · v: verbosity"
    } else {
        "Keys        Ctrl-C: exit"
    };
    eprintln!("{}", log.style("2", keys));
    eprintln!();
}

fn print_tools(descriptions: Vec<(String, String)>) {
    println!("abird dotlink tools");
    println!("────────────────────────────────────────────────────────");
    for (name, title) in descriptions {
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
    fn shell_timeout_ceiling_participates_in_job_authority_intent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let args = Args::try_parse_from(["dotlink"]).unwrap();
        let mut config: crate::setup::AppConfig =
            serde_json::from_value(serde_json::json!({"version": 10})).unwrap();
        config.transports.openai = false;
        config.transports.stdio = true;
        config.permissions.default_allow = true;
        config.max_shell_timeout_secs = 60;
        let setup = SetupResult {
            config,
            new_tunnel: false,
            protected_paths: Vec::new(),
        };
        let policy = Policy::from_args(&args, root, &setup.config.permissions).unwrap();
        let transports = ActiveTransports {
            openai: None,
            stdio: true,
            http: None,
        };
        let before = runtime_authority_intent(&args, &setup, &policy, &transports)
            .unwrap()
            .execution;

        let mut changed = setup;
        changed.config.max_shell_timeout_secs = 61;
        let after = runtime_authority_intent(&args, &changed, &policy, &transports)
            .unwrap()
            .execution;
        assert_ne!(
            before, after,
            "execution-lifetime policy changes must revoke the old job session"
        );
    }

    #[test]
    fn data_exposure_ceiling_changes_revoke_execution_authority() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let args = Args::try_parse_from(["dotlink"]).unwrap();
        let mut config: crate::setup::AppConfig =
            serde_json::from_value(serde_json::json!({"version": 10})).unwrap();
        config.transports.openai = false;
        config.transports.stdio = true;
        config.permissions.default_allow = true;
        let setup = SetupResult {
            config,
            new_tunnel: false,
            protected_paths: Vec::new(),
        };
        let policy = Policy::from_args(&args, root, &setup.config.permissions).unwrap();
        let transports = ActiveTransports {
            openai: None,
            stdio: true,
            http: None,
        };
        let baseline = runtime_authority_intent(&args, &setup, &policy, &transports)
            .unwrap()
            .execution;

        let mut setup = setup;
        let output = setup.config.max_output_bytes;
        let read = setup.config.max_read_bytes;
        let write = setup.config.max_write_bytes;
        for field in ["max_output_bytes", "max_read_bytes", "max_write_bytes"] {
            match field {
                "max_output_bytes" => setup.config.max_output_bytes = output + 1024,
                "max_read_bytes" => setup.config.max_read_bytes = read + 1024,
                "max_write_bytes" => setup.config.max_write_bytes = write + 1024,
                _ => unreachable!(),
            }
            let after = runtime_authority_intent(&args, &setup, &policy, &transports)
                .unwrap()
                .execution;
            assert_ne!(
                baseline, after,
                "{field} changes must invalidate replayable response authority"
            );
            setup.config.max_output_bytes = output;
            setup.config.max_read_bytes = read;
            setup.config.max_write_bytes = write;
        }
    }

    #[test]
    fn runtime_api_key_rotation_does_not_revoke_job_execution_authority() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let args = Args::try_parse_from(["dotlink"]).unwrap();
        let mut config: crate::setup::AppConfig =
            serde_json::from_value(serde_json::json!({"version": 10})).unwrap();
        config.permissions.default_allow = true;
        let setup = SetupResult {
            config,
            new_tunnel: false,
            protected_paths: Vec::new(),
        };
        let policy = Policy::from_args(&args, root, &setup.config.permissions).unwrap();
        let openai = |key: &str| transports::openai::Config {
            base_url: "https://api.openai.com".to_owned(),
            tunnel_id: "tunnel".to_owned(),
            runtime_api_key: key.to_owned(),
            organization_id: Some("org".to_owned()),
            new_tunnel: false,
            execution_authority: Some([0; 32]),
            log: LogConfig::default(),
            process_state: transports::openai::ProcessState::default(),
        };
        let before_transports = ActiveTransports {
            openai: Some(openai("key-one")),
            stdio: false,
            http: None,
        };
        let after_transports = ActiveTransports {
            openai: Some(openai("key-two")),
            stdio: false,
            http: None,
        };
        let before = runtime_authority_intent(&args, &setup, &policy, &before_transports).unwrap();
        let after = runtime_authority_intent(&args, &setup, &policy, &after_transports).unwrap();
        assert_eq!(
            before, after,
            "transport credential rotation must not revoke execution-lifetime job authority"
        );
    }

    #[test]
    fn transport_credentials_do_not_revoke_job_execution_authority() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let args = Args::try_parse_from(["dotlink"]).unwrap();
        let mut config: crate::setup::AppConfig =
            serde_json::from_value(serde_json::json!({"version": 10})).unwrap();
        config.permissions.default_allow = true;
        let setup = SetupResult {
            config,
            new_tunnel: false,
            protected_paths: Vec::new(),
        };
        let policy = Policy::from_args(&args, root, &setup.config.permissions).unwrap();
        let http = |token: &str| transports::http::Config {
            bind: "127.0.0.1:3000".parse().unwrap(),
            ngrok: true,
            ngrok_domain: None,
            http_ephemeral_url: false,
            ngrok_ephemeral_url: false,
            oauth: None,
            local_oauth: false,
            ngrok_oauth: false,
            oauth_public_url: None,
            ngrok_no_auth: true,
            ngrok_authtoken: Some(token.to_owned()),
            log: LogConfig::default(),
        };
        let before = ActiveTransports {
            openai: None,
            stdio: true,
            http: Some(http("token-one")),
        };
        let after = ActiveTransports {
            openai: None,
            stdio: true,
            http: Some(http("token-two")),
        };
        assert_eq!(
            runtime_authority_intent(&args, &setup, &policy, &before).unwrap(),
            runtime_authority_intent(&args, &setup, &policy, &after).unwrap(),
            "ngrok credential rotation must not revoke execution-lifetime job authority"
        );

        let mut changed_setup = setup;
        let before_revision =
            runtime_authority_intent(&args, &changed_setup, &policy, &before).unwrap();
        changed_setup.config.owner_password_revision = Some("rotated".to_owned());
        let after_revision =
            runtime_authority_intent(&args, &changed_setup, &policy, &before).unwrap();
        assert_eq!(
            before_revision, after_revision,
            "OAuth owner-password rotation must not revoke execution-lifetime job authority"
        );
    }

    #[test]
    fn openai_logical_namespace_revokes_job_session_not_execution_authority() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let args = Args::try_parse_from(["dotlink"]).unwrap();
        let mut config: crate::setup::AppConfig =
            serde_json::from_value(serde_json::json!({"version": 10})).unwrap();
        config.permissions.default_allow = true;
        let setup = SetupResult {
            config,
            new_tunnel: false,
            protected_paths: Vec::new(),
        };
        let policy = Policy::from_args(&args, root, &setup.config.permissions).unwrap();
        let openai = |tunnel: &str| transports::openai::Config {
            base_url: "https://api.openai.com".to_owned(),
            tunnel_id: tunnel.to_owned(),
            runtime_api_key: "same-key".to_owned(),
            organization_id: Some("org".to_owned()),
            new_tunnel: false,
            execution_authority: None,
            log: LogConfig::default(),
            process_state: transports::openai::ProcessState::default(),
        };
        let before = ActiveTransports {
            openai: Some(openai("tunnel-one")),
            stdio: false,
            http: None,
        };
        let after = ActiveTransports {
            openai: Some(openai("tunnel-two")),
            stdio: false,
            http: None,
        };
        let before = runtime_authority_intent(&args, &setup, &policy, &before).unwrap();
        let after = runtime_authority_intent(&args, &setup, &policy, &after).unwrap();
        assert_eq!(
            before.execution, after.execution,
            "logical tunnel topology must not change machine execution authority"
        );
        assert_ne!(
            before.job_session, after.job_session,
            "changing the OpenAI replay namespace must revoke hidden request-key authority"
        );
    }

    #[test]
    fn equivalent_openai_base_url_spellings_preserve_job_session_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let args = Args::try_parse_from(["dotlink"]).unwrap();
        let mut config: crate::setup::AppConfig =
            serde_json::from_value(serde_json::json!({"version": 10})).unwrap();
        config.permissions.default_allow = true;
        let setup = SetupResult {
            config,
            new_tunnel: false,
            protected_paths: Vec::new(),
        };
        let policy = Policy::from_args(&args, root, &setup.config.permissions).unwrap();
        let openai = |base_url: &str| transports::openai::Config {
            base_url: base_url.to_owned(),
            tunnel_id: "tunnel".to_owned(),
            runtime_api_key: "key".to_owned(),
            organization_id: Some("org".to_owned()),
            new_tunnel: false,
            execution_authority: None,
            log: LogConfig::default(),
            process_state: transports::openai::ProcessState::default(),
        };
        let transports = |base_url: &str| ActiveTransports {
            openai: Some(openai(base_url)),
            stdio: false,
            http: None,
        };

        let plain = runtime_authority_intent(
            &args,
            &setup,
            &policy,
            &transports("https://api.openai.com"),
        )
        .unwrap();
        let slashed = runtime_authority_intent(
            &args,
            &setup,
            &policy,
            &transports("https://api.openai.com///"),
        )
        .unwrap();
        assert_eq!(
            plain, slashed,
            "equivalent control-plane endpoint spellings must not revoke durable job authority"
        );
        assert_eq!(
            transports::openai::canonical_base_url("https://api.openai.com///"),
            "https://api.openai.com"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn prepared_authority_is_stable_and_reprepare_observes_guard_retarget() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let first = root.join("first");
        let second = root.join("second");
        let deny_alias = root.join("deny-link");
        let protected_alias = root.join("protected-link");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        symlink(&first, &deny_alias).unwrap();
        symlink(&first, &protected_alias).unwrap();
        let manager = jobs::JobManager::new(Duration::from_secs(60)).unwrap();
        let config = MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root.clone()],
                write_roots: vec![root.clone()],
                deny_read_roots: vec![deny_alias.clone()],
                deny_write_roots: vec![deny_alias.clone()],
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            host_shell: host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 60,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,
            task_result_budget: None,
            job_retention_secs: 60,
            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: vec![protected_alias.clone()],
        };
        let prepared_before =
            LocalMachine::prepare_with_job_root(config.clone(), manager.protected_path())
                .await
                .unwrap();
        let before = execution_authority_fingerprint([7; 32], &prepared_before).unwrap();

        std::fs::remove_file(&deny_alias).unwrap();
        std::fs::remove_file(&protected_alias).unwrap();
        symlink(&second, &deny_alias).unwrap();
        symlink(&second, &protected_alias).unwrap();

        assert_eq!(
            before,
            execution_authority_fingerprint([7; 32], &prepared_before).unwrap(),
            "one prepared snapshot must not re-resolve host names during fingerprinting"
        );
        let prepared_after = LocalMachine::prepare_with_job_root(config, manager.protected_path())
            .await
            .unwrap();
        let after = execution_authority_fingerprint([7; 32], &prepared_after).unwrap();
        assert_ne!(before, after, "re-preparing must observe guard retargeting");
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lazy_job_session_root_materialization_preserves_execution_authority() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let manager = jobs::JobManager::new(Duration::from_secs(60)).unwrap();
        assert!(!manager.protected_path().exists());

        let config = MachineConfig {
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
            host_shell: host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 60,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,
            task_result_budget: None,
            job_retention_secs: 60,
            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        };
        let before = LocalMachine::prepare_with_job_root(config.clone(), manager.protected_path())
            .await
            .unwrap();
        let before = execution_authority_fingerprint([63; 32], &before).unwrap();

        let started = manager
            .spawn(30, 1024, None, |_job| async {
                jobs::JobCompletion::completed(
                    Some(0),
                    true,
                    jobs::StreamSummary::default(),
                    jobs::StreamSummary::default(),
                )
            })
            .unwrap();
        drop(started);
        assert!(manager.protected_path().exists());
        let after = LocalMachine::prepare_with_job_root(config, manager.protected_path())
            .await
            .unwrap();
        let after = execution_authority_fingerprint([63; 32], &after).unwrap();
        assert_eq!(
            before, after,
            "materializing dotlink's own lazy session root must not revoke an unchanged job session"
        );
        manager.shutdown().await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn prepared_cache_authority_uses_concrete_pinned_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let cache = root.join("cache");
        std::fs::create_dir(&cache).unwrap();
        let manager = jobs::JobManager::new(Duration::from_secs(60)).unwrap();
        let prepared = LocalMachine::prepare_with_job_root(
            MachineConfig {
                access: AccessSpec {
                    base_dir: root.clone(),
                    read_roots: vec![root],
                    write_roots: Vec::new(),
                    deny_read_roots: Vec::new(),
                    deny_write_roots: Vec::new(),
                    unrestricted_fs: false,
                },
                cache_mounts: vec![SandboxCacheMount {
                    source: cache,
                    target: PathBuf::from("/tmp/home/.cache/test"),
                    writable: false,
                }],
                shell_env: Vec::new(),
                host_shell: host_services::HostShell::default(),
                log: LogConfig::default(),
                serialize: false,
                allow_shell: false,
                sandbox_shell: false,
                allow_network: false,
                max_shell_timeout_secs: 60,
                max_output_bytes: 4096,
                max_read_bytes: 4096,
                max_write_bytes: 4096,
                task_result_budget: None,
                job_retention_secs: 60,
                job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
                protected_paths: Vec::new(),
            },
            manager.protected_path(),
        )
        .await
        .unwrap();
        let cache = &prepared.authority_value()["cache_mounts"][0];
        assert!(cache["dev"].as_u64().is_some(), "{cache}");
        assert!(cache["ino"].as_u64().is_some(), "{cache}");
        assert!(cache["rdev"].as_u64().is_some(), "{cache}");
        manager.shutdown().await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn prepared_grant_fingerprint_uses_the_pinned_root_capability() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let first = root.join("first-grant");
        let second = root.join("second-grant");
        let grant = root.join("grant-link");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        symlink(&first, &grant).unwrap();
        let manager = jobs::JobManager::new(Duration::from_secs(60)).unwrap();
        let config = MachineConfig {
            access: AccessSpec {
                base_dir: root,
                read_roots: vec![grant.clone()],
                write_roots: Vec::new(),
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            host_shell: host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 60,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,
            task_result_budget: None,
            job_retention_secs: 60,
            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        };
        let prepared =
            LocalMachine::prepare_with_job_root(config.clone(), manager.protected_path())
                .await
                .unwrap();
        let before = execution_authority_fingerprint([9; 32], &prepared).unwrap();

        std::fs::remove_file(&grant).unwrap();
        symlink(&second, &grant).unwrap();
        assert_eq!(
            before,
            execution_authority_fingerprint([9; 32], &prepared).unwrap(),
            "the prepared snapshot must use its already-pinned root capability"
        );

        let reparsed = LocalMachine::prepare_with_job_root(config, manager.protected_path())
            .await
            .unwrap();
        let after = execution_authority_fingerprint([9; 32], &reparsed).unwrap();
        assert_ne!(
            before, after,
            "a new preparation must observe the new grant target"
        );
        manager.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn authority_path_identity_tracks_the_object_behind_one_path() {
        let temp = tempfile::tempdir().unwrap();
        let live = temp.path().join("live");
        let replacement = temp.path().join("replacement");
        let old = temp.path().join("old");
        std::fs::create_dir(&live).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        let before = crate::mcp::authority_path_identity(&live);
        std::fs::rename(&live, &old).unwrap();
        std::fs::rename(&replacement, &live).unwrap();
        let after = crate::mcp::authority_path_identity(&live);
        assert_eq!(before["path"], after["path"]);
        assert_ne!(before["ino"], after["ino"]);
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
        let delay = |attempt| RUNTIME_RESTART_BACKOFF.delay(attempt);
        assert_eq!(delay(1), Duration::from_secs(1));
        assert_eq!(delay(2), Duration::from_secs(2));
        assert_eq!(delay(5), Duration::from_secs(16));
        assert_eq!(delay(6), Duration::from_secs(30));
        assert_eq!(delay(100), Duration::from_secs(30));
    }

    #[test]
    fn deny_can_remove_the_last_effective_capability() {
        let args = Args::try_parse_from(["dotlink", "--deny-read"]).unwrap();
        let error =
            Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).unwrap_err();
        assert!(error.to_string().contains("no effective local capability"));
    }

    #[test]
    fn hardware_grants_are_explicit_and_repeatable() {
        let plain = Args::try_parse_from(["dotlink"]).unwrap();
        assert!(!plain.allow_kvm && plain.allow_gpu.is_empty());
        for flag in ["--allow-kvm", "--allow-gpu=amd", "--allow-gpu=nvidia"] {
            let args = Args::try_parse_from(["dotlink", flag]).unwrap();
            let error =
                Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).unwrap_err();
            assert!(error.to_string().contains("Bubblewrap"), "{flag}: {error}");
            assert!(Args::try_parse_from(["dotlink", "--setup", flag]).is_err());
        }
        #[cfg(target_os = "linux")]
        {
            let args = Args::try_parse_from([
                "dotlink",
                "--allow-shell",
                "--allow-kvm",
                "--allow-gpu=amd",
                "--allow-gpu=nvidia",
                "--allow-gpu=amd",
            ])
            .unwrap();
            let policy =
                Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).unwrap();
            assert!(policy.allow_shell && policy.sandbox_shell);
            assert!(policy.allow_kvm);
            assert_eq!(policy.allow_gpu, [GpuVendor::Amd, GpuVendor::Nvidia]);
        }
        assert!(Args::try_parse_from(["dotlink", "--allow-gpu=intel"]).is_err());
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
    fn empty_profile_grants_are_composed_with_cli_before_capability_checks() {
        let mut defaults = no_defaults();
        defaults.default_allow = false;
        defaults.allow_read.clear();
        defaults.allow_write.clear();
        defaults.allow_rw.clear();
        let base = PathBuf::from("/workspace");
        let plain = Args::try_parse_from(["dotlink", "--no-default-allow"]).unwrap();
        assert!(
            Policy::from_args(&plain, base.clone(), &defaults)
                .unwrap_err()
                .to_string()
                .contains("no effective local capability")
        );

        let read = Args::try_parse_from(["dotlink", "--no-default-allow", "--allow-read=/project"])
            .unwrap();
        let policy = Policy::from_args(&read, base.clone(), &defaults).unwrap();
        assert_eq!(policy.read_roots, [PathBuf::from("/project")]);

        let rw = Args::try_parse_from(["dotlink", "--no-default-allow", "--allow-rw=/"]).unwrap();
        let policy = Policy::from_args(&rw, base.clone(), &defaults).unwrap();
        assert_eq!(policy.read_roots, [PathBuf::from("/")]);
        assert_eq!(policy.write_roots, [PathBuf::from("/")]);

        defaults.allow_shell = true;
        let error = Policy::from_args(&plain, base.clone(), &defaults).unwrap_err();
        assert!(error.to_string().contains("shell access requires"));
        #[cfg(target_os = "linux")]
        assert!(
            Policy::from_args(&read, base.clone(), &defaults)
                .unwrap()
                .allow_shell
        );

        defaults.allow_shell = false;
        let denied = Args::try_parse_from([
            "dotlink",
            "--no-default-allow",
            "--allow-read=/project",
            "--deny-read=/project",
        ])
        .unwrap();
        assert!(
            Policy::from_args(&denied, base, &defaults)
                .unwrap_err()
                .to_string()
                .contains("no effective local capability")
        );
    }

    #[test]
    fn no_filesystem_or_shell_capability_is_rejected() {
        let args = Args::try_parse_from(["dotlink", "--no-default-allow"]).unwrap();
        assert!(Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).is_err());
    }

    /// The sandboxed shell is Linux-only, so its readable-directory requirement is
    /// only reachable there; the non-Linux rejection is asserted separately.
    #[cfg(target_os = "linux")]
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

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn native_shell_requires_an_explicit_no_sandbox() {
        let args =
            Args::try_parse_from(["dotlink", "--no-default-allow", "--allow-shell"]).unwrap();
        let error =
            Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("sandboxed shell execution is unavailable on this OS"),
            "{error}"
        );
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
    fn legacy_allow_all_and_no_sandbox_still_require_each_other() {
        for argv in [
            ["dotlink", "--allow-all"].as_slice(),
            ["dotlink", "--no-sandbox"].as_slice(),
        ] {
            let args = Args::try_parse_from(argv).unwrap();
            let error =
                Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).unwrap_err();
            assert!(
                error.to_string().contains("must be used together"),
                "{error}"
            );
        }
        let args = Args::try_parse_from(["dotlink", "--allow-all", "--no-sandbox"]).unwrap();
        assert!(Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).is_ok());
    }

    #[test]
    fn unrestricted_is_the_one_flag_full_host_mode() {
        let args = Args::try_parse_from(["dotlink", "--unrestricted"]).unwrap();
        let policy = Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).unwrap();
        assert!(policy.unrestricted_fs);
        assert!(policy.read_roots.contains(&PathBuf::from("/")));
        assert!(policy.write_roots.contains(&PathBuf::from("/")));
        assert!(policy.allow_shell);
        assert!(!policy.sandbox_shell);
        assert!(policy.allow_network);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unrestricted_supersedes_saved_and_cli_sandbox_integrations() {
        let defaults = PermissionConfig {
            allow_shell: true,
            allow_nix_daemon: true,
            allow_nix_config: true,
            allow_ssh_agent: true,
            allow_git_config: true,
            allow_kvm: true,
            allow_gpu: vec![GpuVendor::Amd],
            ..no_defaults()
        };
        for argv in [
            ["dotlink", "--unrestricted"].as_slice(),
            ["dotlink", "--allow-all", "--no-sandbox"].as_slice(),
            [
                "dotlink",
                "--unrestricted",
                "--allow-ssh-agent",
                "--allow-kvm",
                "--allow-gpu=amd",
                "--allow-cache-npm",
            ]
            .as_slice(),
        ] {
            let args = Args::try_parse_from(argv).unwrap();
            let policy = Policy::from_args(&args, PathBuf::from("/workspace"), &defaults).unwrap();
            assert!(policy.unrestricted_fs);
            assert!(policy.allow_shell);
            assert!(policy.allow_network);
            assert!(!policy.sandbox_shell);
            assert_eq!(policy.host_access, host_services::HostAccess::default());
            assert!(!policy.allow_kvm);
            assert!(policy.allow_gpu.is_empty());
        }

        // Normal sandboxed mode must retain precisely the saved integrations.
        let (_, sandboxed) = parse(&["dotlink"], defaults);
        assert!(sandboxed.sandbox_shell);
        assert!(sandboxed.host_access.nix_daemon);
        assert!(sandboxed.host_access.nix_config);
        assert!(sandboxed.host_access.ssh_agent);
        assert!(sandboxed.host_access.git_config);
        assert!(sandboxed.allow_kvm);
        assert_eq!(sandboxed.allow_gpu, vec![GpuVendor::Amd]);
    }

    #[test]
    fn unrestricted_matches_legacy_allow_all_no_sandbox_policy() {
        let unrestricted = Args::try_parse_from(["dotlink", "--unrestricted"]).unwrap();
        let legacy = Args::try_parse_from(["dotlink", "--allow-all", "--no-sandbox"]).unwrap();
        let left =
            Policy::from_args(&unrestricted, PathBuf::from("/workspace"), &no_defaults()).unwrap();
        let right =
            Policy::from_args(&legacy, PathBuf::from("/workspace"), &no_defaults()).unwrap();
        assert_eq!(left.read_roots, right.read_roots);
        assert_eq!(left.write_roots, right.write_roots);
        assert_eq!(left.unrestricted_fs, right.unrestricted_fs);
        assert_eq!(left.allow_shell, right.allow_shell);
        assert_eq!(left.sandbox_shell, right.sandbox_shell);
        assert_eq!(left.allow_network, right.allow_network);
    }

    #[test]
    fn unrestricted_and_allow_all_reject_deny_rules() {
        let args = Args::try_parse_from([
            "dotlink",
            "--allow-all",
            "--no-sandbox",
            "--deny-rw=/secret",
        ])
        .unwrap();
        assert!(Policy::from_args(&args, PathBuf::from("/workspace"), &no_defaults()).is_err());

        let unrestricted =
            Args::try_parse_from(["dotlink", "--unrestricted", "--deny-rw=/secret"]).unwrap();
        assert!(
            Policy::from_args(&unrestricted, PathBuf::from("/workspace"), &no_defaults()).is_err()
        );
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
    fn unauthenticated_wildcard_bind_fails_instead_of_rejecting_every_real_host() {
        let args = Args::try_parse_from(["dotlink", "--allow-public-no-auth"]).unwrap();
        let defaults = TransportConfig {
            openai: false,
            http: true,
            ..TransportConfig::default()
        };
        let transports = resolve_local_transports(&args, &defaults).unwrap();
        for bind in ["0.0.0.0:3000", "[::ffff:0.0.0.0]:3000"] {
            let error = resolve_http_auth(
                &args,
                &oauth::OAuthConfig::default(),
                &transports,
                Some(bind.parse().unwrap()),
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("wildcard bind"), "bind={bind}: {error}");
        }
        assert!(
            resolve_http_auth(
                &args,
                &oauth::OAuthConfig::default(),
                &transports,
                Some("192.0.2.1:3000".parse().unwrap()),
            )
            .is_ok()
        );
    }

    #[test]
    fn ipv4_mapped_loopback_is_classified_as_loopback_for_http_auth() {
        let args = Args::try_parse_from(["dotlink", "--http"]).unwrap();
        let transports = resolve_local_transports(&args, &TransportConfig::default()).unwrap();
        let auth = resolve_http_auth(
            &args,
            &oauth::OAuthConfig::default(),
            &transports,
            Some("[::ffff:127.0.0.1]:3000".parse().unwrap()),
        )
        .unwrap();
        assert!(!auth.local_oauth);
        assert!(!auth.ngrok_oauth);
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
        let concrete_bind = Some("192.0.2.1:3000".parse().unwrap());

        let args = Args::try_parse_from(["dotlink", "--http"]).unwrap();
        let transports = resolve_local_transports(&args, &TransportConfig::default()).unwrap();
        assert!(
            resolve_http_auth(&args, &oauth::OAuthConfig::default(), &transports, bind).is_err()
        );

        let args = Args::try_parse_from(["dotlink", "--http", "--allow-public-no-auth"]).unwrap();
        let transports = resolve_local_transports(&args, &TransportConfig::default()).unwrap();
        let auth = resolve_http_auth(
            &args,
            &oauth::OAuthConfig::default(),
            &transports,
            concrete_bind,
        )
        .unwrap();
        assert!(!auth.local_oauth);

        let auth = resolve_http_auth(
            &args,
            &oauth::OAuthConfig {
                enabled: true,
                public_url: Some("https://mcp.example.com".to_owned()),
            },
            &transports,
            concrete_bind,
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
    fn detected_cache_families_have_symmetric_ro_rw_flags() {
        use clap::CommandFactory;
        let command = Args::command();
        let cache_flags: Vec<_> = command
            .get_arguments()
            .filter_map(|arg| arg.get_long())
            .filter(|long| long.starts_with("allow-cache-"))
            .collect();
        assert_eq!(cache_flags.len(), CacheKind::ALL.len());
        for kind in CacheKind::ALL {
            let flag = format!("--allow-cache-{}", kind.cli_name());
            let ro = Args::try_parse_from(["dotlink", flag.as_str()]).unwrap();
            assert!(
                ro.cache_flags
                    .requested()
                    .contains(&(kind, CacheMode::ReadOnly)),
                "{flag}"
            );
            let rw_flag = format!("{flag}=rw");
            let rw = Args::try_parse_from(["dotlink", rw_flag.as_str()]).unwrap();
            assert!(
                rw.cache_flags
                    .requested()
                    .contains(&(kind, CacheMode::ReadWrite)),
                "{rw_flag}"
            );
        }
    }

    #[test]
    fn host_service_flags_require_a_sandboxed_shell() {
        for flag in [
            "--allow-nix-daemon",
            "--allow-nix-config",
            "--allow-ssh-agent",
            "--allow-git-config",
            "--allow-cache-npm",
        ] {
            assert!(
                Args::try_parse_from(["dotlink", "--setup", flag]).is_err(),
                "{flag} should not silently be ignored during setup"
            );
            let args = Args::try_parse_from(["dotlink", flag]).unwrap();
            assert!(
                Policy::from_args(
                    &args,
                    PathBuf::from("/workspace"),
                    &PermissionConfig::default()
                )
                .is_err(),
                "{flag}"
            );
        }
        #[cfg(target_os = "linux")]
        for flag in [
            "--allow-nix-daemon",
            "--allow-nix-config",
            "--allow-ssh-agent",
            "--allow-git-config",
            "--allow-cache-npm",
        ] {
            let args = Args::try_parse_from(["dotlink", "--allow-shell", flag]).unwrap();
            assert!(
                Policy::from_args(
                    &args,
                    PathBuf::from("/workspace"),
                    &PermissionConfig::default()
                )
                .is_ok(),
                "{flag}"
            );
        }
    }

    #[test]
    fn serialization_flag_is_opt_in() {
        let normal = Args::try_parse_from(["dotlink"]).unwrap();
        assert!(!normal.serialize);
        assert!(Args::try_parse_from(["dotlink", "-s"]).unwrap().serialize);
        assert!(
            Args::try_parse_from(["dotlink", "--serialize"])
                .unwrap()
                .serialize
        );
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

    #[tokio::test]
    async fn runtime_restart_refuses_failed_transport_cleanup() {
        let cancellation = CancellationToken::new();
        let mut task = tokio::spawn(async { Err(anyhow!("cleanup failed")) });
        let error = stop_runtime_transports(&mut task, &cancellation)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cleanup failed"));
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn dependency_tracing_cannot_bypass_payload_redaction() {
        assert!(!dependency_trace_is_safe("rmcp"));
        assert!(!dependency_trace_is_safe("rmcp::service"));
        assert!(dependency_trace_is_safe("dotlink::mcp"));
        assert!(dependency_trace_is_safe("hyper::proto"));
    }

    #[test]
    fn setup_is_not_mixed_with_protocol_stdio_or_http() {
        assert!(Args::try_parse_from(["dotlink", "--setup", "--stdio"]).is_err());
        assert!(Args::try_parse_from(["dotlink", "--setup", "--http"]).is_err());
    }
}
