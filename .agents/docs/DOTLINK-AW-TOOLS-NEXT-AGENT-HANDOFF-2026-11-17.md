# Dotlink → AW shared tools: next-agent handoff

Prepared: 2026-11-17 · source host pvl-l5 · Owner: @pvl
Purpose: dotlink entrypoint into the **canonical cross-repo handoff** and exact D0–D4 dependency boundary.
Status: documentation/handoff only; this file is not authority to commit, push, rebase, repin or release.

**Post-snapshot update:** D0–D4 and B6 are complete in the current dotlink worktree. AW `main` is published at signed `7e6fe20a95640e32610819ce94b61a121e165dea`; dotlink consumes that exact revision in all three dependency/lock records. The prior writer-quiescence checkpoint remains `beef1bc`; `7e6fe20` adds the shared Job-list metadata lower bound used by B6. Filesystem, shell, batch and finite Job provider metadata now share one immutable Toolset projection while dotlink retains policy/sandbox/replay/transport authority. Fresh exact-pin qualification is **685 passed / 1 ignored** plus **36/36** integrations; locked all-target/all-feature check and strict all-feature Clippy/fmt/diff are green. Prior D4 release qualification also remains green: Markdown 0 issues, all Nix checks, native Bubblewrap and targeted GPU/KVM/device-security smoke. Push/version/tag/release/deploy remain separately authorized actions.

**Read the full current master:**
[AW–dotlink shared tools comprehensive handoff](../../../aw/.agents/docs/AW-DOTLINK-TOOLS-FULL-COMPREHENSIVE-HANDOFF-2026-11-17.md).

The master documents the original AW Agent/ToolPolicy/TurnJournal foundations, dotlink-native 0.16/0.17 history, October A0–A8 extraction, November unified Jobs/Graph/Agent runtime, the builder/typed receipt design, every November 15–17 Job/output fix, exact remaining work, tested gates and a copy/paste next-agent prompt. Do not duplicate or supersede that 60KB history in dotlink.

## 0. Safety and first reads

Use **dotlink---l5 only**. Existing main worktrees:

- AW: /home/pvl/spaces/abird/src/aw
- dotlink: /home/pvl/spaces/abird/src/dotlink

Read each repo's `AGENTS.md`, `.agents/plans/README.md` and `.agents/plans/TODO.md` before working. AW owner index: `.agents/plans/dotlink-tool-port/`, `ai-native-toolset-builder-2026-11-15/`, `ai-native-tool-output-grep-2026-11-15/`. Dotlink current adoption plan: `.agents/plans/aw-job-output-grep-convergence-2026-11-15/{README,DESIGN,TODO}.md`. Check live source, Git status, staging, and tests: they outrank dated prose.

Never inspect, edit or stage AW `.agents/plans/ux/**` (reserved owner). Preserve unrelated dirty files. No new worktrees, A1, speculative cached patches, history rewrites or branch rebases. Commits/pushes/version/tag/deploy need separate explicit approval. ~30-minute implementation turns: one coherent unit, focused source/test gate, update owning plans, pause.

## 1. Verified state of the two repos at handoff time

| | AW | dotlink |
| --- | --- | --- |
| Source | canonical shared implementation | concrete MCP/shell/Task/transport host |
| main HEAD | `7d09d5f3c882894cd46b427a89fa75280223115a` | `6723e3e632b2a73f051d7ce4fc589d99cca33c45` |
| upstream | matched origin/main | main **ahead 1**, with existing local cache-kind revert |
| committed shared source | `0d2b4e3` merged Toolset/receipt and initial Job safety | `44b4bee` selective operational 0.17 port |
| current local state | modified shared Job/receipt/toolset, AW adapter, Job/Builder/Port plans and CURRENT_HANDOFF | dirty `src/jobs.rs`, `src/mcp/tools/jobs.rs`, `tests/stdio_tools.rs`, convergence DESIGN/TODO |
| dotlink AW pins | newer AW dirty changes **not** a commit | **all three** shared Git dependencies pinned to `0a6c6a0bea07a3886c4dc8be4f423eec3ea3c134` |
| current blocker | reviewed, signed, remotely available shared AW runtime commit | cannot safely consume uncommitted AW source; D0 blocked |

