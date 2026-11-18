# 0.17 main shared-consumer backport

Author: Prasanna Loganathar
Co-authors: ChatGPT GPT-5.6 Sol
Owner: @pvl

Dotlink-side companion to AW's active shared-first plan:

- `../../../../aw/.agents/plans/017-shared-runtime-convergence-2026-11-18/`

Authoritative release reference:

- `release/0.17 = v0.17.4 = fd1cdb6429a341a4e3d27805cae0bec288fc26df`
- final stop/provenance implementation: `c185558`
- final hardening: `74820ff`, `06d5548`, `30099aa`, `5b27784`

Current dotlink main checkpoint before this effort:

- `4f60bc3d4d2edb42c7d75f91d5dc6e8bae02d02a`
  `Complete pre-0.17 shared convergence`

The goal is **not** to port release-local runtime machinery into dotlink main.
Portable semantics move into AW first. Dotlink then consumes those types/helpers
and retains only product configuration, transport, wire compatibility, operator
logging and deployment behavior.

## Active ownership boundary

### AW owns

Dotlink must consume rather than duplicate:

- physical `ProcessExit::Signal(n)`;
- cancellation origin;
- process stop cause/attempt/outcome;
- evidence-preserving managed process-tree stop outcome;
- canonical `ProcessTerminal` stop evidence;
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

AW ProcessStopCause::Timeout
    -> stopped_by: "timeout"

AW Cancelled(Caller)
    -> stopped_by: "cancel"

AW Cancelled(Runtime|Authority)
    -> stopped_by: "shutdown"

AW ProcessStopCause::Failed
    -> stopped_by: "failed"
```

No `stopped_by` for:

- outside signal;
- AlreadyGone stop attempt;
- failed stop attempt;
- Cleanup;
- Dropped fail-safe event without a claimable terminal stop.

The exact stop attempt still reaches dotlink's lifecycle-event sink so the
operator can see:

```text
LOG job stop id=<id> reason=<reason> outcome=<delivered|gone|failed>
```

The sink is visible at every verbosity and must use non-panicking stderr writes.

### Task failure projection

Dotlink does not rebuild termination facts from private `JobCompletion` fields.
It projects AW typed Task terminal detail into MCP error `data`.

### Job refs and diagnostics

Use the shared bounded flat Job/Node ref parser. Raw MCP input should not remain
an unbounded internal Job ID.

Keep a bounded display/error backstop at the transport/product boundary even
after typed parsing.

### Job page and response budgets

Consume shared budget separation:

```text
page bytes      = caller/default under JobProfile max
response bytes  = authenticated transport ceiling, otherwise product result ceiling
```

A one-byte `jobs_read` page must still fit inside a normal serialized response
envelope.

### Receipts

Use shared AW receipt values for:

- Job handoff;
- Job status;
- node status;
- Finish/Through ACK;
- cancellation control receipt.

Dotlink may add its receipt metadata/timestamp only through the shared aw-mcp
decorator.

### MCP result handling

Consume shared aw-mcp helpers for:

- receipt `_meta`;
- visible timestamp sidecar where needed;
- replay timestamp reuse;
- typed media/structured payload preservation;
- serialized result fitting;
- progress heartbeat.

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
