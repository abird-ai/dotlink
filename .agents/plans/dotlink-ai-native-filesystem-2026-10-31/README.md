# DL-150 — AI-native filesystem tool surface

Author: Prasanna Loganathar

Co-authors: ChatGPT (GPT-5.6 Sol)

Owner: @pvl

Status: design approved; specification only. No implementation has begun.

Base: `84cd9b7` (`release: dotlink v0.14.3`)

## Why

dotlink's native filesystem tools are consumed primarily by AI models, not by
humans typing commands. The surface should therefore optimize for model tool
selection, schema-token cost, result-token cost, deterministic composition,
bounded work and robust filesystem semantics rather than for compatibility with
Unix command-line conventions.

The current surface has one important overlap: `ls` is both a directory lister
and a recursive discovery tool. It carries glob, depth, kind and sort behavior
that belongs to a finder. At the same time, a non-shell runtime still lacks the
basic filesystem lifecycle operations needed for ordinary repository work:
copy, rename/move and remove.

The target is a complete coding/repository filesystem environment that remains
useful when shell authority is deliberately withheld.

## Design principles

1. **AI-native, not shell-compatible.** Preserve familiar tool names where they
   give the model a clear semantic cue, but do not reproduce shell syntax,
   positional magic or historical flags.
2. **One concept per tool.** Prefer several tiny orthogonal tools over one
   polymorphic filesystem tool.
3. **Structured inputs, compact outputs.** Typed MCP input schemas reduce
   invocation errors; compact deterministic text avoids repeated JSON keys and
   request echoes in model context.
4. **Minimal schema entropy.** Shared concepts use exactly one name everywhere.
   Do not expose aliases or transport internals in model-facing schemas.
5. **Exact paths for mutation.** `cp`, `mv` and `rm` never expand globs or
   infer destination basenames.
6. **Selection and mutation are separate.** Use `find` to select objects, then
   pass exact paths to a mutation tool.
7. **Bounded work everywhere.** Traversal, metadata, content scanning, copying,
   recursive deletion, planning and output retain explicit hard ceilings and
   incomplete-result signaling.
8. **Shared invariants live low.** Authorization, descriptor-rooted traversal,
   metadata, ordering, mutation coordination and compact rendering belong below
   individual tools.
9. **No redundant success data.** A successful mutation normally returns
   `ok`; do not echo paths or arguments the model already supplied.
10. **Compatibility is a wire-boundary concern.** Transitional aliases may be
    accepted while deserializing old requests, but never appear in the new
    advertised schema.

## Target surface

~~~text
discovery   ls      find    grep
text        read    write   edit    patch
lifecycle   cp      mv      rm
binary      bread   bwrite  bedit
shell       bash    powershell      # separate capability, not required
~~~

The model-facing decision boundary should be nearly mechanical:

| Intent | Tool |
| --- | --- |
| inspect this directory | `ls` |
| locate filesystem entries | `find` |
| locate text in files | `grep` |
| inspect a known text file | `read` |
| create/replace complete text | `write` |
| replace exact known text | `edit` |
| make contextual/multi-file text changes | `patch` |
| duplicate an object | `cp` |
| rename an object | `mv` |
| remove objects | `rm` |
| inspect/create/mutate arbitrary bytes | `bread` / `bwrite` / `bedit` |

## Shared vocabulary

Use the following names consistently in public schemas, results, documentation
and shared internal types:

~~~text
path
glob
regex
depth
hidden
gitignore

size
mtime
ctime
btime
mode
~~~

Do not advertise aliases such as `pattern` for a path glob, `created`,
`changed`, `modified`, `include_hidden` or `skip_hidden`.

### Timestamp semantics

- `mtime`: content modification time.
- `ctime`: filesystem status/change time. It does **not** mean creation time.
- `btime`: birth/creation time.

A backend/filesystem may not provide every timestamp. Missing values are
represented as unavailable rather than synthesized.

For sorting, unavailable timestamp values sort last.

For timestamp filtering, an entry lacking the requested timestamp does not
match.