Do not call a local dirty tree a known Git revision. Avoid prematurely repinning just to run dotlink on AW HEAD. The source and plan may change between sessions; revalidate.

## 2. How we got here — essential cross-repo decisions

1. AW originally owns Agent/TurnMachine/ToolPolicy/TurnJournal and exact source/site/effect authority; dotlink originally owns one single-binary MCP/tunnel, FS/shell native tools, permissions/sandbox and HTTP/OpenAI/stdio transport.
2. Oct 25–31: ported useful dotlink native semantics into AW (historical `7ce39d9`), then refined grep, file/dir search, safe native path/mount policy, patch/edits, binary byte windows and transactional staging. `find` is not a separate model tool.
3. Oct 30–Nov 9: canonical shared `aw-tools` extraction (A0–A8). Former `aw-tools-core` renamed to `aw-tools`; AW-specific `aw-tools` became `aw-tools-adapter`. Shared `JobManager` lives in core; AW original Agent/ToolPolicy remains the only AW execution authority. Historical AW reviewed `4a23e92`, dotlink shared cutover `b31683c`.
4. Nov 10–14: unified Jobs+Agent Graph and remote Start design; AW runtime commit `0a6c6a0`; dotlink D1 bridge `c90a660`. Exactly one shared finite JobManager, deterministic frozen Graph, authenticated remote Start and immutable replay receipts. Dotlink deliberately does not import AW Agent/Session/history.
5. Parallel `release/0.16` and `release/0.17` retained original dotlink-era implementation; the user rejected rebasing main onto release/0.17. Instead selectively port **operational Phases 1–3 first**, leave shared AW-tool design last. `v0.17.1` anchor `07ca25f`; latest parallel `release/0.17` is `v0.17.3` / `acb4044` and has later mount/cache changes NOT silently in main.
6. Nov 15: main operational port (config v12, NixOS, CLI precedence, no-tunnel, authenticated response windows, progress, service safety) in `44b4bee`. No release line JobManager, `job_*` alias or multi-file edit grammar imported.
7. Nov 15: AW shared Toolset/receipt builder merged at `0d2b4e3`. Immutable typed profiles, per-tool metadata hooks, shared grep/Job caps, no clock or grants. Selected **Option A**: canonical single-file `edit`, compose multi-file work with individual `batch` Graph edit nodes.
8. Nov 15–17: AW dirty source adds shared two-phase delivery proof, independent consumer leases, current Actor/Control commit, unknown-dominant Graph status, event-driven settlement/byte waits, first-truncation/failure notifications, truthful journal-unknown receipts and guarded ACK/compaction/expiry. Latest AW plan records **338 shared, 250 adapter, 110 read-only** tests and strict checks; these are historical results for dirty source, NOT a released pin.
9. Nov 16–17 dotlink dirty source (still old AW pin) has local settle-only get, byte/EOF read, no skipped-node wait, contiguous MCP delivery fence, bounded Job/control/send receipts, native Task-prefix retention and exact remote replay. Most recent dotlink plan records **676 binary passed / 1 ignored, 5 real stdio MCP passed** at last full send checkpoint (subject to revalidation). These are interim host corrections, NOT D1/D2 shared adoption.

Primary dated records: `.agents/docs/RELEASE-017-OPERATIONS-PORT-2026-11-15.md` (selective 0.17 Phases 1–3), `.agents/docs/CURRENT_HANDOFF.md` (November 14 unified baseline), `.agents/plans/dotlink-ai-native-filesystem-2026-10-31/FULL-COMPREHENSIVE-HANDOFF-2026-11-09.md` (full original A0–A8 design), and the current `.agents/plans/aw-job-output-grep-convergence-2026-11-15/TODO.md` (every post-Nov15 dotlink checkpoint). Further initial product history in `.agents/plans/dotlink-handoff-2026-10-01/`.

