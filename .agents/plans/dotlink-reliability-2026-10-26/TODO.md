# DL-110 — Review remediation

Base: `55f8ef5`

## Committed

### `e597f1b` — Diagnostic and text-paging correctness

- [x] Sanitize stdio top-level method logging; never scan nested/raw payload
- [x] Refuse line-zero truncation or an empty successful read window

### `1a32c8f` — Bounded filesystem discovery

- [x] Bound shared discovery at 100,000 visited entries
- [x] Explicit partial-scan metadata and non-global sorting contract

### `b46f8c1` — Shell safety and lifetime

- [x] Mask missing protected paths for readable or writable sandbox grants
- [x] Abort spawned pipe workers on request cancellation
- [x] Start Windows child suspended; attach Job Object before resuming
- [x] State best-effort unsandboxed Unix process-group boundary

### `cb5d57c` — Crash-consistent credential selection

- [x] Immutable, versioned runtime key and OAuth owner password hash
- [x] One atomic JSONC selector; preserve legacy schema and credentials
- [x] Shared profile/state locks; detect stale interactive setup
- [x] Clean versioned Runtime keys on profile deletion
- [x] Strictly load live selected key; dormant OpenAI needs no key

## Uncommitted

- [x] Record intentional unbounded local HTTP/stdio admission in documentation

## Tests and verification

- [x] `cargo fmt --check` — `cb5d57c` tree
- [x] `cargo test --locked --all-features` — 332 unit + 4 integration
- [x] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [x] `git diff --check`
- [x] `alejandra --check flake.nix nix`
- [x] `nix flake check --no-build` — evaluation only
- [x] `markdownlint-cli2` via Nix dev shell
- [ ] Windows/macOS native CI — not available from this Linux connector
- [ ] Bubblewrap runtime smoke — nested procfs mount unavailable here
