//! Standalone finite Job control surface over the canonical dotlink JobManager.
//!
//! Process/Graph execution, retention, replay, output ownership and ACK remain in
//! `crate::jobs`. This module is only the flat model-facing projection.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Tool},
    schemars,
    service::{RequestContext, RoleServer},
    tool, tool_router,
};
use serde::Deserialize;

use aw_tools::execution_ref::{ExecutionRef, ExecutionSegment};

use super::{LocalMachine, graph, ok_text, tool_error};
use crate::jobs::{
    DOTLINK_JOB_PROFILE, JobHandoff, JobOutputStream, JobWait, ProcessInput, RemoteStartDecision,
    RemoteStartGuard, RemoteStartReceipt,
};

/// Host-verified transport response windows may only narrow a caller's wait.
/// This never changes Process/Graph execution or the Job's retained identity.
fn bounded_wait_ms(requested: u64, transport_remaining: Option<u64>) -> u64 {
    transport_remaining.map_or(requested, |remaining| requested.min(remaining))
}

#[cfg(test)]
#[test]
fn wait_budget_only_narrows_by_authenticated_transport_remaining() {
    assert_eq!(
        bounded_wait_ms(DOTLINK_JOB_PROFILE.get_wait_max_ms, None),
        DOTLINK_JOB_PROFILE.get_wait_max_ms
    );
    assert_eq!(
        bounded_wait_ms(DOTLINK_JOB_PROFILE.get_wait_max_ms, Some(7_000)),
        7_000
    );
    assert_eq!(
        bounded_wait_ms(DOTLINK_JOB_PROFILE.get_wait_max_ms, Some(0)),
        0
    );
    assert_eq!(bounded_wait_ms(0, Some(7_000)), 0);
}

/// Read-only status must be complete; outer response truncation would
/// destroy its outcome/revision semantics while still reporting success.
fn bounded_job_status(status: String, budget: Option<usize>) -> CallToolResult {
    let result = ok_text(status);
    if budget.is_none_or(|limit| {
        crate::mcp::serialized_json_len(&result).is_some_and(|size| size <= limit)
    }) {
        return result;
    }
    tool_error(
        "Job status exceeds the response budget; use an exact node ref or increase the budget",
    )
}

/// A launched Job must retain a complete model-visible continuation ref, even
/// when its full status cannot fit the authenticated transport envelope.
fn bounded_job_handoff(id: &str, status: String, budget: Option<usize>) -> CallToolResult {
    let result = ok_text(status);
    if budget.is_none_or(|limit| {
        crate::mcp::serialized_json_len(&result).is_some_and(|size| size <= limit)
    }) {
        return result;
    }
    let concise = ok_text(format!("j:{id} next=jobs_get no-rerun"));
    if budget.is_none_or(|limit| {
        crate::mcp::serialized_json_len(&concise).is_some_and(|size| size <= limit)
    }) {
        concise
    } else {
        tool_error(
            "Job is already admitted, but its handoff exceeds the response budget; list Jobs to recover it without rerunning",
        )
    }
}

/// The replay ledger retains the original receipt. A narrower authenticated
/// response may project the same Job ref compactly; it may never rerun work or
/// silently clip away the continuation.
fn bounded_replayed_job_handoff(cached: CallToolResult, budget: Option<usize>) -> CallToolResult {
    if cached.is_error.unwrap_or(false)
        || budget.is_none_or(|limit| {
            crate::mcp::serialized_json_len(&cached).is_some_and(|size| size <= limit)
        })
    {
        return cached;
    }
    let Some(status) = cached
        .content
        .iter()
        .find_map(|block| block.as_text().map(|text| text.text.as_str()))
    else {
        return tool_error("Cached Job receipt has no continuation; list Jobs, do not rerun");
    };
    let Some(reference) = status.split_whitespace().next() else {
        return tool_error("Cached Job receipt has no continuation; list Jobs, do not rerun");
    };
    let meta = cached.meta.clone();
    let fits = |result: &CallToolResult| {
        budget.is_none_or(|limit| {
            crate::mcp::serialized_json_len(result).is_some_and(|size| size <= limit)
        })
    };
    match parse_ref(reference) {
        Ok(StandaloneRef { id, node: None }) => {
            let mut full = ok_text(status.to_owned());
            full.meta = meta.clone();
            if fits(&full) {
                return full;
            }
            let mut concise = ok_text(format!("j:{id} next=jobs_get no-rerun"));
            concise.meta = meta.clone();
            if fits(&concise) {
                concise
            } else {
                let mut error = tool_error(
                    "Job is already admitted, but its replay handoff exceeds the response budget; list Jobs to recover it without rerunning",
                );
                error.meta = meta;
                error
            }
        }
        _ => {
            let mut error = tool_error("Cached Job reference is invalid; list Jobs, do not rerun");
            error.meta = meta;
            error
        }
    }
}

#[derive(Clone, Copy)]
enum ControlReceipt {
    AckThrough(u64),
    Finish,
    Cancel,
}

fn bounded_control_success(
    reference: &StandaloneRef,
    status: String,
    effect: ControlReceipt,
    budget: Option<usize>,
) -> CallToolResult {
    let result = ok_text(status);
    let fits = |result: &CallToolResult| {
        budget.is_none_or(|limit| {
            crate::mcp::serialized_json_len(result).is_some_and(|size| size <= limit)
        })
    };
    if fits(&result) {
        return result;
    }
    let reference = crate::jobs::JobManager::standalone_ref(&reference.id, reference.node);
    let receipt = match effect {
        ControlReceipt::AckThrough(through) => {
            format!("{reference} ack=through:{through} next=jobs_get no-rerun")
        }
        ControlReceipt::Finish => format!("{reference} ack=finish next=jobs_get no-rerun"),
        ControlReceipt::Cancel => format!("{reference} cancel=accepted next=jobs_get no-rerun"),
    };
    let compact = ok_text(receipt);
    if fits(&compact) {
        compact
    } else {
        tool_error(format!(
            "{reference} Job Control effect already applied, but its receipt exceeds the response budget; inspect with jobs_get before retrying"
        ))
    }
}

/// Reproject the original completed effect; never mutate its replay receipt
/// or convert an error into a successful Control acknowledgement.
fn bounded_control_replay(
    cached: CallToolResult,
    reference: &StandaloneRef,
    effect: ControlReceipt,
    budget: Option<usize>,
) -> CallToolResult {
    if cached.is_error.unwrap_or(false) {
        return cached;
    }
    let expected = crate::jobs::JobManager::standalone_ref(&reference.id, reference.node);
    let Some(status) = cached
        .content
        .iter()
        .find_map(|block| block.as_text().map(|text| text.text.as_str()))
    else {
        return tool_error("Cached Control outcome is unverified; inspect the Job before retrying");
    };
    if status.split_whitespace().next() != Some(expected.as_str()) {
        return tool_error("Cached Control Job identity differs; inspect the Job before retrying");
    }
    if budget.is_none_or(|limit| {
        crate::mcp::serialized_json_len(&cached).is_some_and(|size| size <= limit)
    }) {
        return cached;
    }
    let receipt = match effect {
        ControlReceipt::AckThrough(through) => {
            format!("{expected} ack=through:{through} next=jobs_get no-rerun")
        }
        ControlReceipt::Finish => format!("{expected} ack=finish next=jobs_get no-rerun"),
        ControlReceipt::Cancel => format!("{expected} cancel=accepted next=jobs_get no-rerun"),
    };
    let mut compact = ok_text(receipt);
    compact.meta = cached.meta.clone();
    if budget.is_none_or(|limit| {
        crate::mcp::serialized_json_len(&compact).is_some_and(|size| size <= limit)
    }) {
        compact
    } else {
        let mut error = tool_error(format!(
            "{expected} Job Control effect already applied, but its replay receipt exceeds the response budget; inspect with jobs_get before retrying"
        ));
        error.meta = cached.meta;
        error
    }
}

/// A completed stdin write must never become an ambiguously truncated
/// success. The minimal receipt deliberately avoids claiming a new revision.
fn bounded_send_result(
    reference: &StandaloneRef,
    original: CallToolResult,
    budget: Option<usize>,
) -> CallToolResult {
    let fits = |result: &CallToolResult| {
        budget.is_none_or(|limit| {
            crate::mcp::serialized_json_len(result).is_some_and(|size| size <= limit)
        })
    };
    if fits(&original) {
        return original;
    }
    let reference = crate::jobs::JobManager::standalone_ref(&reference.id, reference.node);
    let compact = ok_text(format!("{reference} sent"));
    if fits(&compact) {
        compact
    } else {
        tool_error(format!(
            "{reference} stdin was already accepted, but its receipt exceeds the response budget; inspect with jobs_get, do not resend"
        ))
    }
}

