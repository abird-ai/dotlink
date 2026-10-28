use ::http::{HeaderMap, HeaderValue};
use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, prelude::BASE64_STANDARD};
use rmcp::{model::ProtocolVersion, transport::streamable_http_server::StreamableHttpServerConfig};
use serde::Deserialize;
use serde_json::Value;
use tokio::{task::JoinSet, time};
use tokio_util::sync::CancellationToken;

use crate::{mcp::LocalMachine, recovery::Recovery};

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

/// Inspect only the MCP method and tool name, borrowing from a complete JSON
/// request. Serde skips the potentially enormous `arguments` object without
/// constructing a second Value tree. Ordinary known-tool requests then pass
/// to rmcp unchanged; unknown tools still use the canonicalization below.
#[derive(Deserialize)]
struct ToolSelector<'a> {
    method: &'a str,
    params: ToolSelectorParams<'a>,
}

#[derive(Deserialize)]
struct ToolSelectorParams<'a> {
    name: &'a str,
}

pub(crate) fn known_standard_tool_body(bytes: &[u8], machine: &LocalMachine) -> bool {
    serde_json::from_slice::<ToolSelector<'_>>(bytes).is_ok_and(|message| {
        message.method == "tools/call" && machine.has_tool_for_policy(message.params.name)
    })
}

/// Whether the *structured* request needs unknown-tool rewriting. Use before
/// cloning the OpenAI tunnel's already-parsed Value; common known-tool calls
/// should not clone every embedded binary or text payload.
pub(crate) fn unknown_standard_tool_value(
    headers: &HeaderMap,
    message: &Value,
    machine: &LocalMachine,
) -> bool {
    uses_standard_mcp_headers(headers)
        && message.get("method").and_then(Value::as_str) == Some("tools/call")
        && message
            .pointer("/params/name")
            .and_then(Value::as_str)
            .is_some_and(|name| !machine.has_tool_for_policy(name))
}

/// Canonicalize the one attacker-controlled key rmcp would otherwise add to
/// its SEP-2243 schema cache. Everything else continues through rmcp unchanged
/// so its normal HTTP, protocol metadata and standard-header validation wins.
pub(crate) fn canonicalize_unknown_standard_tool(
    headers: &mut HeaderMap,
    message: &mut Value,
    machine: &LocalMachine,
) -> bool {
    if !unknown_standard_tool_value(headers, message, machine) {
        return false;
    }
    let original_name = message
        .pointer("/params/name")
        .and_then(Value::as_str)
        .expect("the preflight found the name above");
    assert!(
        !machine.has_tool_for_policy(UNKNOWN_TOOL_SENTINEL),
        "reserved unknown-tool sentinel collides with a policy tool"
    );

    canonicalize_mcp_name_header(headers, original_name);
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TransportConstraints {
    /// Smallest serialized native-Task result budget across every active
    /// transport sharing one LocalMachine. None means all active transports can
    /// carry the profile-derived exact task result.
    pub(crate) task_result_budget: Option<usize>,
}

pub struct ActiveTransports {
    pub openai: Option<openai::Config>,
    pub stdio: bool,
    pub http: Option<http::Config>,
}

impl ActiveTransports {
    pub(crate) fn constraints(&self) -> TransportConstraints {
        let mut task_result_budget = None;
        if self.openai.is_some() {
            task_result_budget = Some(crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES);
        }
        TransportConstraints { task_result_budget }
    }

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

        supervise(&mut tasks, &cancellation).await
    }
}

