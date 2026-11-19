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
signal but not *which*. `release/0.17` now carries the number (`ca7dd5a`), so the
shared type should too: `Signal(i32)`, fed from `ExitStatus::signal()`.

Why it matters: SIGKILL (an external kill) and SIGSEGV (a crash) currently render
identically, and they call for opposite responses. A day of investigation went
into exactly that ambiguity.

Both trees must agree afterwards: `main`'s mapping in `src/jobs.rs`
(`None if success => Code(0)`, `None => Signal`) becomes `Signal(signal)`.

### 2. Stop cause — `stopped_by`

`release/0.17` adds `stopped_by` (`timeout` | `cancel` | `shutdown` | `failed`,
where `shutdown` means the connector withdrew the work rather than the caller) and
it is absent when the command decided its own fate, including after a sweep of its
leftovers — a sweep is a journal line, not a claim — alongside
`signal`. Combined with `ProcessExit`'s own `Cancelled` / `DeadlineExceeded`
variants on `main`, this is one concept expressed twice; the port should keep a
single representation, not both. `main`'s `Unknown` case maps onto "dotlink did
not stop it", which is the same information as an absent `stopped_by`.

### 3. Stop log line

`release/0.17` emits at most one
`LOG job id=<id> end=<what> [by=<who>] [tree=<what became of the group>]` line per
job. It is a lifecycle line, so it survives every verbosity: for it to be evidence,
absence must mean the command simply ended. `end=` comes from the terminal the runner
reached (`exit code=N`, `signal SIGKILL`, `timeout`, `cancel`, `shutdown`, `error`,
`abandoned`), and `by=` names another actor only when there was one (`dotlink`,
`caller`, `connector`, `external`) — a command's own ending names none. `tree=killed`
or `tree=alive` rides on the same line when the command left its group running and
dotlink swept it. One line, because two lines each needing the other to be read is
how a record stops being evidence.

The port must keep the writer single: the completion writes it, and the fail-safe
`Drop` writes `end=abandoned` only when no completion exists — every completion path
disarms the guard, so a job is never written twice.

### 4. Result fields

`signal` and `stopped_by` are rendered through one helper at every surface that
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
- Re-verify with a real kill rather than by reasoning: an inner `kill -9` in a
  sandbox exits `128+9`, while a kill of the sandbox launcher itself yields no
  exit code at all. Only the launcher's own status proves a signal.

## Mapping onto `main`

`aw-tools`' `ProcessExit` conflates exit shape (`Code`, `Signal`, `Unknown`) with
cause (`Cancelled`, `DeadlineExceeded`). Port it as `Code(i32)` / `Signal(i32)` /
`Unknown`, and keep `stopped_by` as the orthogonal cause field, deriving
`Cancelled`/`DeadlineExceeded` from job *status alone*: `stopped_by` means "we
delivered a signal", so a deadline whose kill found an empty group has none and
would be downgraded to `Unknown` by deriving from it. Carrying both
spellings would be the duplication this plan already warns about.
