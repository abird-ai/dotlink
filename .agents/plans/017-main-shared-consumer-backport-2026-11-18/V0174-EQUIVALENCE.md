# v0.17.x → current main semantic equivalence matrix

Authority:

- current dotlink source/tests on L5;
- AW P1-P8 shared-runtime design/implementation;
- current release worktree `release/0.17 @ 3995448d7bf33794ef84c7bb51150d77cc260ada`
  (`0.17.5`; read-only in this workstream);
- `origin/release/0.17 @ 3995448d7bf33794ef84c7bb51150d77cc260ada`;
- `v0.17.5` resolves to `3995448d7bf33794ef84c7bb51150d77cc260ada`;
- tagged `v0.17.4 @ fd1cdb6429a341a4e3d27805cae0bec288fc26df`.

This is a **behavioral disposition matrix**, not a merge plan. Release-local
runtime machinery is not architectural authority.

Status meanings:

- **Done** — current main already implements the required behavior.
- **Superseded** — current main/shared AW has a newer equivalent contract; do
  not restore the release-local mechanism.
- **Product-only** — intentionally remains dotlink-owned.
- **Blocked** — shared implementation is published at exact AW pin
  `7c7de6c9aeedf714adbe67b17ac863f27697e90e`; only dotlink atomic repin/cutover remains.
- **Deferred** — intentionally outside this active workstream.

## v0.17.0 / v0.17.1 behavior

| Release item | Disposition on current main |
| --- | --- |
| response-window invariant / hosted hold | **Done.** Current authenticated transport remaining-window logic narrows synchronous observation without rewriting Process execution timeout. |
| explicit output cleanup / ACK | **Superseded.** Current `jobs_ack` has explicit Through(offset) and omitted-offset Finish semantics with delivery/owner proof. Do not restore old one-shot `job_ack`. |
| progress continuations | **Done locally; shared cutover blocked.** Current main has token-gated bounded progress and attempted/delivered summaries. D7 replaces the local helper with lightweight `aw-mcp-protocol` after repin; the full AW MCP client adapter is not a dotlink dependency. |
| both-stream cleanup | **Superseded.** Current Job ownership/Finish semantics cover retained stdout+stderr and other owners; no release-local cleanup path should return. |
| compact ACK receipt | **Done locally; shared cutover blocked.** Current `ControlReceipt` preserves same-Job/no-rerun semantics; D4 moves rendering to shared AW receipts after repin. |
| `--no-tunnel` | **Done.** Applied before OpenAI credential/connection environment parsing. |
| compact durable Job payloads | **Superseded.** Current flat `jobs_*` surface and compact `j:<id> ... no-rerun` receipts replace release `job_*` payloads. |
| `utf8_lossy` output projection | **Done.** Current retained output projection labels lossy UTF-8 explicitly and keeps raw-byte offsets. |
| strict tool argument surfaces | **Done.** Current schemas/decoders remain strict and current `jobs_*` arguments are bounded. |
| AW registry migration | **Superseded.** Current main already consumes shared AW Toolset/JobManager semantics; P9 continues reducing private duplicate projections after repin. |
| root/schema validation | **Done/superseded.** Current shared schemas plus dotlink transport validation are stricter than the release-local check. |
| receipt count units | **Done/superseded.** Current receipts/tool results carry explicit semantic fields/units; do not restore release prose formatting. |
| release runtime-health baseline | **Done as docs.** `.agents/docs/runtime-health-check.md` is adapted to current `jobs_*`, 30-second observation waits, explicit ACK and current timestamp semantics. Historical raw counts remain historical only. |

## v0.17.2 / v0.17.3 sandbox/config-v13 bundle

Release commits:

- `21de8b3`
- `e7db299`
- `349a7d0`
- `cb7418a`
- `c71f426`
- `6f48703`
- `ba947c6`
- `ae4d623`
- `d855672`
- `0983d39`

Disposition: **Deferred**.

The reusable design belongs to the frozen AW sandbox/mount plan and dotlink's
deferred sandbox consumer plan. Do not port release-local mount/config-v13
machinery until the user explicitly promotes that scope.

Release/version-only commits `e74d753` and `acb4044` are bookkeeping, not
runtime behavior.

## v0.17.4