## 3. Important public API decisions not yet migrated to dotlink

| Contract | Approved AW-first semantics; dotlink must not assume adopted |
| --- | --- |
| `jobs_get` | quick decision/status, default combined stdout/stderr tail **512 raw bytes total**, explicit max **1024 total**, borrowing unused per-stream share, implicit tail needs separate read grant. Positive wait ends on requested root/node settlement or deadline, not new bytes/revision; a journal unknown is a truthful observation boundary. |
| `jobs_read` | nonblocking `wait_ms=0`, absolute raw offset, negative `-N` newest-N snapshot **without wait**, positive wait at nonnegative offset for bytes/truncation/EOF/node settlement/deadline; never imply ACK. |
| `jobs_ack` | omitted offset means explicit Control-authorized **Finish** of known settled Job/node output audience (even unread bytes); explicit offset means proved contiguous Through. Unknown/in-flight journal blocks destructive changes, and original owner/watermark must validate even after physical release. |
| owner/Task | AW normal Job audience is per Agent, not per turn; optional consumer hash+enroll exists for genuinely distinct authenticated hosts, cap 16. Ordinary Job output and native MCP Task/replay result retention are separate leases. |
| Graph | frozen NodePlan and deterministic effects; aggregate **unknown if any unknown**, **mixed only settled heterogeneous**; known node may Finish independently; no nested batch/alternate JobManager. |
| grep | shared file AND dir search, explicit `context/before/after` each validated at 0..128, bounded work/ignore/glob/partial output; direct+Graph same parser and backend. |
| Toolset | immutable shared builder with typed limits and metadata-only per-tool hooks; dotlink-specific tool prose, `ts` and 35-minute guidance are host choices; no second execution registry or policy source. |
| receipts | concise typed model projection with exact Job ref/status/raw bytes/complete budget, optional host-supplied `ts`; cached remote Start cannot rerun, and a smaller envelope can only reproject the immutable original result. |

**Dotlink today still has temporary contiguous delivery bookkeeping** in `src/jobs.rs`; remove it only when using canonical AW owner proof on an approved pin. Keep dotlink's actual ToolPolicy, sandbox, OAuth, transport budgets and Task ownership. Do not simply copy AW's live Actor gate into dotlink.

## 4. Work queue: D0 → D4

**D0 (complete):** reviewed AW implementation pin `be6d48db552fd44b9dd85364cd8734454109ce68` is signed, remotely available and consumed by all three dotlink shared dependencies/lock records. Dotlink compatibility fixes and D0 qualification are committed in `d0a3922`.

**D1 (complete in the current uncommitted dotlink tree):** shared read delivery proof, Through-ACK, omitted-offset terminal Finish and aggregate `jobs_get.tail` are wired to AW on pin `26a8c78`. Tail uses 512 default / 1024 max combined stdout+stderr with shared redistribution and exact final-response fitting, and remains a pure peek with no delivery/ACK credit. The temporary dotlink watermark is legacy/test-only. Preserve Task/replay owners and authenticated remaining-window clamps.

**D2 (complete in the current uncommitted tree):** direct MCP and Graph-native grep share AW schema/parser/engine. Real stdio qualification covers exact files/directories, context 0/8/128, invalid 129, asymmetric overrides, pagination/resume and compact incomplete markers. No duplicate grep engine or nested Graph schema was added.

**D3 (complete in the current uncommitted tree):** canonical Job/Task output proof, signed read offsets and combined tail are adopted; standalone native Tasks use AW's plan-bound `task_result_owner`, while Graph remains Task-ineligible. Hosted replay distinguishes positive physical delivery from 404/expiry/ambiguous POSTs; namespace/execution-authority rotation retires old durable shell request owners through the bounded existing handoff; Busy retirement remains queued until it can succeed; positive Delivered tombstones remain final under later revocation. Optional consumer leases remain unadopted because no distinct authenticated consumer audience exists.

