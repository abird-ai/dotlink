# dotlink-tool-surface-2026-10-25 — checklist

Base: `1b096da`

Design and decisions: [README.md](README.md). Status: implemented and landed (DL-080).

Landed commits: `4d07303` (0 v1 set), `ffed785` (1 deps/glob), `751e42e` (2 renames),
`716ab2b` (3 ls/read), `15dab52` (4 grep), `7a68e72` (5 edit), `fd2f3b3` (6 patch),
`2f79f22` (7 bedit), this docs commit (8).

## Landed

### Commit 0 — land the existing v1 set — `4d07303`

- [x] Commit the working tree as-is: client-controlled shell limits, tunnel `response_timeout` coordination, progress heartbeats, structured shell timeout errors, `ls` v1, numbered `read`, schema ceiling reporting, docs
- [x] Confirm `cargo test --locked --all-features` (189 tests) and the full gate list before committing

### Commit 1 — dependencies and glob grammar — `ffed785`

- [x] Add `regex` (direct edge), `globset`, `ignore` to `Cargo.toml`; resolve `Cargo.lock`
- [x] Replace `glob_matches` with `globset` for `ls.pattern`; delete the hand-rolled matcher and its test
- [x] Keep `literal_separator(true)` and accept `*`, `?`, `[...]`, `**`, `{a,b}` everywhere

### Commit 2 — binary renames — `751e42e`

- [x] `read_binary` → `bread`, `write_binary` → `bwrite`, `patch_binary` → `bedit` (routes, handlers, descriptions)
- [x] Update `tool_router_for_policy` (read: `ls`/`grep`/`read`/`bread`; write: `write`/`bwrite`; read+write: `edit`/`patch`/`bedit`)
- [x] Update `is_concurrent_read_tool` to `ls`/`grep`/`read`/`bread`
- [x] Update `summarize_tool_call_arguments` keys and TOOL log rendering
- [x] Update `README.md`, `docs/ARCHITECTURE.md`, `docs/CHATGPT_PLUGIN.md`, `docs/SECURITY.md`, `.agents/docs/configuration.md` (the last needed no schema change)

### Commit 3 — ls and read — `716ab2b`

- [x] `ls`: `depth` default 4 when `pattern` contains `/` or `**`, else 1; description states the rule
- [x] `ls`: `kind` (`any`/`file`/`dir`) and `sort` (`path`/`mtime`) filters
- [x] `read`: `limit` default 4096, single trailing truncation marker, `numbered` documented as an option

### Commit 4 — grep — `15dab52`

- [x] Native `secure_fs`-rooted walk reusing the `list_impl` shape; no pathname traversal
- [x] `output_mode` `content`/`files`/`count`; `before`/`after` capped at 8; `numbered` option; `offset` paging
- [x] `literal`, `ignore_case`, `glob`, regex size limits, per-file cap 50, 1 MiB file cap
- [x] `respect_gitignore` and `skip_hidden` opt-in; `.gitignore` read through `secure_fs`, 64 KiB / 1024-line bounds, invalid patterns fail closed

### Commit 5 — edit multi-file — `7a68e72`

- [x] `edits[]` array-only (drop the flat form); inherited top-level `path`; per-entry `replace_all`
- [x] Sequential per-file application, uniqueness, substring-of-previous-`new_text` guard, hash re-check before write, all-or-nothing
- [x] Description states that `read` prefixes `N\t` and `old_text` must exclude it; hint when `old_text` starts with `digits + tab`
- [x] Bounds 64 edits / 16 files; receipt

### Commit 6 — patch — `fd2f3b3`

- [x] Codex grammar parser with mandatory header/terminator
- [x] Content-based hunk location, exact then `trim_end` fallback; multiple matches fail; hunks progress top-down
- [x] `*** Add File:` create-only; `*** Delete File:` / `*** Move to:` rejected with an explicit message
- [x] Atomic plan/verify/write, `best_effort` opt-in, per-hunk failure attribution

### Commit 7 — bedit — `2f79f22`

- [x] `edits[]` ranges against the original file, ascending with delta tracking, overlap and bounds rejection
- [x] Optional `sha256` precondition; receipt with before/after sizes

### Commit 8 — docs and closeout — this commit

- [x] `CHANGELOG.md` under Unreleased (Added/Changed/Fixed)
- [x] Tool tables in `README.md`, `docs/ARCHITECTURE.md`, `docs/CHATGPT_PLUGIN.md`
- [x] Append each landed commit hash to its group here; move the queue line when the plan closes

## Tests and verification

Landed with the commits above; the stdio probe lives in `tests/stdio_tools.rs`.

- [ ] Glob grammar: `*`, `?`, classes, alternation, `**`, no separator crossing
- [ ] `ls`: depth 1 vs pattern-implied 4 vs explicit, `kind`, `sort=mtime`, limit/truncation, deny filtering
- [ ] `read`: numbering, `numbered: false`, offset/limit, truncation marker
- [ ] `grep`: three output modes, before/after, `offset`, `limit`, `literal`, `ignore_case`, `glob`, gitignore and hidden opt-ins, per-file cap
- [ ] `edit`: ordering, uniqueness, substring guard, prefix hint, rollback on failure, per-entry `replace_all`, path inheritance
- [ ] `patch`: parse, apply, add-file, ambiguity failure, delete/move rejection, atomic rollback, `best_effort`
- [ ] `bedit`: multi-range math, overlap rejection, `sha256` mismatch, bounds
- [ ] Rename sweep: no `read_binary`/`write_binary`/`patch_binary` left in source, live docs, or tests
- [ ] stdio `tools/list` probe plus one `tools/call` smoke per new tool
- [ ] Tunnel `EmbeddedMcp` dispatch for `grep`, `edit`, `patch`, `bedit`
- [ ] `cargo fmt --check`
- [ ] `cargo test --locked --all-features`
- [ ] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [ ] `git diff --check`
- [ ] `markdownlint-cli2`
- [ ] `DOTLINK_TEST_BWRAP=1 cargo test --locked --all-features`
- [ ] `nix flake check`
