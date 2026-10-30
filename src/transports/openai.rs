use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use http::{
    Request,
    header::{HeaderName, HeaderValue},
};
use http_body_util::Full;
use reqwest::{Client, StatusCode};
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::never::NeverSessionManager,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::{
    sync::{Semaphore, mpsc, watch},
    task::JoinSet,
    time,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    backoff::Backoff,
    bounded,
    logging::{self, LogConfig, LogOutcome},
    mcp::LocalMachine,
    recovery::{self, Recovery},
};

/// Headroom over the poll hold for the HTTP request timeout, so an idle
/// long-poll is never cut short by the client timeout.
const REQUEST_TIMEOUT_MARGIN: Duration = Duration::from_secs(10);
/// The control plane rejects larger poll batches with HTTP 400.
const CONTROL_PLANE_MAX_POLL_LIMIT: usize = 25;
const WIRE_PROTOCOL_VERSION: &str = "2026-08-25";
const INVALID_TRANSPORT_META: &str = "params._meta must be an object or null";
const SERVER_INFO: &str =
    r#"{"version":2,"channels":[{"name":"main","stateless":true,"proc_affinity":true}]}"#;
// The hosted tunnel imposes one serialized-message ceiling on poll commands
// and terminal responses. Keep this exact service limit visible in code instead
// of hiding a larger process-local allowance behind configuration math.
const MAX_POLL_RESPONSE_BYTES: usize = crate::limits::OPENAI_TUNNEL_SERIALIZED_MESSAGE_BYTES;

fn poll_response_ceiling() -> usize {
    MAX_POLL_RESPONSE_BYTES
}

pub(crate) fn canonical_base_url(value: &str) -> Result<String> {
    let mut url = url::Url::parse(value).context("control-plane base URL is invalid")?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("control-plane base URL must not contain credentials, query, or fragment");
    }

    let https = url.scheme() == "https" && url.host_str().is_some();
    let loopback_http = url.scheme() == "http"
        && match url.host() {
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
            Some(url::Host::Ipv6(ip)) => super::http::normalize_ip(ip.into()).is_loopback(),
            None => false,
        };
    if !https && !loopback_http {
        bail!("control-plane base URL must use HTTPS (loopback HTTP is allowed for testing)");
    }

    // URL parsing canonicalizes scheme/host spelling, default ports and dot
    // segments. Preserve an intentional base path, but make trailing slash
    // variants one runtime/replay identity and one URL-construction form.
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(if path.is_empty() { "/" } else { &path });
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

fn durable_request_key(namespace: &[u8; 32], request_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace);
    digest.update((request_id.len() as u64).to_be_bytes());
    digest.update(request_id.as_bytes());
    hex::encode(digest.finalize())
}

const REQUEST_HEADER_ALLOWLIST: &[&str] = &[
    "mcp-session-id",
    "mcp-protocol-version",
    "mcp-method",
    "mcp-name",
    "last-event-id",
];

const RESPONSE_HEADER_ALLOWLIST: &[&str] = &[
    "content-type",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
    "access-control-expose-headers",
    "www-authenticate",
];

/// Timing, retry and concurrency parameters of one tunnel runtime.
///
/// Built once, validated against its invariants, and shared behind an `Arc`.
/// The one thing kept outside is the client's own response window, which
/// arrives per command and can only be bounded from here by
/// `delivery_reserve`.
#[derive(Debug, Clone)]
pub(crate) struct TunnelTuning {
    /// Long-poll hold; the server releases it as soon as a command is ready.
    pub poll_hold: Duration,
    /// Commands requested per poll, kept between `max_in_flight` and both the
    /// queueing ratio and the control-plane maximum. A batch drains into tasks
    /// that queue on one semaphore while their client window keeps running, so
    /// a limit far above the capacity expires the tail.
    pub poll_limit: usize,
    /// Command tasks allowed to run concurrently.
    pub max_in_flight: usize,
    pub connect_timeout: Duration,
    /// Derived as `poll_hold + REQUEST_TIMEOUT_MARGIN`; must exceed `poll_hold`.
    pub request_timeout: Duration,
    /// Window in which a poll `404`/`403` only means "not activated yet".
    pub activation_grace: Duration,
    /// Pause between activation checks inside that window.
    pub activation_retry: Duration,
    /// Activation checks tolerated before the wait is treated as failed.
    pub activation_checks: u32,
    /// How long credential/tunnel rejections must persist before they count
    /// as permanent; one can come from an auth outage or an edge block.
    pub rejection_confirmation: Duration,
    /// Transient poll failures without an accepted poll before the runtime
    /// restarts. Rejections between them do not prove recovery.
    pub restart_after: u32,
    /// Failed response sends tolerated before the terminal response is dropped.
    pub response_attempts: u32,
    /// Retry schedule for poll failures and response posts. `cap + jitter`
    /// must stay within `request_timeout` so a retry cannot hold a task permit
    /// for an unbounded time.
    pub backoff: Backoff,
    /// Client window held back so the terminal response can still be posted.
    /// The window is client-supplied, so validation only rejects a reserve
    /// that would swallow the whole `request_timeout`.
    pub delivery_reserve: Duration,
}

impl TunnelTuning {
    /// Rejects combinations that break a load-bearing relation.
    pub(crate) fn validated(self) -> Result<Self> {
        if self.poll_hold.is_zero() {
            bail!("tunnel tuning: poll_hold must be non-zero");
        }
        // The margin is what keeps an idle long-poll from being cut short by the
        // HTTP timeout. Reqwest's request timeout also covers the connect
        // phase, so the margin must cover whichever is larger.
        let margin = self.connect_timeout.max(REQUEST_TIMEOUT_MARGIN);
        let required = self
            .poll_hold
            .checked_add(margin)
            .ok_or_else(|| anyhow!("tunnel tuning: poll_hold + margin overflows Duration"))?;
        if self.request_timeout < required {
            bail!(
                "tunnel tuning: request_timeout ({:?}) must be at least poll_hold + max(connect_timeout, {REQUEST_TIMEOUT_MARGIN:?}) ({required:?})",
                self.request_timeout
            );
        }
        if self.connect_timeout.is_zero() {
            bail!("tunnel tuning: connect_timeout must be non-zero");
        }
        if self.max_in_flight == 0 {
            bail!("tunnel tuning: max_in_flight must be non-zero");
        }
        if self.poll_limit < self.max_in_flight {
            bail!(
                "tunnel tuning: poll_limit ({}) must be at least max_in_flight ({})",
                self.poll_limit,
                self.max_in_flight
            );
        }
        let batch_ceiling = self.max_in_flight.saturating_mul(3);
        if self.poll_limit > batch_ceiling {
            bail!(
                "tunnel tuning: poll_limit ({}) must be at most 3 * max_in_flight ({batch_ceiling})",
                self.poll_limit
            );
        }
        if self.poll_limit > CONTROL_PLANE_MAX_POLL_LIMIT {
            bail!(
                "tunnel tuning: poll_limit ({}) must not exceed the control-plane maximum ({CONTROL_PLANE_MAX_POLL_LIMIT})",
                self.poll_limit
            );
        }
        if self.activation_retry.is_zero() {
            bail!("tunnel tuning: activation_retry must be non-zero");
        }
        if self.activation_checks == 0 {
            bail!("tunnel tuning: activation_checks must be non-zero");
        }
        if self.rejection_confirmation.is_zero() {
            bail!("tunnel tuning: rejection_confirmation must be non-zero");
        }
        let activation_window = self.activation_retry.saturating_mul(self.activation_checks);
        if self.activation_grace < activation_window {
            bail!(
                "tunnel tuning: activation_grace ({:?}) must cover activation_retry * activation_checks ({activation_window:?})",
                self.activation_grace
            );
        }
        if self.restart_after == 0 {
            bail!("tunnel tuning: restart_after must be non-zero");
        }
        if self.response_attempts == 0 {
            bail!("tunnel tuning: response_attempts must be non-zero");
        }
        if self.backoff.base.is_zero() {
            bail!("tunnel tuning: backoff.base must be non-zero");
        }
        if self.backoff.cap < self.backoff.base {
            bail!("tunnel tuning: backoff.cap must be at least backoff.base");
        }
        let max_retry = self.backoff.cap.saturating_add(self.backoff.jitter);
        if max_retry > self.request_timeout {
            bail!(
                "tunnel tuning: backoff.cap + jitter ({max_retry:?}) must not exceed request_timeout ({:?})",
                self.request_timeout
            );
        }
        if self.delivery_reserve >= self.request_timeout {
            bail!("tunnel tuning: delivery_reserve must stay below request_timeout");
        }
        Ok(self)
    }
}

impl Default for TunnelTuning {
    fn default() -> Self {
        let poll_hold = Duration::from_secs(30);
        let activation_retry = Duration::from_secs(2);
        let activation_checks: u32 = 30;
        Self {
            poll_hold,
            poll_limit: CONTROL_PLANE_MAX_POLL_LIMIT,
            max_in_flight: 16,
            connect_timeout: Duration::from_secs(10),
            request_timeout: poll_hold + REQUEST_TIMEOUT_MARGIN,
            // One extra interval so the last check is not raced by the grace
            // deadline expiring before it is attempted.
            activation_grace: activation_retry.saturating_mul(activation_checks + 1),
            activation_retry,
            activation_checks,
            rejection_confirmation: Duration::from_secs(60),
            restart_after: 10,
            response_attempts: 6,
            backoff: Backoff {
                base: Duration::from_millis(250),
                cap: Duration::from_secs(5),
                jitter: Duration::from_millis(250),
            },
            delivery_reserve: crate::limits::OPENAI_TUNNEL_DELIVERY_RESERVE,
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub base_url: String,
    pub tunnel_id: String,
    pub runtime_api_key: String,
    pub organization_id: Option<String>,
    pub new_tunnel: bool,
    /// Prepared execution-authority generation for replayable response bodies.
    /// Planning leaves this unset; transport construction fails closed until the
    /// PreparedMachine fingerprint has been installed.
    pub(crate) execution_authority: Option<[u8; 32]>,
    pub log: LogConfig,
    /// Process-wide replay/rejection state; shared by every runtime the process builds.
    pub process_state: ProcessState,
}

pub async fn run(
    machine: LocalMachine,
    config: Config,
    cancellation: CancellationToken,
) -> Result<()> {
    run_with_tuning(machine, config, TunnelTuning::default(), cancellation).await
}

async fn run_with_tuning(
    machine: LocalMachine,
    config: Config,
    tuning: TunnelTuning,
    cancellation: CancellationToken,
) -> Result<()> {
    // Construction depends only on configuration.
    let poll_ceiling = poll_response_ceiling();
    let embedded = EmbeddedMcp::new(machine, cancellation.child_token());
    let tunnel =
        TunnelClient::new(config, tuning, cancellation).map_err(recovery::config_failure)?;

    match tunnel.run(embedded, poll_ceiling).await? {
        TunnelRunOutcome::Cancelled => Ok(()),
        TunnelRunOutcome::RestartRuntime {
            error,
            had_success,
            threshold,
        } => Err(Recovery::Runtime {
            reset_backoff: had_success,
        }
        .tag(anyhow!(
            "OpenAI tunnel hit {threshold} transient poll failures without a successful poll: {error:#}"
        ))),
    }
}

#[derive(Clone)]
pub(crate) struct EmbeddedMcp {
    machine: LocalMachine,
    service: StreamableHttpService<LocalMachine, NeverSessionManager>,
    max_request_bytes: usize,
    max_response_bytes: usize,
}

#[derive(Debug)]
struct LocalResponse {
    status: u16,
    headers: BTreeMap<String, Vec<String>>,
    messages: Vec<Value>,
}

/// Never reflect a caller-supplied oversized or malformed JSON-RPC id into
/// synthesized transport errors.
fn bounded_jsonrpc_id(id: &Value) -> Value {
    match id {
        Value::String(value) if value.len() > super::MAX_STANDARD_ROUTING_VALUE_BYTES => {
            Value::Null
        }
        Value::String(_) | Value::Number(_) | Value::Null => id.clone(),
        _ => Value::Null,
    }
}

fn oversized_routing_response(request: &Value) -> LocalResponse {
    let Some(raw_id) = request.get("id") else {
        return LocalResponse {
            status: StatusCode::ACCEPTED.as_u16(),
            headers: BTreeMap::new(),
            messages: Vec::new(),
        };
    };
    let id = bounded_jsonrpc_id(raw_id);
    LocalResponse {
        status: StatusCode::BAD_REQUEST.as_u16(),
        headers: BTreeMap::from([(
            "Content-Type".to_owned(),
            vec!["application/json".to_owned()],
        )]),
        messages: vec![json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32602,
                "message": format!(
                    "MCP routing value exceeds the {} byte limit",
                    super::MAX_STANDARD_ROUTING_VALUE_BYTES
                )
            }
        })],
    }
}

impl EmbeddedMcp {
    pub(crate) fn new(machine: LocalMachine, cancellation: CancellationToken) -> Self {
        let max_request_bytes = machine
            .max_request_body_bytes()
            .min(crate::limits::OPENAI_TUNNEL_MCP_MESSAGE_BYTES);
        let max_response_bytes = machine
            .max_response_body_bytes()
            .min(crate::limits::OPENAI_TUNNEL_MCP_MESSAGE_BYTES);
        let config = super::embedded_streamable_config(&machine, cancellation)
            .with_max_request_body_bytes(max_request_bytes);
        let service_machine = machine.clone();
        let service = StreamableHttpService::new(
            move || Ok(service_machine.clone()),
            NeverSessionManager::default().into(),
            config,
        );
        Self {
            machine,
            service,
            max_request_bytes,
            max_response_bytes,
        }
    }

    // A convenience collector for tests; the runtime uses dispatch_stream
    // below to forward progress as it is emitted, not after the tool returns.
    #[cfg(test)]
    async fn dispatch(
        &self,
        command_headers: &BTreeMap<String, Vec<String>>,
        jsonrpc: &Value,
    ) -> Result<LocalResponse> {
        let (tx, mut rx) = mpsc::channel(16);
        let dispatch = self.dispatch_stream(command_headers, jsonrpc, tx);
        tokio::pin!(dispatch);
        let mut events = Vec::new();
        let mut local = loop {
            tokio::select! {
                result = &mut dispatch => break result?,
                Some(event) = rx.recv() => events.push(event),
            }
        };
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events.append(&mut local.messages);
        local.messages = events;
        Ok(local)
    }

    async fn dispatch_stream(
        &self,
        command_headers: &BTreeMap<String, Vec<String>>,
        jsonrpc: &Value,
        notifications: mpsc::Sender<Value>,
    ) -> Result<LocalResponse> {
        let routing = super::RoutingPreflight::for_request(jsonrpc, &self.machine);
        if routing.rejects_request() {
            return Ok(oversized_routing_response(jsonrpc));
        }
        let mut request = Request::builder()
            .method("POST")
            .uri("http://localhost/mcp")
            .header("Host", "localhost")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        for (_, header) in effective_forwarded_headers(command_headers) {
            for value in header.values {
                request = request.header(header.name.clone(), value);
            }
        }

        let body_source = jsonrpc;
        let body = serde_json::to_vec(body_source).context("failed to serialize MCP command")?;
        let body_len = body.len();
        let mut request = request
            .body(Full::new(Bytes::from(body)))
            .context("failed to build in-process MCP request")?;
        if body_len <= self.max_request_bytes
            && super::unknown_standard_tool_value(request.headers(), body_source, &self.machine)
        {
            // Only a bounded unknown tool needs a second owned JSON tree. Known
            // tools and oversized routing values stay zero-copy at this layer.
            let mut message = body_source.clone();
            if super::canonicalize_unknown_standard_tool(
                request.headers_mut(),
                &mut message,
                &self.machine,
            ) {
                *request.body_mut() = Full::new(Bytes::from(
                    serde_json::to_vec(&message)
                        .context("failed to canonicalize unknown MCP tool request")?,
                ));
            }
        }
        let response = self.service.handle(request).await;
        let status = response.status().as_u16();
        let response_headers = filtered_response_headers(response.headers());
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        let messages = if content_type.contains("text/event-stream") {
            use http_body_util::BodyExt as _;

            let mut stream = response.into_body();
            let mut parser = SseDecoder::default();
            let mut total = 0_usize;
            let mut terminal = Vec::new();
            while let Some(frame) = stream.frame().await {
                let frame = frame.context("failed reading local MCP stream")?;
                let Some(bytes) = frame.data_ref() else {
                    continue;
                };
                total = total.saturating_add(bytes.len());
                if total > self.max_response_bytes {
                    bail!(
                        "local MCP response exceeded {} bytes",
                        self.max_response_bytes
                    );
                }
                for event in parser.feed(bytes)? {
                    if is_jsonrpc_notification(&event) {
                        notifications
                            .send(event)
                            .await
                            .map_err(|_| anyhow!("MCP progress receiver closed"))?;
                    } else {
                        terminal.push(event);
                    }
                }
            }
            for event in parser.finish()? {
                if is_jsonrpc_notification(&event) {
                    notifications
                        .send(event)
                        .await
                        .map_err(|_| anyhow!("MCP progress receiver closed"))?;
                } else {
                    terminal.push(event);
                }
            }
            terminal
        } else {
            let bytes = bounded::local_response(
                response.into_body(),
                self.max_response_bytes,
                "local MCP response",
            )
            .await?;
            if bytes.is_empty() {
                Vec::new()
            } else if content_type.contains("application/json") || looks_like_json(&bytes) {
                vec![serde_json::from_slice(&bytes).context("local MCP returned invalid JSON")?]
            } else {
                bail!("local MCP returned unsupported Content-Type {content_type:?}");
            }
        };

        Ok(LocalResponse {
            status,
            headers: response_headers,
            messages,
        })
    }
}

fn request_header_allowed(name: &str) -> bool {
    REQUEST_HEADER_ALLOWLIST.contains(&name) || name.starts_with("mcp-param-")
}

struct EffectiveForwardedHeader {
    name: HeaderName,
    values: Vec<HeaderValue>,
}

fn effective_forwarded_headers(
    headers: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, EffectiveForwardedHeader> {
    let mut effective = BTreeMap::new();
    for (name, values) in headers {
        if name.len() > 256 || values.is_empty() {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if !request_header_allowed(&lower) {
            continue;
        }
        let Ok(header_name) = HeaderName::from_bytes(lower.as_bytes()) else {
            continue;
        };
        let mut parsed = Vec::with_capacity(values.len());
        let mut valid = true;
        for value in values {
            if value.len() > super::MAX_STANDARD_ROUTING_VALUE_BYTES {
                valid = false;
                break;
            }
            match HeaderValue::from_str(value) {
                Ok(value) => parsed.push(value),
                Err(_) => {
                    valid = false;
                    break;
                }
            }
        }
        if !valid {
            continue;
        }
        effective
            .entry(lower)
            .and_modify(|header: &mut EffectiveForwardedHeader| {
                header.values.extend(parsed.iter().cloned());
            })
            .or_insert(EffectiveForwardedHeader {
                name: header_name,
                values: parsed,
            });
    }
    effective
}

fn filtered_response_headers(headers: &http::HeaderMap) -> BTreeMap<String, Vec<String>> {
    let mut output = BTreeMap::new();
    for &name in RESPONSE_HEADER_ALLOWLIST {
        let values: Vec<String> = headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if !values.is_empty() {
            output.insert(canonical_response_header(name).to_owned(), values);
        }
    }
    output
}

fn canonical_response_header(name: &str) -> &str {
    match name {
        "content-type" => "Content-Type",
        "mcp-session-id" => "Mcp-Session-Id",
        "mcp-protocol-version" => "Mcp-Protocol-Version",
        "last-event-id" => "Last-Event-ID",
        "access-control-expose-headers" => "Access-Control-Expose-Headers",
        "www-authenticate" => "WWW-Authenticate",
        _ => name,
    }
}

fn looks_like_json(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .copied()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| b == b'{' || b == b'[')
}

/// Incremental SSE decoding: a progress event becomes available as soon as
/// its blank-line delimiter arrives, independent of the terminal JSON-RPC reply.
/// Overall memory is bounded by the caller's stream byte ceiling.
#[derive(Default)]
struct SseDecoder {
    line: Vec<u8>,
    data: Vec<String>,
    after_cr: bool,
}

impl SseDecoder {
    fn flush(&mut self, events: &mut Vec<Value>) -> Result<()> {
        if self.data.is_empty() {
            return Ok(());
        }
        let payload = self.data.join("\n");
        self.data.clear();
        if payload.trim().is_empty() {
            return Ok(());
        }
        events.push(serde_json::from_str(&payload).context("invalid JSON in local MCP SSE event")?);
        Ok(())
    }

    fn finish_line(&mut self, events: &mut Vec<Value>) -> Result<()> {
        let line = std::str::from_utf8(&self.line).context("local MCP returned non-UTF-8 SSE")?;
        if line.is_empty() {
            self.flush(events)?;
        } else if let Some(data) = line.strip_prefix("data:") {
            self.data
                .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
        }
        self.line.clear();
        Ok(())
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Value>> {
        let mut events = Vec::new();
        for &byte in chunk {
            match byte {
                b'\r' => {
                    self.finish_line(&mut events)?;
                    self.after_cr = true;
                }
                b'\n' => {
                    if !self.after_cr {
                        self.finish_line(&mut events)?;
                    }
                    self.after_cr = false;
                }
                byte => {
                    self.after_cr = false;
                    self.line.push(byte);
                }
            }
        }
        Ok(events)
    }

    fn finish(mut self) -> Result<Vec<Value>> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            self.finish_line(&mut events)?;
        }
        self.flush(&mut events)?;
        Ok(events)
    }
}

#[cfg(test)]
fn parse_sse_messages(bytes: &[u8]) -> Result<Vec<Value>> {
    let mut parser = SseDecoder::default();
    let mut events = parser.feed(bytes)?;
    events.extend(parser.finish()?);
    Ok(events)
}

#[derive(Clone)]
pub(crate) struct TunnelClient {
    http: Client,
    base_url: Arc<str>,
    tunnel_id: Arc<str>,
    api_key: Arc<str>,
    organization_id: Option<Arc<str>>,
    instance_id: Arc<str>,
    activation_grace_until: Option<Instant>,
    log: LogConfig,
    cancellation: CancellationToken,
    concurrency: Arc<Semaphore>,
    tuning: Arc<TunnelTuning>,
    identity: [u8; 32],
    /// Stable logical request namespace: endpoint + tunnel + organization, but
    /// deliberately not the credential so API-key rotation cannot make a late
    /// redelivery look like a new side effect.
    request_namespace: [u8; 32],
    execution_authority: [u8; 32],
    rejections: RejectionLedger,
    replays: RequestReplayLedger,
}

/// What OpenAI accepts or rejects: endpoint, tunnel, key and organization.
/// Only a digest is kept; length prefixes keep the fields apart.
fn tunnel_identity(fields: [Option<&str>; 4]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for field in fields {
        match field {
            Some(value) => {
                digest.update([1]);
                digest.update((value.len() as u64).to_le_bytes());
                digest.update(value);
            }
            None => digest.update([0]),
        }
    }
    digest.finalize().into()
}

#[derive(Debug, Deserialize)]
struct PollEnvelope {
    #[serde(default)]
    commands: Vec<TunnelCommand>,
}

#[derive(Debug, Clone, Deserialize)]
struct TunnelCommand {
    request_id: String,
    shard_token: String,
    command_type: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    response_timeout: Option<Value>,
    #[serde(default)]
    headers: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    jsonrpc: Option<Value>,
}

impl TunnelCommand {
    fn request_id_label(&self) -> &str {
        logging::protocol_identifier(Some(&self.request_id), "invalid-request-id")
    }

    fn channel_label(&self) -> &str {
        logging::protocol_identifier(
            Some(self.channel.as_deref().unwrap_or("main")),
            "invalid-channel",
        )
    }

    fn command_type_label(&self) -> &str {
        logging::protocol_identifier(Some(&self.command_type), "invalid-command-type")
    }
}

#[derive(Clone, Debug, Serialize)]
struct TunnelResponse {
    request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resp_json: Option<Value>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    resp_headers: BTreeMap<String, Vec<String>>,
    resp_code: u16,
    resp_type: String,
}

fn serialize_tunnel_response(response: &TunnelResponse) -> Result<Bytes> {
    Ok(Bytes::from(
        serde_json::to_vec(response).context("failed to serialize tunnel response")?,
    ))
}

#[derive(Debug)]
struct PolledBatch {
    received_at: Instant,
    commands: Vec<TunnelCommand>,
}

enum PollFailure {
    /// Retry with backoff; enough without an accepted poll restart the runtime.
    Transient(anyhow::Error),
    /// A new tunnel is not activated yet.
    NotReady(anyhow::Error),
    /// The key or tunnel was rejected; permanent once it persists.
    Rejected(anyhow::Error),
    /// Stop the transport; the process may restart.
    Fatal(anyhow::Error),
}

/// Unresolved key or tunnel rejections, kept for the whole process. An
/// in-process runtime restart proves nothing about the credential, so only an
/// accepted poll or a different tunnel identity (endpoint, tunnel, key,
/// organization) starts a new confirmation window. Without this, recurring
/// 5xx restarts would reset the window and evade the operator threshold.
#[derive(Clone, Default)]
pub struct RejectionLedger(Arc<std::sync::Mutex<Option<Rejections>>>);

/// Rejections of one tunnel identity since its last accepted poll.
#[derive(Debug)]
struct Rejections {
    identity: [u8; 32],
    since: Instant,
    attempts: u32,
}

/// What [`RejectionLedger::record`] observed.
#[derive(Debug, PartialEq)]
struct Rejection {
    attempts: u32,
    /// The rejection state has persisted for the whole confirmation window.
    confirmed: bool,
}

