# DL-150 — AI-native filesystem tool surface

Base: `84cd9b7`

Status: **design sealed** for v1 plus the specified v2 durable shell-node extension; implementation intentionally not started.

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

## Uncommitted

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
  uncertain, and explicit partial batch commit.
- [ ] Ensure write/edit/binary staging failures never expose partial target
  contents; over-limit writes fail before staging/publication.
- [ ] Keep Codex patch grammar, advertise strict-only `patch(patch)`, and
  leave move/delete to dedicated tools.
- [ ] Accept legacy `best_effort` only at the compatibility boundary if a
  migration window is required.

### Phase 5 — lifecycle primitives

- [ ] Design/implement `cp(from,to)` through shared mutation/staging:
  files copy as files, symlinks as links, directories as complete bounded trees.
- [ ] Design/implement same-filesystem `mv(from,to)`; never implicit
  copy+delete.
- [ ] Design/implement `rm(paths,recursive=false)` with full-request preflight
  and safe recursive namespace removal.
- [ ] Create missing destination parents for `cp`/`mv`.
- [ ] Do not add glob expansion, destination inference or implicit overwrite.
- [ ] Add replay protection/capability visibility for all lifecycle mutations.

### Phase 6 — canonical status, paging and progress

- [ ] Implement primary outcomes `ok`, `unchanged`, `fail`, `partial`,
  `uncertain`, `skip` plus orthogonal query modifiers `more` and
  `incomplete`.
- [ ] Make `partial` represent exact mixed known outcomes and list only
  exceptional indexed items.
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

### Phase 7 — batch execution graph

- [ ] Add first-class `batch(ops)` to the planned native tool surface.
- [ ] Represent each operation as compact `[tool,args,when?]` with stable
  1-based coordinates derived from position.
- [ ] Default omitted `when` to previous **complete** success
  (`ok|unchanged` with no `more`/`incomplete`); use `*` for an independent
  root.
- [ ] Implement backward-only dependency expressions with `&`, `|`,
  parentheses, exact status predicates, `.err` and `.done`.
- [ ] Reject forward references/cycles by construction and evaluate conditions
  only after all referenced operations are terminal.
- [ ] Support sequence, fan-out, joins and status-based decision branches without
  variables, loops, output substitution or nested batches.
- [ ] Mark false branches as `skip condition`; expected skips do not make an
  otherwise successful batch an error.
- [ ] Runtime-validate nested args against the already registered tool schema
  instead of duplicating every schema into `batch`.
- [ ] Add a central batchable-tool property; initially restrict to native
  filesystem tools.
- [ ] Add deterministic aggregate result budgeting; child tools must emit native
  `more` cursors rather than being silently truncated by the outer batch.
- [ ] Aggregate `more`/`incomplete` modifiers explicitly and never duplicate
  exceptional nodes in both detail blocks and summaries.
- [ ] Add compact fixed-total batch progress (`3/12 2 ok`) and forward known
  (`412/1000 copy`) or unknown (`918 scan`) child progress snapshots.
- [ ] Before mutation-capable batch ships, implement deterministic per-operation
  replay identity/ledger so redelivery cannot repeat committed sub-operations.
- [ ] Enforce hard operation-count, parallel-width, aggregate byte/work and
  result ceilings.

### Phase 8 — shell write hints

- [ ] Add optional `writes` path scopes to Bash and PowerShell shell arguments;
  omission means unknown, `[]` means expected no writes.
- [ ] Resolve relative hint paths from effective shell `dir`; use no globs,
  shell expansion or environment interpolation.
- [ ] Treat scopes as advisory scheduling metadata only; never as authorization,
  sandbox policy, replay identity or a correctness proof.
- [ ] Carry the same hint through `job_start` so explicit durable shell launch
  does not discard scheduler information.
- [ ] Document create/modify/delete/rename semantics and conservative subtree
  scope behavior.

### Phase 9 — batch v2 durable shell nodes

- [ ] Add `bash` and `powershell` as v2 batchable nodes; keep `job_start`,
  job/task controls and nested batch excluded.
- [ ] Make bare shell dependency / `.ok` mean actual terminal process success,
  `.err` mean non-zero/timeout/launch-failure/cancellation, and `.done` any
  terminal process state.
- [ ] Keep raw exit/success/timeout/stdout/stderr details in the node result;
  shell error must never imply rollback of shell side effects.
