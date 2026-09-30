# Release and installation reference

Canonical upstream: `https://github.com/abird-ai/dotlink`.

## Installer behavior

Linux x86_64:

```bash
curl -fsSL https://raw.githubusercontent.com/abird-ai/dotlink/main/install.sh | sh
```

Windows PowerShell:

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
dotlink-windows-x86_64.exe
dotlink-windows-x86_64.exe.sha256
```

## Nix / Crane outputs

Native:

```bash
nix build
nix run .
nix flake check
nix develop
nix build .#deps
```

Portable Linux x86_64 (musl):

```bash
nix build .#cross-linux-x86_64-deps
nix build .#cross-linux-x86_64
nix build .#dist-linux-x86_64
```

Windows x86_64 (GNU/MinGW):

```bash
nix build .#cross-windows-x86_64-deps
nix build .#cross-windows-x86_64
nix build .#dist-windows-x86_64
```

Build both stable-named release artifacts:

```bash
./scripts/build-release-artifacts.sh
```

The helper enables `nix-command` and `flakes` explicitly.

## Platform notes

- Linux package includes Bash + Bubblewrap.
- Apple Silicon macOS is supported by the current flake.
- Intel macOS should build from Cargo because the pinned nixpkgs line no longer supports x86_64-darwin.
- Native Windows/macOS filesystem tools retain Rust allow/deny enforcement, but shell execution has no Bubblewrap equivalent; WSL2 is the recommended Windows sandbox path.
