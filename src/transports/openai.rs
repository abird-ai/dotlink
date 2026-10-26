use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use http::{Request, header::HeaderName};
use http_body_util::Full;
use reqwest::{Client, StatusCode};
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::never::NeverSessionManager,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinSet,
    time,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{backoff::Backoff, bounded, logging::LogConfig, mcp::LocalMachine};

/// Headroom over the poll hold for the HTTP request timeout, so an idle
/// long-poll is never cut short by the client timeout.
const REQUEST_TIMEOUT_MARGIN: Duration = Duration::from_secs(10);
const WIRE_PROTOCOL_VERSION: &str = "2026-08-25";
const SERVER_INFO: &str =
    r#"{"version":2,"channels":[{"name":"main","stateless":true,"proc_affinity":true}]}"#;
const MAX_CONTROL_PLANE_ERROR_BYTES: usize = 2_000;
// A poll may contain multiple commands; limit the whole external envelope
// before serde deserializes it. Large transfers can use later polls.
const MAX_POLL_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// A deliberate process-memory bound: even an oversized configured write must
/// not make one remote envelope consume arbitrary memory.
const MAX_SAFE_POLL_RESPONSE_BYTES: usize = 256 * 1024 * 1024;
/// Tunnel command wrappers (request id, shard token, headers, JSONRPC framing).
const TUNNEL_COMMAND_OVERHEAD_BYTES: usize = 1024 * 1024;

