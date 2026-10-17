# dotlink agent guide

Use `.agents/AGENTS.md` as the first agent-facing entrypoint for the repository.
All paths and validation commands below are relative to the repository root.

## Read first

1. `.agents/plans/START_HERE.md` for the current canonical handoff and authority order.
2. `.agents/plans/TODO.md` and `.agents/plans/TODO-SPEC.md` for plan status and the tick rule.
3. `.agents/plans/dotlink-handoff-2026-10-01/TODO.md` for the current plan's checklist.
4. `.agents/plans/dotlink-handoff-2026-10-01/HANDOFF.md` for the comprehensive current state/design handoff.
5. `README.md` for the product surface and user workflow.
6. `docs/ARCHITECTURE.md` for runtime boundaries.
7. `docs/SECURITY.md` for authority and sandbox invariants.
8. `.agents/docs/configuration.md` for schema-v10 profiles, OAuth/ngrok behavior, and CLI precedence.
9. `.agents/docs/release-install.md` for release/install details.
10. `.agents/docs/nixos-service.md` for declarative service/module and systemd-credential semantics.
11. `.agents/plans/dotlink-handoff-2026-09-30/` only when historical implementation context is specifically needed; it is not current-state authority.

## Current invariants

- Config schema is currently **v10**. Compatible older schemas migrate forward only through explicit versioned migration steps (currently v9 → v10); newer schemas and older versions without a defined migration path are rejected rather than guessed. OAuth state is separate from JSONC under the Abird XDG state namespace.
- The launch directory is the internal relative-path base and is read-allowed by default unless `default_allow=false` or `--no-default-allow` is used.
- Profile transports start automatically. `--stdio` / `--http` add local transports for one run; `--no-stdio` / `--no-http` / `--no-ngrok` suppress profile defaults. Setup is transactional/default-aware, and `dotlink profile` owns persistent profile mutation. Any externally reachable HTTP is OAuth-protected by default: ngrok is protected automatically, direct non-loopback binds require OAuth + an explicit HTTPS public origin, and only the one-run `--allow-public-no-auth` escape hatch permits unauthenticated public ingress.
- Linux shell execution is Bubblewrap-sandboxed by default. Network is off unless explicitly granted.
- Filesystem denies win over allows. Cache mounts never expand MCP filesystem authority.
- Linux native file IO must stay descriptor-rooted with openat2 and fail closed on unsupported resolution. Linux tools are concurrent by default; `-s/--serialize` opts into one-at-a-time MCP calls. macOS/Windows retain conservative gate until equivalents exist. Never reintroduce pathname IO following authorization for Linux tools.
- Logging is quiet by default: `-v` = TOOL, `-vv` = TOOL + REQ. Interactive non-stdio runs support live `v` cycling plus `Ctrl+R` restart; stdio never shares stdin with runtime controls. Runtime controls must snapshot/restore the exact terminal state on every exit/error path. `--quiet -vv` is REQ-only.
- Linux KVM/GPU grants are independent opt-ins: vendor-check DRM devices, pin character-device FDs and use Bubblewrap `--dev-bind` rather than regular `--bind-fd`; read-only driver resources must not expand native MCP FS grants. NixOS hardware permissions affect only the corresponding systemd unit.
- Keep stdout protocol-clean for stdio; human output goes to stderr.

## Checklists before prose

Plan status is tracked in `.agents/plans/TODO.md`, the plan checklist in
`.agents/plans/<plan>/TODO.md`, and the rules in `.agents/plans/TODO-SPEC.md`.
A plan may add an optional `README.md` (details with Design/Decisions/References
sections and `Author:`/`Co-authors:`); `TODO.md` is required, grouped by the
commit that landed each set, with a `## Tests and verification` section that
must be fully checked before the plan closes. Tick an item only after its checks
pass, in the same session, on the commit you are marking. Do not record a pass
that was not run.

## Validation before commit

```bash
cargo fmt --check
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
git diff --check
alejandra --check flake.nix nix
markdownlint-cli2
```

Use `nix develop` for the pinned Rust, Alejandra, and Markdown lint tools.
`nix flake check` runs the reproducible build, test, and lint checks together.

For sandbox-affecting work, also run `DOTLINK_TEST_BWRAP=1 cargo test --locked --all-features`.
For a machine with configured GPU drivers, optionally run
`DOTLINK_TEST_BWRAP=1 DOTLINK_TEST_GPU_SMOKE=1 cargo test --locked --all-features discovered_gpu_and_kvm_nodes_are_visible_only_when_granted`
to exercise actual `nvidia-smi` and `vulkaninfo` within Bubblewrap (when installed). Profile/setup changes should also smoke `dotlink profile` lifecycle/mutations and transactional setup cancellation. For release/build changes, run the Nix flake checks and relevant dist outputs.

Do not weaken permissions, sandboxing, credential masking, or transport isolation to make a test pass.
