# DL-150 — AI-native tool surface + canonical Job graph runtime

Author: Prasanna Loganathar

Co-authors: ChatGPT (GPT-5.6 Sol)

Owner: @pvl

Status: **design sealed** for v1 plus the specified v2 canonical Job/Graph runtime and shell nodes; implementation intentionally not started.

Implementation should follow this specification as written. A model-facing surface or
semantic change discovered during implementation must explicitly reopen/update DL-150
rather than silently drifting from the sealed design.

Base: `84cd9b7` (`release: dotlink v0.14.3`)

## Seal boundary

No model-facing design questions remain open for the v1 implementation or the
specified v2 canonical Job/Graph runtime and shell nodes in this plan.

Implementation may refactor internals, improve performance, tighten validation,
or choose platform-specific mechanisms while preserving the sealed public
semantics. Any change to tool names, advertised arguments, status/paging
grammar, edit matching semantics, Job/Graph semantics, lifecycle behavior or
coordination policy requires an explicit DL-150 design update before code lands.

Deliberately deferred rather than forgotten:

- line/range-level collaborative locking or automatic merge/rebase;
- runtime revision tokens or fail-fast coordination metadata beyond the v1 shell
  `writes` scheduling hint;
- `mkdir`, `touch`, `chmod`, `chown`, symlink/hardlink creation;
- cross-device `mv` implemented as copy+delete;
- output-value/dataflow expressions, loops or nested workflows in `batch`;
- additional overwrite/replace convenience flags.

These should be reconsidered only from measured model/runtime workloads, not
added speculatively during implementation.

## Why

dotlink's native tools are consumed primarily by AI models, not by humans typing
commands. The surface should therefore optimize for model tool selection,
schema-token cost, result-token cost, deterministic composition, bounded work
and robust execution/filesystem semantics rather than for compatibility with
human shell conventions.

The current surface has one important overlap: `ls` is both a directory lister
and a recursive discovery tool. It carries glob, depth, kind and sort behavior
that belongs to a finder. At the same time, a non-shell runtime still lacks the
basic filesystem lifecycle operations needed for ordinary repository work:
copy, rename/move and remove.

The target is a complete coding/repository environment that remains useful when
shell authority is deliberately withheld, while also giving shell-enabled
runtimes one clean durable execution model. Native filesystem tools stay direct
and strongly typed; `bash`/`pwsh`/`powershell`, `batch`, and `job_start` compose
through the canonical Job/Graph/Node runtime rather than separate durability
systems.

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
11. **Native coordination stays below the model surface.** External runtimes may
    schedule native tools conservatively from existing path arguments while
    dotlink's mutation layer remains the correctness authority. Shell is the one
    deliberate exception: optional advisory `writes` scopes expose an otherwise
    opaque expected mutation footprint.

## Target surface

~~~text
discovery     ls      find    grep
text          read    write   edit    patch
lifecycle     cp      mv      rm
binary        bread   bwrite  bedit
orchestration batch
durable       job_start job_list job_get job_output job_ack job_cancel job_restart
shell         bash    pwsh    powershell   # distinct interpreter capabilities
~~~

The model-facing decision boundary should be nearly mechanical:

| Intent | Tool |
| --- | --- |
| inspect this directory | `ls` |
| locate filesystem entries | `find` |
| locate text in files | `grep` |
| inspect a known text file | `read` |
| create/replace complete text | `write` |
| make exact independent substitutions in one file | `edit` |
| make contextual/multi-file text changes | `patch` |
| duplicate an object | `cp` |
| rename an object | `mv` |
| remove objects | `rm` |
| inspect/create/mutate arbitrary bytes | `bread` / `bwrite` / `bedit` |
| run an arbitrary command | `bash` / `pwsh` / `powershell` |
| run a predetermined graph and optimistically wait | `batch` |
| start the same graph durably and return immediately | `job_start` |
| inspect/control durable work | `job_list` / `job_get` / `job_output` / `job_ack` / `job_cancel` / `job_restart` |

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

### Path composability

`find` returns paths that can be copied directly into another tool's `path`
argument. If `find(path="src", ...)` returns a file, render `src/main.rs`, not
just `main.rs`.

`ls` is the deliberate exception: it returns basenames because the caller
already supplied the directory and repeating that prefix wastes tokens.

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

For `read`, use the canonical `more` cursor:

~~~text
more line=57 byte=0
more line=42 byte=1048217
~~~

The first form continues at the next logical line. The second continues inside
an unusually long logical line.

The cursor always names the first byte not returned and must be on a valid UTF-8
boundary. A transport result ceiling tighter than `max_read_bytes` uses the
same continuation mechanism; the model does not need to know which lower layer
caused the page boundary.

Discovery tools use the same canonical `more` and `incomplete` modifiers.

A hard root/open/authorization failure before useful data exists is an ordinary
tool error.

### Mid-read I/O failure

If a single-file `read` encounters an I/O failure after already obtaining a
valid prefix, preserve the useful prefix but mark the MCP tool result as an
error and terminate it with an explicit fault marker, for example:

~~~text
mixed ok=prefix fail=io more line=81 byte=0
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
fail write limit 1048576; content 1400000
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
ok; durability unknown
ok; cleanup pending
~~~

A platform backend that genuinely cannot determine whether publication crossed
the commit point must surface an explicit `unknown` outcome and must not
encourage automatic retry.

The OpenAI transport replay layer separately protects the case where the
mutation committed but its response could not be delivered: redelivery replays
the committed result rather than executing the side effect again, and an
unrecoverable ambiguity remains fail-closed.

### Multi-object mutation outcomes

A multi-object operation cannot generally promise one filesystem transaction.

Strict multi-file `patch` preflights and stages everything before the first
publication, but a later commit failure can still occur after earlier files
were committed. Likewise a multi-path `rm` may encounter a platform failure
after one namespace operation has committed even after full preflight.

These exceptional outcomes must be explicit and must list only the information
the model needs to recover, for example:

~~~text
mixed ok=2 fail=1
3 fail permission denied
~~~

Such a result is an MCP tool error, not an ordinary success. The word
`mixed` is reserved for a real visible mutation split; never use it for
private staging progress.

Where a multi-object mutation can cheaply roll back unpublished/tombstoned
objects while locks are still held, it should do so, but rollback failure must itself be
reported as a mixed/unknown state rather than hidden.

## Canonical status, paging and progress grammar

MCP's own success/error bit remains authoritative. The model-visible grammar has
two dimensions:

1. one **primary outcome**;
2. zero or more orthogonal **query modifiers**.

Primary outcomes:

~~~text
ok
unchanged
fail
mixed
unknown
skip
~~~

Query modifiers:

~~~text
more
incomplete
~~~

This distinction matters because a successful query can simultaneously have an
exact continuation and incomplete coverage.

### Primary outcomes

`ok` means the requested operation completed successfully:

~~~text
ok
ok 7
~~~

A count is included only when it materially helps the next model step.

`unchanged` is a successful no-op:

~~~text
unchanged
~~~

For a mutation, `fail` means the requested visible mutation is known not to
have committed:

~~~text
fail destination exists
fail write limit 1048576; content 1400000
~~~

A mutation must never use `fail` after its publication commit point.

`unknown` means dotlink genuinely cannot prove the externally visible
side-effect/execution state:

~~~text
unknown commit; do not retry
~~~

It is an error result reserved for fail-closed ambiguity: the operation may
already have produced externally visible effects, so automatic retry is unsafe.

The canonical name is deliberately **`unknown`**, not `maybe`. `unknown` is
a precise machine-contract state; `maybe` is conversational and does not say
what is unknown. There is no `maybe` alias.

`skip` means an operation was deliberately not started:

~~~text
skip condition
~~~

It is primarily useful in conditional Graphs. A skipped mutation has no side effect.

### Mixed outcomes: `mixed`

`mixed` means one request contains more than one kind of **material** terminal
component outcome. It may include known successes, known failures and unknown
components together. `skip` is structural/neutral and does not by itself make
an otherwise uniform aggregate mixed.

Aggregate primary selection is deterministic:

- all material components successful -> `ok`, or `unchanged` when every
  successful component is unchanged;
- all material components known failures -> `fail`;
- all material components unknown -> `unknown`;
- more than one material outcome family, or any child already `mixed` ->
  `mixed`;
- no material component started -> `skip`.

Thus `ok + unknown`, `fail + unknown`, and `ok + fail + unknown` are all
`mixed`, preserving the known parts without pretending the unknown component
is safe to retry.

It is an MCP error result even when useful data or successful mutations are
present.

For indexed multi-item operations, summarize counts first and list only
exceptions:

~~~text
mixed ok=5 fail=2 unknown=1
2 fail not found
6 fail permission denied
8 unknown commit
~~~

Input coordinates are stable and 1-based. Unlisted coordinates are complete
successes unless a modifier summary says otherwise.

A read that obtained a valid prefix before an I/O failure may combine a primary
mixed outcome with an exact continuation modifier:

~~~text
<valid prefix>

mixed ok=prefix fail=io more line=81 byte=0
~~~

The prefix is useful, the error result prevents it from being mistaken for a
complete read, and `more` identifies a safe continuation only when one is
known.

Reserve `mixed` for materially mixed terminal outcomes. Do not use it for
normal paging, uniformly unknown coverage, or private staging progress.

### Paging: `more`

`more` means the current data is valid and an exact continuation exists.

The continuation uses the actual follow-up argument names so the model can copy
it directly.

Unknown total:

~~~text
more offset=200
more line=42 byte=1048217
~~~

Known total, only when already available cheaply:

~~~text
more 2/7 offset=400
~~~

