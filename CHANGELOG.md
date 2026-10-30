# Changelog

All notable changes to dotlink are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.14.2] - 2026-10-31

### Fixed

- Tunnel replay namespace reactivation now promotes inactive `Retired` identities into a fresh fail-closed `Uncertain` generation instead of exposing retired state in an active scope. Delivery leasing/commit semantics apply normally after reactivation, and stale prior-generation expirations cannot remove a newer delivered tombstone or reopen the request for execution.
- Bubblewrap discovery now evaluates complete host-managed provenance per executable PATH candidate and skips rejected shadows in favor of later trusted candidates. Ordinary launchers need a root-owned non-writable canonical path chain. Nix launchers must be real immutable store items on the exact non-FUSE store mount, but immutability alone is not selection authority: every runtime uid requires either the build-pinned derivation or a root-owned final PATH symlink beneath a root-owned non-writable resolved parent chain. Direct user-selected store derivations are rejected; UID-remapped/read-only Nix stores remain unprivileged-only after trusted selection because a read-only launcher view cannot authenticate the dynamic loader/shared-library closure for root.
- Durable shell spooling now uses sealed job-owned writer capabilities that combine lifetime and quota authority. Stdout/stderr slots are reserved atomically before spawn; terminal publication and shutdown seal future registration, await true writer quiescence after cancellation, and only then expose or clean spool state. Immutable terminal capture lengths remain the replay/idempotent-ack history, while per-stream retained-byte/release state follows Task compaction and partial cleanup so `job_get` and `job_output` agree on what is still pageable; native Task rendering stays bounded to the frozen terminal result prefix, and retention expiry hides the handle immediately even when physical cleanup must retry internally.

## [0.14.1] - 2026-10-30

### Fixed

- Control-plane base URLs now use one parsed canonical identity across setup, runtime URLs, job-session fingerprints and tunnel replay namespaces, so equivalent host casing/default-port/path spellings cannot split exactly-once request dedupe.
- Durable spool quota accounting is now owned per job and cancellation-safe: cleanup/Task compaction reconcile retained bytes and residual reservations are released even when an async spool writer is dropped mid-I/O.
- Linux Bubblewrap launcher trust no longer rejects every root-owned system launcher when dotlink runs as root; unprivileged same-uid mutability checks remain, while root execution relies on the pinned sealed snapshot and still rejects privilege-bearing or shebang launchers.
- Shell validation/admission/handoff protocol errors now always emit the matching TOOL completion outcome instead of leaving a start-only log line.

## [0.14.0] - 2026-10-30

### Added

- `--unrestricted` is the one-flag full-host mode, exactly equivalent to root read+write filesystem access + shell + network + no sandbox. The existing `--allow-all --no-sandbox` pair remains supported as the legacy equivalent.
- One durable shell execution layer now backs ordinary `bash`/`powershell`, native MCP Tasks and compatibility jobs. `job_start` returns immediately; `job_list` recovers exposed ids after a lost receipt; `job_get`, pure/replayable `job_output`, explicit idempotent `job_ack`, and `job_cancel` operate on the same stable job/task id.
- Durable job state has a configurable aggregate `job_spool_bytes` quota (512 MiB default, 16 MiB–8 GiB) in addition to the 64 MiB retained prefix per stdout/stderr stream. The first byte omitted by either the stream cap or aggregate session quota permanently switches that stream to drain-only, so later quota reclamation cannot turn the retained prefix into prefix-plus-suffix. stdout/stderr plus terminal metadata are the single durable result source; native MCP Tasks reconstruct the exact original `CallToolResult` from those spools and are selected only when that exact result is guaranteed to fit the task response budget. Otherwise dotlink uses the compatibility job surface; no duplicate full result artifact is stored.

### Changed

- OpenAI's 120-second request lifetime and 5-second delivery reserve now bound only how long a shell request waits. Every shell call starts one durable execution; if it finishes in-window dotlink returns the ordinary result, otherwise the same execution is exposed as an MCP Task/job without changing its execution timeout. Omitted `timeout_secs` uses the profile ceiling. Tunnel request identities now gate every tool explicitly marked `ToolSpec.request_replay = true` through one process-lifetime replay ledger before MCP dispatch, so concurrent, queued-after-delivery, or in-process-restart redelivery cannot repeat side effects.
- OpenAI limits are explicit at each layer: 8 MiB raw/tool transfer ceiling, 9 MiB complete embedded MCP-message ceiling, and 10 MiB hosted tunnel envelope. `bread` exposes the profile read ceiling and automatically chooses encoding-safe raw windows for MCP/base64/hex.
- Job admission, runner ownership and shutdown are atomic in `JobManager`; queued serialized/Windows jobs acquire the operation gate before path/mount preparation, and execution timing begins only afterward. Automatic recovery reuses a job session only when the effective filesystem/shell/network/cache/host authority fingerprint is unchanged, including the prepared shell/Bubblewrap executable identities. The lazily materialized per-JobManager spool root is keyed by its random session pathname (including lexical/resolved XDG spellings), so its first creation does not falsely revoke the same session while external protected paths remain object-identity-sensitive.
- Unix mutation coordination now combines exact pathname locks with stable `(ancestor dev, ancestor inode, relative target)` in-process identities. Cross-process directory lock plans coalesce aliases by directory inode, upgrade to the strongest mode once, and verify the opened inode before locking.
- New profile read/write/output defaults remain 1 MiB and configurable raw/tool ceilings remain 8 MiB; `read.max_read_bytes`, `bread.limit`, shell/job output schemas expose concrete profile bounds.
- TOOL logs default to a 256-rendered-character `c=` shell command excerpt (including quotes/escapes) with `…` before the closing quote when truncated. Interactive non-stdio `V` toggles full commands only while TOOL logging is active; quiet and REQ-only modes hide/ignore the key and clear full mode when TOOL logging turns off. Services and stdio stay capped. `p=`/`d=` values and all distinct paths of valid `edit` calls are always complete. Interactive colored logs distinguish keys, values and outcomes; redirected logs remain plain. Bulk payloads stay byte counts.
- CRLF patch hunks now store borrowed text plus a one-bit trailing-CR adjustment; the shared final output builder emits it without allocating a per-added-line `String`. This preserves context matching, whitespace tolerance and source line endings for dense updates.
- `grep` reuses the shared UTF-8-classified source bytes without cloning them. Content pages collect only the selected hit indices and adjacent page boundaries (at most the per-file hit cap), and render context by walking borrowed lines. Files/count modes count via iterators, without per-line or per-hit vectors. Pagination, result markers and I/O ceilings are unchanged.
- MCP text `patch` parses file paths, hunk anchors and update lines as borrowed slices of the request instead of cloning them; add-file sections retain one contiguous byte range rather than allocating even a per-line slice vector. Snapshot preflight compares file content with a single 64 KiB scratch buffer, and ordinary one-match edits mutate their existing `String` directly. Multi-match non-growing edits compact in place using `memchr`'s SIMD-aware byte-pattern search, avoiding both full-file copies and quadratic UTF-8 validation.
- HTTP standard tool preflight inspects the tool selector without allocating a second JSON `Value` tree for known tools; the OpenAI embedded adapter likewise clones JSON only for unknown-tool canonicalization. Existing unknown-tool schema-cache and header validation behavior is unchanged. Encoded binary inputs are consumed promptly rather than retaining all encoded strings beside their decoded range buffers.

### Fixed

