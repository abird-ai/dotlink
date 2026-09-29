use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use http::{Request, header::HeaderName};
use http_body_util::{BodyExt, Full};
use reqwest::{Client, StatusCode};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{sync::Semaphore, task::JoinSet, time};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::mcp::LocalMachine;

const POLL_LIMIT: u8 = 25;
const POLL_TIMEOUT_MS: u64 = 15_000;
const MAX_IN_FLIGHT: usize = 8;
const WIRE_PROTOCOL_VERSION: &str = "2026-08-25";
const SERVER_INFO: &str =
    r#"{"version":2,"channels":[{"name":"main","stateless":true,"proc_affinity":true}]}"#;
const MAX_CONTROL_PLANE_ERROR_BYTES: usize = 2_000;
const MAX_MCP_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

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

#[derive(Clone)]
pub struct EmbeddedMcp {
    service: StreamableHttpService<LocalMachine, NeverSessionManager>,
}

#[derive(Debug)]
struct LocalResponse {
    status: u16,
    headers: BTreeMap<String, Vec<String>>,
    messages: Vec<Value>,
}

impl EmbeddedMcp {
    pub fn new(machine: LocalMachine, cancellation: CancellationToken) -> Self {
        let config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_sse_keep_alive(None)
            .with_sse_retry(None)
            .with_cancellation_token(cancellation);

        let service = StreamableHttpService::new(
            move || Ok(machine.clone()),
            NeverSessionManager::default().into(),
            config,
        );
        Self { service }
    }

