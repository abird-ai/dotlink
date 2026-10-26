# dotlink-tool-review-2026-10-26

Base: `fc53fc9`

Design and decisions: [README.md](README.md).

## Landed

Landed in signed commits `716ad06` (core), `5a3c0b8` (transport), `8afe8d0` (final core reconciliation), `8a95d4d` (integration tests), and `dc7f59b` (user docs).

- [x] `patch`: one claimed-path set per call; a path named by two sections is
  refused while planning, before any write, in both modes
- [x] `patch`: `*** Add File` treats only `NotFound` as absent; a no-op update
  stays out of the receipt; `best_effort` collects verification failures
- [x] `walk`: entry metadata errors and unreadable child directories are skipped;
  a caller-named root still fails loudly
- [x] `walk`: `.gitignore` loads only for an enumerable directory and widens the
  search on read failure
- [x] `walk`: descent and `.gitignore` reads use pinned handles; search
  candidates stay paths and re-open through the descriptor-rooted reader
- [x] `bedit`: re-verified `sha256` and snapshot before writing; deterministic
  `(offset, length)` range order
- [x] `read`: byte-exact `numbered: false`; an offset past EOF is an error
- [x] `grep`: exact per-file `count`; `page_total` names the returned-page sum;
  one `OutputBudget` bounds all three modes
- [x] `ls`: `**` implies no depth cap, matching `grep`
- [x] Writers: `SecureFs::stage`/`stage_update`/`stage_create` (temp +
  rename/link, mode
  preserved) used by `write`, `bwrite`, `edit`, `patch`, `bedit`; the truncating
  open is gone
- [x] Deny/protected paths folded lexically before resolution
- [x] Tunnel: validation covers the poll hold plus the larger of connect/margin,
  and the activation check budget; `restart_after` travels in the run outcome
- [x] Structure: `src/mcp/tools/{ls,grep,text,edit,patch,binary,shell}.rs` behind
  per-family routers; `TOOL_SPECS` drives visibility, the concurrency gate and
  the TOOL log; `Snapshot`/`Verified` make plan → verify → commit the only order
  for existing-file updates
- [x] TOOL-log argument names come from each tool's schema; one payload list keeps
  file content out of logs
- [x] Docs: `docs/SECURITY.md` (default tools, walk rooting, atomic writes),
  `docs/ARCHITECTURE.md` (tool layout, atomic writes), `README.md`, `CHANGELOG.md`
- [x] Plans: DL-080 README claims corrected, its verification list ticked, the
  queue line given its closing sha; `docs/ARCHITECTURE.md` and the DL-090
  references follow the new module paths

### Second review pass

- [x] `grep`: contiguous paging within the candidate bound; `offset` before
  the per-file cap;
  byte marker when nothing fit; JSON replies trimmed to the ceiling
- [x] `grep`: candidates hold paths only, so a large tree cannot exhaust
  descriptors; reads go through the descriptor-rooted reader
- [x] `patch`/`edit`: stage every file, then commit; a failed Add leaves no file
- [x] `patch`: claim before resolve; CRLF-preserving parser; `@@ numbers + text`
  anchor
- [x] `bedit`: planning read bounded by the patchable-file size
- [x] walk: child opens inherit the root's resolve mask; `EXDEV`/`ELOOP`
  skippable; `/proc/self/fd` fallback
- [x] `Snapshot::verify` states only the change; callers state the consequence
- [x] TOOL log detail computed only when activity logging is on
- [x] Docs/CHANGELOG updated for staged writes, the read path and the paging rule

### Third review pass

- [x] `grep`: every stop reason stops the whole scan; two-file per-file-cap
  paging; marker names the real cause; `page_total`; exact final JSON measured
- [x] `grep`: context-covered hits do not consume page slots
- [x] `ls`: rendered JSON bounded by `max_output_bytes`
- [x] `patch`: lexical and resolved paths both claimed; symlink-alias duplicate
  refused before writing in atomic and best_effort modes
- [x] `patch`: `stage_create` installs with a create-only link; no empty target,
  no path-based cleanup, no leftover on commit failure
- [x] `patch`: added lines inherit the target file's CRLF/LF style
- [x] Writers: non-Linux stages sync, preserve permissions, reject symlink/special
  targets, and use hard-link create-only
