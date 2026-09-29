# Build, release, installer and CI specification

## Goals

The build system should provide:

- reproducible Rust builds;
- reusable Cargo dependency artifacts;
- one source of truth for target/toolchain logic;
- local and CI parity;
- cross-platform release output from one x86_64 Linux Nix host;
- thin CI;
- simple stable asset names;
- checksum verification;
- idempotent installers/updaters.

## Toolchain

```text
Rust: 1.98.1
edition: 2024
```

Pinned by `rust-toolchain.toml` / flake inputs.

## Nix/Crane architecture

Crane is used to separate dependency build artifacts from application source.

Conceptual flow:

```text
clean Cargo source
   ↓
buildDepsOnly
   ↓
cargoArtifacts
   ├── package
   ├── tests
   ├── clippy
   └── fmt/check graph
```

Cross targets use the same pattern.

Do not move cross-target toolchain logic into GitHub Actions.

## Native outputs

Per supported flake system:

```text
packages.default
packages.dotlink
packages.deps
checks.package
checks.tests
checks.clippy
checks.fmt
apps.default
devShells.default
formatter
```

Current flake systems include:

- x86_64-linux;
- aarch64-linux;
- aarch64-darwin.

## Release host

The complete published release set is built from:

```text
x86_64 Linux
```

Command:

```bash
nix build .#release-all
```

This is intentionally the same entry point locally and in CI.

## Published targets

### Linux x86_64

Rust target:

```text
x86_64-unknown-linux-musl
```

Static CRT/musl.

Asset:

```text
dotlink-linux-x86_64
```

### Linux ARM64

Rust target:

```text
aarch64-unknown-linux-musl
```

Asset:

```text
dotlink-linux-aarch64
```

### Windows x86_64

Rust target/toolchain:

```text
x86_64-pc-windows-gnu
MinGW/MSVCRT
```

Asset:

```text
dotlink-windows-x86_64.exe
```

### Windows ARM64

Rust target:

```text
aarch64-pc-windows-gnullvm
```

Toolchain:

- nixpkgs LLVM-MinGW;
- UCRT path;
- explicit linker/CC/CXX/AR/RANLIB.

Asset:

```text
dotlink-windows-aarch64.exe
```

### macOS ARM64

Rust target:

```text
aarch64-apple-darwin
```

Cross-built from Linux.

Uses:

- pinned Apple SDK 14.4 fetch derivation;
- clang-unwrapped;
- ld64.lld;
- llvm-ar/ranlib;
- `MACOSX_DEPLOYMENT_TARGET=11.0`.

Asset:

```text
dotlink-macos-aarch64
```

This intentionally avoids Nixpkgs' problematic full Darwin/xcbuild bootstrap for this cross path.

## No distro-specific Linux binaries

There are no:

```text
dist-nixos-*
dist-debian-*
```

outputs.

Reason:

Linux release artifacts are static musl executables.

The same binary works across compatible distributions including NixOS/Debian/Ubuntu/etc.

No patchelf is needed for the release binary because it has:

- no ELF interpreter;
- no dynamic `NEEDED` libraries.

This is distinct from the native Nix package, which may wrap runtime paths/tools.

## Nix package vs portable release

Native package:

```bash
nix build .#dotlink
```

May include/wrap Nix runtime dependencies such as Bash/Bubblewrap for the Nix-native user experience.

Portable Linux release:

```bash
nix build .#dist-linux-x86_64
```

The dotlink executable itself is static/portable.

Shell support still depends on external shell/sandbox executables being available when relevant.

## Dist derivations

Every dist output:

- copies the target binary;
- uses stable asset name;
- chmod executable;
- creates SHA-256 sidecar;
- writes VERSION in its output.

## release-all

Aggregates:

```text
dotlink-linux-x86_64
dotlink-linux-x86_64.sha256
dotlink-linux-aarch64
dotlink-linux-aarch64.sha256

dotlink-windows-x86_64.exe
dotlink-windows-x86_64.exe.sha256
dotlink-windows-aarch64.exe
dotlink-windows-aarch64.exe.sha256

dotlink-macos-aarch64
dotlink-macos-aarch64.sha256

VERSION
PLATFORMS.txt
```

The public GitHub Release does not need to publish VERSION/PLATFORMS metadata files; they are build/CI metadata.

## Local release helper

```text
scripts/build-release-artifacts.sh
```

It should remain thin:

```text
nix build .#release-all
copy outputs to ./dist or supplied directory
```

