# v0.17.x → current main semantic equivalence matrix

Authority:

- current dotlink source/tests on L5;
- AW P1-P8 shared-runtime design/implementation;
- `release/0.17 @ 887401398da297c458ffa2a152a07e1b85870c35`;
- tagged `v0.17.4 @ fd1cdb6429a341a4e3d27805cae0bec288fc26df`.

This is a **behavioral disposition matrix**, not a merge plan. Release-local
runtime machinery is not architectural authority.

Status meanings:

- **Done** — current main already implements the required behavior.
- **Superseded** — current main/shared AW has a newer equivalent contract; do
  not restore the release-local mechanism.
- **Product-only** — intentionally remains dotlink-owned.
- **Blocked** — shared implementation exists in the unpublished AW P1-P8 tree;
  dotlink consumption waits for authorized publish/repin.
- **Deferred** — intentionally outside this active workstream.

## v0.17.0 / v0.17.1 behavior

| Release item | Disposition on current main |
| --- | --- |
| response-window invariant / hosted hold | **Done.** Current authenticated transport remaining-window logic narrows synchronous observation without rewriting Process execution timeout. |
| explicit output cleanup / ACK | **Superseded.** Current `jobs_ack` has explicit Through(offset) and omitted-offset Finish semantics with delivery/owner proof. Do not restore old one-shot `job_ack`. |
| progress continuations | **Done locally; shared cutover blocked.** Current main has token-gated bounded progress and operator attempted/delivered summaries. D7 replaces the local helper with AW's stronger shared adapter after repin. |
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
| `c185558` signal identity + stop cause/provenance + stop operator line | **Blocked D1-D3.** AW P1-P3 now owns physical `ProcessExit`, minimal `StopReason`/`StopEvidence`/`StopRecord`, terminal detail and lifecycle events. Dotlink must consume those after authorized AW publish/repin while retaining only `cancel` vs `shutdown` context, fixed wire wording and operator logging. Current pre-repin main must not grow a duplicate stop model. |
| external Unix signal number | **Blocked consumer projection.** AW already implements `ProcessExit::Signal(i32)`; dotlink is still pinned to older AW and cannot consume it yet. |
| `stopped_by` compatibility spelling | **Product-only projection, blocked on shared facts.** Dotlink owns exact `timeout/cancel/shutdown/failed` strings after D1-D3. |
| `LOG job stop ...` | **Product-only observer, blocked on shared lifecycle event.** D2 consumes AW `ExecutionLifecycleEvent::ProcessStop`; no current-main line should be treated as missing until that cutover lands. The prerequisite structured logger path is already hardened to locked, non-panicking best-effort stderr writes, including Drop-time unwind coverage, and exposes a production-used always-visible lifecycle channel independent of verbosity. |
| `74820ff` bounded caller-controlled Job diagnostics | **Done defensively; shared cutover blocked.** Current main has a 16 KiB UTF-8-safe private `bounded_job_error`; D4 replaces generic duplicate bounding with AW `bounded_diagnostic` after repin. |
| `06d5548` raw page vs serialized response envelope | **Behavior fixed locally; shared type cutover blocked.** Current main now advertises/accepts `jobs_read(limit=1)`, keeps the raw page independent from the serialized response capacity, and proves one-byte delivery/ACK/absolute-offset continuation over real stdio. AW P4 still owns the final `RawPageBytes` / `SerializedResultBytes` types and resolver; dotlink must consume those after repin before D5 is architecturally complete. |
| `30099aa` module-owned Nix environment | **Product-only — Done D8.** Current module rejects all eight owned keys through one assertion and loop-test while preserving non-owned DOTLINK/RUST_LOG/XDG environment. |
| `5b27784` runtime-health/stop docs | **Docs adapted.** Current health doc uses `jobs_*`, current wait/tail semantics, current receipt timestamps, 30-minute workflow guidance, and labels stop evidence as post-D1-D3 expectation rather than current fact. |
| `8874013` platform signal-label ownership | **Superseded in AW; consumer blocked.** AW platform now owns `signal_name(number)`; dotlink should consume it with D1-D3 and keep product prose outside canonical state. |
| `fd1cdb6` v0.17.4 version/tag | **Release bookkeeping only.** No version/tag action in this workstream without explicit authorization. |

## Current timestamp equivalence

Current main already has local receipt timestamp behavior:

```text
_meta["io.abird/receipt"]["ts"] = UTC Unix seconds
visible result line             = ts=<same value>
replay                           = reuse stored ts
```

AW P6 implements the shared equivalent. D6 is therefore a thin-consumer
replacement, not a behavior invention. Current-main compatibility is now fully
behavior-locked: real stdio + hosted `bread(format=mcp)` preserve exact
Image/Audio/Resource block identity and bytes; decorator coverage preserves
structured content plus stored/visible receipt metadata and replay timestamp
identity; the local serialized-result fitter mutates text only and explicitly
fails when immutable facts cannot fit; result receipt metadata is capped at
16 KiB transactionally, stored replay timestamps win, and visible `ts=` markers
are not duplicated.

## Current progress equivalence

Current main already has bounded progress and only emits when a request carries
a progress token. AW P7 owns the final shared helper.

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

1. Explicit authorization to sign/commit/publish the AW P1-P8 implementation.
2. Atomic dotlink AW repin to that exact published commit.
3. D1-D7 shared-consumer cutover.
4. D5 one-byte/raw-page vs serialized-envelope regression on dotlink.
5. D1-D3 stop/signal/operator-line real-binary equivalence.
6. Full post-cutover binary/integration/Nix qualification.
7. Native platform gates where capable.

Until then, v0.17.4 is fully **classified**, but not fully **implemented on
current main**.
