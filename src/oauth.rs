use std::{
    collections::{BTreeMap, HashMap},
    env, fs, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Form, Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Query, State},
    http::{
        HeaderMap, HeaderValue, Request, StatusCode,
        header::{
            AUTHORIZATION, CACHE_CONTROL, CONTENT_SECURITY_POLICY, PRAGMA, REFERRER_POLICY,
            WWW_AUTHENTICATE, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
        },
    },
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use url::{Host, Url};
use zeroize::Zeroizing;

use crate::setup::{
    atomic_write_private, set_private_dir_permissions, set_private_file_permissions,
};

const STATE_VERSION: u32 = 1;
const ACCESS_TOKEN_TTL_SECS: i64 = 15 * 60;
const AUTH_CODE_TTL_SECS: i64 = 60;
const PENDING_AUTH_TTL_SECS: i64 = 5 * 60;
const REFRESH_TOKEN_TTL_SECS: i64 = 90 * 24 * 60 * 60;
const MAX_DCR_CLIENTS: usize = 64;
const PENDING_DCR_TTL_SECS: i64 = 15 * 60;
const MAX_PENDING_DCR_CLIENTS: usize = 256;
const MAX_PENDING_AUTHORIZATIONS: usize = 256;
const MAX_AUTHORIZATION_CODES: usize = 256;
const MAX_ACCESS_TOKENS: usize = 512;
const MAX_REFRESH_GRANTS: usize = 256;
const MAX_CLIENT_METADATA_BYTES: usize = 64 * 1024;
const OWNER_PASSWORD_MIN_LEN: usize = 12;
const MAX_CONCURRENT_PASSWORD_CHECKS: usize = 2;
const PASSWORD_CHECK_WAIT: Duration = Duration::from_secs(2);
const FAILED_PASSWORD_DELAY: Duration = Duration::from_millis(300);
const SCOPE_MCP: &str = "mcp:access";
const SCOPE_OFFLINE: &str = "offline_access";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OAuthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub public_url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct RegisteredClient {
    client_id: String,
    client_name: Option<String>,
    redirect_uris: Vec<String>,
    grant_types: Vec<String>,
    response_types: Vec<String>,
    token_endpoint_auth_method: String,
    issued_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct RefreshGrant {
    client_id: String,
    issuer: String,
    resource: String,
    scope: String,
    expires_at: i64,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistentState {
    version: u32,
    owner_password_hash: Option<String>,
    #[serde(default)]
    clients: BTreeMap<String, RegisteredClient>,
    #[serde(default)]
    refresh_tokens: BTreeMap<String, RefreshGrant>,
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            owner_password_hash: None,
            clients: BTreeMap::new(),
            refresh_tokens: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct AccessGrant {
    client_id: String,
    issuer: String,
    resource: String,
    scope: String,
    expires_at: i64,
}

#[derive(Clone, Debug)]
struct AuthorizationCode {
    client_id: String,
    issuer: String,
    redirect_uri: String,
    resource: String,
    scope: String,
    code_challenge: String,
    issue_refresh: bool,
    expires_at: i64,
}

#[derive(Clone, Debug)]
struct PendingAuthorization {
    client_id: String,
    client_name: Option<String>,
    issuer: String,
    redirect_uri: String,
    resource: String,
    scope: String,
    state: Option<String>,
    code_challenge: String,
    issue_refresh: bool,
    expires_at: i64,
}

struct RuntimeState {
    state_path: PathBuf,
    persistent: Mutex<PersistentState>,
    pending_clients: Mutex<HashMap<String, RegisteredClient>>,
    access_tokens: Mutex<HashMap<String, AccessGrant>>,
    authorization_codes: Mutex<HashMap<String, AuthorizationCode>>,
    pending: Mutex<HashMap<String, PendingAuthorization>>,
    password_checks: Semaphore,
}

#[derive(Clone)]
pub struct Runtime {
    inner: Arc<RuntimeState>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("state_path", &self.inner.state_path)
            .field("oauth_state", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct Server {
    runtime: Runtime,
    issuer: String,
    resource: String,
    resource_metadata_url: String,
}

pub struct OwnerPasswordChange {
    path: PathBuf,
    previous_hash: Option<String>,
    updated_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ClientMetadata {
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_name: Option<String>,
    redirect_uris: Vec<String>,
    #[serde(default)]
    grant_types: Vec<String>,
    #[serde(default)]
    response_types: Vec<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorizeQuery {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    code_challenge: String,
    code_challenge_method: String,
}

#[derive(Debug, Deserialize)]
struct AuthorizeForm {
    request_id: String,
    password: String,
    decision: String,
}

#[derive(Debug, Deserialize)]
struct TokenForm {
    grant_type: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RevokeForm {
    token: String,
}

#[derive(Debug, Deserialize)]
struct RegistrationRequest {
    #[serde(default)]
    client_name: Option<String>,
    redirect_uris: Vec<String>,
    #[serde(default)]
    grant_types: Vec<String>,
    #[serde(default)]
    response_types: Vec<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

fn client_metadata(client: &RegisteredClient) -> ClientMetadata {
    ClientMetadata {
        client_id: Some(client.client_id.clone()),
        client_name: client.client_name.clone(),
        redirect_uris: client.redirect_uris.clone(),
        grant_types: client.grant_types.clone(),
        response_types: client.response_types.clone(),
        token_endpoint_auth_method: Some(client.token_endpoint_auth_method.clone()),
        token_endpoint_auth_methods_supported: Vec::new(),
    }
}

impl Runtime {
    pub fn load(profile: Option<&str>) -> Result<Self> {
        let path = state_path(profile)?;
        let persistent = read_state(&path)?;
        let hash = persistent
            .owner_password_hash
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "OAuth is enabled but no owner password is configured; run `dotlink --setup{}`",
                    profile
                        .map(|name| format!(" -p {name}"))
                        .unwrap_or_default()
                )
            })?;
        PasswordHash::new(hash)
            .map_err(|error| anyhow!("stored OAuth owner password hash is invalid: {error}"))?;

        Ok(Self {
            inner: Arc::new(RuntimeState {
                state_path: path,
                persistent: Mutex::new(persistent),
                pending_clients: Mutex::new(HashMap::new()),
                access_tokens: Mutex::new(HashMap::new()),
                authorization_codes: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                password_checks: Semaphore::new(MAX_CONCURRENT_PASSWORD_CHECKS),
            }),
        })
    }

    fn update_persistent<T>(
        &self,
        update: impl FnOnce(&mut PersistentState) -> Result<T>,
    ) -> Result<T> {
        let mut memory = self
            .inner
            .persistent
            .lock()
            .map_err(|_| anyhow!("OAuth state lock poisoned"))?;
        let (latest, value) = update_state(&self.inner.state_path, update)?;
        *memory = latest;
        Ok(value)
    }

    fn reload_persistent(&self) -> Result<()> {
        let mut state = self
            .inner
            .persistent
            .lock()
            .map_err(|_| anyhow!("OAuth state lock poisoned"))?;
        let latest = read_state(&self.inner.state_path)?;
        *state = latest;
        Ok(())
    }

    async fn verify_owner_password(&self, password: &str) -> Result<bool> {
        if password.len() > 1024 {
            tokio::time::sleep(FAILED_PASSWORD_DELAY).await;
            return Ok(false);
        }

        self.reload_persistent()?;
        let hash = {
            let state = self
                .inner
                .persistent
                .lock()
                .map_err(|_| anyhow!("OAuth state lock poisoned"))?;
            state
                .owner_password_hash
                .clone()
                .ok_or_else(|| anyhow!("OAuth owner password is not configured"))?
        };

        let permit =
            tokio::time::timeout(PASSWORD_CHECK_WAIT, self.inner.password_checks.acquire())
                .await
                .map_err(|_| anyhow!("OAuth password verifier is busy; retry shortly"))?
                .map_err(|_| anyhow!("OAuth password verifier is unavailable"))?;
        let password = Zeroizing::new(password.as_bytes().to_vec());
        let valid = tokio::task::spawn_blocking(move || -> Result<bool> {
            let parsed = PasswordHash::new(&hash)
                .map_err(|error| anyhow!("stored OAuth owner hash is invalid: {error}"))?;
            Ok(Argon2::default()
                .verify_password(password.as_slice(), &parsed)
                .is_ok())
        })
        .await
        .context("OAuth password verifier task failed")??;
        if !valid {
            tokio::time::sleep(FAILED_PASSWORD_DELAY).await;
        }
        drop(permit);
        Ok(valid)
    }

    fn resolve_registered_client(&self, client_id: &str) -> Result<Option<ClientMetadata>> {
        self.reload_persistent()?;
        {
            let state = self
                .inner
                .persistent
                .lock()
                .map_err(|_| anyhow!("OAuth state lock poisoned"))?;
            if let Some(client) = state.clients.get(client_id) {
                return Ok(Some(client_metadata(client)));
            }
        }

        let mut pending = self
            .inner
            .pending_clients
            .lock()
            .map_err(|_| anyhow!("OAuth pending-client lock poisoned"))?;
        let cutoff = now() - PENDING_DCR_TTL_SECS;
        pending.retain(|_, client| client.issued_at >= cutoff);
        Ok(pending.get(client_id).map(client_metadata))
    }

    fn insert_dcr_client(&self, metadata: RegistrationRequest) -> Result<RegisteredClient> {
        validate_client_redirect_uris(&metadata.redirect_uris)?;

        let token_auth = metadata
            .token_endpoint_auth_method
            .as_deref()
            .unwrap_or("none");
        if token_auth != "none" {
            bail!("only public OAuth clients with token_endpoint_auth_method=none are supported");
        }

        let grant_types = if metadata.grant_types.is_empty() {
            vec!["authorization_code".to_owned(), "refresh_token".to_owned()]
        } else {
            metadata.grant_types
        };
        if !grant_types
            .iter()
            .all(|grant| matches!(grant.as_str(), "authorization_code" | "refresh_token"))
            || !grant_types
                .iter()
                .any(|grant| grant == "authorization_code")
        {
            bail!("unsupported OAuth grant_types");
        }

        let response_types = if metadata.response_types.is_empty() {
            vec!["code".to_owned()]
        } else {
            metadata.response_types
        };
        if response_types.iter().any(|value| value != "code") {
            bail!("unsupported OAuth response_types");
        }

        let client = RegisteredClient {
            client_id: format!("dcr_{}", random_token()?),
            client_name: metadata.client_name.map(|name| bounded_text(&name, 200)),
            redirect_uris: metadata.redirect_uris,
            grant_types,
            response_types,
            token_endpoint_auth_method: "none".to_owned(),
            issued_at: now(),
        };

        let mut pending = self
            .inner
            .pending_clients
            .lock()
            .map_err(|_| anyhow!("OAuth pending-client lock poisoned"))?;
        let cutoff = now() - PENDING_DCR_TTL_SECS;
        pending.retain(|_, existing| existing.issued_at >= cutoff);
        while pending.len() >= MAX_PENDING_DCR_CLIENTS {
            let Some(oldest) = pending
                .iter()
                .min_by_key(|(_, client)| client.issued_at)
                .map(|(client_id, _)| client_id.clone())
            else {
                break;
            };
            pending.remove(&oldest);
        }
        pending.insert(client.client_id.clone(), client.clone());
        Ok(client)
    }

    fn promote_dcr_client(&self, client_id: &str) -> Result<()> {
        if !client_id.starts_with("dcr_") {
            return Ok(());
        }

        let pending_client = {
            let mut pending = self
                .inner
                .pending_clients
                .lock()
                .map_err(|_| anyhow!("OAuth pending-client lock poisoned"))?;
            let cutoff = now() - PENDING_DCR_TTL_SECS;
            pending.retain(|_, existing| existing.issued_at >= cutoff);
            pending.get(client_id).cloned()
        };

        let promoted = self.update_persistent(|state| {
            if state.clients.contains_key(client_id) {
                return Ok(false);
            }
            let client = pending_client.clone().ok_or_else(|| {
                anyhow!("dynamic client registration expired; register again")
            })?;
            if state.clients.len() >= MAX_DCR_CLIENTS {
                bail!(
                    "approved OAuth client limit reached; revoke an unused client before approving another"
                );
            }
            state.clients.insert(client_id.to_owned(), client);
            Ok(true)
        })?;

        if promoted && let Ok(mut pending) = self.inner.pending_clients.lock() {
            pending.remove(client_id);
        }
        Ok(())
    }

    fn issue_access_and_refresh(
        &self,
        client_id: &str,
        issuer: &str,
        resource: &str,
        scope: &str,
        issue_refresh: bool,
    ) -> Result<(String, Option<String>)> {
        let refresh = if issue_refresh {
            let refresh = random_token()?;
            let grant = RefreshGrant {
                client_id: client_id.to_owned(),
                issuer: issuer.to_owned(),
                resource: resource.to_owned(),
                scope: scope.to_owned(),
                expires_at: now() + REFRESH_TOKEN_TTL_SECS,
            };
            self.update_persistent(|state| {
                if client_id.starts_with("dcr_") && !state.clients.contains_key(client_id) {
                    bail!("OAuth client is no longer approved");
                }
                make_room_for_refresh_grant(state);
                state.refresh_tokens.insert(token_hash(&refresh), grant);
                Ok(())
            })?;
            Some(refresh)
        } else {
            self.update_persistent(|state| {
                if client_id.starts_with("dcr_") && !state.clients.contains_key(client_id) {
                    bail!("OAuth client is no longer approved");
                }
                Ok(())
            })?;
            None
        };

        let access = random_token()?;
        let mut access_tokens = self
            .inner
            .access_tokens
            .lock()
            .map_err(|_| anyhow!("OAuth access-token lock poisoned"))?;
        make_room_for_access_token(&mut access_tokens);
        access_tokens.insert(
            token_hash(&access),
            AccessGrant {
                client_id: client_id.to_owned(),
                issuer: issuer.to_owned(),
                resource: resource.to_owned(),
                scope: scope.to_owned(),
                expires_at: now() + ACCESS_TOKEN_TTL_SECS,
            },
        );

        Ok((access, refresh))
    }

    fn rotate_refresh(
        &self,
        raw: &str,
        client_id: &str,
        issuer: &str,
        resource: &str,
    ) -> Result<Option<(String, String, String)>> {
        let hash = token_hash(raw);
        let current = now();
        let rotated = self.update_persistent(|state| {
            let Some(grant) = state.refresh_tokens.remove(&hash) else {
                return Ok(None);
            };
            if grant.expires_at <= current
                || grant.client_id != client_id
                || grant.issuer != issuer
                || grant.resource != resource
            {
                return Ok(None);
            }

            let new_refresh = random_token()?;
            let new_grant = RefreshGrant {
                expires_at: current + REFRESH_TOKEN_TTL_SECS,
                ..grant.clone()
            };
            make_room_for_refresh_grant(state);
            state
                .refresh_tokens
                .insert(token_hash(&new_refresh), new_grant);
            Ok(Some((grant, new_refresh)))
        })?;

        let Some((grant, new_refresh)) = rotated else {
            return Ok(None);
        };

        let access = random_token()?;
        let mut access_tokens = self
            .inner
            .access_tokens
            .lock()
            .map_err(|_| anyhow!("OAuth access-token lock poisoned"))?;
        make_room_for_access_token(&mut access_tokens);
        access_tokens.insert(
            token_hash(&access),
            AccessGrant {
                client_id: grant.client_id,
                issuer: issuer.to_owned(),
                resource: resource.to_owned(),
                scope: grant.scope.clone(),
                expires_at: current + ACCESS_TOKEN_TTL_SECS,
            },
        );

        Ok(Some((access, new_refresh, grant.scope)))
    }

    fn validate_access_token(&self, raw: &str, issuer: &str, resource: &str) -> Result<bool> {
        let hash = token_hash(raw);
        let current = now();
        let mut access = self
            .inner
            .access_tokens
            .lock()
            .map_err(|_| anyhow!("OAuth access-token lock poisoned"))?;
        access.retain(|_, grant| grant.expires_at > current);
        let Some(grant) = access.get(&hash) else {
            return Ok(false);
        };
        Ok(grant.issuer == issuer
            && grant.resource == resource
            && scope_contains(&grant.scope, SCOPE_MCP)
            && !grant.client_id.is_empty())
    }

    fn revoke(&self, raw: &str) -> Result<()> {
        let hash = token_hash(raw);
        self.inner
            .access_tokens
            .lock()
            .map_err(|_| anyhow!("OAuth access-token lock poisoned"))?
            .remove(&hash);

        self.update_persistent(|state| {
            state.refresh_tokens.remove(&hash);
            Ok(())
        })
    }
}

impl Server {
    pub fn new(runtime: Runtime, public_base: Url, mcp_path: &str) -> Result<Self> {
        let issuer = canonical_public_origin(&public_base)?;
        let resource = resource_url(&issuer, mcp_path)?;
        let resource_metadata_url = format!("{issuer}/.well-known/oauth-protected-resource");
        Ok(Self {
            runtime,
            issuer,
            resource,
            resource_metadata_url,
        })
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn resource(&self) -> &str {
        &self.resource
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route(
                "/.well-known/oauth-protected-resource",
                get(protected_resource_metadata),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(authorization_server_metadata),
            )
            .route("/oauth/authorize", get(authorize_get).post(authorize_post))
            .route("/oauth/token", post(token))
            .route("/oauth/register", post(register))
            .route("/oauth/revoke", post(revoke))
            .layer(DefaultBodyLimit::max(MAX_CLIENT_METADATA_BYTES))
            .with_state(self.clone())
    }

    pub async fn require_bearer(&self, request: Request<Body>, next: Next) -> Response {
        let Some(raw) = bearer_token(request.headers()) else {
            return self.unauthorized(false);
        };

        match self
            .runtime
            .validate_access_token(raw, &self.issuer, &self.resource)
        {
            Ok(true) => next.run(request).await,
            Ok(false) | Err(_) => self.unauthorized(true),
        }
    }

    fn unauthorized(&self, invalid_token: bool) -> Response {
        let challenge = if invalid_token {
            format!(
                r#"Bearer resource_metadata="{}", scope="{}", error="invalid_token""#,
                self.resource_metadata_url, SCOPE_MCP
            )
        } else {
            format!(
                r#"Bearer resource_metadata="{}", scope="{}""#,
                self.resource_metadata_url, SCOPE_MCP
            )
        };

        let mut response = (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "unauthorized",
                "error_description": "OAuth authorization is required for this MCP endpoint"
            })),
        )
            .into_response();
        if let Ok(value) = HeaderValue::from_str(&challenge) {
            response.headers_mut().insert(WWW_AUTHENTICATE, value);
        }
        response
    }

    async fn resolve_client(&self, client_id: &str) -> Result<ClientMetadata> {
        if let Some(metadata) = self.runtime.resolve_registered_client(client_id)? {
            return Ok(metadata);
        }
        fetch_cimd(client_id).await
    }
}

pub fn state_dir() -> Result<PathBuf> {
    let base = if let Some(xdg) = env::var_os("XDG_STATE_HOME") {
        PathBuf::from(xdg)
    } else if cfg!(windows) {
        env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or(home_dir()?.join(".local/state"))
    } else {
        home_dir()?.join(".local/state")
    };
    Ok(base.join("abird/dotlink"))
}

pub fn state_path(profile: Option<&str>) -> Result<PathBuf> {
    let dir = state_dir()?;
    Ok(match profile {
        Some(profile) => {
            validate_profile_component(profile)?;
            dir.join(format!("oauth.{profile}.json"))
        }
        None => dir.join("oauth.json"),
    })
}

fn validate_profile_component(profile: &str) -> Result<()> {
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

pub fn owner_configured(profile: Option<&str>) -> Result<bool> {
    Ok(read_state(&state_path(profile)?)?
        .owner_password_hash
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty()))
}

pub fn prepare_owner_password(
    profile: Option<&str>,
    password: &str,
) -> Result<OwnerPasswordChange> {
    validate_owner_password(password)?;
    let path = state_path(profile)?;
    let previous_hash = read_state(&path)?.owner_password_hash;

    let mut salt_bytes = [0u8; 16];
    getrandom::fill(&mut salt_bytes)
        .map_err(|error| anyhow!("failed to generate OAuth password salt: {error}"))?;
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|error| anyhow!("failed to encode OAuth password salt: {error}"))?;
    let updated_hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| anyhow!("failed to hash OAuth owner password: {error}"))?
        .to_string();

    Ok(OwnerPasswordChange {
        path,
        previous_hash,
        updated_hash,
    })
}

impl OwnerPasswordChange {
    pub fn apply(&self) -> Result<()> {
        let updated = self.updated_hash.clone();
        update_state(&self.path, move |state| {
            state.owner_password_hash = Some(updated);
            Ok(())
        })?;
        Ok(())
    }

    pub fn rollback(&self) -> Result<()> {
        let previous = self.previous_hash.clone();
        let updated = self.updated_hash.clone();
        let (state, ()) = update_state(&self.path, move |state| {
            if state.owner_password_hash.as_deref() == Some(updated.as_str()) {
                state.owner_password_hash = previous;
            }
            Ok(())
        })?;

        if state == PersistentState::default() {
            let _lock = open_state_lock(&self.path)?;
            if read_state(&self.path)? == PersistentState::default() {
                match fs::remove_file(&self.path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("failed to remove {}", self.path.display()));
                    }
                }
            }
        }
        Ok(())
    }
}

