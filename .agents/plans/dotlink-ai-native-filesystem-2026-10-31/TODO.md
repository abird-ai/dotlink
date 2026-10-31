# DL-150 — AI-native tool surface + canonical Job graph runtime

Base: `84cd9b7`

Status: **design sealed** after a fresh correctness/regression/minimality review for v1 plus the specified v2 canonical Job/Graph runtime and shell nodes; implementation intentionally not started.

## Committed

### `c6f0e1c` — initial AI-native filesystem specification

- [x] Recorded the approved core tool split, token-efficiency rules, lifecycle
  primitives and implementation roadmap.

### `9b7d4b4` — read limits and intermediate singular-edit design

- [x] Kept read/write byte ceilings as runtime policy rather than routine model
  parameters.
- [x] Specified long-line read continuation and the then-intermediate singular
  edit shape, later superseded by `f09790d`.
- [x] Specified pre/post-publication mutation outcome handling.

### `6c1cb16` — status, paging, progress and initial batch grammar

- [x] Added one canonical outcome grammar and known/unknown paging conventions.
- [x] Added cumulative known-total/open-ended progress semantics.
- [x] Recorded the initial shallow batch orchestration design.

### `f28ce2c` — compact `ops` execution graph

- [x] Replaced shallow stages with positional graph nodes, backward-only status
  dependencies, joins, parallel roots and decision branches.
- [x] Kept nested tool arguments structured while limiting text syntax to the
  small dependency expression.

### `60b348d` — simplified fresh model-facing surface

- [x] Made query modifiers orthogonal to primary outcomes and made bare batch
  dependencies require complete success.
- [x] Removed advertised patch best-effort, cp recursive and asymmetric grep
  context knobs; unified paging and path composability.

### `f09790d` — single-file multi-replacement edit hot path

- [x] Made `edit(path,edits)` batch independent exact substitutions against one
  original snapshot with overlap rejection and one atomic publication.
- [x] Kept one advertised shape for one or many edits.

### `ebb5b84` — sealed v1 AI-native filesystem design

- [x] Sealed the v1 public surface, token/result grammar, concurrency boundary
  and deferred-feature list before implementation.

### `5942393` — shell write hints and durable shell-node v2

- [x] Added advisory shell `writes` scopes and specified graph-owned Bash/
  PowerShell nodes with process-terminal branching; later design converged on
  the canonical shared Job/Graph/Node runtime.

### `2a255f3` — durable batch controls/checkpoints

- [x] Added automatic resume, explicit restart generations, compact polling,
  idempotent cancellation and fail-closed node checkpoint semantics.

### `8a3e7bb` — unified durable lifecycle under `job_*`

- [x] Replaced parallel batch control concepts with the shared `job_*` namespace,
  one durable ID space, generic restart, and node-aware output/ack semantics.

### `a156142` — canonical Job/Graph/Node runtime

- [x] Made one Job owning one Graph the canonical execution architecture and
  defined shell calls as one-ShellNode Jobs rather than nested child Jobs.
- [x] Established `pwsh` as the modern PowerShell name; the final sealed design
  below further makes Windows `powershell` a distinct first-class interpreter.

### `bec528f` — finalized canonical Job graph design

- [x] Unified `batch(ops)` and `job_start(ops)` on one Graph grammar/runtime,
  sealed the three exact shell interpreter capabilities, and centralized shared
  Job lifecycle/replay/output semantics before the final correctness review.

## Implementation roadmap

### Phase 1 — shared discovery/metadata architecture

- [ ] Introduce one shared metadata model with `size`, `mtime`, `ctime`,
  `btime`, `mode` and platform-precise availability.
- [ ] Refactor descriptor-rooted traversal so `hidden`, `gitignore`,
  `depth`, budgets and incomplete reasons are single-sourced.
- [ ] Introduce shared selector, ordering and compact-rendering primitives.
- [ ] Preserve current deny/protected-path and symlink non-traversal invariants.
- [ ] Preserve file/path-level mutation coordination as the correctness baseline;
  do not add model-facing range/revision coordination knobs in v1.

### Phase 2 — `ls` + `find`

