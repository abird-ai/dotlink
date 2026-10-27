use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    env, fs,
    future::Future,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, anyhow, bail};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Form, Json, Router,
    body::Body,
    extract::{
        ConnectInfo, DefaultBodyLimit, Query, State,
        rejection::{FormRejection, JsonRejection, QueryRejection},
    },
    http::{
        HeaderMap, HeaderValue, Request, StatusCode,
        header::{
            AUTHORIZATION, CACHE_CONTROL, CONTENT_SECURITY_POLICY, PRAGMA, REFERRER_POLICY,
            WWW_AUTHENTICATE, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
        },
    },
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use hickory_resolver::TokioResolver;
#[cfg(test)]
use hickory_resolver::{config::ResolverConfig, name_server::TokioConnectionProvider};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use url::{Host, Url};
use zeroize::Zeroizing;

use crate::bounded;
use crate::requester::RequesterId;
use crate::setup::{atomic_write_private, set_private_dir_permissions};

const STATE_VERSION: u32 = 1;
const ACCESS_TOKEN_TTL_SECS: i64 = 15 * 60;
const AUTH_CODE_TTL_SECS: i64 = 60;
const PENDING_AUTH_TTL_SECS: i64 = 5 * 60;
const REFRESH_TOKEN_TTL_SECS: i64 = 90 * 24 * 60 * 60;
const MAX_DCR_CLIENTS: usize = 64;
const MAX_CLIENT_NAME_CHARS: usize = 200;
const PENDING_DCR_TTL_SECS: i64 = 15 * 60;
const MAX_PENDING_DCR_CLIENTS: usize = 256;
const MAX_PENDING_AUTHORIZATIONS: usize = 256;
const MAX_PENDING_AUTHORIZATIONS_PER_CLIENT: usize = 8;
const MAX_PENDING_AUTHORIZATIONS_PER_REQUESTER: usize = 8;
const MAX_PENDING_AUTHORIZATIONS_PER_REQUESTER_CLIENT: usize = 4;
const MAX_PENDING_REGISTRATIONS_PER_REDIRECT: usize = 8;
const MAX_PENDING_REGISTRATIONS_PER_REQUESTER: usize = 8;
const MAX_PENDING_REGISTRATIONS_PER_REQUESTER_REDIRECT: usize = 4;
const MAX_AUTHORIZATION_CODES: usize = 256;
const MAX_ACCESS_TOKENS: usize = 512;
const MAX_ACCESS_TOKENS_PER_CLIENT: usize = 64;
const MAX_REFRESH_GRANTS: usize = 256;
/// One wire-format limit is shared by serialization and bounded reads. Sixteen
/// MiB covers the worst state producible by the public v1 record/count limits,
/// including pre-ceiling releases. Growth beyond it fails before replacement.
const MAX_SERIALIZED_OAUTH_STATE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONCURRENT_PUBLIC_CLIENT_LOOKUPS: usize = 4;
static PUBLIC_CLIENT_LOOKUP_SLOTS: Semaphore =
    Semaphore::const_new(MAX_CONCURRENT_PUBLIC_CLIENT_LOOKUPS);
const MAX_CLIENT_METADATA_BYTES: usize = 64 * 1024;
// Public authorize requests may ask us to fetch a URL. Reject rather than
// queueing unbounded outbound work, and limit sustained anonymous traffic.
const MAX_CONCURRENT_CIMD_FETCHES: usize = 4;
const MAX_CONCURRENT_CIMD_FETCHES_PER_REQUESTER: usize = 2;
const MAX_CIMD_FETCH_STARTS_PER_REQUESTER: usize = 12;
const MAX_CIMD_REQUESTERS: usize = 256;
const CIMD_RATE_WINDOW: Duration = Duration::from_secs(60);
const CIMD_DNS_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CONCURRENT_REVOKES: usize = 2;
const MAX_CONCURRENT_TOKEN_EXCHANGES: usize = 4;
static CIMD_FETCH_SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_CIMD_FETCHES);
static REVOKE_SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_REVOKES);
static TOKEN_EXCHANGE_SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);

fn prune_window(window: &mut VecDeque<Instant>, now: Instant, duration: Duration) {
    while window
        .front()
        .is_some_and(|start| now.saturating_duration_since(*start) >= duration)
    {
        window.pop_front();
    }
}

/// Admission never queues expensive anonymous operations. Permits are held
/// for the whole operation; endpoint-specific limits compose with this layer.
fn admit_public_work(
    slots: &'static Semaphore,
    operation: &'static str,
) -> Result<tokio::sync::SemaphorePermit<'static>> {
    slots
        .try_acquire()
        .map_err(|_| anyhow::Error::new(OAuthCapacityExceeded(operation)))
}

fn admit_revocation() -> Result<tokio::sync::SemaphorePermit<'static>> {
    admit_public_work(&REVOKE_SLOTS, "token revocation")
}

fn admit_token_exchange_with(
    slots: &'static Semaphore,
) -> Result<tokio::sync::SemaphorePermit<'static>> {
    admit_public_work(slots, "token exchange")
}

fn admit_public_client_lookup() -> Result<tokio::sync::SemaphorePermit<'static>> {
    admit_public_work(&PUBLIC_CLIENT_LOOKUP_SLOTS, "registered client lookup")
}

const OWNER_PASSWORD_MIN_LEN: usize = 12;
const MAX_CONCURRENT_PASSWORD_CHECKS: usize = 2;
static PROCESS_PASSWORD_CHECK_SLOTS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_PASSWORD_CHECKS)));

fn process_password_check_slots() -> Arc<Semaphore> {
    Arc::clone(&PROCESS_PASSWORD_CHECK_SLOTS)
}
#[cfg(not(test))]
const STATE_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const STATE_LOCK_TIMEOUT: Duration = Duration::from_millis(200);
const PASSWORD_CHECK_WAIT: Duration = Duration::from_secs(2);
const FAILED_PASSWORD_DELAY: Duration = Duration::from_millis(300);
const MAX_PASSWORD_CHECK_STARTS_PER_REQUESTER: usize = 6;
const MAX_PASSWORD_CHECK_REQUESTERS: usize = 256;
const PASSWORD_CHECK_RATE_WINDOW: Duration = Duration::from_secs(60);
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

#[derive(Clone, Debug)]
struct PendingClient {
    client: RegisteredClient,
    requester: RequesterId,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    owner_password_generations: BTreeMap<String, String>,
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
            owner_password_generations: BTreeMap::new(),
            clients: BTreeMap::new(),
            refresh_tokens: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
struct AccessCapacityPermit {
    client_id: String,
    clients: Arc<Mutex<HashMap<String, usize>>>,
    _global: OwnedSemaphorePermit,
}

impl Drop for AccessCapacityPermit {
    fn drop(&mut self) {
        let mut clients = match self.clients.lock() {
            Ok(clients) => clients,
            Err(error) => {
                self.clients.clear_poison();
                error.into_inner()
            }
        };
        let remove = if let Some(count) = clients.get_mut(&self.client_id) {
            debug_assert!(*count != 0);
            *count = count.saturating_sub(1);
            *count == 0
        } else {
            false
        };
        if remove {
            clients.remove(&self.client_id);
        }
    }
}

#[derive(Debug)]
struct AccessGrant {
    client_id: String,
    issuer: String,
    resource: String,
    scope: String,
    expires_at: i64,
    _capacity: AccessCapacityPermit,
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
struct StoredAuthorizationCode {
    record: AuthorizationCode,
    claimed: bool,
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

/// An in-flight POST retains its slot so unrelated requests cannot consume
/// the capacity needed for a retry. `claimed` prevents parallel redemption.
#[derive(Clone, Debug)]
struct PendingConsent {
    request: PendingAuthorization,
    requester: RequesterId,
    claimed: bool,
}

#[derive(Debug)]
struct OAuthCapacityExceeded(&'static str);
impl std::fmt::Display for OAuthCapacityExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} capacity exhausted; retry later", self.0)
    }
}
impl std::error::Error for OAuthCapacityExceeded {}

#[derive(Default)]
struct CimdRequesterRow {
    in_flight: usize,
    starts: VecDeque<Instant>,
}

#[derive(Default)]
struct CimdRequesterAdmission {
    rows: HashMap<RequesterId, CimdRequesterRow>,
}

impl CimdRequesterAdmission {
    fn prune(&mut self, current: Instant) {
        self.rows.retain(|_, row| {
            prune_window(&mut row.starts, current, CIMD_RATE_WINDOW);
            row.in_flight != 0 || !row.starts.is_empty()
        });
    }
}

#[derive(Default)]
struct PasswordRequesterAdmission {
    rows: HashMap<RequesterId, VecDeque<Instant>>,
}

impl PasswordRequesterAdmission {
    fn prune(&mut self, current: Instant) {
        self.rows.retain(|_, starts| {
            prune_window(starts, current, PASSWORD_CHECK_RATE_WINDOW);
            !starts.is_empty()
        });
    }

    fn check(&self, requesters: &[RequesterId]) -> Result<()> {
        let unseen = requesters
            .iter()
            .filter(|requester| !self.rows.contains_key(requester))
            .count();
        if self.rows.len() + unseen > MAX_PASSWORD_CHECK_REQUESTERS {
            return Err(anyhow::Error::new(OAuthCapacityExceeded(
                "owner password requester admission",
            )));
        }
        if requesters.iter().any(|requester| {
            self.rows
                .get(requester)
                .is_some_and(|starts| starts.len() >= MAX_PASSWORD_CHECK_STARTS_PER_REQUESTER)
        }) {
            return Err(anyhow::Error::new(OAuthCapacityExceeded(
                "owner password checks for this requester",
            )));
        }
        Ok(())
    }
}

/// Owns both admission dimensions for the complete DNS and HTTPS operation.
/// Timeout or request cancellation drops this lease with the DNS future.
struct CimdAdmissionLease {
    requesters: Arc<Mutex<CimdRequesterAdmission>>,
    requester: RequesterId,
    _global: tokio::sync::SemaphorePermit<'static>,
}

impl Drop for CimdAdmissionLease {
    fn drop(&mut self) {
        let mut admission = match self.requesters.lock() {
            Ok(admission) => admission,
            Err(error) => {
                self.requesters.clear_poison();
                error.into_inner()
            }
        };
        if let Some(row) = admission.rows.get_mut(&self.requester) {
            debug_assert!(row.in_flight != 0);
            row.in_flight = row.in_flight.saturating_sub(1);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DcrPromotionTerminal {
    RegistrationExpired,
    ClientCapacity,
}
impl std::fmt::Display for DcrPromotionTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RegistrationExpired => f.write_str("dynamic client registration expired"),
            Self::ClientCapacity => f.write_str("approved OAuth client limit reached"),
        }
    }
}
impl std::error::Error for DcrPromotionTerminal {}

/// An authoritative state transaction proved that a previously approved DCR
/// client was revoked after its authorization code was issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DcrClientRevoked;
impl std::fmt::Display for DcrClientRevoked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("dynamic OAuth client is no longer approved")
    }
}
impl std::error::Error for DcrClientRevoked {}

struct PendingLimits {
    total: usize,
    per_group: usize,
    per_requester: usize,
    per_requester_group: usize,
    label: &'static str,
}

/// Common admission semantics for every anonymous, in-memory OAuth store:
/// reclaim expired entries, but never evict an unexpired request. Every
/// candidate group is bounded globally and within the requester share.
fn admit_pending<T>(
    records: &mut HashMap<String, T>,
    live: impl Fn(&T) -> bool,
    shares_group: impl Fn(&T, &str) -> bool,
    candidate_groups: &[&str],
    requester_of: impl Fn(&T) -> RequesterId,
    requester: RequesterId,
    limits: PendingLimits,
) -> Result<()> {
    records.retain(|_, value| live(value));
    let group_full = candidate_groups.iter().any(|group| {
        records
            .values()
            .filter(|value| shares_group(value, group))
            .count()
            >= limits.per_group
            || records
                .values()
                .filter(|value| requester_of(value) == requester && shares_group(value, group))
                .count()
                >= limits.per_requester_group
    });
    if group_full
        || records
            .values()
            .filter(|value| requester_of(value) == requester)
            .count()
            >= limits.per_requester
        || records.len() >= limits.total
    {
        return Err(anyhow::Error::new(OAuthCapacityExceeded(limits.label)));
    }
    Ok(())
}

struct ConsentClaim {
    runtime: Runtime,
    id: String,
    request: PendingAuthorization,
    requester: RequesterId,
    completed: bool,
}
impl Drop for ConsentClaim {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut pending = match self.runtime.inner.pending.lock() {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(error = ?error, "could not release OAuth authorization claim");
                return;
            }
        };
        if let Some(slot) = pending.get_mut(&self.id) {
            slot.claimed = false;
        }
    }
}

struct RefreshTokenClaim {
    claims: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl Drop for RefreshTokenClaim {
    fn drop(&mut self) {
        let mut claims = match self.claims.lock() {
            Ok(claims) => claims,
            Err(error) => {
                self.claims.clear_poison();
                error.into_inner()
            }
        };
        claims.remove(&self.key);
    }
}

struct AuthorizationCodeClaim {
    runtime: Runtime,
    key: String,
    record: AuthorizationCode,
    completed: bool,
}
impl Drop for AuthorizationCodeClaim {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut codes = match self.runtime.inner.authorization_codes.lock() {
            Ok(codes) => codes,
            Err(error) => {
                self.runtime.inner.authorization_codes.clear_poison();
                error.into_inner()
            }
        };
        if let Some(slot) = codes.get_mut(&self.key) {
            slot.claimed = false;
        }
    }
}

enum AuthorizationCodeClaimResult {
    Claimed(AuthorizationCodeClaim),
    Missing,
    Busy,
}

enum OwnerSelection {
    Profile {
        name: Option<String>,
        /// A deleted persisted profile must not downgrade to its legacy hash.
        required: bool,
    },
    #[cfg(test)]
    Fixed(Option<String>),
}

/// A read-only negative lookup for anonymous token requests. Persistent OAuth
/// state is atomically replaced; a file-version check makes repeated unknown
/// tokens cheap without trusting stale in-memory state from another process.
/// Races fall through to the authoritative transaction; stable unreadable
/// snapshots are cached as unavailable instead of reparsed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StateVersion {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume: u32,
    #[cfg(windows)]
    file_index: u64,
}

