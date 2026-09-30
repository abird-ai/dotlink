# abird-link comprehensive agent handoff

Date: 2026-09-30

This is the single-file comprehensive handoff for the current `abird-link` project. Supporting detailed specs live beside this file in `.agents/plans/abird-link-handoff-2026-09-30/`.

## 0. First instruction: preserve the working tree

**Do not reset, clean, checkout over, or discard the working tree.**

Last committed baseline:

```text
5b8bbdd Rename to abird-link and add profiles and deny policy
```

The current working tree contains a large newer development phase that is intentionally uncommitted. It is the project state to continue from, not junk.

Before editing:

```bash
git status --short --branch
git log --oneline --decorate -12
git diff --stat
git diff --check
```

Then read current source and docs.

## 1. What the product is

`abird-link` is a single Rust binary that connects ChatGPT, Claude.ai, local MCP clients, and other MCP-capable AI tools to local files and optional shell execution under an explicit local permission policy.

Primary README sell:

> Connect ChatGPT, Claude.ai, and other MCP-capable AI tools to your computer files and shell securely — from a single binary.

The system is not merely a tunnel client anymore. It is a local permission/security boundary plus multiple MCP transport adapters.

Canonical name:

```text
abird-link
```

Old historical name:

```text
abird-tunnel
```

Do not reintroduce the old name in product/API/config paths.

## 2. Core product principles

### Least privilege

Default posture:

```text
cwd read access          ON
cwd write access         OFF
extra paths              OFF
shell                    OFF
shell network            OFF
shared caches            OFF
public HTTP ingress      OFF
unsandboxed host mode    OFF
```

### Explicit capability model

Filesystem capabilities:

```text
read
write
read+write
deny-read
deny-write
deny-rw
```

Execution/network:

```text
allow-shell
deny-shell
allow-network
deny-network
```

Deny rules win wherever enforceable.

### Dynamic MCP surface

Unavailable capabilities should remove tools from `tools/list` rather than merely returning permission errors.

### Transport does not equal authority

OpenAI Tunnel, stdio, HTTP and ngrok all terminate at the same `LocalMachine` policy core.

No transport gets extra filesystem/shell authority implicitly.

## 3. Current package/repository state

Current package:

```text
name: abird-link
version: 0.5.0
edition: 2024
rust: 1.98.1
```

Current config schema in source:

```text
8
```

Branch:

```text
main
```

Canonical upstream / `origin`:

```text
https://github.com/abird-ai/abird-link
```

The large JSONC/profile, cache, logging, Bubblewrap/Nix hardening, Crane/release and documentation phases that originally motivated this handoff have been reviewed and committed. Always inspect live `git status` / `git log` for any newer work.

See `03-CURRENT-STATE.md` for exact paths.

## 4. MCP tool design

Text tools intentionally match Pi-style simplicity:

```text
read
write
edit
ls
bash / powershell
```

Binary is separate:

```text
read_binary
write_binary
patch_binary
```

Read-only baseline:

```text
ls
read
read_binary
```

Mutation tools appear only when effective write authority exists.

Shell appears only when effective shell authority exists.

### Binary semantics

`read_binary` supports:

```text
mcp
base64
hex
```

`mcp` maps images/audio/blob resources to typed MCP content.

`write_binary` supports base64/hex.

`patch_binary` supports byte offset/length replacement, insert and delete.

## 5. Filesystem policy

Core policy sets:

```text
read_roots
write_roots
deny_read_roots
deny_write_roots
```

Existing paths are canonicalized before access checks.

Create targets canonicalize the nearest existing ancestor.

Tool requirements:

```text
read/read_binary/ls    read
write/write_binary     write
edit/patch_binary      read + write
```

Write-only directories are valid destinations.

### CLI grants

```text
--allow-read[=<DIR>]
--allow-write[=<DIR>]
--allow-rw[=<DIR>]
```

Bare forms use cwd.

Special historical rule:

```text
bare --allow-write == rw cwd
```

Explicit `--allow-write=/path` remains write-only.

### CLI denies

