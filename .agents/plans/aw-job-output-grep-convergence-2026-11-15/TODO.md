# Dotlink shared Job output + grep convergence

Base dotlink: `5c610279dfd266d33712ad5732e53ca489147ca3`
AW initial pin: `0a6c6a0bea07a3886c4dc8be4f423eec3ea3c134`
Dependency: [AW plan](../../../../aw/.agents/plans/ai-native-tool-output-grep-2026-11-15/README.md).

## Committed

None.

## Uncommitted

- [x] D0. AW A–G shared checkpoint qualified; signed implementation pin `be6d48db552fd44b9dd85364cd8734454109ce68` is published and all three dotlink AW dependencies/lock entries are repinned to it.
- [ ] D1. Dotlink MCP Job get/read/ack adapters use AW native tail/range/finish APIs; no second owner.
- [ ] D2. Shared grep file/dir and 0..128 context input/schema parity across direct and Graph.
- [~] D3. Compact output/limit and transport response-window/replay/Task-owner integration. Independent standalone settle-only `jobs_get` wait qualified on current pin; full shared output/replay/Task adoption remains open.
- [ ] D4. Grouped qualification, independent security review for changed replay/ACK, write handoff and checkpoint.

## Reviewed AW repin checkpoint — 2026-11-17 (uncommitted dotlink tree)

- AW signed implementation commit `be6d48db552fd44b9dd85364cd8734454109ce68` was pushed to GitHub as the direct parent of remote `main` tip `403eb8473498454a97871702ff37aaedb1de9ee1`; remote availability was verified before changing dotlink. The HTTPS push path on L5 has a local CA-chain problem, so publication used SSH only after the observed GitHub ED25519 host key matched GitHub's published fingerprint. TLS verification was not disabled.
- **Exact repin:** `aw-tools`, `aw-tools-fs-unix`, and `aw-tools-fs-win` in `Cargo.toml` plus all three `Cargo.lock` source records now use exactly `be6d48d`. The lockfile was reconstructed from the pre-repin lock and changed only those three source URLs; opportunistic unrelated `windows-sys` resolver churn was explicitly discarded.
- **Required compatibility fixes:** dotlink's `SchemaCaps` projection now supplies the canonical grep context contract (`default=0`, `max=128`). `JobCompletion::from_shared_graph` now treats `Snapshot::journal_unknown` as uncertainty even when the local terminal summary is otherwise known, preventing false Completed/Cancelled publication. The existing direct `SharedSnapshot` test fixture was updated and now covers a known-summary + journal-unknown case.
- **Qualification on the reviewed pin:** locked/offline `cargo check` passes; shared-native focused tests **17/17**; Job/MCP Job focused tests **116/116**; real stdio MCP integration **5/5**; full binary suite with explicit Bash fixture **676 passed / 1 ignored / 0 failed**. Strict `cargo clippy --offline --locked --all-targets --jobs 1 -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` pass. The first full-suite attempt had only two environment fixture failures (`Graph shell tests require bash`); rerun with `AW_BASH=$(command -v bash)` was fully green.
- **Boundary:** this closes D0 and establishes the dependency foundation only. It does not claim D1 shared owner/tail/range/Finish adoption, D2 grep execution/schema qualification, D3 transport/Task/replay convergence, or D4 security/release completion. No dotlink commit, push, version bump, release or separate-worktree change was performed.

## AW dependency checkpoint — 2026-11-16

- AW shared Job/output/grep and configurable toolset code is committed on AW main at `0d2b4e3` (AW HEAD `7d09d5f`). The subsequent event-driven `jobs_get`/byte-aware `jobs_read` implementation was live-qualified on AW but remains uncommitted.
- **D0 remains blocked** on one reviewed, committed shared pin that includes the wait implementation. Dotlink continues pinning all three AW dependencies at `0a6c6a0` without local path patches, mixed dependency sources, version bumps or speculative repins. D1/D2 remain the post-pin shared API adoption gates.

## Pre-pin standalone Job wait checkpoint — 2026-11-16 (uncommitted)

