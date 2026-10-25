# Tool surface v2 — search, multi-edit, patch, binary rename

Author: Prasanna Loganathar

Co-authors: pi agent

Owner: @pvl

Status: design complete, decisions closed, implementation not started.

## Handoff context

Read in this order before touching code:

1. `AGENTS.md` — validation gates, owned-commit rules, terse writing, no pushes.
2. `README.md` (this document) — design, semantics, failure texts, decisions.
3. `.agents/plans/TODO.md` — queue line `DL-080`.
4. `docs/SECURITY.md` and `.agents/docs/configuration.md` — resource-limit and config semantics.
5. `src/mcp.rs` tool section and `src/transports/openai.rs` runtime loop.

Environment:

- Pinned toolchain: `rust-toolchain.toml`; use `nix develop -c <cmd>` for `cargo`, `alejandra`, `markdownlint-cli2`.
- One-time network access is needed to resolve the new `globset` and `ignore` dependencies into `Cargo.lock`.
- `cargo` without nix works too, but run the gates through `nix develop` to match CI.

Working tree state at handoff: HEAD is `1b096da`, and an uncommitted v1 set already implements client-controlled shell limits, tunnel window coordination, progress heartbeats, shell timeout errors, `ls` v1, and numbered `read`. **Land that set first** (Commit 0) so later commits stay reviewable.

## Why this plan exists

Two host journals (pvl-a1 21 h, pvl-l5 6 d) show where model calls go:

- `bash` 2496 / 2911 calls; inside it `rg` 441, `grep` 111, `find` 75, `sed -n` 1127, `nl -ba` 32.
- 27–29 % of bash calls are pure inspection with no mutation and no build.
- `edit` 826 / 866 calls at 1.4–1.9 % error; `write` 104 / 241; `read` 90 / 204 — the file tools are used and they work.
- `python` appears in 29 % / 45 % of bash calls but `write_text` only 4 times per host against `read_text` 209 / 521 — an inspection runtime, not an editing workaround.
- 31 calls died at dotlink's own 120 s cap in six days; the client window never fired.

Model-behaviour cross-check (113 pi sessions driven by DeepSeek on the same host, ~9.5k tool calls): bash 75 %, read 12 %, edit 6 %, write 3 %. Claude-driven dotlink traffic is bash ~69 %, edit ~20–23 %. Harness tool availability moves that split, so the missing primitive is **search**, and multi-step text work must stop costing extra round trips.

## Invariants the implementation must preserve

- Linux text IO stays descriptor-rooted through `secure_fs`/`openat2`; globs, regex, and ignore rules are pure matchers over paths and buffers already held. Never import `walkdir` or `ignore::Walk`; never re-open a path by name after authorization.
- Denies and protected paths win before patterns, sorts, or limits are applied.
- Text tools stay UTF-8-only with per-file caps; arbitrary bytes go through `bread`/`bwrite`/`bedit`.
- Search never reaches shell-only grants (cache mounts, Nix paths); it uses filesystem read authority only.
- Tool descriptions describe behaviour only; availability is expressed by hiding tools in `tool_router_for_policy`.

## Current state (implemented, in the working tree)

| Area | Symbols / behaviour |
| --- | --- |
| Limits | `DEFAULT_MAX_SHELL_TIMEOUT_SECS = 3600`; `resolve_capped`, `resolve_shell_timeout`; `max_output_bytes` is a ceiling |
| Tunnel | `apply_response_window`, `shell_timeout_within`, `RESPONSE_DELIVERY_RESERVE = 5s`, `SHELL_TOOLS` |
| Heartbeat | `ProgressHeartbeat`, `SHELL_HEARTBEAT_INTERVAL` (10 s; 100 ms under `cfg(test)`), `heartbeat_tick` |
| Shell timeout | `TIMEOUT_OUTPUT_GRACE = 250ms`; timed-out results carry `timed_out: true` plus partial output |
| Schema | `with_profile_limits`, `shell_timeout_description`, `shell_output_description`; `list_tools`/`get_tool` overrides |
| `ls` | `ListArgs { path, pattern, depth, max_entries }`; `list_entry_limit`; `DEFAULT_MAX_LIST_ENTRIES = 4096`, `MAX_LIST_ENTRIES = 16384`; returns `(Vec<FsEntry>, bool)`; hand-rolled `glob_matches` (to be replaced) |
| `read` | `ReadArgs { path, offset, limit, numbered }`, numbering on by default |

