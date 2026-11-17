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
