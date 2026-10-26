# DL-110 — Review remediation

Base: `55f8ef5`

## Committed

### `e597f1b` — Diagnostic and text-paging correctness

- [x] Log only sanitized top-level stdio methods; never search nested/raw payload
- [x] Refuse line-zero truncation and an empty successful read window

### `1a32c8f` — Bounded filesystem discovery

- [x] Bound the shared directory walker at 100,000 visited entries per request
- [x] Report partial scans explicitly; never claim global ordering of incomplete scans

### `b46f8c1` — Shell process and task lifetime

- [x] Abort request-owned pipe workers on cancellation
- [x] Start Windows child suspended; associate Job Object before resuming
- [x] State best-effort Unix process-group cleanup for unsandboxed `setsid` children

### `cb5d57c` — Crash-consistent credential selection

- [x] Store immutable, versioned Runtime key and OAuth owner-password generations
- [x] Atomically select both via JSONC; preserve legacy selectors and credentials
- [x] Use shared profile/state locks; detect stale interactive setup
- [x] Delete versioned Runtime keys on profile deletion
- [x] Require the selected key when OpenAI is active, not when dormant

### `5c9063c` — Recorded design decisions

- [x] Document intentionally unrestricted local HTTP/stdio model-directed concurrency
- [x] Document process-group and native-platform testing boundaries

### `724ec23` — Secure handling of absent sandbox mask targets

- [x] Mask existing protected files/directories in Bubblewrap
- [x] Prepare dotlink-owned OAuth state privately before shell mounts
- [x] Refuse missing protected paths under writable mounts rather than creating host-side mount targets
- [x] Preserve the deliberate unsandboxed-shell authority boundary

### `a4390c3` — CLI persistence regression

- [x] Verify dormant selected Runtime key survives successive profile mutations with the key file absent
- [x] Verify local-only startup still succeeds after those edits

## Uncommitted

None.

## Tests and verification

- [x] `cargo fmt --check`
- [x] `cargo test --locked --all-features` — 333 unit + 5 integration
- [x] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [x] `git diff --check`
- [x] `alejandra --check flake.nix nix`
- [x] `nix flake check --no-build` — evaluation only, not cross-build execution
- [x] `markdownlint-cli2` via Nix dev shell
- [x] Manual isolated CLI smoke — profile edits preserve inactive key generation, local startup works
- [!] Windows/macOS native tests — require native CI runners, not available through this Linux connector
- [!] Windows cross-build — Nix dependencies/toolchains not ready locally; prior attempts timed out
- [!] Bubblewrap runtime smoke — connector cannot mount procfs inside nested sandbox (`Operation not permitted`)
