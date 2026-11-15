# Design — dotlink adapter of AW-native Job/output and grep

## Dependency and owner

Normative shared semantics: [AW Job output + grep plan](../../../../aw/.agents/plans/ai-native-tool-output-grep-2026-11-15/DESIGN.md). Adopt shared API without copying JobManager, spool, grep, Graph compiler or policy engine. Dotlink retains local machine authority, MCP text/typed content, private profiles/OAuth/credentials, hosted replay, MCP Task leases, and fixed transport windows. No AW Agent history, receiver wake, or multi-user Session rules imported.

## Builder dependency and focused-edit choice — 2026-11-15

- AW's [shared configurable Toolset factory](../../../../aw/.agents/plans/ai-native-toolset-builder-2026-11-15/DESIGN.md) now supports typed schema/work/Job/receipt profiles and metadata-only per-operation `ToolText` overrides for title, top-level prose, suffix and existing argument descriptions. At adoption, dotlink should project the **same factory descriptor** into MCP title/annotations/tool argument help, with exact dotlink host limits; no parallel description registry or schema rewrite.
- **Option A selected:** canonical `edit` stays an exact single-file operation. Multi-file edits compose as multiple `["edit", {path, edits}]` nodes in the existing `batch` Graph. Dotlink v0.17.x multi-file `edit` is **not** carried into the canonical model-facing contract; do not reintroduce a second batch mutation engine. Consider such a feature only after measured UX evidence and an independently reviewed transactional contract.
- This design decision does not authorize a dotlink implementation, AW dependency repin, or version bump. Dotlink-specific timestamp/35-minute guidance, Job receipt shaping, and other host defaults remain product-level decisions after AW shared runtime security gates close.

## Shared Graph outcome decision — 2026-11-15

- After adopting a reviewed AW Git pin, use the canonical unknown-dominant aggregation: `unknown` if **any** node effect remains unknown; `mixed` only if heterogeneous **known** outcomes have settled. Preserve per-node known counts in Graph receipts.
- Root-level `jobs_ack({ref})` must reject unresolved node effects or journal ambiguity before any logical owner release. Exact known-node Finish remains scoped; Task/replay output owners remain independent. Validate the two-node known+unknown versus known+fail cases over the real MCP response/ACK path.
- Dotlink code and shared dependency pins are **unchanged** until AW's remaining disclosure/delivery and Job wait correctness gates pass.

## Optional shared consumer leases — 2026-11-15

- AW's model host has one original Agent/Job audience; dotlink might authorize **independent** MCP or Task consumers. Shared `aw-tools` now provides stable `JobManager::consumer_output_owner(id, host_scope:[u8;32])` and bounded atomic `enroll_output_consumer(id,node?,view?,owner)`. These are opt-in host primitives, not automatic admission or delegated authority. Preserve the distinct producer Job and MCP Task owner domains.
- For dotlink, derive `host_scope` only from a verified stable consumer identity (including relevant Session/Actor/transport audience, not call ID, model arguments or transient request IDs). Revalidate current machine policy, replay/Task leases, output ACL and transport scope before enrollment/read/ACK. Do not claim consumer-specific quick-Finish until the real MCP integration is tested.
- Enrollment preflights all selected existing streams, cannot resurrect released output, is idempotent, and is limited to 16 owners per stream. A later-created stream needs separate enrollment before first read; an independently authorized consumer not enrolled before physical release cannot recover discarded bytes. Treat lease capacity and retention expiry as explicit bounded failure paths.
- The shared `JobManager` remains the sole delivery and release engine. Do not keep dotlink's separate shared-Job high-water table or copy a second per-consumer owner registry. No implementation, AW dependency pin or publish change in dotlink at this checkpoint.

## Independent live actor/tool authority — AW checkpoint 2026-11-15

