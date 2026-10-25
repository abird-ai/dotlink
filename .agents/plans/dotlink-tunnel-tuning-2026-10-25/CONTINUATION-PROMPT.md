# Continuation prompt — tunnel tuning

Paste this to the implementing agent.

---

You are implementing the `dotlink-tunnel-tuning-2026-10-25` plan in
`/home/pvl/spaces/abird/src/dotlink`. Work only in that repository.

## Read first

1. `AGENTS.md` — repository rules (owned changes only, terse comments, signed
   commits, no pushes without explicit confirmation).
2. `.agents/plans/dotlink-tunnel-tuning-2026-10-25/README.md` — the design.
3. `.agents/plans/dotlink-tunnel-tuning-2026-10-25/TODO.md` — the checklist you
   are filling in.
4. `src/transports/openai.rs` — constants at `:24-32`, client construction
   `:353-366`, poll loop `:380-460`, `poll_once` `:478-520`, `post_terminal`
   `:700+`, `apply_response_window` and `retry_delay` `:888-1056`.

## State to establish before editing

- `git status` and `git diff` — DL-080 (tool surface v2) has landed, so
  `src/transports/openai.rs` already contains progress heartbeats and
  `apply_response_window`; the tree should be clean apart from this plan
  directory. Never revert or reformat work you did not make.
- If another agent has uncommitted edits in `src/transports/openai.rs`, stop and
  ask how to sequence before touching the same functions.
- Base: `ef54b28`. Do not create a commit unless the owner asks.

## Scope — exactly two changes

1. Centralize the tunnel's timing and concurrency parameters into
   `TunnelTuning` (plus `Backoff`), built once, validated, carried on
   `TunnelClient` as `Arc<TunnelTuning>`, used everywhere the current constants
   and literals are used. Behaviour must be identical.
2. Raise concurrency: `poll_limit 25 -> 32`, `max_in_flight 8 -> 16`.

Explicitly out of scope — do not do these even if they look tempting:

- Do not change `POLL_TIMEOUT_MS`; the poll hold stays **15 s**.
- Do not change the runtime restart policy, `runtime_restart_delay`, or the
  runtime loop in `src/main.rs`.
- Do not touch `src/mcp.rs` (heartbeat interval, timeout output grace, tool
  handlers).
- Do not add configuration keys, schema fields, or a schema version bump.
- Do not add fair queuing, per-project limits, long-job permit budgets, or
  queued-command deadline checks.
- Do not add dependencies.

## Implementation order

1. Add `TunnelTuning` and `Backoff` with `Default`, `validated()`, and
   `Backoff::delay(attempt)`. Derive `request_timeout` as
   `poll_hold + REQUEST_TIMEOUT_MARGIN` where the margin is a named constant of
   10 s. Validation rejects: non-positive durations, `request_timeout <=
   poll_hold`, `max_in_flight == 0`, `poll_limit < max_in_flight`,
   `poll_limit > 3 * max_in_flight`, `restart_after == 0`, `backoff.cap <
   backoff.base`, `delivery_reserve >= request_timeout`.
2. Thread it: `TunnelClient::new` takes a `TunnelTuning`, validates it, stores
   `Arc`. Replace every constant/literal usage listed in the design, delete
   `retry_delay`, and change `apply_response_window(jsonrpc, window, reserve)`
   plus `shell_timeout_within(window, reserve)` to take the reserve from the
   tuning instead of the `RESPONSE_DELIVERY_RESERVE` const.
3. Extract `poll_query()` so `limit`/`timeout_ms` are unit-testable.
4. Add `debug!(?self.tuning, "tunnel tuning")` once per runtime start.
5. Flip the two defaults to 32 and 16 and confirm the validated band.
6. Add the tests listed in the TODO, update the one existing test that calls
   `apply_response_window` with the old signature, and add the CHANGELOG entry.

## Definition of done

- Every item in the plan TODO ticked, with the command output you ran.
- `cargo fmt --check`, `cargo test --locked --all-features`,
  `cargo clippy --locked --all-targets --all-features -- -D warnings`,
  `git diff --check`, `markdownlint-cli2` all pass.
- The full test suite passes unchanged except for the `apply_response_window`
  call sites you updated.
- `git diff --stat` lists only: `src/transports/openai.rs`, `CHANGELOG.md`, and
  the plan TODO.
- No commit, tag, or push.

## Report back

State, in order: what changed per file, the validated default values, the tests
you added and their result, the gate commands with results, and anything you
found that contradicts the design (for example a coupling the plan missed).
If the design turns out to be wrong somewhere, stop and report rather than
widening scope.
