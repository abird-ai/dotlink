use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{process::Command, time::timeout};
use zeroize::Zeroizing;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_HTTP_BIND: &str = "127.0.0.1:3000";
const DEFAULT_MAX_SHELL_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_048_576;
const DEFAULT_MAX_FILE_BYTES: usize = 4_194_304;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransportConfig {
    #[serde(default = "default_true")]
    pub openai: bool,
    #[serde(default)]
    pub stdio: bool,
    #[serde(default)]
    pub http: bool,
    #[serde(default = "default_http_bind")]
    pub http_bind: String,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            openai: true,
            stdio: false,
            http: false,
            http_bind: default_http_bind(),
        }
    }
}

impl TransportConfig {
    pub fn enabled_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.openai {
            names.push("openai");
        }
        if self.stdio {
            names.push("stdio");
        }
        if self.http {
            names.push("http");
        }
        names
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PermissionConfig {
    /// Persistent default working directory. Relative paths resolve from the launch cwd.
    #[serde(default)]
    pub cwd: Option<PathBuf>,

    /// Additional readable directories. Relative paths resolve from the effective cwd.
    #[serde(default)]
    pub allow_read: Vec<PathBuf>,

    /// Additional writable directories. Relative paths resolve from the effective cwd.
    #[serde(default)]
    pub allow_write: Vec<PathBuf>,

    /// Additional read+write directories. Relative paths resolve from the effective cwd.
    ///
    /// Compatibility: v7 and older configs used a boolean allow_rw where true
    /// meant rw on cwd.
    #[serde(default, deserialize_with = "deserialize_path_list_or_bool")]
    pub allow_rw: Vec<PathBuf>,

    /// Paths denied for reads. Relative paths resolve from the effective cwd.
    #[serde(default)]
    pub deny_read: Vec<PathBuf>,

    /// Paths denied for writes. Relative paths resolve from the effective cwd.
    #[serde(default)]
    pub deny_write: Vec<PathBuf>,

    /// Paths denied for both reads and writes. Relative paths resolve from the effective cwd.
    #[serde(default)]
    pub deny_rw: Vec<PathBuf>,

    /// Expose the platform shell by default. Linux still uses Bubblewrap.
    #[serde(default)]
    pub allow_shell: bool,

    /// Allow network access from the sandboxed shell by default.
    #[serde(default)]
    pub allow_network: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PathListOrBool {
    Paths(Vec<PathBuf>),
    Bool(bool),
}

fn deserialize_path_list_or_bool<'de, D>(deserializer: D) -> Result<Vec<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match PathListOrBool::deserialize(deserializer)? {
        PathListOrBool::Paths(paths) => Ok(paths),
        PathListOrBool::Bool(true) => Ok(vec![PathBuf::from(".")]),
        PathListOrBool::Bool(false) => Ok(Vec::new()),
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CacheKind {
    CargoRegistry,
    CargoGit,
    Npm,
    Pnpm,
    Yarn,
    Pip,
    Uv,
    GoMod,
    GoBuild,
    Maven,
    Gradle,
    Sccache,
    Ccache,
}

impl CacheKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::CargoRegistry => "Cargo registry",
            Self::CargoGit => "Cargo git",
            Self::Npm => "npm",
            Self::Pnpm => "pnpm store",
            Self::Yarn => "Yarn",
            Self::Pip => "pip",
            Self::Uv => "uv",
            Self::GoMod => "Go module",
            Self::GoBuild => "Go build",
            Self::Maven => "Maven repository",
            Self::Gradle => "Gradle",
            Self::Sccache => "sccache",
            Self::Ccache => "ccache",
        }
    }

    fn fallback_sandbox_path(self) -> PathBuf {
        PathBuf::from(match self {
            Self::CargoRegistry => "/tmp/home/.cargo/registry",
            Self::CargoGit => "/tmp/home/.cargo/git",
            Self::Npm => "/tmp/home/.npm",
            Self::Pnpm => "/tmp/home/.local/share/pnpm/store",
            Self::Yarn => "/tmp/home/.cache/yarn",
            Self::Pip => "/tmp/home/.cache/pip",
            Self::Uv => "/tmp/home/.cache/uv",
            Self::GoMod => "/tmp/home/go/pkg/mod",
            Self::GoBuild => "/tmp/home/.cache/go-build",
            Self::Maven => "/tmp/home/.m2/repository",
            Self::Gradle => "/tmp/home/.gradle/caches",
            Self::Sccache => "/tmp/home/.cache/sccache",
            Self::Ccache => "/tmp/home/.cache/ccache",
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    ReadOnly,
    ReadWrite,
}

impl CacheMode {
    pub fn writable(self) -> bool {
        matches!(self, Self::ReadWrite)
    }

    fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ReadWrite => "read+write",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheGrant {
    pub kind: CacheKind,
    pub path: PathBuf,
    pub mode: CacheMode,
}

impl CacheGrant {
    pub fn sandbox_path(&self) -> Result<PathBuf> {
        let home = home_dir()?;
        let home = fs::canonicalize(&home).unwrap_or(home);
        let path = fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        if let Ok(relative) = path.strip_prefix(&home) {
            return Ok(Path::new("/tmp/home").join(relative));
        }
        Ok(self.kind.fallback_sandbox_path())
    }

    pub fn shell_env(&self, sandbox_path: &Path) -> Option<(String, String)> {
        let value = sandbox_path.to_string_lossy().to_string();
        match self.kind {
            CacheKind::CargoRegistry | CacheKind::CargoGit => sandbox_path.parent().map(|parent| {
                (
                    "CARGO_HOME".to_owned(),
                    parent.to_string_lossy().to_string(),
                )
            }),
            CacheKind::Npm => Some(("NPM_CONFIG_CACHE".to_owned(), value)),
            CacheKind::Pnpm => Some(("npm_config_store_dir".to_owned(), value)),
            CacheKind::Yarn => Some(("YARN_CACHE_FOLDER".to_owned(), value)),
            CacheKind::Pip => Some(("PIP_CACHE_DIR".to_owned(), value)),
            CacheKind::Uv => Some(("UV_CACHE_DIR".to_owned(), value)),
            CacheKind::GoMod => Some(("GOMODCACHE".to_owned(), value)),
            CacheKind::GoBuild => Some(("GOCACHE".to_owned(), value)),
            CacheKind::Gradle => sandbox_path.parent().map(|parent| {
                (
                    "GRADLE_USER_HOME".to_owned(),
                    parent.to_string_lossy().to_string(),
                )
            }),
            CacheKind::Sccache => Some(("SCCACHE_DIR".to_owned(), value)),
            CacheKind::Ccache => Some(("CCACHE_DIR".to_owned(), value)),
            CacheKind::Maven => None,
        }
    }
}

#[derive(Clone, Debug)]
struct DiscoveredCache {
    kind: CacheKind,
    path: PathBuf,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "config_version")]
    pub version: u32,

    #[serde(default)]
    pub transports: TransportConfig,

    #[serde(default)]
    pub permissions: PermissionConfig,

    #[serde(default)]
    pub caches: Vec<CacheGrant>,

    #[serde(default)]
    pub tunnel_id: Option<String>,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub runtime_api_key: String,
    #[serde(default)]
    pub organization_id: Option<String>,
    #[serde(default = "default_base_url")]
    pub base_url: String,

    #[serde(default = "default_shell_timeout")]
    pub max_shell_timeout_secs: u64,
    #[serde(default = "default_output_bytes")]
    pub max_output_bytes: usize,
    #[serde(default = "default_file_bytes")]
    pub max_read_bytes: usize,
    #[serde(default = "default_file_bytes")]
    pub max_write_bytes: usize,
}

pub struct SetupResult {
    pub config: AppConfig,
    pub new_tunnel: bool,
    pub protected_paths: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct TunnelRecord {
    id: String,
}

fn config_version() -> u32 {
    8
}

fn default_true() -> bool {
    true
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
}

fn default_http_bind() -> String {
    DEFAULT_HTTP_BIND.to_owned()
}

fn default_shell_timeout() -> u64 {
    DEFAULT_MAX_SHELL_TIMEOUT_SECS
}

fn default_output_bytes() -> usize {
    DEFAULT_MAX_OUTPUT_BYTES
}

fn default_file_bytes() -> usize {
    DEFAULT_MAX_FILE_BYTES
}

pub async fn load_or_setup(force_setup: bool, profile: Option<&str>) -> Result<SetupResult> {
    validate_profile(profile)?;

    if !force_setup {
        if profile.is_none()
            && let Some(config) = config_from_env()?
        {
            let protected_paths = protected_paths_for_config(&config, profile)?;
            return Ok(SetupResult {
                config,
                new_tunnel: false,
                protected_paths,
            });
        }

        let path = config_path(profile)?;
        let legacy_path = legacy_json_config_path(profile)?;
        let source_path = if path.exists() {
            Some(path.clone())
        } else if legacy_path.exists() {
            Some(legacy_path)
        } else {
            None
        };

        if let Some(source_path) = source_path {
            let text = fs::read_to_string(&source_path)
                .with_context(|| format!("failed to read {}", source_path.display()))?;
            let mut config: AppConfig =
                if source_path.extension().and_then(|value| value.to_str()) == Some("jsonc") {
                    parse_jsonc(&text)
                        .with_context(|| format!("failed to parse {}", source_path.display()))?
                } else {
                    serde_json::from_str(&text)
                        .with_context(|| format!("failed to parse {}", source_path.display()))?
                };

            if config.transports.openai {
                config.runtime_api_key = read_saved_runtime_key(profile)?;
            }

            apply_nonsecret_env_overrides(&mut config)?;
            validate_config(&config)?;
            let protected_paths = protected_paths_for_config(&config, profile)?;
            return Ok(SetupResult {
                config,
                new_tunnel: false,
                protected_paths,
            });
        }
    }

    if !io::stdin().is_terminal() {
        let profile_hint = profile
            .map(|name| format!(" --profile {name}"))
            .unwrap_or_default();
        bail!(
            "abird-link needs first-run setup, but stdin is not interactive. Run 'abird-link --setup{profile_hint}' in a terminal first"
        );
    }

    interactive_setup(profile).await
}

fn config_from_env() -> Result<Option<AppConfig>> {
    let tunnel_id = env_first(&["ABIRD_LINK_ID", "CONTROL_PLANE_TUNNEL_ID"]);
    let runtime_api_key = env_first(&["ABIRD_LINK_API_KEY", "CONTROL_PLANE_API_KEY"]);

    let (Some(tunnel_id), Some(runtime_api_key)) = (tunnel_id, runtime_api_key) else {
        return Ok(None);
    };

    let mut config = AppConfig {
        version: config_version(),
        transports: TransportConfig::default(),
        permissions: PermissionConfig::default(),
        caches: Vec::new(),
        tunnel_id: Some(tunnel_id),
        runtime_api_key,
        organization_id: env_first(&[
            "ABIRD_LINK_ORGANIZATION_ID",
            "CONTROL_PLANE_ORGANIZATION_ID",
            "OPENAI_ORGANIZATION",
        ]),
        base_url: default_base_url(),
        max_shell_timeout_secs: default_shell_timeout(),
        max_output_bytes: default_output_bytes(),
        max_read_bytes: default_file_bytes(),
        max_write_bytes: default_file_bytes(),
    };
    apply_nonsecret_env_overrides(&mut config)?;
    validate_config(&config)?;
    Ok(Some(config))
}

fn apply_nonsecret_env_overrides(config: &mut AppConfig) -> Result<()> {
    if let Some(base_url) = env_first(&["ABIRD_LINK_BASE_URL", "CONTROL_PLANE_BASE_URL"]) {
        config.base_url = base_url;
    }
    if let Some(value) = env_first(&[
        "ABIRD_LINK_ORGANIZATION_ID",
        "CONTROL_PLANE_ORGANIZATION_ID",
        "OPENAI_ORGANIZATION",
    ]) {
        config.organization_id = Some(value);
    }
    Ok(())
}

async fn command_path(program: &str, args: &[&str]) -> Option<PathBuf> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let output = timeout(Duration::from_secs(2), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.lines().next()?.trim();
    if value.is_empty() || matches!(value, "undefined" | "null") {
        return None;
    }
    Some(PathBuf::from(value))
}

fn expand_home_path(path: PathBuf, home: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home.to_path_buf();
    }
    if let Some(relative) = text.strip_prefix("~/") {
        return home.join(relative);
    }
    path
}

fn add_cache_candidate(
    found: &mut BTreeMap<CacheKind, PathBuf>,
    kind: CacheKind,
    path: PathBuf,
    home: &Path,
) {
    let path = expand_home_path(path, home);
    if path.is_dir() {
        let path = fs::canonicalize(&path).unwrap_or(path);
        found.entry(kind).or_insert(path);
    }
}

async fn discover_developer_caches() -> Vec<DiscoveredCache> {
    let Ok(home) = home_dir() else {
        return Vec::new();
    };
    let xdg_cache = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cache"));

    let mut found = BTreeMap::new();

    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"));
    add_cache_candidate(
        &mut found,
        CacheKind::CargoRegistry,
        cargo_home.join("registry"),
        &home,
    );
    add_cache_candidate(
        &mut found,
        CacheKind::CargoGit,
        cargo_home.join("git"),
        &home,
    );

    let npm = env::var_os("NPM_CONFIG_CACHE").map(PathBuf::from);
    if let Some(path) = npm {
        add_cache_candidate(&mut found, CacheKind::Npm, path, &home);
    } else if let Some(path) = command_path("npm", &["config", "get", "cache"]).await {
        add_cache_candidate(&mut found, CacheKind::Npm, path, &home);
    } else {
        add_cache_candidate(&mut found, CacheKind::Npm, home.join(".npm"), &home);
    }

    if let Some(path) = env::var_os("PNPM_STORE_DIR")
        .or_else(|| env::var_os("npm_config_store_dir"))
        .map(PathBuf::from)
    {
        add_cache_candidate(&mut found, CacheKind::Pnpm, path, &home);
    } else {
        add_cache_candidate(
            &mut found,
            CacheKind::Pnpm,
            home.join(".local/share/pnpm/store"),
            &home,
        );
    }

    if let Some(path) = env::var_os("YARN_CACHE_FOLDER").map(PathBuf::from) {
        add_cache_candidate(&mut found, CacheKind::Yarn, path, &home);
    } else {
        add_cache_candidate(&mut found, CacheKind::Yarn, xdg_cache.join("yarn"), &home);
    }

    if let Some(path) = env::var_os("PIP_CACHE_DIR").map(PathBuf::from) {
        add_cache_candidate(&mut found, CacheKind::Pip, path, &home);
    } else if let Some(path) = command_path("pip", &["cache", "dir"]).await {
        add_cache_candidate(&mut found, CacheKind::Pip, path, &home);
    } else {
        add_cache_candidate(&mut found, CacheKind::Pip, xdg_cache.join("pip"), &home);
    }

    if let Some(path) = env::var_os("UV_CACHE_DIR").map(PathBuf::from) {
        add_cache_candidate(&mut found, CacheKind::Uv, path, &home);
    } else if let Some(path) = command_path("uv", &["cache", "dir"]).await {
        add_cache_candidate(&mut found, CacheKind::Uv, path, &home);
    } else {
        add_cache_candidate(&mut found, CacheKind::Uv, xdg_cache.join("uv"), &home);
    }

    let go_mod = env::var_os("GOMODCACHE").map(PathBuf::from);
    if let Some(path) = go_mod {
        add_cache_candidate(&mut found, CacheKind::GoMod, path, &home);
    } else if let Some(path) = command_path("go", &["env", "GOMODCACHE"]).await {
        add_cache_candidate(&mut found, CacheKind::GoMod, path, &home);
    } else {
        add_cache_candidate(&mut found, CacheKind::GoMod, home.join("go/pkg/mod"), &home);
    }

    let go_build = env::var_os("GOCACHE").map(PathBuf::from);
    if let Some(path) = go_build {
        add_cache_candidate(&mut found, CacheKind::GoBuild, path, &home);
    } else if let Some(path) = command_path("go", &["env", "GOCACHE"]).await {
        add_cache_candidate(&mut found, CacheKind::GoBuild, path, &home);
    } else {
        add_cache_candidate(
            &mut found,
            CacheKind::GoBuild,
            xdg_cache.join("go-build"),
            &home,
        );
    }

    add_cache_candidate(
        &mut found,
        CacheKind::Maven,
        home.join(".m2/repository"),
        &home,
    );

    let gradle_home = env::var_os("GRADLE_USER_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".gradle"));
    add_cache_candidate(
        &mut found,
        CacheKind::Gradle,
        gradle_home.join("caches"),
        &home,
    );

    let sccache = env::var_os("SCCACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg_cache.join("sccache"));
    add_cache_candidate(&mut found, CacheKind::Sccache, sccache, &home);

    let ccache = env::var_os("CCACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg_cache.join("ccache"));
    add_cache_candidate(&mut found, CacheKind::Ccache, ccache, &home);

    found
        .into_iter()
        .map(|(kind, path)| DiscoveredCache { kind, path })
        .collect()
}

fn prompt_cache_mode(cache: &DiscoveredCache) -> Result<Option<CacheMode>> {
    loop {
        let input = prompt_line(&format!(
            "   {:<18} {}\n      Access [n]one / [r]ead-only / read+[w]rite [n]: ",
            cache.kind.label(),
            cache.path.display()
        ))?;

        match input.trim().to_ascii_lowercase().as_str() {
            "" | "n" | "none" | "no" => return Ok(None),
            "r" | "ro" | "read" | "read-only" => return Ok(Some(CacheMode::ReadOnly)),
            "w" | "rw" | "write" | "read-write" | "read+write" => {
                return Ok(Some(CacheMode::ReadWrite));
            }
            _ => println!("      Please enter n, r, or w."),
        }
    }
}

async fn setup_developer_caches(allow_shell: bool) -> Result<Vec<CacheGrant>> {
    if !allow_shell || !cfg!(target_os = "linux") {
        return Ok(Vec::new());
    }

    println!();
    println!("3. Discover developer caches");
    println!("   Scanning known package/build cache locations…");

    let discovered = discover_developer_caches().await;
    if discovered.is_empty() {
        println!("   • No supported existing caches found.");
        return Ok(Vec::new());
    }

    println!(
        "   Found {} cache{}:",
        discovered.len(),
        if discovered.len() == 1 { "" } else { "s" }
    );
    for cache in &discovered {
        println!("   • {:<18} {}", cache.kind.label(), cache.path.display());
    }
    println!(
        "   • Only cache directories are shared; adjacent credentials/config files are excluded."
    );
    println!(
        "   • read+write is fastest, but allows sandboxed builds to modify the shared host cache."
    );

    if !prompt_yes_no("   Configure access to discovered caches? [y/N]: ", false)? {
        return Ok(Vec::new());
    }

    let mut grants = Vec::new();
    for cache in discovered {
        if let Some(mode) = prompt_cache_mode(&cache)? {
            grants.push(CacheGrant {
                kind: cache.kind,
                path: cache.path,
                mode,
            });
        }
    }
    Ok(grants)
}

async fn interactive_setup(profile: Option<&str>) -> Result<SetupResult> {
    println!();
    println!("abird-link setup");
    println!("────────────────────────────────────────────────────────");
    if let Some(profile) = profile {
        println!("Profile: {profile}");
        println!();
    }

    println!("1. Choose MCP transports");
    println!("   • openai — OpenAI Secure MCP Tunnel; starts automatically");
    println!("   • stdio  — local stdio MCP server; can also be added with --stdio");
    println!("   • http   — local HTTP MCP server; can also be added with --http");
    println!("   • Enter comma-separated names, 'all', or 'none'.");
    let selection = prompt_line("   Enabled [openai]: ")?;
    let transports = parse_transport_selection(&selection)?;

    println!();
    println!("2. Choose default local permissions");
    println!("   • cwd is always readable unless a deny rule overrides it.");
    let setup_cwd = env::current_dir().context("failed to determine setup cwd")?;
    let persist_cwd = prompt_yes_no(
        &format!("   Pin this profile to {}? [y/N]: ", setup_cwd.display()),
        false,
    )?;
    let allow_rw_cwd = prompt_yes_no("   Allow read+write cwd by default? [y/N]: ", false)?;
    let allow_shell = prompt_yes_no("   Allow shell by default? [y/N]: ", false)?;
    let allow_network = if allow_shell {
        println!("   • Linux shell remains Bubblewrap-sandboxed.");
        prompt_yes_no("   Allow shell network access by default? [y/N]: ", false)?
    } else {
        false
    };
    let permissions = PermissionConfig {
        cwd: persist_cwd.then_some(setup_cwd),
        allow_read: Vec::new(),
        allow_write: Vec::new(),
        allow_rw: allow_rw_cwd
            .then(|| PathBuf::from("."))
            .into_iter()
            .collect(),
        deny_read: Vec::new(),
        deny_write: Vec::new(),
        deny_rw: Vec::new(),
        allow_shell,
        allow_network,
    };
    let caches = setup_developer_caches(allow_shell).await?;

    let (tunnel_id, runtime_api_key, organization_id, new_tunnel) = if transports.openai {
        setup_openai().await?
    } else {
        (None, String::new(), None, false)
    };

    let config = AppConfig {
        version: config_version(),
        transports,
        permissions,
        caches,
        tunnel_id,
        runtime_api_key,
        organization_id,
        base_url: default_base_url(),
        max_shell_timeout_secs: default_shell_timeout(),
        max_output_bytes: default_output_bytes(),
        max_read_bytes: default_file_bytes(),
        max_write_bytes: default_file_bytes(),
    };

    validate_config(&config)?;
    save_config(&config, profile)?;

    println!();
    println!("✓ Setup complete");
    println!("  • Config: {}", config_path(profile)?.display());
    let enabled = config.transports.enabled_names();
    println!(
        "  • Configured transports: {}",
        if enabled.is_empty() {
            "none".to_owned()
        } else {
            enabled.join(", ")
        }
    );
    println!(
        "  • Default permissions: read cwd{}{}{}",
        if config
            .permissions
            .allow_rw
            .iter()
            .any(|path| path == Path::new("."))
        {
            " + write cwd"
        } else {
            ""
        },
        if config.permissions.allow_shell {
            " + shell"
        } else {
            ""
        },
        if config.permissions.allow_network {
            " + network"
        } else {
            ""
        },
    );
    if !config.caches.is_empty() {
        println!("  • Shared developer caches:");
        for cache in &config.caches {
            println!(
                "      {:<18} {:<10} {}",
                cache.kind.label(),
                cache.mode.label(),
                cache.path.display()
            );
        }
    }
    if config.transports.openai {
        println!("  • Runtime key saved securely for this profile.");
        if new_tunnel {
            println!("  • Admin key was not saved — you can delete it now.");
        }
    } else {
        println!("  • OpenAI setup skipped; no OpenAI credentials are required.");
    }
    if config.transports.http {
        println!(
            "  • HTTP default: http://{}/mcp",
            config.transports.http_bind
        );
    }

    let protected_paths = protected_paths_for_config(&config, profile)?;
    Ok(SetupResult {
        config,
        new_tunnel,
        protected_paths,
    })
}

fn parse_transport_selection(input: &str) -> Result<TransportConfig> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(TransportConfig::default());
    }

    let mut config = TransportConfig {
        openai: false,
        stdio: false,
        http: false,
        http_bind: default_http_bind(),
    };

    for token in input.split([',', ' ', ';']) {
        let token = token.trim().to_ascii_lowercase();
        if token.is_empty() {
            continue;
        }
        match token.as_str() {
            "all" => {
                config.openai = true;
                config.stdio = true;
                config.http = true;
            }
            "none" => {
                config.openai = false;
                config.stdio = false;
                config.http = false;
            }
            "openai" | "tunnel" => config.openai = true,
            "stdio" => config.stdio = true,
            "http" => config.http = true,
            other => bail!("unknown transport {other:?}; choose openai, stdio, http, all, or none"),
        }
    }

    Ok(config)
}

async fn setup_openai() -> Result<(Option<String>, String, Option<String>, bool)> {
    println!();
    println!("Configure OpenAI Secure MCP Tunnel");
    println!("   Create a Runtime API key");
    println!("   • Permissions: Tunnels Read + Use");
    println!("   • https://platform.openai.com/settings/organization/api-keys");
    let runtime_api_key = Zeroizing::new(prompt_secret("   Paste key: ")?);
    if runtime_api_key.trim().is_empty() {
        bail!("runtime API key cannot be empty");
    }

    println!();
    println!("   Choose a tunnel");
    println!("   • Paste an existing Tunnel ID, or press Enter to create one.");
    let existing_id = prompt_line("   Tunnel ID [create new]: ")?;

    let (tunnel_id, new_tunnel, runtime_organization_id) = if existing_id.trim().is_empty() {
        println!();
        println!("   Create a one-time Admin key");
        println!("   • Permission: Tunnels Manage");
        println!("   • Used once and never saved; you can delete it after setup.");
        println!("   • https://platform.openai.com/settings/organization/admin-keys");
        let admin_key = Zeroizing::new(prompt_secret("   Paste key: ")?);
        if admin_key.trim().is_empty() {
            bail!("admin API key cannot be empty when creating a tunnel");
        }

        println!();
        println!("   Choose tunnel scope");
        println!("   • Workspace ID:    https://chatgpt.com/admin");
        println!("     Select your ChatGPT workspace → Settings, then copy Workspace ID.");
        println!("   • Organization ID: https://platform.openai.com/settings/organization/general");
        println!("   • Paste a Workspace ID, or press Enter to use your Organization ID.");
        let workspace_id = prompt_line("   ChatGPT workspace ID [optional]: ")?;
        let organization_id = if workspace_id.trim().is_empty() {
            prompt_line("   OpenAI organization ID: ")?
        } else {
            String::new()
        };
        if workspace_id.trim().is_empty() && organization_id.trim().is_empty() {
            bail!("a workspace ID or organization ID is required to create a tunnel");
        }

        print!("   Creating tunnel… ");
        io::stdout().flush()?;
        let tunnel_id = create_tunnel(
            DEFAULT_BASE_URL,
            &admin_key,
            workspace_id.trim(),
            organization_id.trim(),
        )
        .await?;
        println!("done");
        println!("   ✓ {tunnel_id}");

        (
            tunnel_id,
            true,
            if organization_id.trim().is_empty() {
                None
            } else {
                Some(organization_id.trim().to_owned())
            },
        )
    } else {
        validate_tunnel_id(existing_id.trim())?;
        (existing_id.trim().to_owned(), false, None)
    };

    Ok((
        Some(tunnel_id),
        runtime_api_key.as_str().to_owned(),
        runtime_organization_id,
        new_tunnel,
    ))
}

async fn create_tunnel(
    base_url: &str,
    admin_key: &str,
    workspace_id: &str,
    organization_id: &str,
) -> Result<String> {
    let host = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "local-machine".to_owned());

    let mut body = serde_json::json!({
        "name": format!("abird-link · {host}"),
        "description": "Permission-scoped local MCP access via abird-link"
    });
    if !workspace_id.is_empty() {
        body["workspace_ids"] = serde_json::json!([workspace_id]);
    }
    if !organization_id.is_empty() {
        body["organization_ids"] = serde_json::json!([organization_id]);
    }

    let client = reqwest::Client::builder()
        .user_agent(format!("abird-link/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let url = format!("{}/v1/tunnels", base_url.trim_end_matches('/'));
    let response = client
        .post(url)
        .bearer_auth(admin_key)
        .json(&body)
        .send()
        .await
        .context("failed to contact the OpenAI tunnel management API")?;

    if !response.status().is_success() {
        let status = response.status();
        let message = bounded(&response.text().await.unwrap_or_default(), 1000);
        bail!("OpenAI tunnel creation failed ({status}): {message}");
    }

    let record: TunnelRecord = response
        .json()
        .await
        .context("OpenAI returned an invalid tunnel creation response")?;
    validate_tunnel_id(&record.id)?;
    Ok(record.id)
}

fn save_config(config: &AppConfig, profile: Option<&str>) -> Result<()> {
    let path = config_path(profile)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("configuration path has no parent"))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    set_private_dir_permissions(parent)?;

    let text = if path.extension().and_then(|value| value.to_str()) == Some("json") {
        let mut text = serde_json::to_string_pretty(config)?;
        text.push('\n');
        text
    } else {
        serialize_jsonc(config)?
    };
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("config.jsonc"),
        std::process::id()
    ));

    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(&temp)
        .with_context(|| format!("failed to create {}", temp.display()))?;
    file.write_all(text.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);

    set_private_file_permissions(&temp)?;
    fs::rename(&temp, &path).with_context(|| format!("failed to replace {}", path.display()))?;
    set_private_file_permissions(&path)?;

    if config.transports.openai {
        save_runtime_key(&config.runtime_api_key, profile)?;
    } else {
        remove_saved_runtime_key(profile)?;
    }

    Ok(())
}

