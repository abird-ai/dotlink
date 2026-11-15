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

use super::{LocalMachine, graph, ok_text, resolve_capped, tool_error};
use crate::jobs::{
    JobHandoff, JobOutputStream, JobWait, ProcessInput, RemoteStartDecision, RemoteStartReceipt,
};

const MAX_JOB_LIST_LIMIT: usize = 256;
const MAX_WAIT_MS: u64 = 30_000;

/// Host-verified transport response windows may only narrow a caller's wait.
/// This never changes Process/Graph execution or the Job's retained identity.
fn bounded_wait_ms(requested: u64, transport_remaining: Option<u64>) -> u64 {
    transport_remaining.map_or(requested, |remaining| requested.min(remaining))
}

#[cfg(test)]
#[test]
fn wait_budget_only_narrows_by_authenticated_transport_remaining() {
    assert_eq!(bounded_wait_ms(30_000, None), 30_000);
    assert_eq!(bounded_wait_ms(30_000, Some(7_000)), 7_000);
    assert_eq!(bounded_wait_ms(30_000, Some(0)), 0);
    assert_eq!(bounded_wait_ms(0, Some(7_000)), 0);
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

pub(crate) fn with_job_limits(tool: &mut Tool, _max_secs: u64, max_output_bytes: usize) {
    let name = tool.name.as_ref();
    if !matches!(name, "jobs_read" | "jobs_list") {
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
                "Maximum retained raw bytes for this page. Defaults to the profile output ceiling, must be at least {} and is capped at {max_output_bytes}.",
                crate::mcp::MIN_BYTE_LIMIT,
            )),
        );
        property.insert(
            "minimum".to_owned(),
            serde_json::Value::from(crate::mcp::MIN_BYTE_LIMIT),
        );
        property.insert(
            "maximum".to_owned(),
            serde_json::Value::from(max_output_bytes),
        );
    }
    if name == "jobs_list"
        && let Some(property) = properties
            .get_mut("limit")
            .and_then(serde_json::Value::as_object_mut)
    {
        property.insert("minimum".to_owned(), serde_json::Value::from(1));
        property.insert(
            "maximum".to_owned(),
            serde_json::Value::from(MAX_JOB_LIST_LIMIT),
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
    offset: u64,
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
                    Ok((_, status)) => ok_text(status),
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
                                Ok((_, status)) => ok_text(status),
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
                            Ok((_, status)) => ok_text(status),
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
                    .map(|(_, status)| ok_text(status))
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
        let result_budget = self.request_result_budget(&context);
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
            let status = match self.jobs.standalone_status(&started.id, None) {
                Ok((_, status)) => status,
                Err(error) => {
                    guard.mark_unknown();
                    return tool_error(error);
                }
            };
            let result = ok_text(status);
            if let Err(error) = guard.mark_ready(RemoteStartReceipt::Mcp(result.clone())) {
                return tool_error(error);
            }
            return result;
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
                Ok((_, status)) => ok_text(status),
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
        if !(1..=MAX_JOB_LIST_LIMIT).contains(&limit) {
            return tool_error(format!(
                "jobs_list limit must be within 1..={MAX_JOB_LIST_LIMIT}"
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
        description = "Observe one explicit standalone Process/Graph Job or Graph node. rev is a conditional observation token; wait_ms may wait for a different revision without changing execution timeout.",
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
        if wait_ms > MAX_WAIT_MS {
            return tool_error(format!("jobs_get wait_ms exceeds {MAX_WAIT_MS}"));
        }
        let wait_ms = bounded_wait_ms(wait_ms, self.request_sync_budget_millis(&context));
        match self
            .jobs
            .standalone_get(&reference.id, reference.node, args.rev, wait_ms)
            .await
        {
            Ok(status) => ok_text(status),
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
        if args.wait_ms.unwrap_or(0) > MAX_WAIT_MS {
            return tool_error(format!("jobs_read wait_ms exceeds {MAX_WAIT_MS}"));
        }
        let reference = match parse_ref(&args.reference) {
            Ok(reference) => reference,
            Err(error) => return tool_error(error),
        };
        let ceiling = resolve_capped(args.limit, self.config.max_output_bytes);
        let response_ceiling = self.request_result_budget(&context).unwrap_or(ceiling);
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
        description = "Explicitly acknowledge one exact retained Job ref/view through a raw byte offset after safe receipt. ACK is monotonic/idempotent and is never implied by jobs_read.",
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
        let reference = match parse_ref(&args.reference) {
            Ok(reference) => reference,
            Err(error) => return tool_error(error),
        };
        let remote_start = context
            .extensions
            .get::<crate::remote_start::ValidatedRemoteStart>()
            .cloned();
        let guard = if let Some(start) = remote_start.as_ref() {
            match self.jobs.claim_remote_start(start) {
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(result))) => {
                    return result;
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
        let outcome = match self.jobs.acknowledge_node_output(
            &reference.id,
            reference.node,
            args.view.map(Into::into),
            args.offset,
        ) {
            Ok(_) => self
                .jobs
                .standalone_status(&reference.id, reference.node)
                .map(|(_, status)| status)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        match outcome {
            Ok(text) => {
                let result = ok_text(text);
                if let Some(guard) = guard
                    && let Err(error) = guard.mark_ready(RemoteStartReceipt::Mcp(result.clone()))
                {
                    return tool_error(error);
                }
                result
            }
            Err(text) => {
                let result = tool_error(text);
                if let Some(guard) = guard
                    && let Err(error) = guard.mark_ready(RemoteStartReceipt::Mcp(result.clone()))
                {
                    return tool_error(error);
                }
                result
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
        let remote_start = context
            .extensions
            .get::<crate::remote_start::ValidatedRemoteStart>()
            .cloned();
        let guard = if let Some(start) = remote_start.as_ref() {
            match self.jobs.claim_remote_start(start) {
                Ok(RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(result))) => {
                    return result;
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
        match self
            .send_process_input_checked(&reference.id, reference.node, args.rev, input)
            .await
        {
            Ok(revision) => {
                let text = format!(
                    "{} rev={revision} ok",
                    crate::jobs::JobManager::standalone_ref(&reference.id, reference.node)
                );
                let result = ok_text(text);
                if let Some(guard) = guard
                    && let Err(error) = guard.mark_ready(RemoteStartReceipt::Mcp(result.clone()))
                {
                    return tool_error(error);
                }
                result
            }
            Err(error) => {
                let text = error.to_string();
                let result = tool_error(text);
                if let Some(guard) = guard
                    && let Err(replay_error) =
                        guard.mark_ready(RemoteStartReceipt::Mcp(result.clone()))
                {
                    return tool_error(replay_error);
                }
                result
            }
        }
    }

    async fn jobs_cancel_impl(
        &self,
        args: JobsCancelArgs,
        remote_start: Option<crate::remote_start::ValidatedRemoteStart>,
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
                    return result;
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
                    if let Some(guard) = guard
                        && let Err(replay_error) =
                            guard.mark_ready(RemoteStartReceipt::Mcp(result.clone()))
                    {
                        return tool_error(replay_error);
                    }
                    return result;
                }
            };
            if expected != current {
                let result = tool_error(format!(
                    "stale Job revision: expected {expected}, current {current}"
                ));
                if let Some(guard) = guard
                    && let Err(replay_error) =
                        guard.mark_ready(RemoteStartReceipt::Mcp(result.clone()))
                {
                    return tool_error(replay_error);
                }
                return result;
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
                let result = ok_text(text);
                if let Some(guard) = guard
                    && let Err(error) = guard.mark_ready(RemoteStartReceipt::Mcp(result.clone()))
                {
                    return tool_error(error);
                }
                result
            }
            Err(text) => {
                let result = tool_error(text);
                if let Some(guard) = guard
                    && let Err(error) = guard.mark_ready(RemoteStartReceipt::Mcp(result.clone()))
                {
                    return tool_error(error);
                }
                result
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
        self.jobs_cancel_impl(args, remote_start).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut list = LocalMachine::all_tools().get("jobs_list").cloned().unwrap();
        with_job_limits(&mut list, 3600, 8 * 1024 * 1024);
        let limit = &list.input_schema["properties"]["limit"];
        assert_eq!(limit["minimum"], 1);
        assert_eq!(limit["maximum"], MAX_JOB_LIST_LIMIT);
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
        let first = ok_text(status);
        guard
            .mark_ready(RemoteStartReceipt::Mcp(first.clone()))
            .unwrap();
        machine.jobs.wait_until(&started.id, None).await.unwrap();
        assert!(!root.join("a.txt").exists());
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "a");

        let replay = match machine.jobs.claim_remote_start(&start).unwrap() {
            RemoteStartDecision::Existing(RemoteStartReceipt::Mcp(result)) => result,
            _ => panic!("duplicate Graph Start must replay the first receipt"),
        };
        assert_eq!(replay, first);
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
                Some(start),
            )
            .await;
        assert_eq!(
            replay, first,
            "duplicate remote Start must replay the original cancel receipt"
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
        let mut read = LocalMachine::all_tools().get("jobs_read").cloned().unwrap();
        with_job_limits(&mut read, 3600, 8 * 1024 * 1024);
        let output_limit = &read.input_schema["properties"]["limit"];
        assert_eq!(output_limit["maximum"], 8 * 1024 * 1024);
        assert!(
            !read.input_schema["properties"]
                .as_object()
                .unwrap()
                .contains_key("filter")
        );
    }
}
