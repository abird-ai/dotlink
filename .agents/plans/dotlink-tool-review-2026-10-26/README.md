# Tool review sweep — structure, correctness and atomic writes

Author: Prasanna Loganathar

Co-authors: pi agent

Owner: @pvl

Status: implemented and committed; native checks green, cross-platform validation outstanding.

## Why

A fresh review of `4d07303..fc53fc9` (tool surface v2 + tunnel tuning) found a
set of correctness bugs, a security-relevant gap in the descriptor-rooted
invariant, and structural drift: `src/mcp.rs` had grown to 7.4k lines, plan →
verify → write was copied three times and had already diverged, four lists
described the tool surface, and every writer truncated its target before
writing it.

The goal here is not to add surface. It is to make the existing surface
correct, and to arrange it so the next tool or the next editor cannot reintroduce
the same class of bug.

## Base

`fc53fc9` (DL-090 closeout).

## Findings and resolutions

| Area | Finding | Resolution |
| --- | --- | --- |
| `patch` | Two `*** Add File` sections for one path both planned; the first write landed, the second failed `O_EXCL`, so strict mode wrote data and then errored | One `BTreeSet` of claimed paths per call, checked for every section before planning |
| `walk` | An entry whose `symlink_metadata` failed with `EACCES`/`ENOTDIR` aborted the whole `ls`/`grep` | `skippable_io_error` applies to entries as well as directories |
| `walk` | `.gitignore` was loaded before the directory was opened, so an invalid ignore file in an unlistable directory failed the search; a mid-read error was fatal | Ignore files load only for a directory that enumerated, and any read failure widens the search (invalid patterns still fail closed) |
| `walk` | Files were re-opened by pathname after the walk authorized them | Descent and `.gitignore` reads use pinned directory handles; search candidates stay as paths and are re-opened through the descriptor-rooted reader beneath the pinned grant root, one file at a time, so a large tree cannot exhaust descriptors |
| `patch` | `best_effort` used `?` in the verification pass, aborting the call and skipping the clean files | Verification failures are collected per file in that mode |
| `bedit` | The `sha256` precondition was checked against the planning read and never re-checked; the write truncated in place | Verification produces the only token that can write, and the write is atomic |
| `bedit` | Insert and delete at the same offset behaved differently by request order | Ranges sort by `(offset, length)` |
| `read` | `numbered: false` re-joined `str::lines()`, rewriting CRLF and dropping the trailing newline | The window is returned byte for byte |
| `read` | An offset past EOF returned `""` | Explicit error, like `bread`/`bedit` |
| `grep` | `count` reported at most 50 per file | The per-file cap only bounds `content` mode |
| `grep` | `content` accumulated before the byte cap; `files`/`count` were never capped | One `OutputBudget` per call bounds every mode |
| `ls` | A `**` pattern silently stopped at depth 4 while `grep` walked unbounded | `**` implies no level cap in both |
| Writers | `set_len(0)` before writing: ENOSPC or a crash destroyed the file, and a concurrent reader could see it half-written | `SecureFs::stage` installs a sibling temporary at commit, preserving the mode |
| Denies | `canonicalize_deny` kept a `..` that followed a missing component, so the deny matched nothing | Deny/protected paths are lexically folded before resolution |
| Tunnel | `request_timeout > poll_hold` was validated, but the 10 s margin existed only in `Default` | Validation requires `poll_hold + REQUEST_TIMEOUT_MARGIN` |
| Docs | `docs/SECURITY.md` default tool list omitted `grep`; plan README claimed `path` was optional everywhere and `offset` existed only on two tools; the DL-080 verification list was unticked | Corrected |

## Second review pass

A fresh adversarial round over the refactor itself found eight more defects; all
were fixed and covered by tests.