A final known page may use:

~~~text
ok 7/7
~~~

An unknown-total final page simply ends normally or with `ok` when an explicit
status is useful.

Never perform an extra scan merely to discover a page count.

### Coverage modifier: `incomplete`

`incomplete` means returned data is valid but dotlink cannot guarantee that it
observed the complete search/input space:

~~~text
incomplete budget
incomplete walk,gitignore
~~~

There may be no cursor capable of recovering the missing region.

Paging and incomplete coverage can coexist:

~~~text
more offset=200 incomplete=walk
~~~

This means more known results are pageable while the underlying scan itself was
not exhaustive.

### Data and status blocks

When control/status text would corrupt exact returned data, especially
`read(numbered=false)`, return separate MCP content blocks:

~~~text
content[0] = exact data fragment
content[1] = status/control line
~~~

A complete exact read needs only the data block.

### Progress notifications

Progress is ephemeral execution state, not result data. The final tool result
must be fully understandable even if the client observed zero progress
notifications.

Progress values are absolute cumulative snapshots, never deltas. Use one stable
unit for a request whenever possible.

Compact model/UI convention:

~~~text
37/100 copy
918 scan
commit
~~~

The presence of `/total` means the end is known. A bare number means progress
is measurable but the end is unknown. A phase word alone is valid when there is
no useful numeric unit.

At the MCP protocol boundary, dotlink maps these to the protocol's structured
progress fields; model-visible text does not repeat field names.

#### Known fixed end

When the denominator is naturally known before meaningful progress begins:

~~~text
0/100 copy
37/100 copy
100/100 copy
~~~

The denominator remains fixed. The final tool result, not `100/100`, is
authoritative completion.

#### Unknown end

When the final amount of work is genuinely unknown:

~~~text
412 scan
918 scan
~~~

Do not invent a denominator. Keep the request in unknown-total mode for its
lifetime rather than switching from a guessed or late-discovered total.

#### Multi-phase work

Do not mix unrelated units merely to manufacture a percentage. Keep the numeric
unit stable and use a terse phase suffix:

~~~text
37/100 scan
37/100 commit
37/100 cleanup

918 scan
918 commit

commit
cleanup
~~~

Progress may stay numerically unchanged while phase text changes, subject to
protocol monotonicity.

#### Streaming partial data vs progress

Progress notifications never carry the only copy of tool result data.

- execution progress streams through progress notifications;
- result pagination/streaming uses ordinary tool results plus `more`;
- final tool results remain authoritative.

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
  context=0,

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

`context` is symmetric before/after context and remains bounded. For asymmetric
inspection, follow the match with `read`.

Existing bounded regex, UTF-8 classification, per-file scan caps, paging and
output ceilings remain.

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

more line=42 byte=1048217
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
more line=57 byte=0
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

Missing parent directories are created automatically **only as part of the same publication unit**. A failed `write` must not leave newly visible parent directories behind.

The shared mutation layer should stage the complete missing suffix beneath the nearest existing authorized ancestor and publish the first missing component atomically where the platform provides the required no-replace namespace primitive. Any platform fallback must preserve the same externally visible all-or-nothing contract rather than exposing parent directories incrementally.

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

Perform one or more exact, independent text substitutions in one known file.

This is a hot coding-model operation and belongs inside `edit`, not generic
`batch`: one file should be read, locked, planned, verified and atomically
published once even when several exact replacements are requested.

### Proposed schema

~~~text
edit(
  path,
  edits=[
    [old, new],
    [old, new, all]
  ]
)
~~~

Each tuple is:

~~~text
[old, new, all?]
~~~

The optional third value defaults to `false`.

This tuple form avoids repeating `path`, `old`, `new` and `all` object keys
for the common multi-replacement case.

There is one advertised shape even for a single replacement:

~~~text
edit(path="src/x.rs", edits=[["old", "new"]])
~~~

Do not add a second top-level `old/new/all` shorthand; the small one-tuple cost
is preferable to a polymorphic schema and another model decision.

### Snapshot semantics

Every requested edit is matched against the **same original file snapshot**.

For each tuple:

- `old` must be non-empty;
- with omitted/false `all`, `old` must occur exactly once;
- zero matches are an error;
- multiple matches are an error with a concise count/remedy;
- `all=true` selects left-to-right non-overlapping exact occurrences in the
  original snapshot, matching normal replace-all semantics;
- zero selected occurrences are an error even with `all=true`;
- `old == new` still validates matching/ambiguity but contributes no changed
  range.

After every tuple is resolved, all selected source ranges are sorted by original
byte offset. Any overlap, including two edits selecting the same source bytes,
fails the whole call before staging:

~~~text
fail edits 2,3 overlap
~~~

Independent edits are therefore order-independent. A later edit can never
accidentally match text created by an earlier edit.

The final file is built once from the original snapshot plus the non-overlapping
replacement ranges, then the original snapshot is reverified and the result is
staged/published atomically.

The complete resulting file and aggregate plan must fit the configured
mutation/write limits before publication.

### Multiline replacements

`old` and `new` are ordinary UTF-8 strings and may span multiple lines:

~~~text
edit(path="src/x.rs", edits=[
  ["old line 1\nold line 2",
   "new line 1\nnew line 2"],
  ["OldType", "NewType", true]
])
~~~

This covers both one multiline replacement and several independent locations in
the same file.

### Results

~~~text
ok
unchanged
~~~

Typical ambiguity error:

~~~text
fail edit 2 old matches 4 times; set all=true or use patch
~~~

The model-facing decision boundary becomes:

~~~text
complete desired file                         write
exact independent substitutions in one file  edit
contextual/dependent/overlapping/multi-file   patch
heterogeneous tool orchestration              batch
~~~

If transformation B intentionally depends on text produced by transformation A,
or requested changes overlap/context-match rather than independently matching
the original snapshot, use `patch` rather than relying on edit ordering.

Compatibility may accept the previous multi-file edit objects and the temporary
singular `path/old/new/all` shape only at the wire/deserialization boundary.
Neither appears in the advertised schema or new shared internal representation.

## `patch`

### Purpose

Contextual and multi-file text changes.

Keep the Codex patch grammar. Models already know it and it is much more
token-efficient than a nested JSON hunk schema.

~~~text
patch(patch)
~~~

Continue supporting update and add-file sections.

Do not add patch-level delete or move directives. `rm` and `mv` provide
dedicated, explicit lifecycle semantics instead.

The advertised API is strict-only: preflight and stage every section before the
first publication. Each file replacement is atomic while the multi-file commit
sequence is not a filesystem transaction.

Legacy `best_effort` may be accepted temporarily at the compatibility boundary,
but it is not advertised to new models.

## `cp`

### Purpose

Duplicate one exact filesystem object.

### Proposed schema

~~~text
cp(from, to)
~~~

`to` is always the exact resulting path.

There is no shell-style rule where a directory destination implicitly appends
the source basename.

### Semantics

- destination must not exist;
- missing destination parents are created only inside the same staged publication unit; a failed copy must not leave newly visible parent directories behind;
- files copy as files and directories copy as complete directory trees;
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
- when destination parents are missing, stage the complete missing suffix under the nearest existing authorized ancestor and publish its top component once;
- directory copy constructs a staged destination tree and publishes the completed root/suffix by rename;
- a failed copy must not expose either a partially built destination or newly created parent directories.

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
- the destination parent must already exist; `mv` never creates parent directories because that would require a second namespace publication and violate rename-or-no-change semantics;
- files, directories and symlinks are supported;
- symlinks themselves move; their targets are never followed;
- moving a directory into itself is rejected;
- authorization/protected-path checks cover both source and destination;
- directory moves must not permit a protected/denied subtree to escape policy.

Most importantly, `mv` is a rename operation only.

It never silently degrades to copy + delete. Cross-device/volume moves fail
with a concise actionable error:

~~~text
fail cross-device move; use cp then rm
~~~

The invariant is: rename the existing object or change nothing. A failed `mv` leaves the source and surrounding destination namespace unchanged.

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

For multiple roots, preflight the complete request before making any namespace change. Reject duplicate roots, aliases resolving to the same removable object, and ancestor/descendant-overlapping requested namespaces before the first mutation. The caller should pass the single highest root when recursive removal would otherwise subsume another requested path.

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

## Shared `ops` Graph grammar

`batch(ops)` and `job_start(ops)` consume one canonical compact Graph grammar.
`batch` reduces model/tool round trips while optimistically waiting; `job_start`
uses the same Graph but exposes the Job immediately. The semantic tools remain
the actual node operations.

Do not add separate `seq` and `parallel` tools.

### Structured arguments, tiny graph language

Do **not** make Graph input a natural-text command DSL.

Natural text looks shorter for trivial calls but becomes more expensive and
error-prone as soon as arguments contain spaces, multiline `write` content,
Codex patch text, arrays, booleans or binary payloads. That would recreate a
second quoting/escaping language and discard the existing tool schemas.

Instead:

- tool arguments remain their normal structured JSON objects;
- only the dependency/branch expression is compact text.

Both constructors advertise the same one graph argument:

~~~text
batch(ops)
job_start(ops)
~~~

Each operation is the shortest unambiguous tuple:

~~~text
[tool, args, when?]
~~~

Examples:

~~~text
ops=[
  ["read", {"path":"a"}],
  ["read", {"path":"b"}, "*"],
  ["edit", {"path":"x","edits":[["a","b"]]}, "1&2"],
  ["mv", {"from":"x","to":"y"}],
  ["read", {"path":"fallback"}, "3.fail|3.mixed"]
]
~~~

Operation coordinates are their 1-based positions: `1`, `2`, `3`, ...

