# abird dotlink handoff — START HERE

Date: 2026-10-01

This directory is the **canonical current handoff** for the `dotlink` repository after the completed 0.6.0 OAuth, security-review, release, installer, and cross-platform validation work.

## First instruction

Do not reset, clean, checkout over, discard, or rewrite unexpected working-tree changes.

Before doing any work:

```bash
git status --short --branch
git log --oneline --decorate -12
git diff --stat
git diff --check
```

Then inspect the current Rust source and current root documentation before relying on handoff prose.

## Implementation baseline

The implementation baseline described by this handoff is:

```text
657ebb3 Harden OAuth and runtime safety after review
8f94c87 Add OAuth-protected remote MCP access
```

At handoff creation:

```text
crate/binary version: 0.6.0
config schema:         10
branch:                main
origin:                https://github.com/abird-ai/dotlink.git
remote main baseline:  b5c9d22
implementation baseline: 2 commits ahead of origin/main before the handoff-doc commit
latest remote tag:     v0.5.0
v0.6.0 tag:            not yet published
```

A documentation/handoff commit may follow `657ebb3`; always use live `git log` as the authority for the exact current HEAD.

## Authority order

When anything disagrees, use this order:

1. Current Rust source under `src/`.
2. Current `Cargo.toml`, `Cargo.lock`, `flake.nix`, `flake.lock`, scripts and workflow.
3. Current `README.md`, `SECURITY.md`, `ARCHITECTURE.md`, `CHANGELOG.md`.
4. Current `.agents/AGENT.md` and `.agents/docs/`.
5. This 2026-10-01 handoff bundle.
6. Git history.
7. The 2026-09-30 handoff bundle and older notes, which are historical only.

Never let an older plan override current source.

## Read in this order

1. `HANDOFF.md` — comprehensive single-file handoff.
2. `01-CONTINUATION-PROMPT.md` — ready-to-paste continuation prompt.
3. `02-CONTEXT-AND-HISTORY.md` — original design and project evolution.
4. `03-CURRENT-STATE.md` — exact current implementation/repository state.
5. `04-PRODUCT-ARCHITECTURE-SPEC.md` — product goals and architecture.
6. `05-CLI-CONFIG-PROFILES-SETUP-SPEC.md` — CLI, schema v10, profiles, setup.
7. `06-SECURITY-SANDBOX-PERMISSIONS-SPEC.md` — local authority and sandbox.
8. `07-OAUTH-REMOTE-HTTP-NGROK-SPEC.md` — embedded OAuth and public ingress.
9. `08-MCP-TOOLS-TRANSPORTS-RUNTIME-LOGGING.md` — MCP surface and runtime.
10. `09-BUILD-RELEASE-INSTALL-CI-SPEC.md` — Nix/Crane/releases/installers.
11. `10-DECISION-LOG.md` — decisions that should not be casually reversed.
12. `11-VALIDATION-AND-KNOWN-LIMITATIONS.md` — final validation and limitations.
13. `12-FILE-MAP.md` — file/module ownership.
14. `13-NEXT-WORK-ROADMAP.md` — optional operational/future work.

## Current product identity

Human-facing title:

```text
abird dotlink
```

Crate, executable, MCP identity, config namespace and release prefix:

```text
dotlink
```

Canonical repository:

```text
https://github.com/abird-ai/dotlink
```

Owned XDG namespace:

```text
abird/dotlink
```

## Completion status

The implementation described here is considered complete for the current 0.6.0 phase.

The latest final validation on 2026-10-01 passed:

- Rust format;
- 137 Rust tests;
- Clippy with `-D warnings`;
- real Bubblewrap runtime/cache smokes;
- shell syntax + ShellCheck;
- Unix installer first/no-op/update/version-mismatch smokes;
- PowerShell installer parse/first/no-op/update/version-mismatch smokes;
- actionlint;
- RustSec audit with **0 vulnerabilities**;
- all-system Nix flake evaluation;
- native Nix package/tests/Clippy/fmt;
- `nix build .#release-all`;
- checksum and binary-format validation for all five published binaries;
- interactive OAuth setup with private permissions;
- live OAuth discovery and protected MCP HTTP boundary.

See `11-VALIDATION-AND-KNOWN-LIMITATIONS.md` for exact details.

## Historical handoff warning

`.agents/plans/dotlink-handoff-2026-09-30/` contains valuable design history but is not current state. In particular it predates:

- version 0.6.0;
- schema v10;
- embedded OAuth;
- CIMD/DCR support;
- stable ngrok domains;
- OAuth state under XDG state;
- public-ingress OAuth defaults;
- cross-process OAuth state locking;
- Linux ARM64;
- Windows ARM64;
- macOS ARM64 Linux-host cross builds;
- GitHub tag-to-release automation;
- latest-release self-updating installers;
- the final filesystem/symlink/concurrency safety review.

Use it only when the new handoff explicitly points to historical rationale.