| Area | Finding | Resolution |
| --- | --- | --- |
| `grep` | `files`/`count` charged a byte estimate per entry, so an escaped path could push the reply past `max_output_bytes` with `truncated: false` | Collection keeps a cheap budget, and the rendered JSON is trimmed to the ceiling before it is returned |
| `grep` | After a budget/limit stop the scan continued, so a later, smaller path was emitted after a skipped one and could repeat on the next page | The scan stops at the first refusal in every mode: the page is a contiguous prefix of the candidates scanned so far |
| `grep` | `content` applied `offset` after the 50-hit per-file cap, so hits 51+ of a file were unreachable and the page reported `… truncated at 0 hits` | `offset` is consumed before the per-file cap, and a page that emitted nothing reports byte truncation |
| `grep` | One directory handle per candidate entry exhausted descriptors on large trees (`ulimit -n 256`, ~300 directories) | Candidates hold paths only; file contents are read one at a time through the descriptor-rooted reader beneath the pinned root |
| `patch`/`edit` | Sequential commits wrote earlier files before a later one failed, contradicting "nothing written" | `SecureFs::stage` + `StagedWrite::commit`: every file is written and flushed first, then committed; strict mode preflights all files, and the receipt/error states that the commit sequence is not a multi-file transaction |
| `patch` | A failed `*** Add File` write left a partial file; the claim was made after resolution, so a section that failed to resolve did not block a later one | Add stages content beside the target and installs it with a create-only link at commit, so the target is absent until commit and a failure leaves nothing behind; lexical and resolved paths are both claimed |
| `patch` | `str::lines()` stripped the CR from added lines, producing mixed endings in a CRLF file; `@@ -3,1 +3,1 @@ anchor` dropped the anchor | The parser splits on the newline byte only, added lines inherit the target file's terminator, the anchor parser strips range tokens then keeps the remaining text, and the Codex `*** End of File` marker is accepted |
| `bedit` | The planning read was capped by `max_read_bytes`, not the patchable-file size | `Snapshot::read_within` gives `bedit` its own bound |
| walk | `open_child*` hardcoded `NO_XDEV`, so descending into a mount point aborted the whole walk even under a `/` grant; `ELOOP` from a racing symlink swap did the same | Child opens inherit the root's resolve mask, and `EXDEV`/`ELOOP` are skippable |
| walk | Enumeration needed `/proc/self/fd`; a container without `/proc` broke `ls`/`grep` entirely | Falls back to the checked path when `/proc/self/fd` is absent |
| verify text | `best_effort` reported `… changed while patching; nothing was written; skipped` | `Snapshot::verify` reports only the change; each caller states the consequence |

## Third review pass

A final adversarial round over the completed sweep found that the paging,
alias, create-only and cross-platform paths still had holes. All are fixed and
covered.