- Valid MCP `_meta: null` on OpenAI tool/task requests is normalized before trusted sync/result/replay metadata is attached; malformed metadata is rejected before execution. Shell redelivery still resolves to one durable job and never repeats side effects.
- `--unrestricted` now supersedes redundant saved or CLI sandbox-only host, hardware and cache integrations without trying to mount/discover them. Regular sandboxed profiles retain their configured capabilities.
- Bubblewrap's startup capability probe now bounds captured help output as well as its deadline; unsupported flags, excessive help output and nonzero help exit report actionable configuration errors. Executable discovery uses effective Unix `X_OK` access rather than raw mode bits, and Linux preparation validates that exact pinned object before snapshotting it into a sealed executable `memfd`, requesting `MFD_EXEC` explicitly on modern kernels with a pre-`MFD_EXEC` fallback and an operator-facing startup failure when hardened kernel policy prohibits executable memfds. Bubblewrap executes only that immutable snapshot and its host-side launch starts from an empty inherited environment, so dynamic-loader controls such as `LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT` and `GLIBC_TUNABLES` cannot run untrusted code before isolation; the sealed shell is materialized with `--perms 0500 --ro-bind-data`. Bubblewrap's exact pinned launcher object itself must not be owned or writable by the dotlink uid; lexical parent-directory ownership is not part of launcher trust because Linux execution consumes the sealed snapshot rather than reopening that pathname. The system-managed native launcher's host loader/package closure is trusted host configuration, so sandboxed root-RW grants remain compatible with immutable package-store launchers; and setuid/setgid/file-capability or `#!` launchers are rejected because sealed snapshots intentionally do not preserve those host execution privileges. Automatic recovery prepares/fingerprints a candidate without executing it, revokes an incompatible old job session first, and only then capability-probes a new launcher; unchanged authority reuses previously validated bytes. Non-Linux prepared launchers revalidate identity and SHA-256 immediately before pathname spawn.
- stdio request inspection borrows incoming buffers and processes line fragments without per-byte atomic reads or chunk copies; shell job spawning moves its owned arguments instead of cloning potentially large stdin.
- Broad ordinary Linux shell grants such as `$HOME` no longer fail merely because they contain dotlink's protected state. Native tools continue denying it; Bubblewrap turns existing canonical protected files/directories into stable leaf mountpoints (mask or read-only rebind) and self-binds the mutable ancestor chain from pinned directory FDs so the sandboxed child cannot rename around the guard while ordinary writes elsewhere remain available. Nested private files are covered by an already-guarded directory. Missing names and symlink spellings still fail closed when no stable leaf can be anchored.
- Long OpenAI shell calls no longer die around 114–115 seconds, and a fast command with a long requested timeout no longer gets needlessly forced into background mode. Conversely, an omitted-timeout command that outlives the response window continues under its profile execution timeout and returns a durable handle.
- Closing/reloading a job session can no longer race creation and leave a detached runner; partial spool-file admission rolls back transactionally, active readers defer cleanup, and infrastructure spool/result failures surface as failed MCP Tasks rather than invalid parameters/completed tool errors.
- Durable output no longer performs hidden acknowledgement/deletion during a read; cleanup advances only through `job_ack`, so retries are replay-safe.
- A child that intentionally closes shell stdin early no longer turns a benign `BrokenPipe` into a failed job/process-tree kill. Dotlink stops feeding stdin and lets the command's own exit status decide success; other stdin writer failures remain fatal.
- Fully acknowledged compatibility-job output is deleted immediately; native-task spools compact to only the exact ordinary-result prefix required by `tasks/get` and retain that bounded prefix until task TTL.
- OpenAI exactly-once request replay now has two explicit layers. A process-lifetime transport ledger claims every tool explicitly marked `ToolSpec.request_replay = true` before dispatch, caches its terminal response once as serialized `Bytes` and waits/replays concurrent copies. Each `Ready`/`Uncertain` terminal POST holds a replay delivery lease so expiry cannot remove or transform that generation during the network await; successful delivery commits the outer 120-second delivered tombstone before that lease is released or nested shell state is acknowledged/retired, while failed/cancelled delivery releases the lease and applies any deferred expiry. Replay keys are scoped by endpoint+tunnel+organization+request id and intentionally exclude the Runtime API key; each record additionally binds a canonical SHA-256 of the complete semantic MCP request: the JSON-RPC body plus every effective forwarded MCP protocol/routing header. Forwarded headers are normalized once by the same validation path used for embedded dispatch, so dropped or empty header groups cannot split replay identity. Public MCP `_meta` (including client capabilities/protocol metadata), MRTR fields and future request parameters participate automatically; only locally-derived `io.abird.dotlink/*` keys in the exact top-level `params._meta` object are excluded; nested lookalikes remain semantic. The same hosted request id therefore cannot authorize a different side effect or replay a response under different protocol/correlation semantics. Replayable response bodies are additionally bound to the prepared execution-authority generation: filesystem/shell/network/cache/host capability changes, prepared object identity changes, shell timeout changes, or output/read/write/task-result ceiling changes drop cached bytes and keep the request fail-closed. Runtime API-key, ngrok-token, OAuth owner-password, and unrelated HTTP auth/bind changes do not alter execution authority. Job-session reuse separately binds the OpenAI endpoint+tunnel+organization namespace so hidden request keys cannot cross logical tunnels. OpenAI control-plane base URLs use one runtime canonical form with trailing slashes removed; tunnel request identity, job-session namespace and management/runtime URLs all use that form, so equivalent endpoint spellings do not revoke durable jobs. Linux sandboxed hidden shell executions additionally bind the live per-command guard generation captured from the exact deny/protected set used to build that command. Cached synchronous delivery and final hidden retirement both revalidate that same generation: a later guard retarget makes cached delivery fail closed, cancellation-requests still-running hidden work, and discards terminal hidden output instead of promoting it to a compatibility handle. It is bounded by 65,536 request identities and a 256 MiB serialized-response budget: each new side effect reserves the hosted 10 MiB worst case before execution, then shrinks to its actual response bytes; an undelivered full body survives for one 120-second hosted replay horizon and then compacts to zero-byte `Uncertain` or reconstructible `RecoverableShell` identity for one additional 120-second safety grace. Final expiry removes non-shell identities; shell retirement is coordinated with `JobManager` and revalidates the stored live guard generation first. Current authority exposes the same execution as a compatibility job; revoked authority cancellation-requests still-running hidden work and discards it once terminal. Retirement then clears the transport key and leaves one final zero-byte `Uncertain` outer tombstone for another hosted replay horizon before removal so the handoff cannot reopen the request as a fresh execution. Retirement uses O(1) queue membership, handles at most 16 items per prune, and stops at the first capacity-blocked handoff, so neither the 65,536 outer identity cap nor the 4,096 inner request-key cap can accumulate permanently; the larger identity ceiling is for cheap identities/tombstones so the guard does not become an artificial throughput cap; reservation accounting is incremental and response-body/delivered/retired expirations use one FIFO deadline queue, while changing endpoint/tunnel/organization releases cached old-namespace response bytes and retains only 120-second fail-closed retired tombstones. The hosted dispatch deadline bounds replay-protected response attempts, so stalled work releases tunnel admission at the response window. A non-shell mutation that may have crossed commit becomes `Uncertain` with an explicit do-not-auto-retry 502; a timed-out/cancelled shell or `job_start` becomes `RecoverableShell` and may retry only while the current `JobManager` still owns that exact request key. Inner shell delivery now distinguishes `Pending` from committed synchronous/durable response state, so a redelivery admission/capacity error is never published as that durable request's terminal response and cannot retire the original request key; the outer request returns to `RecoverableShell`. Embedded shell-response failures after commitment are reconstructed from that owner and positive delivery of the committed representation finalizes the inner key; cached durable handles are never replayed after their owner is revoked. Within that shell layer, response representation is one atomic first-writer-wins disposition (synchronous result, compatibility job, or native Task). Live response attempts are tracked separately from retention: dropping the last active keyed attempt moves an unresolved execution to transport-only retention, preserving dedupe/execution/spools without consuming one of the 256 ordinary retained-job slots; the first later redelivery must atomically reacquire a slot before waiting or handoff, failing early if capacity is full. Dropping an unkeyed local call exposes a recoverable compatibility job. Keyed synchronous replay is transport-only as well. Outstanding shell request keys remain exempt from generic job TTL until positive delivery or final coordinated replay retirement, shell request-key state plus cleanup-only retries share the separate 4096-entry transport-retention ceiling, and retained spool bytes stay under `job_spool_bytes`. Positive delivery removes hidden synchronous state after cleanup, while an exposed job/task receives a fresh full user-visible retention window so a delayed receipt cannot arrive already expired.
- Completed shell output is rendered from durable spools under the shared MCP result budget; a terminal result that cannot fit hands off the same completed job/task for paging instead of failing after side effects. Escape-heavy `read` pages likewise shorten at whole-line boundaries to fit the serialized MCP result budget.
- Active transports now derive one shared constraint set. Private OpenAI transport constraints are authenticated with a redacted opaque per-runtime capability token; the adapter clears the full reserved `io.abird.dotlink/*` metadata namespace before inserting trusted values, so stdio/local-HTTP clients cannot spoof current or future sync/result/replay fields. Native Task creation uses the tightest result budget across every active transport sharing the runtime. The authenticated result budget now also covers native Task RPCs; `tasks/get`/`tasks/update`/`tasks/cancel` bound protocol errors, and completed task results are checked against the same ceiling. Before rmcp, the embedded adapter rejects reflected/routing body strings above 4 KiB (including JSON-RPC id/method, protocol version, name/URI/taskId and schema-routed parameters) and suppresses oversized/malformed routing-header groups, preventing pre-handler mismatch diagnostics from overflowing the hosted envelope. Synthesized tunnel fallback errors use the same bounded JSON-RPC id sanitizer, so exceptional paths cannot re-reflect oversized or malformed caller ids.
- Serialized result budgeting now covers `grep` content/files/count and `ls` as well as read/bread/shell/job pages; escape-heavy discovery results page at whole-record/whole-line boundaries before the MCP envelope overflows.
- The shared `call_tool` boundary now enforces the trusted serialized-result ceiling on every tool result and protocol error, including arbitrary diagnostics; oversized errors are explicitly shortened, while an unexpected oversized success keeps success status to avoid unsafe side-effect retries.
- Runtime authority is prepared once into an immutable `PreparedMachine` and the same object is both fingerprinted and consumed by `LocalMachine`, removing the recovery resolve/fingerprint/re-resolve race. Shell and Bubblewrap are prepared as executable capabilities too: canonical path, native source identity and executable SHA-256 participate in the fingerprint; Linux execution consumes the sealed snapshot bytes rather than reopening the mutable source pathname, and sandbox shell materialization uses `--perms 0500 --ro-bind-data`. Descriptor-backed Unix authority identity is now fail-closed through one shared native file-identity helper: cache/host/root fingerprinting aborts if an already-pinned descriptor cannot be identified instead of serializing nullable object identity. Linux developer caches are now descriptor-pinned during that preparation, fingerprinted from the pinned inode and mounted from the same descriptor on every shell call; a changed/disappeared cache pathname fails closed before launch. User-controlled implicit runtime aliases are typed and constrained, with `~/.nix-profile` allowed only when its final target stays below `/nix/store`. Unix/macOS grant fingerprints come from pinned `SecureFs` root descriptors, Windows authority uses volume/file-index identity, preparation failure revokes the old session, and required intent/policy binding no longer hides side effects inside `debug_assert!`.
- `grep` content escapes grammar-breaking or invalid-UTF-8 path labels, and candidate retention now matches the actual 4096-file production scan budget instead of retaining the unreachable 65,536-path hard cap; work-budget/incomplete semantics are preserved.
- Automatic job-session recovery fingerprints the resolved deny/protected guard identities, so a symlink retarget revokes old jobs even when the configured pathname is unchanged. Fully acknowledged compatibility metadata is reclaimable under capacity pressure, and transient spool cleanup/compaction failures remain pending and retry until successful; `job_get` reports actual retained bytes after native-task compaction.
- Multi-line patch replacements now retain each removed line's LF/CRLF separator by position, regardless of whether additions precede removals. Unequal groups reuse the last removed line's style for excess additions; pure insertions use their insertion site. All three hunk execution paths share the same borrowed action iterator and byte preflight.
- EOF appends after a literal terminal CR now add a separate delimiter CR before LF, so the original byte stays content instead of becoming part of CRLF syntax. The same protection applies to newly added lines ending in a literal CR. Planning accounts for the bridging byte, and a line becoming the unterminated final line updates its separator metadata.
- Patch additions now choose line endings per contiguous edit group from the actual removed source line, not an unrelated leading context line. One borrowed, allocation-free hunk-action iterator is shared by size preflight, direct final output, and in-place multi-hunk updates; explicit context cannot accidentally change mixed-LF/CRLF replacement bytes.
- `*** Add File` uses one borrowed line decoder for both byte counting and output. Bare blank lines and explicit `+` blank lines in CRLF patches now create the same CRLF separator, including whitespace-only shorthand, without per-line allocations or a hidden one-byte write-ceiling discrepancy.
- Patch update lines now distinguish CRLF syntax (identified by the patch header) from literal trailing CR content in LF patches. LF/CRLF added-line terminators are normalized to nearby source delimiters without dropping a literal CR or introducing mixed separators when appending to an unterminated CRLF file. Shared line output and size accounting include the additional bridging delimiter byte.
- One 30-second mutation-lock deadline now starts before the first in-process path mutex and is carried through the Unix directory or Windows named-mutex acquisition. Waiting behind another request can no longer silently extend that lock deadline; failed multi-path acquisitions release their earlier mutexes automatically.
- Dense deep-offset `grep` pages cache only the bounded surrounding lines of selected hits in one source traversal; output-fit retries render these borrowed lines rather than rescanning the entire file up to 50 times. Existing hit/page/context semantics are retained.
- EOF-constrained patch hunks now select only context matches that actually end at EOF before counting ambiguity or falling back to trimmed matching. An addition-only update hunk may append to an existing file when explicitly anchored by `*** End of File`; unanchored context-free insertions remain rejected.
- Dense `*** Update File` additions no longer require both a replacement-sized and a resulting line-index allocation. The final hunk writes directly to one output buffer; earlier hunks shift their index in place using only the consumed source span. The shared 64 MiB planning budget remains enforced without raising it.
- Mutation concurrency now uses exact per-target in-process locks instead of 256 hash shards. Unrelated files cannot collide and spuriously serialize; a weak-reference registry reclaims idle mutexes, while Unix hierarchical directory locks and Windows cross-process mutexes remain unchanged. Added a regression deliberately choosing two paths that previously hashed to the same shard.
- `patch` no longer mistakes the artificial empty tail from `split('\n')` for an actual blank line. It matches only real lines while preserving LF/CRLF and trailing-newline state, including empty files, actual blank final lines, and deletions at an unterminated CRLF file end. No context-only hunk builds a duplicate replacement index. Matcher context references are pointer-sized, with the KMP failure table charged to the existing scratch plan before allocation.
- CRLF patch updates to an unterminated final line now inherit the adjacent delimiter style; normalization and exact-output-byte checks account for a separator CR that becomes nonterminal after deletion.
- Profiles with no persisted filesystem grants (including a shell preference without a stored read root) are valid defaults. Minimum effective read/write/shell capabilities are enforced once after merging per-run CLI additions and denies, rather than rejecting usable one-run configurations during profile validation. An ungranted runtime still fails closed.
- `patch` searches hunk context with a shared linear-time KMP sequence scanner instead of re-comparing long prefixes at every source line. Exact matches still win over trimmed matches; overlapping candidates, ambiguity previews, anchors, and ordered hunks retain their original behavior.
- `bedit` now detects byte-identical, length-preserving ranges before staging. It revalidates the pinned source digest and returns a bounded `changed=false` receipt without writing a whole temporary file; shifting edits and edits that are not provably identical still use atomic streamed staging.
- `bedit` retains its documented 64 MiB source/result ceiling with a streamed, descriptor-pinned update: source and output SHA-256 verification, original-offset range splicing and atomic staging use fixed-size IO buffers rather than two full file copies. Both Unix and Windows staging use the same write-producer API, preserve file permissions, and remove failed temporaries. Identical edits preserve the original mtime.
- `patch` borrows unchanged source lines instead of allocating one owned string per line. Scratch line-index bytes are charged independently of the resulting file's write limit; projected line-index and output growth are checked before each hunk allocates replacements. Dense-line files no longer fail merely because a small write ceiling was confused with the temporary-memory budget.

