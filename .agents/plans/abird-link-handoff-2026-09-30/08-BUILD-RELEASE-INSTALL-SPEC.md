# Build, release and installer specification

## Rust toolchain

```text
rust-toolchain.toml
channel = 1.98.1
profile = minimal
components = clippy,rustfmt
```

## Crane build design

Current working-tree `flake.nix` uses:

```text
nixpkgs
crane
rust-overlay
```

Primary design:

```text
buildDepsOnly
→ reusable Cargo dependency artifacts
→ buildPackage / cargoTest / cargoClippy reuse artifacts
```

Goal: source-only changes should not invalidate/rebuild the entire dependency graph.

Native conceptual outputs:

```text
.#deps
.#abird-link
default package
checks
devShell
formatter
```

## Cross build targets

Current intended cross builds are exposed only from x86_64 Linux host outputs.

### Portable Linux x86_64

Rust target:

```text
x86_64-unknown-linux-musl
```

Intent:

- static CRT;
- run on Debian and other x86_64 Linux distributions;
- no runtime Nix requirement;
- avoid dependence on host glibc version.

Outputs:

```text
cross-linux-x86_64-deps
cross-linux-x86_64
dist-linux-x86_64
```

Stable asset names:

```text
abird-link-linux-x86_64
abird-link-linux-x86_64.sha256
```

### Windows x86_64

Rust target:

```text
x86_64-pc-windows-gnu
```

Cross system:

```text
x86_64-w64-mingw32 / MinGW/MSVCRT
```

Outputs:

```text
cross-windows-x86_64-deps
cross-windows-x86_64
dist-windows-x86_64
```

Stable assets:

```text
abird-link-windows-x86_64.exe
abird-link-windows-x86_64.exe.sha256
```

## Release helper

```text
scripts/build-release-artifacts.sh
```

Intended workflow:

1. build/cache Linux target deps;
2. build/cache Windows target deps;
3. build Linux dist output;
4. build Windows dist output;
5. copy stable filenames into `./dist`.

`/dist` is ignored.

## Installers

### Unix/Linux

```text
install.sh
```

Current intended behavior:

- detect Linux x86_64;
- download stable release asset;
- download matching `.sha256`;
- verify SHA-256;
- install to `~/.local/bin` by default;
- support custom version/repository/base URL/install dir.

Environment knobs:

```text
ABIRD_LINK_VERSION
ABIRD_LINK_REPO
ABIRD_LINK_RELEASE_BASE_URL
ABIRD_LINK_INSTALL_DIR
```

### Windows

```text
install.ps1
```

Downloads Windows x86_64 asset and verifies SHA-256.

## No remote configured

Current checkout has no Git remote.

Therefore installers intentionally do **not** hard-code an invented GitHub owner.

README examples use placeholders/environment variables.

## Validation status

Completed in the 2026-09-30 continuation pass:

- refreshed `flake.lock` with Crane v0.24.0 and rust-overlay;
- `nix flake show` successfully evaluated native, Linux-musl, Windows-GNU and dist outputs via an isolated writable evaluation store;
- `nix flake check --all-systems --no-build` passed for x86_64 Linux, aarch64 Linux, and aarch64 Darwin;
- removed x86_64-darwin from the flake system matrix because nixpkgs 26.11 dropped support; Intel macOS is a Cargo-from-source path for now;
- full x86_64-linux `nix flake check` completed successfully in the isolated writable Nix store;
- `.#deps`, Linux-musl deps/package/dist, and Windows-GNU deps/package/dist all build successfully;
- Linux release output is ELF64 x86_64 with no dynamic interpreter/dependency entries, checksum verifies, and the executable reports `abird-link 0.5.0`;
- Windows release output is PE x86_64, checksum verifies, and Wine execution reports `abird-link 0.5.0`;
- `install.sh` passes both mock and real Linux release fixture installation/checksum tests;
- `install.ps1` parses/runs under PowerShell and passes its real Windows release fixture hash/install test;
- `install.sh` and `scripts/build-release-artifacts.sh` are mode 0755 and pass `bash -n`;
- release helper build stages succeed. In the isolated rooted-store test environment, the helper's final copy sees logical `/nix/store` output paths while bytes live under the rooted store prefix; the equivalent copy/checksum stage was verified directly against those rooted outputs. A normal Nix store does not have this path translation issue.

No release-build implementation gap remains. A native Windows CI/runtime smoke is useful future coverage but is not required for the current phase to be considered complete.

## Why full Nix validation is separate

The normal `abird-link` Bubblewrap sandbox deliberately:

- makes Nix store read-only;
- hides host Nix daemon when shell network is denied.

That security model prevents a normal sandbox session from performing the full host Nix build/lock operation.

Do not weaken sandbox security merely to validate the release flake; use a trusted host/CI build context.