fn parse_jsonc<T: DeserializeOwned>(text: &str) -> Result<T> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let without_comments = strip_jsonc_comments(text)?;
    let normalized = strip_jsonc_trailing_commas(&without_comments);
    serde_json::from_str(&normalized).context("invalid JSONC")
}

fn serialize_jsonc<T: Serialize>(value: &T) -> Result<String> {
    let body = serde_json::to_string_pretty(value)?;
    Ok(format!(
        "// abird-link configuration (JSONC)\n// Comments and trailing commas are allowed.\n{body}\n"
    ))
}

fn strip_jsonc_comments(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];

        if in_string {
            output.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        if byte == b'"' {
            in_string = true;
            output.push(byte);
            index += 1;
            continue;
        }

        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'/' {
            output.extend_from_slice(b"  ");
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                output.push(if bytes[index] == b'\r' { b'\r' } else { b' ' });
                index += 1;
            }
            continue;
        }

        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
            output.extend_from_slice(b"  ");
            index += 2;
            let mut closed = false;
            while index < bytes.len() {
                if index + 1 < bytes.len() && bytes[index] == b'*' && bytes[index + 1] == b'/' {
                    output.extend_from_slice(b"  ");
                    index += 2;
                    closed = true;
                    break;
                }

                output.push(match bytes[index] {
                    b'\n' => b'\n',
                    b'\r' => b'\r',
                    _ => b' ',
                });
                index += 1;
            }
            if !closed {
                bail!("unterminated block comment in JSONC");
            }
            continue;
        }

        output.push(byte);
        index += 1;
    }

    Ok(String::from_utf8(output)?)
}