## Target surface — 11 tools

```text
discovery   ls     grep
text        read   write   edit   patch
binary      bread  bwrite  bedit
shell       bash   powershell
```

Renames: `read_binary` → `bread`, `write_binary` → `bwrite`, `patch_binary` → `bedit`. Clean cut, no alias routes.

### Shared conventions

- `path` optional everywhere, defaults `.`, resolves from the launch directory.
- One glob grammar: `globset` with `literal_separator(true)` — `*`, `?`, `[...]`, `**`, `{a,b}`.
- Every list/search tool has `limit` with a documented default and hard maximum.
- JSON tools return `truncated`; `read` appends one marker line instead.
- `offset` exists only on `grep` (hits/files) and `read` (lines).
- `edit` and `bedit` are all-or-nothing; `patch` is atomic unless `best_effort`.

### ls

| Arg | Default | Notes |
| --- | --- | --- |
| `path` | `.` | |
| `pattern` | — | globset grammar over entry paths |
| `depth` | 1, or 4 when `pattern` contains `/` or `**` | explicit `depth` always wins |
| `kind` | `any` | `any` / `file` / `dir` |
| `sort` | `path` | `path` / `mtime` (newest first) |
| `limit` | 4096 | hard maximum 16384 |

Output `{"entries":[{"path","kind","size"}],"truncated":bool}`. `mtime` sort needs `fs::metadata(...).modified()`; on error keep the entry with a zero timestamp ordering after dated ones. Symlinked directories are never traversed; denied and protected entries are dropped before the limit.

Depth rule must be visible in the description: "Defaults to 1, or 4 when `pattern` contains `/` or `**`."

### grep

| Arg | Default | Notes |
| --- | --- | --- |
| `pattern` | required | regex unless `literal`; ≤1024 chars, printable |
| `path` | `.` | |
| `glob` | — | entry filter, globset grammar |
| `literal` | false | escape the pattern |
| `ignore_case` | false | |
| `output_mode` | `content` | `content` / `files` / `count` |
| `before` / `after` | 0 | context lines, each maximum 8 |
| `numbered` | true | `path:line:` prefix in `content` mode |
| `offset` | 0 | skip N hits or files before `limit` |
| `limit` | 200 | hard maximum 200 |
| `respect_gitignore` | false | opt-in |
| `skip_hidden` | false | opt-in |

Bounds: UTF-8 files ≤1 MiB each, per-file match cap 50, regex compiled with `size_limit` and `dfa_size_limit` of 1 MiB, output capped at `max_output_bytes` with `truncated`.

Output shapes:

- `content` → lines `path:line: text` (or `path: text` when `numbered: false`) with `-`/`+`-free context lines, hit groups separated by `--`, `{"matches":…}` wrapper not used — return plain text plus a trailing `… truncated at 200 hits` style marker for consistency with `read`;
- `files` → `{"files":[…],"truncated":bool}`;
- `count` → `{"counts":[{"path","count"}],"total":n,"truncated":bool}`.

Gitignore handling: when `respect_gitignore` is set, load `.gitignore` files through `secure_fs` (never the ambient path namespace) with 64 KiB / 1024-line bounds per file, reject invalid patterns, and honour nested files relative to the search root, including negation. `skip_hidden` drops entries whose name starts with `.` from both listing and descent (except the search root itself).

### read

| Arg | Default | Notes |
| --- | --- | --- |
| `path` | required | |
| `offset` | 1 | 1-based first line |
| `limit` | 4096 | lines; still bounded by `max_read_bytes` |
| `numbered` | true | `{n}\t{line}`; one tab separator; `false` returns exact text |

Trailing marker only when a cap is hit: `… truncated at line 4096 of 12044`.

Numbering is on by default: it is the addressing scheme for follow-up reads and edits; Claude Code (`cat -n`) and aw (`{n}: {line}`) both number by default. The hazard is an edit anchor copied with the prefix, so `edit` states the rule and reports it when a failed `old_text` starts with `digits + tab`.

### write

`path`, `content`. Creates or replaces; bounded by `max_write_bytes`.

### edit