No explicit IDs are declared.

### Default sequencing and roots

For operation 1, omitted `when` means `*`: start when the Job is admitted.

For operation N > 1, omitted `when` means the previous operation succeeded:

~~~text
N-1
~~~

Therefore the most common sequential Graph needs no explicit dependency syntax:

~~~text
ops=[
  ["write", {...}],
  ["edit", {...}],
  ["mv", {...}]
]
~~~

Use `"*"` to start an independent root immediately. Multiple roots run in
parallel under the normal dotlink concurrency/admission rules.

### Dependency expressions

The optional `when` string is deliberately tiny:

~~~text
*          unconditional root
N          operation N completed successfully
N.*        operation N reached any terminal outcome
N.status   exact primary outcome / query modifier
&          AND
|          OR
(...)      grouping
~~~

An operation may reference **only earlier operation numbers**. Cycles and
forward references are therefore impossible by construction.

Bare `N` means the referenced operation completed successfully **and
completely**:

~~~text
ok | unchanged
~~~

with neither `more` nor `incomplete`.

The explicit predicates remain two-dimensional: `N.ok` and `N.unchanged`
match the primary outcome, while `N.more` and `N.incomplete` test modifiers
that may coexist with a successful primary outcome.

A paged or incomplete query therefore does not satisfy a bare dependency. This
makes default sequencing safe for dependent mutations.

Exact canonical outcomes/modifiers may be selected explicitly:

~~~text
N.ok
N.unchanged
N.more
N.incomplete
N.fail
N.mixed
N.unknown
N.skip
N.*
~~~

There are no additional status classes. `N.*` is the one shorthand for any terminal node outcome.

Examples:

~~~text
1&2
1|2
1&(2|3)
3.fail|3.mixed
4.more|4.incomplete
~~~

All referenced operations must reach terminal state before the expression is
evaluated. `|` is a boolean branch, not a timing race. This keeps execution
deterministic and replayable.

`N.unknown` matches a uniformly unknown material outcome. `N.mixed` may itself include unknown components, so a `mixed` branch must be treated as potentially ambiguous unless the caller only needs generic non-success handling. The dependency DSL intentionally does not introspect mixed-result counts; if a workflow requires different execution for known-failure versus unknown-containing subsets, split those operations into separate nodes instead of adding another status-class language. `N.*` deliberately matches every terminal outcome.

### Sequence, parallelism, joins and decisions

Parallelism emerges from dependencies rather than a separate syntax.

Two roots:

~~~text
1 *
2 *
~~~

run concurrently.

Fan-out:

~~~text
2 when=1
3 when=1
~~~

runs 2 and 3 concurrently after 1 succeeds.

Join:

~~~text
4 when=2&3
~~~

waits for both.

Decision branch:

~~~text
4 when=3
5 when=3.fail|3.mixed
~~~

selects the success path or a generic non-success/mixed path after operation 3. Because `mixed` may contain unknown components, a mutation on that branch must not assume the prior side effects are fully known.

A realistic graph:

~~~text
ops=[
  ["read", {"path":"config.toml"}],
  ["read", {"path":"Cargo.toml"}, "*"],
  ["edit", {"path":"config.toml","edits":[["a","b"]]}, "1&2"],
  ["mv", {"from":"old","to":"new"}],
  ["read", {"path":"config.example.toml"}, "3.fail|3.mixed"],
  ["rm", {"paths":["tmp"],"recursive":true}, "4"]
]
~~~

Conceptually:

~~~text
1 ─┐
   ├─> 3 ──success──> 4 ──> 6
2 ─┘      \
          fail/mixed
            \
             > 5
~~~

### Deliberate limits: graph, not workflow language

The graph may branch only on canonical execution status.

Do not add:

- output-value predicates;
- variables;
- output substitution into later args;
- loops;
- forward references;
- dynamic node creation;
- nested `batch`;
- rollback scripting;
- timing/race predicates.

For example, do not support expressions such as:

~~~text
3.output contains "foo"
$3.path -> 4.args.path
~~~

If a later action depends on understanding returned data, the model should
receive that result and make the next decision in a new turn.

This boundary preserves the value of an AI model rather than building a second
programming language inside dotlink.

### False conditions and `skip`

If an operation's `when` expression evaluates false, that operation is not
started and reaches:

~~~text
skip condition
~~~

A skipped operation has no side effect.

Condition-selected skips are normal Graph behavior and do **not** make an
otherwise successful Job an error.

### Started parallel operations

Once an operation starts, it is allowed to reach a safe terminal state.
A sibling failure does not blindly cancel an already-running mutation merely to
make the graph look transactional.

Normal dotlink mutation coordination still serializes conflicting filesystem
operations where required.

### Graph results

Results are keyed by the same 1-based coordinates and do not repeat tool/path
names unnecessarily.

Trivial complete `ok` results may be omitted. Data-bearing results and
status-bearing nodes appear exactly once; do not duplicate an exceptional node
both in an individual block and again below the summary.

Example:

~~~text
1
<read data>

2
<read data>
more offset=200

4 fail destination exists
5 skip condition

mixed ok=2 more=1 fail=1 skip=1
~~~

Aggregate counts distinguish complete successes from query modifiers:

- `ok=N`: complete successful nodes;
- `more=N`: nodes with exact continuation;
- `incomplete=N`: nodes with incomplete coverage; may overlap `more`;
- `fail=N`, `mixed=N`, `unknown=N`, `skip=N`: primary outcomes.

If every started node is non-error but one or more query nodes have modifiers,
the aggregate remains successful but exposes them:

~~~text
ok more=2 incomplete=1 skip=1
2 more offset=200
5 more line=42 byte=1048217
7 incomplete walk
8 skip condition
~~~

If materially different terminal outcomes coexist, the aggregate primary
outcome is `mixed`:

~~~text
mixed ok=3 more=1 fail=1 skip=1
2 more offset=200
4 fail permission denied
5 skip condition
~~~

A condition-selected `skip` is expected Graph behavior and by itself does not
make the Job an error.

### Paging inside a Graph

A node retains its native paging semantics.

A Graph Job must never silently truncate a node result merely because the
aggregate response is large. If the aggregate result budget constrains a pageable child,
that child returns its normal `more` cursor.

Per-operation result budgets must be assigned **deterministically**, independent
of parallel completion order, under the aggregate transport/result ceiling.
The exact allocator is an implementation detail, but it must not make replayed
results depend on scheduling.

### Graph progress

The number of Graph nodes is fixed, so top-level Job progress has a known
denominator. Skipped nodes count as terminal operations.

Compact form:

~~~text
3/12 2 ok
7/12 5 skip
~~~

The fraction is terminal operations / total operations. The suffix identifies
the operation that most recently reached a terminal state and, when useful, its
status.

A long-running node may contribute a nested compact snapshot without advancing
the Job's terminal-node count.

Known child end:

~~~text
3/12 5 412/1000 copy
~~~

Unknown child end:

~~~text
3/12 5 918 scan
~~~

The Job fraction stays fixed-total while the node snapshot independently uses
the known/unknown progress convention.

These updates are status hints only. Node terminal results and the final Job
ledger remain authoritative.

### Graph-node eligibility

V1 admits the deterministic native filesystem surface as Graph nodes:

~~~text
ls find grep
read write edit patch
cp mv rm
bwrite bedit
~~~

Keep `bread` direct-only in v1. `bread(format="mcp")` may return typed MCP
image/audio/blob content; the v1 durable Graph result channel is intentionally
text/result-stream based and must not flatten typed content into a lossy
representation. Do not add argument-dependent Graph eligibility merely to admit
base64/hex `bread`. A later content-aware retained-result design may make
`bread` Graph-eligible as one coherent feature.

V1 does not admit shell, job-control or task-control tools as Graph nodes.

V2 adds `bash`, `pwsh` and `powershell` as ShellNodes under the canonical
Job/Graph/Node design specified below. Graph node dispatch still excludes
`batch`, `job_start`, all `job_*` controls and task-control methods.

A central tool-spec property declares **Graph-node eligibility** and version
availability so `batch`, `job_start`, visibility, validation and tests cannot
drift. This supersedes a narrowly named `batchable` concept.

### Validation and schema reuse

Do not embed a discriminated union containing every tool schema inside
`batch` or `job_start`; that would duplicate permanent model context.

`args` is validated at runtime against the already registered named tool's
schema and normal permission/capability checks.

The shared Graph parser validates before execution:

- total operation count and aggregate request bytes;
- eligible Graph-node tool names;
- each nested argument object;
- backward-only references;
- condition grammar;
- maximum per-Job Graph fan-out/concurrency;
- aggregate work and result ceilings.

Invalid graphs fail before any operation starts.

### Execution admission

Job count is not a sufficient execution limit once one Graph may contain many
concurrent nodes. The shared execution layer therefore owns one process-wide
**Graph node-runner permit budget** across all Jobs.

- every Graph node acquires a runner permit before `pending -> claimed`;
- claimed therefore continues to mean one execution attempt has actually been
  admitted, rather than "waiting for capacity";
- each Job also has a bounded parallel width no greater than the global runner
  budget;
- direct `bash` / `pwsh` / `powershell` are one-ShellNode Jobs and use the
  same runner admission;
- active-Job count, running-node permits, retained-Job count and spool bytes are
  separate limits and must not be conflated.

This preserves the resource bound that one-shell-per-Job provided implicitly
today while allowing Graph Jobs without process/fan-out amplification across
many concurrent Jobs.

### Replay and mutation safety