fn strip_jsonc_trailing_commas(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];

        if in_string {
            output.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        if byte == b'"' {
            in_string = true;
            output.push(byte);
            index += 1;
            continue;
        }

        if byte == b',' {
            let mut lookahead = index + 1;
            while lookahead < bytes.len() && bytes[lookahead].is_ascii_whitespace() {
                lookahead += 1;
            }
            if lookahead < bytes.len() && matches!(bytes[lookahead], b'}' | b']') {
                index += 1;
                continue;
            }
        }

        output.push(byte);
        index += 1;
    }

    String::from_utf8(output).expect("JSONC sanitizer preserves UTF-8")
}

fn profiled_path(base: &Path, profile: Option<&str>) -> Result<PathBuf> {
    let Some(profile) = profile else {
        return Ok(base.to_path_buf());
    };
    validate_profile(Some(profile))?;

    let parent = base
        .parent()
        .ok_or_else(|| anyhow!("configuration path has no parent"))?;
    let stem = base
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("configuration filename is invalid"))?;
    let extension = base.extension().and_then(|value| value.to_str());

    let filename = match extension {
        Some(extension) if !extension.is_empty() => format!("{stem}.{profile}.{extension}"),
        _ => format!("{stem}.{profile}"),
    };
    Ok(parent.join(filename))
}