- **Independent host-side alignment:** `JobManager::standalone_get` now observes exact Process/Graph Job or Graph-node settlement, **not** `rev` changes, stdout arrival or sibling progress. Optional `rev` remains informational. A fixed deadline and an already-terminal target return the latest authoritative status. No execution-timeout change, new Job manager, background polling or per-request queue. This is a dotlink-local projection fix compatible with the existing pinned AW baseline.
- **Lost-wakeup protection:** reuse the existing `JobEntry.notify`, register `notified()` before the final settled-state recheck and use the same state/typed shared-node owner to evaluate terminality. Graph-node reads use `shared_jobs.node_state` under the existing Job entry lock; Job root uses the existing terminal status. The original `JobHandle::with_shared_transition` already notifies on committed state changes. Invalid nodes reject before waiting. Deadline responses refresh status; no output data is consumed or ACKed.
- **Live tests:** Process stdout arrives before settlement; deliberately stale `rev` no longer causes an early return; the Job stays observable until deadline/settlement. Two-node Graph: node 1 settles before sibling 2, node-level `jobs_get` returns independently while root remains running; root later settles. Invalid node refs deny. All **8** MCP Job adapter tests passed; real stdio `batch → jobs_get → jobs_read → jobs_ack` integration passed; `cargo check --offline --locked`, strict `cargo clippy --offline --locked --all-targets -- -D warnings`, and `cargo fmt --check` passed.
- **Dependency gate unchanged:** dotlink still consumes AW `0a6c6a0bea07a3886c4dc8be4f423eec3ea3c134`. The newer AW shared wait/output checkpoint is verified but **uncommitted**; D0/D1/D2/D4 remain open and D3 is only partially qualified. Do not repin, copy AW's JobManager, prematurely change MCP `jobs_ack` to Finish, or claim Task/replay delivery and transport-window work finished. Existing machine authority, consumer leases, and replay behavior are untouched.
- No commit, push, release, version bump, dependency repin or worktree change. Concurrent Job-surface source/test edits were preserved; this checkpoint belongs to the current uncommitted tree, not a new Git SHA.

## Standalone MCP settlement-wait checkpoint — 2026-11-16 (uncommitted)

- **Scoped pre-pin D3 fix:** Dotlink's existing `JobManager::standalone_get` now treats `rev` only as output status metadata; `wait_ms>0` waits for this exact Job or Graph node to **settle or reach its deadline**. It does not return on initial/stale `rev`, launched-state bumps, stdout arrivals, or unrelated node progress. `wait_ms=0` remains an immediate status read; a settled target returns immediately.
- **No lost notifications:** existing `Arc<Notify>` is registered/enabled *before* reading authoritative state under the current manager. A waiter awakened by any change rechecks actual root/node settlement; timeout returns a fresh authoritative snapshot. No 10 ms polling loop, alternative scheduler, output ownership engine or unbounded event queue. The existing authenticated transport response-budget clamp remains independent of Process execution timeout.
- **Live qualification:** a real background Bash Process prints early output then exits; an intentionally stale `rev` does not cause a premature `jobs_get` return, a short deadline returns running, a longer wait settles. A two-node shell Graph proves node 1 settles while sibling 2 and root still run; root waits for full completion. Unknown node rejects. Existing remote Start replay/revision tests remain green.
- **Executed green:** dotlink binary units **666 passed / 1 ignored**; real stdio MCP integration **4 passed**; focused MCP Jobs tests **8 passed**, immediate-completion wait **1 passed**; `cargo check --offline --locked`, strict `cargo clippy --offline --locked --all-targets -- -D warnings`, `cargo fmt --all -- --check`, scoped `git diff --check` all pass.
- **Not adopted:** default/explicit combined Process stdout+stderr `jobs_get.tail`, signed `jobs_read.offset`, optional-offset exact caller Finish, shared consumer leases, AW factory descriptor projection, grep parity, Task/replay owner migration and transport-wide output ACK proof remain D1–D4. This is one correctness checkpoint on the **existing pinned standalone JobManager**, not full shared dependency convergence.
- Dotlink was clean at pre-edit HEAD `6723e3e` (one unrelated local commit ahead of `origin/main`); retain it and all existing release/runtime work. No commit, push, tag, release or AW repin.

## Contiguous MCP delivery-proof checkpoint — 2026-11-16 (uncommitted)