- [x] Tunnel: permits acquired inside tasks; up to three poll batches may
  queue, so a full batch does not block polling; sustained backlog applies bounded backpressure
- [x] Tunnel: connect-timeout and activation-window invariants validated
- [x] `bedit`: `changed` in the receipt; no write claimed for a byte-identical plan
- [x] Concurrency tests: same-path lock waits, independent path proceeds,
  concurrent `bedit`/`edit`/`patch` preserve every non-overlapping change;
  commit re-checks the snapshot and rejects an external rewrite
- [x] `grep` context paging does not repeat matching lines across pages
- [x] `bedit` rejects a file above the profile write ceiling and zeroes a no-op
- [x] Tunnel response status classifier covers all server errors; activation
  grace overflow returns a validation error
- [x] `backoff`: nanosecond jitter and saturating add
- [x] `grep`: content pages select exact matching lines; context is clipped at
  adjacent matching lines outside the page
- [x] `grep`: a byte-constrained page selects the largest contiguous hit
  prefix that fits instead of blanking the page
- [x] Shell: stdout/stderr drains start before stdin writes; stdin is written
  concurrently under the same deadline; bidirectional pipe-pressure regression
- [x] XDG: shared resolver ignores empty/relative overrides and falls back home;
  setup, OAuth state, cache discovery and host discovery use it
- [x] `read`: line-streamed window, bounded returned bytes and line length; no
  whole-file preload
- [x] Tunnel: local MCP response collection enforces the 16 MiB ceiling while
  streaming rather than after buffering
- [x] `bedit`: respects `max_write_bytes` and reports zero bytes for a no-op
- [x] Tunnel: delivery reserve bounds all dispatch; every 5xx response POST is
  retryable; up to three batches queue before bounded backpressure
- [x] Tunnel: activation grace covers `checks + 1`; backoff cap+jitter bounded;
  duration overflow returns an error
- [x] Concurrency: bounded sharded per-resolved-path mutation locks held from
  snapshot read through commit; multi-file locks acquired in sorted order;
  updates re-check snapshot length and digest at commit
- [x] Contract: strict preflight + per-file atomic replacement, with multi-file
  partial-commit semantics documented and reported
- [x] Docs corrected: grep re-opens files beneath the pinned root; MAX bounds
  collected results, not traversal; `Verified` is update-only; atomic-replace
  trade-offs documented

### Fifth review pass

- [x] Staging: temporary created at the target's mode (never broader), shared by
  the Linux and non-Linux backends in `src/staging.rs`; regression test observes
  the temporary during an 8 MiB replacement of a `0600` file
- [x] `grep content`: marker space reserved before a hit is accepted; whole-hit
  pages; `cap_text` removed; per-file-cap marker names the cause; out-of-range
  offset reports itself
- [x] Limits: `MIN_BYTE_LIMIT`/`MIN_SHELL_TIMEOUT_SECS` validated in `setup` and
  rejected (not widened) at runtime; test asserts the error names the value and
  that an accepted value is used exactly
- [x] Reply trimming: binary-search prefix; render-count test plus a pathological
  case (211 s -> ~140 ms)
- [x] Walk: descriptor-rooted enumeration fails closed without procfs; staging
  temporaries hidden from `ls`/`grep`
- [x] `read`: reply fits `max_read_bytes` by dropping whole lines, never cutting
  one
- [x] `ls`: default depth derived from the pattern's path segments
- [x] `patch`: `*** End of File` enforced, hunk-after-marker refused
- [x] Shell: deny-write roots (file or directory) rebound read-only by the
  dedicated pass; checked deadline; shell predicate derived from `TOOL_SPECS`
- [x] `bedit`: request position breaks same-offset/same-span ties
- [x] Router cached once per machine; `edit`/`patch`/`bedit` descriptions
  advertise their limits with drift tests
- [x] Docs: SECURITY (logging decision, staging mode, hard ceilings, procfs),
  ARCHITECTURE (staging module, trimming rule, whole-hit pages, `ls` depth),
  README, configuration, CHANGELOG

### Sixth review pass

- [x] Cross-process mutations: advisory directory lock held from snapshot through
  commit, shared within the process, sorted acquisition, 30 s bounded wait;
  eight-process regression test asserts every acknowledged insert survives
