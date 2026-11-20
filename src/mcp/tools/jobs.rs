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

use aw_tools::{
    budget::SerializedResultBytes,
    execution_ref::JobRef,
    receipt::{AckMode, Receipt, render_plain},
};

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
fn bounded_job_status(
    machine: &LocalMachine,
    status: String,
    budget: Option<usize>,
) -> CallToolResult {
    let result = ok_text(status);
    if result_fits(machine, aw_tools::OperationId::JobsGet, &result, budget) {
        return result;
    }
    tool_error(
        "Job status exceeds the response budget; use an exact node ref or increase the budget",
    )
}

fn result_fits(
    machine: &LocalMachine,
    operation: aw_tools::OperationId,
    result: &CallToolResult,
    budget: Option<usize>,
) -> bool {
    budget.is_none_or(|limit| {
        machine
            .exact_decorated_result_len(operation, result.clone())
            .is_ok_and(|size| size <= limit)
    })
}

/// A launched Job must retain a complete model-visible continuation ref, even
/// when its rich status cannot fit the authenticated transport envelope.
fn bounded_job_handoff(
    machine: &LocalMachine,
    reference: &JobRef,
    budget: Option<usize>,
) -> CallToolResult {
    let (id, node) = match job_ref_parts(reference) {
        Ok(parts) => parts,
        Err(error) => return tool_error(error),
    };
    if node.is_some() {
        return tool_error("Job handoff requires a root Job ref");
    }
    machine
        .jobs
        .standalone_handoff_fitted(id, |text| {
            result_fits(
                machine,
                aw_tools::OperationId::Batch,
                &ok_text(text.to_owned()),
                budget,
            )
        })
        .map(ok_text)
        .unwrap_or_else(tool_error)
}

/// The replay ledger retains the original receipt. A narrower authenticated
/// response may project the same Job ref compactly; it may never rerun work or
/// silently clip away the continuation.
fn bounded_cached_job_handoff(
    machine: &LocalMachine,
    reference: &JobRef,
    cached: CallToolResult,
    budget: Option<usize>,
) -> CallToolResult {
    if cached.is_error.unwrap_or(false)
        || result_fits(machine, aw_tools::OperationId::Batch, &cached, budget)
    {
        return cached;
    }
    let meta = cached.meta.clone();
    let (id, node) = match job_ref_parts(reference) {
        Ok(parts) => parts,
        Err(error) => {
            let mut error = tool_error(error);
            error.meta = meta;
            return error;
        }
    };
    if node.is_some() {
        let mut error = tool_error("Cached Job handoff is not a root Job; list Jobs, do not rerun");
        error.meta = meta;
        return error;
    }
    match machine.jobs.standalone_handoff_fitted(id, |text| {
        let mut projected = ok_text(text.to_owned());
        projected.meta = meta.clone();
        result_fits(machine, aw_tools::OperationId::Batch, &projected, budget)
    }) {
        Ok(text) => {
            let mut projected = ok_text(text);
            projected.meta = meta;
            projected
        }
        Err(_) => {
            let mut error = tool_error(
                "Job is already admitted, but its handoff exceeds the response budget; list Jobs to recover it without rerunning",
            );
            error.meta = meta;
            error
        }
    }
}

fn bounded_control_success(
    machine: &LocalMachine,
    operation: aw_tools::OperationId,
    reference: &JobRef,
    result: CallToolResult,
    budget: Option<usize>,
) -> CallToolResult {
    if result_fits(machine, operation, &result, budget) {
        return result;
    }
    let meta = result.meta;
    let mut error = tool_error(format!(
        "{reference} Job Control effect already applied, but its receipt exceeds the response budget; inspect with jobs_get and do not repeat the control"
    ));
    error.meta = meta;
    error
}

/// Reproject the original completed effect; never mutate its replay receipt
/// or convert an error into a successful Control acknowledgement.
fn bounded_control_replay(
    machine: &LocalMachine,
    operation: aw_tools::OperationId,
    cached: CallToolResult,
    reference: &JobRef,
    budget: Option<usize>,
) -> CallToolResult {
    if cached.is_error.unwrap_or(false) {
        return cached;
    }
    if result_fits(machine, operation, &cached, budget) {
        return cached;
    }
    let mut error = tool_error(format!(
        "{reference} Job Control effect already applied, but its replay receipt exceeds the response budget; inspect with jobs_get before retrying"
    ));
    error.meta = cached.meta;
    error
}

/// A completed stdin write must never become an ambiguously truncated
/// success. The minimal receipt deliberately avoids claiming a new revision.
fn bounded_send_result(
    machine: &LocalMachine,
    reference: &JobRef,
    original: CallToolResult,
    budget: Option<usize>,
) -> CallToolResult {
    if result_fits(machine, aw_tools::OperationId::JobsSend, &original, budget) {
        return original;
    }
    let meta = original.meta;
    let mut compact = ok_text(format!("{reference} sent"));
    compact.meta = meta.clone();
    if result_fits(machine, aw_tools::OperationId::JobsSend, &compact, budget) {
        compact
    } else {
        let mut error = tool_error(format!(
            "{reference} stdin was already accepted, but its receipt exceeds the response budget; inspect with jobs_get, do not resend"
        ));
        error.meta = meta;
        error
    }
}

fn bounded_send_replay(
    machine: &LocalMachine,
    reference: &JobRef,
    cached: CallToolResult,
    budget: Option<usize>,
) -> CallToolResult {
    if cached.is_error.unwrap_or(false) {
        return cached;
    }
    if result_fits(machine, aw_tools::OperationId::JobsSend, &cached, budget) {
        return cached;
    }
    let mut compact = ok_text(format!("{reference} sent"));
    compact.meta = cached.meta.clone();
    if result_fits(machine, aw_tools::OperationId::JobsSend, &compact, budget) {
        compact
    } else {
        let mut error = tool_error(format!(
            "{reference} stdin was already accepted, but its replay receipt exceeds the response budget; inspect with jobs_get, do not resend"
        ));
        error.meta = cached.meta;
        error
    }
}