- **Bug reproduced and fixed:** dotlink's existing `record_standalone_delivery` previously used `max(next_offset)` regardless of the returned raw page's `offset`. A successful out-of-order `jobs_read` could therefore authorize `jobs_ack` for a prefix never included in any response. The new regression failed on the old implementation and passed after correction.
- **Narrow existing fence, no new owner:** parse exact `offset` and `next_offset` from the *already-bounded successful* MCP output projection. Advance only if the page begins at/before the previously contiguous end. A later page cannot bridge an unread gap; a zero-offset ACK requires evidence from a projected first page. Reject malformed `next_offset < offset`. Keep the existing positive-offset over-ACK diagnostic and per-job/node/stream scope. The existing dotlink host ledger is temporary and must be retired in favor of canonical AW owner proof at the reviewed dependency repin; **do not** add a second registry.
- **Qualified end-to-end:** unit fixture with real shared retained output reads bytes 8–16 first, rejects ACK through 16 (and an unobserved zero ACK), rejects another stream's ACK, reads 0–8 and accepts ACK 8, rejects an attempted skip to 16, then rereads 8–16 and accepts ACK 16. A real stdio MCP `batch → jobs_read(offset>0) → jobs_ack` similarly denies the gap, then allows ACK after reading from zero. Existing Graph-root synthesis and fenced node-ACK regression remains green; no Task/replay release widening.
- **Executed green:** dotlink binary tests **667 passed / 1 ignored**, real stdio MCP tests **5 passed**, strict `cargo clippy --offline --locked --all-targets --jobs 1 -- -D warnings`, `cargo check --offline --locked`, `cargo fmt --all -- --check` and scoped `git diff --check`.
- **Still blocked:** D0 reviewed committed AW wait pin; D1/D2 shared owner, signed-offset/tail/Finish and grep adoption; D3 independent consumer/Task/replay leases and transport receipt proof; D4 independent review and release gates. This change is a pre-pin **correctness repair**, not full shared convergence.
- No commit, push, release, version bump, dependency pin change, new worktree or modification of other owners' files.

## Model ACK vs native MCP Task retention — 2026-11-16 (uncommitted)

- **Qualified existing shared-owner boundary:** a native MCP Task retains its own independent `shared_task_output_owner` even after model-facing `output_node_page → acknowledge_node_output` credits and ACKs the Job owner. A complete model ACK compacts the shared spool to the Task's exact configured result prefix, rather than freeing the Task's bytes or widening its grant. Duplicate model ACK is idempotent; `detailed_task` still returns that exact prefix.
- **Serialized delivery proof:** failed bounded-response preflight must not create any ACK credit. A large text stream containing JSON-escaped characters is projected into a 1024-byte MCP response; only its represented raw prefix can be acknowledged, not the original fetched buffer. A disjoint base64 page does not bridge an unseen interval, and a subsequent contiguous page advances the prefix. Explicit text/base64 response budgets are validated against the whole serialized MCP result.
- **Executed:** newly added live shared-owner and bounded-serialization tests both pass; full `cargo test --offline --locked --bin dotlink` **669 passed, 1 ignored**; real `--test stdio_tools` **5 passed**; low-parallelism strict `cargo clippy --offline --locked --all-targets --jobs 1 -- -D warnings`, `cargo check --offline --locked`, `cargo fmt --all -- --check`, and scoped `git diff --check` pass. One grouped build was externally interrupted during Clippy; re-running only the unaffected lint/build gates passed without Rust errors.
- **What remains:** this validates current native Task vs Job lease retention on the **existing AW pin** only; it is not transport delivery confirmation, authenticated independent MCP consumer leases, the post-pin shared `record_delivered_output` migration, whole-Graph Finish, or hosted replay/Task security sign-off. D0/D1/D2/D4 remain open; D3 remains partial. No commit, push, version change or AW repin.

## Node-targeted finite output waits — 2026-11-16 (uncommitted)