## [0.13.1] - 2026-10-28

### Changed

- Multi-file text `edit` now permits 512 operations and 64 distinct files per call, both from two shared constants. Its wire decoder, advertised `maxItems`/description, planner, and TOOL-log preview use the same bounds; invalid oversized batches stop during decoding and cannot trigger quadratic log work.
- Sandbox `--allow-rw=/ --allow-shell` remains an intentional full-filesystem opt-out while preserving Bubblewrap's process/network isolation. Without an explicit root RW grant, fine-grained guards stay strict; user-specified deny rules still cannot be silently bypassed. Unrestricted sandboxed shells intentionally do not mount Dotlink-private path masks; this grants filesystem access to those secrets while keeping the separate namespace/network controls.
- Multi-file mutation planning now uses a 64 MiB aggregate budget for retained source and working buffers; both text and binary edits plan replies within the configured output ceiling before any commit. This prevents success-after-write tunnel 502s caused by oversized receipts.
- Streamable HTTP and embedded tunnel request ceilings now include independent path/framing headroom derived from the 512-edit operation ceiling. With a small `max_write_bytes`, valid multi-edit calls with moderately long paths no longer receive an unexpected 413 before execution.
- TOOL path labels resolve symlinked targets for accurate `./` vs outside-absolute display, falling back to lexical labels when resolution is unavailable; this never changes the access decision.
- TOOL activity uses compact fields: `p`, `d`, then complete quoted/escaped `c` for shell commands, with combined payload bytes `b="N"`, `max_t`, and `max_b`. Paths under Dotlink's launch directory appear as `./...`; outside paths remain absolute (`/...` on Unix). Multi-file `edit` shows a bounded plural `paths=[...]` preview, with omitted counts.

### Fixed

- Consolidated `edit` and `patch` multi-file stage/commit lifecycle in `mcp::tools::mutation`: one verified-update/create-only transaction engine stages all changes before any commit, with strict and best-effort modes. `edit`, `patch`, and `bedit` share one 64 MiB source/work-buffer ledger; text/binary `write` and `bwrite` use the same pre-write response-size discipline. A successful mutation cannot exceed the configured response budget solely because of its receipt; multi-file replies retain exact counts with `truncated`, `omitted_files`, and `omitted_failures` when necessary.
- `edit` counts exact-text matches without collecting every match offset; `patch` counts ambiguous hunk locations without unbounded candidate vectors and charges its temporary source-line index to available plan work. `patch` enforces its 32-section ceiling during parsing; `bedit` enforces its 32-range ceiling during deserialization instead of after constructing an oversized array.

- `ls` and `grep` now inspect children under the pinned directory's full authority: Linux uses `openat2` with `O_PATH|O_NOFOLLOW` and the root's mount-resolution mask; macOS uses no-follow `fstatat` with the same device boundary as its native openat walk. Neither pathname races nor prohibited mountpoints leak outside metadata. Windows retains its delete-sharing directory pins.
- Sandboxed shells reject overlapping user *and implicit runtime* mounts that expose a protected/read-denied path (even read-only binds) or make a write-denied path writable. A host-side rename can bypass a pathname mask both after preflight and after the sandbox starts, even with an existing direct-child deny mount; rejecting conflicting live directory binds at the common policy boundary closes both races. Narrow the grant to the directories actually needed and keep private config/credentials outside runtime mounts; native file tools continue honoring fine-grained denies. Runtime bind/alias plans are shared between policy checks and mount assembly, while cache/host integrations retain their own overlap checks.
- OpenAI rejection-window runtime-restart regression test uses a two-second confirmation window, avoiding scheduler-load flakiness from a 200 ms assertion.
- Multi-file text `edit` advertises only `edits: [{path, old_text, new_text, replace_all?}]`, with each operation's path required; a legacy top-level default `path` is still accepted at deserialization for existing callers. TOOL logs show the deduplicated `paths=[...]` target set once, without a redundant singular `path` field.
- Shared TOOL summaries put `path`/`dir` before limits and offsets for every tool. Opted-in TOOL logging (`-v`, including NixOS `logging.verbosity = "tools"`) records the full `bash`/`powershell` command as one losslessly escaped line, restoring the intentionally auditable command history; other payloads remain byte counts.