**D4 (complete):** independent ACK/Finish/Task/replay/retirement review and executable stdio/HTTP/remote/Task/all-feature gates are complete, including the guard-revocation Busy-discard red/green fix and test-fixture race hardening. Host Rust/lint gates are green; Markdown is 0/76; all Nix checks are green, including hermetic release tests **684/0/1 + 36/36 integrations** with a verified store output. Native full Bubblewrap qualification is also **684/0/1 + 36/36**, and all seven targeted GPU/KVM/device-security cases pass on the capable host. No D1–D4 correctness gate remains. Signing, push, version/release/deploy still need explicit approval.

Next execution slices should be ~30 minutes, one coherent checkpoint each, with completed work and tests recorded in the owning plan, then pause.

## 5. Validation checklist

Inspect exact Git SHA/status and source. Avoid claiming an AW dirty-tree test ran through dotlink's older Git dependency.

AW baseline checks (run in AW root):

    cargo fmt -p aw-tools -p aw-tools-adapter --check
    cargo check --offline --locked -p aw-tools --no-default-features
    cargo test --offline --locked -p aw-tools --all-features --lib
    AW_BASH="$(command -v bash)" cargo test --offline --locked -p aw-tools-adapter --all-features --lib
    cargo test --offline --locked -p aw-tools --features read-only --lib
    cargo clippy --offline --locked -p aw-tools -p aw-tools-adapter --all-targets --all-features -- -D warnings

Dotlink (existing main):

    cargo fmt --all -- --check
    cargo check --offline --locked
    AW_BASH="$(command -v bash)" cargo test --offline --locked --bin dotlink -- --test-threads=4
    cargo test --offline --locked --test stdio_tools
    cargo clippy --offline --locked --all-targets -- -D warnings

L5 has on occasion externally terminated large Rust test compilation, without any Rust diagnostic. That is **not** a test failure or pass. Use a bounded single-job/nonincremental retry if warranted; report executed tests precisely and stop at the agreed checkpoint.

## 6. Copyable next-agent prompt

> Use **dotlink---l5 only**. Work from existing `main` worktrees at `/home/pvl/spaces/abird/src/aw` and `/home/pvl/spaces/abird/src/dotlink`. No A1, no new worktree, no reading/editing/staging AW `.agents/plans/ux/**`, and preserve every unrelated dirty/staged file. Do **not** commit/push/tag/release/deploy or update any AW Git pin without explicit authorization. Limit the user/model turn to roughly 30 minutes; checkpoint durably and pause.
>
> Read in **each** repo `AGENTS.md`, `.agents/plans/README.md`, `.agents/plans/TODO.md`; then AW `.agents/docs/AW-DOTLINK-TOOLS-FULL-COMPREHENSIVE-HANDOFF-2026-11-17.md`, dotlink `.agents/docs/DOTLINK-AW-TOOLS-NEXT-AGENT-HANDOFF-2026-11-17.md`; AW Job/output + Toolset builder + dotlink-tool-port plan DESIGN/TODOs; dotlink aw-job-output-grep-convergence DESIGN/TODO; follow linked historical handoffs for a design's origin.
>
> Revalidate current Git state and three exact Cargo dependency pins. Current checkpoint: AW `main` and reviewed shared pin are `7e6fe20a95640e32610819ce94b61a121e165dea`; the current dotlink B6 worktree pins all three AW dependencies/lock entries to `7e6fe20`. Revalidate rather than assuming these remain current.
>
> D0–D4 are complete. Do **not** reopen D1–D4 source work without a concrete regression. All host Rust, Markdown, Nix, native Bubblewrap, and targeted GPU/KVM/device-security qualification is green. Next work requires explicit authorization for commit/push/version/tag/release/deploy or other external publication/signing/platform actions; do not perform them on your own.

**Full design, chronology, all post-merge checkpoints, reference map, and precise source/testing gates:** [canonical AW handoff](../../../aw/.agents/docs/AW-DOTLINK-TOOLS-FULL-COMPREHENSIVE-HANDOFF-2026-11-17.md).