- [ ] Make the outer batch graph the durable execution owner automatically when
  the graph outlives a synchronous transport window; no per-node durability flag
  and no child shell execution escapes as an independent user-visible job/task.
- [ ] Keep the graph scheduler running after handoff so dependents continue when
  long shell nodes terminate even without active client polling.
- [ ] Persist/retain per-node `pending/claimed/running/terminal` state and
  deterministic replay identity.
- [ ] Persist `claimed` before shell launch and bind the child to a deterministic
  graph+node runner identity; ambiguous launch state must fail closed, never
  relaunch.
- [ ] Checkpoint every terminal node before unlocking dependents; later failures
  or lost responses must never re-execute terminal predecessors.
- [ ] Redelivery reattaches to the same running child or marks it `uncertain`
  rather than restarting it.
- [ ] Preserve terminal checkpoints across dotlink restart; if an exact in-flight
  child cannot be safely reattached, mark that node uncertain rather than
  rerunning the graph.
- [ ] Reuse bounded shell spooling for child stdout/stderr and deterministic
  aggregate result budgeting/paging.
- [ ] Add compact durable controls: read-only `batch_get(id)`, idempotent
  `batch_cancel(id)`, and replay-protected `batch_restart(id)`.
- [ ] Make `batch_get` a cheap status snapshot only; it must not repeatedly dump
  retained bulk node output while polling.
- [ ] Define resume as automatic reattachment to the same run/checkpoint ledger;
  do not add a redundant `batch_resume` tool.
- [ ] Make restart create a fresh run/generation from node 1 while retaining the
  old run as history; running/cancelling runs must be cancelled/terminal first.
- [ ] Batch cancellation persists intent before action, prevents pending starts,
  marks them `skip cancelled`, terminates running shell trees, and preserves
  safe native-mutation terminal semantics.
- [ ] Repeated cancellation is idempotent; cancelling a terminal run is a no-op
  returning its terminal state.
- [ ] Forward compact batch progress while shell nodes run without inventing a
  shell percentage.
- [ ] Allow `writes` hints to reduce known scheduling conflicts inside the graph;
  unknown hints may be treated conservatively and never become authority.
- [ ] Do not enable shell nodes by a batchable bit alone; durability, replay,
  cancellation and output retention ship as one coherent v2 feature.

### Phase 10 — compatibility, measurements and docs

- [ ] Keep compatibility strictly at wire/deserialization boundaries.
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
- [ ] Batch mutation failure-injection tests proving partial commit ledgers are
  exact and never hidden behind a generic error.
- [ ] Unit tests for globally correct sorting/paging under bounded traversal.
- [ ] Path-composability tests: every `find` result is directly reusable as a
  tool path; `ls` intentionally returns basenames.
- [ ] Status grammar tests for primary success/failure/partial/uncertain/skip
  outcomes plus orthogonal `more`/`incomplete` modifiers.
- [ ] Paging tests for one canonical cursor grammar, known total, unknown total,
  final page and `more + incomplete` combined state.
- [ ] Progress tests for fixed-total, unknown-total and phase-only updates,
  asserting cumulative/monotonic semantics.
- [ ] Shell `writes` schema/normalization tests: omitted vs empty, relative
  resolution, subtree scopes and no glob/shell expansion.
- [ ] Tests proving shell `writes` does not change authorization/sandboxing and
  inaccurate hints cannot grant access.
- [ ] Batch graph tests for complete-success default sequencing, multiple roots,
  fan-out, joins, modifier/status branches, condition skips and backward-reference
  validation.
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
- [ ] Crash/restart failure-injection around `claimed -> running -> terminal`
  proving ambiguous in-flight shell state becomes `uncertain` rather than being
  relaunched.
- [ ] Durable control tests for compact polling, idempotent cancel, cancel races,
  automatic resume, restart generation/history retention, and replay-safe
  restart delivery.
- [ ] V2 output-spool tests proving large shell output remains bounded/pageable
  under one durable batch owner.
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
- [ ] Directory-tree copy / recursive-delete protected-path and symlink tests.
- [ ] Cross-device `mv` rejection tests.
- [ ] Cross-platform native Windows/macOS filesystem tests.
- [ ] `cargo fmt --all -- --check`.
- [ ] `cargo clippy --locked --all-targets --all-features -- -D warnings`.
- [ ] `cargo test --locked --all-features` with at least 8 build jobs.
- [ ] `nix flake check -L`.
- [ ] Markdown lint.
- [ ] Full CI matrix green.