## [0.13.0] - 2026-10-27

Native macOS and Windows filesystem tools are the focus: macOS directory
enumeration never worked, and tool output depended on the host separator.

### Fixed

- Native macOS filesystem tools enumerate directories through the pinned descriptor
  with `fdopendir` instead of `/dev/fd`, which macOS rejects with `ENOTDIR`; the
  buffered names stay inside the walk's own entry ceiling. `grep` and `ls` were
  unusable on macOS before this.
- Tool replies name paths with `/` on every platform, so the same request produces the
  same `ls`, `grep`, `edit` and `patch` output on Linux, macOS and Windows.
- Cache sandbox targets and their `CARGO_HOME`/`GRADLE_USER_HOME` values keep their
  POSIX spelling inside `/tmp/home` on every build, and a profile cache target is
  validated as a sandbox path rather than as a host path.

### Added

- `mcp::shell_tool_name()` is the single answer to which shell tool this platform
  advertises, so a new shell variant cannot be missed by the schema rewrite, the
  tunnel's delivery reserve or the TOOL log.

### Changed

- Native platform tests now run to completion on macOS and Windows, so a tag produces
  a published release instead of stalling on a failed platform job.

## [0.12.0] - 2026-10-27

### Changed

- Unconfigured services stop cleanly. A service with no saved profile, no explicit
  configuration and non-interactive stdin now exits `NOTCONFIGURED` (6) instead of
  `CONFIG` (78), prints a notice rather than an error, and carries a supervisor hint
  that the stop is clean. The NixOS unit declares `SuccessExitStatus=NOTCONFIGURED`
  and `RestartPreventExitStatus=2 CONFIG NOTCONFIGURED`, so a host that has not run
  `dotlink --setup` yet leaves the unit inactive instead of failing activation and
  rolling back a fleet deploy. Invalid profiles, policies and credentials still exit
  `CONFIG` and still fail loudly.

## [0.11.0] - 2026-10-27

### Changed

- OpenAI tunnel polls hold for 30 s instead of 15 s (`timeout_ms=30000`), halving idle poll churn; the HTTP request timeout stays derived from the hold and rises from 25 s to 40 s.
- Runtime failures that retrying unchanged cannot fix exit with their standard status: 78 (`CONFIG`) for configuration, policy and credential content, and 2 for command-line usage errors. An OpenAI Runtime key or tunnel rejection counts as permanent only after persisting for 60 s; a single 401/403/404 keeps retrying, like an auth outage. NixOS services leave 2/78 stopped and journal how to recover; every other process termination, including exit 0 or clean signals outside an explicit systemd stop/restart job, restarts after 2 s doubling to 10 minutes. The module requires systemd 255 or newer; ten minutes of uptime reset backoff on 258 or newer, while 255–257 emit a warning. One-shot commands keep exit status 1.

### Fixed

- OpenAI tunnel polls stay within the control plane's 25-command batch maximum; larger internal tuning is rejected before startup instead of making every poll fail with HTTP 400.
- Ngrok connection errors name the token only when ngrok answers the authentication step with a rejection, not for a network failure on the way.
- After initial profile planning, Ctrl+C and SIGTERM register once for the process, so a signal arriving while a runtime is rebuilt, stopped or restarted in-process exits gracefully instead of being lost; initial interactive setup retains ordinary Ctrl+C behavior.
- NixOS supervision no longer leaves the long-running service inactive after a handled exit 0 or clean signal; explicit systemd stops and restarts remain authoritative.
- Cache-discovery probes no longer inherit dotlink credential variables, and no child inherits systemd notification variables.
- Protected control-plane paths and deny rules keep lexical and resolved identities, including dangling-symlink targets resolved in kernel order; unlinking or retargeting a deny alias no longer frees its name. Writable sandbox ancestors, cache/host remapping, and runtime aliases such as `/bin -> usr/bin` can no longer bypass protection or filesystem denies: guarded paths in writable binds need an immutable deny boundary, runtime and network paths are bound once by pinned descriptor at their canonical path, and deny-write now blocks socket integrations such as the SSH agent.
- Sandbox policy conflicts and resource limits below their minimum exit 78 (`CONFIG`) instead of restarting forever under systemd.
- An OpenAI key or tunnel rejection keeps its 60 s confirmation window across in-process runtime restarts, so interleaved 5xx failures cannot postpone operator action indefinitely.
- Interactive setup and profile creation now derive editor defaults and their commit baseline from one profile snapshot, so a concurrent edit cannot become the baseline for stale answers.

## [0.10.0] - 2026-10-27

### Added

- Config/secret commits now use immutable key and OAuth-owner credential generations, atomically selected by JSONC; older profiles retain their legacy key/hash. Crash-before-commit leaves the previous selection intact.
- Shared bounded discovery scanning returns explicit `scan_incomplete` metadata rather than unbounded traversal; partial sorted results never claim global completeness.
- Documented that local HTTP/stdio requests intentionally have no global admission budget; clients control parallelism.

- `grep` searches UTF-8 text files with a regex or literal pattern, `glob` filters, optional `ignore_case`, and three output modes: `content` (matching lines with context and `--` group separators), `files`, and `count`. Paging uses `offset`/`limit` (200 hits by default), and `gitignore`/`skip_hidden` are opt-in. It walks with the same descriptor-rooted enumerator as `ls`, so search stays inside filesystem read authority and never reaches shell-only grants.
- `edit` accepts an ordered `edits[]` array of `{path?, old_text, new_text, replace_all?}` across up to 16 files with an inherited top-level `path`. Entries apply sequentially per file, each `old_text` must be unique unless `replace_all` is set, and every file is re-hashed before writing: any concurrent change or failed entry writes nothing. A failed `old_text` that starts with `digits + tab` reports that `read`'s line-number prefix must be stripped.
- `patch` applies the Codex `*** Begin Patch` grammar: mandatory header and terminator, `*** Update File` hunks located by content (exact, then `trim_end`-tolerant, ambiguity failing), `*** Add File` as create-only, `*** End of File` enforced (the hunk must end at the last line, with no hunk after it), and explicit rejection of `*** Delete File` / `*** Move to`. Strict mode plans and verifies every section before any commit; each file replacement is atomic, but the commit sequence is not a multi-file transaction. `best_effort` writes the files that planned cleanly and reports the rest.
- `bedit` edits many byte ranges at once. Offsets are relative to the original file, ranges apply ascending with delta tracking, overlaps and out-of-bounds ranges are rejected, and an optional `sha256` precondition gates the write.
- `ls` takes `pattern` (one `globset` grammar everywhere: `*`, `?`, `[...]`, `{a,b}`, `**`), `depth`, `kind` (`any`/`file`/`dir`), `sort` (`path`/`mtime`), and `limit`, returning `{entries, truncated}`. `depth` defaults to 1, to the pattern's own path-segment count, and to unbounded for a pattern with `**`.
- `read` prefixes each line with its 1-based number and a tab by default (`numbered: false` opts out) and defaults to a 4096-line window that ends with one `… truncated at line N` marker.
- Shell calls emit `notifications/progress` heartbeats while running when the client supplies a progress token: a monotonic counter with no total and no message, so long commands stay visibly alive on streamed transports.

### Fixed