    async fn dispatch(
        &self,
        command_headers: &BTreeMap<String, Vec<String>>,
        jsonrpc: &Value,
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

        let collected = response
            .into_body()
            .collect()
            .await
            .expect("rmcp StreamableHttpService response body is infallible");
        let bytes = collected.to_bytes();
        if bytes.len() > MAX_MCP_RESPONSE_BYTES {
            bail!(
                "local MCP response exceeded {} bytes",
                MAX_MCP_RESPONSE_BYTES
            );
        }

        let messages = if bytes.is_empty() {
            Vec::new()
        } else if content_type.contains("text/event-stream") {
            parse_sse_messages(&bytes)?
        } else if content_type.contains("application/json") || looks_like_json(&bytes) {
            vec![serde_json::from_slice(&bytes).context("local MCP returned invalid JSON")?]
        } else {
            bail!("local MCP returned unsupported Content-Type {content_type:?}");
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

fn parse_sse_messages(bytes: &[u8]) -> Result<Vec<Value>> {
    let text = std::str::from_utf8(bytes).context("local MCP returned non-UTF-8 SSE")?;
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut messages = Vec::new();
    let mut data_lines: Vec<&str> = Vec::new();

    let flush = |data_lines: &mut Vec<&str>, messages: &mut Vec<Value>| -> Result<()> {
        if data_lines.is_empty() {
            return Ok(());
        }
        let payload = data_lines.join("\n");
        data_lines.clear();
        if payload.trim().is_empty() {
            return Ok(());
        }
        messages.push(serde_json::from_str(&payload).with_context(|| {
            format!(
                "invalid JSON in local MCP SSE event: {}",
                bounded(&payload, 500)
            )
        })?);
        Ok(())
    };

    for line in normalized.lines() {
        if line.is_empty() {
            flush(&mut data_lines, &mut messages)?;
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.strip_prefix(' ').unwrap_or(data));
        }
    }
    flush(&mut data_lines, &mut messages)?;
    Ok(messages)
}

#[derive(Clone)]
pub struct TunnelClient {
    http: Client,
    base_url: Arc<str>,
    tunnel_id: Arc<str>,
    api_key: Arc<str>,
    organization_id: Option<Arc<str>>,
    instance_id: Arc<str>,
    activation_grace_until: Option<Instant>,
    verbose: bool,
    cancellation: CancellationToken,
    concurrency: Arc<Semaphore>,
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

impl TunnelClient {
    pub fn new(
        base_url: String,
        tunnel_id: String,
        api_key: String,
        organization_id: Option<String>,
        newly_created: bool,
        verbose: bool,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        if tunnel_id.trim().is_empty() || api_key.trim().is_empty() {
            bail!("tunnel id and runtime API key are required");
        }
        let http = Client::builder()
            .user_agent(format!("abird-tunnel/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(25))
            .build()?;
        Ok(Self {
            http,
            base_url: Arc::from(base_url.trim_end_matches('/').to_owned()),
            tunnel_id: Arc::from(tunnel_id),
            api_key: Arc::from(api_key),
            organization_id: organization_id.map(Arc::from),
            instance_id: Arc::from(Uuid::new_v4().to_string()),
            activation_grace_until: newly_created.then(|| Instant::now() + Duration::from_secs(45)),
            verbose,
            cancellation,
            concurrency: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
        })
    }

    pub async fn run(self, mcp: EmbeddedMcp) -> Result<()> {
        let mut tasks = JoinSet::new();
        let mut failures = 0_u32;
        let mut not_ready_failures = 0_u32;
        let mut announced = false;

        loop {
            while let Some(result) = tasks.try_join_next() {
                if let Err(join_error) = result {
                    error!(%join_error, "tunnel command task failed");
                }
            }

            let batch = tokio::select! {
                _ = self.cancellation.cancelled() => break,
                result = self.poll_once() => {
                    match result {
                        Ok(batch) => {
                            failures = 0;
                            not_ready_failures = 0;
                            if !announced {
                                println!("✓ Connected — ready");
                                println!();
                                info!(tunnel_id = %self.tunnel_id, "secure MCP tunnel connected");
                                announced = true;
                            }
                            batch
                        }
                        Err(PollFailure::Transient(error)) => {
                            failures = failures.saturating_add(1);
                            warn!(%error, attempt = failures, "tunnel poll failed; retrying");
                            self.sleep_backoff(failures).await;
                            continue;
                        }
                        Err(PollFailure::NotReady(error)) => {
                            not_ready_failures = not_ready_failures.saturating_add(1);
                            if not_ready_failures == 1 {
                                eprintln!("Waiting      tunnel activation…");
                            }
                            if not_ready_failures > 30 {
                                return Err(error.context("tunnel was not available after repeated activation checks"));
                            }
                            tokio::select! {
                                _ = self.cancellation.cancelled() => break,
                                _ = time::sleep(Duration::from_secs(2)) => {}
                            }
                            continue;
                        }
                        Err(PollFailure::Fatal(error)) => return Err(error),
                    }
                }
            };

            for command in batch.commands {
                let permit = tokio::select! {
                    _ = self.cancellation.cancelled() => break,
                    permit = self.concurrency.clone().acquire_owned() => permit,
                };
                let permit = match permit {
                    Ok(permit) => permit,
                    Err(_) => break,
                };
                let client = self.clone();
                let mcp = mcp.clone();
                let received_at = batch.received_at;
                tasks.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = client.process_command(mcp, command, received_at).await {
                        warn!(%error, "tunnel command failed");
                    }
                });
            }
        }

        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        Ok(())
    }

    async fn poll_once(&self) -> std::result::Result<PolledBatch, PollFailure> {
        let url = format!("{}/v1/tunnels/{}/poll", self.base_url, self.tunnel_id);
        let response = self
            .common_headers(self.http.get(url).query(&[
                ("limit", POLL_LIMIT.to_string()),
                ("timeout_ms", POLL_TIMEOUT_MS.to_string()),
            ]))
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
            let envelope: PollEnvelope = response.json().await.map_err(|error| {
                PollFailure::Transient(anyhow!(error).context("invalid poll response"))
            })?;
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
            StatusCode::NOT_FOUND => Err(PollFailure::Fatal(error.context(
                "the Tunnel ID was not found; run `abird-tunnel --setup` to reconfigure it",
            ))),
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

    async fn process_command(
        &self,
        mcp: EmbeddedMcp,
        command: TunnelCommand,
        received_at: Instant,
    ) -> Result<()> {
        let deadline = parse_response_timeout(command.response_timeout.as_ref())
            .and_then(|duration| received_at.checked_add(duration));
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            debug!(request_id = %command.request_id, "dropping command that was already expired");
            return Ok(());
        }

        let channel = command.channel.clone().unwrap_or_else(|| "main".to_owned());
        if channel != "main" {
            warn!(request_id = %command.request_id, %channel, "ignoring unsupported tunnel channel");
            return Ok(());
        }

        match command.command_type.as_str() {
            "jsonrpc" => {
                let Some(jsonrpc) = command.jsonrpc.as_ref() else {
                    warn!(request_id = %command.request_id, "jsonrpc command had no jsonrpc payload");
                    return Ok(());
                };
                let has_request_id = jsonrpc.get("id").is_some();
                let started = Instant::now();
                if self.verbose {
                    eprintln!("{}", verbose_request_summary(jsonrpc));
                }
                let dispatch = mcp.dispatch(&command.headers, jsonrpc);
                let local = match await_before_deadline(deadline, dispatch).await {
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
                    Ok(None) => return Ok(()),
                    Err(error) => return Err(error),
                };

                if self.verbose {
                    eprintln!(
                        "← {}  {}  {}ms",
                        verbose_request_label(jsonrpc),
                        local.status,
                        started.elapsed().as_millis()
                    );
                }

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
                            notifications_deliverable =
                                self.post_notification(&command, response, deadline).await;
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
                // abird-tunnel advertises a stateless main channel, so there is no local
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
                return Ok(());
            }
            match self.post_once(command, &response, deadline).await {
                Ok(PostOutcome::Delivered) => return Ok(()),
                Ok(PostOutcome::Fatal(error)) => return Err(error),
                Ok(PostOutcome::Retry(error)) | Err(error) => {
                    attempt = attempt.saturating_add(1);
                    if attempt >= 6 {
                        return Err(error.context("exhausted tunnel response retries"));
                    }
                    let delay = retry_delay(attempt);
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
        if matches!(
            status,
            StatusCode::REQUEST_TIMEOUT
                | StatusCode::TOO_MANY_REQUESTS
                | StatusCode::BAD_GATEWAY
                | StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::GATEWAY_TIMEOUT
        ) {
            Ok(PostOutcome::Retry(error))
        } else {
            Ok(PostOutcome::Fatal(error))
        }
    }

    fn common_headers(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut request = request
            .bearer_auth(self.api_key.as_ref())
            .header("X-Tunnel-Client-Name", "abird-tunnel")
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
        let delay = retry_delay(attempt);
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

fn verbose_request_label(jsonrpc: &Value) -> String {
    let method = jsonrpc
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("jsonrpc");
    if method == "tools/call"
        && let Some(name) = jsonrpc.pointer("/params/name").and_then(Value::as_str)
    {
        return name.to_owned();
    }
    method.to_owned()
}

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

fn summarize_tool_arguments(arguments: &Value) -> String {
    let Some(object) = arguments.as_object() else {
        return String::new();
    };

    let mut parts = Vec::new();
    for key in [
        "path",
        "cwd",
        "command",
        "mode",
        "recursive",
        "parents",
        "create_parents",
        "timeout_secs",
        "max_entries",
        "max_bytes",
        "max_output_bytes",
    ] {
        let Some(value) = object.get(key) else {
            continue;
        };
        parts.push(format!("{key}={}", summarize_value(value)));
    }
    for key in ["content", "stdin"] {
        if let Some(Value::String(value)) = object.get(key) {
            parts.push(format!("{key}=<{} bytes>", value.len()));
        }
    }
    parts.join(" ")
}

fn summarize_value(value: &Value) -> String {
    match value {
        Value::String(value) => {
            let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut preview: String = compact.chars().take(120).collect();
            if compact.chars().count() > 120 {
                preview.push('…');
            }
            format!("{preview:?}")
        }
        other => other.to_string(),
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

fn retry_delay(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(5);
    let base_ms = 250_u64.saturating_mul(1_u64 << exponent).min(5_000);
    Duration::from_millis(base_ms + fastrand::u64(0..=250))
}

async fn response_error_text(response: reqwest::Response) -> String {
    let text = response.text().await.unwrap_or_default();
    bounded(&text, MAX_CONTROL_PLANE_ERROR_BYTES)
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
                "name": "fs_write_text",
                "arguments": {
                    "path": "src/main.rs",
                    "content": "secret-ish payload",
                    "create_parents": true
                }
            }
        });
        let summary = verbose_request_summary(&request);
        assert!(summary.contains("tools/call fs_write_text"));
        assert!(summary.contains("path=\"src/main.rs\""));
        assert!(summary.contains("content=<18 bytes>"));
        assert!(!summary.contains("secret-ish payload"));
    }
}
