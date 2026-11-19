# 0.17 main shared-consumer backport

Base: `4f60bc3d4d2edb42c7d75f91d5dc6e8bae02d02a`

AW owner plan:
`../../../../aw/.agents/plans/017-shared-runtime-convergence-2026-11-18/TODO.md`

Release reference (read-only in this workstream):
`release/0.17 @ 3995448d7bf33794ef84c7bb51150d77cc260ada`,
`origin/release/0.17 @ 3995448d7bf33794ef84c7bb51150d77cc260ada`,
and `v0.17.5` resolves to that same commit.

## Committed

Signed pre-repin checkpoints:

- `8a3ed9d` — Nix service environment ownership hardening;
- `a36e976` — best-effort lifecycle logging;
- `690cfb5` — pre-repin runtime/MCP compatibility hardening;
- `8e14ee2` — 0.17 consumer plans/handoff.

The pre-0.17 shared convergence remains committed at `4f60bc3`.

## Uncommitted fresh-review parity fixes

Current review fixes are intentionally small and local until the shared repin:
zero-byte `jobs_read` rejection, receipt-object merge/validation, mandatory
truncation markers, and post-fit visible timestamp preservation.

### D0 — dependency boundary

- [x] AW fresh-review implementation is signed/published at
      `7c7de6c9aeedf714adbe67b17ac863f27697e90e`; AW docs/main are published
      through `05dd2ff1163753903371b9f36d29922da1f03a20`.
- [x] AW publish authorized and completed in the current turn.
- [ ] Repin the complete direct AW set atomically to the exact published
      implementation commit: existing `aw-tools`, `aw-tools-fs-unix`,
      `aw-tools-fs-win` plus new D1/D6-D7 consumers `aw-platform` and
      lightweight `aw-mcp-protocol`;
      every transitive AW Git package in `Cargo.lock` must resolve to that same
      SHA. Follow AW
      `017-shared-runtime-convergence-2026-11-18/PUBLISH-REPIN-PREFLIGHT.md`.
- [x] No temporary path/sibling dependency introduced.
- [x] Revalidated dotlink main/release refs and preserved unrelated work.
- [ ] D1-D7 shared-semantic hookup is blocked only on the current atomic repin;
      the exact published AW implementation is `7c7de6c9...`;
      do not recreate those shared semantics locally while waiting. Independent
      product-owned prerequisites may land only when they do not encode a
      parallel AW semantic model.

### D1 — process provenance thin consumer

Exact post-repin edit/deletion/test order is frozen in
[D1-D3-SOURCE-CUTOVER-MAP.md](D1-D3-SOURCE-CUTOVER-MAP.md). The source inventory
found one pre-publish AW API gap only: shared physical Unix signal extraction
from `ExitStatus`. That gap is now resolved in the AW working tree via
`aw_platform::exit_signal(&ExitStatus)`, and AW's process adapter consumes the
same helper; dotlink must use it after repin rather than duplicating cfg logic.

- [ ] Replace dotlink-private stop/cancellation semantic enums with AW shared values where architecture permits.
- [ ] Use AW evidence-preserving managed process-tree stop primitive.
- [ ] Map reaped launcher status to shared `ProcessExit::Signal(n)`.
- [ ] Retain no parallel `stop_proven -> Cancelled/DeadlineExceeded` model.
- [ ] Map shared minimal `StopRecord`/terminal `StopReason` to v0.17.4 `stopped_by` wire spellings.
- [ ] External signal => signal number, no `stopped_by`; when physical exit is
      `Signal(n)` and the final stop record has no semantic reason, render the
      v0.17.5 product-only outside-exit line exactly once. A reasoned connector
      stop sequence suppresses `from=outside` even when stop evidence is
      NoTarget/Unknown because direct-child fallback may have delivered the
      physical signal.
- [ ] Retain only dotlink-owned context needed to map shared `Cancelled` to
      product `cancel` vs `shutdown`; do not re-expand AW canonical state.
- [ ] Cleanup/Drop are attempt paths, not stop reasons; housekeeping uses
      `StopRecord.reason=None`.
- [ ] NoTarget and Unknown remain distinct: NoTarget disarms; Unknown may retain
      fail-safe retry responsibility through shared `StopTracker`; use the
      runner's final `stop_record()` rather than reconstructing stop evidence.
- [ ] Preserve actual execution deadline/race semantics through local v0.17.5.

### D2 — final stop/exit records -> operator logs; lifecycle events -> diagnostics

- [ ] Consume AW attempt-level `ExecutionLifecycleEvent::ProcessStop` only for
      diagnostics/metrics; do not render one operator line per attempt.
- [ ] Render exact fixed-order `LOG job stop id=... reason=... outcome=...`
      exactly once from `ProcessReceipt::stop_record()` plus product-only
      cancel-vs-shutdown context. Reason-less housekeeping/external exit emits
      no connector-stop line.
- [x] Establish an always-visible `LogConfig::lifecycle` channel used by
      production notices today; focused coverage proves identical LOG output at
      quiet, TOOL, TOOL+REQ and REQ-only verbosity.
