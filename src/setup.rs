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
    #[serde(default)]
    pub http_ephemeral_url: bool,
    #[serde(default)]
    pub ngrok: bool,
    #[serde(default)]
    pub ngrok_ephemeral_url: bool,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            openai: true,
            stdio: false,
            http: false,
            http_bind: default_http_bind(),
            http_ephemeral_url: false,
            ngrok: false,
            ngrok_ephemeral_url: false,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionConfig {
    /// Read the directory where dotlink is launched unless disabled.
    #[serde(default = "default_true")]
    pub default_allow: bool,

    /// Additional readable directories. Relative paths resolve from the launch directory.
    #[serde(default)]
    pub allow_read: Vec<PathBuf>,

    /// Additional writable directories. Relative paths resolve from the launch directory.
    #[serde(default)]
    pub allow_write: Vec<PathBuf>,

    /// Additional read+write directories. Relative paths resolve from the launch directory.
    #[serde(default)]
    pub allow_rw: Vec<PathBuf>,

    /// Paths denied for reads. Relative paths resolve from the launch directory.
    #[serde(default)]
    pub deny_read: Vec<PathBuf>,

    /// Paths denied for writes. Relative paths resolve from the launch directory.
    #[serde(default)]
    pub deny_write: Vec<PathBuf>,

    /// Paths denied for both reads and writes. Relative paths resolve from the launch directory.
    #[serde(default)]
    pub deny_rw: Vec<PathBuf>,

    /// Expose the platform shell by default. Linux still uses Bubblewrap.
    #[serde(default)]
    pub allow_shell: bool,

    /// Allow network access from the sandboxed shell by default.
    #[serde(default)]
    pub allow_network: bool,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            default_allow: true,
            allow_read: Vec::new(),
            allow_write: Vec::new(),
            allow_rw: Vec::new(),
            deny_read: Vec::new(),
            deny_write: Vec::new(),
            deny_rw: Vec::new(),
            allow_shell: false,
            allow_network: false,
        }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum ProfileRuleKind {
    Read,
    Write,
    Rw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum ProfileBool {
    Openai,
    Stdio,
    Http,
    #[value(name = "http-ephemeral-url", alias = "http-ephemeral")]
    HttpEphemeral,
    Ngrok,
    #[value(name = "ngrok-ephemeral-url", alias = "ngrok-ephemeral")]
    NgrokEphemeral,
    #[value(alias = "default-read")]
    DefaultAllow,
    Shell,
    Network,
}

#[derive(Debug, Deserialize)]
struct TunnelRecord {
    id: String,
}

fn config_version() -> u32 {
    9
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

fn default_app_config() -> AppConfig {
    AppConfig {
        version: config_version(),
        transports: TransportConfig::default(),
        permissions: PermissionConfig::default(),
        caches: Vec::new(),
        tunnel_id: None,
        runtime_api_key: String::new(),
        organization_id: None,
        base_url: default_base_url(),
        max_shell_timeout_secs: default_shell_timeout(),
        max_output_bytes: default_output_bytes(),
        max_read_bytes: default_file_bytes(),
        max_write_bytes: default_file_bytes(),
    }
}

fn read_profile_config(profile: Option<&str>) -> Result<Option<AppConfig>> {
    let path = config_path(profile)?;
    if !path.exists() {
        return Ok(None);
    }
    let text =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let config: AppConfig =
        parse_jsonc(&text).with_context(|| format!("failed to parse {}", path.display()))?;
    validate_config_structure(&config)?;
    Ok(Some(config))
}

fn load_profile_for_edit(profile: Option<&str>) -> Result<Option<AppConfig>> {
    let Some(mut config) = read_profile_config(profile)? else {
        return Ok(None);
    };
    config.runtime_api_key = try_read_saved_runtime_key(profile)?.unwrap_or_default();
    Ok(Some(config))
}

pub async fn load_or_setup(
    force_setup: bool,
    profile: Option<&str>,
    color: bool,
) -> Result<Option<SetupResult>> {
    validate_profile(profile)?;

    if !force_setup {
        if profile.is_none()
            && let Some(config) = config_from_env()?
        {
            let protected_paths = protected_paths_for_config(&config, profile)?;
            return Ok(Some(SetupResult {
                config,
                new_tunnel: false,
                protected_paths,
            }));
        }

        let path = config_path(profile)?;

        if path.exists() {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let mut config: AppConfig = parse_jsonc(&text)
                .with_context(|| format!("failed to parse {}", path.display()))?;

            if config.transports.openai {
                config.runtime_api_key = read_saved_runtime_key(profile)?;
            }

            apply_nonsecret_env_overrides(&mut config)?;
            validate_config(&config)?;
            let protected_paths = protected_paths_for_config(&config, profile)?;
            return Ok(Some(SetupResult {
                config,
                new_tunnel: false,
                protected_paths,
            }));
        }
    }

    if !io::stdin().is_terminal() {
        let profile_hint = profile
            .map(|name| format!(" --profile {name}"))
            .unwrap_or_default();
        bail!(
            "dotlink needs first-run setup, but stdin is not interactive. Run 'dotlink --setup{profile_hint}' in a terminal first"
        );
    }

    let existing = load_profile_for_edit(profile)?;
    interactive_setup(profile, color, existing).await
}

fn config_from_env() -> Result<Option<AppConfig>> {
    let tunnel_id = env_first(&["DOTLINK_ID", "CONTROL_PLANE_TUNNEL_ID"]);
    let runtime_api_key = env_first(&["DOTLINK_API_KEY", "CONTROL_PLANE_API_KEY"]);

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
            "DOTLINK_ORGANIZATION_ID",
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
    if let Some(base_url) = env_first(&["DOTLINK_BASE_URL", "CONTROL_PLANE_BASE_URL"]) {
        config.base_url = base_url;
    }
    if let Some(value) = env_first(&[
        "DOTLINK_ORGANIZATION_ID",
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

fn yes_no_hint(default: bool) -> &'static str {
    if default { "[Y/n]" } else { "[y/N]" }
}

fn prompt_cache_mode(
    cache: &DiscoveredCache,
    existing: Option<CacheMode>,
    color: bool,
) -> Result<Option<CacheMode>> {
    let default_key = match existing {
        Some(CacheMode::ReadOnly) => "r",
        Some(CacheMode::ReadWrite) => "w",
        None => "n",
    };
    loop {
        let input = prompt_line(&format!(
            "   {:<18} {}\n      {} ",
            cache.kind.label(),
            cache.path.display(),
            setup_style(
                color,
                "33",
                format!("Access [n]one / [r]ead-only / read+[w]rite [{default_key}]:")
            )
        ))?;

        let value = if input.trim().is_empty() {
            default_key
        } else {
            input.trim()
        };
        match value.to_ascii_lowercase().as_str() {
            "n" | "none" | "no" => return Ok(None),
            "r" | "ro" | "read" | "read-only" => return Ok(Some(CacheMode::ReadOnly)),
            "w" | "rw" | "write" | "read-write" | "read+write" => {
                return Ok(Some(CacheMode::ReadWrite));
            }
            _ => println!("      Please enter n, r, or w."),
        }
    }
}

async fn setup_developer_caches(
    allow_shell: bool,
    color: bool,
    existing: &[CacheGrant],
) -> Result<Vec<CacheGrant>> {
    if !allow_shell || !cfg!(target_os = "linux") {
        return Ok(Vec::new());
    }

    println!();
    println!("{}", setup_style(color, "1", "3. Developer caches"));
    println!(
        "   {}",
        setup_style(color, "2", "Scanning known package/build cache locations…")
    );

    let mut candidates = BTreeMap::<CacheKind, PathBuf>::new();
    for cache in discover_developer_caches().await {
        candidates.insert(cache.kind, cache.path);
    }
    for cache in existing {
        candidates.insert(cache.kind, cache.path.clone());
    }

    if candidates.is_empty() {
        println!("   • No supported existing caches found.");
        return Ok(Vec::new());
    }

    println!(
        "   Found {} cache{}:",
        candidates.len(),
        if candidates.len() == 1 { "" } else { "s" }
    );
    for (kind, path) in &candidates {
        println!("   • {:<18} {}", kind.label(), path.display());
    }
    println!(
        "   • Only cache directories are shared; adjacent credentials/config files are excluded."
    );
    println!(
        "   • read+write is fastest, but allows sandboxed builds to modify the shared host cache."
    );

    let configure_default = !existing.is_empty();
    if !prompt_yes_no(
        &format!(
            "   {} ",
            setup_style(
                color,
                "33",
                format!(
                    "Configure access to discovered caches? {}:",
                    yes_no_hint(configure_default)
                )
            )
        ),
        configure_default,
    )? {
        return Ok(Vec::new());
    }

    let mut grants = Vec::new();
    for (kind, path) in candidates {
        let cache = DiscoveredCache { kind, path };
        let existing_mode = existing
            .iter()
            .find(|grant| grant.kind == cache.kind)
            .map(|grant| grant.mode);
        if let Some(mode) = prompt_cache_mode(&cache, existing_mode, color)? {
            grants.push(CacheGrant {
                kind: cache.kind,
                path: cache.path,
                mode,
            });
        }
    }
    Ok(grants)
}

fn set_dot_rule(paths: &mut Vec<PathBuf>, enabled: bool) {
    let dot = PathBuf::from(".");
    paths.retain(|path| path != &dot);
    if enabled {
        paths.push(dot);
    }
}

fn setup_cancelled(input: &str) -> bool {
    matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "0" | "cancel" | "none" | "q" | "quit"
    )
}

fn transport_selection_label(config: &TransportConfig) -> String {
    let mut selected = Vec::new();
    if config.openai {
        selected.push("1");
    }
    if config.stdio {
        selected.push("2");
    }
    if config.http {
        selected.push("3");
    }
    if selected.is_empty() {
        "1".to_owned()
    } else {
        selected.join(",")
    }
}

fn setup_style(color: bool, code: &str, text: impl AsRef<str>) -> String {
    if color {
        format!("[{code}m{}[0m", text.as_ref())
    } else {
        text.as_ref().to_owned()
    }
}

async fn interactive_setup(
    profile: Option<&str>,
    color: bool,
    existing: Option<AppConfig>,
) -> Result<Option<SetupResult>> {
    println!();
    println!("{}", setup_style(color, "1;36", "abird dotlink setup"));
    println!(
        "{}",
        setup_style(
            color,
            "2",
            "────────────────────────────────────────────────────────"
        )
    );
    if let Some(profile) = profile {
        println!(
            "{} {}",
            setup_style(color, "1", "Profile:"),
            setup_style(color, "36", profile)
        );
        println!();
    }

    let mut config = existing.unwrap_or_else(default_app_config);
    let previous = config.clone();
    let current_transports = config.transports.clone();

    println!("{}", setup_style(color, "1", "1. MCP transports"));
    println!(
        "   {}  OpenAI Tunnel   {}",
        setup_style(color, "1;36", "1"),
        setup_style(color, "2", "Recommended for ChatGPT")
    );
    println!(
        "   {}  stdio           {}",
        setup_style(color, "1;36", "2"),
        setup_style(
            color,
            "2",
            "Local MCP clients (Claude Desktop, Codex, etc.)"
        )
    );
    println!(
        "   {}  HTTP            {}",
        setup_style(color, "1;36", "3"),
        setup_style(color, "2", "Claude.ai and other web MCP clients")
    );
    println!("   {}  Cancel setup", setup_style(color, "1;36", "0"));

    let selection_default = transport_selection_label(&current_transports);
    let selection = prompt_line(&format!(
        "   {} ",
        setup_style(
            color,
            "1;33",
            format!("Select [{selection_default}] (comma-separated; all / cancel):")
        )
    ))?;
    if setup_cancelled(&selection) {
        println!();
        println!(
            "{}",
            setup_style(color, "2", "Setup cancelled. No changes saved.")
        );
        return Ok(None);
    }

    let selected = if selection.trim().is_empty() {
        selection_default.as_str()
    } else {
        selection.as_str()
    };
    let mut transports = parse_transport_selection(selected)?;
    if transports.enabled_names().is_empty() {
        println!();
        println!(
            "{}",
            setup_style(color, "2", "Setup cancelled. No changes saved.")
        );
        return Ok(None);
    }

    transports.http_bind = current_transports.http_bind.clone();
    if transports.http {
        println!();
        println!("   {}", setup_style(color, "1", "HTTP options"));
        transports.http_ephemeral_url = prompt_yes_no(
            &format!(
                "   {} ",
                setup_style(
                    color,
                    "33",
                    format!(
                        "Use an ephemeral local MCP URL? {}:",
                        yes_no_hint(current_transports.http_ephemeral_url)
                    )
                )
            ),
            current_transports.http_ephemeral_url,
        )?;
        transports.ngrok = prompt_yes_no(
            &format!(
                "   {} ",
                setup_style(
                    color,
                    "33",
                    format!(
                        "Publish a public ngrok HTTPS endpoint? {}:",
                        yes_no_hint(current_transports.ngrok)
                    )
                )
            ),
            current_transports.ngrok,
        )?;
        if transports.ngrok {
            transports.ngrok_ephemeral_url = prompt_yes_no(
                &format!(
                    "   {} ",
                    setup_style(
                        color,
                        "33",
                        format!(
                            "Use an ephemeral ngrok MCP URL? {}:",
                            yes_no_hint(current_transports.ngrok_ephemeral_url)
                        )
                    )
                ),
                current_transports.ngrok_ephemeral_url,
            )?;
        } else {
            transports.ngrok_ephemeral_url = false;
        }
    } else {
        transports.http_ephemeral_url = false;
        transports.ngrok = false;
        transports.ngrok_ephemeral_url = false;
    }

    println!();
    println!("{}", setup_style(color, "1", "2. Local access"));
    let setup_dir = env::current_dir().context("failed to determine setup directory")?;
    let mut permissions = config.permissions.clone();
    permissions.default_allow = prompt_yes_no(
        &format!(
            "   {} {} ",
            setup_style(color, "33", "Allow read access to:"),
            setup_style(
                color,
                "36",
                format!(
                    "{} {}:",
                    setup_dir.display(),
                    yes_no_hint(permissions.default_allow)
                )
            )
        ),
        permissions.default_allow,
    )?;

    let current_rw_base = permissions
        .allow_rw
        .iter()
        .any(|path| path == Path::new("."));
    let allow_rw_base = if permissions.default_allow {
        prompt_yes_no(
            &format!(
                "   {} ",
                setup_style(
                    color,
                    "33",
                    format!("Allow write access too? {}:", yes_no_hint(current_rw_base))
                )
            ),
            current_rw_base,
        )?
    } else {
        false
    };
    set_dot_rule(&mut permissions.allow_rw, allow_rw_base);

    permissions.allow_shell = prompt_yes_no(
        &format!(
            "   {} ",
            setup_style(
                color,
                "33",
                format!(
                    "Allow shell access? {}:",
                    yes_no_hint(permissions.allow_shell)
                )
            )
        ),
        permissions.allow_shell,
    )?;
    permissions.allow_network = if permissions.allow_shell {
        if cfg!(target_os = "linux") {
            println!(
                "   {}",
                setup_style(color, "2", "Linux shell runs inside Bubblewrap.")
            );
        }
        prompt_yes_no(
            &format!(
                "   {} ",
                setup_style(
                    color,
                    "33",
                    format!(
                        "Allow shell network access? {}:",
                        yes_no_hint(permissions.allow_network)
                    )
                )
            ),
            permissions.allow_network,
        )?
    } else {
        false
    };

    let caches = if permissions.allow_shell {
        setup_developer_caches(true, color, &config.caches).await?
    } else {
        config.caches.clone()
    };

    let (tunnel_id, runtime_api_key, organization_id, new_tunnel) = if transports.openai {
        setup_openai(color, Some(&previous)).await?
    } else {
        (
            previous.tunnel_id.clone(),
            previous.runtime_api_key.clone(),
            previous.organization_id.clone(),
            false,
        )
    };

    config.version = config_version();
    config.transports = transports;
    config.permissions = permissions;
    config.caches = caches;
    config.tunnel_id = tunnel_id;
    config.runtime_api_key = runtime_api_key;
    config.organization_id = organization_id;

    validate_config(&config)?;
    save_config(&config, profile)?;

    println!();
    println!("{}", setup_style(color, "1;32", "✓ Setup complete"));
    println!("  • Config: {}", config_path(profile)?.display());
    let enabled = config.transports.enabled_names();
    println!("  • Transports: {}", enabled.join(", "));

    let mut permission_summary = Vec::new();
    if config.permissions.default_allow {
        permission_summary.push("read launch directory");
    }
    if config
        .permissions
        .allow_rw
        .iter()
        .any(|path| path == Path::new("."))
    {
        permission_summary.push("write launch directory");
    }
    if config.permissions.allow_shell {
        permission_summary.push("shell");
    }
    if config.permissions.allow_network {
        permission_summary.push("shell network");
    }
    println!(
        "  • Local access: {}",
        if permission_summary.is_empty() {
            "none".to_owned()
        } else {
            permission_summary.join(" + ")
        }
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
            "  • HTTP: http://{}{}",
            config.transports.http_bind,
            if config.transports.http_ephemeral_url {
                "/mcp/<ephemeral>"
            } else {
                "/mcp"
            }
        );
        if config.transports.ngrok {
            println!(
                "  • ngrok: public HTTPS{}",
                if config.transports.ngrok_ephemeral_url {
                    " + ephemeral MCP path"
                } else {
                    ""
                }
            );
        }
    }

    let protected_paths = protected_paths_for_config(&config, profile)?;
    Ok(Some(SetupResult {
        config,
        new_tunnel,
        protected_paths,
    }))
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
        ..TransportConfig::default()
    };

    for token in input.split([',', ' ', ';']) {
        let token = token.trim().to_ascii_lowercase();
        if token.is_empty() {
            continue;
        }
        match token.as_str() {
            "a" | "all" => {
                config.openai = true;
                config.stdio = true;
                config.http = true;
            }
            "n" | "none" => {
                config.openai = false;
                config.stdio = false;
                config.http = false;
            }
            "1" | "openai" | "tunnel" => config.openai = true,
            "2" | "stdio" => config.stdio = true,
            "3" | "http" => config.http = true,
            other => {
                bail!("unknown transport {other:?}; choose 1/openai, 2/stdio, 3/http, all, or none")
            }
        }
    }

    Ok(config)
}

async fn setup_openai(
    color: bool,
    existing: Option<&AppConfig>,
) -> Result<(Option<String>, String, Option<String>, bool)> {
    println!();
    println!(
        "{}",
        setup_style(color, "1", "Configure OpenAI Secure MCP Tunnel")
    );
    println!("   {}", setup_style(color, "1", "Runtime API key"));
    println!("   • Permissions: Tunnels Read + Use");
    println!("   • https://platform.openai.com/settings/organization/api-keys");

    let existing_key = existing
        .map(|config| config.runtime_api_key.trim())
        .filter(|key| !key.is_empty());
    let key_prompt = if existing_key.is_some() {
        "   Runtime API key [existing key]: "
    } else {
        "   Paste key: "
    };
    let entered_key = Zeroizing::new(prompt_secret(key_prompt)?);
    let runtime_api_key = if entered_key.trim().is_empty() {
        existing_key
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("runtime API key cannot be empty"))?
    } else {
        entered_key.trim().to_owned()
    };

    println!();
    println!("   {}", setup_style(color, "1", "Choose a tunnel"));
    let existing_id = existing.and_then(|config| config.tunnel_id.as_deref());
    let tunnel_prompt = match existing_id {
        Some(id) => format!("   Tunnel ID [{id}] (type 'new' to create): "),
        None => "   Tunnel ID [create new]: ".to_owned(),
    };
    let selected_id = prompt_line(&tunnel_prompt)?;
    let create_new = selected_id.eq_ignore_ascii_case("new")
        || (selected_id.trim().is_empty() && existing_id.is_none());

    let (tunnel_id, new_tunnel, runtime_organization_id) = if create_new {
        println!();
        println!(
            "   {}",
            setup_style(color, "1", "Create a one-time Admin key")
        );
        println!("   • Permission: Tunnels Manage");
        println!("   • Used once and never saved; you can delete it after setup.");
        println!("   • https://platform.openai.com/settings/organization/admin-keys");
        let admin_key = Zeroizing::new(prompt_secret("   Paste key: ")?);
        if admin_key.trim().is_empty() {
            bail!("admin API key cannot be empty when creating a tunnel");
        }

        println!();
        println!("   {}", setup_style(color, "1", "Choose tunnel scope"));
        println!("   • Workspace ID:    https://chatgpt.com/admin");
        println!("     Select your ChatGPT workspace → Settings, then copy Workspace ID.");
        println!("   • Organization ID: https://platform.openai.com/settings/organization/general");
        println!("   • Paste a Workspace ID, or press Enter to use your Organization ID.");
        let workspace_id = prompt_line("   ChatGPT workspace ID [optional]: ")?;

        let existing_org = existing.and_then(|config| config.organization_id.as_deref());
        let organization_prompt = match existing_org {
            Some(org) => format!("   OpenAI organization ID [{org}]: "),
            None => "   OpenAI organization ID: ".to_owned(),
        };
        let organization_input = if workspace_id.trim().is_empty() {
            prompt_line(&organization_prompt)?
        } else {
            String::new()
        };
        let organization_id =
            if workspace_id.trim().is_empty() && organization_input.trim().is_empty() {
                existing_org.unwrap_or_default().to_owned()
            } else {
                organization_input.trim().to_owned()
            };
        if workspace_id.trim().is_empty() && organization_id.is_empty() {
            bail!("a workspace ID or organization ID is required to create a tunnel");
        }

        print!("   Creating tunnel… ");
        io::stdout().flush()?;
        let base_url = existing
            .map(|config| config.base_url.as_str())
            .unwrap_or(DEFAULT_BASE_URL);
        let tunnel_id = create_tunnel(
            base_url,
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
            if organization_id.is_empty() {
                None
            } else {
                Some(organization_id)
            },
        )
    } else {
        let tunnel_id = if selected_id.trim().is_empty() {
            existing_id
                .ok_or_else(|| anyhow!("Tunnel ID is required"))?
                .to_owned()
        } else {
            selected_id.trim().to_owned()
        };
        validate_tunnel_id(&tunnel_id)?;
        (
            tunnel_id,
            false,
            existing.and_then(|config| config.organization_id.clone()),
        )
    };

    Ok((
        Some(tunnel_id),
        runtime_api_key,
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
        "name": format!("abird dotlink · {host}"),
        "description": "Permission-scoped local MCP access via abird dotlink"
    });
    if !workspace_id.is_empty() {
        body["workspace_ids"] = serde_json::json!([workspace_id]);
    }
    if !organization_id.is_empty() {
        body["organization_ids"] = serde_json::json!([organization_id]);
    }

    let client = reqwest::Client::builder()
        .user_agent(format!("dotlink/{}", env!("CARGO_PKG_VERSION")))
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

    let text = serialize_jsonc(config)?;
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

    if !config.runtime_api_key.trim().is_empty() {
        save_runtime_key(&config.runtime_api_key, profile)?;
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
        "// dotlink configuration (JSONC)\n// Comments and trailing commas are allowed.\n{body}\n"
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
    let base = if let Some(path) = env::var_os("DOTLINK_CONFIG") {
        PathBuf::from(path)
    } else if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("abird/dotlink/config.jsonc")
    } else {
        home_dir()?.join(".config/abird/dotlink/config.jsonc")
    };
    profiled_path(&base, profile)
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

fn profile_target(name: &str) -> Result<Option<&str>> {
    if name.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    validate_profile(Some(name))?;
    Ok(Some(name))
}

fn require_profile(name: &str) -> Result<(Option<&str>, AppConfig)> {
    let target = profile_target(name)?;
    let config =
        load_profile_for_edit(target)?.ok_or_else(|| anyhow!("profile {name:?} does not exist"))?;
    Ok((target, config))
}

pub fn list_profiles() -> Result<Vec<String>> {
    let base = config_path(None)?;
    let parent = base
        .parent()
        .ok_or_else(|| anyhow!("configuration path has no parent"))?;
    if !parent.exists() {
        return Ok(Vec::new());
    }

    let base_name = base
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("configuration filename is invalid"))?;
    let stem = base
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("configuration filename is invalid"))?;
    let extension = base.extension().and_then(|value| value.to_str());

    let mut profiles = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == base_name {
            profiles.push("default".to_owned());
            continue;
        }

        let candidate = match extension {
            Some(extension) if !extension.is_empty() => {
                let prefix = format!("{stem}.");
                let suffix = format!(".{extension}");
                name.strip_prefix(&prefix)
                    .and_then(|value| value.strip_suffix(&suffix))
            }
            _ => name.strip_prefix(&format!("{base_name}.")),
        };
        let Some(candidate) = candidate else {
            continue;
        };
        if candidate.is_empty() || candidate.eq_ignore_ascii_case("default") {
            continue;
        }
        if validate_profile(Some(candidate)).is_ok() {
            profiles.push(candidate.to_owned());
        }
    }
    profiles.sort();
    profiles.dedup();
    Ok(profiles)
}

