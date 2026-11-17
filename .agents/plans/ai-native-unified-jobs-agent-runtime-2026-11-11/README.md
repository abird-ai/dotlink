# Unified Jobs + Agent runtime — dotlink integration

Author: Prasanna Loganathar
Co-authors: ChatGPT GPT-5.6 Sol
Owner: @pvl

Status: **thin integration design under review** (2026-11-12). AW owns proposed semantics; dotlink `TODO.md` separates standalone finite-Job cutover (after AW A/B contracts), authenticated bridge (after AW E), and final gates. Parallel implementation does not freeze evolving public/API contracts; unchanged native/sandbox primitives are not repeatedly requalified.

Current dotlink handoff: [../../docs/CURRENT_HANDOFF.md](../../docs/CURRENT_HANDOFF.md)
Canonical cross-repo handoff: `/home/pvl/spaces/abird/src/aw/.agents/docs/CURRENT_HANDOFF.md`
Next-agent launcher: [NEXT-AGENT.md](NEXT-AGENT.md)

Full semantic design lives in AW:

    /home/pvl/spaces/abird/src/aw/.agents/plans/
    ai-native-unified-jobs-agent-runtime-2026-11-11/DESIGN.md

This companion defines only dotlink's integration boundary. It does not rewrite sealed DL-150 or the two 2026-11-10 successor plans.

## New standalone dotlink model surface

Keep flat native filesystem and execution constructors:

    ls find grep read write edit patch cp mv rm bread bwrite bedit
    bash pwsh powershell batch

Standalone unified Job controls:

    jobs_list
    jobs_get
    jobs_read
    jobs_ack
    jobs_send
    jobs_cancel

Standalone dotlink does not advertise agent or jobs_subscribe because durable Agent ownership and wake recipients require a real AW Session/branch/Actor host. It also omits constructor wake fields.

When dotlink is used through an authenticated AW bridge, AW may additionally expose:

    agent
    jobs_subscribe

and wake-capable constructor fields **only for sources whose authoritative event owner can serialize registration with event publication**. In initial vNext, AW-owned Agent/Job sources support that contract; **dotlink-owned live Process/Graph sources do not**. AW must reject constructor `wake` targeting a dotlink-owned source **before source execution**, and reject `jobs_subscribe` on an already-running dotlink source **before registering or delivering an event**, even with an authenticated receiving Agent. Passive get/read/control remains supported. A future private source-fenced register-before-start/event-revision/dedup/owner-loss protocol can enable remote wake without adding public tools. AW owns canonical namespace and recipient authority; dotlink never creates Agent state or a general event bus.

## Canonical semantic IDs

Wire projection maps immediately to:

    read        -> fs.read
    write       -> fs.write
    ...
    jobs_get    -> jobs.get
    jobs_read   -> jobs.read

Model-facing Graph reuses the same flat leaf names as direct tools:

    ["read", {...}]
    ["bash", {...}]
    ["agent", {...}]       # AW Agent-owner Graph only; not dotlink standalone

Initial vNext: **no nested `batch` as a Graph leaf on any surface**, including typed
AW SDK/host Graphs. AW `agent` nodes are explicitly allowed with real owned Turn/child
settlement; those Agents may create further authorized Agents under finite depth/cost/
causal budgets. Standalone dotlink exposes no `agent` node and never creates AW Agent state.

Graph admission maps the flat name through the same below-wire semantic resolver (read -> fs.read, etc.) before policy/preparation/journaling.

The original provider wire ToolCall remains immutable evidence.

## Job ownership boundary

dotlink standalone owns/exposes:

- Process Jobs from bash/pwsh/powershell;
- Graph Jobs from batch;
- Graph embedded deterministic n:<1-based Graph coordinate> node streams/results beneath Graph Job refs, with no extra opaque node mapping;
- finite Job retention/ACK;
- MCP Task correlation;
- transport replay;
- process/sandbox custody.

Control/read refs use the shared AW semantics. Standalone has no implicit self, so get/read/ack/send/cancel require explicit finite ref; jobs_list has no ref.

rev is exposed only where it adds control value:

- send/cancel use supplied rev as exact compare-and-act;
- get uses rev as the conditional observation/change token for lost-wakeup-safe wait_ms polling;
- read and ack have no rev: exact ref + offset/cursor/ACK watermark preserve replayable DL-150 paging and idempotent release.
Calls that permit omitted rev remain valid against current state.

