# dotlink-017-backport-2026-11-17 — checklist

Base: `main` at the time the port starts; lane head is `release/0.17`.

Design: [README.md](README.md).

## Uncommitted

- [ ] `aw-tools`: `ProcessExit::Signal(i32)`, fed from `ExitStatus::signal()`
- [ ] `main`: map a signalled, unsuccessful completion to `Signal(n)` instead of `Signal`
- [ ] Reconcile `stopped_by` with `ProcessExit::{Cancelled,DeadlineExceeded,Unknown}` — one representation
- [ ] Keep the `LOG job stop id=<id> reason=<reason> outcome=<...>` line on the real signal path, including `Drop`
- [ ] Keep the single renderer for `signal` / `stopped_by` across sync, handoff and `job_get`
- [ ] Port the 0.17.0 tool surface (`background: true`) and 0.17.1–0.17.3 fixes
- [ ] Port `.agents/docs/runtime-health-check.md` and the idle bands

## Tests

- [ ] Signalled launcher reports the signal number; a crash and a kill differ
- [ ] dotlink stops report a reason and never a signal
- [ ] An externally killed job reports `signal` with no stop cause
- [ ] Stop line appears for a connector stop and is absent for an external kill
