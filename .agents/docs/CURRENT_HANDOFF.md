# CURRENT HANDOFF — dotlink unified runtime + DL-200 design

Canonical comprehensive handoff:
`../../../aw/.agents/docs/AW-DL200-R6-FULL-COMPREHENSIVE-HANDOFF-2026-11-21.md`.

Copy-ready next-agent entrypoint:
`../../../aw/.agents/docs/AW-DL200-R6-NEXT-AGENT-2026-11-21.md`.

Active DL-200 design:
`../../../aw/.agents/plans/dl200-unified-jobs-agent-events-2026-11-20/SPEC.md`.

The previous AW+dotlink 2026-11-20 comprehensive handoff and the 2026-11-19 017
handoff remain historical chronology referenced by the new canonical R6 handoff.

**DL-200 R6 is SEALED / IMPLEMENTATION PROMOTED / PHASE 0 CROSS-REPO PROCESS CUTOVER COMPLETE IN SOURCE.** R6 was explicitly approved on 2026-11-21 and is authoritative.
AW shared runtime is published through `7c1670821b15082821a0b9621cd1caccbe62f144`; AW main
is `59d67babaa846fa9b60e39a27170ac9d148e3b6f`, whose only later delta is the focused
bounded-drain regression test. Dotlink source checkpoint `16475e4` consumes `aw-process`
directly and pins all eight direct/transitive AW Git packages to `7c16708...`. The old
parallel shell Process owner is deleted. Frozen 017 mounts/config-v13 remain deferred.
No tag/release/deploy is implied by this checkpoint.

Prepared: 2026-11-20
Owner: @pvl
Machine: **L5 implementation/source authority**
Repo: `/home/pvl/spaces/abird/src/dotlink`

> Runtime/source authority remains live AW/dotlink source and tests. Current published AW
> main is `59d67ba...`; dotlink runtime dependency pin is `7c16708...`; current dotlink source
> checkpoint is `16475e4` before this handoff-sync commit.
> DL-200 design authority is the sealed, implementation-promoted AW spec linked above;
`IMPLEMENTATION.md` owns the active Phase-0 checkpoint order. Historical
unsealed/design-only labels below describe earlier checkpoints, not current state.
> Source/tests/executed evidence outrank prose.
>
> Later operational convergence with the 0.17.1 release branch is tracked in
> [RELEASE-017-OPERATIONS-PORT-2026-11-15.md](RELEASE-017-OPERATIONS-PORT-2026-11-15.md).
> It preserves this unified-runtime implementation and is not a release/tag.

## LIVE R6 shared-Process cross-repo checkpoint — 2026-11-21

Source boundary before this handoff-sync commit:

- AW runtime pin: `7c1670821b15082821a0b9621cd1caccbe62f144`;
- AW main: `59d67babaa846fa9b60e39a27170ac9d148e3b6f` (test-only follow-up over the runtime pin);
- dotlink source: `16475e4` — `Converge shell Process runtime on aw-process`;
- all six direct AW manifest revisions and all eight direct/transitive AW lock sources resolve
  to exactly `7c16708...`; no sibling/path override and no registry-version churn.

Implemented:

- `aw-process` is now dotlink's sole shell Process lifetime owner: containment finalization,
  stdio, spawn, Child/Scope custody, stdin, pipe capture, wait/stop/reap and bounded drain;
- deleted dotlink `src/shell_process.rs` and all production `ManagedProcessGuard`, local
  Child/ChildStdin, stop-tree, terminate-child and duplicate stdout/stderr drain machinery;
- `JobCompletion` stores one optional canonical `ProcessTerminal`; physical exit, StopRecord,
  Occupancy and output completeness are projections, while a definite spawn failure has no
  synthetic ProcessTerminal;
- wire compatibility is exact: `Occupancy::Empty` omits `tree`, `Live` => `tree="leftovers"`,
  `Unknown` => `tree="unknown"`; Live leftovers may clear product `success` without rewriting
  exit/status into a generic tool error;
- direct and Bubblewrap Bash both use non-login/non-profile execution (`--noprofile --norc -c`);
- one dotlink runtime profile owns `checkpoint_minutes=30` and hard shell horizon=35 minutes;
  the effective shell deadline is `min(configured shell policy, 35m)` and drives runtime +
  schema projection. This policy remains absent from AW;
- product stdin authority/revision admission remains in dotlink, but ChildStdin never leaves
  `aw-process`; initial input is chunked through the shared 64 KiB primitive and all in-flight
  shared writes are cancellation/deadline-preemptible;
- shared output drain has one global bound. At the bound readers quiesce and flush already
  captured partial sink batches before publishing incomplete output; abort is final fallback.

Grouped evidence without fixture expansion:

- final dotlink all-target/all-feature source graph type-checks green;
- final strict all-target/all-feature Clippy `-D warnings` green;
- rustfmt and `git diff --check` green;
- static ownership audit: no production `ManagedProcessGuard`, `shell_process::`, local Child
  owner, stop-tree/terminate-child or login-shell residue;
- shell grouped executable run after the initial cut reached **133 passed / 1 failed**; the
  single failure was the inherited-FD partial sink-batch loss described above;
- that exact failure boundary is now covered at its actual shared owner: `aw-process` **3/3**,
  including `reader_stop_flushes_partial_sink_batch_before_incomplete`, with strict Clippy green;
