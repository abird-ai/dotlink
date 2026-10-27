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
- Filesystem denies win over allows. Cache mounts never expand MCP filesystem authority. Deny and protected paths retain lexical and kernel-order resolved identities (`mcp::guard_identities`), including dangling-symlink targets; each sandboxed command re-resolves them. Sandboxed shells reject any guarded path in a writable bind unless it lies below an existing non-symlink deny that is the bind's direct child (one rule: `validate_shell_guards`). Masks and deny-write rebinds act on canonical objects; every host bind source is FD-pinned; runtime aliases become sandbox symlinks to one canonical bind. Cache/host/runtime remapping must not bypass denies or protection; deny-write blocks socket/device integrations.
- Linux native file IO must stay descriptor-rooted with openat2 and fail closed on unsupported resolution. Shared traversal errno comparisons use OS-native libc constants, never Linux numeric literals. Linux/macOS native tools are concurrent by default; `-s/--serialize` opts into one-at-a-time MCP calls. Windows retains a conservative operation gate. Never reintroduce pathname IO following authorization for Linux tools.
- Logging is quiet by default: `-v` = TOOL, `-vv` = TOOL + REQ. Interactive non-stdio runs support live `v` cycling plus `Ctrl+R` restart; stdio never shares stdin with runtime controls. Runtime controls must snapshot/restore the exact terminal state on every exit/error path. `--quiet -vv` is REQ-only. All log scopes and network metadata labels are bounded/sanitized; grep patterns are payloads and only their byte count may be logged. Never include control-plane response bodies or malformed protocol payloads in errors; rmcp tracing stays suppressed regardless of `RUST_LOG` because it renders complete MCP payloads.
- Linux KVM/GPU grants are independent opt-ins: vendor-check DRM devices, pin character-device FDs and use Bubblewrap `--dev-bind` rather than regular `--bind-fd`; read-only driver resources must not expand native MCP FS grants. NixOS hardware permissions affect only the corresponding systemd unit.
- Keep stdout protocol-clean for stdio; human output goes to stderr.
- **Intentional open concurrency.** Local HTTP and stdio tool requests have no shared admission/concurrency budget by design; the calling model/client directs parallelism. The OpenAI tunnel's own in-flight bound and per-tool byte/time limits remain. **Do not flag the absence of a global admission cap as an accidental omission in reviews** without a concrete independent issue.
- **Local work ceilings are separate:** directory discovery visits at most 100,000 entries; grep reads at most 32 MiB across 4,096 candidate files per call and reports incomplete coverage. Paged `read` limits actual source I/O to 32 MiB before its requested line offset, including buffered prefetch. Opt-in `.gitignore` scans fail explicitly above 64 KiB/1024 lines per ignore file or 4 MiB/256 non-absent file reads per walk; failed reads charge partial bytes, and denied rules are not read or charged. Denied/unreadable `.gitignore` rules are distinct from absent files, and surface `ignore_rules`; compatibility `incomplete_reason` reports the first cause while `incomplete_reasons` retains every concurrent cause. The same shared walker flags unreadable subtrees and metadata as `traversal_error`, and grep flags unreadable discovered candidates; explicit filtering remains intentional. Text tools share UTF-8/binary-control classification; byte-oriented work belongs to the binary tools.
- **Unsandboxed Unix shell:** cleanup kills the command's process group; `setsid` descendants may escape. Only Bubblewrap provides the default stronger PID-namespace lifecycle. Do not claim all unsandboxed descendants are contained.
- **Failure recovery:** `recovery::Recovery` is the one failure taxonomy (`Runtime` in-process restart, `Process` default, `Operator` = exit `CONFIG`/78, `Unconfigured` = exit `NOTCONFIGURED`/6, which is not a failure: first-run setup is still pending, so a supervisor stops cleanly). Classify at the source and never by elimination. `Operator` covers planning content (`recovery::config_failure`; I/O-caused planning errors restart), sandbox policy the shell cannot enforce and out-of-range limits (tagged in `LocalMachine::new`), and OpenAI key/tunnel rejections that persist for the confirmation window (60 s); one 401/403/404 keeps retrying. The process-wide `openai::RejectionLedger` keeps that window across in-process runtime restarts until an accepted poll or a changed tunnel identity. Unreadable or absent explicit credential files restart. Unclassified or lost tags restart the process; cleanup failures override the primary class; one-shot commands and clap usage errors keep their conventional exit codes (1 and 2). Under NixOS, systemd 255 or newer uses `Restart=always` plus `RestartPreventExitStatus=2 CONFIG NOTCONFIGURED` and `SuccessExitStatus=NOTCONFIGURED`: exit 0, clean signals and all failures except 2/78 restart after 2 s doubling to 10 min, while an unconfigured service goes inactive as a success, and explicit stop/restart jobs remain authoritative. There is no start fuse; OOM handling stays at the systemd default. Ten minutes of uptime send `RESTART_RESET=1`; systemd 255–257 warn because only 258 or newer applies it. Ctrl+C and SIGTERM latch once for the process (`shutdown::Shutdown`), so a signal is never lost across runtime restarts. Supervisor variables are captured and removed before threads start; credential variables are hidden from every child via `credentials::hide_from_child`.
- **Credential lifecycle:** --print-id and --list-tools are configuration-only: neither may read Runtime keys, initialize transports, host services or private state. Resolve explicit env/file credentials before reading a saved Runtime key; active file credentials are read once and carry their descriptor-verified protected target into runtime construction. Never open dormant OpenAI/ngrok credential files. Active saved credentials fail closed when unusable, but explicit overrides and profile management/replacement remain recoverable. Credential generations are pruned only after durable rotation, retaining selected/prior generations through unrelated edits; interrupted deletion remains retryable. A running OAuth server selects the current committed owner generation on each password check, including a post-verify selection recheck; deleting a saved profile never falls back to legacy. Token/code validation precedes consumption; authorization-code redemption uses a short claim without holding its mutex across durable I/O, and temporary store failures return retryable HTTP 503 without burning the code. Argon2 workers retain permits after request cancellation until their uncancellable OS work exits; CIMD uses cancellable async system DNS, so timeout/cancellation releases its full lease immediately; missing system DNS disables CIMD without blocking registered-client OAuth. Public token exchanges have bounded admission and never hold the access-token mutex during durable state work; access-token poison recovery is consistent across publication/validation/revocation, and revocation has no anonymously exhaustible rolling-minute quota. A version-checked read-only index of refresh grants and registered DCR clients rejects definite unknown grants before expensive state work without rejecting fresh cross-process grants; contention/version races fall through to authoritative work, while stable malformed state returns retryable unavailability without repeated parsing. Public OAuth client IDs have separate DCR and HTTPS CIMD namespaces; public errors never expose private control-plane file paths or parser details. Private control-plane files use symmetric read/write ceilings through `bounded` (OAuth 16 MiB, profiles 4 MiB, credential files 16 KiB); serialize and validate before replacement, and reject Unix special files without blocking. Windows OAuth state cache invalidation uses native file identity, not timestamps alone. Pending OAuth clients/authorizations use shared non-evicting admission with every redirect/client share, a four-entry requester-within-redirect/client share, an eight-entry overall trusted-requester share, and explicit 429s. Trusted requester identity comes only from the direct TCP peer or authenticated ngrok tunnel metadata: ports are ignored, mapped IPv6 normalizes to IPv4, IPv6 groups by /48, loopback is one local identity, and forwarded headers remain untrusted. Owner consent POST claims remain reserved through async verification and automatically release on cancellation; password work admits six rolling-minute starts per trusted requester under two global workers, charges both consent-creation and POST source identities, and never evicts active requester history; terminal success/denial or expired/full DCR promotion consumes exactly once, while transient storage failures preserve the claim. Authorization-code, access-token and refresh-grant capacity prunes only expired entries and never evicts live grants; access tokens also have a per-client share, failed issuance preserves the original credential, and an authoritatively revoked DCR client consumes its terminal code. Untrusted client display names neutralize control, line-separator and bidi characters before UI/terminal output. CIMD admits at most two in-flight and 12 rolling-minute starts per requester plus four process-wide; its bounded requester table never evicts live history, and private literal IPs are rejected before admission. Pending owner authorization retries share one bounded insertion/recovery path. One outer Host allowlist covers MCP and every OAuth route, normalizes mapped IPs, returns 400 for malformed/conflicting authorities, and trusts only exact configured public/listener authorities. SEP-2243 HTTP/OpenAI calls reject unknown policy tool names before rmcp's negative schema cache; network preflight shares the exact request-body ceiling. Ngrok connections are served directly with their authenticated edge peer metadata; no forwarding header or loopback proxy establishes identity. HTTP route futures cancel first, accepted IO force-closes at the drain bound, and bounded ngrok tunnel/session close completes before restart; a drain timeout is a cleanup failure, and cleanup failure takes precedence over restart requests to prevent replacement-runtime overlap. Shell progress notification backpressure must not cross process deadlines.

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