Every Graph Job constructor (`batch` and `job_start`) is request-replay
protected, regardless of the apparent node mix. This gives one stable Job
creation/execution identity, prevents duplicate expensive read-only Graphs, and
avoids trusting shell hints/classification to decide whether replay matters.

Every node receives a deterministic internal replay identity derived from the
outer Job/request identity plus its node coordinate. The semantic hash of the
outer request binds the complete Graph definition: operation list, arguments
and conditions.

On redelivery, already terminal sub-operations replay their recorded terminal
state/result and are never executed again. Pending graph nodes resume only when
their dependency predicates are satisfied by that same ledger.

If dotlink cannot prove whether one mutation crossed its commit point, that node
is `unknown`; dependent predicates evaluate from that explicit status.

Do not ship Graph execution (`batch` or `job_start`) until this outer-Job and
per-node replay model is correct under cancellation, response loss and runtime
recovery. Mutation nodes make this a correctness requirement; read-only nodes
benefit from the same uniform identity model.

### Design boundary

Prefer semantic batching inside domain tools when one domain operation can do
the work more efficiently and atomically:

- one `grep` over many files instead of N greps;
- one multi-hunk `patch` instead of N edits;
- one `rm(paths)` instead of N removes.

Use `batch` for heterogeneous predetermined Graphs where reducing model/tool
round trips materially helps; use `job_start` when the same Graph should be
exposed durably immediately.

## V2 — one durable Job graph runtime

The canonical execution model is:

~~~text
Job       one execution instance and lifecycle
Graph     the immutable plan executed by that Job
Node      one operation in the Graph
~~~

Every Job owns exactly one Graph. A Graph may contain one node or many nodes.
There are no nested child Jobs inside a Graph.

This makes the public constructors simple specializations of one runtime:

~~~text
bash(...)        Job(Graph[ShellNode::Bash])
pwsh(...)        Job(Graph[ShellNode::Pwsh])
powershell(...)  Job(Graph[ShellNode::WindowsPowerShell])
batch(ops)       Job(Graph[Node...]), optimistically wait
job_start(ops)   Job(Graph[Node...]), expose immediately
~~~

`bash`, `pwsh` and `powershell` remain first-class model-facing tools because
they express the interpreter intent directly; they are not separate execution
engines. `batch` and `job_start` consume the same `ops` graph grammar and differ
only in presentation: optimistic wait versus immediate durable exposure.

V1 `batch(ops)` may still restrict allowed nodes to deterministic native tools.
V2 adds `bash`, `pwsh` and `powershell` ShellNodes to that same Graph runtime.

### One Job lifecycle

All Jobs use one ID namespace and the existing `job_*` lifecycle. Native MCP
Tasks are another projection of the same underlying Job and use the same ID when
supported.

Ordinary `bash`, `pwsh`, `powershell` and `batch` calls create a Job, then
optimistically wait for its normal synchronous result. If execution outlives the
response window, or the complete result cannot fit the response budget, the
**same Job** is exposed durably. No node is restarted during handoff.

Only exposed/handed-off Jobs enter the model-visible `job_*` namespace. A Job
that completes synchronously and returns its ordinary result needs no durable ID.

`job_start(ops)` is the explicit return-immediately constructor for the same
Graph grammar used by `batch(ops)`. There is no `batch_start` and no shell-only
background schema: `job_start` handles one-node or multi-node Graphs uniformly.

Conceptually:

~~~text
Job / JobManager
  id
  immutable Graph
  job state + cancellation
  replay/delivery identity
  retention
  Task projection
  output paging/ack
  Job-session node ledger

Graph
  Node 1
  Node 2
  ...

Node
  NativeToolNode
  ShellNode
~~~

A ShellNode owns its contained process runner and stdout/stderr spool, but is
**not** another Job. This avoids nested IDs, replay ledgers, cancellation
domains, retention state or Task projections.

Native tool nodes call the same underlying filesystem implementations as direct
tool calls, with the graph runtime only adding node lifecycle, dependency,
checkpoint and retained-result behavior.

The existing `JobManager` should be generalized around this Graph/Node runtime
rather than duplicated. Internal names may remain if renaming creates churn; the
architectural invariant is one Job lifecycle and one execution engine.

The one-node shell path must remain lightweight. Architectural unification must
not impose heavyweight graph persistence on short synchronous commands beyond
the state already needed for replay/cancellation; durable retention is promoted
only when the Job is exposed or otherwise requires it.

Example:

~~~text
batch(ops=[
  ["edit", {"path":"src/x.rs","edits":[["old","new"]]}],
  ["bash", {"command":"cargo fmt","writes":["src"]}],
  ["bash", {"command":"cargo test"}, "2"],
  ["rm", {"paths":["tmp"]}, "3"],
  ["read", {"path":"failure.log"}, "3.fail|3.mixed"]
])
~~~

Platform/capability visibility still applies: a shell node is valid only when
that shell tool is available to the caller.

Graph node dispatch excludes `batch`, `job_start`, all `job_*` controls and
Task-control methods. A ShellNode is graph-owned execution; explicit child
handoff APIs would violate the single outer-Job lifecycle.

### Shell-node terminal semantics

A shell node is **not terminal merely because execution was handed off** to a
durable job or MCP Task.

For Graph control, ShellNodes map process state onto the canonical node outcome
vocabulary:

- successful process completion (`success=true`, normally exit code 0) -> `ok`;
- known non-zero exit, timeout, launch/preparation failure or cancellation -> `fail` with compact process detail;
- genuinely ambiguous process/side-effect state -> `unknown`;
- `N.*` matches any terminal node outcome.

The ShellNode retains compact exit/success/timeout/error metadata plus separate
bounded stdout/stderr streams and their truncation/paging state.

Graph outcome and MCP envelope semantics remain deliberately distinct. A ShellNode uses `fail` for Graph branching when the process is known unsuccessful, but a direct synchronous shell call preserves the existing process-result wire contract: if the process launched and exited normally with a non-zero code, the tool call itself is MCP success carrying `success=false` and the exit/output metadata. Pre-execution failure, timeout/cancellation and internal execution failure remain MCP errors as appropriate.

This keeps process failure separate from protocol/tool failure without weakening Graph control semantics.

A ShellNode `fail` is a process outcome, not the native mutation `fail`
commit guarantee. A shell command may mutate files and then exit non-zero, time
out or be cancelled. V2 must never imply rollback merely because the node
followed a known-error branch.

Aggregate summaries may count shell process errors explicitly:

~~~text
mixed ok=3 fail=1 skip=1
4 fail exit=1
5 skip condition
~~~

If no useful node succeeded, the outer Graph reports `fail` with the relevant
node failure details. That means the Graph failed to complete successfully, not
that arbitrary shell side effects are known absent.

### The Job owns durability

The **Job** is the durable unit; the Graph is its immutable executable plan.

Durability is automatic. There is no per-node `durable=true` flag and the model
does not need to predict whether a one-node shell Job or multi-node Graph Job
will fit inside the current transport window.

A ShellNode may run for seconds, minutes or hours. Crossing the synchronous
response window hands off the **same Job**; it never creates a child Job,
restarts the Graph, or re-executes already terminal nodes.

The Job's Graph scheduler continues after handoff even when nobody is actively
polling. When a long ShellNode reaches terminal state, newly satisfied dependent
nodes may start under the same Job.

~~~text
Job
  |
  +-- immutable Graph
       |
       +-- NativeToolNode
       +-- ShellNode -- bounded stdout/stderr spool
       +-- ...
  |
  +-- Job-session node ledger
~~~

For direct `bash`/`pwsh`/`powershell`, the Graph contains one ShellNode. For
`batch` and `job_start`, it contains the submitted `ops` Graph.

If the Job finishes inside the caller/transport window and its result fits,
return the ordinary tool result. Otherwise expose that same Job durably. Native
MCP Task may be used only when the eventual ordinary tool result is guaranteed
to fit the Task result budget; otherwise use the same Job through the `job_*`
handle.

A compact compatibility receipt for a multi-node graph may be:

~~~text
running <id> 3/12
~~~

The Job ID is opaque. Native MCP Tasks use that same underlying Job identity
rather than inventing a second execution.

The Job retains:

- immutable Graph definition and semantic request identity;
- per-node state and deterministic replay identity;
- ShellNode process identity/lifecycle;
- bounded shell stdout/stderr spool state;
- native node results and paging cursors;
- dependency/skip decisions;
- aggregate result/progress state.

### Job-session node checkpoints

Each node follows a fail-closed Job-session state machine that survives request/transport handoff but is not resurrected across dotlink process replacement:

~~~text
pending
  -> claimed
  -> running
  -> terminal
~~~

`claimed` is recorded in the Job-session ledger before execution may start. It means this graph/node owns exactly one admitted execution attempt.

A shell child uses a deterministic internal runner identity derived from graph
run identity + node coordinate. Duplicate dispatch inside the live durable
session resolves to that same runner rather than creating another process.

A terminal node is checkpointed **before** its result can unlock dependents.
Once terminal, that node is never re-executed because a later node failed, the
final response was lost, or the graph was polled/reconnected.

### Resume versus restart

These semantics belong to the shared durable job model, not only to batches:

~~~text
resume   continue this durable execution
restart  create a new durable execution from the retained definition
~~~

**Resume is automatic.** Transport timeout, request loss, reconnect,
`job_get`, Task polling and redelivery all attach to the same execution. There
is no `job_resume` tool.

Terminal node checkpoints remain terminal and pending nodes may continue when
their dependencies are conclusively satisfied. A claimed/running node must
reattach to the exact existing execution or become `unknown`; it is never
silently relaunched.

**Restart is explicit through `job_restart(id)`.** It creates a fresh Job with a
new ID from the source Job's retained immutable Graph, with every node initially
pending, while preserving the source Job as immutable history.

