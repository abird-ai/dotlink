# 0.17 main shared-consumer backport

Author: Prasanna Loganathar
Co-authors: ChatGPT GPT-5.6 Sol
Owner: @pvl

Dotlink-side companion to AW's active shared-first plan:

- `../../../../aw/.agents/plans/017-shared-runtime-convergence-2026-11-18/`
- canonical comprehensive handoff: `../../../../aw/.agents/docs/AW-DOTLINK-017-FULL-COMPREHENSIVE-HANDOFF-2026-11-18.md`
- next-agent entrypoint: `../../../../aw/.agents/docs/AW-DOTLINK-017-NEXT-AGENT-2026-11-18.md`
- authority-gated publish/repin preflight: `../../../../aw/.agents/plans/017-shared-runtime-convergence-2026-11-18/PUBLISH-REPIN-PREFLIGHT.md`
- D1-D3 exact source cutover map: [D1-D3-SOURCE-CUTOVER-MAP.md](D1-D3-SOURCE-CUTOVER-MAP.md)
- v0.17.x disposition matrix: [V0174-EQUIVALENCE.md](V0174-EQUIVALENCE.md)

Authoritative release reference:

- `release/0.17 = 887401398da297c458ffa2a152a07e1b85870c35`
- `v0.17.4 = fd1cdb6429a341a4e3d27805cae0bec288fc26df`
- post-tag `8874013` delegates signal labels to the platform helper in tests
- final stop/provenance implementation: `c185558`
- final hardening: `74820ff`, `06d5548`, `30099aa`, `5b27784`

Current dotlink main checkpoint before this effort:

- `4f60bc3d4d2edb42c7d75f91d5dc6e8bae02d02a`
  `Complete pre-0.17 shared convergence`

The goal is **not** to port release-local runtime machinery into dotlink main.
Portable semantics move into AW first. Dotlink then consumes those types/helpers
and retains only product configuration, transport, wire compatibility, operator
logging and deployment behavior.

## Current sequencing checkpoint

AW P1-P8 is implemented and host-qualified in its current working tree, but the
exact implementation has not been authorized for commit/publish yet. Dotlink
still pins published AW `792f0b38...`, which does not contain the P1-P8 APIs.

Therefore D1-D7 must **not** start by inventing compatibility copies or a sibling
path dependency. They resume only after the user authorizes an exact signed AW
checkpoint, publication, and atomic dotlink repin.

The independent product-only D8 Nix hardening is implemented now: module-owned
environment keys are rejected through one generic assertion and regression
loop, while non-owned `DOTLINK_*`, `RUST_LOG` and XDG variables remain
available.

The product-owned D2 logging prerequisite is also implemented independently:
structured LOG/TOOL/REQ writes are locked best-effort stderr writes and cannot
panic on BrokenPipe, including when invoked from Drop during unwinding.
`LogConfig::lifecycle` is the always-visible operator-evidence channel;
existing production notices use it and tests prove it is ungated by verbosity.
The actual stop observer/fixed stop rendering still waits for AW lifecycle-event
repin.

D6 compatibility-preservation and local fitting semantics are independently
locked before the shared helper swap: real stdio and hosted
`bread(format=mcp)` calls preserve exact Image/Audio/Resource MIME + payload
identity; decorator coverage preserves structured content and receipt
metadata/replay identity; the local generic fitter now mutates text only and
returns an explicit MCP budget error when immutable facts cannot fit. Receipt
stamping is also transactionally capped at 16 KiB result metadata, preserves
unrelated keys, keeps stored replay timestamps authoritative, and never adds a
second exact visible `ts=` marker. The shared helper replacement still waits for
AW repin.

D4/D5 product-boundary compatibility is also locked pre-repin: hostile finite
Job refs stay compact over stdio/local HTTP, narrow replay keeps the original
Job/effect/receipt metadata, and a one-byte `jobs_read` raw page now has an
independent serialized envelope and truthful ACK/absolute-offset continuation.
Shared typed refs/receipts/budget wrappers still wait for AW repin.

D7 scheduling is likewise corrected before the type swap: a pending progress
notification is raced against the underlying operation, so result readiness is
never charged the 100 ms send budget; when the operation wins, the send future
is dropped rather than detached. Existing hosted tests retain token-gated,
monotonic, no-total/no-message heartbeat shape. Shared heartbeat/observer types
still wait for AW repin.

## Active ownership boundary