- Host validation now wraps the complete merged HTTP router, so OAuth discovery, registration, authorization, token and revocation routes receive the same exact listener/public-origin authority checks as MCP. Forwarded headers are ignored, malformed/conflicting authorities return 400, well-formed disallowed authorities return 403, and IPv4-mapped IPv6 addresses normalize before wildcard/loopback checks. Ngrok connections are served directly with their public authority and authenticated edge peer metadata instead of losing requester identity through a loopback forwarding backend; requester `Debug` output is redacted.
- Private profile, OAuth-state and Runtime-key writes are serialized and checked against the same ceilings as their readers before atomic replacement. Unix private-file reads open nonblocking and reject FIFOs/devices, preventing startup hangs and persisted files that a later process cannot reload.
- OAuth state overflow preserves the previous readable file, speculative refresh-index contention falls through to bounded authoritative work instead of queuing before admission, and normal invalid/capacity-limited authorization traffic no longer amplifies warning logs.
- `.gitignore` work accounting charges every non-absent read attempt and bytes returned before an I/O failure; the 256-file/4 MiB ceilings remain real on faulty or adversarial filesystems. Incomplete text results now state neutrally that unavailable rules may include or omit matches.
- Shell heartbeat delivery is clipped to the command's absolute deadline as well as its 100 ms send budget, so transport backpressure cannot postpone timeout handling.
- Network HTTP/ngrok retains Streamable HTTP SSE keepalives across idle intermediaries; only the in-process OpenAI adapter suppresses wire keepalive/retry events. Outermost HTTP middleware drops accepted MCP/OAuth handlers and body extraction on shutdown; accepted IO force-closes after the drain bound, and bounded ngrok tunnel/session close runs concurrently with draining inside a six-second parent shutdown budget. A drain timeout is a cleanup failure because synchronous handler work cannot be preempted; cleanup failures take precedence over restart requests and prevent replacement-runtime overlap.
- OpenAI tunnel log metadata is restricted to bounded protocol identifiers; all log scopes are sanitized/capped, grep patterns are recorded only as byte counts, and control-plane response bodies or malformed local SSE payloads are excluded from propagated errors. rmcp tracing is suppressed regardless of `RUST_LOG` because upstream debug events render complete MCP payloads.
- SEP-2243 network HTTP and embedded OpenAI calls reject unknown policy tool names before rmcp's negative schema cache can retain attacker-controlled names; the HTTP preflight enforces the same exact request-body ceiling.
- Paged `read` now places its 32 MiB skipped-input limiter beneath buffering, so prefetch cannot exceed the work ceiling and already-buffered selected-window bytes remain available.
- Authorization-code redemption uses a short exclusive claim instead of blocking Tokio workers on a mutex held across durable state I/O. Duplicate exchanges shed immediately without consuming global admission; failed/cancelled issuance releases the claim, successful issuance consumes it once, and access-token poison recovery is consistent across publication, validation and revocation.
- Anonymous OAuth registration and authorization no longer evict unexpired pending work: shared admission reclaims expired entries, enforces every redirect URI/client share plus a four-entry requester share within each group, limits each trusted network requester to eight entries per store, preserves the 256-entry global ceilings, and returns HTTP 429 on overload. Requester identity comes only from accepted TCP or authenticated ngrok tunnel metadata, never caller-selected OAuth values or forwarding headers. Consent POSTs claim slots exclusively for their entire lifecycle; cancelled/erroring requests release the claim while preserving the form for retry, and successful/denied requests consume it once.
- CIMD metadata-fetch throttling is requester-aware instead of attributing work to caller-selected hostnames: each trusted requester gets 12 starts per rolling minute and two in-flight operations under the process-wide four-request limit. The bounded 256-requester table rejects unseen identities when full without evicting live rate history. Disallowed literal IPs are rejected before admission. A cancellation-safe async system resolver replaces detached OS lookups, so DNS timeout/request cancellation releases the complete admission lease immediately while preserving all-address validation and HTTPS pinning; missing system DNS disables CIMD without blocking registered-client OAuth.
- The shared directory-walk error classifier uses target-native `libc::EXDEV`/`libc::ELOOP` errno values instead of Linux numbers, preserving best-effort error reporting on macOS. Windows mutation locks deduplicate and order keys by invariant UTF-16 case-mapped identity instead of mismatched Rust Unicode folding, their diagnostics avoid tracing macro name capture, Windows handle failures convert to the common error type, and mode preservation uses the target's native `mode_t`; all five release cross-targets compile again.
- Shared directory traversal reports unreadable descendants and metadata failures via `WalkProgress`, distinguishing `traversal_error`, `directory_budget` and `ignore_rules`; `ls` and `grep` consume the same status instead of each guessing completeness. Compatibility `incomplete_reason` keeps its first cause while `incomplete_reasons` preserves all concurrent causes. Grep also reports files that disappear or become unreadable between discovery and content scanning.
- Public OAuth client lookup distinguishes local `dcr_` IDs from HTTPS metadata clients: one bounded, version-checked read-only state index handles approved clients and refresh grants, rather than reparsing persistent state for arbitrary client IDs. Cross-process atomic changes invalidate it; definite misses, version races and stable malformed state are distinct, so stable failures return retryable unavailability without repeated authoritative parsing. State-file reads and writes are limited to 16 MiB, covering the largest state produced by earlier schema-v1 limits.
- Public OAuth errors do not echo underlying private file paths, serialization failures, or diagnostics. Local logs retain details; client errors distinguish retryable state unavailability from invalid requests.
- Private control-plane text reads (OAuth state, profile JSONC, saved Runtime key and file-supplied credentials) share descriptor-first regular-file validation and streaming byte ceilings: 16 MiB OAuth, 4 MiB profiles, 16 KiB credentials. Active Runtime/ngrok credential files are opened once and carry a descriptor-verified protected target into execution; dormant files stay unopened, and unusable saved keys remain replaceable through profile management. Windows OAuth state caching identifies atomically replaced files by volume/file index rather than timestamps alone.
- Owner authorization retries use one bounded pending-request store: transient password-state, storage, or code-creation failures restore the claimed request rather than burning the owner's consent form; wrong-password retries also check that the new request was stored successfully. Password verification admits six rolling-minute starts per trusted requester under a two-worker process-wide bound shared across runtime generations, charging both the consent creator and password POST source with non-evicting requester history. Expired registrations and approved-client capacity failures consume the stale form with actionable errors.
- Authorization-code, access-token, and refresh-grant stores prune expired records but never evict live credentials to make room; access tokens have a 64-entry per-client share within the 512-entry process bound, duplicate code/refresh exchanges shed before global admission, and retryable capacity failures preserve the original authorization code or refresh grant. A code for an authoritatively revoked DCR client is consumed as terminally invalid.
- OAuth client display names neutralize terminal controls, line separators, and Unicode bidi controls before owner consent or management output. CIMD rejects deprecated 6to4 relay-anycast IPv4 space while preserving IANA's globally reachable 6a44 exception.

- Anonymous OAuth token exchange/revocation now rejects definite unknown grants before taking persistent state locks or consuming admission slots, using a version-checked, read-only hashed refresh-grant index. Atomic changes by other dotlink processes invalidate the index; ambiguous snapshots fall back to the authoritative transaction.
- `grep(gitignore=true)` distinguishes absent ignore files from inaccessible/denied rules: JSON replies report `scan_incomplete=true` / `incomplete_reason=ignore_rules`, and text results include an explicit warning without silently claiming correct filtering.
- Shared test machine fixtures now initialize the native descriptor filesystem on macOS, and Unix-specific filesystem tests are compiled only on Unix; Windows/macOS CI can exercise their native implementations.
- HTTP MCP now validates explicitly trusted listener/public-origin Host names. Built-in ngrok connections are served directly under the exact public tunnel authority; generic forwarding headers remain untrusted. Public OAuth issuer/resource remain independent of incoming Host headers; unauthenticated wildcard listeners now fail early rather than silently rejecting every real IP Host.
- Public OAuth token exchanges reject malformed opaque tokens before state work and limit concurrent operations. Durable refresh changes and in-memory access-token publishing no longer hold the bearer-validation mutex across disk I/O.
- Shell progress heartbeats time out after 100 ms without spawning detached work, so backpressure cannot delay command deadlines, cancellation or child reaping indefinitely.
- Opt-in `.gitignore` searches reject oversized files (>64 KiB or 1024 lines) instead of silently applying partial ignore rules; traversal accounts for at most 4 MiB and 256 ignore files in total.
- OAuth revocation preserves its two concurrent-work limit but drops the 60-per-minute anonymous global quota, which previously allowed invalid requests to lock out legitimate revocation for a full minute.
- The ngrok HTTP supervisor owns both local/backend server tasks, cancels live requests even when aborted, and drains/aborts children on every normal or failed forwarding exit. A restart cannot detach old HTTP listeners.
- Public OAuth revocation refuses malformed token shapes without reading state and bounds concurrent state work (2 active). In-memory access-token revocation skips persistent disk transactions.
- `read` bounds skipped-line scanning to 32 MiB per call independently of `max_read_bytes` and names the remedy when a distant offset would require more I/O.
- `--list-tools` uses configuration-only policy inspection: no Runtime key, OAuth password, SSH agent, host device, sandbox initialization or private-state creation is needed. Actual startup still checks all execution prerequisites.
- --print-id uses configuration-only inspection, without loading secrets or triggering setup.
- Text tools share UTF-8 and binary-control-byte classification: read/write/edit/patch reject binary content and grep skips it; byte-oriented data stays with bread/bwrite/bedit.
- OAuth authorization codes are consumed only after token issuance succeeds under the code lock; a storage failure returns retryable HTTP 503 without burning the code, and fallible token setup precedes persistent refresh mutations.
- grep limits per-call content I/O to 32 MiB across 4096 candidates, reporting incomplete coverage and its cause independently of local transport concurrency.

- Live OAuth owner-password rotation is applied to new approvals without an HTTP restart: verification uses the currently committed profile generation, checks it again after a successful hash, and rejects fallback to legacy after a saved profile is removed.
- Runtime secrets resolve by explicit environment and `DOTLINK_API_KEY_FILE` precedence before persisted keys. Local-only profiles never open dormant Runtime keys, and valid overrides work when the selected saved key is missing.

- Profile inspection and editing recover from missing selected Runtime keys without relaxing OpenAI startup validation. Retry interrupted profile deletion even when only generated key files remain.
- Safely retire obsolete Runtime key and OAuth owner-hash generations after a durable credential rotation; keep selected and prior credentials (also through unrelated profile edits), and treat post-commit cleanup failure as a warning.
- OAuth password-verification concurrency permits now remain with blocking Argon2 workers after request cancellation.
- Validate refresh-token and authorization-code client/resource/PKCE bindings before consuming their grants.
- Prepare both owned private XDG directories before assembling writable sandbox mounts; unknown absent protected paths still fail closed.
- Sanitize protocol identifiers for both stdio and tunnel diagnostics through one logging primitive.

- Stdio verbose logs never inspect nested request payloads for a method and sanitize all method names.
- Sandbox protection fails closed on missing protected paths under writable mounts rather than letting Bubblewrap create host-side mount targets. Dotlink privately prepares its own OAuth state directory; existing protected files/directories remain masked.
- Text reads error if a truncation marker would displace the only returned line; never emit line zero.
- Shell request cancellation aborts all pipe workers; Windows children start suspended and join their Job Object before running. Unsandboxed Unix group cleanup explicitly does not claim to contain `setsid` descendants.

