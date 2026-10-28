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

/// New profiles default to modest per-call transfer ceilings; users can raise
/// them up to OPENAI_TUNNEL_TRANSFER_CEILING_BYTES when larger transfers are useful.
pub(crate) const DEFAULT_TRANSFER_BYTES: usize = 1024 * 1024;

/// Completed/background job state is retained for this long unless all spool
/// output has already been consumed. Active jobs keep their own shell deadline.
pub(crate) const DEFAULT_JOB_RETENTION_SECS: u64 = 60 * 60;
pub(crate) const MIN_JOB_RETENTION_SECS: u64 = 60;
pub(crate) const MAX_JOB_RETENTION_SECS: u64 = 24 * 60 * 60;

/// Disk-backed shell output is bounded independently of one MCP response page.
/// This allows useful multi-page build logs without letting a runaway producer
/// consume the host filesystem indefinitely.
pub(crate) const MAX_JOB_SPOOL_BYTES_PER_STREAM: u64 = 64 * 1024 * 1024;
/// Aggregate persisted state for one live job session. Per-stream limits stop
/// one noisy child; this shared ceiling stops many retained jobs from filling
/// the host disk during the retention window.
pub(crate) const DEFAULT_JOB_SPOOL_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MIN_JOB_SPOOL_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_JOB_SPOOL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