fn state_version(path: &Path) -> Result<Option<StateVersion>> {
    // Snapshot the same nonblocking regular-file descriptor's metadata, not a
    // separately traversed pathname. Windows exposes a stable volume/file index
    // for atomic replace; timestamp and length alone can collide under writes.
    let Some(file) = bounded::open_regular_file(path, "OAuth state")? else {
        return Ok(None);
    };
    let meta = file.metadata()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;
    #[cfg(windows)]
    let (volume, file_index) = {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // SAFETY: a live file handle and writable fixed-size output buffer.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let info = unsafe { info.assume_init() };
        (
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    };
    Ok(Some(StateVersion {
        len: meta.len(),
        modified: meta.modified().ok(),
        #[cfg(unix)]
        device: meta.dev(),
        #[cfg(unix)]
        inode: meta.ino(),
        #[cfg(windows)]
        volume,
        #[cfg(windows)]
        file_index,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatePresence {
    Present,
    Absent,
    StableUnavailable,
    Uncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StateIndexAvailability {
    Ready,
    StableUnavailable,
    Uncertain,
}

enum ClientIndexLookup {
    Present(ClientMetadata),
    Absent,
    StableUnavailable,
    Uncertain,
}

#[derive(Default)]
struct StateIndex {
    version: Option<StateVersion>,
    initialized: bool,
    valid: bool,
    refresh_expirations: BTreeMap<String, i64>,
    clients: BTreeMap<String, RegisteredClient>,
}

impl StateIndex {
    /// Cache stable decoding failures, but never treat a racing snapshot as
    /// authoritative.
    fn refresh(&mut self, path: &Path) -> StateIndexAvailability {
        let before = match state_version(path) {
            Ok(version) => version,
            Err(error) => {
                tracing::warn!(error = ?error, "cannot inspect OAuth state index");
                return StateIndexAvailability::Uncertain;
            }
        };
        if self.initialized && before == self.version {
            return if self.valid {
                StateIndexAvailability::Ready
            } else {
                StateIndexAvailability::StableUnavailable
            };
        }
        let state = read_state(path);
        let after = match state_version(path) {
            Ok(version) => version,
            Err(error) => {
                tracing::warn!(error = ?error, "cannot verify OAuth state index");
                return StateIndexAvailability::Uncertain;
            }
        };
        if before != after {
            return StateIndexAvailability::Uncertain;
        }
        self.version = after;
        self.initialized = true;
        match state {
            Ok(state) => {
                self.refresh_expirations = state
                    .refresh_tokens
                    .into_iter()
                    .map(|(hash, grant)| (hash, grant.expires_at))
                    .collect();
                self.clients = state.clients;
                self.valid = true;
                StateIndexAvailability::Ready
            }
            Err(error) => {
                self.refresh_expirations.clear();
                self.clients.clear();
                self.valid = false;
                tracing::warn!(error = ?error, "cannot load OAuth state index");
                StateIndexAvailability::StableUnavailable
            }
        }
    }

    fn might_contain(&mut self, path: &Path, hash: &str) -> StatePresence {
        match self.refresh(path) {
            StateIndexAvailability::Ready
                if self
                    .refresh_expirations
                    .get(hash)
                    .is_some_and(|expires_at| *expires_at > now()) =>
            {
                StatePresence::Present
            }
            StateIndexAvailability::Ready => StatePresence::Absent,
            StateIndexAvailability::StableUnavailable => StatePresence::StableUnavailable,
            StateIndexAvailability::Uncertain => StatePresence::Uncertain,
        }
    }

    fn client(&mut self, path: &Path, client_id: &str) -> ClientIndexLookup {
        match self.refresh(path) {
            StateIndexAvailability::Ready => self
                .clients
                .get(client_id)
                .map(client_metadata)
                .map(ClientIndexLookup::Present)
                .unwrap_or(ClientIndexLookup::Absent),
            StateIndexAvailability::StableUnavailable => ClientIndexLookup::StableUnavailable,
            StateIndexAvailability::Uncertain => ClientIndexLookup::Uncertain,
        }
    }
}

struct RuntimeState {
    state_path: PathBuf,
    owner_selection: OwnerSelection,
    persistent: Mutex<PersistentState>,
    state_index: Mutex<StateIndex>,
    pending_clients: Mutex<HashMap<String, PendingClient>>,
    cimd_resolver: Option<TokioResolver>,
    cimd_requesters: Arc<Mutex<CimdRequesterAdmission>>,
    access_tokens: Mutex<HashMap<String, AccessGrant>>,
    access_token_slots: Arc<Semaphore>,
    access_token_clients: Arc<Mutex<HashMap<String, usize>>>,
    authorization_codes: Mutex<HashMap<String, StoredAuthorizationCode>>,
    refresh_token_claims: Arc<Mutex<HashSet<String>>>,
    pending: Mutex<HashMap<String, PendingConsent>>,
    password_checks: Arc<Semaphore>,
    password_requesters: Mutex<PasswordRequesterAdmission>,
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
    revision: String,
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

#[derive(Debug, Clone, Deserialize)]
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

fn load_system_dns_resolver() -> Result<TokioResolver> {
    #[cfg(any(unix, target_os = "windows"))]
    {
        Ok(TokioResolver::builder_tokio()
            .context("failed to load system DNS configuration")?
            .build())
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        bail!("loading system DNS configuration is unsupported on this platform")
    }
}

#[cfg(test)]
fn test_dns_resolver() -> TokioResolver {
    TokioResolver::builder_with_config(
        ResolverConfig::default(),
        TokioConnectionProvider::default(),
    )
    .build()
}

fn client_metadata(client: &RegisteredClient) -> ClientMetadata {
    ClientMetadata {
        client_id: Some(client.client_id.clone()),
        client_name: client.client_name.as_deref().map(sanitize_client_name),
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
        let selection = crate::setup::profile_owner_selection(profile)?;
        let persistent = read_state(&path)?;
        let hash = active_owner_hash(&persistent, selection.revision())
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
        let cimd_resolver = match load_system_dns_resolver() {
            Ok(resolver) => Some(resolver),
            Err(_) => {
                tracing::warn!("system DNS unavailable; CIMD registration is disabled");
                None
            }
        };

        Ok(Self {
            inner: Arc::new(RuntimeState {
                state_path: path,
                owner_selection: OwnerSelection::Profile {
                    name: profile.map(str::to_owned),
                    required: !matches!(selection, crate::setup::ProfileOwnerSelection::Absent),
                },
                persistent: Mutex::new(persistent),
                state_index: Mutex::new(StateIndex::default()),
                pending_clients: Mutex::new(HashMap::new()),
                cimd_resolver,
                cimd_requesters: Arc::new(Mutex::new(CimdRequesterAdmission::default())),
                access_tokens: Mutex::new(HashMap::new()),
                access_token_slots: Arc::new(Semaphore::new(MAX_ACCESS_TOKENS)),
                access_token_clients: Arc::new(Mutex::new(HashMap::new())),
                authorization_codes: Mutex::new(HashMap::new()),
                refresh_token_claims: Arc::new(Mutex::new(HashSet::new())),
                pending: Mutex::new(HashMap::new()),
                password_checks: process_password_check_slots(),
                password_requesters: Mutex::new(PasswordRequesterAdmission::default()),
            }),
        })
    }

    fn admit_cimd_fetch(&self, requester: RequesterId) -> Result<CimdAdmissionLease> {
        self.admit_cimd_fetch_with_slots(requester, Instant::now(), &CIMD_FETCH_SLOTS)
    }

    fn admit_cimd_fetch_with_slots(
        &self,
        requester: RequesterId,
        current: Instant,
        slots: &'static Semaphore,
    ) -> Result<CimdAdmissionLease> {
        let global = admit_public_work(slots, "client metadata fetch")?;
        let mut admission = self
            .inner
            .cimd_requesters
            .lock()
            .map_err(|_| anyhow!("OAuth client metadata admission lock is unavailable"))?;
        admission.prune(current);
        if !admission.rows.contains_key(&requester) && admission.rows.len() >= MAX_CIMD_REQUESTERS {
            return Err(anyhow::Error::new(OAuthCapacityExceeded(
                "client metadata requester admission",
            )));
        }

        let row = admission.rows.entry(requester).or_default();
        if row.in_flight >= MAX_CONCURRENT_CIMD_FETCHES_PER_REQUESTER
            || row.starts.len() >= MAX_CIMD_FETCH_STARTS_PER_REQUESTER
        {
            return Err(anyhow::Error::new(OAuthCapacityExceeded(
                "client metadata fetches for this requester",
            )));
        }
        row.in_flight += 1;
        row.starts.push_back(current);
        drop(admission);

        Ok(CimdAdmissionLease {
            requesters: self.inner.cimd_requesters.clone(),
            requester,
            _global: global,
        })
    }

    fn preflight_password_checks(
        &self,
        consent_requester: RequesterId,
        post_requester: RequesterId,
    ) -> Result<()> {
        let current = Instant::now();
        let requesters = [consent_requester, post_requester];
        let requesters = &requesters[..if consent_requester == post_requester {
            1
        } else {
            2
        }];
        let mut admission = self
            .inner
            .password_requesters
            .lock()
            .map_err(|_| anyhow!("OAuth password requester admission lock is unavailable"))?;
        admission.prune(current);
        admission.check(requesters)
    }

    fn reserve_password_checks(
        &self,
        consent_requester: RequesterId,
        post_requester: RequesterId,
    ) -> Result<()> {
        self.reserve_password_checks_at(consent_requester, post_requester, Instant::now())
    }

    fn reserve_password_checks_at(
        &self,
        consent_requester: RequesterId,
        post_requester: RequesterId,
        current: Instant,
    ) -> Result<()> {
        let requesters = [consent_requester, post_requester];
        let requesters = &requesters[..if consent_requester == post_requester {
            1
        } else {
            2
        }];
        let mut admission = self
            .inner
            .password_requesters
            .lock()
            .map_err(|_| anyhow!("OAuth password requester admission lock is unavailable"))?;
        admission.prune(current);
        admission.check(requesters)?;
        for requester in requesters {
            admission
                .rows
                .entry(*requester)
                .or_default()
                .push_back(current);
        }
        Ok(())
    }

    #[cfg(test)]
    fn reserve_password_check_at(&self, requester: RequesterId, current: Instant) -> Result<()> {
        self.reserve_password_checks_at(requester, requester, current)
    }

    fn reserve_access_token_capacity(&self, client_id: &str) -> Result<AccessCapacityPermit> {
        self.access_tokens()
            .retain(|_, grant| grant.expires_at > now());
        let global = self
            .inner
            .access_token_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::Error::new(OAuthCapacityExceeded("access tokens")))?;
        let mut clients = match self.inner.access_token_clients.lock() {
            Ok(clients) => clients,
            Err(error) => {
                tracing::warn!("recovering poisoned OAuth per-client access-token admission");
                self.inner.access_token_clients.clear_poison();
                error.into_inner()
            }
        };
        let count = clients.entry(client_id.to_owned()).or_default();
        if *count >= MAX_ACCESS_TOKENS_PER_CLIENT {
            return Err(anyhow::Error::new(OAuthCapacityExceeded(
                "access tokens for this client",
            )));
        }
        *count += 1;
        drop(clients);
        Ok(AccessCapacityPermit {
            client_id: client_id.to_owned(),
            clients: self.inner.access_token_clients.clone(),
            _global: global,
        })
    }

    fn claim_refresh_token(&self, raw: &str) -> Result<RefreshTokenClaim> {
        let key = token_hash(raw);
        let mut claims = match self.inner.refresh_token_claims.lock() {
            Ok(claims) => claims,
            Err(error) => {
                tracing::warn!("recovering poisoned OAuth refresh-token claims");
                self.inner.refresh_token_claims.clear_poison();
                error.into_inner()
            }
        };
        if !claims.insert(key.clone()) {
            return Err(anyhow::Error::new(OAuthCapacityExceeded(
                "refresh token exchange already in progress",
            )));
        }
        drop(claims);
        Ok(RefreshTokenClaim {
            claims: self.inner.refresh_token_claims.clone(),
            key,
        })
    }

    fn might_contain_refresh(&self, raw: &str) -> StatePresence {
        let hash = token_hash(raw);
        crate::blocking::run(|| match self.inner.state_index.try_lock() {
            Ok(mut index) => index.might_contain(&self.inner.state_path, &hash),
            Err(std::sync::TryLockError::WouldBlock) => StatePresence::Uncertain,
            Err(std::sync::TryLockError::Poisoned(error)) => {
                self.inner.state_index.clear_poison();
                let mut index = error.into_inner();
                *index = StateIndex::default();
                index.might_contain(&self.inner.state_path, &hash)
            }
        })
    }

    fn has_authorization_code(&self, raw: &str) -> bool {
        self.inner
            .authorization_codes
            .try_lock()
            .map(|codes| codes.contains_key(&token_hash(raw)))
            .unwrap_or(true)
    }

    fn access_tokens(&self) -> std::sync::MutexGuard<'_, HashMap<String, AccessGrant>> {
        match self.inner.access_tokens.lock() {
            Ok(tokens) => tokens,
            Err(error) => {
                tracing::warn!("recovering poisoned OAuth access-token state");
                self.inner.access_tokens.clear_poison();
                error.into_inner()
            }
        }
    }

    fn has_access_token(&self, raw: &str) -> StatePresence {
        let hash = token_hash(raw);
        match self.inner.access_tokens.try_lock() {
            Ok(tokens) => {
                if tokens.contains_key(&hash) {
                    StatePresence::Present
                } else {
                    StatePresence::Absent
                }
            }
            Err(std::sync::TryLockError::WouldBlock) => StatePresence::Uncertain,
            Err(std::sync::TryLockError::Poisoned(error)) => {
                tracing::warn!("recovering poisoned OAuth access-token state");
                self.inner.access_tokens.clear_poison();
                if error.into_inner().contains_key(&hash) {
                    StatePresence::Present
                } else {
                    StatePresence::Absent
                }
            }
        }
    }

    fn update_persistent<T>(
        &self,
        update: impl FnOnce(&mut PersistentState) -> Result<T>,
    ) -> Result<T> {
        // Keep the memory lock and whole disk transaction in one synchronous
        // section; contention must not stall unrelated Tokio requests.
        crate::blocking::run(|| {
            let mut memory = self
                .inner
                .persistent
                .lock()
                .map_err(|_| anyhow!("OAuth state lock poisoned"))?;
            let (latest, value) = update_state(&self.inner.state_path, update)?;
            *memory = latest;
            Ok(value)
        })
    }

    fn reload_owner_hash(&self, revision: Option<&str>) -> Result<String> {
        crate::blocking::run(|| {
            let mut memory = self
                .inner
                .persistent
                .lock()
                .map_err(|_| anyhow!("OAuth state lock poisoned"))?;
            let latest = read_state(&self.inner.state_path)?;
            let hash = active_owner_hash(&latest, revision)
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("OAuth owner password is not configured"))?;
            *memory = latest;
            Ok(hash)
        })
    }

    /// Resolve the *current* committed profile selector. A live HTTP server
    /// observes password rotation without restarting, and profile deletion
    /// cannot silently re-enable an old legacy owner password.
    fn selected_owner_revision(&self) -> Result<Option<String>> {
        match &self.inner.owner_selection {
            OwnerSelection::Profile { name, required } => {
                let selected = crate::setup::profile_owner_selection(name.as_deref())?;
                match selected {
                    crate::setup::ProfileOwnerSelection::Revision(revision) => Ok(Some(revision)),
                    crate::setup::ProfileOwnerSelection::Legacy => Ok(None),
                    crate::setup::ProfileOwnerSelection::Absent if *required => {
                        bail!("OAuth profile was removed; owner authorization is disabled")
                    }
                    crate::setup::ProfileOwnerSelection::Absent => Ok(None),
                }
            }
            #[cfg(test)]
            OwnerSelection::Fixed(revision) => Ok(revision.clone()),
        }
    }

    async fn verify_owner_password(
        &self,
        consent_requester: RequesterId,
        post_requester: RequesterId,
        password: &str,
    ) -> Result<bool> {
        // Avoid waiting behind Argon2 work when either identity is already over
        // quota. The second check below is authoritative after the wait.
        self.preflight_password_checks(consent_requester, post_requester)?;
        let permit = tokio::time::timeout(
            PASSWORD_CHECK_WAIT,
            self.inner.password_checks.clone().acquire_owned(),
        )
        .await
        .map_err(|_| anyhow::Error::new(OAuthCapacityExceeded("owner password verification")))?
        .map_err(|_| anyhow!("OAuth password verifier is unavailable"))?;
        self.reserve_password_checks(consent_requester, post_requester)?;
        if password.len() > 1024 {
            drop(permit);
            tokio::time::sleep(FAILED_PASSWORD_DELAY).await;
            return Ok(false);
        }

        let revision = crate::blocking::run(|| self.selected_owner_revision())?;
        let hash = self.reload_owner_hash(revision.as_deref())?;
        let password = Zeroizing::new(password.as_bytes().to_vec());
        // The OS worker, not this cancellable HTTP request, owns the permit.
        // An aborted request cannot start more Argon2 computations than allowed.
        let valid = crate::blocking::spawn_with_permit(permit, move || -> Result<bool> {
            let parsed = PasswordHash::new(&hash)
                .map_err(|error| anyhow!("stored OAuth owner hash is invalid: {error}"))?;
            Ok(Argon2::default()
                .verify_password(password.as_slice(), &parsed)
                .is_ok())
        })
        .await
        .context("OAuth password verifier task failed")??;
        // A password rotated during the Argon2 computation must not become a
        // successful authorization with the superseded generation.
        let valid = valid && crate::blocking::run(|| self.selected_owner_revision())? == revision;
        if !valid {
            tokio::time::sleep(FAILED_PASSWORD_DELAY).await;
        }
        Ok(valid)
    }

    fn resolve_registered_client(&self, client_id: &str) -> Result<Option<ClientMetadata>> {
        let indexed = crate::blocking::run(|| match self.inner.state_index.try_lock() {
            Ok(mut index) => index.client(&self.inner.state_path, client_id),
            Err(std::sync::TryLockError::WouldBlock) => ClientIndexLookup::Uncertain,
            Err(std::sync::TryLockError::Poisoned(error)) => {
                self.inner.state_index.clear_poison();
                let mut index = error.into_inner();
                *index = StateIndex::default();
                index.client(&self.inner.state_path, client_id)
            }
        });
        let registered = match indexed {
            ClientIndexLookup::Present(client) => Some(client),
            ClientIndexLookup::Absent => None,
            ClientIndexLookup::StableUnavailable => bail!("OAuth state is unavailable"),
            ClientIndexLookup::Uncertain => crate::blocking::run(|| {
                read_state(&self.inner.state_path)
                    .map(|state| state.clients.get(client_id).map(client_metadata))
            })?,
        };
        if registered.is_some() {
            return Ok(registered);
        }

        let mut pending = self
            .inner
            .pending_clients
            .lock()
            .map_err(|_| anyhow!("OAuth pending-client lock poisoned"))?;
        let current = now();
        pending.retain(|_, pending| pending.client.issued_at + PENDING_DCR_TTL_SECS > current);
        Ok(pending
            .get(client_id)
            .map(|pending| client_metadata(&pending.client)))
    }

    fn insert_dcr_client(
        &self,
        requester: RequesterId,
        metadata: RegistrationRequest,
    ) -> Result<RegisteredClient> {
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
            client_name: metadata.client_name.map(|name| sanitize_client_name(&name)),
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
        let current = now();
        let redirects = client
            .redirect_uris
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        admit_pending(
            &mut pending,
            |entry| entry.client.issued_at + PENDING_DCR_TTL_SECS > current,
            |entry, redirect| {
                entry
                    .client
                    .redirect_uris
                    .iter()
                    .any(|existing| existing == redirect)
            },
            &redirects,
            |entry| entry.requester,
            requester,
            PendingLimits {
                total: MAX_PENDING_DCR_CLIENTS,
                per_group: MAX_PENDING_REGISTRATIONS_PER_REDIRECT,
                per_requester: MAX_PENDING_REGISTRATIONS_PER_REQUESTER,
                per_requester_group: MAX_PENDING_REGISTRATIONS_PER_REQUESTER_REDIRECT,
                label: "pending client registrations",
            },
        )?;
        pending.insert(
            client.client_id.clone(),
            PendingClient {
                client: client.clone(),
                requester,
            },
        );
        Ok(client)
    }

    /// Shared bounded insertion for initial and retryable owner consent.
    /// A transient internal failure must not burn the pending request after
    /// it was atomically claimed by a POST.
    fn store_pending_authorization(
        &self,
        requester: RequesterId,
        id: String,
        request: PendingAuthorization,
    ) -> Result<()> {
        if request.expires_at <= now() {
            bail!("authorization request expired");
        }
        let mut pending = self
            .inner
            .pending
            .lock()
            .map_err(|_| anyhow!("OAuth pending-request lock poisoned"))?;
        admit_pending(
            &mut pending,
            |entry| entry.request.expires_at > now() || entry.claimed,
            |entry, client_id| entry.request.client_id == client_id,
            &[request.client_id.as_str()],
            |entry| entry.requester,
            requester,
            PendingLimits {
                total: MAX_PENDING_AUTHORIZATIONS,
                per_group: MAX_PENDING_AUTHORIZATIONS_PER_CLIENT,
                per_requester: MAX_PENDING_AUTHORIZATIONS_PER_REQUESTER,
                per_requester_group: MAX_PENDING_AUTHORIZATIONS_PER_REQUESTER_CLIENT,
                label: "pending authorizations",
            },
        )?;
        pending.insert(
            id,
            PendingConsent {
                request,
                requester,
                claimed: false,
            },
        );
        Ok(())
    }

    fn claim_authorization(&self, id: &str) -> Result<Option<ConsentClaim>> {
        let mut pending = self
            .inner
            .pending
            .lock()
            .map_err(|_| anyhow!("OAuth pending-request lock poisoned"))?;
        let Some(slot) = pending.get_mut(id) else {
            return Ok(None);
        };
        if slot.claimed {
            return Ok(None);
        }
        slot.claimed = true;
        Ok(Some(ConsentClaim {
            runtime: self.clone(),
            id: id.to_owned(),
            request: slot.request.clone(),
            requester: slot.requester,
            completed: false,
        }))
    }

    fn finish_authorization(&self, claim: &mut ConsentClaim) -> Result<()> {
        let removed = self
            .inner
            .pending
            .lock()
            .map_err(|_| anyhow!("OAuth pending-request lock poisoned"))?
            .remove(&claim.id);
        if removed.is_none() {
            bail!("claimed OAuth authorization request is missing");
        }
        claim.completed = true;
        Ok(())
    }

    /// Replacing an existing claimed slot requires no new admission; the live
    /// request retains its capacity and moves to a new one-time form nonce.
    fn retry_authorization(&self, claim: &mut ConsentClaim, new_id: String) -> Result<()> {
        let mut pending = self
            .inner
            .pending
            .lock()
            .map_err(|_| anyhow!("OAuth pending-request lock poisoned"))?;
        if pending.contains_key(&new_id) {
            bail!("duplicate authorization request ID");
        }
        let slot = pending
            .remove(&claim.id)
            .ok_or_else(|| anyhow!("authorization request missing"))?;
        pending.insert(
            new_id,
            PendingConsent {
                request: slot.request,
                requester: claim.requester,
                claimed: false,
            },
        );
        claim.completed = true;
        Ok(())
    }

    fn store_authorization_code(&self, raw: &str, record: AuthorizationCode) -> Result<()> {
        let mut codes = self
            .inner
            .authorization_codes
            .lock()
            .map_err(|_| anyhow!("OAuth authorization-code lock poisoned"))?;
        codes.retain(|_, slot| slot.claimed || slot.record.expires_at > now());
        if codes.len() >= MAX_AUTHORIZATION_CODES {
            return Err(anyhow::Error::new(OAuthCapacityExceeded(
                "authorization codes",
            )));
        }
        codes.insert(
            token_hash(raw),
            StoredAuthorizationCode {
                record,
                claimed: false,
            },
        );
        Ok(())
    }

    fn claim_authorization_code(&self, raw: &str) -> Result<AuthorizationCodeClaimResult> {
        let key = token_hash(raw);
        let mut codes = self.inner.authorization_codes.lock().map_err(|_| {
            anyhow::Error::new(TokenIssuanceFailure(anyhow!(
                "OAuth authorization-code lock poisoned"
            )))
        })?;
        let Some(slot) = codes.get_mut(&key) else {
            return Ok(AuthorizationCodeClaimResult::Missing);
        };
        if slot.claimed {
            return Ok(AuthorizationCodeClaimResult::Busy);
        }
        slot.claimed = true;
        Ok(AuthorizationCodeClaimResult::Claimed(
            AuthorizationCodeClaim {
                runtime: self.clone(),
                key,
                record: slot.record.clone(),
                completed: false,
            },
        ))
    }

    fn finish_authorization_code(&self, claim: &mut AuthorizationCodeClaim) {
        let mut codes = match self.inner.authorization_codes.lock() {
            Ok(codes) => codes,
            Err(error) => {
                self.inner.authorization_codes.clear_poison();
                error.into_inner()
            }
        };
        codes.remove(&claim.key);
        claim.completed = true;
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
            let current = now();
            pending
                .retain(|_, existing| existing.client.issued_at + PENDING_DCR_TTL_SECS > current);
            pending.get(client_id).map(|pending| pending.client.clone())
        };

        let promoted = self.update_persistent(|state| {
            if state.clients.contains_key(client_id) {
                return Ok(false);
            }
            let client = pending_client
                .clone()
                .ok_or_else(|| anyhow::Error::new(DcrPromotionTerminal::RegistrationExpired))?;
            if state.clients.len() >= MAX_DCR_CLIENTS {
                return Err(anyhow::Error::new(DcrPromotionTerminal::ClientCapacity));
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
        // Do every potentially failing token/lock step before the durable
        // refresh-grant update. After that commit, publishing the already
        // prepared access token is infallible.
        let access = random_token()?;
        let access_capacity = self.reserve_access_token_capacity(client_id)?;
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
                    return Err(anyhow::Error::new(DcrClientRevoked));
                }
                ensure_refresh_grant_capacity(state)?;
                state.refresh_tokens.insert(token_hash(&refresh), grant);
                Ok(())
            })?;
            Some(refresh)
        } else {
            self.update_persistent(|state| {
                if client_id.starts_with("dcr_") && !state.clients.contains_key(client_id) {
                    return Err(anyhow::Error::new(DcrClientRevoked));
                }
                Ok(())
            })?;
            None
        };

        self.publish_access_token(
            &access,
            AccessGrant {
                client_id: client_id.to_owned(),
                issuer: issuer.to_owned(),
                resource: resource.to_owned(),
                scope: scope.to_owned(),
                expires_at: now() + ACCESS_TOKEN_TTL_SECS,
                _capacity: access_capacity,
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
        // A committed refresh rotation must never be followed by a fallible
        // token generator or poisoned access-token mutex.
        let access = random_token()?;
        let access_capacity = self.reserve_access_token_capacity(client_id)?;
        let rotated = self.update_persistent(|state| {
            state
                .refresh_tokens
                .retain(|_, grant| grant.expires_at > current);
            let Some(grant) = state.refresh_tokens.get(&hash) else {
                return Ok(None);
            };
            // A malformed or incorrectly bound request never consumes an
            // otherwise-valid grant. Validation and rotation share this lock.
            if grant.expires_at <= current
                || grant.client_id != client_id
                || grant.issuer != issuer
                || grant.resource != resource
            {
                return Ok(None);
            }
            let grant = grant.clone();
            let new_refresh = random_token()?;
            let new_grant = RefreshGrant {
                expires_at: current + REFRESH_TOKEN_TTL_SECS,
                ..grant.clone()
            };
            state.refresh_tokens.remove(&hash);
            state
                .refresh_tokens
                .insert(token_hash(&new_refresh), new_grant);
            Ok(Some((grant, new_refresh)))
        })?;

        let Some((grant, new_refresh)) = rotated else {
            return Ok(None);
        };

        self.publish_access_token(
            &access,
            AccessGrant {
                client_id: grant.client_id,
                issuer: issuer.to_owned(),
                resource: resource.to_owned(),
                scope: grant.scope.clone(),
                expires_at: current + ACCESS_TOKEN_TTL_SECS,
                _capacity: access_capacity,
            },
        );

        Ok(Some((access, new_refresh, grant.scope)))
    }

    /// Publishing cannot fail after a durable refresh-grant mutation.
    fn publish_access_token(&self, raw: &str, grant: AccessGrant) {
        self.access_tokens().insert(token_hash(raw), grant);
    }

    fn validate_access_token(&self, raw: &str, issuer: &str, resource: &str) -> Result<bool> {
        let hash = token_hash(raw);
        let current = now();
        let mut access = self.access_tokens();
        access.retain(|_, grant| grant.expires_at > current);
        let Some(grant) = access.get(&hash) else {
            return Ok(false);
        };
        Ok(grant.issuer == issuer
            && grant.resource == resource
            && scope_contains(&grant.scope, SCOPE_MCP)
            && !grant.client_id.is_empty())
    }

    #[cfg(test)]
    fn revoke(&self, raw: &str) -> Result<()> {
        self.revoke_with_presence(raw, self.might_contain_refresh(raw))
    }

    fn revoke_with_presence(&self, raw: &str, refresh: StatePresence) -> Result<()> {
        let hash = token_hash(raw);
        if self.access_tokens().remove(&hash).is_some() {
            return Ok(());
        }
        match refresh {
            StatePresence::Absent => Ok(()),
            StatePresence::StableUnavailable => bail!("OAuth state is unavailable"),
            StatePresence::Present | StatePresence::Uncertain => self.update_persistent(|state| {
                state.refresh_tokens.remove(&hash);
                Ok(())
            }),
        }
    }
}

/// Internal state failures are never interpolated into public OAuth responses.
#[derive(Debug)]
struct OAuthStateUnavailable(anyhow::Error);
impl std::fmt::Display for OAuthStateUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OAuth state unavailable")
    }
}
impl std::error::Error for OAuthStateUnavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

#[derive(Debug)]
struct OAuthInternalFailure(anyhow::Error);
impl std::fmt::Display for OAuthInternalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OAuth authorization is temporarily unavailable")
    }
}
impl std::error::Error for OAuthInternalFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn oauth_internal(error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(OAuthInternalFailure(error))
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
            .route(
                "/oauth/authorize",
                get(authorize_get_route).post(authorize_post_route),
            )
            .route("/oauth/token", post(token_route))
            .route("/oauth/register", post(register_route))
            .route("/oauth/revoke", post(revoke_route))
            .layer(DefaultBodyLimit::max(MAX_CLIENT_METADATA_BYTES))
            .layer(middleware::from_fn(require_oauth_requester))
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

    async fn resolve_client(
        &self,
        requester: RequesterId,
        client_id: &str,
    ) -> Result<ClientMetadata> {
        // Locally issued DCR IDs have a different namespace from HTTPS CIMD
        // URLs. Never deserialize private state for an arbitrary public URL.
        if client_id.starts_with("dcr_") {
            let _admission = admit_public_client_lookup()?;
            return self
                .runtime
                .resolve_registered_client(client_id)
                .map_err(|error| anyhow::Error::new(OAuthStateUnavailable(error)))?
                .ok_or_else(|| anyhow!("OAuth client is unknown or expired"));
        }
        fetch_cimd(&self.runtime, requester, client_id).await
    }
}