- [ ] After final stop/exit projection hookup, prove both mutually exclusive
      lifecycle lines use the always-visible channel and survive every verbosity.
- [x] Port non-panicking structured logger writes: LOG/TOOL/REQ lines and
      progress continuations use locked best-effort stderr writes; focused tests
      prove BrokenPipe is returned/ignored and Drop-time logging is safe during
      an active unwind.
- [ ] Real-binary stop-line test: exactly one stop line on connector stop and
      no outside-exit line.
- [ ] v0.17.5 real-binary external kill/crash test at quiet/default/verbose:
      exactly one `LOG job exit id=<id> signal=<token> from=outside`, no stop
      line, no `stopped_by`; signal token is platform name or bare number with
      no whitespace.
- [ ] Drop/cleanup/gone/failed attempt tests.

### D3 — Job/Task terminal projection

- [ ] Remove private duplicate process termination fields where shared terminal/detail is authoritative.
- [ ] Project shared compact `TaskTerminalDetail { exit, stop }` into MCP error `data`.
- [ ] Preserve reaped exit/signal behind later internal/spool failure.
- [ ] Preserve prior deadline through `stop=Timeout` behind later Task failure.
- [ ] Use one dotlink compatibility mapper for signal/stopped_by/prose across sync, jobs_get and Task failure.
- [ ] Compact handoff remains a handle, not a second process verdict.

### D4 — typed refs, diagnostics and shared receipts

Exact post-repin D4-D7 edit/deletion/test order is frozen in
[D4-D7-SOURCE-CUTOVER-MAP.md](D4-D7-SOURCE-CUTOVER-MAP.md).

- [ ] Replace local raw flat Job ref parsing with
      `aw_tools::execution_ref::JobRef`; do not flatten AW's hierarchical
      ownership/Agent-routing identity.
- [ ] Consume shared typed `JobStatus/NodeStatus/Handoff/Cancel/Ack` receipts
      and `render_handoff(...)` rather than rebuilding control wording.
- [ ] Remove local `ControlReceipt`/duplicate grammar after parity.
- [ ] Use shared `bounded_diagnostic` plus the product transport-boundary
      backstop where needed.
- [x] Compatibility boundary locked: a 32 KiB caller-controlled Job ref is
      rejected compactly over both real stdio and local HTTP without reflecting
      the supplied identifier into the response.
- [x] Narrow-budget replay preserves the same Job/effect and stored receipt
      metadata; existing Graph handoff and cancel replay regressions prove the
      cached original remains authoritative after compact projection.

### D5 — shared page/result budget semantics

- [ ] Consume shared raw-page budget resolver.
- [ ] Consume shared serialized response-envelope budget.
- [x] Fix current `jobs_read` raw-page boundary: schema/runtime accept a
      one-byte page, reject `limit=0`, and local/stdio fallback uses the
      profile-derived serialized response capacity instead of the raw page size.
      End-to-end stdio proves rejection at zero plus 1-byte page -> ACK through
      1 -> absolute offset 1 next page without changing delivery proof.
- [ ] Schema/default/max and runtime derive from same JobProfile.
- [ ] Raw offsets/EOF/delivery/ACK proof unchanged under response fitting.

### D6 — shared MCP protocol receipt metadata and fitter

- [ ] Add only lightweight `aw-mcp-protocol` and replace local receipt helpers
      with shared
      `stored/has_visible/set_stored_receipt_timestamp` and
      `decorate_and_fit_complete_result`.
- [x] Preserve immutable stored replay timestamp: a pre-stamped replay keeps its
      original `ts`, while fresh selected results still receive a new host UTC
      Unix timestamp. Focused coverage locks stored-wins replay behavior.
- [x] Preserve bounded `_meta["io.abird/receipt"]` and visible `ts=` without
      duplication: result metadata is transactionally capped at **16 KiB**;
      unrelated keys and fields inside the reserved receipt object survive;
      malformed/non-object reserved metadata and oversized metadata fail
      transactionally; a visible stored marker is rechecked after fitting and
      never pays a duplicate sidecar reserve.
- [ ] Replace local generic serialized `CallToolResult` fitter with shared
      `fit_complete_result`.
- [x] Remove dotlink's semantics-changing typed-result-to-text fallback now:
      the local postcondition mutates **text only**, preserving structured
      content, typed media/resources, block order, annotations, per-block/result
      metadata and error status. Truncation always carries an explicit marker;
      if the factual minimum plus marker cannot fit, it returns an MCP budget
      error instead of silently shortening text. The shared
      `fit_complete_result` replacement remains pending on AW repin.
- [x] Compatibility-preservation matrix locked before shared-helper swap: real
      stdio + hosted `bread(format=mcp)` prove exact Image/Audio/Resource block
      type, MIME and base64/blob payload identity; decorator coverage preserves
      synthetic `structured_content`, typed blocks and replay metadata; existing
      stdio/hosted receipt tests preserve `_meta["io.abird/receipt"].ts` + visible
      `ts=`. No live tool currently emits `structuredContent`, so that invariant
      belongs at the decorator boundary rather than a fake transport route.

