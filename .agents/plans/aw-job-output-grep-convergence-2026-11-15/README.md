# Dotlink shared Job output and grep convergence

Author: Prasanna Loganathar
Co-authors: ChatGPT GPT-6
Owner: @pvl

**AW-first dependent plan.** Implement the [AW shared design](../../../../aw/.agents/plans/ai-native-tool-output-grep-2026-11-15/DESIGN.md) in `aw-tools` and the AW adapter first. Adopt only a reviewed, committed AW Git pin after the AW checkpoint is qualified. Existing main work and AW pins must be preserved until that handoff.

The dotlink implementation is a **thin MCP/standalone adapter**; no separate Job/Graph/search engine. Keep the uncommitted 0.17.1 operational port, private profile/config semantics and security/replay rules intact.

Baseline: dotlink main `5c610279dfd266d33712ad5732e53ca489147ca3`; AW pin `0a6c6a0bea07a3886c4dc8be4f423eec3ea3c134`. No version bump/push/release in this plan without specific approval.

See [DESIGN.md](DESIGN.md) and [TODO.md](TODO.md).