- [ ] Shrink `ls` to immediate-directory inspection only.
- [ ] Add `find` for recursive glob/regex/kind/depth/time discovery.
- [ ] Add compact path output and opt-in ordered metadata columns.
- [ ] Add `mtime`/`ctime`/`btime` filtering and sorting.
- [ ] Add `hidden` and `gitignore` consistently to both.
- [ ] Preserve bounded globally correct selection/paging.

### Phase 3 — `grep` + read-side schema cleanup

- [ ] Align `grep` with `depth`, `hidden`, `gitignore`.
- [ ] Keep `glob` as the content-search path prefilter.
- [ ] Replace advertised `before`/`after` with one bounded symmetric
  `context`; use follow-up `read` for asymmetric inspection.
- [ ] Make content results always line-numbered and remove advertised
  `numbered`.
- [ ] Convert files/count results to compact deterministic text.
- [ ] Stop advertising `read.max_read_bytes`; retain it as runtime/profile
  policy and surface the effective limit only in actionable errors.
- [ ] Replace advertised read `offset`/`limit` with
  `line`/`lines`/`byte` and support continuation inside oversized UTF-8
  lines.
- [ ] Use the same continuation cursor when the profile or transport result
  ceiling ends a page.
- [ ] Define mid-read I/O faults as error-marked incomplete results with any
  safely obtained prefix and cursor.

### Phase 4 — text mutation schema/output cleanup

- [ ] Make `edit(path,edits)` the single-file hot-path primitive, with compact
  tuples `[old,new,all?]`.
- [ ] Resolve every tuple against one original snapshot; `all` uses
  left-to-right non-overlapping matches; reject ambiguous non-`all` matches and
  overlaps between selected ranges before staging.
- [ ] Build/verify/publish the file once so several independent replacements
  remain one atomic visible mutation.
- [ ] Keep contextual, dependent, overlapping and multi-file editing
  responsibility in strict `patch`.
- [ ] Keep one advertised edit shape even for a single replacement; do not add a
  top-level singular shorthand.
- [ ] Keep legacy multi-file edit objects and the temporary singular edit shape
  only as compatibility deserialization if needed.
- [ ] Replace verbose successful mutation receipts with compact outcomes where
  no additional information is needed.
- [ ] Define one shared mutation outcome model: not committed, committed,
  unknown, and explicit mixed multi-object commit state.
- [ ] Ensure write/edit/binary staging failures never expose partial target
  contents; over-limit writes fail before staging/publication.
- [ ] Keep Codex patch grammar, advertise strict-only `patch(patch)`, and
  leave move/delete to dedicated tools.
- [ ] Accept legacy `best_effort` only at the compatibility boundary if a
  migration window is required.

### Phase 5 — lifecycle primitives

- [ ] Design/implement `cp(from,to)` through shared mutation/staging:
  files copy as files, symlinks as links, directories as complete bounded trees;
  any missing destination-parent suffix is part of the same atomic publication.
- [ ] Design/implement same-filesystem `mv(from,to)`; never implicit
  copy+delete and require the destination parent to already exist.
- [ ] Design/implement `rm(paths,recursive=false)` with full-request preflight
  and safe recursive namespace removal; reject duplicate/alias/ancestor-
  descendant-overlapping roots before any mutation.
- [ ] Auto-create missing destination parents only for `cp`, inside the same
  staged publication unit; `mv` is strictly one rename or no namespace change.
- [ ] Do not add glob expansion, destination inference or implicit overwrite.
- [ ] Add replay protection/capability visibility for all lifecycle mutations.

### Phase 6 — canonical status, paging and progress

- [ ] Implement primary outcomes `ok`, `unchanged`, `fail`, `mixed`,
  `unknown`, `skip` plus orthogonal query modifiers `more` and
  `incomplete`; keep `unknown` as the sole fail-closed ambiguity name (no
  `maybe` alias).
- [ ] Make `mixed` represent multiple material terminal outcome families,
  including mixtures that contain unknown components; use `unknown` only when
  all material components are unknown, and treat `skip` as structurally
  neutral when another material outcome exists.
- [ ] Use one paging grammar based on actual follow-up argument names
  (`more offset=200`, `more line=42 byte=...`), with optional known page
  fractions only when already cheap.
- [ ] Keep `more` (recoverable continuation) distinct from `incomplete`
  (coverage not guaranteed), including combined states.