Standalone dotlink does not invent domain/principal/project/Agent identity. Its finite refs resolve inside its **authenticated live runtime session** as `j:<handle>` or `j:<handle>/n:<1-based coordinate>`. Every restart creates a **new unforgeable runtime generation**; stale listing cursors/Start proofs fail, old live Process/Graph Jobs are never resurrected, and finite `j:` handles cannot be reused to identify unrelated work across generations. Because it has no current Agent, get/read/ack/send/cancel require an explicit finite ref; no omitted/"." AW self or structural default d:0/i:0/p:0/a:0 can mint dotlink authority. Under an AW bridge those :0 values are fixed AW canonical objects, never caller-relative aliases, and only AW resolves/authorizes them. jobs.subscribe exists only with a real authenticated AW Agent host.

When an authenticated AW bridge is present:

- AW owns fully resolved canonical `/d:<h>/i:<h>/p:<h>/a:<h>[/t:<h>][/j:<h>...][/n:<h>...]` identity;
- canonical handles are already direct immutable identities; no friendly-name alias lookup layer is part of the execution ref;
- AW owns contextual overlay resolution for d:/i:/p:/a:, omitted/".", "^" and t:/j:/n: paths relative to the current Agent;
- AW applies inherit-ancestors/reset-descendants-to-:0 **only to existing provisioned authenticated identities**; missing default principals/projects/root Agents fail closed and are never created by reference lookup;
- AW canonicalizes the request before routing a Process/Graph suffix into dotlink when dotlink owns that execution;
- dotlink never interprets "^", structural defaults, parent/coordinator state, Agent origin or Agent Session semantics.

The **shared aw-tools primitives** must permit both Local and delegated execution callbacks for **every** prepared semantic operation and Graph node without class-based prohibition. Dotlink itself remains a thin concrete filesystem/Process/Graph executor: standalone it has **no Agent provider or AW execution-policy engine**, so it executes locally. With a connected AW host, **AW may choose Local or DelegateAgent for filesystem, Process, jobs.*, batch and agent constructors**, and must pass the resulting exact prepared native/Process/Graph/Job effect to the **same original owner**. This is not a second dotlink Agent runtime: the original sandbox/Process/Graph manager, status, output and replay identity remain authoritative; Agent prose cannot fabricate native effects.

AW host policy defaults to Local with zero auxiliary inference; advanced configuration may choose one delegated operation, DelegateAgent(Graph), or activation-time DelegateAgent(Node), with optional one-shot whole-Graph guardian. Shared preflight captures all permitted candidate routes plus conservative union footprints before roots; dynamic Node route input is restricted by AW's deterministic dependency-closed GraphActivationContext and can only narrow that envelope. AW owns model/guardian/delegate policy and coordinator/Turn state; dotlink owns only concrete Process/native Job/Graph work.

**Standalone/provider-facing dotlink Graph remains on reviewed DL-150 native/shell leaves.** An AW-hosted model Graph additionally admits `agent` only when AW owns its Agent/Turn/effect settlement. **Nested `batch` is rejected by every Graph entrypoint**, including SDK/host and **AW Agent descendants still owned by an active Graph node**; a direct Agent-issued `batch` cannot bypass the active Graph scope. A new independently authorized Turn after Graph settlement can launch its own top-level Graph. Advanced `jobs.*` Graph controls are typed AW SDK/host-only after an explicit owner/Control/result/cycle qualification, never implicitly model-exposed or implemented by standalone dotlink. Child Agent depth/cost/causal authority belongs to AW, not dotlink.

dotlink does not own/copy:

- AW principal/project/Agent namespace state;
- AW Session/branch;
- Agent Turn generation;
- Agent Step/model journal;
- Agent transcript/history;
- Agent parent/coordinator or wake recipient authority.

### Unified policy boundary: AW resolution, dotlink enforcement

AW resolves authenticated direct/team-role grant-path evidence from its existing authority stores and composes it with **one pure scoped Policy** of constraints/denies, guardian `Allow/Deny/Ask/Narrow`, execution preferences, limits and disclosure. `aw-policy` itself is not a grant issuer. dotlink must not copy/reinterpret AW team membership, Session/workspace preferences, guardians or model accounts. The bridge conveys only the authenticated already-resolved canonical operation/ref, original request/replay ID, proven actor/source/site/epoch/grant provenance and bounded effective resource/execution envelope—never unsigned model advice as authority.