The native metadata layer should gather these values in one operation where the
platform permits it; individual tools must not implement their own definitions.

### Hidden and gitignore

Use positive booleans consistently:

~~~text
hidden=false
gitignore=false
~~~

- `hidden=false`: hidden entries do not participate and hidden directories are
  not descended.
- `hidden=true`: hidden entries participate.
- `gitignore=false`: ignore files do not affect traversal.
- `gitignore=true`: bounded, descriptor-rooted `.gitignore` semantics apply.

On Unix, dot-prefixed names are hidden. On platforms with a native hidden
attribute, that attribute should also participate.

## Compact output contract

The normal consumer is an AI model. Repeating JSON field names for every
filesystem entry is unnecessary token overhead.

Discovery tools should return deterministic compact text with one entry per
line. Unsafe/control characters in path labels must be escaped so a single
entry cannot create ambiguous extra records.

Kind prefixes:

~~~text
f  file
d  directory
l  symlink
o  other
~~~

### `ls` normal output

Because the requested directory is already known, return basenames:

~~~text
d  src
f  Cargo.toml
f  README.md
l  current
~~~

### `find` normal output

Return reusable paths relative to the requested root/cwd display convention:

~~~text
f  src/main.rs
f  src/mcp.rs
d  src/mcp/tools
~~~

### Metadata output

Metadata is opt-in through a compact `meta` array. Only requested values are
rendered, in request order, before the path.

For example, `meta=["size","mtime"]`:

~~~text
f  12842  2026-10-31T01:20:04Z  src/main.rs
f  98127  2026-10-30T22:10:11Z  src/mcp.rs
~~~

Unavailable metadata renders as `-`.

### Incomplete results

Do not wrap every successful discovery result in a JSON status envelope. A
complete result has no footer. Partial results end with one compact marker:

~~~text
… more
… incomplete:budget
… incomplete:walk
… incomplete:gitignore
… incomplete:walk,gitignore
~~~

The exact internal reasons may remain richer, but the public vocabulary should
be small, stable and actionable.

## Limits, partial data and commit outcomes

Resource ceilings and logical pagination are different concerns.

The model chooses the logical region it wants. dotlink owns the hard resource
policy:

~~~text
profile/runtime policy
  max_read_bytes
  max_write_bytes
  max_output_bytes
  scan/planning/work budgets

transport
  request/result envelope ceilings

tool call
  line/lines/byte, offset/limit, depth, etc.
~~~

Do not advertise operator ceilings as routine model choices merely because the
implementation needs them. Surface the effective ceiling only when it is
actionable, such as an over-limit error.

### Read-class operations may return partial data

A bounded read/search/list is not a failed mutation. When a valid prefix/page
fits but more data exists, return the usable data as a successful tool result
plus an exact continuation/incomplete marker.

For `read`, the continuation marker is a cursor, not merely prose:

~~~text
… continue line=57 byte=0
… continue line=42 byte=1048217
~~~

The first form means continue at the next logical line. The second continues
inside an unusually long logical line.

The cursor always names the first byte not returned and must be on a valid UTF-8
boundary. A transport result ceiling tighter than `max_read_bytes` uses the
same continuation mechanism; the model does not need to know which lower layer
caused the page boundary.

For discovery tools, bounded traversal remains a successful partial result with
the compact `… more` / `… incomplete:reason` footer.

A hard root/open/authorization failure before useful data exists is an ordinary
tool error.

### Mid-read I/O failure

If a single-file `read` encounters an I/O failure after already obtaining a
valid prefix, preserve the useful prefix but mark the MCP tool result as an
error and terminate it with an explicit fault marker, for example:

~~~text
… incomplete:io continue line=81 byte=0
~~~

The model must never infer completeness from such a result. The cursor is
provided only when the implementation can identify the first unreturned UTF-8
byte safely.

If no useful text was obtained, return only the concise error.

A semantic text failure such as invalid UTF-8/binary content is also an error;
do not convert the file into a misleading successful text prefix. Direct the
model to `bread` where appropriate.