- **Real Process byte/EOF wait qualified:** `output_node_page_wait` returns on retained stdout bytes at the exact absolute offset, gives an empty nonterminal page at a short no-new-bytes deadline, returns the next chunk, then returns an empty terminal page at writer EOF. Reading never advances Job ACK watermark or releases output. The execution timeout remains independent from the wait window.
- **Graph-node correctness fix:** `standalone_output_ready` now checks the requested **Graph node's** terminal state for missing or unsealed output, not only Job-root completion. A skipped native node that attached no result stream exits the wait promptly even while an independent Bash sibling runs. `output_node_page` then reports `has no retained output after settlement` rather than the misleading `not available yet`; there is no fabricated output or ACK proof.
- **Executed:** two live tests (streaming Bash bytes/deadline/EOF; three-node Graph with failed Bash → skipped read and slow independent sibling) pass. Full dotlink binary suite **671 passed / 1 ignored**, real stdio MCP suite **5 passed**, strict `cargo clippy --offline --locked --all-targets --jobs 1 -- -D warnings`, `cargo check --offline --locked`, fmt and diff checks passed before the final settled-output diagnostic-only adjustment; both focused live tests were rerun and passed afterward. AW's matching finite read fix passes its **247-test** full adapter suite and strict Clippy.
- **Remaining:** D0 reviewed committed AW shared wait pin, D1/D2 shared grep and output/Finish migration, D3 independent transport/Task replay and delivery-lease qualification, D4 independent security review and release gates. This is a narrow correctness repair on the **existing pinned AW revision**; no standalone model schema changes, new ownership layer, commit, push or repin.

## AW shared-pin prerequisite review — 2026-11-16 (uncommitted)

- AW `main` remains `7d09d5f` with the event-driven shared Job watch and node-scoped `jobs_read` wait changes uncommitted. New live AW adapter tests cover a result stream attached after the reader subscribes and withdrawal of a controllable live read gate during Process output wait: no lost wake, no forbidden bytes, and no ACKable proof on denial.
- AW grouped gates green: **328** shared all-feature tests, **249** adapter all-feature tests, **110** read-only shared tests; non-async minimal, strict affected-crate Clippy, and MCP/minimal/full CLI builds. This is **source + test qualification**, not a reviewed signed Git pin or approval to change dotlink's dependencies.
- D0 remains **blocked** until the AW owner/coordinator authorizes and creates one signed, reviewed shared-runtime commit. Keep dotlink on `0a6c6a0`, without temporary Cargo path patches or a mixed-source pin. D1–D4 remain separate adoption/Task/replay/security gates.
- No dotlink source edit, commit, push, version change or repin in this prerequisite review.

## Complete bounded Job status and replay handoff — 2026-11-16 (uncommitted)

- **Bug fixed:** `jobs_get` previously sent an oversized successful status to the generic transport fitter, which could truncate away outcome/revision/continuation while still reporting success. The finite handler now preflights the **entire serialized MCP result** against the smaller of authenticated transport budget and profile output cap: either complete status or explicit error; no partial success.
- **Handoff never silently loses ref:** completed/pending/background `batch` fallback and compatibility replay use the same complete-status check, then a bounded `j:<id> next=jobs_get no-rerun` receipt when the full status cannot fit. This does not execute a second Job, change the original result or introduce another receipt engine. If even the minimal continuation cannot fit, return an explicit already-admitted error directing discovery via `jobs_list`, never a fake successful partial receipt.
- **Remote Start replay:** the canonical `RemoteStartReceipt::Mcp` remains immutable. An authenticated replay under a smaller envelope reprojects that cached success to the *same* exact Job ref if necessary; it does not rerun Graph preparation/effects or replace the replay ledger entry. A fitting replay returns the original receipt unchanged. Error receipts remain errors.
- **Tests:** exact-sized status accepted, oversized `jobs_get` status fails closed, compact handoff fits a forced small budget, too-small budget never returns false success; real large-result Graph falls back to exact handoff and replays without a second effect; remote Start duplicate retains the original cached receipt across a narrower replay projection. Full binary **673 passed, 1 ignored**, real stdio MCP **5 passed**, offline `cargo check`, strict all-target Clippy `-D warnings`, formatting and diff checks all green.
- **Scope:** read-only status and `batch` status/handoff responses; not a claim of atomic transport ACK delivery. Other Control/status receipts and canonical shared AW owner APIs remain separate D1–D4. The reviewed AW wait pin is still uncommitted (D0 blocked); no version, dependency pin, commit, push or worktree change.

## Bounded ACK/Cancel and remote Control replay — 2026-11-16 (uncommitted)

