# Unified Jobs + Agent runtime — dotlink integration TODO

Full semantic authority: AW `ai-native-unified-jobs-agent-runtime-2026-11-11/DESIGN.md`
AW implementation order: companion AW `TODO.md`

This file contains only dotlink-owned integration work. Do not duplicate AW Policy,
Session, Agent, guardian or routing semantics here.

## Integration discipline

- **D1 standalone finite Jobs** may start after AW **Checkpoints A/B** expose reviewed shared
  ref/Job contracts. It does not depend on AW Session sharing or delegated Agents.
- **D2 AW-authenticated bridge** waits for AW **Checkpoint E** and policy/Start admission
  contract. D3 follows D2; AW final G validates the product together.
- Pin/consume one exact reviewed shared AW revision at each integration boundary; no sibling path/copy.
- Develop with targeted checks. Run the grouped dotlink checkpoint once per vertical slice,
  not after every edit.
- Carry forward unchanged sandbox/native/replay results; re-run them only when their owner or
  dependency changed, or at the final capable-host gate.
- Preserve standalone/local hot paths: no AW Session/Policy/provider dependency.

## D1 — shared revision + standalone jobs cutover

- [x] Consume exact reviewed shared `aw-tools`/ref/Job types and mechanical flat-name →
      semantic-ID mapping.
      - 2026-11-13: pinned `aw-tools`, `aw-tools-fs-unix` and `aw-tools-fs-win` to reviewed AW
        implementation checkpoint `9e9ab043410241c3f797f760d93a1cfd9126fdd2`. Standalone
        `j:<handle>[/n:<coordinate>]` parsing now uses shared `ExecutionRef`/typed suffixes and
        shell-name resolution uses shared `OperationId`; no AW Agent/Session/Policy host layer is
        imported.
- [x] Replace old standalone job_* routes with `jobs_list/get/read/ack/send/cancel`;
      shell/`batch background:true` replaces start and fresh constructors replace restart.
- [x] Preserve Process natural stdout, Graph natural result/node coordinate reads, retention/ACK,
      same-Job handoff, stdin current-authority checks, cancellation/unknown settlement,
      replay/MCP Task ownership and protected-path/sandbox invariants. Every startup
      generates a fresh authenticated runtime generation: old finite `j:` refs are never
      reused, old cursors/proofs reject, and live Process/Graph Jobs cannot resurrect.
- [x] Keep standalone/model Graph on reviewed DL-150 native/shell leaves. AW-hosted model
      Graphs additionally allow `agent` with original Agent/Turn/child-effect owner; dotlink
      standalone never exposes it. **Nested `batch` is rejected on model/SDK/host Graphs**.
      Qualified typed SDK/host `jobs.*` leaves do not expand the standalone provider schema.

### Dotlink checkpoint 1 — standalone

- [x] Run together: flat catalog, shell sync/handoff/background, Graph whole/node reads,
      ACK ownership, authorized→revoked stdin, cancel/unknown, duplicate control replay,
      **runtime restart rejects stale Job refs/cursors without ID reuse or resurrection**,
      protocol-clean stdio, and representative protected-path/sandbox denial.
      - 2026-11-13: focused changed-path gates plus owner-sensitive MCP Task/replay/stdio/
        protected-path carry-forward are green on L5. Graph root `result` is synthesized and
        non-ACKable; exact node ACK is fenced to the highest offset actually returned by
        `jobs_read`.
      - 2026-11-14 repin qualification against AW `9e9ab043410241c3f797f760d93a1cfd9126fdd2`:
        `cargo fmt --all -- --check`, locked all-feature tests and strict all-target Clippy are
        green; the main binary suite is 637 passed / 1 intentionally ignored and all integration
        suites, including canonical stdio Jobs flow, pass.

## D2 — authenticated AW bridge

- [x] Accept only AW-resolved canonical refs/operations and authenticated original replay ID,
      actor/source/site/resource/grant/membership/writer provenance. AW Policy/Session/team
      semantics remain opaque upstream facts.
- [x] Define one **per-attempt AW source-fenced Start authorization**: AW atomically
      rechecks current grants/epochs and records exact original invocation, effect-attempt ID,
      operation/ref/args digest, target dotlink host/profile **and live runtime generation**,
      bounded expiry and source/site/writer fence, serialized with revocation. dotlink
      consumes it once through the **existing** replay owner and rechecks current local
      launcher/sandbox/source/site/resource authority. Proof expiry must not outlive
      dedupe retention; no old-generation proof can restart an ambiguous effect, and AW
      may not issue a replacement Start for that same unknown attempt. AW offline blocks
      **new** authorization, not an already committed same-generation dispatch.
- [x] Preserve WriterFence requirements for durable/multi-writer remote ownership.
- [x] Keep Agent jobs/read/filter/history/parent state owned by AW. Standalone dotlink never stores it.
- [x] Reject constructor wake and later subscribe on dotlink-owned live Process/Graph sources until
      a separately reviewed atomic source-owner event protocol exists.

### Dotlink checkpoint 2 — bridge