| Area | Finding | Resolution |
| --- | --- | --- |
| `grep` | The 50-hit per-file cap stopped one file but continued the scan, so a later file was emitted on every page and an earlier hit could be skipped | Every stop sets one `StopReason` and stops the whole scan; pages are contiguous within the candidate bound and a two-file test covers the cap |
| `grep` | The prose marker inferred the cause from `emitted == 0`; `count.total` was page-scoped but named global; the final JSON could exceed the ceiling | The marker reads the actual stop reason; `count` reports `page_total`; each JSON mode measures its exact final shape |
| `grep` | A hit already covered by the previous context window consumed a page slot without emitting a line | Context-covered hits no longer increment the per-file/page counters |
| `ls` | No byte ceiling, unlike `grep` | `ls` uses the same rendered-JSON trimmer and `max_output_bytes` |
| `patch` | Duplicate detection was lexical, so two symlink spellings of one file both planned and the later commit silently won | Claim both the lexical and the resolved path; a symlink-alias test refuses the second section before writing |
| `patch` | `*** Add File` created the real target empty before staging, and commit failure or cleanup could leave it behind | `stage_create` stages beside the target and installs with a create-only link; there is no empty target and no path-based cleanup |
| `patch` | An LF patch against a CRLF file wrote LF added lines | Added lines inherit the matched file line's terminator |
| Writers | Non-Linux `stage_write` did not preserve mode, refused no special targets, and did not sync | The non-Linux path syncs the temp, preserves permissions, rejects symlink/special targets, allows a missing create-or-replace target, and uses hard-link create-only |
| Writers | A target swapped for a symlink/special file after staging was replaced by the rename | The replacement path re-checks the target immediately before the rename |
| Tunnel | A full batch blocked the poll loop on a concurrency permit | Tasks acquire permits inside the task; up to three poll batches may queue, and a sustained backlog pauses polling as bounded backpressure |
| Tunnel | `request_timeout` ignored `connect_timeout`; `activation_grace` was shorter than the check budget | Validation covers both relations and the default grace derives from the check budget |
| `bedit` | A byte-identical edit still returned a write receipt | The receipt carries `changed: false` and no write is claimed |
| Concurrency | Concurrent `bedit`/`edit`/`patch` calls could all verify the same original bytes and then overwrite one another | A bounded sharded mutation lock keyed by resolved path is held from snapshot read through commit; multi-file calls acquire shard locks in sorted order. Updates also re-check the snapshot length and digest at commit. Shell commands remain outside path enumeration and therefore outside this lock. |
| Multi-file contract | "All or nothing" overclaimed the commit sequence | Strict mode preflights every file; each replacement is atomic; the commit sequence is documented as not a transaction and commit errors name the partial outcome |
| `grep` context paging | A context window containing several hits was counted as one hit; a page could emit more matching lines than `limit`, then repeat them on the next page | Content pages select exact matching-line records and clip context at adjacent matching lines outside the page |
| `bedit` limits | `bedit` ignored `max_write_bytes`; a no-op still reported `bytes_written` | It uses `min(max_write_bytes, 64 MiB)`, rejects an oversized result, and reports `bytes_written: 0` for a no-op |
| Tunnel delivery | The delivery reserve applied only to shell calls; response POSTs treated most 5xx as fatal | The reserve bounds every dispatch deadline; response POSTs retry all server errors |
| Tunnel backlog | A sustained backlog eventually paused polling | Up to three poll batches queue; after that, pausing is documented bounded backpressure |
| Tunnel tuning | Activation grace could be one interval short; backoff/jitter and duration overflow were unbounded | Validation covers one full check budget, bounds `backoff.cap + jitter` by `request_timeout`, and returns errors on duration overflow |
| `backoff` | Sub-millisecond jitter was silently zero and `step + jitter` could panic | Nanosecond jitter band and saturating add |

## Fourth review pass

| Area | Finding | Resolution |
| --- | --- | --- |
| Shell | Writing all stdin before starting stdout/stderr drains could deadlock both pipes | Drains start immediately; stdin is written by a task selected alongside child wait and timeout; a bidirectional-pressure regression covers 160 KiB each way |
| XDG | Relative XDG overrides put config/state under the launch directory | One resolver accepts only non-empty absolute overrides and falls back to the home directory; config, state, cache and host discovery use it |
| `read` | Whole-file preload rejected a small window in a file above the read ceiling | Line-streamed window; `max_read_bytes` bounds the returned window and one line, and a byte-capped window stops at the last fitting line |
| Tunnel | MCP response body was fully collected before the 16 MiB check | Frame-by-frame collection rejects an oversized body while streaming |
| `grep` | A 50-hit batch larger than the remaining byte budget produced an empty page | The page shrinks to the largest contiguous hit prefix that fits the budget |

## Fifth review pass

An independent review of the same tree reproduced the earlier scenarios and
found four defects plus three contract gaps; all are fixed and covered.