- **Control receipt safety:** `jobs_ack` and `jobs_cancel` now preflight their successful status against the smaller of profile and trusted transport result budgets. If it does not fit, emit an exact compact `j:<id>[/n:<node>] ack=through:<offset> next=jobs_get no-rerun` or `j:<id> cancel=accepted next=jobs_get no-rerun` receipt. If even that cannot fit, report that the effect **already applied** and requires inspection; never silently truncate a successful status or imply a retry is safe. This is dotlink presentation only; canonical Job/Task ownership and ACK effects are unchanged.
- **Remote Start idempotence:** the original `RemoteStartReceipt::Mcp` remains immutable. An exact duplicate under a smaller budget returns the original success if it fits or a compact projection of the **same exact action/Job identity**. Cached failures remain errors. A cached mismatched Job ref fails closed; no second ACK/cancel, stale-revision recheck, replay-ledger replacement or new policy grant.
- **Evidence:** focused synthetic full/compact/too-small ACK and Cancel receipts, fitting original, immutable narrower replay, error preservation and mismatched-ref denial passed; real `jobs_cancel` remote-Start replay passes after the first cancel advances the Job revision, including a narrower duplicate and a subsequent full original replay. Full dotlink binary **674 passed / 1 ignored**; real stdio MCP **5 passed**; strict `cargo clippy --offline --locked --all-targets --jobs 1 -- -D warnings`, offline check, formatting and diff checks green. A grouped run was externally terminated at Clippy; isolated low-parallelism rerun passed.
- **Not covered:** `jobs_send` response budgeting, transport confirmation of receipt, producer authority revocation/Control commit linearization, canonical shared AW owner adoption and the broader D0–D4 rollout. The AW shared wait pin remains uncommitted. No commit, push, tag, version or dependency pin change.

## Bounded live stdin and replay — 2026-11-16 (uncommitted)

- **Exact success projection:** `jobs_send` now bounds the *whole serialized MCP success* by the smaller of the current authenticated transport budget and the profile result ceiling. If `j:<id>[/n:<node>] rev=<n> ok` does not fit, return the shorter exact `j:<id>[/n:<node>] sent` confirmation; never let the outer transport formatter silently truncate an accepted input or invent a revision.
- **Pre-effect fail-closed:** before sending live stdin, verify the minimum complete success receipt fits. A fresh remote Start that cannot fit releases its claim **before effect**; a direct call similarly rejects without delivering bytes. An exact already-committed remote replay is checked first: under a smaller envelope it projects the original confirmed send or an explicit already-accepted `inspect with jobs_get, do not resend` error rather than submitting input again.
- **Replay semantics:** canonical `RemoteStartReceipt::Mcp` remains immutable; successful cached receipts are accepted only for the exact Job/node ref and a validated `rev=<u64> ok` or `sent` status. Cached failures remain failures, mismatched identities/malformed successes deny; a compact cached success never fabricates a missing revision. No second input message, new owner ledger, transport permission or AW repin.
- **Verified:** exact/compact/too-small source-byte-budget unit test and a real interactive Bash job: a too-small response budget leaves its stdin untouched; the admitted send delivers `hello` once and a settled Process rejects further writes. Existing remote Start duplicate-stdin regression confirms receipt replay without resending bytes. Full dotlink unit suite **676 passed, 1 ignored**, real stdio MCP **5 passed**, and strict all-target Clippy/offline check/fmt/diff completed green before an additional unexecuted duplicate remote fixture was temporarily tried and discarded. Its build attempts were externally terminated during test compilation with no Rust diagnostic; the smaller already-qualified test set is retained.
- **Still separate:** physical transport receipt confirmation, non-remote duplicate sends, atomic current source revocation under remote replay, complete MCP Task/consumer lease adoption and the reviewed signed AW wait pin (D0–D4). No commit, push, tag, version change or dependency repin.

## AW shared truncation-notification prerequisite — 2026-11-16 (uncommitted)

