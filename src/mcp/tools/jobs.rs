//! Compatibility job tools for long-running shell commands.
//!
//! Native MCP Tasks and these tools address the same JobManager entries and use
//! the same job id. Clients without the Tasks extension can therefore start,
//! poll, page output and cancel without holding one request open.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Tool},
    schemars,
    service::{RequestContext, RoleServer},
    tool, tool_router,
};
use serde::Deserialize;

use super::{LocalMachine, dot, ok_text, resolve_capped, tool_error};
use crate::jobs::JobStream;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobStartArgs {
    /// Command string to execute in the background.
    command: String,
    /// Working directory. Relative paths resolve from dotlink's launch directory.
    #[serde(default = "dot")]
    dir: String,
    /// Optional stdin text passed to the process.
    #[serde(default)]
    stdin: Option<String>,
    /// Execution timeout in seconds. Defaults to the profile shell ceiling and is capped there.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

impl From<JobStartArgs> for super::shell::ShellArgs {
    fn from(args: JobStartArgs) -> Self {
        Self {
            command: args.command,
            dir: args.dir,
            stdin: args.stdin,
            timeout_secs: args.timeout_secs,
            max_output_bytes: None,
        }
    }
}

pub(crate) fn decode_job_start(
    arguments: serde_json::Map<String, serde_json::Value>,
) -> Result<super::shell::ShellArgs, serde_json::Error> {
    serde_json::from_value::<JobStartArgs>(serde_json::Value::Object(arguments)).map(Into::into)
}

pub(crate) fn with_job_limits(tool: &mut Tool, max_secs: u64, max_output_bytes: usize) {
    let name = tool.name.as_ref();
    if !matches!(name, "job_start" | "job_output") {
        return;
    }
    let mut schema = (*tool.input_schema).clone();
    let Some(properties) = schema
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    if name == "job_start"
        && let Some(property) = properties
            .get_mut("timeout_secs")
            .and_then(serde_json::Value::as_object_mut)
    {
        property.insert(
            "description".to_owned(),
            serde_json::Value::String(format!(
                "Background execution timeout in seconds. Defaults to {max_secs} and is capped at {max_secs}."
            )),
        );
        property.insert("maximum".to_owned(), serde_json::Value::from(max_secs));
    }
    if name == "job_output"
        && let Some(property) = properties
            .get_mut("max_output_bytes")
            .and_then(serde_json::Value::as_object_mut)
    {
        property.insert(
            "description".to_owned(),
            serde_json::Value::String(format!(
                "Serialized MCP result budget for this page. Defaults to {max_output_bytes}, must be at least {} and is capped at {max_output_bytes}.",
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
    tool.input_schema = std::sync::Arc::new(schema);
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobIdArgs {
    /// Job/task identifier returned by bash, powershell or job_start.
    job_id: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum OutputStream {
    #[default]
    Stdout,
    Stderr,
}

impl From<OutputStream> for JobStream {
    fn from(value: OutputStream) -> Self {
        match value {
            OutputStream::Stdout => Self::Stdout,
            OutputStream::Stderr => Self::Stderr,
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
struct JobOutputArgs {
    /// Job/task identifier returned by bash, powershell or job_start.
    job_id: String,
    /// stdout or stderr. Defaults to stdout.
    #[serde(default)]
    stream: OutputStream,
    /// Raw byte offset in the selected spool stream. Defaults to zero.
    #[serde(default)]
    offset: Option<u64>,
    /// Maximum response bytes for this page. Defaults to the profile output ceiling and is capped there.
    #[serde(default)]
    max_output_bytes: Option<usize>,
    /// text returns UTF-8-lossy text for model readability; base64 preserves arbitrary bytes exactly.
    #[serde(default)]
    encoding: OutputEncoding,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobAckArgs {
    /// Job/task identifier.
    job_id: String,
    /// stdout or stderr. Defaults to stdout.
    #[serde(default)]
    stream: OutputStream,
    /// Highest byte offset safely received by the caller.
    offset: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobListArgs {
    /// Maximum recent exposed jobs to return. Defaults to 64, capped at 256.
    #[serde(default)]
    limit: Option<usize>,
}

#[tool_router(router = "jobs_router", vis = "pub(crate)")]
impl LocalMachine {
    #[tool(
        description = "Start a shell command explicitly in the background and return its durable job/task id immediately. Ordinary bash/powershell calls use the same execution layer but first wait through their response window. Each stdout/stderr spool retains up to 64 MiB; larger streams are fully drained but the retained prefix is marked truncated. Poll with job_get, page durable output with job_output, acknowledge received bytes with job_ack, and cancel with job_cancel.",
        annotations(
            title = "Start background shell job",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn job_start(&self, Parameters(args): Parameters<JobStartArgs>) -> CallToolResult {
        match self.start_shell_job(args.into(), cfg!(windows), None) {
            Ok(started) => match self.jobs.expose_compatibility(&started.id) {
                Ok(result) => result,
                Err(error) => tool_error(error),
            },
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "List recent durable shell jobs that were exposed to the caller. Useful after a lost creation response; returns stable job ids, status/phase, elapsed time and session spool usage.",
        annotations(
            title = "List shell jobs",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn job_list(&self, Parameters(args): Parameters<JobListArgs>) -> CallToolResult {
        ok_text(self.jobs.list(args.limit.unwrap_or(64).clamp(1, 256)))
    }

    #[tool(
        description = "Get the current state of a long-running shell job. Returns status, elapsed time, exit state and durable stdout/stderr byte counts. The job id is also the native MCP task id when Tasks are supported.",
        annotations(
            title = "Get shell job status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn job_get(&self, Parameters(args): Parameters<JobIdArgs>) -> CallToolResult {
        match self.jobs.status(&args.job_id) {
            Ok(status) => ok_text(status),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Read one durable page of a shell job's stdout or stderr by raw byte offset. This operation is pure and replayable: it never acknowledges or deletes output. Pages are bounded by max_output_bytes; continue from next_offset until complete=true, then call job_ack with the highest safely received offset. Each stream retains at most 64 MiB and reports truncation beyond that. After both terminal streams are fully acknowledged, compatibility-job spools are released; native MCP Tasks compact them to the ordinary-result prefix needed for exact `tasks/get` replay and remove that prefix when task retention expires.",
        annotations(
            title = "Read shell job output",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn job_output(
        &self,
        Parameters(args): Parameters<JobOutputArgs>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let ceiling = resolve_capped(args.max_output_bytes, self.config.max_output_bytes);
        let response_ceiling = self.request_result_budget(&context).unwrap_or(ceiling);
        let stream = args.stream.into();
        let offset = args.offset.unwrap_or(0);
        let base64 = matches!(args.encoding, OutputEncoding::Base64);
        match crate::blocking::run(|| {
            self.jobs.output_page(
                &args.job_id,
                stream,
                offset,
                ceiling,
                response_ceiling,
                base64,
            )
        }) {
            Ok(page) => ok_text(page),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Acknowledge a durable stdout/stderr byte offset after the caller has safely received it. This is the only operation that advances output cleanup; job_output itself is a pure replayable read. Once both terminal streams are fully acknowledged, compatibility-job spools are released. Native MCP Tasks instead compact them to the exact-result prefix required by future `tasks/get` calls and retain only that prefix until task retention expires.",
        annotations(
            title = "Acknowledge shell job output",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn job_ack(&self, Parameters(args): Parameters<JobAckArgs>) -> CallToolResult {
        match self
            .jobs
            .acknowledge_output(&args.job_id, args.stream.into(), args.offset)
        {
            Ok(status) => ok_text(status),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Request cancellation of a running shell job. Cancellation kills the contained command tree; terminal jobs are left unchanged. Output already spooled remains available through job_output until acknowledged or expired.",
        annotations(
            title = "Cancel shell job",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn job_cancel(&self, Parameters(args): Parameters<JobIdArgs>) -> CallToolResult {
        match self.jobs.cancel(&args.job_id) {
            Ok(status) => ok_text(status),
            Err(error) => tool_error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_schemas_expose_only_the_relevant_profile_limits() {
        let mut start = LocalMachine::all_tools().get("job_start").cloned().unwrap();
        with_job_limits(&mut start, 3600, 8 * 1024 * 1024);
        let start_props = start.input_schema["properties"].as_object().unwrap();
        assert_eq!(start_props["timeout_secs"]["maximum"], 3600);
        assert!(
            !start_props.contains_key("max_output_bytes"),
            "background output is paged separately and must not advertise a synchronous stream cap"
        );

        let mut output = LocalMachine::all_tools()
            .get("job_output")
            .cloned()
            .unwrap();
        with_job_limits(&mut output, 3600, 8 * 1024 * 1024);
        let output_limit = &output.input_schema["properties"]["max_output_bytes"];
        assert_eq!(output_limit["maximum"], 8 * 1024 * 1024);
        assert!(
            output_limit["description"]
                .as_str()
                .unwrap()
                .contains("8388608")
        );

        let start_description = start.description.as_deref().unwrap();
        assert!(start_description.contains("64 MiB"));
    }
}