For tree-wide `find`/`grep` discovery, an unreadable individual entry may
instead contribute an incomplete-scan reason while the rest of the result
remains useful. Root-level failures still fail the call.

### Writes never use partial output as pagination

`max_write_bytes` is an atomic-operation ceiling, not a chunk size.

If the complete requested result exceeds it, reject the request before staging
or mutating anything:

~~~text
content is 1400000 bytes; write limit is 1048576
~~~

The model may then choose a different operation or the operator may raise the
profile limit. dotlink must never write the first N bytes and call that a
partial write.

The same principle applies when the transport request ceiling rejects a payload:
the tool has not executed and no mutation occurred.

### Mutation commit states

Every mutation backend must classify failures around an explicit publication
commit point:

~~~text
preflight / plan / stage
        |
        | failure => NOT COMMITTED
        |            visible target unchanged/absent
        v
atomic publication
        |
        | success => COMMITTED
        v
durability / hidden cleanup
~~~

Before publication, a failure is a normal tool error and the visible namespace
must remain unchanged. Temporary/staged data is private implementation state and
is cleaned best-effort without becoming a user-visible partial result.

After publication succeeds, the requested namespace mutation has happened.
Later sync/cleanup failures must never be reported as a generic "write failed"
that invites the model to repeat the mutation. Report a committed success with
a concise exceptional qualifier only when the model needs to know, for example:

~~~text
ok; durability uncertain
ok; cleanup pending
~~~

A platform backend that genuinely cannot determine whether publication crossed
the commit point must surface an explicit `uncertain` outcome and must not
encourage automatic retry.

The OpenAI transport replay layer separately protects the case where the
mutation committed but its response could not be delivered: redelivery replays
the committed result rather than executing the side effect again, and an
unrecoverable ambiguity remains fail-closed.

### Batch mutations

A multi-object operation cannot generally promise one filesystem transaction.

Strict multi-file `patch` preflights and stages everything before the first
publication, but a later commit failure can still occur after earlier files
were committed. Likewise a multi-path `rm` may encounter a platform failure
after one namespace operation has committed even after full preflight.

These exceptional outcomes must be explicit and must list only the information
the model needs to recover, for example:

~~~text
partial 2/3 committed
src/a.rs
src/b.rs
failed src/c.rs: permission denied
~~~

Such a result is an MCP tool error, not an ordinary success. The word
`partial` is reserved for a real visible mutation split; never use it for
private staging progress.

Where a batch operation can cheaply roll back unpublished/tombstoned objects
while locks are still held, it should do so, but rollback failure must itself be
reported as a partial/uncertain state rather than hidden.

## `ls`

### Purpose

Inspect exactly one directory.

`ls` is never recursive and never performs pathname search.

### Proposed schema

~~~text
ls(
  path=".",
  sort="path",
  meta=[],
  hidden=false,
  gitignore=false,
  offset=0,
  limit=256
)
~~~

Allowed `sort` keys:

~~~text
path
size
mtime
ctime
btime
~~~

Ordering is fixed to avoid another model-facing parameter:

- `path`: ascending.
- `size`, `mtime`, `ctime`, `btime`: descending.
- ties: path ascending.
- unavailable timestamps: last.

### Removed from `ls`

The current recursive/discovery concepts move out:

~~~text
pattern / glob
depth
kind
recursive traversal
~~~

These belong to `find`.

## `find`

### Purpose

Locate filesystem entries by pathname, type, depth or metadata.

It never searches file contents.

### Proposed schema

~~~text
find(
  path=".",
  glob=null,
  regex=null,
  depth=null,
  kind="any",

  hidden=false,
  gitignore=false,

  mtime=null,
  ctime=null,
  btime=null,

  sort="path",
  meta=[],

  offset=0,
  limit=200
)
~~~

Kinds:

~~~text
any
file
dir
symlink
other
~~~

`depth=null` means recursive within dotlink's bounded traversal budget.
`depth=1` means immediate children. The named root itself is not a result.

### Glob semantics

Keep the existing useful glob grammar:

~~~text
*
?
[abc]
{rs,md}
**
~~~

A glob with no `/` matches basenames at any depth:

~~~text
glob="*.rs"
~~~

A glob containing `/` matches the relative path:

~~~text
glob="src/**/*.rs"
~~~

### Regex semantics

`regex` matches the relative pathname through the existing bounded Rust regex
engine. It is a path matcher, not a content matcher.

When both `glob` and `regex` are supplied, both must match. All filters are
ANDed.

### Time filters

Each timestamp accepts the same small range object:

~~~text
mtime={after: RFC3339?, before: RFC3339?}
ctime={after: RFC3339?, before: RFC3339?}
btime={after: RFC3339?, before: RFC3339?}
~~~

Do not add a relative-time mini-language such as `-7d`; the calling model can
translate user-relative time into an absolute timestamp without creating a
second grammar.

## `grep`

### Purpose

Search UTF-8 file contents.

It remains a one-call content search; do not force the model to run `find`
followed by one `grep` call per path.

### Proposed schema

~~~text
grep(
  pattern,
  path=".",
  glob=null,
  depth=null,

  literal=false,
  ignore_case=false,

  mode="content",
  before=0,
  after=0,

  hidden=false,
  gitignore=false,

  offset=0,
  limit=200
)
~~~

Modes:

~~~text
content
files
count
~~~

`glob` remains a cheap path prefilter. `depth`, `hidden` and `gitignore`
share exactly the same traversal semantics as `find`.

Remove model-facing `numbered`: content matches always include line numbers.
Raw unnumbered content remains available through `read(numbered=false)`.

Compact outputs:

~~~text
# content
src/main.rs:42:foo();
src/lib.rs:19:foo();

# files
src/main.rs
src/lib.rs

# count
3  src/main.rs
1  src/lib.rs
~~~

Existing bounded regex, UTF-8 classification, per-file scan caps, context
windows, paging and output ceilings remain.

## `read`

### Purpose

Read a known UTF-8 file with line-oriented paging and safe continuation inside
an unusually long line.

### Proposed schema

~~~text
read(
  path,
  line=1,
  lines=4096,
  byte=0,
  numbered=true
)
~~~

- `line` is the first 1-based logical line.
- `lines` is the maximum number of logical lines requested.
- `byte` is a zero-based UTF-8 byte offset inside the first requested logical
  line. It normally remains zero and exists primarily for long-line
  continuation.
- `numbered` controls the ordinary line-number prefix.

The profile `max_read_bytes`, scan budget and transport result budget remain
enforced internally. They are runtime/operator policy, not routine
model-controlled pagination fields.

The effective page budget is the tightest relevant lower-layer ceiling. The
renderer reserves room for the continuation marker before accepting the final
fragment.

Default output remains line-numbered because those numbers are useful for
follow-up reasoning:

~~~text
41  fn foo() {
42      ...
43  }
~~~

### Long lines

A logical line may be much larger than the effective page budget. That is not
an error.

Return the largest valid UTF-8 fragment that fits, then an exact cursor:

~~~text
42  {"very":"long", ... bounded fragment ...
… continue line=42 byte=1048217
~~~

A follow-up:

~~~text
read(path="large.json", line=42, byte=1048217)
~~~

continues the same logical line. Once that line is complete, the call may
continue with subsequent lines up to `lines` and the page budget.

The byte cursor counts UTF-8 source bytes inside the logical line, not rendered
line-number prefix bytes, and always lands on a valid UTF-8 boundary.

When the page ends between complete lines, use `byte=0` for the next line:

~~~text
… continue line=57 byte=0
~~~

This same cursor shape covers the logical line limit, `max_read_bytes` and a
tighter transport result ceiling.

`numbered=false` removes line-number prefixes from returned file text. A
continuation/incomplete footer is control information, not file content, and
must remain clearly separated from the returned fragment.

Compatibility may accept the old `offset`/`limit` fields at the wire
boundary, but the new advertised schema is `line`/`lines`/`byte`.

## `write`

### Purpose

Create or completely replace one UTF-8 file.

~~~text
write(path, content)
~~~

Missing parent directories are created automatically.

Success:

~~~text
ok
~~~

No-op:

~~~text
unchanged
~~~

Do not echo the path/content.

## `edit`

### Purpose

Perform one exact old-to-new text substitution in one known file.

Batch/contextual editing belongs to `patch`; keeping `edit` singular makes
the model's tool choice obvious and substantially reduces schema/call tokens.

### Proposed schema

~~~text
edit(
  path,
  old,
  new,
  all=false
)
~~~

Semantics:

- `old` must be non-empty;
- without `all`, `old` must occur exactly once;
- zero matches are an error;
- multiple matches are an error with a concise count/remedy;
- `all=true` replaces every exact occurrence;
- `old == new` is a no-op;
- snapshot/verification, shared mutation locking, staging and atomic publication
  remain mandatory;
- the complete resulting file must fit the configured mutation/write planning
  limits before publication.

Results:

~~~text
ok
unchanged
~~~

Typical ambiguity error:

~~~text
old matches 4 times; set all=true or use patch
~~~

This creates a sharp decision boundary:

~~~text
complete desired file                 write
one exact old -> new substitution     edit
multiple/contextual/multi-file edits  patch
~~~

Compatibility may accept the old array form and old field names only at the
wire/deserialization boundary. They must not appear in the advertised schema or
new shared internal representation.

## `patch`

### Purpose

Contextual and multi-file text changes.

Keep the Codex patch grammar. Models already know it and it is much more
token-efficient than a nested JSON hunk schema.

~~~text
patch(
  patch,
  best_effort=false
)
~~~

Continue supporting update and add-file sections.

Do not add patch-level delete or move directives. `rm` and `mv` provide
dedicated, explicit lifecycle semantics instead.

Strict mode continues to preflight every section before commits; each file
replacement is atomic while the multi-file commit sequence is not a filesystem
transaction.

## `cp`

### Purpose

Duplicate one exact filesystem object.

### Proposed schema

~~~text
cp(
  from,
  to,
  recursive=false
)
~~~

`to` is always the exact resulting path.

There is no shell-style rule where a directory destination implicitly appends
the source basename.

### Semantics

- destination must not exist;
- missing destination parents are created;
- directories require `recursive=true`;
- symlinks are copied as symlinks and never followed;
- recursive traversal is descriptor-rooted and bounded;
- policy-denied/protected descendants fail the operation closed;
- file bytes and directory structure are preserved;
- basic mode metadata is preserved where the platform safely supports it;
- ownership, ACL, xattr and timestamp preservation are not promised as
  filesystem-administration semantics.

Do not initially expose `replace=true`. Replacement is deliberately a
separate destructive decision.

For robust publication:

- file copy writes staged bytes and atomically publishes the destination;
- directory copy should construct a staged destination tree and publish the
  completed root by rename where the platform permits;
- a failed copy must not expose a partially built destination.

Success:

~~~text
ok
~~~

## `mv`

### Purpose

Rename one exact filesystem object.

### Proposed schema

~~~text
mv(from, to)
~~~

`to` is the exact final path.

### Semantics

- destination must not exist;
- missing destination parents are created;
- files, directories and symlinks are supported;
- symlinks themselves move; their targets are never followed;
- moving a directory into itself is rejected;
- authorization/protected-path checks cover both source and destination;
- directory moves must not permit a protected/denied subtree to escape policy.

Most importantly, `mv` is a rename operation only.

It never silently degrades to copy + delete. Cross-device/volume moves fail
with a concise actionable error:

~~~text
cross-device move; use cp then rm
~~~

The invariant is: rename the existing object or change nothing.

Success:

~~~text
ok
~~~

Source and destination resolving to the same operation may return:

~~~text
unchanged
~~~

## `rm`

### Purpose

Remove exact filesystem objects.

### Proposed schema

~~~text
rm(
  paths,
  recursive=false
)
~~~

`paths` is an array because bulk deletion is common and cheaper than one tool
call per path.

No globs.

### Semantics

Without `recursive`:

- files: allowed;
- symlinks: allowed and never followed;
- empty directories: allowed;
- non-empty directories: rejected.

With `recursive=true`:

- exact named directory trees may be removed;
- traversal remains bounded;
- protected/denied descendants fail closed;
- symlinks are removed as links, never traversed.

Missing paths are errors rather than silently successful operations. A typo must
not look like a successful deletion.

For multiple roots, preflight the complete request before making namespace
changes wherever possible.

For robust recursive deletion, prefer:

~~~text
validate
→ atomically rename root to private tombstone
→ namespace removal committed
→ delete tombstone
~~~

rather than exposing a tree that disappears child-by-child. Private cleanup
names must remain invisible to discovery. Cleanup failure must not resurrect the
original path.

Success:

~~~text
ok
~~~

or for multiple roots:

~~~text
ok 7
~~~

## Binary tools

Keep binary operations separate from text operations. This avoids making every
text schema carry encoding and byte-range concerns.

### `bread`

Retain the existing byte-window model and typed MCP image/audio/blob support.

### `bwrite`

Retain path + encoded payload + encoding.

### `bedit`

Retain the existing offset/length/data range model and optional SHA-256
precondition.

These are specialized enough that aggressive public-field shortening is not a
priority unless schema measurement shows material savings.

## Compact success and error contract

MCP already distinguishes success from tool errors. Do not wrap successful
mutations in JSON such as `{"success":true}`.

Preferred successful vocabulary:

~~~text
ok
unchanged
ok 12
~~~

Errors should be concise and actionable:

~~~text
destination exists: src/new.rs
non-empty directory; set recursive=true
cross-device move; use cp then rm
old matches 3 times; set all=true or provide more context
~~~

Do not echo complete requests, payloads or long path lists unless necessary to
identify the failure.

## Sorting and paging

`ls` and `find` share:

~~~text
offset
limit
sort
~~~

Default sort is `path`.

Ordering is fixed:

~~~text
path       ascending
size       descending
mtime      descending
ctime      descending
btime      descending
~~~

Path ascending is the deterministic tie-breaker.

Do not initially add `reverse` or `order`. Add it only if real model traces
show that oldest/smallest-first queries justify another parameter.

Selection under a limit must be globally correct over the completed bounded
scan, not an arbitrary traversal-order prefix.

## Shared internal architecture

The public redesign should also simplify the implementation. Shared concepts
must move below individual tool modules.

Target shape:

~~~text
filesystem/
  metadata
  traversal
  selector
  ordering
  render
  mutation

tools/
  ls
  find
  grep
  read
  write
  edit
  patch
  cp
  mv
  rm
  bread
  bwrite
  bedit
~~~

The exact Rust module layout may differ; the architectural rule matters more
than the filenames.

Shared concepts should include equivalents of:

~~~text
WalkOptions
Entry
EntryKind
EntryMeta
TimeRange
EntrySelector
EntrySort
WalkProgress
CompactRenderer
~~~

### Shared discovery layer

`ls`, `find` and `grep` must reuse one descriptor-rooted walk that owns:

- root authorization;
- deny/protected checks;
- hidden handling;
- bounded `.gitignore`;
- symlink non-traversal;
- visit/work budgets;
- metadata acquisition;
- incomplete-reason collection.

`ls` becomes essentially depth-one traversal + ordering + rendering.

`find` becomes traversal + entry selector + ordering + rendering.

`grep` becomes traversal + cheap path selector + bounded content scanner.

### Shared metadata layer

The common entry record should carry, when available:

~~~text
path
relative
kind
size
mtime
ctime
btime
mode
~~~

Metadata acquisition must preserve the existing race-resistant descriptor-rooted
design. Do not add independent pathname metadata calls merely to populate new
fields.

### Shared mutation layer

`write`, `bwrite`, `edit`, `patch`, `bedit`, `cp`, `mv` and `rm`
must all reuse the established lower-layer invariants:

~~~text
lexical admission
protected-path checks
resolved authority
name reservation
cross-process coordination
snapshot/object verification where applicable
staging
atomic publication
bounded planning
compact receipts
~~~

Do not implement `cp`, `mv` or `rm` as independent pathname-manipulation
islands.

All three new lifecycle tools are side-effecting and therefore require tunnel
request replay protection.

## Capability model

Tool visibility remains permission-aware.

| Tool | Required authority |
| --- | --- |
| `ls`, `find`, `grep`, `read`, `bread` | read |
| `write`, `bwrite` | write |
| `edit`, `patch`, `bedit` | read + write |
| `cp` | read source + write destination |
| `mv` | mutation authority for source + destination |
| `rm` | mutation/write authority |

The final implementation should determine whether `TOOL_SPECS` needs a more
precise capability representation for mixed-source/destination operations,
rather than weakening policy semantics to fit the current enum.

If the effective policy cannot execute a tool at all, do not advertise it.

## Deliberate omissions

### No `sed`

`edit`, `patch` and `write` already cover filesystem text mutation with a
clearer model decision boundary and stronger typed semantics.

A stream transformer belongs to shell/process composition.

### No `tree`

`find(depth=N)` covers tree discovery.

### No `stat`

`ls(meta=[...])` and `find(meta=[...])` cover metadata inspection.

### No `mkdir` initially

`write`, `cp` and `mv` create missing destination parents. Intentionally
empty directories are rare in coding repositories and Git does not track them.

Add a dedicated primitive only if model traces show a real need.

### No `touch` initially

Timestamp-only mutation is not a core coding operation.

### No `chmod`, `chown`, symlink or hardlink creation initially

These are filesystem-administration capabilities beyond the basic repository
editing surface. Add them independently only when real workloads justify the
extra authority and schema.

## Non-shell completeness

With this surface an AI model can complete the ordinary repository lifecycle
without arbitrary process execution:

~~~text
inspect repository        ls
locate source/config      find
search identifiers        grep
inspect files             read
create/replace files      write
exact substitutions       edit
contextual refactors      patch
duplicate paths           cp
rename paths              mv
remove obsolete paths     rm
binary work               bread / bwrite / bedit
~~~

The remaining omissions are filesystem-administration features rather than
ordinary coding/file-management operations.

## Token-efficiency policy

Tool-schema size and result size are first-class engineering budgets.

### Schema rules

- one concise tool description;
- short property descriptions;
- constraints in JSON Schema rather than repeated prose where practical;
- no aliases in the advertised schema;
- no examples repeated inside every tool definition;
- no transport framing/ceiling explanation unless it changes a model decision;
- no overloaded shell-compatible flags;
- use the same property name for the same concept everywhere.

Measure the serialized tool schemas before and after implementation. Treat a
material increase in total advertised schema tokens as a design regression that
requires justification.

### Result rules

Default outputs contain only what the next model step needs:

~~~text
ls                  kind + basename
find                kind + reusable path
grep                path + line + content
read                line + content
mutation success    ok / unchanged
~~~

Metadata is opt-in.

Do not repeat JSON keys per filesystem entry.

Do not echo successful request arguments.

Do not return verbose success receipts unless an operation has a materially
different partial/uncertain outcome that the model must reason about.

For read-only pagination, return usable data plus a compact continuation marker.
For mutation failures, never return physical staging progress as partial
success. Report only externally visible commit state.

## Compatibility strategy

The new model-visible schema is authoritative.

Where transition cost justifies it, accept old fields only while
deserializing:

~~~text
grep.skip_hidden
  → hidden = !skip_hidden

read.offset / read.limit
  → line / lines

legacy edit array + old_text/new_text/replace_all
  → singular path / old / new / all
~~~

Old recursive `ls` fields may temporarily be accepted by a compatibility
adapter that dispatches to the new discovery implementation, but new clients
must see only the reduced `ls` schema.

Compatibility must not force old representations into shared internal types.

A later cleanup can remove the wire compatibility after the migration window.

## Implementation constraints

This document intentionally specifies no implementation commit yet. When the
plan is activated:

1. refactor shared metadata/traversal/rendering first;
2. introduce `find` and shrink `ls` on top of that shared layer;
3. align `grep` traversal vocabulary and compact output;
4. simplify `read`/`edit` model-facing schemas with compatibility adapters;
5. add `cp`, `mv`, `rm` through the shared mutation layer;
6. measure tool-schema and result token costs;
7. run the full cross-platform, Nix, replay, mutation and CI validation matrix.

Do not implement lifecycle operations before their shared mutation semantics are
designed against Unix, macOS and Windows rename/delete behavior.

## Acceptance criteria

The implementation is complete only when all of the following hold:

- `ls` is immediate-directory-only.
- `find` owns recursive pathname/metadata discovery.
- `grep` exclusively searches contents while retaining cheap path filtering.
- `hidden`, `gitignore`, `mtime`, `ctime`, `btime` mean the same thing
  everywhere.
- discovery defaults produce compact text rather than per-entry JSON objects.
- metadata is opt-in.
- mutation success does not echo redundant request data.
- `max_read_bytes` and `max_write_bytes` remain runtime/profile policy rather
  than routine model-facing knobs.
- `read` can continue inside a line larger than the effective result budget
  using an exact UTF-8 `line` + `byte` cursor.
- ordinary read pagination returns usable partial data; a mid-read I/O fault is
  explicitly incomplete and error-marked.
- over-limit or mid-stage writes never expose partial target contents.
- post-publication failures are reported as committed/uncertain rather than as
  generic retryable mutation failures.
- singular `edit(path,old,new,all)` owns one exact replacement; `patch` owns
  multiple/contextual/multi-file changes.
- `cp`, `mv` and `rm` provide the missing non-shell lifecycle operations.
- no mutation tool expands globs or performs destination inference.
- `mv` never silently becomes copy + delete.
- recursive `cp`/`rm` remain bounded and fail closed around protected/denied
  content.
- all new mutation tools participate in replay protection and shared mutation
  coordination.
- tool visibility remains derived from the effective authority.
- compatibility aliases are absent from advertised schemas.
- serialized tool-schema size and representative output-token cost are measured
  and recorded.
- existing correctness/security invariants remain covered on Linux, macOS,
  Windows and Nix CI.

## Decisions

1. The target consumer is the AI model; human CLI familiarity is secondary.
2. `ls` and `find` are separate because listing and discovery are distinct
   model intents.
3. `find` and `grep` remain separate because pathname search and content
   search are distinct model intents.
4. `grep` retains `glob`; composing `find` with N content scans is too
   expensive for a common operation.
5. `cp`, `mv`, `rm` are required for practical non-shell filesystem
   completeness.
6. `sed` is unnecessary; `edit`, `patch`, `write` cover the filesystem
   editing use case more clearly.
7. Public timestamps are `mtime`, `ctime`, `btime`, consistently.
8. Public hidden handling is `hidden`, consistently with positive
   `gitignore`.
9. Inputs stay structured; high-volume discovery results become compact text.
10. Exact mutation paths are preferred over shell-style inference.
11. Destructive overwrite flags are omitted initially; explicit composition is
    safer and easier for a model to reason about.
12. Tool count is not minimized at the expense of semantic clarity. Small
    orthogonal schemas are preferable to overloaded tools.
13. Token cost is a first-class metric for both tool definitions and results.
14. Runtime byte ceilings bound work; they are not ordinary model pagination
    controls.
15. Read-only operations may return marked usable prefixes/pages. Mutations do
    not expose partial target bytes as a paging mechanism.
16. Every mutation has an explicit publication commit point; post-commit
    failures must never masquerade as safe-to-retry pre-commit failures.
17. `edit` is intentionally singular; batching/context belongs to `patch`.

## References

- [Tool surface v2](../dotlink-tool-surface-2026-10-25/README.md)
- [Tool correctness review](../dotlink-tool-review-2026-10-26/README.md)
- [Architecture](../../../docs/ARCHITECTURE.md)
- [Security](../../../docs/SECURITY.md)
- [Plan queue](../TODO.md)