- L5 repeatedly SIGKILLs the final dotlink test/binary during dotlink codegen/link after pin
  changes, before any test executes. This is an explicit executable-qualification environment
  gap, not a Rust diagnostic; do not split the suite into fixture fragments to work around it.

Phase-0 Process architecture is now converged in source. Before declaring final Phase-0
acceptance/release qualification, rerun the grouped shell/Process/Remote-Start executable gate
on a host where dotlink test linking is not externally killed. **Do not start Phase 1 in this
handoff.**

## Historical pre-R6-implementation checkpoint — 2026-11-21

- dotlink source checkpoint: `89ff3fb` — `Harden Remote Start replay admission`;
- exact retained Remote Start replay now resolves before mutable lease/runtime-generation/
  issuer-trust admission for a genuinely new effect;
- fresh expired/stale/untrusted claims fail, while retained exact factual receipts remain
  replayable under the existing JobManager owner;
- focused Remote Start/replay suite: **16/16** green;
- locked all-target/all-feature `cargo check`: green;
- rustfmt and `git diff --check`: green;
- one test compilation was externally SIGKILLed by L5 and passed cleanly on the warmed rerun;
- this is Phase-0B/replay hardening only. The R6 `aw-process` consumer cutover, Occupancy
  projection, non-login Bash convergence and 30/35-minute runtime-horizon work remain next.

## LIVE published AW repin + process/control/Remote Start trust checkpoint — 2026-11-20

This section supersedes older dependency, pending-work and validation totals
below where they conflict. D4-D7 and v0.17.5 semantic equivalence remain
closed.

Current source boundary:

- AW source implementation is signed/published at
  `b8a46a942a56a5ae5cb7e2a8fad47010fde4d65b`
  (`Complete v36 runtime admission and process semantics`);
- dotlink signed source checkpoint is
  `648519add3d174b9e13de81aeca62431591dac6d`
  (`Repin AW and complete process-control hardening`);
- all seven direct/transitive AW Git packages in dotlink are pinned exactly to
  `b8a46a942a56a5ae5cb7e2a8fad47010fde4d65b`;
- no sibling/path override and no registry package/version drift were introduced;
- the read-only release worktree remains clean at local
  `release/0.17 = a2be5098e990f78656db746e10f5652a494347c2`,
  28 commits ahead / 0 behind published
  `origin/release/0.17 = v0.17.5 = 3995448d7bf33794ef84c7bb51150d77cc260ada`;
- sandbox/mount/config-v13 remains frozen and untouched.

### Process lifetime and output truth

The waited child, managed containment target and inherited output pipes have
three deliberately independent lifetimes:

1. **Process execution** ends at the waited child result or requested stop.
2. **Managed descendants** are closed immediately after ordinary child exit
   through the existing `ManagedProcessGuard` as **reasonless housekeeping**.
3. **Inherited stdout/stderr** receive a fresh **250 ms post-exit grace**,
   independent of the Process execution deadline.

If an unsandboxed `setsid` descendant retains an inherited FD past that grace,
dotlink seals the drain and preserves the factual command exit while marking
output incomplete. The repinned shared AW owner now projects this directly:

```text
ProcessExit::Code(0) + no semantic stop  -> Primary::Ok
nonzero code or physical signal          -> Primary::Fail
unknown physical exit                     -> Primary::Unknown
proven semantic stop                      -> Primary::Fail
!ProcessTerminal.output.complete          -> Status.incomplete=true
```

Dotlink's temporary `shared_process_status` compatibility projection is gone.
`JobManager` calls shared `settle_process(...)` and consumes
`ProcessTerminal::status()` directly. Known exit truth and output coverage are
therefore orthogonal in the shared owner, with no second lifecycle taxonomy.

### Job control linearization

Public model-visible control is shared-owner first:

- revision-fenced `jobs_cancel` requires a canonical shared Job owner before
  compare-and-act; there is no public non-shared cancel CAS fallback;
- the per-Job transition gate serializes canonical revision checkpointing with
  the local cancel signal;
- Remote Start exact replay still resolves before current revision checks.

Process stdin acceptance is one transition:

1. reserve channel capacity without effect;
2. acquire the per-Job transition gate;
3. re-derive exact shared process tool + shell-guard generation;
4. synchronously revalidate current host authority;
5. compare/advance the canonical shared revision;
6. enqueue bytes.

A revoke/generation change before step 6 cannot cross the input-acceptance
boundary. No second stdin-authority registry was added.

### Trusted AW Remote Start issuer

Ordinary OAuth authentication and AW Remote Start issuer authority are now
separate capabilities.

Private OAuth state owns one optional trust record per approved DCR client:

```text
RemoteStartTrust { epoch: u64, enabled: bool }
```

Owner controls:

```bash
dotlink oauth clients -p <profile>
dotlink oauth trust-aw-bridge -p <profile> <client-id>
dotlink oauth untrust-aw-bridge -p <profile> <client-id>
```

Trust transitions are monotonic and idempotent:

```text
untrusted -> trust       epoch 1
trust -> trust           epoch 1
trust -> untrust         epoch 2
untrust -> untrust       epoch 2
untrust -> trust         epoch 3
```

