use std::{error::Error, fmt};

use ::http::{HeaderMap, HeaderValue};
use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, prelude::BASE64_STANDARD};
use rmcp::{model::ProtocolVersion, transport::streamable_http_server::StreamableHttpServerConfig};
use serde_json::Value;
use tokio::{task::JoinSet, time};
use tokio_util::sync::CancellationToken;

use crate::mcp::LocalMachine;

#[derive(Debug)]
pub struct RuntimeRestartRequested {
    reason: String,
    reset_backoff: bool,
}

impl RuntimeRestartRequested {
    pub fn new(reason: String, reset_backoff: bool) -> Self {
        Self {
            reason,
            reset_backoff,
        }
    }

    pub fn reset_backoff(&self) -> bool {
        self.reset_backoff
    }
}

impl fmt::Display for RuntimeRestartRequested {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl Error for RuntimeRestartRequested {}

pub fn runtime_restart_request(error: &anyhow::Error) -> Option<&RuntimeRestartRequested> {
    error.downcast_ref::<RuntimeRestartRequested>()
}

/// Network Streamable HTTP keeps rmcp's SSE keepalives for intermediary idle
/// timeouts. One place owns the request-body ceiling derived from the machine's
/// write ceiling, avoiding rmcp's smaller fixed default.
pub(crate) fn streamable_config(
    machine: &LocalMachine,
    cancellation: CancellationToken,
) -> StreamableHttpServerConfig {
    StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(cancellation)
        .with_max_request_body_bytes(machine.max_request_body_bytes())
}

/// The in-process OpenAI adapter consumes events immediately and needs no wire
/// keepalives or retry hints.
pub(crate) fn embedded_streamable_config(
    machine: &LocalMachine,
    cancellation: CancellationToken,
) -> StreamableHttpServerConfig {
    streamable_config(machine, cancellation)
        .with_sse_keep_alive(None)
        .with_sse_retry(None)
}

const MCP_PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";
const MCP_NAME_HEADER: &str = "Mcp-Name";
const SEP_BASE64_PREFIX: &str = "=?base64?";
const SEP_BASE64_SUFFIX: &str = "?=";
// Empty is shorter than every non-empty attacker name, so canonicalization
// cannot push an exact-limit request over the transport byte ceiling.
const UNKNOWN_TOOL_SENTINEL: &str = "";
const UNKNOWN_TOOL_MISMATCH: &str = "~";

/// Match rmcp's SEP-2243 gate exactly: a UTF-8 protocol-version header whose
/// selected value compares lexically at or above the standard-header revision.
pub(crate) fn uses_standard_mcp_headers(headers: &HeaderMap) -> bool {
    headers
        .get(MCP_PROTOCOL_VERSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|version| version >= ProtocolVersion::STANDARD_HEADERS.as_str())
}

/// Canonicalize the one attacker-controlled key rmcp would otherwise add to
/// its SEP-2243 schema cache. Everything else continues through rmcp unchanged
/// so its normal HTTP, protocol metadata and standard-header validation wins.
pub(crate) fn canonicalize_unknown_standard_tool(
    headers: &mut HeaderMap,
    message: &mut Value,
    machine: &LocalMachine,
) -> bool {
    if !uses_standard_mcp_headers(headers)
        || message.get("method").and_then(Value::as_str) != Some("tools/call")
    {
        return false;
    }
    let Some(original_name) = message
        .pointer("/params/name")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return false;
    };
    if machine.has_tool_for_policy(&original_name) {
        return false;
    }
    assert!(
        !machine.has_tool_for_policy(UNKNOWN_TOOL_SENTINEL),
        "reserved unknown-tool sentinel collides with a policy tool"
    );

    canonicalize_mcp_name_header(headers, &original_name);
    *message
        .pointer_mut("/params/name")
        .expect("the tool name was present above") =
        Value::String(UNKNOWN_TOOL_SENTINEL.to_owned());
    true
}

