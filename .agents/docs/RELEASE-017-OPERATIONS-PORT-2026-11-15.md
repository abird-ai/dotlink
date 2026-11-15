# dotlink 0.17.1 operational convergence — Phase 1–3 checkpoint

Date: 2026-11-15 · L5 · `main` · **implementation/qualification checkpoint; not a release**

## Authority and safety

- Base main HEAD: `5c610279dfd266d33712ad5732e53ca489147ca3`.
- Release reference: `v0.17.1` = `07ca25feaece21857e02aa44462ce7fa7dde6af9`; `v0.16.8` also reviewed.
- Working directory: `/home/pvl/spaces/abird/src/dotlink`, not the read-only reference worktree `../dotlink-016-fixes`.
- The branch has not been rebased, reset, committed, pushed, version-bumped, tagged or deployed.
- User-unrelated dirty `docs/CHATGPT_PLUGIN.md` predates this work; **do not stage, edit, or overwrite it**.
- Preserve the exact three AW Git dependency pins at `0a6c6a0bea07a3886c4dc8be4f423eec3ea3c134`.
- All AW tools, filesystem, shared Graph/JobManager semantics, native replay/ACK and authenticated AW Remote Start remain the canonical implementation. No legacy native tool engine was imported.

## Phase 1 — complete: configuration and NixOS

Ported the final 0.17.1 *operational* behavior using a common-base three-way merge, preserving main's more recent durability/private-state changes:

- v10 → v11 → v12 in-memory profile migrations (starting at v9), strict decode after migration, path-less cache discovery, cache-mode precedence and stable credential generations.
- `DOTLINK_CONFIG_FILE` canonical env/explicit-path selection; legacy `DOTLINK_CONFIG` alias; reject ambiguous explicit/profiler sibling selection.
- CLI > `DOTLINK_*` environment > saved profile > defaults where applicable; runtime-only overrides do not mutate profiles.
- NixOS single declarative `settings`, private runtime-directory staging of active config, independent deployment `extraGroups`/`extraPackages`, systemd safety assertions and profile/ingress collision checks.
- Corresponding config, OAuth, credential-rotation, child environment and service tests.
- Docs updated for v12 and the current config/NixOS behavior.

## Phase 2 — complete: CLI and transport behavior

- `--no-tunnel` suppresses a saved OpenAI tunnel *before credential selection*; `--no-serialize` reverses a configured serialization preference. `DOTLINK_PROFILE` and `DOTLINK_VERBOSITY` are supported.
- The hosted shell synchronous hold is capped at 90 seconds or the remaining window minus the delivery reserve, whichever is shorter. The Process execution timeout and same-Job replay identity are unchanged.
- `jobs_get(wait_ms)` and `jobs_read(wait_ms)` use the **authenticated transport's remaining window** to narrow the already bounded wait; untrusted `_meta` does not widen it. No Job schema or AW design change.
- One reusable bounded MCP progress sink now covers every non-shell `tools/call`; shell uses the same helper with its command/response deadline, rather than double-reporting. No progress token means no progress traffic. Per-notification delivery is bounded and cannot delay Job cleanup.
- Progress logs include delivered/dropped tick counts with aligned continuations; the OpenAI tunnel reports successful notification delivery at REQ verbosity.
- Existing MCP tool names, `background:true`, AW-native Graph and remote Start are retained. No `job_*` compatibility reintroduction.

## Phase 3 — grouped qualification and operational compatibility

Passes at the current working tree:

- `cargo fmt --all -- --check`
- `cargo check --offline --locked --all-targets --all-features`
- `cargo clippy --offline --locked --all-targets --all-features -- -D warnings`
- `AW_BASH="$(command -v bash)" cargo test --offline --locked --all-features --bin dotlink -- --test-threads=4`: **664 passed, 0 failed, 1 intentional ignored**. First attempt without `AW_BASH` had two environment-only Graph-shell fixture failures; both pass with the required explicit path.
- `AW_BASH="$(command -v bash)" cargo test --offline --locked --all-features --tests -- --test-threads=4`: **all 5 integration binaries passed**, including private config generations and protocol-clean stdio.
- Focused non-shell progress end-to-end test: background Process + `jobs_get` long-poll receives progress before result, with no separate shell progress stream; passed. Shell progress test passed. Authenticated wait-window/replay metadata tests passed.
- `alejandra --check nix/module.nix nix/tests/module.nix`; `git diff --check`: pass.
- **NixOS module evaluation/build passed** using the installed Nixpkgs channel and a locally substituted package (`pkgs.hello`) in `nix/module.nix`'s expected `self.packages.x86_64-linux.default` slot, evaluating the real `nix/tests/module.nix` assertions and generated service contents. Full standard flake check still cannot fetch the AW Git dependency in this environment, and is **not** claimed green.
- Prior 2026-11-14 real OAuth/TCP AW→dotlink Process/FS/Graph bridge test evidence is retained for unchanged original Start/Job owners. New source changes are confined to local configuration, progress, and bounded response waits; current all-feature core and integration suites are green. Native Windows/macOS deployment gates are separate and not claimed.

## Fresh review remediation — 2026-11-15

All seven findings from a new read-only review were resolved in the operational
port. No AW tool/Job/Graph semantic changes:

- Effective profile: resolve `DOTLINK_PROFILE` once at process entry (CLI wins);
  malformed UTF-8 fails closed. Config/OAuth/Job bridge all consume the resolved
  `Args.profile`. Add profile to the Job-session intent fingerprint so changing
  private identity cannot reuse a differently scoped live JobManager. Tests cover
  environment-only selection, invalid selector with/without CLI override and
  distinct Job-session identities under identical execution policy.
- `--no-tunnel`: prevent dormant/incomplete `DOTLINK_ID` and
  `DOTLINK_API_KEY_FILE` overrides from being parsed or opened after the
  caller explicitly disables OpenAI. Preserve validation for other active
  options. Subprocess regression checks the original failure case.
- v12 strictness: `OAuthConfig` now rejects unknown nested fields.
  Regression includes `oauth.enabld`, which previously silently defaulted
  `enabled=false`.
- `--list-tools`: apply the same permission environment override helper used
  by real startup, without credential loading, cache discovery or transport
  initialization. Regression proves `DOTLINK_ALLOW_SHELL=1` is advertised.
- NixOS ingress: distinct fixed TCP ports are required for declarative HTTP
  services, conservatively covering wildcard, IPv6 and IPv4-mapped collisions.
  Matching ports normalize leading zeros; port 0 uses OS-selected ephemeral
  binding and is exempt. External saved/configFile binds are runtime-validated.
  Assertions test duplicate, wildcard, alias, disjoint and ephemeral cases.
- Color/verbosity: explicit CLI `--color=auto|always|never` is distinct from
  omission and overrides `DOTLINK_COLOR`. Verbosity has a single parsing
  owner at process entry (using `LogVerbosity::parse`); CLI `-v/-q` wins.
  Invalid nonselected environment values cannot disrupt an explicit CLI choice,
  while selected malformed values still fail. Blank verbosity environment
  permits falling back to the saved profile.
- NixOS staging safety (additional issue caught while fixing ingress):
  `ExecStartPre` no longer has a systemd ignore-failure prefix. Failure to
  stage the selected private config aborts startup instead of permitting a
  previously staged config. Module test proves the generated command fails
  closed and the docs describe this boundary.

Post-remediation validation on L5 working tree:

- `cargo check --offline --locked --all-targets --all-features` **pass**.
- `cargo clippy --offline --locked --all-targets --all-features -- -D warnings`
  **pass**.