pub fn show_profile(name: &str) -> Result<String> {
    let (target, config) = require_profile(name)?;
    let path = config_path(target)?;
    let has_key = try_read_saved_runtime_key(target)?.is_some();
    let mut output = String::new();
    output.push_str(&format!("// Profile: {name}\n"));
    output.push_str(&format!("// Config: {}\n", path.display()));
    if has_key {
        output.push_str("// Runtime API key: [existing key]\n");
    }
    output.push_str(&serialize_jsonc(&config)?);
    Ok(output)
}

pub async fn create_profile(name: &str, color: bool) -> Result<bool> {
    let target = profile_target(name)?;
    let path = config_path(target)?;
    if path.exists() || credential_path(target)?.exists() {
        bail!("profile {name:?} already exists");
    }
    if !io::stdin().is_terminal() {
        bail!("profile creation is interactive; run it in a terminal");
    }
    Ok(interactive_setup(target, color, None).await?.is_some())
}

pub async fn edit_profile(name: &str, color: bool) -> Result<bool> {
    let target = profile_target(name)?;
    let existing =
        load_profile_for_edit(target)?.ok_or_else(|| anyhow!("profile {name:?} does not exist"))?;
    if !io::stdin().is_terminal() {
        bail!("profile editing is interactive; run it in a terminal");
    }
    Ok(interactive_setup(target, color, Some(existing))
        .await?
        .is_some())
}

