# Decision log

This records decisions that should not be casually reversed without understanding the rationale.

## Product name

**Decision:** human title is **abird dotlink**; executable/crate/MCP identity is `dotlink`.

Rationale:

- keeps Abird brand;
- aligns with ChatGPT dot concept;
- broader than a tunnel-only product.

Historical names are not current API names.

## One binary, no UI

**Decision:** keep dotlink as one lightweight binary with guided setup and no separate GUI.

Rationale:

- easy install/run;
- local security boundary remains inspectable;
- transport/auth/sandbox fit naturally in one process.

## Pi-style tool names

**Decision:**

```text
ls
read
write
edit
bash / powershell
```

Binary separate:

```text
read_binary
write_binary
patch_binary
```

Rationale:

- model-friendly;
- no internal-prefix noise;
- text and binary semantics stay clear.

## Dynamic tool discovery

**Decision:** hide unavailable tools instead of always advertising and rejecting.

Rationale:

- tools/list should reflect effective authority;
- less model confusion;
- smaller attack/decision surface.

## No persistent cwd authority

**Decision:** launch directory is internal relative-path base and default read grant; all actual authority uses allow/deny roots.

Rationale:

- one filesystem permission vocabulary;
- fewer special cases;
- config/CLI merge predictably.

## Default read-only launch directory

**Decision:** `default_allow=true` grants read launch directory by default.

`--no-default-allow` removes it for one run.

Rationale:

- useful immediately;
- conservative mutation posture.

## Bare allow-write means rw launch directory

**Decision:** bare `--allow-write` is ergonomic rw on launch directory; explicit path remains write-only.

Rationale:

- common project-edit workflow is one flag;
- write-only destinations still possible explicitly.

## Deny wins

**Decision:** denies override all allows.

Final review also checks denies against lexical request path and canonical target.

Rationale:

- explicit negative rule must remain authoritative even with symlink aliasing.

## Parent traversal rejected

**Decision:** client `..` path traversal is rejected.

Rationale:

- removes ambiguous lexical policy behavior;
- caller can provide canonical path instead.

## Operation gate

**Decision:** pure reads may share; mutators/shell are exclusive.

Rationale:

- prevents dotlink-originated concurrent symlink/path-topology races without OS-specific handle-walking complexity;
- future tools default safe/exclusive.

## Special files rejected for writes/edits

**Decision:** write existing target must be regular file; edit/patch regular only.

Rationale:

- no FIFO/socket/device blocking/surprises;
- clear file-tool contract.

## Bubblewrap by default on Linux

**Decision:** shell requires Bubblewrap unless user explicitly selects full-host unsandboxed mode.

Rationale:

- Rust path checks alone cannot constrain arbitrary shell.

## Full host authority is paired

**Decision:**

```text
--allow-all --no-sandbox
```

Each requires the other.

Rationale:

- loud explicit boundary crossing;
- no proliferation of "dangerous" variants.

## Shell network off by default

**Decision:** sandbox network disabled unless `--allow-network`.

Rationale:

- package tools wanting internet must not silently expand authority.

## Nix runtime RO, not whole home

**Decision:** mount needed Nix runtime/profile paths RO.

Rationale:

- NixOS tools work;
- home secrets remain hidden.

## Nix daemon hidden when network denied

**Decision:** mask daemon endpoint namespaces, not guessed socket leaves.

Rationale:

- daemon can bypass intended boundary;
- filesystem type/layout may vary.

## Developer caches are shell-only

**Decision:** cache grants do not expand MCP filesystem roots.

Rationale:

- performance convenience is not general file authority.

## Cache RW explicit

**Decision:** default none; user picks RO/RW per discovered cache.

Rationale:

- RW lets sandbox code mutate host cache and must be acknowledged.

## JSONC config

**Decision:** hand-editable JSONC with comments/trailing commas.

Rationale:

- structured;
- readable;
- easy profile editing.

## Strict config schema

**Decision:** schema v10 only; no semantic migration.

Rationale:

- security semantics must not be silently reinterpreted;
- setup can replace old profile only after explicit confirmation.

## Secrets outside JSONC

**Decision:**

- OpenAI Runtime key separate config-owned private file;
- OAuth owner hash/grants separate XDG state file.

Rationale:

- profile can be inspected/shared without dumping key material;
- different lifecycle/permissions.

## Abird XDG namespace

**Decision:**

```text
abird/dotlink
```

under relevant XDG roots.

Rationale:

- all Abird products can coexist under common namespace.

## Profile-first runtime semantics

**Decision:** a profile-enabled transport starts automatically.

CLI `--stdio`/`--http` add for one run; `--no-*` suppress.

Rationale:

- profile should represent a complete runnable configuration.

## Setup zero transport means cancel

**Decision:** setup `none`/cancel does not save.

Rationale:

- avoids first-run broken profile and confusing immediate runtime failure.

Direct profile management may intentionally disable all transports.

## OpenAI optional

**Decision:** local-only/HTTP-only setup skips OpenAI credentials entirely.

Rationale:

- no unnecessary provider coupling.

## OpenAI manual tunnel path

**Decision:** support existing tunnel ID without Admin key.

Rationale:

- least privilege;
- user can manage tunnel in OpenAI Platform.

Automated one-time Admin-key creation remains optional.

## OpenAI full-runtime recovery

**Decision:** after 10 consecutive transient tunnel poll failures, rebuild whole runtime.

Rationale:

- avoids indefinitely retrying poisoned/stale transport/runtime state;
- reconstruction is simpler/safer than partial repair.

## stdio stdout purity

**Decision:** stdout is MCP protocol only.

Rationale:

- required by stdio framing/clients.

All human output → stderr.

## Runtime controls

**Decision:** Ctrl-C exit, Ctrl-R full restart, v verbosity cycle for interactive non-stdio runs.

Rationale:

- useful local operational control;
- no protocol stdin interference.

## Quiet logging by default

**Decision:**

```text
default quiet
-v TOOL
-vv TOOL + REQ
-q suppress TOOL
```

Rationale:

- clean normal experience;
- observability opt-in;
- developer diagnostics separate from tool activity.

## Logging privacy

**Decision:** summarize metadata/byte counts, not raw content/payloads/secrets.

Rationale:

- debug mode must not become data exfiltration.

## ngrok is HTTP enhancer

**Decision:** not a fourth MCP transport category.

Rationale:

- protocol is still Streamable HTTP;
- simpler model.

## Separate local/ngrok backend listeners

**Decision:** public ngrok uses distinct loopback backend.

Rationale:

- path/auth/resource policies can differ cleanly;
- avoids route leakage between local/public surfaces.

## Ephemeral paths are not authentication

**Decision:** document as obscurity/high-entropy routing only.

Rationale:

- avoid false security claims.

## Embedded OAuth only for single-owner self-hosted model

**Decision:** implement local AS/resource server rather than external IdP.

Rationale:

- no users/tenants/signup/federation;
- substantially smaller scope;
- best one-binary UX.

If multi-user hosted product emerges, switch to mature IdP rather than growing this into a general IdP.

## OAuth authenticates entry, not local capabilities

**Decision:** OAuth scope stays small.

```text
mcp:access
offline_access
```

Filesystem/shell authority remains LocalMachine policy.

Rationale:

- avoid duplicated/conflicting authorization systems.

## Public ingress safe by default

**Decision:**

- ngrok OAuth automatically;
- direct non-loopback HTTP requires OAuth + explicit HTTPS public origin;
- only one-run `--allow-public-no-auth` bypasses.

Rationale:

- externally reachable machine-control endpoint should not accidentally be open;
- dangerous state should not persist silently.

## No Host-header issuer inference

**Decision:** canonical external origin must be explicit for reverse proxy/non-loopback OAuth.

Rationale:

- prevent issuer/resource spoofing and proxy-header trust ambiguity.

## CIMD + DCR

**Decision:** support both.

Rationale:

- current remote MCP ecosystem uses both;
- CIMD preferred modern path;
- DCR required for compatibility.

## DCR pending state memory-only until consent

**Decision:** public registration does not immediately write durable client state.

Rationale:

- prevent unauthenticated registry/disk abuse;
- owner consent is durable-approval boundary.

## PKCE S256 mandatory

**Decision:** no plain PKCE/no PKCE.

Rationale:

- current OAuth/MCP security expectation;
- public clients have no secret.

## Opaque tokens, not JWT

**Decision:** random opaque access/refresh tokens.

Rationale:

- AS and resource server same process;
- no signing-key machinery;
- straightforward revocation/state binding.

## Refresh token only when supported

**Decision:** issue refresh only if client advertises `refresh_token`; `offline_access` requires it.

Rationale:

- respect client metadata and least privilege.

## OAuth durable state cross-process locked

**Decision:** entire read/modify/write transaction protected by OS file lock.

Rationale:

- atomic rename alone cannot prevent lost concurrent updates.

## No global password lockout

**Decision:** Argon2 in bounded blocking workers + delay rather than global failure counter/lockout.

Rationale:

- avoid remote DoS that locks legitimate owner out;
- protect Tokio worker pool.

## Stable ngrok domain supported

**Decision:** optional reserved hostname is first-class config/CLI.

Rationale:

- OAuth issuer/resource should stay stable for durable remote connector.

## Nix/Crane owns release graph

**Decision:** CI calls Nix; CI does not own compilation logic.

Rationale:

- reproducible local/CI parity;
- maintenance-light.

## One generic Linux release per architecture

**Decision:** static musl Linux artifacts; no Debian/NixOS aliases.

Rationale:

- same binary;
- fewer confusing duplicated assets.

## Cross-build all releases from x86_64 Linux

**Decision:** Linux x64/ARM64, Windows x64/ARM64, macOS ARM64 all reproducible from one host.

Rationale:

- local reproducibility;
- thin CI;
- one target graph.

## macOS ARM64 minimal SDK/LLVM cross path

**Decision:** pinned SDK + clang/ld64.lld rather than full Darwin xcbuild bootstrap.

Rationale:

- full Nixpkgs Darwin bootstrap failed from Linux;
- minimal toolchain is sufficient for dotlink and reproducible.

## Release assets individual, not bundle

**Decision:** GitHub Release gets each binary/checksum separately.

Rationale:

- direct installer/download URLs;
- no extra unzip;
- obvious platform assets.

Actions bundle remains internal.

## Installers are updaters

**Decision:** same install one-liner performs future update.

Rationale:

- simple UX;
- no separate update command required.

No-op when hash already current.

## Installer identity check

**Decision:** checksum plus `dotlink --version`; explicit requested version must match.

Rationale:

- detect wrong/mislabeled mirror asset even when matching sidecar accompanies it.

## Current release version

Current implementation:

```text
0.6.0
```

Do not tag/publish as a different version without updating Cargo/Nix/release metadata consistently.