### AW owns

Dotlink must consume rather than duplicate:

- physical `ProcessExit::{Code,Signal(n),Unknown}`;
- minimal `StopReason::{Timeout,Cancelled,RuntimeError}`;
- `StopEvidence::{Stopped,NoTarget,Unknown}` and `StopRecord`;
- evidence-preserving managed process-tree stop outcome;
- canonical `ProcessTerminal { exit, stop, output }`;
- typed Task terminal/failure detail;
- execution lifecycle event values;
- typed flat Job/Node references;
- bounded shared diagnostics;
- Job handoff/control/ACK receipt grammar;
- raw page vs serialized response budget separation;
- MCP receipt metadata decoration;
- complete MCP result fitting;
- MCP heartbeat/progress utility;
- signal-name utility.

### Dotlink owns

- profile/CLI/environment configuration and precedence;
- OpenAI/HTTP/stdio transport response windows;
- outer OpenAI request replay body/lease/tombstone ledger;
- exact v0.17.4 wire spelling such as `stopped_by="shutdown"`;
- outside-dotlink explanatory prose;
- exact `LOG job stop ...` format and logging sink;
- non-panicking logger implementation used by that sink;
- NixOS module/systemd deployment policy;
- runtime-health deployment documentation;
- release/version/tagging, only when separately authorized.

## Canonical v0.17.4 mapping

### Stop provenance

Use shared AW facts and project:

```text
AW ProcessExit::Signal(n)
    -> JSON signal: n

AW StopReason::Timeout + Stopped
    -> stopped_by: "timeout"

AW StopReason::Cancelled + Stopped
    -> stopped_by: "cancel" or "shutdown" from dotlink-owned context

AW StopReason::RuntimeError + Stopped
    -> stopped_by: "failed"
```

No `stopped_by` unless the canonical AW `StopRecord` has
`evidence=Stopped` and `reason=Some(...)`. In particular, none for:

- outside signal;
- NoTarget;
- Unknown;
- housekeeping with `reason=None`.

Caller/parent/runtime/authority provenance and explicit-vs-Drop path are not AW
process-stop semantics. Dotlink retains only the product context it needs for
wire compatibility and operator diagnostics through its higher-layer state or
lifecycle observer.

The exact stop attempt still reaches dotlink's lifecycle-event sink so the
operator can see:

```text
LOG job stop id=<id> reason=<reason> outcome=<delivered|gone|failed>
```

The sink is visible at every verbosity and must use non-panicking stderr writes.

### Task failure projection

Dotlink does not rebuild termination facts from private `JobCompletion` fields.
It projects AW compact `TaskTerminalDetail { exit, stop }` into MCP error `data`;
full output metadata remains in canonical Job/process state.

### Job refs and diagnostics

Consume `aw_tools::execution_ref::JobRef` for the finite Jobs surface:

```text
j:<handle>
j:<handle>/n:<coordinate>
```

Do not recreate a dotlink-local parser. AW keeps hierarchical `ExecutionRef`
for ownership/Agent routing; finite Job output uses `JobRef`, while the AW
adapter still accepts prior hierarchical finite refs as compatibility input.

Use `aw_tools::diagnostic::bounded_diagnostic` as the shared UTF-8-safe
16 KiB error backstop where caller-controlled product/transport prose can still
reach diagnostics after typed parsing.

### Job page and response budgets

Consume the implemented AW budget separation:

```text
RawPageBytes
  = caller/default resolved by JobProfile::resolve_read_page under its raw max

SerializedResultBytes
  = authenticated invocation/transport result ceiling, independent of raw page size
```

A one-byte `jobs_read` page must still fit inside a normal serialized response
envelope. A narrower serialized envelope may fit fewer raw bytes for delivery,
but it must not alter source page offsets, EOF, retained bytes or ACK semantics.

### Receipts

Consume the shared AW receipt grammar directly:

- `Receipt::JobStatus`;
- `Receipt::NodeStatus`;
- `Receipt::Handoff` / `render_handoff(...)`;
- `Receipt::Cancel`;
- `Receipt::Ack { AckMode::Finish | AckMode::Through(...) }`.

Every finite receipt takes a typed `JobRef`. The shared handoff fallback keeps
the same Job and explicit `no-rerun` semantics under a tighter result budget;
dotlink must not rebuild that grammar locally.

Dotlink may add its receipt metadata/timestamp only through the shared aw-mcp
decorator.

