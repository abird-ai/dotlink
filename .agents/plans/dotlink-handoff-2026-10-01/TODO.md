# dotlink-handoff-2026-10-01 — checklist

Plan `DL-060`. Implementation is complete; the open items below are operational,
external, or optional maintenance work. Rules and glyphs: `../TODO-SPEC.md`.

Agent scratch lives in `.agents/runs/dotlink-handoff-2026-10-01-<agent>/TODO.md`
and is appended here in one atomic write when finished.

## Operational (owner or external action required)

- [ ] **DL-061** Push the rewritten history to `origin/main` — requires explicit owner go-ahead — since `v0.9.0`
- [ ] **DL-062** Publish the single release tag and GitHub Release — build `release-all`, validate, attach each platform asset and checksum — since `v0.9.0`
- [!] **DL-063** Refresh the live ChatGPT MCP App tool schema after deployment — external service — since `v0.9.0`
- [!] **DL-064** Claude.ai remote OAuth interoperability smoke against a stable ngrok domain — external service — since `v0.9.0`

## Hardening (in repository, optional)

- [ ] **DL-065** Native Windows CI runtime smoke (`dotlink.exe --version`, PowerShell installer) — keep the Nix build graph authoritative — since `v0.9.0`
- [ ] **DL-066** Native macOS ARM64 runtime smoke — since `v0.9.0`
- [ ] **DL-067** Release signatures, SBOM, provenance, and notarization on top of the reproducible Nix + checksum flow — since `v0.9.0`

## Structural (optional, maintenance only)

- [ ] **DL-068** Split `oauth.rs`, `mcp.rs`, and `setup.rs` by responsibility — one module at a time, no behavior change, tests green after each move — since `v0.9.0`

## Known limitations (tracked, not open gates)

The authoritative list is `11-VALIDATION-AND-KNOWN-LIMITATIONS.md`. These are
accepted boundaries of the current design, not unfinished work.
