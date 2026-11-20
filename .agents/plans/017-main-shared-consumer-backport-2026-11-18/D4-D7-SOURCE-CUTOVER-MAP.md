# D4-D7 exact source cutover map

Status: **D1-D7 + final equivalence + post-review process/control/Remote Start trust cutover complete in signed source checkpoint `648519add3d174b9e13de81aeca62431591dac6d`**

Parent:
[README.md](README.md)

AW owner:
`../../../../aw/.agents/plans/017-shared-runtime-convergence-2026-11-18/`

This file freezes the mechanical D4-D7 consumer swap against the current
fresh-review AW APIs. Dotlink is already repinned to final AW implementation
`b8a46a942a56a5ae5cb7e2a8fad47010fde4d65b`; D1-D7 are complete.

Do not add sibling/path overrides and do not recreate unpublished AW APIs
locally.

## 1. Dependency cutover

Current direct AW Git dependencies in `Cargo.toml`:

```text
aw-tools
aw-tools-fs-unix
aw-tools-fs-win
aw-platform
aw-mcp-protocol
```

The five direct packages are atomically pinned to the final `AW_IMPL_SHA`:

```toml
aw-tools = { git = "https://github.com/abird-ai/aw", rev = "b8a46a942a56a5ae5cb7e2a8fad47010fde4d65b",
             default-features = false, features = ["conformance", "durable"] }

aw-platform = { git = "https://github.com/abird-ai/aw",
                rev = "b8a46a942a56a5ae5cb7e2a8fad47010fde4d65b", default-features = false }

aw-mcp-protocol = { git = "https://github.com/abird-ai/aw",
                    rev = "b8a46a942a56a5ae5cb7e2a8fad47010fde4d65b" }
```

The target-specific `aw-tools-fs-unix` / `aw-tools-fs-win` entries are already
repinned to the same SHA.

The protocol crate enables **no aw-tools feature at all**. Dotlink's existing
`aw-tools/conformance,durable` feature set therefore stays exactly as-is through
Cargo feature unification.

Every `git+https://github.com/abird-ai/aw` package in `Cargo.lock` currently
resolves to that same SHA. Mixed AW revisions remain a hard stop.

Do **not** add full `aw-mcp`. It is the AW client adapter and would import
unneeded Agent/client dependencies.

## 2. D4 — flat Job refs

Current duplicate implementation:

```text
src/mcp/tools/jobs.rs
  use ExecutionRef / ExecutionSegment
  StandaloneRef
  parse_ref(...)
```

The only uses of `ExecutionRef` / `ExecutionSegment` in this module are the
local flat-ref parser.

Replace the whole parser surface with:

```rust
use aw_tools::execution_ref::JobRef;

let reference = JobRef::parse(input)
    .map_err(|_| "invalid finite Job ref")?;
```

Keep product error wording constant/bounded; never print the hostile input.

Access the internal manager key only at the host boundary:

```text
reference.job().as_str()
reference.node().map(|node| node.get() as usize)
```

Use checked integer conversion where a host API still takes `usize`.

### Internal ref construction

Current:

```text
src/jobs.rs::JobManager::standalone_ref(id, node) -> String
```

Replace with one typed helper that constructs a `JobRef` from the trusted
internal handle/node and only renders with `Display` at the model boundary.

Do not keep a second string grammar.

The shared `JobRef` limit/grammar becomes the only finite-ref parser:

```text
j:<handle>
j:<handle>/n:<positive-canonical-coordinate>
```

## 3. D4 — status/handoff/control receipts

Current duplicate surfaces in `src/mcp/tools/jobs.rs`:

```text
bounded_job_status
bounded_job_handoff
bounded_replayed_job_handoff

ControlReceipt
bounded_control_success
bounded_control_replay
```

Current shared-backed status string construction in `src/jobs.rs`:

```text
compact_root_status
standalone_status_locked
standalone_status
```

Pinned shared owner:

```rust
aw_tools::receipt::{
    Receipt,
    AckMode,
    NextAction,
    render_plain,
    render_handoff,
}
```

### Root / node status

For entries backed by the shared JobManager:

- get the authoritative shared `Snapshot` / `NodeState`;
- get the retained `ProcessTerminal` where applicable;
- render `Receipt::JobStatus` or `Receipt::NodeStatus`;
- use `render_handoff` for an admitted Job that needs a narrower response.

Do not reclassify Job lifecycle locally. Shared rendering already distinguishes:

```text
running
cancelling
revoking
settled
cancelled
revoked
```

and retains process:

```text
exit=<...>
stop=timeout|cancelled|runtime_error
```

A narrow legacy compatibility formatter may remain only for a genuinely
non-shared historical entry that cannot supply a shared `Snapshot`; it must
not be used for shared-backed Jobs and must not become another canonical
grammar.

### ACK / Finish / Cancel

Delete `ControlReceipt` after parity.

Map:

```text
ACK offset Some(n) -> Receipt::Ack { mode: AckMode::Through(n), ... }
ACK offset None    -> Receipt::Ack { mode: AckMode::Finish, ... }
Cancel             -> Receipt::Cancel { ... }
```

The shared receipt is projection **after** the actual effect. It never performs
ACK/cancel itself.

Current local wording such as:

```text
ack=through:<n> next=jobs_get no-rerun
ack=finish next=jobs_get no-rerun
```

is intentionally replaced by the shared factual grammar. Tests should assert
the shared grammar, not preserve the old duplicate string.

### jobs_send

`Receipt` intentionally has no generic Send variant. Keep the tiny
dotlink-owned stdin receipt/replay projection:

```text
j:<ref> sent
```

but change it to consume a typed `JobRef`; do not add Send to AW merely to
remove a few product lines.

## 4. D4 — replay must not parse rendered prose as state

Important migration hazard:

```text
bounded_replayed_job_handoff
bounded_control_replay
bounded_send_replay
```

currently inspect cached text to recover/validate Job identity.

After typed-ref adoption:

- do not use arbitrary rendered prose as semantic Job state;
- carry the already-known typed `JobRef` through the replay call;
- for status/handoff, re-read the same retained Job's authoritative revision /
  shared snapshot when a narrower factual receipt is needed;
- for control/send replay, the call already has the requested typed ref, so
  validate replay identity against that trusted ref rather than reparsing a
  free-form status string;
- preserve the original cached result/metadata as authoritative replay state;
  a narrow projection must never replace the cached original or rerun the
  effect.

Keep the existing no-rerun / same-effect regressions.

## 5. D4 — diagnostics

Current duplicate:

```text
src/jobs.rs
  MAX_JOB_ERROR_BYTES
  bounded_job_error(...)
```

Replace the generic UTF-8-safe truncation helper with:

```rust
aw_tools::diagnostic::bounded_diagnostic
```

Product-specific protocol-envelope error fitting
(`fit_mcp_error_to_budget`) remains dotlink-owned because it bounds serialized
MCP `ErrorData`, not a shared Job diagnostic.

The hostile 32 KiB Job-ref tests over stdio and local HTTP remain mandatory.

## 6. D5 — raw page budget vs serialized result budget

Current model-facing handler:

```text
src/mcp/tools/jobs.rs::jobs_read
```

Current compatibility behavior is already correct:

- `limit=0` rejected;
- `limit=1` accepted;
- raw page limit is independent from serialized response capacity.

After repin, replace manual limit validation with:

```rust
let raw_page = profile.resolve_read_page(args.limit)?;
```

and represent the authenticated/fallback envelope independently:

```rust
let result_budget = SerializedResultBytes::new(
    request_result_budget.unwrap_or_else(|| self.max_response_body_bytes())
)?;
```

No implicit conversion between the two types.

### Internal request types

Current product request state still carries two untyped `usize` fields:

```text
src/jobs.rs::OutputPageRequest
  requested_bytes: usize
  response_ceiling: usize
```

During D5 convert these boundaries to:

```text
requested_bytes: RawPageBytes
response_ceiling: SerializedResultBytes
```

or unwrap each exactly once at the low-level renderer boundary if changing the
internal struct would create needless churn.

Prefer typed fields: they prevent the old raw-page/envelope swap at compile
time and are `repr(transparent)` zero-cost newtypes.

### Read path coupling to remove

Current code still derives a `payload_ceiling` from
`response_ceiling` before reading:

```text
output_page
standalone_graph_result_page
output_node_page
job_output_payload_ceiling
```

The final D5 shape is:

1. raw source read/window is bounded by `RawPageBytes`;
2. source offset/retained length/EOF are fixed independently;
3. serialized projection may fit a smaller delivered prefix to
   `SerializedResultBytes`;
4. only the raw prefix actually included in the validated response receives
   delivery credit;
5. ACK proof remains based on that delivered prefix, not the originally
   requested raw page size.

Do not mutate retained source bytes or absolute offsets to satisfy the response
envelope.

Keep `record_standalone_delivery` / `require_standalone_delivery` semantics
until the equivalent shared owner proof is demonstrably authoritative for every
legacy/current owner.