impl RejectionLedger {
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Rejections>> {
        // Plain data with no invariant spanning a panic.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record one rejection of `identity`. Transient statuses in between do
    /// not prove that it recovered.
    fn record(&self, identity: [u8; 32], window: Duration) -> Rejection {
        let mut slot = self.slot();
        let rejections = match slot.take() {
            Some(rejections) if rejections.identity == identity => rejections,
            _ => Rejections {
                identity,
                since: Instant::now(),
                attempts: 0,
            },
        };
        let rejections = slot.insert(Rejections {
            attempts: rejections.attempts.saturating_add(1),
            ..rejections
        });
        Rejection {
            attempts: rejections.attempts,
            confirmed: rejections.since.elapsed() >= window,
        }
    }

    /// An accepted poll of `identity` resolves its rejections.
    fn accept(&self, identity: [u8; 32]) {
        let mut slot = self.slot();
        if slot
            .as_ref()
            .is_some_and(|rejections| rejections.identity == identity)
        {
            *slot = None;
        }
    }
}

/// Process-lifetime OpenAI state. Rejection history and request replay survive
/// an in-process transport/runtime rebuild. Replay scope is retired explicitly
/// when endpoint/tunnel/organization or prepared execution authority changes.
#[derive(Clone, Default)]
pub struct ProcessState {
    rejections: RejectionLedger,
    replays: RequestReplayLedger,
}

impl ProcessState {
    fn activate_scope(&self, namespace: [u8; 32], authority: [u8; 32]) {
        self.replays.activate_scope(namespace, authority);
    }
}

/// Delivered tombstones are tiny but must cover the full hosted redelivery
/// horizon. Replayable terminal bodies have the much tighter byte bound below.
const MAX_REQUEST_REPLAY_ENTRIES: usize = 65_536;
const MAX_REQUEST_REPLAY_BYTES: usize = 256 * 1024 * 1024;
const REQUEST_REPLAY_RESERVATION_BYTES: usize =
    crate::limits::OPENAI_TUNNEL_SERIALIZED_MESSAGE_BYTES;
const REQUEST_REPLAY_RETENTION: Duration = crate::limits::OPENAI_TUNNEL_REQUEST_DEADLINE;
const MAX_SHELL_RETIREMENTS_PER_PRUNE: usize = 16;

#[derive(Clone, Default)]
struct RequestReplayLedger(Arc<Mutex<RequestReplayState>>);

#[derive(Default)]
struct RequestReplayState {
    entries: HashMap<String, RequestReplayRecord>,
    reserved_bytes: usize,
    expirations: VecDeque<ReplayExpiration>,
    shell_retirements: VecDeque<String>,
    queued_shell_retirements: HashSet<String>,
    active_namespace: Option<[u8; 32]>,
    active_authority: Option<[u8; 32]>,
}

struct RequestReplayRecord {
    namespace: [u8; 32],
    authority: [u8; 32],
    fingerprint: [u8; 32],
    state: RequestReplayEntry,
    delivery_leases: usize,
    deferred_expiry: Option<ReplayExpirationKind>,
}

#[derive(Clone)]
struct ReplayTerminal {
    response: TunnelResponse,
    finalize_job_delivery: bool,
    requires_job_owner: bool,
    /// Exact dynamic Linux sandbox guard generation used to produce synchronous
    /// shell output. None for non-shell, durable receipts and unsandboxed shells.
    shell_guard_generation: Option<[u8; 32]>,
    /// This response reports a retryable outer delivery/admission failure for an
    /// already-owned shell execution. Delivering it must not retire the inner key.
    recoverable_shell: bool,
}

#[derive(Clone)]
struct CachedReplayTerminal {
    body: Bytes,
    finalize_job_delivery: bool,
    requires_job_owner: bool,
    shell_guard_generation: Option<[u8; 32]>,
    /// Once the large cached body ages out, a committed shell response can be
    /// reconstructed from the still-owned JobManager request instead of becoming
    /// permanently uncertain.
    recover_after_body_expiry: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplayExpirationKind {
    DropReadyBody,
    ExpireIdentity,
    RemoveTombstone,
}

struct ReplayExpiration {
    until: Instant,
    key: String,
    kind: ReplayExpirationKind,
}

enum RequestReplayEntry {
    InFlight {
        signal: watch::Sender<()>,
    },
    /// The outer response attempt disappeared, but the current JobManager may
    /// still own the exact durable shell request. After one additional hosted
    /// replay horizon this becomes an explicit inner-job retirement handoff.
    RecoverableShell {
        until: Instant,
    },
    /// Execution may have committed and no replay-safe owner can prove otherwise.
    /// Keep the fail-closed identity for one additional hosted replay horizon.
    Uncertain {
        until: Instant,
    },
    /// The supported replay horizon ended; the inner shell request must be
    /// retired before this outer identity can be removed or reused.
    RetireShell,
    Ready {
        terminal: Arc<CachedReplayTerminal>,
        reserved_bytes: usize,
        body_until: Instant,
    },
    Delivered {
        until: Instant,
    },
    /// The logical tunnel namespace was deactivated. Retain a short fail-closed
    /// tombstone for the old hosted request horizon, but release response bytes.
    Retired {
        until: Instant,
    },
}

enum RequestReplayClaim {
    Leader,
    Wait(watch::Receiver<()>),
    RecoverableShell,
    Uncertain(Option<ReplayDeliveryGuard>),
    RetireShell,
    Ready(Arc<CachedReplayTerminal>, ReplayDeliveryGuard),
    Delivered,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplayDeliveryKind {
    Ready,
    Uncertain,
}

struct ReplayDeliveryGuard {
    ledger: RequestReplayLedger,
    key: String,
    namespace: [u8; 32],
    authority: [u8; 32],
    fingerprint: [u8; 32],
    kind: ReplayDeliveryKind,
}

impl ReplayDeliveryGuard {
    fn new(
        ledger: RequestReplayLedger,
        key: &str,
        namespace: [u8; 32],
        authority: [u8; 32],
        fingerprint: [u8; 32],
        kind: ReplayDeliveryKind,
    ) -> Self {
        Self {
            ledger,
            key: key.to_owned(),
            namespace,
            authority,
            fingerprint,
            kind,
        }
    }
}

impl Drop for ReplayDeliveryGuard {
    fn drop(&mut self) {
        self.ledger.release_delivery(
            &self.key,
            self.namespace,
            self.authority,
            self.fingerprint,
            self.kind,
        );
    }
}

struct ReplayRequestContext<'a> {
    command: &'a TunnelCommand,
    jsonrpc: &'a Value,
    request_key: &'a str,
    namespace: [u8; 32],
    fingerprint: [u8; 32],
    dispatch_deadline: Option<Instant>,
    deadline: Option<Instant>,
}

struct ReplayLeaderGuard {
    ledger: RequestReplayLedger,
    key: String,
    namespace: [u8; 32],
    fingerprint: [u8; 32],
    retryable_by_inner_owner: bool,
    armed: bool,
}

impl ReplayLeaderGuard {
    fn new(
        ledger: RequestReplayLedger,
        key: &str,
        namespace: [u8; 32],
        fingerprint: [u8; 32],
        retryable_by_inner_owner: bool,
    ) -> Self {
        Self {
            ledger,
            key: key.to_owned(),
            namespace,
            fingerprint,
            retryable_by_inner_owner,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ReplayLeaderGuard {
    fn drop(&mut self) {
        if self.armed {
            self.ledger.abandon(
                &self.key,
                self.namespace,
                self.fingerprint,
                self.retryable_by_inner_owner,
            );
        }
    }
}

impl RequestReplayLedger {
    fn slot(&self) -> std::sync::MutexGuard<'_, RequestReplayState> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn same_identity(
        record: &RequestReplayRecord,
        namespace: [u8; 32],
        fingerprint: [u8; 32],
    ) -> bool {
        record.namespace == namespace && record.fingerprint == fingerprint
    }

    fn state_bytes(state: &RequestReplayEntry) -> usize {
        match state {
            RequestReplayEntry::InFlight { .. } => REQUEST_REPLAY_RESERVATION_BYTES,
            RequestReplayEntry::Ready { reserved_bytes, .. } => *reserved_bytes,
            RequestReplayEntry::RecoverableShell { .. }
            | RequestReplayEntry::Uncertain { .. }
            | RequestReplayEntry::RetireShell
            | RequestReplayEntry::Delivered { .. }
            | RequestReplayEntry::Retired { .. } => 0,
        }
    }

    fn identity_until(now: Instant) -> Instant {
        now.checked_add(REQUEST_REPLAY_RETENTION).unwrap_or(now)
    }

    fn apply_expiration(
        state: &mut RequestReplayState,
        expiration: ReplayExpiration,
        now: Instant,
    ) {
        match expiration.kind {
            ReplayExpirationKind::RemoveTombstone => {
                let remove = state.entries.get(&expiration.key).is_some_and(|record| {
                    matches!(
                        record.state,
                        RequestReplayEntry::Delivered { until }
                            | RequestReplayEntry::Retired { until }
                            if until == expiration.until
                    )
                });
                if remove {
                    state.entries.remove(&expiration.key);
                }
            }
            ReplayExpirationKind::DropReadyBody => {
                let transition = {
                    let Some(record) = state.entries.get_mut(&expiration.key) else {
                        return;
                    };
                    let RequestReplayEntry::Ready {
                        terminal,
                        reserved_bytes,
                        body_until,
                    } = &record.state
                    else {
                        return;
                    };
                    if *body_until != expiration.until {
                        return;
                    }
                    if record.delivery_leases != 0 {
                        record.deferred_expiry = Some(ReplayExpirationKind::DropReadyBody);
                        return;
                    }
                    Some((*reserved_bytes, terminal.recover_after_body_expiry))
                };
                if let Some((released, recoverable_shell)) = transition {
                    state.reserved_bytes = state.reserved_bytes.saturating_sub(released);
                    let until = Self::identity_until(now);
                    if let Some(record) = state.entries.get_mut(&expiration.key) {
                        record.state = if recoverable_shell {
                            RequestReplayEntry::RecoverableShell { until }
                        } else {
                            RequestReplayEntry::Uncertain { until }
                        };
                        record.deferred_expiry = None;
                    }
                    state.expirations.push_back(ReplayExpiration {
                        until,
                        key: expiration.key,
                        kind: ReplayExpirationKind::ExpireIdentity,
                    });
                }
            }
            ReplayExpirationKind::ExpireIdentity => {
                enum ExpiryAction {
                    Remove,
                    RetireShell,
                }
                let action = {
                    let Some(record) = state.entries.get_mut(&expiration.key) else {
                        return;
                    };
                    match record.state {
                        RequestReplayEntry::Uncertain { until } if until == expiration.until => {
                            if record.delivery_leases != 0 {
                                record.deferred_expiry = Some(ReplayExpirationKind::ExpireIdentity);
                                return;
                            }
                            Some(ExpiryAction::Remove)
                        }
                        RequestReplayEntry::RecoverableShell { until }
                            if until == expiration.until =>
                        {
                            Some(ExpiryAction::RetireShell)
                        }
                        _ => None,
                    }
                };
                match action {
                    Some(ExpiryAction::Remove) => {
                        state.entries.remove(&expiration.key);
                    }
                    Some(ExpiryAction::RetireShell) => {
                        if let Some(record) = state.entries.get_mut(&expiration.key) {
                            record.state = RequestReplayEntry::RetireShell;
                            record.deferred_expiry = None;
                        }
                        let key = expiration.key;
                        if state.queued_shell_retirements.insert(key.clone()) {
                            state.shell_retirements.push_back(key);
                        }
                    }
                    None => {}
                }
            }
        }
    }

    fn prune_expired(state: &mut RequestReplayState, now: Instant) {
        while state
            .expirations
            .front()
            .is_some_and(|expiration| expiration.until <= now)
        {
            let expiration = state.expirations.pop_front().expect("front checked above");
            Self::apply_expiration(state, expiration, now);
        }
    }

    fn release_delivery(
        &self,
        key: &str,
        namespace: [u8; 32],
        authority: [u8; 32],
        fingerprint: [u8; 32],
        kind: ReplayDeliveryKind,
    ) {
        let mut state = self.slot();
        let deferred = {
            let Some(record) = state.entries.get_mut(key) else {
                return;
            };
            if record.namespace != namespace
                || record.authority != authority
                || record.fingerprint != fingerprint
                || record.delivery_leases == 0
            {
                return;
            }
            let state_matches = matches!(
                (kind, &record.state),
                (ReplayDeliveryKind::Ready, RequestReplayEntry::Ready { .. })
                    | (
                        ReplayDeliveryKind::Uncertain,
                        RequestReplayEntry::Uncertain { .. }
                    )
            );
            if !state_matches {
                return;
            }
            record.delivery_leases -= 1;
            if record.delivery_leases != 0 {
                return;
            }
            record.deferred_expiry.take()
        };
        let Some(kind) = deferred else {
            return;
        };
        let until = match state.entries.get(key).map(|record| &record.state) {
            Some(RequestReplayEntry::Ready { body_until, .. })
                if kind == ReplayExpirationKind::DropReadyBody =>
            {
                *body_until
            }
            Some(RequestReplayEntry::Uncertain { until })
                if kind == ReplayExpirationKind::ExpireIdentity =>
            {
                *until
            }
            _ => return,
        };
        Self::apply_expiration(
            &mut state,
            ReplayExpiration {
                until,
                key: key.to_owned(),
                kind,
            },
            Instant::now(),
        );
    }

    fn prune(&self) -> Vec<String> {
        let mut state = self.slot();
        Self::prune_expired(&mut state, Instant::now());
        let mut retirements =
            Vec::with_capacity(MAX_SHELL_RETIREMENTS_PER_PRUNE.min(state.shell_retirements.len()));
        let mut scanned = 0;
        while scanned < MAX_SHELL_RETIREMENTS_PER_PRUNE {
            let Some(key) = state.shell_retirements.pop_front() else {
                break;
            };
            scanned += 1;
            state.queued_shell_retirements.remove(&key);
            if state
                .entries
                .get(&key)
                .is_some_and(|record| matches!(record.state, RequestReplayEntry::RetireShell))
            {
                retirements.push(key);
            }
        }
        retirements
    }

    fn finish_shell_retirement(&self, key: &str) {
        let mut state = self.slot();
        state.queued_shell_retirements.remove(key);
        if state
            .entries
            .get(key)
            .is_some_and(|record| matches!(record.state, RequestReplayEntry::RetireShell))
        {
            // The inner execution is now user-discoverable and no longer owns
            // the transport request key, but the hosted request identity must
            // remain fail-closed for one final replay horizon. Removing it here
            // would let the same redelivery immediately claim a fresh leader.
            let until = Self::identity_until(Instant::now());
            let record = state.entries.get_mut(key).expect("record checked above");
            record.state = RequestReplayEntry::Uncertain { until };
            record.delivery_leases = 0;
            record.deferred_expiry = None;
            state.expirations.push_back(ReplayExpiration {
                until,
                key: key.to_owned(),
                kind: ReplayExpirationKind::ExpireIdentity,
            });
        }
    }

    fn defer_shell_retirement(&self, key: &str) {
        let mut state = self.slot();
        if state
            .entries
            .get(key)
            .is_some_and(|record| matches!(record.state, RequestReplayEntry::RetireShell))
            && state.queued_shell_retirements.insert(key.to_owned())
        {
            state.shell_retirements.push_back(key.to_owned());
        }
    }

    fn activate_scope(&self, namespace: [u8; 32], authority: [u8; 32]) {
        let signals = {
            let mut state = self.slot();
            let now = Instant::now();
            let until = Self::identity_until(now);
            Self::prune_expired(&mut state, now);
            if state.active_namespace == Some(namespace)
                && state.active_authority == Some(authority)
            {
                return;
            }
            state.active_namespace = Some(namespace);
            state.active_authority = Some(authority);
            let mut signals = Vec::new();
            let mut expirations = Vec::new();
            let mut dequeue = Vec::new();
            let mut released = 0_usize;
            for (key, record) in &mut state.entries {
                if record.namespace != namespace {
                    if matches!(
                        record.state,
                        RequestReplayEntry::Delivered { .. } | RequestReplayEntry::Retired { .. }
                    ) {
                        continue;
                    }
                    released = released.saturating_add(Self::state_bytes(&record.state));
                    if let RequestReplayEntry::InFlight { signal } = &record.state {
                        signals.push(signal.clone());
                    }
                    if matches!(record.state, RequestReplayEntry::RetireShell) {
                        dequeue.push(key.clone());
                    }
                    record.state = RequestReplayEntry::Retired { until };
                    record.delivery_leases = 0;
                    record.deferred_expiry = None;
                    expirations.push(ReplayExpiration {
                        until,
                        key: key.clone(),
                        kind: ReplayExpirationKind::RemoveTombstone,
                    });
                    continue;
                }

                if matches!(record.state, RequestReplayEntry::Retired { .. }) {
                    // This logical namespace was inactive and is active again.
                    // Its response bytes/inner ownership were deliberately
                    // discarded when it was retired, so it can only re-enter
                    // through the normal fail-closed uncertainty lifecycle.
                    // Promoting here keeps the strong invariant that an active
                    // scope never exposes a Retired entry to claim/delivery.
                    record.authority = authority;
                    record.state = RequestReplayEntry::Uncertain { until };
                    record.delivery_leases = 0;
                    record.deferred_expiry = None;
                    expirations.push(ReplayExpiration {
                        until,
                        key: key.clone(),
                        kind: ReplayExpirationKind::ExpireIdentity,
                    });
                    continue;
                }

                if record.authority == authority {
                    continue;
                }
                record.authority = authority;
                if matches!(record.state, RequestReplayEntry::Delivered { .. }) {
                    continue;
                }

                // Policy/object authority changed under the same tunnel identity.
                // Cached response bytes and recoverable shell ownership belong to
                // the revoked generation, so release them while retaining a
                // fail-closed request identity for one hosted replay horizon.
                released = released.saturating_add(Self::state_bytes(&record.state));
                if let RequestReplayEntry::InFlight { signal } = &record.state {
                    signals.push(signal.clone());
                }
                if matches!(record.state, RequestReplayEntry::RetireShell) {
                    dequeue.push(key.clone());
                }
                record.state = RequestReplayEntry::Uncertain { until };
                record.delivery_leases = 0;
                record.deferred_expiry = None;
                expirations.push(ReplayExpiration {
                    until,
                    key: key.clone(),
                    kind: ReplayExpirationKind::ExpireIdentity,
                });
            }
            state.reserved_bytes = state.reserved_bytes.saturating_sub(released);
            state.expirations.extend(expirations);
            for key in dequeue {
                state.queued_shell_retirements.remove(&key);
            }
            signals
        };
        for signal in signals {
            signal.send_replace(());
        }
    }

    fn claim(
        &self,
        key: &str,
        namespace: [u8; 32],
        authority: [u8; 32],
        fingerprint: [u8; 32],
    ) -> Result<RequestReplayClaim> {
        let mut state = self.slot();
        Self::prune_expired(&mut state, Instant::now());
        if state.active_namespace != Some(namespace) || state.active_authority != Some(authority) {
            bail!("tunnel request replay scope is not active");
        }
        if let Some(record) = state.entries.get(key)
            && !Self::same_identity(record, namespace, fingerprint)
        {
            return Ok(RequestReplayClaim::Conflict);
        }
        if let Some(record) = state.entries.get_mut(key) {
            return Ok(match &record.state {
                RequestReplayEntry::InFlight { signal } => {
                    RequestReplayClaim::Wait(signal.subscribe())
                }
                RequestReplayEntry::RecoverableShell { .. } => RequestReplayClaim::RecoverableShell,
                RequestReplayEntry::Uncertain { .. } => {
                    record.delivery_leases = record.delivery_leases.saturating_add(1);
                    RequestReplayClaim::Uncertain(Some(ReplayDeliveryGuard::new(
                        self.clone(),
                        key,
                        namespace,
                        authority,
                        fingerprint,
                        ReplayDeliveryKind::Uncertain,
                    )))
                }
                RequestReplayEntry::Retired { .. } => RequestReplayClaim::Uncertain(None),
                RequestReplayEntry::RetireShell => RequestReplayClaim::RetireShell,
                RequestReplayEntry::Ready { terminal, .. } => {
                    let terminal = terminal.clone();
                    record.delivery_leases = record.delivery_leases.saturating_add(1);
                    RequestReplayClaim::Ready(
                        terminal,
                        ReplayDeliveryGuard::new(
                            self.clone(),
                            key,
                            namespace,
                            authority,
                            fingerprint,
                            ReplayDeliveryKind::Ready,
                        ),
                    )
                }
                RequestReplayEntry::Delivered { .. } => RequestReplayClaim::Delivered,
            });
        }
        if state.entries.len() >= MAX_REQUEST_REPLAY_ENTRIES {
            bail!(
                "too many retained tunnel request replays ({MAX_REQUEST_REPLAY_ENTRIES}); retry after older request identities expire or outstanding responses are delivered"
            );
        }
        if state
            .reserved_bytes
            .checked_add(REQUEST_REPLAY_RESERVATION_BYTES)
            .is_none_or(|bytes| bytes > MAX_REQUEST_REPLAY_BYTES)
        {
            bail!(
                "tunnel request replay memory is at its {} MiB safety ceiling; retry after an earlier side-effecting response is delivered or its namespace is retired",
                MAX_REQUEST_REPLAY_BYTES / (1024 * 1024)
            );
        }
        let (signal, _receiver) = watch::channel(());
        state.entries.insert(
            key.to_owned(),
            RequestReplayRecord {
                namespace,
                authority,
                fingerprint,
                state: RequestReplayEntry::InFlight { signal },
                delivery_leases: 0,
                deferred_expiry: None,
            },
        );
        state.reserved_bytes += REQUEST_REPLAY_RESERVATION_BYTES;
        Ok(RequestReplayClaim::Leader)
    }

    fn resume_recoverable_shell(
        &self,
        key: &str,
        namespace: [u8; 32],
        authority: [u8; 32],
        fingerprint: [u8; 32],
    ) -> Result<RequestReplayClaim> {
        let mut state = self.slot();
        Self::prune_expired(&mut state, Instant::now());
        if state.active_namespace != Some(namespace) || state.active_authority != Some(authority) {
            bail!("tunnel request replay scope is not active");
        }
        let Some(record) = state.entries.get(key) else {
            return Ok(RequestReplayClaim::Uncertain(None));
        };
        if !Self::same_identity(record, namespace, fingerprint) {
            return Ok(RequestReplayClaim::Conflict);
        }
        if !matches!(record.state, RequestReplayEntry::RecoverableShell { .. }) {
            let record = state.entries.get_mut(key).expect("record checked above");
            return Ok(match &record.state {
                RequestReplayEntry::InFlight { signal } => {
                    RequestReplayClaim::Wait(signal.subscribe())
                }
                RequestReplayEntry::Uncertain { .. } => {
                    record.delivery_leases = record.delivery_leases.saturating_add(1);
                    RequestReplayClaim::Uncertain(Some(ReplayDeliveryGuard::new(
                        self.clone(),
                        key,
                        namespace,
                        authority,
                        fingerprint,
                        ReplayDeliveryKind::Uncertain,
                    )))
                }
                RequestReplayEntry::Retired { .. } => RequestReplayClaim::Uncertain(None),
                RequestReplayEntry::RetireShell => RequestReplayClaim::RetireShell,
                RequestReplayEntry::Ready { terminal, .. } => {
                    let terminal = terminal.clone();
                    record.delivery_leases = record.delivery_leases.saturating_add(1);
                    RequestReplayClaim::Ready(
                        terminal,
                        ReplayDeliveryGuard::new(
                            self.clone(),
                            key,
                            namespace,
                            authority,
                            fingerprint,
                            ReplayDeliveryKind::Ready,
                        ),
                    )
                }
                RequestReplayEntry::Delivered { .. } => RequestReplayClaim::Delivered,
                RequestReplayEntry::RecoverableShell { .. } => unreachable!("checked above"),
            });
        }
        if state
            .reserved_bytes
            .checked_add(REQUEST_REPLAY_RESERVATION_BYTES)
            .is_none_or(|bytes| bytes > MAX_REQUEST_REPLAY_BYTES)
        {
            bail!(
                "tunnel request replay memory is at its {} MiB safety ceiling; retry after an earlier side-effecting response is delivered or its namespace is retired",
                MAX_REQUEST_REPLAY_BYTES / (1024 * 1024)
            );
        }
        let (signal, _receiver) = watch::channel(());
        let record = state.entries.get_mut(key).expect("record checked above");
        record.state = RequestReplayEntry::InFlight { signal };
        record.delivery_leases = 0;
        record.deferred_expiry = None;
        state.reserved_bytes += REQUEST_REPLAY_RESERVATION_BYTES;
        Ok(RequestReplayClaim::Leader)
    }

    fn mark_uncertain_if_recoverable(&self, key: &str, namespace: [u8; 32], fingerprint: [u8; 32]) {
        let mut state = self.slot();
        if let Some(record) = state.entries.get_mut(key)
            && Self::same_identity(record, namespace, fingerprint)
            && let RequestReplayEntry::RecoverableShell { until } = record.state
        {
            record.state = RequestReplayEntry::Uncertain { until };
            record.delivery_leases = 0;
            record.deferred_expiry = None;
        }
    }

    fn invalidate_ready(&self, key: &str, namespace: [u8; 32], fingerprint: [u8; 32]) -> bool {
        let mut state = self.slot();
        let released = state.entries.get(key).and_then(|record| {
            (Self::same_identity(record, namespace, fingerprint)
                && matches!(record.state, RequestReplayEntry::Ready { .. }))
            .then(|| Self::state_bytes(&record.state))
        });
        let Some(released) = released else {
            return false;
        };
        state.reserved_bytes = state.reserved_bytes.saturating_sub(released);
        let until = Self::identity_until(Instant::now());
        if let Some(record) = state.entries.get_mut(key) {
            record.state = RequestReplayEntry::Uncertain { until };
            record.delivery_leases = 0;
            record.deferred_expiry = None;
        }
        state.expirations.push_back(ReplayExpiration {
            until,
            key: key.to_owned(),
            kind: ReplayExpirationKind::ExpireIdentity,
        });
        true
    }

    fn complete(
        &self,
        key: &str,
        namespace: [u8; 32],
        fingerprint: [u8; 32],
        terminal: ReplayTerminal,
    ) -> Result<()> {
        let body = serialize_tunnel_response(&terminal.response)?;
        if body.len() > REQUEST_REPLAY_RESERVATION_BYTES {
            bail!(
                "terminal tunnel response exceeded the {} byte replay reservation",
                REQUEST_REPLAY_RESERVATION_BYTES
            );
        }
        let terminal = CachedReplayTerminal {
            body,
            finalize_job_delivery: terminal.finalize_job_delivery,
            requires_job_owner: terminal.requires_job_owner,
            shell_guard_generation: terminal.shell_guard_generation,
            recover_after_body_expiry: terminal.finalize_job_delivery,
        };
        let signal = {
            let mut state = self.slot();
            let Some(record) = state.entries.get(key) else {
                return Ok(());
            };
            if !Self::same_identity(record, namespace, fingerprint) {
                bail!("tunnel replay identity changed while execution was active");
            }
            let RequestReplayEntry::InFlight { signal } = &record.state else {
                return Ok(());
            };
            let signal = signal.clone();
            let reserved_bytes = terminal.body.len().max(1);
            let now = Instant::now();
            let body_until = now.checked_add(REQUEST_REPLAY_RETENTION).unwrap_or(now);
            state.reserved_bytes = state
                .reserved_bytes
                .saturating_sub(REQUEST_REPLAY_RESERVATION_BYTES)
                .saturating_add(reserved_bytes);
            let record = state.entries.get_mut(key).expect("record checked above");
            record.state = RequestReplayEntry::Ready {
                terminal: Arc::new(terminal),
                reserved_bytes,
                body_until,
            };
            record.delivery_leases = 0;
            record.deferred_expiry = None;
            state.expirations.push_back(ReplayExpiration {
                until: body_until,
                key: key.to_owned(),
                kind: ReplayExpirationKind::DropReadyBody,
            });
            signal
        };
        signal.send_replace(());
        Ok(())
    }

    fn abandon(
        &self,
        key: &str,
        namespace: [u8; 32],
        fingerprint: [u8; 32],
        retryable_by_inner_owner: bool,
    ) {
        let signal = {
            let mut state = self.slot();
            let Some(record) = state.entries.get(key) else {
                return;
            };
            if !Self::same_identity(record, namespace, fingerprint) {
                return;
            }
            let RequestReplayEntry::InFlight { signal } = &record.state else {
                return;
            };
            let signal = signal.clone();
            state.reserved_bytes = state
                .reserved_bytes
                .saturating_sub(REQUEST_REPLAY_RESERVATION_BYTES);
            let until = Self::identity_until(Instant::now());
            let record = state.entries.get_mut(key).expect("record checked above");
            record.state = if retryable_by_inner_owner {
                RequestReplayEntry::RecoverableShell { until }
            } else {
                RequestReplayEntry::Uncertain { until }
            };
            record.delivery_leases = 0;
            record.deferred_expiry = None;
            state.expirations.push_back(ReplayExpiration {
                until,
                key: key.to_owned(),
                kind: ReplayExpirationKind::ExpireIdentity,
            });
            signal
        };
        signal.send_replace(());
    }

    fn commit_delivered(
        &self,
        key: &str,
        namespace: [u8; 32],
        authority: [u8; 32],
        fingerprint: [u8; 32],
        from_ready: bool,
    ) -> bool {
        let mut state = self.slot();
        let now = Instant::now();
        let until = Self::identity_until(now);
        Self::prune_expired(&mut state, now);
        if state.active_namespace != Some(namespace) || state.active_authority != Some(authority) {
            return false;
        }
        let Some(record) = state.entries.get(key) else {
            return false;
        };
        if record.authority != authority || !Self::same_identity(record, namespace, fingerprint) {
            return false;
        }
        let expected = if from_ready {
            matches!(record.state, RequestReplayEntry::Ready { .. })
        } else {
            matches!(record.state, RequestReplayEntry::Uncertain { .. })
        };
        if !expected {
            return false;
        }
        let released = Self::state_bytes(&record.state);
        state.reserved_bytes = state.reserved_bytes.saturating_sub(released);
        let record = state.entries.get_mut(key).expect("record checked above");
        record.state = RequestReplayEntry::Delivered { until };
        record.delivery_leases = 0;
        record.deferred_expiry = None;
        state.expirations.push_back(ReplayExpiration {
            until,
            key: key.to_owned(),
            kind: ReplayExpirationKind::RemoveTombstone,
        });
        true
    }

    fn commit_ready_delivery(
        &self,
        key: &str,
        namespace: [u8; 32],
        authority: [u8; 32],
        fingerprint: [u8; 32],
    ) -> bool {
        self.commit_delivered(key, namespace, authority, fingerprint, true)
    }

    fn commit_uncertain_delivery(
        &self,
        key: &str,
        namespace: [u8; 32],
        authority: [u8; 32],
        fingerprint: [u8; 32],
    ) -> bool {
        self.commit_delivered(key, namespace, authority, fingerprint, false)
    }
}

/// Race one shutdown-sensitive operation with cancellation. Bias is
/// load-bearing: a simultaneously ready HTTP result or semaphore permit must
/// not turn an already-requested shutdown into a transport failure or new work.
async fn cancellation_first<F>(cancellation: &CancellationToken, operation: F) -> Option<F::Output>
where
    F: std::future::Future,
{
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        result = operation => Some(result),
    }
}

enum TunnelRunOutcome {
    Cancelled,
    RestartRuntime {
        error: anyhow::Error,
        had_success: bool,
        /// The configured threshold, reported verbatim.
        threshold: u32,
    },
}

impl TunnelClient {
    pub(crate) fn new(
        config: Config,
        tuning: TunnelTuning,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        let Config {
            base_url,
            tunnel_id,
            runtime_api_key: api_key,
            organization_id,
            new_tunnel,
            execution_authority,
            log,
            process_state,
        } = config;
        let execution_authority = execution_authority
            .ok_or_else(|| anyhow!("OpenAI transport execution authority was not prepared"))?;
        if tunnel_id.trim().is_empty() || api_key.trim().is_empty() {
            bail!("tunnel id and runtime API key are required");
        }
        let base_url = canonical_base_url(&base_url)?;
        let identity = tunnel_identity([
            Some(&base_url),
            Some(&tunnel_id),
            Some(&api_key),
            organization_id.as_deref(),
        ]);
        let request_namespace = tunnel_identity([
            Some(&base_url),
            Some(&tunnel_id),
            None,
            organization_id.as_deref(),
        ]);
        let tuning = Arc::new(tuning.validated()?);
        let activation_grace_until = match new_tunnel {
            true => Some(
                Instant::now()
                    .checked_add(tuning.activation_grace)
                    .ok_or_else(|| {
                        anyhow!("tunnel tuning: activation_grace overflows the clock")
                    })?,
            ),
            false => None,
        };
        let http = Client::builder()
            .user_agent(format!("dotlink/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(tuning.connect_timeout)
            .timeout(tuning.request_timeout)
            .build()?;
        // Replay-scope retirement is externally visible state. Perform it only
        // after every fallible client-construction step has succeeded.
        process_state.activate_scope(request_namespace, execution_authority);
        Ok(Self {
            http,
            base_url: Arc::from(base_url),
            tunnel_id: Arc::from(tunnel_id),
            api_key: Arc::from(api_key),
            organization_id: organization_id.map(Arc::from),
            instance_id: Arc::from(Uuid::new_v4().to_string()),
            activation_grace_until,
            log,
            cancellation,
            concurrency: Arc::new(Semaphore::new(tuning.max_in_flight)),
            tuning,
            identity,
            request_namespace,
            execution_authority,
            rejections: process_state.rejections.clone(),
            replays: process_state.replays.clone(),
        })
    }

    fn settle_shell_retirement(&self, mcp: &EmbeddedMcp, key: &str) -> bool {
        match mcp.machine.retire_job_request_delivery(key) {
            crate::jobs::RequestRetirement::Retired | crate::jobs::RequestRetirement::Missing => {
                self.replays.finish_shell_retirement(key);
                true
            }
            crate::jobs::RequestRetirement::Busy => {
                self.replays.defer_shell_retirement(key);
                false
            }
        }
    }

    fn prune_replays(&self, mcp: &EmbeddedMcp) {
        let retirements = self.replays.prune();
        for (index, key) in retirements.iter().enumerate() {
            if self.settle_shell_retirement(mcp, key) {
                continue;
            }
            // A busy retirement commonly means the ordinary retained-job pool
            // is saturated. Stop after the first blocked item so one poll never
            // multiplies a full-map capacity scan by a large replay backlog.
            for pending in &retirements[index + 1..] {
                self.replays.defer_shell_retirement(pending);
            }
            break;
        }
    }

    async fn run(self, mcp: EmbeddedMcp, poll_ceiling: usize) -> Result<TunnelRunOutcome> {
        debug!(?self.tuning, "tunnel tuning");
        let mut tasks = JoinSet::new();
        let mut failures = 0_u32;
        let mut not_ready_failures = 0_u32;
        let mut announced = false;
        let mut reconnect_reason = None;

        'run: loop {
            // Replay body/tombstone expiry is FIFO/incremental. Prune once per
            // control-plane poll so stale response memory is released promptly
            // even when no new side-effecting request arrives.
            self.prune_replays(&mcp);
            while let Some(result) = tasks.try_join_next() {
                if let Err(join_error) = result {
                    error!(%join_error, "tunnel command task failed");
                }
            }

            let batch = tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => break,
                result = self.poll_once(poll_ceiling) => {
                    // Only an accepted poll proves that an earlier rejection
                    // recovered. 429/5xx, other failures and runtime restarts
                    // leave both health counters intact, so alternating
                    // statuses cannot evade every recovery path.
                    if result.is_ok() {
                        self.rejections.accept(self.identity);
                    }
                    match result {
                        Ok(batch) => {
                            failures = 0;
                            not_ready_failures = 0;
                            if !announced {
                                eprintln!("✓ Connected — ready");
                                eprintln!();
                                info!(tunnel_id = %self.tunnel_id, "secure MCP tunnel connected");
                                announced = true;
                            }
                            batch
                        }
                        Err(PollFailure::Transient(error)) => {
                            failures = failures.saturating_add(1);

                            if should_restart_runtime_after_poll_failures(
                                failures,
                                self.tuning.restart_after,
                            ) {
                                warn!(
                                    attempt = failures,
                                    "tunnel poll failure threshold reached; restarting dotlink runtime"
                                );
                                self.log.developer(
                                    "openai",
                                    format!("poll failure triggering runtime restart: {error:#}"),
                                );
                                debug!(%error, "OpenAI tunnel poll failure triggering runtime restart");
                                reconnect_reason = Some(error);
                                break;
                            }

                            warn!(attempt = failures, "tunnel poll failed; retrying");
                            self.log.developer(
                                "openai",
                                format!("poll failure attempt {failures}: {error:#}"),
                            );
                            debug!(%error, attempt = failures, "OpenAI tunnel poll failure detail");
                            self.sleep_backoff(failures).await;
                            continue;
                        }
                        Err(PollFailure::NotReady(error)) => {
                            not_ready_failures = not_ready_failures.saturating_add(1);
                            if not_ready_failures == 1 {
                                eprintln!("Waiting      tunnel activation…");
                            }
                            if not_ready_failures > self.tuning.activation_checks {
                                return Err(error.context("tunnel was not available after repeated activation checks"));
                            }
                            tokio::select! {
                                biased;
                                _ = self.cancellation.cancelled() => break,
                                _ = time::sleep(self.tuning.activation_retry) => {}
                            }
                            continue;
                        }
                        Err(PollFailure::Rejected(error)) => {
                            let window = self.tuning.rejection_confirmation;
                            let rejection = self.rejections.record(self.identity, window);
                            if rejection.confirmed {
                                return Err(recovery::operator(error));
                            }
                            if rejection.attempts == 1 {
                                warn!(
                                    "{error:#}; stopping if this persists for {}s",
                                    window.as_secs()
                                );
                            }
                            self.sleep_backoff(rejection.attempts).await;
                            continue;
                        }
                        Err(PollFailure::Fatal(error)) => return Err(error),
                    }
                }
            };

            let outstanding_limit = self.tuning.poll_limit.saturating_mul(OUTSTANDING_BATCHES);
            for command in batch.commands {
                // Bound the number of outstanding command tasks, but leave
                // room for several batches to queue. A single full batch must
                // not stop polling; a sustained backlog applies backpressure.
                while tasks.len() >= outstanding_limit {
                    tokio::select! {
                        biased;
                        _ = self.cancellation.cancelled() => break 'run,
                        result = tasks.join_next() => {
                            if let Some(result) = result
                                && let Err(join_error) = result
                            {
                                error!(%join_error, "tunnel command task failed");
                            }
                        }
                    }
                }
                let client = self.clone();
                let mcp = mcp.clone();
                let received_at = batch.received_at;
                tasks.spawn(async move {
                    let permit = cancellation_first(
                        &client.cancellation,
                        client.concurrency.clone().acquire_owned(),
                    )
                    .await;
                    let Some(Ok(_permit)) = permit else {
                        return;
                    };
                    if let Err(error) = client.process_command(mcp, command, received_at).await {
                        warn!("tunnel command failed");
                        debug!(%error, "tunnel command failure detail");
                    }
                });
            }
        }

        tasks.abort_all();
        while tasks.join_next().await.is_some() {}

        if let Some(error) = reconnect_reason {
            Ok(TunnelRunOutcome::RestartRuntime {
                error,
                had_success: announced,
                threshold: self.tuning.restart_after,
            })
        } else {
            Ok(TunnelRunOutcome::Cancelled)
        }
    }

