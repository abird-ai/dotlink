# dotlink-017-backport-2026-11-17 — checklist

Base: `main` at the time the port starts; lane head is `release/0.17`.

Design: [README.md](README.md).

## Uncommitted

- [ ] `aw-tools`: `ProcessExit::Signal(i32)`, fed from `ExitStatus::signal()`
- [ ] `main`: map a signalled, unsuccessful completion to `Signal(n)` instead of `Signal`
- [ ] Reconcile `stopped_by` with `ProcessExit::{Cancelled,DeadlineExceeded,Unknown}` — one representation
- [ ] Keep the single lifecycle line, including failed cleanup and fail-safe `Drop` (see design §3)
- [ ] Share `signal` / `stopped_by` rendering across result surfaces; handoff carries only a handle
- [ ] Port the 0.17.0 tool surface (`background: true`) and 0.17.1–0.17.3 fixes
- [ ] Port `.agents/docs/runtime-health-check.md` and the idle bands

## Tests

- [ ] Render the shell's observed signal by name; distinguish a signal from an explicit exit 137
- [ ] Report `stopped_by` only for an attributed stop; observed leftovers clear `success`, not `exit_code`
- [ ] A signal in the wait status without a stop cause gets `by=external`; never infer one from an exit code
- [ ] An ordinary exit is silent except for a tree observation or cleanup alarm; a finalized job never logs `end=abandoned`
