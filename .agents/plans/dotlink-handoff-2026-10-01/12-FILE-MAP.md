# File map

This maps current files to responsibilities so the next agent can navigate quickly.

## Root

### Cargo.toml

Owns:

- crate identity/version;
- Rust dependencies;
- dev dependencies.

Current key additions for 0.6:

- argon2;
- getrandom;
- sha2;
- tower dev support.

### Cargo.lock

Pinned Cargo dependency graph.

Do not casually regenerate with unrelated updates.

When adding a dependency:

- update lock intentionally;
- review unrelated drift;
- run RustSec.

### rust-toolchain.toml

Pins Rust 1.98.1 toolchain.

### flake.nix

Authoritative Nix build/release graph.

Owns:

- Rust toolchain overlay/Crane;
- native package/checks/dev shell;
- cross targets;
- static Linux;
- Windows x64/ARM64;
- macOS ARM64;
- dist derivations;
- `release-all`;
- package metadata.

Target/build logic should stay here, not in CI YAML.

### flake.lock

Pinned Nix inputs.

### README.md

Public product story and user guide.

Owns:

- marketing intro;
- install;
- quick start;
- ChatGPT setup;
- Claude.ai setup;
- local MCP usage;
- permissions;
- HTTP/ngrok/OAuth overview;
- CLI summary;
- FAQ;
- build/release overview.

Do not turn it into an internal implementation dump.

### SECURITY.md

Public/current security boundary.

Owns:

- permission precedence;
- sandbox;
- public ingress;
- OAuth security properties;
- secret storage;
- logging privacy;
- resource limits;
- threat-boundary caveats.

### ARCHITECTURE.md

Current technical architecture.

Owns:

- module/layer relationships;
- transports;
- policy model;
- runtime restart;
- OAuth router composition;
- build/release architecture.

### CHANGELOG.md

Current user/maintainer-visible change history.

Unreleased currently describes 0.6.0 + review hardening.

### LICENSE

MIT.

### .gitignore

Rust/Nix/release/env/editor artifacts.

Keep source/handoff files visible.

## Installers

### install.sh

Unix installer/updater.

Responsibilities:

- OS/arch detection;
- latest/version URL;
- download asset/checksum;
- SHA verification;
- binary version identity;
- idempotent no-op;
- same-dir temp replacement;
- install as `dotlink`.

### install.ps1

Windows installer/updater.

Responsibilities mirror Unix path:

- x64/ARM64 selection;
- SHA verification;
- version identity;
- no-op;
- same-dir temp replacement;
- install as `dotlink.exe`.

## Scripts

### scripts/build-release-artifacts.sh

Thin materializer for:

```bash
nix build .#release-all
```

Do not add target-specific compilation logic here.

## GitHub

### .github/workflows/build-binaries.yml

Thin Nix workflow.

Build job:

- checkout;
- install Nix;
- build release-all;
- validate;
- upload internal artifact.

Release job:

- tag only;
- write permission;
- verify tag/version;
- create/update Release;
- upload individual `dotlink-*` assets.

## src/main.rs

Top-level CLI/runtime.

Key ownership:

### Args

All normal CLI flags.

### Command / OAuthCommand / ProfileCommand

Persistent management subcommands.

### Policy

Merges profile + CLI into effective local authority.

### main()

- logging init;
- command dispatch;
- top-level restart loop.

### run_runtime()

- setup/load profile;
- effective transport selection;
- HTTP auth selection;
- LocalMachine creation;
- transport startup;
- runtime controls;
- lifecycle/teardown.

### resolve_local_transports()

Profile + CLI transport precedence.

### resolve_http_auth()

Public ingress OAuth/no-auth safety rules.

### print_banner()

Human-facing runtime summary.

If adding a top-level flag, check:

- setup conflicts;
- profile-subcommand conflicts;
- runtime activation detection;
- help/docs/tests.

## src/setup.rs

Config/setup/profile state.

### TransportConfig

Persisted:

- openai;
- stdio;
- http;
- bind;
- local ephemeral;
- ngrok;
- ngrok domain;
- ngrok ephemeral.

### OAuthConfig

Defined in oauth module but embedded in AppConfig.

### PermissionConfig

Persisted path/shell/network policy.

### CacheKind / CacheMode / CacheGrant

Developer cache model.

### AppConfig

Strict schema v10.

### load_or_setup()

Normal config load / first-run setup.

### interactive_setup()

Wizard.

### setup_openai()

Tunnel/key onboarding.

### cache discovery

Known tool cache detection.

### save_config / atomic private writes

Transactional private persistence.

### JSONC parser helpers

Comments/trailing commas.

### profile management

List/show/create/edit/delete/rules/toggles.

### config path helpers

Abird XDG config namespace.

### protected_paths_for_config()

Builds self-protected control-plane paths passed into LocalMachine.

### validate_config_structure()

Schema/security invariants.

When changing schema:

- bump version if semantics incompatible;
- update config docs;
- update setup strict replacement behavior/tests;
- do not silently migrate security meaning.

## src/oauth.rs

Embedded OAuth implementation.

Large module, behaviorally well-tested.