    fn poll_query(&self) -> [(&'static str, String); 2] {
        [
            ("limit", self.tuning.poll_limit.to_string()),
            ("timeout_ms", self.tuning.poll_hold.as_millis().to_string()),
        ]
    }

    async fn poll_once(
        &self,
        poll_ceiling: usize,
    ) -> std::result::Result<PolledBatch, PollFailure> {
        let url = format!("{}/v1/tunnels/{}/poll", self.base_url, self.tunnel_id);
        let response = self
            .common_headers(self.http.get(url).query(&self.poll_query()))
            .send()
            .await
            .map_err(|error| PollFailure::Transient(anyhow!(error)))?;
        let received_at = Instant::now();
        let status = response.status();

        if status == StatusCode::NO_CONTENT {
            return Ok(PolledBatch {
                received_at,
                commands: Vec::new(),
            });
        }
        if status == StatusCode::OK {
            let bytes = bounded::reqwest_response(response, poll_ceiling, "tunnel poll response")
                .await
                .map_err(|error| {
                    PollFailure::Transient(error.context("failed reading poll response"))
                })?;
            let envelope: PollEnvelope = serde_json::from_slice(&bytes).map_err(|error| {
                PollFailure::Transient(anyhow!(error).context("invalid poll response"))
            })?;
            if envelope.commands.len() > self.tuning.poll_limit {
                return Err(PollFailure::Transient(anyhow!(
                    "tunnel returned {} commands, above the {} command poll limit",
                    envelope.commands.len(),
                    self.tuning.poll_limit
                )));
            }
            return Ok(PolledBatch {
                received_at,
                commands: envelope.commands,
            });
        }

        let error = control_plane_status_error("poll", status);
        let in_activation_grace = self
            .activation_grace_until
            .is_some_and(|until| Instant::now() < until);
        match status {
            StatusCode::NOT_FOUND if in_activation_grace => Err(PollFailure::NotReady(error)),
            StatusCode::FORBIDDEN if in_activation_grace => Err(PollFailure::NotReady(error)),
            StatusCode::NOT_FOUND => {
                Err(PollFailure::Rejected(error.context(
                    "the Tunnel ID was not found; run `dotlink --setup` to reconfigure it",
                )))
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(PollFailure::Rejected(
                error.context("check the Runtime API key and its Tunnels Read + Use permissions"),
            )),
            StatusCode::TOO_MANY_REQUESTS => Err(PollFailure::Transient(error)),
            status if status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT => {
                Err(PollFailure::Transient(error))
            }
            _ => Err(PollFailure::Fatal(error)),
        }
    }

    async fn forward_notification(
        &self,
        command: &TunnelCommand,
        message: Value,
        deadline: Option<Instant>,
    ) -> bool {
        self.post_notification(
            command,
            TunnelResponse {
                request_id: command.request_id.clone(),
                channel: command.channel.clone(),
                resp_json: Some(message),
                resp_headers: BTreeMap::new(),
                resp_code: 200,
                resp_type: "jsonrpc_notify".to_owned(),
            },
            deadline,
        )
        .await
    }

    async fn dispatch_and_forward(
        &self,
        mcp: &EmbeddedMcp,
        command: &TunnelCommand,
        jsonrpc: &Value,
        deadline: Option<Instant>,
    ) -> Result<LocalResponse> {
        // A bounded channel separates SSE body decoding from tunnel delivery.
        // Neither the decoder nor the caller buffers an entire command's
        // heartbeats waiting for its terminal response.
        let (tx, mut events) = mpsc::channel(16);
        let dispatch = mcp.dispatch_stream(&command.headers, jsonrpc, tx);
        tokio::pin!(dispatch);
        let mut deliverable = true;
        let local = loop {
            tokio::select! {
                result = &mut dispatch => break result?,
                Some(message) = events.recv() => {
                    if deliverable {
                        deliverable = self.forward_notification(command, message, deadline).await;
                    }
                }
            }
        };
        // The body has finished; drain notifications queued with its last frame
        // before posting the terminal response.
        while let Ok(message) = events.try_recv() {
            if deliverable {
                deliverable = self.forward_notification(command, message, deadline).await;
            }
        }
        Ok(local)
    }

    async fn build_replay_terminal(
        &self,
        mcp: &EmbeddedMcp,
        replay: &ReplayRequestContext<'_>,
    ) -> Result<ReplayTerminal> {
        let request_label = verbose_request_label(replay.jsonrpc);
        let started = self.log.request("openai", &request_label);
        let local = self
            .dispatch_and_forward(
                mcp,
                replay.command,
                replay.jsonrpc,
                replay.dispatch_deadline,
            )
            .await
            .context("local MCP dispatch failed")?;
        self.log.request_done(
            "openai",
            &request_label,
            started,
            LogOutcome::http(local.status),
        );

        let mut terminal = None;
        let mut notifications_deliverable = true;
        for message in local.messages {
            if is_terminal_jsonrpc(&message) {
                terminal = Some(message);
            } else if is_jsonrpc_notification(&message) && notifications_deliverable {
                let response = TunnelResponse {
                    request_id: replay.command.request_id.clone(),
                    channel: replay.command.channel.clone(),
                    resp_json: Some(message),
                    resp_headers: BTreeMap::new(),
                    resp_code: 200,
                    resp_type: "jsonrpc_notify".to_owned(),
                };
                notifications_deliverable = self
                    .post_notification(replay.command, response, replay.dispatch_deadline)
                    .await;
            }
        }
        let terminal = terminal.ok_or_else(|| {
            anyhow!("embedded MCP response ended without a terminal JSON-RPC message")
        })?;
        let delivery_state = mcp.machine.job_request_delivery_state(replay.request_key);
        let response_is_error = terminal.get("error").is_some();
        let (finalize_job_delivery, requires_job_owner, recoverable_shell) =
            match (delivery_state, response_is_error) {
                (Some(crate::jobs::RequestDeliveryState::Pending), _) | (Some(_), true) => {
                    (false, false, true)
                }
                (Some(crate::jobs::RequestDeliveryState::Synchronous), false) => {
                    (true, false, false)
                }
                (Some(crate::jobs::RequestDeliveryState::Durable), false) => (true, true, false),
                (None, _) => (false, false, false),
            };
        let shell_guard_generation = matches!(
            delivery_state,
            Some(crate::jobs::RequestDeliveryState::Synchronous)
        )
        .then(|| {
            mcp.machine
                .job_request_shell_guard_generation(replay.request_key)
        })
        .flatten();
        Ok(ReplayTerminal {
            response: TunnelResponse {
                request_id: replay.command.request_id.clone(),
                channel: replay.command.channel.clone(),
                resp_json: Some(terminal),
                resp_headers: local.headers,
                resp_code: local.status,
                resp_type: "jsonrpc_response".to_owned(),
            },
            finalize_job_delivery,
            requires_job_owner,
            shell_guard_generation,
            recoverable_shell,
        })
    }

    fn recovered_shell_terminal(
        &self,
        mcp: &EmbeddedMcp,
        replay: &ReplayRequestContext<'_>,
    ) -> Result<Option<ReplayTerminal>> {
        let Some((result, requires_job_owner)) = mcp
            .machine
            .recover_job_request_result(
                replay.request_key,
                Some(crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES),
            )
            .map_err(|error| anyhow!(error.to_string()))?
        else {
            return Ok(None);
        };
        let id = replay
            .jsonrpc
            .get("id")
            .map(bounded_jsonrpc_id)
            .unwrap_or(Value::Null);
        let shell_guard_generation = (!requires_job_owner)
            .then(|| {
                mcp.machine
                    .job_request_shell_guard_generation(replay.request_key)
            })
            .flatten();
        Ok(Some(ReplayTerminal {
            response: TunnelResponse {
                request_id: replay.command.request_id.clone(),
                channel: replay.command.channel.clone(),
                resp_json: Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                })),
                resp_headers: BTreeMap::new(),
                resp_code: StatusCode::OK.as_u16(),
                resp_type: "jsonrpc_response".to_owned(),
            },
            finalize_job_delivery: true,
            requires_job_owner,
            shell_guard_generation,
            recoverable_shell: false,
        }))
    }

    fn uncertain_terminal(&self, replay: &ReplayRequestContext<'_>) -> ReplayTerminal {
        ReplayTerminal {
            response: TunnelResponse {
                request_id: replay.command.request_id.clone(),
                channel: replay.command.channel.clone(),
                resp_json: Some(synthesized_jsonrpc_error(
                    replay.jsonrpc,
                    "execution_uncertain",
                )),
                resp_headers: BTreeMap::new(),
                resp_code: StatusCode::BAD_GATEWAY.as_u16(),
                resp_type: "jsonrpc_response".to_owned(),
            },
            finalize_job_delivery: false,
            requires_job_owner: false,
            shell_guard_generation: None,
            recoverable_shell: false,
        }
    }

    async fn execute_replay_leader(
        &self,
        mcp: &EmbeddedMcp,
        replay: &ReplayRequestContext<'_>,
        recoverable_shell: bool,
    ) -> Result<bool> {
        let mut leader = ReplayLeaderGuard::new(
            self.replays.clone(),
            replay.request_key,
            replay.namespace,
            replay.fingerprint,
            recoverable_shell,
        );
        let built = await_before_deadline(
            replay.dispatch_deadline,
            self.build_replay_terminal(mcp, replay),
        )
        .await?;
        let terminal = match built {
            None => {
                drop(leader);
                return Ok(false);
            }
            Some(Ok(terminal)) if terminal.recoverable_shell => {
                match mcp.machine.job_request_delivery_state(replay.request_key) {
                    Some(
                        crate::jobs::RequestDeliveryState::Synchronous
                        | crate::jobs::RequestDeliveryState::Durable,
                    ) => match self.recovered_shell_terminal(mcp, replay)? {
                        Some(recovered) => recovered,
                        None => {
                            drop(leader);
                            return Ok(false);
                        }
                    },
                    Some(crate::jobs::RequestDeliveryState::Pending) | None => {
                        // The JSON-RPC error belongs only to this response attempt
                        // (for example retained-job capacity). Publishing it would
                        // make the caller believe the durable execution failed and
                        // invite a new-id retry. Keep the same outer request
                        // recoverable instead; no terminal POST is emitted.
                        drop(leader);
                        return Ok(false);
                    }
                }
            }
            Some(Ok(terminal)) => terminal,
            Some(Err(error)) => {
                warn!(request_id = %replay.command.request_id_label(), "local MCP replay dispatch failed");
                debug!(request_id = %replay.command.request_id_label(), %error, "local MCP replay dispatch failure detail");
                if recoverable_shell
                    && mcp.machine.has_job_request_delivery(replay.request_key)
                    && let Some(terminal) = self.recovered_shell_terminal(mcp, replay)?
                {
                    terminal
                } else {
                    self.uncertain_terminal(replay)
                }
            }
        };
        self.replays.complete(
            replay.request_key,
            replay.namespace,
            replay.fingerprint,
            terminal,
        )?;
        leader.disarm();
        Ok(true)
    }

    async fn wait_replay_signal(
        &self,
        mut signal: watch::Receiver<()>,
        dispatch_deadline: Option<Instant>,
    ) -> Result<bool> {
        let Some(waited) = cancellation_first(
            &self.cancellation,
            await_before_deadline(dispatch_deadline, signal.changed()),
        )
        .await
        else {
            return Ok(false);
        };
        Ok(waited?.is_some())
    }

    fn cached_shell_guard_is_current(mcp: &EmbeddedMcp, terminal: &CachedReplayTerminal) -> bool {
        mcp.machine
            .shell_guard_generation_is_current(terminal.shell_guard_generation)
    }

    async fn process_replay_protected_jsonrpc(
        &self,
        mcp: &EmbeddedMcp,
        replay: ReplayRequestContext<'_>,
    ) -> Result<()> {
        loop {
            self.prune_replays(mcp);
            match self.replays.claim(
                replay.request_key,
                replay.namespace,
                self.execution_authority,
                replay.fingerprint,
            )? {
                RequestReplayClaim::Leader => {
                    if !self
                        .execute_replay_leader(
                            mcp,
                            &replay,
                            durable_shell_tool_call(replay.jsonrpc),
                        )
                        .await?
                    {
                        return Ok(());
                    }
                }
                RequestReplayClaim::Wait(signal) => {
                    if !self
                        .wait_replay_signal(signal, replay.dispatch_deadline)
                        .await?
                    {
                        return Ok(());
                    }
                }
                RequestReplayClaim::RecoverableShell => {
                    if !mcp.machine.has_job_request_delivery(replay.request_key) {
                        self.replays.mark_uncertain_if_recoverable(
                            replay.request_key,
                            replay.namespace,
                            replay.fingerprint,
                        );
                        continue;
                    }
                    match self.replays.resume_recoverable_shell(
                        replay.request_key,
                        replay.namespace,
                        self.execution_authority,
                        replay.fingerprint,
                    )? {
                        RequestReplayClaim::Leader => {
                            if !self.execute_replay_leader(mcp, &replay, true).await? {
                                return Ok(());
                            }
                        }
                        RequestReplayClaim::Wait(signal) => {
                            if !self
                                .wait_replay_signal(signal, replay.dispatch_deadline)
                                .await?
                            {
                                return Ok(());
                            }
                        }
                        RequestReplayClaim::RetireShell => {
                            if !self.settle_shell_retirement(mcp, replay.request_key) {
                                return Ok(());
                            }
                        }
                        RequestReplayClaim::RecoverableShell
                        | RequestReplayClaim::Uncertain(_)
                        | RequestReplayClaim::Ready(_, _)
                        | RequestReplayClaim::Delivered
                        | RequestReplayClaim::Conflict => continue,
                    }
                }
                RequestReplayClaim::RetireShell => {
                    if !self.settle_shell_retirement(mcp, replay.request_key) {
                        return Ok(());
                    }
                    continue;
                }
                RequestReplayClaim::Uncertain(_delivery) => {
                    let response = TunnelResponse {
                        request_id: replay.command.request_id.clone(),
                        channel: replay.command.channel.clone(),
                        resp_json: Some(synthesized_jsonrpc_error(
                            replay.jsonrpc,
                            "execution_uncertain",
                        )),
                        resp_headers: BTreeMap::new(),
                        resp_code: StatusCode::BAD_GATEWAY.as_u16(),
                        resp_type: "jsonrpc_response".to_owned(),
                    };
                    if self
                        .post_terminal(replay.command, response, replay.deadline)
                        .await?
                    {
                        let _ = self.replays.commit_uncertain_delivery(
                            replay.request_key,
                            replay.namespace,
                            self.execution_authority,
                            replay.fingerprint,
                        );
                    }
                    return Ok(());
                }
                RequestReplayClaim::Ready(terminal, _delivery) => {
                    if !Self::cached_shell_guard_is_current(mcp, &terminal) {
                        if self.replays.invalidate_ready(
                            replay.request_key,
                            replay.namespace,
                            replay.fingerprint,
                        ) {
                            let _ = mcp
                                .machine
                                .discard_hidden_job_request_delivery(replay.request_key);
                        }
                        continue;
                    }
                    if terminal.requires_job_owner
                        && !mcp.machine.has_job_request_delivery(replay.request_key)
                    {
                        let _ = self.replays.invalidate_ready(
                            replay.request_key,
                            replay.namespace,
                            replay.fingerprint,
                        );
                        continue;
                    }
                    let delivered = self
                        .post_terminal_bytes(replay.command, terminal.body.clone(), replay.deadline)
                        .await?;
                    if delivered
                        && self.replays.commit_ready_delivery(
                            replay.request_key,
                            replay.namespace,
                            self.execution_authority,
                            replay.fingerprint,
                        )
                        && terminal.finalize_job_delivery
                    {
                        match mcp
                            .machine
                            .acknowledge_job_request_delivery(replay.request_key)
                        {
                            crate::jobs::RequestDeliveryAck::Acknowledged
                            | crate::jobs::RequestDeliveryAck::Missing => {}
                            crate::jobs::RequestDeliveryAck::Pending => {
                                error!(
                                    request_key = %replay.request_key,
                                    "delivered shell response reached an uncommitted inner job; retiring it to the compatibility surface"
                                );
                                let _ = mcp.machine.retire_job_request_delivery(replay.request_key);
                            }
                        }
                    }
                    return Ok(());
                }
                RequestReplayClaim::Delivered => return Ok(()),
                RequestReplayClaim::Conflict => {
                    let response = TunnelResponse {
                        request_id: replay.command.request_id.clone(),
                        channel: replay.command.channel.clone(),
                        resp_json: Some(synthesized_jsonrpc_error(
                            replay.jsonrpc,
                            "request_identity_conflict",
                        )),
                        resp_headers: BTreeMap::new(),
                        resp_code: StatusCode::CONFLICT.as_u16(),
                        resp_type: "jsonrpc_response".to_owned(),
                    };
                    let _ = self
                        .post_terminal(replay.command, response, replay.deadline)
                        .await?;
                    return Ok(());
                }
            }
        }
    }

    async fn process_command(
        &self,
        mcp: EmbeddedMcp,
        mut command: TunnelCommand,
        received_at: Instant,
    ) -> Result<()> {
        let request_id_label = command.request_id_label().to_owned();
        let channel_label = command.channel_label().to_owned();
        let command_type_label = command.command_type_label().to_owned();
        let response_window = effective_response_window(command.response_timeout.as_ref());
        let deadline = received_at.checked_add(response_window);
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            warn!(
                request_id = %request_id_label,
                "dropping command the client had already expired; no response is deliverable"
            );
            return Ok(());
        }

        let channel = command.channel.clone().unwrap_or_else(|| "main".to_owned());
        if channel != "main" {
            warn!(request_id = %request_id_label, channel = %channel_label, "ignoring unsupported tunnel channel");
            return Ok(());
        }

        // Keep the delivery reserve for the terminal response. Shell caller
        // intent is never shortened: the remaining window is metadata used by
        // the shell layer to choose synchronous execution versus a durable job.
        // Every dispatch is still bounded so the terminal response can be sent.
        let dispatch_deadline = deadline.map(|deadline| {
            deadline
                .checked_sub(self.tuning.delivery_reserve)
                .unwrap_or_else(Instant::now)
        });
        let durable_request_key = durable_request_key(&self.request_namespace, &command.request_id);
        if let Some(remaining) =
            deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
            && let Some(jsonrpc) = command.jsonrpc.as_mut()
        {
            let sync_secs = match annotate_response_context(
                jsonrpc,
                remaining,
                self.tuning.delivery_reserve,
                &durable_request_key,
                mcp.machine.transport_context_token(),
            ) {
                Ok(budget) => budget,
                Err(message) => {
                    // Invalid metadata must never turn off the transport-owned
                    // replay key, result ceiling or synchronous handoff window.
                    let response = TunnelResponse {
                        request_id: command.request_id.clone(),
                        channel: command.channel.clone(),
                        resp_json: jsonrpc.get("id").map(|id| {
                            json!({
                                "jsonrpc": "2.0",
                                "id": bounded_jsonrpc_id(id),
                                "error": {
                                    "code": -32602,
                                    "message": message
                                }
                            })
                        }),
                        resp_headers: BTreeMap::new(),
                        resp_code: StatusCode::BAD_REQUEST.as_u16(),
                        resp_type: if jsonrpc.get("id").is_some() {
                            "jsonrpc_response"
                        } else {
                            "notify_ack"
                        }
                        .to_owned(),
                    };
                    let _ = self.post_terminal(&command, response, deadline).await?;
                    return Ok(());
                }
            };
            if let Some(secs) = sync_secs {
                debug!(
                    request_id = %request_id_label,
                    window_secs = remaining.as_secs_f32(),
                    sync_budget_secs = secs,
                    "annotated shell call with durable request identity and synchronous budget"
                );
            }
        }