| Arg | Notes |
| --- | --- |
| `path` | optional default for entries that omit their own |
| `edits[]` | required; `{path?, old_text, new_text, replace_all?}` |

Array-only; the flat `{path, old_text, new_text, replace_all}` form is removed (clean cut, same policy as the renames).

Semantics:

- applied in array order; entries for one file apply to that file's evolving buffer;
- each `old_text` must be unique unless that entry sets `replace_all`;
- an entry's `old_text` must not be a substring of an earlier entry's `new_text` for the same file;
- plan against in-memory snapshots, then re-hash each file before writing; a mismatch fails that file and nothing is written;
- any failure writes nothing.

Bounds: ≤64 entries, ≤16 distinct files, combined bytes ≤ `max_write_bytes`, `old_text` non-empty.

Failure texts:

```text
edit 2/3 (crates/x/src/lib.rs): old_text does not occur at the point that edit applies
edit 3/3 (crates/y/src/lib.rs): old_text occurs 4 times at lines 12, 44, 91, 130; make it unique or set replace_all
edit 2/3 (crates/x/src/lib.rs): old_text is a substring of this edit's predecessor's new_text in the same file
edit 3/3: no path (set edits[].path or a top-level path)
edit 2/3 (crates/x/src/lib.rs): old_text appears to include a read line-number prefix (digits then tab); strip it
crates/y/src/lib.rs changed while editing; nothing was written
```

Receipt: `{"files":[{"path","edits","line_delta"}],"edits_applied","changed_files"}`.

### patch

| Arg | Default | Notes |
| --- | --- | --- |
| `dir` | `.` | base for relative paths in the patch |
| `patch` | required | Codex `*** Begin Patch` text |
| `best_effort` | false | write files that planned cleanly, report the rest |

Grammar (v1):

```text
*** Begin Patch
*** Update File: path
@@ optional anchor text
 context
-removed
+added
*** Add File: path
+content
*** End Patch
```

Parsing rules:

- header and terminator are mandatory; stray text between hunks is an error;
- `@@` may carry anchor text or unified-diff line numbers; numbers are accepted and ignored, anchor text narrows the search start;
- a hunk's context+removed block is located by exact match, then a per-line `trim_end` comparison; more than one match is an error;
- hunks within a file apply top-down without re-scanning above the previous match;
- `*** Add File:` is create-only and refuses existing paths;
- `*** Delete File:` and `*** Move to:` are rejected, never ignored.

Bounds: ≤32 files, ≤64 hunks, total ≤ `max_write_bytes`. Plan → verify hashes → write.

Failure texts:

```text
patch: missing '*** Begin Patch' header
patch: missing '*** End Patch' terminator
patch: line 7: unsupported directive '*** Delete File'
hunk 2/4 (crates/x/src/lib.rs): context not found near line 412
hunk 1/4 (crates/x/src/lib.rs): context matches 3 locations (lines 12, 88, 140)
crates/x/src/lib.rs: '*** Add File' target already exists
crates/y/src/lib.rs changed while patching; nothing was written
```

Receipt: `{"files":[{"path","hunks","added","removed"}],"mode":"atomic"|"best_effort"}`.

### bread / bwrite / bedit

- `bread`: `path`, `offset` (0), `limit`, `format` (`mcp` / `base64` / `hex`); `mcp` returns typed image/audio/blob content and requires a complete file.
- `bwrite`: `path`, `data`, `encoding` (`base64` / `hex`); creates or replaces.
- `bedit`: `path`, `edits:[{offset, length?, data, encoding}], sha256?`; `length` defaults to the decoded size; `0` inserts; empty `data` with positive `length` deletes.

`bedit` rules: offsets are relative to the original file — sort ascending and track the applied delta; overlap is an error; `offset > len` is an error; ≤32 ranges; result ≤ `MAX_PATCH_FILE_BYTES`; all-or-nothing; optional `sha256` precondition (lowercase hex of the current content) checked before writing.

Failure texts:

```text
bedit 2/3: range overlaps range 1 (offset 1024, length 16)
bedit 1/3: offset 999 is beyond end of file (512 bytes)
bedit: sha256 mismatch (expected …, found …)
```

Receipt: `{"path","ranges","bytes_written","size_before","size_after"}`.

## Capability gating

