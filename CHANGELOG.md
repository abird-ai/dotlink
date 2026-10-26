# Changelog

All notable changes to dotlink are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

- Stdio verbose logs never inspect nested request payloads for a method and sanitize all method names.
- Missing protected paths are masked inside Bubblewrap even under writable parent mounts.
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
- The OpenAI tunnel's embedded MCP response ceiling is derived from `max_read_bytes`/`max_output_bytes`, including JSON escaping and framing, instead of silently rejecting valid reads over 16 MiB.
- Non-Linux staging now owns each temporary from the moment of creation through async writes and sync. RAII cleanup runs on cancellation, and the platform-neutral path-backend tests run under Linux CI too.
- `bread(format=mcp)` percent-encodes each resource-URI path segment so spaces, `#`, `?`, `%` and Unicode do not corrupt the resource identifier.

- Removed the Linux OFD record-lock fast path and its `/run/lock` dependency. Cross-process coordination now uses only directory `flock` (shared ancestor intent plus exclusive parent), while in-process shards retain parallelism.
- OAuth client metadata fetches ignore proxy environment variables, bound DNS lookup time, and reject anonymous requests above four concurrent or 24 per rolling minute.
- OpenAI tunnel poll envelopes, local MCP responses, and OAuth metadata use a shared streaming byte ceiling before deserialization (64 MiB per tunnel poll); control-plane error reads are bounded as well.
- Windows replacement now uses a named backup with `ReplaceFileW`; documented partial-failure states restore the backup or preserve recovery artifacts instead of deleting the last recoverable file.
- Linux atomic writes explicitly guarantee complete-file visibility; parent-directory `fsync` failures are logged without misreporting an already-committed operation as failed.

- `read` now reports an error for the first nonexistent line after a final newline, including CRLF and blank-line files, while preserving empty-file reads and unbounded skipped-line scanning.
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

[Unreleased]: https://github.com/abird-ai/dotlink/compare/v0.9.1...HEAD
[0.9.1]: https://github.com/abird-ai/dotlink/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/abird-ai/dotlink/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/abird-ai/dotlink/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/abird-ai/dotlink/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/abird-ai/dotlink/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/abird-ai/dotlink/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/abird-ai/dotlink/compare/v0.2.0...v0.5.0
[0.2.0]: https://github.com/abird-ai/dotlink/releases/tag/v0.2.0
[0.1.0]: https://github.com/abird-ai/dotlink/commit/d10e69e
