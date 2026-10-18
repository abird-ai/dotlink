# Checklist tracking spec

Work is tracked as hand-edited markdown checklists. The plan `TODO.md` is the
source of truth, the commit is the evidence, and a user-accepted checkpoint is
the proof of record. Read this before opening a plan.

## Plan layout

A plan is a directory under `.agents/plans/<plan>/`. `TODO.md` is required; the
optional `README.md` is the single home for everything else:

- `README.md` — the plan details: what it is and why it exists, plus goals,
  scope, and links, and sections as needed:
  - **Design** — interfaces, invariants, alternatives considered, migration and
    rollback.
  - **Decisions** — user decisions that bind the plan, with their rationale, so
    agents follow them instead of re-litigating settled questions.
  - **References** — research and source references: papers, specs, prior art,
    source files, benchmarks, with enough provenance to reproduce the reasoning.

  Open it with the plan's authorship: when human-led, `Author: <user>` plus
  `Co-authors: <comma-separated AI models used>`; when AI-led,
  `Author: <the AI models only>`. Keep it one file while it stays readable. When
  a section grows large, split it into `DESIGN.md`, `DECISIONS.md`, or
  `REFERENCES.md` beside `README.md` and link back from `README.md`.
- `TODO.md` — the detailed todo: implementation, tests, verification, gates, and
  evidence. Required, and it needs to be extensive — every step, test, and gate
  gets its own item. Completed work is grouped under the commit that landed it;
  work without a commit yet goes under `## Uncommitted`.

```text
.agents/plans/TODO.md                      index: one checkbox and link per plan
.agents/plans/<plan>/README.md             plan details (optional)
.agents/plans/<plan>/DESIGN.md             detailed architecture (optional)
.agents/plans/<plan>/DECISIONS.md          user decisions (optional)
.agents/plans/<plan>/REFERENCES.md         research and source references (optional)
.agents/plans/<plan>/TODO.md               the plan's implementation checklist
.agents/runs/<plan>-<agent-id>/TODO.md     one agent's working checklist (in flight)
```

Keep plans flat: a plan is a top-level directory under `.agents/plans/`, not
nested inside another, and one flat plan per effort is preferred while its
`TODO.md` stays tractable. Only an extremely complex or large design splits into
smaller plans that stand on their own — added as sibling plan directories, with
the larger plan's `TODO.md` referring to them by link instead of carrying every
item.

`.agents/runs/` is git-ignored scratch. Delete the run directory once its
finished lines have been appended to the plan checklist.

## TODO shape

```markdown
# TODO — <plan>

Status: <active | done | blocked>
Provenance: <base-sha>            # commit the plan started from
Accepted checkpoint: <sha>        # user-accepted snapshot, when closed

## Committed
<!-- grouped by the commit that landed each set; newest last -->
### `<sha>` — <what this set landed>
- [x] <item>

## Uncommitted
<!-- the working list; the only place new work appears -->
- [ ] <item> — <owner / reason>
- [~] <item> — in progress
- [!] <item> — blocked outside this repository

## Tests and verification
<!-- every test and gate the plan must pass; all [x] before Status: DONE -->
- [ ] <test or gate> — <command / evidence>

## Deferred
<!-- optional; accepted, fail-closed, with the reason -->
- [ ] <item> — <reason>
```

An optional bold id may prefix an item for plans that carry one:

```text
- [x] **DL-046** descriptor-rooted openat2 file IO — passed on `<sha>`
```

## The rule

Tick an item only after its checks pass, in the same session, on the commit you
are marking. The check is the evidence; no separate artifact is required for an
agent to have checked it. Completed work is grouped under a `### <sha>` heading;
the heading is the provenance for every item beneath it.

`## Tests and verification` lists every test and gate the plan must pass, each
with the command or evidence that proves it. A plan is not done while any of
these is open; all must be `[x]` on the closing commit.

Glyphs: `[x]` done · `[~]` in progress · `[ ]` not started · `[!]` blocked
outside this repository.

## Rules

- Checklists are hand-edited. A generated checklist that rejects editing is a
  report, not a TODO.
- Invariant and contract gates stay executable scripts. The canonical gate list
  is `AGENTS.md` → "Validation before commit". A checklist may name and
  require a gate; only a script enforces it.
- Flow: an agent works in `.agents/runs/<plan>-<agent>/TODO.md`, appends finished
  lines to `.agents/plans/<plan>/TODO.md` in one atomic write, then deletes its
  run directory. Closing a plan flips one line in `.agents/plans/TODO.md`.
- Never mark an external gate done from authority alone.
- Never enable later phases.
- Never record a pass that was not run.