pub fn delete_profile_state(profile: Option<&str>) -> Result<()> {
    let path = state_path(profile)?;
    let _lock = open_state_lock(&path)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

pub fn status(profile: Option<&str>) -> Result<String> {
    let path = state_path(profile)?;
    let state = read_state(&path)?;
    let owner = state.owner_password_hash.is_some();
    Ok(format!(
        "OAuth state: {}\nOwner credential: {}\nDCR clients: {}\nRefresh grants: {}\n",
        path.display(),
        if owner {
            "configured"
        } else {
            "not configured"
        },
        state.clients.len(),
        state.refresh_tokens.len()
    ))
}

pub fn clients(profile: Option<&str>) -> Result<Vec<String>> {
    let state = read_state(&state_path(profile)?)?;
    Ok(state
        .clients
        .values()
        .map(|client| {
            format!(
                "{}\t{}\t{} redirect URI(s)",
                client.client_id,
                client.client_name.as_deref().unwrap_or("[unnamed]"),
                client.redirect_uris.len()
            )
        })
        .collect())
}

pub fn revoke_client(profile: Option<&str>, client_id: &str) -> Result<bool> {
    let path = state_path(profile)?;
    let (_, changed) = update_state(&path, |state| {
        let removed_client = state.clients.remove(client_id).is_some();
        let before = state.refresh_tokens.len();
        state
            .refresh_tokens
            .retain(|_, grant| grant.client_id != client_id);
        Ok(removed_client || before != state.refresh_tokens.len())
    })?;
    Ok(changed)
}

pub fn revoke_all(profile: Option<&str>) -> Result<usize> {
    let path = state_path(profile)?;
    let (_, count) = update_state(&path, |state| {
        let count = state.refresh_tokens.len();
        state.refresh_tokens.clear();
        Ok(count)
    })?;
    Ok(count)
}

pub fn validate_public_base_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("OAuth public URL is invalid")?;
    canonical_public_origin(&url)?;
    Ok(url)
}