- Private config/key/OAuth writes now treat atomic replacement as the commit boundary: a failed parent-directory sync cannot trigger a credential rollback after the file is already installed. The empty temporary is permission-hardened before any secret bytes are written.
- Windows private state uses a protected user/SYSTEM/Administrators ACL, and the PowerShell installer restores a backup when safe or preserves recovery artifacts on partial replacement failure.
- Unix native staging/commit and OAuth persistence run as cancellation-safe blocking sections rather than stalling Tokio workers; OAuth state locks have a finite acquisition deadline.
- OpenAI tunnel progress notifications are decoded and delivered incrementally before the command completes, through a bounded queue rather than a buffered terminal response.
- Shell timeouts and request cancellation terminate Unix process groups or Windows Job Objects, without preventing successful commands from launching background work.
- `ls` and `grep` select globally sorted candidates with bounded top-K heaps; the truncation and pagination contract is independent of directory enumeration order.

- Consolidated the signed stale-index correction into the preceding transport commit, preserving the tree and removing the broken intermediate commit.
- OpenAI tunnel poll bodies now fit a configured largest single write plus framing, subject to a 256 MiB safety limit and explicit oversized-profile startup errors.
- macOS native files use pinned-root, component-wise openat/O_NOFOLLOW for authorization, writes and walks instead of unchecked pathname reopens.
- Windows native files pin ancestor directory handles and refuse reparse points; named cross-process kernel mutexes coordinate same-path writers with crash-abandonment recovery.

- Mutations take ordered intention locks on existing ancestor directories and an exclusive lock on their deepest existing parent. This fixes execute-only-directory lock bypasses without breaking automatic nested parent creation or cross-process concurrency in separate subdirectories.
- The OpenAI tunnel's embedded MCP response ceiling is derived from `max_read_bytes`/`max_output_bytes`, including ordinary JSON escaping, both independently capped shell streams' nested escaping, and framing, instead of silently rejecting valid reads over 16 MiB.
- Non-Linux staging now owns each temporary from the moment of creation through async writes and sync. RAII cleanup runs on cancellation, and the platform-neutral path-backend tests run under Linux CI too.
- `bread(format=mcp)` percent-encodes each resource-URI path segment so spaces, `#`, `?`, `%` and Unicode do not corrupt the resource identifier.

- Removed the Linux OFD record-lock fast path and its `/run/lock` dependency. Cross-process coordination now uses only directory `flock` (shared ancestor intent plus exclusive parent), while in-process shards retain parallelism.
- OAuth client metadata fetches ignore proxy environment variables, bound DNS lookup time, and reject anonymous requests above four concurrent or 24 per rolling minute.
- OpenAI tunnel poll envelopes, local MCP responses, and OAuth metadata use a shared streaming byte ceiling before deserialization (64 MiB per tunnel poll); control-plane error reads are bounded as well.
- Windows replacement now uses a named backup with `ReplaceFileW`; documented partial-failure states restore the backup or preserve recovery artifacts instead of deleting the last recoverable file.
- Linux atomic writes explicitly guarantee complete-file visibility; parent-directory `fsync` failures are logged without misreporting an already-committed operation as failed.

- `read` now reports an error for the first nonexistent line after a final newline, including CRLF and blank-line files, while preserving empty-file reads and unbuffered skipped-line scanning.
- Windows atomic replacement now copies existing DACLs to the empty temporary before writing and uses `ReplaceFileW` without disabling ACL preservation; new-file installs use no-overwrite `MoveFileW` instead of requiring hard-link support.
- TOOL and REQ logs redact all shell command bodies as `command_bytes`, so a short command containing an inline credential cannot leak into the journal.

- `edit` reports a signed `line_delta`; a file that shrinks no longer returns an underflowed count.
- `read` clamps `limit` to at least one line, so `limit: 0` cannot produce a `truncated at line 0` marker.
- `bedit` bounds the total decoded payload size, not just each range, so a multi-range call cannot pin more than one patchable file in memory.
- `grep`/`ls` skip directories, entries and mount points they cannot inspect (`EACCES`, `ENOTDIR`, `EXDEV`, `ELOOP`) instead of failing the whole search, `grep` refuses to read a `.gitignore` that policy denies, and an unreadable or invalid `.gitignore` in an unlistable directory no longer fails the call.
- `grep` paging stops at the first refusal and selects exact hit records, including the 50-hit per-file cap. `content` with context clips a page's context at the next/previous matching line so a later page does not repeat a hit as context; the prose marker names the actual stop reason (`hits`, `bytes`, or `files`). `count` reports `page_total` and both JSON modes are measured after all derived fields are added.
- `grep` holds at most one file open at a time, so a search over a tree with thousands of directories no longer fails with `EMFILE`, and its `files`/`count` replies stay inside `max_output_bytes` even when paths render longer than they are (escaping included).
- `patch` refuses a path claimed by two sections, including two spellings of one file through a symlink, keeps the target file's CRLF style for added lines even when the patch itself is LF, accepts `@@ -3,1 +3,1 @@ anchor` with both numbers and text, and treats a `*** Add File` target that is not simply absent as an error.
- `read` with `numbered: false` returns the window byte for byte (CRLF and a trailing newline survive), and an offset past the end of the file is an error instead of an empty result. `read` streams line by line, so a small window works on a file larger than `max_read_bytes`; the ceiling bounds the whole reply (window, marker and one line), and a capped window ends with `… truncated at line N`.
- `grep output_mode=count` reports exact per-file totals; the 50-hit cap now only bounds `content` mode. All three modes stop collecting at `max_output_bytes`, so the ceiling bounds memory as well as the reply.
- `patch` refuses a path claimed by two sections (`*** Add File` included) while planning, so strict mode never writes one copy and then fails; a `*** Add File` target that is not simply absent is an error rather than "absent", and a no-op update stays out of the receipt.
- `bedit` re-checks its `sha256` precondition against fresh bytes before writing, orders an insert before a replacement at the same offset regardless of request order, keeps the original file when the plan is rejected, respects `max_write_bytes` (with the 64 MiB hard planning cap as an upper bound), and reports `changed: false` with `bytes_written: 0` when the result is byte-identical.
- Deny and protected paths are folded lexically before they are resolved, so a deny such as `<dir>/missing/../secret` matches the path it names instead of silently never applying.
- `ls` with a `**` pattern walks without a level cap, matching what `grep` does with the same pattern.
- `patch` with `best_effort` skips and reports a file whose snapshot changed — including one that vanished or can no longer be read — and continues past per-file write failures instead of aborting the call.
- `*** Add File` stages its content beside the target and installs it with a create-only link, so the target is absent until commit and an existing path is never replaced. A failed create leaves nothing behind.
- `patch` and `edit` no longer rewrite a file whose planned text is byte-identical.
- Shell timeouts return a real MCP tool error (`isError`) carrying `timed_out: true`, the failure reason, and any partial stdout/stderr salvaged within a 250 ms grace, instead of a message-only error that discarded the output collected before the deadline.
- Undeliverable tunnel responses are logged at warn instead of debug when the client's `response_timeout` window has already expired.
- `ls` replies now honour `max_output_bytes` as well as the entry limit, matching `grep`; the rendered JSON is measured after every field is present.
- `grep` context paging selects matching lines as page records instead of counting context windows; a page that contains a hit as context no longer makes the next page repeat it.
- `grep content` shrinks the selected hit batch to the largest contiguous prefix that fits the remaining byte budget, instead of blanking a page when a 50-hit batch is larger than the ceiling.
- Shell execution starts stdout/stderr draining before writing stdin, so a child that writes before reading its input cannot deadlock both pipes.
- `read` streams line by line; a small window works on a file larger than `max_read_bytes`, which bounds the returned window and one line. A capped window ends with `… truncated at line N`.
- Local MCP responses are collected with a streaming byte ceiling, so an oversized tunnel response is rejected before the whole body is buffered.
- XDG overrides must be non-empty absolute paths; relative or empty values fall back to the home directory. Config, state, cache and host-discovery paths share one resolver.
- `*** End of File` is accepted in Codex patch updates.
- The OpenAI tunnel applies the delivery reserve to every dispatch deadline, not only shell calls, and retries every 5xx response POST (not just 502/503/504).
- The docs now state that `MAX_LIST_ENTRIES`/`MAX_GREP_FILES` bound collected results, not traversal, and that `grep` re-opens file contents through the descriptor-rooted reader beneath the pinned root rather than through a pinned per-file parent.
- A staged write creates its temporary with a mode **no broader than the target's**, so replacing a `0600` file no longer leaves a group/world-readable `.dotlink-*.tmp` holding the new content while it is written. Applies to the Linux descriptor backend and the non-Linux path backend, which now share one staging policy in `src/staging.rs`.
- In-flight staging temporaries are hidden from `ls`/`grep`, so a listing or a search cannot report a half-installed file or match its bytes.
- `grep content` reserves room for its truncation marker before accepting a hit, so a byte-limited page ends with whole hit lines only; the marker names the per-file cap when that is the reason, and an offset past the last hit answers with `… no hits at offset N`.
- Configured `max_output_bytes`, `max_read_bytes` and `max_write_bytes` are honoured exactly. A value below the supported 1024-byte minimum is rejected when the profile loads (with the number the user wrote), instead of being silently raised to 1024 at startup.
- `read` fits its whole reply inside `max_read_bytes` by dropping whole lines to make room for the truncation marker; a returned line is never cut short.
- `ls` derives its default depth from the pattern instead of a fixed 4, so a pattern deeper than four segments returns its entries instead of a silently empty listing.
- `patch` enforces `*** End of File`: a hunk carrying the marker must end at the file's last line, and a later hunk in the same file is refused.
- A shell `deny-write` that names a single file no longer breaks the sandbox: deny-write roots are rebound read-only by the dedicated pass and never opened as directories.
- A `timeout_secs`/ceiling too large for the monotonic clock returns a tool error instead of panicking on `Instant + Duration`.
- `bedit` breaks a tie between two ranges with the same offset and span by request position, so the applied order is deterministic.
- The descriptor-rooted walk fails closed with an explicit error when procfs is unavailable, instead of reopening the directory by pathname (which a concurrent rename could redirect outside the grant).
- `read` builds its window in one output buffer with whole lines, instead of retaining a `Vec` per returned line: a 1 MiB file of empty lines no longer costs tens of MiB of resident memory, and the page stays bounded by `max_read_bytes`.
- `read` skips lines before `offset` by scanning for their newline, so a line larger than the whole read ceiling no longer fails a window that starts after it. Only returned lines are decoded, so content outside the window cannot make a text read fail.
- The streamable-HTTP request-body ceiling follows the profile's `max_write_bytes` (six times it, plus 64 KiB of framing) instead of rmcp's fixed 4 MiB default, for the local HTTP server and the OpenAI tunnel alike. A write the tools accept is no longer refused with `413` on HTTP while succeeding over stdio.
- The non-Linux staging test module compiles on Windows: its Unix permission assertions are gated instead of importing `std::os::unix` unconditionally.
- `backoff` jitter is expressed in nanoseconds and added with saturating arithmetic, so sub-millisecond jitter is not silently zero and absurd caps cannot panic the retry path.