fn config_path(profile: Option<&str>) -> Result<PathBuf> {
    let base = if let Some(path) = env::var_os("ABIRD_LINK_CONFIG") {
        PathBuf::from(path)
    } else if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("abird-link/config.jsonc")
    } else {
        home_dir()?.join(".config/abird-link/config.jsonc")
    };
    profiled_path(&base, profile)
}

fn legacy_json_config_path(profile: Option<&str>) -> Result<PathBuf> {
    let path = config_path(profile)?;
    match path.extension().and_then(|value| value.to_str()) {
        Some("json") => Ok(path),
        Some("jsonc") => Ok(path.with_extension("json")),
        _ => Ok(PathBuf::from(format!("{}.json", path.display()))),
    }
}

fn credential_path(profile: Option<&str>) -> Result<PathBuf> {
    let config = config_path(profile)?;
    let parent = config
        .parent()
        .ok_or_else(|| anyhow!("credential path has no parent"))?;
    Ok(match profile {
        Some(profile) => parent.join(format!("runtime.{profile}.key")),
        None => parent.join("runtime.key"),
    })
}

fn runtime_key_paths(parent: &Path) -> Result<Vec<PathBuf>> {
    let mut protected = Vec::new();
    if !parent.exists() {
        return Ok(protected);
    }

    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "runtime.key" || (name.starts_with("runtime.") && name.ends_with(".key")) {
            protected.push(entry.path());
        }
    }
    protected.sort();
    Ok(protected)
}

