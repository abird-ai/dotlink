# AGENTS.md

<!-- BEGIN common -->

## Shared conventions

These are byte-identical across all abird repositories. Repo-specific rules
follow the markers below.

- **Source of truth.** Live source, tests and executed results outrank dated
  prose. Start at `.agents/plans/README.md` (overview + spec), then work from
  `.agents/plans/TODO.md` (the queue).
- **XDG paths.** Where the project owns user config, data, cache or state,
  resolve it under `$XDG_*_HOME/abird/<project>` — an absolute XDG base, falling
  back to `HOME` when unset. No legacy or alternate root.
- **Shared `.gitignore`.** The block at the top of `.gitignore` is identical
  across repos; repo-specific entries follow it. Keep it mechanical.
- **Preserve other work.** Never revert or overwrite changes you did not make.
- **Owned changes only.** Commit only the paths you changed for the request.
  Before committing, inspect `git diff --cached --stat`; if it lists files you
  did not touch, stop and ask — unless the user explicitly asked you to include
  them.
- **Ownership.** A plan directory belongs to the `@owner` on the queue line, and
  the coordinator alone reconciles `.agents/plans/TODO.md`; do not edit another
  owner's plan or worktree unless the user explicitly asks.
- **Clean pushes.** Do not push from a dirty worktree unless the user explicitly
  asked to include those changes; otherwise commit/stash your own change first,
  or push from a clean temporary worktree.
- **Terse writing.** Keep code comments and docs succinct. Comments need not be
  full sentences; prefer symbols and fragments unless the user asked for an
  explanation or the topic is design-level or otherwise complex. Prefer bullets
  and engineering focus over prose in other docs.
- **Terse output.** Keep replies short and on point, preferably bulleted; avoid
  prose unless the user asked for an explanation. Be terse without dropping
  detail.
- **Git safety.** Commits are signed; never disable signing. Never push,
  force-push, tag, publish, deploy, or run destructive operations — history
  rewrite, `reset --hard`, branch deletion, or touching another agent's staged
  or uncommitted work — without explicit user confirmation for each action. A
  request to "commit" or "apply" is not authorization to push.

<!-- END common -->

<!-- BEGIN repo-specific -->
## Repository

Use `AGENTS.md` as the first agent-facing entrypoint for the repository.
All paths and validation commands below are relative to the repository root.

- **XDG project:** `abird/dotlink`.

## Read first

1. `.agents/plans/README.md` for the entry and spec.
2. `.agents/plans/TODO.md` for plan status and the tick rule.
3. `.agents/plans/dotlink-handoff-2026-10-01/TODO.md` for the current plan's checklist.
4. `.agents/plans/dotlink-handoff-2026-10-01/HANDOFF.md` for the comprehensive current state/design handoff.
5. `README.md` for the product surface and user workflow.
6. `docs/ARCHITECTURE.md` for runtime boundaries.
7. `docs/SECURITY.md` for authority and sandbox invariants.
8. `.agents/docs/configuration.md` for schema-v10 profiles, OAuth/ngrok behavior, and CLI precedence.
9. `.agents/docs/release-install.md` for release/install details.
10. `.agents/docs/nixos-service.md` for declarative service/module and systemd-credential semantics.
11. `.agents/plans/archive/history/dotlink-handoff-2026-09-30/` only when historical implementation context is specifically needed; it is not current-state authority.

## Current invariants

- Config schema is currently **v10**. Compatible older schemas migrate forward only through explicit versioned migration steps (currently v9 → v10); newer schemas and older versions without a defined migration path are rejected rather than guessed. OAuth state is separate from JSONC under the Abird XDG state namespace.
- The launch directory is the internal relative-path base and is read-allowed by default unless `default_allow=false` or `--no-default-allow` is used.
- Profile transports start automatically. `--stdio` / `--http` add local transports for one run; `--no-stdio` / `--no-http` / `--no-ngrok` suppress profile defaults. Setup is transactional/default-aware, and `dotlink profile` owns persistent profile mutation. Any externally reachable HTTP is OAuth-protected by default: ngrok is protected automatically, direct non-loopback binds require OAuth + an explicit HTTPS public origin, and only the one-run `--allow-public-no-auth` escape hatch permits unauthenticated public ingress.
- Linux shell execution is Bubblewrap-sandboxed by default. Network is off unless explicitly granted.
- Filesystem denies win over allows. Cache mounts never expand MCP filesystem authority.
- Linux native file IO must stay descriptor-rooted with openat2 and fail closed on unsupported resolution. Linux/macOS native tools are concurrent by default; `-s/--serialize` opts into one-at-a-time MCP calls. Windows retains a conservative operation gate. Never reintroduce pathname IO following authorization for Linux tools.
- Logging is quiet by default: `-v` = TOOL, `-vv` = TOOL + REQ. Interactive non-stdio runs support live `v` cycling plus `Ctrl+R` restart; stdio never shares stdin with runtime controls. Runtime controls must snapshot/restore the exact terminal state on every exit/error path. `--quiet -vv` is REQ-only.
- Linux KVM/GPU grants are independent opt-ins: vendor-check DRM devices, pin character-device FDs and use Bubblewrap `--dev-bind` rather than regular `--bind-fd`; read-only driver resources must not expand native MCP FS grants. NixOS hardware permissions affect only the corresponding systemd unit.
- Keep stdout protocol-clean for stdio; human output goes to stderr.
- **Intentional open concurrency.** Local HTTP and stdio tool requests have no shared admission/concurrency budget by design; the calling model/client directs parallelism. The OpenAI tunnel's own in-flight bound and per-tool byte/time limits remain. **Do not flag the absence of a global admission cap as an accidental omission in reviews** without a concrete independent issue.
- **Unsandboxed Unix shell:** cleanup kills the command's process group; `setsid` descendants may escape. Only Bubblewrap provides the default stronger PID-namespace lifecycle. Do not claim all unsandboxed descendants are contained.

## Plans and parallel work

Plan status and checklists follow `.agents/plans/README.md`; the plan record
lives in `.agents/plans/`. Do not add a private evidence file or a second status
log.

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

<!-- END repo-specific -->
