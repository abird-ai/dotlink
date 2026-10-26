# Tunnel tuning: centralized parameters and concurrency bump

Author: Prasanna Loganathar

Co-authors: pi agent

Owner: @pvl

## Why

The tunnel transport's timing and concurrency parameters are scattered: named constants at the top of `src/transports/openai.rs`, bare literals at their usage sites, and no statement of the relations between them. Two of those relations are load-bearing and currently invisible:

- `Client::builder().timeout(25 s)` must stay above `POLL_TIMEOUT_MS` (15 s), or every idle long-poll becomes a transport failure and trips the ten-failure runtime restart. Raising the poll hold to 30 s without touching the client timeout would put dotlink into a permanent restart loop.
- `POLL_LIMIT` must stay near `MAX_IN_FLIGHT`, because commands drain into tasks that wait on one semaphore while their client window (`batch.received_at`) keeps running. A limit far above the in-flight capacity silently expires the tail of each batch.

Both were found while investigating nightly connector disconnects: l5 lost its network for ~25 s at 04:00, absorbed ten poll failures at 15–25 s each, and restarted the whole runtime. The restart path itself is out of scope here; this plan makes the numbers explicit, validated, and tunable in one place, and raises throughput now that dotlink serves several projects from one process.

## Base

`ef54b28` — after DL-080 landed. The tree should be clean apart from this plan directory; confirm with `git status` before editing.

## Scope

Two changes only:

1. **Centralize** the tunnel's timing and concurrency parameters into one `TunnelTuning` value that is built once, validated against its invariants, carried on `TunnelClient`, and used everywhere the literals and constants live today. Behaviour is unchanged.
2. **Bump** concurrency: `POLL_LIMIT 25 -> 32` and `MAX_IN_FLIGHT 8 -> 16` (ratio 2x).

Out of scope, deliberately:

- `POLL_TIMEOUT_MS` stays at **15 s**. Delivery latency does not depend on it (the server releases a held poll as soon as a command is available), and a longer hold only delays detection of a dead link and of recovery.
- No config schema change. These stay internal defaults; making them profile-tunable would require a v11 schema and is a separate plan.
- No change to the runtime restart policy: the threshold (10) and the schedule (1 s doubling, 30 s cap) stay exactly as they are, though that schedule now reuses the same `Backoff` type. No fairness/per-project queueing, no per-command start-deadline check.
- Shell timings (`SHELL_HEARTBEAT_INTERVAL`, `TIMEOUT_OUTPUT_GRACE`, now in `src/mcp/tools/shell.rs`) are untouched.
- `delivery_reserve` stays a flat 5 s. Deriving it from the client window is noted as a follow-up, not implemented here.

## Design

### TunnelTuning

One value per tunnel runtime, constructed once, `Arc`-shared into spawned command tasks.

| Field | Today | After | Rule |
| --- | --- | --- | --- |
| `poll_hold` | 15 s (`POLL_TIMEOUT_MS`) | 15 s | `> 0` |
| `poll_limit` | 25 (`POLL_LIMIT`, `u8`) | **32** | `max_in_flight ..= 3 * max_in_flight` |
| `max_in_flight` | 8 (`MAX_IN_FLIGHT`) | **16** | `> 0` |
| `connect_timeout` | 10 s (literal) | 10 s | field, `> 0` |
| `request_timeout` | 25 s (literal) | **derived**: `poll_hold + REQUEST_TIMEOUT_MARGIN (10 s)` | must exceed `poll_hold` |
| `activation_grace` | 45 s (literal) | 45 s | field |
| `activation_retry` | 2 s (literal in the not-ready arm) | 2 s | `> 0` |
| `activation_checks` | 30 (literal) | 30 | `> 0` |
| `restart_after` | 10 (`POLL_FAILURES_BEFORE_RUNTIME_RESTART`) | 10 | `> 0` |
| `response_attempts` | 6 (literal in `post_terminal`) | 6 | `> 0` |
| `backoff` | `retry_delay`: base 250 ms, cap 5 s, jitter 0..=250 ms | shared `Backoff` (`src/backoff.rs`) | base `> 0`, cap `>= base` |
| `delivery_reserve` | 5 s (`RESPONSE_DELIVERY_RESERVE`) | 5 s | `< request_timeout` |

Shape:

```rust
pub(crate) struct TunnelTuning {
    pub poll_hold: Duration,
    pub poll_limit: usize,
    pub max_in_flight: usize,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub activation_grace: Duration,
    pub activation_retry: Duration,
    pub activation_checks: u32,
    pub restart_after: u32,
    pub response_attempts: u32,
    pub backoff: Backoff,
    pub delivery_reserve: Duration,
}

// src/backoff.rs — one schedule, two consumers.
pub(crate) struct Backoff {
    pub base: Duration,
    pub cap: Duration,
    pub jitter: Duration,
}

impl Backoff {
    fn delay(&self, attempt: u32) -> Duration;
}

impl TunnelTuning {
    /// Rejects combinations that break a load-bearing relation.
    pub(crate) fn validated(self) -> Result<Self>;
}

impl Default for TunnelTuning {
    /// Defaults above, infallible; `TunnelClient::new` validates them.
    fn default() -> Self;
}
```

Validation rules, each rejecting with a specific message:

- `poll_hold` and `request_timeout` non-zero, and `request_timeout > poll_hold`.
- `max_in_flight > 0`.
- `poll_limit >= max_in_flight` and `poll_limit <= 3 * max_in_flight`.
- `activation_retry > 0`, `activation_checks > 0`, `restart_after > 0`, `response_attempts > 0`.
- `backoff.base > 0`, `backoff.cap >= backoff.base`.
- `delivery_reserve < request_timeout`.

