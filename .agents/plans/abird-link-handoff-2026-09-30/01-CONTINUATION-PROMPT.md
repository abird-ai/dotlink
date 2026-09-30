# Ready-to-paste continuation prompt

Paste this to the next coding agent:

---

You are continuing the `abird-link` Rust project from its committed 2026-09-30 implementation state.

First inspect the live repository:

```bash
git status --short --branch
git log --oneline --decorate -12
git diff --stat
git diff --check
```

Do not reset, clean, checkout over, or discard unexpected working-tree changes.

Then read, in order:

1. `.agents/plans/abird-link-handoff-2026-09-30/00-START-HERE.md`
2. `.agents/plans/abird-link-handoff-2026-09-30/HANDOFF.md`
3. `.agents/plans/abird-link-handoff-2026-09-30/02-CONTEXT-AND-HISTORY.md`
4. `.agents/plans/abird-link-handoff-2026-09-30/03-CURRENT-STATE.md`
5. `.agents/plans/abird-link-handoff-2026-09-30/04-PRODUCT-AND-ARCHITECTURE-SPEC.md`
6. `.agents/plans/abird-link-handoff-2026-09-30/05-CLI-CONFIG-ONBOARDING-SPEC.md`
7. `.agents/plans/abird-link-handoff-2026-09-30/06-SECURITY-SANDBOX-SPEC.md`
8. `.agents/plans/abird-link-handoff-2026-09-30/07-MCP-TOOLS-TRANSPORTS-LOGGING.md`
9. `.agents/plans/abird-link-handoff-2026-09-30/08-BUILD-RELEASE-INSTALL-SPEC.md`
10. `.agents/plans/abird-link-handoff-2026-09-30/09-DECISION-LOG.md`
11. `.agents/plans/abird-link-handoff-2026-09-30/10-VALIDATION-KNOWN-ISSUES.md`
12. `.agents/plans/abird-link-handoff-2026-09-30/11-NEXT-WORK-ROADMAP.md`
13. `.agents/plans/abird-link-handoff-2026-09-30/12-FILE-MAP.md`

Read current source before modifying:

```text
src/main.rs
src/setup.rs
src/mcp.rs
src/logging.rs
src/transports/{mod,openai,stdio,http}.rs
flake.nix
Cargo.toml
README.md
SECURITY.md
ARCHITECTURE.md
CHANGELOG.md
```

Important current product intent:

- `abird-link` securely connects ChatGPT, Claude.ai, and other MCP clients to local files/tools.
- Least privilege by default.
- Pi-like text tools plus explicit binary tools.
- Linux shell is Bubblewrap-sandboxed by default.
- Deny rules take precedence wherever enforceable.
- OpenAI Tunnel, stdio, Streamable HTTP, and optional ngrok share one policy engine.
- Profiles use JSONC and can persist cwd/path grants, shell, sandbox network and typed cache grants.
- Shared developer caches are shell-only and do not expand MCP filesystem tool roots.
- Normal tool activity logging is timestamped/default; `-s` hides it; `-v` adds developer request logging; `--color` controls ANSI.
- Full unsandboxed host authority is intentionally paired as `--allow-all --no-sandbox`.
- `--allow-rw=/` means filesystem-wide RW while preserving the normal Linux sandbox model.
- Crane dependency caching, native checks, Linux-musl release builds, Windows-GNU release builds, checksums, release helper, Unix installer and PowerShell installer logic were all validated on 2026-09-30.

Key implementation commits from this pass:

```text
b76f8e6 Expand profiles, sandbox caches, and logging
7b93e92 Add Crane cross builds and release installers
```

The main remaining operational task is to restart the live `abird-link` process used by ChatGPT and refresh the developer connection/tool schema if the client must be proven against the final binary. Future product candidates are listed in `11-NEXT-WORK-ROADMAP.md`.

Before committing future work, run the strongest relevant validation in `10-VALIDATION-KNOWN-ISSUES.md`.

---
