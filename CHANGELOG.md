# Changelog

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