- [ ] Separate exact data and status into distinct MCP content blocks when status
  text would corrupt returned bytes/text.
- [ ] Standardize progress as cumulative snapshots: fixed total when known,
  omitted total when open-ended, stable units and terse phase messages.
- [ ] Ensure progress never carries the only copy of result data.

### Phase 7 — shared Job/Graph/Node runtime + native `batch`

- [ ] Introduce the canonical execution model: one `Job` owns one immutable
  `Graph`; a Graph owns one or more `Node`s; nodes are never nested Jobs.
- [ ] Generalize the existing JobManager around a lightweight shared Graph/Node
  runtime without duplicating replay, cancellation, retention or Task state.
- [ ] Add the two shared Graph constructors together: `batch(ops)`
  optimistically waits, while `job_start(ops)` durably exposes the same Graph
  immediately.
- [ ] Represent each operation as compact `[tool,args,when?]` with stable
  1-based coordinates derived from position.
- [ ] Default omitted `when` to previous **complete** success
  (`ok|unchanged` with no `more`/`incomplete`); use `*` for an independent
  root.
- [ ] Implement backward-only dependency expressions with `&`, `|`,
  parentheses, exact status/modifier predicates and `N.*` for any terminal
  outcome; add no status classes such as `.err` or `.done`.
- [ ] Reject forward references/cycles by construction and evaluate conditions
  only after all referenced operations are terminal.
- [ ] Support sequence, fan-out, joins and status-based decision branches without
  variables, loops, output substitution or nested batches.
- [ ] Mark false branches as `skip condition`; expected skips do not make an
  otherwise successful Job an error.
- [ ] Runtime-validate Graph-node args against the already registered tool schema
  instead of duplicating every schema into `batch` or `job_start`.
- [ ] Add one central Graph-node eligibility/version property; initially restrict
  v1 Graph nodes to exact-result native filesystem tools and reuse it for both
  `batch` and `job_start` validation; keep typed-content `bread` direct-only.
- [ ] Add deterministic aggregate result budgeting; child tools must emit native
  `more` cursors rather than being silently truncated by the outer Job/Graph.
- [ ] Aggregate `more`/`incomplete` modifiers explicitly and never duplicate
  exceptional nodes in both detail blocks and summaries.
- [ ] Add compact fixed-total Graph/Job progress (`3/12 2 ok`) and forward known
  (`412/1000 copy`) or unknown (`918 scan`) node progress snapshots.
- [ ] Replay-protect every `batch`/`job_start` Job creation regardless of node
  mix, and derive deterministic per-node replay identities so redelivery cannot
  duplicate Graph work or repeat committed mutations.
- [ ] For `job_start`, commit durable Job identity/exposure/replay state after
  complete Graph preflight and before any root node starts; lost receipts must be
  recoverable through `job_list` without duplicate creation.
- [ ] Enforce hard operation-count, parallel-width, aggregate byte/work and
  result ceilings.
- [ ] Add one process-wide Graph node-runner permit budget shared by every Job;
  acquire a permit before `pending -> claimed`, keep a per-Job parallel-width
  ceiling, and keep active-Job/runner/retained-Job/spool budgets distinct.
- [ ] Decouple Job controls from shell permission: any runtime that can expose a
  Job must expose the controls needed to observe/cancel/consume it, including
  native-only read/write profiles.
- [ ] Generalize abandoned-response safety: durable-request transports retain
  hidden replay ownership; transports without durable request identity expose
  the same Job through `job_list` when the last undecided response disappears.
- [ ] Project a Job as native MCP Task only when its exact eventual ordinary
  tool result is guaranteed to fit the Task-result budget; otherwise use the
  same Job through `job_*` without truncation or semantic rewriting.

### Phase 8 — shell naming + write hints

- [ ] Advertise three distinct interpreter capabilities when actually available:
  `bash`, modern PowerShell `pwsh`, and Windows PowerShell `powershell`.
- [ ] Bind each tool/node to its exact prepared interpreter; never alias or
  silently fall back between `pwsh` and `powershell`.
- [ ] Add optional `writes` path scopes identically to all advertised shell
  arguments; omission means unknown, `[]` means expected no writes.
- [ ] Resolve relative hint paths from effective shell `dir`; use no globs,
  shell expansion or environment interpolation.
