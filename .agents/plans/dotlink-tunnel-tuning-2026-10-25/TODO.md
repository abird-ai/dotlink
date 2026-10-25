# dotlink-tunnel-tuning-2026-10-25 — checklist

Base: `ef54b28`

Design: [README.md](README.md). Handoff prompt: [CONTINUATION-PROMPT.md](CONTINUATION-PROMPT.md).

Coordination: DL-080 (tool surface v2) has landed; this plan stacks on it at `ef54b28`. This plan never touches `src/mcp.rs`, and it must not reopen DL-080's decisions (progress heartbeats, `apply_response_window` behaviour, tool names).

## Committed

### `00e45db` — `TunnelTuning`, concurrency bump, review fixes and docs

Deviations from the design text (recorded, not scope changes):

- `TunnelClient::new` takes `Config` plus `(TunnelTuning, CancellationToken)` instead of seven loose strings. Adding `tuning` as an eighth parameter tripped `clippy::too_many_arguments` under `-D warnings`; passing the existing `Config` drops the count to three and is neutral otherwise.
- `should_restart_runtime_after_poll_failures(failures, restart_after)` keeps its free-function shape and takes the threshold as a parameter, so the existing threshold test pins `tuning.restart_after` rather than a constant.
- `connect_timeout` is validated as non-zero (design table rule `> 0`); `activation_grace` is not validated, so "no grace" stays expressible.
- Sweep after the design landed: `README.md`'s "Parallel tool execution" section said "up to eight in-flight MCP requests" and now says sixteen; `.markdownlint-cli2.jsonc` ignores `.agents/runs/*/**`, because gitignored agent scratch is not repository documentation and was failing the lint gate from files this plan does not own.

Implementation:

- [x] Add `TunnelTuning` and `Backoff`: fields, `Default`, `validated()`, `delay(attempt)`
- [x] Derive `request_timeout` from `poll_hold + REQUEST_TIMEOUT_MARGIN (10 s)` in the single `Default` impl
- [x] `TunnelClient::new` takes `TunnelTuning`, validates it, stores `Arc<TunnelTuning>`
- [x] Replace the client builder literals with `connect_timeout` and `request_timeout`
- [x] Replace `POLL_TIMEOUT_MS`, `POLL_LIMIT`, `MAX_IN_FLIGHT`, `POLL_FAILURES_BEFORE_RUNTIME_RESTART`, the `activation_grace` literal, and `RESPONSE_DELIVERY_RESERVE` with the tuning; all six constants deleted
- [x] Replace `retry_delay(attempt)` with `tuning.backoff.delay(attempt)`; delete the free function
- [x] `apply_response_window(jsonrpc, window, reserve)` and `shell_timeout_within(window, reserve)` take the reserve as a parameter; `process_command` passes `self.tuning.delivery_reserve`
- [x] Extract `poll_query()` for testability
- [x] Emit `debug!(?self.tuning, "tunnel tuning")` once per runtime start
- [x] `poll_limit` default `25 -> 32`
- [x] `max_in_flight` default `8 -> 16`; semaphore built inline in `TunnelClient::new` from the tuning
- [x] Confirm the validated band still holds (`16 <= 32 <= 48`) and document the ratio rule in the field docs
- [x] `CHANGELOG.md` under Unreleased: tunnel concurrency `8 -> 16` and poll batch `25 -> 32`; parameters centralized and validated, no protocol change
- [x] Note the follow-ups (config surface, fairness, delivery-reserve derivation) in the plan, not in code

Review fixes (post-implementation):

- [x] `Backoff::delay` no longer caps the exponent at 5; it doubles `base` with `checked_shl` and clamps to `cap`, so a wide cap is reachable and `u32::MAX` attempts cannot overflow. Default delays are unchanged.
- [x] `Default` for `TunnelTuning` is infallible: validation happens once, in `TunnelClient::new`, so a bad default surfaces as a startup error rather than a panic
- [x] Deleted the `concurrency_semaphore` helper; `client_semaphore_matches_max_in_flight` builds a real `TunnelClient` with `max_in_flight: 12`, so it proves the wiring instead of restating the helper
- [x] `poll_query_carries_tuning` also asserts the serialized wire query (`limit=24&timeout_ms=20000`), turning "protocol untouched" into a test
- [x] Remaining tunnel literals centralized: `activation_retry` (2 s), `activation_checks` (30), `response_attempts` (6) are validated fields, so nothing in the poll/response path is a bare number any more
- [x] `Backoff` moved to `src/backoff.rs` and shared with `main.rs`'s runtime restart schedule (`RUNTIME_RESTART_BACKOFF`, 1 s doubling to a 30 s cap); delays are unchanged and the old capped exponent is gone
- [x] `TunnelTuning` doc states the only thing kept outside: the client-supplied response window, bounded from here by `delivery_reserve`
- [x] `EmbeddedMcp`/`TunnelClient` and their constructors are `pub(crate)`; only `Config` and `run` stay public

## Uncommitted

None.

## Tests and verification

- [x] `tunnel_tuning_defaults_are_valid` — defaults pass `validated()`; `poll_hold == 15 s`, `request_timeout == 25 s`, `poll_limit == 32`, `max_in_flight == 16`, `activation_retry == 2 s`, `activation_checks == 30`, `response_attempts == 6`, backoff 250 ms/5 s — `00e45db`
- [x] `invalid_tuning_is_rejected` — `request_timeout <= poll_hold`, `poll_limit < max_in_flight`, `poll_limit > 3 * max_in_flight`, `max_in_flight == 0`, `activation_retry == 0`, `activation_checks == 0`, `restart_after == 0`, `response_attempts == 0`, `backoff.cap < backoff.base`, `delivery_reserve >= request_timeout` — `00e45db`
- [x] `poll_query_carries_tuning` — `limit` and `timeout_ms` come from the tuning (non-default values, so the wiring is proven) and serialize to the exact wire parameters — `00e45db`
- [x] `backoff::tests::{delay_doubles_from_the_base_up_to_the_cap,cap_is_reachable_however_wide_it_is,zero_jitter_is_deterministic}` — jitter bands, cap held at `u32::MAX`, a 60 s cap over a 100 ms base reached, deterministic schedule for `jitter == 0` — `00e45db`
- [x] `runtime_restart_backoff_is_bounded` still sees 1/2/16/30/30 s, so sharing `Backoff` changed no delay — `00e45db`
- [x] `client_semaphore_matches_max_in_flight` — a real client built with `max_in_flight: 12` reports 12 permits — `00e45db`
- [x] Existing `client_window_bounds_shell_timeouts_only` updated for the `reserve` parameter and still asserts the 5 s reserve behaviour — `00e45db`
- [x] `poll_failure_threshold_restarts_runtime_on_tenth_failure` updated for the threshold parameter — `00e45db`
- [x] Existing tunnel tests still pass unchanged (`descriptor_rooted_native_tools_round_trip_through_mcp`, `mcp_shells_are_parallel_by_default_and_serial_when_requested`, progress heartbeat, client-window tests) — 231 bin tests + 1 stdio integration test — `00e45db`
- [x] `cargo fmt --check` — `00e45db`
- [x] `cargo test --locked --all-features` — `00e45db`
- [x] `cargo clippy --locked --all-targets --all-features -- -D warnings` — `00e45db`
- [x] `git diff --check` — `00e45db`
- [x] `markdownlint-cli2` — 49 files, 0 issues (gitignored agent scratch under `.agents/runs/*/**` is excluded) — `00e45db`
- [x] `nix flake check` — package, test, clippy, fmt, alejandra, markdownlint and NixOS module eval all pass — `00e45db`