### MCP result handling

Consume the implemented shared `aw-mcp` helpers:

- `stored_receipt_timestamp`;
- `has_visible_receipt_timestamp`;
- `set_stored_receipt_timestamp`;
- `decorate_and_fit_complete_result`;
- `fit_complete_result`.

Result-level MCP `_meta` is retained by `McpToolResult` and bounded to 16 KiB.
The shared decorator stores `io.abird/receipt.ts`, preserves that timestamp
across replay, adds a visible `ts=` sidecar only when absent, and reserves the
sidecar atomically inside the serialized response ceiling.

Dotlink's local fitter now already matches the shared behavioral contract:
only text may shrink. Typed media/resources and structured JSON remain
identity-preserving; if immutable facts cannot fit, the result is an explicit
budget error rather than replacement prose. D6 therefore retains only the
shared helper/type adoption, not a behavioral correction.

P7 heartbeat/progress is now implemented in shared `aw-mcp`:

- obtain the authenticated/request-scoped rmcp progress token in dotlink;
- construct `ProgressHeartbeat::from_token(...)`;
- wrap long waits with `await_with_progress_heartbeat(...)`;
- pass dotlink's `peer.notify_progress(...)` as the send closure;
- implement `ProgressHeartbeatObserver` in the product logging layer to retain
  attempted/delivered summaries.

The shared helper preserves dotlink's 10 s heartbeat / 100 ms per-send defaults,
but improves one behavior: the underlying operation remains polled while a
progress send is backpressured, so the send budget can never add latency after
the operation itself is ready. There is no detached task or queue.

Bare heartbeats remain monotonic unknown-total/no-message ticks. Real semantic
progress uses `semantic_progress_notification` /
`notify_semantic_progress_bounded` and AW's existing known/unknown total
semantics rather than inventing percentages.

Delete/reduce dotlink-local generic equivalents after parity is proved.

## Product-only v0.17.4 hardening

### Nix environment ownership — `30099aa`

Still dotlink-owned. Reject module-owned keys in
`services.dotlink.environment`:

- `DOTLINK_PROFILE`;
- `DOTLINK_CONFIG_FILE`;
- `DOTLINK_STDIO`;
- `DOTLINK_COLOR`;
- `DOTLINK_SUPERVISOR`;
- `DOTLINK_API_KEY_FILE`;
- `NGROK_AUTHTOKEN_FILE`;
- `DOTLINK_SSH_AGENT_SOCKET`.

Allow unrelated environment such as `DOTLINK_VERBOSITY`, `RUST_LOG`, XDG
variables and, once the deferred sandbox plan is promoted, `DOTLINK_MOUNTS`.

### Runtime-health docs — `5b27784`

Adapt, do not copy verbatim. Current main uses:

- `jobs_get`, `jobs_read`, `jobs_ack`;
- raw-byte tails/pages;
- shared AW owner/ACK semantics;
- 30-minute ReceiptProfile guidance;
- `io.abird/receipt` timestamps.

## Explicitly deferred sandbox/config-v13 work

No generic mount/config-v13 implementation belongs to this active plan.

The AW frozen design is:

- `../../../../aw/.agents/plans/017-sandbox-mount-convergence-2026-11-18/`

Until explicit user promotion, do not add:

- config v13;
- profile `mounts`;
- `DOTLINK_MOUNTS`;
- `--mount`;
- generic `src/mounts.rs` equivalent;
- developer-cache generic-mount convergence.

The release v0.17.2/v0.17.3 mount code remains reference material only.

## Release items already satisfied

Do not re-port v0.17.0/v0.17.1 release-local implementations that current
main/shared AW already supersede:

- background shell single durable path;
- explicit ACK/Finish ownership;
- compact Job payloads;
- progress;
- `--no-tunnel`;
- strict schemas;
- `utf8_lossy`;
- receipt count units;
- old `job_output` / line-tail surface.

## Landing discipline

1. finish and qualify AW shared phases;
2. publish AW only under explicit user authorization;
3. repin all AW dependencies and lock entries atomically;
4. convert dotlink to shared consumers one semantic area at a time;
5. remove a dotlink-local generic mechanism only after focused parity tests pass;
6. preserve the existing outer OpenAI replay ledger;
7. keep sandbox frozen;
8. run full v0.17.4 semantic equivalence;
9. version/tag/release only as a separate authorized action.