fn protected_paths_for_config(_config: &AppConfig, profile: Option<&str>) -> Result<Vec<PathBuf>> {
    let config = config_path(profile)?;
    let parent = config
        .parent()
        .ok_or_else(|| anyhow!("configuration path has no parent"))?;

    let mut protected = runtime_key_paths(parent)?;
    let current = credential_path(profile)?;
    if current.exists() && !protected.iter().any(|path| path == &current) {
        protected.push(current);
    }
    Ok(protected)
}

fn save_runtime_key(key: &str, profile: Option<&str>) -> Result<()> {
    let path = credential_path(profile)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("credential path has no parent"))?;
    fs::create_dir_all(parent)?;
    set_private_dir_permissions(parent)?;

    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(&path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(key.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    set_private_file_permissions(&path)?;
    Ok(())
}

fn remove_saved_runtime_key(profile: Option<&str>) -> Result<()> {
    let path = credential_path(profile)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn read_saved_runtime_key(profile: Option<&str>) -> Result<String> {
    let path = credential_path(profile)?;
    let key = fs::read_to_string(&path).with_context(|| {
        format!(
            "failed to read {}; run 'abird-link --setup{}' to repair credentials",
            path.display(),
            profile
                .map(|name| format!(" --profile {name}"))
                .unwrap_or_default()
        )
    })?;
    let key = key.trim().to_owned();
    if key.is_empty() {
        bail!("saved runtime API key is empty; rerun abird-link setup");
    }
    Ok(key)
}

fn validate_profile(profile: Option<&str>) -> Result<()> {
    let Some(profile) = profile else {
        return Ok(());
    };
    if profile.is_empty()
        || profile.len() > 64
        || !profile
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        bail!("invalid profile name; use 1-64 letters, digits, '-' or '_'");
    }
    Ok(())
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
        .ok_or_else(|| anyhow!("could not determine the user's home directory"))
}

fn validate_config(config: &AppConfig) -> Result<()> {
    if config.permissions.allow_network && !config.permissions.allow_shell {
        bail!("permissions.allow_network requires permissions.allow_shell");
    }

    let mut cache_kinds = std::collections::BTreeSet::new();
    for cache in &config.caches {
        if !cache.path.is_absolute() {
            bail!(
                "cache path for {} must be absolute: {}",
                cache.kind.label(),
                cache.path.display()
            );
        }
        if !cache_kinds.insert(cache.kind) {
            bail!("duplicate cache grant for {}", cache.kind.label());
        }
    }

    config
        .transports
        .http_bind
        .parse::<SocketAddr>()
        .with_context(|| {
            format!(
                "invalid HTTP bind address {:?}",
                config.transports.http_bind
            )
        })?;

    if config.transports.openai {
        let tunnel_id = config
            .tunnel_id
            .as_deref()
            .ok_or_else(|| anyhow!("OpenAI transport is enabled but tunnel_id is missing"))?;
        validate_tunnel_id(tunnel_id)?;

        if config.runtime_api_key.trim().is_empty() {
            bail!("OpenAI transport is enabled but runtime API key is empty");
        }
        if !(config.base_url.starts_with("https://")
            || config.base_url.starts_with("http://localhost"))
        {
            bail!("control-plane base URL must use HTTPS (localhost is allowed for testing)");
        }
    }

    Ok(())
}

fn validate_tunnel_id(id: &str) -> Result<()> {
    let Some(suffix) = id.strip_prefix("tunnel_") else {
        bail!("invalid Tunnel ID: expected an identifier beginning with tunnel_");
    };
    if suffix.is_empty()
        || id.len() > 256
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("invalid Tunnel ID");
    }
    Ok(())
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_owned())
}