fn complete_remote_start_result(
    machine: &LocalMachine,
    operation: aw_tools::OperationId,
    guard: Option<RemoteStartGuard>,
    reference: &JobRef,
    mut canonical: CallToolResult,
    project: impl FnOnce(CallToolResult) -> CallToolResult,
) -> CallToolResult {
    let Some(guard) = guard else {
        return project(canonical);
    };
    if let Err(error) = machine.stamp_cached_mcp_receipt(operation, &mut canonical) {
        let fallback = tool_error(format!(
            "remote Start effect already crossed its owner boundary, but its timestamp receipt could not be finalized: {error}; inspect the Job and do not repeat the effect"
        ));
        return match guard.mark_ready(RemoteStartReceipt::JobMcp {
            reference: reference.clone(),
            result: fallback.clone(),
        }) {
            Ok(()) => fallback,
            Err(cache_error) => tool_error(format!(
                "remote Start effect already crossed its owner boundary, but its replay receipt could not be committed: {cache_error}; inspect the Job and do not repeat the effect"
            )),
        };
    }
    match guard.mark_ready(RemoteStartReceipt::JobMcp {
        reference: reference.clone(),
        result: canonical.clone(),
    }) {
        Ok(()) => project(canonical),
        Err(error) => tool_error(format!(
            "remote Start effect already crossed its owner boundary, but its replay receipt could not be committed: {error}; inspect the Job and do not repeat the effect"
        )),
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
    let profile = crate::jobs::dotlink_job_profile(max_output_bytes);
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
                "Maximum retained raw bytes for this page. Defaults to {} bytes, may be as small as 1 byte and is capped at {}.",
                profile.read_page_default_bytes,
                profile.read_page_max_bytes
            )),
        );
        property.insert("minimum".to_owned(), serde_json::Value::from(1));
        property.insert(
            "maximum".to_owned(),
            serde_json::Value::from(profile.read_page_max_bytes),
        );
        property.insert(
            "default".to_owned(),
            serde_json::Value::from(profile.read_page_default_bytes),
        );
    }
    tool.input_schema = std::sync::Arc::new(schema);
}

fn parse_ref(reference: &str) -> Result<JobRef, String> {
    JobRef::parse(reference).map_err(|_| "invalid finite Job ref".into())
}