The same bearer token remains valid ordinary MCP authentication after AW-bridge
trust is revoked. Only its Remote Start issuer annotation disappears.

Lease issuance is explicit:

- unauthenticated lease request -> HTTP 401;
- authenticated ordinary OAuth client without AW-bridge trust -> HTTP 403;
- trusted client -> `RemoteStartLease { target, issuer }`, where issuer binds
  the authenticated `client_id` and current trust epoch.

New Start validation requires the authorization issuer's client ID to equal the
authenticated MCP client. The authorization's v2 digest binds that issuer.
Current trust epoch is enforced only at the **new-claim** boundary:

```text
exact committed Start replay
    -> replay existing factual receipt first
    -> does not require current trust epoch

genuinely new Start
    -> current authenticated client must still be trusted
    -> current epoch must equal authorization issuer epoch
```

Therefore issuer revocation blocks new effects without erasing committed replay,
and re-trust at a later epoch cannot revive an old lease.

### Instruction ownership and deterministic regressions

The duplicate static `#[tool_handler(... instructions = ...)]` copy is removed.
`get_info().server_info.instructions` is the single instruction owner.

Control-plane wait/read/cancel tests use canonical shared Running Process
fixtures with real shared output writers and notifications rather than
child-process sleeps or ambient utility discovery.

### Qualification after repin

Focused trust/replay regressions are green for:

- monotonic/idempotent trust epochs with live state observation;
- bearer authentication surviving AW-bridge trust revocation;
- unauthenticated/untrusted/trusted lease 401/403/200 behavior;
- issuer-bound HTTP wire execution + exact replay;
- revoked issuer blocking a fresh claim while exact committed replay survives;
- Graph, shell and filesystem Remote Start no-rerun replay;
- shared process outcome vs output-incomplete projection;
- immediate descendant cleanup, escaped-`setsid` output handling;
- stdin revoke-before-bytes, shared-owner cancel and replay-before-revision.

Final host-available repinned gate:

- main binary: **706 passed / 0 failed / 1 intentional ignored**;
- integrations: **42/42**:
  child-env 1/1, exit-line 2/2, exit-status 15/15,
  OAuth live rotation 1/1, private generations 13/13, stdio tools 10/10;
- strict
  `cargo clippy --locked --all-targets --all-features -j1 -- -D warnings`:
  green;
- `cargo fmt --all -- --check`: green;
- `git diff --check`: green;
- `alejandra --check flake.nix nix`: green;
- Markdown lint: **0 issues across 86 files**.

Two combined `cargo test --locked --all-features` invocations were externally
SIGKILLed by L5 before producing test output or Rust diagnostics. The same test
set was then qualified as the complete main binary plus all six integration
targets serially, all green. Treat those external kills as host noise, not a
source failure.

### Remaining boundaries

- native Windows Job Object runtime qualification remains external;
- native macOS runtime/deployment qualification remains external;
- trusted-selector Bubblewrap runtime smoke remains host-gated;
- sandbox/config-v13 remains frozen pending explicit user promotion;
- no release/tag/deploy action is implied by this source publication.

## LIVE 0.17 shared-consumer checkpoint — D1-D7 fresh-reviewed, remediated and qualified 2026-11-19

This section supersedes older dependency/state details below where they conflict.

Current Git / dependency boundary:

- signed fresh-review remediation checkpoint:
  `d32a3196ec18ada7f85176a9feed42667e5f2d6b`
  (`Harden Jobs replay and response budget boundaries`);
- preceding signed D4-D7 implementation checkpoint:
  `fa01e18e3bb023dfe1d60b8ed45fd0ce53bfc0f7`
  (`Complete D4-D7 shared consumer convergence`);
- preceding signed equivalence/docs checkpoint:
  `8b77bae48633d8c609b44ccec7291e5e20ab5624`
  (`Record final 0.17 equivalence checkpoint`);
- prior signed/pushed D1-D3 docs checkpoint remains
  `4c3b96fd5ba9e6772bc159f10a3ad893c9729887`
  (`Record repinned D1-D3 convergence checkpoint`);
- `origin/main` remains `4c3b96fd...` because push has not been authorized;
- pre-repin signed checkpoints remain `46215fe` product parity and `0aea1f4` consumer docs;
- AW final published implementation pin:
  AW_IMPL_SHA = 1a5d0f65208e4a05b737c804a55a3aaad3e2fa59;
- AW origin/main = 4234447612421069a746c925c76a25e3ba27774a;
- all direct/transitive AW Git packages in dotlink resolve to exactly 1a5d0f65...;
- the dotlink registry package/version set is unchanged by the repin;
- no sibling/path override exists;
- release/0.17 = origin/release/0.17 = v0.17.5 =
  3995448d7bf33794ef84c7bb51150d77cc260ada.

### D0 repin — complete

The direct AW set is now aw-tools, aw-tools-fs-unix, aw-tools-fs-win,
aw-platform and lightweight aw-mcp-protocol. The full aw-mcp client adapter is
intentionally absent.

The first attempted repin exposed one shared packaging defect before commit:
aw-mcp-protocol's exact Tokio/serde_json requirements would have downgraded
dotlink's existing compatible runtime stack. AW fixed that in signed/published
1a5d0f65...; the final repin preserves dotlink's registry package/version set
exactly.