- [ ] Treat scopes as advisory scheduling metadata only; never as authorization,
  sandbox policy, replay identity or a correctness proof.
- [ ] Preserve each shell node's `writes` arguments unchanged inside
  `batch(ops)` and `job_start(ops)` so durable/graph execution does not discard
  scheduler information.
- [ ] Document create/modify/delete/rename semantics and conservative subtree
  scope behavior.

### Phase 9 — one-node shell Jobs + v2 shell Graph nodes

- [ ] Route ordinary `bash`, `pwsh` and `powershell` through the canonical
  one-node Job Graph path rather than separate shell execution/durability
  machinery.
- [ ] Keep the one-node path lightweight; architectural unification must not add
  unnecessary persistence/allocation to short synchronous shell commands.
- [ ] Add `bash`, `pwsh` and `powershell` as v2 ShellNodes; keep `job_start`,
  job/task controls and nested `batch` excluded from Graph node dispatch.
- [ ] Preserve the already-shared `batch(ops)` / `job_start(ops)` Graph
  constructors when adding ShellNodes; do not fork shell-specific graph parsing.
- [ ] Map ShellNode process outcomes canonically everywhere: successful exit ->
  `ok`; known non-zero/timeout/launch-failure/cancellation -> `fail`; ambiguous
  state -> `unknown`.
- [ ] Keep Graph outcome separate from the direct MCP envelope: a normally
  launched process that exits non-zero is Graph-`fail` but preserves the
  existing direct-shell MCP-success process result with `success=false`;
  pre-execution/timeout/internal failures remain MCP errors as appropriate.
- [ ] Keep compact ShellNode exit/success/timeout/error metadata plus separate
  bounded stdout/stderr streams; shell failure must never imply rollback of
  shell side effects.
- [ ] Make the outer Job the sole durable owner automatically when execution
  outlives a synchronous transport window; no per-node durability flag and no
  ShellNode escapes as an independent user-visible child Job/Task.
- [ ] Keep the graph scheduler running after handoff so dependents continue when
  long shell nodes terminate even without active client polling.
- [ ] Persist/retain per-node `pending/claimed/running/terminal` state and
  deterministic replay identity.
- [ ] Persist `claimed` before shell launch and bind the child to a deterministic
  graph+node runner identity; ambiguous launch state must fail closed, never
  relaunch.
- [ ] Checkpoint every terminal node before unlocking dependents; later failures
  or lost responses must never re-execute terminal predecessors.
- [ ] Redelivery reattaches to the same running child or marks it `unknown`
  rather than restarting it.
- [ ] Keep Job/Graph durability scoped to one live dotlink Job session; do not
  resurrect Job IDs or Graphs after process exit/crash/session replacement.
- [ ] Preserve cross-process **request replay safety** separately: either prove a
  retired-session request cannot be redelivered to a replacement process or use
  a bounded protected replay tombstone committed crash-atomically before root
  execution; bind request identity + semantic hash, retain through the maximum
  redelivery horizon, fail closed as `unknown`, and never persist the full Graph
  merely for deduplication.
- [ ] Reuse bounded shell spooling for child stdout/stderr and deterministic
  aggregate result budgeting/paging.
- [ ] Keep one `job_*` lifecycle and one ID namespace for all exposed Jobs;
  derive compact status labels from Graph shape/tool identity rather than
  durability kinds, and do not add parallel `batch_get/cancel/output/ack` tools.
- [ ] Prefer concise advertised `id` in new job control schemas while accepting
  legacy `job_id` at the compatibility boundary.
- [ ] Make `job_get(id)` a cheap Graph-status snapshot only; label one-node Jobs
  by node tool/interpreter and multi-node Jobs as `graph`, without repeatedly
  dumping retained bulk output.
- [ ] Define resume as automatic reattachment to the same job/checkpoint ledger;
  do not add a redundant `job_resume` tool.
- [ ] Extend `job_output`/`job_ack` with optional Graph `node`: one-node Jobs may
  omit it, multi-node Jobs require it; NativeToolNodes expose `result`, while
  ShellNodes expose stdout/stderr/result with natural stream inference.