pub fn state_dir() -> Result<PathBuf> {
    Ok(crate::xdg::state_home()?.join("abird/dotlink"))
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

fn active_owner_hash<'a>(state: &'a PersistentState, revision: Option<&str>) -> Option<&'a str> {
    match revision {
        Some(revision) => state
            .owner_password_generations
            .get(revision)
            .map(String::as_str),
        None => state.owner_password_hash.as_deref(),
    }
}

pub fn owner_configured(profile: Option<&str>, revision: Option<&str>) -> Result<bool> {
    let state = read_state(&state_path(profile)?)?;
    Ok(active_owner_hash(&state, revision).is_some_and(|value| !value.trim().is_empty()))
}

pub fn prepare_owner_password(
    profile: Option<&str>,
    password: &str,
) -> Result<OwnerPasswordChange> {
    validate_owner_password(password)?;
    let path = state_path(profile)?;
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
        revision: uuid::Uuid::new_v4().simple().to_string(),
        updated_hash,
    })
}

impl OwnerPasswordChange {
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// Stage an immutable password hash in the existing OAuth state. Neither
    /// existing authorizations nor the legacy owner credential change until
    /// the profile atomically selects this revision.
    pub fn apply(&self) -> Result<()> {
        let revision = self.revision.clone();
        let updated = self.updated_hash.clone();
        update_state(&self.path, move |state| {
            if state.owner_password_generations.contains_key(&revision) {
                bail!("duplicate OAuth owner generation");
            }
            state.owner_password_generations.insert(revision, updated);
            Ok(())
        })?;
        Ok(())
    }

    pub fn rollback(&self) -> Result<()> {
        let revision = self.revision.clone();
        let updated = self.updated_hash.clone();
        let (state, ()) = update_state(&self.path, move |state| {
            if state.owner_password_generations.get(&revision) == Some(&updated) {
                state.owner_password_generations.remove(&revision);
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

fn retain_owner_password_generations(
    state: &mut PersistentState,
    selected: Option<&str>,
    previous: Option<&str>,
) {
    state.owner_password_generations.retain(|revision, _| {
        Some(revision.as_str()) == selected || Some(revision.as_str()) == previous
    });
    if selected.is_some() && previous.is_some() {
        state.owner_password_hash = None;
    }
}

/// Retire superseded credential hashes only after the profile's selector
/// rename is durable. At most the active and immediately prior generation
/// remain; refresh grants and registered clients are untouched.
pub(crate) fn prune_owner_password_generations(
    profile: Option<&str>,
    selected: Option<&str>,
    previous: Option<&str>,
) -> Result<()> {
    let path = state_path(profile)?;
    // Avoid allocating a new OAuth state file for profiles that never used it.
    if !path.exists() {
        return Ok(());
    }
    update_state(&path, |state| {
        retain_owner_password_generations(state, selected, previous);
        Ok(())
    })?;
    Ok(())
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
    let selection = crate::setup::profile_owner_selection(profile)?;
    let selected = selection.revision();
    let owner = active_owner_hash(&state, selected).is_some();
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

fn client_terminal_line(client: &RegisteredClient) -> String {
    let client_name = client
        .client_name
        .as_deref()
        .map(sanitize_client_name)
        .unwrap_or_else(|| "[unnamed]".to_owned());
    format!(
        "{}\t{}\t{} redirect URI(s)",
        client.client_id,
        client_name,
        client.redirect_uris.len()
    )
}

pub fn clients(profile: Option<&str>) -> Result<Vec<String>> {
    let state = read_state(&state_path(profile)?)?;
    Ok(state.clients.values().map(client_terminal_line).collect())
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

async fn require_oauth_requester(request: Request<Body>, next: Next) -> Response {
    if request
        .extensions()
        .get::<ConnectInfo<RequesterId>>()
        .is_none()
    {
        return no_store_json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "error": "temporarily_unavailable",
                "error_description": "OAuth requester admission is unavailable"
            }),
        );
    }
    next.run(request).await
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

async fn authorize_get_route(
    State(server): State<Server>,
    ConnectInfo(requester): ConnectInfo<RequesterId>,
    query: Result<Query<AuthorizeQuery>, QueryRejection>,
) -> Response {
    let query = match query {
        Ok(query) => query,
        Err(_) => {
            return oauth_error_page(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Authorization request is invalid",
            );
        }
    };
    authorize_get(State(server), ConnectInfo(requester), query).await
}

async fn authorize_get(
    State(server): State<Server>,
    ConnectInfo(requester): ConnectInfo<RequesterId>,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    match prepare_authorization(&server, requester, query).await {
        Ok(pending_id) => {
            let request = server
                .runtime
                .inner
                .pending
                .lock()
                .ok()
                .and_then(|pending| pending.get(&pending_id).map(|entry| entry.request.clone()));
            match request {
                Some(request) => authorization_page(&pending_id, &request, None),
                None => oauth_error_page(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "Authorization request could not be stored",
                ),
            }
        }
        Err(error) => {
            let unavailable = error.is::<OAuthStateUnavailable>();
            let internal = error.is::<OAuthInternalFailure>();
            let capacity = error.is::<OAuthCapacityExceeded>();
            if unavailable || internal {
                tracing::warn!(error = ?error, "OAuth authorization unavailable");
            } else {
                // Invalid and capacity-limited public traffic is expected and
                // must not provide an unauthenticated warning-log amplifier.
                tracing::debug!(%error, "OAuth authorization request rejected");
            }
            oauth_error_page(
                if capacity {
                    StatusCode::TOO_MANY_REQUESTS
                } else if unavailable {
                    StatusCode::SERVICE_UNAVAILABLE
                } else if internal {
                    StatusCode::INTERNAL_SERVER_ERROR
                } else {
                    StatusCode::BAD_REQUEST
                },
                if capacity {
                    "temporarily_unavailable"
                } else if unavailable || internal {
                    "server_error"
                } else {
                    "invalid_request"
                },
                if capacity {
                    "Too many pending requests; retry after an existing request completes"
                } else if unavailable || internal {
                    "OAuth authorization is unavailable; retry shortly"
                } else {
                    "Invalid client, redirect URI, resource or scope"
                },
            )
        }
    }
}

async fn prepare_authorization(
    server: &Server,
    requester: RequesterId,
    query: AuthorizeQuery,
) -> Result<String> {
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

    let metadata = server.resolve_client(requester, &query.client_id).await?;
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

    let request_id = random_token().map_err(oauth_internal)?;
    let pending = PendingAuthorization {
        client_id: query.client_id,
        client_name: metadata.client_name.map(|name| sanitize_client_name(&name)),
        issuer: server.issuer.clone(),
        redirect_uri: query.redirect_uri,
        resource: server.resource.clone(),
        scope,
        state: query.state,
        code_challenge: query.code_challenge,
        issue_refresh,
        expires_at: now() + PENDING_AUTH_TTL_SECS,
    };

    match server
        .runtime
        .store_pending_authorization(requester, request_id.clone(), pending)
    {
        Ok(()) => Ok(request_id),
        Err(error) if error.is::<OAuthCapacityExceeded>() => Err(error),
        Err(error) => Err(oauth_internal(error)),
    }
}

fn retryable_authorization_failure() -> Response {
    oauth_error_page(
        StatusCode::SERVICE_UNAVAILABLE,
        "server_error",
        "Authorization temporarily unavailable; return to the form and retry",
    )
}

fn authorization_state_failure(error: anyhow::Error, operation: &'static str) -> Response {
    tracing::warn!(error = ?error, operation, "OAuth authorization state unavailable");
    oauth_error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "Authorization state is unavailable",
    )
}

async fn authorize_post_route(
    State(server): State<Server>,
    ConnectInfo(requester): ConnectInfo<RequesterId>,
    form: Result<Form<AuthorizeForm>, FormRejection>,
) -> Response {
    let form = match form {
        Ok(form) => form,
        Err(_) => {
            return oauth_error_page(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Authorization form is invalid",
            );
        }
    };
    authorize_post(State(server), ConnectInfo(requester), form).await
}

async fn authorize_post(
    State(server): State<Server>,
    ConnectInfo(post_requester): ConnectInfo<RequesterId>,
    Form(form): Form<AuthorizeForm>,
) -> Response {
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

    let mut claim = match server.runtime.claim_authorization(&form.request_id) {
        Ok(Some(claim)) => claim,
        Ok(None) => {
            return oauth_error_page(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Authorization request expired, invalid, or already being processed",
            );
        }
        Err(error) => {
            return authorization_state_failure(error, "claim request");
        }
    };
    let request = claim.request.clone();
    if request.expires_at <= now()
        || request.issuer != server.issuer
        || request.resource != server.resource
    {
        if let Err(error) = server.runtime.finish_authorization(&mut claim) {
            return authorization_state_failure(error, "discard invalid request");
        }
        return oauth_error_page(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Authorization request expired",
        );
    }

    if form.decision != "allow" {
        if let Err(error) = server.runtime.finish_authorization(&mut claim) {
            return authorization_state_failure(error, "finish denied request");
        }
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
        .verify_owner_password(claim.requester, post_requester, password.as_str())
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            let retry_id = match random_token() {
                Ok(id) => id,
                Err(error) => {
                    tracing::warn!(error = ?error, "could not generate authorization retry ID");
                    return retryable_authorization_failure();
                }
            };
            if let Err(error) = server
                .runtime
                .retry_authorization(&mut claim, retry_id.clone())
            {
                tracing::warn!(error = ?error, "could not retry OAuth owner password");
                return retryable_authorization_failure();
            }
            return authorization_page(&retry_id, &request, Some("Invalid owner password"));
        }
        Err(error) if error.is::<OAuthCapacityExceeded>() => {
            return oauth_error_page(
                StatusCode::TOO_MANY_REQUESTS,
                "temporarily_unavailable",
                "Too many authorization attempts; retry shortly",
            );
        }
        Err(error) => {
            tracing::warn!(error = ?error, "OAuth owner verification unavailable");
            return retryable_authorization_failure();
        }
    }

    if let Err(error) = server.runtime.promote_dcr_client(&request.client_id) {
        tracing::warn!(error = ?error, "OAuth client approval failed");
        if let Some(terminal) = error.downcast_ref::<DcrPromotionTerminal>() {
            if let Err(finish_error) = server.runtime.finish_authorization(&mut claim) {
                return authorization_state_failure(
                    finish_error,
                    "consume terminal client request",
                );
            }
            let description = match terminal {
                DcrPromotionTerminal::RegistrationExpired => {
                    "Client registration expired; register the client again and restart authorization"
                }
                DcrPromotionTerminal::ClientCapacity => {
                    "Approved client limit reached; revoke an unused client, then restart authorization"
                }
            };
            return oauth_error_page(StatusCode::BAD_REQUEST, "invalid_client", description);
        }
        return retryable_authorization_failure();
    }

    let code = match random_token() {
        Ok(code) => code,
        Err(error) => {
            tracing::warn!(error = ?error, "OAuth code generation unavailable");
            return retryable_authorization_failure();
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
    if let Err(error) = server.runtime.store_authorization_code(&code, record) {
        tracing::warn!(error = ?error, "OAuth authorization-code storage unavailable");
        return retryable_authorization_failure();
    }

    if let Err(error) = server.runtime.finish_authorization(&mut claim) {
        return authorization_state_failure(error, "finish approved request");
    }
    redirect_authorization_success(
        &request.redirect_uri,
        &code,
        request.state.as_deref(),
        &server.issuer,
    )
}

/// Distinguish an invalid grant from a temporary failure to persist an
/// otherwise-valid token exchange. Clients may retry a 503 using the same code.
#[derive(Debug)]
struct TokenIssuanceFailure(anyhow::Error);

impl std::fmt::Display for TokenIssuanceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "OAuth token issuance is temporarily unavailable")
    }
}

impl std::error::Error for TokenIssuanceFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

async fn token_route(
    State(server): State<Server>,
    form: Result<Form<TokenForm>, FormRejection>,
) -> Response {
    let form = match form {
        Ok(form) => form,
        Err(_) => return token_error("invalid_request", "Token request is invalid"),
    };
    token(State(server), form).await
}

async fn token(State(server): State<Server>, Form(form): Form<TokenForm>) -> Response {
    token_with_slots(server, form, &TOKEN_EXCHANGE_SLOTS).await
}

async fn token_with_slots(server: Server, form: TokenForm, slots: &'static Semaphore) -> Response {
    if ensure_max_len("grant_type", &form.grant_type, 64).is_err() {
        return token_error("invalid_request", "grant_type is too long");
    }

    let candidate = match form.grant_type.as_str() {
        "authorization_code" => form.code.as_deref(),
        "refresh_token" => form.refresh_token.as_deref(),
        _ => None,
    };
    if matches!(
        form.grant_type.as_str(),
        "authorization_code" | "refresh_token"
    ) && !candidate.is_some_and(looks_like_issued_token)
    {
        return token_error("invalid_grant", "token is invalid or missing");
    }
    // Stable misses are authoritative, races fall through, and stable corrupt
    // snapshots fail without repeating the same bounded read under a lock.
    if let Some(raw) = candidate {
        match form.grant_type.as_str() {
            "authorization_code" if !server.runtime.has_authorization_code(raw) => {
                return token_error("invalid_grant", "token is invalid or expired");
            }
            "refresh_token" => match server.runtime.might_contain_refresh(raw) {
                StatePresence::Absent => {
                    return token_error("invalid_grant", "token is invalid or expired");
                }
                StatePresence::StableUnavailable => {
                    return token_state_unavailable();
                }
                StatePresence::Present | StatePresence::Uncertain => {}
            },
            _ => {}
        }
    }
    let result = match form.grant_type.as_str() {
        "authorization_code" => exchange_authorization_code_with_slots(&server, form, slots).await,
        "refresh_token" => exchange_refresh_token_with_slots(&server, form, slots),
        _ => {
            return token_error(
                "unsupported_grant_type",
                "grant_type must be authorization_code or refresh_token",
            );
        }
    };

    match result {
        Ok(value) => no_store_json(StatusCode::OK, value),
        Err(error) if error.is::<OAuthCapacityExceeded>() => no_store_json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "error": "temporarily_unavailable",
                "error_description": "OAuth token exchange is busy; retry later"
            }),
        ),
        Err(error) if error.is::<TokenIssuanceFailure>() => {
            tracing::warn!(error = ?error, "OAuth token issuance unavailable");
            no_store_json(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({
                    "error": "server_error",
                    "error_description": "OAuth token storage is unavailable; retry the same grant"
                }),
            )
        }
        Err(error) => {
            tracing::debug!(%error, "OAuth token grant rejected");
            token_error("invalid_grant", "Token grant is invalid or expired")
        }
    }
}