## 7. D5 — schema

Current:

```text
src/mcp/tools/jobs.rs::with_job_limits
```

Continue deriving default/max from the same `JobProfile` used at runtime.

The schema must advertise:

```text
minimum = 1
maximum = effective profile.read_page_max_bytes
```

and the runtime must call `resolve_read_page` from that same effective
profile.

No generic `MIN_BYTE_LIMIT` belongs in the raw-page path.

## 8. D6 — generic MCP result/receipt projection

Pre-D6 local duplicates in `src/mcp.rs` — now removed:

```text
fitted_text_blocks
fit_call_tool_response_to_budget

RECEIPT_META_KEY
MAX_MCP_RESULT_META_BYTES
stored_receipt_ts
has_visible_receipt_ts
set_stored_receipt_ts
ensure_visible_receipt_ts
stamp_receipt_metadata

LocalMachine::decorate_and_fit_tool_response
```

Pinned shared owner:

```rust
aw_mcp_protocol::{
    stored_receipt_timestamp,
    has_visible_receipt_timestamp,
    set_stored_receipt_timestamp,
    validate_complete_result,
    fit_complete_result,
    decorate_and_fit_complete_result,
}
```

### Final response path — implemented

Keep a tiny dotlink wrapper around `CallToolResponse` only to:

- convert `Option<usize>` / transport fallback into
  `SerializedResultBytes`;
- select `OperationId`;
- pass `self.shared_toolset().receipt_profile()` as the copyable policy;
- supply the host clock timestamp;
- map `aw_mcp_protocol::Error` into dotlink's `McpError`;
- leave non-Complete response variants unchanged.

For a selected operation call:

```text
decorate_and_fit_complete_result(
    toolset.receipt_profile(),
    operation,
    result,
    Some(host_unix_ts),
    serialized_ceiling,
)
```

For a generic complete result with no semantic operation, use
`fit_complete_result`.

Do not retain a second text fitter.

### Cached Remote Start receipt — implemented

`stamp_cached_mcp_receipt` occurs before final response-envelope projection.
Keep the **host clock / cache timing decision** product-owned, but use shared
metadata mutation:

- `Toolset::receipt_profile().timestamps(operation)` selects whether this
  operation needs a timestamp;
- host obtains `unix_ts_now()` only when selected;
- `set_stored_receipt_timestamp` writes it.

Do not fit cached replay state to one request's narrow response budget.

The stored timestamp remains immutable replay state and later wins over a fresh
host timestamp in `decorate_and_fit_complete_result`.

### What remains product-owned

Keep:

```text
unix_ts_now
fit_mcp_error_to_budget
serialized_json_len (where needed for McpError / product envelopes)
```

The shared protocol helper deliberately owns only complete
`CallToolResult` model projection.

## 9. D6 — mandatory compatibility gates — complete

The shared path passes:

- exact Image/PNG identity;
- exact Audio/WAV identity;
- exact Resource/PDF identity;
- structured-content preservation at the decorator boundary;
- unrelated result metadata preserved;
- fields inside `io.abird/receipt` preserved;
- malformed non-object reserved receipt metadata fails transactionally;
- stored replay timestamp wins;
- visible timestamp survives/reappears under a tight envelope;
- no unnecessary sidecar reserve when the visible timestamp survives fitting;
- truncation always has an explicit marker;
- immutable typed/structured facts return explicit limit failure.

The existing stdio/hosted and `src/mcp.rs` tests are the compatibility oracle.

## 10. D7 — progress heartbeat

Pre-D7 duplicate implementation — now removed:

```text
src/mcp/tools/mod.rs
  PROGRESS_HEARTBEAT_INTERVAL
  PROGRESS_SEND_BUDGET
  deliver_heartbeat_bounded
  ProgressHeartbeat
  heartbeat_tick
  await_wait_or_delivery
  await_with_heartbeat
```

Shared owner:

```rust
aw_mcp_protocol::{
    ProgressHeartbeatConfig,
    ProgressHeartbeat,
    ProgressHeartbeatSummary,
    ProgressHeartbeatObserver,
    await_with_progress_heartbeat,
    semantic_progress_notification,
    notify_semantic_progress_bounded,
}
```

### Request/token boundary stays in dotlink — implemented

Dotlink still owns rmcp server request context:

```text
context.meta.get_progress_token()
context.peer.clone()
```

Construct shared heartbeat with:

```text
ProgressHeartbeat::from_token(
    token,
    config,
    observer,
)
```

Use default 10 s / 100 ms in production.