| Capability | Tools |
| --- | --- |
| read | `ls`, `grep`, `read`, `bread` |
| write | `write`, `bwrite` |
| read+write | `edit`, `patch`, `bedit` |
| shell | `bash` (Unix) or `powershell` (Windows) |

## Rename sweep

Source: `#[tool(name = …)]` routes and handler fns (`read_binary`, `write_binary`, `patch_binary`), `tool_router_for_policy` arrays, `is_concurrent_read_tool` (`ls`, `grep`, `read`, `bread`), `summarize_tool_call_arguments` key list and log rendering.

Live docs: `README.md` capability tables, `docs/ARCHITECTURE.md` tool list and "Binary MCP content", `docs/CHATGPT_PLUGIN.md`, `docs/SECURITY.md`, `.agents/docs/configuration.md`, `CHANGELOG.md`.

Historical record: `.agents/plans/dotlink-handoff-2026-10-01/` is the dated design/plan record and is not rewritten wholesale; `08-MCP-TOOLS-TRANSPORTS-RUNTIME-LOGGING.md` was already updated for `ls`/`read` behaviour.

## Constants to add

```rust
const MAX_GREP_HITS: usize = 200;
const MAX_GREP_FILE_BYTES: usize = 1_048_576;
const MAX_GREP_PATTERN: usize = 1024;
const MAX_GREP_PER_FILE: usize = 50;
const MAX_GREP_CONTEXT: usize = 8;
const MAX_EDIT_OPS: usize = 64;
const MAX_EDIT_FILES: usize = 16;
const MAX_PATCH_FILES: usize = 32;
const MAX_PATCH_HUNKS: usize = 64;
const MAX_BEDIT_RANGES: usize = 32;
```

## Dependencies

| Need | Crate | Status |
| --- | --- | --- |
| regex | `regex 1` | already in `Cargo.lock`; add the direct edge |
| globs | `globset 0.4` | new |
| gitignore | `ignore 0.4` | new; `GitignoreBuilder`/`Gitignore` only |

Never added: `walkdir`, `ignore::Walk`/`WalkParallel`, `grep-*` searcher crates.

## Deliberately dropped

- `type:` file-type filter — `glob: "*.{py,pyi}"` is strictly more general.
- `multiline` regex — available through `bash` + `rg -U`.
- Alias routes for renamed tools, and the flat `edit` args form.

## Decisions (closed)

- `grep.output_mode` defaults to `content`.
- `read.limit` defaults to 4096 lines.
- `ls.depth` defaults to 4 when the pattern implies recursion, else 1.
- `numbered` is an option on `read` and `grep`, default true, one tab separator.
- `respect_gitignore` and `skip_hidden` are opt-in.
- `patch` uses the Codex grammar; delete/move are rejected in v1.
- `bedit` takes an optional `sha256` precondition.
- Clean cut on renames and on the flat `edit` form.
- Binary tools are named `bread`, `bwrite`, `bedit`.

No open questions remain. If an implementation detail is not covered here, prefer the smaller behaviour that preserves the invariants and record the choice in this file.

## Implementation notes

Details this design leaves implicit, resolved the smaller way that keeps the invariants, recorded here rather than deviated from silently.