async fn protected_resource_metadata(State(server): State<Server>) -> Response {
    public_json(json!({
        "resource": server.resource,
        "authorization_servers": [server.issuer],
        "scopes_supported": [SCOPE_MCP, SCOPE_OFFLINE],
        "bearer_methods_supported": ["header"]
    }))
}

async fn authorization_server_metadata(State(server): State<Server>) -> Response {
    public_json(json!({
        "issuer": server.issuer,
        "authorization_response_iss_parameter_supported": true,
        "authorization_endpoint": format!("{}/oauth/authorize", server.issuer),
        "token_endpoint": format!("{}/oauth/token", server.issuer),
        "registration_endpoint": format!("{}/oauth/register", server.issuer),
        "revocation_endpoint": format!("{}/oauth/revoke", server.issuer),
        "client_id_metadata_document_supported": true,
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["none"],
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": [SCOPE_MCP, SCOPE_OFFLINE]
    }))
}

async fn authorize_get(
    State(server): State<Server>,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    match prepare_authorization(&server, query).await {
        Ok(pending_id) => {
            let request = server
                .runtime
                .inner
                .pending
                .lock()
                .ok()
                .and_then(|pending| pending.get(&pending_id).cloned());
            match request {
                Some(request) => authorization_page(&pending_id, &request, None),
                None => oauth_error_page(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "Authorization request could not be stored",
                ),
            }
        }
        Err(error) => oauth_error_page(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            &bounded_text(&format!("{error:#}"), 500),
        ),
    }
}

