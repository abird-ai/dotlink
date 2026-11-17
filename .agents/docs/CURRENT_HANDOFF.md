# CURRENT HANDOFF — dotlink unified Jobs + Agent runtime

Prepared: 2026-11-14
Owner: @pvl
Machine: **L5 implementation/source authority**
Repo: `/home/pvl/spaces/abird/src/dotlink`

> Canonical cross-repo design authority remains AW
> `890e9a383fe2edc3e0bd0aa34a2b217f2724475c`.
> Source/tests/executed evidence outrank prose.
>
> Later operational convergence with the 0.17.1 release branch is tracked in
> [RELEASE-017-OPERATIONS-PORT-2026-11-15.md](RELEASE-017-OPERATIONS-PORT-2026-11-15.md).
> It preserves this unified-runtime implementation and is not a release/tag.

## Final implementation state

The unified Jobs + Agent dotlink integration is complete.

Exact shared AW revision:

`0a6c6a0bea07a3886c4dc8be4f423eec3ea3c134`

All three shared dependencies pin that exact reachable Git SHA:

- `aw-tools`
- `aw-tools-fs-unix`
- `aw-tools-fs-win`

The temporary local remote-Start argument-digest implementation is gone.
dotlink now reuses `aw_tools::remote_start::arguments_sha256` directly.

Standalone model surface:

- shell constructor (`bash`, plus prepared PowerShell spelling where supported)
- `batch`
- `jobs_list`
- `jobs_get`
- `jobs_read`
- `jobs_ack`
- `jobs_send`
- `jobs_cancel`

Standalone refs remain `j:<handle>[/n:<coordinate>]`.

Intentionally absent:

- `job_start`
- `job_restart`
- old `job_*`
- Agent constructor/history/filter
- `jobs_subscribe`
- wake
- team/plan/review routes
- nested `batch`

dotlink remains the concrete finite Process/Graph executor. AW exclusively owns
Agent, Session, Policy, membership, transcript, hierarchy and upstream authority.

## Remote Start bridge

The authenticated AW -> dotlink bridge is implemented and qualified:

1. OAuth-authenticated MCP client identity is attached to HTTP requests.
2. The authenticated lease endpoint returns host/profile + live runtime generation.
3. AW supplies one immutable source-fenced `RemoteStartAuthorization`.
4. dotlink validates exact target generation, operation and canonical arguments.
5. The existing JobManager replay owner claims the Start **before mutable
   state/policy/topology checks and before effects**.
6. Only a genuinely new claim rechecks current dotlink launcher/sandbox/tool/resource authority.
7. Exact redelivery returns the original factual receipt or pending/unknown state.
8. Expired, changed-semantics and old-generation proofs fail closed.

Final replay-first review fixed and regression-locked:

- `jobs_cancel`: claim before current revision check;
- remote `batch`: claim before Graph preparation/topology revalidation;
- remote shell: claim before current shell-policy recheck.

`jobs_ack`, `jobs_send` and shared-native effects already had correct claim-first ordering.

## Final qualification

Against exact AW pin
`0a6c6a0bea07a3886c4dc8be4f423eec3ea3c134`:

- `cargo check --offline --locked --all-targets --all-features` — green
- `AW_BASH=... cargo test --offline --locked --all-features --quiet`
  - main: **651 passed / 0 failed / 1 intentional external-fixture ignore**
  - integrations: **1/1, 8/8, 1/1, 12/12, 4/4**
- `cargo clippy --offline --locked --all-targets --all-features -- -D warnings` — green
- `cargo fmt --all -- --check` — green
- `git diff --check` excluding unrelated `docs/CHATGPT_PLUGIN.md` — green

The final Unix standalone catalog remains 21 tools / 13,832 serialized MCP Tool
bytes (~3,458 rough tokens).

Checkpoint F's real OAuth/TCP Process and FS/Graph bridge evidence remains
current. Native Windows/macOS execution remains a separate platform/release gate
and is not claimed from L5.

## Working-tree safety

- `docs/CHATGPT_PLUGIN.md` is unrelated pre-existing dirty work and is not part of
  this implementation checkpoint.
- Separate worktree:
  `/home/pvl/spaces/abird/src/dotlink-016-fixes`, branch `release/0.16`.
  Do not touch it or create another worktree.
- Never use A1 for this project.
- x2 was used only as an inference host for the live Qwen benchmark; it is not
  source authority.

## What is next

No unified-runtime implementation item remains.

Future work should begin from live Git state and only reopen this surface for a
new scoped design/change. Release/tag/deploy/platform certification remain
separate actions and require their own authorization.

## Cross-repo shared-tools handoff update — 2026-11-17

