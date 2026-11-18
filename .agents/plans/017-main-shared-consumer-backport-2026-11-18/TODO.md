# 0.17 main shared-consumer backport

Base: `4f60bc3d4d2edb42c7d75f91d5dc6e8bae02d02a`

AW owner plan:
`../../../../aw/.agents/plans/017-shared-runtime-convergence-2026-11-18/TODO.md`

Release reference:
`v0.17.4 @ fd1cdb6429a341a4e3d27805cae0bec288fc26df`

## Committed

None in this plan. The pre-0.17 convergence is already committed separately at
`4f60bc3`.

## Uncommitted

### D0 — dependency boundary

- [ ] Wait for active AW shared-runtime phases P1-P8 to reach reviewed checkpoints.
- [ ] Publish AW only with explicit user authorization.
- [ ] Repin all AW Git dependencies and every lock record to the exact published commit.
- [ ] No temporary path/sibling dependency.
- [ ] Revalidate dotlink main/release refs and preserve unrelated work.

### D1 — process provenance thin consumer

- [ ] Replace dotlink-private stop/cancellation semantic enums with AW shared values where architecture permits.
- [ ] Use AW evidence-preserving managed process-tree stop primitive.
- [ ] Map reaped launcher status to shared `ProcessExit::Signal(n)`.
- [ ] Retain no parallel `stop_proven -> Cancelled/DeadlineExceeded` model.
- [ ] Map shared terminal stop cause to v0.17.4 `stopped_by` wire spellings.
- [ ] External signal => signal number, no `stopped_by`.
- [ ] Caller cancel vs runtime/authority withdrawal maps to cancel vs shutdown.
- [ ] Cleanup/Drop events never become false terminal stop claims.
- [ ] Preserve actual execution deadline/race semantics from v0.17.4.

### D2 — lifecycle event -> operator log

- [ ] Consume AW `ExecutionLifecycleEvent::ProcessStopAttempt`.
- [ ] Render exact fixed-order `LOG job stop id=... reason=... outcome=...`.
- [ ] Stop line survives every verbosity.
- [ ] Port v0.17.4 non-panicking logger writes because sink may run from Drop.
- [ ] Real-binary stop-line test: one line on connector stop.
- [ ] External kill test: no connector stop line.
- [ ] Drop/cleanup/gone/failed attempt tests.

### D3 — Job/Task terminal projection

- [ ] Remove private duplicate process termination fields where shared terminal/detail is authoritative.
- [ ] Project shared Task terminal/failure detail into MCP error `data`.
- [ ] Preserve reaped exit/signal behind later internal/spool failure.
- [ ] Preserve prior deadline detail behind later Task failure.
- [ ] Use one dotlink compatibility mapper for signal/stopped_by/prose across sync, jobs_get and Task failure.
- [ ] Compact handoff remains a handle, not a second process verdict.

### D4 — typed refs, diagnostics and shared receipts

- [ ] Replace local raw flat Job ref parsing with shared typed Job/Node ref.
- [ ] Use shared Job handoff/status/control/ACK receipts.
- [ ] Remove local `ControlReceipt`/duplicate grammar after parity.
- [ ] Keep bounded product error backstop.
- [ ] Verify huge caller-controlled ref cannot inflate stdio/local HTTP response.
- [ ] Replay under narrower budget preserves same Job/effect and metadata.

### D5 — shared page/result budget semantics

- [ ] Consume shared raw-page budget resolver.
- [ ] Consume shared serialized response-envelope budget.
- [ ] Fix current `jobs_read(limit=1)` no-transport-budget envelope bug.
- [ ] Schema/default/max and runtime derive from same JobProfile.
- [ ] Raw offsets/EOF/delivery/ACK proof unchanged under response fitting.

### D6 — aw-mcp receipt metadata and fitter

- [ ] Remove local generic receipt decorator after shared helper parity.
- [ ] Preserve immutable replay timestamp vs fresh observation timestamp.
- [ ] Preserve `_meta["io.abird/receipt"]` and visible `ts=`.
- [ ] Remove local generic serialized `CallToolResult` fitter after parity.
- [ ] Typed media/structured/raw payload identity tests over stdio + hosted path.

### D7 — aw-mcp heartbeat

- [ ] Replace local generic `ProgressHeartbeat` with aw-mcp adapter.
- [ ] Preserve authenticated progress token behavior.
- [ ] Preserve bounded per-tick send and no operation delay.
- [ ] Preserve attempted/delivered operator summary through dotlink observer/log sink.

### D8 — product-only Nix hardening

- [ ] Port v0.17.4 `30099aa` module-owned environment rejection.
- [ ] Loop test over every reserved key.
- [ ] Preserve allowed `DOTLINK_*`, `RUST_LOG`, XDG environment.
- [ ] Update Nix service documentation.

### D9 — v0.17.4 operational docs and equivalence

- [ ] Adapt runtime-health-check to current jobs_* surface and shared receipts.
- [ ] Document stop line/evidence semantics from shared AW facts.
- [ ] Document current 30-minute guidance/timestamp semantics.
- [ ] Full semantic matrix: every v0.17.4 behavior Done / Superseded / intentionally product-only.
- [ ] Full binary and integration suite.
- [ ] strict all-target/all-feature Clippy.
- [ ] fmt + diff checks.
- [ ] Nix module checks.
- [ ] Current D1-D4/B6/pre-0.17 regressions remain green.

### Deferred DS — sandbox/config-v13

- [ ] **DO NOT IMPLEMENT without explicit user promotion.**
- [ ] Follow the frozen AW sandbox plan only after promotion.

## Tests and verification

- [ ] process signal/stop/cancellation-origin matrix.
- [ ] real stop-line integration test at every verbosity.
- [ ] failed native Task terminal-detail parity.
- [ ] huge Job ref/error bound over stdio/local HTTP.
- [ ] one-byte jobs_read page inside normal response envelope.
- [ ] receipt metadata typed-media/structured replay tests.
- [ ] heartbeat backpressure/deadline tests.
- [ ] Nix reserved-environment evaluation tests.
- [ ] full dotlink v0.17.4 equivalence gate.