- [ ] Add generic replay-protected `job_restart(id)`: any terminal exposed Job
  creates a fresh Job from its retained immutable Graph while retaining the
  source Job as history.
- [ ] Revalidate the entire retained Graph against current tool visibility,
  authority and exact prepared interpreter availability before a restart can
  begin; old Job authority must never authorize new work.
- [ ] Make Jobs runtime-session scoped rather than per-client; callers sharing
  one authorized LocalMachine/runtime share one exposed Job namespace.
- [ ] On incompatible execution-authority change, revoke the old Job session as
  a whole: stop new starts, mark pending nodes `skip revoked`, terminate
  ShellNode trees, let active native mutations settle safely, and settle the Job
  lifecycle cancelled/revoked before replacing the session.
- [ ] Job cancellation records intent before action, prevents pending node
  starts, marks them `skip cancelled`, terminates running ShellNode trees, and
  preserves safe native-mutation terminal semantics.
- [ ] Repeated cancellation is idempotent; cancelling a terminal run is a no-op
  returning its terminal state.
- [ ] Forward compact Job/Graph progress while ShellNodes run without inventing
  a shell percentage.
- [ ] Allow `writes` hints to reduce known scheduling conflicts inside the graph;
  unknown hints may be treated conservatively and never become authority.
- [ ] Do not enable ShellNodes by a registry bit alone; exact interpreter
  binding, runtime-session Job control, durability, replay, cancellation and
  output retention ship as one coherent v2 feature.

### Phase 10 — compatibility, measurements and docs

- [ ] Keep compatibility strictly at wire/deserialization boundaries.
- [ ] Do not publish an intermediate `job_start` regression: advertise the new
  `job_start(ops)` shape only when ShellNodes are ready; temporarily accept the
  old shell-shaped `job_start(command,...)` request only at deserialization and
  translate it to one ShellNode.
- [ ] Measure exact serialized advertised-tool schema bytes before/after,
  including the explicit `edits` vs abbreviation tradeoff.
- [ ] Record a representative tokenizer estimate for the same serialized surface.
- [ ] Add a repeatable baseline/regression gate for permanent schema cost.
- [ ] Measure representative `ls`/`find`/`grep` result token cost
  before/after.
- [ ] Update Architecture, Security and user-facing documentation only where the
  final behavior requires it.
- [ ] Remove stale duplicated tool-contract prose from older live docs rather
  than maintaining conflicting specifications.

## Tests and verification

- [ ] Unit tests for shared metadata semantics on supported platforms.
- [ ] Unit tests for `hidden` and `gitignore` parity across discovery tools.
- [ ] Unit tests for glob/regex/depth/kind/time selectors.
- [ ] Unit tests for compact escaping/rendering and result-budget truncation.
- [ ] Read continuation tests for a single UTF-8 line larger than
  `max_read_bytes` and larger than the transport result budget.
- [ ] Read I/O fault tests proving useful prefixes are marked incomplete/error
  and never presented as complete data.
- [ ] Write failure-injection tests before staging, during staging, before
  publication and after publication, asserting the reported commit state.
- [ ] Batch mutation failure-injection tests proving mixed commit ledgers are
  exact and never hidden behind a generic error.
- [ ] Unit tests for globally correct sorting/paging under bounded traversal.
- [ ] Path-composability tests: every `find` result is directly reusable as a
  tool path; `ls` intentionally returns basenames.
- [ ] Status grammar tests for primary success/failure/mixed/unknown/skip
  outcomes plus orthogonal `more`/`incomplete` modifiers.
- [ ] Paging tests for one canonical cursor grammar, known total, unknown total,
  final page and `more + incomplete` combined state.
- [ ] Progress tests for fixed-total, unknown-total and phase-only updates,
  asserting cumulative/monotonic semantics.
- [ ] Shell `writes` schema/normalization tests: omitted vs empty, relative
  resolution, subtree scopes and no glob/shell expansion.
- [ ] Shell capability tests proving `bash`, `pwsh` and `powershell` are exposed
  independently only when their exact prepared interpreters are available, with
  no alias/fallback between PowerShell generations.
- [ ] Tests proving shell `writes` does not change authorization/sandboxing and
  inaccurate hints cannot grant access.
