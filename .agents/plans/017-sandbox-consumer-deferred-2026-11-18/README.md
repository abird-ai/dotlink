# 0.17 sandbox consumer — deferred

Author: Prasanna Loganathar
Co-authors: ChatGPT GPT-5.6 Sol
Owner: @pvl

**Status: frozen. Do not implement until explicit user promotion.**

Dotlink-side pointer to the owning shared sandbox design:

- `../../../../aw/.agents/plans/017-sandbox-mount-convergence-2026-11-18/`

When promoted, dotlink consumes AW generic sandbox mount semantics and retains
only:

- config v13 migration;
- profile `mounts`;
- `DOTLINK_MOUNTS`;
- repeatable `--mount`;
- developer-cache discovery;
- product layout/configuration;
- Nix/service integration;
- product diagnostics/docs.

Reference release semantics are the final v0.17.2/v0.17.3 mount state through
`ae4d623 + d855672 + 0983d39`; mount core did not drift through v0.17.4.

No source/config/Nix work belongs here until explicit promotion.
