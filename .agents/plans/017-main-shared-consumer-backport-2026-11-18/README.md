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
- D4-D7 exact source cutover map: [D4-D7-SOURCE-CUTOVER-MAP.md](D4-D7-SOURCE-CUTOVER-MAP.md)
- v0.17.x disposition matrix: [V0174-EQUIVALENCE.md](V0174-EQUIVALENCE.md)

Current release reference worktree (read-only here):

- local `release/0.17 = 3995448d7bf33794ef84c7bb51150d77cc260ada`
  (`chore(release): 0.17.5`);
- `origin/release/0.17 = 3995448d7bf33794ef84c7bb51150d77cc260ada`;
- `v0.17.5` resolves to `3995448d7bf33794ef84c7bb51150d77cc260ada`;
- `v0.17.4 = fd1cdb6429a341a4e3d27805cae0bec288fc26df`;
- v0.17.5 additions: `f9537bb` outside-signal lifecycle record and `a1f8505`
  hard execution-deadline wording;
- v0.17.4 stop/provenance implementation: `c185558`;
- v0.17.4 hardening: `74820ff`, `06d5548`, `30099aa`, `5b27784`.

Current dotlink main checkpoint before this effort:

- `4f60bc3d4d2edb42c7d75f91d5dc6e8bae02d02a`
  `Complete pre-0.17 shared convergence`

The goal is **not** to port release-local runtime machinery into dotlink main.
Portable semantics move into AW first. Dotlink then consumes those types/helpers
and retains only product configuration, transport, wire compatibility, operator
logging and deployment behavior.

## Current sequencing checkpoint

AW fresh-review implementation is now signed/published at
`7c7de6c9aeedf714adbe67b17ac863f27697e90e`; AW main is published through
`05dd2ff1163753903371b9f36d29922da1f03a20`. Dotlink still pins
`792f0b38...` only until the atomic repin in the current authorized turn.

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
The actual stop/outside-exit rendering still waits for the shared physical
exit/final-stop-record repin; attempt-level lifecycle events remain diagnostics.

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
Job/effect/receipt metadata, `jobs_read(limit=0)` is rejected, and a one-byte
raw page has an independent serialized envelope with truthful ACK/absolute-offset
continuation. Shared typed refs/receipts/budget wrappers still wait for AW repin.

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
- `StopEvidence::{Stopped,NoTarget,Unknown}`, `StopRecord` and zero-cost
  `StopTracker` for Unknown->retry reason preservation;
- evidence-preserving managed process-tree stop outcome and final
  `ProcessReceipt::stop_record()`;
- canonical `ProcessTerminal { exit, stop, output }`;
- typed Task terminal/failure detail;
- execution lifecycle event values;
- typed flat Job/Node references;
- bounded shared diagnostics;
- Job handoff/control/ACK receipt grammar;
- raw page vs serialized response budget separation;
- lightweight `aw-mcp-protocol` receipt metadata decoration;
- complete MCP result fitting;
- MCP heartbeat/progress utility without pulling the full AW client adapter;
- signal-name utility.

### Dotlink owns

- profile/CLI/environment configuration and precedence;
- OpenAI/HTTP/stdio transport response windows;
- outer OpenAI request replay body/lease/tombstone ledger;
- exact v0.17.x wire spelling such as `stopped_by="shutdown"`;
- outside-dotlink explanatory prose and v0.17.5 `LOG job exit ... from=outside`;
- exact mutually exclusive `LOG job stop ...` / `LOG job exit ...` formats and logging sink;
- non-panicking logger implementation used by that sink;
- NixOS module/systemd deployment policy;
- runtime-health deployment documentation;
- release/version/tagging, only when separately authorized.

## Canonical v0.17.x mapping through local 0.17.5

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
wire compatibility and operator diagnostics.

`ExecutionLifecycleEvent::ProcessStop` is attempt-level evidence and may fire
for graceful, forced and housekeeping attempts. It is useful for diagnostics,
but must not map one-for-one to operator lines. The runner exposes one final
`ProcessReceipt::stop_record()` for the logical stop. Dotlink renders exactly
one product line from that final record plus its own cancel-vs-shutdown context:

```text
LOG job stop id=<id> reason=<reason> outcome=<delivered|gone|failed>
```

Reason-less housekeeping/external exit emits no connector-stop line. For
v0.17.5, a physical `ProcessExit::Signal(n)` with no reasoned final stop record
instead emits exactly one product-only line:

```text
LOG job exit id=<id> signal=<name-or-number> from=outside
```

`from=outside` means only that dotlink did not enter a semantic stop sequence;
it never identifies the sender. A reasoned final stop record suppresses this
outside line even when evidence is NoTarget/Unknown, because direct-child
fallback may have delivered the physical signal. Signal tokens use
`aw_platform::signal_name(n)` or bare decimal when unknown, with no whitespace.
The sink is visible at every verbosity and uses non-panicking stderr writes.

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

Dotlink may add its receipt metadata/timestamp only through the shared
`aw-mcp-protocol` decorator.

### MCP result handling

Consume the implemented lightweight `aw-mcp-protocol` helpers:

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

P7 heartbeat/progress is implemented in shared `aw-mcp-protocol`:

- obtain the authenticated/request-scoped rmcp progress token in dotlink;
- construct `ProgressHeartbeat::from_token(...)`;
- wrap long waits with `await_with_progress_heartbeat(...)`;
- pass dotlink's `peer.notify_progress(...)` as the send closure;
- implement `ProgressHeartbeatObserver` in the product logging layer to retain
  attempted/delivered summaries.

The protocol crate depends on `aw-tools` with no aw-tools features, rmcp model
types, `serde_json` and Tokio timing/macros; it carries no Agent/runtime, reqwest,
jsonschema, clap or MCP transport stack. It preserves dotlink's 10 s heartbeat /
100 ms per-send defaults, while the underlying operation remains polled during
send backpressure. There is no detached task or queue.

Bare heartbeats remain monotonic unknown-total/no-message ticks. Real semantic
progress uses `semantic_progress_notification` /
`notify_semantic_progress_bounded` and AW's existing known/unknown total
semantics rather than inventing percentages.

Delete/reduce dotlink-local generic equivalents after parity is proved.

## Product-only v0.17.x hardening through local 0.17.5

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

### v0.17.5 shell deadline / outside-exit projection — `a1f8505`, `f9537bb`

Current main already ports the product-only deadline wording: `timeout_secs` is
a hard execution deadline that stops the command tree; the caller's response
window and `background:true` affect waiting/handoff only.

The outside-signal lifecycle line waits for D1/D2 shared physical exit + final
stop facts rather than reintroducing a local signal/provenance model. After
repin, use the mutually exclusive stop/outside-exit projection defined above.

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
8. run full v0.17.x semantic equivalence through the current local 0.17.5 reference;
9. version/tag/release only as a separate authorized action.