### D1-D3 process convergence — complete

Dotlink no longer owns a parallel process-stop model:

- src/shell_process.rs is only Tokio child/command glue around
  aw_platform::ManagedProcessGuard;
- physical exit is retained as shared ProcessExit::{Code(i32),Signal(i32),Unknown};
- semantic stop is retained as shared StopRecord / StopReason / StopEvidence;
- shared StopTracker preserves the original reason across Unknown retries;
- old stop_proven, synthetic DeadlineExceeded/Cancelled ProcessExit values,
  private process-group/Job Object Guard semantics and local signal-name tables
  are gone;
- later spool/journal/internal failure may downgrade product Job status but
  cannot erase already observed physical exit or final stop evidence;
- pre-spawn cancellation/deadline correctly carries no process stop claim.

Dotlink retains only product context:

- CancelOrigin::{Caller,Connector} lives beside the Job cancellation token;
- shared Cancelled + Stopped projects to stopped_by:"cancel" for caller
  cancellation and stopped_by:"shutdown" for connector teardown;
- shared Timeout projects to "timeout";
- shared RuntimeError projects to "failed";
- NoTarget/Unknown never produce stopped_by.

One product mapper now drives synchronous results, jobs_get, Task failure detail
and lifecycle prose. Physical signal becomes JSON signal; proven semantic stop
becomes stopped_by; Task failure data is derived through shared
TaskTerminalDetail; outside-signal prose uses aw_platform::signal_name or the
bare number.

Lifecycle logging is final-result based, not attempt-event based:

```text
reasoned final StopRecord
  -> exactly one LOG job stop id=<id> reason=<product reason>
                 outcome=<delivered|gone|failed>

ProcessExit::Signal(n) + no reasoned stop sequence
  -> exactly one LOG job exit id=<id> signal=<name-or-number> from=outside
```

The two lines are mutually exclusive. from=outside means only that dotlink did
not enter a semantic stop sequence; it does not identify a sender.

run_shell_job() emits the lifecycle record exactly once after the final
completion has been assembled. Shared attempt-level lifecycle evidence remains
diagnostic semantics and is never mapped one-for-one to operator lines.

### D4 typed refs, shared receipts and diagnostics — complete

The finite Jobs surface now consumes the pinned shared D4 primitives directly:

- model Job refs parse as `aw_tools::execution_ref::JobRef`; trusted internal
  construction uses the same `Handle + NodeCoordinate -> JobRef` grammar;
- shared-backed root/node status uses shared `Receipt::JobStatus/NodeStatus`;
- compatibility handoff uses shared `render_handoff` and typed
  `Receipt::Handoff` fallback for the same retained Job;
- cancel and ACK use shared typed receipts; the local `ControlReceipt` grammar
  is gone;
- `jobs_send` intentionally keeps its tiny product-specific receipt;
- the remote replay ledger retains typed `JobRef` beside the immutable MCP
  result, so narrow replay never reparses rendered receipt prose as identity;
- shared `Snapshot.sequence` is the public Jobs revision for shared-backed
  controls; stdin/cancel reserve it immediately before their effect;
- generic Job diagnostic truncation now uses shared `bounded_diagnostic`;
- D6 below supersedes the former local generic MCP Complete-result fitter.

Hostile refs are rejected before reflection, narrow Graph handoff re-reads the
same retained Job rather than rerunning, exact cancel replay still precedes
revision recheck, and stored replay timestamp metadata remains authoritative.

### D5 raw-page vs serialized-result budgets — complete

The Jobs read boundary now consumes AW's zero-cost typed budgets directly:

- caller `limit` resolves exactly once through the effective
  `JobProfile::resolve_read_page(...)` to `RawPageBytes`;
- the authenticated/fallback MCP result ceiling becomes an independent
  `SerializedResultBytes`;
- `OutputPageRequest`, legacy Process reads, shared Process/node reads and
  synthesized Graph-result reads carry those distinct types;
- raw source reads use only `RawPageBytes`; response-envelope capacity no longer
  shrinks the retained source window before projection;
- response fitting happens only after the raw window is fixed, and only the
  fitted raw prefix is recorded as delivery credit;
- raw absolute offsets, retained length, EOF and ACK proof therefore remain
  independent of response fitting;
- `jobs_read` schema default/max and runtime validation derive from the same
  effective `JobProfile`;
- the old unrelated generic MCP minimum-byte floor is no longer applied to a
  Jobs page. A small serialized budget succeeds when the factual page fits and
  fails only when the actual response cannot fit.

The existing legacy delivery fence remains only for genuinely non-shared
historical/test entries; shared output continues to use AW's owner-specific
delivery proof.

### D6 shared MCP result/receipt projection — complete

Dotlink now consumes the pinned lightweight `aw-mcp-protocol` result helpers
instead of retaining a parallel generic MCP fitter/receipt implementation:

- `fit_complete_result` owns generic Complete-result fitting;
- `decorate_and_fit_complete_result` owns selected-operation receipt metadata,
  stored timestamp replay, visible `ts=` placement and bounded fitting;
- `validate_complete_result`, `stored_receipt_timestamp` and
  `set_stored_receipt_timestamp` own cached Remote Start receipt validation and
  metadata mutation without fitting immutable cache state;
