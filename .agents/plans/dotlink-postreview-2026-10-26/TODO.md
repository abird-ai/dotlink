# DL-140 — Post-review correctness and work limits

Base: `46fa0d6`

## Committed

### `36ff4d8` — Configuration-only tunnel inspection

- [x] `--print-id` reads migrated JSONC or default tunnel ID override
- [x] Never requires Runtime secret resolution or interactive setup
- [x] CLI regression covers missing key and override precedence

### `734b6e3` — Shared text classification and bounded search

- [x] One UTF-8/non-binary-control predicate for read/write/edit/patch/grep
- [x] Preserve unrestricted binary tools for byte-oriented payloads
- [x] Charge actual grep file-read bytes and limit candidates per request
- [x] Explicit incomplete marker/reason for directory or content budget
- [x] MCP binary and grep paging regression tests

### `e81dca8` — Transactional OAuth code redemption

- [x] Validate and lock one-time code until grant issuance succeeds
- [x] Perform fallible token generation/access lock before durable mutation
- [x] Return retryable HTTP 503 on storage failures
- [x] End-to-end failure/retry test; concurrent requests redeem once

### `04cb912` — Architecture and user-facing contracts

- [x] Update README, CHANGELOG, AGENTS, architecture, security, configuration
- [x] Document local work limits separately from intentionally open concurrency

## Uncommitted

None.

## Tests and verification

- [x] `cargo fmt --check`
- [x] `cargo test --locked --all-features` — 340 unit + 14 integration
- [x] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [x] `git diff --check`
- [x] `alejandra --check flake.nix nix`
- [x] `nix flake check --no-build` — evaluation only
- [x] `markdownlint-cli2` — 59 Markdown files, zero issues
- [x] Production-size 35 MiB grep probe returned content_budget incomplete
- [x] Live OAuth failed-store recovery plus one successful concurrent redemption
- [x] Stdio binary NUL tests across text and binary tool families
- [!] Native Windows/macOS execution — requires native platform runners
- [!] Nested Bubblewrap smoke — procfs mount unavailable via connector
