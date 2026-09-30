# Current repository state

Date captured: 2026-09-30

## Git state

Branch:

```text
main
```

The large implementation phase that originally sat above historical baseline `5b8bbdd` has been reviewed and committed.

Key continuation commits:

```text
7b93e92 Add Crane cross builds and release installers
b76f8e6 Expand profiles, sandbox caches, and logging
5b8bbdd Rename to abird-link and add profiles and deny policy
```

The final documentation/handoff commit follows `7b93e92`; use `git log --oneline --decorate -12` for the exact current HEAD.

No Git remote is configured.

## Package/toolchain

```text
crate name: abird-link
crate version: 0.5.0
edition: 2024
rust toolchain: 1.98.1
config schema: 8
```

## Config

Canonical format:

```text
JSONC
```

Default:

```text
~/.config/abird-link/config.jsonc
~/.config/abird-link/runtime.key
```

Named profile `work`:

```text
~/.config/abird-link/config.work.jsonc
~/.config/abird-link/runtime.work.key
```

Legacy `.json` profiles remain readable.

Persistent permission state supports:

```text
cwd
allow_read[]
allow_write[]
allow_rw[]
deny_read[]
deny_write[]
deny_rw[]
allow_shell
allow_network
typed developer caches
transport defaults
```

Older boolean `allow_rw: true` remains compatible and maps to `["."]`.

## Current major CLI

```text
-S, --setup
-p, --profile <NAME>
--cwd <DIR>

--stdio
--http
--http-bind <ADDR>
--ngrok
--ephemeral-url
--http-ephemeral-url[=<BOOL>]
--ngrok-ephemeral-url[=<BOOL>]

--allow-read[=<DIR>]
--allow-write[=<DIR>]
--allow-rw[=<DIR>]
--deny-read[=<DIR>]
--deny-write[=<DIR>]
--deny-rw[=<DIR>]
--deny <PATH>

--allow-shell
--deny-shell
--allow-network
--deny-network

--allow-all --no-sandbox

--print-id
-s, --silent
-v, --verbose
--color <auto|always|never>
--list-tools
```

Important:

- `-s` means silent.
- setup shorthand is uppercase `-S`.
- explicit `--stdio` / `--http` activate those transports even if profile defaults are false.
- bare `--allow-rw` means RW cwd.
- bare `--allow-write` retains historical RW-cwd behavior; explicit `--allow-write=/path` is write-only.
- `--allow-rw=/` is unrestricted filesystem RW inside the sandbox model; it does not disable Bubblewrap.
- paired `--allow-all --no-sandbox` is the full-host unsandboxed escape hatch.
- no dangerous-suffixed authority flags remain.

## MCP tool surface

Read-only:

```text
ls
read
read_binary
```

With write authority:

```text
write
edit
write_binary
patch_binary
```

Shell:

```text
bash         Unix/Linux
powershell   Windows
```

Tool visibility is policy-dependent; unavailable tools are omitted.

## Transport model

All active transports share one `LocalMachine` policy domain:

```text
OpenAI Secure MCP Tunnel
stdio
Streamable HTTP
optional ngrok publication for HTTP
```

Local HTTP default:

```text
http://127.0.0.1:3000/mcp
```

## Logging model

Default:

```text
[HH:MM:SS.mmm] TOOL read → path=README.md limit=1
[HH:MM:SS.mmm] TOOL read ← ok 1ms
```

- `-s/--silent`: hides TOOL activity.
- `-v/--verbose`: adds REQ-level developer logs.
- `--silent --verbose`: REQ-only.
- `--color=auto|always|never`: controls ANSI.

## Developer cache model

Typed cache grants:

```text
cargo_registry
cargo_git
npm
pnpm
yarn
pip
uv
go_mod
go_build
maven
gradle
sccache
ccache
```

Modes:

```text
read_only
read_write
```

Cache mounts are shell-only and do not expand the MCP filesystem policy.

## Build/release state

The Crane release architecture is implemented and validated.

Native outputs:

```text
deps
abird-link
checks: package/tests/clippy/fmt
devShell
formatter
```

x86_64 Linux release:

```text
cross-linux-x86_64-deps
cross-linux-x86_64
dist-linux-x86_64
```

x86_64 Windows GNU release:

```text
cross-windows-x86_64-deps
cross-windows-x86_64
dist-windows-x86_64
```

Stable assets:

```text
abird-link-linux-x86_64
abird-link-linux-x86_64.sha256
abird-link-windows-x86_64.exe
abird-link-windows-x86_64.exe.sha256
```

Validated on 2026-09-30:

- `flake.lock` locks Crane v0.24.0 and rust-overlay.
- Rust fmt/test/Clippy passes; 70/70 tests.
- real Bubblewrap runtime and cache-mount smokes pass.
- full x86_64-linux `nix flake check` passes using an isolated writable Nix store without exposing the host daemon.
- `nix flake check --all-systems --no-build` evaluates x86_64 Linux, aarch64 Linux and aarch64 Darwin.
- x86_64-darwin was removed because nixpkgs 26.11 dropped support; Intel macOS is documented as Cargo-from-source for now.
- Crane native dependency artifacts build separately and are reused.
- Linux-musl dependency/package/dist builds succeed.
- Linux artifact is ELF64 x86_64, has no interpreter, `ldd` reports statically linked, checksum verifies, and `--version` runs.
- Windows-GNU dependency/package/dist builds succeed.
- Windows artifact is PE32+ x86_64 / Windows CUI and imports only Windows system DLLs; checksum verifies.
- Windows executable runs under Wine and prints `abird-link 0.5.0`.
- `scripts/build-release-artifacts.sh` succeeds end-to-end and produces the four expected release files.
- `install.sh` succeeds against the real Linux release fixture and installs a byte-identical runnable binary.
- `install.ps1` parses under PowerShell 7.6.6 and its checksum/copy/install flow succeeds against the real Windows release fixture.

## Residual operational limitations

- The currently running ChatGPT connector may still be an older process until it is restarted and the developer connection/tool schema is refreshed.
- Public HTTP/ngrok still has no built-in application-layer caller authentication; ephemeral paths are not authentication.
- Bubblewrap is Linux-only. Native Windows/macOS shell uses the explicitly unsandboxed full-host path.
- There is no configured Git remote, so nothing has been pushed and installers intentionally require repository/base-URL configuration.