        match command.command_type.as_str() {
            "jsonrpc" => {
                let Some(jsonrpc) = command.jsonrpc.as_ref() else {
                    warn!(request_id = %request_id_label, "jsonrpc command had no jsonrpc payload");
                    return Ok(());
                };
                let has_request_id = jsonrpc.get("id").is_some();
                if replay_protected_tool_call(jsonrpc) {
                    let fingerprint = replay_request_fingerprint(jsonrpc, &command.headers)?;
                    return self
                        .process_replay_protected_jsonrpc(
                            &mcp,
                            ReplayRequestContext {
                                command: &command,
                                jsonrpc,
                                request_key: &durable_request_key,
                                namespace: self.request_namespace,
                                fingerprint,
                                dispatch_deadline,
                                deadline,
                            },
                        )
                        .await;
                }
                let request_label = verbose_request_label(jsonrpc);
                let started = self.log.request("openai", &request_label);
                let dispatch =
                    self.dispatch_and_forward(&mcp, &command, jsonrpc, dispatch_deadline);
                let local = match await_before_deadline(dispatch_deadline, dispatch).await {
                    Ok(Some(result)) => match result {
                        Ok(response) => response,
                        Err(error) => {
                            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                                return Ok(());
                            }
                            warn!(request_id = %request_id_label, "local MCP dispatch failed");
                            debug!(request_id = %request_id_label, %error, "local MCP dispatch failure detail");
                            if has_request_id {
                                let response = TunnelResponse {
                                    request_id: command.request_id.clone(),
                                    channel: command.channel.clone(),
                                    resp_json: Some(synthesized_jsonrpc_error(
                                        jsonrpc,
                                        "client_internal",
                                    )),
                                    resp_headers: BTreeMap::new(),
                                    resp_code: 502,
                                    resp_type: "jsonrpc_response".to_owned(),
                                };
                                let _ = self.post_terminal(&command, response, deadline).await?;
                            } else {
                                let response = TunnelResponse {
                                    request_id: command.request_id.clone(),
                                    channel: command.channel.clone(),
                                    resp_json: None,
                                    resp_headers: BTreeMap::new(),
                                    resp_code: 502,
                                    resp_type: "notify_ack".to_owned(),
                                };
                                let _ = self.post_terminal(&command, response, deadline).await?;
                            }
                            return Ok(());
                        }
                    },
                    Ok(None) => {
                        warn!(
                            request_id = %request_id_label,
                            request = %request_label,
                            "client response window expired mid-call; result was not deliverable"
                        );
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };

                self.log.request_done(
                    "openai",
                    &request_label,
                    started,
                    LogOutcome::http(local.status),
                );

                if !has_request_id {
                    let response = TunnelResponse {
                        request_id: command.request_id.clone(),
                        channel: command.channel.clone(),
                        resp_json: None,
                        resp_headers: local.headers,
                        resp_code: local.status,
                        resp_type: "notify_ack".to_owned(),
                    };
                    let _ = self.post_terminal(&command, response, deadline).await?;
                    return Ok(());
                }

                let mut terminal: Option<Value> = None;
                let mut notifications_deliverable = true;
                for message in local.messages {
                    if is_terminal_jsonrpc(&message) {
                        terminal = Some(message);
                    } else if is_jsonrpc_notification(&message) {
                        if notifications_deliverable {
                            let response = TunnelResponse {
                                request_id: command.request_id.clone(),
                                channel: command.channel.clone(),
                                resp_json: Some(message),
                                resp_headers: BTreeMap::new(),
                                resp_code: 200,
                                resp_type: "jsonrpc_notify".to_owned(),
                            };
                            notifications_deliverable = self
                                .post_notification(&command, response, dispatch_deadline)
                                .await;
                        }
                    } else {
                        debug!(request_id = %request_id_label, "ignoring unsupported nonterminal MCP stream message");
                    }
                }

                let (resp_json, resp_code, real_terminal) = match terminal {
                    Some(message) => (message, local.status, true),
                    None => (synthesized_jsonrpc_error(jsonrpc, "protocol"), 502, false),
                };
                let response = TunnelResponse {
                    request_id: command.request_id.clone(),
                    channel: command.channel.clone(),
                    resp_json: Some(resp_json),
                    resp_headers: local.headers,
                    resp_code,
                    resp_type: "jsonrpc_response".to_owned(),
                };
                if self.post_terminal(&command, response, deadline).await? && real_terminal {
                    mcp.machine
                        .acknowledge_job_request_delivery(&durable_request_key);
                }
            }
            "session_termination" => {
                // dotlink advertises a stateless main channel, so there is no local
                // MCP session to close. Acknowledge termination exactly as required by
                // the tunnel protocol.
                let response = TunnelResponse {
                    request_id: command.request_id.clone(),
                    channel: command.channel.clone(),
                    resp_json: None,
                    resp_headers: BTreeMap::new(),
                    resp_code: 204,
                    resp_type: "session_termination_response".to_owned(),
                };
                let _ = self.post_terminal(&command, response, deadline).await?;
            }
            _ => {
                warn!(request_id = %request_id_label, command_type = %command_type_label, "ignoring unknown tunnel command type");
            }
        }

        Ok(())
    }

    async fn post_notification(
        &self,
        command: &TunnelCommand,
        response: TunnelResponse,
        deadline: Option<Instant>,
    ) -> bool {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return false;
        }
        let body = match serialize_tunnel_response(&response) {
            Ok(body) => body,
            Err(error) => {
                warn!(request_id = %command.request_id_label(), "dropping intermediate tunnel notification after serialization failure");
                debug!(request_id = %command.request_id_label(), %error, "intermediate tunnel notification serialization detail");
                return false;
            }
        };
        match self.post_once(command, body, deadline).await {
            Ok(PostOutcome::Delivered) => true,
            Ok(PostOutcome::Undeliverable | PostOutcome::Cancelled) => false,
            Ok(PostOutcome::Retry(error)) | Ok(PostOutcome::Fatal(error)) | Err(error) => {
                warn!(request_id = %command.request_id_label(), "dropping intermediate tunnel notification after send failure");
                debug!(request_id = %command.request_id_label(), %error, "intermediate tunnel notification failure detail");
                false
            }
        }
    }

    async fn post_terminal(
        &self,
        command: &TunnelCommand,
        response: TunnelResponse,
        deadline: Option<Instant>,
    ) -> Result<bool> {
        self.post_terminal_bytes(command, serialize_tunnel_response(&response)?, deadline)
            .await
    }

    async fn post_terminal_bytes(
        &self,
        command: &TunnelCommand,
        response_body: Bytes,
        deadline: Option<Instant>,
    ) -> Result<bool> {
        let mut attempt = 0_u32;
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                warn!(
                    request_id = %command.request_id_label(),
                    "client response window expired before the response could be posted"
                );
                return Ok(false);
            }
            match self
                .post_once(command, response_body.clone(), deadline)
                .await
            {
                Ok(PostOutcome::Delivered) => return Ok(true),
                Ok(PostOutcome::Undeliverable | PostOutcome::Cancelled) => return Ok(false),
                Ok(PostOutcome::Fatal(error)) => return Err(error),
                Ok(PostOutcome::Retry(error)) | Err(error) => {
                    attempt = attempt.saturating_add(1);
                    if attempt >= self.tuning.response_attempts {
                        return Err(error.context("exhausted tunnel response retries"));
                    }
                    let delay = self.tuning.backoff.delay(attempt);
                    if let Some(deadline) = deadline
                        && Instant::now()
                            .checked_add(delay)
                            .is_none_or(|next| next >= deadline)
                    {
                        return Ok(false);
                    }
                    if cancellation_first(&self.cancellation, time::sleep(delay))
                        .await
                        .is_none()
                    {
                        return Ok(false);
                    }
                }
            }
        }
    }

    async fn post_once(
        &self,
        command: &TunnelCommand,
        response_body: Bytes,
        deadline: Option<Instant>,
    ) -> Result<PostOutcome> {
        let url = format!("{}/v1/tunnels/{}/response", self.base_url, self.tunnel_id);
        let future = self
            .common_headers(self.http.post(url))
            .header("X-Tunnel-Shard-Token", command.shard_token.as_str())
            .header("Content-Type", "application/json")
            .body(response_body)
            .send();

        let Some(response) =
            cancellation_first(&self.cancellation, await_before_deadline(deadline, future)).await
        else {
            return Ok(PostOutcome::Cancelled);
        };
        let response = match response? {
            Some(result) => match result {
                Ok(response) => response,
                Err(error) => return Ok(PostOutcome::Retry(anyhow!(error))),
            },
            None => return Ok(PostOutcome::Undeliverable),
        };
        let status = response.status();
        if status.is_success() {
            return Ok(PostOutcome::Delivered);
        }
        if status == StatusCode::NOT_FOUND {
            // The control plane uses 404 for both already-fulfilled and expired
            // requests. That is terminal for this POST attempt, but it is not a
            // positive delivery acknowledgement; retain request-key replay state.
            return Ok(PostOutcome::Undeliverable);
        }
        let error = control_plane_status_error("response", status);
        if retryable_response_status(status) {
            Ok(PostOutcome::Retry(error))
        } else {
            Ok(PostOutcome::Fatal(error))
        }
    }

    fn common_headers(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut request = request
            .bearer_auth(self.api_key.as_ref())
            .header("X-Tunnel-Client-Name", "dotlink")
            .header("X-Tunnel-Client-Version", env!("CARGO_PKG_VERSION"))
            .header(
                "X-Tunnel-Client-Wire-Protocol-Version",
                WIRE_PROTOCOL_VERSION,
            )
            .header("X-Tunnel-Client-Instance-Id", self.instance_id.as_ref())
            .header("X-Tunnel-MCP-Server-Info", SERVER_INFO);
        if let Some(organization_id) = &self.organization_id {
            request = request.header("OpenAI-Organization", organization_id.as_ref());
        }
        request
    }

    async fn sleep_backoff(&self, attempt: u32) {
        let delay = self.tuning.backoff.delay(attempt);
        let _ = cancellation_first(&self.cancellation, time::sleep(delay)).await;
    }
}

enum PostOutcome {
    Delivered,
    Undeliverable,
    Cancelled,
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

async fn await_before_deadline<F, T>(deadline: Option<Instant>, future: F) -> Result<Option<T>>
where
    F: std::future::Future<Output = T>,
{
    match deadline {
        None => Ok(Some(future.await)),
        Some(deadline) => {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            let deadline = time::Instant::from_std(deadline);
            match time::timeout_at(deadline, future).await {
                Ok(value) => Ok(Some(value)),
                Err(_) => Ok(None),
            }
        }
    }
}

fn effective_response_window(value: Option<&Value>) -> Duration {
    parse_response_timeout(value)
        .unwrap_or(crate::limits::OPENAI_TUNNEL_REQUEST_DEADLINE)
        .min(crate::limits::OPENAI_TUNNEL_REQUEST_DEADLINE)
}

fn parse_response_timeout(value: Option<&Value>) -> Option<Duration> {
    let text = value?.as_str()?;
    if text.is_empty() || text.trim() != text {
        return None;
    }
    let split = text.find(|ch: char| !ch.is_ascii_digit())?;
    if split == 0 {
        return None;
    }
    let (digits, unit) = text.split_at(split);
    if unit.is_empty() || unit.chars().any(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let amount: u128 = digits.parse().ok()?;
    let nanos_per_unit: u128 = match unit {
        "ns" => 1,
        "us" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60 * 1_000_000_000,
        "h" => 60 * 60 * 1_000_000_000,
        _ => return None,
    };
    let total_nanos = amount.checked_mul(nanos_per_unit)?;
    let secs = total_nanos / 1_000_000_000;
    let nanos = (total_nanos % 1_000_000_000) as u32;
    let secs: u64 = secs.try_into().ok()?;
    Some(Duration::new(secs, nanos))
}

/// Keep polling through a bounded backlog of command tasks. Three poll batches
/// fit in the queue; beyond that, pausing the poll loop is deliberate
/// backpressure rather than an accidental stall.
const OUTSTANDING_BATCHES: usize = 3;

fn shell_timeout_within(window: Duration, reserve: Duration) -> u64 {
    window.saturating_sub(reserve).as_secs()
}

/// Tell the local MCP server how long this response may wait and which durable
/// request it represents. Execution deadlines are never rewritten: every shell
/// call starts one durable execution and waits only until this response window;
/// a still-running command is then exposed through that same stable job/task id.
fn annotate_response_context(
    jsonrpc: &mut Value,
    window: Duration,
    reserve: Duration,
    request_key: &str,
    transport_context_token: &str,
) -> std::result::Result<Option<u64>, &'static str> {
    let Some(method) = jsonrpc.get("method").and_then(Value::as_str) else {
        return Ok(None);
    };
    let tools_call = method == "tools/call";
    let bounded_response = tools_call || method.starts_with("tasks/");
    if !bounded_response {
        return Ok(None);
    }
    let name = jsonrpc.pointer("/params/name").and_then(Value::as_str);
    let shell = tools_call && name.is_some_and(crate::mcp::is_shell_tool);
    let durable_shell = shell || (tools_call && name == Some("job_start"));
    // Missing and null metadata are both valid MCP representations. Normalize
    // either to an object before adding authenticated transport constraints.
    let Some(params) = jsonrpc.get_mut("params").and_then(Value::as_object_mut) else {
        return Ok(None); // Invalid tool params will be rejected by rmcp.
    };
    let meta = params
        .entry("_meta")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if meta.is_null() {
        *meta = Value::Object(serde_json::Map::new());
    }
    let Some(meta) = meta.as_object_mut() else {
        return Err(INVALID_TRANSPORT_META); // Before any side effects.
    };
    meta.retain(|key, _| !key.starts_with(crate::mcp::TRANSPORT_META_PREFIX));
    meta.insert(
        crate::mcp::TRANSPORT_CONTEXT_TOKEN_META_KEY.to_owned(),
        Value::String(transport_context_token.to_owned()),
    );
    meta.insert(
        crate::mcp::RESULT_BUDGET_META_KEY.to_owned(),
        Value::from(crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES as u64),
    );
    if !tools_call {
        return Ok(None);
    }
    if durable_shell {
        meta.insert(
            crate::mcp::REQUEST_KEY_META_KEY.to_owned(),
            Value::String(request_key.to_owned()),
        );
    }
    if !shell {
        return Ok(None);
    }
    let budget = window.saturating_sub(reserve);
    let millis = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX).max(1);
    meta.insert(
        crate::mcp::SYNC_BUDGET_META_KEY.to_owned(),
        Value::from(millis),
    );
    Ok(Some(shell_timeout_within(window, reserve)))
}

fn tool_call_name(jsonrpc: &Value) -> Option<&str> {
    (jsonrpc.get("id").is_some()
        && jsonrpc.get("method").and_then(Value::as_str) == Some("tools/call"))
    .then(|| jsonrpc.pointer("/params/name").and_then(Value::as_str))
    .flatten()
}

#[derive(Clone, Copy)]
enum ReplayFingerprintScope {
    Root,
    Params,
    ParamsMeta,
    Nested,
}

fn fingerprint_bytes(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn fingerprint_json(digest: &mut Sha256, value: &Value, scope: ReplayFingerprintScope) {
    match value {
        Value::Null => digest.update([0]),
        Value::Bool(value) => digest.update([1, u8::from(*value)]),
        Value::Number(value) => {
            digest.update([2]);
            fingerprint_bytes(digest, value.to_string().as_bytes());
        }
        Value::String(value) => {
            digest.update([3]);
            fingerprint_bytes(digest, value.as_bytes());
        }
        Value::Array(values) => {
            digest.update([4]);
            digest.update((values.len() as u64).to_be_bytes());
            for value in values {
                fingerprint_json(digest, value, ReplayFingerprintScope::Nested);
            }
        }
        Value::Object(values) => {
            let mut entries = values
                .iter()
                .filter(|(key, value)| {
                    if matches!(scope, ReplayFingerprintScope::ParamsMeta) {
                        return !key.starts_with(crate::mcp::TRANSPORT_META_PREFIX);
                    }
                    if matches!(scope, ReplayFingerprintScope::Params) && key.as_str() == "_meta" {
                        return match value {
                            Value::Null => false,
                            Value::Object(meta) => meta
                                .keys()
                                .any(|key| !key.starts_with(crate::mcp::TRANSPORT_META_PREFIX)),
                            _ => true,
                        };
                    }
                    true
                })
                .collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            digest.update([5]);
            digest.update((entries.len() as u64).to_be_bytes());
            for (key, value) in entries {
                fingerprint_bytes(digest, key.as_bytes());
                let child_scope = match (scope, key.as_str()) {
                    (ReplayFingerprintScope::Root, "params") => ReplayFingerprintScope::Params,
                    (ReplayFingerprintScope::Params, "_meta") => ReplayFingerprintScope::ParamsMeta,
                    _ => ReplayFingerprintScope::Nested,
                };
                fingerprint_json(digest, value, child_scope);
            }
        }
    }
}

fn fingerprint_forwarded_headers(digest: &mut Sha256, headers: &BTreeMap<String, Vec<String>>) {
    let effective = effective_forwarded_headers(headers);
    digest.update((effective.len() as u64).to_be_bytes());
    for (name, header) in effective {
        fingerprint_bytes(digest, name.as_bytes());
        digest.update((header.values.len() as u64).to_be_bytes());
        for value in header.values {
            fingerprint_bytes(digest, value.as_bytes());
        }
    }
}

/// Bind exactly-once replay to the complete semantic MCP request. Only
/// authenticated dotlink transport-private metadata is excluded; public MCP
/// metadata, MRTR fields, future request parameters, and every effective
/// forwarded MCP routing/protocol header remain part of identity.
fn replay_request_fingerprint(
    jsonrpc: &Value,
    headers: &BTreeMap<String, Vec<String>>,
) -> Result<[u8; 32]> {
    tool_call_name(jsonrpc)
        .ok_or_else(|| anyhow!("replay fingerprint requires a tools/call request"))?;
    if jsonrpc.get("id").is_none() {
        bail!("replay fingerprint requires a JSON-RPC id");
    }
    let mut digest = Sha256::new();
    digest.update(b"dotlink-replay-request-v3\0");
    fingerprint_json(&mut digest, jsonrpc, ReplayFingerprintScope::Root);
    fingerprint_forwarded_headers(&mut digest, headers);
    Ok(digest.finalize().into())
}

fn replay_protected_tool_call(jsonrpc: &Value) -> bool {
    tool_call_name(jsonrpc).is_some_and(crate::mcp::tool_requires_request_replay)
}

fn durable_shell_tool_call(jsonrpc: &Value) -> bool {
    tool_call_name(jsonrpc)
        .is_some_and(|name| crate::mcp::is_shell_tool(name) || name == "job_start")
}

fn verbose_request_label(jsonrpc: &Value) -> String {
    crate::logging::jsonrpc_request_label(jsonrpc, true)
}

#[cfg(test)]
fn verbose_request_summary(jsonrpc: &Value) -> String {
    let method = jsonrpc
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("jsonrpc");
    if method != "tools/call" {
        return format!("→ {method}");
    }

    let name = jsonrpc
        .pointer("/params/name")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let arguments = jsonrpc.pointer("/params/arguments");
    let args = arguments
        .map(|value| summarize_tool_arguments(name, value))
        .unwrap_or_default();
    if args.is_empty() {
        format!("→ tools/call {name}")
    } else {
        format!("→ tools/call {name}  {args}")
    }
}

#[cfg(test)]
fn summarize_tool_arguments(name: &str, arguments: &Value) -> String {
    // One summarizer, shared with the TOOL log: argument order comes from the
    // object itself here because a test has no tool schema to read.
    match arguments.as_object() {
        Some(object) => crate::logging::summarize_arguments(
            name,
            object,
            object.keys().map(String::as_str),
            None,
        ),
        None => String::new(),
    }
}

fn is_terminal_jsonrpc(value: &Value) -> bool {
    value.get("id").is_some() && (value.get("result").is_some() || value.get("error").is_some())
}

fn is_jsonrpc_notification(value: &Value) -> bool {
    value.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && value.get("id").is_none()
        && value.get("method").and_then(Value::as_str).is_some()
}

fn synthesized_jsonrpc_error(request: &Value, source: &str) -> Value {
    let id = request
        .get("id")
        .map(bounded_jsonrpc_id)
        .unwrap_or(Value::Null);
    let message = match source {
        "execution_uncertain" => {
            "Execution outcome is uncertain; the side effect may already have committed. Do not automatically retry this operation with a new request id."
        }
        "request_identity_conflict" => {
            "Tunnel request id was reused with a different side-effecting tool payload; execution was refused."
        }
        _ => "Bad Gateway",
    };
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32603,
            "message": message,
            "data": {
                "tunnel_failure": {
                    "version": 1,
                    "source": source,
                    "upstream_response_received": false
                }
            }
        }
    })
}

fn should_restart_runtime_after_poll_failures(failures: u32, restart_after: u32) -> bool {
    failures >= restart_after
}

fn retryable_response_status(status: StatusCode) -> bool {
    status.is_server_error()
        || matches!(
            status,
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
        )
}