| Area | Finding | Resolution |
| --- | --- | --- |
| Staging | The temporary was created `0666` (umask) and narrowed to the target's mode only after the payload was written, so replacing a `0600` file left a `0644` sibling holding the new content for the whole write. Reproduced with an 8 MiB payload (scanner observed `0644` from 49 KiB to the full size, target `0600` afterwards). A regression of the atomic-write work. | `src/staging.rs` owns the mode policy: the temporary is created at `existing_mode` (or `0666` for a create), so it is never broader than the target. Both backends and the regression test are covered. |
| `grep` | The byte-limited page appended its marker after budgeting and then trimmed the content, cutting an accepted hit line and gluing the marker into it; the page reported more hits than it returned whole. | The content budget reserves the longest marker before a hit is accepted, and `cap_text` is gone: a page now ends with whole hit lines, and a test asserts every returned line is complete. |
| Limits | `LocalMachine::new` raised `max_output_bytes`/`max_read_bytes`/`max_write_bytes` to 1024, so `max_write_bytes: 32` still wrote 500 bytes. | `MIN_BYTE_LIMIT`/`MIN_SHELL_TIMEOUT_SECS` are validated in `setup` with the value the user wrote, and the runtime rejects rather than widens. |
| Reply trimming | `trim_to_ceiling` re-serialized the whole array per dropped entry: 16 000 escaped names at a 1 MiB ceiling took 211 s. | One binary search over the kept prefix: identical result in ~140 ms; a render-count test pins the complexity. |
| Walk | `/proc/self/fd` unavailability fell back to a pathname `read_dir`, silently weakening the pinned-directory guarantee. | Enumeration fails closed with an explicit procfs error. |
| `read` | The marker was appended after budgeting, so the reply exceeded `max_read_bytes` by its length. | Whole lines are dropped to make room for the marker; the reply, marker included, fits. |
| `ls` | A non-`**` pattern implied a fixed depth of 4, so a deeper pattern returned an empty listing with `truncated: false`. | Depth derives from the pattern's path-segment count. |
| Shell | A `deny-write` naming a file was passed to the directory-mount path, making every sandboxed shell call fail (`ENOTDIR`). Also `Instant + Duration` was unchecked. | Deny-write roots are enforced by the ro-bind pass only (files included); the deadline uses `checked_add`. |
| Composability | Tool names were repeated in `with_profile_limits` and the tunnel's `SHELL_TOOLS`; the router was rebuilt per call; the two staging backends were written twice. | `is_shell_tool` derives from `TOOL_SPECS`, the router is cached once per machine, and the staging policy is single-sourced. `edit`/`patch`/`bedit` descriptions advertise their limits, with drift tests. |

## Sixth review pass

A cross-platform review found that the mutation lock only coordinated threads of
one process. All six findings are resolved or explicitly scoped.

| Area | Finding | Resolution |
| --- | --- | --- |
| Concurrency | Per-path locks were process-local. Eight independent servers editing one file either lost seven acknowledged inserts or failed with a digest mismatch; the pre-commit re-hash also left a check→rename window. | The guard now also takes an exclusive advisory `flock` on each target's directory for snapshot → commit. The directory is the identity because the commit rename replaces the inode; locks are shared by concurrent calls in one process through a weak device/inode registry, taken in sorted order, released by the descriptor on exit, and bounded by a 30 s wait with an explicit error. `tests/stdio_tools.rs` runs eight servers against one file and asserts every acknowledged insert survives (fails without the lock). |
| `read` memory | The page retained a `Vec` per returned line: 1 MiB of empty lines cost ~55 MiB resident. | One output buffer with whole lines; making room for the marker truncates at a line start. Measured peak RSS growth is now 1.5–3.4 MiB for 100 KiB–1 MiB inputs, independent of input size (was 6.2/28.1/55.3 MiB). |
| `read` skip | `offset` skipping buffered each skipped line, so a line larger than the ceiling failed the call before the requested window. | Skipping scans for newlines in constant memory and never decodes what it skips; only returned lines are decoded and length-checked. |
| Transports | rmcp's fixed 4 MiB request body refused over HTTP a write the tool accepted over stdio. | `transports::streamable_config` builds both HTTP entry points and derives the ceiling from `max_write_bytes` (6× worst-case JSON escaping + 64 KiB framing). Verified live: a 5 MiB and an 8 MiB write now succeed over HTTP; 9 MiB is refused by the tool, not the transport. |
| Windows build | The non-Linux staging test module imported `std::os::unix` unconditionally. | Unix permission assertions are `#[cfg(unix)]`-gated and the body is platform-neutral. Compiled by inspection only: no Windows toolchain is available here. |
| Windows ACLs | Replacing a file inherits the parent directory's ACL instead of the target's. | Documented in `SECURITY.md` and beside the backend, with the mitigation (scope the restrictive ACL to the directory). Not coded: `ReplaceFileW` would need Win32 security APIs that cannot be compiled or tested in this environment, and the Windows backend is unit-tested only. Open item for a Windows validation pass. |