async fn exchange_authorization_code_with_slots(
    server: &Server,
    form: TokenForm,
    slots: &'static Semaphore,
) -> Result<Value> {
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

    // Claim before global admission and release the in-memory mutex before
    // durable I/O. Duplicates shed immediately without occupying tasks or
    // process-wide exchange capacity; dropping a failed claim permits retry.
    let mut claim = match server.runtime.claim_authorization_code(&code)? {
        AuthorizationCodeClaimResult::Claimed(claim) => claim,
        AuthorizationCodeClaimResult::Missing => {
            bail!("authorization code is invalid or already used")
        }
        AuthorizationCodeClaimResult::Busy => {
            return Err(anyhow::Error::new(OAuthCapacityExceeded(
                "authorization code exchange",
            )));
        }
    };
    let record = &claim.record;
    if record.expires_at <= now()
        || record.client_id != client_id
        || record.redirect_uri != redirect_uri
        || record.issuer != server.issuer
        || record.resource != server.resource
        || pkce_challenge(&verifier) != record.code_challenge
    {
        bail!("authorization code validation failed");
    }

    // If process capacity is busy, dropping the claim makes it retryable.
    let _admission = admit_token_exchange_with(slots)?;
    let issuance = server.runtime.issue_access_and_refresh(
        &client_id,
        &server.issuer,
        &server.resource,
        &record.scope,
        record.issue_refresh,
    );
    let (access, refresh) = match issuance {
        Ok(tokens) => tokens,
        Err(error) if error.is::<DcrClientRevoked>() => {
            // Only this typed error follows a successful authoritative read
            // proving the approved client absent. The code is terminally
            // invalid; storage/read failures continue through the retry path.
            server.runtime.finish_authorization_code(&mut claim);
            return Err(error);
        }
        Err(error) => return Err(anyhow::Error::new(TokenIssuanceFailure(error))),
    };
    let scope = record.scope.clone();
    server.runtime.finish_authorization_code(&mut claim);
    Ok(token_response(access, refresh, scope))
}

fn exchange_refresh_token_with_slots(
    server: &Server,
    form: TokenForm,
    slots: &'static Semaphore,
) -> Result<Value> {
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

    // The process-local claim sheds duplicate work without replacing the
    // durable transaction that remains authoritative across runtimes/processes.
    let _claim = server.runtime.claim_refresh_token(&refresh)?;
    let _admission = admit_token_exchange_with(slots)?;
    let Some((access, new_refresh, scope)) = server
        .runtime
        .rotate_refresh(&refresh, &client_id, &server.issuer, &server.resource)
        .map_err(|error| anyhow::Error::new(TokenIssuanceFailure(error)))?
    else {
        bail!("refresh token is invalid or expired");
    };

    Ok(token_response(access, Some(new_refresh), scope))
}

async fn register_route(
    State(server): State<Server>,
    ConnectInfo(requester): ConnectInfo<RequesterId>,
    request: Result<Json<RegistrationRequest>, JsonRejection>,
) -> Response {
    let request = match request {
        Ok(request) => request,
        Err(_) => {
            return no_store_json(
                StatusCode::BAD_REQUEST,
                json!({
                    "error": "invalid_client_metadata",
                    "error_description": "Client registration request is invalid"
                }),
            );
        }
    };
    register(State(server), ConnectInfo(requester), request).await
}

async fn register(
    State(server): State<Server>,
    ConnectInfo(requester): ConnectInfo<RequesterId>,
    Json(request): Json<RegistrationRequest>,
) -> Response {
    match server.runtime.insert_dcr_client(requester, request) {
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
        Err(error) => {
            let full = error.is::<OAuthCapacityExceeded>();
            tracing::debug!(%error, "OAuth client registration rejected");
            no_store_json(
                if full {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::BAD_REQUEST
                },
                json!({
                    "error": if full { "temporarily_unavailable" } else { "invalid_client_metadata" },
                    "error_description": if full { "Too many pending client registrations; retry later" } else { "Unsupported or invalid client metadata" }
                }),
            )
        }
    }
}

async fn revoke_route(
    State(server): State<Server>,
    form: Result<Form<RevokeForm>, FormRejection>,
) -> Response {
    let form = match form {
        Ok(form) => form,
        Err(_) => {
            return no_store_json(
                StatusCode::BAD_REQUEST,
                json!({
                    "error": "invalid_request",
                    "error_description": "Revocation request is invalid"
                }),
            );
        }
    };
    revoke(State(server), form).await
}

async fn revoke(State(server): State<Server>, Form(form): Form<RevokeForm>) -> Response {
    if !looks_like_issued_token(&form.token) {
        return StatusCode::OK.into_response();
    }
    let access = server.runtime.has_access_token(&form.token);
    let refresh = server.runtime.might_contain_refresh(&form.token);
    match (access, refresh) {
        (StatePresence::Absent, StatePresence::Absent) => {
            return StatusCode::OK.into_response();
        }
        (StatePresence::Absent, StatePresence::StableUnavailable) => {
            return revocation_state_unavailable();
        }
        _ => {}
    }
    let _permit = match admit_revocation() {
        Ok(permit) => permit,
        Err(_) => {
            return no_store_json(
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error": "temporarily_unavailable", "error_description": "OAuth revocation is busy; retry later"}),
            );
        }
    };

    match server.runtime.revoke_with_presence(&form.token, refresh) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => {
            tracing::warn!(error = ?error, "OAuth token revocation unavailable");
            revocation_state_unavailable()
        }
    }
}

fn token_state_unavailable() -> Response {
    no_store_json(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({
            "error": "server_error",
            "error_description": "OAuth token storage is unavailable; retry the same grant"
        }),
    )
}

