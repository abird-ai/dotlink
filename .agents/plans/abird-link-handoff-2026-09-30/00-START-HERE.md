# abird-link handoff — START HERE

Date: 2026-09-30

This directory is the canonical handoff bundle for the completed 2026-09-30 `abird-link` development pass.

## First instruction

Inspect the repository before changing it:

```bash
git status --short --branch
git log --oneline --decorate -12
git diff --stat
git diff --check
```

Do not reset, clean, checkout over, or discard unexpected future working-tree changes. The large development phase that originally motivated this handoff has now been reviewed, validated, and committed.

The key implementation commits produced by the continuation are:

```text
b76f8e6 Expand profiles, sandbox caches, and logging
7b93e92 Add Crane cross builds and release installers
```

The documentation/handoff commit follows those commits. Use `git log` as the authority for the current HEAD.

## Read in this order

1. `HANDOFF.md` — comprehensive single-document handoff.
2. `01-CONTINUATION-PROMPT.md` — ready-to-paste continuation prompt.
3. `02-CONTEXT-AND-HISTORY.md` — original design, evolution, and empirical issues that shaped decisions.
4. `03-CURRENT-STATE.md` — current committed implementation and validation state.
5. `04-PRODUCT-AND-ARCHITECTURE-SPEC.md` — product goals and intended architecture.
6. `05-CLI-CONFIG-ONBOARDING-SPEC.md` — CLI, JSONC profiles, onboarding and precedence.
7. `06-SECURITY-SANDBOX-SPEC.md` — filesystem policy, Bubblewrap, NixOS, caches, secrets.
8. `07-MCP-TOOLS-TRANSPORTS-LOGGING.md` — tool surface, transports, logging.
9. `08-BUILD-RELEASE-INSTALL-SPEC.md` — Crane, cross builds, artifacts and installers.
10. `09-DECISION-LOG.md` — major decisions and rationale.
11. `10-VALIDATION-KNOWN-ISSUES.md` — completed validation and residual limitations.
12. `11-NEXT-WORK-ROADMAP.md` — remaining live-client/future work.
13. `12-FILE-MAP.md` — source/docs ownership map.

## Source of truth priority

When documents disagree, use this priority:

1. Current Rust source under `src/`.
2. Current `README.md`, `SECURITY.md`, `ARCHITECTURE.md`.
3. Current `CHANGELOG.md`.
4. This handoff bundle.
5. Git history / older commits.

## Repository identity

```text
product / binary / MCP server: abird-link
crate version: 0.5.0
config schema version in source: 8
branch: main
```

Canonical upstream: `https://github.com/abird-ai/abird-link` (Git remote `origin`).