- Glob candidates: `ls` and `grep` test the pattern against the entry path relative to the tool's `path` root **and** against the bare entry name, so `*.rs` stays useful at any depth while `src/**/*.rs` addresses a subtree. `literal_separator(true)` means a bare `*` never crosses `/`. An invalid pattern is a tool error, not an empty result.
- `grep` requires an existing directory at `path`; searching one file is `read`.
- `grep` collects candidate files first and sorts them by path, so paging and output order do not depend on directory enumeration order. Candidate files are bounded at 65536, after which `truncated` is set.
- Files above 1 MiB and files that are not valid UTF-8 are skipped silently rather than reported; text tools stay UTF-8-only.
- The `--` group separator is only emitted when `before`/`after` requested context, because without context there is no gap to mark. Context windows are per file.
- A file that exceeds the 50-match cap sets `truncated`; there is no per-file marker line.
- `count.total` is the sum of the returned per-file counts (the current page), not the whole tree.
- `.gitignore` handling builds one matcher per directory (loaded through the descriptor-rooted reader, never by ambient path), evaluates deepest-first, and lets a whitelist stop the search — git's precedence. A file over 64 KiB or 1024 lines is truncated rather than fatal; an invalid pattern fails the whole call.
- `grep` output is capped at `max_output_bytes` with a trailing `… truncated at N bytes` marker, in addition to the hit-limit marker.
- `edit` numbering in failure text is the 1-based position of the entry in the whole `edits` array; the parenthesised name is the resolved file displayed relative to the launch directory.
- The ambiguity message lists at most 10 matching line numbers, then stops, so a pathological `old_text` cannot produce an unbounded error string.
- Empty `old_text`, too many edits, too many files and combined-size overruns are attributed to the entry position as `edit i/n (path): …` or `edit i/n: …` when the file is not yet known.
- File-count and combined-byte bounds are checked while planning, so a call that exceeds them does no IO beyond the reads already performed.
- Hash verification happens for every changed file before the first write. Writes themselves are sequential and not rolled back if the filesystem fails mid-way; the guarantee is "no write when a snapshot no longer matches".
- `edit` treats an entry that changes nothing as applied but not changed, so it does not appear in the receipt's `files` and does not touch disk.
- `patch` applies over the file's line vector, not byte offsets, so a trailing newline (or its absence) is preserved exactly. Context lines keep the file's own bytes; added lines are inserted verbatim.
- `@@` content that starts with `-` or `+` is treated as unified-diff line numbers and ignored. Anchor text only narrows the search start; if the anchor is not found the search starts at the previous hunk instead of failing.
- A hunk whose old block has no context or removed lines cannot be located and reports `context not found` — v1 has no insertion-only hunks outside `*** Add File`.
- The ambiguity message lists at most 10 line numbers.
- `patch` accepts a bare `*** Update File` with no hunks as an error, and a repeated path across sections as an error (`appears in more than one section`) in both modes.
- `best_effort` returns the receipt with an extra `failures: [{path, error}]` array when anything failed; `mode` says which contract applied. Atomic mode returns the first failure as a tool error with nothing written.
- `Add File` content is the concatenation of `+` lines plus one trailing newline; an empty section creates an empty file.
- `bedit` reports the offending range by its position in the request array, even after sorting by offset; the overlap message names the preceding range by its request position and its own offset/length.
- `bedit` also accepts a whole-file bound (`MAX_PATCH_FILE_BYTES`) for the source and the result; the source bound keeps the read in memory bounded.
- `sha256` comparison is case-insensitive, and the mismatch message prints the expected and found digests.
- `bedit` decodes every range before checking overlap, so an invalid payload is reported before any overlap or size decision.

## Validation

- Unit: glob grammar; `depth` default rule; `kind`/`sort`; grep modes, context, pagination, ignore/hidden; edit ordering, uniqueness, substring guard, rollback; patch parse/apply, add-file, ambiguity, delete/move rejection; bedit range math, overlap, `sha256`.
- Integration: stdio `tools/list` probe and a `tools/call` smoke per new tool; `EmbeddedMcp` dispatch for `grep`, `edit`, `patch`, `bedit`.
- Gates: `cargo fmt --check`, `cargo test --locked --all-features`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `git diff --check`, `markdownlint-cli2`, `DOTLINK_TEST_BWRAP=1 cargo test --locked --all-features`, and `nix flake check` when dependencies change.

## Definition of done

- All eleven tools advertised with behaviour-only descriptions and the capability gating above.
- Old binary names absent from source, live docs, and tests.
- `edit` is array-only; `ls` has no `recursive`; `grep` has the three output modes.
- Every failure text above is reachable and asserted by a test.
- Gates green; each commit scoped to owned paths; nothing pushed.

## References

- `../aw/crates/aw-tools/src/search.rs` — `ls`/`find`/`grep`, `GitignoreBuilder` use, bounds.
- `../aw/crates/aw-tools/src/lib.rs` — exact-match `read`/`write`/`edit`, always-numbered reads.
- Codex 0.162.1 binary strings — `apply_patch_tool_type: "freeform"`, `update_plan`, `view_image`.
- Claude Code 2.1.300 binary strings — `Edit`/`MultiEdit` `old_string`/`new_string`/`replace_all`, `Grep` `output_mode`, `Read` `cat -n`.
- `src/mcp.rs`, `src/transports/openai.rs` — current implementation surface.
