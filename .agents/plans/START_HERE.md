# dotlink agent handoff — canonical entry point

Plan status lives in the checklists:

```text
.agents/plans/TODO.md          one line per plan
.agents/plans/TODO-SPEC.md     the tick rule and line format
.agents/plans/dotlink-handoff-2026-10-01/TODO.md   the current plan's checklist
```

Tick an item only after its checks pass, in the same session, on the commit you
are marking. See `TODO-SPEC.md` before editing any checklist.

The comprehensive design/state handoff is:

```text
.agents/plans/dotlink-handoff-2026-10-01/00-START-HERE.md
```

Read that file first and follow its authority/read order.

The older `.agents/plans/dotlink-handoff-2026-09-30/` package is a historical
record only. It describes important design history, but it predates dotlink
0.6.0, schema v10, embedded OAuth, stable ngrok domains, the complete
Linux/Windows/macOS release graph, and the final safety review. Do not use it as
current-state authority.