- [ ] Shared Graph-parser tests proving `batch(ops)` and `job_start(ops)` accept
  the same valid graphs/reject the same invalid graphs and never permit nested
  `batch`, `job_start`, `job_*` or Task-control nodes.
- [ ] Graph execution tests for complete-success default sequencing, multiple
  roots, fan-out, joins, modifier/status branches, condition skips and
  backward-reference validation.
- [ ] Assert paged/incomplete child results do not satisfy a bare dependency.
- [ ] Batch replay tests proving committed mutation nodes are not re-executed
  after response loss/redelivery.
- [ ] Batch aggregate-result-budget tests proving pageable children return
  deterministic native `more` cursors independent of completion order.
- [ ] V2 shell-node graph tests for exit-0 success, non-zero error branch,
  timeout, launch failure, cancellation, joins and downstream skips.
- [ ] V2 durability/replay tests proving transport handoff does not mark a shell
  node terminal, the scheduler continues after handoff, terminal predecessors
  never rerun, and redelivery never starts a duplicate process.
- [ ] Process/session-boundary tests proving controlled shutdown/reload revokes
  the old Job session, replacement runtimes do not resurrect old Job IDs, and
  stale spool/session directories are cleaned rather than treated as live Jobs.
- [ ] Cross-process replay tests proving retired-session redelivery is either
  protocol-impossible or rejected from a minimal replay claim/tombstone without
  re-executing the Graph.
- [ ] Security tests proving retained live-session Graph definitions/spools stay
  in protected private state, do not leak commands/stdin/args into routine logs,
  and are cleaned with Job/session retention.
- [ ] Shared durable-control tests for compact shape-derived `job_list`/`job_get`,
  idempotent `job_cancel`, cancel races, automatic resume, generic `job_restart`
  generation/history retention, current-authority revalidation and replay-safe
  restart delivery.
- [ ] Lost `job_start` response tests proving its durable ID is discoverable and
  the Graph is never created/executed twice.
- [ ] Abandoned optimistic-`batch` tests proving local transports expose the
  same Job when the last response attempt disappears while durable-request
  transports retain replay ownership instead of duplicating execution.
- [ ] Job capability tests proving native-only runtimes expose the shared
  `job_*` controls without shell authority.
- [ ] Global runner-admission tests proving many Graph Jobs cannot exceed the
  process-wide running-node/process budget.
- [ ] Native Task projection tests proving Tasks are used only when the exact
  eventual ordinary result fits; otherwise the same Job uses `job_*`.
- [ ] Typed-result tests proving `bread(format=mcp)` remains direct-only until a
  content-aware retained-result representation exists.
- [ ] `job_output`/`job_ack` tests for one-node implicit node 1, required
  multi-node selection, ShellNode stdout/stderr/result, NativeToolNode result,
  paging, acknowledgement and cleanup.
- [ ] V2 output-spool tests proving large ShellNode output remains
  bounded/pageable under one durable Job owner.
- [ ] Edit tests for multiple independent substitutions, multiline tuples,
  `all`, ambiguity, duplicate/overlapping ranges, order-independence and one
  atomic publication.
- [ ] Strict-patch tests proving legacy best-effort behavior is absent from the
  advertised schema.
- [ ] `cp` tests proving directories copy as complete bounded trees without a
  recursive flag.
- [ ] Grep tests for one symmetric `context` field and compatibility-only old
  asymmetric fields if retained.
- [ ] Mutation race/alias/replay tests for `cp`, `mv`, `rm`.
- [ ] `mv` tests proving missing destination parents fail without creating any
  namespace entries.
- [ ] `cp` failure-injection tests proving auto-created parent suffixes publish
  atomically with the destination or remain invisible.
- [ ] `rm(paths)` tests rejecting duplicates, aliases and ancestor/descendant
  overlaps before any deletion.
- [ ] Directory-tree copy / recursive-delete protected-path and symlink tests.
- [ ] Cross-device `mv` rejection tests.
- [ ] Cross-platform native Windows/macOS filesystem tests.
- [ ] `cargo fmt --all -- --check`.
- [ ] `cargo clippy --locked --all-targets --all-features -- -D warnings`.
- [ ] `cargo test --locked --all-features` with at least 8 build jobs.
- [ ] `nix flake check -L`.
- [ ] Markdown lint.
- [ ] Full CI matrix green.
