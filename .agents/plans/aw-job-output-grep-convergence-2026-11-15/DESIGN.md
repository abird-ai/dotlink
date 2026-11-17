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

## Independent pre-pin settle-only wait — 2026-11-16

The existing dotlink Job notify and canonical shared-node state already support `jobs_get` settling only. A pre-pin adapter/runtime correction now waits for its exact Process/Graph root or node to become terminal, or for the requested deadline, independently of optional `rev` and unrelated output/Graph progress. The bounded response window still narrows this observation wait. Keep this correction when adopting AW's newer shared watch API; do **not** add a second notification engine or treat `rev` as a completion predicate. D0 remains blocked on a reviewed committed AW pin; no shared API adoption happened in this checkpoint.

## Pre-pin MCP ACK prefix correctness — 2026-11-16

- Until AW's reviewed shared owner-delivery API is repinned, dotlink's existing output delivery fence must admit only the **contiguous** raw bytes proven by a successfully projected `jobs_read` page. Authoritative byte interval is `[offset, next_offset)`; disjoint pages do not bridge gaps. A page starting at zero establishes the first delivery proof, including an empty page's zero-prefix receipt. Model-requested `jobs_ack(offset)` may never exceed that verified prefix, even if a later page has already been read.
- Delivery proof remains isolated to the exact Job/node/stream. Underlying shared JobManager and independent MCP Task/replay leases continue to own physical release; a model-visible page never implicitly ACKs. Preserve the prior positive-offset over-ACK diagnostic. This temporary adapter fence is **not** a second durable ownership engine and will be replaced by AW's canonical `record_delivered_output` after D0.
- Verified with shared JobManager direct output and real stdio MCP `jobs_read → jobs_ack` tests. No change to native Task release, hosted response delivery or transport authority.

## Exact-node output observation — 2026-11-16

- A finite Process/node `jobs_read(wait_ms>0,offset>=0)` watches the exact retained stream: first bytes, newly available continuation, stream EOF, or terminal settlement; a short deadline returns current truthful state without changing execution timeout. Observation alone never ACKs.
- An exact Graph node may become terminal skipped/failed without ever attaching output. Its read wait must stop on **that node's** terminal state rather than waiting for an unrelated sibling or whole-Graph terminality. If the stream is permanently absent, say so; do not fabricate an empty byte page, delivery watermark or retryable `not available yet` outcome. AW's pending shared adapter implements the same gate before adoption.
- Current dotlink `Notify` remains the only observation source on the pinned AW revision. After the reviewed shared repin, use AW's canonical bounded Job watch, preserving target-node semantics and live source grants.

## Complete Job receipts within transport budgets — 2026-11-16

- `jobs_get` must return a complete, serialized-budget-checked Job/node status, **never** a success truncated by the outer transport formatter. If the status cannot fit the authenticated request budget or profile result ceiling, return a bounded explicit error.
- `batch`/Graph handoff after execution admission must preserve the exact Job ref even when full status exceeds the budget. Project the small `j:<id> next=jobs_get no-rerun` continuation when it fits; if the transport cannot fit even that, report the already-admitted state and direct recovery through `jobs_list`. Do not retry/restart an effect to repair its receipt.
- A remote Start duplicate replays the immutable cached MCP receipt if it fits, or a narrower **projection of the same Job identity** for a smaller authenticated envelope. Never change the underlying `RemoteStartReceipt`, infer a new grant, rerun the Graph or return a misleading success fragment. Other Control/Task delivery/authority gates are independent and remain open.

## Exact Control receipts under authenticated transport caps — 2026-11-16

- `jobs_ack` and `jobs_cancel` may apply an effect before reading current Job status. Never pass an oversized **success** to the generic transport result fitter: it can clip status or the reference while still claiming success. Preflight exact serialized MCP bytes. When full status does not fit, use a complete action-specific receipt identifying `j:<id>[/n:<node>]`, acknowledged offset or accepted cancellation, plus `next=jobs_get no-rerun`. A budget too small for either yields an explicit **effect already applied** error, not a claim of no effect.
- Same remote-Start replay identity, no extra mutation. Reproject the immutable cached successful Control receipt only for an exact validated Job reference; mismatched cache identity fails closed and cached failures stay failures. A narrower replay must never replace the original ledger entry or recheck stale current revision before deduplication.
- This is an existing-dotlink MCP projection boundary, not the canonical shared AW Finish/consumer grant migration. `jobs_send` and transport delivery confirmation remain separate gates.

## Public calls

- `jobs_get({ref?,rev?,wait_ms?,tail?})`: **settlement-only wait now implemented on dotlink's existing standalone JobManager** (2026-11-16). `rev` is a status token, never a reason to end the wait early; root waits for root, a node waits for that node, and the transport deadline only clamps observation. After reviewed AW pin adoption, Process/Process node omitted `tail` selects **512 raw bytes total across stdout and stderr** while running or terminal; explicit `tail` 0..1024 is *also an aggregate cap*, NOT per-stream. Allocate half to each stream (256+256 default), then reassign unused share to the other. Bound final projected text/headers inside result cap; exact byte spans and truncation/EOF. Graph roots return status/nodes. Implicit unauthorized tail produces status plus `tail=denied`; explicit nonzero unauthorized tail fails before revealing bytes. Never auto-ACK.
- `jobs_read({ref?,view?,offset?,limit?,encoding?,wait_ms?})`: signed `offset` for retained byte streams, -N means N newest bytes at pinned stream-length snapshot; positive is raw absolute offset, next always absolute. Preserve existing Agent-history-free dotlink schema, owner proof and truthful incomplete status. No `tail_bytes` alias, no legacy `job_output`.
- `jobs_ack({ref,view?,offset?})`: omitted offset is explicit Control-authorized `Finish` of this exact caller's terminal retained stream(s), with Graph-root all-own-node cleanup; exact offset is prefix `Through` and must be contiguous and proven delivered. Check terminal/unknown and all output owners before mutating; never release Task/replay/other output owner data early. Does not touch independent child Agent/Job.
- `grep`: one shared canonical 0..128 per-side `context`, `before`, `after`, direct/Graph parity, exact-file and directory-tree behavior, bounded candidates/scanning/output/incomplete markers.
- Keep DL-150 compact model text, known fixed tool names and flat `batch` nodes. Do not expose `jobs_*` as model-Graph leaves without separate exact owner/control/wait-cycle qualification; no nested batch.

## Implementation stages

1. Wait for AW shared output ownership, tail/read/ACK primitives, grep schema and AW adapter to pass a coherent targeted checkpoint. Obtain a reviewed, available AW Git commit; no speculative pin updates.
2. Dotlink MCP schema + adapter reuse existing `src/mcp/tools/jobs.rs`, `src/jobs.rs` and `src/mcp/tools/shared_native.rs`; preserve local authority, hidden replay/delivery and native MCP Task owners.
3. Qualify combined default/explicit tail across both streams, signed offsets and exact continuation, quick finish vs prefix ACK, Job/node/Graph-root reads, replay/Task owner cleanup, revoked/unknown, live waits, response windows, file/dir grep 0/128 and incomplete results.
4. Run grouped all-feature targeted Rust tests, strict Clippy, formatting, and only newly affected Nix/config checks. Native Windows/macOS and full real AW-dependent flake remain separate honest release gates. No commit/push/release unless approved.
