use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_MAX_SHELL_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_048_576;
const DEFAULT_MAX_FILE_BYTES: usize = 4_194_304;

#[derive(Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "config_version")]
    pub version: u32,
    pub tunnel_id: String,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub runtime_api_key: String,
    #[serde(default)]
    pub organization_id: Option<String>,
    #[serde(default = "default_true")]
    pub allow_shell: bool,
    #[serde(default = "default_shell")]
    pub shell: String,
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
    2
}
fn default_true() -> bool {
    true
}
fn default_shell() -> String {
    "bash".to_owned()
}
fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
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

pub async fn load_or_setup(force_setup: bool) -> Result<SetupResult> {
    if !force_setup {
        if let Some(config) = config_from_env()? {
            return Ok(SetupResult {
                config,
                new_tunnel: false,
                protected_paths: vec![credential_path()?],
            });
        }

        let path = config_path()?;
        if path.exists() {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let mut config: AppConfig = toml::from_str(&text)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            config.runtime_api_key = read_saved_runtime_key()?;
            apply_nonsecret_env_overrides(&mut config)?;
            validate_config(&config)?;
            return Ok(SetupResult {
                config,
                new_tunnel: false,
                protected_paths: vec![credential_path()?],
            });
        }
    }

    if !io::stdin().is_terminal() {
        bail!(
            "abird-tunnel needs first-run setup, but stdin is not interactive. Set CONTROL_PLANE_TUNNEL_ID and CONTROL_PLANE_API_KEY, or run `abird-tunnel --setup` in a terminal first"
        );
    }

    interactive_setup().await
}

fn config_from_env() -> Result<Option<AppConfig>> {
    let tunnel_id = env_first(&["ABIRD_TUNNEL_ID", "CONTROL_PLANE_TUNNEL_ID"]);
    let runtime_api_key = env_first(&["ABIRD_TUNNEL_API_KEY", "CONTROL_PLANE_API_KEY"]);

    let (Some(tunnel_id), Some(runtime_api_key)) = (tunnel_id, runtime_api_key) else {
        return Ok(None);
    };

    let mut config = AppConfig {
        version: config_version(),
        tunnel_id,
        runtime_api_key,
        organization_id: env_first(&[
            "ABIRD_TUNNEL_ORGANIZATION_ID",
            "CONTROL_PLANE_ORGANIZATION_ID",
            "OPENAI_ORGANIZATION",
        ]),
        allow_shell: true,
        shell: default_shell(),
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
    if let Some(shell) = env_first(&["ABIRD_TUNNEL_SHELL"]) {
        config.shell = shell;
    }
    if let Some(base_url) = env_first(&["ABIRD_TUNNEL_BASE_URL", "CONTROL_PLANE_BASE_URL"]) {
        config.base_url = base_url;
    }
    if let Some(value) = env_first(&["ABIRD_TUNNEL_ALLOW_SHELL"]) {
        config.allow_shell = parse_bool(&value)?;
    }
    if let Some(value) = env_first(&[
        "ABIRD_TUNNEL_ORGANIZATION_ID",
        "CONTROL_PLANE_ORGANIZATION_ID",
        "OPENAI_ORGANIZATION",
    ]) {
        config.organization_id = Some(value);
    }
    Ok(())
}

async fn interactive_setup() -> Result<SetupResult> {
    println!();
    println!("abird-tunnel setup");
    println!("────────────────────────────────────────────────────────");
    println!("1. Create a Runtime API key");
    println!("   • Permissions: Tunnels Read + Use");
    println!("   • https://platform.openai.com/settings/organization/api-keys");
    let runtime_api_key = Zeroizing::new(prompt_secret("   Paste key: ")?);
    if runtime_api_key.trim().is_empty() {
        bail!("runtime API key cannot be empty");
    }

    println!();
    println!("2. Choose a tunnel");
    println!("   • Paste an existing Tunnel ID, or press Enter to create one.");
    let existing_id = prompt_line("   Tunnel ID [create new]: ")?;

    let (tunnel_id, new_tunnel, runtime_organization_id) = if existing_id.trim().is_empty() {
        println!();
        println!("3. Create a one-time Admin key");
        println!("   • Permission: Tunnels Manage");
        println!("   • Used once and never saved; you can delete it after setup.");
        println!("   • https://platform.openai.com/settings/organization/admin-keys");
        let admin_key = Zeroizing::new(prompt_secret("   Paste key: ")?);
        if admin_key.trim().is_empty() {
            bail!("admin API key cannot be empty when creating a tunnel");
        }

        println!();
        println!("4. Choose tunnel scope");
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

    let config = AppConfig {
        version: config_version(),
        tunnel_id,
        runtime_api_key: runtime_api_key.as_str().to_owned(),
        organization_id: runtime_organization_id,
        allow_shell: true,
        shell: default_shell(),
        base_url: default_base_url(),
        max_shell_timeout_secs: default_shell_timeout(),
        max_output_bytes: default_output_bytes(),
        max_read_bytes: default_file_bytes(),
        max_write_bytes: default_file_bytes(),
    };
    validate_config(&config)?;
    save_config(&config)?;

    println!();
    println!("✓ Setup complete");
    println!("  • Runtime key saved securely for future runs.");
    if new_tunnel {
        println!("  • Admin key was not saved — you can delete it now.");
    }
    println!("  • Bash is enabled. Use --no-shell to disable it.");

    Ok(SetupResult {
        config,
        new_tunnel,
        protected_paths: vec![credential_path()?],
    })
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
        "name": format!("abird-tunnel · {host}"),
        "description": "Local filesystem and Bash MCP access via abird-tunnel"
    });
    if !workspace_id.is_empty() {
        body["workspace_ids"] = serde_json::json!([workspace_id]);
    }
    if !organization_id.is_empty() {
        body["organization_ids"] = serde_json::json!([organization_id]);
    }

    let client = reqwest::Client::builder()
        .user_agent(format!("abird-tunnel/{}", env!("CARGO_PKG_VERSION")))
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
        let message = response.text().await.unwrap_or_default();
        let message = bounded(&message, 1000);
        bail!("OpenAI tunnel creation failed ({status}): {message}");
    }

    let record: TunnelRecord = response
        .json()
        .await
        .context("OpenAI returned an invalid tunnel creation response")?;
    validate_tunnel_id(&record.id)?;
    Ok(record.id)
}

