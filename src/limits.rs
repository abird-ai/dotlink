//! Product and transport limits that shape public tool contracts.
//!
//! Keep hosted-transport limits named here rather than burying magic numbers in
//! adapters: local policy can be stricter, but must never claim a synchronous
//! OpenAI tunnel capability the hosted transport cannot carry.

use std::time::Duration;

/// OpenAI Secure MCP Tunnel's absolute lifetime for one request, including
/// queueing, local execution and delivery of the terminal response.
pub(crate) const OPENAI_TUNNEL_REQUEST_DEADLINE: Duration = Duration::from_secs(120);
/// Space held back from the hosted deadline so a terminal response can be sent.
pub(crate) const OPENAI_TUNNEL_DELIVERY_RESERVE: Duration = Duration::from_secs(5);
/// Longest one shell call holds the caller's turn synchronously.
///
/// The hosted deadline counts from the caller's request, while local execution
/// starts only when a poll delivers it: the inbound queue hop is not observable
/// here. Consuming the remaining window in full therefore races the caller's own
/// timeout and loses the response, and durable execution makes those last
/// seconds worth nothing. Past this point the same job handle is returned.
pub(crate) const OPENAI_TUNNEL_SYNC_HOLD: Duration = Duration::from_secs(90);
const _: () = assert!(
    OPENAI_TUNNEL_SYNC_HOLD.as_secs() + OPENAI_TUNNEL_DELIVERY_RESERVE.as_secs()
        < OPENAI_TUNNEL_REQUEST_DEADLINE.as_secs()
);
/// Smallest accepted profile byte limit. Below this the fixed JSON reply framing
/// and a truncation marker no longer fit, so a configured ceiling could not be
/// honoured; the value is rejected rather than silently widened.
pub(crate) const MIN_BYTE_LIMIT: usize = 1024;
/// Smallest accepted shell timeout.
pub(crate) const MIN_SHELL_TIMEOUT_SECS: u64 = 1;

/// How many recent jobs `job_list` returns when the caller does not ask.
pub(crate) const DEFAULT_JOB_LIST: usize = 64;
/// Ceiling for `job_list.limit`, and the shape of the advertised bound.
pub(crate) const MAX_JOB_LIST: usize = 256;

/// Default bounded wait for one `job_get`/`job_output` poll. The transport
/// clamps it to the caller's remaining response budget, so a wait never outlives
/// the response it is attached to.
pub(crate) const DEFAULT_JOB_WAIT_SECS: u64 = 90;
pub(crate) const DEFAULT_JOB_WAIT: Duration = Duration::from_secs(DEFAULT_JOB_WAIT_SECS);

/// Turn horizon for one user/model turn: the guidance asks a caller to stop and
/// hand back around here. Advisory only — dotlink never infers a turn boundary,
/// it stamps `ts` and the caller keeps its own clock.
pub(crate) const TURN_CHECKPOINT_MINUTES: u64 = 35;

/// OpenAI Secure MCP Tunnel's serialized request/response message ceiling.
pub(crate) const OPENAI_TUNNEL_SERIALIZED_MESSAGE_BYTES: usize = 10 * 1024 * 1024;
/// Reserve between the complete embedded MCP message and the hosted tunnel
/// envelope for request/response ids, channel metadata, headers and framing.
pub(crate) const OPENAI_TUNNEL_ENVELOPE_RESERVE_BYTES: usize = 1024 * 1024;
/// Largest embedded MCP request/response carried inside one hosted envelope.
pub(crate) const OPENAI_TUNNEL_MCP_MESSAGE_BYTES: usize =
    OPENAI_TUNNEL_SERIALIZED_MESSAGE_BYTES - OPENAI_TUNNEL_ENVELOPE_RESERVE_BYTES;
/// Budget for one serialized MCP `CallToolResult` inside the embedded message.
/// The remaining 64 KiB covers JSON-RPC ids/fields and response framing.
pub(crate) const OPENAI_TUNNEL_TOOL_RESULT_RESERVE_BYTES: usize = 64 * 1024;
pub(crate) const OPENAI_TUNNEL_TOOL_RESULT_BYTES: usize =
    OPENAI_TUNNEL_MCP_MESSAGE_BYTES - OPENAI_TUNNEL_TOOL_RESULT_RESERVE_BYTES;
/// Conservative room inside an embedded MCP response for binary-content
/// metadata (path/URI, mime type, JSON fields and content-block framing).
pub(crate) const OPENAI_TUNNEL_BINARY_RESPONSE_RESERVE_BYTES: usize = 64 * 1024;
/// Round profile transfer ceiling below the hosted serialized-message limit.
///
/// This is a policy ceiling, not a claim that every 8 MiB raw payload serializes
/// below the hosted envelope: JSON escaping/base64 can expand source bytes. The
/// embedded MCP endpoint enforces OPENAI_TUNNEL_MCP_MESSAGE_BYTES, the outer tunnel
/// enforces OPENAI_TUNNEL_SERIALIZED_MESSAGE_BYTES, and pageable text/job surfaces
/// fit their complete CallToolResult beneath OPENAI_TUNNEL_TOOL_RESULT_BYTES.
pub(crate) const OPENAI_TUNNEL_TRANSFER_CEILING_BYTES: usize = 8 * 1024 * 1024;

/// New profiles default every per-call transfer ceiling to this value; users can
/// raise them up to OPENAI_TUNNEL_TRANSFER_CEILING_BYTES when larger transfers
/// are useful. This is the *maximum* a call may ask for, not what it gets when
/// it does not ask.
pub(crate) const DEFAULT_TRANSFER_BYTES: usize = 1024 * 1024;

/// What a shell result or `job_output` page gets when the call does not ask for a
/// size. A tool result is context the model must read, so the default favours a
/// short one: the profile ceiling stays the caller's maximum, the spool keeps
/// every byte, and `job_output` pages the rest on demand.
pub(crate) const DEFAULT_OUTPUT_PAGE_BYTES: usize = 256 * 1024;

/// Completed/background job state is retained for this long unless all spool
/// output has already been consumed. Active jobs keep their own shell deadline.
pub(crate) const DEFAULT_JOB_RETENTION_SECS: u64 = 60 * 60;
pub(crate) const MIN_JOB_RETENTION_SECS: u64 = 60;
pub(crate) const MAX_JOB_RETENTION_SECS: u64 = 24 * 60 * 60;

/// Disk-backed shell output is bounded independently of one MCP response page.
/// This allows useful multi-page build logs without letting a runaway producer
/// consume the host filesystem indefinitely.
pub(crate) const MAX_JOB_SPOOL_BYTES_PER_STREAM: u64 = 64 * 1024 * 1024;

/// Trailing lines a `job_get` reply carries per stream unless the caller asks
/// otherwise. The end of a build answers "how is it going" and "why did it
/// fail" in one round trip, without paging the middle; four lines is usually the
/// verdict, and a caller that needs a wider window asks for it.
pub(crate) const DEFAULT_JOB_TAIL_LINES: usize = 4;
/// Ceiling on the same argument: enough to see a failure block, bounded so a
/// status probe cannot quietly become a page read.
pub(crate) const MAX_JOB_TAIL_LINES: usize = 16;
/// Aggregate persisted state for one live job session. Per-stream limits stop
/// one noisy child; this shared ceiling stops many retained jobs from filling
/// the host disk during the retention window.
pub(crate) const DEFAULT_JOB_SPOOL_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MIN_JOB_SPOOL_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_JOB_SPOOL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