async fn prepare_authorization(server: &Server, query: AuthorizeQuery) -> Result<String> {
    if query.response_type != "code" {
        bail!("response_type must be code");
    }
    ensure_max_len("client_id", &query.client_id, 4096)?;
    ensure_max_len("redirect_uri", &query.redirect_uri, 4096)?;
    ensure_max_len(
        "resource",
        query.resource.as_deref().unwrap_or_default(),
        4096,
    )?;
    ensure_max_len("scope", query.scope.as_deref().unwrap_or_default(), 512)?;
    ensure_max_len("state", query.state.as_deref().unwrap_or_default(), 4096)?;
    if query.code_challenge_method != "S256" {
        bail!("code_challenge_method must be S256");
    }
    validate_code_challenge(&query.code_challenge)?;

    let resource = query
        .resource
        .as_deref()
        .ok_or_else(|| anyhow!("resource is required"))?;
    if resource != server.resource {
        bail!("resource does not match this MCP server");
    }

    let metadata = server.resolve_client(&query.client_id).await?;
    validate_client_metadata(&query.client_id, &metadata)?;
    if !metadata
        .redirect_uris
        .iter()
        .any(|uri| uri == &query.redirect_uri)
    {
        bail!("redirect_uri is not registered for this client");
    }

    let scope = normalize_scope(query.scope.as_deref())?;
    let issue_refresh = metadata
        .grant_types
        .iter()
        .any(|grant| grant == "refresh_token");
    if scope_contains(&scope, SCOPE_OFFLINE) && !issue_refresh {
        bail!("client does not support refresh_token grant required by offline_access");
    }

    let request_id = random_token()?;
    let pending = PendingAuthorization {
        client_id: query.client_id,
        client_name: metadata.client_name.map(|name| bounded_text(&name, 200)),
        issuer: server.issuer.clone(),
        redirect_uri: query.redirect_uri,
        resource: server.resource.clone(),
        scope,
        state: query.state,
        code_challenge: query.code_challenge,
        issue_refresh,
        expires_at: now() + PENDING_AUTH_TTL_SECS,
    };

    let mut requests = server
        .runtime
        .inner
        .pending
        .lock()
        .map_err(|_| anyhow!("OAuth pending-request lock poisoned"))?;
    requests.retain(|_, request| request.expires_at > now());
    while requests.len() >= MAX_PENDING_AUTHORIZATIONS {
        let Some(oldest) = requests
            .iter()
            .min_by_key(|(_, request)| request.expires_at)
            .map(|(request_id, _)| request_id.clone())
        else {
            break;
        };
        requests.remove(&oldest);
    }
    requests.insert(request_id.clone(), pending);
    Ok(request_id)
}

async fn authorize_post(State(server): State<Server>, Form(form): Form<AuthorizeForm>) -> Response {
    if ensure_max_len("request_id", &form.request_id, 512).is_err()
        || ensure_max_len("decision", &form.decision, 16).is_err()
        || !matches!(form.decision.as_str(), "allow" | "deny")
    {
        return oauth_error_page(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Authorization form is invalid",
        );
    }

    let request = {
        let mut pending = match server.runtime.inner.pending.lock() {
            Ok(pending) => pending,
            Err(_) => {
                return oauth_error_page(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "Authorization state is unavailable",
                );
            }
        };
        pending.remove(&form.request_id)
    };

    let Some(request) = request else {
        return oauth_error_page(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Authorization request expired or is invalid",
        );
    };
    if request.expires_at <= now()
        || request.issuer != server.issuer
        || request.resource != server.resource
    {
        return oauth_error_page(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Authorization request expired",
        );
    }

    if form.decision != "allow" {
        return redirect_authorization_error(
            &request.redirect_uri,
            request.state.as_deref(),
            &server.issuer,
            "access_denied",
            "The resource owner denied the request",
        );
    }

    let password = Zeroizing::new(form.password);
    match server
        .runtime
        .verify_owner_password(password.as_str())
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            let retry_id = match random_token() {
                Ok(id) => id,
                Err(_) => {
                    return oauth_error_page(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Could not retry authorization",
                    );
                }
            };
            if let Ok(mut pending) = server.runtime.inner.pending.lock() {
                pending.insert(retry_id.clone(), request.clone());
            }
            return authorization_page(&retry_id, &request, Some("Invalid owner password"));
        }
        Err(_) => {
            return oauth_error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Owner authentication failed",
            );
        }
    }

    if let Err(error) = server.runtime.promote_dcr_client(&request.client_id) {
        return oauth_error_page(
            StatusCode::BAD_REQUEST,
            "invalid_client",
            &bounded_text(&format!("{error:#}"), 400),
        );
    }

    let code = match random_token() {
        Ok(code) => code,
        Err(_) => {
            return oauth_error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Could not issue authorization code",
            );
        }
    };

    let record = AuthorizationCode {
        client_id: request.client_id,
        issuer: server.issuer.clone(),
        redirect_uri: request.redirect_uri.clone(),
        resource: request.resource,
        scope: request.scope,
        code_challenge: request.code_challenge,
        issue_refresh: request.issue_refresh,
        expires_at: now() + AUTH_CODE_TTL_SECS,
    };
    if let Ok(mut codes) = server.runtime.inner.authorization_codes.lock() {
        codes.retain(|_, record| record.expires_at > now());
        while codes.len() >= MAX_AUTHORIZATION_CODES {
            let Some(oldest) = codes
                .iter()
                .min_by_key(|(_, record)| record.expires_at)
                .map(|(hash, _)| hash.clone())
            else {
                break;
            };
            codes.remove(&oldest);
        }
        codes.insert(token_hash(&code), record);
    } else {
        return oauth_error_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Authorization state is unavailable",
        );
    }

    redirect_authorization_success(
        &request.redirect_uri,
        &code,
        request.state.as_deref(),
        &server.issuer,
    )
}

async fn token(State(server): State<Server>, Form(form): Form<TokenForm>) -> Response {
    if ensure_max_len("grant_type", &form.grant_type, 64).is_err() {
        return token_error("invalid_request", "grant_type is too long");
    }

    let result = match form.grant_type.as_str() {
        "authorization_code" => exchange_authorization_code(&server, form),
        "refresh_token" => exchange_refresh_token(&server, form),
        _ => {
            return token_error(
                "unsupported_grant_type",
                "grant_type must be authorization_code or refresh_token",
            );
        }
    };

    match result {
        Ok(value) => no_store_json(StatusCode::OK, value),
        Err(error) => token_error("invalid_grant", &bounded_text(&format!("{error:#}"), 400)),
    }
}

fn exchange_authorization_code(server: &Server, form: TokenForm) -> Result<Value> {
    let code = form.code.ok_or_else(|| anyhow!("code is required"))?;
    let client_id = form
        .client_id
        .ok_or_else(|| anyhow!("client_id is required"))?;
    let redirect_uri = form
        .redirect_uri
        .ok_or_else(|| anyhow!("redirect_uri is required"))?;
    let verifier = form
        .code_verifier
        .ok_or_else(|| anyhow!("code_verifier is required"))?;
    let resource = form
        .resource
        .ok_or_else(|| anyhow!("resource is required"))?;

    ensure_max_len("code", &code, 512)?;
    ensure_max_len("client_id", &client_id, 4096)?;
    ensure_max_len("redirect_uri", &redirect_uri, 4096)?;
    ensure_max_len("resource", &resource, 4096)?;

    if resource != server.resource {
        bail!("resource does not match this MCP server");
    }
    validate_code_verifier(&verifier)?;

    let record = server
        .runtime
        .inner
        .authorization_codes
        .lock()
        .map_err(|_| anyhow!("OAuth authorization-code lock poisoned"))?
        .remove(&token_hash(&code))
        .ok_or_else(|| anyhow!("authorization code is invalid or already used"))?;

    if record.expires_at <= now()
        || record.client_id != client_id
        || record.redirect_uri != redirect_uri
        || record.issuer != server.issuer
        || record.resource != server.resource
        || pkce_challenge(&verifier) != record.code_challenge
    {
        bail!("authorization code validation failed");
    }

    let (access, refresh) = server.runtime.issue_access_and_refresh(
        &client_id,
        &server.issuer,
        &server.resource,
        &record.scope,
        record.issue_refresh,
    )?;
    Ok(token_response(access, refresh, record.scope))
}