Not reproducible here: `nix flake check` (package build passes), NixOS module
evaluation, live ngrok/OpenAI connectivity, Windows/macOS runtime behaviour.

## Seventh review pass

Review of the extracted `src/mutation.rs` coordinator and the Windows staging
backend.

| Area | Finding | Resolution |
| --- | --- | --- |
| Lock mode | SH/EX was chosen from the call's whole path list, so a call spanning a shared and a private directory took EX on both, and a concurrent call in the private directory alone (SH) waited on it for the 30 s timeout. Reproduced in the fast path. | The mode now comes from the directory actually locked (its owner and mode), so every caller of one inode agrees. Regression test enables the fast path on a temporary sticky root and fails with the old logic. |
| Capability probe | `PathRecords::open()` ran per call, so a transient failure produced SH and EX locks on one inode from two calls. | Probed once per lock root and cached (first answer wins); a later failure fails closed instead of degrading to a directory SH lock alone. |
| Testability | The Linux fast path was unreachable on this host (`/run/lock` is `0755`), so nothing exercised SH locks plus records. | The lock root is a parameter (`lock_with`); tests cover the fast path, sibling independence, and the per-directory mode. End-to-end: 8 processes on a `1777` bind-mounted root keep every acknowledged insert, and sibling files stay independent. |
| Timeout text | The 30 s bail named `paths[0]` even when another path's record was blocked. | Names the blocked path. |
| Docs | The fast path's precondition and its failure mode across lock roots were under-specified. | `SECURITY.md` names the container topology (separate `/run`, bind-mounted checkout) where per-UID record files cannot see each other, and requires a shared lock root or a non-shared workspace. |
| Non-Linux staging | The commit re-check read the whole target into memory to hash it. | Streamed with a 64 KiB buffer, matching the Linux backend. |

Verified but documented, not changed: a container pair with separate `/run/lock`
and a shared workspace loses coordination (measured 5/8 acknowledged inserts
surviving, the rest refused by the snapshot re-hash). The Windows DACL path
(`GetFileSecurityW` → `SetFileSecurityW` on the empty temporary → `ReplaceFileW`)
was reviewed against the vendored `windows-sys` signatures and enabled features;
it cannot be compiled or run here.

Deliberate, documented behaviour: TOOL logging prints a shell command as a
≤120-character excerpt (or `command_bytes`), because the command is the audited
operation. Payload arguments stay byte counts. `docs/SECURITY.md` states the
exception and its journal consequence.

Left as documented trade-offs, not defects: `ls`/`grep` collect up to
`MAX_LIST_ENTRIES`/`MAX_GREP_FILES` matches before sorting (a correct but
arbitrary subset on a huge tree, and the bound applies to collected results, not
traversal), the directory `fsync` after a rename is best effort and skipped in a
directory that is writable but not readable, `Verified::stage` re-resolves the
target with the same root choice `open_edit` makes rather than holding the
verified parent descriptor, atomic replace does not preserve ownership,
extended attributes, ACLs or hard links, and the final digest check cannot make
the check-to-rename window atomic against an external writer. Shell commands
remain outside path-keyed mutation locks because their write paths are arbitrary.

## Design

### Module layout

~~~text
src/mcp.rs              AccessPolicy, LocalMachine, pinned descriptors, TOOL_SPECS, ServerHandler
src/mcp/tools/mod.rs    walk, GlobFilter, limits, truncation markers, Snapshot/Verified
src/mcp/tools/ls.rs     ListArgs, list_impl, ls
src/mcp/tools/text.rs   read, write
src/mcp/tools/grep.rs   GrepArgs, grep_impl, grep
src/mcp/tools/edit.rs   EditsArgs, edit_impl, edit
src/mcp/tools/patch.rs  Codex grammar, apply_patch_hunks, patch_impl, patch
src/mcp/tools/binary.rs bread, bwrite, bedit
src/mcp/tools/shell.rs  bash, powershell, bubblewrap, mounts, heartbeat
~~~

