# dotlink companion — AI-native operations, Jobs and Agent transcript

Author: Prasanna Loganathar
Co-authors: ChatGPT GPT-6, ChatGPT GPT-5.6 Sol
Owner: @pvl

Status: **DESIGN RESOLVED companion / implementation not authorized** (2026-11-10).

The **one canonical vNext design** is in the new AW plan:

- `/home/pvl/spaces/abird/src/aw/.agents/plans/ai-native-operations-agents-jobs-transcript-2026-11-10/README.md`
- `/home/pvl/spaces/abird/src/aw/.agents/plans/ai-native-operations-agents-jobs-transcript-2026-11-10/DESIGN.md`
- `/home/pvl/spaces/abird/src/aw/.agents/plans/ai-native-operations-agents-jobs-transcript-2026-11-10/TODO.md`
- `/home/pvl/spaces/abird/src/aw/.agents/plans/ai-native-operations-agents-jobs-transcript-2026-11-10/NEXT-AGENT.md`

**Read AW DESIGN.md for authoritative semantics; this file only constrains dotlink integration.** Never duplicate the same Graph/Job/policy specification in the dotlink tree.

Preserve, without edits:

- original sealed dotlink `dotlink-ai-native-filesystem-2026-10-31/` (DL-150);
- earlier dotlink `ai-native-job-control-wake-2026-11-10/`;
- earlier AW Jobs/wake plan;
- existing dotlink Profile/sandbox/replay/MCP Task and platform release gates.

## Public model surface and operation normalization

Standalone dotlink retains DL-150's 13 small native filesystem leaf tools, the exact prepared `bash`/`pwsh`/`powershell` interpreters available on that platform, and `batch`. New grouped `jobs({op,...})` is a host **model-wire** projection; its allowed op values must be filtered to exact granted capabilities and per-op arguments validated before preparation.

The canonical `[operation,args]` tuple is the **internal/Graph/SDK language**, not a second advertised provider tool. `jobs({op:"get",id:"j42"})` resolves to trusted `jobs.get`, while the original provider wire name/arguments/ID remain immutable journal/replay evidence. Do not advertise both forms simultaneously or embed all nested tool schemas into batch.

Standalone dotlink **does not** advertise/implement invented `agents.run`, `agents.read`, durable AW Agent IDs, Session/branch transcript authority or parent model wake. A **real** AW host may transport authorized Agent operations and wake, but AW remains their Agent/Session/TurnJournal/lineage/disclosure owner.

## No weaker trust boundary

- Consume reviewed shared aw-tools/aw-tools-fs-{unix,win} from an exact AW Git revision. No copied native filesystem, Graph, JobManager or alternate event database.
- New `Effect::Control` and the `batch` outer Control admission must *not* grant filesystem Write/Process: every Graph leaf is separately preflighted and rechecked before effects, including conditional/possibly skipped nodes.
- The grouped `jobs` public name alone grants nothing; exact kernel-safe per-op source/epoch/actor/site/effect and resource grants remain mandatory. Read-only native-only Graph and explicitly granted Job controls must work without hidden shell access.
- `jobs.get` reads compact status; `jobs.output` reads bounded raw bytes; `jobs.ack` releases only exactly owned delivered output ranges; `jobs.send` shell stdin requires actual owned open Process input and original sandbox/launcher authority.
- `jobs.get(wait_ms)` awaits meaningful status changes and `jobs.output(wait_ms)` awaits requested bytes or EOF without changing execution timeout, holding global execution locks or losing wakeups.
- Preserve DL-150's immutable Graph `[tool,args,when?]`, captured node semantic result/work ceilings, `more`/`incomplete` and status/dependency predicates. Outer MCP envelope fitting may hand off the same Job but may not re-page/re-execute a node or change `when` decisions.
- Preserve original OpenAI tunnel replay/delivery, native MCP Task result owner and multi-owner ACK; no duplicate Job on redelivery, mismatched request identity fails closed.
- Keep `powershell` distinct from `pwsh` when an actually authorized launcher exists. Do not replace one interpreter with the other under a compatibility alias.
- Retain legacy DL-150 `job_restart` and `job_*` adapter behavior during intentional migration. Optional advanced `jobs.restart` is an explicit new presentation, not a second restart engine.
- Typed `bread(format=mcp)` Graph result must originate from shared aw-tools and keep exact bytes/MIME/hash/current host media disclosure checks. dotlink only handles its transport projection.
- Dotlink currently has a factual shell/Job completion record and internal status waiters; it does **not** automatically wake AW. Without an authenticated AW parent, advertise neither `wake` nor an apparent live subscription that cannot deliver. No invented ownership or cross-process Job resurrection.

## Durable transcript boundary (AW only)

`agents.read` belongs solely to AW SessionStore/branch authority and current projection permissions. One AW-owned `filter` field replaces `include` plus top-level `tools`: omitted/`"default"` yields latest committed public result; `"none"` requests all *currently authorized sanitized* categories; a custom object matches `result`, `messages`, `tool_calls`, `tool_outputs` booleans and optional exact `tools` names; `exclude:true` instead removes matches from currently authorized public content (category-wide or selected tool records). All modes retain AW's redaction, byte/Turn limits, current disclosure and exact pagination. A dotlink bridge only forwards the validated already-authorized projection; it must not implement its own filter logic or disclose provider-private reasoning/system/credential/hidden-context content.

`jobs.output` remains source-byte Job stdout/stderr/result with Job output ACK and replay ownership. `agents.read` remains sanctioned historical Turn/ToolRecord projection without Job ACK. Do **not** copy Session history into dotlink's Job/session spool, replay ledger or local cache. A bridge forwards only already-authorized projected data after AW validates it.

## AI-native DL-150 efficiency gate

The original DL-150 is normative for short schemas, one-concept-per-tool filesystem calls, exact paths, compact status/output, unambiguous Graph dependencies, no request echoes and bounded truthful continuation.

Before changing the advertised public dotlink schema, compare:

1. **Actually generated** model tool-schema bytes and tokenizer estimate for authorized profiles, especially grouped `jobs` vs current `job_*`.
2. Representative model workflows: read, 3-node read-only Graph, shell background/status/output, read-only Job cancel/ACK, and error/unknown cases.
3. First schema-valid tool choice, model/tool round count and returned result tokens. More powerful semantics do not by themselves prove fewer model tokens.

If a grouped model projection raises tool errors or token cost materially, revisit the **new AW vNext design** before schema cutover; do not re-open sealed DL-150 or add duplicate advertised aliases for convenience.

## Integration sequencing

1. Obtain implementation authorization; inspect live AW/dotlink source, dirty work, current permission and shared Git pin.
2. Consume reviewed AW Control/operation resolver and typed Job result work through one pinned revision, once it actually lands.
3. Integrate thin dotlink grouped Job projection with exact per-op authority and original provider/replay provenance.
4. Preserve native MCP Tasks, original shell/job_* transition, timeout/handoff/output/ACK and protected sandbox access.
5. Integrate factual shell completion event projection only if a real AW parent integration needs it; otherwise keep passive Job completion.
6. Run existing product tests and model-schema/real workflow metrics, plus native platforms when available; keep unavailable Windows/macOS/Bubblewrap smoke gates open.
7. Never edit older sealed plans or change old current implementation merely to match a draft; any deliberate public cutover/commit/push/release requires separate approval.