| Commit / behavior | Disposition on current main |
| --- | --- |
| `c185558` signal identity + stop cause/provenance + stop operator line | **Blocked D1-D3 consumer cutover.** AW owns physical `ProcessExit`, minimal stop facts, zero-cost `StopTracker`, terminal detail, attempt-level lifecycle events and one final `ProcessReceipt::stop_record()`. Dotlink must consume those after publish/repin while retaining only `cancel` vs `shutdown` context, wire wording and operator logging. |
| external Unix signal number | **Blocked consumer projection.** AW already implements `ProcessExit::Signal(i32)`; dotlink is still pinned to older AW and cannot consume it yet. |
| `stopped_by` compatibility spelling | **Product-only projection, blocked on shared facts.** Dotlink owns exact `timeout/cancel/shutdown/failed` strings after D1-D3. |
| `LOG job stop ...` | **Product-only final projection, blocked on shared cutover.** Attempt-level `ExecutionLifecycleEvent::ProcessStop` is diagnostic only; D2 renders exactly one operator line from the runner's final `ProcessReceipt::stop_record()` plus dotlink cancel-vs-shutdown context. The logger path is already locked, non-panicking, Drop-safe and always visible. |
| `74820ff` bounded caller-controlled Job diagnostics | **Done defensively; shared cutover blocked.** Current main has a 16 KiB UTF-8-safe private `bounded_job_error`; D4 replaces generic duplicate bounding with AW `bounded_diagnostic` after repin. |
| `06d5548` raw page vs serialized response envelope | **Behavior fixed locally; shared type cutover blocked.** Current main rejects `jobs_read(limit=0)`, accepts one raw byte, keeps page bytes independent from serialized response capacity, and proves one-byte delivery/ACK/absolute-offset continuation over real stdio. AW owns `RawPageBytes` / `SerializedResultBytes` and the rejecting resolver; D5 consumes them after repin. |
| `30099aa` module-owned Nix environment | **Product-only — Done D8.** Current module rejects all eight owned keys through one assertion and loop-test while preserving non-owned DOTLINK/RUST_LOG/XDG environment. |
| `5b27784` runtime-health/stop docs | **Docs adapted.** Current health doc uses `jobs_*`, current wait/tail semantics, current receipt timestamps, 30-minute workflow guidance, and labels stop evidence as post-D1-D3 expectation rather than current fact. |
| `8874013` platform signal-label ownership | **Superseded in AW; consumer blocked.** AW platform now owns `signal_name(number)`; dotlink should consume it with D1-D3 and keep product prose outside canonical state. |
| `fd1cdb6` v0.17.4 version/tag | **Release bookkeeping only.** No version/tag action in this workstream without explicit authorization. |

## v0.17.5 local release additions

| Commit / behavior | Disposition on current main |
| --- | --- |
| `f9537bb` outside-signal lifecycle record | **Product projection, D1/D2 cutover blocked.** v0.17.5 emits exactly one `LOG job exit id=<id> signal=<token> from=outside` when a process physically exits by signal and dotlink did not own a semantic stop sequence. Current main intentionally does not duplicate Unix signal extraction while pinned to old AW. After repin, derive the predicate from shared `ProcessTerminal.exit=Signal(n)` plus final `ProcessReceipt::stop_record()` having no semantic reason; attempt events are not enough. A reasoned stop record suppresses `from=outside` even for NoTarget/Unknown because direct-child fallback may have delivered the physical signal. |
| stop-vs-exit mutual exclusion | **Blocked on D1/D2 shared facts.** Connector semantic stop => one stop line, no outside-exit line. Unattributed physical signal => one outside-exit line, no stop line/stopped_by. Both are always-visible lifecycle records and use the same final factual predicate as result prose. |
| signal token | **Product-only projection.** Use `aw_platform::signal_name(n)` when known, otherwise bare decimal `n`; never spaces inside the `signal=` value. Do not add signal-name tables to dotlink. |
| `a1f8505` timeout/deadline wording | **Done locally.** Current shell tool/schema now says `timeout_secs` is a hard execution deadline that stops the command tree, distinct from caller/transport wait; `background:true` returns the durable handle immediately without removing the execution deadline. Focused schema regression passes. |
| `3995448` release 0.17.5 version bump | **Release bookkeeping only.** No tag/version action here without explicit authorization. |

## Current timestamp equivalence

Current main already has local receipt timestamp behavior:

```text
_meta["io.abird/receipt"]["ts"] = UTC Unix seconds
visible result line             = ts=<same value>
replay                           = reuse stored ts
```

`aw-mcp-protocol` implements the shared equivalent. D6 is a thin-consumer
replacement, not a behavior invention. Current-main compatibility is fully
behavior-locked: typed media identity is exact; structured/result metadata is
preserved; reserved receipt fields survive timestamp mutation; malformed
reserved metadata fails transactionally; stored timestamps win; a visible
`ts=` is rechecked after fitting and never pays a duplicate sidecar reserve;
text truncation always has an explicit marker; immutable facts fail explicitly.

## Current progress equivalence

Current main already has bounded progress and only emits when a request carries
a progress token. Lightweight `aw-mcp-protocol` owns the final shared helper.

D7 must preserve:

- 10 s default heartbeat interval;
- 100 ms per-send budget;
- monotonic unknown-total/no-message heartbeat ticks;
- attempted/delivered product logging.

Current main now already matches AW's stronger scheduling behavior: a
backpressured notification does not stop polling the underlying operation, and
an in-flight send is dropped if the operation completes first. D7 therefore
remains a shared type/observer/semantic-progress cutover rather than a pending
behavioral fix.

## Current operational workflow guidance

The project's roughly **30-minute implementation-turn** rule is agent workflow,
not a protocol/runtime timeout. It must never be conflated with:

- shell execution timeout;
- hosted response window;
- `jobs_get/jobs_read` wait limits;
- Job retention;
- cancellation/deadline semantics.

## Equivalence blockers before the matrix can be called fully green

1. Signed fresh-review AW source checkpoint and explicit publish authorization.
2. Atomic dotlink AW repin to that exact published commit, adding `aw-platform`
   and `aw-mcp-protocol` (not full `aw-mcp`).
3. D1-D7 shared-consumer cutover.
4. D1-D3 stop/outside-exit mutual-exclusion real-binary equivalence through
   local release 0.17.5.
5. Full post-cutover binary/integration/Nix qualification.
6. Native platform gates where capable.

Until then, the current v0.17.x release behavior through local 0.17.5 is fully
**classified**, but not fully **implemented on current main**.