Do not duplicate target-specific compilation logic here.

## GitHub Actions

Workflow:

```text
.github/workflows/build-binaries.yml
```

Host:

```text
ubuntu-24.04 x86_64
```

The host distro is not part of the target ABI; Nix pins target compilers/SDKs.

### Build job

Permissions:

```text
contents: read
```

Steps:

1. checkout;
2. install Nix;
3. `nix build .#release-all`;
4. materialize output;
5. verify checksums;
6. verify runnable Linux x86_64 version;
7. upload CI artifact.

### Release job

Only on:

```text
refs/tags/v*
```

Requires build job.

Permissions:

```text
contents: write
```

Checks:

```text
tag == v$(cat VERSION)
```

Then:

- create release with generated notes if absent;
- otherwise upload/replace assets;
- upload each `dotlink-*` binary/checksum individually.

The Actions ZIP is an internal transfer/debug artifact and is not a public binary release artifact.

## Versioning/release procedure

Before publishing:

1. set Cargo package version;
2. lockfile reflects it;
3. Nix VERSION derives it;
4. run validation;
5. push implementation;
6. create matching annotated tag;
7. push tag.

For current intended release:

```bash
git tag -a v0.6.0 -m "abird dotlink v0.6.0"
git push origin v0.6.0
```

Do not tag until implementation commit is pushed and current live git state confirms readiness.

## install.sh

Supported:

- Linux x86_64;
- Linux ARM64;
- macOS ARM64;
- Windows under MSYS/Cygwin shell path when applicable.

Default:

```text
DOTLINK_VERSION=latest
repo=abird-ai/dotlink
install dir=$HOME/.local/bin
```

Asset selection is internal.

Installed user command:

```text
dotlink
```

not architecture-specific.

Flow:

1. derive asset;
2. resolve latest or versioned release URL;
3. download asset + sidecar;
4. validate sidecar syntax;
5. hash asset;
6. compare hash;
7. chmod executable;
8. execute `--version` and require valid dotlink version;
9. if explicit version requested, require exact match;
10. compare installed hash;
11. no-op if identical;
12. otherwise same-directory temp copy + rename;
13. report Installed/Updated/Already up to date.

Requires curl + SHA-256 tool.

## install.ps1

Supported:

- Windows x64;
- Windows ARM64.

Uses `RuntimeInformation.OSArchitecture`.

Installed filename:

```text
dotlink.exe
```

Flow parallels Unix installer:

- latest/versioned release;
- asset + SHA sidecar;
- Get-FileHash;
- downloaded `--version`;
- requested-version match;
- existing hash check;
- same-directory temp file;
- replacement move;
- cleanup temp.

## Installer override environment

Unix and PowerShell support repository/release overrides for forks/mirrors/testing.

Key environment concepts:

```text
DOTLINK_VERSION
DOTLINK_REPO
DOTLINK_RELEASE_BASE_URL
DOTLINK_INSTALL_DIR
```

Preserve these unless there is a strong compatibility reason to change them.

## Installer security model

Checksums and binaries are fetched from the same configured release base.

This protects against corruption/mismatch, not compromise of the release origin itself.

The additional `--version` identity check catches:

- mislabeled mirror file;
- stale asset;
- wrong version under a requested tag.

Future signing/provenance could strengthen origin authenticity.

## Release validation requirements

Before calling a release build good:

### Linux

```text
SHA sidecar verifies
x86_64 --version runs
ELF machine correct
ARM64 ELF machine correct
no INTERP
no dynamic NEEDED
```

### Windows

Validate PE:

```text
x86_64 machine = 0x8664
ARM64 machine = 0xaa64
```

Wine x86 smoke is useful when available but not required for every source-only doc change.

### macOS

Validate:

- Mach-O 64-bit;
- ARM64 CPU type;
- executable file type.

Native macOS code-sign/notarization is not currently part of the release system.

## Current CI/release status at handoff

GitHub remote:

```text
origin https://github.com/abird-ai/dotlink.git
```

Remote main at handoff is behind local by two commits.

Current remote tag:

```text
v0.5.0
```

Current code:

```text
0.6.0
```

Do not say 0.6.0 is released until push/tag workflow confirms it.

## Future release enhancements

Optional, not blockers:

- release signing;
- SBOM;
- SLSA/provenance;
- macOS signing/notarization;
- native Windows runtime CI smoke;
- native macOS runtime CI smoke;
- Intel macOS publication strategy if demand justifies it.

Keep Nix as the build graph authority even if adding those.
