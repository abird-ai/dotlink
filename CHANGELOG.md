# Changelog

## Unreleased

- Replace the old fs_* API with a Pi-like tool surface: read, write, edit, ls, and platform shell.
- Add read_binary, write_binary, and patch_binary with MCP/base64/hex support.
- Make the default tool surface read-only.
- Add additive --allow-read, --allow-write, --allow-rw, and precedence --deny directory policies.
- Make bare --allow-write shorthand for read+write cwd.
- Add Linux Bubblewrap shell sandboxing with network disabled by default.
- Add --allow-network for sandboxed network access.
- Add --allow-rw-all-dangerous, --allow-network-dangereous, and --allow-all-dangerous for explicit unsandboxed host authority.
- Add Bash on Unix and PowerShell selection on Windows.
- Simplify first-run setup into a compact beginner-friendly checklist.
- Hide verbose tunnel INFO logs by default while keeping them available through RUST_LOG.
- Add a complete Nix flake package, app, development shell, formatter, Bubblewrap runtime, and nix flake check checks.
- Add -v / --verbose concise MCP request and tool-call logging.
- Add --list-tools generated from the live policy-aware MCP router.
- Add direct setup links for ChatGPT Workspace IDs and OpenAI Organization IDs.

## 0.2.0

- Added --cwd=<DIR>.
- The filesystem workspace defaults to the process launch directory.
- Added ChatGPT tunnel-registration and Plugin Creator instructions.

## 0.1.0

- Initial native Rust Secure MCP Tunnel client.
- Embedded rmcp server with local filesystem and Bash tools.
