# Checklist tracking spec

Replace handoff prose with checklists. Three levels and one rule.

## Layout

- `.agents/plans/TODO.md` — one line per plan.
- `.agents/plans/TODO-SPEC.md` — this file.
- `.agents/plans/<plan>/TODO.md` — the plan's checklist.
- `.agents/runs/<plan>-<agent>/TODO.md` — one working agent's scratch checklist.

`.agents/runs/` is git-ignored scratch. Delete the run directory once its
finished lines have been appended to the plan checklist.

## Line format

```text
- [~] **DL-046** Short title — 0/7 checks — since `<tag-or-sha>`
```

Glyphs:

- `[x]` done
- `[~]` in progress
- `[ ]` not started
- `[!]` blocked outside this repository

## The rule

Tick an item only after its checks pass, in the same session, on the commit you
are marking. The check is the evidence; no separate artifact is required for an
agent to have checked it.

## Rules

- Checklists are hand-edited. A generated checklist that rejects editing is a
  report, not a TODO.
- Invariant and contract gates stay executable scripts. The canonical gate list
  is `.agents/AGENTS.md` → "Validation before commit". A checklist may name and
  require a gate; only a script enforces it.
- Flow: an agent works in `.agents/runs/<plan>-<agent>/TODO.md`, appends finished
  lines to `.agents/plans/<plan>/TODO.md` in one atomic write, then deletes its
  run directory. Closing a plan flips one line in `.agents/plans/TODO.md`.
- Never mark an external gate done from authority alone.
- Never enable later phases.
- Never record a pass that was not run.