Restart may repeat side effects. A running/cancelling job cannot be restarted:

~~~text
fail running; cancel first
~~~

After the source job is terminal, restart is permitted even after `ok`,
`fail`, `mixed`, `cancelled` or `unknown`; the explicit restart action
acknowledges that side effects may happen again.

`job_restart` is replay-protected so duplicate delivery of one restart request
returns the same new job rather than creating multiple executions.

A successful restart returns the new durable ID:

~~~text
running <new-id>
~~~

For a multi-node Graph it may include graph progress:

~~~text
running <new-id> graph 0/12
~~~

Selective "restart from node N" is deliberately omitted in v2. It requires
dependency invalidation and snapshot semantics that are not justified yet.

### Shared `job_*` control surface

Do not add `batch_get`, `batch_cancel`, `batch_output` or `batch_ack`.

Use one durable execution control surface for every exposed Job:

~~~text
job_start(ops)
job_list(limit=64)
job_get(id)
job_output(id, node=null, stream=null, offset=0, ...)
job_ack(id, node=null, stream=null, offset)
job_cancel(id)
job_restart(id)
~~~

`job_start(ops)` uses the exact same Graph grammar, validation and Graph-node
registry as `batch(ops)`:

- `batch(ops)`: create the Job and optimistically wait;
- `job_start(ops)`: create the Job and expose its durable ID immediately.

Neither tool may appear as a Graph node, so there are no nested Jobs/Graphs.

`job_start` has a stronger creation-order invariant because it promises an
immediately durable handle:

~~~text
preflight complete Graph
→ reserve/commit durable Job identity + replay state
→ expose Job
→ allow root nodes to start
~~~

If the `job_start` response is lost after creation, `job_list` can recover that
same Job; the graph is never hidden behind a lost receipt and never recreated
merely to obtain another ID.

`batch` uses the same preflight/Graph compiler but may remain a lightweight
optimistically synchronous Job until it needs durable exposure. If handoff is
needed, promote/expose that **same Job** and preserve all node checkpoints.

The existing abandoned-response safety rule generalizes to every optimistic Job:

- when the transport has a durable replay/request identity, retain the hidden
  Job/replay owner until that transport redelivers or positively retires it;
- when no durable replay identity exists and the last undecided response attempt
  disappears, atomically expose the still-running/committed Job so `job_list`
  can recover it.

No completed or side-effecting Graph execution may become permanently hidden
merely because a local response future/client disappeared.

The advertised control schemas use concise `id`; compatibility may continue to
accept legacy `job_id` at deserialization.

The current public `job_start(command, dir, stdin, timeout_secs)` shape is a
compatibility migration concern. Implementation phases are **not release
boundaries**: do not ship an intermediate release that replaces shell
`job_start` with native-only `job_start(ops)` before ShellNodes are available.
At the final schema switch, advertise only `job_start(ops)` and, for the
migration window, accept the old shell-shaped request only at the
deserialization boundary by translating it into the matching one-ShellNode
Graph. Remove that adapter after the compatibility window.

Semantics:

- `job_start`: side-effecting/replay-protected durable Job creation;
- `job_list`: read-only discovery/recovery of recent exposed Jobs;
- `job_get`: pure compact status snapshot and reattachment to the same Job;
- `job_output`: pure pageable retained node output/result read;
- `job_ack`: idempotently advance cleanup watermarks after safe receipt;
- `job_cancel`: idempotently request Job cancellation;
- `job_restart`: explicitly create a fresh Job from a retained immutable Graph.

`job_restart` is fully generic because every Job retains one immutable Graph.
Restart requires the source Job to be terminal and retained, creates a new Job
with every node initially pending, preserves the old Job as history, may repeat
side effects, and is replay-protected so duplicate delivery returns the same
restarted Job.

When eligible, native MCP Tasks map onto the same Job rather than creating
another state machine: Task get/poll is the same underlying status operation and
Task cancel is the same underlying cancellation intent. A Job that is not
exact-result-fit eligible simply uses the `job_*` projection; it is never
forced into a lossy Task representation.

No control operation mutates or deletes the historical ledger of a terminal
execution.

### Job lifecycle status versus node outcomes

Job lifecycle status is intentionally separate from the canonical Graph-node
outcome grammar.

Node outcomes remain:

~~~text
ok  unchanged  fail  mixed  unknown  skip
~~~

plus query modifiers `more` / `incomplete`.

An exposed Job may additionally be in lifecycle states that are not node
outcomes:

~~~text
running
cancelling
cancelled
~~~

A terminal one-node Job may surface its node outcome directly
(`ok/unchanged/fail/mixed/unknown`). A multi-node Job uses the aggregate
Graph outcome. After an explicit cancellation request has settled, the Job
lifecycle is `cancelled` while individual nodes retain their exact terminal
outcomes (for example earlier `ok`, running ShellNode `fail cancelled`, and
pending `skip cancelled`).

`cancelled` is therefore never a `when` predicate for a Graph node. Dependency
expressions see only canonical node outcomes; Job lifecycle is observed through
`job_get`.

### Listing and polling

`job_list(limit=64)` lists all exposed Jobs in one compact namespace so a lost
creation/handoff response can be recovered. The display label is derived from
the Graph shape, not from a separate durability kind:

- one ShellNode -> its interpreter name (`bash`, `pwsh`, `powershell`);
- one NativeToolNode -> its tool name;
- multiple nodes -> `graph`.

Example:

~~~text
j1 bash running
j2 graph running 5/12
j3 cp ok
j4 powershell fail exit=1
~~~

`job_get(id)` returns **status, not bulk output**, so repeated polling is cheap.

One-node examples:

~~~text
running pwsh
ok cp
fail powershell exit=1
~~~

Multi-node example:

~~~text
running graph 5/12
1-3 ok
4 fail exit=1
5 ok
6 run
7-12 pending
~~~

Contiguous nodes with identical simple states may be range-compressed. Active
nodes may append their latest meaningful progress snapshot:

~~~text
running graph 5/12
6 run 412/1000 copy
~~~

For arbitrary shell commands, do not invent a percentage:

~~~text
running graph 5/12
6 run
~~~

Terminal graph examples:

~~~text
ok graph 12/12
~~~

~~~text
mixed graph 12/12
4 fail exit=1
9 unknown
10-12 skip condition
~~~

Status polling never acknowledges, consumes or deletes retained results/output.
Potentially large output remains behind `job_output` and is not repeated on
every status poll.

### Cancellation

`job_cancel(id)` is the one idempotent cancellation primitive for every
exposed Job.

Cancellation first records Job cancellation intent in the live Job session. Once recorded, no new
pending node may start.

Pending nodes become:

~~~text
skip cancelled
~~~

Running ShellNode process trees receive termination requests. Already-started
native mutations are allowed to reach their existing safe terminal semantics
rather than being interrupted at an unsafe publication point.

For a one-ShellNode Job this naturally reduces to terminating its one contained
process tree while preserving already-spooled output.

While a multi-node Graph settles:

~~~text
cancelling graph 11/12
6 run
7-12 skip cancelled
~~~

When all nodes are terminal:

~~~text
cancelled graph 12/12
~~~

Repeating `job_cancel` never creates another action. Cancelling an already
terminal job returns an unchanged terminal status, for example:

~~~text
unchanged ok
unchanged cancelled
~~~

Cancellation does not imply rollback. Shell or native operations that already
produced externally visible side effects remain visible and are represented by
their retained outcomes.

### Process/session durability boundary

Request/transport durability is mandatory within one live dotlink Job session: a disconnect, hosted request timeout, lost response, reconnect or redelivery attaches to the same Job and never restarts work.

V1/v2 do **not** make JobManager a persistent workflow database. A dotlink
process exit, crash, or execution-authority/session replacement ends that Job
session:

- old Job IDs are not resurrected in the replacement process;
- running work is cancelled on controlled shutdown where possible;
- leftover spool/session state is protected cleanup state, not reconstructed as
  live Jobs;
- `job_restart(id)` creates another execution only from a retained Job in the
  current live session.

Cross-process Job resurrection is deliberately deferred. It requires a separate
design for crash-atomic storage, schema/version migration, corruption recovery,
quota ownership, binary-upgrade compatibility and OS child reattachment.

Cross-process **replay safety** remains mandatory even though Jobs are not
resurrected. Each transport must explicitly establish one of two strategies:

1. prove from its protocol/session contract that a request belonging to a retired
   process session cannot be redelivered to the replacement process; or
2. use a minimal protected replay tombstone.

Where a tombstone is required, its claim is committed crash-atomically **before
any root node may start**. It stores only the fixed-width request/namespace
identity digest, semantic request hash, claim state and bounded timestamps/
terminal summary needed to reject mismatched reuse and prevent re-execution. It
does not store the Graph, command/stdin, file contents or node payloads. After a
process replacement, a matching claimed request whose exact original result is
not reconstructible fails closed as `unknown`; it is never executed again merely
to regenerate a response.

Tombstones use a bounded protected-state quota and expire only after the
transport's maximum redelivery horizon (or an authoritative positive retirement)
so cleanup cannot reopen the duplicate-execution window. This is a transport
correctness mechanism, not a persistent Graph/Job store.

The complete graph request, including command, `dir`, `stdin`, timeout/output
limits, `writes` hints and dependency expressions, participates in the outer
semantic request identity.

The shell command's execution timeout remains its process lifetime. Transport
response windows only decide when the graph hands off to durable mode; they
never shorten the command timeout or trigger a new execution attempt.

### Output, results and acknowledgement