fn poll_response_ceiling(max_request_bytes: usize) -> Result<usize> {
    let needed = max_request_bytes
        .checked_add(TUNNEL_COMMAND_OVERHEAD_BYTES)
        .ok_or_else(|| anyhow!("OpenAI tunnel request ceiling overflows the address space"))?;
    if needed > MAX_SAFE_POLL_RESPONSE_BYTES {
        bail!(
            "OpenAI tunnel cannot carry a configured single request of {max_request_bytes} bytes              inside the {} MiB safe poll envelope; lower max_write_bytes or use local HTTP/stdio",
            MAX_SAFE_POLL_RESPONSE_BYTES / (1024 * 1024),
        );
    }
    Ok(MAX_POLL_RESPONSE_BYTES.max(needed))
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
    /// Commands requested per poll, kept in `max_in_flight ..= 3 * max_in_flight`:
    /// a batch drains into tasks that queue on one semaphore while their client
    /// window keeps running, so a limit far above the capacity expires the tail.
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
    /// Consecutive transient poll failures before the runtime restarts.
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
        if self.activation_retry.is_zero() {
            bail!("tunnel tuning: activation_retry must be non-zero");
        }
        if self.activation_checks == 0 {
            bail!("tunnel tuning: activation_checks must be non-zero");
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
        let poll_hold = Duration::from_secs(15);
        let activation_retry = Duration::from_secs(2);
        let activation_checks: u32 = 30;
        Self {
            poll_hold,
            poll_limit: 32,
            max_in_flight: 16,
            connect_timeout: Duration::from_secs(10),
            request_timeout: poll_hold + REQUEST_TIMEOUT_MARGIN,
            // One extra interval so the last check is not raced by the grace
            // deadline expiring before it is attempted.
            activation_grace: activation_retry.saturating_mul(activation_checks + 1),
            activation_retry,
            activation_checks,
            restart_after: 10,
            response_attempts: 6,
            backoff: Backoff {
                base: Duration::from_millis(250),
                cap: Duration::from_secs(5),
                jitter: Duration::from_millis(250),
            },
            delivery_reserve: Duration::from_secs(5),
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
    pub log: LogConfig,
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
    let max_request_bytes = machine.max_request_body_bytes();
    let poll_ceiling = poll_response_ceiling(max_request_bytes)?;
    let embedded = EmbeddedMcp::new(machine, cancellation.child_token());
    let tunnel = TunnelClient::new(config, tuning, cancellation)?;

    match tunnel.run(embedded, poll_ceiling).await? {
        TunnelRunOutcome::Cancelled => Ok(()),
        TunnelRunOutcome::RestartRuntime {
            error,
            had_success,
            threshold,
        } => Err(anyhow::Error::new(super::RuntimeRestartRequested::new(
            format!("OpenAI tunnel hit {threshold} consecutive transient poll failures: {error:#}"),
            had_success,
        ))),
    }
}

#[derive(Clone)]
pub(crate) struct EmbeddedMcp {
    service: StreamableHttpService<LocalMachine, NeverSessionManager>,
    max_response_bytes: usize,
}

#[derive(Debug)]
struct LocalResponse {
    status: u16,
    headers: BTreeMap<String, Vec<String>>,
    messages: Vec<Value>,
}

impl EmbeddedMcp {
    pub(crate) fn new(machine: LocalMachine, cancellation: CancellationToken) -> Self {
        let max_response_bytes = machine.max_response_body_bytes();
        let config = super::streamable_config(&machine, cancellation);
        let service = StreamableHttpService::new(
            move || Ok(machine.clone()),
            NeverSessionManager::default().into(),
            config,
        );
        Self {
            service,
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
        let body = serde_json::to_vec(jsonrpc).context("failed to serialize MCP command")?;
        let mut request = Request::builder()
            .method("POST")
            .uri("http://localhost/mcp")
            .header("Host", "localhost")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        for (name, values) in command_headers {
            let lower = name.to_ascii_lowercase();
            if !request_header_allowed(&lower) {
                continue;
            }
            let Ok(header_name) = HeaderName::from_bytes(lower.as_bytes()) else {
                continue;
            };
            for value in values {
                request = request.header(header_name.clone(), value.as_str());
            }
        }

        let request = request
            .body(Full::new(Bytes::from(body)))
            .context("failed to build in-process MCP request")?;
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
        events.push(serde_json::from_str(&payload).with_context(|| {
            format!(
                "invalid JSON in local MCP SSE event: {}",
                bounded(&payload, 500)
            )
        })?);
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

#[derive(Debug, Serialize)]
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

#[derive(Debug)]
struct PolledBatch {
    received_at: Instant,
    commands: Vec<TunnelCommand>,
}

enum PollFailure {
    Transient(anyhow::Error),
    NotReady(anyhow::Error),
    Fatal(anyhow::Error),
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
            log,
        } = config;
        if tunnel_id.trim().is_empty() || api_key.trim().is_empty() {
            bail!("tunnel id and runtime API key are required");
        }
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
        Ok(Self {
            http,
            base_url: Arc::from(base_url.trim_end_matches('/').to_owned()),
            tunnel_id: Arc::from(tunnel_id),
            api_key: Arc::from(api_key),
            organization_id: organization_id.map(Arc::from),
            instance_id: Arc::from(Uuid::new_v4().to_string()),
            activation_grace_until,
            log,
            cancellation,
            concurrency: Arc::new(Semaphore::new(tuning.max_in_flight)),
            tuning,
        })
    }

    async fn run(self, mcp: EmbeddedMcp, poll_ceiling: usize) -> Result<TunnelRunOutcome> {
        debug!(?self.tuning, "tunnel tuning");
        let mut tasks = JoinSet::new();
        let mut failures = 0_u32;
        let mut not_ready_failures = 0_u32;
        let mut announced = false;
        let mut reconnect_reason = None;

        'run: loop {
            while let Some(result) = tasks.try_join_next() {
                if let Err(join_error) = result {
                    error!(%join_error, "tunnel command task failed");
                }
            }

            let batch = tokio::select! {
                _ = self.cancellation.cancelled() => break,
                result = self.poll_once(poll_ceiling) => {
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
                                _ = self.cancellation.cancelled() => break,
                                _ = time::sleep(self.tuning.activation_retry) => {}
                            }
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
                    let permit = tokio::select! {
                        _ = client.cancellation.cancelled() => return,
                        permit = client.concurrency.clone().acquire_owned() => permit,
                    };
                    let Ok(_permit) = permit else {
                        return;
                    };
                    if let Err(error) = client.process_command(mcp, command, received_at).await {
                        warn!(%error, "tunnel command failed");
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

        let message = response_error_text(response).await;
        let error = anyhow!("OpenAI tunnel poll failed ({status}): {message}");
        let in_activation_grace = self
            .activation_grace_until
            .is_some_and(|until| Instant::now() < until);
        match status {
            StatusCode::NOT_FOUND if in_activation_grace => Err(PollFailure::NotReady(error)),
            StatusCode::FORBIDDEN if in_activation_grace => Err(PollFailure::NotReady(error)),
            StatusCode::NOT_FOUND => {
                Err(PollFailure::Fatal(error.context(
                    "the Tunnel ID was not found; run `dotlink --setup` to reconfigure it",
                )))
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(PollFailure::Fatal(
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

    async fn process_command(
        &self,
        mcp: EmbeddedMcp,
        mut command: TunnelCommand,
        received_at: Instant,
    ) -> Result<()> {
        let deadline = parse_response_timeout(command.response_timeout.as_ref())
            .and_then(|duration| received_at.checked_add(duration));
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            warn!(
                request_id = %command.request_id,
                "dropping command the client had already expired; no response is deliverable"
            );
            return Ok(());
        }

        let channel = command.channel.clone().unwrap_or_else(|| "main".to_owned());
        if channel != "main" {
            warn!(request_id = %command.request_id, %channel, "ignoring unsupported tunnel channel");
            return Ok(());
        }

        // Keep the delivery reserve for the terminal response. Shell timeouts
        // are shortened explicitly; every other dispatch is bounded by the
        // same effective deadline.
        let dispatch_deadline = deadline.map(|deadline| {
            deadline
                .checked_sub(self.tuning.delivery_reserve)
                .unwrap_or_else(Instant::now)
        });
        if let Some(remaining) =
            deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
            && let Some(jsonrpc) = command.jsonrpc.as_mut()
            && let Some(secs) =
                apply_response_window(jsonrpc, remaining, self.tuning.delivery_reserve)
        {
            debug!(
                request_id = %command.request_id,
                window_secs = remaining.as_secs_f32(),
                timeout_secs = secs,
                "bounded shell timeout to the client response window"
            );
        }

        match command.command_type.as_str() {
            "jsonrpc" => {
                let Some(jsonrpc) = command.jsonrpc.as_ref() else {
                    warn!(request_id = %command.request_id, "jsonrpc command had no jsonrpc payload");
                    return Ok(());
                };
                let has_request_id = jsonrpc.get("id").is_some();
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
                            warn!(request_id = %command.request_id, %error, "local MCP dispatch failed");
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
                                self.post_terminal(&command, response, deadline).await?;
                            } else {
                                let response = TunnelResponse {
                                    request_id: command.request_id.clone(),
                                    channel: command.channel.clone(),
                                    resp_json: None,
                                    resp_headers: BTreeMap::new(),
                                    resp_code: 502,
                                    resp_type: "notify_ack".to_owned(),
                                };
                                self.post_terminal(&command, response, deadline).await?;
                            }
                            return Ok(());
                        }
                    },
                    Ok(None) => {
                        warn!(
                            request_id = %command.request_id,
                            request = %request_label,
                            "client response window expired mid-call; result was not deliverable"
                        );
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };

                self.log
                    .request_done("openai", &request_label, started, local.status.to_string());

                if !has_request_id {
                    let response = TunnelResponse {
                        request_id: command.request_id.clone(),
                        channel: command.channel.clone(),
                        resp_json: None,
                        resp_headers: local.headers,
                        resp_code: local.status,
                        resp_type: "notify_ack".to_owned(),
                    };
                    self.post_terminal(&command, response, deadline).await?;
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
                        debug!(request_id = %command.request_id, "ignoring unsupported nonterminal MCP stream message");
                    }
                }

                let (resp_json, resp_code) = match terminal {
                    Some(message) => (message, local.status),
                    None => (synthesized_jsonrpc_error(jsonrpc, "protocol"), 502),
                };
                let response = TunnelResponse {
                    request_id: command.request_id.clone(),
                    channel: command.channel.clone(),
                    resp_json: Some(resp_json),
                    resp_headers: local.headers,
                    resp_code,
                    resp_type: "jsonrpc_response".to_owned(),
                };
                self.post_terminal(&command, response, deadline).await?;
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
                self.post_terminal(&command, response, deadline).await?;
            }
            other => {
                warn!(request_id = %command.request_id, command_type = other, "ignoring unknown tunnel command type");
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
        match self.post_once(command, &response, deadline).await {
            Ok(PostOutcome::Delivered) => true,
            Ok(PostOutcome::Retry(error)) | Ok(PostOutcome::Fatal(error)) | Err(error) => {
                warn!(request_id = %command.request_id, %error, "dropping intermediate tunnel notification after send failure");
                false
            }
        }
    }

    async fn post_terminal(
        &self,
        command: &TunnelCommand,
        response: TunnelResponse,
        deadline: Option<Instant>,
    ) -> Result<()> {
        let mut attempt = 0_u32;
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                warn!(
                    request_id = %command.request_id,
                    "client response window expired before the response could be posted"
                );
                return Ok(());
            }
            match self.post_once(command, &response, deadline).await {
                Ok(PostOutcome::Delivered) => return Ok(()),
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
                        return Ok(());
                    }
                    tokio::select! {
                        _ = self.cancellation.cancelled() => return Ok(()),
                        _ = time::sleep(delay) => {}
                    }
                }
            }
        }
    }

    async fn post_once(
        &self,
        command: &TunnelCommand,
        response_body: &TunnelResponse,
        deadline: Option<Instant>,
    ) -> Result<PostOutcome> {
        let url = format!("{}/v1/tunnels/{}/response", self.base_url, self.tunnel_id);
        let future = self
            .common_headers(self.http.post(url))
            .header("X-Tunnel-Shard-Token", command.shard_token.as_str())
            .json(response_body)
            .send();

        let response = match await_before_deadline(deadline, future).await? {
            Some(result) => match result {
                Ok(response) => response,
                Err(error) => return Ok(PostOutcome::Retry(anyhow!(error))),
            },
            None => return Ok(PostOutcome::Delivered),
        };
        let status = response.status();
        if status.is_success() || status == StatusCode::NOT_FOUND {
            // A response POST may return 404 after the request has already been
            // fulfilled or expired. The protocol treats that command as terminal.
            return Ok(PostOutcome::Delivered);
        }
        let message = response_error_text(response).await;
        let error = anyhow!("OpenAI tunnel response failed ({status}): {message}");
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
        tokio::select! {
            _ = self.cancellation.cancelled() => {}
            _ = time::sleep(delay) => {}
        }
    }
}

enum PostOutcome {
    Delivered,
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
    window.saturating_sub(reserve).as_secs().max(1)
}

/// Thread the client-owned response window into a shell `tools/call`.
///
/// The caller's `timeout_secs` is client-controlled and preserved when it fits;
/// otherwise it is shortened so dotlink can return a real tool result instead of
/// being dropped at the window boundary. The policy ceiling still applies later.
fn apply_response_window(jsonrpc: &mut Value, window: Duration, reserve: Duration) -> Option<u64> {
    if jsonrpc.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    let name = jsonrpc.pointer("/params/name").and_then(Value::as_str)?;
    // The capability table is the only list of shell tools; a renamed or added
    // shell variant is bounded by the delivery reserve without a second edit.
    if !crate::mcp::is_shell_tool(name) {
        return None;
    }
    let object = jsonrpc.pointer_mut("/params/arguments")?.as_object_mut()?;
    let budget = shell_timeout_within(window, reserve);
    let applied = match object.get("timeout_secs") {
        Some(value) => value.as_u64()?.min(budget),
        None => budget,
    };
    object.insert("timeout_secs".to_owned(), Value::from(applied));
    Some(applied)
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
    let args = arguments.map(summarize_tool_arguments).unwrap_or_default();
    if args.is_empty() {
        format!("→ tools/call {name}")
    } else {
        format!("→ tools/call {name}  {args}")
    }
}

#[cfg(test)]
fn summarize_tool_arguments(arguments: &Value) -> String {
    // One summarizer, shared with the TOOL log: argument order comes from the
    // object itself here because a test has no tool schema to read.
    match arguments.as_object() {
        Some(object) => {
            crate::logging::summarize_arguments(object, object.keys().map(String::as_str))
        }
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
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32603,
            "message": "Bad Gateway",
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

async fn response_error_text(response: reqwest::Response) -> String {
    match bounded::reqwest_response(
        response,
        MAX_CONTROL_PLANE_ERROR_BYTES,
        "control-plane error",
    )
    .await
    {
        Ok(bytes) => bounded(
            &String::from_utf8_lossy(&bytes),
            MAX_CONTROL_PLANE_ERROR_BYTES,
        ),
        Err(error) => error.to_string(),
    }
}

fn bounded(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
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
            log: LogConfig::default(),
        }
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
                assert_eq!(a.unwrap().status, 200);
                assert_eq!(b.unwrap().status, 200);
                let log = fs::read_to_string(events).unwrap();
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
    async fn embedded_binary_read_respects_configured_read_ceiling_not_fixed_16mib() {
        use crate::mcp::{AccessSpec, MachineConfig};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let size = 13 * 1024 * 1024;
        std::fs::write(root.join("large.bin"), vec![b'z'; size]).unwrap();
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
            max_read_bytes: 20 * 1024 * 1024,
            max_write_bytes: 1024 * 1024,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();
        let embedded = EmbeddedMcp::new(machine, CancellationToken::new());
        assert!(embedded.max_response_bytes > 16 * 1024 * 1024);
        let response = embedded
            .dispatch(
                &BTreeMap::new(),
                &json!({
                    "jsonrpc": "2.0", "id": 42, "method": "tools/call",
                    "params": {"name": "bread", "arguments": {
                        "path": "large.bin", "format": "base64"}}
                }),
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        let result = response
            .messages
            .iter()
            .find(|value| value.get("id") == Some(&json!(42)))
            .expect("large binary MCP response");
        assert_ne!(result["result"]["isError"], true);
        let receipt = result["result"]["content"][0]["text"].as_str().unwrap();
        assert!(receipt.len() > 16 * 1024 * 1024);
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
    fn poll_ceiling_covers_a_configured_single_request_and_fails_closed_for_oversize() {
        assert_eq!(
            poll_response_ceiling(1_000_000).unwrap(),
            MAX_POLL_RESPONSE_BYTES
        );
        // 12 MiB of control characters can expand to 72 MiB of JSON: the
        // previous fixed 64 MiB ceiling rejected a valid tool request.
        let large_request = 12 * 1024 * 1024 * 6 + 64 * 1024;
        let raised = poll_response_ceiling(large_request).unwrap();
        assert!(raised > MAX_POLL_RESPONSE_BYTES);
        assert!(raised >= large_request + TUNNEL_COMMAND_OVERHEAD_BYTES);
        assert!(poll_response_ceiling(MAX_SAFE_POLL_RESPONSE_BYTES).is_err());
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

    #[tokio::test]
    async fn repeated_transient_poll_failures_request_runtime_restart() {
        use crate::mcp::{AccessSpec, MachineConfig};

        let temp = tempfile::tempdir().unwrap();
        let base_dir = temp.path().to_path_buf();
        let machine = LocalMachine::new(MachineConfig {
            access: AccessSpec {
                base_dir: base_dir.clone(),
                read_roots: vec![base_dir],
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
            max_shell_timeout_secs: 1,
            max_output_bytes: 1024,
            max_read_bytes: 1024,
            max_write_bytes: 1024,
            protected_paths: Vec::new(),
        })
        .await
        .unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let tuning = TunnelTuning {
            backoff: Backoff {
                base: Duration::from_millis(1),
                cap: Duration::from_millis(2),
                jitter: Duration::ZERO,
            },
            ..TunnelTuning::default()
        };
        let error = run_with_tuning(
            machine,
            Config {
                base_url: format!("http://{addr}"),
                tunnel_id: "tunnel_test".into(),
                runtime_api_key: "test-key".into(),
                organization_id: None,
                new_tunnel: false,
                log: LogConfig::default(),
            },
            tuning,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

        let request = crate::transports::runtime_restart_request(&error)
            .expect("transient failure threshold should request a restart");
        assert!(!request.reset_backoff());
        assert!(
            error
                .to_string()
                .contains("10 consecutive transient poll failures")
        );
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
    fn tunnel_tuning_defaults_are_valid() {
        assert!(TunnelTuning::default().validated().is_ok());
        let tuning = TunnelTuning::default();
        assert_eq!(tuning.poll_hold, Duration::from_secs(15));
        assert_eq!(tuning.request_timeout, Duration::from_secs(25));
        assert_eq!(tuning.poll_limit, 32);
        assert_eq!(tuning.max_in_flight, 16);
        // The validated band: 16 <= 32 <= 3 * 16.
        assert!(tuning.poll_limit <= 3 * tuning.max_in_flight);
        assert_eq!(tuning.activation_retry, Duration::from_secs(2));
        assert_eq!(tuning.activation_checks, 30);
        assert_eq!(tuning.activation_grace, Duration::from_secs(62));
        assert_eq!(tuning.response_attempts, 6);
        assert_eq!(tuning.backoff.base, Duration::from_millis(250));
        assert_eq!(tuning.backoff.cap, Duration::from_secs(5));
    }

    #[test]
    fn invalid_tuning_is_rejected() {
        let base = TunnelTuning::default;
        for (label, tuning) in [
            (
                "request_timeout at the poll hold",
                TunnelTuning {
                    request_timeout: Duration::from_secs(15),
                    ..base()
                },
            ),
            (
                "request_timeout without the margin",
                TunnelTuning {
                    request_timeout: Duration::from_secs(20),
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
                        cap: Duration::from_secs(30),
                        jitter: Duration::ZERO,
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
                    delivery_reserve: Duration::from_secs(25),
                    ..base()
                },
            ),
        ] {
            assert!(tuning.validated().is_err(), "{label} was accepted");
        }
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
    fn client_window_bounds_shell_timeouts_only() {
        let window = Duration::from_secs(120);
        let reserve = TunnelTuning::default().delivery_reserve;
        assert_eq!(reserve, Duration::from_secs(5));
        assert_eq!(shell_timeout_within(window, reserve), 115);
        assert_eq!(shell_timeout_within(Duration::from_secs(3), reserve), 1);

        let shell = |arguments: Value| json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"bash","arguments":arguments}});

        // Omitted -> derived from the client window.
        let mut call = shell(json!({"command":"cargo build --release"}));
        assert_eq!(apply_response_window(&mut call, window, reserve), Some(115));
        assert_eq!(
            call.pointer("/params/arguments/timeout_secs"),
            Some(&json!(115))
        );

        // Caller value is preserved when it fits.
        let mut call = shell(json!({"command":"true","timeout_secs":30}));
        assert_eq!(apply_response_window(&mut call, window, reserve), Some(30));
        assert_eq!(
            call.pointer("/params/arguments/timeout_secs"),
            Some(&json!(30))
        );

        // Caller value is shortened to keep the response deliverable.
        let mut call = shell(json!({"command":"true","timeout_secs":600}));
        assert_eq!(apply_response_window(&mut call, window, reserve), Some(115));
        assert_eq!(
            call.pointer("/params/arguments/timeout_secs"),
            Some(&json!(115))
        );

        // Wrong types are left to schema validation instead of being rewritten.
        let mut call = shell(json!({"command":"true","timeout_secs":"600"}));
        assert_eq!(apply_response_window(&mut call, window, reserve), None);
        assert_eq!(
            call.pointer("/params/arguments/timeout_secs"),
            Some(&json!("600"))
        );

        // Non-shell tools and non-call methods are untouched.
        let mut call = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read","arguments":{"path":"a"}}});
        let original = call.clone();
        assert_eq!(apply_response_window(&mut call, window, reserve), None);
        assert_eq!(call, original);

        let mut call = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let original = call.clone();
        assert_eq!(apply_response_window(&mut call, window, reserve), None);
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
        assert_eq!(
            apply_response_window(&mut call, Duration::from_millis(2_000), reserve),
            Some(1)
        );

        let started = Instant::now();
        let result = mcp.dispatch(&BTreeMap::new(), &call).await.unwrap();
        assert_eq!(result.status, 200);
        assert!(started.elapsed() < Duration::from_secs(10));
        let body = result.messages.first().unwrap().to_string();
        assert!(body.contains("timed out after 1 seconds"), "{body}");
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
        assert!(summary.contains("path=\"src/main.rs\""));
        assert!(summary.contains("content_bytes=18"), "{summary}");
        assert!(!summary.contains("secret-ish payload"));
    }
}
