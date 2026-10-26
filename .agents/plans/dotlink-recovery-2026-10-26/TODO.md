# DL-120 — Credential and OAuth reliability

Base: `81a1c9f`

## Committed

### `d6652d6` — Credential lifecycle and OAuth correctness

- [x] Separate strict runtime key loading from repairable profile management
- [x] Retry interrupted profile deletion, including orphaned generated keys
- [x] Retain active and immediately prior generations; prune older secrets only
  after a durable credential rotation, never after unrelated profile edits
- [x] Keep Argon2 worker concurrency permits alive after HTTP cancellation
- [x] Validate refresh-token and authorization-code bindings before consumption
- [x] Initialize both owned private XDG directories before writable sandbox mounts
- [x] Unit/integration coverage for all lifecycle regressions

### `cd96358` — Safe protocol diagnostics

- [x] Shared bounded/sanitized JSON-RPC identifier formatting for stdio/tunnel
- [x] Reject control characters, unexpected identifiers and oversized values

### `603f356` — Document architectural contracts

- [x] Update README, CHANGELOG, security, architecture and config reference
- [x] Record that local HTTP/stdio tool admission is intentionally unrestricted

## Uncommitted

None.

## Tests and verification

- [x] `cargo fmt --check`
- [x] `cargo test --locked --all-features` — 338 unit + 9 integration
- [x] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [x] `git diff --check`
- [x] `alejandra --check flake.nix nix`
- [x] `nix flake check --no-build` — evaluation, not cross-build
- [x] `markdownlint-cli2` — 55 Markdown files, no findings
- [x] End-to-end isolated OAuth HTTP regression: wrong-client refresh returns
  400 without consuming the persisted valid refresh token
- [x] Isolated CLI smoke: profile management, key selector, private XDG dirs
- [!] Windows/macOS native CI — requires platform runners unavailable here
- [!] Bubblewrap mount smoke — nested procfs mount denied in connector sandbox