fn bounded_send_replay(
    reference: &StandaloneRef,
    cached: CallToolResult,
    budget: Option<usize>,
) -> CallToolResult {
    if cached.is_error.unwrap_or(false) {
        return cached;
    }
    let expected = crate::jobs::JobManager::standalone_ref(&reference.id, reference.node);
    let Some(text) = cached
        .content
        .iter()
        .find_map(|block| block.as_text().map(|text| text.text.as_str()))
    else {
        return tool_error("Cached stdin receipt is unverified; inspect Job, do not resend");
    };
    let mut tokens = text.split_whitespace();
    if tokens.next() != Some(expected.as_str()) {
        return tool_error("Cached stdin Job identity differs; inspect Job, do not resend");
    }
    let next = tokens.next();
    let valid = match next {
        Some("sent") => tokens.next().is_none(),
        Some(revision) => {
            revision
                .strip_prefix("rev=")
                .and_then(|value| value.parse::<u64>().ok())
                .is_some()
                && tokens.next() == Some("ok")
                && tokens.next().is_none()
        }
        None => false,
    };
    if !valid {
        return tool_error("Cached stdin outcome is unverified; inspect Job, do not resend");
    }
    if budget.is_none_or(|limit| {
        crate::mcp::serialized_json_len(&cached).is_some_and(|size| size <= limit)
    }) {
        return cached;
    }
    let mut compact = ok_text(format!("{expected} sent"));
    compact.meta = cached.meta.clone();
    if budget.is_none_or(|limit| {
        crate::mcp::serialized_json_len(&compact).is_some_and(|size| size <= limit)
    }) {
        compact
    } else {
        let mut error = tool_error(format!(
            "{expected} stdin was already accepted, but its replay receipt exceeds the response budget; inspect with jobs_get, do not resend"
        ));
        error.meta = cached.meta;
        error
    }
}

fn complete_remote_start_result(
    machine: &LocalMachine,
    operation: aw_tools::OperationId,
    guard: Option<RemoteStartGuard>,
    mut result: CallToolResult,
) -> CallToolResult {
    let Some(guard) = guard else {
        return result;
    };
    if let Err(error) = machine.stamp_cached_mcp_receipt(operation, &mut result) {
        let fallback = tool_error(format!(
            "remote Start effect completed but its timestamp receipt could not be finalized: {error}; inspect the Job before retrying"
        ));
        return match guard.mark_ready(RemoteStartReceipt::Mcp(fallback.clone())) {
            Ok(()) => fallback,
            Err(cache_error) => tool_error(cache_error),
        };
    }
    match guard.mark_ready(RemoteStartReceipt::Mcp(result.clone())) {
        Ok(()) => result,
        Err(error) => tool_error(error),
    }
}

pub(crate) fn canonicalize_graph_router(
    router: &mut rmcp::handler::server::router::tool::ToolRouter<LocalMachine>,
    capabilities: crate::mcp::Capabilities,
    shell_tool: Option<aw_tools::filesystem::batch::NodeTool>,
) {
    let tools = graph::allowed_node_tools(capabilities, shell_tool);
    let schema = graph::input_schema(&tools)
        .as_object()
        .cloned()
        .expect("canonical Graph schema is an object");
    let schema = std::sync::Arc::new(schema);
    if let Some(route) = router.map.get_mut("batch") {
        route.attr.input_schema = schema;
    }
}