- [x] Run together: AW→remote Process/Graph execution/control/read, stale writer/source/site denial,
      missing local sandbox grant despite upstream allow, revoked stdin before bytes, duplicate
      request idempotence, no Agent state leakage, unsupported remote wake rejection. Race
      AW revoke against remote Start: revoke-first denies; Start-first permits at most one
      accepted/unknown attempt. Test duplicate proof/response loss, expiry versus dedupe
      retention, target generation restart, offline **before new authorization** versus offline
      **after accepted Start**. Old proof never executes in a new generation; no double launch.
      - 2026-11-14 L5 grouped checkpoint-2 acceptance is green. Real production-client tests run
        `aw-mcp::RemoteBridge` over OAuth/TCP to the current dotlink binary: one explicit
        no-sandbox Process/stdin/read lane and one narrow-RW filesystem/Graph/read lane. Duplicate
        Start/control deliveries replay the original receipt and do not repeat effects. The narrow
        lane also proves upstream Start cannot bypass local path or shell/launcher grants.
      - AW source gates are green: shared remote-Start 5/5, Store remote-Start 3/3, ToolBinding
        revoke serialization, exact-replay/fresh-after-revoke behavior, writer/membership and site
        fences, policy-operation-router regression, explicit bearer-HTTP site selection and exact
        proof `_meta` serialization, plus all-feature CLI compilation.
      - dotlink target/regression gates are green: 649 passed / 1 reviewed fixture ignored and all
        integration suites; strict all-target/all-feature Clippy; explicit old-generation proof
        rejection after a new JobManager runtime; revoked interactive stdin before bytes; no
        standalone wake/`jobs_subscribe`/Agent; Tasks/replay/ACK and stdout cleanliness. Dependency
        inspection confirms no AW Agent/Store/Host/Policy/Session crate enters dotlink. A committed
        same-generation proof dispatches without an AW callback; new authorization still requires
        the live authenticated lease/commit path.

## D3 — cutover cleanup + final integration gate

- [x] Delete old job_start/job_restart/job_* compatibility decoders/adapters and dead bridge
      helpers; keep only explicit absence assertions that prove the hard cutover. No migration or
      legacy replay service exists.
- [x] Update dotlink live docs/examples to the implemented standalone/bridge surface.
- [x] Record final standalone schema bytes/tokens and confirm advanced shared host capabilities did
      not grow the model Graph/tool surface.
      - 2026-11-14 L5: actual max Unix standalone `tools/list` projection = 21 tools / 13,832
        serialized MCP `Tool` bytes (~3,458 at a deliberately rough 4-bytes/token estimate): 13
        filesystem tools + Bash + Batch + `jobs_list/get/read/ack/send/cancel`. `agent`,
        `jobs_subscribe`, wake and all old `job_*`/team/plan/review routes are absent. This MCP
        serialization envelope is not directly byte-comparable with AW's internal `ToolSpec`
        serialization.

### Dotlink checkpoint 3 — final

- [x] Run only the affected dotlink package/integration lanes plus:
  - standalone Job flows from checkpoint 1;
  - bridge authority/replay flows from checkpoint 2;
  - MCP Task/OpenAI replay/retention lanes if their owner changed;
  - native Bubblewrap/Windows/macOS gates only when touched or on a capable final host.
  - 2026-11-14 L5 final pass: `AW_BASH=... cargo test --offline --locked --all-features`
    is green after the final replay-first review at 651 passed / 0 failed / 1 intentional external-fixture ignore; all five integration
    binaries are green (1/1, 8/8, 1/1, 12/12, 4/4); strict all-target/all-feature Clippy is green.
    Explicit ownership regressions are also green: old-generation remote Start is rejected after
    runtime restart; interactive Process stdin revocation fences before bytes; standalone
    constructors expose `background` without wake or legacy Start; final Unix standalone catalog
    remains exactly 21 tools / 13,832 serialized MCP Tool bytes (~3,458 at the rough 4-byte/token
    heuristic). The real OAuth/TCP AW->dotlink Process and FS/Graph bridge flows from checkpoint 2
    remain the current cross-process evidence; neither bridge owner nor remote-Start semantics
    changed after that checkpoint, so the expensive fixture was not rerun by habit. MCP Task/OpenAI
    replay/retention owners likewise did not change. Bubblewrap filesystem/sandbox primitives were
    untouched; native Windows/macOS are not available on L5 and are not claimed here.
    Final cross-repo diff review also fixed replay-first ordering in three remote-effect owners:
    `jobs_cancel` now claims before mutable revision checks; remote `batch` claims before Graph
    preparation/topology revalidation; and remote shell claims before current local shell-policy
    recheck. A regression proves cancel replay survives the revision advanced by the first cancel,
    and the existing shell duplicate test now revokes local shell policy before redelivery while
    still receiving the original receipt. Full suite and strict Clippy are green after these fixes.
- [x] Do not requalify unchanged native filesystem/sandbox primitives merely because old routes were removed.

### Final exact shared pin

- [x] Finalized against pushed AW `0a6c6a0bea07a3886c4dc8be4f423eec3ea3c134`:
  `aw-tools`, `aw-tools-fs-unix`, and `aw-tools-fs-win` all use that exact Git revision;
  the temporary local digest implementation is removed in favor of
  `aw_tools::remote_start::arguments_sha256`; `Cargo.lock` changes only those three Git
  source revisions. Final locked qualification is green: all-target/all-feature check,
  651 passed / 1 intentional ignore plus all five integrations, strict Clippy, fmt and diff.
  No sibling path, `file://`, vendored shared copy or unpushed SHA remains.

## Completion

dotlink integration is complete when the standalone surface is canonical, the AW bridge remains
thin, current-authority-intersecting, runtime-generation-fenced and replay-safe at remote Start,
no old route is reachable, no Agent/Session/Policy engine exists in dotlink, and all
applicable grouped checkpoints pass.

## Execution guard

Implementation sequencing only. Preserve unrelated work. No push/version/tag/release/deploy
without separate explicit authorization.
