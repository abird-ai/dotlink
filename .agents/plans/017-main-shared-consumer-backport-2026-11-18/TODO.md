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

- [x] AW final implementation is signed/published at
      `1a5d0f65208e4a05b737c804a55a3aaad3e2fa59`; AW docs/main are published
      through `4234447612421069a746c925c76a25e3ba27774a`.
- [x] AW publish authorized and completed in the current turn.
- [x] Repin the complete direct AW set atomically to `1a5d0f65...`: existing
      `aw-tools`, `aw-tools-fs-unix`, `aw-tools-fs-win` plus `aw-platform` and
      lightweight `aw-mcp-protocol`. Every transitive AW Git package in
      `Cargo.lock` resolves to that same SHA, and the registry package/version
      set is unchanged by the repin.
- [x] No temporary path/sibling dependency introduced.
- [x] Revalidated dotlink main/release refs and preserved unrelated work.
- [x] D1-D3 shared process-semantic hookup completed against `1a5d0f65...`.
- [ ] D4-D7 shared ref/budget/MCP/progress hookup remains; do not recreate the
      shared implementations locally while completing those slices.

### D1 — process provenance thin consumer

Exact post-repin edit/deletion/test order is frozen in
[D1-D3-SOURCE-CUTOVER-MAP.md](D1-D3-SOURCE-CUTOVER-MAP.md). The source inventory
found one pre-publish AW API gap only: shared physical Unix signal extraction
from `ExitStatus`. That gap is now resolved in the AW working tree via
`aw_platform::exit_signal(&ExitStatus)`, and AW's process adapter consumes the
same helper; current main now consumes it rather than duplicating cfg logic.

- [x] Replace dotlink-private stop semantics with AW shared values; retain only
      product-only `CancelOrigin::{Caller,Connector}` beside the cancellation token.
- [x] Use AW `ManagedProcessGuard` for evidence-preserving process containment.
- [x] Map reaped launcher status to shared `ProcessExit::Signal(n)` via
      `aw_platform::exit_signal`.
- [x] Remove the parallel `stop_proven -> Cancelled/DeadlineExceeded` model.
- [x] Map shared `StopRecord`/terminal `StopReason` to v0.17.x `stopped_by` spellings.
- [x] External signal => signal number, no `stopped_by`; when physical exit is
      `Signal(n)` and the final stop record has no semantic reason, render the
      v0.17.5 product-only outside-exit line exactly once. A reasoned connector
      stop sequence suppresses `from=outside` even when stop evidence is
      NoTarget/Unknown because direct-child fallback may have delivered the
      physical signal.
- [x] Retain only dotlink-owned cancellation origin needed to map shared
      `Cancelled` to product `cancel` vs `shutdown`.
- [x] Cleanup/Drop are attempt paths, not stop reasons; reasonless housekeeping
      never produces `stopped_by` or an operator stop line.
- [x] NoTarget and Unknown remain distinct; shared `StopTracker` preserves an
      original reason across Unknown retry without reconstructing evidence.
- [x] Preserve actual execution deadline/race semantics through local v0.17.5.

### D2 — final stop/exit records -> operator logs; lifecycle events -> diagnostics

- [x] Attempt-level stop evidence is not mapped one-for-one to operator output;
      dotlink logs only the final assembled completion record.
- [x] Render exact fixed-order `LOG job stop id=... reason=... outcome=...`
      exactly once from the final shared `StopRecord` plus product-only
      cancel-vs-shutdown context. Reasonless housekeeping emits no stop line.
- [x] Establish an always-visible `LogConfig::lifecycle` channel used by
      production notices today; focused coverage proves identical LOG output at
      quiet, TOOL, TOOL+REQ and REQ-only verbosity.
- [x] Prove stop/outside-exit lines are mutually exclusive, always-visible and
      survive default, verbose and quiet modes with real-binary tests.
- [x] Port non-panicking structured logger writes: LOG/TOOL/REQ lines and
      progress continuations use locked best-effort stderr writes; focused tests
      prove BrokenPipe is returned/ignored and Drop-time logging is safe during
      an active unwind.