- AW's pending bounded per-Job watch now notifies a reader when `OutputWriter::discard_if_full` first marks a running Process stream truncated, **even without newly retained bytes or EOF**. Later discarded chunks retain exact totals without repeated wakeups. The direct shared regression failed before the fix and passed afterward.
- AW qualification: `aw-tools --all-features --lib` **329**, `aw-tools-adapter --all-features --lib` **249**, read-only **110** tests passed; non-async minimal build, strict affected-crate Clippy and MCP/minimal/full CLI builds green.
- Dotlink has **not** adopted this fix: D0 still requires a reviewed signed AW commit containing the complete shared wait contract. Keep existing dotlink AW dependency pins unchanged; qualify the real MCP streaming/truncation path after the repin. No dotlink source or version change.

## AW empty-output Finish prerequisite — 2026-11-17 (uncommitted)

- AW shared `finish_output_for_owner` now permits idempotent no-op Finish for a known settled Skip/output-free root or exact node without an explicit stream view. Named nonexistent views still fail; unknown, journal-ambiguous and unsettled Jobs still reject before the no-op. AW shared red/green test and live node-level `jobs_ack` pass.
- AW qualification: **330** shared tests, **249** adapter tests, **110** read-only tests passed; strict lint, minimal build, MCP and minimal/full CLI checks green. This refinement is **not** in the committed AW Git revision.
- Dotlink still pins AW `0a6c6a0` and must not claim the new Finish behavior until D0 reviewed signed AW commit and subsequent D1/D2 integration. No dotlink source/pin changes.

## AW post-release owner-proof prerequisite — 2026-11-17 (uncommitted)

- The AW shared JobManager now retains exact **owner and recorded offset validation even after physical output release**. Both `acknowledge_output` and `acknowledge_delivered_output` previously returned a released no-op for arbitrary owners or offsets after compaction; shared red/green tests now reject foreign owners and offsets greater than the same owner's prior ACK, while preserving valid replay and terminal Finish idempotence.
- Qualified on AW: **331 shared tests**, **249 adapter tests**, **110 read-only tests**, minimal build, strict Clippy and MCP/minimal/full CLI checks; consumer lease hash domain-separator cleanup preserves the same compiled NUL byte and passes its focused test.
- **Not present on dotlink's current AW pin.** D0 still awaits the reviewed signed AW shared-runtime commit. Continue to require dotlink's independent existing authenticated source/owner checks; do not treat cached/released Task results as a general replay permission. No dotlink source, dependency, commit or version change.

## AW append-notification prerequisite — 2026-11-17 (uncommitted)

- Pending AW `OutputWriter::append` now signals its existing bounded Job watch only for **newly retained bytes or first truncation**, not every later discarded source chunk. Shared test failed on repeated wakes before the fix and passes after, with exact source totals and no ACK credit. AW shared **332**, adapter **249**, read-only **110** tests; minimal/strict lint/MCP/CLI gates green.
- Dotlink is **not** repinned and has not adopted the shared watcher. D0 remains gated on reviewed signed AW commit; after repin validate real MCP direct-stream/high-volume quota behavior with the same single source of truth. No dotlink source changes or new output-owner engine.

## AW failed-output watcher prerequisite — 2026-11-17 (uncommitted)

- Pending shared AW `OutputWriter` now wakes readers on a failed `append`, the first failed-drain truncation, and final EOF, **not** on each later `observe_after_failure` source chunk. This preserves exact source-byte accounting, avoids a notification storm on a failed high-volume Process, and never creates ACK credit.
- Verified on AW with red/green shared drain regression and independent failed-append wake test; **334 shared**, **249 adapter** tests passed, minimal non-async, strict Clippy and MCP/minimal/full CLI host builds green. The read-only shared surface remains unchanged.
- This behavior is **not present in dotlink** until the reviewed signed AW shared revision becomes available. Keep the existing AW pin and separate Task/consumer/transport receipt obligations; no dotlink source/dep change.

## AW uncertain-transition notifier prerequisite — 2026-11-17 (uncommitted)

- Pending AW shared `JobManager` now publishes a bounded watch invalidation when a returning host journal transition no longer matches the in-flight proposal and fences the Job journal as unknown. The original error remains `JournalUnknown`; no effect or permit is inferred. Red/green shared test reproduces the missing notification.
- AW validation: **335 shared**, **249 adapter**, **110 read-only** tests; minimal non-async, strict Clippy and MCP/minimal/full CLI builds green. This does **not** change the finite get/read settle-or-deadline contract or independently qualify host recovery after journal ambiguity.
- Dotlink still pins the earlier committed AW shared runtime. D0 awaits reviewed signed AW commit; preserve existing dotlink replay/Task and retention authority until that pin is adopted and verified. No dotlink source or dependency change.