fn save_config(config: &AppConfig) -> Result<()> {
    let path = config_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("configuration path has no parent"))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    set_private_dir_permissions(parent)?;

    let text = toml::to_string_pretty(config)?;
    let temp = parent.join(format!(".config.toml.tmp-{}", std::process::id()));

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
    file.sync_all()?;
    drop(file);
    set_private_file_permissions(&temp)?;
    fs::rename(&temp, &path).with_context(|| format!("failed to replace {}", path.display()))?;
    set_private_file_permissions(&path)?;
    save_runtime_key(&config.runtime_api_key)?;
    Ok(())
}

fn credential_path() -> Result<PathBuf> {
    let config = config_path()?;
    let parent = config
        .parent()
        .ok_or_else(|| anyhow!("configuration path has no parent"))?;
    Ok(parent.join("runtime.key"))
}

fn save_runtime_key(key: &str) -> Result<()> {
    let path = credential_path()?;
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

fn read_saved_runtime_key() -> Result<String> {
    let path = credential_path()?;
    let key = fs::read_to_string(&path).with_context(|| {
        format!(
            "failed to read {}; run `abird-tunnel --setup` to repair credentials",
            path.display()
        )
    })?;
    let key = key.trim().to_owned();
    if key.is_empty() {
        bail!("saved runtime API key is empty; run `abird-tunnel --setup`");
    }
    Ok(key)
}

fn config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("ABIRD_TUNNEL_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("abird-tunnel/config.toml"));
    }
    let home = home_dir()?;
    Ok(home.join(".config/abird-tunnel/config.toml"))
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
        .ok_or_else(|| anyhow!("could not determine the user's home directory"))
}

fn validate_config(config: &AppConfig) -> Result<()> {
    validate_tunnel_id(&config.tunnel_id)?;
    if config.runtime_api_key.trim().is_empty() {
        bail!("runtime API key is empty");
    }
    if config.shell.trim().is_empty() {
        bail!("shell executable is empty");
    }
    if !(config.base_url.starts_with("https://") || config.base_url.starts_with("http://localhost"))
    {
        bail!("control-plane base URL must use HTTPS (localhost is allowed for testing)");
    }
    Ok(())
}

fn validate_tunnel_id(id: &str) -> Result<()> {
    // Tunnel IDs are opaque protocol identifiers. Avoid baking the current hex
    // representation into the client; only reject obviously malformed input.
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

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("invalid boolean value: {value}"),
    }
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

    #[test]
    fn tunnel_id_validation_keeps_ids_opaque() {
        assert!(validate_tunnel_id("tunnel_0123456789abcdef0123456789abcdef").is_ok());
        assert!(validate_tunnel_id("tunnel_future-format-v2").is_ok());
        assert!(validate_tunnel_id("tunnel_bad id").is_err());
        assert!(validate_tunnel_id("tunnel_bad/path").is_err());
        assert!(validate_tunnel_id("nope").is_err());
    }

    #[test]
    fn bool_parser_accepts_common_forms() {
        assert!(parse_bool("yes").unwrap());
        assert!(!parse_bool("off").unwrap());
    }

    #[test]
    fn runtime_key_is_never_serialized_into_config() {
        let config = AppConfig {
            version: 2,
            tunnel_id: "tunnel_0123456789abcdef0123456789abcdef".to_owned(),
            runtime_api_key: "secret-runtime-key".to_owned(),
            organization_id: None,
            allow_shell: true,
            shell: "bash".to_owned(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            max_shell_timeout_secs: 120,
            max_output_bytes: 1024,
            max_read_bytes: 1024,
            max_write_bytes: 1024,
        };

        let serialized = toml::to_string(&config).unwrap();
        assert!(!serialized.contains("secret-runtime-key"));
        assert!(!serialized.contains("runtime_api_key"));
    }
}