pub fn delete_profile(name: &str) -> Result<()> {
    let target = profile_target(name)?;
    let path = config_path(target)?;
    if !path.exists() {
        bail!("profile {name:?} does not exist");
    }
    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    remove_saved_runtime_key(target)?;
    Ok(())
}

fn rule_paths_mut(config: &mut AppConfig, allow: bool, kind: ProfileRuleKind) -> &mut Vec<PathBuf> {
    match (allow, kind) {
        (true, ProfileRuleKind::Read) => &mut config.permissions.allow_read,
        (true, ProfileRuleKind::Write) => &mut config.permissions.allow_write,
        (true, ProfileRuleKind::Rw) => &mut config.permissions.allow_rw,
        (false, ProfileRuleKind::Read) => &mut config.permissions.deny_read,
        (false, ProfileRuleKind::Write) => &mut config.permissions.deny_write,
        (false, ProfileRuleKind::Rw) => &mut config.permissions.deny_rw,
    }
}

pub fn mutate_profile_rule(
    name: &str,
    allow: bool,
    kind: ProfileRuleKind,
    path: PathBuf,
    remove: bool,
) -> Result<()> {
    let (target, mut config) = require_profile(name)?;
    let paths = rule_paths_mut(&mut config, allow, kind);
    if remove {
        let before = paths.len();
        paths.retain(|existing| existing != &path);
        if paths.len() == before {
            bail!("rule not found in profile {name:?}: {}", path.display());
        }
    } else if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
    validate_config(&config)?;
    save_config(&config, target)
}