- AW controlled Agent dispatch now propagates the real current `AgentStepGate` to `ToolContext`; `jobs_get` tails require the separate `jobs_read` Actor Envelope grant. Finite `jobs_read` rechecks before return, and finite `jobs_ack` rechecks Control immediately before the shared ACK/Finish. SDK/host contexts without Actor gates still use live ToolPolicy; this is not a dotlink permission source.
- Dotlink must project this model to its actual authenticated machine/transport/Task authority without importing an AW Actor gate or using stale replay output as a grant. Shared `aw-tools` owns the output/ACK effects; host policy owns source/actor/site/consumer checks.
- Do not call the AW check atomic wrt a concurrent Actor Envelope update; that linearization and remaining Control verbs still require review. Dependency pins remain unchanged until AW qualification.

## AW atomic finite-ACK checkpoint — 2026-11-15

- AW finite `jobs_ack` now composes `ToolBindingAuthority::with_revalidated_binding` with the live `AgentSessionController::with_authorized_tool_commit` while calling the original shared JobManager ACK/Finish. One synchronous critical section spans Control-grant and Actor Envelope checks through the effect. Revocation-before-commit denies without effect; commit-before-revocation is serialized.
- Dotlink must perform its own exact **machine/transport/Task** policy commit guard with its actual authority source. Do not copy an AW Actor gate into dotlink or substitute an ordinary check-before-ACK; a custom host gate that cannot atomically commit fails closed. A post-effect gate failure is outcome-uncertain and must not trigger an automatic mutation retry.
- This is *not* a claim that external writer-epoch, producer-runtime source, independently held replay leases or other Control operations are fully fenced. No dotlink code or shared dependency pin changed.

## Public calls

- `jobs_get({ref?,rev?,wait_ms?,tail?})`: for Process/Process node, omitted `tail` selects **512 raw bytes total across stdout and stderr** while running or terminal; explicit `tail` 0..1024 is *also an aggregate cap*, NOT per-stream. Allocate half to each stream (256+256 default), then reassign unused share to the other. Bound final projected text/headers inside result cap; exact byte spans and truncation/EOF. Graph roots return status/nodes. Implicit unauthorized tail produces status plus `tail=denied`; explicit nonzero unauthorized tail fails before revealing bytes. Never auto-ACK.
- `jobs_read({ref?,view?,offset?,limit?,encoding?,wait_ms?})`: signed `offset` for retained byte streams, -N means N newest bytes at pinned stream-length snapshot; positive is raw absolute offset, next always absolute. Preserve existing Agent-history-free dotlink schema, owner proof and truthful incomplete status. No `tail_bytes` alias, no legacy `job_output`.
- `jobs_ack({ref,view?,offset?})`: omitted offset is explicit Control-authorized `Finish` of this exact caller's terminal retained stream(s), with Graph-root all-own-node cleanup; exact offset is prefix `Through` and must be contiguous and proven delivered. Check terminal/unknown and all output owners before mutating; never release Task/replay/other output owner data early. Does not touch independent child Agent/Job.
- `grep`: one shared canonical 0..128 per-side `context`, `before`, `after`, direct/Graph parity, exact-file and directory-tree behavior, bounded candidates/scanning/output/incomplete markers.
- Keep DL-150 compact model text, known fixed tool names and flat `batch` nodes. Do not expose `jobs_*` as model-Graph leaves without separate exact owner/control/wait-cycle qualification; no nested batch.

## Implementation stages

1. Wait for AW shared output ownership, tail/read/ACK primitives, grep schema and AW adapter to pass a coherent targeted checkpoint. Obtain a reviewed, available AW Git commit; no speculative pin updates.
2. Dotlink MCP schema + adapter reuse existing `src/mcp/tools/jobs.rs`, `src/jobs.rs` and `src/mcp/tools/shared_native.rs`; preserve local authority, hidden replay/delivery and native MCP Task owners.
3. Qualify combined default/explicit tail across both streams, signed offsets and exact continuation, quick finish vs prefix ACK, Job/node/Graph-root reads, replay/Task owner cleanup, revoked/unknown, live waits, response windows, file/dir grep 0/128 and incomplete results.
4. Run grouped all-feature targeted Rust tests, strict Clippy, formatting, and only newly affected Nix/config checks. Native Windows/macOS and full real AW-dependent flake remain separate honest release gates. No commit/push/release unless approved.