Each family names its own `#[tool_router(router = "..._router")]`; `LocalMachine`
merges them once. Tools live under `src/mcp/` so they remain descendants of the
module that owns `LocalMachine` and its policy: no field, no policy method and no
pinned descriptor had to be made `pub(crate)`.

### One capability table

`TOOL_SPECS` records, per tool, its capability (`Read`, `Write`, `ReadWrite`,
`ShellUnix`, `ShellWindows`) and whether it is a pure concurrent read.
`tool_router_for_policy(Capabilities)` is the only place that hides tools; the
platform operation gate, `--list-tools` and the TOOL log read the same rows. A
test asserts the table and the routers agree, so adding a tool without a table
row fails the suite.

### Plan → verify → commit

`Snapshot::read` captures the bytes a text tool plans against.
`Snapshot::verify` re-reads the file and is the only way to obtain a `Verified`,
and an existing-file update can only be staged through that token. That makes
"check, then write" the only expressible order for updates: `bedit` cannot skip
the re-check, and `commit` returns `Ok(false)` for byte-identical text so a no-op
never churns mtime or appears in a write receipt. Creates have no prior content
and stage directly.

### Atomic replace

`SecureFs::stage`/`stage_update` resolve the pinned parent directory, writes a
`.dotlink-<pid>-<rand>.tmp` sibling with `O_EXCL`, copies the existing file's
mode, `fsync`s, and `renameat`s it over the target. A symlink or non-regular
target is refused, never followed and never replaced. `stage_create` uses the
same staging but installs with `linkat`, which fails if the target exists; the
target is therefore absent until commit. Non-Linux uses the same staged shape,
preserving permissions and rejecting symlink/special targets, with hard-link
create-only.

### Descriptor-rooted walk

The walk pins the caller's root once and queues `(parent handle, name)` pairs, so
descent opens a child of an already-pinned directory instead of resolving a path
again. Entries carry paths only; search reads a candidate through the
descriptor-rooted reader beneath the pinned grant root, one file at a time, so a
large tree cannot exhaust descriptors. A directory renamed inside the grant can
change which inode that path names, but never escapes the grant. Unreadable
children are skipped where they are opened; the caller-named root still fails
loudly.

## Decisions

- Split tools into `src/mcp/tools/` rather than a top-level `src/tools/`: keeps
  the machine's internals private to its own module tree.
- Keep the capability list as data (`TOOL_SPECS`) rather than four positional
  booleans, and derive the concurrency class from the same rows.
- Derive TOOL-log argument names from each tool's schema; keep one semantic list
  of payload arguments (`content`, `stdin`, `data`, `old_text`, `new_text`,
  `patch`) whose values are logged as byte counts only.
- `**` means unbounded depth in `ls`, matching `grep`; `MAX_LIST_ENTRIES`
  bounds collected results, not traversal.
- `read` past EOF is an error rather than an empty answer; it matches the
  binary tools and cannot be mistaken for an empty file.
- Do not implement an operation-gate-free writer on non-Linux platforms in this
  pass; the atomic rename there relies on the existing gate.

## Follow-ups (not this plan)

- A Linux OFD-per-file fast-path experiment was removed in favor of simpler hierarchical directory locking; no /run/lock service or per-profile lock namespace remains.
- Windows/macOS native runtime and the full cross-build still require separate host validation.
- `grep`'s candidate list and `ls`'s entry list still stop at their `MAX_*`
  bound before sorting, so a very large tree returns a correct-but-partial
  prefix. A bounded top-N selection would fix that at the cost of walking the
  whole tree.
- `nix flake check` and the Bubblewrap suite are run from `nix develop`; this
  plan records the plain cargo gates.

## References

- `.agents/plans/dotlink-tool-surface-2026-10-25/` — the surface this reviews.
- `.agents/plans/dotlink-tunnel-tuning-2026-10-25/` — tunnel tuning.
- `src/secure_fs.rs` — `stage`, `stage_update`, `stage_create`, `StagedWrite::commit`, `open_child_read`, `open_child_dir`.
- `src/mutation.rs` — hierarchical directory intention/exclusive locks, with process-local shards.
- `src/mcp/tools/mod.rs` — walk, glob, markers, `Snapshot`/`Verified`.