Derivation lives in one place: `TunnelTuning::default()` computes `request_timeout` from `poll_hold` and `REQUEST_TIMEOUT_MARGIN`, so raising the hold can no longer leave the HTTP timeout behind.

### Plumbing

- `TunnelClient` gains `tuning: Arc<TunnelTuning>`; `TunnelClient::new` takes the existing `Config`, a `TunnelTuning` and the cancellation token, validates the tuning, and stores the `Arc`.
- Replace every usage: the `Client::builder()` timeouts (`:353-354`), `Semaphore::new(MAX_IN_FLIGHT)` (`:366`), the poll query (`:482-483`), `activation_grace_until` (`:363`), the restart threshold check, `retry_delay` call sites, and `RESPONSE_DELIVERY_RESERVE` in `apply_response_window`.
- `retry_delay(attempt)` becomes `tuning.backoff.delay(attempt)`; delete the free function.
- `apply_response_window(jsonrpc, window, reserve)` and `shell_timeout_within(window, reserve)` take the reserve as a parameter instead of reading the `RESPONSE_DELIVERY_RESERVE` const; `process_command` passes `self.tuning.delivery_reserve`.
- Extract `fn poll_query(&self) -> [(&'static str, String); 2]` so the wire parameters are unit-testable without a network call.
- Emit `debug!(?self.tuning, "tunnel tuning")` once per runtime start. It stays out of the default `dotlink=warn` filter and is visible with `RUST_LOG=dotlink=debug`, which is what a future log investigation needs.

### Concurrency bump

`POLL_LIMIT 25 -> 32`, `MAX_IN_FLIGHT 8 -> 16`.

- Ratio is 2x, inside the validated band, so a full batch leaves at most 16 commands queueing for a permit.
- Every permit is held for the whole call, and the shell ceiling is now 3600 s, so 16 long builds can still pin every slot; this plan does not change that.
- Host risk is memory and CPU, not project count: l5's service cgroup already peaked at 15.6 GB. Smaller hosts can be protected later by a config knob (separate plan).
- No fairness is introduced: one ChatGPT session's burst still occupies the shared semaphore.

## Invariants preserved

- Protocol untouched: same poll query parameters apart from `limit`, same response POST, same `request_id`/`shard_token` correlation.
- No new configuration surface, no schema version change, no migration.
- No new dependencies.
- Default log output unchanged; the tuning line is debug-level.

## Risks

- A validation rule that is too tight would reject a legitimate future tuning; each rule above is derived from an observed failure mode, not taste.
- The concurrency bump can raise peak memory on small hosts; it is a constant, revertible in one line, and the plan records the measured baseline (15.6 GB on l5 with 8 in flight).
- The `Arc<TunnelTuning>` is shared by all sessions of one runtime; nothing mutates it, so no locking is needed.

## Follow-ups (not this plan)

- Derive `delivery_reserve` from the client window (for example `clamp(window / 10, 1 s, 10 s)`) instead of a flat 5 s.
- Config-tunable `poll_hold`, `poll_limit`, `max_in_flight` under a v11 schema with defaults that preserve today's behaviour.
- Fair queuing per `dir`/session, and a separate permit budget for long-running shells.
- Drop queued commands whose remaining window cannot cover a minimal run, with a warn-level log.
- Outage-tolerant restart policy: keep the runtime and in-flight tools alive through transport-level poll failures.

## References

- `src/transports/openai.rs` — constants (`:24-32`), client construction (`:353-366`), poll loop and batch drain (`:380-460`), `poll_once` (`:478-520`), `post_terminal` (`:700+`), `apply_response_window` and `retry_delay` (`:888-1056`).
- `src/backoff.rs` — the shared doubling schedule, with the jitter band and the cap-reachability rules.
- `src/main.rs` — `RUNTIME_RESTART_BACKOFF`, stop timeout.
- `.agents/plans/dotlink-tool-surface-2026-10-25/` (DL-080) — landed on 2026-10-25 (`grep`, `patch`, `bedit`, `edit` rework, progress heartbeats); this plan stacks on that and must not reopen it.
- Journal evidence used for the numbers: pvl-l5 one runtime restart per day at 04:02 local with 958/1265 poll failures in that hour; failure intervals clustered at 0–5 s (fast failure) and 15 s/25 s (held poll, client timeout).

## Validation

- `tunnel_tuning_defaults_are_valid`: defaults pass `validated()`, with `poll_hold == 15 s`, `request_timeout == 25 s`, `poll_limit == 32`, `max_in_flight == 16`.
- `invalid_tuning_is_rejected`: `request_timeout <= poll_hold`, `poll_limit < max_in_flight`, `poll_limit > 3 * max_in_flight`, `max_in_flight == 0`, `restart_after == 0`, `backoff.cap < backoff.base`.
- `poll_query_carries_tuning`: `limit` and `timeout_ms` reflect the tuning.
- `backoff_delay_is_bounded`: monotonic up to `cap`, jitter within range.
- Existing `client_window_bounds_shell_timeouts_only` updated for the `reserve` parameter.
- Semaphore wiring test: a helper builds the semaphore from the tuning and `available_permits()` equals `max_in_flight`.
- Gates: `cargo fmt --check`, `cargo test --locked --all-features`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `git diff --check`, `markdownlint-cli2`.