fn apply_profile_bool(config: &mut AppConfig, setting: ProfileBool, enabled: bool) -> Result<()> {
    match (setting, enabled) {
        (ProfileBool::Openai, value) => config.transports.openai = value,
        (ProfileBool::Stdio, value) => config.transports.stdio = value,
        (ProfileBool::Http, true) => config.transports.http = true,
        (ProfileBool::Http, false) => {
            config.transports.http = false;
            config.transports.http_ephemeral_url = false;
            config.transports.ngrok = false;
            config.transports.ngrok_ephemeral_url = false;
        }
        (ProfileBool::HttpEphemeral, true) => {
            config.transports.http = true;
            config.transports.http_ephemeral_url = true;
        }
        (ProfileBool::HttpEphemeral, false) => config.transports.http_ephemeral_url = false,
        (ProfileBool::Ngrok, true) => {
            config.transports.http = true;
            config.transports.ngrok = true;
        }
        (ProfileBool::Ngrok, false) => {
            config.transports.ngrok = false;
            config.transports.ngrok_ephemeral_url = false;
        }
        (ProfileBool::NgrokEphemeral, true) => {
            config.transports.http = true;
            config.transports.ngrok = true;
            config.transports.ngrok_ephemeral_url = true;
        }
        (ProfileBool::NgrokEphemeral, false) => config.transports.ngrok_ephemeral_url = false,
        (ProfileBool::DefaultAllow, value) => config.permissions.default_allow = value,
        (ProfileBool::Shell, true) => config.permissions.allow_shell = true,
        (ProfileBool::Shell, false) => {
            config.permissions.allow_shell = false;
            config.permissions.allow_network = false;
        }
        (ProfileBool::Network, true) => {
            config.permissions.allow_shell = true;
            config.permissions.allow_network = true;
        }
        (ProfileBool::Network, false) => config.permissions.allow_network = false,
    }
    Ok(())
}