Reuse one `job_output` / `job_ack` lifecycle for every Job rather than
inventing graph-specific output tools.

Advertised target shape:

~~~text
job_output(id, node=null, stream=null, offset=0,
           max_output_bytes=null, encoding="text")

job_ack(id, node=null, stream=null, offset)
~~~

Node selection is structural:

- one-node Job: `node` may be omitted and means node 1;
- multi-node Job: `node=N` is required.

Available streams depend on that node:

- ShellNode: `stdout | stderr | result`;
- NativeToolNode: `result`.

When `stream` is omitted, infer the natural primary stream:

- ShellNode -> `stdout`;
- NativeToolNode -> `result`.

An explicit invalid stream/node combination fails rather than guessing.

`result` is the retained canonical **Graph-node result representation**, not a
serialized copy of the public MCP `CallToolResult` envelope. Native node
adapters retain the structured outcome/modifiers/payload needed by the common
Graph renderer; v1 Graph-eligible native tools must have an exact compact
text/result representation. For a ShellNode, `result` is compact terminal
metadata (exit/success/timeout/error/context/truncation counters as applicable),
**not another copy of stdout/stderr**; those bytes live only in their dedicated
streams.

Large native results therefore use the same durable paging/ack lifecycle as
shell streams without being copied into every `job_get` response or duplicated
inside shell metadata. Typed MCP content is not silently flattened; a tool whose
result cannot be represented exactly by this channel is not Graph-eligible until
the retained-result layer explicitly supports that content type.

Acknowledging a node stream/result may release retained bytes but never erases
the node's terminal checkpoint/status or immutable Graph definition from the Job
ledger while the Job itself remains retained.

`job_output` remains a pure replayable read and never advances cleanup.
`job_ack` advances the cleanup watermark for exactly one Job/node/stream after
the caller safely receives it.

The Job ledger stores stream/result identity and bounded metadata, not unbounded
payload copies. Existing shell stdout/stderr spooling is reused.

A completed graph's compact ordinary result may inline node results that fit the
aggregate budget. Larger retained outputs stay available through `job_output`
with stable offsets.

Output allocation/rendering must be deterministic and independent of parallel
completion order.

### Progress

Top-level progress remains terminal nodes / total nodes:

~~~text
3/12 5 run
4/12 5 ok
~~~

While a ShellNode runs, the Job may emit the same completed-node fraction with
a terse node/phase message. Arbitrary shell commands have no honest
percentage, so do not invent a child total.

### Shell write hints inside v2 graphs

The scheduler may use `writes` to avoid known overlap between simultaneously
runnable shell/native mutation nodes.

This remains an optimization, not a correctness boundary. A missing/inaccurate
hint does not become authority and V2 does not claim transactional safety for
arbitrary shell writes.

For conservative scheduling, an omitted shell footprint may be treated as
conflicting with other mutating nodes. A provided footprint can allow more
parallelism when scopes are disjoint.

### V2 implementation boundary

Do not enable ShellNodes merely by flipping a registry bit. V2 requires the
canonical Job/Graph/Node runtime, exact interpreter binding, process-terminal
ShellNode semantics, replay reattachment, cancellation, bounded output retention
and the corresponding tests as one coherent feature.

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

execution/
  job
  graph
  node
  shell
  retained_output

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

### Shared execution layer

The Job/Graph runtime is a shared execution substrate, not a wrapper around the
public tool router.

Key rules:

- public `bash` / `pwsh` / `powershell` construct one-ShellNode Jobs;
- `batch(ops)` and `job_start(ops)` compile the same Graph grammar;
- Graph validation resolves each node through one central Graph-node registry;
- node arguments are validated against the already registered tool schema;
- Graph execution dispatches through an **internal node adapter/shared tool
  implementation**, not by recursively calling the public MCP tool handler;
- therefore a ShellNode runs a process directly under its parent Job and never
  creates a nested Job;
- NativeToolNodes reuse the same underlying filesystem implementations as direct
  calls, with only node lifecycle/checkpoint/result retention added;
- Graph control tools (`batch`, `job_start`, `job_*`, Task control) are never
  themselves Graph nodes.

The shared registry should carry at least tool name, Graph-node eligibility,
version gate, capability requirements, replay/side-effect classification and
node adapter. Schema/visibility metadata must remain single-sourced with
`TOOL_SPECS` or its successor rather than copied into the Graph engine.

## Shell naming + write hints — v1

Native filesystem mutations expose their target paths directly in ordinary
arguments. Arbitrary shell commands do not, so every advertised shell
interpreter gains the same optional **advisory scheduling hint**:

~~~text
bash(command, dir=".", stdin=null, timeout_secs=null,
     max_output_bytes=null, writes=null)

pwsh(command, dir=".", stdin=null, timeout_secs=null,
     max_output_bytes=null, writes=null)

powershell(command, dir=".", stdin=null, timeout_secs=null,
           max_output_bytes=null, writes=null)
~~~

The same arguments naturally carry through when a shell node appears inside
`batch(ops)` or `job_start(ops)`, so scheduling information is not lost when the
same operation is graph-owned or immediately durable.

### Interpreter identity and capability exposure

The three shell tools are distinct first-class capabilities:

~~~text
bash         Bash
pwsh         modern PowerShell (PowerShell 6/7+, `pwsh` / `pwsh.exe`)
powershell   Windows PowerShell (`powershell.exe`, typically 5.1)
~~~

There is **no aliasing or fallback between them**. `pwsh` never silently invokes
`powershell.exe`, and `powershell` never silently invokes `pwsh.exe`.

Tool discovery is capability-derived: advertise only interpreters that dotlink
has actually discovered/prepared and is authorized to execute. A Windows host
may therefore expose both `pwsh` and `powershell`, only one, or neither. On
platforms where Windows PowerShell is unavailable, `powershell` simply does not
appear.

This makes interpreter selection explicit and lets the model choose the command
language/runtime that matches the task instead of making dotlink guess.

### `writes` semantics

`writes` is omitted/null or an array of path scopes:

~~~text
writes=["src/foo.rs", "Cargo.lock"]
writes=["src"]
writes=[]
~~~

- omitted/null means the command's write footprint is **unknown**;
- `[]` means the caller expects no filesystem writes;
- a non-empty array is the caller's conservative expected write footprint;
- relative entries resolve from the command's effective `dir`;
- absolute entries remain absolute;
- no globs, shell expansion, environment interpolation or destination inference;
- a scope covers the named path and, conservatively, descendants beneath it;
- `"."` therefore means the whole working-directory subtree;
- create, modify, delete and rename all count as writes; a rename should list
  both source and destination scopes when known.

The name is deliberately `writes`, not `mutates`: it is short, familiar and
states the scheduling question directly.

### Hint, never authority

`writes` is not a security declaration, sandbox policy, mutation lock or
correctness proof. dotlink does not assume an arbitrary command stayed inside
the declared scopes. As an ordinary tool argument it is still bound by the
request's existing semantic replay hash; it does not create a separate replay
identity or authorization rule.

A runtime such as aw may use it to avoid **known** contention. Omitted
footprints should be scheduled conservatively when contention matters. A wrong
or incomplete hint can reduce scheduling quality and may allow shell-level
contention; it never grants additional filesystem authority.

Do not add line/range hints, read footprints, scheduler IDs or revision tokens
to the v1 shell schema.

## Concurrency and external runtime coordination

The native v1 model-facing filesystem surface adds **no** line/range lock hints,
revision tokens, scheduler IDs or coordination flags. Shell's optional
`writes` field is the one deliberate path-level scheduling hint because shell
commands otherwise hide their mutation footprint.

For native tools, dotlink remains the correctness authority. Existing mutation
coordination, snapshot verification, staging and atomic publication must keep
concurrent native writers safe even when an external runtime schedules poorly.

An orchestration runtime such as aw already has enough information for a
conservative path-level scheduler from normal tool arguments:

~~~text
write(path)        writes path
edit(path, edits)  writes path
patch(patch)       writes paths named by patch sections
cp(from, to)       reads from, writes to
mv(from, to)       mutates both namespace paths
rm(paths)          mutates those paths
read/bread(path)   reads path
ls/find/grep       read-side scopes
~~~

A runtime may therefore serialize mutations that target the same path and run
disjoint mutations in parallel. This scheduling is an optimization only; it
must never be required for correctness.

Do not add range-level concurrent commits in v1. Two edits touching different
byte/line ranges still construct whole-file replacement images from snapshots;
allowing simultaneous commits safely would require merge/rebase semantics, not
merely finer locks.

Do not add model-facing strict revision preconditions, line/range locks or
fail-fast lock controls in v1. If measured high-concurrency workloads later show
a need, add machine/runtime-only coordination metadata below the AI schema, for
example opaque revisions, resolved mutation ranges or fail-fast admission. Such
metadata must remain advisory/optimizing; hard native-mutation safety continues
to come from dotlink's mutation layer.

Arbitrary shell writes are inherently outside the native transactional mutation
model. `writes` improves scheduling visibility but does not make shell effects
atomic, verified or rollback-safe.

The design preference is to coalesce compatible same-file work into one
`edit(path, edits)` call when a runtime can do so safely, rather than creating a
distributed line-locking protocol.

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
| `bash`, `pwsh`, `powershell` | shell authority + exact prepared interpreter |
| `batch`, `job_start` | every referenced node must be visible/authorized |
| `job_list`, `job_get`, `job_output`, `job_ack`, `job_cancel` | access to the current runtime Job session |
| `job_restart` | current runtime Job-session access + current authority for the entire retained Graph |

