//! Thin rmcp projection for the canonical `aw-tools` filesystem surface.
//! Filesystem semantics stay in aw-tools + the prepared CapabilitySet.

use aw_tools::{
    filesystem::{
        access::Action,
        direct::{self, Body},
        request,
        schema::{SchemaCaps, Tool as CoreTool},
    },
    media::MediaKind,
    operation::OperationId,
    toolset::{ReceiptProfile, Toolset},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rmcp::{
    ErrorData as McpError,
    handler::server::router::tool::{ToolRoute, ToolRouter},
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ResourceContents,
        Tool, ToolAnnotations,
    },
    service::{RequestContext, RoleServer},
};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::{collections::BTreeSet, sync::Arc};

use anyhow::{Result as AnyResult, anyhow};

use super::LocalMachine;

#[cfg(unix)]
type BackendError = aw_tools_fs_unix::scoped_fs::ScopedError;
#[cfg(windows)]
type BackendError = aw_tools_fs_win::scoped_fs::ScopedError;

pub(crate) const NAMES: [&str; 13] = [
    "ls", "find", "grep", "read", "write", "edit", "patch", "cp", "mv", "rm", "bread", "bwrite",
    "bedit",
];

const PROJECTION_RESERVE_BYTES: usize = 512;

pub(crate) fn is_native(name: &str) -> bool {
    NAMES.contains(&name)
}

pub(crate) fn core_tool(name: &str) -> Option<CoreTool> {
    CoreTool::ALL.into_iter().find(|tool| tool.name() == name)
}

pub(crate) fn graph_core_tool(name: &str) -> Option<CoreTool> {
    CoreTool::GRAPH_NATIVE
        .into_iter()
        .find(|tool| tool.name() == name)
}

pub(crate) fn caps(max_read: usize, max_write: usize) -> Result<SchemaCaps, &'static str> {
    let binary_data_bytes = max_write
        .checked_mul(2)
        .ok_or("dotlink write ceiling overflows the encoded binary schema bound")?;
    SchemaCaps {
        text_write_bytes: max_write,
        patch_bytes: max_write,
        binary_data_bytes,
        binary_edit_ranges: aw_tools::MAX_BINARY_EDIT_RANGES,
        binary_read_bytes: max_read,
        // Dotlink's profile already bounds the raw read. Encoded pages are
        // additionally paged by the actual result/transport ceiling at call time.
        binary_encoded_read_bytes: max_read,
        read_line_limit: 65_536,
        grep_context_max: aw_tools::filesystem::schema::MAX_GREP_CONTEXT,
        grep_context_default: 0,
        parallel_reads: false,
        typed_media: true,
    }
    .validate()
}

/// Dotlink-selected shared catalog profile. Receipt timestamps/checkpoint
/// guidance stay disabled until dotlink has a typed shared receipt projection;
/// never decorate existing free-form MCP results by parsing text.
pub(crate) fn toolset(caps: SchemaCaps) -> Toolset {
    Toolset::builder()
        .filesystem(caps)
        .jobs(crate::jobs::DOTLINK_JOB_PROFILE)
        .receipts(ReceiptProfile::default())
        .build()
        .expect("dotlink shared Toolset profile is valid")
}

fn annotation(action: Action, title: String) -> ToolAnnotations {
    let read = action == Action::Read;
    ToolAnnotations::from_raw(
        Some(title),
        Some(read),
        Some(matches!(
            action,
            Action::Edit | Action::Move | Action::Remove
        )),
        Some(matches!(
            action,
            Action::Read | Action::Write | Action::Edit
        )),
        Some(false),
    )
}

pub(crate) fn projected_tool(core: CoreTool, toolset: &Toolset) -> Tool {
    let descriptor = toolset
        .filesystem_descriptor(core)
        .expect("dotlink canonical filesystem tool is enabled");
    let action = aw_tools::filesystem::schema::spec(core, toolset.schema_caps()).action;
    let parameters = descriptor
        .parameters
        .as_object()
        .cloned()
        .expect("canonical tool schema is an object");
    Tool::new(
        descriptor.name,
        descriptor.description,
        Arc::new(parameters),
    )
    .with_title(descriptor.title.clone())
    .with_annotations(annotation(action, descriptor.title))
}

/// Apply shared metadata to host-owned non-filesystem operations after the
/// host has projected its dynamic schema limits. This never changes routing,
/// authority or execution; host descriptions and schema shape are preserved.
pub(crate) fn apply_operation_metadata(tool: &mut Tool, toolset: &Toolset) {
    let Some(operation) = OperationId::from_model_name(tool.name.as_ref()) else {
        return;
    };
    if matches!(operation, OperationId::Fs(_)) {
        return;
    }
    let descriptor = toolset
        .describe_operation(
            operation,
            serde_json::Value::Object((*tool.input_schema).clone()),
            tool.description.as_deref().unwrap_or(""),
        )
        .expect("dotlink operation schema is compatible with the shared Toolset descriptor");
    let parameters = descriptor
        .parameters
        .as_object()
        .cloned()
        .expect("shared operation descriptor parameters are an object");
    tool.title = Some(descriptor.title.clone());
    tool.description = Some(descriptor.description.into());
    tool.input_schema = Arc::new(parameters);
    if let Some(annotations) = tool.annotations.as_mut() {
        annotations.title = Some(descriptor.title);
    }
}

