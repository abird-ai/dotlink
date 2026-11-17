# Dotlink shared Job output and grep convergence

Author: Prasanna Loganathar
Co-authors: ChatGPT GPT-6
Owner: @pvl

**Current cross-repo continuation (2026-11-17):** [dotlink next-agent handoff](../../docs/DOTLINK-AW-TOOLS-NEXT-AGENT-HANDOFF-2026-11-17.md) and [canonical full AW–dotlink handoff](../../../../aw/.agents/docs/AW-DOTLINK-TOOLS-FULL-COMPREHENSIVE-HANDOFF-2026-11-17.md). Read these before D0–D4; historical tests remain evidence but live source and current plan TODO are authority.

**AW-first dependent plan.** Implement the [AW shared design](../../../../aw/.agents/plans/ai-native-tool-output-grep-2026-11-15/DESIGN.md) in `aw-tools` and the AW adapter first. Adopt only a reviewed, committed AW Git pin after the AW checkpoint is qualified. Existing main work and AW pins must be preserved until that handoff.

The dotlink implementation is a **thin MCP/standalone adapter**; no separate Job/Graph/search engine. Keep the uncommitted 0.17.1 operational port, private profile/config semantics and security/replay rules intact.

Historical baseline: dotlink main `5c610279dfd266d33712ad5732e53ca489147ca3`; initial AW pin `0a6c6a0bea07a3886c4dc8be4f423eec3ea3c134`. Published dotlink checkpoint `d0a392234e918bd4ec9f0fbd7ab6fe771e1eb696` adopted AW `be6d48d`; the finalized D1–D4 checkpoint advances all three shared dependencies/lock records to reviewed AW writer-quiescence follow-up `beef1bc2d2f37b5660ebe928d368bca37368e608`, completes shared Job output/grep/Task/replay convergence, and includes the D4 guard-revocation discard fix plus independent security review. Markdown, all Nix checks, full native Bubblewrap qualification, and targeted GPU/KVM/device-security smoke are green. D4 is complete; push/version/release/signing/platform publication remain separately authorized actions. No version bump/release in this plan without specific approval.

See [DESIGN.md](DESIGN.md) and [TODO.md](TODO.md).