### Changed

- Binary tools are `bread`, `bwrite`, and `bedit` (`read_binary`, `write_binary`, `patch_binary` are gone, with no alias routes), and `edit` is array-only. Clients must refresh their tool list after upgrading.
- Tool descriptions describe only tool behavior: access boilerplate was dropped because unavailable tools are already hidden, and the `bash`/`powershell` arguments advertise the configured ceilings as concrete numbers instead of naming config keys.
- `max_shell_timeout_secs` and `max_output_bytes` are now policy ceilings, defaulting to 3600 s and 1 MiB. The shell timeout is caller-controlled via `timeout_secs`, clamped to the ceiling, and defaults to the ceiling when omitted instead of a hidden 30 s. `max_output_bytes` also bounds the JSON replies of `grep` and `ls`.
- The OpenAI tunnel threads the client-supplied `response_timeout` into shell calls: an omitted `timeout_secs` defaults to that window minus a 5 s delivery reserve, and a larger explicit value is shortened, so a long command returns a real tool result instead of being dropped at the client window boundary.
- The OpenAI tunnel keeps its timing, retry and concurrency parameters in one validated `TunnelTuning` value: the HTTP request timeout covers the poll hold plus the larger of the connect timeout and the 10 s delivery margin; the activation grace covers one full check budget; and `backoff.cap + jitter` cannot exceed the request timeout. `poll_limit` is held between `max_in_flight` and `3 * max_in_flight`.
- Tunnel concurrency is 16 command tasks with 32 commands per poll (was 8 and 25). A full batch spawns its tasks immediately; they acquire permits inside the task. Up to three batches may queue, so a single burst does not stop polling; a sustained backlog pauses the poll loop as deliberate bounded backpressure.
- `regex`, `globset`, and `ignore` are direct dependencies; `ignore` is used only for `GitignoreBuilder`/`Gitignore`, never `ignore::Walk`.
- Every existing-file write is an atomic replace: a sibling temporary beneath the pinned parent directory is renamed over the target, keeping the existing mode, so no tool truncates a file in place. Create-only writes link the staged temporary into a free name. `edit` and `patch` stage every affected file before committing any, so a write failure changes nothing (the commits themselves are the last step, not a multi-file transaction). Non-Linux uses the same staged shape, preserving mode and rejecting special or symlink targets.
- Mutating filesystem tools take a bounded sharded mutation lock keyed by the resolved path, held from snapshot read through commit. Concurrent edits of one file can no longer both verify the same bytes and overwrite one another; multi-file calls acquire their shard locks in sorted order; independent files usually remain concurrent, and shard collisions serialize. Updates also re-check the snapshot length and digest immediately before commit as best-effort defense against external writers.
- `edit` and `patch` preflight every affected file before any commit. Each file replacement is atomic, but the multi-file commit sequence is not a transaction; a commit-stage failure can leave earlier files replaced, and the error states that consequence. `patch` reports `mode: strict` rather than implying whole-call atomicity.
- `SecureFs` gained `stage`/`stage_update`/`stage_create`/`StagedWrite::commit`, `open_child_read`/`open_child_dir` with the root's resolve mask plus mandatory beneath/no-symlink flags, and `read_resolve`, so the walk descends with exactly the restrictions its root was pinned with.
- The TOOL log reads its argument names from the tool schema only when activity logging is on, so the default quiet path no longer builds the router twice per call.
- Tool implementations moved to `src/mcp/tools/` (one module per family, each with its own `#[tool_router]`) behind a single capability table (`TOOL_SPECS`) that drives visibility, the platform operation gate and the TOOL log; `src/mcp.rs` keeps the policy, the pinned descriptors and the server handler.
- The TOOL log renders the arguments a tool's own schema declares, and payload arguments (`content`, `stdin`, `data`, `old_text`, `new_text`, `patch`) are always logged as byte counts, so adding an argument cannot drop it from the log and a payload cannot reach the journal.
- JSON replies are trimmed to `max_output_bytes` with one binary search over the kept prefix instead of re-serializing after every dropped entry: a pathological 16 000-entry reply drops from minutes to ~140 ms while keeping the identical result.
- The advertised tool router is built once per machine instead of once (or twice, when TOOL logging is on) per `tools/call`; tool visibility, the shell schema-limit rewrite, the tunnel delivery reserve and the TOOL log all derive from `TOOL_SPECS` rather than repeating tool names.
- Both staging backends, and the walk's staging-name filter, share one mode and naming policy in `src/staging.rs`, so the "never broader than the target" rule cannot be fixed in one platform and missed in the other.
- Staging rules, the hard-ceiling contract, the whole-hit paging rule and the procfs requirement are documented in `docs/SECURITY.md`, `docs/ARCHITECTURE.md`, `README.md` and `.agents/docs/configuration.md`.
- TOOL logging intentionally prints a `bash`/`powershell` command as a ≤120-character excerpt (or `command_bytes` when longer): the command is the audited operation. Payload arguments stay byte counts, and `docs/SECURITY.md` now states the exception and its journal consequence explicitly.

## [0.9.1] - 2026-10-16

### Fixed

- Host-mount deny/protected policy is now validated before resolving `bash` or Bubblewrap, so sandbox policy violations fail closed even when the sandbox tooling is absent from `PATH` (restores `nix flake check` in minimal build environments).

### Changed

- Attribute the MIT license to Prasanna Loganathar and dotlink contributors, and document the project's human-led, AI-assisted development posture.

## [0.9.0] - 2026-10-14

### Added

- Linux Bubblewrap hardware capabilities: `--allow-kvm` and repeatable `--allow-gpu=amd|nvidia`, including additive profile settings, guided hardware discovery, and NixOS `hostAccess.kvm` / `hostAccess.gpu` options with service-scoped `kvm`/`render`/`video` group access.
- Vendor-verified AMD/NVIDIA DRM discovery, pinned character-device `--dev-bind` mounts, and read-only GPU driver/runtime metadata mounts; hardware remains shell-only and disabled unless explicitly granted.

## [0.8.1] - 2026-10-13

### Fixed

- Explicit `--allow-cache-<kind>` flags and NixOS `cacheAccess` entries whose cache directory does not exist are now skipped with a warning instead of aborting startup, so declarative services no longer fail before a cache has been created on the host.

## [0.8.0] - 2026-10-13

### Added

- Markdown lint and Alejandra checks in `nix flake check`, both tools in the dev shell, and CI validation before release builds.
- Optional `-s/--serialize` runtime flag and NixOS service setting to run MCP calls one at a time; Linux runs tools concurrently by default.
- Explicit `--allow-nix-daemon`, `--allow-nix-config`, `--allow-ssh-agent`, and `--allow-git-config` Linux shell grants, each discoverable during onboarding; their mounts remain pinned, shell-only, and off by default.
- `--allow-cache-<kind>[=ro|rw]` for all 13 existing developer-cache families, using the same detection as setup and explicit read-only/read-write choices. NixOS gained matching `hostAccess`, `sshAgentSocket`, and `cacheAccess` options.
- GCR SSH-agent autodetection for interactive and NixOS system services: valid `SSH_AUTH_SOCK`, `$XDG_RUNTIME_DIR/gcr/ssh`, and `/run/user/<effective-uid>/gcr/ssh`. An optional `DOTLINK_SSH_AGENT_SOCKET` (the NixOS `sshAgentSocket` setting) takes strict precedence; no explicit socket configuration is necessary when GCR is running.

### Changed

- Moved the agent guide to `AGENTS.md` and updated its references.
- `nix fmt` now uses Alejandra for Nix formatting.
- Split Nix build, release, check, and development definitions into `nix/`; package versions now come from `Cargo.toml`.

### Fixed