fn canonicalize_mcp_name_header(headers: &mut HeaderMap, original_name: &str) {
    enum Rewrite {
        Matching,
        Mismatched,
        None,
    }

    let rewrite = {
        let mut values = headers.get_all(MCP_NAME_HEADER).iter();
        match (values.next(), values.next()) {
            (Some(value), None) => match value.to_str().ok().and_then(decode_sep_header_value) {
                Some(value) if value == original_name => Rewrite::Matching,
                Some(_) => Rewrite::Mismatched,
                None => Rewrite::None,
            },
            _ => Rewrite::None,
        }
    };
    let value = match rewrite {
        Rewrite::Matching => HeaderValue::from_static(UNKNOWN_TOOL_SENTINEL),
        // A mismatched header might already equal the body sentinel. Always use
        // a distinct fixed value so canonicalization cannot make it valid.
        Rewrite::Mismatched => HeaderValue::from_static(UNKNOWN_TOOL_MISMATCH),
        Rewrite::None => return,
    };
    headers.insert(MCP_NAME_HEADER, value);
}

/// Match rmcp's wrapper decoding: a complete wrapper must contain valid
/// Base64/UTF-8; a value missing either sentinel remains a raw header value.
fn decode_sep_header_value(value: &str) -> Option<String> {
    match value
        .strip_prefix(SEP_BASE64_PREFIX)
        .and_then(|inner| inner.strip_suffix(SEP_BASE64_SUFFIX))
    {
        Some(inner) => String::from_utf8(BASE64_STANDARD.decode(inner).ok()?).ok(),
        None => Some(value.to_owned()),
    }
}

pub mod http;
pub mod openai;
pub mod stdio;

const TRANSPORT_SHUTDOWN_BUDGET_SECS: u64 = 6;
const _: () = assert!(TRANSPORT_SHUTDOWN_BUDGET_SECS > http::HTTP_SHUTDOWN_BUDGET_SECS);

#[cfg(not(test))]
const TRANSPORT_SHUTDOWN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(TRANSPORT_SHUTDOWN_BUDGET_SECS);
#[cfg(test)]
const TRANSPORT_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(10);

pub struct ActiveTransports {
    pub openai: Option<openai::Config>,
    pub stdio: bool,
    pub http: Option<http::Config>,
}

impl ActiveTransports {
    pub fn is_empty(&self) -> bool {
        self.openai.is_none() && !self.stdio && self.http.is_none()
    }

    pub fn names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.openai.is_some() {
            names.push("openai");
        }
        if self.stdio {
            names.push("stdio");
        }
        if self.http.is_some() {
            names.push("http");
        }
        names
    }

    pub async fn run(self, machine: LocalMachine, cancellation: CancellationToken) -> Result<()> {
        if self.is_empty() {
            return Err(anyhow!("no MCP transports are active"));
        }

        let mut tasks = JoinSet::new();

        if let Some(config) = self.openai {
            let machine = machine.clone();
            let cancellation = cancellation.child_token();
            tasks.spawn(async move {
                openai::run(machine, config, cancellation)
                    .await
                    .context("OpenAI transport failed")
            });
        }

        if self.stdio {
            let machine = machine.clone();
            let cancellation = cancellation.child_token();
            tasks.spawn(async move {
                stdio::run(machine, cancellation)
                    .await
                    .context("stdio transport failed")
            });
        }

        if let Some(config) = self.http {
            let cancellation = cancellation.child_token();
            tasks.spawn(async move {
                http::run(machine, config, cancellation)
                    .await
                    .context("HTTP transport failed")
            });
        }

        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {
                    // A cleanly closed transport (for example stdio EOF) does not
                    // stop other active transports.
                }
                Ok(Err(error)) => {
                    if let Err(shutdown) = stop_peer_transports(&mut tasks, &cancellation).await
                        && runtime_restart_request(&shutdown).is_none()
                    {
                        return Err(shutdown.context(
                            "peer transport shutdown failed after another transport exited",
                        ));
                    }
                    return Err(error);
                }
                Err(error) => {
                    if let Err(shutdown) = stop_peer_transports(&mut tasks, &cancellation).await
                        && runtime_restart_request(&shutdown).is_none()
                    {
                        return Err(shutdown.context(
                            "peer transport shutdown failed after another transport panicked",
                        ));
                    }
                    return Err(anyhow!("transport task panicked: {error}"));
                }
            }
        }

        Ok(())
    }
}

