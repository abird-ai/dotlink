# dotlink-017-backport-2026-11-17

Bring the `release/0.17` lane onto `main`, which has diverged in both directions.

## Why this is a plan and not a merge

`main` already carries work the lane does not: the `aw-tools` process layer
(`ProcessExit`, `ProcessOutputMeta`) imported at `src/jobs.rs` as
`SharedProcessExit`, and the surrounding refactors. The lane carries work `main`
does not: the 0.17.0 tool surface (`background: true`), the 0.17.1–0.17.3 fixes,
and the two changes below from 2026-11-17. A plain merge would drop one side, so
each item is ported deliberately.

## Items to pick up when the lane lands on `main`

### 1. Signal identity — `ProcessExit::Signal` has no payload

`aw-tools`' `ProcessExit::Signal` records *that* the waited process died by a
signal but not *which*. `release/0.17` keeps the number internally (`ca7dd5a`) and
renders it as a name — `signal: "SIGKILL"`, never `signal: 9` — so the shared type
should carry `Signal(i32)` fed from `ExitStatus::signal()` and convert at the edge.
One representation on the wire: do not add a second numeric field beside the name.

Why it matters: SIGKILL (an external kill) and SIGSEGV (a crash) currently render
identically, and they call for opposite responses. A day of investigation went
into exactly that ambiguity.

Both trees must agree afterwards: `main`'s mapping in `src/jobs.rs`
(`None if success => Code(0)`, `None => Signal`) becomes `Signal(signal)`.

### 2. Stop cause — `stopped_by`

`release/0.17` adds `stopped_by` (`timeout` | `cancel` | `shutdown` | `failed`,
where `shutdown` means the connector withdrew the work rather than the caller) and
it is absent when the command decided its own fate. A sweep of its leftovers
never sets `stopped_by` or changes `signal`. Combined with `ProcessExit`'s own `Cancelled` / `DeadlineExceeded`
variants on `main`, this is one concept expressed twice; the port should keep a
single representation, not both. `main`'s `Unknown` case maps onto "dotlink did
not stop it", which is the same information as an absent `stopped_by`.

### 3. Stop log line

Preserve the lane's [lifecycle line](../../../README.md#logging), not a log of
signal attempts. An ordinary exit without tree facts is silent. `tree=leftovers` records live work at cleanup and clears `success` without changing the exit code or failing the tool. An attributed
cancel names `caller` or `connector`; a signal in the waited process's status
with no dotlink stop names `external`. Never infer a signal from exit 128+N.

The completion writes the line with `cleanup=kill-failed` only if the tree kill
is still unconfirmed after one immediate retry. Hold the Unix leader waitable through that last group signal; never signal its PGID after reaping. A Windows exit other than the private termination marker rules out dotlink’s kill. Disarm the guard before publishing;
`Drop` writes `end=abandoned` only when no completion exists.

### 4. Result fields

`tree`, `signal`, and `stopped_by` are rendered through one helper at every surface that
can carry them: the ordinary result, `job_get`'s status payload, the
internal-failure envelope (`streams_omitted` instead of the streams, and only the
context keys that exist), and a failed native task, which carries the facts in
the error object's `data` because `aw-tools`' task payload has no field for them.
The context-less result is the deliberate exception — no execution context means
no runner, so it has no facts to render and asserts that. The
compact handoff receipt deliberately stays thin: it carries a handle, not a
process verdict. Keep that set closed — each new surface is a place they can
disagree. Keep one renderer after the port. A
signalled result with no stop cause also sets the surface's prose field —
`next` on a result, `error` on `job_get`, whose `next` is the follow-up chain —
naming the signal so the reader is not left with a bare number; that wording
belongs with the fields, not in a separate explanation path. An internal failure
still reports the status the command was reaped with (`exit_code`, `signal`), so
a spool failure does not hide the command's own outcome.

## Rules for the port

- The lane's behaviour is the reference for shell/job semantics; `main`'s process
  layer is the reference for its own abstractions. Do not keep two mechanisms.
- No result growth beyond these fields: payload bytes ship on every call.
- Re-verify in both modes: a shell killed by SIGKILL reports a signal; an explicit
  `exit 137` never does. A normal launcher exit must corroborate the sandbox report.
  If a stop interrupts the launcher, preserve the inner status when independently
  observed; never treat a partial scan as a clean tree.

## Mapping onto `main`

`aw-tools`' `ProcessExit` conflates exit shape (`Code`, `Signal`, `Unknown`) with
cause (`Cancelled`, `DeadlineExceeded`). Port it as `Code(i32)` / `Signal(i32)` /
`Unknown`, and keep `stopped_by` as the orthogonal cause field, deriving
`Cancelled`/`DeadlineExceeded` from job *status alone*: `stopped_by` means "we
delivered a signal", so a deadline whose kill found an empty group has none and
would be downgraded to `Unknown` by deriving from it. Carrying both
spellings would be the duplication this plan already warns about.
