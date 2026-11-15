# Dotlink shared Job output + grep convergence

Base dotlink: `5c610279dfd266d33712ad5732e53ca489147ca3`
AW initial pin: `0a6c6a0bea07a3886c4dc8be4f423eec3ea3c134`
Dependency: [AW plan](../../../../aw/.agents/plans/ai-native-tool-output-grep-2026-11-15/README.md).

## Committed

None.

## Uncommitted

- [ ] D0. AW A–G shared checkpoint qualified and approved Git pin available; preserve existing pins until then.
- [ ] D1. Dotlink MCP Job get/read/ack adapters use AW native tail/range/finish APIs; no second owner.
- [ ] D2. Shared grep file/dir and 0..128 context input/schema parity across direct and Graph.
- [ ] D3. Compact output/limit and full transport response-window/replay/Task-owner integration.
- [ ] D4. Grouped qualification, independent security review for changed replay/ACK, write handoff and checkpoint.

## AW dependency checkpoint — 2026-11-15

AW shared grep/output owner primitives and initial AW Job get/read/ACK projection are uncommitted and only partially qualified. C (wait/notifications), D (live AW end-to-end authority/Graph/ACK) and F/G catalog/checkpoint remain pending. **D0 is blocked until AW has a reviewed committed and available shared pin.** Do not alter the current dotlink AW pins or runtime while waiting for that checkpoint.

## Tests and verification

- [ ] Default `jobs_get` 512 aggregate (256/256 and unused redistribution), explicit 0/512/1024, running/settled/Graph node/root and current disclosure.
- [ ] Signed read + raw continuation, prefix vs terminal Finish, replay/Task lease, unknown/revoke, no premature cleanup.
- [ ] End-to-end grep direct-file/directory + Graph, context 0/128/129, output/incomplete/pagination.
- [ ] All-feature Rust targeted integration/strict Clippy, formatting and affected release gates.
- [ ] Preservation of existing operational dirty tree, unrelated plugin doc and release reference; no unauthorized push.
