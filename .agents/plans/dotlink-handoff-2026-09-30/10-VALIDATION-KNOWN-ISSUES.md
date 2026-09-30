# Validation history and known issues

Date: 2026-09-30

## Rust validation

The final implementation was validated with the intended Rust 1.98.1 toolchain:

```bash
cargo fmt --check
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
git diff --check
```

Final observed suite:

```text
88 tests passed
Clippy clean
fmt clean
diff check clean
current binary rebuilt successfully
```

## Bubblewrap validation

The opt-in real runtime smokes passed with `DOTLINK_TEST_BWRAP=1`, including:

- project RW when granted;
- host home hidden;
- Nix profile/store tooling visible where intended;
- network denied by default;
- Nix daemon unavailable when denied;
- runtime credentials hidden;
- deny-read masking;
- deny-write RO downgrade;
- typed cache RO/RW mounts;
- cache-specific environment mapping.

## Runtime key-control validation

A pseudo-terminal smoke verified: aligned banner/log output while raw key input is active, banner key hint, three-step live `v` verbosity cycle with log notices, full `Ctrl+R` transport/runtime restart with a second banner, preserved verbosity state, and clean `Ctrl+C` exit. `stty -g` before/after was byte-for-byte identical. A separate forced HTTP bind failure after runtime controls started also restored the exact pre-run terminal state before returning exit 1. stdio intentionally does not enable runtime key capture.

## Logging validation

Verified behavior:

```text
default             no TOOL / REQ
-v                  TOOL
-vv                 TOOL + REQ
--silent -vv         REQ only
```

Also verified:

- timestamps;
- forced ANSI color;
- HTTP method/path/status request logging;
- stdio stdout remains protocol-only;
- verbose summaries avoid raw bulk payload/file-content logging.

## Cache validation

Verified:

- typed cache config serialization;
- onboarding discovery for supported cache families;
- none/RO/RW choices;
- candidates must already exist;
- discovery avoids creation-prone Yarn/pnpm probes;
- cache grants do not expand MCP filesystem roots;
- deny-read removes matching cache grants;
- deny-write downgrades RW cache grants to RO;
- real Bubblewrap cache reuse.

## Transport validation

Verified historically and during the implementation phase:

- OpenAI transport integration path;
- unreachable local OpenAI test endpoint reaches 10 consecutive transient poll failures and emits the typed full-runtime restart signal;
- restart marker survives multi-transport anyhow context wrapping;
- peer transport teardown is cancellation-first with a 5-second force-abort bound;
- repeated runtime restart backoff is capped at 30 seconds and resets after a successfully connected runtime;
- stdio initialize;
- HTTP initialize;
- profile-only HTTP starts without repeating `--http`;
- profile-only stdio starts without repeating `--stdio`;
- setup cancel/none writes no profile and exits successfully;
- re-setup preserves existing defaults and masks retained Runtime API keys;
- profile manager create/list/show/edit/delete plus allow/deny add/remove and bool enable/disable workflows;
- direct profile disable commands may intentionally leave no configured transport; such a profile can start with one-run `--stdio` / `--http`;
- `--no-stdio` / `--no-http` suppress persisted local transports and `--no-ngrok` suppresses persisted ngrok;
- clean stdio EOF does not kill active peer transports;
- explicit runtime `--stdio` / `--http` override persisted false;
- stable and ephemeral HTTP routing;
- independent local/ngrok ephemeral path policy;
- ngrok missing-token path is clear;
- logging does not corrupt stdio MCP traffic.

The final live ChatGPT/OpenAI process still needs a restart/connection refresh to guarantee the client is using the final committed binary/schema.

## JSONC/profile validation

Tests cover:

- line comments;
- block comments;
- trailing commas;
- comment markers inside strings;
- BOM;
- unterminated block-comment rejection;
- runtime key omitted from serialization;
- named profile paths;
- schema-v9 default_allow + persistent allow/deny arrays;
- shell/network defaults;
- - deny-shell/deny-network precedence.

## Dependency advisory review

`cargo-audit 0.22.2` scanned the locked dependency graph against the current RustSec advisory database:

- no known vulnerabilities were found;
- two transitive maintenance warnings remain:
  - `generational-arena 0.2.9` via `ngrok 0.19.0 -> awaitdrop`;
  - `rustls-pemfile 2.2.0` via `ngrok 0.19.0`.
