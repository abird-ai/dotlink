# Changelog

## Unreleased

- Simplify first-run setup into a compact beginner-friendly checklist.
- Hide verbose tunnel INFO logs by default while keeping them available through `RUST_LOG`.
- Add a complete Nix flake package, app, development shell, formatter, and `nix flake check` checks for tests, rustfmt, and Clippy.
- Add `-v` / `--verbose` concise MCP request and tool-call logging.
- Add `--list-tools`, generated directly from the live MCP tool router.
- Add direct setup links for finding ChatGPT Workspace IDs and OpenAI Organization IDs.


## 0.2.0

- Added `--cwd=<DIR>`.
- The filesystem workspace now defaults to the process launch directory on every run.
- Removed the persisted filesystem root from tunnel configuration.
- Strengthened MCP descriptions around the workspace boundary.
- Clarified that `fs_*` tools are workspace-bounded while `shell_exec` is intentionally not an OS sandbox.
- Added ChatGPT tunnel-registration and Plugin Creator instructions.

## 0.1.0

- Initial native Rust Secure MCP Tunnel client.
- Embedded `rmcp` server with local filesystem and Bash tools.
