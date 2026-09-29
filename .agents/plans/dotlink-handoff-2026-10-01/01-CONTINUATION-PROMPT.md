# Ready-to-paste continuation prompt

Use this prompt when handing the repository to another agent:

---

You are taking over the `abird-ai/dotlink` repository.

Use the connected dotlink repository tool to access the live tree. **Do not reset, clean, checkout over, discard, or rewrite unexpected working-tree changes.**

Before doing any work, read:

```text
.agents/plans/START_HERE.md
.agents/plans/dotlink-handoff-2026-10-01/00-START-HERE.md
.agents/plans/dotlink-handoff-2026-10-01/HANDOFF.md
```

Then follow the complete read order in `00-START-HERE.md`.

Before making changes, inspect:

```bash
git status --short --branch
git log --oneline --decorate -12
git diff --stat
git diff --check
```

Authority order:

1. current Rust source;
2. current Cargo/Nix/scripts/workflow;
3. current README/SECURITY/ARCHITECTURE/CHANGELOG;
4. current agent docs;
5. the 2026-10-01 handoff;
6. history/older plans.

The 2026-09-30 handoff is historical-only and must not override current source.

Current implementation baseline described by the handoff:

```text
657ebb3 Harden OAuth and runtime safety after review
8f94c87 Add OAuth-protected remote MCP access
```

Current product/repository expectations:

- human title: **abird dotlink**;
- binary/crate/MCP identity: `dotlink`;
- crate version: `0.6.0`;
- strict profile schema: `10`;
- canonical upstream: `https://github.com/abird-ai/dotlink`;
- Abird-owned XDG namespace: `abird/dotlink`;
- public ngrok and direct non-loopback HTTP are OAuth-protected by default;
- unsafe public no-auth is one-run-only via `--allow-public-no-auth`;
- Linux shell is Bubblewrap-sandboxed by default;
- filesystem allow/deny policy is shared by all transports;
- developer caches are shell-only mounts and do not expand MCP filesystem access;
- OpenAI Tunnel, stdio, HTTP/ngrok all share one LocalMachine policy domain;
- OAuth is embedded, single-owner, PKCE S256, exact-resource-bound, CIMD-first with DCR fallback;
- OAuth persistent state uses private XDG state files and cross-process locked read/modify/write;
- `release-all` cross-builds static Linux x86_64/ARM64, Windows x86_64/ARM64, and macOS ARM64 from x86_64 Linux;
- GitHub `v*` tags create/update Releases with individual binary/checksum assets;
- installers resolve the latest Release by default and are idempotent updaters.

The current 0.6.0 implementation phase is complete and comprehensively validated. Do not invent more implementation work just because an older roadmap lists it.

If continuing development, first identify a concrete user request or demonstrable issue from the current tree. Preserve the existing least-privilege and composability invariants unless the user explicitly changes product direction.

When changing security-sensitive behavior, add regression tests before claiming completion. Re-run the appropriate Rust/Nix/release validation described in:

```text
.agents/plans/dotlink-handoff-2026-10-01/11-VALIDATION-AND-KNOWN-LIMITATIONS.md
```

Do not push unless the user explicitly requests it.

---