- Linux directory listings now apply deny/protected-path checks and format relative paths against the original authorized directory instead of internal `/proc/self/fd/` names, restoring recursive traversal and avoiding filtered-entry disclosure.
- File reads, text edits, binary MCP reads, and binary patches now bound the actual bytes read even when another process grows the file concurrently. Listings tolerate entries removed during enumeration.
- Read+write tools now pin the narrower of overlapping read and write grants, preventing replacement paths from bypassing the narrower authority; matching grants share the same opened directory identity.

### Security

- Linux native file tools now open authorized paths through pinned directory handles and kernel-enforced `openat2` resolution, removing reliance on the global tool gate for symlink/path-race protection. Legacy conservative dispatch remains on non-Linux platforms.
- Linux Bubblewrap grant/cache mounts now use pinned directory FDs inherited only by the sandbox child, preventing mount-source symlink swaps across concurrent requests.
- SSH-agent opt-in uses a pinned socket and minimal read-only Unix account lookup files, allowing real Git SSH commit signing without sharing any private SSH keys.
- Nix daemon access now remains masked without explicit opt-in even when shell network access or broad filesystem grants are enabled; pinned host mounts are authorized against the exact resolved path used to open the FD.

## [0.7.0] - 2026-10-11

### Added

- A native `nixosModules.default` / `nixosModules.dotlink` flake module with `services.dotlink.enable`, independent named instances, optional declarative settings, user-profile fallback, configurable service user/working directory/package, systemd supervision and journald logging.
- Optional systemd `LoadCredential` integration for OpenAI Runtime and ngrok tokens. Explicit file-based credentials override saved keys without requiring a new profile or embedding secrets in the Nix store.
- SIGTERM-aware graceful shutdown for unattended systemd services and NixOS module evaluation checks.

### Changed

- Standardized the repository agent guide filename and updated all handoff references.

### Security

- Explicit credential-file overrides are included among MCP-protected control-plane paths, and credential-file environment paths are stripped from spawned shells.

## [0.6.0] - 2026-10-01

### Added

- Embedded single-owner OAuth 2.1 authorization/resource server for Streamable HTTP, including Protected Resource Metadata, Authorization Server Metadata, authorization code + mandatory PKCE S256, exact resource binding, RFC 9207 `iss`, CIMD, DCR fallback, revocation, rotating refresh tokens, and short-lived opaque access tokens.
- Stable `--ngrok-domain`, explicit reverse-proxy `--public-url`, local `--oauth/--no-oauth`, and the one-run `--allow-public-no-auth` escape hatch.
- `dotlink oauth status/clients/revoke/revoke-all` for inspecting and revoking persisted remote OAuth clients and refresh grants.
- OAuth owner credentials stored as Argon2id hashes, approved DCR clients and hashed refresh grants stored under the private Abird XDG state namespace, while authorization codes/access tokens remain memory-only.
- Cross-process locking around OAuth state read/modify/write so setup, management commands, and a live server cannot lose one another's changes.
- Task-oriented, colorized CLI help with sections for setup/profiles, transports, remote HTTP/OAuth, filesystem access, shell/network, and diagnostics; profile/OAuth subcommand help now documents argument meanings and examples.
- Automatic forward migration for known-compatible profile schemas. Schema v9 now migrates to v10 in memory because the v10 fields are additive/defaulted; migrated profiles are written as v10 on their next edit/save.

### Changed

- Public ngrok ingress is OAuth-protected by default, independently of local HTTP OAuth.
- Direct non-loopback HTTP is also safe-by-default: it requires OAuth plus an explicit HTTPS public origin, unless `--allow-public-no-auth` is supplied for that run.
- Refresh tokens are issued only when the approved client advertises the `refresh_token` grant; `offline_access` requires that capability.
- Installers now resolve the latest GitHub Release by default and act as idempotent updaters when re-run.
- The version command and GitHub Release title now use the concise `dotlink v0.6.0` form; installer and CI version checks accept the new CLI format while still recognizing older installed binaries.
- README positioning now emphasizes invoking `@dotlink` directly from ChatGPT chats/Spaces/dots and connecting several machines without installing the ChatGPT desktop app or provisioning SSH.
- Profile and OAuth CLI documentation has been updated to match the live command surface, including the persisted `oauth` setting.
- Setup now describes launch-directory access accurately: the default `.` permission follows whichever directory dotlink is started from, while the current directory is shown only as context.
- Compatible config migrations are explicit and versioned rather than globally rejecting every older schema; newer schemas and older schemas without a defined migration path are still rejected rather than guessed.

### Fixed

- Filesystem deny checks now apply to both lexical and canonical paths so a denied namespace cannot be reintroduced through a later symlink alias.
- Filesystem mutators and shell execution are serialized against reads through the LocalMachine operation gate, closing dotlink-originated path-topology TOCTOU races.
- Existing write/edit targets must be regular files; special files such as sockets/FIFOs/devices are rejected.
- Runtime-key writes are atomic, credentials stay in dotlink's owned XDG directory even with `DOTLINK_CONFIG`, and active config/credential paths are protected from MCP/sandbox self-modification.
- Shell timeout uses one absolute deadline across stdin, process execution, and output collection, and timed-out/failed children are explicitly reaped.
- Runtime flags mixed with profile subcommands are rejected rather than silently ignored.
- Mixed stdio/OpenAI human status remains off stdout, and manual stdio Ctrl+C handling is symmetric across Unix/Windows.
- Installers validate the downloaded binary's reported version before replacement and use same-directory replacement paths for updates.

### Security

- Argon2 verification runs in bounded blocking workers rather than Tokio workers, with bounded concurrency and failed-attempt delay but no remotely triggerable global owner lockout.
- CIMD validation uses HTTPS-only metadata URLs, redirect refusal, public-IP/DNS checks, pinned vetted addresses, bounded bodies, and exact client-ID matching.
- Unauthenticated DCR registrations remain bounded and memory-only until owner approval.
- Public OAuth state is bounded, durable refresh-token material is stored only as hashes, and owner password plaintext is never persisted.
- Public ingress never derives OAuth issuer/resource identity from Host/Forwarded headers.
- Public no-auth remains an explicit one-run-only escape hatch and is not persisted.

## [0.5.0] - 2026-09-30

### Added

- Modular OpenAI Tunnel, stdio, Streamable HTTP, and optional ngrok MCP transports around one shared LocalMachine policy.
- Pi-style MCP tool surface: `ls`, `read`, `write`, `edit`, platform shell, plus separate binary read/write/patch tools.
- Named JSONC profiles with persisted transport, filesystem allow/deny, shell/network, and developer-cache configuration.
- Linux Bubblewrap shell sandbox with network disabled by default, NixOS runtime/profile support, Nix-daemon masking, and explicit developer-cache mounts.
- Interactive setup, profile management commands, live `Ctrl+R` restart and `v` verbosity cycling, quiet-by-default TOOL/REQ logging, and policy-aware `--list-tools`.
- Automatic full-runtime OpenAI Tunnel recovery after repeated transient poll failures.
- Crane-based Nix builds, static Linux x86_64/ARM64 releases, Windows x86_64/ARM64 cross builds, macOS ARM64 cross build, release aggregation, and curl/PowerShell installers.
- GitHub tag-to-release automation with individual binaries and SHA-256 sidecars.

### Changed

- Rebranded the project to **abird dotlink** with `dotlink` as the crate, executable, MCP identity, config namespace, and release prefix.
- Moved dotlink-owned XDG state under `abird/dotlink`.
- Made profile-enabled transports start automatically; runtime `--stdio/--http` add transports and `--no-*` flags suppress persisted defaults.
- Made the launch directory the relative-path base and default read-only grant; all additional authority is expressed through additive allow/deny rules.
- Made `--allow-rw=/` the natural unrestricted filesystem grant while keeping Linux shell sandboxing independent.
- Consolidated unsandboxed full-host authority into the paired `--allow-all --no-sandbox` flags.
- Made developer caches shell-only capabilities that never expand MCP filesystem access.
- Made stdio stdout protocol-only and routed human/log output to stderr.

### Fixed

- Restored exact terminal state on clean exit, restart, transport errors, and unwind paths.
- Fixed NixOS shell executable/PATH resolution and runtime mount behavior without exposing the whole home directory.
- Hardened Nix-daemon masking against varying filesystem shapes.
- Hardened Runtime API key storage and protected control-plane paths.
- Made setup transactional and re-setup preserve existing secrets by default.

## [0.2.0] - 2026-09-29

### Changed

- Scoped the filesystem workspace to the process launch directory by default.
- Added ChatGPT tunnel-registration instructions.

## [0.1.0] - 2026-09-29

### Added

- Initial native Rust Secure MCP Tunnel client.
- Embedded rmcp server with local filesystem and Bash tools.

[Unreleased]: https://github.com/abird-ai/dotlink/compare/v0.14.0...HEAD
[0.14.0]: https://github.com/abird-ai/dotlink/compare/v0.13.1...v0.14.0
[0.13.1]: https://github.com/abird-ai/dotlink/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/abird-ai/dotlink/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/abird-ai/dotlink/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/abird-ai/dotlink/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/abird-ai/dotlink/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/abird-ai/dotlink/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/abird-ai/dotlink/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/abird-ai/dotlink/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/abird-ai/dotlink/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/abird-ai/dotlink/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/abird-ai/dotlink/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/abird-ai/dotlink/compare/v0.2.0...v0.5.0
[0.2.0]: https://github.com/abird-ai/dotlink/releases/tag/v0.2.0
[0.1.0]: https://github.com/abird-ai/dotlink/commit/d10e69e