At the original concrete native/Process/Graph/Job owner, dotlink must independently check its **current local profile/sandbox/launcher/source/site/resource/transport grants and deny precedence**. A remote AW authorization is a required upstream gate, **not sufficient local native authority**. The effective bridge permission is the intersection of both sides' applicable restrictions; unknown/expired domain membership, target owner or source version blocks a new effect. Local means original executor and may point to a remote dotlink host when site is granted; DelegateAgent means AW model coordination, not dotlink provider execution. The existing exact process/input/ACK/replay rules still apply after any policy or route choice.

**Cross-host Start boundary:** AW commits one single-use, replay-bound authorization under current actor/grant/source/site/resource/membership/policy/writer epochs **before each new remote effect/Process stdin attempt**. Its exact proof binds `(AW original invocation + effect attempt, canonical operation/ref/argument digest, target dotlink host/profile + authenticated live runtime generation, source/site/writer epochs, bounded expiry)`. Dotlink checks authentication and **current local launcher/sandbox** and atomically consumes that exact attempt through its **existing replay/Job owner ledger before effects**. Duplicate delivery in the same live generation reattaches to the factual/pending/unknown receipt; expiry cannot outlive the owner's dedupe claim. A different runtime generation rejects the old proof, and AW **cannot mint a replacement** for an ambiguously started attempt merely because dotlink restarted or its response was lost. No old live Job is resurrected. Revoke-before-AW-Start denies; a previously accepted Start may physically launch within its bounded lifetime even if AW is disconnected. **AW offline blocks new Start authorization, not already committed dispatch**. Stronger no-post-revoke-launch semantics require fencing/draining accepted Starts before claiming quiescence. Unknown/expired/mismatched attempts fail closed without a second execution. This is one original owner protocol, **not** another permission DB, paid agent, offline general-purpose grant or extra standalone Local hop.

The dotlink registry/router carries only exact routing identity and minimum signed/current Start provenance needed by its concrete owner, never shadow Agent state or a second scoped policy database. Source-owner event/wake restrictions remain unchanged.

The generic wire/API must not grow node/turn/step fields. Shared handles use the same delimiter-safe canonical lexical grammar as AW. Execution suffixes are owner-typed; Graph nodes use directly derivable `n:<1-based coordinate>` segments (`n:0`/leading zeros invalid), and any `n:<coordinate>/j:<handle>` continuation exists only for genuinely independent durable child work. Agent `t:<handle>`/Step semantics remain AW-owned, and Step has no public path segment.

### Shared Session and coordination boundary

**AW owns all human/session collaboration semantics.** Sharing an AW Session with another user does **not** create a team user, a second root Agent, a different Session owner, or a new dotlink coordination runtime. The existing canonical project `a:0` may be lazily bound to its primary Session/branch; additional Session/branch Agents are AW-owned and created only when independently executed. One shared Session/branch has one AW Agent coordinator/Turn writer, and all participant submissions retain their original authenticated initiator and replay ID.

The shared `aw-tools` core provides only small **host-neutral coordination extension points**—opaque original request/provenance, owner/writer admission epoch, typed accepted/unknown Job/Graph receipt and existing callbacks. `aw-tools-adapter` binds those to **AW's original authorized executor**; neither shared core nor dotlink implements user/team memberships, session-wide audience, shared-draft collaboration, per-participant Agent inbox, provider/payer selection or a new TeamAgent type.

On a bridged call, AW resolves the real Step/request actor, current Session/branch membership/audience, exact writer fences, workspace/domain restrictions, original ToolPolicy/Envelope and payer/provider policy. The **exact original operation/request ID plus a fresh AW Start-authorized actor/source/resource/site/epoch proof** reaches dotlink for each new effect; a stale earlier envelope is not an execution grant. No participant-rights union or Session-owner privilege is inferred downstream. Durable/multi-writer remote sites require real writer Epoch fences—`NotApplicable` is valid only where AW's authoritative backend truly has no corresponding fence. Dotlink independently checks current launcher/sandbox/source/site/Process/Graph authority and returns factual receipts. AW upstream permission is necessary but never sufficient.

