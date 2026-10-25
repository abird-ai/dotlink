# dotlink-tool-surface-2026-10-25 — checklist

Base: `1b096da`

Design and decisions: [README.md](README.md). Status: design closed, implementation not started.

## Uncommitted

### Commit 0 — land the existing v1 set

- [ ] Commit the working tree as-is: client-controlled shell limits, tunnel `response_timeout` coordination, progress heartbeats, structured shell timeout errors, `ls` v1, numbered `read`, schema ceiling reporting, docs
- [ ] Confirm `cargo test --locked --all-features` (189 tests) and the full gate list before committing

### Commit 1 — dependencies and glob grammar

- [ ] Add `regex` (direct edge), `globset`, `ignore` to `Cargo.toml`; resolve `Cargo.lock`
- [ ] Replace `glob_matches` with `globset` for `ls.pattern`; delete the hand-rolled matcher and its test
- [ ] Keep `literal_separator(true)` and accept `*`, `?`, `[...]`, `**`, `{a,b}` everywhere

### Commit 2 — binary renames

- [ ] `read_binary` → `bread`, `write_binary` → `bwrite`, `patch_binary` → `bedit` (routes, handlers, descriptions)
- [ ] Update `tool_router_for_policy` (read: `ls`/`grep`/`read`/`bread`; write: `write`/`bwrite`; read+write: `edit`/`patch`/`bedit`)
- [ ] Update `is_concurrent_read_tool` to `ls`/`grep`/`read`/`bread`
- [ ] Update `summarize_tool_call_arguments` keys and TOOL log rendering
- [ ] Update `README.md`, `docs/ARCHITECTURE.md`, `docs/CHATGPT_PLUGIN.md`, `docs/SECURITY.md`, `.agents/docs/configuration.md`

### Commit 3 — ls and read

- [ ] `ls`: `depth` default 4 when `pattern` contains `/` or `**`, else 1; description states the rule
- [ ] `ls`: `kind` (`any`/`file`/`dir`) and `sort` (`path`/`mtime`) filters
- [ ] `read`: `limit` default 4096, single trailing truncation marker, `numbered` documented as an option

### Commit 4 — grep

- [ ] Native `secure_fs`-rooted walk reusing the `list_impl` shape; no pathname traversal
- [ ] `output_mode` `content`/`files`/`count`; `before`/`after` capped at 8; `numbered` option; `offset` paging
- [ ] `literal`, `ignore_case`, `glob`, regex size limits, per-file cap 50, 1 MiB file cap
- [ ] `respect_gitignore` and `skip_hidden` opt-in; `.gitignore` read through `secure_fs`, 64 KiB / 1024-line bounds, invalid patterns fail closed

### Commit 5 — edit multi-file

- [ ] `edits[]` array-only (drop the flat form); inherited top-level `path`; per-entry `replace_all`
- [ ] Sequential per-file application, uniqueness, substring-of-previous-`new_text` guard, hash re-check before write, all-or-nothing
- [ ] Description states that `read` prefixes `N\t` and `old_text` must exclude it; hint when `old_text` starts with `digits + tab`
- [ ] Bounds 64 edits / 16 files; receipt

### Commit 6 — patch

- [ ] Codex grammar parser with mandatory header/terminator
- [ ] Content-based hunk location, exact then `trim_end` fallback; multiple matches fail; hunks progress top-down
- [ ] `*** Add File:` create-only; `*** Delete File:` / `*** Move to:` rejected with an explicit message
- [ ] Atomic plan/verify/write, `best_effort` opt-in, per-hunk failure attribution

### Commit 7 — bedit

- [ ] `edits[]` ranges against the original file, ascending with delta tracking, overlap and bounds rejection
- [ ] Optional `sha256` precondition; receipt with before/after sizes

### Commit 8 — docs and closeout

- [ ] `CHANGELOG.md` under Unreleased (Added/Changed/Fixed)
- [ ] Tool tables in `README.md`, `docs/ARCHITECTURE.md`, `docs/CHATGPT_PLUGIN.md`
- [ ] Append each landed commit hash to its group here; move the queue line when the plan closes

## Tests and verification

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