/// Run transports until the first failure, then stop its peers.
async fn supervise(
    tasks: &mut JoinSet<Result<()>>,
    cancellation: &CancellationToken,
) -> Result<()> {
    while let Some(result) = tasks.join_next().await {
        let error = match result {
            // A cleanly closed transport (for example stdio EOF) does not
            // stop other active transports.
            Ok(Ok(())) => continue,
            Ok(Err(error)) => error,
            Err(error) => anyhow!("transport task panicked: {error}"),
        };
        // A peer's own restart request is not a cleanup failure. Any other
        // shutdown failure wins: resources may still be live, so it must block
        // an in-process restart.
        if let Err(shutdown) = stop_peer_transports(tasks, cancellation).await
            && !Recovery::of(&shutdown).restarts_runtime()
        {
            return Err(
                shutdown.context("peer transport shutdown failed after another transport stopped")
            );
        }
        return Err(error);
    }
    Ok(())
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
            // Keep the first failure, but never let a restart request mask a
            // failure that must block in-process restart.
            if failure.as_ref().is_none_or(|existing| {
                Recovery::of(existing).restarts_runtime()
                    && !Recovery::of(&error).restarts_runtime()
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
    fn known_tool_preflight_borrows_large_arguments_and_leaves_unknowns_to_validation() {
        let temp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::test_machine(temp.path().canonicalize().unwrap());
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":"write", "arguments":{
                "path":"file.txt", "content":"é".repeat(2 * 1024 * 1024)
            }}
        }))
        .unwrap();
        assert!(known_standard_tool_body(&body, &machine));
        let unknown =
            String::from_utf8(body.clone())
                .unwrap()
                .replacen("\"write\"", "\"other\"", 1);
        assert!(!known_standard_tool_body(unknown.as_bytes(), &machine));
        assert!(!known_standard_tool_body(
            b"{\"method\":\"tools/call\",\"params\":{\"name\":3}}",
            &machine
        ));
        assert!(!known_standard_tool_body(b"{invalid json", &machine));
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
            Err(Recovery::Runtime {
                reset_backoff: false,
            }
            .tag(anyhow!("restart")))
        });
        tasks.spawn(async { Err(anyhow!("cleanup failed")) });
        let error = stop_peer_transports(&mut tasks, &cancellation)
            .await
            .unwrap_err();
        assert_eq!(Recovery::of(&error), Recovery::Process);
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

    #[tokio::test]
    async fn transport_failures_restart_the_process_unless_classified() {
        let run = |failure: fn() -> Result<()>| async move {
            let mut tasks = JoinSet::new();
            tasks.spawn(async move { failure() });
            supervise(&mut tasks, &CancellationToken::new())
                .await
                .unwrap_err()
        };

        let panicked = run(|| panic!("transport bug")).await;
        assert_eq!(Recovery::of(&panicked), Recovery::Process);

        let unclassified = run(|| Err(anyhow!("bind failed"))).await;
        assert_eq!(Recovery::of(&unclassified), Recovery::Process);

        let rejected = run(|| Err(crate::recovery::operator(anyhow!("bad key")))).await;
        assert_eq!(Recovery::of(&rejected), Recovery::Operator);

        let restart = run(|| {
            Err(Recovery::Runtime {
                reset_backoff: true,
            }
            .tag(anyhow!("tunnel unhealthy")))
        })
        .await;
        assert_eq!(
            Recovery::of(&restart),
            Recovery::Runtime {
                reset_backoff: true
            }
        );
    }

    #[tokio::test]
    async fn peer_cleanup_failure_overrides_the_primary_classification() {
        for primary in [
            Recovery::Runtime {
                reset_backoff: false,
            },
            Recovery::Operator,
        ] {
            let mut tasks = JoinSet::new();
            tasks.spawn(async move { Err(primary.tag(anyhow!("primary failure"))) });
            tasks.spawn(std::future::pending::<Result<()>>());
            let error = supervise(&mut tasks, &CancellationToken::new())
                .await
                .unwrap_err();
            assert_eq!(Recovery::of(&error), Recovery::Process, "{primary:?}");
            assert!(format!("{error:#}").contains("shutdown deadline"));
        }
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

#[cfg(test)]
mod constraint_tests {
    use super::*;

    #[test]
    fn active_transport_constraints_derive_task_budget() {
        let local = ActiveTransports {
            openai: None,
            stdio: true,
            http: None,
        };
        assert_eq!(local.constraints().task_result_budget, None);

        let bounded = ActiveTransports {
            openai: Some(openai::Config {
                base_url: "https://example.invalid".to_owned(),
                tunnel_id: "tunnel_test".to_owned(),
                runtime_api_key: "test-key".to_owned(),
                organization_id: None,
                new_tunnel: false,
                log: crate::logging::LogConfig::default(),
                rejections: openai::RejectionLedger::default(),
            }),
            stdio: true,
            http: None,
        };
        assert_eq!(
            bounded.constraints().task_result_budget,
            Some(crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES)
        );
    }
}
