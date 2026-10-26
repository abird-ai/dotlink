# DL-130 — Live credential selection and recovery

Base: `713f017`

## Committed

### `afa28ea` — Resolve effective Runtime credentials

- [x] Apply explicit environment/file overrides before resolving saved keys
- [x] Never read a dormant Runtime key for an inactive OpenAI transport
- [x] Preserve strict selected-key validation when active and not overridden
- [x] Isolated CLI regressions for missing, unreadable and overridden credentials

### `a568829` — Live OAuth owner-password selection

- [x] Resolve the current committed owner generation for every approval
- [x] Recheck a valid password's generation before authorizing
- [x] Fail closed if a previously persisted profile is removed
- [x] Preserve environment-only legacy owner-hash compatibility
- [x] End-to-end HTTP DCR/authorization test for rotation and deletion

### `2b0093e` — Document architectural contracts

- [x] Update architecture, security, configuration, README and changelog
- [x] Record the explicit three-state owner selection and source precedence
- [x] Preserve deliberately unrestricted local HTTP/stdio tool concurrency

## Uncommitted

None.

## Tests and verification

- [x] `cargo fmt --check`
- [x] `cargo test --locked --all-features` — 338 unit + 12 integration
- [x] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [x] `git diff --check`
- [x] `alejandra --check flake.nix nix`
- [x] `nix flake check --no-build` — evaluation only
- [x] `markdownlint-cli2` — 57 files, zero findings
- [x] End-to-end live OAuth: old password works before rotation, new password works afterward, old password is refused; removed profile fails closed
- [x] Isolated CLI startup: inactive unreadable key ignored, env/file overrides win over missing saved key
- [!] Native Windows/macOS execution — requires platform runners
- [!] Bubblewrap runtime smoke — nested procfs mount unavailable via connector