```text
--deny-read[=<DIR>]
--deny-write[=<DIR>]
--deny-rw[=<DIR>]
--deny <PATH>   legacy deny-rw synonym
```

## 6. Shell and Linux sandbox

Unix shell tool:

```text
bash
```

Windows shell:

```text
powershell
```

Linux Bash is Bubblewrap-sandboxed by default.

Normal sandbox:

```text
/proc       fresh
/dev        minimal/Bubblewrap-managed
/tmp        private tmpfs
HOME        /tmp/home
network     off/unshared
PID/IPC/UTS isolated
```

Policy mounts:

```text
read-only grant  → RO
read+write grant → RW
write-only       → not exposed to shell
deny-read        → masked
deny-write       → rebound RO
```

### NixOS runtime

Mount read-only as present:

```text
/nix/store
/run/current-system
/etc/profiles
/nix/var/nix/profiles
~/.nix-profile
```

Do not mount whole home.

Canonicalize shell executable and PATH entries.

### Nix daemon security

When network is denied, hide daemon endpoint roots:

```text
/nix/var/nix/daemon-socket
/run/nix-daemon
```

Do not assume the leaf is always a Unix socket.

Generic masking:

```text
directory   → inaccessible tmpfs
file/socket → /dev/null bind
```

This design came from a real Bubblewrap failure where a presumed socket leaf was actually a directory.

## 7. Network/full-host model

Normal sandbox network is off.

```text
--allow-network
--deny-network
```

Profile `allow_network` requires shell.

Full unsandboxed authority is deliberately paired:

```bash
abird-link --allow-all --no-sandbox
```

`--allow-rw=/` by itself is filesystem-wide RW under the normal sandbox model, not equivalent to no sandbox.

## 8. Profiles and JSONC

Default:

```text
~/.config/abird-link/config.jsonc
~/.config/abird-link/runtime.key
```

Named:

```text
~/.config/abird-link/config.work.jsonc
~/.config/abird-link/runtime.work.key
```

Use:

```bash
abird-link -p work
abird-link -S -p work
```

`-s` is silent; setup shorthand is uppercase `-S`.

JSONC supports:

- line comments;
- block comments;
- trailing commas;
- BOM;
- comment-like text in strings.

Legacy `.json` is readable.

Runtime key is never serialized into config.

Current profile can persist:

- cwd;
- allow_read;
- allow_write;
- allow_rw;
- deny_read;
- deny_write;
- deny_rw;
- allow_shell;
- allow_network;
- typed caches;
- transport defaults.

Compatibility: older boolean `allow_rw: true` maps to `allow_rw: ["."]`.

## 9. Onboarding

### Transport step

Choices:

```text
openai
stdio
http
all
none
```

If OpenAI is not chosen, skip OpenAI configuration entirely.

### Local permission step

Current intended questions:

```text
Pin this profile to current project?
Allow rw cwd by default?
Allow shell by default?
Allow shell network by default?   only if shell=yes
```

### Developer cache autodiscovery

Linux sandbox only.

Supported families:

```text
Cargo registry/git
npm
pnpm
Yarn
pip
uv
Go modules/build cache
Maven
Gradle
sccache
ccache
```

Discovery rules:

- only known existing cache locations;
- env variables where appropriate;
- safe non-mutating local queries where needed;
- no whole-home scan;
- avoid probes that initialize/create caches;
- show exact discovered path;
- explicitly explain adjacent credentials/config are excluded.

Per-cache choice:

```text
none
read-only
read+write
```

## 10. Developer cache security model

Caches are **shell-only** grants.

They must not be added to MCP filesystem read/write roots.

Example:

```text
host ~/.cargo/registry
→ sandbox /tmp/home/.cargo/registry
```

RO:

- reuse existing packages;
- no cache population;
- protects host cache integrity.

RW:

- normal package manager behavior;
- cache persists/warm;
- sandboxed code can mutate shared host cache.

Never implicitly expose:

```text
~/.cargo/credentials.toml
~/.cargo/config.toml
~/.npmrc
```

