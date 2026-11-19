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
- **Blocked** — no current semantic row remains blocked after final closeout;
  unavailable host/platform runtime smokes are recorded separately rather than
  weakening or misclassifying the semantic matrix. AW is pinned at
  `1a5d0f65208e4a05b737c804a55a3aaad3e2fa59`; D0-D7 are complete.
- **Deferred** — intentionally outside this active workstream.

## v0.17.0 / v0.17.1 behavior

| Release item | Disposition on current main |
| --- | --- |
| response-window invariant / hosted hold | **Done.** Current authenticated transport remaining-window logic narrows synchronous observation without rewriting Process execution timeout. |
| explicit output cleanup / ACK | **Superseded.** Current `jobs_ack` has explicit Through(offset) and omitted-offset Finish semantics with delivery/owner proof. Do not restore old one-shot `job_ack`. |
| progress continuations | **Done D7.** Current main uses lightweight `aw-mcp-protocol` for token-gated heartbeat scheduling, monotonic unknown-total ticks, bounded delivery and attempted/delivered summaries; dotlink retains only rmcp peer/token extraction plus product logging. Full `aw-mcp` is not a dependency. |
| both-stream cleanup | **Superseded.** Current Job ownership/Finish semantics cover retained stdout+stderr and other owners; no release-local cleanup path should return. |
| compact ACK receipt | **Done D4.** Current `jobs_ack` renders shared typed `Receipt::Ack` facts from authoritative output ACK state; the local `ControlReceipt` grammar is gone and exact replay keeps typed Job identity without reparsing prose. |
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
| `74820ff` bounded caller-controlled Job diagnostics | **Done D4.** Current main uses AW `bounded_diagnostic` for generic Job diagnostics while product MCP envelope fitting remains dotlink-owned. |
| `06d5548` raw page vs serialized response envelope | **Done D5.** Current main resolves raw pages through shared `JobProfile::resolve_read_page` into `RawPageBytes`, carries an independent `SerializedResultBytes` envelope, reads the requested raw source window before projection, and credits only the fitted prefix. Real stdio proves zero-byte rejection, one-byte delivery/ACK and absolute-offset continuation. |
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

Current main has shared receipt timestamp behavior:

```text
_meta["io.abird/receipt"]["ts"] = UTC Unix seconds
visible result line             = ts=<same value>
replay                           = reuse stored ts
```

**Done D6.** Current main consumes lightweight `aw-mcp-protocol` directly for
Complete-result validation/fitting and receipt projection. Typed media identity
is exact; structured/result metadata is preserved; reserved receipt fields
survive timestamp mutation; malformed reserved metadata fails transactionally;
stored timestamps win; a visible `ts=` is rechecked after fitting and never pays
a duplicate sidecar reserve; text truncation always has an explicit marker; and
immutable facts fail explicitly. Cached replay state is stamped but never fitted
to one request, and native Task replay uses the same shared decorator with its
stable first-finalization timestamp.

## Current progress equivalence

**Done D7.** Current main uses lightweight `aw-mcp-protocol` and only emits
heartbeats when a request carries a progress token. Production defaults remain
10 s heartbeat interval / 100 ms send budget; ticks are monotonic,
unknown-total and message-free; shared attempted/delivered summary maps to the
existing product progress log.

The shared scheduler preserves the stronger backpressure contract: a blocked
notification never stops polling the underlying operation, and an in-flight send
is dropped if the operation completes first. Shell waits retain their real
response-window deadline. Current dotlink exposes no real cumulative AW semantic
progress source, so no percentages/totals/messages are synthesized from heartbeat
count, elapsed time, Job revision or output bytes; shared semantic-progress
projection remains the required path when factual progress exists.

## Current operational workflow guidance

The project's roughly **30-minute implementation-turn** rule is agent workflow,
not a protocol/runtime timeout. It must never be conflated with:

- shell execution timeout;
- hosted response window;
- `jobs_get/jobs_read` wait limits;
- Job retention;
- cancellation/deadline semantics.

## Final equivalence qualification

The v0.17.x → current-main **semantic matrix is fully green** for every
non-deferred behavior through local release `0.17.5`. The reviewed signed
implementation checkpoint is
`fa01e18e3bb023dfe1d60b8ed45fd0ce53bfc0f7`.

Closing checkpoint on x86_64 Linux:

- final post-D4-D7 main suite: **701 passed / 0 failed / 1 intentional ignored**;
- integrations: **42/42** across child environment, lifecycle, exit/status,
  OAuth live rotation, private generations and stdio tools;
- locked all-target/all-feature `cargo check` and strict Clippy `-D warnings`:
  green;
- rustfmt, Alejandra, repo-wide Markdown lint and `git diff --check`: green;
- real flake `checks.x86_64-linux.nixos-module`: green against the current
  working tree and pinned inputs;
- executable native Linux closeout: hardware discovery/policy **4/4**,
  process-tree timeout containment **1/1**, and real signal/timeout lifecycle
  **2/2**.

Runtime/deployment gates unavailable on this host do not weaken the semantic
result: the connector exposes Bubblewrap only through a direct Nix-store path,
which dotlink correctly rejects as launcher selection authority, so the forced
Bubblewrap device-isolation smoke is host-gated rather than bypassed. Native
Windows/macOS runtime/deployment checks remain external. The v0.17.2/v0.17.3
sandbox/config-v13 bundle remains intentionally **Deferred** until explicitly
promoted by the user.
