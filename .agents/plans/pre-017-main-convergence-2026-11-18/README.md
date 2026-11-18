# Pre-0.17 main convergence

Author: Prasanna Loganathar
Co-authors: ChatGPT GPT-5.6 Sol
Owner: @pvl

Dotlink-side companion to AW's canonical shared-first plan:

`../../../../aw/.agents/plans/pre-017-shared-convergence-2026-11-18/`

Current dotlink baseline: published `main`
`5381729cd469eda54691e540deb3fee13e9f3e3e`.

Current published AW dependency pin: `7e6fe20a95640e32610819ce94b61a121e165dea`.
Do not repin until the new shared implementation is reviewed, signed and
published.

## Scope

This wave accounts for the **55 release-line commits before the v0.17.0 release
commit** (`84cd9b7..3317429`). The full commit-by-commit disposition lives in
the AW plan's `AUDIT.md`.

Do not import:

- the v0.17.0 release commit or later v0.17.x implementation;
- v0.17.2 generic mounts/config v13;
- post-v0.17.3 signal/stop provenance;
- any version/tag/release/deploy changes.

Current main already has newer shared-AW equivalents for most pre-0.17 behavior.
This plan ports only the remaining product deltas and requalifies the rest.
`88d7b71` is accounted here by the new 30-minute ReceiptProfile guidance;
`75ff9fe` / `3317429` are already satisfied by current `background:true`
and the single durable execution path.

## Product selections

### Receipt/result metadata

Consume the shared scoped `ReceiptProfile` with:

- timestamp enabled for `Process | Batch | Jobs`;
- Filesystem and Agent excluded initially;
- `checkpoint_minutes = Some(30)`.

The shared timestamp `ts` means **UTC Unix seconds when dotlink first finalizes
the represented result or observation receipt**. Immutable terminal/native-Task
results keep that timestamp across exact replay. Fresh `jobs_list/get/read`
observations get a new timestamp when rendered. It is not file mtime or
output-byte production time.

Append the shared model checkpoint instruction exactly once to MCP server
instructions. It is advisory only and never changes execution or transport
deadlines.

For MCP results:

- attach typed response metadata as
  `CallToolResult._meta["io.abird/receipt"] = {"ts":...}`;
- keep the timestamp visible to the model with a small receipt content block
  when the main payload is raw/structured/media and no compact typed receipt
  already carries it;
- never rewrite `jobs_read.data`, structured JSON, binary/media content or
  existing factual result bytes.

Exact cached/native-Task replay keeps the original result timestamp and exact
result identity; it is not re-stamped merely because it was replayed.

### Job read pages

Consume the shared Job profile:

- `read_page_default_bytes = min(256 KiB, max_output_bytes)`;
- `read_page_max_bytes = max_output_bytes` (existing config ceiling <=8 MiB);
- explicit `limit` remains caller-selected under the max;
- omitted `limit` uses the 256 KiB product default;
- actual authenticated transport result budget may narrow the page further
  without changing raw source offsets, EOF, delivery proof or ACK credit.

Schema descriptions/defaults/maxima must come from the same profile used by
runtime.

### Synchronous shell output

Restore the pre-0.17 omitted shell default independently:

- omitted `max_output_bytes` => `min(256 KiB, configured max_output_bytes)`;
- explicit caller value => clamp to the configured ceiling;
- spool retention and execution timeout remain independent.

This is a dotlink product default. Do not make AW's trusted native shell inherit
it merely because the shared Job page default is 256 KiB.

## Existing behavior to preserve

The following are already current/newer and must be reverified rather than
copied from release history:

- config v11/v12 migration, strict decode and config-file override;
- CLI > environment > profile precedence;
- `DOTLINK_VERBOSITY`, `--no-serialize`, `--no-tunnel`;
- NixOS single settings + private active-config staging;
- exact-file grep and 0..128 context through shared AW direct + Graph paths;
- authenticated response-window narrowing and generic MCP progress;
- one durable shell execution, current background/Task/compatibility exposure;
- shared AW event-driven Jobs get/read waits;
- D1 output owner/delivery/Through-ACK/Finish semantics;
- D3 Task/replay/authority retirement semantics;
- D4 guard-revocation/security behavior;
- B6 shared Toolset metadata projection.

Do not restore release-local `job_output`, line-tail defaults, duplicate
JobManager logic, response-clock parsing, descriptor substitution or old ACK
semantics.

## Landing order

1. Wait for the reviewed shared AW implementation.
2. Publish AW only after explicit user authorization.
3. Repin all three AW Git dependencies and all lock records together.
4. Enable dotlink metadata profile + 30-minute guidance.
5. Apply 256 KiB Job read and synchronous shell defaults.
6. Run the complete pre-v0.17 equivalence matrix and current D1-D4/B6 gates.
7. Commit/publish dotlink only under the normal explicit authorization rules.
8. Return to v0.17.x only after this plan is complete.

No version bump, tag, release or deployment belongs to this plan.