pub fn set_profile_bool(name: &str, setting: ProfileBool, enabled: bool) -> Result<()> {
    let (target, mut config) = require_profile(name)?;
    apply_profile_bool(&mut config, setting, enabled)?;
    if config.transports.openai
        && (config.tunnel_id.is_none() || config.runtime_api_key.trim().is_empty())
    {
        bail!(
            "OpenAI is not fully configured for profile {name:?}; run `dotlink profile edit {name}` first"
        );
    }
    validate_config(&config)?;
    save_config(&config, target)
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

fn try_read_saved_runtime_key(profile: Option<&str>) -> Result<Option<String>> {
    let path = credential_path(profile)?;
    let key = match fs::read_to_string(&path) {
        Ok(key) => key,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let key = key.trim().to_owned();
    if key.is_empty() {
        return Ok(None);
    }
    Ok(Some(key))
}

fn read_saved_runtime_key(profile: Option<&str>) -> Result<String> {
    try_read_saved_runtime_key(profile)?.ok_or_else(|| {
        anyhow!(
            "saved runtime API key is missing or empty; run 'dotlink --setup{}' to repair credentials",
            profile
                .map(|name| format!(" --profile {name}"))
                .unwrap_or_default()
        )
    })
}

fn validate_profile(profile: Option<&str>) -> Result<()> {
    let Some(profile) = profile else {
        return Ok(());
    };
    if profile.eq_ignore_ascii_case("default") {
        bail!("profile name 'default' is reserved for the unnamed default profile");
    }
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

fn validate_config_structure(config: &AppConfig) -> Result<()> {
    if config.version != config_version() {
        bail!(
            "unsupported config version {}; expected {}. Rerun dotlink --setup for this profile",
            config.version,
            config_version()
        );
    }

    if config.permissions.allow_network && !config.permissions.allow_shell {
        bail!("permissions.allow_network requires permissions.allow_shell");
    }
    if config.transports.http_ephemeral_url && !config.transports.http {
        bail!("transports.http_ephemeral_url requires transports.http");
    }
    if config.transports.ngrok && !config.transports.http {
        bail!("transports.ngrok requires transports.http");
    }
    if config.transports.ngrok_ephemeral_url && !config.transports.ngrok {
        bail!("transports.ngrok_ephemeral_url requires transports.ngrok");
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

    if let Some(tunnel_id) = config.tunnel_id.as_deref() {
        validate_tunnel_id(tunnel_id)?;
    }
    if !(config.base_url.starts_with("https://") || config.base_url.starts_with("http://localhost"))
    {
        bail!("control-plane base URL must use HTTPS (localhost is allowed for testing)");
    }

    Ok(())
}

fn validate_config(config: &AppConfig) -> Result<()> {
    validate_config_structure(config)?;
    if config.transports.openai {
        if config.tunnel_id.is_none() {
            bail!("OpenAI transport is enabled but tunnel_id is missing");
        }
        if config.runtime_api_key.trim().is_empty() {
            bail!("OpenAI transport is enabled but runtime API key is empty");
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

        let numbered = parse_transport_selection("2,3").unwrap();
        assert!(!numbered.openai);
        assert!(numbered.stdio);
        assert!(numbered.http);

        let all = parse_transport_selection("all").unwrap();
        assert!(all.openai && all.stdio && all.http);

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
        let base = Path::new("/tmp/dotlink/config.jsonc");
        assert_eq!(
            profiled_path(base, None).unwrap(),
            PathBuf::from("/tmp/dotlink/config.jsonc")
        );
        assert_eq!(
            profiled_path(base, Some("work")).unwrap(),
            PathBuf::from("/tmp/dotlink/config.work.jsonc")
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
        assert!(serialized.starts_with("// dotlink configuration (JSONC)"));
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
    fn only_schema_v9_is_accepted() {
        let mut config = example_config();
        config.version = 8;
        let error = validate_config(&config).unwrap_err();
        assert!(error.to_string().contains("unsupported config version 8"));

        let missing =
            parse_jsonc::<AppConfig>(r#"{ "transports": { "openai": false }, "permissions": {} }"#);
        assert!(missing.is_err());
    }

    #[test]
    fn http_profile_options_round_trip() {
        let mut config = example_config();
        config.transports = TransportConfig {
            openai: false,
            stdio: false,
            http: true,
            http_bind: "127.0.0.1:4321".to_owned(),
            http_ephemeral_url: true,
            ngrok: true,
            ngrok_ephemeral_url: true,
        };
        config.tunnel_id = None;
        config.runtime_api_key.clear();

        let serialized = serialize_jsonc(&config).unwrap();
        let parsed: AppConfig = parse_jsonc(&serialized).unwrap();
        assert!(parsed.transports.http);
        assert_eq!(parsed.transports.http_bind, "127.0.0.1:4321");
        assert!(parsed.transports.http_ephemeral_url);
        assert!(parsed.transports.ngrok);
        assert!(parsed.transports.ngrok_ephemeral_url);
        validate_config(&parsed).unwrap();
    }

    #[test]
    fn permissions_round_trip_in_jsonc() {
        let mut config = example_config();
        config.permissions = PermissionConfig {
            default_allow: false,
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
        assert!(!parsed.permissions.default_allow);
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
    fn setup_cancel_tokens_are_explicit() {
        for value in ["0", "none", "cancel", "q", "quit", " CANCEL "] {
            assert!(setup_cancelled(value), "{value:?} should cancel setup");
        }
        for value in ["", "1", "all", "stdio"] {
            assert!(!setup_cancelled(value), "{value:?} should not cancel setup");
        }
    }

    #[test]
    fn profile_boolean_dependencies_and_cascades_are_safe() {
        let mut config = example_config();
        config.transports.openai = false;
        config.runtime_api_key.clear();
        config.tunnel_id = None;
        config.transports.http = false;
        config.transports.ngrok = false;
        config.permissions.allow_shell = false;
        config.permissions.allow_network = false;

        apply_profile_bool(&mut config, ProfileBool::NgrokEphemeral, true).unwrap();
        apply_profile_bool(&mut config, ProfileBool::HttpEphemeral, true).unwrap();
        apply_profile_bool(&mut config, ProfileBool::Network, true).unwrap();
        assert!(config.transports.http);
        assert!(config.transports.ngrok);
        assert!(config.transports.ngrok_ephemeral_url);
        assert!(config.transports.http_ephemeral_url);
        assert!(config.permissions.allow_shell);
        assert!(config.permissions.allow_network);

        apply_profile_bool(&mut config, ProfileBool::Http, false).unwrap();
        assert!(!config.transports.http);
        assert!(!config.transports.ngrok);
        assert!(!config.transports.ngrok_ephemeral_url);
        assert!(!config.transports.http_ephemeral_url);

        apply_profile_bool(&mut config, ProfileBool::Shell, true).unwrap();
        apply_profile_bool(&mut config, ProfileBool::Network, true).unwrap();
        assert!(config.permissions.allow_network);
        apply_profile_bool(&mut config, ProfileBool::Shell, false).unwrap();
        assert!(!config.permissions.allow_shell);
        assert!(!config.permissions.allow_network);
    }

    #[test]
    fn default_profile_alias_is_reserved_for_profile_manager() {
        assert_eq!(profile_target("default").unwrap(), None);
        assert!(validate_profile(Some("default")).is_err());
        assert_eq!(profile_target("work").unwrap(), Some("work"));
    }

    #[test]
    fn rule_kind_selects_expected_permission_array() {
        let mut config = example_config();
        rule_paths_mut(&mut config, true, ProfileRuleKind::Read).push(PathBuf::from("/allow-read"));
        rule_paths_mut(&mut config, true, ProfileRuleKind::Write)
            .push(PathBuf::from("/allow-write"));
        rule_paths_mut(&mut config, true, ProfileRuleKind::Rw).push(PathBuf::from("/allow-rw"));
        rule_paths_mut(&mut config, false, ProfileRuleKind::Read).push(PathBuf::from("/deny-read"));
        rule_paths_mut(&mut config, false, ProfileRuleKind::Write)
            .push(PathBuf::from("/deny-write"));
        rule_paths_mut(&mut config, false, ProfileRuleKind::Rw).push(PathBuf::from("/deny-rw"));

        assert!(
            config
                .permissions
                .allow_read
                .contains(&PathBuf::from("/allow-read"))
        );
        assert!(
            config
                .permissions
                .allow_write
                .contains(&PathBuf::from("/allow-write"))
        );
        assert!(
            config
                .permissions
                .allow_rw
                .contains(&PathBuf::from("/allow-rw"))
        );
        assert!(
            config
                .permissions
                .deny_read
                .contains(&PathBuf::from("/deny-read"))
        );
        assert!(
            config
                .permissions
                .deny_write
                .contains(&PathBuf::from("/deny-write"))
        );
        assert!(
            config
                .permissions
                .deny_rw
                .contains(&PathBuf::from("/deny-rw"))
        );
    }

    #[test]
    fn network_default_requires_shell_default() {
        let mut config = example_config();
        config.permissions.allow_network = true;
        config.permissions.allow_shell = false;

        assert!(validate_config(&config).is_err());
    }
}