- only text may shrink; typed Image/Audio/Resource blocks, structured content,
  block order/metadata, result metadata and error status remain factual identity;
- immutable facts that cannot fit fail explicitly rather than collapsing to
  replacement text;
- stored receipt timestamps win over a fresh host clock and a surviving visible
  timestamp never pays a duplicate sidecar reserve;
- native MCP Task finalization also uses the shared decorator, deriving the
  actual shared Bash/Pwsh/PowerShell operation and retaining the first Task
  timestamp across replay.

Dotlink retains only the product boundary decisions: transport/fallback result
budget selection, `OperationId` selection, host UTC clock acquisition,
`aw-mcp-protocol::Error -> McpError` mapping, and protocol-error fitting. Cached
Remote Start results are never request-budget fitted before entering the replay
ledger. The full `aw-mcp` client adapter remains absent.

The deleted local owners are `fitted_text_blocks`,
`fit_call_tool_response_to_budget`, local receipt metadata constants/helpers,
`ensure_visible_receipt_ts` and `stamp_receipt_metadata`.

### D7 shared progress heartbeat — complete

The generic heartbeat scheduler now lives entirely in lightweight
`aw-mcp-protocol`:

- `ProgressHeartbeat::from_token(...)` owns token-gated shared heartbeat state;
- `ProgressHeartbeatConfig::default()` supplies the production **10 s / 100 ms**
  interval/send-budget contract; tests use one validated 100 ms interval helper
  only to keep focused/hosted regressions fast;
- `await_with_progress_heartbeat(...)` owns tick timing, monotonic unknown-total
  progress numbers, send deadlines, and the operation-vs-backpressured-delivery
  race;
- dotlink's `RequestProgress` is only rmcp transport context: shared heartbeat +
  request peer + product log; it owns no timer, counter or scheduling state;
- the send closure calls `peer.notify_progress(...)`, records the existing
  per-tick `progress_mark(delivered)`, and returns the same boolean to the shared
  helper;
- product-only `ProgressLogObserver` implements
  `ProgressHeartbeatObserver::completed(...)` and maps the shared
  attempted/delivered summary to `progress_done(...)`, including wrapper
  cancellation/drop paths;
- shell waits still pass their real response-window deadline; non-shell outer
  calls remain deadline-free unless they already have a real one;
- no detached progress task exists.

A source audit found no current dotlink operation exposing real cumulative AW
semantic progress. Therefore D7 deliberately adds no fabricated percentage,
total or message from heartbeat count, elapsed time, Job revision or output
bytes. When a real source exists, the shared
`semantic_progress_notification`/`notify_semantic_progress_bounded` path remains
the required projection boundary.

The deleted local scheduler owners are `PROGRESS_HEARTBEAT_INTERVAL`,
`PROGRESS_SEND_BUDGET`, `deliver_heartbeat_bounded`, local `ProgressHeartbeat`
and its Drop counters, `heartbeat_tick`, `await_wait_or_delivery`, and
`await_with_heartbeat`.

### Final post-repin/D1-D7 equivalence qualification

- focused D4/D5 Jobs receipt/ref/replay/read suite: **17/17**;
- focused D6 compatibility oracle green: shared fitting/timestamp replay,
  transactional metadata limits, structured/typed preservation, immutable-fact
  failure, native Task exact replay/fallback, real stdio and hosted media +
  receipt timestamp paths;
- frozen D7 progress regressions **4/4** green:
  pending delivery never delays/outlives a ready operation, expired shell
  deadline prevents delivery polling, outer `jobs_get` emits progress, and shell
  heartbeat ticks stay monotonic with no total/message;
- mandatory D4 compatibility regressions green:
  huge-ref stdio/HTTP, duplicate Graph no-rerun replay and cancel
  replay-before-revision;
- focused D5 page/budget regressions green:
  profile resolver/schema parity, full raw-window then fitted-prefix credit,
  response-envelope reservation, wait-for-bytes/EOF and no polling gap;
- real stdio D5 contracts green:
  `limit=0` rejection, one-byte raw page inside its independent response
  envelope, ACK-through-1, absolute-offset continuation and unread-gap ACK
  rejection;
- main binary: **701 passed / 0 failed / 1 intentional ignored**;
- integrations: **42/42**:
  - child-env 1/1;
  - exit-status 15/15;
  - OAuth live rotation 1/1;
  - private generations 13/13;
  - stdio tools 10/10;
  - real lifecycle exit/stop 2/2;
- real Unix external SIGKILL proves at default, verbose and quiet:
  signal 9, no stopped_by, exactly one exit ... from=outside;
- real timeout proves at the same verbosity modes:
  stopped_by:"timeout", exactly one stop line and no outside-exit line;
- locked all-target/all-feature cargo check: green;
- strict all-target/all-feature Clippy: green;
- rustfmt and git diff --check: green;
- repo-wide Markdown lint: green;
- final real flake `checks.x86_64-linux.nixos-module` build: green against the current dirty working tree and pinned flake inputs;
- Alejandra `nix/module.nix` + `nix/tests/module.nix`: green;
- available native Linux closeout: hardware discovery/policy **4/4**, process-tree timeout containment **1/1**, real signal/timeout lifecycle **2/2**;
- Bubblewrap runtime device-isolation smoke remains host-gated: this connector exposes `bwrap` only as a direct Nix-store selector, which dotlink correctly rejects as launcher selection authority. Do not bypass that policy to manufacture a smoke result;
- native Windows/macOS runtime/deployment validation remains external to this x86_64 Linux host.