fn control_plane_status_error(operation: &'static str, status: StatusCode) -> anyhow::Error {
    anyhow!("OpenAI tunnel {operation} failed with HTTP status {status}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            base_url: "https://example.invalid".to_owned(),
            tunnel_id: "tunnel".to_owned(),
            runtime_api_key: "key".to_owned(),
            organization_id: None,
            new_tunnel: false,
            execution_authority: Some([0; 32]),
            log: LogConfig::default(),
            process_state: ProcessState::default(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn client_supplied_reserved_dotlink_meta_is_ignored_without_runtime_token() {
        use crate::mcp::{AccessSpec, MachineConfig};
        let Some(_shell) = crate::mcp::resolve_executable(crate::mcp::shell_tool_name()) else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root],
                write_roots: Vec::new(),
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 5,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,
            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,
            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let shell = crate::mcp::shell_tool_name();
        let call = |id: i64, output: &str| {
            json!({
                "jsonrpc":"2.0", "id":id, "method":"tools/call",
                "params": {
                    "name": shell,
                    "arguments": {"command": format!("printf {output}")},
                    "_meta": {
                        crate::mcp::TRANSPORT_CONTEXT_TOKEN_META_KEY: "attacker",
                        crate::mcp::REQUEST_KEY_META_KEY: "same-key",
                        crate::mcp::SYNC_BUDGET_META_KEY: 1,
                        crate::mcp::RESULT_BUDGET_META_KEY: 64
                    }
                }
            })
        };
        for (id, expected) in [(1, "first"), (2, "second")] {
            let response = mcp
                .dispatch(&BTreeMap::new(), &call(id, expected))
                .await
                .unwrap();
            let message = response
                .messages
                .iter()
                .find(|message| message.get("id") == Some(&json!(id)))
                .unwrap();
            let text = message["result"]["content"][0]["text"].as_str().unwrap();
            let result: Value = serde_json::from_str(text).unwrap();
            assert_eq!(result["stdout"], expected, "{message}");
        }
    }

    #[test]
    fn valid_null_or_missing_meta_never_loses_trusted_shell_constraints() {
        let name = crate::mcp::shell_tool_name();
        for original in [
            None,
            Some(Value::Null),
            Some(json!({"client": "untouched"})),
        ] {
            let mut call = json!({
                "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": {"name": name, "arguments": {"command": "true"}}
            });
            if let Some(meta) = original {
                call["params"]["_meta"] = meta;
            }
            assert_eq!(
                annotate_response_context(
                    &mut call,
                    Duration::from_secs(20),
                    Duration::from_secs(5),
                    "durable-key",
                    "trusted-token"
                ),
                Ok(Some(15))
            );
            let meta = call["params"]["_meta"].as_object().unwrap();
            assert_eq!(meta[crate::mcp::REQUEST_KEY_META_KEY], "durable-key");
            assert_eq!(
                meta[crate::mcp::TRANSPORT_CONTEXT_TOKEN_META_KEY],
                "trusted-token"
            );
            assert_eq!(meta[crate::mcp::SYNC_BUDGET_META_KEY], 15_000);
            assert_eq!(
                meta[crate::mcp::RESULT_BUDGET_META_KEY],
                crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES as u64
            );
        }

        let mut task = json!({
            "jsonrpc": "2.0", "id": 9, "method": "tasks/get",
            "params": {"taskId": "unknown", "_meta": null}
        });
        assert_eq!(
            annotate_response_context(
                &mut task,
                Duration::from_secs(20),
                Duration::from_secs(5),
                "unused",
                "trusted-token"
            ),
            Ok(None)
        );
        let meta = task["params"]["_meta"].as_object().unwrap();
        assert_eq!(
            meta[crate::mcp::TRANSPORT_CONTEXT_TOKEN_META_KEY],
            "trusted-token"
        );
        assert_eq!(
            meta[crate::mcp::RESULT_BUDGET_META_KEY],
            crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES as u64
        );
        assert!(!meta.contains_key(crate::mcp::REQUEST_KEY_META_KEY));

        for meta in [json!([]), json!("bad"), json!(true), json!(7)] {
            let mut call = json!({
                "jsonrpc":"2.0", "id": 8, "method":"tools/call",
                "params": {"name": name, "arguments": {"command": "true"}, "_meta": meta}
            });
            assert_eq!(
                annotate_response_context(
                    &mut call,
                    Duration::from_secs(20),
                    Duration::from_secs(5),
                    "durable-key",
                    "trusted-token"
                ),
                Err(INVALID_TRANSPORT_META)
            );
            assert!(
                call.pointer("/params/_meta/io.abird.dotlink~1requestKey")
                    .is_none()
            );
        }
    }

    #[cfg(unix)]
    async fn shell_enabled_test_machine(root: &std::path::Path) -> LocalMachine {
        use crate::mcp::{AccessSpec, MachineConfig};
        LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.to_path_buf(),
                read_roots: vec![root.to_path_buf()],
                write_roots: vec![root.to_path_buf()],
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 20,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,
            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,
            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn null_meta_request_replays_original_side_effect_not_a_second_execution() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let effect = root.join("effect.txt");
        let machine = shell_enabled_test_machine(&root).await;
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let mut call = json!({
            "jsonrpc": "2.0", "id": 12, "method": "tools/call",
            "params": {
                "name": crate::mcp::shell_tool_name(),
                "arguments": {"command": format!("printf x >> '{}'", effect.display())},
                "_meta": null
            }
        });
        assert_eq!(
            annotate_response_context(
                &mut call,
                Duration::from_secs(20),
                Duration::from_secs(5),
                "same-delivery-key",
                mcp.machine.transport_context_token()
            ),
            Ok(Some(15))
        );
        let headers = BTreeMap::new();
        for _ in 0..2 {
            let response = mcp.dispatch(&headers, &call).await.unwrap();
            assert_eq!(response.status, 200, "{response:?}");
            let text = response.messages[0]["result"]["content"][0]["text"]
                .as_str()
                .unwrap();
            let result: Value = serde_json::from_str(text).unwrap();
            assert_eq!(result["success"], true, "{result}");
        }
        assert_eq!(
            std::fs::read(&effect).unwrap(),
            b"x",
            "redelivery ran a second shell"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_shell_tunnel_delivery_retires_inner_request_key() {
        use std::sync::atomic::Ordering;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let effect = root.join("shell-delivery.txt");
        let machine = shell_enabled_test_machine(&root).await;
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let (base_url, posts) = fake_control_plane(|_| (StatusCode::OK, Duration::ZERO)).await;
        let client = TunnelClient::new(
            Config {
                base_url,
                ..test_config()
            },
            TunnelTuning::default(),
            CancellationToken::new(),
        )
        .unwrap();
        let command = TunnelCommand {
            request_id: "successful-shell-delivery".to_owned(),
            shard_token: "test-shard".to_owned(),
            command_type: "jsonrpc".to_owned(),
            channel: Some("main".to_owned()),
            response_timeout: Some(json!("20s")),
            headers: BTreeMap::new(),
            jsonrpc: Some(json!({
                "jsonrpc":"2.0", "id":55, "method":"tools/call",
                "params": {
                    "name": crate::mcp::shell_tool_name(),
                    "arguments": {"command": format!("printf x > '{}'", effect.display())}
                }
            })),
        };
        let request_key = durable_request_key(&client.request_namespace, &command.request_id);
        let fingerprint =
            replay_request_fingerprint(command.jsonrpc.as_ref().unwrap(), &command.headers)
                .unwrap();
        client
            .process_command(mcp.clone(), command, Instant::now())
            .await
            .unwrap();

        assert_eq!(std::fs::read(&effect).unwrap(), b"x");
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        assert!(
            !mcp.machine.has_job_request_delivery(&request_key),
            "positive committed shell delivery must retire the inner dedupe key"
        );
        assert!(matches!(
            client
                .replays
                .claim(
                    &request_key,
                    client.request_namespace,
                    client.execution_authority,
                    fingerprint
                )
                .unwrap(),
            RequestReplayClaim::Delivered
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_idempotent_mutation_replay_is_process_wide_and_tombstoned() {
        use std::sync::atomic::Ordering;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let target = root.join("data.bin");
        std::fs::write(&target, b"A").unwrap();
        let machine = test_machine_with_write(&root, true).await;
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let process_state = ProcessState::default();
        let (base_url, posts) = fake_control_plane(|_| (StatusCode::OK, Duration::ZERO)).await;
        let make_client = || {
            TunnelClient::new(
                Config {
                    base_url: base_url.clone(),
                    process_state: process_state.clone(),
                    ..test_config()
                },
                TunnelTuning::default(),
                CancellationToken::new(),
            )
            .unwrap()
        };
        let command = TunnelCommand {
            request_id: "same-mutation".to_owned(),
            shard_token: "test-shard".to_owned(),
            command_type: "jsonrpc".to_owned(),
            channel: Some("main".to_owned()),
            response_timeout: Some(json!("20s")),
            headers: BTreeMap::new(),
            jsonrpc: Some(json!({
                "jsonrpc": "2.0",
                "id": 77,
                "method": "tools/call",
                "params": {
                    "name": "bedit",
                    "arguments": {
                        "path": "data.bin",
                        "edits": [{"offset": 1, "length": 0, "data": "Qg=="}]
                    }
                }
            })),
        };

        let client = make_client();
        let received_at = Instant::now();
        let (first, duplicate) = tokio::join!(
            client.process_command(mcp.clone(), command.clone(), received_at),
            client.process_command(mcp.clone(), command.clone(), received_at),
        );
        first.unwrap();
        duplicate.unwrap();
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"AB",
            "concurrent redelivery must execute the insertion exactly once"
        );

        let posts_after_delivery = posts.load(Ordering::SeqCst);
        assert!(posts_after_delivery >= 1);

        let mut conflicting = command.clone();
        conflicting.jsonrpc.as_mut().unwrap()["params"]["arguments"]["edits"][0]["data"] =
            Value::String("Qw==".to_owned());
        make_client()
            .process_command(mcp.clone(), conflicting, Instant::now())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"AB",
            "same request id with different mutation arguments must be refused"
        );
        let posts_after_conflict = posts.load(Ordering::SeqCst);
        assert_eq!(posts_after_conflict, posts_after_delivery + 1);

        // A newly constructed client models automatic in-process runtime/tunnel
        // recovery. The process-scoped delivered tombstone must suppress an exact
        // copy already queued or redelivered after the successful response.
        make_client()
            .process_command(mcp, command, Instant::now())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"AB");
        assert_eq!(
            posts.load(Ordering::SeqCst),
            posts_after_conflict,
            "delivered tombstone must suppress both execution and a second terminal POST"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn capacity_error_redelivery_does_not_retire_original_shell_request_key() {
        use crate::mcp::ShellArgs;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = shell_enabled_test_machine(&root).await;
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let (base_url, posts) = fake_control_plane(|_| (StatusCode::OK, Duration::ZERO)).await;
        let client = TunnelClient::new(
            Config {
                base_url,
                ..test_config()
            },
            TunnelTuning::default(),
            CancellationToken::new(),
        )
        .unwrap();
        let request_id = "capacity-redelivery";
        let request_key = durable_request_key(&client.request_namespace, request_id);
        let shell_args = ShellArgs {
            command: "printf original".to_owned(),
            dir: ".".to_owned(),
            stdin: None,
            timeout_secs: Some(5),
            max_output_bytes: Some(4096),
        };
        let original = mcp
            .machine
            .start_shell_job(shell_args.clone(), false, Some(request_key.clone()))
            .unwrap();
        drop(original);
        assert_eq!(
            mcp.machine.job_request_delivery_state(&request_key),
            Some(crate::jobs::RequestDeliveryState::Pending)
        );

        for _ in 0..crate::jobs::MAX_RETAINED_JOBS {
            loop {
                match mcp.machine.start_shell_job(
                    ShellArgs {
                        command: "true".to_owned(),
                        dir: ".".to_owned(),
                        stdin: None,
                        timeout_secs: Some(2),
                        max_output_bytes: Some(1024),
                    },
                    false,
                    None,
                ) {
                    Ok(started) => {
                        // An abandoned local response is deliberately exposed as
                        // a compatibility job and therefore occupies one ordinary slot.
                        drop(started);
                        break;
                    }
                    Err(error) if error.to_string().contains("too many active shell jobs") => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("could not fill retained-job capacity: {error}"),
                }
            }
        }

        let command = TunnelCommand {
            request_id: request_id.to_owned(),
            shard_token: "test-shard".to_owned(),
            command_type: "jsonrpc".to_owned(),
            channel: Some("main".to_owned()),
            response_timeout: Some(json!("20s")),
            headers: BTreeMap::new(),
            jsonrpc: Some(json!({
                "jsonrpc":"2.0", "id":77, "method":"tools/call",
                "params": {
                    "name": crate::mcp::shell_tool_name(),
                    "arguments": {
                        "command": shell_args.command,
                        "dir": shell_args.dir,
                        "timeout_secs": shell_args.timeout_secs,
                        "max_output_bytes": shell_args.max_output_bytes
                    }
                }
            })),
        };
        let fingerprint =
            replay_request_fingerprint(command.jsonrpc.as_ref().unwrap(), &command.headers)
                .unwrap();
        client
            .process_command(mcp.clone(), command, Instant::now())
            .await
            .unwrap();

        assert_eq!(
            posts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a pre-commit redelivery error must not be published as this durable request's terminal response"
        );
        assert!(
            mcp.machine.has_job_request_delivery(&request_key),
            "a capacity-blocked redelivery must not consume the original execution key"
        );
        assert_eq!(
            mcp.machine.job_request_delivery_state(&request_key),
            Some(crate::jobs::RequestDeliveryState::Pending)
        );
        assert!(matches!(
            client
                .replays
                .claim(
                    &request_key,
                    client.request_namespace,
                    client.execution_authority,
                    fingerprint
                )
                .unwrap(),
            RequestReplayClaim::RecoverableShell
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lost_embedded_shell_response_recovers_from_inner_owner_and_finalizes_delivery() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = shell_enabled_test_machine(&root).await;
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let request_key = "lost-embedded-response";
        let mut jsonrpc = json!({
            "jsonrpc":"2.0", "id":91, "method":"tools/call",
            "params": {"name":crate::mcp::shell_tool_name(), "arguments":{"command":"printf recovered"}}
        });
        annotate_response_context(
            &mut jsonrpc,
            Duration::from_secs(20),
            Duration::from_secs(5),
            request_key,
            mcp.machine.transport_context_token(),
        )
        .unwrap();

        // Execute normally but intentionally discard the first embedded response,
        // modeling a post-execution response collection/streaming failure.
        let first = mcp.dispatch(&BTreeMap::new(), &jsonrpc).await.unwrap();
        assert_eq!(first.status, 200);
        assert!(mcp.machine.has_job_request_delivery(request_key));

        let (base_url, _) = fake_control_plane(|_| (StatusCode::OK, Duration::ZERO)).await;
        let client = TunnelClient::new(
            Config {
                base_url,
                ..test_config()
            },
            TunnelTuning::default(),
            CancellationToken::new(),
        )
        .unwrap();
        let command = TunnelCommand {
            request_id: "lost-embedded-response".to_owned(),
            shard_token: "test-shard".to_owned(),
            command_type: "jsonrpc".to_owned(),
            channel: Some("main".to_owned()),
            response_timeout: Some(json!("20s")),
            headers: BTreeMap::new(),
            jsonrpc: Some(jsonrpc.clone()),
        };
        let fingerprint = replay_request_fingerprint(&jsonrpc, &BTreeMap::new()).unwrap();
        let replay = ReplayRequestContext {
            command: &command,
            jsonrpc: &jsonrpc,
            request_key,
            namespace: client.request_namespace,
            fingerprint,
            dispatch_deadline: None,
            deadline: None,
        };
        let recovered = client
            .recovered_shell_terminal(&mcp, &replay)
            .unwrap()
            .expect("durable shell response should be recoverable");
        assert!(recovered.finalize_job_delivery);
        let body = serialize_tunnel_response(&recovered.response).unwrap();
        assert!(
            client
                .post_terminal_bytes(
                    &command,
                    body,
                    Some(Instant::now() + Duration::from_secs(1))
                )
                .await
                .unwrap()
        );
        mcp.machine.acknowledge_job_request_delivery(request_key);
        assert!(
            !mcp.machine.has_job_request_delivery(request_key),
            "positive delivery must retire the inner request key after recovered response delivery"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recoverable_shell_without_its_inner_job_owner_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let effect = root.join("must-not-run.txt");
        let machine = shell_enabled_test_machine(&root).await;
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let (base_url, _) = fake_control_plane(|_| (StatusCode::OK, Duration::ZERO)).await;
        let client = TunnelClient::new(
            Config {
                base_url,
                ..test_config()
            },
            TunnelTuning::default(),
            CancellationToken::new(),
        )
        .unwrap();
        let command = TunnelCommand {
            request_id: "lost-shell-owner".to_owned(),
            shard_token: "test-shard".to_owned(),
            command_type: "jsonrpc".to_owned(),
            channel: Some("main".to_owned()),
            response_timeout: Some(json!("20s")),
            headers: BTreeMap::new(),
            jsonrpc: Some(json!({
                "jsonrpc": "2.0",
                "id": 88,
                "method": "tools/call",
                "params": {
                    "name": crate::mcp::shell_tool_name(),
                    "arguments": {"command": format!("printf duplicate > '{}'", effect.display())}
                }
            })),
        };
        let key = durable_request_key(&client.request_namespace, &command.request_id);
        let fingerprint = replay_request_fingerprint(
            command.jsonrpc.as_ref().expect("jsonrpc"),
            &command.headers,
        )
        .unwrap();
        assert!(matches!(
            client
                .replays
                .claim(
                    &key,
                    client.request_namespace,
                    client.execution_authority,
                    fingerprint
                )
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        drop(ReplayLeaderGuard::new(
            client.replays.clone(),
            &key,
            client.request_namespace,
            fingerprint,
            true,
        ));
        assert!(matches!(
            client
                .replays
                .claim(
                    &key,
                    client.request_namespace,
                    client.execution_authority,
                    fingerprint
                )
                .unwrap(),
            RequestReplayClaim::RecoverableShell
        ));

        // This models Ctrl+R / authority replacement: the outer process replay
        // state survived, but the old JobManager session and its request key did
        // not. Re-dispatching the shell would repeat any side effects completed
        // before cancellation, so the request must become fail-closed instead.
        client
            .process_command(mcp, command, Instant::now())
            .await
            .unwrap();
        assert!(
            !effect.exists(),
            "shell was re-executed without its durable owner"
        );
        assert!(matches!(
            client
                .replays
                .claim(
                    &key,
                    client.request_namespace,
                    client.execution_authority,
                    fingerprint
                )
                .unwrap(),
            RequestReplayClaim::Delivered
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_tunnel_metadata_never_dispatches_a_side_effecting_tool() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mcp = EmbeddedMcp::new(
            shell_enabled_test_machine(&root).await,
            CancellationToken::new(),
        );
        let (base_url, posts) = fake_control_plane(|_| (StatusCode::OK, Duration::ZERO)).await;
        let client = TunnelClient::new(
            Config {
                base_url,
                ..test_config()
            },
            TunnelTuning::default(),
            CancellationToken::new(),
        )
        .unwrap();
        for (index, meta) in [json!([]), json!(false), json!("invalid"), json!(3)]
            .into_iter()
            .enumerate()
        {
            let effect = root.join(format!("invalid-meta-{index}"));
            let command = TunnelCommand {
                request_id: format!("bad-meta-{index}"),
                shard_token: "test-shard".to_owned(),
                command_type: "jsonrpc".to_owned(),
                channel: Some("main".to_owned()),
                response_timeout: Some(json!("20s")),
                headers: BTreeMap::new(),
                jsonrpc: Some(json!({
                    "jsonrpc": "2.0", "id": index + 1, "method": "tools/call",
                    "params": {
                        "name": crate::mcp::shell_tool_name(),
                        "arguments": {"command": format!("printf x > '{}'", effect.display())},
                        "_meta": meta
                    }
                })),
            };
            client
                .process_command(mcp.clone(), command, Instant::now())
                .await
                .unwrap();
            assert!(
                !effect.exists(),
                "malformed _meta dispatched a shell command"
            );
        }
        assert_eq!(posts.load(std::sync::atomic::Ordering::SeqCst), 4);
    }

    #[test]
    fn task_rpc_receives_authenticated_result_budget() {
        let mut call = json!({
            "jsonrpc":"2.0", "id":7, "method":"tasks/get",
            "params": {
                "taskId":"unknown",
                "_meta": {
                    "io.abird.dotlink/futureField":"attacker",
                    "client":"preserved"
                }
            }
        });
        assert_eq!(
            annotate_response_context(
                &mut call,
                Duration::from_secs(120),
                TunnelTuning::default().delivery_reserve,
                "unused-request-key",
                "task-token",
            ),
            Ok(None)
        );
        let meta = call["params"]["_meta"].as_object().unwrap();
        assert_eq!(
            meta.get(crate::mcp::TRANSPORT_CONTEXT_TOKEN_META_KEY),
            Some(&json!("task-token"))
        );
        assert_eq!(
            meta.get(crate::mcp::RESULT_BUDGET_META_KEY),
            Some(&json!(
                crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES as u64
            ))
        );
        assert!(!meta.contains_key("io.abird.dotlink/futureField"));
        assert_eq!(meta.get("client"), Some(&json!("preserved")));
    }

    #[tokio::test]
    async fn task_rpc_budget_reaches_the_real_handler_and_bounds_unknown_ids() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = crate::mcp::test_machine(root);
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let token = mcp.machine.transport_context_token().to_owned();
        let task_id = "x".repeat(1024);
        let call = json!({
            "jsonrpc":"2.0", "id":77, "method":"tasks/get",
            "params": {
                "taskId": task_id.clone(),
                "_meta": {
                    crate::mcp::TRANSPORT_CONTEXT_TOKEN_META_KEY: token,
                    crate::mcp::RESULT_BUDGET_META_KEY: 512,
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {"name":"task-budget-test","version":"1"},
                    "io.modelcontextprotocol/clientCapabilities": {
                        "extensions": {"io.modelcontextprotocol/tasks": {}}
                    }
                }
            }
        });
        let headers = BTreeMap::from([
            (
                "mcp-protocol-version".to_owned(),
                vec!["2026-07-28".to_owned()],
            ),
            ("mcp-method".to_owned(), vec!["tasks/get".to_owned()]),
            ("mcp-name".to_owned(), vec![task_id]),
        ]);
        let response = mcp.dispatch(&headers, &call).await.unwrap();
        let message = response
            .messages
            .iter()
            .find(|message| message.get("id") == Some(&json!(77)))
            .unwrap();
        assert_eq!(message["error"]["code"], -32602, "{message}");
        assert!(
            serde_json::to_vec(&message["error"]).unwrap().len() <= 512,
            "{}",
            serde_json::to_vec(&message["error"]).unwrap().len()
        );
        assert!(
            message["error"]["message"]
                .as_str()
                .unwrap()
                .contains("transport response budget"),
            "{message}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn descriptor_rooted_native_tools_round_trip_through_mcp() {
        use crate::mcp::{AccessSpec, MachineConfig};
        use std::fs;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let denied = root.join("denied");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&denied).unwrap();
        let protected = root.join("protected");
        fs::create_dir(&protected).unwrap();
        fs::write(protected.join("private.txt"), "private").unwrap();
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root.clone()],
                write_roots: vec![root.clone()],
                deny_read_roots: vec![denied.clone()],
                deny_write_roots: vec![denied.clone()],
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 5,
            max_output_bytes: 1024,
            max_read_bytes: 1024,
            max_write_bytes: 1024,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: vec![protected],
        })
        .await
        .unwrap();
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let mut index = 0;
        let mut call = |name: &str, args: Value| {
            index += 1;
            let command = json!({
                "jsonrpc": "2.0", "id": index, "method": "tools/call",
                "params": {"name": name, "arguments": args}
            });
            let mcp = mcp.clone();
            async move {
                let result = mcp.dispatch(&BTreeMap::new(), &command).await.unwrap();
                assert_eq!(result.status, 200);
                result.messages.into_iter().next().unwrap()
            }
        };
        let written = call(
            "write",
            json!({"path":"nested/test.txt", "content":"first"}),
        )
        .await;
        assert_ne!(written["result"]["isError"], true, "{written}");
        assert_eq!(
            fs::read_to_string(root.join("nested/test.txt")).unwrap(),
            "first"
        );

        let read = call("read", json!({"path":"nested/test.txt"})).await;
        assert!(read.to_string().contains("first"));
        let edited = call(
            "edit",
            json!({"path":"nested/test.txt", "edits":[{"old_text":"first", "new_text":"second"}]}),
        )
        .await;
        assert_ne!(edited["result"]["isError"], true, "{edited}");
        assert_eq!(
            fs::read_to_string(root.join("nested/test.txt")).unwrap(),
            "second"
        );
        let grepped = call("grep", json!({"pattern":"seco", "path":"nested"})).await;
        assert_ne!(grepped["result"]["isError"], true, "{grepped}");
        assert!(
            grepped.to_string().contains("nested/test.txt:1: second"),
            "{grepped}"
        );
        let grep_count = call(
            "grep",
            json!({"pattern":"seco", "path":"nested", "output_mode":"count"}),
        )
        .await;
        assert_ne!(grep_count["result"]["isError"], true, "{grep_count}");
        let counted: Value =
            serde_json::from_str(grep_count["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(counted["page_total"], serde_json::json!(1));

        // Multi-file edit in one call, with the top-level default path unused
        // because every entry names its own file.
        let prepared = call(
            "write",
            json!({"path":"nested/other.txt", "content":"other"}),
        )
        .await;
        assert_ne!(prepared["result"]["isError"], true, "{prepared}");
        let multi = call(
            "edit",
            json!({"edits":[
                {"path":"nested/test.txt", "old_text":"second", "new_text":"second!"},
                {"path":"nested/other.txt", "old_text":"other", "new_text":"another"}
            ]}),
        )
        .await;
        assert_ne!(multi["result"]["isError"], true, "{multi}");
        assert_eq!(
            fs::read_to_string(root.join("nested/test.txt")).unwrap(),
            "second!"
        );
        assert_eq!(
            fs::read_to_string(root.join("nested/other.txt")).unwrap(),
            "another"
        );

        // Patch dispatch: one Update section, planned and written atomically.
        let patched_file = call(
            "patch",
            json!({"patch":"*** Begin Patch\n*** Update File: nested/test.txt\n@@\n-second!\n+second! patched\n*** End Patch"}),
        )
        .await;
        assert_ne!(patched_file["result"]["isError"], true, "{patched_file}");
        let patch_receipt: Value = serde_json::from_str(
            patched_file["result"]["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(patch_receipt["mode"], "strict");
        assert_eq!(
            fs::read_to_string(root.join("nested/test.txt")).unwrap(),
            "second! patched"
        );

        // Preserve existing in-grant symlink alias behavior while rejecting
        // aliases whose canonical target lies in an explicitly denied tree.
        std::os::unix::fs::symlink(root.join("nested/test.txt"), root.join("alias")).unwrap();
        let linked = call("read", json!({"path":"alias"})).await;
        assert!(linked.to_string().contains("second"));
        fs::write(denied.join("secret"), "blocked").unwrap();
        std::os::unix::fs::symlink(denied.join("secret"), root.join("bad-alias")).unwrap();
        let refused_alias = call("read", json!({"path":"bad-alias"})).await;
        assert_eq!(refused_alias["result"]["isError"], true);

        let binary = call(
            "bread",
            json!({"path":"nested/test.txt", "format":"base64"}),
        )
        .await;
        assert!(binary.to_string().contains("c2Vjb25k"));
        let bwrite = call(
            "bwrite",
            json!({"path":"nested/data.bin", "data":"YWJjZA=="}),
        )
        .await;
        assert_ne!(bwrite["result"]["isError"], true, "{bwrite}");
        let patched = call(
            "bedit",
            json!({"path":"nested/data.bin", "edits":[{"offset":1, "length":2, "data":"WFk="}]}),
        )
        .await;
        assert_ne!(patched["result"]["isError"], true, "{patched}");
        assert_eq!(fs::read(root.join("nested/data.bin")).unwrap(), b"aXYd");
        fs::create_dir(root.join("nested/deeper")).unwrap();
        fs::write(root.join("nested/deeper/child.txt"), b"child").unwrap();
        let listed = call("ls", json!({"path":"nested", "depth":5})).await;
        let listing: Value =
            serde_json::from_str(listed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(listing["truncated"], Value::Bool(false));
        let listed_paths: Vec<&str> = listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect();
        assert!(
            listed_paths.contains(&"nested/test.txt"),
            "{listed_paths:?}"
        );
        assert!(
            listed_paths.contains(&"nested/data.bin"),
            "{listed_paths:?}"
        );
        assert!(
            listed_paths.contains(&"nested/deeper/child.txt"),
            "{listed_paths:?}"
        );
        assert!(
            listed_paths
                .iter()
                .all(|path| !path.starts_with("/proc/self/fd/"))
        );
        let limited = call("ls", json!({"path":"nested", "limit":2})).await;
        let limited_payload: Value =
            serde_json::from_str(limited["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(limited_payload["truncated"], Value::Bool(true));
        let limited_entries = limited_payload["entries"].as_array().unwrap();
        assert_eq!(limited_entries.len(), 2);
        assert!(
            limited_entries[0]["path"].as_str().unwrap()
                <= limited_entries[1]["path"].as_str().unwrap()
        );

        let root_listing = call("ls", json!({"path":".", "depth":10})).await;
        let root_payload: Value = serde_json::from_str(
            root_listing["result"]["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let entries = root_payload["entries"].as_array().unwrap();
        assert!(
            entries
                .iter()
                .all(|entry| !entry["path"].as_str().unwrap().starts_with("denied"))
        );
        assert!(
            entries
                .iter()
                .all(|entry| !entry["path"].as_str().unwrap().starts_with("protected")),
            "protected paths must not appear in directory listings"
        );

        let refused = call("write", json!({"path":"denied/secret", "content":"bad"})).await;
        assert_eq!(refused["result"]["isError"], true, "{refused}");
        assert_eq!(
            fs::read_to_string(denied.join("secret")).unwrap(),
            "blocked"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mcp_shells_are_parallel_by_default_and_serial_when_requested() {
        use crate::mcp::{AccessSpec, MachineConfig};
        use std::fs;

        let sandbox_modes: &[bool] = if std::env::var_os("DOTLINK_TEST_BWRAP").is_some() {
            &[false, true]
        } else {
            &[false]
        };
        for &sandbox_shell in sandbox_modes {
            for serialize in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path().canonicalize().unwrap();
                let events = root.join("events");
                let machine = LocalMachine::new(MachineConfig {
                    access: AccessSpec {
                        base_dir: root.clone(),
                        read_roots: vec![root.clone()],
                        write_roots: vec![root.clone()],
                        deny_read_roots: Vec::new(),
                        deny_write_roots: Vec::new(),
                        unrestricted_fs: false,
                    },
                    cache_mounts: Vec::new(),
                    shell_env: Vec::new(),
                    #[cfg(target_os = "linux")]
                    host_shell: crate::host_services::HostShell::default(),
                    log: LogConfig::default(),
                    serialize,
                    allow_shell: true,
                    // Exercise both direct test shells and real Bubblewrap
                    // concurrency under DOTLINK_TEST_BWRAP.
                    sandbox_shell,
                    allow_network: false,
                    max_shell_timeout_secs: 10,
                    max_output_bytes: 1024,
                    max_read_bytes: 1024,
                    max_write_bytes: 1024,

                    task_result_budget: None,
                    job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

                    job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
                    protected_paths: Vec::new(),
                })
                .await
                .unwrap();
                let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
                let request = |id: i64, name: &str| {
                    let command = if serialize {
                        format!(
                            "printf 'start-{name}\\n' >> '{}' ; sleep 0.3 ; printf 'end-{name}\\n' >> '{}'",
                            events.display(),
                            events.display(),
                        )
                    } else {
                        // Make both starts observable before either end. A
                        // fixed sleep is flaky when a loaded host delays the
                        // second spawn past it.
                        let other = if name == "a" { "b" } else { "a" };
                        format!(
                            "printf 'start-{name}\\n' >> '{}'; touch '{}'; i=0; while [ ! -f '{}' ] && [ $i -lt 1000 ]; do sleep 0.01; i=$((i+1)); done; printf 'end-{name}\\n' >> '{}'",
                            events.display(),
                            root.join(format!("ready-{name}")).display(),
                            root.join(format!("ready-{other}")).display(),
                            events.display(),
                        )
                    };
                    json!({
                        "jsonrpc": "2.0", "id": id, "method": "tools/call",
                        "params": {"name": "bash", "arguments": {"command": command}}
                    })
                };
                let req_a = request(1, "a");
                let req_b = request(2, "b");
                let headers = BTreeMap::new();
                let (a, b) = tokio::join!(
                    mcp.dispatch(&headers, &req_a),
                    mcp.dispatch(&headers, &req_b)
                );
                for response in [a.unwrap(), b.unwrap()] {
                    assert_eq!(response.status, 200, "{response:?}");
                    let terminal = response.messages.iter().find(|message| {
                        message.get("result").is_some() || message.get("error").is_some()
                    });
                    let result = terminal.expect("shell call must produce a terminal response");
                    assert!(result.get("error").is_none(), "shell RPC failed: {result}");
                    let text = result
                        .pointer("/result/content/0/text")
                        .and_then(Value::as_str)
                        .expect("shell result must contain its JSON summary");
                    let shell: Value = serde_json::from_str(text).expect("valid shell summary");
                    assert_eq!(shell["success"], true, "shell failed: {shell}");
                }
                let log = fs::read_to_string(&events).unwrap_or_else(|error| {
                    panic!("missing shell events {}: {error}", events.display())
                });
                let entries: Vec<_> = log.lines().collect();
                assert_eq!(entries.len(), 4, "unexpected events: {log}");
                if serialize {
                    assert!(entries[0].starts_with("start-"));
                    assert!(entries[1].starts_with("end-"));
                    assert_eq!(&entries[0][6..], &entries[1][4..]);
                } else {
                    assert!(entries[0].starts_with("start-"), "{log}");
                    assert!(entries[1].starts_with("start-"), "{log}");
                }
            }
        }
    }

    #[tokio::test]
    async fn embedded_binary_read_uses_the_hosted_envelope_above_the_raw_tool_ceiling() {
        use crate::mcp::{AccessSpec, MachineConfig};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let fits_after_base64 = 6 * 1024 * 1024;
        let exceeds_after_base64 = 8 * 1024 * 1024;
        std::fs::write(root.join("fits.bin"), vec![b'z'; fits_after_base64]).unwrap();
        std::fs::write(root.join("too-large.bin"), vec![b'z'; exceeds_after_base64]).unwrap();
        let machine = LocalMachine::new(MachineConfig {
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
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 10,
            max_output_bytes: 1024 * 1024,
            max_read_bytes: crate::limits::OPENAI_TUNNEL_TRANSFER_CEILING_BYTES,
            max_write_bytes: 1024 * 1024,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,
            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let embedded = EmbeddedMcp::new(machine, CancellationToken::new());
        let transport_token = embedded.machine.transport_context_token().to_owned();
        assert_eq!(
            embedded.max_response_bytes,
            crate::limits::OPENAI_TUNNEL_MCP_MESSAGE_BYTES
        );

        let response = embedded
            .dispatch(
                &BTreeMap::new(),
                &json!({
                    "jsonrpc": "2.0", "id": 42, "method": "tools/call",
                    "params": {"_meta": {
                        crate::mcp::TRANSPORT_CONTEXT_TOKEN_META_KEY: transport_token,
                        crate::mcp::RESULT_BUDGET_META_KEY: crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES
                    }, "name": "bread", "arguments": {
                        "path": "fits.bin", "format": "base64", "limit": fits_after_base64}}
                }),
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        let result = response
            .messages
            .iter()
            .find(|value| value.get("id") == Some(&json!(42)))
            .expect("binary MCP response");
        assert_ne!(result["result"]["isError"], true);

        let response = embedded
            .dispatch(
                &BTreeMap::new(),
                &json!({
                    "jsonrpc": "2.0", "id": 43, "method": "tools/call",
                    "params": {"_meta": {
                        crate::mcp::TRANSPORT_CONTEXT_TOKEN_META_KEY: transport_token,
                        crate::mcp::RESULT_BUDGET_META_KEY: crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES
                    }, "name": "bread", "arguments": {
                        "path": "too-large.bin", "format": "base64", "limit": exceeds_after_base64}}
                }),
            )
            .await
            .unwrap();
        let result = response
            .messages
            .iter()
            .find(|value| value.get("id") == Some(&json!(43)))
            .expect("oversized binary tool response");
        assert_eq!(result["result"]["isError"], true);
        let text = result["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("cannot fit this encoding"), "{text}");
        assert!(
            text.contains(&crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES.to_string()),
            "{text}"
        );
    }

    #[tokio::test]
    async fn local_response_collection_stops_at_the_byte_limit() {
        let small = Full::new(Bytes::from_static(b"ok"));
        assert_eq!(
            bounded::local_response(small, 2, "local MCP response")
                .await
                .unwrap(),
            Bytes::from_static(b"ok")
        );

        let large = Full::new(Bytes::from(vec![b'x'; 1024]));
        let error = bounded::local_response(large, 16, "local MCP response")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("16 bytes"), "{error}");
    }

    #[test]
    fn synthesized_jsonrpc_errors_bound_untrusted_ids() {
        let huge = "x".repeat(super::super::MAX_STANDARD_ROUTING_VALUE_BYTES + 1);
        for id in [Value::String(huge), json!({"bad": "id"}), json!(["bad-id"])] {
            let request = json!({"jsonrpc":"2.0", "id":id, "method":"tools/list"});
            let error = synthesized_jsonrpc_error(&request, "test");
            assert_eq!(error["id"], Value::Null);
            assert!(serde_json::to_vec(&error).unwrap().len() < 4096);
        }

        let request = json!({"jsonrpc":"2.0", "id":42, "method":"tools/list"});
        assert_eq!(synthesized_jsonrpc_error(&request, "test")["id"], 42);
    }

    #[tokio::test]
    async fn oversized_sep_routing_values_fail_compactly_before_rmcp_can_reflect_them() {
        const VERSION: &str = "2026-07-28";
        let temp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::test_machine(temp.path().canonicalize().unwrap());
        let embedded = EmbeddedMcp::new(machine, CancellationToken::new());
        let huge = "x".repeat(super::super::MAX_STANDARD_ROUTING_VALUE_BYTES + 1024);

        let task = json!({
            "jsonrpc":"2.0", "id":41, "method":"tasks/get",
            "params": {
                "taskId": huge,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": VERSION,
                    "io.modelcontextprotocol/clientInfo": {"name":"routing-test","version":"1"},
                    "io.modelcontextprotocol/clientCapabilities": {
                        "extensions": {"io.modelcontextprotocol/tasks": {}}
                    }
                }
            }
        });
        let task_headers = BTreeMap::from([
            ("mcp-protocol-version".to_owned(), vec![VERSION.to_owned()]),
            ("mcp-method".to_owned(), vec!["tasks/get".to_owned()]),
            ("mcp-name".to_owned(), vec!["small-mismatch".to_owned()]),
        ]);
        let response = embedded.dispatch(&task_headers, &task).await.unwrap();
        let encoded = serde_json::to_vec(&response.messages).unwrap();
        assert_eq!(response.status, StatusCode::BAD_REQUEST.as_u16());
        assert_eq!(response.messages[0]["error"]["code"], -32602);
        assert!(
            encoded.len() < 4096,
            "oversized task routing leaked into response"
        );
        assert!(String::from_utf8_lossy(&encoded).contains("routing value exceeds"));
        assert!(!String::from_utf8_lossy(&encoded).contains(&"x".repeat(1024)));

        let oversized_method_call = json!({
            "jsonrpc":"2.0", "id":44, "method": huge.clone(),
            "params": {}
        });
        let response = embedded
            .dispatch(&BTreeMap::new(), &oversized_method_call)
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::BAD_REQUEST.as_u16());
        assert_eq!(response.messages[0]["error"]["code"], -32602);
        assert!(
            response.messages[0]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("routing value exceeds")
        );

        let oversized_meta = json!({
            "jsonrpc":"2.0", "id":45, "method":"tools/list",
            "params": {"_meta": {
                "io.modelcontextprotocol/protocolVersion": huge.clone(),
                "io.modelcontextprotocol/clientInfo": {"name":"routing-test","version":"1"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }}
        });
        let response = embedded
            .dispatch(&BTreeMap::new(), &oversized_meta)
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::BAD_REQUEST.as_u16());
        assert_eq!(response.messages[0]["error"]["code"], -32602);

        let oversized_id = json!({
            "jsonrpc":"2.0", "id": huge.clone(), "method":"tools/list",
            "params": {}
        });
        let response = embedded
            .dispatch(&BTreeMap::new(), &oversized_id)
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::BAD_REQUEST.as_u16());
        assert_eq!(response.messages[0]["id"], Value::Null);
        assert_eq!(response.messages[0]["error"]["code"], -32602);
        let encoded_id = serde_json::to_vec(&response.messages).unwrap();
        assert!(encoded_id.len() < 4096);
        assert!(!String::from_utf8_lossy(&encoded_id).contains(&"x".repeat(1024)));

        let method_headers = BTreeMap::from([
            ("mcp-protocol-version".to_owned(), vec![VERSION.to_owned()]),
            ("mcp-method".to_owned(), vec![huge.clone()]),
        ]);
        let method_call = json!({
            "jsonrpc":"2.0", "id":42, "method":"tools/list",
            "params": {"_meta": {
                "io.modelcontextprotocol/protocolVersion": VERSION,
                "io.modelcontextprotocol/clientInfo": {"name":"routing-test","version":"1"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }}
        });
        let response = embedded
            .dispatch(&method_headers, &method_call)
            .await
            .unwrap();
        let encoded = serde_json::to_vec(&response.messages).unwrap();
        assert!(encoded.len() < 4096);
        assert!(String::from_utf8_lossy(&encoded).contains("missing required Mcp-Method"));

        let protocol_headers = BTreeMap::from([
            ("mcp-protocol-version".to_owned(), vec![huge.clone()]),
            ("mcp-method".to_owned(), vec!["tools/list".to_owned()]),
        ]);
        let response = embedded
            .dispatch(&protocol_headers, &method_call)
            .await
            .unwrap();
        let encoded = serde_json::to_vec(&response.messages).unwrap();
        assert!(encoded.len() < 4096);
        assert!(String::from_utf8_lossy(&encoded).contains("requires MCP-Protocol-Version header"));

        let invalid_headers = BTreeMap::from([
            ("mcp-protocol-version".to_owned(), vec![VERSION.to_owned()]),
            (
                "mcp-method".to_owned(),
                vec!["tools/list\nforged".to_owned()],
            ),
        ]);
        let response = embedded
            .dispatch(&invalid_headers, &method_call)
            .await
            .unwrap();
        let encoded = serde_json::to_vec(&response.messages).unwrap();
        assert!(encoded.len() < 4096);
        assert!(String::from_utf8_lossy(&encoded).contains("missing required Mcp-Method"));
    }

    #[tokio::test]
    async fn oversized_initialize_protocol_version_is_rejected_compactly_before_rmcp() {
        let temp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::test_machine(temp.path().canonicalize().unwrap());
        let embedded = EmbeddedMcp::new(machine, CancellationToken::new());
        let huge = "v".repeat(super::super::MAX_STANDARD_ROUTING_VALUE_BYTES + 1024);
        let request = json!({
            "jsonrpc":"2.0", "id":43, "method":"initialize",
            "params": {
                "protocolVersion": huge,
                "capabilities": {},
                "clientInfo": {"name":"routing-test","version":"1"}
            }
        });
        let headers = BTreeMap::from([("mcp-protocol-version".to_owned(), vec![huge])]);
        let response = embedded.dispatch(&headers, &request).await.unwrap();
        let encoded = serde_json::to_vec(&response.messages).unwrap();
        assert_eq!(response.status, StatusCode::BAD_REQUEST.as_u16());
        assert_eq!(response.messages[0]["error"]["code"], -32602);
        assert!(encoded.len() < 16 * 1024, "{response:?}");
        assert!(String::from_utf8_lossy(&encoded).contains("routing value exceeds"));
        assert!(!String::from_utf8_lossy(&encoded).contains(&"v".repeat(1024)));
    }

    #[tokio::test]
    async fn embedded_standard_unknown_tool_is_canonicalized_before_rmcp() {
        use base64::{Engine as _, prelude::BASE64_STANDARD};

        const VERSION: &str = "2026-07-28";
        let temp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::test_machine(temp.path().canonicalize().unwrap());
        let embedded = EmbeddedMcp::new(machine, CancellationToken::new());
        let unknown_name = "attacker controlled ☃";
        let encoded_name = format!("=?base64?{}?=", BASE64_STANDARD.encode(unknown_name));
        let headers = BTreeMap::from([
            ("mcp-protocol-version".to_owned(), vec![VERSION.to_owned()]),
            ("mcp-method".to_owned(), vec!["tools/call".to_owned()]),
            ("mcp-name".to_owned(), vec![encoded_name]),
        ]);
        let response = embedded
            .dispatch(
                &headers,
                &json!({
                    "jsonrpc": "2.0",
                    "id": "original-id",
                    "method": "tools/call",
                    "params": {
                        "name": unknown_name,
                        "arguments": {},
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": VERSION,
                            "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1.0"},
                            "io.modelcontextprotocol/clientCapabilities": {}
                        }
                    }
                }),
            )
            .await
            .unwrap();

        assert_eq!(response.status, StatusCode::BAD_REQUEST.as_u16());
        let error = response
            .messages
            .iter()
            .find(|message| message.get("id") == Some(&json!("original-id")))
            .expect("rmcp correlated unknown-tool response");
        assert_eq!(error["error"]["code"], -32602);
        assert!(
            !serde_json::to_string(&response.messages)
                .unwrap()
                .contains(unknown_name)
        );

        // Missing SEP metadata is still rmcp's header-mismatch response; the
        // adapter must not synthesize or bypass it during canonicalization.
        let missing_name = BTreeMap::from([
            ("mcp-protocol-version".to_owned(), vec![VERSION.to_owned()]),
            ("mcp-method".to_owned(), vec!["tools/call".to_owned()]),
        ]);
        let response = embedded
            .dispatch(
                &missing_name,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": unknown_name,
                        "arguments": {},
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": VERSION,
                            "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1.0"},
                            "io.modelcontextprotocol/clientCapabilities": {}
                        }
                    }
                }),
            )
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::BAD_REQUEST.as_u16());
        assert_eq!(response.messages[0]["id"], 2);
        assert_eq!(response.messages[0]["error"]["code"], -32020);
    }

    #[test]
    fn tunnel_metadata_labels_reject_control_sequences_and_oversize_values() {
        let malicious = format!("prefix\n\u{1b}[31m{}", "x".repeat(200));
        let command = TunnelCommand {
            request_id: malicious.clone(),
            shard_token: "not-logged".to_owned(),
            command_type: malicious.clone(),
            channel: Some(malicious.clone()),
            response_timeout: None,
            headers: BTreeMap::new(),
            jsonrpc: None,
        };
        assert_eq!(command.request_id_label(), "invalid-request-id");
        assert_eq!(command.channel_label(), "invalid-channel");
        assert_eq!(command.command_type_label(), "invalid-command-type");
        for label in [
            command.request_id_label(),
            command.channel_label(),
            command.command_type_label(),
        ] {
            assert!(!label.contains('\n'));
            assert!(!label.contains('\u{1b}'));
            assert!(!label.contains("prefix"));
            assert!(label.len() <= 96);
        }
    }

    #[test]
    fn malformed_sse_and_control_plane_errors_do_not_echo_bodies() {
        let body = b"data: SECRET\x1b[31mCONTROL\n\n";
        let error = parse_sse_messages(body).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("invalid JSON in local MCP SSE event"));
        assert!(!message.contains("SECRET"));
        assert!(!message.contains("CONTROL"));
        assert!(!message.contains('\u{1b}'));

        let message = control_plane_status_error("poll", StatusCode::BAD_GATEWAY).to_string();
        assert_eq!(
            message,
            "OpenAI tunnel poll failed with HTTP status 502 Bad Gateway"
        );
    }

    #[test]
    fn response_retry_classifier_covers_all_server_errors() {
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            assert!(retryable_response_status(status), "{status}");
        }
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::BAD_REQUEST,
        ] {
            assert!(!retryable_response_status(status), "{status}");
        }
    }

    #[test]
    fn equivalent_control_plane_urls_share_durable_request_namespace() {
        let first = canonical_base_url("https://API.OpenAI.com:443/v1/../control///").unwrap();
        let second = canonical_base_url("https://api.openai.com/control").unwrap();
        assert_eq!(first, second);
        let first = tunnel_identity([Some(&first), Some("tunnel"), None, Some("org")]);
        let second = tunnel_identity([Some(&second), Some("tunnel"), None, Some("org")]);
        assert_eq!(first, second);
        assert_eq!(
            durable_request_key(&first, "request"),
            durable_request_key(&second, "request")
        );
    }

    #[test]
    fn durable_request_keys_are_fixed_width_and_namespace_scoped() {
        let namespace_a = tunnel_identity([Some("https://a"), Some("tunnel"), None, None]);
        let namespace_b = tunnel_identity([Some("https://b"), Some("tunnel"), None, None]);
        let a = durable_request_key(&namespace_a, "request");
        let b = durable_request_key(&namespace_b, "request");
        assert_ne!(a, b, "different tunnel namespaces must not deduplicate");
        assert_eq!(a.len(), 64);
        assert_eq!(a, durable_request_key(&namespace_a, "request"));

        // Credential rotation intentionally does not participate in the request
        // namespace; a late redelivery from the same endpoint/tunnel/org must
        // still find the old exactly-once replay state after a key rotation.
        let with_key_one =
            tunnel_identity([Some("https://a"), Some("tunnel"), Some("key-one"), None]);
        let with_key_two =
            tunnel_identity([Some("https://a"), Some("tunnel"), Some("key-two"), None]);
        assert_ne!(with_key_one, with_key_two);
        assert_eq!(
            namespace_a,
            tunnel_identity([Some("https://a"), Some("tunnel"), None, None])
        );
    }

    #[test]
    fn poll_ceiling_is_the_hosted_serialized_message_limit() {
        assert_eq!(
            poll_response_ceiling(),
            crate::limits::OPENAI_TUNNEL_SERIALIZED_MESSAGE_BYTES
        );
    }

    #[test]
    fn eight_mib_ascii_write_fits_embedded_and_hosted_envelopes() {
        let content = "a".repeat(crate::limits::OPENAI_TUNNEL_TRANSFER_CEILING_BYTES);
        let jsonrpc = json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"tools/call",
            "params":{"name":"write","arguments":{"path":"large.txt","content":content}}
        });
        let inner = serde_json::to_vec(&jsonrpc).unwrap().len();
        assert!(
            inner <= crate::limits::OPENAI_TUNNEL_MCP_MESSAGE_BYTES,
            "8 MiB raw write serialized to {inner} bytes"
        );
        let envelope = json!({
            "commands":[{
                "request_id":"request",
                "shard_token":"shard",
                "command_type":"jsonrpc",
                "channel":"main",
                "response_timeout":"120s",
                "headers":{},
                "jsonrpc":jsonrpc
            }]
        });
        let outer = serde_json::to_vec(&envelope).unwrap().len();
        assert!(
            outer <= crate::limits::OPENAI_TUNNEL_SERIALIZED_MESSAGE_BYTES,
            "8 MiB raw write tunnel envelope serialized to {outer} bytes"
        );
    }

    #[test]
    fn cancelled_replay_leader_is_retryable_only_with_a_durable_inner_owner() {
        let namespace = [1; 32];
        let fingerprint = [2; 32];

        let mutator = RequestReplayLedger::default();
        mutator.activate_scope(namespace, [0; 32]);
        assert!(matches!(
            mutator
                .claim("mutator", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        drop(ReplayLeaderGuard::new(
            mutator.clone(),
            "mutator",
            namespace,
            fingerprint,
            false,
        ));
        assert!(matches!(
            mutator
                .claim("mutator", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Uncertain(_)
        ));
        assert_eq!(mutator.slot().reserved_bytes, 0);

        let shell = RequestReplayLedger::default();
        shell.activate_scope(namespace, [0; 32]);
        assert!(matches!(
            shell
                .claim("shell", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        drop(ReplayLeaderGuard::new(
            shell.clone(),
            "shell",
            namespace,
            fingerprint,
            true,
        ));
        assert!(matches!(
            shell
                .claim("shell", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::RecoverableShell
        ));
        assert_eq!(shell.slot().reserved_bytes, 0);
        assert!(matches!(
            shell
                .resume_recoverable_shell("shell", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        assert_eq!(
            shell.slot().reserved_bytes,
            REQUEST_REPLAY_RESERVATION_BYTES
        );
    }

    #[test]
    fn replay_ledger_reserves_worst_case_before_execution_and_shrinks_on_completion() {
        let ledger = RequestReplayLedger::default();
        let namespace = [3; 32];
        let fingerprint = [4; 32];
        ledger.activate_scope(namespace, [0; 32]);
        let worst_case_slots = MAX_REQUEST_REPLAY_BYTES / REQUEST_REPLAY_RESERVATION_BYTES;
        assert!(worst_case_slots >= TunnelTuning::default().max_in_flight);
        for index in 0..worst_case_slots {
            assert!(matches!(
                ledger
                    .claim(&format!("request-{index}"), namespace, [0; 32], fingerprint)
                    .unwrap(),
                RequestReplayClaim::Leader
            ));
        }
        let error = match ledger.claim("overflow", namespace, [0; 32], fingerprint) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("worst-case replay reservation admitted past the safety ceiling"),
        };
        assert!(error.contains("replay memory"), "{error}");

        ledger
            .complete(
                "request-0",
                namespace,
                fingerprint,
                ReplayTerminal {
                    response: TunnelResponse {
                        request_id: "request-0".to_owned(),
                        channel: Some("main".to_owned()),
                        resp_json: Some(json!({"jsonrpc":"2.0","id":1,"result":{}})),
                        resp_headers: BTreeMap::new(),
                        resp_code: 200,
                        resp_type: "jsonrpc_response".to_owned(),
                    },
                    finalize_job_delivery: false,
                    requires_job_owner: false,
                    shell_guard_generation: None,
                    recoverable_shell: false,
                },
            )
            .unwrap();
        {
            let state = ledger.slot();
            assert!(
                state.reserved_bytes
                    < worst_case_slots.saturating_mul(REQUEST_REPLAY_RESERVATION_BYTES),
                "small terminal response must release unused worst-case reservation"
            );
        }
        assert!(matches!(
            ledger
                .claim("after-small-response", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
    }

    #[test]
    fn expired_ready_body_releases_memory_but_keeps_fail_closed_identity() {
        let ledger = RequestReplayLedger::default();
        let namespace = [8; 32];
        let fingerprint = [9; 32];
        ledger.activate_scope(namespace, [0; 32]);
        assert!(matches!(
            ledger
                .claim("stale-ready", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        ledger
            .complete(
                "stale-ready",
                namespace,
                fingerprint,
                ReplayTerminal {
                    response: TunnelResponse {
                        request_id: "stale-ready".to_owned(),
                        channel: Some("main".to_owned()),
                        resp_json: Some(json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}})),
                        resp_headers: BTreeMap::new(),
                        resp_code: 200,
                        resp_type: "jsonrpc_response".to_owned(),
                    },
                    finalize_job_delivery: false,
                    requires_job_owner: false,
                    shell_guard_generation: None,
                    recoverable_shell: false,
                },
            )
            .unwrap();
        assert!(ledger.slot().reserved_bytes > 0);

        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        {
            let mut state = ledger.slot();
            // Model enough undeliverable responses to consume the entire replay
            // byte budget without actually allocating hundreds of MiB in this test.
            state.reserved_bytes = MAX_REQUEST_REPLAY_BYTES;
            let record = state.entries.get_mut("stale-ready").unwrap();
            let RequestReplayEntry::Ready {
                reserved_bytes,
                body_until,
                ..
            } = &mut record.state
            else {
                panic!("completed replay did not become Ready");
            };
            *reserved_bytes = MAX_REQUEST_REPLAY_BYTES;
            *body_until = past;
            state.expirations.push_front(ReplayExpiration {
                until: past,
                key: "stale-ready".to_owned(),
                kind: ReplayExpirationKind::DropReadyBody,
            });
        }

        assert!(matches!(
            ledger
                .claim("stale-ready", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Uncertain(_)
        ));
        assert_eq!(ledger.slot().reserved_bytes, 0);
        assert!(matches!(
            ledger.claim("fresh", namespace, [0; 32], [10; 32]).unwrap(),
            RequestReplayClaim::Leader
        ));
    }

    #[test]
    fn expired_committed_shell_body_releases_memory_and_falls_back_to_inner_owner() {
        let ledger = RequestReplayLedger::default();
        let namespace = [18; 32];
        let fingerprint = [19; 32];
        ledger.activate_scope(namespace, [0; 32]);
        assert!(matches!(
            ledger
                .claim("shell-ready", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        ledger
            .complete(
                "shell-ready",
                namespace,
                fingerprint,
                ReplayTerminal {
                    response: TunnelResponse {
                        request_id: "shell-ready".to_owned(),
                        channel: Some("main".to_owned()),
                        resp_json: Some(
                            json!({"jsonrpc":"2.0","id":1,"result":{"job_id":"owned"}}),
                        ),
                        resp_headers: BTreeMap::new(),
                        resp_code: 200,
                        resp_type: "jsonrpc_response".to_owned(),
                    },
                    finalize_job_delivery: true,
                    requires_job_owner: true,
                    shell_guard_generation: None,
                    recoverable_shell: false,
                },
            )
            .unwrap();
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        {
            let mut state = ledger.slot();
            let record = state.entries.get_mut("shell-ready").unwrap();
            let RequestReplayEntry::Ready { body_until, .. } = &mut record.state else {
                panic!("completed shell replay did not become Ready");
            };
            *body_until = past;
            state.expirations.push_front(ReplayExpiration {
                until: past,
                key: "shell-ready".to_owned(),
                kind: ReplayExpirationKind::DropReadyBody,
            });
        }
        assert!(matches!(
            ledger
                .claim("shell-ready", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::RecoverableShell
        ));
        assert_eq!(ledger.slot().reserved_bytes, 0);
    }

    #[test]
    fn expired_fail_closed_identities_release_the_full_identity_ceiling() {
        let ledger = RequestReplayLedger::default();
        let namespace = [20; 32];
        ledger.activate_scope(namespace, [0; 32]);
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        {
            let mut state = ledger.slot();
            for index in 0..MAX_REQUEST_REPLAY_ENTRIES {
                let key = format!("expired-{index}");
                state.entries.insert(
                    key.clone(),
                    RequestReplayRecord {
                        namespace,
                        authority: [0; 32],
                        fingerprint: [21; 32],
                        state: RequestReplayEntry::Uncertain { until: past },
                        delivery_leases: 0,
                        deferred_expiry: None,
                    },
                );
                state.expirations.push_back(ReplayExpiration {
                    until: past,
                    key,
                    kind: ReplayExpirationKind::ExpireIdentity,
                });
            }
        }
        assert_eq!(ledger.slot().entries.len(), MAX_REQUEST_REPLAY_ENTRIES);
        assert!(matches!(
            ledger
                .claim("fresh-after-expiry", namespace, [0; 32], [22; 32])
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        assert_eq!(ledger.slot().entries.len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn expired_shell_identity_retires_inner_key_and_exposes_same_job() {
        use crate::mcp::ShellArgs;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = shell_enabled_test_machine(&root).await;
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let client = TunnelClient::new(
            test_config(),
            TunnelTuning::default(),
            CancellationToken::new(),
        )
        .unwrap();
        let request_key = durable_request_key(&client.request_namespace, "retired-shell");
        let started = mcp
            .machine
            .start_shell_job(
                ShellArgs {
                    command: "sleep 1".to_owned(),
                    dir: ".".to_owned(),
                    stdin: None,
                    timeout_secs: Some(5),
                    max_output_bytes: Some(1024),
                },
                false,
                Some(request_key.clone()),
            )
            .unwrap();
        let job_id = started.id.clone();
        drop(started);
        assert!(mcp.machine.has_job_request_delivery(&request_key));

        let fingerprint = [23; 32];
        assert!(matches!(
            client
                .replays
                .claim(
                    &request_key,
                    client.request_namespace,
                    client.execution_authority,
                    fingerprint
                )
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        drop(ReplayLeaderGuard::new(
            client.replays.clone(),
            &request_key,
            client.request_namespace,
            fingerprint,
            true,
        ));
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        {
            let mut state = client.replays.slot();
            let record = state.entries.get_mut(&request_key).unwrap();
            let RequestReplayEntry::RecoverableShell { until } = &mut record.state else {
                panic!("abandoned shell replay did not become RecoverableShell");
            };
            *until = past;
            state.expirations.push_front(ReplayExpiration {
                until: past,
                key: request_key.clone(),
                kind: ReplayExpirationKind::ExpireIdentity,
            });
        }
        client.prune_replays(&mcp);
        assert!(
            !mcp.machine.has_job_request_delivery(&request_key),
            "final replay retirement must release the inner transport key"
        );
        {
            let state = client.replays.slot();
            assert!(matches!(
                state.entries[&request_key].state,
                RequestReplayEntry::Uncertain { .. }
            ));
        }
        let retirement_claim = client
            .replays
            .claim(
                &request_key,
                client.request_namespace,
                client.execution_authority,
                fingerprint,
            )
            .unwrap();
        assert!(
            matches!(retirement_claim, RequestReplayClaim::Uncertain(Some(_))),
            "retired shell request must remain fail-closed instead of becoming a fresh leader"
        );
        drop(retirement_claim);

        let response = mcp
            .dispatch(
                &BTreeMap::new(),
                &json!({
                    "jsonrpc":"2.0", "id":1, "method":"tools/call",
                    "params":{"name":"job_list","arguments":{}}
                }),
            )
            .await
            .unwrap();
        assert!(
            response
                .messages
                .iter()
                .any(|message| message.to_string().contains(&job_id)),
            "retired shell execution must remain discoverable through job_list"
        );
    }

    #[test]
    fn replay_namespace_switch_releases_cached_bytes_and_keeps_fail_closed_tombstone() {
        let ledger = RequestReplayLedger::default();
        let namespace_a = [10; 32];
        let namespace_b = [11; 32];
        let fingerprint = [12; 32];
        ledger.activate_scope(namespace_a, [0; 32]);
        assert!(matches!(
            ledger
                .claim("request", namespace_a, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        ledger
            .complete(
                "request",
                namespace_a,
                fingerprint,
                ReplayTerminal {
                    response: TunnelResponse {
                        request_id: "request".to_owned(),
                        channel: Some("main".to_owned()),
                        resp_json: Some(json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}})),
                        resp_headers: BTreeMap::new(),
                        resp_code: 200,
                        resp_type: "jsonrpc_response".to_owned(),
                    },
                    finalize_job_delivery: false,
                    requires_job_owner: false,
                    shell_guard_generation: None,
                    recoverable_shell: false,
                },
            )
            .unwrap();
        assert!(ledger.slot().reserved_bytes > 0);

        ledger.activate_scope(namespace_b, [0; 32]);
        assert_eq!(ledger.slot().reserved_bytes, 0);
        let retired_until = {
            let state = ledger.slot();
            let RequestReplayEntry::Retired { until } = state.entries["request"].state else {
                panic!("inactive namespace did not retain a fail-closed tombstone");
            };
            until
        };

        ledger.activate_scope(namespace_a, [0; 32]);
        let delivery = match ledger
            .claim("request", namespace_a, [0; 32], fingerprint)
            .unwrap()
        {
            RequestReplayClaim::Uncertain(Some(delivery)) => delivery,
            _ => panic!("reactivated namespace did not promote Retired to leased Uncertain"),
        };
        assert!(ledger.commit_uncertain_delivery("request", namespace_a, [0; 32], fingerprint));
        drop(delivery);

        // The stale retirement expiration from namespace A's inactive period
        // must not remove the newer Delivered generation.
        {
            let mut state = ledger.slot();
            RequestReplayLedger::apply_expiration(
                &mut state,
                ReplayExpiration {
                    until: retired_until,
                    key: "request".to_owned(),
                    kind: ReplayExpirationKind::RemoveTombstone,
                },
                Instant::now(),
            );
            assert!(matches!(
                state.entries["request"].state,
                RequestReplayEntry::Delivered { .. }
            ));
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cached_synchronous_shell_output_is_invalid_after_live_guard_retarget() {
        use std::os::unix::fs::symlink;

        if crate::mcp::resolve_executable("bash").is_none()
            || !crate::mcp::trusted_bubblewrap_available()
        {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let first = root.join("first");
        let second = root.join("second");
        let deny_alias = temp.path().join("deny-link");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        symlink(&first, &deny_alias).unwrap();

        let machine = LocalMachine::new(crate::mcp::MachineConfig {
            access: crate::mcp::AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root.clone()],
                write_roots: vec![root.clone()],
                deny_read_roots: vec![deny_alias.clone()],
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: true,
            allow_network: false,
            max_shell_timeout_secs: 20,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,
            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,
            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let before = machine
            .current_shell_guard_generation()
            .unwrap()
            .expect("sandboxed Linux shell has a live guard generation");
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let terminal = CachedReplayTerminal {
            body: Bytes::from_static(b"cached-secret"),
            finalize_job_delivery: true,
            requires_job_owner: false,
            shell_guard_generation: Some(before),
            recover_after_body_expiry: true,
        };
        assert!(TunnelClient::cached_shell_guard_is_current(&mcp, &terminal));

        std::fs::remove_file(&deny_alias).unwrap();
        symlink(&second, &deny_alias).unwrap();

        let after = mcp
            .machine
            .current_shell_guard_generation()
            .unwrap()
            .expect("retargeted sandbox still has a live guard generation");
        assert_ne!(before, after);
        assert!(
            !TunnelClient::cached_shell_guard_is_current(&mcp, &terminal),
            "cached synchronous output must not survive a live guard retarget"
        );
    }

    #[test]
    fn authority_change_discards_cached_response_bytes_and_fails_closed() {
        let ledger = RequestReplayLedger::default();
        let namespace = [40; 32];
        let old_authority = [41; 32];
        let new_authority = [42; 32];
        let fingerprint = [43; 32];
        ledger.activate_scope(namespace, old_authority);
        assert!(matches!(
            ledger
                .claim("secret-shell", namespace, old_authority, fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        ledger
            .complete(
                "secret-shell",
                namespace,
                fingerprint,
                ReplayTerminal {
                    response: TunnelResponse {
                        request_id: "secret-shell".to_owned(),
                        channel: Some("main".to_owned()),
                        resp_json: Some(json!({
                            "jsonrpc":"2.0",
                            "id":1,
                            "result":{"content":[{"type":"text","text":"revoked-secret"}]}
                        })),
                        resp_headers: BTreeMap::new(),
                        resp_code: 200,
                        resp_type: "jsonrpc_response".to_owned(),
                    },
                    finalize_job_delivery: true,
                    requires_job_owner: false,
                    shell_guard_generation: None,
                    recoverable_shell: false,
                },
            )
            .unwrap();
        assert!(ledger.slot().reserved_bytes > 0);
        assert!(matches!(
            ledger
                .claim("secret-shell", namespace, old_authority, fingerprint)
                .unwrap(),
            RequestReplayClaim::Ready(_, _)
        ));

        ledger.activate_scope(namespace, new_authority);

        let state = ledger.slot();
        assert_eq!(state.reserved_bytes, 0);
        assert_eq!(state.entries["secret-shell"].authority, new_authority);
        assert!(matches!(
            state.entries["secret-shell"].state,
            RequestReplayEntry::Uncertain { .. }
        ));
        drop(state);
        assert!(matches!(
            ledger
                .claim("secret-shell", namespace, new_authority, fingerprint)
                .unwrap(),
            RequestReplayClaim::Uncertain(_)
        ));
        let stale_scope = match ledger.claim("secret-shell", namespace, old_authority, fingerprint)
        {
            Err(error) => error.to_string(),
            Ok(_) => panic!("revoked replay authority unexpectedly remained active"),
        };
        assert!(stale_scope.contains("scope is not active"), "{stale_scope}");
    }

    #[test]
    fn terminal_delivery_lease_prevents_ready_expiry_from_beating_positive_post() {
        let ledger = RequestReplayLedger::default();
        let namespace = [53; 32];
        let authority = [54; 32];
        let fingerprint = [55; 32];
        ledger.activate_scope(namespace, authority);
        assert!(matches!(
            ledger
                .claim("ready-delivery", namespace, authority, fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        ledger
            .complete(
                "ready-delivery",
                namespace,
                fingerprint,
                ReplayTerminal {
                    response: TunnelResponse {
                        request_id: "ready-delivery".to_owned(),
                        channel: Some("main".to_owned()),
                        resp_json: Some(json!({
                            "jsonrpc":"2.0",
                            "id":1,
                            "result":{"ok":true}
                        })),
                        resp_headers: BTreeMap::new(),
                        resp_code: 200,
                        resp_type: "jsonrpc_response".to_owned(),
                    },
                    finalize_job_delivery: false,
                    requires_job_owner: false,
                    shell_guard_generation: None,
                    recoverable_shell: false,
                },
            )
            .unwrap();
        let (_terminal, delivery) = match ledger
            .claim("ready-delivery", namespace, authority, fingerprint)
            .unwrap()
        {
            RequestReplayClaim::Ready(terminal, delivery) => (terminal, delivery),
            _ => panic!("completed replay did not become Ready"),
        };
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        {
            let mut state = ledger.slot();
            let record = state.entries.get_mut("ready-delivery").unwrap();
            let RequestReplayEntry::Ready { body_until, .. } = &mut record.state else {
                panic!("completed replay did not remain Ready");
            };
            *body_until = past;
            state.expirations.push_front(ReplayExpiration {
                until: past,
                key: "ready-delivery".to_owned(),
                kind: ReplayExpirationKind::DropReadyBody,
            });
        }

        assert!(ledger.prune().is_empty());
        {
            let state = ledger.slot();
            let record = &state.entries["ready-delivery"];
            assert!(matches!(record.state, RequestReplayEntry::Ready { .. }));
            assert_eq!(record.delivery_leases, 1);
            assert_eq!(
                record.deferred_expiry,
                Some(ReplayExpirationKind::DropReadyBody)
            );
            assert!(state.reserved_bytes > 0);
        }
        assert!(ledger.commit_ready_delivery("ready-delivery", namespace, authority, fingerprint));
        drop(delivery);
        assert!(matches!(
            ledger
                .claim("ready-delivery", namespace, authority, fingerprint)
                .unwrap(),
            RequestReplayClaim::Delivered
        ));
    }

    #[test]
    fn failed_terminal_delivery_releases_lease_and_applies_deferred_expiry() {
        let ledger = RequestReplayLedger::default();
        let namespace = [59; 32];
        let authority = [60; 32];
        let fingerprint = [61; 32];
        ledger.activate_scope(namespace, authority);
        assert!(matches!(
            ledger
                .claim("failed-delivery", namespace, authority, fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        ledger
            .complete(
                "failed-delivery",
                namespace,
                fingerprint,
                ReplayTerminal {
                    response: TunnelResponse {
                        request_id: "failed-delivery".to_owned(),
                        channel: Some("main".to_owned()),
                        resp_json: Some(json!({"jsonrpc":"2.0","id":1,"result":{}})),
                        resp_headers: BTreeMap::new(),
                        resp_code: 200,
                        resp_type: "jsonrpc_response".to_owned(),
                    },
                    finalize_job_delivery: false,
                    requires_job_owner: false,
                    shell_guard_generation: None,
                    recoverable_shell: false,
                },
            )
            .unwrap();
        let delivery = match ledger
            .claim("failed-delivery", namespace, authority, fingerprint)
            .unwrap()
        {
            RequestReplayClaim::Ready(_, delivery) => delivery,
            _ => panic!("completed replay did not become Ready"),
        };
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        {
            let mut state = ledger.slot();
            let record = state.entries.get_mut("failed-delivery").unwrap();
            let RequestReplayEntry::Ready { body_until, .. } = &mut record.state else {
                panic!("completed replay did not remain Ready");
            };
            *body_until = past;
            state.expirations.push_front(ReplayExpiration {
                until: past,
                key: "failed-delivery".to_owned(),
                kind: ReplayExpirationKind::DropReadyBody,
            });
        }
        assert!(ledger.prune().is_empty());
        drop(delivery);

        let state = ledger.slot();
        assert_eq!(state.reserved_bytes, 0);
        assert!(matches!(
            state.entries["failed-delivery"].state,
            RequestReplayEntry::Uncertain { .. }
        ));
    }

    #[test]
    fn terminal_delivery_lease_prevents_uncertain_expiry_from_reopening_identity() {
        let ledger = RequestReplayLedger::default();
        let namespace = [56; 32];
        let authority = [57; 32];
        let fingerprint = [58; 32];
        ledger.activate_scope(namespace, authority);
        assert!(matches!(
            ledger
                .claim("uncertain-delivery", namespace, authority, fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        drop(ReplayLeaderGuard::new(
            ledger.clone(),
            "uncertain-delivery",
            namespace,
            fingerprint,
            false,
        ));
        let delivery = match ledger
            .claim("uncertain-delivery", namespace, authority, fingerprint)
            .unwrap()
        {
            RequestReplayClaim::Uncertain(Some(delivery)) => delivery,
            _ => panic!("abandoned mutation did not become Uncertain"),
        };
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        {
            let mut state = ledger.slot();
            let record = state.entries.get_mut("uncertain-delivery").unwrap();
            let RequestReplayEntry::Uncertain { until } = &mut record.state else {
                panic!("abandoned mutation did not remain Uncertain");
            };
            *until = past;
            state.expirations.push_front(ReplayExpiration {
                until: past,
                key: "uncertain-delivery".to_owned(),
                kind: ReplayExpirationKind::ExpireIdentity,
            });
        }

        assert!(ledger.prune().is_empty());
        assert!(ledger.slot().entries.contains_key("uncertain-delivery"));
        assert!(ledger.commit_uncertain_delivery(
            "uncertain-delivery",
            namespace,
            authority,
            fingerprint
        ));
        drop(delivery);
        assert!(matches!(
            ledger
                .claim("uncertain-delivery", namespace, authority, fingerprint)
                .unwrap(),
            RequestReplayClaim::Delivered
        ));
    }

    #[test]
    fn delivered_tombstone_wins_over_late_owner_dependency_invalidation() {
        let ledger = RequestReplayLedger::default();
        let namespace = [44; 32];
        let authority = [45; 32];
        let fingerprint = [46; 32];
        ledger.activate_scope(namespace, authority);
        assert!(matches!(
            ledger
                .claim("durable", namespace, authority, fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        ledger
            .complete(
                "durable",
                namespace,
                fingerprint,
                ReplayTerminal {
                    response: TunnelResponse {
                        request_id: "durable".to_owned(),
                        channel: Some("main".to_owned()),
                        resp_json: Some(json!({
                            "jsonrpc":"2.0",
                            "id":1,
                            "result":{"job_id":"stable"}
                        })),
                        resp_headers: BTreeMap::new(),
                        resp_code: 200,
                        resp_type: "jsonrpc_response".to_owned(),
                    },
                    finalize_job_delivery: true,
                    requires_job_owner: true,
                    shell_guard_generation: None,
                    recoverable_shell: false,
                },
            )
            .unwrap();
        let (held_by_racing_redelivery, delivery) = match ledger
            .claim("durable", namespace, authority, fingerprint)
            .unwrap()
        {
            RequestReplayClaim::Ready(terminal, delivery) => (terminal, delivery),
            _ => panic!("completed durable response was not ready"),
        };

        assert!(ledger.commit_ready_delivery("durable", namespace, authority, fingerprint));
        drop(delivery);
        // Model the second redelivery observing that the inner job key was
        // intentionally acknowledged after the first delivery committed.
        ledger.invalidate_ready("durable", namespace, fingerprint);
        assert!(matches!(
            ledger
                .claim("durable", namespace, authority, fingerprint)
                .unwrap(),
            RequestReplayClaim::Delivered
        ));
        assert!(!held_by_racing_redelivery.body.is_empty());
    }

    #[test]
    fn shell_retirement_queue_is_bounded_and_deduplicated() {
        let ledger = RequestReplayLedger::default();
        let namespace = [47; 32];
        let authority = [48; 32];
        let fingerprint = [49; 32];
        ledger.activate_scope(namespace, authority);
        let count = MAX_SHELL_RETIREMENTS_PER_PRUNE * 3 + 1;
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        {
            let mut state = ledger.slot();
            for index in 0..count {
                let key = format!("retire-{index}");
                state.entries.insert(
                    key.clone(),
                    RequestReplayRecord {
                        namespace,
                        authority,
                        fingerprint,
                        state: RequestReplayEntry::RecoverableShell { until: past },
                        delivery_leases: 0,
                        deferred_expiry: None,
                    },
                );
                state.expirations.push_back(ReplayExpiration {
                    until: past,
                    key,
                    kind: ReplayExpirationKind::ExpireIdentity,
                });
            }
        }

        let first = ledger.prune();
        assert_eq!(first.len(), MAX_SHELL_RETIREMENTS_PER_PRUNE);
        {
            let state = ledger.slot();
            assert_eq!(
                state.shell_retirements.len(),
                count - MAX_SHELL_RETIREMENTS_PER_PRUNE
            );
            assert_eq!(
                state.queued_shell_retirements.len(),
                count - MAX_SHELL_RETIREMENTS_PER_PRUNE
            );
        }

        for key in &first {
            ledger.defer_shell_retirement(key);
            ledger.defer_shell_retirement(key);
        }
        let state = ledger.slot();
        assert_eq!(state.shell_retirements.len(), count);
        assert_eq!(state.queued_shell_retirements.len(), count);
        drop(state);
        assert_eq!(ledger.prune().len(), MAX_SHELL_RETIREMENTS_PER_PRUNE);
    }

    #[test]
    fn stale_shell_retirement_scan_is_bounded() {
        let ledger = RequestReplayLedger::default();
        let namespace = [50; 32];
        let authority = [51; 32];
        ledger.activate_scope(namespace, authority);
        let count = MAX_SHELL_RETIREMENTS_PER_PRUNE * 3;
        let until = RequestReplayLedger::identity_until(Instant::now());
        {
            let mut state = ledger.slot();
            for index in 0..count {
                let key = format!("stale-retirement-{index}");
                state.entries.insert(
                    key.clone(),
                    RequestReplayRecord {
                        namespace,
                        authority,
                        fingerprint: [52; 32],
                        state: RequestReplayEntry::Retired { until },
                        delivery_leases: 0,
                        deferred_expiry: None,
                    },
                );
                assert!(state.queued_shell_retirements.insert(key.clone()));
                state.shell_retirements.push_back(key);
            }
        }

        assert!(ledger.prune().is_empty());
        let state = ledger.slot();
        assert_eq!(
            state.shell_retirements.len(),
            count - MAX_SHELL_RETIREMENTS_PER_PRUNE
        );
        assert_eq!(
            state.queued_shell_retirements.len(),
            count - MAX_SHELL_RETIREMENTS_PER_PRUNE
        );
    }

    #[test]
    fn cached_durable_handle_becomes_uncertain_when_its_job_owner_is_revoked() {
        let ledger = RequestReplayLedger::default();
        let namespace = [16; 32];
        let fingerprint = [17; 32];
        ledger.activate_scope(namespace, [0; 32]);
        assert!(matches!(
            ledger
                .claim("request", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        ledger
            .complete(
                "request",
                namespace,
                fingerprint,
                ReplayTerminal {
                    response: TunnelResponse {
                        request_id: "request".to_owned(),
                        channel: Some("main".to_owned()),
                        resp_json: Some(json!({"jsonrpc":"2.0","id":1,"result":{"job_id":"gone"}})),
                        resp_headers: BTreeMap::new(),
                        resp_code: 200,
                        resp_type: "jsonrpc_response".to_owned(),
                    },
                    finalize_job_delivery: true,
                    requires_job_owner: true,
                    shell_guard_generation: None,
                    recoverable_shell: false,
                },
            )
            .unwrap();
        assert!(ledger.slot().reserved_bytes > 0);
        ledger.invalidate_ready("request", namespace, fingerprint);
        assert_eq!(ledger.slot().reserved_bytes, 0);
        assert!(matches!(
            ledger
                .claim("request", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Uncertain(_)
        ));
    }

    #[test]
    fn replay_fingerprint_binds_jsonrpc_correlation_id() {
        let first = json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":"write","arguments":{"path":"x","content":"y"}}
        });
        let second = json!({
            "jsonrpc":"2.0", "id":2, "method":"tools/call",
            "params":{"name":"write","arguments":{"path":"x","content":"y"}}
        });
        assert_ne!(
            replay_request_fingerprint(&first, &BTreeMap::new()).unwrap(),
            replay_request_fingerprint(&second, &BTreeMap::new()).unwrap(),
            "one hosted request id cannot replay a cached response under a different JSON-RPC id"
        );
        let mut different_version = first.clone();
        different_version["jsonrpc"] = Value::String("1.0".to_owned());
        assert_ne!(
            replay_request_fingerprint(&first, &BTreeMap::new()).unwrap(),
            replay_request_fingerprint(&different_version, &BTreeMap::new()).unwrap(),
            "a different JSON-RPC envelope must not borrow a cached valid response"
        );

        let mut public_meta = first.clone();
        public_meta["params"]["_meta"] = json!({
            "io.modelcontextprotocol/clientCapabilities": {"tasks": {}}
        });
        assert_ne!(
            replay_request_fingerprint(&first, &BTreeMap::new()).unwrap(),
            replay_request_fingerprint(&public_meta, &BTreeMap::new()).unwrap(),
            "public MCP metadata can change protocol response semantics and must participate in replay identity"
        );

        let mut mrtr = first.clone();
        mrtr["params"]["requestState"] = Value::String("opaque-state".to_owned());
        mrtr["params"]["inputResponses"] = json!({"answer":"yes"});
        assert_ne!(
            replay_request_fingerprint(&first, &BTreeMap::new()).unwrap(),
            replay_request_fingerprint(&mrtr, &BTreeMap::new()).unwrap(),
            "future/request-semantic params must participate without maintaining a hand-written field list"
        );

        let mut private_a = first.clone();
        private_a["params"]["_meta"] = json!({
            crate::mcp::TRANSPORT_CONTEXT_TOKEN_META_KEY: "first",
            crate::mcp::REQUEST_KEY_META_KEY: "key-a",
            crate::mcp::RESULT_BUDGET_META_KEY: 1
        });
        let mut private_b = first.clone();
        private_b["params"]["_meta"] = json!({
            crate::mcp::TRANSPORT_CONTEXT_TOKEN_META_KEY: "second",
            crate::mcp::REQUEST_KEY_META_KEY: "key-b",
            crate::mcp::RESULT_BUDGET_META_KEY: 999999
        });
        assert_eq!(
            replay_request_fingerprint(&private_a, &BTreeMap::new()).unwrap(),
            replay_request_fingerprint(&private_b, &BTreeMap::new()).unwrap(),
            "transport-private delivery metadata is derived locally and must not split semantic replay identity"
        );

        let mut nested_private_a = first.clone();
        nested_private_a["params"]["arguments"]["nested"] = json!({
            "params": {
                "_meta": {
                    crate::mcp::TRANSPORT_CONTEXT_TOKEN_META_KEY: "semantic-a"
                }
            }
        });
        let mut nested_private_b = nested_private_a.clone();
        nested_private_b["params"]["arguments"]["nested"]["params"]["_meta"]
            [crate::mcp::TRANSPORT_CONTEXT_TOKEN_META_KEY] = Value::String("semantic-b".to_owned());
        assert_ne!(
            replay_request_fingerprint(&nested_private_a, &BTreeMap::new()).unwrap(),
            replay_request_fingerprint(&nested_private_b, &BTreeMap::new()).unwrap(),
            "only the exact top-level params._meta transport namespace is private"
        );

        let protocol_headers = BTreeMap::from([(
            "MCP-Protocol-Version".to_owned(),
            vec!["2026-07-28".to_owned()],
        )]);
        assert_ne!(
            replay_request_fingerprint(&first, &BTreeMap::new()).unwrap(),
            replay_request_fingerprint(&first, &protocol_headers).unwrap(),
            "forwarded MCP protocol/routing headers affect request semantics and must participate in replay identity"
        );
        let ignored_headers = BTreeMap::from([(
            "X-Ignored-By-Embedded-MCP".to_owned(),
            vec!["different".to_owned()],
        )]);
        assert_eq!(
            replay_request_fingerprint(&first, &BTreeMap::new()).unwrap(),
            replay_request_fingerprint(&first, &ignored_headers).unwrap(),
            "headers dropped before embedded dispatch must not split replay identity"
        );

        let invalid_allowed_name = BTreeMap::from([(
            "mcp-param-bad
name"
                .to_owned(),
            vec!["value".to_owned()],
        )]);
        let empty_allowed_group = BTreeMap::from([("mcp-protocol-version".to_owned(), Vec::new())]);
        assert_eq!(
            replay_request_fingerprint(&first, &BTreeMap::new()).unwrap(),
            replay_request_fingerprint(&first, &invalid_allowed_name).unwrap(),
            "syntactically invalid allowed-prefix headers are dropped by both dispatch and identity"
        );
        assert_eq!(
            replay_request_fingerprint(&first, &BTreeMap::new()).unwrap(),
            replay_request_fingerprint(&first, &empty_allowed_group).unwrap(),
            "empty header groups have no effective forwarded semantics"
        );
        let lower_protocol_headers = BTreeMap::from([(
            "mcp-protocol-version".to_owned(),
            vec!["2026-07-28".to_owned()],
        )]);
        assert_eq!(
            replay_request_fingerprint(&first, &protocol_headers).unwrap(),
            replay_request_fingerprint(&first, &lower_protocol_headers).unwrap(),
            "header-name casing must normalize exactly as embedded dispatch does"
        );
    }

    #[test]
    fn recoverable_shell_rejects_same_request_id_with_different_payload_fingerprint() {
        let ledger = RequestReplayLedger::default();
        let namespace = [13; 32];
        let original = [14; 32];
        let different = [15; 32];
        ledger.activate_scope(namespace, [0; 32]);
        assert!(matches!(
            ledger
                .claim("request", namespace, [0; 32], original)
                .unwrap(),
            RequestReplayClaim::Leader
        ));
        drop(ReplayLeaderGuard::new(
            ledger.clone(),
            "request",
            namespace,
            original,
            true,
        ));
        assert!(matches!(
            ledger
                .claim("request", namespace, [0; 32], different)
                .unwrap(),
            RequestReplayClaim::Conflict
        ));
        assert!(matches!(
            ledger
                .resume_recoverable_shell("request", namespace, [0; 32], different)
                .unwrap(),
            RequestReplayClaim::Conflict
        ));
    }

    #[test]
    fn poll_failure_threshold_restarts_runtime_on_tenth_failure() {
        let restart_after = TunnelTuning::default().restart_after;
        assert!(!should_restart_runtime_after_poll_failures(
            0,
            restart_after
        ));
        assert!(!should_restart_runtime_after_poll_failures(
            9,
            restart_after
        ));
        assert!(should_restart_runtime_after_poll_failures(
            10,
            restart_after
        ));
        assert!(should_restart_runtime_after_poll_failures(
            11,
            restart_after
        ));
    }

    async fn test_machine_with_write(base_dir: &std::path::Path, write: bool) -> LocalMachine {
        use crate::mcp::{AccessSpec, MachineConfig};

        LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: base_dir.to_path_buf(),
                read_roots: vec![base_dir.to_path_buf()],
                write_roots: if write {
                    vec![base_dir.to_path_buf()]
                } else {
                    Vec::new()
                },
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: false,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 1,
            max_output_bytes: 1024,
            max_read_bytes: 1024,
            max_write_bytes: 1024,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap()
    }

    async fn test_machine(base_dir: &std::path::Path) -> LocalMachine {
        test_machine_with_write(base_dir, false).await
    }

    fn fast_retry_tuning() -> TunnelTuning {
        TunnelTuning {
            backoff: Backoff {
                base: Duration::from_millis(1),
                cap: Duration::from_millis(2),
                jitter: Duration::ZERO,
            },
            ..TunnelTuning::default()
        }
    }

    /// A control plane answering poll number `n` (from 0) with `respond(n)`:
    /// a status and how long to hold it. Returns its URL and a poll counter.
    async fn fake_control_plane(
        respond: impl Fn(usize) -> (StatusCode, Duration) + Send + Sync + 'static,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let polls = Arc::new(AtomicUsize::new(0));
        let respond = Arc::new(respond);
        let counter = polls.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let (status, hold) = respond(counter.fetch_add(1, Ordering::SeqCst));
                tokio::spawn(async move {
                    let mut request = [0_u8; 4096];
                    let _ = stream.read(&mut request).await;
                    time::sleep(hold).await;
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (url, polls)
    }

    #[tokio::test]
    async fn poll_statuses_distinguish_rejection_activation_and_fatal_failures() {
        for (status, new_tunnel, expected) in [
            (StatusCode::UNAUTHORIZED, false, "rejected"),
            (StatusCode::FORBIDDEN, false, "rejected"),
            (StatusCode::NOT_FOUND, false, "rejected"),
            (StatusCode::FORBIDDEN, true, "not ready"),
            (StatusCode::NOT_FOUND, true, "not ready"),
            (StatusCode::BAD_REQUEST, false, "fatal"),
            (StatusCode::CONFLICT, false, "fatal"),
        ] {
            let (base_url, _) = fake_control_plane(move |_| (status, Duration::ZERO)).await;
            let client = TunnelClient::new(
                Config {
                    base_url,
                    new_tunnel,
                    ..test_config()
                },
                TunnelTuning::default(),
                CancellationToken::new(),
            )
            .unwrap();
            let actual = match client.poll_once(MAX_POLL_RESPONSE_BYTES).await {
                Err(PollFailure::Rejected(_)) => "rejected",
                Err(PollFailure::NotReady(_)) => "not ready",
                Err(PollFailure::Fatal(error)) => {
                    assert_eq!(Recovery::of(&error), Recovery::Process, "{status}");
                    "fatal"
                }
                Err(PollFailure::Transient(_)) => "transient",
                Ok(_) => "ok",
            };
            assert_eq!(actual, expected, "{status}, new tunnel: {new_tunnel}");
        }
    }

    #[tokio::test]
    async fn expired_or_ambiguous_response_posts_are_not_delivery_acknowledgements() {
        let command = TunnelCommand {
            request_id: "request".to_owned(),
            shard_token: "shard".to_owned(),
            command_type: "jsonrpc".to_owned(),
            channel: Some("main".to_owned()),
            response_timeout: None,
            headers: BTreeMap::new(),
            jsonrpc: Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call"})),
        };
        let response = TunnelResponse {
            request_id: command.request_id.clone(),
            channel: command.channel.clone(),
            resp_json: Some(json!({"jsonrpc":"2.0","id":1,"result":{}})),
            resp_headers: BTreeMap::new(),
            resp_code: 200,
            resp_type: "jsonrpc_response".to_owned(),
        };

        let (base_url, _) =
            fake_control_plane(|_| (StatusCode::OK, Duration::from_millis(80))).await;
        let client = TunnelClient::new(
            Config {
                base_url,
                ..test_config()
            },
            TunnelTuning::default(),
            CancellationToken::new(),
        )
        .unwrap();
        let outcome = client
            .post_once(
                &command,
                serialize_tunnel_response(&response).unwrap(),
                Some(Instant::now() + Duration::from_millis(10)),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, PostOutcome::Undeliverable));

        let (base_url, _) = fake_control_plane(|_| (StatusCode::NOT_FOUND, Duration::ZERO)).await;
        let client = TunnelClient::new(
            Config {
                base_url,
                ..test_config()
            },
            TunnelTuning::default(),
            CancellationToken::new(),
        )
        .unwrap();
        let outcome = client
            .post_once(
                &command,
                serialize_tunnel_response(&response).unwrap(),
                None,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, PostOutcome::Undeliverable));
    }

    #[tokio::test]
    async fn cancellation_wins_over_ready_poll_post_and_permit_paths() {
        let temp = tempfile::tempdir().unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        // This helper is shared by POSTs and permit acquisition. Exercise the
        // simultaneous-ready case directly, not merely a pending operation.
        assert_eq!(
            cancellation_first(&cancellation, std::future::ready(42)).await,
            None
        );
        let permit_client =
            TunnelClient::new(test_config(), TunnelTuning::default(), cancellation.clone())
                .unwrap();
        let permit = cancellation_first(
            &cancellation,
            permit_client.concurrency.clone().acquire_owned(),
        )
        .await;
        assert!(permit.is_none(), "a ready permit beat requested shutdown");

        let post_client =
            TunnelClient::new(test_config(), TunnelTuning::default(), cancellation.clone())
                .unwrap();
        let command = TunnelCommand {
            request_id: "request".to_owned(),
            shard_token: "shard".to_owned(),
            command_type: "session_termination".to_owned(),
            channel: Some("main".to_owned()),
            response_timeout: None,
            headers: BTreeMap::new(),
            jsonrpc: None,
        };
        let response = TunnelResponse {
            request_id: command.request_id.clone(),
            channel: command.channel.clone(),
            resp_json: None,
            resp_headers: BTreeMap::new(),
            resp_code: 204,
            resp_type: "session_termination_response".to_owned(),
        };
        assert!(matches!(
            post_client
                .post_once(
                    &command,
                    serialize_tunnel_response(&response).unwrap(),
                    None
                )
                .await,
            Ok(PostOutcome::Cancelled)
        ));

        let poll_client =
            TunnelClient::new(test_config(), TunnelTuning::default(), cancellation.clone())
                .unwrap();
        let machine = test_machine(temp.path()).await;
        let mcp = EmbeddedMcp::new(machine, cancellation.child_token());
        assert!(matches!(
            poll_client.run(mcp, MAX_POLL_RESPONSE_BYTES).await.unwrap(),
            TunnelRunOutcome::Cancelled
        ));
    }

    #[tokio::test]
    async fn persistent_rejections_need_the_operator() {
        let temp = tempfile::tempdir().unwrap();
        let window = Duration::from_millis(150);
        let (base_url, polls) =
            fake_control_plane(|_| (StatusCode::UNAUTHORIZED, Duration::ZERO)).await;
        let started = Instant::now();
        let error = run_with_tuning(
            test_machine(temp.path()).await,
            Config {
                base_url,
                ..test_config()
            },
            TunnelTuning {
                rejection_confirmation: window,
                ..fast_retry_tuning()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(Recovery::of(&error), Recovery::Operator);
        assert!(format!("{error:#}").contains("Runtime API key"));
        assert!(started.elapsed() >= window);
        assert!(polls.load(std::sync::atomic::Ordering::SeqCst) > 1);
    }

    #[tokio::test]
    async fn an_accepted_poll_restarts_the_rejection_window() {
        let temp = tempfile::tempdir().unwrap();
        let window = Duration::from_millis(100);
        // Two rejections, one accepted poll held past the window, then
        // rejections again. Without a reset, the first one after the
        // accepted poll would already be confirmed.
        let (base_url, polls) = fake_control_plane(move |poll| match poll {
            2 => (StatusCode::NO_CONTENT, window * 2),
            _ => (StatusCode::UNAUTHORIZED, Duration::ZERO),
        })
        .await;
        let error = run_with_tuning(
            test_machine(temp.path()).await,
            Config {
                base_url,
                ..test_config()
            },
            TunnelTuning {
                rejection_confirmation: window,
                ..fast_retry_tuning()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(Recovery::of(&error), Recovery::Operator);
        assert!(polls.load(std::sync::atomic::Ordering::SeqCst) > 4);
    }

    #[test]
    fn rejection_ledger_tracks_one_identity_until_it_is_accepted() {
        let window = Duration::from_secs(60);
        let ledger = RejectionLedger::default();
        let [first, second] = [[1; 32], [2; 32]];
        let attempts = |identity| ledger.record(identity, window).attempts;

        assert_eq!(attempts(first), 1);
        assert_eq!(attempts(first), 2);
        // A changed endpoint, tunnel, key or organization starts over.
        assert_eq!(attempts(second), 1);
        ledger.accept(first);
        assert_eq!(
            attempts(second),
            2,
            "another identity's poll proves nothing"
        );
        ledger.accept(second);
        assert_eq!(attempts(second), 1);
        assert_eq!(
            ledger.record(second, Duration::ZERO),
            Rejection {
                attempts: 2,
                confirmed: true
            }
        );
    }

    #[tokio::test]
    async fn rejection_window_survives_in_process_runtime_restarts() {
        let temp = tempfile::tempdir().unwrap();
        // Real async poll tasks may be delayed under parallel Nix/CI load.
        // Only the second run should pass the confirmation window.
        let window = Duration::from_secs(2);
        let (base_url, _) = fake_control_plane(|poll| {
            let status = if poll % 2 == 0 {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            (status, Duration::ZERO)
        })
        .await;
        let config = Config {
            base_url,
            ..test_config()
        };
        let tuning = TunnelTuning {
            rejection_confirmation: window,
            restart_after: 2,
            ..fast_retry_tuning()
        };
        let run = || async {
            run_with_tuning(
                test_machine(temp.path()).await,
                config.clone(),
                tuning.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err()
        };

        // Each runtime restarts on 5xx well inside the window; the rebuilt
        // runtime inherits the unresolved rejection instead of a fresh one.
        assert!(Recovery::of(&run().await).restarts_runtime());
        time::sleep(window).await;
        assert_eq!(Recovery::of(&run().await), Recovery::Operator);
    }

    #[tokio::test]
    async fn alternating_rejections_and_transients_still_request_runtime_restart() {
        let temp = tempfile::tempdir().unwrap();
        let (base_url, polls) = fake_control_plane(|poll| {
            let status = if poll % 2 == 0 {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::TOO_MANY_REQUESTS
            };
            (status, Duration::ZERO)
        })
        .await;

        let error = run_with_tuning(
            test_machine(temp.path()).await,
            Config {
                base_url,
                ..test_config()
            },
            TunnelTuning {
                rejection_confirmation: Duration::from_secs(30),
                restart_after: 3,
                ..fast_retry_tuning()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(
            Recovery::of(&error),
            Recovery::Runtime {
                reset_backoff: false
            }
        );
        assert!(polls.load(std::sync::atomic::Ordering::SeqCst) >= 6);
    }

    #[tokio::test]
    async fn alternating_rejections_and_server_errors_still_confirm_rejection() {
        let temp = tempfile::tempdir().unwrap();
        let window = Duration::from_millis(100);
        let (base_url, polls) = fake_control_plane(|poll| {
            let status = if poll % 2 == 0 {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            (status, Duration::ZERO)
        })
        .await;
        let started = Instant::now();

        let error = run_with_tuning(
            test_machine(temp.path()).await,
            Config {
                base_url,
                ..test_config()
            },
            TunnelTuning {
                rejection_confirmation: window,
                restart_after: 1_000,
                ..fast_retry_tuning()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(Recovery::of(&error), Recovery::Operator);
        assert!(started.elapsed() >= window);
        assert!(polls.load(std::sync::atomic::Ordering::SeqCst) > 2);
    }

    #[tokio::test]
    async fn repeated_transient_poll_failures_request_runtime_restart() {
        let temp = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let error = run_with_tuning(
            test_machine(temp.path()).await,
            Config {
                base_url: format!("http://{addr}"),
                ..test_config()
            },
            fast_retry_tuning(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(
            Recovery::of(&error),
            Recovery::Runtime {
                reset_backoff: false
            },
            "transient failure threshold should request a runtime restart"
        );
        assert!(
            error
                .to_string()
                .contains("10 transient poll failures without a successful poll")
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn replay_protected_dispatch_releases_tunnel_attempt_at_response_deadline() {
        use crate::mcp::{AccessSpec, MachineConfig};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let marker = root.join("gate-held");
        let target = root.join("must-not-write-yet");
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root.clone()],
                write_roots: vec![root.clone()],
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: true,
            allow_shell: true,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 10,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,
            task_result_budget: None,
            job_retention_secs: 60,
            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let background = json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params": {
                "name":"job_start",
                "arguments": {
                    "command": format!("printf held > '{}'; sleep 1", marker.display()),
                    "timeout_secs": 3
                }
            }
        });
        mcp.dispatch(&BTreeMap::new(), &background).await.unwrap();
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            marker.exists(),
            "background shell never acquired the operation gate"
        );

        let (base_url, _) = fake_control_plane(|_| (StatusCode::OK, Duration::ZERO)).await;
        let client = TunnelClient::new(
            Config {
                base_url,
                ..test_config()
            },
            TunnelTuning {
                delivery_reserve: Duration::from_millis(20),
                ..TunnelTuning::default()
            },
            CancellationToken::new(),
        )
        .unwrap();
        let command = TunnelCommand {
            request_id: "deadline-write".to_owned(),
            shard_token: "test-shard".to_owned(),
            command_type: "jsonrpc".to_owned(),
            channel: Some("main".to_owned()),
            response_timeout: Some(json!("100ms")),
            headers: BTreeMap::new(),
            jsonrpc: Some(json!({
                "jsonrpc":"2.0", "id":2, "method":"tools/call",
                "params": {"name":"write", "arguments":{"path":"must-not-write-yet","content":"x"}}
            })),
        };
        let key = durable_request_key(&client.request_namespace, &command.request_id);
        let fingerprint =
            replay_request_fingerprint(command.jsonrpc.as_ref().unwrap(), &command.headers)
                .unwrap();
        let started = Instant::now();
        client
            .process_command(mcp, command, Instant::now())
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "replay-protected dispatch held the tunnel attempt past its response window"
        );
        assert!(
            !target.exists(),
            "blocked write unexpectedly crossed the operation gate"
        );
        assert!(matches!(
            client
                .replays
                .claim(
                    &key,
                    client.request_namespace,
                    client.execution_authority,
                    fingerprint
                )
                .unwrap(),
            RequestReplayClaim::Uncertain(_)
        ));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serialized_background_job_control_remains_pollable_and_cancellable() {
        use crate::mcp::{AccessSpec, MachineConfig};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: root.clone(),
                read_roots: vec![root.clone()],
                write_roots: vec![root],
                deny_read_roots: Vec::new(),
                deny_write_roots: Vec::new(),
                unrestricted_fs: false,
            },
            cache_mounts: Vec::new(),
            shell_env: Vec::new(),
            #[cfg(target_os = "linux")]
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: true,
            allow_shell: true,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 10,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: 60,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let headers = BTreeMap::new();
        let call = |id: i64, name: &str, arguments: Value| {
            json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": name, "arguments": arguments}
            })
        };
        let terminal = |response: &LocalResponse, id: i64| {
            response
                .messages
                .iter()
                .find(|value| value.get("id") == Some(&json!(id)))
                .cloned()
                .expect("terminal JSON-RPC response")
        };

        let started = Instant::now();
        let created = mcp
            .dispatch(
                &headers,
                &call(
                    1,
                    "job_start",
                    json!({"command":"printf started; sleep 5", "timeout_secs":8}),
                ),
            )
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        let created = terminal(&created, 1);
        let receipt: Value =
            serde_json::from_str(created["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        let job_id = receipt["job_id"].as_str().unwrap().to_owned();

        // The running job owns the serialize gate. Control tools must bypass
        // that gate or they would be unusable until the job had already ended.
        let polled_at = Instant::now();
        let polled = mcp
            .dispatch(&headers, &call(2, "job_get", json!({"job_id":job_id})))
            .await
            .unwrap();
        assert!(polled_at.elapsed() < Duration::from_secs(1));
        let polled = terminal(&polled, 2);
        let status: Value =
            serde_json::from_str(polled["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(status["status"], "working");

        let cancelled_at = Instant::now();
        let cancelled = mcp
            .dispatch(&headers, &call(3, "job_cancel", json!({"job_id":job_id})))
            .await
            .unwrap();
        assert!(cancelled_at.elapsed() < Duration::from_secs(1));
        assert_ne!(terminal(&cancelled, 3)["result"]["isError"], true);

        let mut final_status = None;
        for attempt in 0..100_i64 {
            let polled = mcp
                .dispatch(
                    &headers,
                    &call(10 + attempt, "job_get", json!({"job_id":job_id})),
                )
                .await
                .unwrap();
            let polled = terminal(&polled, 10 + attempt);
            let value: Value =
                serde_json::from_str(polled["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            if value["status"] != "working" {
                final_status = value["status"].as_str().map(str::to_owned);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(final_status.as_deref(), Some("cancelled"));
    }

    #[test]
    fn response_timeout_parser_accepts_contract_values() {
        assert_eq!(
            parse_response_timeout(Some(&Value::String("30s".into()))),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_response_timeout(Some(&Value::String("4500ms".into()))),
            Some(Duration::from_millis(4500))
        );
        assert_eq!(
            parse_response_timeout(Some(&Value::String("0s".into()))),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn response_timeout_parser_fails_open_for_malformed_values() {
        for value in [
            json!(30),
            json!(" 1s"),
            json!("1s "),
            json!("4.5s"),
            json!("-1s"),
            json!("1m30s"),
            json!("30d"),
        ] {
            assert_eq!(parse_response_timeout(Some(&value)), None);
        }
    }

    #[test]
    fn response_window_always_has_the_known_hosted_deadline_fallback() {
        assert_eq!(
            effective_response_window(None),
            crate::limits::OPENAI_TUNNEL_REQUEST_DEADLINE
        );
        assert_eq!(
            effective_response_window(Some(&json!("not-a-duration"))),
            crate::limits::OPENAI_TUNNEL_REQUEST_DEADLINE
        );
        assert_eq!(
            effective_response_window(Some(&json!("30s"))),
            Duration::from_secs(30)
        );
        assert_eq!(
            effective_response_window(Some(&json!("10m"))),
            crate::limits::OPENAI_TUNNEL_REQUEST_DEADLINE
        );
    }

    #[test]
    fn client_rejects_unprepared_execution_authority() {
        let mut config = test_config();
        config.execution_authority = None;
        let error =
            match TunnelClient::new(config, TunnelTuning::default(), CancellationToken::new()) {
                Err(error) => error.to_string(),
                Ok(_) => panic!("unprepared OpenAI config unexpectedly constructed a client"),
            };
        assert!(
            error.contains("execution authority was not prepared"),
            "{error}"
        );
    }

    #[test]
    fn tunnel_tuning_defaults_are_valid() {
        assert!(TunnelTuning::default().validated().is_ok());
        let tuning = TunnelTuning::default();
        assert_eq!(tuning.poll_hold, Duration::from_secs(30));
        assert_eq!(tuning.request_timeout, Duration::from_secs(40));
        assert_eq!(tuning.poll_limit, CONTROL_PLANE_MAX_POLL_LIMIT);
        assert_eq!(tuning.max_in_flight, 16);
        // The validated band: 16 <= 25 <= 3 * 16 and the wire maximum.
        assert!(tuning.poll_limit <= 3 * tuning.max_in_flight);
        assert!(tuning.poll_limit <= CONTROL_PLANE_MAX_POLL_LIMIT);
        assert_eq!(tuning.activation_retry, Duration::from_secs(2));
        assert_eq!(tuning.activation_checks, 30);
        assert_eq!(tuning.activation_grace, Duration::from_secs(62));
        assert_eq!(tuning.response_attempts, 6);
        assert_eq!(tuning.backoff.base, Duration::from_millis(250));
        assert_eq!(tuning.backoff.cap, Duration::from_secs(5));
    }

    #[test]
    fn failed_client_replacement_does_not_retire_the_live_replay_namespace() {
        let process_state = ProcessState::default();
        let first = TunnelClient::new(
            Config {
                process_state: process_state.clone(),
                ..test_config()
            },
            TunnelTuning::default(),
            CancellationToken::new(),
        )
        .unwrap();
        let namespace = first.request_namespace;
        let fingerprint = [31; 32];
        assert!(matches!(
            process_state
                .replays
                .claim("live", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Leader
        ));

        let error = match TunnelClient::new(
            Config {
                tunnel_id: "replacement-tunnel".to_owned(),
                process_state: process_state.clone(),
                ..test_config()
            },
            TunnelTuning {
                response_attempts: 0,
                ..TunnelTuning::default()
            },
            CancellationToken::new(),
        ) {
            Err(error) => error,
            Ok(_) => panic!("invalid replacement tuning unexpectedly constructed a client"),
        };
        assert!(error.to_string().contains("response_attempts"));
        assert_eq!(
            process_state.replays.slot().active_namespace,
            Some(namespace),
            "a replacement that failed validation must not retire live replay authority"
        );
        assert!(matches!(
            process_state
                .replays
                .claim("live", namespace, [0; 32], fingerprint)
                .unwrap(),
            RequestReplayClaim::Wait(_)
        ));
    }

    #[test]
    fn invalid_tuning_is_rejected() {
        let base = TunnelTuning::default;
        for (label, tuning) in [
            (
                "request_timeout at the poll hold",
                TunnelTuning {
                    request_timeout: Duration::from_secs(30),
                    ..base()
                },
            ),
            (
                "request_timeout without the margin",
                TunnelTuning {
                    request_timeout: Duration::from_secs(35),
                    ..base()
                },
            ),
            (
                "connect_timeout larger than the request margin",
                TunnelTuning {
                    connect_timeout: Duration::from_secs(30),
                    ..base()
                },
            ),
            (
                "activation grace shorter than the check budget",
                TunnelTuning {
                    activation_grace: Duration::from_secs(30),
                    ..base()
                },
            ),
            (
                "poll_limit below max_in_flight",
                TunnelTuning {
                    poll_limit: 8,
                    ..base()
                },
            ),
            (
                "poll_limit above 3 * max_in_flight",
                TunnelTuning {
                    poll_limit: 49,
                    ..base()
                },
            ),
            (
                "poll_limit above the control-plane maximum",
                TunnelTuning {
                    poll_limit: CONTROL_PLANE_MAX_POLL_LIMIT + 1,
                    ..base()
                },
            ),
            (
                "max_in_flight zero",
                TunnelTuning {
                    max_in_flight: 0,
                    ..base()
                },
            ),
            (
                "activation_retry zero",
                TunnelTuning {
                    activation_retry: Duration::ZERO,
                    ..base()
                },
            ),
            (
                "activation_checks zero",
                TunnelTuning {
                    activation_checks: 0,
                    ..base()
                },
            ),
            (
                "restart_after zero",
                TunnelTuning {
                    restart_after: 0,
                    ..base()
                },
            ),
            (
                "response_attempts zero",
                TunnelTuning {
                    response_attempts: 0,
                    ..base()
                },
            ),
            (
                "backoff cap below base",
                TunnelTuning {
                    backoff: Backoff {
                        cap: Duration::from_millis(100),
                        ..base().backoff
                    },
                    ..base()
                },
            ),
            (
                "backoff cap plus jitter exceeds the request timeout",
                TunnelTuning {
                    backoff: Backoff {
                        cap: base().request_timeout,
                        ..base().backoff
                    },
                    ..base()
                },
            ),
            (
                "poll_hold overflow",
                TunnelTuning {
                    poll_hold: Duration::MAX,
                    ..base()
                },
            ),
            (
                "delivery reserve at the request timeout",
                TunnelTuning {
                    delivery_reserve: base().request_timeout,
                    ..base()
                },
            ),
        ] {
            assert!(tuning.validated().is_err(), "{label} was accepted");
        }
    }

    #[test]
    fn default_poll_query_pins_control_plane_batch_ceiling() {
        let client = TunnelClient::new(
            test_config(),
            TunnelTuning::default(),
            CancellationToken::new(),
        )
        .unwrap();
        let request = Client::new()
            .get("https://example.invalid/poll")
            .query(&client.poll_query())
            .build()
            .unwrap();
        let limit = request
            .url()
            .query_pairs()
            .find_map(|(key, value)| (key == "limit").then(|| value.into_owned()));
        assert_eq!(limit.as_deref(), Some("25"));
    }

    #[test]
    fn poll_query_carries_tuning() {
        let tuning = TunnelTuning {
            poll_hold: Duration::from_secs(20),
            request_timeout: Duration::from_secs(30),
            poll_limit: 24,
            ..TunnelTuning::default()
        };
        let client = TunnelClient::new(test_config(), tuning, CancellationToken::new()).unwrap();
        assert_eq!(
            client.poll_query(),
            [
                ("limit", "24".to_owned()),
                ("timeout_ms", "20000".to_owned())
            ]
        );

        // The pairs reach the wire as these exact parameters.
        let request = Client::new()
            .get("https://example.invalid/poll")
            .query(&client.poll_query())
            .build()
            .unwrap();
        assert_eq!(request.url().query(), Some("limit=24&timeout_ms=20000"));
    }

    #[test]
    fn activation_grace_overflow_returns_an_error() {
        let mut config = test_config();
        config.new_tunnel = true;
        let error = match TunnelClient::new(
            config,
            TunnelTuning {
                activation_grace: Duration::MAX,
                ..TunnelTuning::default()
            },
            CancellationToken::new(),
        ) {
            Ok(_) => panic!("oversized activation_grace was accepted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("activation_grace overflows"), "{error}");
    }

    #[test]
    fn client_semaphore_matches_max_in_flight() {
        let tuning = TunnelTuning {
            max_in_flight: 12,
            poll_limit: 20,
            ..TunnelTuning::default()
        };
        let client = TunnelClient::new(test_config(), tuning, CancellationToken::new()).unwrap();
        assert_eq!(client.concurrency.available_permits(), 12);
    }

    #[test]
    fn client_window_is_metadata_and_never_rewrites_shell_timeout() {
        let window = Duration::from_secs(120);
        let reserve = TunnelTuning::default().delivery_reserve;
        assert_eq!(reserve, crate::limits::OPENAI_TUNNEL_DELIVERY_RESERVE);
        assert_eq!(shell_timeout_within(window, reserve), 115);
        assert_eq!(shell_timeout_within(Duration::from_secs(3), reserve), 0);
        let mut subsecond = json!({
            "jsonrpc":"2.0","id":9,"method":"tools/call",
            "params":{"name":crate::mcp::shell_tool_name(),"arguments":{"command":"true","timeout_secs":1}}
        });
        assert_eq!(
            annotate_response_context(
                &mut subsecond,
                reserve + Duration::from_millis(999),
                reserve,
                "request",
                "token",
            ),
            Ok(Some(0))
        );
        assert_eq!(
            subsecond.pointer("/params/_meta/io.abird.dotlink~1syncBudgetMs"),
            Some(&json!(999))
        );

        let shell_name = crate::mcp::shell_tool_name();
        let shell = |arguments: Value| json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":shell_name,"arguments":arguments}});
        let sync_meta = |call: &Value| {
            call.pointer("/params/_meta/io.abird.dotlink~1syncBudgetMs")
                .and_then(Value::as_u64)
        };
        let request_meta = |call: &Value| {
            call.pointer("/params/_meta/io.abird.dotlink~1requestKey")
                .and_then(Value::as_str)
                .map(str::to_owned)
        };

        // Omitted execution timeout stays omitted: the durable runner uses the
        // profile ceiling. The transport budget controls only how long this
        // request waits before returning the same durable job/task handle.
        let mut call = shell(json!({"command":"cargo build --release"}));
        call["params"]["_meta"] = json!({
            "io.abird.dotlink/futureField": "attacker",
            "io.abird.dotlink/syncBudgetMs": 999999999_u64,
            "unrelated-client-meta": "preserve-me"
        });
        assert_eq!(
            annotate_response_context(&mut call, window, reserve, "request", "token"),
            Ok(Some(115))
        );
        assert_eq!(call.pointer("/params/arguments/timeout_secs"), None);
        assert_eq!(sync_meta(&call), Some(115_000));
        assert_eq!(request_meta(&call), Some("request".to_owned()));
        let meta = call["params"]["_meta"].as_object().unwrap();
        assert!(!meta.contains_key("io.abird.dotlink/futureField"));
        assert_eq!(
            meta.get("unrelated-client-meta"),
            Some(&json!("preserve-me"))
        );
        assert_eq!(
            call.pointer("/params/_meta/io.abird.dotlink~1transportContextToken")
                .and_then(Value::as_str),
            Some("token")
        );
        assert_eq!(
            call.pointer("/params/_meta/io.abird.dotlink~1resultBudgetBytes")
                .and_then(Value::as_u64),
            Some(crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES as u64)
        );

        // Explicit caller values are never rewritten, regardless of whether
        // they fit this request. The shell layer decides sync vs durable job.
        for value in [30_u64, 600] {
            let mut call = shell(json!({"command":"true","timeout_secs":value}));
            assert_eq!(
                annotate_response_context(&mut call, window, reserve, "request", "token"),
                Ok(Some(115))
            );
            assert_eq!(
                call.pointer("/params/arguments/timeout_secs"),
                Some(&json!(value))
            );
            assert_eq!(sync_meta(&call), Some(115_000));
        }

        // Invalid argument types are also left untouched for schema validation;
        // metadata annotation must not become a second argument parser.
        let mut call = shell(json!({"command":"true","timeout_secs":"600"}));
        assert_eq!(
            annotate_response_context(&mut call, window, reserve, "request", "token"),
            Ok(Some(115))
        );
        assert_eq!(
            call.pointer("/params/arguments/timeout_secs"),
            Some(&json!("600"))
        );
        assert_eq!(sync_meta(&call), Some(115_000));
        assert_eq!(request_meta(&call), Some("request".to_owned()));

        // Explicit background jobs carry request identity but no synchronous
        // wait budget because they return immediately by definition.
        let mut job_start = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"job_start","arguments":{"command":"true"}}});
        assert_eq!(
            annotate_response_context(&mut job_start, window, reserve, "job-request", "token"),
            Ok(None)
        );
        assert_eq!(request_meta(&job_start), Some("job-request".to_owned()));
        assert_eq!(sync_meta(&job_start), None);

        // Every tools/call gets the serialized result budget, while only
        // durable shell calls get request identity / synchronous-window metadata.
        let mut call = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read","arguments":{"path":"a"}}});
        assert_eq!(
            annotate_response_context(&mut call, window, reserve, "request", "token"),
            Ok(None)
        );
        assert_eq!(sync_meta(&call), None);
        assert_eq!(request_meta(&call), None);
        assert_eq!(
            call.pointer("/params/_meta/io.abird.dotlink~1resultBudgetBytes")
                .and_then(Value::as_u64),
            Some(crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES as u64)
        );

        // Non-call methods remain untouched.
        let mut call = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let original = call.clone();
        assert_eq!(
            annotate_response_context(&mut call, window, reserve, "request", "token"),
            Ok(None)
        );
        assert_eq!(call, original);
    }

    #[tokio::test]
    async fn shell_progress_heartbeat_is_forwarded_and_carries_no_message() {
        if crate::mcp::resolve_executable("bash").is_none() {
            return;
        }
        use crate::mcp::{AccessSpec, MachineConfig};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = LocalMachine::new(MachineConfig {
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
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 60,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());

        let call = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": "bash",
                "arguments": {"command": "sleep 1"},
                "_meta": {"progressToken": "beat"}
            }
        });
        let (tx, mut rx) = mpsc::channel(16);
        let task =
            tokio::spawn(async move { mcp.dispatch_stream(&BTreeMap::new(), &call, tx).await });
        let first = tokio::time::timeout(Duration::from_millis(700), rx.recv())
            .await
            .expect("first progress must be emitted before the shell exits")
            .expect("SSE stream closed without a progress event");
        assert!(
            !task.is_finished(),
            "progress arrived only after command completion"
        );
        let mut messages = vec![first];
        while let Some(event) = rx.recv().await {
            messages.push(event);
        }
        let mut result = task.await.unwrap().unwrap();
        messages.append(&mut result.messages);
        result.messages = messages;
        assert_eq!(result.status, 200);

        let progress: Vec<&Value> = result
            .messages
            .iter()
            .filter(|message| {
                message.get("method").and_then(Value::as_str) == Some("notifications/progress")
            })
            .collect();
        assert!(!progress.is_empty(), "{:?}", result.messages);
        for message in &progress {
            assert_eq!(message["params"]["progressToken"], "beat");
            assert!(message["params"].get("message").is_none(), "{message}");
            assert!(message["params"].get("total").is_none(), "{message}");
        }
        let numbers: Vec<f64> = progress
            .iter()
            .map(|message| message["params"]["progress"].as_f64().unwrap())
            .collect();
        assert!(
            numbers.windows(2).all(|pair| pair[0] < pair[1]),
            "{numbers:?}"
        );
        assert!(
            result
                .messages
                .iter()
                .any(crate::transports::openai::is_terminal_jsonrpc),
            "{:?}",
            result.messages
        );
    }

    #[tokio::test]
    async fn client_window_reaches_the_shell_deadline_end_to_end() {
        use crate::mcp::{AccessSpec, MachineConfig};

        if crate::mcp::resolve_executable("bash").is_none() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = LocalMachine::new(MachineConfig {
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
            host_shell: crate::host_services::HostShell::default(),
            log: LogConfig::default(),
            serialize: false,
            allow_shell: true,
            sandbox_shell: false,
            allow_network: false,
            max_shell_timeout_secs: 600,
            max_output_bytes: 4096,
            max_read_bytes: 4096,
            max_write_bytes: 4096,

            task_result_budget: None,
            job_retention_secs: crate::limits::DEFAULT_JOB_RETENTION_SECS,

            job_spool_bytes: crate::limits::DEFAULT_JOB_SPOOL_BYTES,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let mcp = EmbeddedMcp::new(machine, CancellationToken::new());
        let mut call = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "bash", "arguments": {"command": "sleep 30"}}
        });
        let reserve = TunnelTuning::default().delivery_reserve;
        let transport_token = mcp.machine.transport_context_token().to_owned();
        assert_eq!(
            annotate_response_context(
                &mut call,
                Duration::from_millis(2_000),
                reserve,
                "request",
                &transport_token
            ),
            Ok(Some(0))
        );

        let started = Instant::now();
        let result = mcp.dispatch(&BTreeMap::new(), &call).await.unwrap();
        assert_eq!(result.status, 200);
        assert!(started.elapsed() < Duration::from_secs(1));
        let message = result.messages.first().unwrap();
        let receipt_text = message["result"]["content"][0]["text"]
            .as_str()
            .expect("durable shell receipt text");
        let receipt: Value = serde_json::from_str(receipt_text).unwrap();
        assert_eq!(receipt["async"], true, "{receipt}");
        assert_eq!(receipt["timeout_secs"], 600, "{receipt}");
        assert!(receipt["job_id"].as_str().is_some(), "{receipt}");
    }

    #[test]
    fn sse_decoder_handles_split_crlf_without_delaying_events() {
        let mut parser = SseDecoder::default();
        let mut first = parser
            .feed(br#"data: {"jsonrpc":"2.0","method":"notifications/progress"}"#)
            .unwrap();
        assert!(first.is_empty());
        first.extend(parser.feed(b"\r").unwrap());
        assert!(first.is_empty());
        first.extend(parser.feed(b"\n\r").unwrap());
        assert_eq!(first.len(), 1, "event must complete at the blank line");
        assert_eq!(first[0]["method"], "notifications/progress");
        assert!(parser.feed(b"\n").unwrap().is_empty());
        assert!(parser.finish().unwrap().is_empty());
    }

    #[test]
    fn sse_parser_collects_json_data_events() {
        let input = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let messages = parse_sse_messages(input).unwrap();
        assert_eq!(messages.len(), 2);
        assert!(is_jsonrpc_notification(&messages[0]));
        assert!(is_terminal_jsonrpc(&messages[1]));
    }

    #[test]
    fn verbose_tool_summary_is_short_and_redacts_bulk_content() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "write",
                "arguments": {
                    "path": "src/main.rs",
                    "content": "secret-ish payload"
                }
            }
        });
        let summary = verbose_request_summary(&request);
        assert!(summary.contains("tools/call write"));
        assert!(summary.contains("p=\"src/main.rs\""));
        assert!(summary.contains("b=\"18\""), "{summary}");
        assert!(!summary.contains("secret-ish payload"));
    }
}