fn job_ref_parts(reference: &JobRef) -> Result<(&str, Option<usize>), String> {
    let node = reference
        .node()
        .map(|node| {
            usize::try_from(node.get()).map_err(|_| "Job node coordinate is unsupported".to_owned())
        })
        .transpose()?;
    Ok((reference.job().as_str(), node))
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
        let reference = match crate::jobs::JobManager::standalone_ref(id, None) {
            Ok(reference) => reference,
            Err(error) => return tool_error(error),
        };
        match self.jobs.wait_until(id, deadline).await {
            Ok(JobWait::Pending) => match self.jobs.expose_compatibility(id) {
                Ok(_) => bounded_job_handoff(self, &reference, result_budget),
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
                        if !result_fits(self, aw_tools::OperationId::Batch, &result, result_budget)
                        {
                            let _ = self.jobs.expose_compatibility(id);
                            return bounded_job_handoff(self, &reference, result_budget);
                        }
                        match self.jobs.mark_synchronous_result(id) {
                            Ok(_) => result,
                            Err(error) => tool_error(error),
                        }
                    }
                    Ok(None) => {
                        let _ = self.jobs.expose_compatibility(id);
                        bounded_job_handoff(self, &reference, result_budget)
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
                crate::jobs::JobManager::standalone_ref(id, None)
                    .map(|reference| bounded_job_handoff(self, &reference, result_budget))
                    .unwrap_or_else(tool_error),
            ),
            Ok(Some(JobHandoff::Synchronous)) => {
                let render_budget = result_budget
                    .unwrap_or(self.config.max_output_bytes)
                    .min(self.config.max_output_bytes)
                    .max(1);
                Some(match self.jobs.render_graph_result(id, render_budget) {
                    Ok(Some(text)) => {
                        let result = ok_text(text);
                        if result_fits(self, aw_tools::OperationId::Batch, &result, result_budget) {
                            result
                        } else {
                            tool_error(
                                "committed synchronous Graph result no longer fits the request budget",
                            )
                        }
                    }
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
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::JobMcp {
                    reference,
                    result,
                })) => {
                    return bounded_cached_job_handoff(self, &reference, result, result_budget);
                }
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(result))) => {
                    return result;
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
            let reference = match crate::jobs::JobManager::standalone_ref(&started.id, None) {
                Ok(reference) => reference,
                Err(error) => {
                    guard.mark_unknown();
                    return tool_error(error);
                }
            };
            let canonical = self
                .jobs
                .standalone_handoff_fitted(&started.id, |_| true)
                .map(ok_text)
                .unwrap_or_else(tool_error);
            return complete_remote_start_result(
                self,
                aw_tools::OperationId::Batch,
                Some(guard),
                &reference,
                canonical,
                |cached| bounded_cached_job_handoff(self, &reference, cached, result_budget),
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
            return match crate::jobs::JobManager::standalone_ref(&started.id, None) {
                Ok(reference) => bounded_job_handoff(self, &reference, result_budget),
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
                result_fits(
                    self,
                    aw_tools::OperationId::JobsList,
                    &ok_text(page.to_owned()),
                    Some(ceiling),
                )
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
        let (id, node) = match job_ref_parts(&reference) {
            Ok(parts) => parts,
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
        match self.jobs.standalone_get(id, node, args.rev, wait_ms).await {
            Ok(status) => {
                match self.jobs.standalone_process_tail_fitted(
                    id,
                    node,
                    args.tail,
                    status,
                    |candidate| {
                        result_fits(
                            self,
                            aw_tools::OperationId::JobsGet,
                            &ok_text(candidate.to_owned()),
                            Some(result_budget),
                        )
                    },
                ) {
                    Ok(status) => bounded_job_status(self, status, Some(result_budget)),
                    Err(error) => tool_error(error),
                }
            }
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
        let (id, node) = match job_ref_parts(&reference) {
            Ok(parts) => parts,
            Err(error) => return tool_error(error),
        };
        let profile = crate::jobs::dotlink_job_profile(self.config.max_output_bytes);
        let raw_page = match profile.resolve_read_page(args.limit) {
            Ok(raw_page) => raw_page,
            Err(_) => {
                return tool_error(format!(
                    "jobs_read limit must be within 1..={}",
                    profile.read_page_max_bytes
                ));
            }
        };
        // Raw page bytes and serialized response bytes are independent
        // budgets. In particular, a one-byte raw page still needs room for its
        // JSON/MCP envelope. Trusted transports may narrow the serialized
        // result; local/stdio callers fall back to the same profile-derived
        // response capacity used by the transport configuration.
        let response_ceiling = match SerializedResultBytes::new(
            self.request_result_budget(&context)
                .unwrap_or_else(|| self.max_response_body_bytes()),
        ) {
            Some(response_ceiling) => response_ceiling,
            None => return tool_error("jobs_read response budget must be at least 1 byte"),
        };
        let offset = args.offset.unwrap_or(0);
        let base64 = matches!(args.encoding, OutputEncoding::Base64);
        let wait_ms = bounded_wait_ms(
            args.wait_ms.unwrap_or(0),
            self.request_sync_budget_millis(&context),
        );
        let is_graph = match self.jobs.standalone_is_graph(id) {
            Ok(value) => value,
            Err(error) => return tool_error(error),
        };
        if is_graph && node.is_none() {
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
                    id,
                    offset,
                    raw_page,
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
                id,
                crate::jobs::OutputPageRequest {
                    node,
                    stream: args.view.map(Into::into),
                    offset,
                    requested_bytes: raw_page,
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
        let (id, node) = match job_ref_parts(&reference) {
            Ok(parts) => parts,
            Err(error) => return tool_error(error),
        };
        let mode = args.offset.map(AckMode::Through).unwrap_or(AckMode::Finish);
        let remote_start = context
            .extensions
            .get::<crate::remote_start::ValidatedRemoteStart>()
            .cloned();
        let guard = if let Some(start) = remote_start.as_ref() {
            match self.jobs.claim_remote_start(start) {
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::JobMcp {
                    reference: cached_reference,
                    result,
                })) => {
                    if cached_reference != reference {
                        return tool_error(
                            "Cached Job Control identity differs; inspect the Job before retrying",
                        );
                    }
                    return bounded_control_replay(
                        self,
                        aw_tools::OperationId::JobsAck,
                        result,
                        &reference,
                        result_budget,
                    );
                }
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(_))) => {
                    return tool_error(
                        "Cached Job Control outcome lacks typed identity; inspect the Job before retrying",
                    );
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
            Some(offset) => {
                self.jobs
                    .acknowledge_node_output(id, node, args.view.map(Into::into), offset)
            }
            None => self
                .jobs
                .finish_node_output(id, node, args.view.map(Into::into)),
        };
        match applied {
            Ok(ack) => {
                let receipt = match render_plain(
                    Receipt::Ack {
                        reference: &reference,
                        mode,
                        changed: ack.changed,
                        released: ack.released,
                    },
                    aw_tools::filesystem::job_manager::MAX_COMPACT_JOB_STATUS_BYTES,
                ) {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        let result =
                            tool_error(format!("cannot render typed Job ACK receipt: {error:?}"));
                        return complete_remote_start_result(
                            self,
                            aw_tools::OperationId::JobsAck,
                            guard,
                            &reference,
                            result,
                            |result| result,
                        );
                    }
                };
                complete_remote_start_result(
                    self,
                    aw_tools::OperationId::JobsAck,
                    guard,
                    &reference,
                    ok_text(receipt),
                    |canonical| {
                        bounded_control_success(
                            self,
                            aw_tools::OperationId::JobsAck,
                            &reference,
                            canonical,
                            result_budget,
                        )
                    },
                )
            }
            Err(error) => {
                let result = tool_error(error);
                complete_remote_start_result(
                    self,
                    aw_tools::OperationId::JobsAck,
                    guard,
                    &reference,
                    result,
                    |result| result,
                )
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
        let (id, node) = match job_ref_parts(&reference) {
            Ok(parts) => parts,
            Err(error) => return tool_error(error),
        };
        let input = match args.input.into_process() {
            Ok(input) => input,
            Err(error) => return tool_error(error),
        };
        let guard = if let Some(start) = remote_start.as_ref() {
            match self.jobs.claim_remote_start(start) {
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::JobMcp {
                    reference: cached_reference,
                    result,
                })) => {
                    if cached_reference != reference {
                        return tool_error(
                            "Cached stdin Job identity differs; inspect Job, do not resend",
                        );
                    }
                    return bounded_send_replay(self, &reference, result, result_budget);
                }
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(_))) => {
                    return tool_error(
                        "Cached stdin outcome lacks typed identity; inspect Job, do not resend",
                    );
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
        let minimal = ok_text(format!("{reference} sent"));
        let minimal_size =
            match self.exact_decorated_result_len(aw_tools::OperationId::JobsSend, minimal) {
                Ok(size) => size,
                Err(error) => {
                    if let Some(guard) = guard {
                        guard.release_before_effect();
                    }
                    return tool_error(format!(
                        "jobs_send receipt preflight failed before stdin delivery: {error}"
                    ));
                }
            };
        if result_budget.is_some_and(|limit| minimal_size > limit) {
            if let Some(guard) = guard {
                guard.release_before_effect();
            }
            return tool_error(
                "jobs_send response budget cannot fit a complete receipt; stdin was not sent",
            );
        }
        match self
            .send_process_input_checked(id, node, args.rev, input)
            .await
        {
            Ok(revision) => {
                let canonical = ok_text(format!("{reference} rev={revision} ok"));
                complete_remote_start_result(
                    self,
                    aw_tools::OperationId::JobsSend,
                    guard,
                    &reference,
                    canonical,
                    |canonical| bounded_send_result(self, &reference, canonical, result_budget),
                )
            }
            Err(error) => {
                let result = tool_error(error);
                complete_remote_start_result(
                    self,
                    aw_tools::OperationId::JobsSend,
                    guard,
                    &reference,
                    result,
                    |result| result,
                )
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
        let (id, node) = match job_ref_parts(&reference) {
            Ok(parts) => parts,
            Err(error) => return tool_error(error),
        };
        if node.is_some() {
            return tool_error("standalone jobs_cancel targets a Job root, not a Graph node");
        }
        let guard = if let Some(start) = remote_start.as_ref() {
            match self.jobs.claim_remote_start(start) {
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::JobMcp {
                    reference: cached_reference,
                    result,
                })) => {
                    if cached_reference != reference {
                        return tool_error(
                            "Cached cancellation Job identity differs; inspect the Job before retrying",
                        );
                    }
                    return bounded_control_replay(
                        self,
                        aw_tools::OperationId::JobsCancel,
                        result,
                        &reference,
                        result_budget,
                    );
                }
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(_))) => {
                    return tool_error(
                        "Cached cancellation outcome lacks typed identity; inspect the Job before retrying",
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
        match self.jobs.standalone_cancel_checked(id, args.rev) {
            Ok((changed, revision)) => {
                let receipt = match render_plain(
                    Receipt::Cancel {
                        reference: &reference,
                        revision,
                        changed,
                    },
                    aw_tools::filesystem::job_manager::MAX_COMPACT_JOB_STATUS_BYTES,
                ) {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        let result = tool_error(format!(
                            "Job cancellation was already accepted, but its typed receipt could not be rendered: {error:?}; inspect with jobs_get and do not repeat cancellation"
                        ));
                        return complete_remote_start_result(
                            self,
                            aw_tools::OperationId::JobsCancel,
                            guard,
                            &reference,
                            result,
                            |result| result,
                        );
                    }
                };
                complete_remote_start_result(
                    self,
                    aw_tools::OperationId::JobsCancel,
                    guard,
                    &reference,
                    ok_text(receipt),
                    |canonical| {
                        bounded_control_success(
                            self,
                            aw_tools::OperationId::JobsCancel,
                            &reference,
                            canonical,
                            result_budget,
                        )
                    },
                )
            }
            Err(error) => {
                let result = tool_error(error);
                complete_remote_start_result(
                    self,
                    aw_tools::OperationId::JobsCancel,
                    guard,
                    &reference,
                    result,
                    |result| result,
                )
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

    fn raw(bytes: usize) -> aw_tools::budget::RawPageBytes {
        aw_tools::budget::RawPageBytes::new(bytes).expect("test raw page budget")
    }

    fn serialized(bytes: usize) -> SerializedResultBytes {
        SerializedResultBytes::new(bytes).expect("test serialized result budget")
    }

    fn running_shared_process(
        manager: &crate::jobs::JobManager,
    ) -> (crate::jobs::JobHandle, crate::jobs::SharedShellWriters) {
        if manager.policy_fingerprint().is_err() {
            assert!(manager.bind_policy([9; 32]));
        }
        let job = manager.create_for_test(30).unwrap();
        let result_limit = crate::limits::DEFAULT_TRANSFER_BYTES;
        let writers = job
            .claim_prepared_shell(
                aw_tools::filesystem::batch::NodeTool::Bash,
                serde_json::json!({
                    "command":"synthetic-job-control",
                    "dir":".",
                    "stdin":null,
                    "timeout_secs":30,
                    "max_output_bytes":result_limit
                }),
                None,
                result_limit,
                crate::limits::MAX_JOB_SPOOL_BYTES_PER_STREAM,
            )
            .unwrap();
        job.mark_launched().unwrap();
        manager.expose_compatibility(&job.id).unwrap();
        (job, writers)
    }

    fn settle_shared_process(
        manager: &crate::jobs::JobManager,
        job: &crate::jobs::JobHandle,
        writers: &crate::jobs::SharedShellWriters,
    ) {
        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        manager.finish(
            &job.id,
            crate::jobs::JobCompletion::completed(
                Some(0),
                true,
                job.shared_stream_summary(crate::jobs::JobStream::Stdout, false),
                job.shared_stream_summary(crate::jobs::JobStream::Stderr, false),
            ),
        );
    }

    fn process_tail_with_ceiling(
        machine: &LocalMachine,
        id: &str,
        node: Option<usize>,
        requested: Option<usize>,
        status: String,
        response_ceiling: usize,
    ) -> crate::jobs::JobResult<String> {
        machine
            .jobs
            .standalone_process_tail_fitted(id, node, requested, status, |candidate| {
                crate::mcp::serialized_json_len(&ok_text(candidate.to_owned()))
                    .is_some_and(|size| size <= response_ceiling)
            })
    }

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
    fn standalone_refs_use_the_shared_finite_job_ref_grammar() {
        let root = parse_ref("j:abc-123").unwrap();
        assert_eq!(root.job().as_str(), "abc-123");
        assert_eq!(root.node(), None);
        assert_eq!(root.to_string(), "j:abc-123");

        let node = parse_ref("j:abc-123/n:4").unwrap();
        assert_eq!(node.job().as_str(), "abc-123");
        assert_eq!(node.node().map(|node| node.get()), Some(4));
        assert_eq!(node.to_string(), "j:abc-123/n:4");
        assert_eq!(
            crate::jobs::JobManager::standalone_ref("abc-123", Some(4))
                .unwrap()
                .to_string(),
            node.to_string(),
            "trusted internal construction and model parsing must share one grammar"
        );

        for invalid in ["", ".", "^", "abc", "j:", "j:x/n:0", "j:x/n:01", "j:x/x:1"] {
            assert!(parse_ref(invalid).is_err(), "{invalid}");
        }
        let hostile = format!("j:{}", "x".repeat(32 * 1024));
        let error = parse_ref(&hostile).unwrap_err();
        assert_eq!(error, "invalid finite Job ref");
        assert!(!error.contains(&hostile));
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

    #[tokio::test]
    async fn finite_get_waits_for_process_settlement_even_with_stale_revision() {
        let manager = crate::jobs::JobManager::new(std::time::Duration::from_secs(60)).unwrap();
        let (job, writers) = running_shared_process(&manager);
        assert_eq!(writers.stdout.append(b"early").unwrap(), 5);
        job.notify_output_changed();
        let (rev, _) = manager.standalone_status(&job.id, None).unwrap();

        // A stale observed revision is never permission to return before the
        // requested settle-or-deadline condition.
        let pending = manager
            .standalone_get(&job.id, None, Some(rev.saturating_sub(1)), 40)
            .await
            .unwrap();
        assert!(
            pending.contains(" running ") || pending.contains(" waiting "),
            "{pending}"
        );

        let waiting_manager = manager.clone();
        let waiting_id = job.id.clone();
        let terminal_wait = tokio::spawn(async move {
            waiting_manager
                .standalone_get(&waiting_id, None, Some(rev.saturating_sub(1)), 1_500)
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        assert!(
            !terminal_wait.is_finished(),
            "jobs_get must remain pending until settlement even when revision is stale"
        );

        assert_eq!(writers.stdout.append(b"late").unwrap(), 4);
        job.notify_output_changed();
        settle_shared_process(&manager, &job, &writers);
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), terminal_wait)
            .await
            .expect("jobs_get woke after process settlement")
            .unwrap();
        assert!(terminal.contains(" settled "), "{terminal}");
        assert_eq!(
            manager
                .standalone_get(&job.id, None, None, 1_500)
                .await
                .unwrap(),
            terminal,
            "terminal get must not wait again"
        );
        manager.shutdown().await.unwrap();
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
        let first_tail =
            process_tail_with_ceiling(&machine, &started.id, Some(1), None, first.clone(), 4096)
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
            process_tail_with_ceiling(&machine, &started.id, None, None, root.clone(), 4096)
                .unwrap(),
            root,
            "Graph roots do not synthesize Process tails"
        );
        assert!(
            process_tail_with_ceiling(&machine, &started.id, None, Some(1), root.clone(), 4096,)
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
                    requested_bytes: raw(64),
                    response_ceiling: serialized(4096),
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

    #[tokio::test]
    async fn finite_read_waits_for_new_bytes_and_eof_not_shell_revision() {
        let manager = crate::jobs::JobManager::new(std::time::Duration::from_secs(60)).unwrap();
        let (job, writers) = running_shared_process(&manager);
        let request = |offset: u64| crate::jobs::OutputPageRequest {
            node: None,
            stream: Some(crate::jobs::JobOutputStream::Stdout),
            offset,
            requested_bytes: raw(128),
            response_ceiling: serialized(4096),
            base64: false,
        };

        let first_manager = manager.clone();
        let first_id = job.id.clone();
        let first_wait = tokio::spawn(async move {
            first_manager
                .output_node_page_wait(&first_id, request(0), 1_500)
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        assert!(!first_wait.is_finished());
        assert_eq!(writers.stdout.append(b"first").unwrap(), 5);
        job.notify_output_changed();
        let page = tokio::time::timeout(std::time::Duration::from_secs(2), first_wait)
            .await
            .expect("jobs_read woke for first retained bytes")
            .unwrap();
        let first: serde_json::Value = serde_json::from_str(&page).unwrap();
        assert_eq!(first["data"], "first", "{first}");
        assert_eq!(first["next_offset"], 5);
        assert_eq!(first["complete"], false, "first bytes precede writer EOF");

        // Job/status revisions are irrelevant to byte availability. With no
        // new bytes and no EOF, the read waits until its own short deadline.
        let short = manager
            .output_node_page_wait(&job.id, request(5), 35)
            .await
            .unwrap();
        let short: serde_json::Value = serde_json::from_str(&short).unwrap();
        assert_eq!(short["data"], "", "{short}");
        assert_eq!(short["next_offset"], 5);
        assert_eq!(short["complete"], false);

        let second_manager = manager.clone();
        let second_id = job.id.clone();
        let second_wait = tokio::spawn(async move {
            second_manager
                .output_node_page_wait(&second_id, request(5), 1_500)
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        assert!(!second_wait.is_finished());
        assert_eq!(writers.stdout.append(b"second").unwrap(), 6);
        job.notify_output_changed();
        let page = tokio::time::timeout(std::time::Duration::from_secs(2), second_wait)
            .await
            .expect("jobs_read woke for second retained bytes")
            .unwrap();
        let second: serde_json::Value = serde_json::from_str(&page).unwrap();
        assert_eq!(second["data"], "second", "{second}");
        assert_eq!(second["next_offset"], 11);

        let eof_manager = manager.clone();
        let eof_id = job.id.clone();
        let eof_wait = tokio::spawn(async move {
            eof_manager
                .output_node_page_wait(&eof_id, request(11), 1_500)
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        assert!(!eof_wait.is_finished());
        writers.stdout.finish().unwrap();
        job.notify_output_changed();
        let eof = tokio::time::timeout(std::time::Duration::from_secs(2), eof_wait)
            .await
            .expect("jobs_read woke for writer EOF")
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
            serde_json::from_str(&manager.status(&job.id).unwrap()).unwrap();
        assert_eq!(status["stdout"]["acknowledged_bytes"], 0);
        assert_eq!(status["stdout"]["released"], false);

        writers.stderr.finish().unwrap();
        manager.finish(
            &job.id,
            crate::jobs::JobCompletion::completed(
                Some(0),
                true,
                job.shared_stream_summary(crate::jobs::JobStream::Stdout, false),
                job.shared_stream_summary(crate::jobs::JobStream::Stderr, false),
            ),
        );
        manager.shutdown().await.unwrap();
    }

    #[test]
    fn status_budget_never_returns_truncated_success() {
        let temp = tempfile::tempdir().unwrap();
        let machine =
            crate::mcp::tools::test_support::test_machine(temp.path().canonicalize().unwrap());
        let full = "j:example node settled rev=12 ".to_owned() + &"status-output".repeat(800);
        let success = ok_text(full.clone());
        let exact = machine
            .exact_decorated_result_len(aw_tools::OperationId::JobsGet, success)
            .unwrap();
        let accepted = bounded_job_status(&machine, full.clone(), Some(exact));
        assert!(!accepted.is_error.unwrap_or(false));
        assert_eq!(accepted.content[0].as_text().unwrap().text, full);

        let oversized = bounded_job_status(&machine, full, Some(512));
        assert!(oversized.is_error.unwrap_or(false));
        let message = &oversized.content[0].as_text().unwrap().text;
        assert!(message.contains("exceeds the response budget"));
        assert!(!message.contains("status-output"));
    }

    #[tokio::test]
    async fn large_graph_result_handoff_keeps_typed_ref_under_small_budget() {
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
        let (revision, _) = machine.jobs.standalone_status(&started.id, None).unwrap();
        let reference = crate::jobs::JobManager::standalone_ref(&started.id, None).unwrap();
        let concise = render_plain(
            Receipt::Handoff {
                reference: &reference,
                revision,
            },
            aw_tools::filesystem::job_manager::MAX_COMPACT_JOB_STATUS_BYTES,
        )
        .unwrap();
        let budget = machine
            .exact_decorated_result_len(aw_tools::OperationId::Batch, ok_text(concise))
            .unwrap();
        let result = machine
            .finish_graph_call(&started.id, None, Some(budget))
            .await;
        assert!(!result.is_error.unwrap_or(false), "{result:?}");
        let result_text = &result.content[0].as_text().unwrap().text;
        assert_eq!(
            result_text.split_whitespace().next(),
            Some(format!("j:{}", started.id).as_str())
        );
        assert!(result_text.contains("rev="), "{result_text}");
        assert!(!result_text.contains("long-line"), "{result_text}");
        assert!(crate::mcp::serialized_json_len(&result).unwrap() <= budget);
        let repeated = machine
            .replay_graph_response(&started.id, Some(budget))
            .await
            .unwrap();
        assert!(!repeated.is_error.unwrap_or(false), "{repeated:?}");
        let repeated_text = &repeated.content[0].as_text().unwrap().text;
        assert_eq!(
            repeated_text.split_whitespace().next(),
            Some(format!("j:{}", started.id).as_str())
        );
        assert!(repeated_text.contains("rev="), "{repeated_text}");
        assert!(!repeated_text.contains("long-line"), "{repeated_text}");
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
    fn shared_control_receipts_keep_typed_identity_and_factual_grammar() {
        let temp = tempfile::tempdir().unwrap();
        let machine =
            crate::mcp::tools::test_support::test_machine(temp.path().canonicalize().unwrap());
        let root = parse_ref("j:control-123").unwrap();
        let node = parse_ref("j:control-123/n:2").unwrap();

        let ack = render_plain(
            Receipt::Ack {
                reference: &node,
                mode: AckMode::Through(47),
                changed: true,
                released: false,
            },
            128,
        )
        .unwrap();
        assert_eq!(ack, "j:control-123/n:2 ok ack=47 released=false");

        let finish = render_plain(
            Receipt::Ack {
                reference: &node,
                mode: AckMode::Finish,
                changed: false,
                released: true,
            },
            128,
        )
        .unwrap();
        assert_eq!(finish, "j:control-123/n:2 unchanged ack=all released=true");

        let cancel = render_plain(
            Receipt::Cancel {
                reference: &root,
                revision: 42,
                changed: true,
            },
            128,
        )
        .unwrap();
        assert_eq!(cancel, "j:control-123 rev=42 cancel=accepted");

        let mut canonical = ok_text(cancel.clone());
        crate::mcp::set_stored_receipt_ts(&mut canonical, 77).unwrap();
        let exact = machine
            .exact_decorated_result_len(aw_tools::OperationId::JobsCancel, canonical.clone())
            .unwrap();
        assert_eq!(
            bounded_control_success(
                &machine,
                aw_tools::OperationId::JobsCancel,
                &root,
                canonical.clone(),
                Some(exact),
            ),
            canonical
        );
        let too_small = bounded_control_success(
            &machine,
            aw_tools::OperationId::JobsCancel,
            &root,
            canonical.clone(),
            Some(exact - 1),
        );
        assert!(too_small.is_error.unwrap_or(false));
        assert_eq!(crate::mcp::stored_receipt_ts(&too_small), Some(77));
        assert!(
            too_small.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("effect already applied")
        );

        let cached = canonical;
        assert_eq!(
            bounded_control_replay(
                &machine,
                aw_tools::OperationId::JobsCancel,
                cached.clone(),
                &root,
                Some(exact),
            ),
            cached,
            "typed identity is validated by the replay ledger before projection"
        );
        let failed = tool_error("stale Job revision");
        assert_eq!(
            bounded_control_replay(
                &machine,
                aw_tools::OperationId::JobsCancel,
                failed.clone(),
                &root,
                Some(1),
            ),
            failed,
            "a failed effect replay cannot be turned into Control success"
        );
    }

    #[test]
    fn send_receipts_are_exact_bounded_and_replay_preserves_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let machine =
            crate::mcp::tools::test_support::test_machine(temp.path().canonicalize().unwrap());
        let reference = parse_ref("j:send-123/n:2").unwrap();
        let name = "j:send-123/n:2";
        let mut full = ok_text(format!("{name} rev={} ok", u64::MAX));
        crate::mcp::set_stored_receipt_ts(&mut full, 88).unwrap();
        let full_size = machine
            .exact_decorated_result_len(aw_tools::OperationId::JobsSend, full.clone())
            .unwrap();
        let bare_compact = ok_text(format!("{name} sent"));
        let mut compact = bare_compact.clone();
        compact.meta = full.meta.clone();
        let compact_size = machine
            .exact_decorated_result_len(aw_tools::OperationId::JobsSend, compact.clone())
            .unwrap();
        assert!(compact_size < full_size);

        assert_eq!(
            bounded_send_result(&machine, &reference, full.clone(), Some(full_size)),
            full
        );
        let narrowed = bounded_send_result(&machine, &reference, full.clone(), Some(compact_size));
        assert_eq!(narrowed, compact);
        assert!(crate::mcp::serialized_json_len(&narrowed).unwrap() <= compact_size);
        let too_small =
            bounded_send_result(&machine, &reference, full.clone(), Some(compact_size - 1));
        assert!(too_small.is_error.unwrap_or(false));
        assert_eq!(crate::mcp::stored_receipt_ts(&too_small), Some(88));
        let detail = &too_small.content[0].as_text().unwrap().text;
        assert!(detail.contains("already accepted") && detail.contains("do not resend"));

        assert_eq!(
            bounded_send_replay(&machine, &reference, full.clone(), Some(compact_size)),
            compact,
            "narrow replay may drop a revision but must preserve the exact send identity"
        );
        assert_eq!(
            bounded_send_replay(&machine, &reference, compact.clone(), Some(full_size)),
            compact,
            "a replay cannot invent a revision omitted from the cached receipt"
        );
        assert_eq!(
            full.content[0].as_text().unwrap().text,
            format!("{name} rev={} ok", u64::MAX)
        );
        let denied = tool_error("Process authority revoked; stdin outcome unknown");
        assert_eq!(
            bounded_send_replay(&machine, &reference, denied.clone(), Some(1)),
            denied,
            "cached error can never become a success"
        );
        let opaque = ok_text("cached-rendering-is-not-semantic-state");
        let opaque_size = machine
            .exact_decorated_result_len(aw_tools::OperationId::JobsSend, opaque.clone())
            .unwrap();
        assert_eq!(
            bounded_send_replay(&machine, &reference, opaque.clone(), Some(opaque_size)),
            opaque,
            "trusted replay identity is external to rendered prose"
        );
        let opaque_large = ok_text("cached-rendering-is-not-semantic-state ".repeat(64));
        let bare_compact_size = machine
            .exact_decorated_result_len(aw_tools::OperationId::JobsSend, bare_compact.clone())
            .unwrap();
        assert_eq!(
            bounded_send_replay(&machine, &reference, opaque_large, Some(bare_compact_size)),
            bare_compact,
            "a narrow replay projects the already-known typed Job ref without parsing cached text"
        );
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
        let bare_minimal_size = crate::mcp::serialized_json_len(&minimal).unwrap();
        let minimal_size = machine
            .exact_decorated_result_len(aw_tools::OperationId::JobsSend, minimal.clone())
            .unwrap();
        assert!(
            bare_minimal_size < minimal_size,
            "receipt metadata + visible timestamp must consume response envelope space"
        );
        let args = || {
            serde_json::from_value::<JobsSendArgs>(serde_json::json!({
                "ref":reference, "input":{"text":"hello\n"}
            }))
            .unwrap()
        };

        let denied = machine
            .jobs_send_impl(args(), None, Some(bare_minimal_size))
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
                issuer: Some(aw_tools::remote_start::RemoteStartIssuer {
                    client_id: "aw-client".into(),
                    trust_epoch: 1,
                }),
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
                remote_start_trust_epoch: Some(1),
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
        let reference = crate::jobs::JobManager::standalone_ref(&started.id, None).unwrap();
        let (_, status) = machine.jobs.standalone_status(&started.id, None).unwrap();
        let narrow_first = complete_remote_start_result(
            &machine,
            aw_tools::OperationId::Batch,
            Some(guard),
            &reference,
            ok_text(status.clone()),
            |cached| bounded_cached_job_handoff(&machine, &reference, cached, Some(1)),
        );
        assert!(
            narrow_first.is_error.unwrap_or(false),
            "an impossible first response budget must project an error after the canonical receipt is cached"
        );
        let first_ts = crate::mcp::stored_receipt_ts(&narrow_first)
            .expect("narrow first response preserves cached Batch timestamp");
        machine.jobs.wait_until(&started.id, None).await.unwrap();
        assert!(!root.join("a.txt").exists());
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "a");

        let replay = match machine.jobs.claim_remote_start(&start).unwrap() {
            RemoteStartDecision::Existing(RemoteStartReceipt::JobMcp {
                reference: cached_reference,
                result,
            }) => {
                assert_eq!(cached_reference, reference);
                result
            }
            _ => panic!("duplicate Graph Start must replay the first typed receipt"),
        };
        assert!(
            !replay.is_error.unwrap_or(false),
            "a wider replay must recover the canonical successful receipt, not the first narrow projection"
        );
        assert_eq!(replay.content[0].as_text().unwrap().text, status);
        assert_eq!(crate::mcp::stored_receipt_ts(&replay), Some(first_ts));
        assert_ne!(
            replay, narrow_first,
            "the replay ledger must not cache a request-specific narrow projection"
        );
        // A replay under a smaller envelope keeps the original ledger result
        // and returns a complete ref instead of a clipped success receipt.
        let (current_revision, _) = machine.jobs.standalone_status(&started.id, None).unwrap();
        let concise = render_plain(
            Receipt::Handoff {
                reference: &reference,
                revision: current_revision,
            },
            aw_tools::filesystem::job_manager::MAX_COMPACT_JOB_STATUS_BYTES,
        )
        .unwrap();
        let mut concise_result = ok_text(concise);
        concise_result.meta = replay.meta.clone();
        let small_budget = machine
            .exact_decorated_result_len(aw_tools::OperationId::Batch, concise_result)
            .unwrap();
        let projected =
            bounded_cached_job_handoff(&machine, &reference, replay.clone(), Some(small_budget));
        let fits = result_fits(
            &machine,
            aw_tools::OperationId::Batch,
            &replay,
            Some(small_budget),
        );
        if fits {
            assert_eq!(projected, replay);
        } else {
            assert!(!projected.is_error.unwrap_or(false), "{projected:?}");
            let text = &projected.content[0].as_text().unwrap().text;
            assert!(text.starts_with(&format!("j:{} ", started.id)), "{text}");
            assert!(text.contains("rev="), "{text}");
            assert_eq!(crate::mcp::stored_receipt_ts(&projected), Some(first_ts));
            assert!(result_fits(
                &machine,
                aw_tools::OperationId::Batch,
                &projected,
                Some(small_budget),
            ));
        }
        let mut long_cached = ok_text(format!(
            "j:{} graph waiting rev=1 {}",
            started.id,
            "node waiting ".repeat(100)
        ));
        long_cached.meta = replay.meta.clone();
        let projected =
            bounded_cached_job_handoff(&machine, &reference, long_cached, Some(small_budget));
        assert!(!projected.is_error.unwrap_or(false), "{projected:?}");
        let projected_text = &projected.content[0].as_text().unwrap().text;
        assert!(
            projected_text.starts_with(&format!("j:{} ", started.id)),
            "{projected_text}"
        );
        assert!(projected_text.contains("rev="), "{projected_text}");
        assert_eq!(crate::mcp::stored_receipt_ts(&projected), Some(first_ts));
        assert!(crate::mcp::serialized_json_len(&projected).unwrap() <= small_budget);
        let original = match machine.jobs.claim_remote_start(&start).unwrap() {
            RemoteStartDecision::Existing(RemoteStartReceipt::JobMcp {
                reference: cached_reference,
                result,
            }) => {
                assert_eq!(cached_reference, reference);
                result
            }
            _ => panic!("duplicate Start must preserve the cached typed receipt"),
        };
        assert_eq!(
            original, replay,
            "narrow projections must never replace the canonical cached Graph receipt"
        );
        assert!(!root.join("a.txt").exists());
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "a");
    }

    #[tokio::test]
    async fn remote_cancel_replay_precedes_revision_recheck() {
        use aw_tools::remote_start::{
            REMOTE_START_VERSION, RemoteRuntimeLease, RemoteSessionAction,
            RemoteStartAuthorization, RemoteStartIntent, RemoteStartProvenance, RemoteStartSource,
        };

        let temp = tempfile::tempdir().unwrap();
        let machine =
            crate::mcp::tools::test_support::test_machine(temp.path().canonicalize().unwrap());
        let (job, writers) = running_shared_process(&machine.jobs);
        let (initial_rev, _) = machine.jobs.standalone_status(&job.id, None).unwrap();
        let reference = format!("j:{}", job.id);
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
                canonical_ref: format!("/d:d/i:i/p:p/a:0/j:{}", job.id),
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
                issuer: Some(aw_tools::remote_start::RemoteStartIssuer {
                    client_id: "aw-client".into(),
                    trust_epoch: 1,
                }),
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
                remote_start_trust_epoch: Some(1),
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

        let narrow_first = machine
            .jobs_cancel_impl(
                JobsCancelArgs {
                    reference: reference.clone(),
                    rev: Some(initial_rev),
                },
                Some(start.clone()),
                Some(1),
            )
            .await;
        assert!(
            narrow_first.is_error.unwrap_or(false),
            "the first narrow response may fail projection after cancellation, but must still cache canonical success"
        );
        let first_ts = crate::mcp::stored_receipt_ts(&narrow_first)
            .expect("narrow first cancel response preserves cached receipt timestamp");
        let (after_rev, _) = machine.jobs.standalone_status(&job.id, None).unwrap();
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
        assert!(
            !replay.is_error.unwrap_or(false),
            "wide duplicate must recover canonical cancel success rather than the first narrow projection"
        );
        assert_eq!(crate::mcp::stored_receipt_ts(&replay), Some(first_ts));
        assert_ne!(
            replay, narrow_first,
            "request-specific narrow projection must not replace the cached cancel receipt"
        );
        let exact = machine
            .exact_decorated_result_len(aw_tools::OperationId::JobsCancel, replay.clone())
            .unwrap();
        let narrow = machine
            .jobs_cancel_impl(
                JobsCancelArgs {
                    reference: format!("j:{}", job.id),
                    rev: Some(initial_rev),
                },
                Some(start.clone()),
                Some(exact - 1),
            )
            .await;
        assert!(
            narrow.is_error.unwrap_or(false),
            "a replay budget below the exact decorated receipt must fail projection without replacing cache"
        );
        assert_eq!(crate::mcp::stored_receipt_ts(&narrow), Some(first_ts));
        let still_original = machine
            .jobs_cancel_impl(
                JobsCancelArgs {
                    reference: format!("j:{}", job.id),
                    rev: Some(initial_rev),
                },
                Some(start),
                None,
            )
            .await;
        assert_eq!(
            still_original, replay,
            "narrow replay must not replace the canonical cached Control result"
        );

        // This fixture has no OS runner; terminalize it only for cleanup.
        // Do not invent a physical signal for the synthetic cancellation.
        writers.stdout.finish().unwrap();
        writers.stderr.finish().unwrap();
        machine.jobs.finish(
            &job.id,
            crate::jobs::JobCompletion::cancelled(
                job.shared_stream_summary(crate::jobs::JobStream::Stdout, false),
                job.shared_stream_summary(crate::jobs::JobStream::Stderr, false),
            ),
        );
        machine.jobs.wait_until(&job.id, None).await.unwrap();
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
    fn jobs_read_schema_and_runtime_share_the_effective_raw_page_profile() {
        for (max_output_bytes, expected_default) in
            [(8 * 1024 * 1024, 256 * 1024), (128 * 1024, 128 * 1024)]
        {
            let read = project_job_tool(
                LocalMachine::all_tools().get("jobs_read").cloned().unwrap(),
                max_output_bytes,
            );
            let profile = crate::jobs::dotlink_job_profile(max_output_bytes);
            let output_limit = &read.input_schema["properties"]["limit"];
            assert_eq!(output_limit["minimum"], 1);
            assert_eq!(output_limit["maximum"], profile.read_page_max_bytes);
            assert_eq!(output_limit["default"], expected_default);
            assert_eq!(
                profile.resolve_read_page(None).unwrap().get(),
                expected_default
            );
            assert_eq!(
                profile
                    .resolve_read_page(Some(profile.read_page_max_bytes))
                    .unwrap()
                    .get(),
                profile.read_page_max_bytes
            );
            assert!(profile.resolve_read_page(Some(0)).is_err());
            assert!(
                profile
                    .resolve_read_page(Some(profile.read_page_max_bytes + 1))
                    .is_err()
            );
            assert!(
                !read.input_schema["properties"]
                    .as_object()
                    .unwrap()
                    .contains_key("filter")
            );
        }
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