fn fallback_route(tool: Tool) -> ToolRoute<LocalMachine> {
    ToolRoute::new_dyn(tool, |_context| {
        Box::pin(async {
            Err(McpError::internal_error(
                "canonical filesystem dispatch bypassed shared-native call path",
                None,
            ))
        })
    })
}

pub(crate) fn canonicalize_router(router: &mut ToolRouter<LocalMachine>, toolset: &Toolset) {
    let mut seen = BTreeSet::new();
    for core in CoreTool::ALL {
        let projected = projected_tool(core, toolset);
        let name = core.name();
        seen.insert(name);
        if let Some(route) = router.map.get_mut(name) {
            // Preserve no legacy input/output schema. Annotations are canonical
            // hints only; all authorization remains in dotlink policy/backend.
            route.attr = projected;
        } else {
            router.add_route(fallback_route(projected));
        }
    }
    debug_assert_eq!(seen.len(), NAMES.len());
}

fn transport_text_ceiling(result_budget: Option<usize>) -> Option<usize> {
    result_budget.map(|budget| {
        budget
            .saturating_sub(crate::mcp::serialized_text_tool_result_fixed_len())
            .saturating_sub(PROJECTION_RESERVE_BYTES)
            / 2
    })
}

fn binary_render_ceiling(max_read: usize, result_budget: Option<usize>) -> usize {
    let semantic = max_read
        .saturating_mul(2)
        .saturating_add(4096)
        .min(aw_tools::filesystem::MAX_RENDER_BYTES);
    match transport_text_ceiling(result_budget) {
        Some(transport) => semantic.min(transport),
        None => semantic,
    }
}

fn typed_media_raw_ceiling(max_read: usize, result_budget: Option<usize>) -> usize {
    let Some(budget) = result_budget else {
        return max_read;
    };
    // Typed MCP content base64-expands raw bytes. Keep the same conservative
    // response-envelope reserve used by dotlink's legacy binary transport.
    let payload = budget.saturating_sub(crate::limits::OPENAI_TUNNEL_BINARY_RESPONSE_RESERVE_BYTES);
    ((payload / 4) * 3).min(max_read)
}

fn bread_is_mcp(arguments: &Value) -> bool {
    match arguments.get("format").and_then(Value::as_str) {
        None | Some("mcp") => true,
        Some(_) => false,
    }
}

fn request_limits(
    machine: &LocalMachine,
    tool: CoreTool,
    arguments: &Value,
    result_budget: Option<usize>,
) -> Result<request::Limits, CallToolResult> {
    let mut schema = machine.config.shared_caps;
    if tool == CoreTool::Bread && bread_is_mcp(arguments) {
        let safe = typed_media_raw_ceiling(machine.config.max_read_bytes, result_budget);
        if safe == 0 {
            return Err(tool_error(
                "fail typed binary result cannot fit the current transport response budget; use base64/hex byte windows",
            ));
        }
        schema.binary_read_bytes = safe;
        schema.binary_encoded_read_bytes = schema.binary_encoded_read_bytes.min(safe);
    }
    Ok(request::Limits {
        schema,
        edit_text_bytes: machine.config.max_write_bytes,
        binary_decoded_bytes: machine.config.max_write_bytes,
    })
}

fn direct_limits(
    machine: &LocalMachine,
    request_limits: request::Limits,
    request: &request::Request,
    result_budget: Option<usize>,
) -> Result<direct::Limits, CallToolResult> {
    let transport_text = transport_text_ceiling(result_budget);
    let result_bytes = transport_text
        .map(|transport| machine.config.max_output_bytes.min(transport))
        .unwrap_or(machine.config.max_output_bytes);
    if result_bytes < 512 {
        return Err(tool_error(
            "fail canonical filesystem result cannot fit the current transport response budget",
        ));
    }

    let read_data_bytes = match result_budget {
        None => machine.config.max_read_bytes,
        Some(_) => {
            let transport = transport_text.unwrap_or(0);
            let cap = match request.kind() {
                request::RequestKind::Read(read) if read.numbered => {
                    transport.saturating_sub(PROJECTION_RESERVE_BYTES)
                }
                request::RequestKind::Read(_) => transport
                    .saturating_sub(PROJECTION_RESERVE_BYTES)
                    .saturating_div(2),
                _ => machine.config.max_read_bytes,
            };
            machine.config.max_read_bytes.min(cap)
        }
    };
    if read_data_bytes == 0 {
        return Err(tool_error(
            "fail canonical read cannot fit the current transport response budget",
        ));
    }

    let binary_result_bytes = binary_render_ceiling(machine.config.max_read_bytes, result_budget);
    if binary_result_bytes < 512 {
        return Err(tool_error(
            "fail encoded binary result cannot fit the current transport response budget",
        ));
    }

    Ok(direct::Limits {
        request: request_limits,
        result_bytes,
        read_data_bytes,
        binary_result_bytes,
        text_read_bytes: machine.config.max_read_bytes,
        text_write_bytes: machine.config.max_write_bytes,
        // Dotlink's profile ceiling bounds both the binary source and result;
        // the shared engine additionally enforces its fixed 64 MiB safety max.
        binary_file_bytes: machine
            .config
            .max_write_bytes
            .min(aw_tools::MAX_READ_WINDOW_BYTES),
        binary_default_window: request_limits.schema.binary_read_bytes,
        grep: aw_tools::search::GrepLimits::default(),
    })
}