The current `Capability::Job => shell` coupling must be removed. Job control is
not shell authority: native-only `batch` / `job_start` can create durable Jobs
when shell is disabled. Introduce a control-plane Job visibility capability (or
derive it directly from the presence of any visible Job-producing surface)
independently of shell permission. A runtime that can expose a Job must expose
the controls needed to observe/cancel/consume that Job.

Jobs are deliberately **runtime-session scoped**, not per-MCP-client objects.
All callers authorized to use the same LocalMachine/runtime Job session share
its exposed Job namespace. Do not add per-client principals/ACLs to JobManager
without a real multi-principal isolation requirement; the runtime/profile is the
security boundary today.

The implementation should also evolve `TOOL_SPECS` (or its successor) toward a
more precise capability representation for mixed-source/destination operations,
rather than weakening policy semantics to fit the current enum.

Graph creation preflights every node against the current effective tool
visibility, schema and capability rules before the first node starts. Conditional
branches do not bypass authorization merely because they may later be skipped.

A running Job is bound to the prepared execution-authority generation captured
for that Job. An incompatible policy/launcher/interpreter change revokes the
**old Job session as a whole**, matching the current runtime-reload model:

- record revocation intent before allowing further scheduling;
- start no new pending nodes;
- mark pending nodes `skip revoked`;
- terminate running ShellNode process trees;
- let already-started native mutations reach their safe publication/terminal
  semantics rather than interrupting an unsafe commit window;
- settle the Job lifecycle as cancelled with reason `revoked` before the old
  session is discarded/replaced.

Never leave pending nodes indefinitely non-terminal under stale authority, and
never widen an existing Job because configuration became more permissive.

`job_restart(id)` is a **new execution** and therefore revalidates the retained
immutable Graph against current authority and current prepared interpreter/tool
availability before any new side effect starts. The old Job's authority is not a
capability token for the restart.

Status/output/cancellation of an exposed Job in the current Job session must remain possible
as needed for observation and safe shutdown even when new execution authority is
no longer available; they must never confer authority to start new work.

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

## Final model-facing surface snapshot

This is the target advertised core after the design reductions above:

~~~text
ls(path=".", sort="path", meta=[], hidden=false, gitignore=false,
   offset=0, limit=256)

find(path=".", glob=null, regex=null, depth=null, kind="any",
     hidden=false, gitignore=false,
     mtime=null, ctime=null, btime=null,
     sort="path", meta=[], offset=0, limit=200)

grep(pattern, path=".", glob=null, depth=null,
     literal=false, ignore_case=false,
     mode="content", context=0,
     hidden=false, gitignore=false,
     offset=0, limit=200)

read(path, line=1, lines=4096, byte=0, numbered=true)

write(path, content)
edit(path, edits)
patch(patch)

cp(from, to)
mv(from, to)
rm(paths, recursive=false)

bread(...)
bwrite(...)
bedit(...)

batch(ops)

bash(command, dir=".", stdin=null, timeout_secs=null,
     max_output_bytes=null, writes=null)
pwsh(command, dir=".", stdin=null, timeout_secs=null,
     max_output_bytes=null, writes=null)
powershell(command, dir=".", stdin=null, timeout_secs=null,
           max_output_bytes=null, writes=null)

# shared durable execution control
job_start(ops)
job_list(limit=64)
job_get(id)
job_output(id, node=null, stream=null, offset=0, ...)
job_ack(id, node=null, stream=null, offset)
job_cancel(id)
job_restart(id)

# v2: bash/pwsh/powershell may also appear as ShellNodes in multi-node Graphs
~~~

The surface is intentionally asymmetric where safety requires it:

- `cp` copies the named object completely, including a directory tree, with no
  shell-derived `recursive` flag.
- `rm` keeps explicit `recursive=false` because recursive deletion is a
  destructive escalation.
- `patch` is strict-only in the advertised API.
- `grep` uses one symmetric `context`; asymmetric inspection belongs to
  follow-up `read`.
- `find` returns directly reusable paths; `ls` returns basenames to avoid
  repeating its already-known directory prefix.

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

Measure the complete serialized advertised tool surface before and after
implementation.

Keep two regression metrics:

1. exact serialized schema bytes, which is model-independent and stable;
2. a representative tokenizer estimate, which approximates model context cost.

Record a baseline for the filesystem/tool surface in CI or an equivalent
repeatable gate. A material increase requires explicit justification.

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
different mixed/unknown outcome that the model must reason about.

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

legacy multi-file edit objects or temporary singular edit
  → path + edits=[[old,new,all?], ...]

legacy job control `job_id`
  → id
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
6. generalize the existing job execution layer into the lightweight canonical
   `Job -> Graph -> Node` runtime, then add native-node `batch(ops)` and
   `job_start(ops)` together on the same parser/registry;
7. route `bash`/`pwsh`/`powershell` through the same one-ShellNode Job path and
   enable those ShellNodes in v2 Graphs without nested child Jobs;
8. measure exact serialized schema bytes, tokenizer estimates and representative
   result token costs;
9. run the full cross-platform, Nix, replay, mutation and CI validation matrix.

Do not implement lifecycle operations before their shared mutation semantics are
designed against Unix, macOS and Windows rename/delete behavior.

## Acceptance criteria

### V1 core + native Job/Graph runtime + shell write hints

The v1 implementation is complete only when all of the following hold:

- `ls` is immediate-directory-only.
- `find` owns recursive pathname/metadata discovery.
- `grep` exclusively searches contents while retaining cheap path filtering.
- `hidden`, `gitignore`, `mtime`, `ctime`, `btime` mean the same thing
  everywhere.
- discovery defaults produce compact text rather than per-entry JSON objects.
- every `find` path is directly reusable as another tool path; `ls` is the
  basename-only token-saving exception.
- metadata is opt-in.
- mutation success does not echo redundant request data.
- `max_read_bytes` and `max_write_bytes` remain runtime/profile policy rather
  than routine model-facing knobs.
- `read` can continue inside a line larger than the effective result budget
  using an exact UTF-8 `line` + `byte` cursor.
- ordinary read pagination returns usable partial data; a mid-read I/O fault is
  explicitly incomplete and error-marked.
- over-limit or mid-stage writes never expose partial target contents.
- post-publication failures are reported as committed/unknown rather than as
  generic retryable mutation failures.
- `edit(path,edits)` owns one or more independent exact substitutions in one
  file, planned against one original snapshot and published once; strict-only
  `patch(patch)` owns contextual/dependent/overlapping/multi-file changes.
- `grep` exposes one symmetric `context` field rather than separate before/after
  controls.
- `cp(from,to)` copies files, symlinks or complete directory trees without a
  shell-derived recursive flag.
- `cp` publishes any missing destination-parent suffix atomically with the
  destination; failure leaves no newly visible parent entries.
- `cp`, `mv` and `rm` provide the missing non-shell lifecycle operations.
- no mutation tool expands globs or performs destination inference.
- `mv` never silently becomes copy + delete and requires its destination parent
  to exist, preserving strict rename-or-no-change semantics.
- multi-root `rm` rejects duplicate/alias/ancestor-descendant overlaps before
  the first deletion.
- directory-tree `cp` and recursive `rm` remain bounded and fail closed
  around protected/denied content.
- all new mutation tools participate in replay protection and shared mutation
  coordination.
- tool visibility remains derived from the effective authority.
- compatibility aliases are absent from advertised schemas.
- exact serialized schema bytes, a representative token estimate and
  representative output-token costs are measured and recorded.
- bare Graph dependencies require complete success; `more`/`incomplete` nodes
  require explicit predicates and cannot silently unlock later mutations.
- condition-selected Graph skips are non-errors; aggregate summaries expose
  query modifiers without duplicating exceptional nodes.
- aggregate outcomes use `mixed` for multiple material outcome families,
  including mixtures containing unknown components; uniformly unknown material
  outcomes use `unknown`; expected `skip` is structurally neutral.
- dependency grammar has literal status/modifier predicates plus `N.*` for any
  terminal outcome and no `.err`/`.done` status classes.
- `batch(ops)` and `job_start(ops)` share one native-node Graph parser/registry;
  `batch` waits optimistically while `job_start` exposes the Job immediately.
- `job_start` commits durable Job identity/exposure/replay state after complete
  Graph preflight and before any root node starts; a lost receipt is recoverable
  through `job_list` without duplicate creation/execution.
- Graph control/job/task tools cannot appear as nodes.
- `bread(format=mcp)` remains direct-only until durable retained results support
  typed MCP content exactly; Graph eligibility never depends on `bread` args.
- one process-wide node-runner admission budget bounds concurrency across all
  Graph Jobs, independently from active/retained Job and spool quotas.
- Job controls are visible independently of shell permission whenever the
  runtime can expose Jobs, including native-only profiles.
- native MCP Task projection is used only when the exact eventual ordinary
  result is guaranteed to fit; otherwise the same Job uses `job_*`.
- an abandoned optimistic `batch` can never hide execution: durable-request
  transports retain replay ownership, while transports without such identity
  expose the same Job for `job_list` recovery.
- `bash`, `pwsh`, and `powershell` expose the same optional advisory `writes`
  scopes; omission means unknown and `[]` means expected no writes.
- `pwsh` and `powershell` are distinct interpreter capabilities with no aliasing
  or silent fallback; discovery advertises only prepared/authorized interpreters.
- `writes` never changes authorization, sandbox policy or native mutation
  correctness semantics.
- the public edit field remains `edits`, not an opaque abbreviation.
- native v1 tools need no model-facing coordination hints/revisions/range locks;
  shell exposes only advisory `writes` path scopes because its footprint is
  otherwise opaque.
