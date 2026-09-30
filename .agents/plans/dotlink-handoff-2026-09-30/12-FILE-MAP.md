# File map

## Core Rust

### `src/main.rs`

Owns:

- clap CLI;
- profile selection;
- effective launch-directory/base policy merge and one-run transport overrides;
- transport activation;
- cache-grant runtime assembly;
- logging CLI configuration;
- startup banner;
- process lifecycle;
- many CLI/policy regression tests.

### `src/setup.rs`

Owns:

- strict schema-v9 JSONC parser/serializer;
- named profile paths;
- runtime key persistence;
- interactive onboarding;
- OpenAI tunnel setup/creation;
- schema-v9 default_allow + persistent path allow/deny arrays;
- persistent shell/network defaults;
- developer cache discovery;
- typed cache grants;
- strict schema-v9 config validation.

### `src/mcp.rs`

Owns:

- filesystem access policy;
- path canonicalization;
- tool argument/result behavior;
- policy-aware tool router;
- text/binary tools;
- Bash/PowerShell execution;
- Bubblewrap command construction;
- NixOS runtime mounts;
- Nix daemon masking;
- shell cache mounts;
- child env sanitization;
- central optional TOOL activity logging;
- security/runtime regression tests.

### `src/controls.rs`

Owns:

- interactive runtime key handling;
- raw terminal mode lifecycle;
- `v` live verbosity cycling;
- `Ctrl+R` restart and `Ctrl+C` exit events;
- stdio-safe disabling of key capture.

### `src/logging.rs`

Owns:

- `ColorMode`;
- `LogConfig`;
- timestamp formatting;
- TOOL and REQ rendering;
- TTY-aware ANSI;
- safe truncation.

## Transport layer

### `src/transports/mod.rs`

Owns concurrent transport lifecycle, cancellation and error propagation.

### `src/transports/openai.rs`

Owns OpenAI Secure MCP Tunnel protocol/client integration and OpenAI request-level verbose logging.

### `src/transports/stdio.rs`

Owns rmcp stdio server.

Important:

- stdout protocol-only;
- stderr human logs;
- verbose request method observer.

### `src/transports/http.rs`

Owns:

- Streamable HTTP MCP;
- ngrok SDK forwarding;
- local/ngrok endpoint separation;
- independent ephemeral URL paths;
- HTTP request logging middleware.

## User documentation

### `README.md`

Primary user-facing source.

Includes:

- quick sell;
- quick start;
- ChatGPT setup;
- Claude.ai/ngrok;
- local MCP;
- technical architecture;
- profiles/JSONC;
- permissions;
- cache discovery;
- logging;
- build/release/install.

### `SECURITY.md`

Security guarantees/tradeoffs.

### `ARCHITECTURE.md`

Internal architectural narrative/design.

### `CHANGELOG.md`

Large `Unreleased` section describes uncommitted phase.

### `docs/CHATGPT_PLUGIN.md`

ChatGPT developer tunnel connection workflow.

## Agent-facing repository docs

### `.agents/AGENT.md`

First entrypoint for agents: invariants, reading order, and validation rules.

### `.agents/docs/configuration.md`

Schema-v9 config/setup/cache reference.

### `.agents/docs/release-install.md`

Installer, release artifact, Nix/Crane, and platform reference.

### `.agents/skills/README.md`

Repository-local skill conventions.

## Build/release

### `Cargo.toml`

Package/dependencies, currently v0.5.0.

### `Cargo.lock`

Lockfile; modified in current tree.

### `rust-toolchain.toml`

Rust 1.98.1 minimal + Clippy/rustfmt.

### `flake.nix`

Current Crane-oriented build/cross-build design.

### `flake.lock`

Refreshed on 2026-09-30 to lock Crane v0.24.0 and rust-overlay; full native/cross build execution remains pending.

### `install.sh`

Unix installer.

### `install.ps1`

Windows installer.

### `scripts/build-release-artifacts.sh`

Cross-release helper.

## Handoff bundle

Directory:

```text
.agents/plans/dotlink-handoff-2026-09-30/
```

Canonical starting file:

```text
00-START-HERE.md
```

Comprehensive single-file handoff:

```text
HANDOFF.md
```

Ready-to-paste continuation prompt:

```text
01-CONTINUATION-PROMPT.md
```

Do not copy only a random subset of the handoff when delegating the project; use `00-START-HERE.md` or the continuation prompt.