pub(crate) fn with_job_limits(tool: &mut Tool, max_output_bytes: usize) {
    let name = tool.name.as_ref();
    if name != "jobs_read" {
        return;
    }
    let mut schema = (*tool.input_schema).clone();
    let Some(properties) = schema
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    if name == "jobs_read"
        && let Some(property) = properties
            .get_mut("limit")
            .and_then(serde_json::Value::as_object_mut)
    {
        property.insert(
            "description".to_owned(),
            serde_json::Value::String(format!(
                "Maximum retained raw bytes for this page. Defaults to the profile output ceiling, may be as small as 1 byte and is capped at {max_output_bytes}."
            )),
        );
        property.insert("minimum".to_owned(), serde_json::Value::from(1));
        property.insert(
            "maximum".to_owned(),
            serde_json::Value::from(max_output_bytes),
        );
    }
    tool.input_schema = std::sync::Arc::new(schema);
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StandaloneRef {
    id: String,
    node: Option<usize>,
}

fn parse_ref(reference: &str) -> Result<StandaloneRef, String> {
    if !reference.starts_with("j:") {
        return Err("standalone Job ref must start with j:".into());
    }
    let resolved = ExecutionRef::parse(reference)
        .map_err(|_| "invalid standalone Job ref".to_owned())?
        .resolve(None, None)
        .map_err(|_| "invalid standalone Job ref".to_owned())?;
    match resolved.suffix() {
        [ExecutionSegment::Job(job)] => Ok(StandaloneRef {
            id: job.as_str().to_owned(),
            node: None,
        }),
        [ExecutionSegment::Job(job), ExecutionSegment::Node(node)] => Ok(StandaloneRef {
            id: job.as_str().to_owned(),
            node: Some(node.get() as usize),
        }),
        _ => Err("standalone Job ref supports only j:<handle>[/n:<coordinate>]".into()),
    }
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ReadView {
    Result,
    Stdout,
    Stderr,
}

impl From<ReadView> for JobOutputStream {
    fn from(value: ReadView) -> Self {
        match value {
            ReadView::Result => Self::Result,
            ReadView::Stdout => Self::Stdout,
            ReadView::Stderr => Self::Stderr,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum OutputEncoding {
    #[default]
    Text,
    Base64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobsListArgs {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobsGetArgs {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    rev: Option<u64>,
    #[serde(default)]
    wait_ms: Option<u64>,
    #[serde(default)]
    tail: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobsReadArgs {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    view: Option<ReadView>,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    encoding: OutputEncoding,
    #[serde(default)]
    wait_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobsAckArgs {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    view: Option<ReadView>,
    #[serde(default)]
    offset: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SendMode {
    #[default]
    Auto,
    Queue,
    Steer,
    Interrupt,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendInput {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    eof: Option<bool>,
}

impl SendInput {
    fn into_process(self) -> Result<ProcessInput, String> {
        match (self.text, self.eof) {
            (Some(text), None) if text.len() <= 64 * 1024 => {
                Ok(ProcessInput::Text(text.into_bytes()))
            }
            (None, Some(true)) => Ok(ProcessInput::Eof),
            _ => Err("jobs_send input must be exactly {text:string} or {eof:true}".into()),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobsSendArgs {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    rev: Option<u64>,
    #[serde(default)]
    mode: SendMode,
    input: SendInput,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobsCancelArgs {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    rev: Option<u64>,
}

#[tool_router(router = "jobs_router", vis = "pub(crate)")]
impl LocalMachine {
    async fn finish_graph_call(
        &self,
        id: &str,
        deadline: Option<tokio::time::Instant>,
        result_budget: Option<usize>,
    ) -> CallToolResult {
        match self.jobs.wait_until(id, deadline).await {
            Ok(JobWait::Pending) => match self.jobs.expose_compatibility(id) {
                Ok(_) => match self.jobs.standalone_status(id, None) {
                    Ok((_, status)) => bounded_job_handoff(id, status, result_budget),
                    Err(error) => tool_error(error),
                },
                Err(error) => tool_error(error),
            },
            Err(error) => tool_error(error),
            Ok(JobWait::Terminal) => {
                let render_budget = result_budget
                    .unwrap_or(self.config.max_output_bytes)
                    .min(self.config.max_output_bytes)
                    .max(1);
                match self.jobs.render_graph_result(id, render_budget) {
                    Ok(Some(text)) => {
                        let result = ok_text(text);
                        if result_budget.is_some_and(|limit| {
                            crate::mcp::serialized_json_len(&result).is_none_or(|size| size > limit)
                        }) {
                            let _ = self.jobs.expose_compatibility(id);
                            return match self.jobs.standalone_status(id, None) {
                                Ok((_, status)) => bounded_job_handoff(id, status, result_budget),
                                Err(error) => tool_error(error),
                            };
                        }
                        match self.jobs.mark_synchronous_result(id) {
                            Ok(_) => result,
                            Err(error) => tool_error(error),
                        }
                    }
                    Ok(None) => {
                        let _ = self.jobs.expose_compatibility(id);
                        match self.jobs.standalone_status(id, None) {
                            Ok((_, status)) => bounded_job_handoff(id, status, result_budget),
                            Err(error) => tool_error(error),
                        }
                    }
                    Err(error) => tool_error(error),
                }
            }
        }
    }

    async fn replay_graph_response(
        &self,
        id: &str,
        result_budget: Option<usize>,
    ) -> Option<CallToolResult> {
        match self.jobs.replay_handoff(id) {
            Ok(None) => None,
            Err(error) => Some(tool_error(error)),
            Ok(Some(JobHandoff::Compatibility(_))) => Some(
                self.jobs
                    .standalone_status(id, None)
                    .map(|(_, status)| bounded_job_handoff(id, status, result_budget))
                    .unwrap_or_else(tool_error),
            ),
            Ok(Some(JobHandoff::Synchronous)) => {
                let render_budget = result_budget
                    .unwrap_or(self.config.max_output_bytes)
                    .min(self.config.max_output_bytes)
                    .max(1);
                Some(match self.jobs.render_graph_result(id, render_budget) {
                    Ok(Some(text)) => ok_text(text),
                    Ok(None) => tool_error(
                        "committed synchronous Graph result no longer fits its response budget",
                    ),
                    Err(error) => tool_error(error),
                })
            }
            Ok(Some(JobHandoff::NativeTask(_))) => Some(tool_error(
                "Graph response was unexpectedly committed as a native Task",
            )),
        }
    }

    #[tool(
        description = "Execute a bounded canonical Graph of already-granted native filesystem and shell nodes. Set background=true to return the admitted durable Graph Job immediately; otherwise dotlink returns the exact compact result when it completes within this request and hands off the same Job if the synchronous window closes.",
        annotations(
            title = "Run tool Graph",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn batch(
        &self,
        Parameters(args): Parameters<graph::GraphArgs>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        // The authenticated transport may narrow the profile ceiling; even
        // a local stdio handoff must be bounded before the Job is exposed.
        let result_budget = Some(
            self.request_result_budget(&context)
                .unwrap_or(self.config.max_output_bytes)
                .min(self.config.max_output_bytes),
        );
        let remote_start = context
            .extensions
            .get::<crate::remote_start::ValidatedRemoteStart>()
            .cloned();
        let request_key = remote_start
            .as_ref()
            .map(crate::remote_start::ValidatedRemoteStart::replay_key)
            .or_else(|| self.request_job_key(&context));
        let background = args.background;
        let _display_title = args.title;
        if let Some(start) = remote_start {
            // Exact replay lookup precedes all mutable Graph preparation and
            // topology/policy revalidation. A duplicate successful Start must
            // return its original receipt even if the workspace has changed.
            let guard = match self.jobs.claim_remote_start(&start) {
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(result))) => {
                    return bounded_replayed_job_handoff(result, result_budget);
                }
                Ok(RemoteStartDecision::Unknown) => {
                    return tool_error(
                        "remote Start outcome is unknown; inspect the original attempt before any new invocation",
                    );
                }
                Ok(RemoteStartDecision::New(guard)) => guard,
                Err(error) => return tool_error(error),
            };
            let prepared = match graph::prepare(self, args.ops).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    guard.release_before_effect();
                    return tool_error(error);
                }
            };
            let started = match graph::start(self, prepared, None, request_key, true).await {
                Ok(started) => started,
                Err(error) => {
                    guard.release_before_effect();
                    return tool_error(error);
                }
            };
            let status = match self.jobs.standalone_status(&started.id, None) {
                Ok((_, status)) => status,
                Err(error) => {
                    guard.mark_unknown();
                    return tool_error(error);
                }
            };
            let result = bounded_job_handoff(&started.id, status, result_budget);
            return complete_remote_start_result(
                self,
                aw_tools::OperationId::Batch,
                Some(guard),
                result,
            );
        }

        let prepared = match graph::prepare(self, args.ops).await {
            Ok(prepared) => prepared,
            Err(error) => return tool_error(error),
        };
        let started = match graph::start(self, prepared, None, request_key, background).await {
            Ok(started) => started,
            Err(error) => return tool_error(error),
        };
        if !started.is_new
            && let Some(response) = self.replay_graph_response(&started.id, result_budget).await
        {
            return response;
        }
        if background {
            return match self.jobs.standalone_status(&started.id, None) {
                Ok((_, status)) => bounded_job_handoff(&started.id, status, result_budget),
                Err(error) => tool_error(error),
            };
        }
        let deadline = self
            .request_sync_budget_millis(&context)
            .and_then(|millis| {
                tokio::time::Instant::now().checked_add(std::time::Duration::from_millis(millis))
            });
        self.finish_graph_call(&started.id, deadline, result_budget)
            .await
    }

    #[tool(
        description = "List recent exposed standalone Process/Graph Jobs newest first. Paging uses a runtime-generation-bound opaque cursor; no Agent, Session or subtree state is listed.",
        annotations(
            title = "List Jobs",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn jobs_list(
        &self,
        Parameters(args): Parameters<JobsListArgs>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let limit = args.limit.unwrap_or(20);
        if !(1..=DOTLINK_JOB_PROFILE.list_page_max).contains(&limit) {
            return tool_error(format!(
                "jobs_list limit must be within 1..={}",
                DOTLINK_JOB_PROFILE.list_page_max
            ));
        }
        let ceiling = self
            .request_result_budget(&context)
            .unwrap_or(self.config.max_output_bytes)
            .min(self.config.max_output_bytes);
        match self
            .jobs
            .standalone_list_fitted(args.cursor.as_deref(), limit, |page| {
                let result = ok_text(page.to_owned());
                crate::mcp::serialized_json_len(&result).is_some_and(|size| size <= ceiling)
            }) {
            Ok(page) => ok_text(page),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Observe one standalone Process/Graph Job or Graph node. wait_ms waits only for this target to settle or for the wait deadline. Process tail peeks never ACK.",
        annotations(
            title = "Get Job status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn jobs_get(
        &self,
        Parameters(args): Parameters<JobsGetArgs>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let reference = match parse_ref(&args.reference) {
            Ok(reference) => reference,
            Err(error) => return tool_error(error),
        };
        let wait_ms = args.wait_ms.unwrap_or(0);
        if wait_ms > DOTLINK_JOB_PROFILE.get_wait_max_ms {
            return tool_error(format!(
                "jobs_get wait_ms exceeds {}",
                DOTLINK_JOB_PROFILE.get_wait_max_ms
            ));
        }
        if args
            .tail
            .is_some_and(|tail| tail > DOTLINK_JOB_PROFILE.get_tail_max)
        {
            return tool_error(format!(
                "jobs_get tail exceeds {} combined bytes",
                DOTLINK_JOB_PROFILE.get_tail_max
            ));
        }
        let wait_ms = bounded_wait_ms(wait_ms, self.request_sync_budget_millis(&context));
        let result_budget = self
            .request_result_budget(&context)
            .unwrap_or(self.config.max_output_bytes)
            .min(self.config.max_output_bytes);
        match self
            .jobs
            .standalone_get(&reference.id, reference.node, args.rev, wait_ms)
            .await
        {
            Ok(status) => match self.jobs.standalone_process_tail(
                &reference.id,
                reference.node,
                args.tail,
                status,
                result_budget,
            ) {
                Ok(status) => bounded_job_status(status, Some(result_budget)),
                Err(error) => tool_error(error),
            },
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Read one replayable retained standalone Job view by raw offset. Process and Process-backed Graph nodes default to stdout; native/Graph results default to result. Reading never acknowledges or releases bytes.",
        annotations(
            title = "Read Job output",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn jobs_read(
        &self,
        Parameters(args): Parameters<JobsReadArgs>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        if args.wait_ms.unwrap_or(0) > DOTLINK_JOB_PROFILE.read_wait_max_ms {
            return tool_error(format!(
                "jobs_read wait_ms exceeds {}",
                DOTLINK_JOB_PROFILE.read_wait_max_ms
            ));
        }
        let reference = match parse_ref(&args.reference) {
            Ok(reference) => reference,
            Err(error) => return tool_error(error),
        };
        let profile = crate::jobs::dotlink_job_profile(self.config.max_output_bytes);
        let ceiling = match args.limit {
            Some(limit) if limit > profile.read_page_max_bytes => {
                return tool_error(format!(
                    "jobs_read limit exceeds {}",
                    profile.read_page_max_bytes
                ));
            }
            Some(limit) => limit,
            None => profile.read_page_default_bytes,
        };
        // Raw page bytes and serialized response bytes are independent
        // budgets. In particular, a one-byte raw page still needs room for its
        // JSON/MCP envelope. Trusted transports may narrow the serialized
        // result; local/stdio callers fall back to the same profile-derived
        // response capacity used by the transport configuration.
        let response_ceiling = self
            .request_result_budget(&context)
            .unwrap_or_else(|| self.max_response_body_bytes());
        let offset = args.offset.unwrap_or(0);
        let base64 = matches!(args.encoding, OutputEncoding::Base64);
        let wait_ms = bounded_wait_ms(
            args.wait_ms.unwrap_or(0),
            self.request_sync_budget_millis(&context),
        );
        let is_graph = match self.jobs.standalone_is_graph(&reference.id) {
            Ok(value) => value,
            Err(error) => return tool_error(error),
        };
        if is_graph && reference.node.is_none() {
            if args
                .view
                .is_some_and(|view| !matches!(view, ReadView::Result))
            {
                return tool_error(
                    "Graph root exposes only result; use j:<handle>/n:<coordinate> for node streams",
                );
            }
            return match self
                .jobs
                .standalone_graph_result_page_wait(
                    &reference.id,
                    offset,
                    ceiling,
                    response_ceiling,
                    base64,
                    wait_ms,
                )
                .await
            {
                Ok(page) => ok_text(page),
                Err(error) => tool_error(error),
            };
        }
        match self
            .jobs
            .output_node_page_wait(
                &reference.id,
                crate::jobs::OutputPageRequest {
                    node: reference.node,
                    stream: args.view.map(Into::into),
                    offset,
                    requested_bytes: ceiling,
                    response_ceiling,
                    base64,
                },
                wait_ms,
            )
            .await
        {
            Ok(page) => ok_text(page),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Explicitly acknowledge one retained Job ref/view. With offset, ACK only that contiguous delivered raw-byte prefix. Omit offset to Finish this exact terminal Job/node scope, releasing unread bytes for only the current Job owner. ACK is never implied by jobs_read.",
        annotations(
            title = "Acknowledge Job output",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn jobs_ack(
        &self,
        Parameters(args): Parameters<JobsAckArgs>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let result_budget = Some(
            self.request_result_budget(&context)
                .unwrap_or(self.config.max_output_bytes)
                .min(self.config.max_output_bytes),
        );
        let reference = match parse_ref(&args.reference) {
            Ok(reference) => reference,
            Err(error) => return tool_error(error),
        };
        let effect = args
            .offset
            .map(ControlReceipt::AckThrough)
            .unwrap_or(ControlReceipt::Finish);
        let remote_start = context
            .extensions
            .get::<crate::remote_start::ValidatedRemoteStart>()
            .cloned();
        let guard = if let Some(start) = remote_start.as_ref() {
            match self.jobs.claim_remote_start(start) {
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(result))) => {
                    return bounded_control_replay(result, &reference, effect, result_budget);
                }
                Ok(RemoteStartDecision::Unknown) => {
                    return tool_error(
                        "remote Start outcome is unknown; do not repeat Job acknowledgement",
                    );
                }
                Ok(RemoteStartDecision::New(guard)) => Some(guard),
                Err(error) => return tool_error(error),
            }
        } else {
            None
        };
        let applied = match args.offset {
            Some(offset) => self.jobs.acknowledge_node_output(
                &reference.id,
                reference.node,
                args.view.map(Into::into),
                offset,
            ),
            None => self.jobs.finish_node_output(
                &reference.id,
                reference.node,
                args.view.map(Into::into),
            ),
        };
        let outcome = match applied {
            Ok(_) => self
                .jobs
                .standalone_status(&reference.id, reference.node)
                .map(|(_, status)| status)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        match outcome {
            Ok(text) => {
                let result = bounded_control_success(&reference, text, effect, result_budget);
                complete_remote_start_result(self, aw_tools::OperationId::JobsAck, guard, result)
            }
            Err(text) => {
                let result = tool_error(text);
                complete_remote_start_result(self, aw_tools::OperationId::JobsAck, guard, result)
            }
        }
    }

    #[tool(
        description = "Send typed live stdin to one explicit interactive Process Job/node. Process supports mode=auto only; input is exactly {text} or {eof:true}. Current launcher/sandbox/tool authority is rechecked before delivery.",
        annotations(
            title = "Send Job input",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn jobs_send(
        &self,
        Parameters(args): Parameters<JobsSendArgs>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let result_budget = Some(
            self.request_result_budget(&context)
                .unwrap_or(self.config.max_output_bytes)
                .min(self.config.max_output_bytes),
        );
        self.jobs_send_impl(
            args,
            context
                .extensions
                .get::<crate::remote_start::ValidatedRemoteStart>()
                .cloned(),
            result_budget,
        )
        .await
    }

    async fn jobs_send_impl(
        &self,
        args: JobsSendArgs,
        remote_start: Option<crate::remote_start::ValidatedRemoteStart>,
        result_budget: Option<usize>,
    ) -> CallToolResult {
        if !matches!(args.mode, SendMode::Auto) {
            return tool_error("standalone Process jobs_send supports only mode=auto");
        }
        let reference = match parse_ref(&args.reference) {
            Ok(reference) => reference,
            Err(error) => return tool_error(error),
        };
        let input = match args.input.into_process() {
            Ok(input) => input,
            Err(error) => return tool_error(error),
        };
        let guard = if let Some(start) = remote_start.as_ref() {
            match self.jobs.claim_remote_start(start) {
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(result))) => {
                    return bounded_send_replay(&reference, result, result_budget);
                }
                Ok(RemoteStartDecision::Unknown) => {
                    return tool_error(
                        "remote Start outcome is unknown; do not resend Process stdin",
                    );
                }
                Ok(RemoteStartDecision::New(guard)) => Some(guard),
                Err(error) => return tool_error(error),
            }
        } else {
            None
        };
        // Do not send irreversible stdin unless a complete confirmation can
        // fit. An existing replay was inspected above, before this preflight.
        let minimal = ok_text(format!(
            "{} sent",
            crate::jobs::JobManager::standalone_ref(&reference.id, reference.node)
        ));
        if result_budget.is_some_and(|limit| {
            crate::mcp::serialized_json_len(&minimal).is_none_or(|size| size > limit)
        }) {
            if let Some(guard) = guard {
                guard.release_before_effect();
            }
            return tool_error(
                "jobs_send response budget cannot fit a complete receipt; stdin was not sent",
            );
        }
        match self
            .send_process_input_checked(&reference.id, reference.node, args.rev, input)
            .await
        {
            Ok(revision) => {
                let full = ok_text(format!(
                    "{} rev={revision} ok",
                    crate::jobs::JobManager::standalone_ref(&reference.id, reference.node)
                ));
                let result = bounded_send_result(&reference, full, result_budget);
                complete_remote_start_result(self, aw_tools::OperationId::JobsSend, guard, result)
            }
            Err(error) => {
                let text = error.to_string();
                let result = tool_error(text);
                complete_remote_start_result(self, aw_tools::OperationId::JobsSend, guard, result)
            }
        }
    }

    async fn jobs_cancel_impl(
        &self,
        args: JobsCancelArgs,
        remote_start: Option<crate::remote_start::ValidatedRemoteStart>,
        result_budget: Option<usize>,
    ) -> CallToolResult {
        let reference = match parse_ref(&args.reference) {
            Ok(reference) => reference,
            Err(error) => return tool_error(error),
        };
        if reference.node.is_some() {
            return tool_error("standalone jobs_cancel targets a Job root, not a Graph node");
        }
        let guard = if let Some(start) = remote_start.as_ref() {
            match self.jobs.claim_remote_start(start) {
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(result))) => {
                    return bounded_control_replay(
                        result,
                        &reference,
                        ControlReceipt::Cancel,
                        result_budget,
                    );
                }
                Ok(RemoteStartDecision::Unknown) => {
                    return tool_error(
                        "remote Start outcome is unknown; do not repeat Job cancellation",
                    );
                }
                Ok(RemoteStartDecision::New(guard)) => Some(guard),
                Err(error) => return tool_error(error),
            }
        } else {
            None
        };
        // Replay lookup must precede current-state preconditions: a successful
        // first cancellation may itself advance the Job revision, so an exact
        // duplicate remote Start must return the original receipt rather than
        // being reinterpreted as a fresh stale-revision request.
        if let Some(expected) = args.rev {
            let (current, _) = match self.jobs.standalone_status(&reference.id, None) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let result = tool_error(error);
                    return complete_remote_start_result(
                        self,
                        aw_tools::OperationId::JobsCancel,
                        guard,
                        result,
                    );
                }
            };
            if expected != current {
                let result = tool_error(format!(
                    "stale Job revision: expected {expected}, current {current}"
                ));
                return complete_remote_start_result(
                    self,
                    aw_tools::OperationId::JobsCancel,
                    guard,
                    result,
                );
            }
        }
        let outcome = match self.jobs.cancel(&reference.id) {
            Ok(_) => self
                .jobs
                .standalone_status(&reference.id, None)
                .map(|(_, status)| status)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        match outcome {
            Ok(text) => {
                let result = bounded_control_success(
                    &reference,
                    text,
                    ControlReceipt::Cancel,
                    result_budget,
                );
                complete_remote_start_result(self, aw_tools::OperationId::JobsCancel, guard, result)
            }
            Err(text) => {
                let result = tool_error(text);
                complete_remote_start_result(self, aw_tools::OperationId::JobsCancel, guard, result)
            }
        }
    }

    #[tool(
        description = "Idempotently request cancellation of one explicit standalone Process/Graph Job. rev is an optional compare-and-act precondition; Graph-node cancellation is not exposed initially.",
        annotations(
            title = "Cancel Job",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn jobs_cancel(
        &self,
        Parameters(args): Parameters<JobsCancelArgs>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let remote_start = context
            .extensions
            .get::<crate::remote_start::ValidatedRemoteStart>()
            .cloned();
        let result_budget = Some(
            self.request_result_budget(&context)
                .unwrap_or(self.config.max_output_bytes)
                .min(self.config.max_output_bytes),
        );
        self.jobs_cancel_impl(args, remote_start, result_budget)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_toolset(max_output_bytes: usize) -> aw_tools::toolset::Toolset {
        crate::mcp::tools::shared_native::toolset(
            aw_tools::filesystem::schema::SchemaCaps::default(),
            max_output_bytes,
        )
    }

    fn project_job_tool(mut tool: Tool, max_output_bytes: usize) -> Tool {
        with_job_limits(&mut tool, max_output_bytes);
        crate::mcp::tools::shared_native::apply_operation_metadata(
            &mut tool,
            &test_toolset(max_output_bytes),
        );
        tool
    }

    #[test]
    fn standalone_refs_are_explicit_and_node_coordinates_are_canonical() {
        assert_eq!(
            parse_ref("j:abc-123").unwrap(),
            StandaloneRef {
                id: "abc-123".into(),
                node: None
            }
        );
        assert_eq!(
            parse_ref("j:abc-123/n:4").unwrap(),
            StandaloneRef {
                id: "abc-123".into(),
                node: Some(4)
            }
        );
        for invalid in ["", ".", "^", "abc", "j:", "j:x/n:0", "j:x/n:01", "j:x/x:1"] {
            assert!(parse_ref(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn jobs_list_schema_rejects_zero_and_caps_pages() {
        let list = project_job_tool(
            LocalMachine::all_tools().get("jobs_list").cloned().unwrap(),
            8 * 1024 * 1024,
        );
        let limit = &list.input_schema["properties"]["limit"];
        assert_eq!(limit["minimum"], 1);
        assert_eq!(limit["maximum"], DOTLINK_JOB_PROFILE.list_page_max);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finite_get_waits_for_process_settlement_even_with_stale_revision() {
        use aw_tools::filesystem::batch::NodeTool;

        let Some(bash) = std::env::var_os("AW_BASH").map(std::path::PathBuf::from) else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with_shell(
            tmp.path().canonicalize().unwrap(),
            bash,
        );
        let started = machine
            .start_shell_job(
                crate::mcp::tools::shell::ShellArgs {
                    command: "printf early; sleep 0.55; printf late".into(),
                    dir: ".".into(),
                    stdin: None,
                    timeout_secs: Some(5),
                    max_output_bytes: Some(1024),
                    writes: None,
                },
                NodeTool::Bash,
                None,
                false,
            )
            .unwrap();
        machine.jobs.expose_compatibility(&started.id).unwrap();
        let (rev, _) = machine.jobs.standalone_status(&started.id, None).unwrap();

        // Older implementation treats this stale revision as permission to
        // return immediately. New contract: settle-or-deadline, regardless of
        // revision changes, early stdout or shell launch progress.
        let pending = machine
            .jobs
            .standalone_get(&started.id, None, Some(rev.saturating_sub(1)), 40)
            .await
            .unwrap();
        assert!(
            pending.contains(" running ") || pending.contains(" waiting "),
            "{pending}"
        );

        let terminal = machine
            .jobs
            .standalone_get(&started.id, None, Some(rev.saturating_sub(1)), 1500)
            .await
            .unwrap();
        assert!(terminal.contains(" settled "), "{terminal}");
        assert_eq!(
            machine
                .jobs
                .standalone_get(&started.id, None, None, 1500,)
                .await
                .unwrap(),
            terminal,
            "terminal get must not wait again"
        );
        machine.jobs.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finite_get_node_settles_before_unrelated_graph_sibling() {
        let Some(bash) = std::env::var_os("AW_BASH").map(std::path::PathBuf::from) else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with_shell(
            tmp.path().canonicalize().unwrap(),
            bash,
        );
        let args = serde_json::json!({"ops":[
            ["bash", {"command":"printf first"}, "*"],
            ["bash", {"command":"sleep 0.65; printf second"}, "*"]
        ]});
        let prepared = graph::prepare(&machine, args["ops"].as_array().unwrap().clone())
            .await
            .unwrap();
        let started = graph::start(&machine, prepared, None, None, true)
            .await
            .unwrap();
        let (rev, _) = machine.jobs.standalone_status(&started.id, None).unwrap();

        let first = machine
            .jobs
            .standalone_get(&started.id, Some(1), Some(rev), 1500)
            .await
            .unwrap();
        assert!(first.contains(" settled "), "{first}");
        let first_tail = machine
            .jobs
            .standalone_process_tail(&started.id, Some(1), None, first.clone(), 4096)
            .unwrap();
        assert!(first_tail.contains("stdout [0..5] eof"), "{first_tail}");
        assert!(first_tail.contains("first"), "{first_tail}");
        assert!(
            machine
                .jobs
                .acknowledge_node_output(
                    &started.id,
                    Some(1),
                    Some(crate::jobs::JobOutputStream::Stdout),
                    5,
                )
                .is_err(),
            "jobs_get tail must not create node delivery credit"
        );
        let root = machine
            .jobs
            .standalone_get(&started.id, None, Some(rev), 30)
            .await
            .unwrap();
        assert!(
            root.contains(" running ") || root.contains(" waiting "),
            "root must not be settled while sibling remains active: {root}"
        );
        assert_eq!(
            machine
                .jobs
                .standalone_process_tail(&started.id, None, None, root.clone(), 4096,)
                .unwrap(),
            root,
            "Graph roots do not synthesize Process tails"
        );
        assert!(
            machine
                .jobs
                .standalone_process_tail(&started.id, None, Some(1), root.clone(), 4096,)
                .is_err(),
            "explicit Graph-root tail must fail rather than guess a node"
        );
        let settled = machine
            .jobs
            .standalone_get(&started.id, None, Some(rev), 1500)
            .await
            .unwrap();
        assert!(settled.contains(" settled "), "{settled}");
        assert!(
            machine
                .jobs
                .standalone_get(&started.id, Some(17), None, 1500,)
                .await
                .is_err(),
            "invalid node must fail immediately"
        );
        machine.jobs.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skipped_graph_node_read_does_not_wait_for_unrelated_sibling() {
        let Some(bash) = std::env::var_os("AW_BASH").map(std::path::PathBuf::from) else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with_shell(root, bash);
        let prepared = graph::prepare(
            &machine,
            vec![
                serde_json::json!(["bash", {"command":"false"}, "*"]),
                serde_json::json!(["read", {"path":"a.txt","numbered":false}, "1.ok"]),
                serde_json::json!(["bash", {"command":"sleep 1.2; printf sibling"}, "*"]),
            ],
        )
        .await
        .unwrap();
        let started = graph::start(&machine, prepared, None, None, true)
            .await
            .unwrap();
        let skipped = machine
            .jobs
            .standalone_get(&started.id, Some(2), None, 1000)
            .await
            .unwrap();
        assert!(
            skipped.contains("settled") && skipped.contains("skip"),
            "{skipped}"
        );
        let (_, root) = machine.jobs.standalone_status(&started.id, None).unwrap();
        assert!(root.contains("running"), "{root}");

        // A skipped node has no result stream. Return the truthful missing
        // output immediately instead of consuming a slow sibling's lifetime.
        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            machine.jobs.output_node_page_wait(
                &started.id,
                crate::jobs::OutputPageRequest {
                    node: Some(2),
                    stream: Some(crate::jobs::JobOutputStream::Result),
                    offset: 0,
                    requested_bytes: 64,
                    response_ceiling: 4096,
                    base64: false,
                },
                1200,
            ),
        )
        .await
        .expect("skipped node must not wait for sibling");
        let error = outcome.expect_err("skipped node cannot fabricate a result page");
        assert!(
            error
                .to_string()
                .contains("has no retained output after settlement"),
            "{error}"
        );
        let (_, root) = machine.jobs.standalone_status(&started.id, None).unwrap();
        assert!(root.contains("running"), "{root}");
        machine.jobs.wait_until(&started.id, None).await.unwrap();
        machine.jobs.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finite_read_waits_for_new_bytes_and_eof_not_shell_revision() {
        use aw_tools::filesystem::batch::NodeTool;

        let Some(bash) = std::env::var_os("AW_BASH").map(std::path::PathBuf::from) else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with_shell(
            temp.path().canonicalize().unwrap(),
            bash,
        );
        let started = machine
            .start_shell_job(
                crate::mcp::tools::shell::ShellArgs {
                    command: "sleep 0.18; printf first; sleep 0.65; printf second".into(),
                    dir: ".".into(),
                    stdin: None,
                    timeout_secs: Some(5),
                    max_output_bytes: Some(1024),
                    writes: None,
                },
                NodeTool::Bash,
                None,
                false,
            )
            .unwrap();
        machine.jobs.expose_compatibility(&started.id).unwrap();
        let request = |offset: u64| crate::jobs::OutputPageRequest {
            node: None,
            stream: Some(crate::jobs::JobOutputStream::Stdout),
            offset,
            requested_bytes: 128,
            response_ceiling: 4096,
            base64: false,
        };
        let page = machine
            .jobs
            .output_node_page_wait(&started.id, request(0), 1500)
            .await
            .unwrap();
        let first: serde_json::Value = serde_json::from_str(&page).unwrap();
        assert_eq!(first["data"], "first", "{first}");
        assert_eq!(first["next_offset"], 5);
        assert_eq!(first["complete"], false, "first bytes precede process EOF");

        // A status revision can change (launch/other progress) without
        // making bytes at offset 5 available. The short deadline returns
        // an empty running page; it does not consume the Process timeout.
        let short = machine
            .jobs
            .output_node_page_wait(&started.id, request(5), 35)
            .await
            .unwrap();
        let short: serde_json::Value = serde_json::from_str(&short).unwrap();
        assert_eq!(short["data"], "", "{short}");
        assert_eq!(short["next_offset"], 5);
        assert_eq!(short["complete"], false);

        let page = machine
            .jobs
            .output_node_page_wait(&started.id, request(5), 1500)
            .await
            .unwrap();
        let second: serde_json::Value = serde_json::from_str(&page).unwrap();
        assert_eq!(second["data"], "second", "{second}");
        assert_eq!(second["next_offset"], 11);

        let eof = machine
            .jobs
            .output_node_page_wait(&started.id, request(11), 1500)
            .await
            .unwrap();
        let eof: serde_json::Value = serde_json::from_str(&eof).unwrap();
        assert_eq!(eof["data"], "", "{eof}");
        assert_eq!(eof["next_offset"], 11);
        assert_eq!(
            eof["complete"], true,
            "writer EOF is the stream completion gate"
        );
        // The status projection retains the shared owner ACK watermark.
        let status: serde_json::Value =
            serde_json::from_str(&machine.jobs.status(&started.id).unwrap()).unwrap();
        assert_eq!(status["stdout"]["acknowledged_bytes"], 0);
        assert_eq!(status["stdout"]["released"], false);
        machine.jobs.shutdown().await.unwrap();
    }

    #[test]
    fn status_budget_never_returns_truncated_success_and_handoff_keeps_ref() {
        let full = "j:example node settled rev=12 ".to_owned() + &"status-output".repeat(800);
        let success = ok_text(full.clone());
        let exact = crate::mcp::serialized_json_len(&success).unwrap();
        let accepted = bounded_job_status(full.clone(), Some(exact));
        assert!(!accepted.is_error.unwrap_or(false));
        assert_eq!(accepted.content[0].as_text().unwrap().text, full);

        let oversized = bounded_job_status(full.clone(), Some(512));
        assert!(oversized.is_error.unwrap_or(false));
        let message = &oversized.content[0].as_text().unwrap().text;
        assert!(message.contains("exceeds the response budget"));
        assert!(!message.contains("status-output"));

        let id = "20261116-abc";
        let concise = format!("j:{id} next=jobs_get no-rerun");
        let concise_size = crate::mcp::serialized_json_len(&ok_text(concise.clone())).unwrap();
        let admitted = bounded_job_handoff(id, full.clone(), Some(concise_size));
        assert!(!admitted.is_error.unwrap_or(false));
        assert_eq!(admitted.content[0].as_text().unwrap().text, concise);
        assert!(crate::mcp::serialized_json_len(&admitted).unwrap() <= concise_size);
        let impossible = bounded_job_handoff(id, full, Some(concise_size - 1));
        assert!(
            impossible.is_error.unwrap_or(false),
            "cannot fake a partial handoff"
        );
    }

    #[tokio::test]
    async fn large_graph_result_handoff_keeps_exact_ref_under_small_budget() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("large.txt"), "long-line ".repeat(2048)).unwrap();
        let machine = crate::mcp::tools::test_support::test_machine(root.clone());
        let ops = vec![serde_json::json!([
            "read", {"path":"large.txt","numbered":false}
        ])];
        let prepared = graph::prepare(&machine, ops).await.unwrap();
        let started = graph::start(&machine, prepared, None, None, true)
            .await
            .unwrap();
        machine.jobs.wait_until(&started.id, None).await.unwrap();
        let concise = format!("j:{} next=jobs_get no-rerun", started.id);
        let budget = crate::mcp::serialized_json_len(&ok_text(concise.clone())).unwrap() + 1;
        let result = machine
            .finish_graph_call(&started.id, None, Some(budget))
            .await;
        assert!(!result.is_error.unwrap_or(false), "{result:?}");
        assert_eq!(result.content[0].as_text().unwrap().text, concise);
        assert!(crate::mcp::serialized_json_len(&result).unwrap() <= budget);
        let repeated = machine
            .replay_graph_response(&started.id, Some(budget))
            .await
            .unwrap();
        assert!(!repeated.is_error.unwrap_or(false));
        assert_eq!(repeated.content[0].as_text().unwrap().text, concise);
        assert_eq!(
            machine
                .jobs
                .standalone_status(&started.id, None)
                .unwrap()
                .1
                .split_whitespace()
                .next(),
            Some(format!("j:{}", started.id).as_str()),
        );
        machine.jobs.shutdown().await.unwrap();
    }

    #[test]
    fn control_receipts_keep_action_and_exact_ref_under_tight_budgets() {
        let root = parse_ref("j:control-123").unwrap();
        let node = parse_ref("j:control-123/n:2").unwrap();
        let large = format!(
            "j:control-123 process settled rev=42 {}",
            "detail ".repeat(800)
        );
        let full = ok_text(large.clone());
        let exact = crate::mcp::serialized_json_len(&full).unwrap();

        let accepted =
            bounded_control_success(&root, large.clone(), ControlReceipt::Cancel, Some(exact));
        assert_eq!(
            accepted, full,
            "an already-fitting status must not be modified"
        );

        let ack_compact = "j:control-123/n:2 ack=through:47 next=jobs_get no-rerun";
        let ack_size = crate::mcp::serialized_json_len(&ok_text(ack_compact)).unwrap();
        let acknowledged = bounded_control_success(
            &node,
            large.clone(),
            ControlReceipt::AckThrough(47),
            Some(ack_size),
        );
        assert!(!acknowledged.is_error.unwrap_or(false));
        assert_eq!(acknowledged.content[0].as_text().unwrap().text, ack_compact);
        assert!(crate::mcp::serialized_json_len(&acknowledged).unwrap() <= ack_size);
        let finish_compact = "j:control-123/n:2 ack=finish next=jobs_get no-rerun";
        let finish_size = crate::mcp::serialized_json_len(&ok_text(finish_compact)).unwrap();
        let finished = bounded_control_success(
            &node,
            large.clone(),
            ControlReceipt::Finish,
            Some(finish_size),
        );
        assert!(!finished.is_error.unwrap_or(false));
        assert_eq!(finished.content[0].as_text().unwrap().text, finish_compact);
        let too_small = bounded_control_success(
            &node,
            large.clone(),
            ControlReceipt::AckThrough(47),
            Some(ack_size - 1),
        );
        assert!(too_small.is_error.unwrap_or(false));
        let detail = &too_small.content[0].as_text().unwrap().text;
        assert!(detail.contains("effect already applied") && detail.contains("before retrying"));

        let cancel_compact = "j:control-123 cancel=accepted next=jobs_get no-rerun";
        let cancel_size = crate::mcp::serialized_json_len(&ok_text(cancel_compact)).unwrap();
        let cancelled = bounded_control_success(
            &root,
            large.clone(),
            ControlReceipt::Cancel,
            Some(cancel_size),
        );
        assert_eq!(cancelled.content[0].as_text().unwrap().text, cancel_compact);
        assert!(!cancelled.is_error.unwrap_or(false));

        let replayed = bounded_control_replay(
            full.clone(),
            &root,
            ControlReceipt::Cancel,
            Some(cancel_size),
        );
        assert_eq!(replayed, cancelled);
        assert_eq!(
            full.content[0].as_text().unwrap().text,
            large,
            "cached receipt is immutable"
        );

        let failed = tool_error("stale Job revision");
        assert_eq!(
            bounded_control_replay(failed.clone(), &root, ControlReceipt::Cancel, Some(1)),
            failed,
            "a failed effect replay cannot be turned into Control success"
        );
        let mismatch = ok_text("j:other-456 process settled rev=42");
        assert!(
            bounded_control_replay(mismatch, &root, ControlReceipt::Cancel, Some(cancel_size))
                .is_error
                .unwrap_or(false),
            "wrong cached Job identity must fail closed"
        );
    }

    #[test]
    fn send_receipts_are_exact_bounded_and_replay_preserves_semantics() {
        let reference = parse_ref("j:send-123/n:2").unwrap();
        let name = "j:send-123/n:2";
        let full = ok_text(format!("{name} rev={} ok", u64::MAX));
        let full_size = crate::mcp::serialized_json_len(&full).unwrap();
        let compact = ok_text(format!("{name} sent"));
        let compact_size = crate::mcp::serialized_json_len(&compact).unwrap();
        assert!(compact_size < full_size);

        assert_eq!(
            bounded_send_result(&reference, full.clone(), Some(full_size)),
            full
        );
        let narrowed = bounded_send_result(&reference, full.clone(), Some(compact_size));
        assert_eq!(narrowed, compact);
        assert!(crate::mcp::serialized_json_len(&narrowed).unwrap() <= compact_size);
        let too_small = bounded_send_result(&reference, full.clone(), Some(compact_size - 1));
        assert!(too_small.is_error.unwrap_or(false));
        let detail = &too_small.content[0].as_text().unwrap().text;
        assert!(detail.contains("already accepted") && detail.contains("do not resend"));

        assert_eq!(
            bounded_send_replay(&reference, full.clone(), Some(compact_size)),
            compact,
            "narrow replay may drop a revision but must preserve the exact send identity"
        );
        assert_eq!(
            bounded_send_replay(&reference, compact.clone(), Some(full_size)),
            compact,
            "a replay cannot invent a revision omitted from the cached receipt"
        );
        assert_eq!(
            full.content[0].as_text().unwrap().text,
            format!("{name} rev={} ok", u64::MAX)
        );
        let denied = tool_error("Process authority revoked; stdin outcome unknown");
        assert_eq!(
            bounded_send_replay(&reference, denied.clone(), Some(1)),
            denied,
            "cached error can never become a success"
        );
        for invalid in [
            "j:different/n:2 rev=1 ok",
            "j:send-123/n:2 rev=1 maybe",
            "j:send-123/n:2 rev=bad ok",
            "j:send-123/n:2 rev=1 ok extra",
            "j:send-123/n:2 ack=all",
            "j:send-123/n:2 sent extra",
        ] {
            assert!(
                bounded_send_replay(&reference, ok_text(invalid), Some(compact_size))
                    .is_error
                    .unwrap_or(false),
                "malformed or unrelated cached receipt must be denied: {invalid}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interactive_send_preflights_receipt_and_delivers_once() {
        use aw_tools::filesystem::batch::NodeTool;
        let Some(bash) = std::env::var_os("AW_BASH").map(std::path::PathBuf::from) else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with_shell(root.clone(), bash);
        let started = machine
            .start_shell_job(
                crate::mcp::tools::shell::ShellArgs {
                    command: "IFS= read -r line; printf '%s' \"$line\" > received.txt".into(),
                    dir: ".".into(),
                    stdin: None,
                    timeout_secs: Some(4),
                    max_output_bytes: Some(1024),
                    writes: None,
                },
                NodeTool::Bash,
                None,
                true,
            )
            .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while !machine.jobs.process_input_ready_for_test(&started.id, None) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "stdin custody not ready"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let reference = format!("j:{}", started.id);
        let minimal = ok_text(format!("{reference} sent"));
        let minimal_size = crate::mcp::serialized_json_len(&minimal).unwrap();
        let args = || {
            serde_json::from_value::<JobsSendArgs>(serde_json::json!({
                "ref":reference, "input":{"text":"hello\n"}
            }))
            .unwrap()
        };

        let denied = machine
            .jobs_send_impl(args(), None, Some(minimal_size - 1))
            .await;
        assert!(denied.is_error.unwrap_or(false));
        assert!(
            denied.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("stdin was not sent")
        );
        assert!(machine.jobs.process_input_ready_for_test(&started.id, None));
        assert!(
            !root.join("received.txt").exists(),
            "insufficient receipt budget must not send"
        );
        let sent = machine
            .jobs_send_impl(args(), None, Some(minimal_size))
            .await;
        assert_eq!(sent, minimal);
        machine.jobs.wait_until(&started.id, None).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("received.txt")).unwrap(),
            "hello"
        );
        let resend = machine.jobs_send_impl(args(), None, None).await;
        assert!(
            resend.is_error.unwrap_or(false),
            "settled Process cannot accept more input"
        );
        machine.jobs.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn remote_start_duplicate_graph_replays_receipt_without_second_effect() {
        use aw_tools::remote_start::{
            REMOTE_START_VERSION, RemoteRuntimeLease, RemoteSessionAction,
            RemoteStartAuthorization, RemoteStartIntent, RemoteStartProvenance, RemoteStartSource,
        };

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let machine = crate::mcp::tools::test_support::test_machine(root.clone());
        let raw = serde_json::json!({
            "ops": [["mv", {"from":"a.txt", "to":"b.txt"}]]
        });
        let args: graph::GraphArgs = serde_json::from_value(raw.clone()).unwrap();
        let generation = machine.jobs.runtime_generation_fingerprint();
        let now = crate::remote_start::now_unix_ms().unwrap();
        let authorization = RemoteStartAuthorization::new(
            RemoteStartIntent {
                version: REMOTE_START_VERSION,
                original_invocation_id: "turn:1/call:graph".into(),
                turn_id: 1,
                call_id: "graph".into(),
                effect_attempt_id: "graph".into(),
                operation: aw_tools::OperationId::Batch.semantic_name().into(),
                canonical_ref: "/d:d/i:i/p:p/a:0/t:1".into(),
                arguments_sha256: crate::remote_start::arguments_sha256(&raw),
                original_request_sha256: [3; 32],
                session_action: RemoteSessionAction::Submit,
                target: RemoteRuntimeLease {
                    target_host: "l5".into(),
                    target_profile: "default".into(),
                    generation,
                    issued_at_unix_ms: now,
                    expires_at_unix_ms: now + 5_000,
                },
            },
            RemoteStartProvenance {
                actor: "actor".into(),
                source: RemoteStartSource {
                    source_kind: "host".into(),
                    source_id: "batch".into(),
                    source_epoch: 1,
                    site: "dotlink:l5".into(),
                    resource_scope: "root".into(),
                    grant_sha256: [4; 32],
                },
                policy_revision_sha256: [5; 32],
                policy_admission_id: "policy".into(),
                session_id: "session".into(),
                branch_id: "branch".into(),
                writer_epoch: 1,
                membership_epoch: 1,
                audience_epoch: 1,
            },
        )
        .unwrap();
        let target = crate::remote_start::BridgeTarget::new("l5", "default").unwrap();
        let start = crate::remote_start::validate_authorization(
            authorization,
            crate::remote_start::ValidationContext {
                client_id: "aw-client",
                target: &target,
                generation,
                now_unix_ms: now,
                operation: aw_tools::OperationId::Batch,
                class: crate::remote_start::StartClass::Submit,
                arguments: &raw,
                control_ref: None,
            },
        )
        .unwrap();

        let prepared = graph::prepare(&machine, args.ops).await.unwrap();
        let guard = match machine.jobs.claim_remote_start(&start).unwrap() {
            RemoteStartDecision::New(guard) => guard,
            _ => panic!("first Graph Start claim must be new"),
        };
        let started = graph::start(&machine, prepared, None, Some(start.replay_key()), true)
            .await
            .unwrap();
        let (_, status) = machine.jobs.standalone_status(&started.id, None).unwrap();
        let first = complete_remote_start_result(
            &machine,
            aw_tools::OperationId::Batch,
            Some(guard),
            ok_text(status),
        );
        let first_ts =
            crate::mcp::stored_receipt_ts(&first).expect("cached Batch receipt timestamp");
        machine.jobs.wait_until(&started.id, None).await.unwrap();
        assert!(!root.join("a.txt").exists());
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "a");

        let replay = match machine.jobs.claim_remote_start(&start).unwrap() {
            RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(result)) => result,
            _ => panic!("duplicate Graph Start must replay the first receipt"),
        };
        assert_eq!(replay, first);
        assert_eq!(crate::mcp::stored_receipt_ts(&replay), Some(first_ts));
        // A replay under a smaller envelope keeps the original ledger result
        // and returns a complete ref instead of a clipped success receipt.
        let concise = format!("j:{} next=jobs_get no-rerun", started.id);
        let mut concise_result = ok_text(concise.clone());
        concise_result.meta = replay.meta.clone();
        let small_budget = crate::mcp::serialized_json_len(&concise_result).unwrap();
        // This particular one-node receipt may already fit that smaller
        // budget; verify its exact replay before exercising the forced-large
        // projection case.
        let projected = bounded_replayed_job_handoff(replay.clone(), Some(small_budget));
        let fits = crate::mcp::serialized_json_len(&replay).unwrap() <= small_budget;
        assert_eq!(
            projected,
            if fits {
                replay.clone()
            } else {
                concise_result.clone()
            }
        );
        let mut long_cached = ok_text(format!(
            "j:{} graph waiting rev=1 {}",
            started.id,
            "node waiting ".repeat(100)
        ));
        long_cached.meta = replay.meta.clone();
        let projected = bounded_replayed_job_handoff(long_cached, Some(small_budget));
        assert!(!projected.is_error.unwrap_or(false));
        assert_eq!(projected.content[0].as_text().unwrap().text, concise);
        assert_eq!(crate::mcp::stored_receipt_ts(&projected), Some(first_ts));
        assert!(crate::mcp::serialized_json_len(&projected).unwrap() <= small_budget);
        let original = match machine.jobs.claim_remote_start(&start).unwrap() {
            RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(result)) => result,
            _ => panic!("duplicate Start must preserve the cached original receipt"),
        };
        assert_eq!(original, first);
        assert!(!root.join("a.txt").exists());
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "a");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remote_cancel_replay_precedes_revision_recheck() {
        use aw_tools::{
            filesystem::batch::NodeTool,
            remote_start::{
                REMOTE_START_VERSION, RemoteRuntimeLease, RemoteSessionAction,
                RemoteStartAuthorization, RemoteStartIntent, RemoteStartProvenance,
                RemoteStartSource,
            },
        };

        let Some(bash) = std::env::var_os("AW_BASH").map(std::path::PathBuf::from) else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with_shell(root, bash);
        let started = machine
            .start_shell_job(
                crate::mcp::tools::shell::ShellArgs {
                    command: "sleep 30".into(),
                    dir: ".".into(),
                    stdin: None,
                    timeout_secs: Some(30),
                    max_output_bytes: Some(1024),
                    writes: None,
                },
                NodeTool::Bash,
                None,
                false,
            )
            .unwrap();
        machine.jobs.expose_compatibility(&started.id).unwrap();
        let (initial_rev, _) = machine.jobs.standalone_status(&started.id, None).unwrap();
        let reference = format!("j:{}", started.id);
        let raw = serde_json::json!({"ref":reference,"rev":initial_rev});
        let generation = machine.jobs.runtime_generation_fingerprint();
        let now = crate::remote_start::now_unix_ms().unwrap();
        let authorization = RemoteStartAuthorization::new(
            RemoteStartIntent {
                version: REMOTE_START_VERSION,
                original_invocation_id: "turn:1/call:cancel".into(),
                turn_id: 1,
                call_id: "cancel".into(),
                effect_attempt_id: "cancel".into(),
                operation: aw_tools::OperationId::JobsCancel.semantic_name().into(),
                canonical_ref: format!("/d:d/i:i/p:p/a:0/j:{}", started.id),
                arguments_sha256: crate::remote_start::arguments_sha256(&raw),
                original_request_sha256: [3; 32],
                session_action: RemoteSessionAction::Control,
                target: RemoteRuntimeLease {
                    target_host: "l5".into(),
                    target_profile: "default".into(),
                    generation,
                    issued_at_unix_ms: now,
                    expires_at_unix_ms: now + 5_000,
                },
            },
            RemoteStartProvenance {
                actor: "actor".into(),
                source: RemoteStartSource {
                    source_kind: "host".into(),
                    source_id: "jobs.cancel".into(),
                    source_epoch: 1,
                    site: "dotlink:l5".into(),
                    resource_scope: "root".into(),
                    grant_sha256: [4; 32],
                },
                policy_revision_sha256: [5; 32],
                policy_admission_id: "policy".into(),
                session_id: "session".into(),
                branch_id: "branch".into(),
                writer_epoch: 1,
                membership_epoch: 1,
                audience_epoch: 1,
            },
        )
        .unwrap();
        let target = crate::remote_start::BridgeTarget::new("l5", "default").unwrap();
        let start = crate::remote_start::validate_authorization(
            authorization,
            crate::remote_start::ValidationContext {
                client_id: "aw-client",
                target: &target,
                generation,
                now_unix_ms: now,
                operation: aw_tools::OperationId::JobsCancel,
                class: crate::remote_start::StartClass::Control,
                arguments: &raw,
                control_ref: Some(&reference),
            },
        )
        .unwrap();

        let first = machine
            .jobs_cancel_impl(
                JobsCancelArgs {
                    reference: reference.clone(),
                    rev: Some(initial_rev),
                },
                Some(start.clone()),
                None,
            )
            .await;
        assert_ne!(first.is_error, Some(true));
        let (after_rev, _) = machine.jobs.standalone_status(&started.id, None).unwrap();
        assert_ne!(
            after_rev, initial_rev,
            "first cancellation must advance revision so replay exercises stale-current-state ordering"
        );

        let replay = machine
            .jobs_cancel_impl(
                JobsCancelArgs {
                    reference,
                    rev: Some(initial_rev),
                },
                Some(start.clone()),
                None,
            )
            .await;
        assert_eq!(
            replay, first,
            "duplicate remote Start must replay the original cancel receipt"
        );
        let first_ts =
            crate::mcp::stored_receipt_ts(&first).expect("cached cancel receipt timestamp");
        let mut minimal = ok_text(format!(
            "j:{} cancel=accepted next=jobs_get no-rerun",
            started.id
        ));
        minimal.meta = first.meta.clone();
        minimal
            .content
            .push(rmcp::model::ContentBlock::text(format!("ts={first_ts}")));
        let budget = crate::mcp::serialized_json_len(&minimal).unwrap();
        let narrow = machine
            .jobs_cancel_impl(
                JobsCancelArgs {
                    reference: format!("j:{}", started.id),
                    rev: Some(initial_rev),
                },
                Some(start.clone()),
                Some(budget),
            )
            .await;
        assert!(!narrow.is_error.unwrap_or(false), "{narrow:?}");
        assert!(crate::mcp::serialized_json_len(&narrow).unwrap() <= budget);
        let still_original = machine
            .jobs_cancel_impl(
                JobsCancelArgs {
                    reference: format!("j:{}", started.id),
                    rev: Some(initial_rev),
                },
                Some(start),
                None,
            )
            .await;
        assert_eq!(
            still_original, first,
            "narrow replay must not replace the cached Control result"
        );
        machine.jobs.wait_until(&started.id, None).await.unwrap();
    }

    #[test]
    fn standalone_constructors_expose_background_without_wake_or_legacy_start() {
        let router = LocalMachine::all_tools();
        let batch = router.get("batch").expect("batch");
        let batch_props = batch.input_schema["properties"].as_object().unwrap();
        assert!(batch_props.contains_key("background"));
        assert!(!batch_props.contains_key("wake"));
        assert!(router.get("job_start").is_none());
        assert!(router.get("job_restart").is_none());
        assert!(router.get("jobs_subscribe").is_none());
        assert!(router.get("agent").is_none());

        if let Some(shell) = router.get(crate::mcp::shell_tool_name()) {
            let shell_props = shell.input_schema["properties"].as_object().unwrap();
            assert!(shell_props.contains_key("background"));
            assert!(!shell_props.contains_key("wake"));
        }
    }

    #[test]
    fn jobs_read_schema_uses_profile_result_ceiling_and_has_no_filter() {
        let read = project_job_tool(
            LocalMachine::all_tools().get("jobs_read").cloned().unwrap(),
            8 * 1024 * 1024,
        );
        let output_limit = &read.input_schema["properties"]["limit"];
        assert_eq!(output_limit["minimum"], 1);
        assert_eq!(output_limit["maximum"], 8 * 1024 * 1024);
        assert_eq!(output_limit["default"], 256 * 1024);
        assert!(
            !read.input_schema["properties"]
                .as_object()
                .unwrap()
                .contains_key("filter")
        );
    }

    #[test]
    fn jobs_get_schema_advertises_combined_tail_bounds() {
        let get = project_job_tool(
            LocalMachine::all_tools().get("jobs_get").cloned().unwrap(),
            8 * 1024 * 1024,
        );
        let tail = &get.input_schema["properties"]["tail"];
        assert_eq!(tail["minimum"], 0);
        assert_eq!(tail["maximum"], 1024);
        assert!(
            !get.input_schema["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|value| value == "tail")),
            "omitted tail must select the 512-byte Process default"
        );
    }

    #[test]
    fn jobs_ack_schema_makes_offset_optional_for_terminal_finish() {
        let ack = LocalMachine::all_tools().get("jobs_ack").cloned().unwrap();
        let required = ack.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|value| value == "ref"));
        assert!(
            !required.iter().any(|value| value == "offset"),
            "omitting offset is the explicit terminal Finish form"
        );
    }
}