- existing correctness/security invariants remain covered on Linux, macOS,
  Windows and Nix CI.

### V2 canonical Job graph runtime + shell-node extension

The v2 extension is complete only when:

- every exposed execution is one Job owning one immutable Graph; ShellNodes are
  contained node runners, never child Jobs;
- direct `bash`, `pwsh`, and `powershell` use the same one-ShellNode Job/Graph
  runtime as shell nodes inside multi-node Graphs;
- all three are distinct ShellNodes, advertised independently only when their
  exact prepared interpreters are available; no PowerShell alias/fallback exists;
- shell node success/error follows actual process terminal state, never
  job/task handoff;
- non-zero exit, timeout, launch failure and cancellation map to canonical
  ShellNode `fail` and retain raw process details;
- a normally launched direct shell process that exits non-zero preserves the
  existing MCP-success process-result contract with `success=false`; Graph
  `fail` does not redefine process exit as protocol/tool failure;
- shell errors never imply rollback or absence of arbitrary shell side effects;
- the outer Job is the sole durable owner of its Graph and running ShellNodes,
  and its scheduler continues after transport handoff without active polling;
- nodes use durable `pending -> claimed -> running -> terminal` checkpoints;
  terminal predecessors are never rerun;
- redelivery/recovery never duplicates an already-started process; ambiguous
  in-flight state becomes `unknown` rather than being relaunched;
- one durable Job model controls every exposed Graph in one ID namespace;
- that Job namespace is runtime-session scoped rather than per-client, and
  Job/Graph objects are not resurrected after process/session replacement;
- cross-process request replay cannot duplicate execution: the transport must
  prove retired requests cannot redeliver or use a minimal replay
  claim/tombstone to fail closed, without persisting a full Job database;
- the v1 `batch(ops)` / `job_start(ops)` Graph parser, registry, preflight and
  creation-order guarantees remain unchanged when ShellNodes are enabled;
- `job_list` labels Jobs from Graph shape/tool identity rather than a separate
  durability kind;
- `job_get(id)` provides compact status polling and automatic resume/reattach;
- `job_output`/`job_ack` reuse one paging/cleanup lifecycle: one-node Jobs may
  omit `node`, multi-node Jobs require it, and nodes expose retained `result`
  plus ShellNode stdout/stderr;
- `job_cancel(id)` is idempotent and durably freezes new graph-node starts before
  terminating/settling running work;
- generic replay-protected `job_restart(id)` creates a fresh Job from the
  retained immutable Graph, never mutates or resets history in place, and
  revalidates the whole Graph against current authority/interpreter availability
  before any new node starts;
- `job_output`/`job_ack` support structural node selection and retained `result`
  streams without special durability kinds;
- cancellation, bounded output spooling/paging and deterministic rendering work
  through the graph owner;
- execution-authority revocation terminates the old Job session rather than
  stranding pending nodes: pending nodes become `skip revoked`, ShellNodes are
  terminated, active native mutations settle safely, and the Job settles
  cancelled/revoked;
- the advertised `job_start(ops)` migration never creates an intermediate
  release regression: legacy shell-shaped `job_start` is accepted only at the
  compatibility boundary until the one-ShellNode path is available;
- `writes` may improve scheduling but never becomes authority or a transactional
  guarantee;
- v2 durability/replay/cancellation/output, lost-receipt, authority-revalidation
  and exact-interpreter tests pass across supported transports/platforms.

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
17. `edit` batches the hot case of multiple independent exact substitutions
    in one file; every tuple matches the same original snapshot and overlapping
    ranges fail before publication.
18. Primary outcomes are `ok`, `unchanged`, `fail`, `mixed`,
    `unknown`, `skip`; `more` and `incomplete` are orthogonal query
    modifiers that may coexist.
19. Paging supports both known totals and cursor-only unknown totals without
    performing extra work merely to count pages.
20. Progress updates are cumulative snapshots. Model-visible status uses
    compact `current/total phase` when the fixed end is known and
    `current phase` when it is open-ended.
21. Result data is never available only through progress notifications; final
    results and explicit paging remain authoritative.
22. One canonical `ops=[tool,args,when?]` Graph grammar powers both
    `batch(ops)` and `job_start(ops)`; their only semantic difference is
    optimistic waiting versus immediate durable exposure.
23. Graph structure may express sequence, parallel roots/fan-out, joins and
    status-based decisions, but never output dataflow, loops or a general
    workflow language.
24. Graph node arguments stay structured; only the tiny `when` dependency
    expression is textual, avoiding a second quoting/escaping language.
25. `more` and `incomplete` are orthogonal query modifiers, not peer primary
    terminal outcomes.
26. Bare Graph dependencies require complete success; paged/incomplete results
    never silently unlock dependent mutations.
27. `patch` is strict-only, `cp` copies complete named objects, and `grep`
    uses one symmetric context field to keep the advertised surface smaller.
28. Semantic batching stays inside domain tools where it improves the hot path:
    `edit` batches independent replacements within one file; the generic Graph
    runtime remains for heterogeneous operations.
29. `edit` has one advertised shape for both one and many replacements; do not
    add a singular shorthand that would make the schema polymorphic.
30. Keep the descriptive `edits` field name. Abbreviating it to `v` saves
    negligible bytes while losing semantic clarity; the compact inner tuples
    capture the meaningful token savings.
31. Native v1 tools add no line/range locks, revision tokens or coordination
    knobs. Shell alone gains optional path-scope `writes` because its filesystem
    footprint is otherwise opaque; the hint is advisory, never authority.
32. The canonical execution architecture is `Job -> Graph -> Node`: every Job
    owns exactly one immutable Graph, and nodes are never nested child Jobs.
33. `bash`, `pwsh`, and `powershell` are one-ShellNode Job constructors;
    `batch(ops)` is the optimistic Graph constructor and `job_start(ops)` is the
    immediate durable Graph constructor. All use the same execution engine.
34. V2 enables `bash`/`pwsh`/`powershell` as ShellNodes with process-terminal
    semantics; ShellNodes never escape as independent child Jobs/Tasks.
35. Durability is automatic at the Job level: long ShellNodes continue after
    transport handoff, and terminal predecessors are checkpointed before they
    unlock dependents.
36. Resume means reattach to the same durable Job **within the live Job
    session**; restart means create a new Job from the retained immutable Graph.
    Only explicit restart may intentionally re-execute terminal nodes.
37. Durable lifecycle control is shared through `job_*`, not duplicated under
    `batch_*`: `job_get` polls, `job_cancel` cancels, `job_output`/`job_ack`
    manage retained output, and generic replay-protected `job_restart` creates a
    fresh execution while preserving history.
38. `unknown` is a primary outcome for uniformly unknown material results;
    `mixed` may contain unknown components and is therefore conservatively
    ambiguous for retry/compensation. The DSL does not add a second class system
    to introspect mixed counts.
39. Shell-node process success maps to `ok`; known non-zero exit, timeout,
    launch failure or cancellation maps to `fail`. Shell failure never implies
    rollback of arbitrary filesystem side effects.
40. `pwsh` and `powershell` are both first-class, distinct interpreter
    capabilities: modern PowerShell versus Windows PowerShell. Neither aliases
    or falls back to the other, and only actually available interpreters are
    advertised.
41. `mixed` is the aggregate material-outcome state: success/failure/unknown
    families may coexist inside it; `unknown` means all material outcomes are
    unknown, while `skip` is structural/neutral when other material outcomes
    exist.
42. The dependency DSL has no status classes. Bare `N` means complete
    `ok|unchanged`; exact predicates select primary outcomes/modifiers; `N.*`
    alone means any terminal outcome.
43. `mv` requires an existing destination parent so it remains exactly one
    rename or no namespace change. `cp` may create missing parent suffixes only
    as part of the same atomic publication; `rm(paths)` rejects overlapping
    roots before deletion.
44. `bread(format=mcp)` stays direct-only until retained Graph results can
    preserve typed MCP content exactly; Graph eligibility never varies by
    runtime argument shape merely to admit base64/hex variants.
45. Graph execution has a process-wide node-runner admission budget in addition
    to per-Job width, active/retained Job limits and spool quotas.
46. Job control is a runtime control-plane capability, not shell authority.
    Native-only runtimes can expose/control Jobs, and Jobs are runtime-session
    scoped rather than per-client ACL objects.
47. Native MCP Task is only an exact projection of the same Job when the
    eventual ordinary result is guaranteed to fit; otherwise the Job stays on
    the `job_*` projection without lossy conversion.
48. Dotlink process/session replacement ends the live Job session; v1/v2 do not
    add a persistent workflow database. Cross-process replay safety is solved at
    the transport identity/tombstone layer, not by resurrecting Graphs.
49. Execution-authority revocation terminates the old Job session instead of
    freezing pending nodes indefinitely.
50. A ShellNode may be Graph-`fail` for a non-zero process exit while the direct
    shell tool preserves its existing MCP-success process-result envelope with
    `success=false`; process failure and tool/protocol failure are distinct.
51. Optimistic Jobs inherit the existing abandoned-response invariant: execution
    is either replay-owned or exposed for `job_list` recovery, never hidden by a
    vanished response future.
52. The public `job_start(ops)` switch ships only with the one-ShellNode
    compatibility path ready; the old shell-shaped request may exist only at the
    wire/deserialization boundary during migration.

## References

- [Tool surface v2](../dotlink-tool-surface-2026-10-25/README.md)
- [Tool correctness review](../dotlink-tool-review-2026-10-26/README.md)
- [Architecture](../../../docs/ARCHITECTURE.md)
- [Security](../../../docs/SECURITY.md)
- [Plan queue](../TODO.md)