- [x] `read` builds its page in one buffer (no per-line `Vec`) and skips
  preceding lines by scanning, so a skipped oversized line cannot block a later
  window; only returned lines are decoded
- [x] Streamable-HTTP body ceiling derived from `max_write_bytes` in one shared
  config for the HTTP server and the tunnel; drift test pins the derivation
- [x] Non-Linux staging test compiles on Windows (Unix assertions gated)
- [x] Windows ACL inheritance documented with mitigation; open Windows
  validation item recorded rather than adding unverifiable Win32 code
- [x] Docs: SECURITY (coordination scope, advisory/wait semantics, Windows ACL
  caveat, transport ceiling, read skip), ARCHITECTURE (lock layers, shared
  config, read buffer), configuration, README, CHANGELOG

### Seventh review pass

- [x] Lock mode is decided by the locked directory, not the call's path list;
  regression test fails with the old logic
- [x] Record capability cached per lock root, fail closed if it regresses;
  fast path covered by tests through an injectable root
- [x] Timeout names the blocked path
- [x] Non-Linux commit re-check streams the target instead of buffering it
- [x] Fast path validated end-to-end across processes on a `1777` lock root
  (8/8 inserts on one file, sibling files independent)
- [x] Docs: fast-path precondition, per-directory mode, and the separate-lock-root
  topology that loses coordination

## Tests and verification

- [x] `patch`: duplicate section (Add+Add, Add+Update) refused with nothing
  written; `best_effort` reports the conflict; non-absent Add target errors;
  no-op update absent from the receipt
- [x] `walk`: unreadable subdirectory skipped; unreadable entry skipped; invalid
  `.gitignore` in an unlistable directory does not fail; a swapped symlink is
  refused when read through the pinned handle
- [x] `Snapshot`/`Verified`: identical text writes nothing, a stale snapshot is
  refused with the contract text, a verified commit replaces atomically and
  leaves no temporary
- [x] `secure_fs`: `stage`/`stage_update` keep content and mode, refuse symlink
  and directory targets, leave no temporary; `stage_create` is absent until commit;
  `open_child_*` cannot escape
- [x] `bedit`: both request orders at a shared offset produce the same file;
  `sha256` mismatch writes nothing; an invalid plan leaves the file intact
- [x] `read`: CRLF and trailing newline preserved with `numbered: false`; past-EOF
  offset is an error
- [x] `grep`: exact per-file `count`; `page_total`; `files` mode not truncated by
extra hits; every mode stops at `max_output_bytes`
- [x] `ls`: `**` depth rule
- [x] `TOOL_SPECS` matches the merged routers; deny with a missing component and
  `..` still denies
- [x] Second pass: staged-write failure leaves every file unchanged and no
  temporary; a claimed `*** Add File` is undone when a later file fails; CRLF patch; claim-before-resolve in `best_effort`; anchor with
  numbers and text; content paging past the per-file cap; byte marker when
  nothing fits; JSON replies inside the ceiling with escaped paths; symlink loop
  skipped; `grep` over 300 directories under `ulimit -n 256`
- [x] `cargo fmt --check`
- [x] `cargo test --locked --all-features` — 282 unit + 2 stdio probes
- [x] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [x] `git diff --check`
- [x] `markdownlint-cli2`
- [x] `DOTLINK_TEST_BWRAP=1 cargo test --locked --all-features` — Bubblewrap smoke included
- [x] `nix flake check` — build, clippy, fmt, alejandra, markdownlint, tests
- [x] Release integration: `src/mcp/tools/` and the plan record are staged so
  Nix's Git source snapshot includes them; plain `nix flake check` passes
- [x] Fifth pass: staging mode never broader than the target (fails without the
  fix); whole-hit byte-limited pages; per-file-cap marker; out-of-range offset;
  staging temporaries hidden; deep `ls` pattern; `*** End of File` enforced and
  contradicted; same-span `bedit` ties; limit rejection naming the value; shell
  predicate vs `TOOL_SPECS`; JSON trim render count
- [x] Sixth pass: eight-process mutation test (fails without the directory
  lock), read page buffer and skipped-line cases, transport ceiling derivation,
  Windows-safe staging test
- [x] `cargo fmt --check`, `cargo test --locked --all-features` (316 + 3),
  `cargo clippy --locked --all-targets --all-features -- -D warnings`,
  `git diff --check`, `markdownlint-cli2`
