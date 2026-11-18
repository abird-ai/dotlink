# Pre-0.17 main convergence

Owner: @pvl
Canonical shared design: `../../../../aw/.agents/plans/pre-017-shared-convergence-2026-11-18/README.md`
Shared audit: `../../../../aw/.agents/plans/pre-017-shared-convergence-2026-11-18/AUDIT.md`
Reference release range: `84cd9b7..3317429` (55 commits, before v0.17.0 release commit)

## A. Preconditions

- [x] Freeze the cutoff immediately before the v0.17.0 release commit.
- [x] Account for `88d7b71`, `75ff9fe`, `3317429` in this wave; keep the
      v0.17.0 release commit and later mount/stop/release work deferred.
- [x] Current published dotlink baseline is `5381729`.
- [x] Revalidate main, exact origin/main and release reference before
      implementation. Dotlink started from `5381729 == origin/main`; only this
      plan was dirty.
- [x] Wait for one reviewed signed/published AW shared implementation pin:
      AW `792f0b38d104659264b70c31736655df1a46b60f` is published on `main`.
- [x] Never use sibling/path dependencies for the repin.

## B. Shared pin adoption

- [x] Move `aw-tools`, `aw-tools-fs-unix`, `aw-tools-fs-win` and every lock
      occurrence together to the exact new AW commit.
- [x] Confirm shared ReceiptProfile categories/scope and JobProfile read-page
      defaults/maxima are the new shared contracts consumed here.
- [x] Run locked compile after the shared API adaptation and before receipt
      boundary wiring.

## C. Dotlink receipt/result metadata

- [x] Select timestamp scope `Process | Batch | Jobs`.
- [x] Leave Filesystem/Agent timestamps disabled initially.
- [x] Set `checkpoint_minutes = Some(30)`.
- [x] Append shared model instructions exactly once.
- [x] Supply host UTC Unix seconds at first result/observation finalization; no
      shared clock. Immutable result/Task replay reuses its original timestamp,
      while fresh Jobs observations receive a fresh timestamp.
- [x] Attach receipt metadata in
      `CallToolResult._meta["io.abird/receipt"]`.
- [x] Add a compact model-visible receipt block where necessary without mutating
      raw/structured/media payloads.
- [x] Ensure compact typed receipts do not duplicate the timestamp block.
- [x] Timestamp `jobs_read` pages through the same selected profile.
- [x] Exact cached/native-Task replays retain original effect/result identity
      **and** original result timestamp; distinct executions may differ.
- [x] Tests cover Process, Batch, every Jobs operation, excluded filesystem,
      raw text, structured JSON, typed image/audio/resource, error results,
      replay, and result-budget boundaries.

## D. Dotlink page/output defaults

- [x] Build `DOTLINK_JOB_PROFILE` with read default
      `min(256 KiB,max_output_bytes)` and read max `max_output_bytes`.
- [x] Omitted `jobs_read.limit` uses that default; explicit limit clamps/rejects
      according to the shared profile.
- [x] Schema advertises exact default/max from the same profile.
- [x] Preserve negative-offset snapshot, byte/EOF wait and ACK delivery proof.
- [x] Restore omitted synchronous shell `max_output_bytes` to
      `min(256 KiB,max_output_bytes)`.
- [x] Explicit shell output size remains caller-selected under the configured
      ceiling.
- [x] Spool retention, Task result budget and execution timeout remain
      independent.

## E. Pre-v0.17 equivalence matrix

- [x] `6a13963`: hosted synchronous response wait leaves execution timeout
      independent.
- [x] `960185d`: platform shell schema identity.
- [x] `c5d9d15` / `094059e`: verbosity env + Nix.
- [x] `4f78a26` / `7dc862d` / `7c5734f`: migration, strict config and env
      precedence.
- [x] `be03aa7`: `--no-serialize`.
- [x] `d798158` / `b351381`: exact-file grep + context 0..128.
- [x] `fe02e12` / `23f5b44` / `ea0b9e4`: Nix settings, staging,
      ambiguity failure.
- [x] `032af01`: config-file override + deprecated alias.
- [x] `e251f12` / `80b12b2` / `fc03329`: current shared event-driven Job
      waits satisfy the old bounded-wait intent.
- [x] `16173df` / `4e0b36b`: exact Task/result replay identity is independent
      of response timestamp.
- [x] `75c11e8` / `2684777`: runtime and advertised limits come from the same
      immutable profiles.
- [x] `9ff6e3a` / `693b090` / `9f5b221`: generic bounded progress,
      including blocked/waiting tools.
- [x] `ba49c5a` / `4d60e27` / `1d68d25` / `a2706fd` / `81e1c05` /
      `a9e04d1`: current shared aggregate tail + signed offsets provide the
      old end-reading intent without legacy APIs.
- [x] `c24a40f` / `b93b14e`: one durable background execution and one failure
      shape.
- [x] `88d7b71`: 30-minute ReceiptProfile guidance replaces the historical
      35-minute value.
- [x] `75ff9fe` / `3317429`: current `background:true` and one durable
      execution path satisfy the pre-tag v0.17 preparation intent.
- [x] Release-only commits are recorded as historical only and cause no source
      version change.

## F. Qualification

- [x] `cargo check --offline --locked --all-targets --all-features`.
- [x] Strict all-target/all-feature Clippy `-D warnings`.
- [x] Full dotlink binary suite: **688 passed / 1 intentional fixture ignored**.
- [x] All integration suites, including real stdio: **37/37**.
- [x] Receipt metadata end-to-end over stdio and hosted-result projection:
      explicit real-binary stdio and OpenAI EmbeddedMcp wire assertions verify
      `_meta["io.abird/receipt"].ts` plus the matching visible `ts=` block.
      Exact remote-Start/native-Task replay and transport fitting remain green.
- [x] Current D1-D4 + B6 focused regressions.
- [x] Nix/Alejandra gates. `flake check --no-build --offline` passed with a
      temporary GitHub HTTPS-to-SSH rewrite because this host's HTTPS CA chain
      is broken; no user/repo Git config was changed.
- [x] Native Bubblewrap/security/hardware gates covered by the complete current
      test matrix where available on L5.
- [x] `cargo fmt --all -- --check` and `git diff --check`.
- [x] Record intentional supersessions rather than claiming byte-identical
      release parity.

## Implementation checkpoint — 2026-11-18

- [x] AW shared pin published and adopted:
      `792f0b38d104659264b70c31736655df1a46b60f`.
- [x] Process/Batch/Jobs receipt timestamps use shared scoped ReceiptProfile;
      filesystem remains excluded and shared 30-minute guidance is appended
      exactly once.
- [x] Remote-Start result caches are timestamped before `mark_ready`; compact
      replay reprojections preserve/charge cached `_meta` instead of re-stamping.
- [x] Native MCP Task terminal result timestamps are assigned on first
      successful result materialization and reused exactly on replay.
- [x] Transport result fitting preserves `_meta` and charges its actual
      serialized overhead before truncating text.
- [x] `jobs_read` and synchronous shell omitted limits are 256 KiB by default
      under their configured profile ceilings; schemas advertise the same facts.
- [x] Explicit stdio + hosted wire assertions for `io.abird/receipt` and the
      visible `ts=` block pass.
- [x] **Pre-v0.17 convergence wave complete.** No v0.17.x implementation was
      included in this checkpoint.

## G. Deferred

- [ ] v0.17.2 generic mounts/config v13.
- [ ] post-v0.17.3 signal/stop provenance.
- [ ] any v0.17.x release/version/tag/deploy action.

Do not begin G until A-F are complete.