### D7 — shared MCP protocol heartbeat

- [ ] Replace local generic `ProgressHeartbeat` /
      `await_with_heartbeat` with shared `ProgressHeartbeat::from_token` and
      `await_with_progress_heartbeat`.
- [ ] Extract the request-scoped rmcp progress token in dotlink and pass the
      request peer as the shared send closure. `aw-mcp-protocol` has no MCP
      client/server transport feature and must remain the only new MCP helper
      dependency; do not pull the full `aw-mcp` client adapter into dotlink.
- [x] Pre-repin compatibility locked: production defaults remain 10 s / 100 ms;
      real hosted shell coverage proves token-gated monotonic heartbeats with no
      fake total/message. The shared cutover must preserve those values/shape.
- [x] Adopt the stronger backpressure behavior locally before repin: once a tick
      fires, pending progress delivery is raced against the same pinned operation
      future, so a blocked notification cannot add send-budget latency after the
      result is ready. Deterministic coverage uses a permanently pending delivery
      and proves it is dropped when the operation wins (no detached/post-return
      send); real hosted `jobs_get` + shell heartbeat regressions remain green.
- [ ] Implement the dotlink log sink as `ProgressHeartbeatObserver` and preserve
      attempted/delivered summary on normal completion and wrapper cancellation.
- [ ] Use shared semantic-progress projection when a real AW cumulative
      known/unknown progress value exists; never replace it with heartbeat guesses.

### D8 — product-only Nix hardening

- [x] Port v0.17.4 `30099aa` module-owned environment rejection into current main.
- [x] Loop-test every reserved key:
      `DOTLINK_PROFILE`, `DOTLINK_CONFIG_FILE`, `DOTLINK_API_KEY_FILE`,
      `NGROK_AUTHTOKEN_FILE`, `DOTLINK_STDIO`, `DOTLINK_COLOR`,
      `DOTLINK_SUPERVISOR`, `DOTLINK_SSH_AGENT_SOCKET`.
- [x] Preserve allowed non-owned environment; regression covers
      `DOTLINK_VERBOSITY`, `RUST_LOG`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`.
- [x] Update Nix service documentation with ownership/source-of-truth rule.
- [x] NixOS module derivation evaluates/builds green with command-local verified
      CA bundle; Alejandra and `git diff --check` green.

### D9 — v0.17.x operational docs and equivalence

- [x] Add/adapt runtime-health-check to current `jobs_*` surface:
      30 s `jobs_get/jobs_read` waits, forward `jobs_read` paging, explicit
      ACK/Finish, compact same-Job/no-rerun handoffs and current progress logs.
- [x] Document stop-line/evidence semantics from shared AW facts as an explicit
      **post-D1-D3** contract, not a claim about the current pre-repin binary.
- [x] Document current receipt timestamp semantics and the roughly 30-minute
      implementation-turn guidance as workflow, not a runtime timeout.
- [x] Full semantic disposition matrix written in `V0174-EQUIVALENCE.md`;
      every v0.17.x item is classified Done / Superseded / Product-only /
      Blocked / Deferred. The matrix is classified, not falsely declared fully
      implemented while D1-D7 remain blocked.
- [x] Pre-repin full binary/integration baseline green after fresh-review parity fixes:
      main binary **698 passed / 0 failed / 1 intentional ignored**;
      integrations **40/40** across child-env, exit-status, OAuth rotation,
      private generations and stdio tool suites.
- [x] Pre-repin strict all-target/all-feature `cargo check` + Clippy green.
- [x] Pre-repin rustfmt + `git diff --check` green.
- [x] Nix module + Alejandra + repo-wide Markdown lint green using the existing
      command-local immutable NSS CA bundle.
- [x] Current D1-D4/B6/pre-0.17 regressions remain green in the full current-main
      suite. **Mandatory rerun after D1-D7 repin/cutover remains open for final equivalence.**
- [x] Port v0.17.5 hard-deadline tool/schema wording: `timeout_secs` explicitly
      stops the command tree and is distinct from the caller/transport response
      wait; `background:true` changes waiting only. Focused schema regression
      passes. The v0.17.5 outside-signal line remains correctly blocked on D1/D2
      shared physical exit/final-stop facts rather than adding a duplicate local
      signal provenance model.

### Deferred DS — sandbox/config-v13

- [ ] **DO NOT IMPLEMENT without explicit user promotion.**
- [ ] Follow the frozen AW sandbox plan only after promotion.

## Tests and verification

- [ ] physical-exit/final-stop-record matrix, including timeout/cancel/shutdown product projection.
- [ ] real stop/outside-exit mutual-exclusion integration test at every verbosity.
- [ ] failed native Task terminal-detail parity.
- [x] huge Job ref/error bound over stdio/local HTTP.
- [x] zero-byte rejection + one-byte jobs_read page inside normal response envelope.
- [x] receipt metadata typed-media/structured replay tests.
- [x] heartbeat backpressure/deadline tests.
- [x] Nix reserved-environment evaluation tests.
- [ ] full dotlink v0.17.x equivalence gate through local release 0.17.5.