Full Session sharing preserves the same AW coordinator and only currently authorized history. Approved-context sharing creates a new sanitized AW Session rather than treating a private Session branch as a disclosure boundary. Member revocation invalidates future commands/reads/inference/wake under current epochs and cannot cause a remote Process to be falsely declared stopped. A stale remote Workspace writer/follower cannot create a second AW Session coordinator or replay an already-effectful request. Moving a personally owned Session into a team workspace requires explicit AW ownership transfer/import with new target identities, not an automatic reaction to sharing.

**Standalone dotlink remains Local** without an AW Agent host and gains no collaboration or paid-inference capability merely by receiving these host-neutral fields. The existing remote source-fenced wake restriction continues to apply; no new cross-host subscription guarantee, model tool, persistent queue or Session store is introduced.

## jobs.read

Replace old model-facing job_output with jobs_read.

Standalone valid views:

Process:

- stdout
- stderr
- result

Graph:

- result
- valid n:<1-based coordinate> result/stdout/stderr

Preserve exact DL-150 retained-byte semantics: Process/Process-backed Graph-node default read is **stdout**; native/Graph default is result. Raw offsets are encoding-independent; EOF/truncation remains immutable; read never ACKs. ACK is monotonic/idempotent exact ref/view/offset with no rev, cannot exceed retained bytes delivered/available to that exact owner's lease, and cleanup waits for every replay/Task/protocol owner.

jobs.list is flat deterministic exposed-Job-root discovery with immutable exposure ordering and an opaque strictly-older cursor; later activity never reorders pages and it is not subtree traversal. jobs.get/read/ack/send/cancel use explicit standalone refs. Process Job refs are `j:<handle>`; Graph nodes are directly derivable `j:<handle>/n:<1-based coordinate>`.

Shared schemas stay strongly typed: ReadView=result|stdout|stderr for standalone resolved kinds; SendInput is exactly {text}|{eof:true}. jobs.send mode is auto|queue|steer|interrupt. **For bridged retries**, AW first looks up the original authenticated invocation claim and uses its stored canonical target, *before* resolving `^` or contextual overlays; dotlink receives only the already accepted exact canonical ref and original attempt. Reparenting cannot retarget a replay. Process `j:<handle>` accepts live stdin only if interactive custody was retained **and current exact Control plus independent `process.input` launcher/source/epoch/resource policy still permits new input under the Process's actual retained original sandbox**. An existing open handle cannot bypass policy revocation/narrowing. Graph interactive `n:<handle>` delegates to node owner. Every effectful SDK/transport/bridge send carries a **stable host-envelope invocation ID** established before effect and reused on retry; uncertain stdin bytes are never blindly written twice. Standalone dotlink does not interpret AW causal budgets, subscriptions or Agent filters; AW owns them. Unsupported inputs/modes reject.

Public finite lifecycle uses only the shared vocabulary subset running|waiting|cancelling|cancelled|settled, with DL-150 outcomes reported separately.

jobs.read never acknowledges.

jobs.ack remains the only explicit retained-byte release watermark operation.

Agent views result/messages/tool_calls/tool_outputs/trace and the optional combined **Agent-only `filter`** are implemented/authorized by AW only. AW supports omitted/`"default"` result, `"none"` all currently authorized public categories, or custom booleans/tools with `exclude:true`; `filter` and `view` cannot be combined. Standalone dotlink does **not advertise** Agent filtering. If authenticated AW bridge transports such a call, it forwards the typed request/filter/cursor/result unchanged after AW's own authority check; dotlink never stores/reconstructs Agent transcript or implements its own disclosure evaluator.

## No compatibility surface

The new **advertised model API** does not preserve:

    job_start
    job_list
    job_get
    job_output
    job_ack
    job_cancel
    job_restart

or grouped jobs({op}) / agents({op}) proposals.

No model-facing restart exists.

DL-150 Job functionality is simplified rather than weakened:

- job_start(ops) -> batch({ops,background:true});
- shell-shaped job_start -> original shell constructor with background:true;
- job_output -> jobs.read;
- job_restart -> no equivalent retained-definition capability; a re-run is a fresh explicit replay-protected constructor under current schema/authority.

Retained Job definitions remain evidence only; a re-run is a fresh explicit constructor, never a retained-Graph execution capability. This remains a **hard pre-1.0 source/API cutover** with obsolete decoders/routes/state adapters removed, no migration/drain/dual-schema layer. However, resetting old transient Job/replay/Task state is allowed **only for an explicitly disposable isolated dev/test runtime after verified quiescence**: ingress closed, original runtime/supervised children stopped, and no live or ambiguously settled external effects, clients, paid attempts, Task/replay responses or output/ACK owner obligations. If this cannot be proven, abort that reset and require operator resolution or a separately isolated new deployment. Never erase unrelated durable Agent/Session data or reinterpret an old request under a new operation.