fn revocation_state_unavailable() -> Response {
    no_store_json(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({
            "error": "temporarily_unavailable",
            "error_description": "OAuth revocation storage is unavailable; retry later"
        }),
    )
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
    let client = request
        .client_name
        .as_deref()
        .map(sanitize_client_name)
        .unwrap_or_else(|| request.client_id.clone());
    let error = error
        .map(|message| format!(r#"<p class="error">{}</p>"#, html_escape(message)))
        .unwrap_or_default();
    let body = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>abird dotlink authorization</title><style>body{{font:16px system-ui,sans-serif;max-width:560px;margin:10vh auto;padding:0 20px;color:#171717}}h1{{font-size:24px}}code{{word-break:break-all}}input,button{{font:inherit;padding:10px;margin-top:8px}}input{{width:100%;box-sizing:border-box}}button{{margin-right:8px}}.muted{{color:#666}}.error{{color:#b00020}}</style></head><body><h1>abird dotlink</h1><p><strong>{}</strong> is requesting access to this dotlink MCP server.</p><p class="muted">Client ID: <code>{}</code></p><p class="muted">Redirect: <code>{}</code></p><p class="muted">Resource: <code>{}</code></p><p class="muted">Scope: <code>{}</code></p>{}<form method="post" action="/oauth/authorize"><input type="hidden" name="request_id" value="{}"><label>Owner password<input type="password" name="password" autocomplete="current-password" required autofocus></label><div><button type="submit" name="decision" value="allow">Allow</button><button type="submit" name="decision" value="deny" formnovalidate>Deny</button></div></form></body></html>"#,
        html_escape(&client),
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

async fn run_cimd_resolver<L, T, F>(lease: L, timeout: Duration, resolver: F) -> Result<(L, T)>
where
    F: Future<Output = Result<T>>,
{
    // Poll DNS in the request task. Timeout or request cancellation drops both
    // the lookup future and its admission lease instead of detaching either.
    let result = tokio::time::timeout(timeout, resolver)
        .await
        .context("CIMD DNS lookup timed out")??;
    Ok((lease, result))
}

async fn fetch_cimd(
    runtime: &Runtime,
    requester: RequesterId,
    client_id: &str,
) -> Result<ClientMetadata> {
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
    // Reject literal private/reserved addresses before consuming network or
    // requester admission. DNS-resolved addresses are checked again below.
    if let Some(literal) = match url.host() {
        Some(Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
        _ => None,
    } && !is_public_ip(literal)
    {
        bail!("CIMD host is a non-public address");
    }
    let resolver = runtime
        .inner
        .cimd_resolver
        .as_ref()
        .ok_or_else(|| anyhow!("system DNS is unavailable"))?;
    let admission = runtime.admit_cimd_fetch(requester)?;
    let port = url.port_or_known_default().unwrap_or(443);
    let (admission, resolved) = run_cimd_resolver(admission, CIMD_DNS_TIMEOUT, async {
        resolver
            .lookup_ip(host.clone())
            .await
            .context("failed to resolve CIMD host")
    })
    .await?;
    let mut addresses = resolved
        .iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect::<Vec<_>>();
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() || addresses.iter().any(|addr| !is_public_ip(addr.ip())) {
        bail!("CIMD host resolves to a non-public address");
    }

    let client = reqwest::Client::builder()
        // Never allow proxy environment variables to bypass DNS pinning.
        .no_proxy()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(5))
        .https_only(true)
        .resolve_to_addrs(&host, &addresses)
        .user_agent(format!("dotlink-oauth/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build CIMD HTTP client")?;
    let response = client
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

    let body = bounded::reqwest_response(response, MAX_CLIENT_METADATA_BYTES, "CIMD metadata")
        .await
        .context("failed to read CIMD metadata")?;

    let mut metadata: ClientMetadata =
        serde_json::from_slice(&body).context("CIMD metadata is invalid JSON")?;
    if metadata.client_id.as_deref() != Some(client_id) {
        bail!("CIMD document client_id does not match requested client_id");
    }
    validate_client_metadata(client_id, &metadata)?;
    metadata.client_name = metadata.client_name.as_deref().map(sanitize_client_name);
    drop(admission);
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

fn looks_like_issued_token(raw: &str) -> bool {
    raw.len() == 43
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn token_hash(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}

fn now() -> i64 {
    Utc::now().timestamp()
}

fn ensure_refresh_grant_capacity(state: &mut PersistentState) -> Result<()> {
    let current = now();
    state
        .refresh_tokens
        .retain(|_, grant| grant.expires_at > current);
    if state.refresh_tokens.len() >= MAX_REFRESH_GRANTS {
        return Err(anyhow::Error::new(OAuthCapacityExceeded("refresh grants")));
    }
    Ok(())
}

fn state_lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    PathBuf::from(value)
}

fn open_state_lock(path: &Path) -> Result<fs::File> {
    crate::private_state::acquire(&state_lock_path(path), STATE_LOCK_TIMEOUT)
}

fn update_state<T>(
    path: &Path,
    update: impl FnOnce(&mut PersistentState) -> Result<T>,
) -> Result<(PersistentState, T)> {
    update_state_within(path, STATE_LOCK_TIMEOUT, update)
}

/// Same read-modify-write with an explicit lock budget. The configured budget is
/// short so a contended caller gives up quickly; a test that deliberately
/// serializes many writers needs to wait out their real, host-dependent work
/// instead of failing on a deadline that only suits one machine.
fn update_state_within<T>(
    path: &Path,
    timeout: Duration,
    update: impl FnOnce(&mut PersistentState) -> Result<T>,
) -> Result<(PersistentState, T)> {
    let _lock = crate::private_state::acquire(&state_lock_path(path), timeout)?;
    let mut state = read_state(path)?;
    let before = state.clone();
    let value = update(&mut state)?;
    if state != before {
        write_state(path, &state)?;
    }
    Ok((state, value))
}

fn read_state(path: &Path) -> Result<PersistentState> {
    let text =
        match bounded::text_file(path, MAX_SERIALIZED_OAUTH_STATE_BYTES as u64, "OAuth state")? {
            Some(text) => text,
            None => return Ok(PersistentState::default()),
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

fn serialize_state(state: &PersistentState) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_SERIALIZED_OAUTH_STATE_BYTES {
        bail!(
            "OAuth state would exceed the {} byte storage limit",
            MAX_SERIALIZED_OAUTH_STATE_BYTES
        );
    }
    Ok(bytes)
}

fn write_state(path: &Path, state: &PersistentState) -> Result<()> {
    // Serialize and enforce the reader's exact byte limit before creating
    // directories or atomically replacing the last known-readable state.
    let bytes = serialize_state(state)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        set_private_dir_permissions(parent)?;
    }
    atomic_write_private(path, &bytes, true)
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
    let [a, b, c, d] = ip.octets();
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
        // Deprecated 6to4 relay anycast is not globally reachable. IANA
        // separately marks the 192.88.99.2/32 6a44 service as globally
        // reachable, so preserve that longest-prefix exception.
        || (a == 192 && b == 88 && c == 99 && d != 2)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
    {
        return false;
    }
    true
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    // Ordinary allocated global unicast is within 2000::/3. This excludes
    // mapped/compatible IPv4, NAT64/SIIT, discard-only, ULA, link-local,
    // site-local and multicast space without trying to decode embedded IPv4.
    if segments[0] & 0xe000 != 0x2000 {
        return false;
    }

    // IANA special-purpose space inside 2000::/3: protocol assignments
    // (including benchmarking and ORCHID), documentation, 6to4, AS112 direct
    // delegation, retired 6bone, and the additional documentation prefix.
    if (segments[0] == 0x2001 && (segments[1] <= 0x01ff || segments[1] == 0x0db8))
        || segments[0] == 0x2002
        || (segments[0] == 0x2620 && segments[1] == 0x004f && segments[2] == 0x8000)
        || segments[0] == 0x3ffe
        || (segments[0] == 0x3fff && segments[1] & 0xf000 == 0)
    {
        return false;
    }
    true
}

fn is_unsafe_client_name_char(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

fn sanitize_client_name(value: &str) -> String {
    value
        .chars()
        .take(MAX_CLIENT_NAME_CHARS)
        .map(|character| {
            if is_unsafe_client_name_char(character) {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
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

    fn test_requester(last_octet: u8) -> RequesterId {
        RequesterId::Ipv4(Ipv4Addr::new(192, 0, 2, last_octet))
    }

    fn fill_access_token_capacity(runtime: &Runtime, issuer: &str, resource: &str) -> String {
        let first = "capacity-access-0".to_owned();
        for index in 0..MAX_ACCESS_TOKENS {
            let raw = format!("capacity-access-{index}");
            let client_id = format!("capacity-client-{}", index / MAX_ACCESS_TOKENS_PER_CLIENT);
            let capacity = runtime.reserve_access_token_capacity(&client_id).unwrap();
            runtime.publish_access_token(
                &raw,
                AccessGrant {
                    client_id,
                    issuer: issuer.into(),
                    resource: resource.into(),
                    scope: SCOPE_MCP.into(),
                    expires_at: now() + ACCESS_TOKEN_TTL_SECS,
                    _capacity: capacity,
                },
            );
        }
        first
    }

    #[test]
    fn revocation_ignores_unknown_token_shapes_without_state_work() {
        let token = random_token().unwrap();
        assert!(looks_like_issued_token(&token));
        assert!(!looks_like_issued_token("not-an-issued-token"));
        assert!(!looks_like_issued_token(&"a".repeat(44)));
        assert!(!looks_like_issued_token(&"=".repeat(43)));
    }

    #[test]
    fn public_work_is_concurrent_bounded_without_sustained_anonymous_quota() {
        static SLOTS: Semaphore = Semaphore::const_new(1);
        let held = admit_public_work(&SLOTS, "test").unwrap();
        assert!(admit_public_work(&SLOTS, "test").is_err());
        drop(held);
        for _ in 0..128 {
            let permit = admit_public_work(&SLOTS, "test").unwrap();
            drop(permit);
        }
    }

    #[tokio::test]
    async fn timed_out_cimd_resolver_immediately_releases_full_admission() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_CIMD_FETCHES);
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let requester = test_requester(1);
        let lease = runtime
            .admit_cimd_fetch_with_slots(requester, Instant::now(), &SLOTS)
            .unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let lookup = tokio::spawn(async move {
            run_cimd_resolver(lease, Duration::from_millis(10), async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Ok::<_, anyhow::Error>(())
            })
            .await
        });
        started_rx.await.unwrap();
        let error = lookup.await.unwrap().err().unwrap();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert_eq!(
            runtime
                .inner
                .cimd_requesters
                .lock()
                .unwrap()
                .rows
                .get(&requester)
                .unwrap()
                .in_flight,
            0
        );
        assert_eq!(SLOTS.available_permits(), MAX_CONCURRENT_CIMD_FETCHES);
        assert!(
            release_tx.send(()).is_err(),
            "timed-out lookup was detached"
        );
    }

    #[tokio::test]
    async fn cancelled_cimd_resolver_immediately_releases_full_admission() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_CIMD_FETCHES);
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let requester = test_requester(1);
        let lease = runtime
            .admit_cimd_fetch_with_slots(requester, Instant::now(), &SLOTS)
            .unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let lookup = tokio::spawn(async move {
            run_cimd_resolver(lease, Duration::from_secs(60), async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Ok::<_, anyhow::Error>(())
            })
            .await
        });
        started_rx.await.unwrap();
        lookup.abort();
        assert!(matches!(lookup.await, Err(error) if error.is_cancelled()));
        assert_eq!(
            runtime
                .inner
                .cimd_requesters
                .lock()
                .unwrap()
                .rows
                .get(&requester)
                .unwrap()
                .in_flight,
            0
        );
        assert_eq!(SLOTS.available_permits(), MAX_CONCURRENT_CIMD_FETCHES);
        assert!(
            release_tx.send(()).is_err(),
            "cancelled lookup was detached"
        );
    }

    #[tokio::test]
    async fn successful_cimd_resolver_returns_admission_to_https_phase() {
        static SLOTS: Semaphore = Semaphore::const_new(1);
        let permit = SLOTS.try_acquire().unwrap();
        let (permit, value) = run_cimd_resolver(permit, Duration::from_secs(1), async {
            Ok::<_, anyhow::Error>(42)
        })
        .await
        .unwrap();
        assert_eq!(value, 42);
        assert_eq!(SLOTS.available_permits(), 0);
        drop(permit);
        assert_eq!(SLOTS.available_permits(), 1);
    }

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
                    owner_selection: OwnerSelection::Fixed(None),
                    state_index: Mutex::new(StateIndex::default()),
                    persistent: Mutex::new(PersistentState {
                        owner_password_hash: Some(
                            "$argon2id$v=19$m=19456,t=2,p=1$YWJjZGVmZ2hpamtsbW5vcA$RmFrZUhhc2hUaGF0SXNOZXZlclVzZWQ".to_owned(),
                        ),
                        ..PersistentState::default()
                    }),
                    pending_clients: Mutex::new(HashMap::new()),
                    cimd_resolver: Some(test_dns_resolver()),
                    cimd_requesters: Arc::new(Mutex::new(CimdRequesterAdmission::default())),
                    access_tokens: Mutex::new(HashMap::new()),
                    access_token_slots: Arc::new(Semaphore::new(MAX_ACCESS_TOKENS)),
                    access_token_clients: Arc::new(Mutex::new(HashMap::new())),
                    authorization_codes: Mutex::new(HashMap::new()),
                    refresh_token_claims: Arc::new(Mutex::new(HashSet::new())),
                    pending: Mutex::new(HashMap::new()),
                    password_checks: Arc::new(Semaphore::new(MAX_CONCURRENT_PASSWORD_CHECKS)),
                    password_requesters: Mutex::new(PasswordRequesterAdmission::default()),
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
    fn oauth_disk_state_lock_has_bounded_contention() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oauth.json");
        let held = open_state_lock(&path).unwrap();
        let started = Instant::now();
        let busy = open_state_lock(&path).unwrap_err().to_string();
        assert!(busy.contains("busy"), "{busy}");
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(held);
        assert!(open_state_lock(&path).is_ok());
    }

    #[tokio::test]
    async fn pending_consents_never_evict_another_live_consent() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let make_request = |client_id: &str| PendingAuthorization {
            client_id: client_id.to_owned(),
            client_name: None,
            issuer: "https://server.example".into(),
            redirect_uri: "https://client.example/cb".into(),
            resource: "https://server.example/mcp".into(),
            scope: SCOPE_MCP.into(),
            state: None,
            code_challenge: "a".repeat(43),
            issue_refresh: false,
            expires_at: now() + PENDING_AUTH_TTL_SECS,
        };
        let first_id = random_token().unwrap();
        runtime
            .store_pending_authorization(
                test_requester(1),
                first_id.clone(),
                make_request("client-A"),
            )
            .unwrap();
        for n in 1..MAX_PENDING_AUTHORIZATIONS_PER_REQUESTER_CLIENT {
            runtime
                .store_pending_authorization(
                    test_requester(1),
                    format!("client-A-{n}"),
                    make_request("client-A"),
                )
                .unwrap();
        }
        let error = runtime
            .store_pending_authorization(
                test_requester(1),
                "requester-group-overload".into(),
                make_request("client-A"),
            )
            .unwrap_err();
        assert!(error.is::<OAuthCapacityExceeded>());
        for n in 0..MAX_PENDING_AUTHORIZATIONS_PER_REQUESTER_CLIENT {
            runtime
                .store_pending_authorization(
                    test_requester(2),
                    format!("client-A-other-{n}"),
                    make_request("client-A"),
                )
                .unwrap();
        }
        let error = runtime
            .store_pending_authorization(
                test_requester(3),
                "global-group-overload".into(),
                make_request("client-A"),
            )
            .unwrap_err();
        assert!(error.is::<OAuthCapacityExceeded>());
        assert!(
            runtime
                .inner
                .pending
                .lock()
                .unwrap()
                .contains_key(&first_id)
        );
        let mut claim = runtime.claim_authorization(&first_id).unwrap().unwrap();
        assert!(runtime.claim_authorization(&first_id).unwrap().is_none());
        // An aborted request releases the claim, without freeing its slot.
        drop(claim);
        claim = runtime.claim_authorization(&first_id).unwrap().unwrap();
        assert_eq!(claim.request.client_id, "client-A");
        assert_eq!(claim.requester, test_requester(1));
        // Retrying switches the one-time request ID without using more capacity.
        let replacement = "new-one-time-id".to_owned();
        runtime
            .retry_authorization(&mut claim, replacement.clone())
            .unwrap();
        assert_eq!(
            runtime
                .inner
                .pending
                .lock()
                .unwrap()
                .get(&replacement)
                .unwrap()
                .requester,
            test_requester(1)
        );
        drop(claim);
        let mut claim = runtime.claim_authorization(&replacement).unwrap().unwrap();
        runtime.finish_authorization(&mut claim).unwrap();
        assert!(runtime.claim_authorization(&replacement).unwrap().is_none());
        runtime
            .store_pending_authorization(
                test_requester(1),
                "new-open-slot".into(),
                make_request("client-A"),
            )
            .unwrap();

        // Saturating the aggregate pool with distinct clients also refuses
        // new work, rather than selecting a live victim for eviction.
        let mut records = HashMap::new();
        for n in 0..MAX_PENDING_AUTHORIZATIONS {
            let client_id = format!("client-{n}");
            admit_pending(
                &mut records,
                |c: &PendingAuthorization| c.expires_at > now(),
                |c, group| c.client_id == group,
                &[client_id.as_str()],
                |_| test_requester(1),
                test_requester(1),
                PendingLimits {
                    total: MAX_PENDING_AUTHORIZATIONS,
                    per_group: 1,
                    per_requester: MAX_PENDING_AUTHORIZATIONS + 1,
                    per_requester_group: 1,
                    label: "consents",
                },
            )
            .unwrap();
            records.insert(format!("id-{n}"), make_request(&client_id));
        }
        assert!(
            admit_pending(
                &mut records,
                |c| c.expires_at > now(),
                |c, group| c.client_id == group,
                &["new-client"],
                |_| test_requester(1),
                test_requester(1),
                PendingLimits {
                    total: MAX_PENDING_AUTHORIZATIONS,
                    per_group: 1,
                    per_requester: MAX_PENDING_AUTHORIZATIONS + 1,
                    per_requester_group: 1,
                    label: "consents",
                }
            )
            .is_err()
        );
        assert!(records.contains_key("id-0"));
    }

    #[test]
    fn pending_consent_requester_cap_cannot_fill_the_total_store() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let make_request = |client_id: &str| PendingAuthorization {
            client_id: client_id.to_owned(),
            client_name: None,
            issuer: "https://server.example".into(),
            redirect_uri: format!("https://{client_id}.example/cb"),
            resource: "https://server.example/mcp".into(),
            scope: SCOPE_MCP.into(),
            state: None,
            code_challenge: "a".repeat(43),
            issue_refresh: false,
            expires_at: now() + PENDING_AUTH_TTL_SECS,
        };
        let first_id = "first-consent".to_owned();
        for index in 0..MAX_PENDING_AUTHORIZATIONS_PER_REQUESTER {
            let id = if index == 0 {
                first_id.clone()
            } else {
                format!("consent-{index}")
            };
            runtime
                .store_pending_authorization(
                    test_requester(1),
                    id,
                    make_request(&format!("client-{index}")),
                )
                .unwrap();
        }
        let error = runtime
            .store_pending_authorization(
                test_requester(1),
                "requester-overload".into(),
                make_request("new-client"),
            )
            .unwrap_err();
        assert!(error.is::<OAuthCapacityExceeded>());
        assert_eq!(
            runtime.inner.pending.lock().unwrap().len(),
            MAX_PENDING_AUTHORIZATIONS_PER_REQUESTER
        );
        assert!(
            runtime
                .inner
                .pending
                .lock()
                .unwrap()
                .contains_key(&first_id)
        );

        runtime
            .store_pending_authorization(
                test_requester(2),
                "other-requester".into(),
                make_request("other-client"),
            )
            .unwrap();
        let pending = runtime.inner.pending.lock().unwrap();
        assert!(pending.contains_key(&first_id));
        assert!(pending.contains_key("other-requester"));
    }

    #[test]
    fn pending_dcr_registrations_do_not_evict_live_clients() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let register = |requester, redirect: &str| {
            runtime.insert_dcr_client(
                requester,
                RegistrationRequest {
                    client_name: Some("test".into()),
                    redirect_uris: vec![redirect.to_owned()],
                    grant_types: vec!["authorization_code".into(), "refresh_token".into()],
                    response_types: vec!["code".into()],
                    token_endpoint_auth_method: Some("none".into()),
                },
            )
        };
        let first = register(test_requester(1), "https://client-0.example/cb").unwrap();
        for index in 1..MAX_PENDING_REGISTRATIONS_PER_REQUESTER {
            register(
                test_requester(1),
                &format!("https://client-{index}.example/cb"),
            )
            .unwrap();
        }
        let error = register(test_requester(1), "https://new-client.example/cb").unwrap_err();
        assert!(error.is::<OAuthCapacityExceeded>());
        assert!(
            runtime
                .inner
                .pending_clients
                .lock()
                .unwrap()
                .contains_key(&first.client_id)
        );
        assert_eq!(
            runtime.inner.pending_clients.lock().unwrap().len(),
            MAX_PENDING_REGISTRATIONS_PER_REQUESTER
        );
        // A different requester has an independent fair share of the pool.
        assert!(register(test_requester(2), "https://different.example/cb").is_ok());
    }

    #[test]
    fn pending_dcr_share_applies_to_every_redirect_uri() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let register = |requester, redirect_uris: Vec<String>| {
            runtime.insert_dcr_client(
                requester,
                RegistrationRequest {
                    client_name: Some("test".into()),
                    redirect_uris,
                    grant_types: vec!["authorization_code".into()],
                    response_types: vec!["code".into()],
                    token_endpoint_auth_method: Some("none".into()),
                },
            )
        };
        let shared = "https://shared.example/callback";
        let mut first = None;
        for index in 0..MAX_PENDING_REGISTRATIONS_PER_REQUESTER_REDIRECT {
            let client = register(
                test_requester(1),
                vec![
                    format!("https://client-a-{index}.example/callback"),
                    shared.to_owned(),
                ],
            )
            .unwrap();
            first.get_or_insert(client);
        }
        let error = register(
            test_requester(1),
            vec![
                "https://client-a-new.example/callback".into(),
                shared.into(),
            ],
        )
        .unwrap_err();
        assert!(error.is::<OAuthCapacityExceeded>());

        for index in 0..MAX_PENDING_REGISTRATIONS_PER_REQUESTER_REDIRECT {
            register(
                test_requester(2),
                vec![
                    format!("https://client-b-{index}.example/callback"),
                    shared.to_owned(),
                ],
            )
            .unwrap();
        }
        let error = register(
            test_requester(3),
            vec!["https://client-c.example/callback".into(), shared.into()],
        )
        .unwrap_err();
        assert!(error.is::<OAuthCapacityExceeded>());
        assert!(
            runtime
                .inner
                .pending_clients
                .lock()
                .unwrap()
                .contains_key(&first.unwrap().client_id)
        );
        assert!(
            register(
                test_requester(1),
                vec![
                    "https://unrelated.example/first".into(),
                    "https://unrelated.example/second".into(),
                ],
            )
            .is_ok()
        );
    }

    #[test]
    fn dcr_promotion_types_terminal_and_retryable_failures() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let register = || {
            runtime.insert_dcr_client(
                test_requester(1),
                RegistrationRequest {
                    client_name: Some("test".into()),
                    redirect_uris: vec!["https://client.example/callback".into()],
                    grant_types: vec!["authorization_code".into()],
                    response_types: vec!["code".into()],
                    token_endpoint_auth_method: Some("none".into()),
                },
            )
        };

        let expired = register().unwrap();
        runtime
            .inner
            .pending_clients
            .lock()
            .unwrap()
            .get_mut(&expired.client_id)
            .unwrap()
            .client
            .issued_at = now() - PENDING_DCR_TTL_SECS - 1;
        let error = runtime.promote_dcr_client(&expired.client_id).unwrap_err();
        assert_eq!(
            error.downcast_ref::<DcrPromotionTerminal>(),
            Some(&DcrPromotionTerminal::RegistrationExpired)
        );

        let retryable = register().unwrap();
        fs::write(&runtime.inner.state_path, b"broken OAuth state").unwrap();
        let error = runtime
            .promote_dcr_client(&retryable.client_id)
            .unwrap_err();
        assert!(error.downcast_ref::<DcrPromotionTerminal>().is_none());
        assert_eq!(
            runtime
                .inner
                .pending_clients
                .lock()
                .unwrap()
                .get(&retryable.client_id)
                .map(|pending| pending.requester),
            Some(test_requester(1))
        );
    }

    #[tokio::test]
    async fn terminal_dcr_capacity_consumes_consent_with_actionable_error() {
        use axum::body::to_bytes;
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let client = runtime
            .insert_dcr_client(
                test_requester(1),
                RegistrationRequest {
                    client_name: Some("test".into()),
                    redirect_uris: vec!["https://client.example/callback".into()],
                    grant_types: vec!["authorization_code".into()],
                    response_types: vec!["code".into()],
                    token_endpoint_auth_method: Some("none".into()),
                },
            )
            .unwrap();
        update_state(&runtime.inner.state_path, |state| {
            for index in 0..MAX_DCR_CLIENTS {
                let client_id = format!("dcr_existing_{index}");
                state.clients.insert(
                    client_id.clone(),
                    RegisteredClient {
                        client_id,
                        client_name: None,
                        redirect_uris: vec!["https://existing.example/callback".into()],
                        grant_types: vec!["authorization_code".into()],
                        response_types: vec!["code".into()],
                        token_endpoint_auth_method: "none".into(),
                        issued_at: now(),
                    },
                );
            }
            Ok(())
        })
        .unwrap();
        let request_id = random_token().unwrap();
        runtime
            .store_pending_authorization(
                test_requester(1),
                request_id.clone(),
                PendingAuthorization {
                    client_id: client.client_id,
                    client_name: None,
                    issuer: server.issuer.clone(),
                    redirect_uri: "https://client.example/callback".into(),
                    resource: server.resource.clone(),
                    scope: SCOPE_MCP.into(),
                    state: None,
                    code_challenge: "a".repeat(43),
                    issue_refresh: false,
                    expires_at: now() + PENDING_AUTH_TTL_SECS,
                },
            )
            .unwrap();

        let response = authorize_post(
            State(server),
            ConnectInfo(test_requester(1)),
            Form(AuthorizeForm {
                request_id: request_id.clone(),
                password: "long-enough-owner-password".into(),
                decision: "allow".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("revoke an unused client"));
        assert!(!body.contains(runtime.inner.state_path.to_str().unwrap()));
        assert!(
            !runtime
                .inner
                .pending
                .lock()
                .unwrap()
                .contains_key(&request_id)
        );
    }

    #[test]
    fn cimd_requester_rate_is_host_independent_and_identity_scoped() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_CIMD_FETCHES);
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let current = Instant::now();
        let first = test_requester(1);
        let other = test_requester(2);

        // Host is deliberately absent from admission: changing targets cannot
        // change this requester's rolling identity budget.
        for _ in 0..MAX_CIMD_FETCH_STARTS_PER_REQUESTER {
            drop(
                runtime
                    .admit_cimd_fetch_with_slots(first, current, &SLOTS)
                    .unwrap(),
            );
        }
        assert!(
            runtime
                .admit_cimd_fetch_with_slots(first, current, &SLOTS)
                .is_err()
        );
        drop(
            runtime
                .admit_cimd_fetch_with_slots(other, current, &SLOTS)
                .unwrap(),
        );
    }

    #[test]
    fn cimd_requester_in_flight_limit_is_independent_between_requesters() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_CIMD_FETCHES);
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let first = test_requester(1);
        let other = test_requester(2);
        let first_lease = runtime
            .admit_cimd_fetch_with_slots(first, Instant::now(), &SLOTS)
            .unwrap();
        let second_lease = runtime
            .admit_cimd_fetch_with_slots(first, Instant::now(), &SLOTS)
            .unwrap();
        assert!(
            runtime
                .admit_cimd_fetch_with_slots(first, Instant::now(), &SLOTS)
                .is_err()
        );
        let other_lease = runtime
            .admit_cimd_fetch_with_slots(other, Instant::now(), &SLOTS)
            .unwrap();

        drop(first_lease);
        assert!(
            runtime
                .admit_cimd_fetch_with_slots(first, Instant::now(), &SLOTS)
                .is_ok()
        );
        drop(second_lease);
        drop(other_lease);
    }

    #[test]
    fn full_cimd_requester_table_never_evicts_or_resets_live_history() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_CIMD_FETCHES);
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let current = Instant::now();
        let requester = |index: usize| {
            let mut prefix = [0u8; 6];
            prefix[..2].copy_from_slice(&(index as u16).to_be_bytes());
            RequesterId::Ipv6Prefix(prefix)
        };

        for index in 0..MAX_CIMD_REQUESTERS {
            drop(
                runtime
                    .admit_cimd_fetch_with_slots(requester(index), current, &SLOTS)
                    .unwrap(),
            );
        }
        let first = requester(0);
        let unseen = RequesterId::Local;
        assert!(
            runtime
                .admit_cimd_fetch_with_slots(unseen, current, &SLOTS)
                .is_err()
        );
        {
            let admission = runtime.inner.cimd_requesters.lock().unwrap();
            assert_eq!(admission.rows.len(), MAX_CIMD_REQUESTERS);
            assert!(admission.rows.contains_key(&first));
        }

        // A full table does not reset an existing requester's retained window.
        for _ in 1..MAX_CIMD_FETCH_STARTS_PER_REQUESTER {
            drop(
                runtime
                    .admit_cimd_fetch_with_slots(first, current, &SLOTS)
                    .unwrap(),
            );
        }
        assert!(
            runtime
                .admit_cimd_fetch_with_slots(first, current, &SLOTS)
                .is_err()
        );

        // Once every event has expired and no work is in flight, rows may be
        // pruned and a previously unseen requester can enter.
        drop(
            runtime
                .admit_cimd_fetch_with_slots(unseen, current + CIMD_RATE_WINDOW, &SLOTS)
                .unwrap(),
        );
        let admission = runtime.inner.cimd_requesters.lock().unwrap();
        assert_eq!(admission.rows.len(), 1);
        assert!(admission.rows.contains_key(&unseen));
    }

    #[test]
    fn password_requester_windows_are_bounded_non_evicting_and_expire() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let current = Instant::now();
        let first = test_requester(1);
        for _ in 0..MAX_PASSWORD_CHECK_STARTS_PER_REQUESTER {
            runtime.reserve_password_check_at(first, current).unwrap();
        }
        assert!(runtime.reserve_password_check_at(first, current).is_err());
        runtime
            .reserve_password_check_at(test_requester(2), current)
            .unwrap();

        let (full, _temp) = test_runtime("long-enough-owner-password");
        let requester = |index: usize| {
            let mut prefix = [0u8; 6];
            prefix[..2].copy_from_slice(&(index as u16).to_be_bytes());
            RequesterId::Ipv6Prefix(prefix)
        };
        for index in 0..MAX_PASSWORD_CHECK_REQUESTERS {
            full.reserve_password_check_at(requester(index), current)
                .unwrap();
        }
        let first = requester(0);
        assert!(
            full.reserve_password_check_at(RequesterId::Local, current)
                .is_err()
        );
        assert!(
            full.inner
                .password_requesters
                .lock()
                .unwrap()
                .rows
                .contains_key(&first)
        );
        for _ in 1..MAX_PASSWORD_CHECK_STARTS_PER_REQUESTER {
            full.reserve_password_check_at(first, current).unwrap();
        }
        assert!(full.reserve_password_check_at(first, current).is_err());

        full.reserve_password_check_at(RequesterId::Local, current + PASSWORD_CHECK_RATE_WINDOW)
            .unwrap();
        let admission = full.inner.password_requesters.lock().unwrap();
        assert_eq!(admission.rows.len(), 1);
        assert!(admission.rows.contains_key(&RequesterId::Local));
    }

    #[test]
    fn client_names_are_sanitized_at_ingestion_and_terminal_sinks() {
        let hostile = "雪 café <owner>\n\u{1b}\u{061c}\u{200e}\u{200f}\u{2028}\u{2029}\u{202a}\u{202e}\u{2066}\u{2069} done";
        let sanitized = sanitize_client_name(hostile);
        assert!(sanitized.contains("雪 café <owner>"));
        assert!(sanitized.contains('\u{fffd}'));
        assert!(!sanitized.chars().any(is_unsafe_client_name_char));
        assert_eq!(
            sanitize_client_name(&"雪".repeat(MAX_CLIENT_NAME_CHARS + 1))
                .chars()
                .count(),
            MAX_CLIENT_NAME_CHARS
        );

        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let registered = runtime
            .insert_dcr_client(
                test_requester(1),
                RegistrationRequest {
                    client_name: Some(hostile.to_owned()),
                    redirect_uris: vec!["https://client.example/callback".into()],
                    grant_types: vec!["authorization_code".into()],
                    response_types: vec!["code".into()],
                    token_endpoint_auth_method: Some("none".into()),
                },
            )
            .unwrap();
        assert_eq!(registered.client_name.as_deref(), Some(sanitized.as_str()));

        // Legacy state can contain names written before sanitization existed;
        // every persisted/terminal display path must still neutralize them.
        let legacy = RegisteredClient {
            client_id: "dcr_legacy".into(),
            client_name: Some(hostile.into()),
            redirect_uris: vec!["https://client.example/callback".into()],
            grant_types: vec!["authorization_code".into()],
            response_types: vec!["code".into()],
            token_endpoint_auth_method: "none".into(),
            issued_at: now(),
        };
        runtime
            .update_persistent(|state| {
                state
                    .clients
                    .insert(legacy.client_id.clone(), legacy.clone());
                Ok(())
            })
            .unwrap();
        let persisted = read_state(&runtime.inner.state_path)
            .unwrap()
            .clients
            .remove(&legacy.client_id)
            .unwrap();
        assert_eq!(persisted.client_name.as_deref(), Some(hostile));
        let metadata_name = client_metadata(&persisted).client_name.unwrap();
        assert_eq!(metadata_name, sanitized);
        let terminal = client_terminal_line(&persisted);
        assert!(terminal.contains("雪 café <owner>"));
        assert!(terminal.contains('\u{fffd}'));
        let terminal_fields = terminal.split('\t').collect::<Vec<_>>();
        assert_eq!(terminal_fields.len(), 3);
        assert!(!terminal_fields[1].chars().any(is_unsafe_client_name_char));
    }

    #[tokio::test]
    async fn owner_html_neutralizes_unsafe_client_name_characters() {
        use axum::body::to_bytes;

        let request = PendingAuthorization {
            client_id: "https://client.example/oauth.json".into(),
            client_name: Some(
                "雪 café <owner>\n\u{1b}\u{061c}\u{200f}\u{2028}\u{202e}\u{2066}\u{2069}".into(),
            ),
            issuer: "https://server.example".into(),
            redirect_uri: "https://client.example/callback".into(),
            resource: "https://server.example/mcp".into(),
            scope: SCOPE_MCP.into(),
            state: None,
            code_challenge: "a".repeat(43),
            issue_refresh: false,
            expires_at: now() + PENDING_AUTH_TTL_SECS,
        };
        let response = authorization_page("request", &request, None);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("雪 café &lt;owner&gt;"));
        assert!(body.contains('\u{fffd}'));
        assert!(!body.chars().any(is_unsafe_client_name_char));
    }

    #[test]
    fn private_addresses_are_rejected_for_cimd() {
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("10.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("192.168.1.1".parse().unwrap()));
        assert!(!is_public_ip("192.88.99.1".parse().unwrap()));
        assert!(!is_public_ip("192.88.99.255".parse().unwrap()));
        assert!(
            is_public_ip("192.88.99.2".parse().unwrap()),
            "IANA classifies the 6a44 /32 exception as globally reachable"
        );
        assert!(!is_public_ip("::1".parse().unwrap()));
        assert!(!is_public_ip("fd00::1".parse().unwrap()));
        for special in [
            "64:ff9b::7f00:1",
            "64:ff9b:1::1",
            "fec0::1",
            "::2",
            "2001:db8::1",
            "2001:2::1",
            "2002::1",
            "2620:4f:8000::1",
            "3fff::1",
        ] {
            assert!(!is_public_ip(special.parse().unwrap()), "{special}");
        }
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
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let error = fetch_cimd(
            &runtime,
            test_requester(1),
            "https://chatgpt.com/oauth/client.json?variant=1",
        )
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

    #[cfg(target_os = "linux")]
    #[test]
    fn owned_oauth_state_is_created_private_before_shell_mounts() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("nested/state");
        crate::private_state::prepare_owned_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Repeated preparation must not widen an existing directory.
        crate::private_state::prepare_owned_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn retiring_owner_generations_preserves_selected_and_recent_predecessor() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("oauth.json");
        let mut state = PersistentState {
            owner_password_hash: Some("legacy-hash".into()),
            ..PersistentState::default()
        };
        for (revision, hash) in [
            ("00000000000000000000000000000001", "hash-one"),
            ("00000000000000000000000000000002", "hash-two"),
            ("00000000000000000000000000000003", "hash-three"),
        ] {
            state
                .owner_password_generations
                .insert(revision.into(), hash.into());
        }
        write_state(&path, &state).unwrap();
        // Pure transaction logic: verify an independent file path without
        // overriding global XDG state for unrelated concurrent tests.
        let active = "00000000000000000000000000000003";
        let previous = "00000000000000000000000000000002";
        update_state(&path, |state| {
            retain_owner_password_generations(state, Some(active), Some(previous));
            Ok(())
        })
        .unwrap();
        let stored = read_state(&path).unwrap();
        assert_eq!(stored.owner_password_generations.len(), 2);
        assert_eq!(
            stored
                .owner_password_generations
                .get(active)
                .map(String::as_str),
            Some("hash-three")
        );
        assert_eq!(
            stored
                .owner_password_generations
                .get(previous)
                .map(String::as_str),
            Some("hash-two")
        );
        assert!(stored.owner_password_hash.is_none());
    }

    #[test]
    fn staged_owner_revision_does_not_change_active_legacy_password() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("oauth.json");
        let original = PersistentState {
            owner_password_hash: Some("old-hash".into()),
            ..PersistentState::default()
        };
        write_state(&path, &original).unwrap();

        let change = OwnerPasswordChange {
            path: path.clone(),
            revision: "000000000000000000000000000000ab".into(),
            updated_hash: "new-hash".into(),
        };
        change.apply().unwrap();
        let staged = read_state(&path).unwrap();
        assert_eq!(active_owner_hash(&staged, None), Some("old-hash"));
        assert_eq!(
            active_owner_hash(&staged, Some(change.revision())),
            Some("new-hash")
        );
        assert_eq!(active_owner_hash(&staged, Some("does-not-exist")), None);
        change.rollback().unwrap();
        let rolled_back = read_state(&path).unwrap();
        assert_eq!(active_owner_hash(&rolled_back, None), Some("old-hash"));
        assert_eq!(
            active_owner_hash(&rolled_back, Some(change.revision())),
            None
        );
    }

    #[test]
    fn new_owner_password_rollback_removes_empty_state_file() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("oauth.json");
        let change = OwnerPasswordChange {
            path: path.clone(),
            revision: "00000000000000000000000000000001".to_owned(),
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
            revision: "00000000000000000000000000000002".to_owned(),
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
                update_state_within(&path, Duration::from_secs(30), |state| {
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
    fn maximum_public_v1_record_counts_and_widths_round_trip() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("oauth.json");
        let mut state = PersistentState::default();
        for index in 0..MAX_DCR_CLIENTS {
            let client_id = format!("dcr_{index:04}");
            let redirect_uris = (0..12)
                .map(|redirect| {
                    let prefix = format!("https://client-{index}-{redirect}.example/");
                    format!("{prefix}{}", "c".repeat(4096 - prefix.len()))
                })
                .collect();
            state.clients.insert(
                client_id.clone(),
                RegisteredClient {
                    client_id,
                    client_name: Some(format!("Client {index}")),
                    redirect_uris,
                    grant_types: vec!["authorization_code".into(), "refresh_token".into()],
                    response_types: vec!["code".into()],
                    token_endpoint_auth_method: "none".into(),
                    issued_at: now(),
                },
            );
        }
        let client_prefix = "https://client.example/";
        let wide_client = format!("{client_prefix}{}", "c".repeat(4096 - client_prefix.len()));
        let resource_prefix = "https://mcp.example.com/";
        let wide_resource = format!(
            "{resource_prefix}{}",
            "r".repeat(4096 - resource_prefix.len())
        );
        for index in 0..MAX_REFRESH_GRANTS {
            state.refresh_tokens.insert(
                format!("{index:064x}"),
                RefreshGrant {
                    client_id: wide_client.clone(),
                    issuer: "https://mcp.example.com".into(),
                    resource: wide_resource.clone(),
                    scope: "mcp:access offline_access".into(),
                    expires_at: now() + REFRESH_TOKEN_TTL_SECS,
                },
            );
        }

        let serialized = serialize_state(&state).unwrap();
        assert!(serialized.len() > 2 * 1024 * 1024);
        assert!(serialized.len() <= MAX_SERIALIZED_OAUTH_STATE_BYTES);
        write_state(&path, &state).unwrap();
        assert!(read_state(&path).unwrap() == state);
    }

    #[test]
    fn oversized_state_cannot_replace_readable_bytes() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("oauth.json");
        let original = PersistentState::default();
        write_state(&path, &original).unwrap();
        let original_bytes = fs::read(&path).unwrap();

        let error = update_state(&path, |state| {
            state.owner_password_hash = Some("x".repeat(MAX_SERIALIZED_OAUTH_STATE_BYTES));
            Ok(())
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains("storage limit"), "{error:#}");
        assert_eq!(fs::read(&path).unwrap(), original_bytes);
        assert!(read_state(&path).unwrap() == original);
    }

    #[test]
    fn owner_password_has_a_minimum_length() {
        assert!(validate_owner_password("short").is_err());
        assert!(validate_owner_password("long-enough-owner-password").is_ok());
    }

    fn test_runtime(password: &str) -> (Runtime, TempDir) {
        test_runtime_with_password_checks(
            password,
            Arc::new(Semaphore::new(MAX_CONCURRENT_PASSWORD_CHECKS)),
        )
    }

    fn test_runtime_with_password_checks(
        password: &str,
        password_checks: Arc<Semaphore>,
    ) -> (Runtime, TempDir) {
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
                owner_selection: OwnerSelection::Fixed(None),
                persistent: Mutex::new(persistent),
                state_index: Mutex::new(StateIndex::default()),
                pending_clients: Mutex::new(HashMap::new()),
                cimd_resolver: Some(test_dns_resolver()),
                cimd_requesters: Arc::new(Mutex::new(CimdRequesterAdmission::default())),
                access_tokens: Mutex::new(HashMap::new()),
                access_token_slots: Arc::new(Semaphore::new(MAX_ACCESS_TOKENS)),
                access_token_clients: Arc::new(Mutex::new(HashMap::new())),
                authorization_codes: Mutex::new(HashMap::new()),
                refresh_token_claims: Arc::new(Mutex::new(HashSet::new())),
                pending: Mutex::new(HashMap::new()),
                password_checks,
                password_requesters: Mutex::new(PasswordRequesterAdmission::default()),
            }),
        };
        (runtime, temp)
    }

    #[test]
    fn production_runtime_generations_share_password_check_slots() {
        let (first, _first_temp) = test_runtime_with_password_checks(
            "long-enough-owner-password",
            process_password_check_slots(),
        );
        let (second, _second_temp) = test_runtime_with_password_checks(
            "long-enough-owner-password",
            process_password_check_slots(),
        );
        assert!(Arc::ptr_eq(
            &first.inner.password_checks,
            &second.inner.password_checks
        ));

        let (isolated, _isolated_temp) = test_runtime("long-enough-owner-password");
        assert!(!Arc::ptr_eq(
            &first.inner.password_checks,
            &isolated.inner.password_checks
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn refresh_disk_contention_does_not_block_bearer_validation() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let issuer = "https://example.com";
        let resource = "https://example.com/mcp";
        let (access, refresh) = runtime
            .issue_access_and_refresh("client", issuer, resource, SCOPE_MCP, true)
            .unwrap();
        let guard = crate::private_state::acquire(
            &state_lock_path(&runtime.inner.state_path),
            Duration::from_secs(1),
        )
        .unwrap();
        let worker = runtime.clone();
        let rotate = tokio::spawn(async move {
            worker.rotate_refresh(&refresh.unwrap(), "client", issuer, resource)
        });
        let mut contended = false;
        for _ in 0..100 {
            if runtime.inner.persistent.try_lock().is_err() {
                contended = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            contended,
            "refresh never entered its persistent state transaction"
        );
        let checked = runtime.clone();
        let validation = tokio::task::spawn_blocking(move || {
            checked.validate_access_token(&access, issuer, resource)
        });
        let valid = tokio::time::timeout(Duration::from_millis(100), validation)
            .await
            .expect("bearer validation blocked on persistent state")
            .unwrap()
            .unwrap();
        assert!(valid);
        drop(guard);
        assert!(rotate.await.unwrap().unwrap().is_some());
    }

    #[tokio::test]
    async fn malformed_token_exchange_does_not_touch_persistent_state() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        std::fs::write(&runtime.inner.state_path, b"invalid state").unwrap();
        let response = token(
            State(server),
            Form(TokenForm {
                grant_type: "refresh_token".into(),
                refresh_token: Some("malformed".into()),
                client_id: Some("client".into()),
                resource: Some("https://example.com/mcp".into()),
                code: None,
                redirect_uri: None,
                code_verifier: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn authorization_code_does_not_hold_mutex_across_durable_io() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let code = random_token().unwrap();
        let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~abc";
        runtime
            .store_authorization_code(
                &code,
                AuthorizationCode {
                    client_id: "client".into(),
                    issuer: server.issuer.clone(),
                    redirect_uri: "https://client.example/callback".into(),
                    resource: server.resource.clone(),
                    scope: SCOPE_MCP.into(),
                    code_challenge: pkce_challenge(verifier),
                    issue_refresh: true,
                    expires_at: now() + AUTH_CODE_TTL_SECS,
                },
            )
            .unwrap();
        let state_lock = crate::private_state::acquire(
            &state_lock_path(&runtime.inner.state_path),
            Duration::from_secs(1),
        )
        .unwrap();
        let exchanged = {
            let server = server.clone();
            let code = code.clone();
            tokio::spawn(async move {
                exchange_authorization_code_with_slots(
                    &server,
                    TokenForm {
                        grant_type: "authorization_code".into(),
                        code: Some(code),
                        redirect_uri: Some("https://client.example/callback".into()),
                        client_id: Some("client".into()),
                        code_verifier: Some(verifier.into()),
                        refresh_token: None,
                        resource: Some(server.resource.clone()),
                    },
                    &SLOTS,
                )
                .await
            })
        };

        let mut observed_claim = false;
        for _ in 0..100 {
            if let Ok(codes) = runtime.inner.authorization_codes.try_lock()
                && codes
                    .get(&token_hash(&code))
                    .is_some_and(|slot| slot.claimed)
            {
                observed_claim = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(observed_claim, "authorization-code mutex remained locked");
        drop(state_lock);
        assert!(exchanged.await.unwrap().is_ok());
        assert!(
            !runtime
                .inner
                .authorization_codes
                .lock()
                .unwrap()
                .contains_key(&token_hash(&code))
        );
    }

    #[tokio::test]
    async fn duplicate_authorization_codes_shed_without_waiting_or_global_admission() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~abc";
        let code = random_token().unwrap();
        runtime
            .store_authorization_code(
                &code,
                AuthorizationCode {
                    client_id: "client".into(),
                    issuer: server.issuer.clone(),
                    redirect_uri: "https://client.example/callback".into(),
                    resource: server.resource.clone(),
                    scope: SCOPE_MCP.into(),
                    code_challenge: pkce_challenge(verifier),
                    issue_refresh: false,
                    expires_at: now() + AUTH_CODE_TTL_SECS,
                },
            )
            .unwrap();
        let held = match runtime.claim_authorization_code(&code).unwrap() {
            AuthorizationCodeClaimResult::Claimed(claim) => claim,
            _ => panic!("authorization code was not claimed"),
        };
        let all_global = SLOTS
            .try_acquire_many(MAX_CONCURRENT_TOKEN_EXCHANGES as u32)
            .expect("initial code claim consumed global exchange permits");

        for _ in 0..(MAX_AUTHORIZATION_CODES * 4) {
            let exchange = exchange_authorization_code_with_slots(
                &server,
                TokenForm {
                    grant_type: "authorization_code".into(),
                    code: Some(code.clone()),
                    redirect_uri: Some("https://client.example/callback".into()),
                    client_id: Some("client".into()),
                    code_verifier: Some(verifier.into()),
                    refresh_token: None,
                    resource: Some(server.resource.clone()),
                },
                &SLOTS,
            );
            let error = tokio::time::timeout(Duration::from_millis(50), exchange)
                .await
                .expect("duplicate authorization-code exchange waited")
                .unwrap_err();
            assert!(error.is::<OAuthCapacityExceeded>());
        }
        assert!(
            runtime
                .inner
                .authorization_codes
                .lock()
                .unwrap()
                .get(&token_hash(&code))
                .is_some_and(|slot| slot.claimed)
        );

        drop(held);
        let busy_error = exchange_authorization_code_with_slots(
            &server,
            TokenForm {
                grant_type: "authorization_code".into(),
                code: Some(code.clone()),
                redirect_uri: Some("https://client.example/callback".into()),
                client_id: Some("client".into()),
                code_verifier: Some(verifier.into()),
                refresh_token: None,
                resource: Some(server.resource.clone()),
            },
            &SLOTS,
        )
        .await
        .unwrap_err();
        assert!(busy_error.is::<OAuthCapacityExceeded>());
        assert!(
            runtime
                .inner
                .authorization_codes
                .lock()
                .unwrap()
                .get(&token_hash(&code))
                .is_some_and(|slot| !slot.claimed)
        );

        drop(all_global);
        assert!(
            exchange_authorization_code_with_slots(
                &server,
                TokenForm {
                    grant_type: "authorization_code".into(),
                    code: Some(code),
                    redirect_uri: Some("https://client.example/callback".into()),
                    client_id: Some("client".into()),
                    code_verifier: Some(verifier.into()),
                    refresh_token: None,
                    resource: Some(server.resource.clone()),
                },
                &SLOTS,
            )
            .await
            .is_ok()
        );
    }

    #[test]
    fn duplicate_refresh_claims_shed_before_global_admission_and_release() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let (_, refresh) = runtime
            .issue_access_and_refresh(
                "client",
                server.issuer(),
                server.resource(),
                SCOPE_MCP,
                true,
            )
            .unwrap();
        let refresh = refresh.unwrap();
        let form = || TokenForm {
            grant_type: "refresh_token".into(),
            code: None,
            redirect_uri: None,
            client_id: Some("client".into()),
            code_verifier: None,
            refresh_token: Some(refresh.clone()),
            resource: Some(server.resource.clone()),
        };

        let held = runtime.claim_refresh_token(&refresh).unwrap();
        let error = exchange_refresh_token_with_slots(&server, form(), &SLOTS).unwrap_err();
        assert!(error.is::<OAuthCapacityExceeded>());
        assert_eq!(SLOTS.available_permits(), MAX_CONCURRENT_TOKEN_EXCHANGES);
        drop(held);

        let all_global = SLOTS
            .try_acquire_many(MAX_CONCURRENT_TOKEN_EXCHANGES as u32)
            .unwrap();
        let error = exchange_refresh_token_with_slots(&server, form(), &SLOTS).unwrap_err();
        assert!(error.is::<OAuthCapacityExceeded>());
        assert!(
            runtime
                .inner
                .refresh_token_claims
                .lock()
                .unwrap()
                .is_empty()
        );
        assert!(
            read_state(&runtime.inner.state_path)
                .unwrap()
                .refresh_tokens
                .contains_key(&token_hash(&refresh))
        );
        drop(all_global);
        assert!(exchange_refresh_token_with_slots(&server, form(), &SLOTS).is_ok());
    }

    #[test]
    fn authorization_code_capacity_rejects_without_evicting_live_codes() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let make_code = || AuthorizationCode {
            client_id: "client".into(),
            issuer: "https://example.com".into(),
            redirect_uri: "https://client.example/callback".into(),
            resource: "https://example.com/mcp".into(),
            scope: SCOPE_MCP.into(),
            code_challenge: "a".repeat(43),
            issue_refresh: false,
            expires_at: now() + AUTH_CODE_TTL_SECS,
        };
        let first = "capacity-code-0";
        for index in 0..MAX_AUTHORIZATION_CODES {
            runtime
                .store_authorization_code(&format!("capacity-code-{index}"), make_code())
                .unwrap();
        }
        let error = runtime
            .store_authorization_code("capacity-code-overload", make_code())
            .unwrap_err();
        assert!(error.is::<OAuthCapacityExceeded>());
        let codes = runtime.inner.authorization_codes.lock().unwrap();
        assert_eq!(codes.len(), MAX_AUTHORIZATION_CODES);
        assert!(codes.contains_key(&token_hash(first)));
    }

    #[test]
    fn in_flight_access_reservations_obey_per_client_and_global_capacity() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let mut held = Vec::new();
        for _ in 0..MAX_ACCESS_TOKENS_PER_CLIENT {
            held.push(runtime.reserve_access_token_capacity("client-a").unwrap());
        }
        assert!(runtime.reserve_access_token_capacity("client-a").is_err());
        let other = runtime.reserve_access_token_capacity("client-b").unwrap();
        drop(held.pop());
        held.push(runtime.reserve_access_token_capacity("client-a").unwrap());
        drop(held);
        drop(other);
        assert!(
            runtime
                .inner
                .access_token_clients
                .lock()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            runtime.inner.access_token_slots.available_permits(),
            MAX_ACCESS_TOKENS
        );
    }

    #[test]
    fn rotating_one_client_cannot_exhaust_another_clients_access_capacity() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let issuer = "https://example.com";
        let resource = "https://example.com/mcp";
        let (first_access, refresh) = runtime
            .issue_access_and_refresh("client-a", issuer, resource, SCOPE_MCP, true)
            .unwrap();
        let mut refresh = refresh.unwrap();
        let mut accesses = vec![first_access.clone()];
        for _ in 1..MAX_ACCESS_TOKENS_PER_CLIENT {
            let (access, next, _) = runtime
                .rotate_refresh(&refresh, "client-a", issuer, resource)
                .unwrap()
                .unwrap();
            accesses.push(access);
            refresh = next;
        }
        let error = runtime
            .rotate_refresh(&refresh, "client-a", issuer, resource)
            .unwrap_err();
        assert!(error.is::<OAuthCapacityExceeded>());
        assert!(
            read_state(&runtime.inner.state_path)
                .unwrap()
                .refresh_tokens
                .contains_key(&token_hash(&refresh))
        );
        assert!(
            runtime
                .validate_access_token(&first_access, issuer, resource)
                .unwrap()
        );

        let (other_access, _) = runtime
            .issue_access_and_refresh("client-b", issuer, resource, SCOPE_MCP, false)
            .unwrap();
        assert!(
            runtime
                .validate_access_token(&other_access, issuer, resource)
                .unwrap()
        );

        // Explicit removal and expiry both release the per-client and global
        // reservations, allowing the blocked refresh chain to continue.
        runtime.revoke(&first_access).unwrap();
        let (_, next, _) = runtime
            .rotate_refresh(&refresh, "client-a", issuer, resource)
            .unwrap()
            .unwrap();
        refresh = next;
        runtime
            .inner
            .access_tokens
            .lock()
            .unwrap()
            .get_mut(&token_hash(&accesses[1]))
            .unwrap()
            .expires_at = now() - 1;
        assert!(
            runtime
                .rotate_refresh(&refresh, "client-a", issuer, resource)
                .unwrap()
                .is_some()
        );

        // A reservation made before durable validation is released on failure.
        assert!(
            runtime
                .issue_access_and_refresh("dcr_missing", issuer, resource, SCOPE_MCP, false,)
                .is_err()
        );
        assert!(
            !runtime
                .inner
                .access_token_clients
                .lock()
                .unwrap()
                .contains_key("dcr_missing")
        );

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _clients = runtime.inner.access_token_clients.lock().unwrap();
            panic!("poison per-client access admission");
        }));
        assert!(poisoned.is_err());
        assert!(
            runtime
                .issue_access_and_refresh("client-b", issuer, resource, SCOPE_MCP, false)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn access_capacity_preserves_live_tokens_refreshes_and_codes() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let first_access = fill_access_token_capacity(&runtime, server.issuer(), server.resource());
        assert!(
            runtime
                .validate_access_token(&first_access, server.issuer(), server.resource())
                .unwrap()
        );

        let refresh = random_token().unwrap();
        update_state(&runtime.inner.state_path, |state| {
            state.refresh_tokens.insert(
                token_hash(&refresh),
                RefreshGrant {
                    client_id: "client".into(),
                    issuer: server.issuer.clone(),
                    resource: server.resource.clone(),
                    scope: SCOPE_MCP.into(),
                    expires_at: now() + REFRESH_TOKEN_TTL_SECS,
                },
            );
            Ok(())
        })
        .unwrap();
        let error = runtime
            .rotate_refresh(&refresh, "client", server.issuer(), server.resource())
            .unwrap_err();
        assert!(error.is::<OAuthCapacityExceeded>());
        assert!(
            read_state(&runtime.inner.state_path)
                .unwrap()
                .refresh_tokens
                .contains_key(&token_hash(&refresh))
        );

        let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~abc";
        let code = random_token().unwrap();
        runtime
            .store_authorization_code(
                &code,
                AuthorizationCode {
                    client_id: "client".into(),
                    issuer: server.issuer.clone(),
                    redirect_uri: "https://client.example/callback".into(),
                    resource: server.resource.clone(),
                    scope: SCOPE_MCP.into(),
                    code_challenge: pkce_challenge(verifier),
                    issue_refresh: false,
                    expires_at: now() + AUTH_CODE_TTL_SECS,
                },
            )
            .unwrap();
        let error = exchange_authorization_code_with_slots(
            &server,
            TokenForm {
                grant_type: "authorization_code".into(),
                code: Some(code.clone()),
                redirect_uri: Some("https://client.example/callback".into()),
                client_id: Some("client".into()),
                code_verifier: Some(verifier.into()),
                refresh_token: None,
                resource: Some(server.resource.clone()),
            },
            &SLOTS,
        )
        .await
        .unwrap_err();
        assert!(error.is::<TokenIssuanceFailure>());
        let codes = runtime.inner.authorization_codes.lock().unwrap();
        let stored = codes.get(&token_hash(&code)).unwrap();
        assert!(!stored.claimed);
        assert_eq!(
            runtime.inner.access_tokens.lock().unwrap().len(),
            MAX_ACCESS_TOKENS
        );
    }

    #[test]
    fn refresh_capacity_rejects_growth_without_evicting_and_allows_rotation() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let issuer = "https://example.com";
        let resource = "https://example.com/mcp";
        let refresh = random_token().unwrap();
        let selected_hash = token_hash(&refresh);
        update_state(&runtime.inner.state_path, |state| {
            state.refresh_tokens.insert(
                selected_hash.clone(),
                RefreshGrant {
                    client_id: "client".into(),
                    issuer: issuer.into(),
                    resource: resource.into(),
                    scope: SCOPE_MCP.into(),
                    expires_at: now() + REFRESH_TOKEN_TTL_SECS,
                },
            );
            for index in 1..MAX_REFRESH_GRANTS {
                state.refresh_tokens.insert(
                    format!("capacity-refresh-{index}"),
                    RefreshGrant {
                        client_id: "other".into(),
                        issuer: issuer.into(),
                        resource: resource.into(),
                        scope: SCOPE_MCP.into(),
                        expires_at: now() + REFRESH_TOKEN_TTL_SECS,
                    },
                );
            }
            Ok(())
        })
        .unwrap();

        let (_, rotated, _) = runtime
            .rotate_refresh(&refresh, "client", issuer, resource)
            .unwrap()
            .unwrap();
        let state = read_state(&runtime.inner.state_path).unwrap();
        assert_eq!(state.refresh_tokens.len(), MAX_REFRESH_GRANTS);
        assert!(!state.refresh_tokens.contains_key(&selected_hash));
        assert!(state.refresh_tokens.contains_key(&token_hash(&rotated)));
        assert!(state.refresh_tokens.contains_key("capacity-refresh-1"));

        let error = runtime
            .issue_access_and_refresh("client", issuer, resource, SCOPE_MCP, true)
            .unwrap_err();
        assert!(error.is::<OAuthCapacityExceeded>());
        let state = read_state(&runtime.inner.state_path).unwrap();
        assert_eq!(state.refresh_tokens.len(), MAX_REFRESH_GRANTS);
        assert!(state.refresh_tokens.contains_key("capacity-refresh-1"));
    }

    #[test]
    fn refresh_rotation_storage_failure_preserves_old_grant() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let issuer = "https://example.com";
        let resource = "https://example.com/mcp";
        let (_, refresh) = runtime
            .issue_access_and_refresh("client", issuer, resource, SCOPE_MCP, true)
            .unwrap();
        let refresh = refresh.unwrap();
        let held = crate::private_state::acquire(
            &state_lock_path(&runtime.inner.state_path),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(
            runtime
                .rotate_refresh(&refresh, "client", issuer, resource)
                .is_err()
        );
        drop(held);
        assert!(
            read_state(&runtime.inner.state_path)
                .unwrap()
                .refresh_tokens
                .contains_key(&token_hash(&refresh))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authorization_code_storage_failure_preserves_the_code() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~abc";
        let code = random_token().unwrap();
        runtime
            .store_authorization_code(
                &code,
                AuthorizationCode {
                    client_id: "client".into(),
                    issuer: server.issuer.clone(),
                    redirect_uri: "https://client.example/callback".into(),
                    resource: server.resource.clone(),
                    scope: SCOPE_MCP.into(),
                    code_challenge: pkce_challenge(verifier),
                    issue_refresh: true,
                    expires_at: now() + AUTH_CODE_TTL_SECS,
                },
            )
            .unwrap();
        let held = crate::private_state::acquire(
            &state_lock_path(&runtime.inner.state_path),
            Duration::from_secs(1),
        )
        .unwrap();
        let error = exchange_authorization_code_with_slots(
            &server,
            TokenForm {
                grant_type: "authorization_code".into(),
                code: Some(code.clone()),
                redirect_uri: Some("https://client.example/callback".into()),
                client_id: Some("client".into()),
                code_verifier: Some(verifier.into()),
                refresh_token: None,
                resource: Some(server.resource.clone()),
            },
            &SLOTS,
        )
        .await
        .unwrap_err();
        assert!(error.is::<TokenIssuanceFailure>());
        drop(held);
        let codes = runtime.inner.authorization_codes.lock().unwrap();
        let stored = codes.get(&token_hash(&code)).unwrap();
        assert!(!stored.claimed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn revoked_dcr_client_terminally_consumes_code_after_authoritative_read() {
        use axum::body::to_bytes;

        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let client_id = "dcr_revoked";
        runtime
            .update_persistent(|state| {
                state.clients.insert(
                    client_id.into(),
                    RegisteredClient {
                        client_id: client_id.into(),
                        client_name: Some("Revoked client".into()),
                        redirect_uris: vec!["https://client.example/callback".into()],
                        grant_types: vec!["authorization_code".into()],
                        response_types: vec!["code".into()],
                        token_endpoint_auth_method: "none".into(),
                        issued_at: now(),
                    },
                );
                Ok(())
            })
            .unwrap();

        let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~abc";
        let code = random_token().unwrap();
        runtime
            .store_authorization_code(
                &code,
                AuthorizationCode {
                    client_id: client_id.into(),
                    issuer: server.issuer.clone(),
                    redirect_uri: "https://client.example/callback".into(),
                    resource: server.resource.clone(),
                    scope: SCOPE_MCP.into(),
                    code_challenge: pkce_challenge(verifier),
                    issue_refresh: false,
                    expires_at: now() + AUTH_CODE_TTL_SECS,
                },
            )
            .unwrap();
        runtime
            .update_persistent(|state| {
                assert!(state.clients.remove(client_id).is_some());
                Ok(())
            })
            .unwrap();

        let resource = server.resource.clone();
        let form = || TokenForm {
            grant_type: "authorization_code".into(),
            code: Some(code.clone()),
            redirect_uri: Some("https://client.example/callback".into()),
            client_id: Some(client_id.into()),
            code_verifier: Some(verifier.into()),
            refresh_token: None,
            resource: Some(resource.clone()),
        };

        // Failure to acquire/read authoritative storage remains retryable and
        // must not consume the code even though the client was revoked.
        let held = crate::private_state::acquire(
            &state_lock_path(&runtime.inner.state_path),
            Duration::from_secs(1),
        )
        .unwrap();
        let unavailable = token_with_slots(server.clone(), form(), &SLOTS).await;
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(unavailable.into_body(), 64 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "server_error");
        assert!(
            runtime
                .inner
                .authorization_codes
                .lock()
                .unwrap()
                .get(&token_hash(&code))
                .is_some_and(|stored| !stored.claimed)
        );
        drop(held);

        // The next authoritative read proves absence, making revocation a
        // terminal invalid_grant and consuming the exclusively claimed code.
        let revoked = token_with_slots(server, form(), &SLOTS).await;
        assert_eq!(revoked.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(revoked.into_body(), 64 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "invalid_grant");
        assert!(
            !runtime
                .inner
                .authorization_codes
                .lock()
                .unwrap()
                .contains_key(&token_hash(&code))
        );
    }

    #[test]
    fn failed_or_cancelled_authorization_code_claim_is_retryable() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let code = random_token().unwrap();
        runtime
            .store_authorization_code(
                &code,
                AuthorizationCode {
                    client_id: "client".into(),
                    issuer: "https://example.com".into(),
                    redirect_uri: "https://client.example/callback".into(),
                    resource: "https://example.com/mcp".into(),
                    scope: SCOPE_MCP.into(),
                    code_challenge: "a".repeat(43),
                    issue_refresh: false,
                    expires_at: now() + AUTH_CODE_TTL_SECS,
                },
            )
            .unwrap();
        let claim = match runtime.claim_authorization_code(&code).unwrap() {
            AuthorizationCodeClaimResult::Claimed(claim) => claim,
            _ => panic!("code was not claimed"),
        };
        assert!(matches!(
            runtime.claim_authorization_code(&code).unwrap(),
            AuthorizationCodeClaimResult::Busy
        ));
        drop(claim);
        assert!(matches!(
            runtime.claim_authorization_code(&code).unwrap(),
            AuthorizationCodeClaimResult::Claimed(_)
        ));
    }

    #[tokio::test]
    async fn authorization_code_state_failure_is_retryable_server_error() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = runtime.inner.authorization_codes.lock().unwrap();
            panic!("poison authorization-code state");
        }));
        assert!(poisoned.is_err());

        let response = token_with_slots(
            server.clone(),
            TokenForm {
                grant_type: "authorization_code".into(),
                code: Some(random_token().unwrap()),
                redirect_uri: Some("https://client.example/callback".into()),
                client_id: Some("client".into()),
                code_verifier: Some(
                    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~abc".into(),
                ),
                refresh_token: None,
                resource: Some(server.resource.clone()),
            },
            &SLOTS,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn speculative_refresh_lookup_does_not_wait_for_state_index() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let held = runtime.inner.state_index.lock().unwrap();
        let worker = runtime.clone();
        let (result, completed) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            result
                .send(worker.might_contain_refresh(&"a".repeat(43)))
                .unwrap();
        });

        assert_eq!(
            completed
                .recv_timeout(Duration::from_secs(1))
                .expect("speculative lookup waited for the state-index mutex"),
            StatePresence::Uncertain
        );
        drop(held);
        thread.join().unwrap();
    }

    #[test]
    fn refresh_negative_cache_tracks_durable_cross_process_grants() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let raw = random_token().unwrap();
        assert_eq!(runtime.might_contain_refresh(&raw), StatePresence::Absent);
        // Simulate another dotlink process committing a grant: the cached
        // negative must become positive when the atomic state file changes.
        update_state(&runtime.inner.state_path, |state| {
            state.refresh_tokens.insert(
                token_hash(&raw),
                RefreshGrant {
                    client_id: "client".into(),
                    issuer: "https://example.com".into(),
                    resource: "https://example.com/mcp".into(),
                    scope: SCOPE_MCP.into(),
                    expires_at: now() + 3600,
                },
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(runtime.might_contain_refresh(&raw), StatePresence::Present);
        assert_eq!(
            runtime.might_contain_refresh("beyond-issuer"),
            StatePresence::Absent
        );
        update_state(&runtime.inner.state_path, |state| {
            state.refresh_tokens.remove(&token_hash(&raw));
            Ok(())
        })
        .unwrap();
        assert_eq!(runtime.might_contain_refresh(&raw), StatePresence::Absent);
    }

    #[test]
    fn stable_malformed_and_oversized_indexes_are_unavailable_not_uncertain() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let raw = random_token().unwrap();
        fs::write(&runtime.inner.state_path, b"broken OAuth state").unwrap();
        assert_eq!(
            runtime.might_contain_refresh(&raw),
            StatePresence::StableUnavailable
        );
        assert_eq!(
            runtime.might_contain_refresh(&raw),
            StatePresence::StableUnavailable
        );

        let file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&runtime.inner.state_path)
            .unwrap();
        file.set_len(MAX_SERIALIZED_OAUTH_STATE_BYTES as u64 + 1)
            .unwrap();
        drop(file);
        assert_eq!(
            runtime.might_contain_refresh(&raw),
            StatePresence::StableUnavailable
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stable_unavailable_state_returns_sanitized_503_without_locked_reread() {
        use axum::body::to_bytes;
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        fs::write(&runtime.inner.state_path, b"broken OAuth state").unwrap();
        let raw = random_token().unwrap();
        assert_eq!(
            runtime.might_contain_refresh(&raw),
            StatePresence::StableUnavailable
        );
        let state_lock = crate::private_state::acquire(
            &state_lock_path(&runtime.inner.state_path),
            Duration::from_secs(1),
        )
        .unwrap();

        let started = Instant::now();
        let token_response = token_with_slots(
            server.clone(),
            TokenForm {
                grant_type: "refresh_token".into(),
                code: None,
                redirect_uri: None,
                client_id: Some("client".into()),
                code_verifier: None,
                refresh_token: Some(raw.clone()),
                resource: Some(server.resource.clone()),
            },
            &SLOTS,
        )
        .await;
        assert_eq!(token_response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(started.elapsed() < STATE_LOCK_TIMEOUT / 2);
        let body = to_bytes(token_response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains(runtime.inner.state_path.to_str().unwrap()));
        assert!(!body.contains("broken OAuth state"));

        let started = Instant::now();
        let revoke_response = revoke(State(server), Form(RevokeForm { token: raw })).await;
        assert_eq!(revoke_response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(started.elapsed() < STATE_LOCK_TIMEOUT / 2);
        drop(state_lock);
    }

    #[tokio::test]
    async fn state_index_contention_falls_through_to_cross_process_grant() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let raw = random_token().unwrap();
        update_state(&runtime.inner.state_path, |state| {
            state.refresh_tokens.insert(
                token_hash(&raw),
                RefreshGrant {
                    client_id: "client".into(),
                    issuer: server.issuer.clone(),
                    resource: server.resource.clone(),
                    scope: SCOPE_MCP.into(),
                    expires_at: now() + REFRESH_TOKEN_TTL_SECS,
                },
            );
            Ok(())
        })
        .unwrap();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder_runtime = runtime.clone();
        let holder = std::thread::spawn(move || {
            let _index = holder_runtime.inner.state_index.lock().unwrap();
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        held_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let response = token_with_slots(
            server.clone(),
            TokenForm {
                grant_type: "refresh_token".into(),
                code: None,
                redirect_uri: None,
                client_id: Some("client".into()),
                code_verifier: None,
                refresh_token: Some(raw),
                resource: Some(server.resource.clone()),
            },
            &SLOTS,
        )
        .await;
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn expired_refreshes_skip_contended_state_lock() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let raw = random_token().unwrap();
        update_state(&runtime.inner.state_path, |state| {
            state.refresh_tokens.insert(
                token_hash(&raw),
                RefreshGrant {
                    client_id: "client".into(),
                    issuer: server.issuer.clone(),
                    resource: server.resource.clone(),
                    scope: SCOPE_MCP.into(),
                    expires_at: now() - 1,
                },
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(runtime.might_contain_refresh(&raw), StatePresence::Absent);

        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let lock_path = state_lock_path(&runtime.inner.state_path);
        let holder = std::thread::spawn(move || {
            let _lock = crate::private_state::acquire(&lock_path, Duration::from_secs(1)).unwrap();
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        held_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let token_server = server.clone();
        let token_raw = raw.clone();
        let token_response = tokio::time::timeout(
            Duration::from_millis(100),
            tokio::spawn(async move {
                token_with_slots(
                    token_server.clone(),
                    TokenForm {
                        grant_type: "refresh_token".into(),
                        code: None,
                        redirect_uri: None,
                        client_id: Some("client".into()),
                        code_verifier: None,
                        refresh_token: Some(token_raw),
                        resource: Some(token_server.resource.clone()),
                    },
                    &SLOTS,
                )
                .await
            }),
        )
        .await
        .expect("expired refresh waited on persistent state")
        .unwrap();
        assert_eq!(token_response.status(), StatusCode::BAD_REQUEST);

        let revoke_response = tokio::time::timeout(
            Duration::from_millis(100),
            tokio::spawn(
                async move { revoke(State(server), Form(RevokeForm { token: raw })).await },
            ),
        )
        .await
        .expect("expired revocation waited on persistent state")
        .unwrap();
        assert_eq!(revoke_response.status(), StatusCode::OK);

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    #[tokio::test]
    async fn registered_clients_reuse_shared_versioned_state_and_sanitize_failures() {
        use axum::body::to_bytes;
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let client_id = "dcr_regression";
        assert!(
            server
                .resolve_client(test_requester(1), client_id)
                .await
                .is_err()
        );
        // A second process can approve a DCR client; the cached negative must
        // invalidate when the durable file is atomically replaced.
        update_state(&runtime.inner.state_path, |state| {
            state.clients.insert(
                client_id.into(),
                RegisteredClient {
                    client_id: client_id.into(),
                    client_name: Some("Example".into()),
                    redirect_uris: vec!["https://client.example/callback".into()],
                    grant_types: vec!["authorization_code".into(), "refresh_token".into()],
                    response_types: vec!["code".into()],
                    token_endpoint_auth_method: "none".into(),
                    issued_at: now(),
                },
            );
            Ok(())
        })
        .unwrap();
        assert!(
            server
                .resolve_client(test_requester(1), client_id)
                .await
                .is_ok()
        );
        update_state(&runtime.inner.state_path, |state| {
            state.clients.remove(client_id);
            Ok(())
        })
        .unwrap();
        assert!(
            server
                .resolve_client(test_requester(1), client_id)
                .await
                .is_err()
        );

        std::fs::write(&runtime.inner.state_path, b"broken private state").unwrap();
        let request = AuthorizeQuery {
            response_type: "code".into(),
            client_id: client_id.into(),
            redirect_uri: "https://client.example/callback".into(),
            state: None,
            scope: None,
            resource: Some(server.resource.clone()),
            code_challenge: "a".repeat(43),
            code_challenge_method: "S256".into(),
        };
        for _ in 0..3 {
            let response = authorize_get(
                State(server.clone()),
                ConnectInfo(test_requester(1)),
                Query(request.clone()),
            )
            .await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            let body = String::from_utf8_lossy(&body);
            assert!(!body.contains(runtime.inner.state_path.to_str().unwrap()));
            assert!(!body.contains("broken private state"));
        }
    }

    #[tokio::test]
    async fn password_capacity_is_hardened_429_and_preserves_the_claim() {
        use axum::body::to_bytes;
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let requester = test_requester(1);
        let request_id = "password-capacity-request".to_owned();
        runtime
            .store_pending_authorization(
                requester,
                request_id.clone(),
                PendingAuthorization {
                    client_id: "https://client.example/oauth.json".into(),
                    client_name: None,
                    issuer: server.issuer.clone(),
                    redirect_uri: "https://client.example/callback".into(),
                    resource: server.resource.clone(),
                    scope: SCOPE_MCP.into(),
                    state: None,
                    code_challenge: "a".repeat(43),
                    issue_refresh: false,
                    expires_at: now() + PENDING_AUTH_TTL_SECS,
                },
            )
            .unwrap();
        let current = Instant::now();
        for _ in 0..MAX_PASSWORD_CHECK_STARTS_PER_REQUESTER {
            runtime
                .reserve_password_check_at(requester, current)
                .unwrap();
        }
        let held = runtime
            .inner
            .password_checks
            .clone()
            .acquire_many_owned(MAX_CONCURRENT_PASSWORD_CHECKS as u32)
            .await
            .unwrap();
        let started = Instant::now();
        let response = authorize_post(
            State(server),
            ConnectInfo(requester),
            Form(AuthorizeForm {
                request_id: request_id.clone(),
                password: "long-enough-owner-password".into(),
                decision: "allow".into(),
            }),
        )
        .await;
        assert!(started.elapsed() < Duration::from_millis(100));
        drop(held);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&HeaderValue::from_static("no-store"))
        );
        assert!(response.headers().contains_key(CONTENT_SECURITY_POLICY));
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains("capacity exhausted"));
        let pending = runtime.inner.pending.lock().unwrap();
        let consent = pending.get(&request_id).unwrap();
        assert_eq!(consent.requester, requester);
        assert!(!consent.claimed);
    }

    #[tokio::test]
    async fn password_global_admission_precedes_private_state_reads() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        fs::write(&runtime.inner.state_path, b"broken private state").unwrap();
        let held = runtime
            .inner
            .password_checks
            .clone()
            .acquire_many_owned(MAX_CONCURRENT_PASSWORD_CHECKS as u32)
            .await
            .unwrap();
        let worker = runtime.clone();
        let check = tokio::spawn(async move {
            worker
                .verify_owner_password(
                    test_requester(1),
                    test_requester(1),
                    "long-enough-owner-password",
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !check.is_finished(),
            "private state was read before admission"
        );
        assert!(
            runtime
                .inner
                .password_requesters
                .lock()
                .unwrap()
                .rows
                .is_empty()
        );
        drop(held);
        assert!(check.await.unwrap().is_err());
        assert!(
            runtime
                .inner
                .password_requesters
                .lock()
                .unwrap()
                .rows
                .contains_key(&test_requester(1))
        );
    }

    #[test]
    fn password_dual_requester_reservation_is_deduplicated_and_atomic() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let current = Instant::now();
        let first = test_requester(1);
        let full = test_requester(2);
        for _ in 0..MAX_PASSWORD_CHECK_STARTS_PER_REQUESTER {
            runtime
                .reserve_password_checks_at(full, full, current)
                .unwrap();
        }
        assert!(
            runtime
                .reserve_password_checks_at(first, full, current)
                .is_err()
        );
        let admission = runtime.inner.password_requesters.lock().unwrap();
        assert!(!admission.rows.contains_key(&first));
        assert_eq!(
            admission.rows.get(&full).unwrap().len(),
            MAX_PASSWORD_CHECK_STARTS_PER_REQUESTER
        );
        drop(admission);

        let (deduplicated, _temp) = test_runtime("long-enough-owner-password");
        for _ in 0..MAX_PASSWORD_CHECK_STARTS_PER_REQUESTER {
            deduplicated
                .reserve_password_checks_at(first, first, current)
                .unwrap();
        }
        assert!(
            deduplicated
                .reserve_password_checks_at(first, first, current)
                .is_err()
        );
        assert_eq!(
            deduplicated
                .inner
                .password_requesters
                .lock()
                .unwrap()
                .rows
                .get(&first)
                .unwrap()
                .len(),
            MAX_PASSWORD_CHECK_STARTS_PER_REQUESTER
        );
    }

    #[tokio::test]
    async fn oversized_password_charges_both_requesters_but_releases_argon_capacity() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let worker = runtime.clone();
        let check = tokio::spawn(async move {
            worker
                .verify_owner_password(test_requester(1), test_requester(2), &"x".repeat(1025))
                .await
        });
        for _ in 0..100 {
            if runtime.inner.password_requesters.lock().unwrap().rows.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            runtime.inner.password_requesters.lock().unwrap().rows.len(),
            2
        );
        for _ in 0..100 {
            if runtime.inner.password_checks.available_permits() == MAX_CONCURRENT_PASSWORD_CHECKS {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            runtime.inner.password_checks.available_permits(),
            MAX_CONCURRENT_PASSWORD_CHECKS,
            "oversized-password delay retained an Argon2 permit"
        );
        assert!(!check.await.unwrap().unwrap());
        let admission = runtime.inner.password_requesters.lock().unwrap();
        assert_eq!(admission.rows.get(&test_requester(1)).unwrap().len(), 1);
        assert_eq!(admission.rows.get(&test_requester(2)).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn wrong_owner_password_retries_with_original_requester() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let requester = test_requester(1);
        let request_id = "wrong-password-request".to_owned();
        runtime
            .store_pending_authorization(
                requester,
                request_id.clone(),
                PendingAuthorization {
                    client_id: "https://client.example/oauth.json".into(),
                    client_name: None,
                    issuer: server.issuer.clone(),
                    redirect_uri: "https://client.example/callback".into(),
                    resource: server.resource.clone(),
                    scope: SCOPE_MCP.into(),
                    state: None,
                    code_challenge: "a".repeat(43),
                    issue_refresh: false,
                    expires_at: now() + PENDING_AUTH_TTL_SECS,
                },
            )
            .unwrap();
        let post_requester = test_requester(2);
        let response = authorize_post(
            State(server),
            ConnectInfo(post_requester),
            Form(AuthorizeForm {
                request_id: request_id.clone(),
                password: "incorrect-owner-password".into(),
                decision: "allow".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let admission = runtime.inner.password_requesters.lock().unwrap();
        assert_eq!(admission.rows.get(&requester).unwrap().len(), 1);
        assert_eq!(admission.rows.get(&post_requester).unwrap().len(), 1);
        drop(admission);
        let pending = runtime.inner.pending.lock().unwrap();
        assert!(!pending.contains_key(&request_id));
        assert_eq!(pending.len(), 1);
        let retry = pending.values().next().unwrap();
        assert_eq!(retry.requester, requester);
        assert!(!retry.claimed);
    }

    #[tokio::test]
    async fn temporary_owner_storage_failure_preserves_retryable_consent() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let request_id = random_token().unwrap();
        let request = PendingAuthorization {
            client_id: "https://client.example/oauth.json".into(),
            client_name: None,
            issuer: server.issuer.clone(),
            redirect_uri: "https://client.example/callback".into(),
            resource: server.resource.clone(),
            scope: SCOPE_MCP.into(),
            state: None,
            code_challenge: "a".repeat(43),
            issue_refresh: false,
            expires_at: now() + PENDING_AUTH_TTL_SECS,
        };
        runtime
            .store_pending_authorization(test_requester(1), request_id.clone(), request)
            .unwrap();
        std::fs::write(&runtime.inner.state_path, b"broken JSON").unwrap();
        let form = || AuthorizeForm {
            request_id: request_id.clone(),
            password: "long-enough-owner-password".into(),
            decision: "allow".into(),
        };
        let response = authorize_post(
            State(server.clone()),
            ConnectInfo(test_requester(1)),
            Form(form()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            runtime
                .inner
                .pending
                .lock()
                .unwrap()
                .contains_key(&request_id)
        );
        // Restore the valid owner state, preserving the original password hash.
        let salt = SaltString::encode_b64(&[7u8; 16]).unwrap();
        let hash = Argon2::default()
            .hash_password(b"long-enough-owner-password", &salt)
            .unwrap()
            .to_string();
        write_state(
            &runtime.inner.state_path,
            &PersistentState {
                owner_password_hash: Some(hash),
                ..PersistentState::default()
            },
        )
        .unwrap();
        let response =
            authorize_post(State(server), ConnectInfo(test_requester(1)), Form(form())).await;
        assert!(
            response.status().is_redirection(),
            "status={}",
            response.status()
        );
        assert!(
            !runtime
                .inner
                .pending
                .lock()
                .unwrap()
                .contains_key(&request_id)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authorization_cleanup_failure_is_sanitized_server_error() {
        use axum::body::to_bytes;
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let request_id = random_token().unwrap();
        runtime
            .store_pending_authorization(
                test_requester(1),
                request_id.clone(),
                PendingAuthorization {
                    client_id: "https://client.example/oauth.json".into(),
                    client_name: None,
                    issuer: server.issuer.clone(),
                    redirect_uri: "https://client.example/callback".into(),
                    resource: server.resource.clone(),
                    scope: SCOPE_MCP.into(),
                    state: None,
                    code_challenge: "a".repeat(43),
                    issue_refresh: false,
                    expires_at: now() + PENDING_AUTH_TTL_SECS,
                },
            )
            .unwrap();
        let password_permits = runtime
            .inner
            .password_checks
            .clone()
            .acquire_many_owned(MAX_CONCURRENT_PASSWORD_CHECKS as u32)
            .await
            .unwrap();
        let post = {
            let server = server.clone();
            let request_id = request_id.clone();
            tokio::spawn(async move {
                authorize_post(
                    State(server),
                    ConnectInfo(test_requester(1)),
                    Form(AuthorizeForm {
                        request_id,
                        password: "long-enough-owner-password".into(),
                        decision: "allow".into(),
                    }),
                )
                .await
            })
        };
        for _ in 0..100 {
            let claimed = runtime
                .inner
                .pending
                .try_lock()
                .ok()
                .and_then(|pending| pending.get(&request_id).map(|slot| slot.claimed))
                .unwrap_or(false);
            if claimed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            runtime
                .inner
                .pending
                .lock()
                .unwrap()
                .get(&request_id)
                .is_some_and(|slot| slot.claimed)
        );
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = runtime.inner.pending.lock().unwrap();
            panic!("poison pending authorization state");
        }));
        assert!(poisoned.is_err());
        drop(password_permits);

        let response = post.await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("Authorization state is unavailable"));
        assert!(!body.contains("poison"));
        assert!(!body.contains(runtime.inner.state_path.to_str().unwrap()));
    }

    #[test]
    fn oauth_state_reader_bounds_malformed_files() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        std::fs::write(
            &runtime.inner.state_path,
            vec![b'a'; MAX_SERIALIZED_OAUTH_STATE_BYTES + 1],
        )
        .unwrap();
        let message = read_state(&runtime.inner.state_path)
            .err()
            .unwrap()
            .to_string();
        assert!(
            message.contains("at most") || message.contains("exceeds"),
            "{message}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bogus_refreshes_and_revokes_never_wait_on_busy_state_lock() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let lock = crate::private_state::acquire(
            &state_lock_path(&runtime.inner.state_path),
            Duration::from_secs(1),
        )
        .unwrap();
        let valid_shape = "a".repeat(43);
        for _ in 0..(MAX_CONCURRENT_TOKEN_EXCHANGES + MAX_CONCURRENT_REVOKES + 2) {
            let form = TokenForm {
                grant_type: "refresh_token".into(),
                code: None,
                redirect_uri: None,
                client_id: Some("client".into()),
                code_verifier: None,
                refresh_token: Some(valid_shape.clone()),
                resource: Some(server.resource.clone()),
            };
            let token_result = tokio::time::timeout(
                Duration::from_millis(100),
                token_with_slots(server.clone(), form, &SLOTS),
            )
            .await
            .expect("unknown refresh consumed a persistent lock");
            assert_eq!(token_result.status(), StatusCode::BAD_REQUEST);
            let revoke_result = tokio::time::timeout(
                Duration::from_millis(100),
                revoke(
                    State(server.clone()),
                    Form(RevokeForm {
                        token: valid_shape.clone(),
                    }),
                ),
            )
            .await
            .expect("unknown revoke consumed a persistent lock");
            assert_eq!(revoke_result.status(), StatusCode::OK);
        }
        drop(lock);
    }

    #[test]
    fn revoking_access_token_needs_no_persistent_state_lock() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let raw = random_token().unwrap();
        let capacity = runtime.reserve_access_token_capacity("client").unwrap();
        runtime.inner.access_tokens.lock().unwrap().insert(
            token_hash(&raw),
            AccessGrant {
                client_id: "client".into(),
                issuer: "https://example.com".into(),
                resource: "https://example.com/mcp".into(),
                scope: SCOPE_MCP.into(),
                expires_at: now() + ACCESS_TOKEN_TTL_SECS,
                _capacity: capacity,
            },
        );
        std::fs::write(&runtime.inner.state_path, b"broken JSON").unwrap();
        assert!(runtime.revoke(&raw).is_ok());
        assert!(runtime.inner.access_tokens.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_revocation_never_reads_broken_persistent_state() {
        let (runtime, _dir) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime.clone(),
            Url::parse("https://example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        std::fs::write(&runtime.inner.state_path, b"broken JSON").unwrap();
        let response = revoke(
            State(server),
            Form(RevokeForm {
                token: "malformed".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
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
            .insert_dcr_client(
                test_requester(1),
                RegistrationRequest {
                    client_name: Some("No refresh client".to_owned()),
                    redirect_uris: vec!["https://client.example/callback".to_owned()],
                    grant_types: vec!["authorization_code".to_owned()],
                    response_types: vec!["code".to_owned()],
                    token_endpoint_auth_method: Some("none".to_owned()),
                },
            )
            .unwrap();

        let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~abc";
        let error = prepare_authorization(
            &server,
            test_requester(1),
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
                .verify_owner_password(
                    test_requester(1),
                    test_requester(1),
                    "first-long-owner-password",
                )
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
                .verify_owner_password(
                    test_requester(1),
                    test_requester(1),
                    "first-long-owner-password",
                )
                .await
                .unwrap()
        );
        assert!(
            runtime
                .verify_owner_password(
                    test_requester(1),
                    test_requester(1),
                    "second-long-owner-password",
                )
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
                    .verify_owner_password(test_requester(1), test_requester(1), "wrong-password",)
                    .await
                    .unwrap()
            );
        }
        assert!(
            runtime
                .verify_owner_password(
                    test_requester(1),
                    test_requester(1),
                    "correct-long-owner-password",
                )
                .await
                .unwrap()
        );
    }

    #[test]
    fn poisoned_access_token_store_recovers_before_returning_success() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = runtime.inner.access_tokens.lock().unwrap();
            panic!("poison access-token state");
        }));
        assert!(poisoned.is_err());

        let issuer = "https://mcp.example.com";
        let resource = "https://mcp.example.com/mcp";
        let (access, refresh) = runtime
            .issue_access_and_refresh("client", issuer, resource, SCOPE_MCP, true)
            .unwrap();
        assert!(refresh.is_some());
        assert!(
            runtime
                .validate_access_token(&access, issuer, resource)
                .unwrap()
        );
        runtime.revoke(&access).unwrap();
        assert!(
            !runtime
                .validate_access_token(&access, issuer, resource)
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
    fn wrong_refresh_client_or_resource_cannot_consume_valid_grant() {
        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let client = "https://client.example/oauth.json";
        let issuer = "https://mcp.example.com";
        let resource = "https://mcp.example.com/mcp";
        let (_, token) = runtime
            .issue_access_and_refresh(client, issuer, resource, SCOPE_MCP, true)
            .unwrap();
        let token = token.unwrap();
        let hash = token_hash(&token);

        for (client_id, authority, target) in [
            ("https://wrong.example/oauth.json", issuer, resource),
            (client, "https://wrong.example", resource),
            (client, issuer, "https://wrong.example/mcp"),
        ] {
            assert!(
                runtime
                    .rotate_refresh(&token, client_id, authority, target)
                    .unwrap()
                    .is_none()
            );
            assert!(
                read_state(&runtime.inner.state_path)
                    .unwrap()
                    .refresh_tokens
                    .contains_key(&hash)
            );
        }
        assert!(
            runtime
                .rotate_refresh(&token, client, issuer, resource)
                .unwrap()
                .is_some()
        );
        assert!(
            !read_state(&runtime.inner.state_path)
                .unwrap()
                .refresh_tokens
                .contains_key(&hash)
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

        let router = server.router();
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .extension(ConnectInfo(test_requester(1)))
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
    async fn oauth_extractor_rejections_are_generic_and_no_store() {
        use axum::body::to_bytes;
        use axum::http::Method;
        use tower::ServiceExt;

        let (runtime, _temp) = test_runtime("long-enough-owner-password");
        let server = Server::new(
            runtime,
            Url::parse("https://mcp.example.com").unwrap(),
            "/mcp",
        )
        .unwrap();
        let router = server.router();
        for (method, uri, content_type, body) in [
            (
                Method::GET,
                "/oauth/authorize?response_type=code",
                "application/x-www-form-urlencoded",
                "",
            ),
            (
                Method::POST,
                "/oauth/authorize",
                "application/x-www-form-urlencoded",
                "",
            ),
            (
                Method::POST,
                "/oauth/token",
                "application/x-www-form-urlencoded",
                "",
            ),
            (
                Method::POST,
                "/oauth/revoke",
                "application/x-www-form-urlencoded",
                "",
            ),
            (Method::POST, "/oauth/register", "application/json", "{"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("content-type", content_type)
                        .extension(ConnectInfo(test_requester(1)))
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(
                response.headers().get(CACHE_CONTROL),
                Some(&HeaderValue::from_static("no-store")),
                "{uri}"
            );
            let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            let body = String::from_utf8_lossy(&bytes);
            assert!(!body.contains("missing field"), "{uri}: {body}");
            assert!(!body.contains("Failed to deserialize"), "{uri}: {body}");
            assert!(!body.contains("expected value"), "{uri}: {body}");
        }
    }

    #[tokio::test]
    async fn dcr_pkce_refresh_rotation_and_revocation_flow() {
        static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_TOKEN_EXCHANGES);
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
            .insert_dcr_client(
                test_requester(1),
                RegistrationRequest {
                    client_name: Some("Test client".to_owned()),
                    redirect_uris: vec![redirect_uri.clone()],
                    grant_types: vec!["authorization_code".to_owned(), "refresh_token".to_owned()],
                    response_types: vec!["code".to_owned()],
                    token_endpoint_auth_method: Some("none".to_owned()),
                },
            )
            .unwrap();
        assert!(runtime.inner.persistent.lock().unwrap().clients.is_empty());
        assert_eq!(runtime.inner.pending_clients.lock().unwrap().len(), 1);

        let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~abc";
        let challenge = pkce_challenge(verifier);
        let request_id = prepare_authorization(
            &server,
            test_requester(1),
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
            ConnectInfo(test_requester(1)),
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

        // A wrong verifier/client binding must not invalidate the valid code.
        assert!(
            exchange_authorization_code_with_slots(
                &server,
                TokenForm {
                    grant_type: "authorization_code".to_owned(),
                    code: Some(code.clone()),
                    redirect_uri: Some(redirect_uri.clone()),
                    client_id: Some(client.client_id.clone()),
                    code_verifier: Some(
                        "XabcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~abc"
                            .to_owned()
                    ),
                    refresh_token: None,
                    resource: Some(server.resource().to_owned()),
                },
                &SLOTS,
            )
            .await
            .is_err()
        );
        assert!(
            runtime
                .inner
                .authorization_codes
                .lock()
                .unwrap()
                .contains_key(&token_hash(&code))
        );

        let token_json = exchange_authorization_code_with_slots(
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
            &SLOTS,
        )
        .await
        .unwrap();
        let access = token_json["access_token"].as_str().unwrap().to_owned();
        let refresh = token_json["refresh_token"].as_str().unwrap().to_owned();
        assert!(
            runtime
                .validate_access_token(&access, server.issuer(), server.resource())
                .unwrap()
        );

        assert!(
            exchange_authorization_code_with_slots(
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
                &SLOTS,
            )
            .await
            .is_err()
        );

        let refreshed = exchange_refresh_token_with_slots(
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
            &SLOTS,
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
            exchange_refresh_token_with_slots(
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
                &SLOTS,
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