A small cfg(test) helper may construct a shorter validated config for tests;
do not reintroduce a second scheduler.

### Delivery closure — implemented

The shared helper accepts a send closure. Dotlink supplies the server peer:

```text
peer.notify_progress(param).await.is_ok()
```

Per-tick product logging may remain in that closure:

```text
log.progress_mark(delivered)
```

Return the same boolean to the shared helper.

### Final observer — implemented

Replace the local `Drop for ProgressHeartbeat` summary with one product
observer carrying only:

```text
LogConfig
tool name
```

and implementing:

```rust
ProgressHeartbeatObserver::completed(summary)
```

which calls the existing product `progress_done(tool, attempted, delivered)`.

This preserves product logging without putting log/tool strings into AW.

### Call sites

Non-shell outer tool path:

```text
src/mcp.rs::LocalMachine::call_tool
```

Shell path:

```text
src/mcp/tools/shell.rs
  handle_shell_call
  handle_shell_call_with / synchronous wait paths
```

The shell call keeps its operation deadline when invoking
`await_with_progress_heartbeat`; non-shell outer calls use no delivery
deadline unless they already have a real one.

No handler starts a detached progress task.

## 11. D7 — real semantic progress — audited

Heartbeats remain advisory and unknown-total. Current dotlink exposes no real
cumulative AW semantic-progress source, so D7 intentionally adds no synthetic
progress.

If/when a real AW operation exposes cumulative semantic progress, project only
that fact through:

```text
semantic_progress_notification
notify_semantic_progress_bounded
```

Do not derive percentages from heartbeat count, elapsed time, Job revision or
output bytes.

## 12. Cutover order

After the exact AW SHA is published and repinned:

1. lock all five direct AW dependencies to one SHA and verify `Cargo.lock`;
2. compile before source changes;
3. D4: switch input parsing/output refs to `JobRef`;
4. D4: switch shared-backed status/handoff/control rendering to shared receipts;
5. D4: replace generic Job diagnostic truncation;
6. run hostile-ref + replay + receipt tests;
7. D5: type raw/result budgets and remove response-ceiling influence on raw
   source reads;
8. run zero/one-byte/ACK/absolute-offset tests;
9. D6: switch cached receipt metadata mutation;
10. D6: switch final result decorator/fitter;
11. run typed-media/structured/timestamp/tight-budget tests;
12. D7: switch heartbeat scheduler/send closure/observer;
13. run no-token/monotonic/backpressure/deadline/drop/hosted progress tests;
14. delete the superseded local generic helpers;
15. strict Clippy + full binary/integration/Nix qualification;
16. then rerun the full v0.17.x equivalence matrix.

Do not combine D4-D7 deletion before each replacement passes its focused
compatibility gates.

## 13. Current compatibility tests that must survive

Already green and must remain:

```text
stdio_huge_job_ref_is_rejected_without_reflection
local_http_huge_job_ref_is_rejected_without_reflection

remote_start_duplicate_graph_replays_receipt_without_second_effect
remote_cancel_replay_precedes_revision_recheck

stdio_jobs_read_one_byte_page_has_independent_response_envelope
jobs_read_schema_uses_profile_result_ceiling_and_has_no_filter

stdio_bread_mcp_preserves_typed_media_identity
hosted_bread_mcp_preserves_typed_media_identity
receipt_metadata_is_bounded_replay_stable_and_not_duplicated
transport_postcondition_never_silently_truncates_without_marker
transport_postcondition_preserves_immutable_facts_while_shrinking_text
transport_postcondition_fails_when_immutable_facts_cannot_fit

pending_progress_delivery_never_delays_or_outlives_ready_operation
heartbeat_delivery_never_crosses_the_command_deadline
job_status_wait_emits_progress_from_the_outer_call_boundary
shell_progress_heartbeat_is_forwarded_and_carries_no_message
```

Also retain the full current-main baseline and all D1-D3/v0.17.5 tests once
that preceding cutover lands.

## 14. Stop conditions

Stop the cutover if any of these occurs:

- any AW Git package resolves to a different SHA;
- a path/sibling override is needed;
- shared `JobRef` cannot represent an existing finite model-facing ref;
- narrow replay would require parsing arbitrary rendered prose as trusted state;
- raw-page fitting changes retained source offsets/EOF/ACK proof;
- typed/structured MCP facts would be converted to text;
- receipt replay would mint a new timestamp;
- progress send backpressure can delay operation completion;
- final semantics require importing full `aw-mcp` into dotlink.

The correct response to any of those is to repair the shared boundary first,
not add another dotlink-local semantic layer.