Sections:

### Config/state types

- OAuthConfig;
- RegisteredClient;
- RefreshGrant;
- PersistentState;
- AccessGrant;
- AuthorizationCode;
- PendingAuthorization.

### Runtime

- durable state load;
- in-memory tokens/pending state;
- bounded Argon2 verification;
- DCR pending/promotion;
- token issuance/rotation/revoke;
- durable state reload/update.

### Server

- issuer/resource;
- router;
- bearer middleware;
- client metadata resolution.

### state path / owner setup

- XDG state;
- profile names;
- password hash transaction;
- management commands.

### OAuth handlers

- metadata;
- authorize;
- token;
- register;
- revoke.

### consent/response helpers

- hardened HTML;
- redirects;
- JSON no-store.

### CIMD

- URL validation;
- DNS/public-IP checks;
- reqwest pinned addresses;
- metadata validation.

### cryptographic helpers

- random token;
- SHA-256 token hash;
- PKCE.

### persistent state lock

- lock-path;
- private lock file;
- cross-process locked update_state;
- read/write state.

### tests

Extensive protocol/security regression coverage.

Future modularization is reasonable but should be structural-only first.

## src/mcp.rs

LocalMachine and local authority.

### AccessSpec / AccessPolicy

Filesystem roots and policy.

### MachineConfig / RuntimeConfig

Runtime machine state.

### LocalMachine

MCP server implementation.

### path resolution

- lexical normalization;
- deny checks;
- canonicalization;
- create ancestor handling.

### operation_gate

Shared pure reads, exclusive mutators/shell.

### filesystem tools

- ls;
- read;
- write;
- edit.

### binary tools

- read_binary;
- write_binary;
- patch_binary.

### shell

- execute_shell;
- direct shell;
- Bubblewrap.

### sandbox mount helpers

- runtime paths;
- masks;
- mount targets;
- caches.

### tool router

Dynamic route visibility.

### ServerHandler

Logging + operation gate + tool dispatch.

### tests

Path/symlink/policy/sandbox/runtime tests.

This is another candidate for future structural modularization.

## src/controls.rs

Terminal/runtime key controls.

Owns:

- Unix/Windows terminal state;
- raw input;
- Ctrl-C;
- Ctrl-R;
- v;
- stdio interrupt handling;
- RAII restoration.

Do not let stdio protocol stdin share the runtime key reader.

## src/logging.rs

Shared mutable log/verbosity state.

Owns:

- color mode;
- TOOL/REQ enablement;
- quiet override;
- live verbosity cycling;
- human activity formatting;
- truncation.

## src/transports/mod.rs

Transport supervisor.

Owns:

- RuntimeRestartRequested marker;
- ActiveTransports;
- peer task shutdown/drain;
- cancellation.

## src/transports/openai.rs

OpenAI Secure MCP Tunnel implementation.

Owns:

- embedded MCP dispatch;
- tunnel poll protocol;
- response posting;
- request deadline handling;
- header filtering;
- transient/fatal classification;
- 10-failure restart escalation;
- tunnel logs.

Security note:

Do not log API keys/raw sensitive payloads.

## src/transports/stdio.rs

stdio transport.

Owns:

- rmcp stdio service;
- request method diagnostics;
- bounded prefix capture.

stdout must remain protocol-only.

## src/transports/http.rs

HTTP/ngrok runtime.

Owns:

- local listener;
- ngrok backend listener;
- stable/ephemeral paths;
- OAuth Server wiring;
- bearer middleware composition;
- request logs;
- ngrok domain verification;
- public OAuth warning/status.

## docs/

### docs/CHATGPT_PLUGIN.md

Detailed ChatGPT connection instructions.

Keep current with ChatGPT plugin/MCP App wording and current URLs.

## .agents/AGENT.md

Short maintainer entry guide.

Should point at the newest handoff.

## .agents/docs/

### configuration.md

Concise current schema/CLI reference.

### release-install.md

Release/install details.

### README.md

Index.

## .agents/skills/

Reserved project-specific agent skill notes.

Currently minimal.

## .agents/plans/

Historical and current planning/handoff packages.

### dotlink-handoff-2026-09-30/

Historical design context.

Do not treat as current state.

### dotlink-handoff-2026-10-01/

Current canonical handoff.

### START_HERE.md

Stable pointer to newest handoff.

## target/

Generated build/test/cache scratch.

Ignored.

Do not record target paths as portable project state except in validation notes where explicitly labeled environment-specific.

## Ownership heuristic

If changing:

- CLI/setup/profile → `main.rs`, `setup.rs`;
- local authority/tools/sandbox → `mcp.rs`;
- remote HTTP auth → `oauth.rs`, `transports/http.rs`, `main.rs`;
- OpenAI tunnel → `transports/openai.rs`;
- terminal runtime UX → `controls.rs`, `logging.rs`, `main.rs`;
- build/release → `flake.nix`, scripts/workflow/installers;
- public security claims → `SECURITY.md`;
- public user UX → `README.md`;
- technical design → `ARCHITECTURE.md`.

After significant changes, update the matching current docs and handoff only if handing off again.