The [current dotlink AW-tools continuation](DOTLINK-AW-TOOLS-NEXT-AGENT-HANDOFF-2026-11-17.md) points to the [canonical full AW–dotlink design and implementation history](../../../aw/.agents/docs/AW-DOTLINK-TOOLS-FULL-COMPREHENSIVE-HANDOFF-2026-11-17.md). It supersedes this dated summary for the *subsequent* Toolset, output/ACK, wait, replay and dependent AW pin D0–D4 work; the unified Jobs runtime baseline documented above remains historical authority for its own commit.

Latest published shared-tools adoption checkpoint: signed dotlink commit `d0a392234e918bd4ec9f0fbd7ab6fe771e1eb696` (`Adopt reviewed AW Job output runtime`) consumes reviewed AW implementation pin `be6d48db552fd44b9dd85364cd8734454109ce68`. D0 is complete; D1 shared owner/tail/range/Finish adoption, D2 grep end-to-end qualification, D3 transport/Task/replay convergence and D4 independent security/release gates remain open. Qualification at that checkpoint: full binary **676 passed / 1 ignored**, real stdio MCP **5/5**, strict Clippy, locked check, fmt and diff checks green.

The finalized shared-tools checkpoint consumes reviewed AW follow-up `beef1bc2d2f37b5660ebe928d368bca37368e608` in all three dependency/lock records. **D1–D4 are complete.** The new shared writer-quiescence invariant requires retained writers to finish before hidden retirement, automatic capacity reclamation or AW runtime replacement; dotlink's existing Process/Graph settlement order already satisfies that invariant, so the repin required no host source change. Fresh exact-pin qualification is **684 passed / 1 ignored** in the all-feature binary plus **36/36 integrations**, with locked all-target/all-feature check, strict Clippy, fmt and diff checks green. The prior D4 security qualification remains valid: guard revocation uses bounded discard cleanup; Markdown is **0 issues across 76 files**; all Nix checks, native full Bubblewrap and seven targeted GPU/KVM/device-security cases are green. **No D1–D4 correctness gate remains open.** Push/signing/notarization/version/tag/release/deploy/provider actions remain separate authorization/external gates.

## AW writer-quiescence repin checkpoint — 2026-11-18

- AW signed/published `beef1bc2d2f37b5660ebe928d368bca37368e608`;
  GitHub `main` and AW local `origin/main` were verified at that exact SHA.
- Dotlink repinned `aw-tools`, `aw-tools-fs-unix` and `aw-tools-fs-win`
  plus the three lock source records from `26a8c78` to `beef1bc`.
- Full current-tree Rust qualification after the repin:
  **684/0/1 + 36/36**, strict all-feature Clippy, fmt/diff and locked
  all-target/all-feature check green.
- No dotlink execution/owner/replay source change was required for the new AW
  invariant. The D1–D4 source/docs plus repin are one reviewed owned checkpoint;
  unrelated release-worktree state remains separate.

## Shared Toolset B6 metadata checkpoint — 2026-11-18

- Published dotlink `main` before this B6 checkpoint is
  `4f343c3183d427e036a6cb1a9cc42c8a4f2c2cb4`; the B6 checkpoint consumes
  published AW `7e6fe20a95640e32610819ce94b61a121e165dea`. Push remains a
  separate action.
- Canonical filesystem MCP projection now uses shared
  `Toolset::filesystem_descriptor`, including shared title, description and
  argument documentation. Finite Job MCP metadata uses
  `Toolset::describe_operation`.
- One `DOTLINK_JOB_PROFILE` now drives both runtime checks and shared metadata:
  Process tail 512/1024, get/read wait 30 s, list max 256. Grep remains shared
  0/128 context with default shared work budgets.
- Receipt timestamp/checkpoint guidance remains intentionally disabled via
  `ReceiptProfile::default()`; do not parse existing free-form MCP results to
  synthesize typed receipts.
- A provider-facing regression found `JobsList.limit` lacked shared
  `minimum=1`. AW published `7e6fe20` centralizes the invariant; dotlink is
  repinned and the temporary local lower-bound shim is removed.
- Shared descriptor metadata now also covers Bash/PowerShell/Batch after
  dotlink's dynamic schema-limit patching. Product descriptions and dynamic
  timeout/output bounds are preserved; inspection, `list_tools` and
  `get_tool` agree on canonical titles.
- Current dotlink B6 gates: binary **685/0/1**, integrations **36/36**, locked
  all-target/all-feature check, strict Clippy, fmt and diff green. No runtime
  authority, replay, output-owner, sandbox or transport behavior changed.
- B6 is complete for current product surfaces. Receipt timestamp/checkpoint
  metadata remains deliberately disabled pending a typed factual receipt path.
  Version/tag/release/deploy remain separate.