#[derive(Clone)]
pub(crate) struct PreparedNativeGraphNode {
    pub(crate) tool: CoreTool,
    pub(crate) request: request::Request,
    pub(crate) backend: Arc<super::super::shared_fs::SharedBackend>,
    pub(crate) limits: direct::Limits,
    pub(crate) footprint: aw_tools::filesystem::footprint::Footprint,
}

pub(crate) struct NativeGraphOutcome {
    pub(crate) text: String,
    pub(crate) status: aw_tools::filesystem::batch::Status,
}

impl PreparedNativeGraphNode {
    pub(crate) fn result_bytes(&self) -> usize {
        self.limits.result_bytes
    }

    pub(crate) fn footprint_is_current(&self) -> bool {
        self.request
            .footprint(self.backend.as_ref())
            .is_ok_and(|current| current == self.footprint)
    }

    pub(crate) fn execute_graph(
        &self,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> NativeGraphOutcome {
        match self.execute(cancellation) {
            Ok(output) => {
                let status = direct::graph_status(self.tool, &output);
                let text = direct::graph_text(&output).unwrap_or_else(|| {
                    "unknown native Graph result is not text-projectable".into()
                });
                NativeGraphOutcome { text, status }
            }
            Err(error) => {
                let status = direct::graph_error_status(&error);
                let text = match &error {
                    direct::Error::Invalid(reason) => format!("fail {reason}"),
                    direct::Error::Limit(reason) => format!("fail {reason}"),
                    direct::Error::Cancelled => "fail cancelled".into(),
                    direct::Error::Backend(_) => {
                        "fail file unavailable under the granted filesystem".into()
                    }
                    direct::Error::UnknownCommit(_) => {
                        "unknown filesystem commit may be visible; inspect before retrying".into()
                    }
                    direct::Error::PartialCommit(_) => {
                        "mixed filesystem mutation partially committed; inspect before retrying"
                            .into()
                    }
                };
                NativeGraphOutcome { text, status }
            }
        }
    }

    pub(crate) fn execute(
        &self,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<direct::Output, direct::Error<BackendError>> {
        direct::execute(
            self.backend.as_ref(),
            self.request.clone(),
            self.limits,
            &|| cancellation.is_cancelled(),
        )
    }
}

fn semantic_request_limits(machine: &LocalMachine) -> AnyResult<request::Limits> {
    request::Limits {
        schema: machine.config.shared_caps,
        edit_text_bytes: machine.config.max_write_bytes,
        binary_decoded_bytes: machine.config.max_write_bytes,
    }
    .validate()
    .map_err(|error| anyhow!("invalid graph request ceilings: {error}"))
}

fn graph_direct_limits(
    machine: &LocalMachine,
    request_limits: request::Limits,
) -> AnyResult<direct::Limits> {
    direct::Limits {
        request: request_limits,
        result_bytes: machine.config.max_output_bytes,
        read_data_bytes: machine.config.max_read_bytes,
        binary_result_bytes: machine.config.max_output_bytes,
        text_read_bytes: machine.config.max_read_bytes,
        text_write_bytes: machine.config.max_write_bytes,
        binary_file_bytes: machine
            .config
            .max_write_bytes
            .min(aw_tools::MAX_READ_WINDOW_BYTES),
        binary_default_window: request_limits.schema.binary_read_bytes,
        grep: aw_tools::search::GrepLimits::default(),
    }
    .validate()
    .map_err(|error| anyhow!("invalid graph direct-execution ceilings: {error:?}"))
}

pub(crate) fn validate_graph_arguments(
    machine: &LocalMachine,
    name: &str,
    arguments: &Value,
) -> bool {
    if !machine.has_tool_for_policy(name) {
        return false;
    }
    let Some(tool) = graph_core_tool(name) else {
        return false;
    };
    let Ok(limits) = semantic_request_limits(machine) else {
        return false;
    };
    request::parse(tool, arguments, limits).is_ok()
}

pub(crate) fn prepare_graph_node(
    machine: &LocalMachine,
    name: &str,
    arguments: &Value,
) -> AnyResult<PreparedNativeGraphNode> {
    if !machine.has_tool_for_policy(name) {
        return Err(anyhow!(
            "native Graph tool is not available under current policy"
        ));
    }
    let tool = graph_core_tool(name).ok_or_else(|| anyhow!("tool is not Graph-native"))?;
    let request_limits = semantic_request_limits(machine)?;
    let request = request::parse(tool, arguments, request_limits)
        .map_err(|error| anyhow!("invalid canonical Graph node: {error}"))?;
    let backend = machine
        .shared_fs
        .backend_for_request(&request)
        .map_err(|error| anyhow!("Graph filesystem authority preparation failed: {error}"))?;
    let footprint = request
        .footprint(backend.as_ref())
        .map_err(|error| anyhow!("Graph filesystem footprint preparation failed: {error:?}"))?;
    let limits = graph_direct_limits(machine, request_limits)?;
    Ok(PreparedNativeGraphNode {
        tool,
        request,
        backend,
        limits,
        footprint,
    })
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn backend_error(error: BackendError) -> CallToolResult {
    #[cfg(unix)]
    use aw_tools_fs_unix::scoped_fs::ScopedError;
    #[cfg(windows)]
    use aw_tools_fs_win::scoped_fs::ScopedError;

    match error {
        ScopedError::Denied(reason) | ScopedError::Limit(reason) => {
            tool_error(format!("fail {reason}"))
        }
        ScopedError::Cancelled => tool_error("fail cancelled"),
        ScopedError::Io(_) => tool_error("fail file unavailable under the granted filesystem"),
    }
}

fn direct_error(error: direct::Error<BackendError>) -> Result<CallToolResult, McpError> {
    match error {
        direct::Error::Invalid(reason) => Ok(tool_error(format!("fail {reason}"))),
        direct::Error::Limit(reason) => Ok(tool_error(format!("fail {reason}"))),
        direct::Error::Cancelled => Ok(tool_error("fail cancelled")),
        direct::Error::Backend(error) => Ok(backend_error(error)),
        direct::Error::UnknownCommit(_) => Err(McpError::internal_error(
            "unknown filesystem commit; do not retry automatically; inspect the workspace first",
            None,
        )),
        direct::Error::PartialCommit(_) => Err(McpError::internal_error(
            "mixed filesystem mutation partially committed; do not retry automatically; inspect the workspace first",
            None,
        )),
    }
}

fn resource_uri(path: &str) -> Result<String, CallToolResult> {
    let mut uri = url::Url::parse("abird://file/")
        .map_err(|_| tool_error("fail invalid binary resource URI base"))?;
    uri.path_segments_mut()
        .map_err(|_| tool_error("fail invalid binary resource URI path"))?
        .pop_if_empty()
        .extend(path.split('/'));
    Ok(uri.into())
}

fn project(output: direct::Output) -> CallToolResult {
    let is_error = output.is_error;
    match output.body {
        Body::Text(content) => {
            if is_error {
                tool_error(content)
            } else {
                CallToolResult::success(vec![ContentBlock::text(content)])
            }
        }
        Body::Read {
            data,
            status,
            next: _,
            numbered,
        } => {
            let projected =
                aw_tools::filesystem::direct::project_read_parts(data, status.as_deref(), numbered);
            let mut blocks = vec![ContentBlock::text(projected.data)];
            if let Some(status) = projected.status {
                blocks.push(ContentBlock::text(status));
            }
            if is_error {
                CallToolResult::error(blocks)
            } else {
                CallToolResult::success(blocks)
            }
        }
        Body::Media { path, packet } => {
            if !packet.verify() {
                return tool_error("fail binary media bytes changed before MCP projection");
            }
            let descriptor = packet.descriptor();
            let encoded = BASE64.encode(packet.bytes());
            let block = match descriptor.kind {
                MediaKind::Image => ContentBlock::image(encoded, descriptor.mime),
                MediaKind::Audio => ContentBlock::audio(encoded, descriptor.mime),
                MediaKind::Resource => {
                    let uri = match resource_uri(&path) {
                        Ok(uri) => uri,
                        Err(error) => return error,
                    };
                    ContentBlock::resource(
                        ResourceContents::blob(encoded, uri).with_mime_type(descriptor.mime),
                    )
                }
            };
            CallToolResult::success(vec![block])
        }
    }
}

/// Execute one canonical native filesystem call through the shared engine.
/// Dotlink owns authority, transport budgets and MCP projection only.
fn execute_projected(
    machine: &LocalMachine,
    name: &str,
    arguments: Value,
    result_budget: Option<usize>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<CallToolResult, McpError> {
    if !machine.has_tool_for_policy(name) {
        return Err(McpError::invalid_params("tool not found", None));
    }
    let core = core_tool(name).ok_or_else(|| McpError::invalid_params("tool not found", None))?;
    let request_limits = match request_limits(machine, core, &arguments, result_budget) {
        Ok(limits) => limits,
        Err(error) => return Ok(error),
    };
    let parsed = match request::parse(core, &arguments, request_limits) {
        Ok(request) => request,
        Err(error) => return Ok(tool_error(format!("fail {error}"))),
    };
    let limits = match direct_limits(machine, request_limits, &parsed, result_budget) {
        Ok(limits) => limits,
        Err(error) => return Ok(error),
    };
    let backend = match machine.shared_fs.backend_for_request(&parsed) {
        Ok(backend) => backend,
        Err(error) => {
            return Ok(tool_error(format!(
                "fail shared filesystem authority preparation failed: {error}"
            )));
        }
    };
    let result = crate::blocking::run(|| {
        direct::execute(&*backend, parsed, limits, &|| cancellation.is_cancelled())
    });
    match result {
        Ok(output) => Ok(project(output)),
        Err(error) => direct_error(error),
    }
}

fn enforce_result_budget(
    result: CallToolResult,
    result_budget: Option<usize>,
) -> Result<CallToolResult, McpError> {
    if let Some(limit) = result_budget {
        let actual = crate::mcp::serialized_json_len(&result).unwrap_or(usize::MAX);
        if actual > limit {
            tracing::error!(
                actual,
                limit,
                "canonical filesystem response exceeded its precomputed transport budget"
            );
            return Err(McpError::internal_error(
                "canonical filesystem response exceeded its transport budget",
                None,
            ));
        }
    }
    Ok(result)
}

pub(crate) fn execute(
    machine: &LocalMachine,
    name: &str,
    arguments: Value,
    result_budget: Option<usize>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<CallToolResult, McpError> {
    let result = execute_projected(machine, name, arguments, result_budget, cancellation)?;
    enforce_result_budget(result, result_budget)
}

fn execute_with_remote_start(
    machine: &LocalMachine,
    name: &str,
    arguments: Value,
    result_budget: Option<usize>,
    cancellation: &tokio_util::sync::CancellationToken,
    start: Option<crate::remote_start::ValidatedRemoteStart>,
) -> Result<CallToolResult, McpError> {
    let Some(start) = start else {
        return execute(machine, name, arguments, result_budget, cancellation);
    };

    let guard = match machine
        .jobs
        .claim_remote_start(&start)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?
    {
        crate::jobs::RemoteStartDecision::Existing(crate::jobs::RemoteStartReceipt::Mcp(
            result,
        )) => return Ok(result),
        crate::jobs::RemoteStartDecision::Unknown => {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "remote Start outcome is unknown; inspect the original filesystem attempt before any new invocation",
            )]));
        }
        crate::jobs::RemoteStartDecision::New(guard) => guard,
    };

    match execute(machine, name, arguments, result_budget, cancellation) {
        Ok(result) => {
            guard
                .mark_ready(crate::jobs::RemoteStartReceipt::Mcp(result.clone()))
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            Ok(result)
        }
        Err(error) => {
            guard.mark_unknown();
            Err(error)
        }
    }
}

pub(crate) async fn call(
    machine: &LocalMachine,
    request: CallToolRequestParams,
    context: RequestContext<RoleServer>,
    result_budget: Option<usize>,
) -> Result<CallToolResponse, McpError> {
    let name = request.name.to_string();
    let arguments = Value::Object(request.arguments.unwrap_or_default());
    let start = context
        .extensions
        .get::<crate::remote_start::ValidatedRemoteStart>()
        .cloned();
    execute_with_remote_start(machine, &name, arguments, result_budget, &context.ct, start)
        .map(CallToolResponse::Complete)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_native_preparation_is_transport_independent_and_uses_shared_footprints() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(temp.path().join("b.txt"), "b\n").unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with(
            temp.path().canonicalize().unwrap(),
            |_| {},
        );
        let a = prepare_graph_node(&machine, "read", &json!({"path":"a.txt","numbered":false}))
            .unwrap();
        let output = a
            .execute(&tokio_util::sync::CancellationToken::new())
            .unwrap();
        let direct::Body::Read {
            data,
            status,
            next,
            numbered,
        } = output.body
        else {
            panic!("prepared read node must return canonical structured read output")
        };
        assert_eq!(data, "a\n");
        assert!(status.is_none());
        assert!(next.is_none());
        assert!(!numbered);
        assert!(matches!(a.request.kind(), request::RequestKind::Read(_)));
        let _ = a.backend.identities().unwrap();
        let b = prepare_graph_node(&machine, "write", &json!({"path":"b.txt","content":"new"}))
            .unwrap();
        assert_eq!(a.result_bytes(), machine.config.max_output_bytes);
        assert!(!a.footprint.conflicts(&b.footprint));
        assert_eq!(a.tool, CoreTool::Read);
        assert_eq!(b.tool, CoreTool::Write);

        let same = prepare_graph_node(&machine, "write", &json!({"path":"a.txt","content":"new"}))
            .unwrap();
        assert!(a.footprint.conflicts(&same.footprint));
    }