L5 intermittently externally SIGKILLed several compile/Nix-client invocations
before diagnostics. The host still had roughly 24 GiB available RAM and 66 GiB
free swap, low load and no kernel OOM/cgroup kill evidence. D6's locked check
passed before one combined strict-Clippy invocation was externally killed;
rerunning only Clippy serially passed cleanly. D7 itself completed its strict
check/Clippy gates without a kill. These were host/connector kills, not
timeout/cancel attribution and not Rust/test failures.

One first-pass D7 full-suite run hit the pre-existing zero-window shell handoff
race in `client_window_reaches_the_shell_deadline_end_to_end`: the immediate
receipt was valid `j:... process waiting rev=1` before the shared Bash node had
claimed its tool identity, while the assertion happened to require `bash` even
for `waiting`. No source was changed; the exact test passed **3/3** in isolation,
and the complete second full run passed **701/0/1**. Treat this as timing noise,
not a D7 behavior change.

### Fresh independent review remediation — complete

A fresh review from the clean `fa01e18` + `8b77bae` stack found two related
correctness defects at the D4/D5/D6 boundary:

1. Remote Start Job/Graph completion could apply a request-specific narrow
   projection before `RemoteStartReceipt::JobMcp` was stored. A narrow first
   Batch/ACK/Cancel/Send response could therefore replace the richer canonical
   replay receipt. `complete_remote_start_result` now stamps and stores one
   canonical typed receipt first, then derives only the current response from
   that immutable cache state. Narrow-first Graph and Cancel regressions prove a
   later wide duplicate recovers canonical success without a second effect.
2. Several Jobs fit/preflight decisions measured the bare `CallToolResult`
   before D6 added receipt metadata and visible `ts=`. Most importantly,
   `jobs_send` could accept irreversible stdin when the bare `j:<ref> sent`
   receipt fit but the required final decorated receipt did not. One
   `LocalMachine::exact_decorated_result_len` helper now uses the shared
   `aw-mcp-protocol` decorator as the sizing oracle for Batch/List/Get/
   ACK/Send/Cancel. `jobs_send` preflights that exact final shape before stdin
   leaves the owner.

The D5 page algorithm remains intentionally separate and raw-byte based. Its
existing response reserve was freshly regression-tested through the D6
`JobsRead` decorator: the final decorated result stays within the same ceiling
and the page text is byte-for-byte unchanged after delivery credit, so
`next_offset` remains exactly the ACKable raw prefix.

The review also corrected the stale `shared_native.rs` comment that still said
receipt timestamps were disabled. Shared-native filesystem Remote Start was
reviewed but intentionally not changed: its budget is part of canonical shared
filesystem execution, and a post-effect enforcement failure is retained as an
unknown replay outcome rather than caching a request-specific projection.

Fresh-review qualification after `d32a319...`:

- focused cache/final-envelope suite: **8/8** green;
- full main binary: **701 passed / 0 failed / 1 intentional ignored**;
- integrations: **42/42**;
- locked all-target/all-feature check + strict Clippy `-D warnings`: green;
- rustfmt and `git diff --check`: green;
- one L5 compile invocation was externally SIGKILLed without Rust diagnostics;
  the same focused slice then passed cleanly on the warmed build;
- no Nix files changed, so the prior real flake NixOS/native closeout remains
  the applicable Nix/platform evidence.

### Post-commit fresh-review hardening — working tree complete

Independent review from clean committed checkpoint
`20b6f045f79fa7725f286e05b40e9262b1c9e4d3` found and fixed two related
`jobs_get` Process-tail projection defects:

1. Tail selection fitted the **bare** `CallToolResult` before D6 added required
   receipt metadata and visible `ts=`. A smaller valid tail could therefore
   exist while the handler returned a response-budget error.
2. The old binary search assumed serialized size was monotone in raw tail bytes.
   That is true for ASCII, but lossy UTF-8 can make a larger raw suffix serialize
   smaller when a boundary rejoins a valid multibyte character.

Final design:

- production exposes one API:
  `JobManager::standalone_process_tail_fitted(..., fits)`;
- the Job layer owns factual tail generation; the host supplies a zero-cost
  final-response fit predicate, so MCP/receipt policy never leaks into Jobs;
- the requested tail is tried once first, then optional bytes shrink before
  required status/receipt facts;
- private `largest_fitting_projection` returns the **exact candidate it proved
  fits**—a winning candidate is never re-read after admission;
- ASCII projections use logarithmic search; any observed non-ASCII projection
  switches to an exact descending fallback bounded by the public **1024-byte**
  tail maximum;
- shared AW JobManager remains the sole output/owner authority; tail observation
  never creates delivery or ACK proof;
- legacy byte-ceiling helpers now live only in test modules, leaving one
  production tail surface.

Regression coverage proves:

- a bare tail can fit while its D6-decorated result does not, and the final
  predicate-driven projection still succeeds;
- a UTF-8 lead/continuation boundary where 513 raw bytes fit better than 512
  still returns the largest valid tail;
