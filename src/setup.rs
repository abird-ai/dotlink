use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
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
    /// Grant read+write access to the process cwd by default.
    #[serde(default)]
    pub allow_rw: bool,

    /// Expose the platform shell by default. Linux still uses Bubblewrap.
    #[serde(default)]
    pub allow_shell: bool,
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
    5
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
        if path.exists() {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let mut config: AppConfig = serde_json::from_str(&text)
                .with_context(|| format!("failed to parse {}", path.display()))?;

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
    let allow_rw = prompt_yes_no("   Allow read+write cwd by default? [y/N]: ", false)?;
    let allow_shell = prompt_yes_no("   Allow shell by default? [y/N]: ", false)?;
    if allow_shell {
        println!(
            "   • Linux shell remains Bubblewrap-sandboxed; network stays disabled by default."
        );
    }
    let permissions = PermissionConfig {
        allow_rw,
        allow_shell,
    };

    let (tunnel_id, runtime_api_key, organization_id, new_tunnel) = if transports.openai {
        setup_openai().await?
    } else {
        (None, String::new(), None, false)
    };

    let config = AppConfig {
        version: config_version(),
        transports,
        permissions,
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
        "  • Default permissions: read cwd{}{}",
        if config.permissions.allow_rw {
            " + write cwd"
        } else {
            ""
        },
        if config.permissions.allow_shell {
            " + shell"
        } else {
            ""
        },
    );
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
    println!("3. Configure OpenAI Secure MCP Tunnel");
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

    let text = serde_json::to_string_pretty(config)?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("config.json"),
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
        PathBuf::from(xdg).join("abird-link/config.json")
    } else {
        home_dir()?.join(".config/abird-link/config.json")
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
    fn profile_paths_are_predictable() {
        let base = Path::new("/tmp/abird-link/config.json");
        assert_eq!(
            profiled_path(base, None).unwrap(),
            PathBuf::from("/tmp/abird-link/config.json")
        );
        assert_eq!(
            profiled_path(base, Some("work")).unwrap(),
            PathBuf::from("/tmp/abird-link/config.work.json")
        );
        assert!(profiled_path(base, Some("../bad")).is_err());
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
    fn runtime_key_is_never_serialized_into_json() {
        let serialized = serde_json::to_string_pretty(&example_config()).unwrap();
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
    fn permissions_round_trip_in_json() {
        let mut config = example_config();
        config.permissions.allow_rw = true;
        config.permissions.allow_shell = true;

        let serialized = serde_json::to_string_pretty(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&serialized).unwrap();
        assert!(parsed.permissions.allow_rw);
        assert!(parsed.permissions.allow_shell);
    }
}