## AW journal-unknown status / wait prerequisite — 2026-11-17 (uncommitted)

- Pending AW shared `JobManager::Snapshot` now exposes its original journal-ambiguity flag; Job status/list and typed node receipts report `journal=unknown` without claiming the underlying Job is terminal or overwriting independently known node outcomes. Finite `jobs_get(wait_ms>0)` ends observation on journal fence, and unreadable `jobs_read` waits fail with a reconciliation conflict instead of misreporting EOF or waiting unnecessarily.
- Red/green shared uncertainty-status test, typed receipt regression, and live background Bash Process get/read conflict tests pass. AW validation: **336 shared**, **250 adapter**, **110 read-only** tests, minimal build, strict Clippy and MCP/minimal/full CLI builds green. Actual reconciliation/host durability and transport delivery remain independent unqualified gates.
- **D0 unchanged:** this is still uncommitted AW shared/adapter source, not present in dotlink's pinned AW revision `0a6c6a0`. Keep dotlink's existing Job/replay interpretation until a reviewed signed AW checkpoint is available and repinned with exact host transport tests. No dotlink source or dependency mutation.

## AW journal-fenced output retention prerequisite — 2026-11-17 (uncommitted)

- Shared AW `JobManager` now fences **through-ACK**, trusted-host output ACK, and trusted physical compaction while the host persistence transition is unresolved (`Busy`) or its journal result is unknown (`ReplayUnknown`). Previously these could release output retained for reconciliation even after journal uncertainty, bypassing the terminal Finish safeguard.
- All three operations use one existing manager → output lock scope for the journal preflight and effect. A red/green shared test proves a six-byte, already-projected retained stream and owner watermark remain intact during an in-flight Start persistence callback and after that callback returns `OutcomeUnknown`. Full AW qualification: **337 shared**, **250 adapter**, **110 read-only** tests, minimal build, strict Clippy, MCP/minimal/full CLI builds green.
- **D0 not complete:** the reviewed signed AW shared-runtime commit has not landed. Dotlink still pins `0a6c6a0`, so the new journal/retention guard is not yet in its binary. Do not repin, infer MCP transport receipt or bypass its independent Task/replay authority until post-pin integration qualification. No dotlink source or dependency change.

## AW terminal-journal retention prerequisite — 2026-11-17 (uncommitted)

- Pending AW shared `JobManager` now prevents **explicit terminal expiry and automatic capacity pruning** of `Lifecycle::Terminal` Jobs still fenced `journal_ambiguous()`. Previously such Jobs could disappear even with no retained stream bytes or replay-ledger entry; the clock/retention deadline and terminal status alone do not prove that recovery evidence is discardable.
- AW shared red/green test confirms: explicit expiry returns `Busy`, exposed-capacity pressure reclaims a separate **known safe** terminal instead, and exclusive uncertain-terminal pressure fails `Capacity` without deleting its history. Qualified **338 shared**, **250 adapter**, **110 read-only** tests plus minimal build, strict Clippy and MCP/minimal/full CLI checks.
- **D0 remains blocked:** dotlink still pins AW `0a6c6a0` and has not adopted this shared retention fence. Do not equate current dotlink Task/replay expiry semantics with the new AW behavior before a reviewed signed AW commit and post-pin transport qualification. No dotlink source/dep edits.

## Tests and verification

- [ ] Default `jobs_get` 512 aggregate (256/256 and unused redistribution), explicit 0/512/1024, running/settled/Graph node/root and current disclosure.
- [ ] Signed read + raw continuation, prefix vs terminal Finish, replay/Task lease, unknown/revoke, no premature cleanup.
- [ ] End-to-end grep direct-file/directory + Graph, context 0/128/129, output/incomplete/pagination.
- [ ] All-feature Rust targeted integration/strict Clippy, formatting and affected release gates.
- [ ] Preservation of existing operational dirty tree, unrelated plugin doc and release reference; no unauthorized push.
