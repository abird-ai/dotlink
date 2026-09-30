# dotlink agent guide

Use this as the first agent-facing entrypoint for the repository.

## Read first

1. `README.md` for the product surface and user workflow.
2. `ARCHITECTURE.md` for runtime boundaries.
3. `SECURITY.md` for authority and sandbox invariants.
4. `.agents/docs/configuration.md` for schema-v9 profiles and CLI precedence.
5. `.agents/docs/release-install.md` for release/install details.
6. `.agents/plans/dotlink-handoff-2026-09-30/00-START-HERE.md` only when historical implementation context is needed.

## Current invariants

- Config schema is **v9 only**; older profile schemas are rejected.
- The launch directory is the internal relative-path base and is read-allowed by default unless `default_allow=false` or `--no-default-allow` is used.
- Profile transports start automatically. `--stdio` / `--http` add local transports for one run; `--no-stdio` / `--no-http` / `--no-ngrok` suppress profile defaults. Setup is transactional/default-aware, and `dotlink profile` owns persistent profile mutation.
- Linux shell execution is Bubblewrap-sandboxed by default. Network is off unless explicitly granted.
- Filesystem denies win over allows. Cache mounts never expand MCP filesystem authority.
- Logging is quiet by default: `-v` = TOOL, `-vv` = TOOL + REQ. Interactive non-stdio runs support live `v` cycling plus `Ctrl+R` restart; stdio never shares stdin with runtime controls. `--silent -vv` is REQ-only.
- Keep stdout protocol-clean for stdio; human output goes to stderr.

## Validation before commit

```bash
cargo fmt --check
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
git diff --check
```

For sandbox-affecting work, also run `DOTLINK_TEST_BWRAP=1 cargo test --locked --all-features`. Profile/setup changes should also smoke `dotlink profile` lifecycle/mutations and transactional setup cancellation. For release/build changes, run the Nix flake checks and relevant dist outputs.

Do not weaken permissions, sandboxing, credential masking, or transport isolation to make a test pass.
