# Release and installation reference

Canonical upstream: `https://github.com/abird-ai/dotlink`.

## Installer behavior

Linux x86_64/ARM64 or macOS Apple Silicon:

```bash
curl -fsSL https://raw.githubusercontent.com/abird-ai/dotlink/main/install.sh | sh
```

Windows x86_64/ARM64 PowerShell:

```powershell
irm https://raw.githubusercontent.com/abird-ai/dotlink/main/install.ps1 | iex
```

The installers verify SHA-256 sidecars before replacement. Unix replacement is atomic.

Supported overrides:

```text
DOTLINK_VERSION
DOTLINK_REPO
DOTLINK_RELEASE_BASE_URL
DOTLINK_INSTALL_DIR
```

Default Unix install path: `~/.local/bin`.

## Stable release assets

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

The Linux binaries are static musl builds with no distro-specific glibc dependency, so the same artifact runs across NixOS, Debian, and other compatible Linux distributions.

## Nix / Crane outputs

Native development:

```bash
nix build
nix run .
nix flake check
nix develop
nix build .#deps
```

Complete release bundle from x86_64 Linux:

```bash
nix build .#release-all
```

The build host distribution is not part of the target ABI: the GitHub workflow uses an x86_64 Linux hosted runner, while the same `release-all` output can be built directly from any suitable x86_64 Linux Nix host. All target compilers, SDKs, and linkers are pinned by Nix.

Individual cross outputs:

```bash
# Linux x86_64 / ARM64 (static musl)
nix build .#cross-linux-x86_64
nix build .#cross-linux-aarch64
nix build .#dist-linux-x86_64
nix build .#dist-linux-aarch64

# Windows x86_64 / ARM64
nix build .#cross-windows-x86_64
nix build .#cross-windows-aarch64
nix build .#dist-windows-x86_64
nix build .#dist-windows-aarch64

# macOS ARM64, cross-built from Linux
nix build .#cross-macos-aarch64
nix build .#dist-macos-aarch64
```

Dependency-layer outputs are available as the matching `*-deps` attributes. Windows ARM64 uses the pinned LLVM-MinGW/UCRT toolchain from nixpkgs. macOS ARM64 uses a pinned Apple SDK fetched by Nix plus Linux-hosted LLVM/ld64.lld.

To materialize the complete bundle under `./dist`:

```bash
./scripts/build-release-artifacts.sh
```

The helper enables `nix-command` and `flakes` explicitly and delegates all target logic to `release-all`.

## Platform notes

- Linux x86_64/ARM64 release binaries are static and run on NixOS, Debian, and other compatible Linux distributions without a Nix runtime.
- Windows x86_64 uses GNU/MinGW; Windows ARM64 uses LLVM-MinGW/UCRT.
- Apple Silicon macOS is cross-built from x86_64 Linux with SDK 14.4 and deployment target macOS 11.0.
- Intel macOS is not currently published; build from Cargo if needed.
- Native Windows/macOS filesystem tools retain Rust allow/deny enforcement, but shell execution has no Bubblewrap equivalent; WSL2 is the recommended Windows sandbox path.