- `ngrok 0.19.0` is the current published ngrok SDK, so there is no supported newer ngrok release to upgrade to for those warnings at this time.

These are unmaintained-crate warnings, not RustSec vulnerability findings.

## Nix/Crane validation — complete

The release build was validated without exposing the host Nix daemon by using a writable isolated Nix store under ignored `target/`.

Validated:

- `flake.lock` contains Crane v0.24.0 and rust-overlay.
- `nix flake show` evaluates.
- `nix flake check --all-systems --no-build` passes for supported systems:
  - x86_64-linux;
  - aarch64-linux;
  - aarch64-darwin.
- full x86_64-linux `nix flake check` builds/passes package, tests, Clippy and fmt.
- `.#deps` builds as a separate Crane dependency artifact.
- `.#cross-linux-x86_64-deps`, `.#cross-linux-x86_64`, and `.#dist-linux-x86_64` build.
- `.#cross-windows-x86_64-deps`, `.#cross-windows-x86_64`, and `.#dist-windows-x86_64` build.
- the same cross/dist outputs were rebuilt from the actual Git-backed committed source after implementation commits.

### Linux release artifact

Verified:

- ELF64 x86_64;
- no program interpreter;
- `ldd` reports `statically linked`;
- SHA-256 sidecar verifies;
- binary runs locally and prints `dotlink 0.5.0`.

### Windows release artifact

Verified:

- PE32+ / `pei-x86-64`;
- x86_64 Windows CUI subsystem;
- SHA-256 sidecar verifies;
- imports only Windows system DLLs;
- no libgcc/libstdc++/libwinpthread/libssp runtime sidecar imports;
- runs under Wine and prints `dotlink 0.5.0`.

## Release/install validation — complete

`scripts/build-release-artifacts.sh` was run end-to-end against the validated isolated Nix build context and produced exactly:

```text
dotlink-linux-x86_64
dotlink-linux-x86_64.sha256
dotlink-windows-x86_64.exe
dotlink-windows-x86_64.exe.sha256
```

Both checksums verify.

`install.sh`:

- executable mode verified;
- `sh -n` and ShellCheck pass;
- canonical default URLs resolve to `abird-ai/dotlink`;
- repository/base-URL overrides remain functional;
- missing `HOME` fails safely unless an install directory is explicit;
- malformed checksum sidecars are rejected before installation;
- ran against the real Linux release fixture;
- SHA-256 verification passed using explicit hash comparison;
- install replacement is staged in the destination directory and renamed atomically;
- installed binary is byte-identical and runs.

`install.ps1`:

- parsed/executed with PowerShell 7.6.6;
- canonical default URLs resolve to `abird-ai/dotlink`;
- tested with `Invoke-WebRequest` mocked to the real Windows release fixture;
- checksum format and value verification passed;
- output `dotlink.exe` hash matches the release fixture.

`scripts/build-release-artifacts.sh` also passes ShellCheck and now enables `nix-command` + `flakes` explicitly, so it does not depend on those experimental features being globally configured.

A native Windows host test is still useful future CI coverage, but there is no known packaging failure after PE inspection + Wine execution + PowerShell installer logic validation.

## Commit structure

Implementation was split into coherent compilable commits:

```text
b76f8e6 Expand profiles, sandbox caches, and logging
7b93e92 Add Crane cross builds and release installers
```

Documentation/handoff follows those commits.

## Current known limitations

### Running connector freshness

The ChatGPT connector process can outlive a binary rebuild. After changing tool/schema/sandbox metadata, restart `dotlink` and refresh the ChatGPT developer connection before using live tool behavior as final evidence.

### Repository upstream

Canonical upstream / `origin` is `https://github.com/abird-ai/dotlink`. Release installers default to `abird-ai/dotlink` while retaining explicit repository/base-URL overrides.

### Public HTTP authentication

dotlink HTTP/ngrok currently has no built-in application-layer caller authentication.

Ephemeral paths are not authentication.

### Platform sandboxing

Bubblewrap is Linux-only.

Windows/macOS filesystem MCP tools still use Rust allow/deny policy, but native shell execution cannot claim Linux Bubblewrap isolation. Windows users are advised to use WSL2 + Bubblewrap for the stronger shell boundary.

### Intel macOS Nix

Current nixpkgs unstable dropped `x86_64-darwin`; the flake intentionally does not advertise it. Intel macOS is documented as Cargo-from-source until a separate supported nixpkgs path/release artifact is introduced.