- `cargo fmt --all -- --check` **pass**.
- `AW_BASH="$(command -v bash)" cargo test --offline --locked --all-features
  --tests -- --test-threads=4`: **664 unit tests pass, 1 ignored; all 32
  integration tests pass in five integration binaries**.
- Direct local NixOS module `nix build --impure --no-link` with installed
  Nixpkgs and a dummy dotlink package (the same real module assertion/test
  source) **pass**. Exact original full flake/AW Git source gate remains
  unqualified due to external fetch in this environment.
- `alejandra --check nix/module.nix nix/tests/module.nix`, `git diff --check`
  **pass**.
- Native Windows/macOS deployment validation remains external; no
  tag/version/release/deploy.

## Second fresh review remediation — 2026-11-15

- **NixOS profile namespace:** reject raw `environment.DOTLINK_PROFILE`
  in any enabled service, including inherited environment, so the typed
  `profile` remains authoritative for systemd and duplicate-slot checks.
- **Matching validators:** profile/instance identifiers contain 1–64 ASCII
  letters, digits, `-` or `_`, with a case-insensitive `default` reservation.
  NixOS permits only the exact lowercase `profile = "default"` alias for the
  unnamed profile. Negative and boundary-positive NixOS tests added.
- **Disabled OpenAI:** `--no-tunnel` is applied before OpenAI-only environment
  connection, URL or organization settings; existing profiles without an
  enabled OpenAI transport also skip those fields. Active invalid URLs still
  fail. Credential selection and saved profile contents are not rewritten.
- **CLI precedence:** one `RuntimeOverrides` value crosses the setup boundary.
  Either `--stdio`/`--no-stdio` or
  `--serialize`/`--no-serialize` bypasses its own environment variable
  before parsing; malformed active and unrelated settings still fail closed.
- No legacy `job_*`, AW tool schema or Graph implementation was imported.

- **2026-11-15 L5 verification**:
  - `cargo check --offline --locked --all-targets --all-features` — pass.
  - `cargo clippy --offline --locked --all-targets --all-features -- -D warnings` — pass.
  - `AW_BASH="$(command -v bash)" cargo test --offline --locked --all-features --tests -- --test-threads=4` — 664 unit tests passed, 1 ignored; **34 integration tests passed** across five integration binaries.
  - Local NixOS module `nix build --impure --no-link` using installed Nixpkgs and dummy package in `self.packages.x86_64-linux.default` — pass, including raw identity bypass, valid boundary/alias, rejected names and existing ingress/staging coverage. New profile cases were grouped into three NixOS evaluations to limit test-side resource pressure.
  - `cargo fmt --all -- --check`, `alejandra --check nix/module.nix nix/tests/module.nix`, `git diff --check` — pass.
  - Full flake build with the actual AW Git source, native Windows/macOS and deployment are **not** qualified.
- This checkpoint remains uncommitted; unrelated `docs/CHATGPT_PLUGIN.md` modifications, AW Git pins and the release worktree stay untouched.

## Deferred intentionally — AW tool design last

Do **not** import release-line JobManager/Graph/filesystem implementations or old `job_*` aliases. The release branch's tail-oriented Job reading, one-call ACK/discard, status/receipt shapes, grep limits, schema-description work, payload defaults and return timestamps affect the shared AW tool contract. Reconcile those **only after** the operational port is settled, with a dedicated AW design review and shared-first implementation. The 35-minute `ts`-anchored turn guidance depends on that deferred output/clock contract and was not copied into the current tool schema.

## Next checkpoint / external gates

- Do not call this a released or deployed v0.17.1 replacement: `Cargo.toml` intentionally remains at main's existing `0.14.3` version.
- For release readiness, rerun the real flake/NixOS build with its exact AW source available and qualify native platforms when capable; authorize version, tag, deployment and push separately.
- Use this checkpoint for Phase 4 planning only when asked. No remaining known Phase 1–3 source work item; review newly observed regressions before touching the completed runtime.