fn prompt_yes_no(prompt: &str, default: bool) -> Result<bool> {
    loop {
        let value = prompt_line(prompt)?;
        if value.is_empty() {
            return Ok(default);
        }
        match value.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" | "1" => return Ok(true),
            "n" | "no" | "false" | "0" => return Ok(false),
            _ => println!("   Please enter y or n."),
        }
    }
}

fn prompt_secret(prompt: &str) -> Result<String> {
    rpassword::prompt_password(prompt).context("failed to read secret from terminal")
}

fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn bounded(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_config() -> AppConfig {
        AppConfig {
            version: config_version(),
            transports: TransportConfig::default(),
            permissions: PermissionConfig::default(),
            caches: Vec::new(),
            tunnel_id: Some("tunnel_0123456789abcdef0123456789abcdef".to_owned()),
            runtime_api_key: "secret-runtime-key".to_owned(),
            organization_id: None,
            base_url: DEFAULT_BASE_URL.to_owned(),
            max_shell_timeout_secs: 120,
            max_output_bytes: 1024,
            max_read_bytes: 1024,
            max_write_bytes: 1024,
        }
    }

    #[test]
    fn transport_selection_defaults_to_openai() {
        let transports = parse_transport_selection("").unwrap();
        assert!(transports.openai);
        assert!(!transports.stdio);
        assert!(!transports.http);
    }

    #[test]
    fn transport_selection_is_modular_and_allows_none() {
        let transports = parse_transport_selection("stdio,http").unwrap();
        assert!(!transports.openai);
        assert!(transports.stdio);
        assert!(transports.http);

        let none = parse_transport_selection("none").unwrap();
        assert!(!none.openai && !none.stdio && !none.http);
    }

    #[test]
    fn all_profile_runtime_keys_are_protected() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("runtime.key"), b"default").unwrap();
        std::fs::write(temp.path().join("runtime.work.key"), b"work").unwrap();
        std::fs::write(temp.path().join("runtime.personal.key"), b"personal").unwrap();
        std::fs::write(temp.path().join("runtime.txt"), b"not-a-key").unwrap();

        let paths = runtime_key_paths(temp.path()).unwrap();
        assert_eq!(paths.len(), 3);
        assert!(paths.contains(&temp.path().join("runtime.key")));
        assert!(paths.contains(&temp.path().join("runtime.work.key")));
        assert!(paths.contains(&temp.path().join("runtime.personal.key")));
    }

    #[test]
    fn cache_candidates_require_existing_directories() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let existing = home.join(".cargo/registry");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(home.join("not-a-dir"), b"x").unwrap();

        let mut found = BTreeMap::new();
        add_cache_candidate(&mut found, CacheKind::CargoRegistry, existing.clone(), home);
        add_cache_candidate(&mut found, CacheKind::Npm, home.join("not-a-dir"), home);
        add_cache_candidate(&mut found, CacheKind::Uv, home.join("missing"), home);

        assert_eq!(found.get(&CacheKind::CargoRegistry), Some(&existing));
        assert!(!found.contains_key(&CacheKind::Npm));
        assert!(!found.contains_key(&CacheKind::Uv));
    }

    #[test]
    fn cache_grants_map_home_paths_into_private_sandbox_home() {
        let home = home_dir().unwrap();
        let cargo = CacheGrant {
            kind: CacheKind::CargoRegistry,
            path: home.join(".cargo/registry"),
            mode: CacheMode::ReadOnly,
        };
        let target = cargo.sandbox_path().unwrap();
        assert_eq!(target, PathBuf::from("/tmp/home/.cargo/registry"));
        assert_eq!(
            cargo.shell_env(&target),
            Some(("CARGO_HOME".to_owned(), "/tmp/home/.cargo".to_owned()))
        );

        let npm = CacheGrant {
            kind: CacheKind::Npm,
            path: home.join(".npm"),
            mode: CacheMode::ReadWrite,
        };
        let target = npm.sandbox_path().unwrap();
        assert_eq!(target, PathBuf::from("/tmp/home/.npm"));
        assert_eq!(
            npm.shell_env(&target),
            Some(("NPM_CONFIG_CACHE".to_owned(), "/tmp/home/.npm".to_owned()))
        );
        assert!(npm.mode.writable());
        assert!(!cargo.mode.writable());
    }

    #[test]
    fn cache_validation_rejects_relative_and_duplicate_grants() {
        let mut config = example_config();
        config.caches = vec![CacheGrant {
            kind: CacheKind::Npm,
            path: PathBuf::from(".npm"),
            mode: CacheMode::ReadOnly,
        }];
        assert!(validate_config(&config).is_err());

        let home = home_dir().unwrap();
        config.caches = vec![
            CacheGrant {
                kind: CacheKind::Npm,
                path: home.join(".npm"),
                mode: CacheMode::ReadOnly,
            },
            CacheGrant {
                kind: CacheKind::Npm,
                path: home.join(".npm-alt"),
                mode: CacheMode::ReadWrite,
            },
        ];
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn profile_paths_are_predictable() {
        let base = Path::new("/tmp/abird-link/config.jsonc");
        assert_eq!(
            profiled_path(base, None).unwrap(),
            PathBuf::from("/tmp/abird-link/config.jsonc")
        );
        assert_eq!(
            profiled_path(base, Some("work")).unwrap(),
            PathBuf::from("/tmp/abird-link/config.work.jsonc")
        );
        assert!(profiled_path(base, Some("../bad")).is_err());
    }

    #[test]
    fn jsonc_supports_comments_trailing_commas_and_comment_markers_in_strings() {
        let value: serde_json::Value = parse_jsonc(
            r#"{
                // line comment
                "url": "https://example.com/a//b",
                "items": [
                    1,
                    2,
                ],
                /* block comment */
                "nested": {
                    "enabled": true,
                },
            }"#,
        )
        .unwrap();

        assert_eq!(value["url"].as_str(), Some("https://example.com/a//b"));
        assert_eq!(value["items"], serde_json::json!([1, 2]));
        assert_eq!(value["nested"]["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn jsonc_rejects_unterminated_block_comment() {
        let result = parse_jsonc::<serde_json::Value>("{ /* never closed");
        assert!(result.is_err());
    }

    #[test]
    fn tunnel_id_validation_keeps_ids_opaque() {
        assert!(validate_tunnel_id("tunnel_0123456789abcdef0123456789abcdef").is_ok());
        assert!(validate_tunnel_id("tunnel_future-format-v2").is_ok());
        assert!(validate_tunnel_id("tunnel_bad id").is_err());
        assert!(validate_tunnel_id("tunnel_bad/path").is_err());
        assert!(validate_tunnel_id("nope").is_err());
    }

    #[test]
    fn runtime_key_is_never_serialized_into_jsonc() {
        let serialized = serialize_jsonc(&example_config()).unwrap();
        assert!(serialized.starts_with("// abird-link configuration (JSONC)"));
        assert!(!serialized.contains("secret-runtime-key"));
        assert!(!serialized.contains("runtime_api_key"));
    }

    #[test]
    fn local_only_config_needs_no_openai_fields() {
        let mut config = example_config();
        config.transports.openai = false;
        config.transports.stdio = true;
        config.tunnel_id = None;
        config.runtime_api_key.clear();
        validate_config(&config).unwrap();
    }

    #[test]
    fn legacy_boolean_allow_rw_migrates_to_cwd_path() {
        let parsed: AppConfig = parse_jsonc(
            r#"{
                "version": 7,
                "transports": {
                    "openai": false,
                    "stdio": true,
                    "http": false,
                    "http_bind": "127.0.0.1:3000"
                },
                "permissions": {
                    "allow_rw": true,
                    "allow_shell": false,
                    "allow_network": false
                },
                "base_url": "https://api.openai.com"
            }"#,
        )
        .unwrap();

        assert_eq!(parsed.permissions.allow_rw, [PathBuf::from(".")]);
    }

    #[test]
    fn permissions_round_trip_in_jsonc() {
        let mut config = example_config();
        config.permissions = PermissionConfig {
            cwd: Some(PathBuf::from("/workspace")),
            allow_read: vec![PathBuf::from("/reference")],
            allow_write: vec![PathBuf::from("generated")],
            allow_rw: vec![PathBuf::from(".")],
            deny_read: vec![PathBuf::from("private")],
            deny_write: vec![PathBuf::from("locked")],
            deny_rw: vec![PathBuf::from("secret")],
            allow_shell: true,
            allow_network: true,
        };

        let serialized = serialize_jsonc(&config).unwrap();
        let parsed: AppConfig = parse_jsonc(&serialized).unwrap();
        assert_eq!(parsed.permissions.cwd, Some(PathBuf::from("/workspace")));
        assert_eq!(parsed.permissions.allow_read, [PathBuf::from("/reference")]);
        assert_eq!(parsed.permissions.allow_write, [PathBuf::from("generated")]);
        assert_eq!(parsed.permissions.allow_rw, [PathBuf::from(".")]);
        assert_eq!(parsed.permissions.deny_read, [PathBuf::from("private")]);
        assert_eq!(parsed.permissions.deny_write, [PathBuf::from("locked")]);
        assert_eq!(parsed.permissions.deny_rw, [PathBuf::from("secret")]);
        assert!(parsed.permissions.allow_shell);
        assert!(parsed.permissions.allow_network);
    }

    #[test]
    fn network_default_requires_shell_default() {
        let mut config = example_config();
        config.permissions.allow_network = true;
        config.permissions.allow_shell = false;

        assert!(validate_config(&config).is_err());
    }
}