fn exchange_refresh_token(server: &Server, form: TokenForm) -> Result<Value> {
    let refresh = form
        .refresh_token
        .ok_or_else(|| anyhow!("refresh_token is required"))?;
    let client_id = form
        .client_id
        .ok_or_else(|| anyhow!("client_id is required"))?;
    let resource = form
        .resource
        .ok_or_else(|| anyhow!("resource is required"))?;
    ensure_max_len("refresh_token", &refresh, 512)?;
    ensure_max_len("client_id", &client_id, 4096)?;
    ensure_max_len("resource", &resource, 4096)?;
    if resource != server.resource {
        bail!("resource does not match this MCP server");
    }

    let Some((access, new_refresh, scope)) =
        server
            .runtime
            .rotate_refresh(&refresh, &client_id, &server.issuer, &server.resource)?
    else {
        bail!("refresh token is invalid or expired");
    };

    Ok(token_response(access, Some(new_refresh), scope))
}

async fn register(
    State(server): State<Server>,
    Json(request): Json<RegistrationRequest>,
) -> Response {
    match server.runtime.insert_dcr_client(request) {
        Ok(client) => no_store_json(
            StatusCode::CREATED,
            json!({
                "client_id": client.client_id,
                "client_id_issued_at": client.issued_at,
                "client_name": client.client_name,
                "redirect_uris": client.redirect_uris,
                "grant_types": client.grant_types,
                "response_types": client.response_types,
                "token_endpoint_auth_method": client.token_endpoint_auth_method
            }),
        ),
        Err(error) => no_store_json(
            StatusCode::BAD_REQUEST,
            json!({
                "error": "invalid_client_metadata",
                "error_description": bounded_text(&format!("{error:#}"), 400)
            }),
        ),
    }
}

async fn revoke(State(server): State<Server>, Form(form): Form<RevokeForm>) -> Response {
    if form.token.len() > 512 {
        return StatusCode::OK.into_response();
    }

    match server.runtime.revoke(&form.token) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => no_store_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "error": "server_error",
                "error_description": "Token revocation could not be persisted"
            }),
        ),
    }
}

fn token_response(access: String, refresh: Option<String>, scope: String) -> Value {
    let mut value = json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ACCESS_TOKEN_TTL_SECS,
        "scope": scope
    });
    if let Some(refresh) = refresh {
        value["refresh_token"] = Value::String(refresh);
    }
    value
}

fn token_error(error: &str, description: &str) -> Response {
    no_store_json(
        StatusCode::BAD_REQUEST,
        json!({
            "error": error,
            "error_description": description
        }),
    )
}