For the **new** API, finite constructors still reserve replay/exposure recovery capacity before the first effect and preserve DL-150 lost-receipt behavior: ambiguous synchronous delivery stays attached to its current replay owner when available; if no such owner remains within the same running implementation, expose the same execution for jobs.list recovery rather than discarding or rerunning it. Background/automatic handoff returns compact ref + state + rev so the caller can immediately conditional-poll jobs_get without an extra discovery round. Keep new-schema startup/replay/private-artifact/MCP Task/ACK correctness; removing job_restart does not weaken current recovery invariants.

## Security invariants retained

Do not regress:

- protected path/deny precedence;
- descriptor-rooted native filesystem authority;
- Bubblewrap guard semantics;
- explicit shell network/hardware grants;
- current launcher/interpreter identity;
- fixed transport body/time ceilings;
- replay claim correctness;
- exact output owner/ACK leases;
- MCP Task ownership;
- protocol-clean stdout;
- no cross-process live Job resurrection.

## Shared-code boundary

Consume exact reviewed aw-tools / secured backend revisions.

Shared low layer may provide **only concretely needed host-neutral primitives**:

- DomainId/PrincipalId/ProjectId/AgentId/TurnId/JobId/NodeId/Revision where genuinely host-neutral;
- typed immutable-handle ExecutionRef/ResolvedExecutionRef parsing and bounded contextual-path representation;
- typed Job snapshot/read/control request/result types;
- generic Job-kind specialization contracts for Meta/Input/View/Event/default+supported send modes;
- Graph/finite Job runtime;
- typed Text/ExactRead/Bytes/Media values;
- common owner/router traits and small host-neutral PolicyContext/EffectivePolicy **projection** types for exact source/site/effect/resource/route envelope validation (no team-membership service, policy scripting, provider/guardian runtime or mutable permission registry).

AW-specific current-Agent contextual overlays, d:0/i:0/p:0/a:0 structural defaults, "^", parent/coordinator and Agent namespace resolution stay in AW rather than being copied into dotlink.

Do not copy AW Agent/Session code into dotlink.

### AW crate topology and local-first host composition

AW has established **three separate lightweight library boundaries**: `aw-policy` (pure scoped policy constraints/preferences; no grant issuer/database), `aw-session` (backend-neutral Session/coordinator contract over existing `aw-store-api`; no new Session database), and `aw-host` (thin composition of the original Agent/Actor runtime and native tools adapter). The existing `aw-agent-kernel`/`aw-agent-core` own the sole Actor/Turn engine, and `aw-tools` is already the shared native core; **do not create `aw-actor` or `aw-tools-core` crates or make `aw-tools-adapter` a competing Session/Policy manager**.

AW's initial `aw-host::LocalHost` uses the same original native executor and ToolPolicy, with no store/team/guardian/model routing overhead beyond normal local Agent execution. Optional host Sessions use `aw-session` and a selected authoritative backend, not an implicit SQLite dependency on local tool calls. `DelegateAgent` and distributed/team workflows are later features behind real admission/grants.

**dotlink does not depend on or reimplement `aw-host`, `aw-session` or `aw-policy`.** It consumes reviewed `aw-tools` primitives and enforces its own native Process/Graph/tool/sandbox grants when AW submits a bridged, authenticated original execution. A high-level PolicyDecision or shared Session membership is upstream evidence, never a dotlink filesystem/Process grant. A private/local dotlink tunnel needs no AW Session coordination.

## Cutover

This is intentionally a breaking vNext API. Do not implement adapters merely to keep old model tool names alive.

Before cutover, compare **Local**, delegated **single FS/Process/Job-control/constructor**, **one whole-Graph coordinating Agent/Turn**, **dynamic per-node Local/Agent routing**, and optional **one-shot whole-Graph guardian**. Measure actual schema bytes, first-valid tool rate, all parent/guardian/coordinator/per-node provider Steps/tokens/cost/latency, including skipped branches; verify original dotlink native effect, Job/Graph outcome, Process sandbox, replay/ACK, exact output and protected-path invariants in every route. Standalone dotlink has no AW Agent executor, so remains Local until attached to a real AW host.