- D5/D6 `jobs_read` composition is safe for **every ceiling 1..=1024** where a
  page can be produced: D6 leaves the credited page text byte-for-byte unchanged,
  so `next_offset` remains exactly the ACKable raw prefix.

No-change audits remain clean for D1-D3 stop/cancellation provenance, D4
revision/control ordering, Remote Start fail-closed replay, D6 native Task
timestamp/operation replay, exact AW Graph-result fallback and D7 heartbeat
backpressure/accounting.

Working-tree qualification:

- full main binary: **701 passed / 0 failed / 1 intentional ignored**;
- integrations: **42/42**;
- locked all-target/all-feature `cargo check`: green;
- strict Clippy `-D warnings`: green;
- rustfmt and `git diff --check`: green;
- no Cargo/Nix/dependency changes.

Several L5 compilation attempts were externally SIGKILLed without Rust
diagnostics; completed warmed/serial runs above are green. This source/docs
checkpoint remains intentionally **unstaged, uncommitted and unpushed**.

### Final qualified checkpoint

D4-D7 consumer cutover and the full v0.17.x semantic equivalence gate are
complete. Do not reopen those slices without a newly reproduced regression.
The only unavailable runtime/deployment smokes are explicitly host/platform
external: trusted-selector Bubblewrap on this connector and native
Windows/macOS.

The latest **signed** source checkpoint remains
`d32a3196ec18ada7f85176a9feed42667e5f2d6b`; the newer second/third-review
tail-envelope hardening above is green but intentionally uncommitted. No push,
version, tag, release or deployment was performed.

Frozen sandbox/config-v13 work remains deferred and untouched.

## Archived historical handoff material below

Everything below this marker predates the live 0.17 shared-consumer checkpoint at
the top of this file. It is retained only for chronology. Historical AW pins,
dependency counts, test totals, working-tree notes and "what is next" statements
below are **not current state** and must not override the final-qualified
checkpoint above.

## Final implementation state

The unified Jobs + Agent dotlink integration is complete.

Exact shared AW revision:

`0a6c6a0bea07a3886c4dc8be4f423eec3ea3c134`

All three shared dependencies pin that exact reachable Git SHA:

- `aw-tools`
- `aw-tools-fs-unix`
- `aw-tools-fs-win`

The temporary local remote-Start argument-digest implementation is gone.
dotlink now reuses `aw_tools::remote_start::arguments_sha256` directly.

Standalone model surface:

- shell constructor (`bash`, plus prepared PowerShell spelling where supported)
- `batch`
- `jobs_list`
- `jobs_get`
- `jobs_read`
- `jobs_ack`
- `jobs_send`
- `jobs_cancel`

Standalone refs remain `j:<handle>[/n:<coordinate>]`.

Intentionally absent:

- `job_start`
- `job_restart`
- old `job_*`
- Agent constructor/history/filter
- `jobs_subscribe`
- wake
- team/plan/review routes
- nested `batch`

dotlink remains the concrete finite Process/Graph executor. AW exclusively owns
Agent, Session, Policy, membership, transcript, hierarchy and upstream authority.

## Remote Start bridge

The authenticated AW -> dotlink bridge is implemented and qualified:

1. OAuth-authenticated MCP client identity is attached to HTTP requests.
2. The authenticated lease endpoint returns host/profile + live runtime generation.
3. AW supplies one immutable source-fenced `RemoteStartAuthorization`.
4. dotlink validates exact target generation, operation and canonical arguments.
5. The existing JobManager replay owner claims the Start **before mutable
   state/policy/topology checks and before effects**.
6. Only a genuinely new claim rechecks current dotlink launcher/sandbox/tool/resource authority.
7. Exact redelivery returns the original factual receipt or pending/unknown state.
8. Expired, changed-semantics and old-generation proofs fail closed.

Final replay-first review fixed and regression-locked:

- `jobs_cancel`: claim before current revision check;
- remote `batch`: claim before Graph preparation/topology revalidation;
- remote shell: claim before current shell-policy recheck.

`jobs_ack`, `jobs_send` and shared-native effects already had correct claim-first ordering.

## Final qualification

Against exact AW pin
`0a6c6a0bea07a3886c4dc8be4f423eec3ea3c134`:

- `cargo check --offline --locked --all-targets --all-features` — green
- `AW_BASH=... cargo test --offline --locked --all-features --quiet`
  - main: **651 passed / 0 failed / 1 intentional external-fixture ignore**
  - integrations: **1/1, 8/8, 1/1, 12/12, 4/4**
- `cargo clippy --offline --locked --all-targets --all-features -- -D warnings` — green
- `cargo fmt --all -- --check` — green
- `git diff --check` excluding unrelated `docs/CHATGPT_PLUGIN.md` — green

The final Unix standalone catalog remains 21 tools / 13,832 serialized MCP Tool
bytes (~3,458 rough tokens).

Checkpoint F's real OAuth/TCP Process and FS/Graph bridge evidence remains
current. Native Windows/macOS execution remains a separate platform/release gate
and is not claimed from L5.

## Working-tree safety

- `docs/CHATGPT_PLUGIN.md` is unrelated pre-existing dirty work and is not part of
  this implementation checkpoint.