async fn stop_peer_transports(
    tasks: &mut JoinSet<Result<()>>,
    cancellation: &CancellationToken,
) -> Result<()> {
    cancellation.cancel();

    let drained = time::timeout(TRANSPORT_SHUTDOWN_TIMEOUT, async {
        let mut failure = None;
        while let Some(result) = tasks.join_next().await {
            let error = match result {
                Ok(Ok(())) => continue,
                Ok(Err(error)) => error,
                Err(error) => anyhow!("peer transport task panicked during shutdown: {error}"),
            };
            if failure.as_ref().is_none_or(|existing| {
                runtime_restart_request(existing).is_some()
                    && runtime_restart_request(&error).is_none()
            }) {
                failure = Some(error);
            }
        }
        failure
    })
    .await;

    match drained {
        Ok(Some(error)) => Err(error),
        Ok(None) => Ok(()),
        Err(_) => {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            Err(anyhow!("peer transports exceeded the shutdown deadline"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_header_gate_matches_rmcp_version_detection() {
        let mut headers = HeaderMap::new();
        assert!(!uses_standard_mcp_headers(&headers));

        for (version, expected) in [
            ("2025-11-25", false),
            ("2026-07-28", true),
            ("2026-07-29", true),
        ] {
            headers.insert(
                MCP_PROTOCOL_VERSION_HEADER,
                version.parse().expect("valid test header"),
            );
            assert_eq!(uses_standard_mcp_headers(&headers), expected, "{version}");
        }

        headers.insert(
            MCP_PROTOCOL_VERSION_HEADER,
            ::http::HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        assert!(!uses_standard_mcp_headers(&headers));
    }

    #[test]
    fn unknown_tool_canonicalization_preserves_sep_header_validity_and_invalidity() {
        fn message(name: &str) -> Value {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "original-id",
                "method": "tools/call",
                "params": {"name": name, "arguments": {}}
            })
        }

        let temp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::test_machine(temp.path().canonicalize().unwrap());
        let original = "unknown tool ☃";
        let encoded = format!("=?base64?{}?=", BASE64_STANDARD.encode(original));
        let mut headers = HeaderMap::new();
        headers.insert(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28".parse().unwrap());
        headers.insert(MCP_NAME_HEADER, encoded.parse().unwrap());
        let mut body = message(original);
        assert!(canonicalize_unknown_standard_tool(
            &mut headers,
            &mut body,
            &machine
        ));
        assert_eq!(body["id"], "original-id");
        assert_eq!(body["params"]["name"], UNKNOWN_TOOL_SENTINEL);
        assert_eq!(headers[MCP_NAME_HEADER], UNKNOWN_TOOL_SENTINEL);

        // A pre-existing mismatch equal to the body sentinel must not become
        // valid merely because the body was canonicalized to that sentinel.
        let mut headers = HeaderMap::new();
        headers.insert(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28".parse().unwrap());
        headers.insert(MCP_NAME_HEADER, UNKNOWN_TOOL_SENTINEL.parse().unwrap());
        let mut body = message("different-unknown");
        assert!(canonicalize_unknown_standard_tool(
            &mut headers,
            &mut body,
            &machine
        ));
        assert_eq!(headers[MCP_NAME_HEADER], UNKNOWN_TOOL_MISMATCH);

        // Missing, duplicate and invalid wrapped values stay malformed for
        // rmcp rather than being repaired by the preflight.
        for kind in ["missing", "duplicate", "invalid-base64", "non-utf8"] {
            let mut headers = HeaderMap::new();
            headers.insert(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28".parse().unwrap());
            match kind {
                "missing" => {}
                "duplicate" => {
                    headers.append(MCP_NAME_HEADER, "different-unknown".parse().unwrap());
                    headers.append(MCP_NAME_HEADER, "different-unknown".parse().unwrap());
                }
                "invalid-base64" => {
                    headers.insert(MCP_NAME_HEADER, "=?base64?%%%?=".parse().unwrap());
                }
                "non-utf8" => {
                    headers.insert(MCP_NAME_HEADER, HeaderValue::from_bytes(&[0xff]).unwrap());
                }
                _ => unreachable!(),
            }
            let before: Vec<_> = headers
                .get_all(MCP_NAME_HEADER)
                .iter()
                .map(|value| value.as_bytes().to_vec())
                .collect();
            let mut body = message("different-unknown");
            assert!(canonicalize_unknown_standard_tool(
                &mut headers,
                &mut body,
                &machine
            ));
            let after: Vec<_> = headers
                .get_all(MCP_NAME_HEADER)
                .iter()
                .map(|value| value.as_bytes().to_vec())
                .collect();
            assert_eq!(after, before, "{kind}");
        }
    }

    #[tokio::test]
    async fn peer_shutdown_propagates_cleanup_failures() {
        let cancellation = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(async { Err(anyhow!("cleanup failed")) });
        let error = stop_peer_transports(&mut tasks, &cancellation)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cleanup failed"));
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn peer_cleanup_failure_precedes_a_restart_request() {
        let cancellation = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(async {
            Err(anyhow::Error::new(RuntimeRestartRequested::new(
                "restart".into(),
                false,
            )))
        });
        tasks.spawn(async { Err(anyhow!("cleanup failed")) });
        let error = stop_peer_transports(&mut tasks, &cancellation)
            .await
            .unwrap_err();
        assert!(runtime_restart_request(&error).is_none());
        assert_eq!(error.to_string(), "cleanup failed");
    }

    #[test]
    fn transport_parent_reserves_time_beyond_http_shutdown_budget() {
        assert_eq!(http::HTTP_SHUTDOWN_BUDGET_SECS, 5);
        assert_eq!(TRANSPORT_SHUTDOWN_BUDGET_SECS, 6);
    }

    #[test]
    fn runtime_transport_modules_do_not_write_human_status_to_stdout() {
        for (name, source) in [
            ("openai", include_str!("openai.rs")),
            ("http", include_str!("http.rs")),
            ("stdio", include_str!("stdio.rs")),
        ] {
            let writes_stdout = source.lines().any(|line| {
                let line = line.trim_start();
                line.starts_with("println!(") || line.starts_with("print!(")
            });
            assert!(
                !writes_stdout,
                "{name} transport must keep stdout protocol-clean"
            );
        }
    }

    #[test]
    fn runtime_restart_marker_survives_transport_context() {
        let error = anyhow::Error::new(RuntimeRestartRequested::new("restart".into(), true))
            .context("OpenAI transport failed");
        let request =
            runtime_restart_request(&error).expect("restart marker should survive context");
        assert!(request.reset_backoff());
    }

    #[tokio::test]
    async fn peer_transport_shutdown_cancels_and_drains() {
        let cancellation = CancellationToken::new();
        let child = cancellation.child_token();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            child.cancelled().await;
            Ok(())
        });

        stop_peer_transports(&mut tasks, &cancellation)
            .await
            .unwrap();

        assert!(cancellation.is_cancelled());
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn peer_transport_shutdown_force_aborts_unresponsive_task() {
        let cancellation = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(std::future::pending::<Result<()>>());

        let error = stop_peer_transports(&mut tasks, &cancellation)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("shutdown deadline"));
        assert!(cancellation.is_cancelled());
        assert!(tasks.is_empty());
    }
}