Path denies override cache grants:

```text
deny-read  → remove matching cache mount
deny-write → downgrade matching RW cache to RO
```

## 11. Transports

### OpenAI Secure MCP Tunnel

Private ChatGPT developer path.

OpenAI setup:

- Runtime API key: Tunnels Read + Use;
- existing Tunnel ID or one-time Admin key;
- Workspace ID/Organization ID when creating tunnel;
- Admin key never saved;
- Runtime key per profile.

Automatic recovery:

- transient poll failures retry with backoff;
- after 10 consecutive transient failures, OpenAI requests a full runtime restart;
- all active transports are cancelled and given up to 5 seconds to stop before remaining tasks are force-aborted;
- profile/policy, `LocalMachine`, embedded MCP and transports are reconstructed;
- repeated unhealthy runtimes back off from 1 second up to 30 seconds;
- a runtime that had successfully connected resets the restart backoff;
- fatal tunnel/auth failures remain fatal;
- raw request/URL details are DEBUG-only under `-v`.

### stdio

Local subprocess MCP.

Primary users:

- Claude Desktop;
- Claude Code;
- similar local clients.

stdout must remain MCP protocol only.

### Streamable HTTP

Default:

```text
http://127.0.0.1:3000/mcp
```

### ngrok

HTTP publication enhancer, not separate MCP transport.

```bash
abird-link --http --ngrok
```

Remote client receives normal Streamable HTTP MCP endpoint.

## 12. Ephemeral URL policy

Global:

```text
--ephemeral-url
```

Specific:

```text
--http-ephemeral-url[=BOOL]
--ngrok-ephemeral-url[=BOOL]
```

Specific overrides global.

HTTP and ngrok can be independent.

Ephemeral path is not authentication.

## 13. ChatGPT user flow

Documented current flow:

```text
abird-link --setup
choose openai
run abird-link
ChatGPT Settings → Security and login → Developer mode
Plugins → +
Connection: Tunnel
select/paste tunnel ID
review tools
select Abird Link in chat
```

Optional private plugin creator material:

```text
docs/CHATGPT_PLUGIN.md
prompts/PLUGIN_CREATOR.md
```

## 14. Claude.ai user flow

Claude.ai remote service needs public HTTPS MCP.

Recommended:

```bash
abird-link --http --ngrok --ngrok-ephemeral-url
```

Register printed URL as custom connector.

## 15. Logging

Normal activity is default:

```text
[01:06:47.410] TOOL read → path=README.md limit=1
[01:06:47.411] TOOL read ← ok 1ms
```

`-s/--silent` suppresses TOOL activity.

`-v/--verbose` adds developer REQ logs.

`--silent --verbose` means REQ-only.

`--color=auto|always|never` controls ANSI; auto checks interactive stderr.

Developer logging must never intentionally dump file contents/raw payload bodies/secrets.

## 16. Build/release

Rust toolchain:

```text
1.98.1
```

Crane refactor intent:

```text
buildDepsOnly
→ cache dependency artifacts separately
→ package/test/clippy reuse them
```

Linux release target:

```text
x86_64-unknown-linux-musl
```

Windows:

```text
x86_64-pc-windows-gnu
```

Current intended outputs:

```text
deps
cross-linux-x86_64-deps
cross-linux-x86_64
dist-linux-x86_64
cross-windows-x86_64-deps
cross-windows-x86_64
dist-windows-x86_64
```

Release files:

```text
abird-link-linux-x86_64
abird-link-linux-x86_64.sha256
abird-link-windows-x86_64.exe
abird-link-windows-x86_64.exe.sha256
```

Installers:

```text
install.sh
install.ps1
scripts/build-release-artifacts.sh
```

Canonical upstream is `https://github.com/abird-ai/abird-link`; installers default there while retaining repository/base-URL overrides.

## 17. Validation status

Latest completed Rust validation included:

```text
cargo fmt --check
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
git diff --check
```

Latest observed full test pass:

```text
77 tests
```

The 2026-09-30 continuation pass also revalidated the current source with the intended Rust 1.98.1 toolchain:

```text
cargo fmt --check
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
git diff --check
```

All 77 tests passed, Clippy and formatting were clean, the current binary rebuilt successfully, and the opt-in real Bubblewrap runtime/cache smokes passed with `ABIRD_TEST_BWRAP=1`. Recovery coverage includes the 10-failure OpenAI escalation, restart-marker propagation, bounded peer teardown, and restart-backoff reset/cap behavior.

Live smokes have validated:

- stdio MCP initialize;
- HTTP MCP initialize;
- concurrent transport lifecycle;
- runtime transport override;
- ephemeral HTTP path behavior;
- Bubblewrap isolation;
- NixOS toolchain accessibility;
- Nix daemon hiding;
- cache RO/RW mounts;
- cache denies;
- normal/silent/verbose logging;
- HTTP request logging;
- color forcing.

Crane/release validation completed on 2026-09-30:

- refreshed `flake.lock` with Crane v0.24.0 and rust-overlay;
- `nix flake show` evaluated native, Linux-musl, Windows-GNU and dist outputs using an isolated writable evaluation store;
- `nix flake check --all-systems --no-build` passes for x86_64 Linux, aarch64 Linux, and aarch64 Darwin;
- all-systems evaluation exposed that nixpkgs 26.11 dropped x86_64-darwin, so that broken advertised flake system was removed and Intel macOS is documented as a Cargo-from-source path;
- full x86_64-linux `nix flake check` passes in the isolated writable Nix store;
- native Crane dependency/package/test/Clippy/fmt outputs build successfully;
- Linux-musl dependency/package/dist outputs build successfully;
- Linux release artifact is ELF64 x86_64 with no interpreter or dynamic `NEEDED` entries, its checksum verifies, and `--version` runs as `abird-link 0.5.0`;
- Windows-GNU dependency/package/dist outputs build successfully;
- Windows release artifact is a valid x86_64 PE executable, its checksum verifies, and it runs under Wine printing `abird-link 0.5.0`;
- `install.sh` installs the real Linux release fixture and verifies SHA-256;
- `install.ps1` parses/runs under PowerShell and its fixture test verifies the installed Windows binary hash;
- `install.sh` and `scripts/build-release-artifacts.sh` are executable and pass shell syntax validation;
- the release helper's build stages resolve successfully; when using the isolated rooted store its final copy requires mapping logical `/nix/store` paths to the rooted store path, while a normal Nix store has those paths directly.

## 18. Remaining operational work

The implementation/release phase is complete. Remaining work is operational rather than code recovery:

1. restart the live connector so ChatGPT is definitely using the final committed binary;
2. refresh the ChatGPT developer connection/tool schema and smoke the final tool surface/logging;
3. configure the intended Git remote/release destination;
4. publish/push only when explicitly desired;
5. optionally add CI, including a native Windows runner for future regression coverage.

## 19. What not to do

- do not reset the working tree;
- do not expose whole home for package caches;
- do not expose Nix daemon by default;
- do not let cache grants expand MCP file roots;
- do not call ephemeral URL authentication;
- do not restore `fs_*` naming;
- do not overload text read/write with binary modes;
- do not make `-v` normal activity logging again;
- do not reuse `-s` for setup;
- do not silently enable network because a package manager wants a dependency;
- do not weaken path canonicalization/deny precedence just to get a build passing;
- do not hard-code an invented GitHub owner.

## 20. Recommended first actions for the next agent

1. Read every file in this handoff bundle.
2. Inspect `git status` and current history.
3. Treat commits `b76f8e6` and `7b93e92` as the completed implementation/release phases.
4. Confirm documentation/handoff commit state.
5. Restart/refresh the live ChatGPT connector if final runtime verification is needed.
6. Configure a Git remote/release destination only when the user is ready to publish.
7. Continue with optional CI/auth/additional-platform work from `11-NEXT-WORK-ROADMAP.md`.

Supporting specs in this directory contain deeper details and rationale; do not treat this file as a substitute for reading them before significant architectural changes.
