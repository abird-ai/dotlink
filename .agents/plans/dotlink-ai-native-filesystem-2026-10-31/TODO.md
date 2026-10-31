# DL-150 — AI-native filesystem tool surface

Base: `84cd9b7`

Status: specification only; implementation intentionally not started.

## Committed

None yet. The initial commit records only the approved design and queue entry.

## Uncommitted

### Phase 1 — shared discovery/metadata architecture

- [ ] Introduce one shared metadata model with `size`, `mtime`, `ctime`,
  `btime`, `mode` and platform-precise availability.
- [ ] Refactor descriptor-rooted traversal so `hidden`, `gitignore`,
  `depth`, budgets and incomplete reasons are single-sourced.
- [ ] Introduce shared selector, ordering and compact-rendering primitives.
- [ ] Preserve current deny/protected-path and symlink non-traversal invariants.

### Phase 2 — `ls` + `find`

- [ ] Shrink `ls` to immediate-directory inspection only.
- [ ] Add `find` for recursive glob/regex/kind/depth/time discovery.
- [ ] Add compact path output and opt-in ordered metadata columns.
- [ ] Add `mtime`/`ctime`/`btime` filtering and sorting.
- [ ] Add `hidden` and `gitignore` consistently to both.
- [ ] Preserve bounded globally correct selection/paging.

### Phase 3 — `grep` + read-side schema cleanup

- [ ] Align `grep` with `depth`, `hidden`, `gitignore`.
- [ ] Keep `glob` as the content-search path prefilter.
- [ ] Make content results always line-numbered and remove advertised
  `numbered`.
- [ ] Convert files/count results to compact deterministic text.
- [ ] Stop advertising `read.max_read_bytes`; retain internal ceilings.

### Phase 4 — text mutation schema/output cleanup

- [ ] Advertise `edit` fields as `old`, `new`, `all`.
- [ ] Keep old edit names only as compatibility deserialization if needed.
- [ ] Replace verbose successful mutation receipts with compact outcomes where
  no additional information is needed.
- [ ] Keep Codex patch grammar and leave move/delete to dedicated tools.

### Phase 5 — lifecycle primitives

- [ ] Design/implement `cp(from,to,recursive=false)` through shared mutation
  and staging layers.
- [ ] Design/implement same-filesystem `mv(from,to)`; never implicit
  copy+delete.
- [ ] Design/implement `rm(paths,recursive=false)` with full-request preflight
  and safe recursive namespace removal.
- [ ] Create missing destination parents for `cp`/`mv`.
- [ ] Do not add glob expansion, destination inference or implicit overwrite.
- [ ] Add replay protection/capability visibility for all lifecycle mutations.

### Phase 6 — compatibility, measurements and docs

- [ ] Keep compatibility strictly at wire/deserialization boundaries.
- [ ] Measure total serialized advertised-tool schema tokens before/after.
- [ ] Measure representative `ls`/`find`/`grep` result token cost
  before/after.
- [ ] Update Architecture, Security and user-facing documentation only where the
  final behavior requires it.
- [ ] Remove stale duplicated tool-contract prose from older live docs rather
  than maintaining conflicting specifications.

## Tests and verification

- [ ] Unit tests for shared metadata semantics on supported platforms.
- [ ] Unit tests for `hidden` and `gitignore` parity across discovery tools.
- [ ] Unit tests for glob/regex/depth/kind/time selectors.
- [ ] Unit tests for compact escaping/rendering and result-budget truncation.
- [ ] Unit tests for globally correct sorting/paging under bounded traversal.
- [ ] Mutation race/alias/replay tests for `cp`, `mv`, `rm`.
- [ ] Recursive copy/delete protected-path and symlink tests.
- [ ] Cross-device `mv` rejection tests.
- [ ] Cross-platform native Windows/macOS filesystem tests.
- [ ] `cargo fmt --all -- --check`.
- [ ] `cargo clippy --locked --all-targets --all-features -- -D warnings`.
- [ ] `cargo test --locked --all-features` with at least 8 build jobs.
- [ ] `nix flake check -L`.
- [ ] Markdown lint.
- [ ] Full CI matrix green.