- [x] Real-binary timeout test: exactly one stop line and no outside-exit line
      at quiet/default/verbose.
- [x] v0.17.5 real-binary external SIGKILL test at quiet/default/verbose:
      exactly one `LOG job exit id=<id> signal=<token> from=outside`, no stop
      line, no `stopped_by`; signal token is platform name or bare number with
      no whitespace.
- [x] Focused projection tests cover reasonless housekeeping plus
      delivered/gone/failed stop evidence without changing `stopped_by` rules.

### D3 — Job/Task terminal projection

- [x] Remove private duplicate process termination types/booleans; host
      completion retains shared `ProcessExit` and `StopRecord` facts directly.
- [x] Project shared compact `TaskTerminalDetail { exit, stop }` into MCP error `data`.
- [x] Preserve reaped exit/signal behind later internal/spool failure.
- [x] Preserve prior deadline through `stop=Timeout` behind later Task failure.
- [x] Use one dotlink compatibility mapper for signal/stopped_by/prose across
      synchronous result, `jobs_get` and Task failure.
- [x] Compact handoff remains a handle, not a second process verdict.

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
- [x] Document and implement stop-line/evidence semantics from shared AW facts;
      D1-D3 now make this a current-main contract rather than a future one.
- [x] Document current receipt timestamp semantics and the roughly 30-minute
      implementation-turn guidance as workflow, not a runtime timeout.
- [x] Full semantic disposition matrix written in `V0174-EQUIVALENCE.md`;
      D1-D3 release behavior is now Done/Superseded on current main; D4-D7 and
      final/native-platform gates remain explicitly open.
- [x] Pre-repin full binary/integration baseline green after fresh-review parity fixes:
      main binary **698 passed / 0 failed / 1 intentional ignored**;
      integrations **40/40** across child-env, exit-status, OAuth rotation,
      private generations and stdio tool suites.
- [x] Pre-repin strict all-target/all-feature `cargo check` + Clippy green.
- [x] Pre-repin rustfmt + `git diff --check` green.
- [x] Nix module + Alejandra + repo-wide Markdown lint green using the existing
      command-local immutable NSS CA bundle.
- [x] Post-repin D1-D3 + existing D4/B6/pre-0.17 regressions are green in the
      full current-main suite. **Mandatory rerun after D4-D7 remains open for
      final equivalence.**
- [x] Port v0.17.5 hard-deadline tool/schema wording: `timeout_secs` explicitly
      stops the command tree and is distinct from the caller/transport response
      wait; `background:true` changes waiting only. Focused schema regression
      passes. The v0.17.5 outside-signal line is now implemented from shared
      physical exit/final-stop facts without a duplicate local signal provenance model.
- [x] Post-repin/D1-D3 qualification: main binary **701 passed / 0 failed /
      1 intentional ignored**; integrations **42/42** including real external
      SIGKILL and timeout lifecycle tests at quiet/default/verbose; locked
      all-target/all-feature check + strict Clippy, rustfmt/diff, Markdown lint
      and NixOS module check are green.

### Deferred DS — sandbox/config-v13

- [ ] **DO NOT IMPLEMENT without explicit user promotion.**
- [ ] Follow the frozen AW sandbox plan only after promotion.

## Tests and verification

- [x] physical-exit/final-stop-record matrix, including timeout/cancel/shutdown product projection.
- [x] real stop/outside-exit mutual-exclusion integration test at every verbosity.
- [x] failed native Task terminal-detail parity, including prior timeout + signal retention.
- [x] huge Job ref/error bound over stdio/local HTTP.
- [x] zero-byte rejection + one-byte jobs_read page inside normal response envelope.
- [x] receipt metadata typed-media/structured replay tests.
- [x] heartbeat backpressure/deadline tests.
- [x] Nix reserved-environment evaluation tests.
- [ ] full dotlink v0.17.x equivalence gate through local release 0.17.5.
