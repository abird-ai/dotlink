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
- **Blocked** — only still-open D4-D7 consumer replacement/final gates use this
  status. AW is pinned at `1a5d0f65208e4a05b737c804a55a3aaad3e2fa59`; D0-D3 are complete.
- **Deferred** — intentionally outside this active workstream.

## v0.17.0 / v0.17.1 behavior

| Release item | Disposition on current main |
| --- | --- |
| response-window invariant / hosted hold | **Done.** Current authenticated transport remaining-window logic narrows synchronous observation without rewriting Process execution timeout. |
| explicit output cleanup / ACK | **Superseded.** Current `jobs_ack` has explicit Through(offset) and omitted-offset Finish semantics with delivery/owner proof. Do not restore old one-shot `job_ack`. |
| progress continuations | **Done locally; shared cutover blocked.** Current main has token-gated bounded progress and attempted/delivered summaries. D7 now replaces the local helper with lightweight `aw-mcp-protocol`; the full AW MCP client adapter is not a dotlink dependency. |
| both-stream cleanup | **Superseded.** Current Job ownership/Finish semantics cover retained stdout+stderr and other owners; no release-local cleanup path should return. |
| compact ACK receipt | **Done locally; shared cutover blocked.** Current `ControlReceipt` preserves same-Job/no-rerun semantics; D4 now moves rendering to shared AW receipts. |
| `--no-tunnel` | **Done.** Applied before OpenAI credential/connection environment parsing. |
| compact durable Job payloads | **Superseded.** Current flat `jobs_*` surface and compact `j:<id> ... no-rerun` receipts replace release `job_*` payloads. |
| `utf8_lossy` output projection | **Done.** Current retained output projection labels lossy UTF-8 explicitly and keeps raw-byte offsets. |
| strict tool argument surfaces | **Done.** Current schemas/decoders remain strict and current `jobs_*` arguments are bounded. |
| AW registry migration | **Superseded.** Current main already consumes shared AW Toolset/JobManager semantics; P9 continues reducing the remaining private duplicate projections. |
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
| `c185558` signal identity + stop cause/provenance + stop operator line | **Done/superseded D1-D3.** Current main consumes shared `ProcessExit`, `StopRecord`, `StopTracker`, `ManagedProcessGuard` and terminal detail; dotlink retains only `cancel` vs `shutdown`, wire wording and logging. The old private stop model is gone. |
| external Unix signal number | **Done.** Reaped status maps through `aw_platform::exit_signal` to shared `ProcessExit::Signal(i32)` and is projected consistently to result/Job/Task surfaces. |
| `stopped_by` compatibility spelling | **Product-only — Done.** One mapper owns exact `timeout/cancel/shutdown/failed`; only `Stopped + semantic reason` may emit it. |
| `LOG job stop ...` | **Product-only — Done.** Final completion emits exactly one fixed-order lifecycle line; attempt evidence is never mapped one-for-one. Quiet/default/verbose real-binary tests are green. |
| `74820ff` bounded caller-controlled Job diagnostics | **Done defensively; shared cutover blocked.** Current main has a 16 KiB UTF-8-safe private `bounded_job_error`; D4 now replaces generic duplicate bounding with AW `bounded_diagnostic`. |
| `06d5548` raw page vs serialized response envelope | **Behavior fixed locally; shared type cutover blocked.** Current main rejects `jobs_read(limit=0)`, accepts one raw byte, keeps page bytes independent from serialized response capacity, and proves one-byte delivery/ACK/absolute-offset continuation over real stdio. AW owns `RawPageBytes` / `SerializedResultBytes` and the rejecting resolver; D5 now consumes them. |
| `30099aa` module-owned Nix environment | **Product-only — Done D8.** Current module rejects all eight owned keys through one assertion and loop-test while preserving non-owned DOTLINK/RUST_LOG/XDG environment. |
| `5b27784` runtime-health/stop docs | **Docs adapted.** Current health doc uses `jobs_*`, current wait/tail semantics, current receipt timestamps, 30-minute workflow guidance, and labels stop evidence as post-D1-D3 expectation rather than current fact. |
| `8874013` platform signal-label ownership | **Superseded/Done.** Current main consumes `aw_platform::signal_name(number)` and keeps product prose outside canonical state; no local signal table remains. |
| `fd1cdb6` v0.17.4 version/tag | **Release bookkeeping only.** No version/tag action in this workstream without explicit authorization. |

## v0.17.5 local release additions

| Commit / behavior | Disposition on current main |
| --- | --- |
| `f9537bb` outside-signal lifecycle record | **Product-only — Done.** `ProcessExit::Signal(n)` plus no reasoned final stop sequence emits exactly one `LOG job exit ... from=outside`; a reasoned stop record suppresses it even for NoTarget/Unknown. Real SIGKILL coverage passes at quiet/default/verbose. |
| stop-vs-exit mutual exclusion | **Done.** Connector semantic stop => exactly one stop line/no outside-exit. Unattributed physical signal => exactly one outside-exit/no stop line/no `stopped_by`. Both use the same final factual predicate as result prose. |
| signal token | **Product-only — Done.** Uses `aw_platform::signal_name(n)` when known, otherwise bare decimal `n`; no local signal-name table. |
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

1. D4 typed refs/shared receipts/bounded diagnostics.
2. D5 shared raw-page vs serialized-result budget types.
3. D6 shared `aw-mcp-protocol` receipt/result fitter replacement.
4. D7 shared heartbeat/progress adapter replacement.
5. Full post-D4-D7 binary/integration/Nix equivalence rerun.
6. Native platform gates where capable.

D1-D3/v0.17.5 process lifecycle behavior is implemented and qualified on
current main. The matrix remains not fully green only because D4-D7 shared
consumer replacement and native/final gates remain open.