fn authorization_page(
    request_id: &str,
    request: &PendingAuthorization,
    error: Option<&str>,
) -> Response {
    let client = request.client_name.as_deref().unwrap_or(&request.client_id);
    let error = error
        .map(|message| format!(r#"<p class="error">{}</p>"#, html_escape(message)))
        .unwrap_or_default();
    let body = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>abird dotlink authorization</title><style>body{{font:16px system-ui,sans-serif;max-width:560px;margin:10vh auto;padding:0 20px;color:#171717}}h1{{font-size:24px}}code{{word-break:break-all}}input,button{{font:inherit;padding:10px;margin-top:8px}}input{{width:100%;box-sizing:border-box}}button{{margin-right:8px}}.muted{{color:#666}}.error{{color:#b00020}}</style></head><body><h1>abird dotlink</h1><p><strong>{}</strong> is requesting access to this dotlink MCP server.</p><p class="muted">Client ID: <code>{}</code></p><p class="muted">Redirect: <code>{}</code></p><p class="muted">Resource: <code>{}</code></p><p class="muted">Scope: <code>{}</code></p>{}<form method="post" action="/oauth/authorize"><input type="hidden" name="request_id" value="{}"><label>Owner password<input type="password" name="password" autocomplete="current-password" required autofocus></label><div><button type="submit" name="decision" value="allow">Allow</button><button type="submit" name="decision" value="deny" formnovalidate>Deny</button></div></form></body></html>"#,
        html_escape(client),
        html_escape(&request.client_id),
        html_escape(&request.redirect_uri),
        html_escape(&request.resource),
        html_escape(&request.scope),
        error,
        html_escape(request_id)
    );
    hardened_html(StatusCode::OK, body)
}

fn oauth_error_page(status: StatusCode, error: &str, description: &str) -> Response {
    let body = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>dotlink OAuth error</title></head><body><h1>OAuth error</h1><p><strong>{}</strong></p><p>{}</p></body></html>"#,
        html_escape(error),
        html_escape(description)
    );
    hardened_html(status, body)
}

fn hardened_html(status: StatusCode, body: String) -> Response {
    let mut response = (status, Html(body)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

fn public_json(value: Value) -> Response {
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert("access-control-allow-origin", HeaderValue::from_static("*"));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn no_store_json(status: StatusCode, value: Value) -> Response {
    let mut response = (status, Json(value)).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

fn redirect_authorization_success(
    redirect_uri: &str,
    code: &str,
    state: Option<&str>,
    issuer: &str,
) -> Response {
    let mut params = vec![("code", code), ("iss", issuer)];
    if let Some(state) = state {
        params.push(("state", state));
    }
    redirect_with_params(redirect_uri, &params)
}

fn redirect_authorization_error(
    redirect_uri: &str,
    state: Option<&str>,
    issuer: &str,
    error: &str,
    description: &str,
) -> Response {
    let mut params = vec![
        ("error", error),
        ("error_description", description),
        ("iss", issuer),
    ];
    if let Some(state) = state {
        params.push(("state", state));
    }
    redirect_with_params(redirect_uri, &params)
}

fn redirect_with_params(redirect_uri: &str, params: &[(&str, &str)]) -> Response {
    let Ok(mut url) = Url::parse(redirect_uri) else {
        return oauth_error_page(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "redirect_uri is invalid",
        );
    };
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in params {
            query.append_pair(key, value);
        }
    }
    Redirect::to(url.as_str()).into_response()
}

async fn fetch_cimd(client_id: &str) -> Result<ClientMetadata> {
    let url = Url::parse(client_id).context("client_id is not a registered DCR client or URL")?;
    if url.scheme() != "https"
        || url.path().is_empty()
        || url.path() == "/"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!(
            "CIMD client_id must be a stable HTTPS metadata-document URL with a non-root path and no query/fragment"
        );
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("CIMD URL has no host"))?
        .to_owned();
    let port = url.port_or_known_default().unwrap_or(443);
    let mut addresses = tokio::net::lookup_host((host.as_str(), port))
        .await
        .context("failed to resolve CIMD host")?
        .collect::<Vec<_>>();
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() || addresses.iter().any(|addr| !is_public_ip(addr.ip())) {
        bail!("CIMD host resolves to a non-public address");
    }

    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(5))
        .https_only(true)
        .resolve_to_addrs(&host, &addresses)
        .user_agent(format!("dotlink-oauth/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build CIMD HTTP client")?;
    let mut response = client
        .get(url.clone())
        .send()
        .await
        .context("failed to fetch CIMD metadata")?;
    if !response.status().is_success() {
        bail!("CIMD metadata returned HTTP {}", response.status());
    }
    if response
        .content_length()
        .is_some_and(|length| length as usize > MAX_CLIENT_METADATA_BYTES)
    {
        bail!("CIMD metadata exceeds size limit");
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read CIMD metadata")?
    {
        if body.len() + chunk.len() > MAX_CLIENT_METADATA_BYTES {
            bail!("CIMD metadata exceeds size limit");
        }
        body.extend_from_slice(&chunk);
    }

    let metadata: ClientMetadata =
        serde_json::from_slice(&body).context("CIMD metadata is invalid JSON")?;
    if metadata.client_id.as_deref() != Some(client_id) {
        bail!("CIMD document client_id does not match requested client_id");
    }
    validate_client_metadata(client_id, &metadata)?;
    Ok(metadata)
}

fn validate_client_metadata(client_id: &str, metadata: &ClientMetadata) -> Result<()> {
    validate_client_redirect_uris(&metadata.redirect_uris)?;
    if !metadata.response_types.is_empty()
        && !metadata.response_types.iter().any(|value| value == "code")
    {
        bail!("client does not support response_type=code");
    }
    if !metadata.grant_types.is_empty()
        && !metadata
            .grant_types
            .iter()
            .any(|value| value == "authorization_code")
    {
        bail!("client does not support authorization_code grant");
    }

    let supports_none = if metadata.token_endpoint_auth_methods_supported.is_empty() {
        metadata
            .token_endpoint_auth_method
            .as_deref()
            .is_none_or(|value| value == "none")
    } else {
        metadata
            .token_endpoint_auth_methods_supported
            .iter()
            .any(|value| value == "none")
    };
    if !supports_none {
        bail!("client metadata does not allow public token exchange");
    }
    if let Some(document_client_id) = metadata.client_id.as_deref()
        && document_client_id != client_id
    {
        bail!("client metadata client_id mismatch");
    }
    Ok(())
}

fn validate_client_redirect_uris(uris: &[String]) -> Result<()> {
    if uris.is_empty() || uris.len() > 16 {
        bail!("client must register between 1 and 16 redirect URIs");
    }
    for value in uris {
        ensure_max_len("redirect_uri", value, 4096)?;
        let url = Url::parse(value).with_context(|| format!("invalid redirect URI {value:?}"))?;
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            bail!("redirect URI must not contain credentials or fragment");
        }
        match url.scheme() {
            "https" => {
                if url.host_str().is_none() {
                    bail!("HTTPS redirect URI requires a host");
                }
            }
            "http" => {
                let loopback = match url.host() {
                    Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
                    Some(Host::Ipv4(ip)) => ip.is_loopback(),
                    Some(Host::Ipv6(ip)) => ip.is_loopback(),
                    None => false,
                };
                if !loopback {
                    bail!("HTTP redirect URIs are allowed only on loopback hosts");
                }
            }
            _ => bail!("redirect URI must use HTTPS or loopback HTTP"),
        }
    }
    Ok(())
}

fn ensure_max_len(name: &str, value: &str, max: usize) -> Result<()> {
    if value.len() > max {
        bail!("{name} exceeds the maximum supported length");
    }
    Ok(())
}

pub fn validate_owner_password(password: &str) -> Result<()> {
    if password.len() < OWNER_PASSWORD_MIN_LEN {
        bail!("OAuth owner password must be at least {OWNER_PASSWORD_MIN_LEN} characters");
    }
    if password.len() > 1024 {
        bail!("OAuth owner password is too long");
    }
    Ok(())
}

fn validate_code_challenge(value: &str) -> Result<()> {
    if value.len() != 43 || !value.bytes().all(is_pkce_char) {
        bail!("PKCE S256 code_challenge is invalid");
    }
    Ok(())
}

fn validate_code_verifier(value: &str) -> Result<()> {
    if !(43..=128).contains(&value.len()) || !value.bytes().all(is_pkce_char) {
        bail!("PKCE code_verifier is invalid");
    }
    Ok(())
}

fn is_pkce_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn normalize_scope(value: Option<&str>) -> Result<String> {
    let mut scopes = value
        .unwrap_or(SCOPE_MCP)
        .split_ascii_whitespace()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !scopes.iter().any(|scope| scope == SCOPE_MCP) {
        scopes.push(SCOPE_MCP.to_owned());
    }
    if scopes
        .iter()
        .any(|scope| scope != SCOPE_MCP && scope != SCOPE_OFFLINE)
    {
        bail!("unsupported OAuth scope");
    }
    scopes.sort();
    scopes.dedup();
    Ok(scopes.join(" "))
}

fn scope_contains(scope: &str, expected: &str) -> bool {
    scope
        .split_ascii_whitespace()
        .any(|candidate| candidate == expected)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.trim().is_empty() {
        return None;
    }
    Some(token.trim())
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow!("failed to generate OAuth random token: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn token_hash(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}

fn now() -> i64 {
    Utc::now().timestamp()
}

fn make_room_for_access_token(tokens: &mut HashMap<String, AccessGrant>) {
    let current = now();
    tokens.retain(|_, grant| grant.expires_at > current);
    while tokens.len() >= MAX_ACCESS_TOKENS {
        let Some(oldest) = tokens
            .iter()
            .min_by_key(|(_, grant)| grant.expires_at)
            .map(|(hash, _)| hash.clone())
        else {
            break;
        };
        tokens.remove(&oldest);
    }
}

fn make_room_for_refresh_grant(state: &mut PersistentState) {
    let current = now();
    state
        .refresh_tokens
        .retain(|_, grant| grant.expires_at > current);
    while state.refresh_tokens.len() >= MAX_REFRESH_GRANTS {
        let Some(oldest) = state
            .refresh_tokens
            .iter()
            .min_by_key(|(_, grant)| grant.expires_at)
            .map(|(hash, _)| hash.clone())
        else {
            break;
        };
        state.refresh_tokens.remove(&oldest);
    }
}

fn state_lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    PathBuf::from(value)
}

fn open_state_lock(path: &Path) -> Result<fs::File> {
    let lock_path = state_lock_path(path);
    let parent = lock_path
        .parent()
        .ok_or_else(|| anyhow!("OAuth state lock path has no parent"))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    set_private_dir_permissions(parent)?;

    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    set_private_file_permissions(&lock_path)?;
    lock.lock()
        .with_context(|| format!("failed to lock {}", lock_path.display()))?;
    Ok(lock)
}

fn update_state<T>(
    path: &Path,
    update: impl FnOnce(&mut PersistentState) -> Result<T>,
) -> Result<(PersistentState, T)> {
    let _lock = open_state_lock(path)?;
    let mut state = read_state(path)?;
    let before = state.clone();
    let value = update(&mut state)?;
    if state != before {
        write_state(path, &state)?;
    }
    Ok((state, value))
}

fn read_state(path: &Path) -> Result<PersistentState> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PersistentState::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let state: PersistentState = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if state.version != STATE_VERSION {
        bail!(
            "unsupported OAuth state version {} in {}",
            state.version,
            path.display()
        );
    }
    Ok(state)
}

fn write_state(path: &Path, state: &PersistentState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        set_private_dir_permissions(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    atomic_write_private(path, &bytes, true)
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

fn canonical_public_origin(url: &Url) -> Result<String> {
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("OAuth public URL must not contain credentials, query, or fragment");
    }
    if !matches!(url.path(), "" | "/") {
        bail!("OAuth public URL must be an origin without a path");
    }

    let secure = url.scheme() == "https" && url.host_str().is_some();
    let loopback_http = url.scheme() == "http"
        && match url.host() {
            Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        };
    if !secure && !loopback_http {
        bail!("OAuth public URL must use HTTPS (loopback HTTP is allowed for local testing)");
    }

    let mut origin = format!("{}://", url.scheme());
    match url.host() {
        Some(Host::Domain(host)) => origin.push_str(host),
        Some(Host::Ipv4(ip)) => origin.push_str(&ip.to_string()),
        Some(Host::Ipv6(ip)) => origin.push_str(&format!("[{ip}]")),
        None => bail!("OAuth public URL requires a host"),
    }
    if let Some(port) = url.port() {
        let default =
            (url.scheme() == "https" && port == 443) || (url.scheme() == "http" && port == 80);
        if !default {
            origin.push(':');
            origin.push_str(&port.to_string());
        }
    }
    Ok(origin)
}

fn resource_url(issuer: &str, mcp_path: &str) -> Result<String> {
    if !mcp_path.starts_with('/') {
        bail!("MCP path must start with /");
    }
    Ok(format!("{}{}", issuer.trim_end_matches('/'), mcp_path))
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _d] = ip.octets();
    if a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
    {
        return false;
    }
    true
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    let segments = ip.segments();
    if (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
    {
        return false;
    }
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    true
}

fn bounded_text(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn pkce_s256_matches_rfc_example() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn public_url_requires_https_or_loopback_http() {
        assert!(validate_public_base_url("https://example.com").is_ok());
        assert!(validate_public_base_url("http://127.0.0.1:3000").is_ok());
        assert!(validate_public_base_url("http://localhost:3000").is_ok());
        assert!(validate_public_base_url("http://example.com").is_err());
        assert!(validate_public_base_url("https://example.com/path").is_err());
    }

    #[test]
    fn resource_is_bound_to_public_origin_and_mcp_path() {
        let server = Server::new(
            Runtime {
                inner: Arc::new(RuntimeState {
                    state_path: PathBuf::from("/tmp/unused"),
                    persistent: Mutex::new(PersistentState {
                        owner_password_hash: Some(
                            "$argon2id$v=19$m=19456,t=2,p=1$YWJjZGVmZ2hpamtsbW5vcA$RmFrZUhhc2hUaGF0SXNOZXZlclVzZWQ".to_owned(),
                        ),
                        ..PersistentState::default()
                    }),
                    pending_clients: Mutex::new(HashMap::new()),
                    access_tokens: Mutex::new(HashMap::new()),
                    authorization_codes: Mutex::new(HashMap::new()),
                    pending: Mutex::new(HashMap::new()),
                    password_checks: Semaphore::new(MAX_CONCURRENT_PASSWORD_CHECKS),
                }),
            },
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        assert_eq!(server.issuer(), "https://example.com");
        assert_eq!(server.resource(), "https://example.com/mcp");
    }

    #[test]
    fn private_addresses_are_rejected_for_cimd() {
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("10.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("192.168.1.1".parse().unwrap()));
        assert!(!is_public_ip("::1".parse().unwrap()));
        assert!(!is_public_ip("fd00::1".parse().unwrap()));
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn cimd_plural_token_auth_methods_are_authoritative() {
        let client_id = "https://chatgpt.com/oauth/client.json";
        let base = ClientMetadata {
            client_id: Some(client_id.to_owned()),
            client_name: Some("ChatGPT".to_owned()),
            redirect_uris: vec!["https://chatgpt.com/connector_platform_oauth_redirect".to_owned()],
            grant_types: vec!["authorization_code".to_owned(), "refresh_token".to_owned()],
            response_types: vec!["code".to_owned(), "token".to_owned()],
            token_endpoint_auth_method: Some("private_key_jwt".to_owned()),
            token_endpoint_auth_methods_supported: vec![
                "none".to_owned(),
                "private_key_jwt".to_owned(),
            ],
        };
        assert!(validate_client_metadata(client_id, &base).is_ok());

        let mut private_key_only = base;
        private_key_only.token_endpoint_auth_methods_supported = vec!["private_key_jwt".to_owned()];
        private_key_only.token_endpoint_auth_method = None;
        assert!(validate_client_metadata(client_id, &private_key_only).is_err());
    }

    #[tokio::test]
    async fn cimd_client_id_rejects_query_strings_before_network() {
        let error = fetch_cimd("https://chatgpt.com/oauth/client.json?variant=1")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no query/fragment"));
    }

    #[test]
    fn redirect_validation_allows_https_and_loopback_only() {
        assert!(
            validate_client_redirect_uris(&[
                "https://chatgpt.com/connector_platform_oauth_redirect".to_owned(),
                "http://127.0.0.1:54321/callback".to_owned(),
            ])
            .is_ok()
        );
        for invalid in [
            "http://example.com/callback",
            "myapp://oauth/callback",
            "javascript:alert(1)",
            "file:///tmp/callback",
        ] {
            assert!(
                validate_client_redirect_uris(&[invalid.to_owned()]).is_err(),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn scope_is_small_and_explicit() {
        assert_eq!(normalize_scope(None).unwrap(), SCOPE_MCP);
        assert_eq!(
            normalize_scope(Some("offline_access mcp:access")).unwrap(),
            "mcp:access offline_access"
        );
        assert!(normalize_scope(Some("admin")).is_err());
    }

    #[test]
    fn bearer_parser_is_case_insensitive() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("bearer abc123"));
        assert_eq!(bearer_token(&headers), Some("abc123"));
    }

    #[test]
    fn token_hash_does_not_store_raw_token() {
        let raw = "secret";
        let hash = token_hash(raw);
        assert_ne!(hash, raw);
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn new_owner_password_rollback_removes_empty_state_file() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("oauth.json");
        let change = OwnerPasswordChange {
            path: path.clone(),
            previous_hash: None,
            updated_hash: "new-hash".to_owned(),
        };

        change.apply().unwrap();
        assert!(path.exists());
        change.rollback().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn owner_password_change_preserves_concurrent_oauth_state() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("oauth.json");
        let mut initial = PersistentState {
            owner_password_hash: Some("old-hash".to_owned()),
            ..PersistentState::default()
        };
        initial.clients.insert(
            "client-a".to_owned(),
            RegisteredClient {
                client_id: "client-a".to_owned(),
                client_name: Some("Client A".to_owned()),
                redirect_uris: vec!["https://client.example/callback".to_owned()],
                grant_types: vec!["authorization_code".to_owned(), "refresh_token".to_owned()],
                response_types: vec!["code".to_owned()],
                token_endpoint_auth_method: "none".to_owned(),
                issued_at: now(),
            },
        );
        write_state(&path, &initial).unwrap();

        let change = OwnerPasswordChange {
            path: path.clone(),
            previous_hash: Some("old-hash".to_owned()),
            updated_hash: "new-hash".to_owned(),
        };
        change.apply().unwrap();

        update_state(&path, |state| {
            state.refresh_tokens.insert(
                "refresh-hash".to_owned(),
                RefreshGrant {
                    client_id: "client-a".to_owned(),
                    issuer: "https://mcp.example.com".to_owned(),
                    resource: "https://mcp.example.com/mcp".to_owned(),
                    scope: SCOPE_MCP.to_owned(),
                    expires_at: now() + REFRESH_TOKEN_TTL_SECS,
                },
            );
            Ok(())
        })
        .unwrap();

        change.rollback().unwrap();
        let state = read_state(&path).unwrap();
        assert_eq!(state.owner_password_hash.as_deref(), Some("old-hash"));
        assert!(state.clients.contains_key("client-a"));
        assert!(state.refresh_tokens.contains_key("refresh-hash"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(state_lock_path(&path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn concurrent_state_updates_do_not_lose_clients() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("oauth.json");
        write_state(&path, &PersistentState::default()).unwrap();

        let mut threads = Vec::new();
        for index in 0..8 {
            let path = path.clone();
            threads.push(std::thread::spawn(move || {
                let client_id = format!("client-{index}");
                update_state(&path, |state| {
                    state.clients.insert(
                        client_id.clone(),
                        RegisteredClient {
                            client_id: client_id.clone(),
                            client_name: None,
                            redirect_uris: vec!["https://client.example/callback".to_owned()],
                            grant_types: vec!["authorization_code".to_owned()],
                            response_types: vec!["code".to_owned()],
                            token_endpoint_auth_method: "none".to_owned(),
                            issued_at: now(),
                        },
                    );
                    Ok(())
                })
                .unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let state = read_state(&path).unwrap();
        assert_eq!(state.clients.len(), 8);
    }

    #[test]
    fn owner_password_has_a_minimum_length() {
        assert!(validate_owner_password("short").is_err());
        assert!(validate_owner_password("long-enough-owner-password").is_ok());
    }

    fn test_runtime(password: &str) -> (Runtime, TempDir) {
        let temp = TempDir::new().unwrap();
        let salt = SaltString::encode_b64(&[7u8; 16]).unwrap();
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string();
        let state_path = temp.path().join("oauth.json");
        let persistent = PersistentState {
            owner_password_hash: Some(hash),
            ..PersistentState::default()
        };
        write_state(&state_path, &persistent).unwrap();

        let runtime = Runtime {
            inner: Arc::new(RuntimeState {
                state_path,
                persistent: Mutex::new(persistent),
                pending_clients: Mutex::new(HashMap::new()),
                access_tokens: Mutex::new(HashMap::new()),
                authorization_codes: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                password_checks: Semaphore::new(MAX_CONCURRENT_PASSWORD_CHECKS),
            }),
        };
        (runtime, temp)
    }

    #[tokio::test]
    async fn offline_access_requires_refresh_token_grant() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://mcp.example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let client = runtime
            .insert_dcr_client(RegistrationRequest {
                client_name: Some("No refresh client".to_owned()),
                redirect_uris: vec!["https://client.example/callback".to_owned()],
                grant_types: vec!["authorization_code".to_owned()],
                response_types: vec!["code".to_owned()],
                token_endpoint_auth_method: Some("none".to_owned()),
            })
            .unwrap();

        let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~abc";
        let error = prepare_authorization(
            &server,
            AuthorizeQuery {
                response_type: "code".to_owned(),
                client_id: client.client_id,
                redirect_uri: "https://client.example/callback".to_owned(),
                state: None,
                scope: Some("mcp:access offline_access".to_owned()),
                resource: Some(server.resource().to_owned()),
                code_challenge: pkce_challenge(verifier),
                code_challenge_method: "S256".to_owned(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("refresh_token"));
    }

    #[tokio::test]
    async fn owner_password_changes_are_observed_without_restart() {
        let (runtime, _temp) = test_runtime("first-long-owner-password");
        assert!(
            runtime
                .verify_owner_password("first-long-owner-password")
                .await
                .unwrap()
        );

        let salt = SaltString::encode_b64(&[9u8; 16]).unwrap();
        let new_hash = Argon2::default()
            .hash_password(b"second-long-owner-password", &salt)
            .unwrap()
            .to_string();
        let mut disk = read_state(&runtime.inner.state_path).unwrap();
        disk.owner_password_hash = Some(new_hash);
        write_state(&runtime.inner.state_path, &disk).unwrap();

        assert!(
            !runtime
                .verify_owner_password("first-long-owner-password")
                .await
                .unwrap()
        );
        assert!(
            runtime
                .verify_owner_password("second-long-owner-password")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn failed_password_attempts_do_not_globally_lock_out_owner() {
        let (runtime, _temp) = test_runtime("correct-long-owner-password");
        for _ in 0..5 {
            assert!(
                !runtime
                    .verify_owner_password("wrong-password")
                    .await
                    .unwrap()
            );
        }
        assert!(
            runtime
                .verify_owner_password("correct-long-owner-password")
                .await
                .unwrap()
        );
    }

    #[test]
    fn access_only_client_does_not_receive_refresh_token() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let issuer = "https://mcp.example.com";
        let resource = "https://mcp.example.com/mcp";
        let (access, refresh) = runtime
            .issue_access_and_refresh(
                "https://client.example/oauth.json",
                issuer,
                resource,
                SCOPE_MCP,
                false,
            )
            .unwrap();

        assert!(refresh.is_none());
        assert!(
            runtime
                .validate_access_token(&access, issuer, resource)
                .unwrap()
        );
        assert!(
            read_state(&runtime.inner.state_path)
                .unwrap()
                .refresh_tokens
                .is_empty()
        );
    }

    #[test]
    fn external_refresh_revocation_is_observed_by_running_runtime() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let issuer = "https://mcp.example.com";
        let resource = "https://mcp.example.com/mcp";
        let (_access, refresh) = runtime
            .issue_access_and_refresh(
                "https://client.example/oauth.json",
                issuer,
                resource,
                SCOPE_MCP,
                true,
            )
            .unwrap();
        let refresh = refresh.expect("refresh token");

        let mut disk = read_state(&runtime.inner.state_path).unwrap();
        assert_eq!(disk.refresh_tokens.len(), 1);
        disk.refresh_tokens.clear();
        write_state(&runtime.inner.state_path, &disk).unwrap();

        assert!(
            runtime
                .rotate_refresh(
                    &refresh,
                    "https://client.example/oauth.json",
                    issuer,
                    resource,
                )
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn metadata_router_and_unauthorized_challenge_are_discoverable() {
        use axum::body::to_bytes;
        use tower::ServiceExt;

        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime,
            Url::parse("https://mcp.example.com").unwrap(),
            "/mcp",
        )
        .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let metadata: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(metadata["resource"], "https://mcp.example.com/mcp");
        assert_eq!(
            metadata["authorization_servers"][0],
            "https://mcp.example.com"
        );

        let response = server.unauthorized(false);
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let challenge = response
            .headers()
            .get(WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(challenge.contains("oauth-protected-resource"));
        assert!(challenge.contains("mcp:access"));
    }

    #[tokio::test]
    async fn dcr_pkce_refresh_rotation_and_revocation_flow() {
        let owner_password = "long-enough-owner-password";
        let (runtime, _temp) = test_runtime(owner_password);
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://mcp.example.com").unwrap(),
            "/mcp",
        )
        .unwrap();

        let redirect_uri = "https://client.example/callback".to_owned();
        let client = runtime
            .insert_dcr_client(RegistrationRequest {
                client_name: Some("Test client".to_owned()),
                redirect_uris: vec![redirect_uri.clone()],
                grant_types: vec!["authorization_code".to_owned(), "refresh_token".to_owned()],
                response_types: vec!["code".to_owned()],
                token_endpoint_auth_method: Some("none".to_owned()),
            })
            .unwrap();
        assert!(runtime.inner.persistent.lock().unwrap().clients.is_empty());
        assert_eq!(runtime.inner.pending_clients.lock().unwrap().len(), 1);

        let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~abc";
        let challenge = pkce_challenge(verifier);
        let request_id = prepare_authorization(
            &server,
            AuthorizeQuery {
                response_type: "code".to_owned(),
                client_id: client.client_id.clone(),
                redirect_uri: redirect_uri.clone(),
                state: Some("opaque-state".to_owned()),
                scope: Some("mcp:access".to_owned()),
                resource: Some(server.resource().to_owned()),
                code_challenge: challenge,
                code_challenge_method: "S256".to_owned(),
            },
        )
        .await
        .unwrap();

        let response = authorize_post(
            State(server.clone()),
            Form(AuthorizeForm {
                request_id,
                password: owner_password.to_owned(),
                decision: "allow".to_owned(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(runtime.inner.persistent.lock().unwrap().clients.len(), 1);
        assert!(runtime.inner.pending_clients.lock().unwrap().is_empty());

        let location = response
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap();
        let callback = Url::parse(location).unwrap();
        let params = callback
            .query_pairs()
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(
            params.get("state").map(String::as_str),
            Some("opaque-state")
        );
        assert_eq!(
            params.get("iss").map(String::as_str),
            Some("https://mcp.example.com")
        );
        let code = params.get("code").unwrap().clone();

        let token_json = exchange_authorization_code(
            &server,
            TokenForm {
                grant_type: "authorization_code".to_owned(),
                code: Some(code.clone()),
                redirect_uri: Some(redirect_uri),
                client_id: Some(client.client_id.clone()),
                code_verifier: Some(verifier.to_owned()),
                refresh_token: None,
                resource: Some(server.resource().to_owned()),
            },
        )
        .unwrap();
        let access = token_json["access_token"].as_str().unwrap().to_owned();
        let refresh = token_json["refresh_token"].as_str().unwrap().to_owned();
        assert!(
            runtime
                .validate_access_token(&access, server.issuer(), server.resource())
                .unwrap()
        );

        assert!(
            exchange_authorization_code(
                &server,
                TokenForm {
                    grant_type: "authorization_code".to_owned(),
                    code: Some(code),
                    redirect_uri: Some("https://client.example/callback".to_owned()),
                    client_id: Some(client.client_id.clone()),
                    code_verifier: Some(verifier.to_owned()),
                    refresh_token: None,
                    resource: Some(server.resource().to_owned()),
                },
            )
            .is_err()
        );

        let refreshed = exchange_refresh_token(
            &server,
            TokenForm {
                grant_type: "refresh_token".to_owned(),
                code: None,
                redirect_uri: None,
                client_id: Some(client.client_id.clone()),
                code_verifier: None,
                refresh_token: Some(refresh.clone()),
                resource: Some(server.resource().to_owned()),
            },
        )
        .unwrap();
        let new_access = refreshed["access_token"].as_str().unwrap();
        let new_refresh = refreshed["refresh_token"].as_str().unwrap();
        assert_ne!(new_refresh, refresh);
        assert!(
            runtime
                .validate_access_token(new_access, server.issuer(), server.resource())
                .unwrap()
        );

        assert!(
            exchange_refresh_token(
                &server,
                TokenForm {
                    grant_type: "refresh_token".to_owned(),
                    code: None,
                    redirect_uri: None,
                    client_id: Some(client.client_id),
                    code_verifier: None,
                    refresh_token: Some(refresh),
                    resource: Some(server.resource().to_owned()),
                },
            )
            .is_err()
        );

        runtime.revoke(new_access).unwrap();
        assert!(
            !runtime
                .validate_access_token(new_access, server.issuer(), server.resource())
                .unwrap()
        );
    }
}