    #[test]
    fn direct_only_binary_read_is_not_graph_native() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.bin"), b"a").unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with(
            temp.path().canonicalize().unwrap(),
            |_| {},
        );
        assert!(
            prepare_graph_node(
                &machine,
                "bread",
                &json!({"path":"a.bin","format":"base64"}),
            )
            .is_err()
        );
    }

    #[test]
    fn canonical_projection_fails_closed_if_serialized_result_exceeds_budget() {
        let result = CallToolResult::success(vec![ContentBlock::text("x".repeat(1024))]);
        let actual = crate::mcp::serialized_json_len(&result).unwrap();
        assert!(actual > 128);
        let error = enforce_result_budget(result, Some(128)).unwrap_err();
        assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(error.message.contains("transport budget"));

        let small = CallToolResult::success(vec![ContentBlock::text("ok")]);
        let exact = crate::mcp::serialized_json_len(&small).unwrap();
        assert!(enforce_result_budget(small, Some(exact)).is_ok());
    }

    #[test]
    fn uncertain_or_partial_commit_is_an_mcp_execution_error() {
        #[cfg(unix)]
        use aw_tools_fs_unix::scoped_fs::ScopedError;
        #[cfg(windows)]
        use aw_tools_fs_win::scoped_fs::ScopedError;

        for error in [
            direct::Error::UnknownCommit(ScopedError::Denied("fixture")),
            direct::Error::PartialCommit(Some(ScopedError::Denied("fixture"))),
        ] {
            let error = match direct_error(error) {
                Err(error) => error,
                Ok(_) => panic!("uncertain publication must not become an ordinary tool failure"),
            };
            assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
            assert!(
                error.message.contains("do not retry automatically"),
                "{}",
                error.message
            );
        }

        let safe = direct_error(direct::Error::Backend(ScopedError::Denied("fixture")))
            .expect("known pre-effect backend failure stays a tool result");
        assert!(safe.is_error.unwrap_or(false));
    }

    #[test]
    fn read_projection_preserves_shared_fault_detail() {
        let output = direct::Output {
            body: Body::Read {
                data: "prefix".into(),
                status: Some("mixed ok=prefix fail=io more line=2 byte=7\n".into()),
                next: Some((2, 7)),
                numbered: false,
            },
            is_error: true,
            truncated: true,
        };
        let projected = project(output);
        assert!(projected.is_error.unwrap_or(false));
        assert_eq!(projected.content.len(), 2);
        assert_eq!(
            projected.content[0].as_text().unwrap().text.as_str(),
            "prefix"
        );
        assert_eq!(
            projected.content[1].as_text().unwrap().text.as_str(),
            "mixed ok=prefix fail=io more line=2 byte=7\n"
        );
    }

    #[test]
    fn binary_file_limit_never_exceeds_dotlink_profile_write_ceiling() {
        let temp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with(
            temp.path().canonicalize().unwrap(),
            |config| config.max_write_bytes = 1024,
        );
        let request_limits = request_limits(
            &machine,
            CoreTool::Bedit,
            &json!({"path":"data.bin","edits":[{"offset":0,"data":"00","encoding":"hex"}]}),
            None,
        )
        .unwrap();
        let parsed = request::parse(
            CoreTool::Bedit,
            &json!({"path":"data.bin","edits":[{"offset":0,"data":"00","encoding":"hex"}]}),
            request_limits,
        )
        .unwrap();
        let limits = direct_limits(&machine, request_limits, &parsed, None).unwrap();
        assert_eq!(limits.binary_file_bytes, 1024);
    }

    #[test]
    fn projected_default_schemas_and_metadata_are_exact_shared_toolset() {
        let caps = SchemaCaps::default();
        let toolset = toolset(caps);
        for core in CoreTool::ALL {
            let projected = projected_tool(core, &toolset);
            let canonical = toolset.filesystem_descriptor(core).unwrap();
            assert_eq!(
                serde_json::Value::Object(projected.input_schema.as_ref().clone()),
                canonical.parameters,
                "{}",
                core.name()
            );
            assert_eq!(projected.title.as_deref(), Some(canonical.title.as_str()));
            assert_eq!(
                projected.description.as_deref(),
                Some(canonical.description.as_str())
            );
            assert_eq!(
                projected
                    .annotations
                    .as_ref()
                    .and_then(|a| a.title.as_deref()),
                Some(canonical.title.as_str())
            );
        }
        assert!(!toolset.receipt_profile().unix_ts);
        assert_eq!(toolset.receipt_profile().checkpoint_minutes, None);
        assert_eq!(toolset.jobs_profile(), crate::jobs::DOTLINK_JOB_PROFILE);
    }

    #[test]
    fn dotlink_caps_do_not_inherit_aw_encoded_binary_default() {
        let caps = caps(1024 * 1024, 1024 * 1024).unwrap();
        assert_eq!(caps.binary_read_bytes, 1024 * 1024);
        assert_eq!(caps.binary_encoded_read_bytes, 1024 * 1024);
        assert_eq!(caps.binary_data_bytes, 2 * 1024 * 1024);
    }

    #[test]
    fn every_shared_conformance_request_parses_under_dotlink_profile_caps() {
        let limits = request::Limits {
            schema: caps(64 * 1024, 64 * 1024).unwrap(),
            edit_text_bytes: 64 * 1024,
            binary_decoded_bytes: 64 * 1024,
        };
        for vector in aw_tools::conformance::requests() {
            let tool = core_tool(&vector.tool).expect("fixture tool is canonical");
            request::parse(tool, &vector.arguments, limits)
                .unwrap_or_else(|error| panic!("{}: {error}", vector.id));
        }
    }

    #[test]
    fn binary_schema_capacity_is_encoded_but_raw_profile_ceiling_is_decoded() {
        let limits = request::Limits {
            schema: caps(64, 64).unwrap(),
            edit_text_bytes: 64,
            binary_decoded_bytes: 4,
        };
        assert_eq!(limits.schema.binary_data_bytes, 128);
        assert!(
            request::parse(
                CoreTool::Bwrite,
                &json!({"path":"a.bin","data":"01020304","encoding":"hex"}),
                limits,
            )
            .is_ok()
        );
        assert!(
            request::parse(
                CoreTool::Bwrite,
                &json!({"path":"a.bin","data":"0102030405","encoding":"hex"}),
                limits,
            )
            .is_err()
        );
        assert!(
            request::parse(
                CoreTool::Bedit,
                &json!({"path":"a.bin","edits":[
                    {"offset":0,"length":0,"data":"0102","encoding":"hex"},
                    {"offset":0,"length":0,"data":"0304","encoding":"hex"}
                ]}),
                limits,
            )
            .is_ok()
        );
        assert!(
            request::parse(
                CoreTool::Bedit,
                &json!({"path":"a.bin","edits":[
                    {"offset":0,"length":0,"data":"0102","encoding":"hex"},
                    {"offset":0,"length":0,"data":"030405","encoding":"hex"}
                ]}),
                limits,
            )
            .is_err()
        );
    }

    #[test]
    fn profile_grep_and_transport_bounded_read_fit_dotlink_projection() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for index in 0..24 {
            std::fs::write(
                root.join(format!("f{index}.txt")),
                format!("needle {}\n", "x".repeat(200)),
            )
            .unwrap();
        }
        let machine = crate::mcp::tools::test_support::test_machine_with(root.clone(), |config| {
            config.max_output_bytes = 512
        });
        let token = tokio_util::sync::CancellationToken::new();
        let grep = execute(
            &machine,
            "grep",
            json!({"pattern":"needle","path":".","mode":"content","limit":200}),
            None,
            &token,
        )
        .unwrap();
        let grep_text = grep.content[0].as_text().unwrap().text.as_str();
        assert!(grep_text.len() <= 512, "{} bytes", grep_text.len());
        assert!(
            grep_text.contains("more") || grep_text.contains("incomplete"),
            "{grep_text}"
        );

        let quotes = format!("{}\n", "\\\"".repeat(2040)).repeat(64);
        std::fs::write(root.join("quotes.txt"), quotes).unwrap();
        let budget = 4096;
        for numbered in [false, true] {
            let read = execute(
                &machine,
                "read",
                json!({"path":"quotes.txt","line":1,"lines":64,"byte":0,"numbered":numbered}),
                Some(budget),
                &token,
            )
            .unwrap();
            assert!(
                crate::mcp::serialized_json_len(&read).unwrap() <= budget,
                "numbered={numbered}"
            );
            let read_text = read.content[0].as_text().unwrap().text.as_str();
            if numbered {
                assert!(read_text.contains("more"), "numbered read: {read_text}");
                assert_eq!(read.content.len(), 1);
            } else {
                assert!(
                    read_text.starts_with("\\\""),
                    "unnumbered data block was not exact source text: {read_text:?}"
                );
                assert_eq!(read.content.len(), 2);
                let status = read.content[1].as_text().unwrap().text.as_str();
                assert!(status.contains("more"), "unnumbered status: {status}");
            }
        }
    }

    #[test]
    fn binary_resource_uri_escapes_each_model_path_segment() {
        for (input, suffix) in [
            ("simple.bin", "simple.bin"),
            ("report #1.bin", "report%20%231.bin"),
            ("report?tab.bin", "report%3Ftab.bin"),
            ("100%.bin", "100%25.bin"),
            ("pi=π.bin", "pi=%CF%80.bin"),
            (
                "nested/folder #1/file?x%.bin",
                "nested/folder%20%231/file%3Fx%25.bin",
            ),
        ] {
            let uri = resource_uri(input).unwrap();
            assert_eq!(uri, format!("abird://file/{suffix}"), "{input:?}");
            let parsed = url::Url::parse(&uri).unwrap();
            assert!(parsed.query().is_none() && parsed.fragment().is_none());
        }
    }

    fn remote_fs_start(
        machine: &LocalMachine,
        operation: aw_tools::OperationId,
        arguments: &Value,
    ) -> crate::remote_start::ValidatedRemoteStart {
        use aw_tools::remote_start::{
            REMOTE_START_VERSION, RemoteRuntimeLease, RemoteSessionAction,
            RemoteStartAuthorization, RemoteStartIntent, RemoteStartProvenance, RemoteStartSource,
        };
        let now = crate::remote_start::now_unix_ms().unwrap();
        let generation = machine.jobs.runtime_generation_fingerprint();
        let authorization = RemoteStartAuthorization::new(
            RemoteStartIntent {
                version: REMOTE_START_VERSION,
                original_invocation_id: "turn:1/call:fs".into(),
                turn_id: 1,
                call_id: "fs".into(),
                effect_attempt_id: "fs".into(),
                operation: operation.semantic_name().into(),
                canonical_ref: "/d:d/i:i/p:p/a:0/t:1".into(),
                arguments_sha256: crate::remote_start::arguments_sha256(arguments),
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
                    source_id: "fs".into(),
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
        crate::remote_start::validate_authorization(
            authorization,
            crate::remote_start::ValidationContext {
                client_id: "aw-client",
                target: &target,
                generation,
                now_unix_ms: now,
                operation,
                class: crate::remote_start::StartClass::Submit,
                arguments,
                control_ref: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn remote_start_filesystem_move_replays_first_receipt_without_second_effect() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let machine = crate::mcp::tools::test_support::test_machine(root.clone());
        let arguments = json!({"from":"a.txt","to":"b.txt"});
        let operation = aw_tools::OperationId::Fs(CoreTool::Move);
        let start = remote_fs_start(&machine, operation, &arguments);
        let cancellation = tokio_util::sync::CancellationToken::new();

        let first = execute_with_remote_start(
            &machine,
            "mv",
            arguments.clone(),
            None,
            &cancellation,
            Some(start.clone()),
        )
        .unwrap();
        assert_ne!(first.is_error, Some(true), "{first:?}");
        assert!(!root.join("a.txt").exists());
        assert!(root.join("b.txt").exists());

        let replay = execute_with_remote_start(
            &machine,
            "mv",
            arguments.clone(),
            None,
            &cancellation,
            Some(start),
        )
        .unwrap();
        assert_eq!(replay, first);
        assert!(!root.join("a.txt").exists());
        assert!(root.join("b.txt").exists());

        let direct_second = execute(&machine, "mv", arguments, None, &cancellation).unwrap();
        assert_eq!(direct_second.is_error, Some(true));
    }

    #[test]
    fn shared_execution_reaches_canonical_engine_for_read_write_lifecycle_and_binary() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("a.txt"), "x\nneedle\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "needle\n").unwrap();
        std::fs::write(root.join("blob.bin"), [1_u8, 2, 3]).unwrap();
        let machine = crate::mcp::tools::test_support::test_machine(root.clone());
        let cancellation = tokio_util::sync::CancellationToken::new();
        let call = |name: &str, arguments: Value| {
            execute(&machine, name, arguments, None, &cancellation).unwrap()
        };

        for (name, arguments) in [
            ("ls", json!({"path":".","limit":16})),
            ("find", json!({"path":".","glob":"**/*.rs","limit":16})),
            (
                "grep",
                json!({"path":".","pattern":"needle","mode":"content","limit":8}),
            ),
            (
                "read",
                json!({"path":"a.txt","line":1,"lines":1,"byte":0,"numbered":false}),
            ),
            ("write", json!({"path":"new.txt","content":"new"})),
            ("edit", json!({"path":"a.txt","edits":[["x","X",false]]})),
            (
                "patch",
                json!({"patch":"*** Begin Patch\n*** Add File: patched.txt\n+patched\n*** End Patch"}),
            ),
            ("cp", json!({"from":"a.txt","to":"copy.txt"})),
            ("mv", json!({"from":"copy.txt","to":"moved.txt"})),
            (
                "bread",
                json!({"path":"blob.bin","offset":0,"limit":2,"format":"hex"}),
            ),
            (
                "bwrite",
                json!({"path":"new.bin","data":"00ff","encoding":"hex"}),
            ),
            (
                "bedit",
                json!({"path":"blob.bin","edits":[{"offset":0,"length":1,"data":"09","encoding":"hex"}]}),
            ),
        ] {
            let result = call(name, arguments);
            assert_ne!(result.is_error, Some(true), "{name}: {result:?}");
        }
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "X\nneedle\n"
        );
        assert!(root.join("moved.txt").exists());
        assert_eq!(std::fs::read(root.join("new.bin")).unwrap(), [0, 255]);
        assert_eq!(std::fs::read(root.join("blob.bin")).unwrap(), [9, 2, 3]);

        // Test fixtures intentionally use a volatile private-artifact registry;
        // rm must reach the shared engine and fail closed on durable tombstone
        // custody rather than bypassing canonical lifecycle semantics.
        let remove = call("rm", json!({"paths":["moved.txt"],"recursive":false}));
        assert_eq!(remove.is_error, Some(true));
        let text = remove.content[0].as_text().unwrap().text.as_str();
        assert!(
            text.contains("cannot durably reserve private mutation artifact"),
            "{text}"
        );
        assert!(root.join("moved.txt").exists());
    }

    #[test]
    fn shared_execution_respects_policy_and_request_cancellation_before_effect() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "old").unwrap();
        let read_root = root.clone();
        let read_only =
            crate::mcp::tools::test_support::test_machine_with(root.clone(), move |config| {
                config.access = crate::mcp::tools::test_support::access(&read_root);
            });
        let token = tokio_util::sync::CancellationToken::new();
        assert!(
            execute(
                &read_only,
                "write",
                json!({"path":"x","content":"x"}),
                None,
                &token
            )
            .is_err()
        );
        assert!(!root.join("x").exists());

        let machine = crate::mcp::tools::test_support::test_machine(root.clone());
        token.cancel();
        let cancelled = execute(
            &machine,
            "write",
            json!({"path":"a.txt","content":"new"}),
            None,
            &token,
        )
        .unwrap();
        assert_eq!(cancelled.is_error, Some(true));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "old");
    }
}