- Separate worktree:
  `/home/pvl/spaces/abird/src/dotlink-016-fixes`, branch `release/0.16`.
  Do not touch it or create another worktree.
- Never use A1 for this project.
- x2 was used only as an inference host for the live Qwen benchmark; it is not
  source authority.

## What is next

No unified-runtime implementation item remains.

Future work should begin from live Git state and only reopen this surface for a
new scoped design/change. Release/tag/deploy/platform certification remain
separate actions and require their own authorization.

## Cross-repo shared-tools handoff update — 2026-11-17

The [current dotlink AW-tools continuation](DOTLINK-AW-TOOLS-NEXT-AGENT-HANDOFF-2026-11-17.md) points to the [canonical full AW–dotlink design and implementation history](../../../aw/.agents/docs/AW-DOTLINK-TOOLS-FULL-COMPREHENSIVE-HANDOFF-2026-11-17.md). It supersedes this dated summary for the *subsequent* Toolset, output/ACK, wait, replay and dependent AW pin D0–D4 work; the unified Jobs runtime baseline documented above remains historical authority for its own commit.

Latest published shared-tools adoption checkpoint: signed dotlink commit `d0a392234e918bd4ec9f0fbd7ab6fe771e1eb696` (`Adopt reviewed AW Job output runtime`) consumes reviewed AW implementation pin `be6d48db552fd44b9dd85364cd8734454109ce68`. D0 is complete; D1 shared owner/tail/range/Finish adoption, D2 grep end-to-end qualification, D3 transport/Task/replay convergence and D4 independent security/release gates remain open. Qualification at that checkpoint: full binary **676 passed / 1 ignored**, real stdio MCP **5/5**, strict Clippy, locked check, fmt and diff checks green.

The finalized shared-tools checkpoint consumes reviewed AW follow-up `beef1bc2d2f37b5660ebe928d368bca37368e608` in all three dependency/lock records. **D1–D4 are complete.** The new shared writer-quiescence invariant requires retained writers to finish before hidden retirement, automatic capacity reclamation or AW runtime replacement; dotlink's existing Process/Graph settlement order already satisfies that invariant, so the repin required no host source change. Fresh exact-pin qualification is **684 passed / 1 ignored** in the all-feature binary plus **36/36 integrations**, with locked all-target/all-feature check, strict Clippy, fmt and diff checks green. The prior D4 security qualification remains valid: guard revocation uses bounded discard cleanup; Markdown is **0 issues across 76 files**; all Nix checks, native full Bubblewrap and seven targeted GPU/KVM/device-security cases are green. **No D1–D4 correctness gate remains open.** Push/signing/notarization/version/tag/release/deploy/provider actions remain separate authorization/external gates.

## AW writer-quiescence repin checkpoint — 2026-11-18

- AW signed/published `beef1bc2d2f37b5660ebe928d368bca37368e608`;
  GitHub `main` and AW local `origin/main` were verified at that exact SHA.
- Dotlink repinned `aw-tools`, `aw-tools-fs-unix` and `aw-tools-fs-win`
  plus the three lock source records from `26a8c78` to `beef1bc`.
- Full current-tree Rust qualification after the repin:
  **684/0/1 + 36/36**, strict all-feature Clippy, fmt/diff and locked
  all-target/all-feature check green.
- No dotlink execution/owner/replay source change was required for the new AW
  invariant. The D1–D4 source/docs plus repin are one reviewed owned checkpoint;
  unrelated release-worktree state remains separate.

## Shared Toolset B6 metadata checkpoint — 2026-11-18

- Published dotlink `main` before this B6 checkpoint is
  `4f343c3183d427e036a6cb1a9cc42c8a4f2c2cb4`; the B6 checkpoint consumes
  published AW `7e6fe20a95640e32610819ce94b61a121e165dea`. Push remains a
  separate action.
- Canonical filesystem MCP projection now uses shared
  `Toolset::filesystem_descriptor`, including shared title, description and
  argument documentation. Finite Job MCP metadata uses
  `Toolset::describe_operation`.
- One `DOTLINK_JOB_PROFILE` now drives both runtime checks and shared metadata:
  Process tail 512/1024, get/read wait 30 s, list max 256. Grep remains shared
  0/128 context with default shared work budgets.
- Receipt timestamp/checkpoint guidance remains intentionally disabled via
  `ReceiptProfile::default()`; do not parse existing free-form MCP results to
  synthesize typed receipts.
- A provider-facing regression found `JobsList.limit` lacked shared
  `minimum=1`. AW published `7e6fe20` centralizes the invariant; dotlink is
  repinned and the temporary local lower-bound shim is removed.
- Shared descriptor metadata now also covers Bash/PowerShell/Batch after
  dotlink's dynamic schema-limit patching. Product descriptions and dynamic
  timeout/output bounds are preserved; inspection, `list_tools` and
  `get_tool` agree on canonical titles.
- Current dotlink B6 gates: binary **685/0/1**, integrations **36/36**, locked
  all-target/all-feature check, strict Clippy, fmt and diff green. No runtime
  authority, replay, output-owner, sandbox or transport behavior changed.
- B6 is complete for current product surfaces. Receipt timestamp/checkpoint
  metadata remains deliberately disabled pending a typed factual receipt path.
  Version/tag/release/deploy remain separate.
