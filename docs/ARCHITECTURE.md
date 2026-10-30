# Architecture

## Core split

dotlink is one Tokio process with a transport-neutral MCP core.

~~~text
                     setup/config
                         |
                         v
                  permission policy
                         |
                         v
                    LocalMachine
          read/write/edit/ls/binary/shell
                         |
          +--------------+---------------+
          |              |               |
          v              v               v
        OpenAI          stdio      Streamable HTTP
     Secure Tunnel   stdin/stdout       /mcp
                                         |
                                         +-- optional ngrok
~~~

LocalMachine, its access policy and the dynamic tool router live in `src/mcp.rs`; the tools themselves live one family per module under `src/mcp/tools/` (see [Tool layout](#tool-layout)).

Transport code is isolated under:

~~~text
src/transports/
  mod.rs
  openai.rs
  stdio.rs
  http.rs
~~~

No transport owns filesystem policy. `--list-tools` resolves the same canonical access policy and tool router without building a `LocalMachine` or loading keys/host integrations; execution alone owns those resources.

## NixOS service

The flake exports `nixosModules.default` (`nixosModules.dotlink`). The thin module in `nix/module.nix` runs the unchanged binary through systemd and delegates authority/OAuth to the normal runtime. With only `user` and `workingDirectory` configured, it intentionally sets no `DOTLINK_CONFIG` or credential overrides: startup reads that Unix user's saved XDG profile/key/OAuth state. Optional declarative `settings` generate a read-only schema-v10 JSON file in the Nix store, while optional `LoadCredential` files override only the relevant secret. Named instances create independent systemd units with named-profile defaults. Service mode always suppresses stdio MCP, uses stderr/journald logging, and handles SIGTERM with normal bounded transport cancellation. Dotlink retries OpenAI tunnel outages in-process; `src/recovery.rs` is the one failure taxonomy, applied at each failure's source. Only exit 2 and failures that retrying unchanged cannot fix stop the unit: planning content (policy, profile and credential validation) exits `CONFIG`/78, OpenAI key/tunnel rejections only after persisting for 60 s, and command-line usage errors exit 2. `Restart=always` restarts every other process termination, including exit 0 and systemd-clean signals outside an explicit stop/restart job; systemd jobs suppress that automatic restart. Unclassified failures, I/O-caused planning errors and lost classifications exit 1 and restart with exponential backoff up to ten minutes. Cleanup failures override the primary failure's class because live resources forbid an in-process restart. The module requires systemd 255 (`RestartSteps`/`RestartMaxDelaySec` require 254 and `SetLoginEnvironment` requires 255). `DOTLINK_SUPERVISOR=systemd` enables the operator hint and a `RESTART_RESET=1` notification after ten minutes of uptime; releases 255–257 are supported with a warning because reset notifications require 258. `src/supervisor.rs` removes `DOTLINK_SUPERVISOR` and `NOTIFY_SOCKET` from the environment before any thread starts. `src/shutdown.rs` latches Ctrl+C and SIGTERM once for the whole process, so a signal is never lost across runtime restarts. See `.agents/docs/nixos-service.md`.

## Public OAuth admission

Pending client registrations and owner consent use one non-evicting capacity
policy: reclaim expired state; cap the total, per-client/redirect, trusted
requester, and requester-within-client/redirect shares; and return HTTP 429
rather than invalidate a live request. The requester comes only from the direct
TCP peer or authenticated ngrok tunnel metadata; self-declared OAuth values and
forwarded headers never establish it. Source ports are discarded, IPv6 groups
by /48, and loopback is one identity. Owner consent POSTs hold an exclusive
claim across password and storage work. The claim resets on cancellation/error,
is re-keyed on a wrong-password retry, and is consumed after success or denial.
Password verification allows six rolling-minute starts per requester under two
process-wide workers; it charges both the consent creator and POST source, and
active rows in its bounded requester table are not evicted. Expired/full DCR promotions terminate with actionable errors; storage
failures preserve the claim. Authorization-code, access-token, and refresh-grant
stores prune expired entries but reject overload rather than evict live entries;
access tokens also have a 64-entry per-client share within the 512-entry process
bound, and failed issuance preserves the original code/grant. CIMD fetches reject
disallowed literal IPs before accounting, then allow 12 rolling-minute starts
and two in-flight operations per requester under a process-wide concurrency of
four. Its bounded requester table prunes only expired inactive rows and never
erases live rate history. Cancellable async system DNS makes timeout/request
cancellation release the complete lease immediately; missing system DNS
disables CIMD only, not registered-client OAuth. No anonymous global minute
counter is used.

## Descriptor-rooted discovery and shell guard lifetime

On Linux/macOS, directory names and each entry's metadata are obtained from
the same pinned directory capability. Linux uses `openat2` with no-follow
`O_PATH` and the directory's mount-resolution mask; macOS uses no-follow
`fstatat` with device-boundary enforcement. Both prevent pathname redirection,
with Linux also rejecting bind-mount crossings even on the same device.
Windows holds ancestor directories without delete sharing. Sandbox mounts
expose live host objects, so a host process can replace a pathname mask after
preflight or while an existing command runs. `validate_shell_guards` therefore
rejects any read-visible user bind overlapping protected/read-denied paths,
and any writable user bind overlapping write-denied paths. Implicit runtime
binds and aliases are checked against the same read-visible guards, using the
exact RuntimeLayout subsequently assembled by Bubblewrap. This includes
read-only binds and direct-child deny mounts: mount-point immutability inside
the sandbox does not prevent host-side renames. An explicit root read+write
grant (`--allow-rw=/`) intentionally waives private-path isolation for the
shell, but retains Bubblewrap and its separate network/process/host controls;
explicit deny rules are not waived. A profile should give shell
a narrow host directory grant, leaving denied/private paths outside it;
native MCP tools still enforce fine-grained denies per operation. Cache/host
integration overlap checks stay conservative.

For multi-file `edit`, each ordered `edits[].path` is the single public target
source; the compatibility deserializer accepts an older top-level default
without exposing it in the tool schema. Two global constants bound a call to
512 operations and 64 distinct files: the parser rejects oversize input,
planner enforces unique canonical file count, and advertised metadata and TOOL
log work use the same ceilings. A 64 MiB aggregate budget bounds retained
original snapshots and working text, and success receipts are trimmed to
`max_output_bytes` *before* any commit. TOOL logs derive a bounded `paths`
preview from operations rather than a redundant target list.

## Shared mutation lifecycle

`mcp::tools::mutation` is the common boundary for bounded multi-file work.
`edit` and `patch` retain their respective text/patch grammars, but share one
64 MiB `PlanBudget` for live source and working bytes, one bounded
`MutationReceipt`, and one `stage_and_commit` engine with typed verified
updates versus create-only targets. Strict commits stage every file first;
`patch` best-effort records per-file staging/commit failures. The latter is
not a transaction, and commit errors explicitly identify partial results.

The receipt budget is verified before the first stage; large success lists are
trimmed with exact omitted counts, and dynamically generated best-effort
failures use the same bounded builder. `bedit` shares the planning ledger and
precommits its result envelope, while `write` and `bwrite` precompute bounded
success text. The lower-level `largest_fitting_prefix` serves `ls`, `grep`, and
multi-file receipts, rather than maintaining independent binary searches.

The request-grammar AST for `patch` borrows paths, anchors and update-hunk
lines from its owned `PatchArgs.patch` string through planning. An add-file
section retains just a byte range and precomputed output size/line count,
then writes the output in one pass; it allocates no per-line strings or slice
vector, even for dense additions. `Snapshot::verify` compares the open pinned
source against its retained plan with a fixed 64 KiB buffer and one extra-byte
probe instead of allocating another complete snapshot. `edit` mutates common
single matches in place and compacts non-growing multi-match replacements via
byte-level `memchr` search, avoiding repeated full-suffix UTF-8 validation;
complex growing multi-match cases retain the standard library implementation.

The `grep` source reader retains its bounded byte buffer after the shared
`text_bytes` validation, so search borrows the UTF-8 view rather than cloning
it. A content page scans matching lines to retain only bounded selected
indices and adjacent page boundaries, then captures their at-most-8-line
contexts in one pass. The renderer reuses this bounded borrowed cache for
output-fit prefixes, instead of rescanning deep source offsets for each
rejected candidate; there is no full line/match index. Files/count results count hits directly from iterators. Discovery retains only the candidate paths the scan-file budget can actually visit (4096 in production), rather than the larger historical hard cap. Line-oriented content labels escape delimiter/control-bearing UTF-8 names and byte-escape invalid-UTF-8 Unix names, while ordinary paths keep the familiar format. Original
paging, context clipping, markers and scan budgets remain intact.

Standard-header HTTP preflight first deserializes only a borrowed method and
tool name; for known tools the unchanged request goes straight to rmcp without
a redundant JSON `Value` tree. Unknown tools still use the original complete
canonicalization, including header-consistency checks. The embedded OpenAI
adapter clones a JSON-RPC request only when rewriting an unknown tool name.
Binary input decoding consumes encoded owned strings instead of holding every
encoded range beside the retained decoded buffers. Byte-identical `bedit`
ranges are checked against the pinned source and its revalidated digest before
returning without staging any temporary file.

`patch` builds a borrowed `WorkingLine` index from `split_terminator('\n')`:
real blank lines remain addressable, but a terminal LF does not create a
phantom context line. Trailing-LF presence is tracked independently for exact
output reconstruction; a CR in an LF-formatted patch hunk is literal content,
whereas a CRLF patch header identifies delimiter CRs to strip from hunk
syntax. CRs belonging to source CRLF delimiters are distinguished from
literal trailing CRs. The shared `LineOutput` delays writing one line so an
unterminated source line can gain a CRLF separator when appended to; its
extra delimiter byte is counted before allocation. An unterminated line ending
in literal CR also requires a *separate* CR for the new delimiter, even in an
LF-style file; new lines with literal terminal CR follow the same rule.
Turning a formerly terminated line into EOF clears its separator metadata. Context-only hunks
advance the cursor without a replacement vector. Source-consuming context
references use one pointer per parsed `PatchLine`, and their KMP scratch table
is precharged against the existing mutation plan. The shared allocation-free `visit_hunk` enumerates context, removal and
addition actions identically for byte-size preflight, direct output and
in-place editing. Context boundaries separate edit groups. Added lines inherit their
positionally corresponding removed lines' separators, with excess additions
reusing the last removed line's style; pure insertions use their actual
insertion site. Unrelated context cannot change separator bytes in
mixed-ending files. `add_file_line` is also shared by `*** Add File` parsing/byte accounting
and output: bare and prefixed CRLF blank lines are equivalent without per-line
allocations. For a final mutating hunk,
`LineOutput` writes prefix, replacement and suffix directly into the result
without expanding the borrowed line index. Earlier hunks shift the index
in place and save only the source lines they actually consume, instead of
allocating a duplicate replacement-sized vector. Both preserve the shared
64 MiB planning budget and the existing output-before-commit checks.

`mcp::tools::matching::scan_sequence` implements a single generic KMP scan over
ordered inputs. `patch` uses it to locate hunk context in linear comparisons
rather than checking every candidate position against every context line. Exact
matching takes priority; whitespace-trimmed matching runs only if no exact
candidate exists. The helper reports overlapping occurrences through a callback,
so ambiguity diagnostics retain only a bounded preview of matching line numbers.

`edit` streams literal match positions for count and at most ten diagnostic
lines, with no per-match offsets retained. `patch` keeps only bounded hunk
ambiguity locations and represents source/added lines as borrowed
`WorkingLine` descriptors. For LF patch additions to CRLF files, a one-bit
trailing-CR flag is emitted directly into the final String rather than
allocating `Cow::Owned` for every line; its temporary line-index work is
accounted separately from the output-file
write limit, including growth projected before each hunk. It rejects section
33 while parsing. `bedit` rejects range 33 during deserialization and streams
source hashing and range replacement in 64 KiB chunks. Shared staged-update
writer callbacks in the Unix descriptor backend and Windows path backend
retain mode/DACL preservation, cancellation cleanup and the commit-time
SHA-256 precondition without retaining source plus output in memory.

## Profiles and persisted defaults

The default config is `~/.config/abird/dotlink/config.jsonc`. Named profiles use `config.<profile>.jsonc` and are selected with `-p/--profile`. Setup can target the same profile with `-S/--setup -p <name>`. Schema v10 JSONC supports line/block comments and trailing commas. Compatible older schemas migrate forward in memory through explicit version-to-version migration steps (currently v9 → v10); newer schemas and older versions without a defined migration path are rejected. Migrated profiles are serialized as the current schema when they are next edited or saved.

Each profile persists transport behavior plus the local permission model:

~~~text
permissions.default_allow
permissions.allow_read[]
permissions.allow_write[]
permissions.allow_rw[]
permissions.deny_read[]
permissions.deny_write[]
permissions.deny_rw[]
permissions.allow_shell
permissions.allow_network

transports.openai
transports.stdio
transports.http
transports.http_bind
transports.http_ephemeral_url
transports.ngrok
transports.ngrok_domain
transports.ngrok_ephemeral_url

oauth.enabled
oauth.public_url
~~~

OAuth secrets/grants are deliberately not stored in JSONC.
Config changes select immutable generations of saved OpenAI runtime keys and
OAuth owner password hashes. Generation bytes are persisted first; the JSONC
profile rename is the single selection commit. A crash before that rename
leaves the previously selected key/password valid. After a durable profile
credential rotation commit, best-effort generation pruning retains the selected
and immediately preceding secrets; unrelated profile edits leave these secrets
intact for older running dotlink processes. Failed cleanup retries on the
next credential rotation.
At runtime, environment or credential-file overrides take precedence before
reading a saved Runtime key. Active file credentials are read once from a regular
descriptor whose identity is verified against its protected target; Runtime and
ngrok transports consume that snapshot instead of resolving the path again. A
disabled transport never opens dormant files; execution with no usable override
fails closed for any unusable selected key, while profile inspection/editing stays
usable for replacement. `--print-id` and `--list-tools` are configuration-only
inspection paths that never resolve a Runtime secret.
OAuth owner authorization resolves the currently committed profile generation
on each password check (with a post-verify recheck); a deleted persisted
profile cannot fall back to a legacy hash. Interrupted deletion can be
retried even if only generated credentials survive. Legacy profiles without
selectors continue using their original credential files and owner hash.
Profile-specific OAuth state lives under the Abird XDG state namespace (`$XDG_STATE_HOME/abird/dotlink/oauth[.<profile>].json`), while OpenAI Runtime keys remain under the Abird XDG config namespace. Both are protected control-plane paths.

The process launch directory is the internal relative-path base. It is readable by default when `permissions.default_allow=true`; `--no-default-allow` removes that implicit grant for one run. Relative profile/CLI paths resolve from the launch directory. Saved profiles can intentionally contain no filesystem grants; profile validation checks structural dependencies while `Policy::from_args` determines effective capabilities only after merging persisted defaults, one-run grants and denies. No effective capability, or a shell without readable directory access, still fails. `allow_rw: ["."]` means rw on the launch directory; `allow_rw: ["/"]` is unrestricted filesystem read+write.

allow_shell enables the normal platform shell. allow_network controls network access for the normal sandboxed shell path. On Linux Bubblewrap still applies unless --unrestricted (or the legacy --allow-all --no-sandbox pair) is explicitly selected.

Every transport enabled in the selected profile starts automatically. Runtime flags add or suppress local transports for one run:

~~~text
--stdio / --no-stdio
--http  / --no-http
--ngrok / --no-ngrok
--ngrok-domain / --no-ngrok-domain
--oauth / --no-oauth
--public-url
--allow-public-no-auth
~~~

`--ngrok` modifies the HTTP transport; it is not a fourth MCP transport. HTTP bind and ephemeral-path flags override persisted HTTP/ngrok settings without requiring `--http` again when HTTP is already enabled in the profile.

A profile may configure no transport at all; explicit --stdio/--http can still activate local transports for a run.

## Runtime transport orchestration

main.rs builds one LocalMachine and one ActiveTransports plan.

Each active transport gets a clone of the same LocalMachine.

Clean termination of one transport does not terminate the others. For example, stdio EOF from a Claude subprocess session does not stop an active HTTP server or OpenAI Tunnel.

A transport error cancels the remaining active transports.

Filesystem discovery builds one `WalkProgress` that records entry budget,
unreadable descendants/metadata, and missing ignore rules. Both `ls` and `grep`
derive completeness from this result; `grep` additionally records unreadable
content candidates. JSON keeps the singular compatibility reason and an ordered
`incomplete_reasons` array so concurrent causes are not lost. Permission denies
and explicit ignore/hidden filters are intentional exclusions, never mistaken
for traversal failures. Paged reads place their 32 MiB skipped-input limiter
under the buffer, so prefetch counts while selected-window bytes remain usable.

Anonymous OAuth grant checks first consult an inexpensive, version-checked
read-only index of hashed refresh tokens and approved DCR clients, or the
in-memory authorization-code map. Definite misses are rejected before admission;
contention/version races fall through to the serialized authoritative transaction,
while a stable malformed snapshot returns retryable unavailability without being
reparsed per request. The same `StateIndex` invalidates by inode identity on Unix
or file index on Windows. Authorization-code redemption uses a short claim plus
notification rather than retaining its mutex across durable I/O. Access-token
validation remains independent of disk locks and shares recoverable poison
semantics with publication/revocation. Private state serialization and profile/key
saves enforce the same ceilings as their bounded readers before atomic replacement.

One outer HTTP Host allowlist wraps the merged MCP and OAuth router. It accepts
only exact listener/public-origin authorities, ignores forwarded headers,
normalizes mapped IPs, and rejects conflicting Host and URI/HTTP2 authorities
while preserving explicit/default-port semantics across reverse proxies. Ngrok
connections are served directly and accept only the tunnel's public authority;
OAuth issuer/resource identity stays bound to configured public origins.

The ngrok HTTP supervisor owns both HTTP server tasks in a `JoinSet`, rather than detaching their handles. Axum receives the direct peer from TCP accept metadata and the public peer from ngrok's authenticated connection metadata; its redacted `Debug` form prevents framework tracing from logging network identities, and HTTP forwarding headers are not an identity source. Tunnel EOF/error, cancellation, or a supervisor panic cancels the MCP token and remaining tasks. Outermost route middleware drops in-flight MCP/OAuth handlers and body extraction on cancellation; normal exit then allows up to five seconds for graceful server shutdown. Accepted TCP/ngrok IO is force-close-aware if detached Axum connection tasks exceed that bound, and bounded ngrok tunnel/session close RPCs run concurrently with draining. The parent transport budget is six seconds, leaving a one-second reserve around the five-second HTTP shutdown budget. A drain timeout is itself a cleanup failure because synchronous handler work cannot be preempted by closing IO. Cleanup failures take precedence over restart requests and propagate through peer and runtime teardown, preventing a replacement runtime from overlapping old ingress. A restarted runtime cannot retain old listeners, tunnel connections, or active MCP calls.

Interactive non-stdio runs also start a terminal-control loop: `v` mutates shared logging verbosity live, `V` toggles full command output only while TOOL logging is enabled (otherwise the key and its hint are inactive/hidden), `Ctrl+R` cancels/drains the active transports and rebuilds the runtime from the current profile/CLI state, and `Ctrl+C` exits. The command modifier resets when controls stop and can never be enabled by stdio or a noninteractive service. stdio disables key capture so protocol stdin remains exclusive.

## stdio

stdio uses rmcp's standard stdin/stdout transport.

stdout is reserved for JSON-RPC protocol frames. All human-readable status is written to stderr. For manual stdio runs on a TTY, dotlink temporarily enables the terminal interrupt signal for `Ctrl+C`; piped stdio clients are untouched.

This is intended for MCP clients that launch the server as a subprocess.

## HTTP

HTTP uses rmcp StreamableHttpService mounted at:

~~~text
/mcp
~~~

Default persisted bind:

~~~text
127.0.0.1:3000
~~~

Runtime override:

~~~text
--http-bind=<ADDR>
~~~

HTTP uses `LocalSessionManager` for normal Streamable HTTP sessions. The
streamable-HTTP settings are built once, in `transports::streamable_config`,
for both this server and the in-process tunnel service. That config owns the
request-body ceiling: six times `max_write_bytes` for JSON escaping, plus
64 KiB of base framing and practical, worst-case-escaped pathname headroom
for every edit allowed by `MAX_EDIT_OPS`. Edit path strings must not hit an
independent transport limit when the profile sets a small write-byte ceiling. rmcp's 4 MiB
default would otherwise refuse a write the tools accept. Network HTTP/ngrok keeps rmcp's SSE keepalives for intermediary idle timeouts; only the in-process OpenAI adapter disables wire keepalive/retry events. For SEP-2243 calls, network HTTP and the embedded adapter reject unknown policy tool names before rmcp's schema cache; the network preflight buffers under the same exact request-body ceiling.

When OAuth is enabled for a route, the Axum topology is deliberately split:

~~~text
public OAuth routes
  /.well-known/oauth-protected-resource
  /.well-known/oauth-authorization-server
  /oauth/authorize
  /oauth/token
  /oauth/register
  /oauth/revoke

protected MCP subrouter
  bearer middleware
    -> /mcp[/<ephemeral>]
      -> rmcp StreamableHttpService
        -> LocalMachine
~~~

Bearer validation happens **before** rmcp receives the request. OAuth authenticates a remote caller; the existing `LocalMachine` permission policy still determines what that authenticated caller may actually do.

`oauth.enabled` controls local/reverse-proxied HTTP only. Any direct non-loopback HTTP listener is treated as public ingress: persisted configuration requires OAuth plus an explicit HTTPS `oauth.public_url`, and one-run overrides require either the same OAuth/public-origin pair or `--allow-public-no-auth`. Issuer/resource identity is never inferred from request headers.

The embedded OAuth server is single-owner and uses:

- authorization code + mandatory PKCE S256;
- Protected Resource Metadata + Authorization Server Metadata;
- exact resource/issuer binding;
- CIMD client metadata with SSRF-resistant HTTPS fetching;
- DCR fallback;
- opaque in-memory access tokens;
- rotating refresh tokens persisted only as hashes and issued only to clients that advertise the refresh grant;
- an Argon2id owner-password hash with process-wide bounded `spawn_blocking` verification across runtime generations;
- cross-process file locking around OAuth state read/modify/write operations so setup, a live server, and management commands compose without lost updates; lock acquisition times out, and synchronous transactions release Tokio workers via a cancellation-safe blocking section.

Unauthenticated DCR registrations stay bounded/in-memory until successful owner consent; only approved clients are persisted.

## Independent ephemeral HTTP routes

The HTTP transport resolves two route policies:

~~~text
http_ephemeral_url
ngrok_ephemeral_url
~~~

--ephemeral-url sets both to true by default.

--http-ephemeral-url and --ngrok-ephemeral-url are per-transport overrides and accept explicit =false.

Local HTTP and ngrok are genuinely isolated: the TCP listener and direct ngrok tunnel use separate Axum routers. This lets one side expose `/mcp` while the other uses `/mcp/<random-token>` without accidentally mounting both routes on the same externally reachable ingress.

Each ephemeral path is generated independently on process start.

## ngrok

When `--http --ngrok` is selected:

1. the normal local HTTP listener is bound;
2. the ngrok Rust SDK opens a public HTTPS tunnel, optionally requesting `transports.ngrok_domain`;
3. dotlink verifies that a requested stable domain matches the hostname ngrok actually returned;
4. a separate Axum router and OAuth server are bound to the exact public authority/issuer/resource URL;
5. dotlink serves accepted ngrok connections directly, using authenticated tunnel peer metadata for requester admission, and prints the public MCP URL.

Public ngrok is OAuth-protected automatically, even when local HTTP OAuth is disabled. Only the explicit one-run `--allow-public-no-auth` flag suppresses that protection.

The OAuth runtime is shared across the local/public HTTP servers for the same dotlink process, but each `Server` instance binds tokens/grants to its own exact issuer/resource. This prevents a token issued for local HTTP from being replayed against the public ngrok resource or vice versa.

For durable remote connectors, both the ngrok hostname and MCP path must stay stable. An automatic ngrok hostname or `ngrok_ephemeral_url` changes OAuth identity and requires re-linking.

ngrok uses `NGROK_AUTHTOKEN` through the SDK's authtoken-from-environment flow.

## OpenAI Secure MCP Tunnel

src/transports/openai.rs owns the OpenAI-specific protocol implementation.

It converts tunnel-polled JSON-RPC requests into in-process requests against an rmcp StreamableHttpService and returns the MCP response to the OpenAI control plane. SSE is decoded incrementally: progress notifications pass through a bounded channel and reach the tunnel before the terminal tool response. The response deadline also bounds intermediate delivery.

The OpenAI transport is the only transport that requires:

~~~text
tunnel_id
runtime API key
optional organization ID
~~~

When OpenAI is disabled, setup skips these fields and no runtime key is loaded.

### Recovery lifecycle

Transient tunnel poll failures are retried with per-poll backoff. At normal log level the retry message stays concise; the underlying request error and URL are DEBUG details available with `-v`.

After 10 transient poll failures without an accepted poll, the OpenAI transport emits a typed runtime-restart request instead of retrying the same client/runtime state indefinitely. Rejection responses between transient failures do not reset this counter. The shared transport supervisor cancels every active transport and gives peer tasks up to 6 seconds to shut down cleanly before force-aborting anything still running.

The top-level runtime then reloads the selected profile, reconstructs the permission policy, cache mounts, `LocalMachine`, embedded MCP service, and all configured transports. Repeated unhealthy runtimes restart with exponential backoff capped at 30 seconds. If a runtime had connected successfully before becoming unhealthy, the restart backoff resets to its initial 1-second delay.

A rejected Runtime key or missing/rejected tunnel retries for a 60-second confirmation window before exiting with `CONFIG`/78. One 401/403/404 therefore cannot stop the service during an auth outage or edge rejection.

## Permission model

The access policy contains:

~~~text
base_dir                  # internal relative-path base
read_roots[]
write_roots[]
deny_read_roots[]
deny_write_roots[]
unrestricted_fs
~~~

Rules:

- read succeeds when a read grant covers the canonical target and no read deny covers it;
- write succeeds when a write grant covers the canonical target and no write deny covers it;
- read+write operations require both capabilities;
- allow-read, allow-write, and allow-rw are additive;
- deny-read, deny-write, and deny-rw are additive and take precedence over grants;
- legacy --deny maps to both read and write deny sets;
- deny-shell and deny-network override profile defaults and runtime allows;
- unrestricted filesystem access is represented naturally by read+write grant `/`.

The base directory is where dotlink was launched. It is readable by default unless the profile sets `default_allow=false` or the run uses `--no-default-allow`.

Bare --allow-write and --allow-rw add rw permission to the launch/base directory. Bare deny-read/deny-write/deny-rw target that same directory symmetrically.

Existing paths are canonicalized before allow checks. Create targets canonicalize their nearest existing ancestor before the final path is checked. Denies are evaluated against both the normalized lexical request path and the resolved canonical target, so a denied path cannot be reintroduced as a symlink alias to another allowed subtree.

On Linux, MCP tools dispatch concurrently by default. **Intentional design:** local
HTTP and stdio have no global tool-admission semaphore; model/client-directed
parallelism is unrestricted beyond the tool's own limits. The OpenAI tunnel
alone uses its separate transport-level in-flight ceiling. The aggregate
resource implications are accepted, not an omitted admission mechanism.
Native file I/O
uses `src/secure_fs.rs`: canonical policy authorization first, followed by
kernel-enforced `openat2` traversal from pinned allowed-directory handles
(`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, plus `RESOLVE_NO_XDEV` inside
normal grants). Reads, edits, binary patching, recursive listing, writes and
parent-directory creation then use opened handles rather than reopening
validated host paths. The unrestricted `/` grant intentionally permits
cross-mount traversal. Bubblewrap shell sandbox mounts use pinned source FDs
(`--bind-fd` / `--ro-bind-fd`) and pass them only to the forked child,
preventing a concurrent symlink swap from redirecting an authorized mount.
Runtime and network paths are bound once at their canonical path; an alias
such as `/bin -> usr/bin` becomes a sandbox symlink, so deny and protected
masks, applied to canonical objects, cover every spelling.
Read+write tools select the narrower pinned grant root, rather than relying
only on the writable ancestor, and duplicate grants reuse the same inode.
Native mutations share `src/mutation.rs`: sorted exact-path mutexes preserve
pathname/topology ordering, and Unix additionally derives a stable target key
from the nearest existing ancestor's `(dev, ino)` plus the remaining relative
file path. This makes lexical/bind aliases of one directory entry converge on
one in-process mutex without hash shards. Cross-process Unix directory locks
are coalesced by directory `(dev, ino)` rather than lexical spelling, upgrading
the deepest relevant identity to exclusive and verifying that the opened inode
still matches the plan before locking. Existing ancestors take shared intent
locks; missing-parent creation is covered by the nearest existing ancestor's
exclusive lock. An unreadable/retargeted directory fails closed. Disjoint
entries remain concurrent, and one 30-second deadline spans exact-path,
stable-target and OS-level acquisition. No workspace files, XDG coordination
state or `/run/lock` service is needed. Windows retains its conservative gate
and named cross-process mutexes. External writers and shells remain outside
advisory coordination.

The tunnel polls, OAuth metadata fetches, and local MCP responses all use the
same streaming byte-ceiling primitive (`src/bounded.rs`) before deserialization;
OAuth metadata requests additionally use trusted-requester rolling/in-flight admission, a process-wide concurrent-fetch limit, native IP validation and no proxies.
Every bounded transport ultimately enforces a serialized `CallToolResult` postcondition at the shared `LocalMachine::call_tool` boundary. Normal success paths still size/preflight their own exact semantic result before mutation or expensive encoding; the shared guard primarily bounds arbitrary diagnostics/errors and acts as a last-resort invariant for unexpected successes.

OpenAI Secure MCP Tunnel messages are bounded by the hosted 10 MiB serialized-envelope limit. dotlink caps the complete embedded MCP message at 9 MiB, reserving 1 MiB for tunnel wrapper/header metadata, while raw/tool transfer ceilings remain 8 MiB. The hosted request lifetime is a fixed 120 s and dotlink holds back 5 s for terminal delivery. Every shell call is durable from admission: the request waits only through its transport budget, then renders the ordinary result from terminal metadata plus leased spools if that complete result fits the response budget; otherwise it exposes the same stable job/task id, even when execution already finished. Execution timeouts are never rewritten.

Only -s/--serialize runs all handlers through one exclusive gate. Linux
uses openat2; macOS uses pinned-root, component-wise openat with O_NOFOLLOW.
Both permit independent native requests to run concurrently. Windows retains
its operation gate and uses named cross-process mutation mutexes deduplicated
and ordered by Windows invariant UTF-16 case-mapped identity, with thread-affine
release and automatic crash abandonment. Ancestor directory
handles are pinned against rename and reparse points refused. External
writers remain outside these advisory locks.

Native Unix writes and OAuth state transactions execute their synchronous,
descriptor-safe stages within a cancellation-safe Tokio blocking section. This
does not detach a write after its mutation guard has been dropped. Private
configuration and OAuth file replacement has one commit boundary: every byte is
serialized and bounded before staging, and post-rename directory-sync failures
are warnings, never grounds to roll back a paired key. Unix private-file readers
use nonblocking opens and reject non-regular descriptors before reading.

Developer caches are modeled separately from filesystem grants. A cache record stores its tool family, host source path, and RO/RW mode. At runtime the source is canonicalized and mounted only into the shell sandbox, typically below `/tmp/home` at the tool's expected path. Cache mounts never expand the LocalMachine read/write roots. Read/write deny rules are re-applied to canonical cache sources before mounting.

## Dynamic tool router

The implementation defines:

~~~text
discovery   ls     grep
text        read   write   edit   patch
binary      bread  bwrite  bedit
shell       bash   powershell
jobs        job_start job_list job_get job_output job_ack job_cancel
~~~

`JobManager` is the single shell execution-lifetime owner below both protocol surfaces. Admission, request-key dedupe, runner registration and shutdown share one state lock, closing create/attach teardown races. Each job has queued/running/terminal phases; under `--serialize`/Windows the runner acquires the operation gate before path/mount preparation, and the execution timeout starts only after preparation and `mark_running`. The manager owns cancellation, stable ids, task TTL state, request-id replay mapping, explicit read leases, a 64 MiB retained prefix per stdout/stderr stream, and a configurable aggregate session quota (512 MiB default). Once either ceiling omits one byte, that stream becomes permanently drain-only so the durable file remains one exact prefix even if session capacity later returns. OpenAI non-idempotent `tools/call` requests first pass through a process-lifetime replay ledger above MCP dispatch. Replay protection is declared explicitly by the central `ToolSpec.request_replay` field: one request key has exactly one execution leader, concurrent copies wait/replay, and a terminal response is serialized once into cached `Bytes`. Each `Ready`/`Uncertain` terminal POST holds a replay delivery lease so expiry cannot remove or transform that generation during the network await; positive delivery commits the outer 120-second tombstone before that lease is released or nested shell replay state is acknowledged/retired, while failed/cancelled delivery releases the lease and applies any deferred expiry. Replay keys are scoped by endpoint+tunnel+organization+request id and intentionally exclude the Runtime API key, so key rotation preserves dedupe. Replayable response bodies are additionally bound to the prepared execution-authority generation; filesystem/shell/network/cache/host capability changes, prepared object identity changes, shell timeout changes, or output/read/write/task-result ceiling changes drop cached bytes and retain only fail-closed identity. Runtime API-key, ngrok-token, OAuth owner-password, and unrelated HTTP auth/bind changes do not alter execution authority. Job-session reuse separately binds the OpenAI endpoint+tunnel+organization namespace so hidden request keys cannot cross logical tunnels. OpenAI control-plane base URLs use one runtime canonical form with trailing slashes removed; tunnel request identity, job-session namespace and management/runtime URLs all use that form, so equivalent endpoint spellings do not revoke durable jobs. Linux sandboxed hidden shell executions additionally bind the live per-command guard generation captured from the exact deny/protected set used to build that command. Cached synchronous delivery and final hidden retirement both revalidate that same generation: a later guard retarget makes cached delivery fail closed, cancellation-requests still-running hidden work, and discards terminal hidden output instead of promoting it to a compatibility handle. Each record also binds a canonical SHA-256 of the complete semantic MCP request: the JSON-RPC body plus every effective forwarded MCP protocol/routing header. Forwarded headers are normalized by the same validation path used for embedded dispatch, so dropped or empty groups cannot split replay identity. Public MCP `_meta`, MRTR fields and future request parameters participate automatically; only locally-derived `io.abird.dotlink/*` keys in the exact top-level `params._meta` object are excluded; nested lookalikes remain semantic, so one hosted request id cannot replay a response under different protocol/correlation semantics or authorize a different side effect. The ledger survives in-process runtime restarts, admits at most 65,536 identities, reserves the 10 MiB hosted worst-case before executing a side effect under a 256 MiB serialized-response budget, then shrinks completed entries to their actual terminal bytes. A full undelivered terminal body is retained for one 120-second hosted replay horizon only; expiry releases its bytes and keeps a zero-byte fail-closed identity, or `RecoverableShell` when the exact inner execution can regenerate the response, for one additional 120-second safety grace. Final expiry removes non-shell identities; shell identities enter coordinated `JobManager` retirement and first revalidate the stored live guard generation. Current authority exposes the same execution as a compatibility job; revoked authority cancellation-requests still-running hidden work and discards it once terminal. Retirement then clears the transport key and converts the outer identity to one final zero-byte `Uncertain` tombstone for another hosted replay horizon before removal, so the handoff never reopens the request. Retirement uses O(1) queue membership, handles at most 16 items per prune, and stops a batch at the first capacity-blocked handoff. Reservation accounting is incremental and response-body/delivered/retired expirations are consumed from one FIFO deadline queue rather than by rescanning the map. A logical namespace change is the only deliberate full-map operation: cached response bytes from the old endpoint/tunnel/organization are released and those identities become 120-second fail-closed retired tombstones. The larger identity ceiling exists for tiny delivered tombstones so the 120-second protection window does not become an artificial throughput cap; replayable response bytes remain governed by the tighter memory budget. An actively executing `InFlight` leader is never discarded merely by wall clock. Once a response attempt ends, `Uncertain`/`RecoverableShell` state follows the bounded two-stage safety grace above; namespace retirement still converts old-namespace state to bounded fail-closed tombstones. The hosted dispatch deadline independently bounds every replay-protected response attempt. A filesystem mutator that may have crossed commit becomes fail-closed `Uncertain` and releases the tunnel attempt; a timed-out/cancelled shell or `job_start` becomes `RecoverableShell`; it may re-enter dispatch only when the current `JobManager` still owns the exact request key. If reload/authority replacement destroyed that owner, the outer request becomes `Uncertain` and cannot execute again. OpenAI request-key shell jobs use terminal metadata plus the existing stdout/stderr spools as their exact replay source until terminal delivery is confirmed. Inner delivery explicitly distinguishes `Pending` from committed `Synchronous`/durable response state. The manager itself rejects positive acknowledgement of `Pending`, so caller mistakes cannot consume an uncommitted request key: a redelivery admission or retained-capacity error while still pending is not published as that durable request's terminal result; it does not consume the request key and the outer replay state returns to `RecoverableShell`. Failed, expired, or ambiguous delivery of a committed representation keeps the nested state, while only confirmed successful delivery of that committed representation retires the key and removes hidden state after successful cleanup (or leaves bounded `CleanupOnly` retry state). Three orthogonal pieces of state keep this lifecycle explicit: `JobDisposition::{Pending,Synchronous,Compatibility,NativeTask}` fixes the first committed wire representation; a response-attempt lease count protects only callers that are actively deciding that representation; and `RetentionClass::{Ordinary,TransportOnly,CleanupOnly}` says which capacity pool owns the entry. The first disposition commit is atomic under the manager lock and every concurrent/redelivered caller replays it. When the last active keyed request future drops while disposition is still pending, the lease transition moves the execution to `TransportOnly`: request-key dedupe, runner and spools survive, but the 256 ordinary retained-job slots are released. A surviving concurrent response attempt keeps the ordinary slot until it commits or also disappears. A dropped unkeyed local request instead commits a compatibility job so the side effect is discoverable through `job_list`. Keyed synchronous replay also lives in `TransportOnly` until positive delivery. Any still-present transport request key is exempt from generic job TTL and remains the dedupe authority until positive delivery or final coordinated transport replay retirement clears it; `request_jobs + CleanupOnly` share the 4096-entry transport-retention cap and all bytes remain under the aggregate spool quota. The first later live response attempt from `TransportOnly` atomically reacquires an ordinary slot before it can wait/render/choose a response; if capacity is unavailable the redelivery fails before handoff while the request key stays replayable. Durable exposure retains that reservation; synchronous replay returns to transport-only retention. Positive delivery removes hidden synchronous state after cleanup, but for an exposed job/task it starts a fresh full user-visible retention window independent of execution completion; native Task TTL during replay is derived so a delayed receipt cannot arrive already expired. Exposed compatibility/native jobs stay `Ordinary`. Hidden positive delivery removes metadata immediately once cleanup succeeds. `ActiveTransports` derives shared transport constraints. Private request constraints are carried through rmcp `_meta` only with a redacted opaque per-runtime capability token; OpenAI normalizes missing/null metadata into an object, rejects other types before dispatch, and clears the complete `io.abird.dotlink/*` namespace before inserting trusted fields. Public HTTP/stdio values with the same names are ignored. Result-budget authentication covers both `tools/call` and native `tasks/*` methods; Task handlers bound `McpError` diagnostics and verify completed `GetTaskResult` serialization against that ceiling. The embedded OpenAI boundary additionally rejects body routing/reflected strings above 4 KiB (`id`, method, protocol version, name/URI/taskId and schema-routed parameters) before rmcp deserialization/header validation; malformed or oversized routing headers are omitted as whole groups so rmcp produces compact missing-header errors rather than reflecting attacker-sized values. There is no duplicate full task-result file. Native MCP Tasks reconstruct the exact original shell `CallToolResult` from leased spools and are exposed only when the configured ordinary-result ceiling is worst-case safe for the minimum native-task result budget of every active transport sharing the runtime (and the current request); otherwise the same execution is exposed through the compatibility job surface. `job_output` is pure/replayable; only `job_ack` advances cleanup watermarks, and active readers defer cleanup. Fully acknowledged compatibility output is deleted and its exposed metadata becomes reclaimable under capacity pressure; native Tasks compact it to the exact ordinary-result prefix required by future `tasks/get` calls and keep only that prefix through task retention. Cleanup/compaction failures remain pending and are retried by later acknowledgements/sweeps. `job_list` recovers exposed ids after a lost receipt. Automatic recovery prepares authority once into `PreparedMachine`. The lazily materialized per-JobManager spool root is authority-bound by its random session pathname (including lexical/resolved spellings through XDG symlink parents), not by a transient missing-vs-present inode; external protected paths remain object-identity-sensitive. The prepared snapshot contains: canonical policy, resolved guards, descriptor-pinned cache mounts, pinned host resources, pinned Unix/macOS `SecureFs` grant roots, and prepared shell/Bubblewrap executables are frozen together with their object identities. Executable authority includes the canonical source path, native dev/inode/rdev on Unix and the prepared executable SHA-256; Linux runtime execution consumes the sealed bytes represented by that digest. Linux cache mounts fingerprint the pinned directory FD and Bubblewrap clones that same FD for each command; guard validation also requires the cache source pathname to still canonicalize to the prepared path/inode before launch. That frozen snapshot is fingerprinted and then consumed directly by `LocalMachine`; construction performs no second authority traversal. Descriptor-backed cache/host/root identity comes from one fallible native file-identity primitive; inability to identify an already-pinned capability aborts preparation instead of degrading the fingerprint to nullable fields. Windows path identities use volume/file index. Reuse occurs only after a successful snapshot proves equality; preparation failure, identity change, manual reload or normal exit cancels+awaits the old session. New manager intent/policy binding is performed unconditionally in release builds, with debug assertions only checking the result. `Drop` remains only a fallback.

The visible router is policy-specific.

Default:

~~~text
ls
grep
read
bread
~~~

Write tools are hidden if no write capability exists.

`edit`, `patch`, and `bedit` need an overlapping read+write grant.

Shell is hidden unless explicitly enabled.

Only bash is exposed on Unix and only powershell on Windows. Executable discovery selects a runnable regular file rather than merely the first matching pathname: Unix candidates are checked with the process's effective `X_OK` decision, covering ownership classes, ACLs and mount execution policy, so an unusable earlier PATH entry cannot shadow a later usable shell or Bubblewrap binary. Runtime preparation converts the resolved shell and Bubblewrap into `PreparedExecutable` capabilities and fingerprints their native identities. On Linux the exact pinned source object—not an earlier pathname lookup—must pass the effective `X_OK` check before its bytes are copied into a sealed executable `memfd`; modern kernels are requested with `MFD_EXEC | MFD_ALLOW_SEALING | MFD_CLOEXEC`, pre-`MFD_EXEC` kernels use the compatible legacy flags, and a hardened kernel that prohibits executable memfds fails startup with an operator-facing diagnostic. Bubblewrap executes that immutable snapshot through `/proc/self/fd/<fd>` from an empty inherited host environment; the sealed shell bytes are materialized at the canonical sandbox path with `--perms 0500 --ro-bind-data`, avoiding anonymous-memfd mount-source path resolution. Bubblewrap's exact pinned launcher object is rejected whenever it is owned or writable by the dotlink uid; lexical parent-directory ownership is not part of launcher trust because Linux execution consumes the sealed snapshot rather than reopening that pathname. The system-managed native launcher's host loader/package closure is trusted host configuration, so sandboxed root-RW grants remain compatible with immutable package-store launchers. Setuid/setgid or Linux file-capability launchers and `#!` wrappers are also rejected because sealed byte snapshots intentionally drop those host execution semantics. Ordinary shell wrappers remain supported by child-only descriptor inheritance. Preparation computes authority without executing a newly discovered Bubblewrap; capability probing occurs only after a new authority is safe to activate, after any incompatible old job session has been revoked. Non-Linux launchers revalidate their prepared identity and digest immediately before pathname spawn. Bubblewrap preparation/probing failures are operator configuration errors.

Shell stdin is fed concurrently after stdout/stderr drains start. A child may intentionally close its stdin without consuming all supplied bytes; `BrokenPipe` therefore ends input delivery but does not fail or terminate an otherwise valid command. Other stdin I/O failures remain execution failures, and the child's exit status remains authoritative after an early close.

## Linux Bubblewrap mapping

On Linux, enabled shell runs inside Bubblewrap unless `--unrestricted` (or the legacy `--allow-all --no-sandbox` pair) was explicitly selected. Fine-grained broad grants may contain dotlink's own protected objects: `validate_shell_guards` returns a shallow-to-deep `GuardAnchor` plan for every mutable ancestor between a user bind and an existing canonical guarded leaf. Bubblewrap self-binds those pinned ancestors with their effective RO/RW mode; the leaf mask/read-only rebind becomes the final mountpoint. This prevents sandbox-child rename/unlink while preserving ordinary writes below writable anchors. Nested descendants collapse under a guarded directory. Missing names and symlink spellings remain fail-closed when overlapping a live broad bind.

Effective filesystem grants become mounts:

- readable only -> read-only bind;
- read+write -> writable bind;
- write-only -> not mounted into shell.

More-specific mounts may override broader mounts.

Read-denied paths are masked after allow mounts. Write-denied paths inside otherwise writable trees are rebound read-only, preserving reads while preventing writes.

The saved OpenAI runtime credential is masked when present.

Bubblewrap provides an empty temporary home and temporary directory. Approved developer caches are then mounted selectively into that private home, for example host `~/.cargo/registry` -> sandbox `/tmp/home/.cargo/registry`; tool-specific cache environment variables are set when useful.

On NixOS, the sandbox also preserves the standard Nix executable/profile graph read-only when those paths exist:

~~~text
/nix/store
/run/current-system
/etc/profiles
/nix/var/nix/profiles
~/.nix-profile
~~~

Only the ~/.nix-profile entry is exposed; the user's home directory itself is not mounted for this purpose. Runtime sources are typed: fixed system entries may canonicalize normally, while the user-controlled `~/.nix-profile` alias carries a target policy requiring its final canonical target to stay below `/nix/store`. An escaping profile alias is rejected before Bubblewrap construction. PATH entries are canonicalized and filtered to sandbox-visible locations, so valid profile symlinks resolve to their mounted /nix/store targets.

The Nix daemon socket is intentionally not mounted by default: exposing it would let a sandboxed command ask the host daemon to perform work outside the shell's direct filesystem/network namespace.

## Opt-in developer service mounts

`src/host_services.rs` discovers supported host integrations but never grants
access implicitly. With explicit profile/CLI consent, it pins the Nix daemon
or SSH-agent socket and selected Nix/Git config files at runtime startup.
`src/mcp/tools/shell.rs` reuses its Bubblewrap FD-binding mechanism to expose only the
individual socket/file inside the private shell home (or the Nix daemon's
conventional socket location). Host integration FDs remain CLOEXEC in the
parent and are passed only to the sandbox startup child. SSH-agent
discovery checks `DOTLINK_SSH_AGENT_SOCKET`, valid `SSH_AUTH_SOCK`,
`$XDG_RUNTIME_DIR/gcr/ssh`, and the effective UID's `/run/user/<uid>/gcr/ssh`,
in that order. The explicit override never silently falls back, while an
expired session variable can use GCR. A changed host agent socket requires a
dotlink restart, rather than redirecting a live capability.
SSH signing needs read-only Unix account lookup files (`/etc/passwd`,
`/etc/group`, and optionally `/etc/nsswitch.conf`) so OpenSSH can resolve
the invoking user without accessing private SSH keys.
Caches remain independent shell-only mounts; their explicit `--allow-cache-*`
flags use the same discovery as onboarding, skip families whose directory is
missing (warning rather than failing startup), and require explicit `=rw` for
write authority. None expands native MCP filesystem permissions.

## Opt-in hardware device mounts

`src/hardware.rs` enumerates `/dev/kvm`, DRM render/card nodes filtered by
sysfs PCI vendor ID, AMD KFD, and explicitly named NVIDIA kernel devices.
The same pinned `HostMount` representation supports both read-only directory
and character-device mounts. Unlike ordinary read-only mounts, a hardware
mount uses Bubblewrap `--dev-bind /proc/self/fd/<pinned-fd> <target>`; this
avoids `MS_NODEV` and allows the sandboxed command to open the device while
preventing a mutable host pathname from redirecting the device source. The
FD stays `CLOEXEC` in dotlink and becomes inherited only by the Bubblewrap
startup child. GPU driver libraries/ICDs, selected NVIDIA proc metadata, and
sysfs use read-only directory binds. Hardware is independent of
`--allow-network` and ordinary filesystem grants, and is never added to
native MCP filesystem permissions.

The NixOS module emits hardware flags and per-service `SupplementaryGroups`.
The GPU drivers and compute/toolchain packages themselves are managed by
NixOS and the user project's Nix environment, not by dotlink.

## Network isolation

Sandboxed shell starts with an unshared network namespace.

--allow-network restores host network access for the shell child.

This policy applies only to the shell child.

The main dotlink process may still need outbound network access for:

- OpenAI Secure MCP Tunnel;
- ngrok SDK ingress.

## Unsandboxed shell

An unsandboxed shell cannot be constrained by Rust path checks, so dotlink exposes one preferred single-flag full-host mode:

~~~text
--unrestricted
~~~

This grants filesystem read+write `/`, shell, and network with no shell sandbox. The legacy `--allow-all --no-sandbox` pair remains equivalent. Filesystem/network deny rules are rejected in this mode because they cannot constrain an arbitrary unsandboxed child process.

For unrestricted filesystem access while keeping Linux Bubblewrap, use the normal path model instead:

~~~text
--allow-rw=/
~~~

Then add --allow-shell and/or --allow-network separately.

## Logging model

Logging has two layers:

- normal activity logging is emitted centrally around the MCP tool router, so OpenAI, stdio, and HTTP produce the same TOOL lines: complete `p`/`d` path names before escaped `c` for `bash`/`powershell`, with `b="N"`, `max_t`, and `max_b`. Paths inside launch cwd appear as `./...`, outside as absolute `/...` on Unix. Commands can contain inline secrets; default logging is quiet;
- TOOL logging uses one shared argument renderer: `c=` defaults to at most 256 visible characters (including quotes and escapes), with `…` before the closing quote when truncated. Only interactive `V` **while TOOL logging is active** toggles the full escaped command; quiet/REQ-only mode hides the hint, ignores the key, and resets full mode. `p=`, `d=`, and all valid `edit` path names remain full; over-limit requests retain bounded operation/file counts. Interactive color formatting styles keys and values as structured elements rather than parsing user-supplied text, and highlights outcomes. Noninteractive argument summaries stay plain. Other payloads stay byte counts.
- REQ logging records incoming request metadata at the transport boundary without dumping request bodies; all scopes are ASCII-sanitized/capped, HTTP method/path labels are bounded and control-character sanitized, ephemeral MCP paths are redacted, OpenAI command metadata uses strict protocol identifiers, and grep patterns are logged only as byte counts. Control-plane error bodies and malformed SSE payloads are excluded from propagated errors. rmcp tracing is suppressed independently of `RUST_LOG` because its debug events render complete protocol payloads.

Default activity logging is quiet. `-v` enables TOOL logs; `-vv` also enables REQ diagnostics. In interactive non-stdio runs, `v` cycles `quiet → TOOL → TOOL + REQ → quiet` immediately across all logger clones. `-q/--quiet` suppresses TOOL logs. `--color=auto|always|never` controls ANSI rendering; auto follows whether stderr is interactive.

## Build and release architecture

Nix builds use Crane. Each target has a separate dependency artifact derivation created with buildDepsOnly; final package/test/lint derivations import those Cargo artifacts instead of rebuilding dependencies after source-only changes.

Native outputs expose:

~~~text
deps
dotlink
~~~

On x86_64-linux, the release graph cross-builds every published binary:

~~~text
Linux x86_64   x86_64-unknown-linux-musl
Linux ARM64    aarch64-unknown-linux-musl
Windows x86_64 x86_64-pc-windows-gnu
Windows ARM64  aarch64-pc-windows-gnullvm
macOS ARM64    aarch64-apple-darwin
~~~

Each target exposes `cross-*-deps`, `cross-*`, and `dist-*` outputs. Linux is published only as generic static musl `dist-linux-*` artifacts; no distro-specific aliases are needed. `release-all` aggregates every published binary, checksum sidecar, `VERSION`, and `PLATFORMS.txt` into one derivation.

Windows ARM64 uses nixpkgs' pinned LLVM-MinGW/UCRT toolchain. macOS ARM64 uses the hashed Apple SDK 14.4 fetch derivation directly with Linux-hosted clang/ld64.lld and a macOS 11.0 deployment target, avoiding a host-specific Darwin build step.

Dist outputs use stable filenames plus SHA-256 sidecars so CI and local releases consume identical Nix results. `scripts/build-release-artifacts.sh` and the GitHub workflow both delegate target logic to `release-all`. `install.sh` selects Linux x86_64/ARM64 or macOS ARM64 automatically; `install.ps1` selects Windows x86_64/ARM64. The canonical upstream repository is `https://github.com/abird-ai/dotlink`; installers default to that repository while still allowing `DOTLINK_REPO` or a custom release base URL for forks and mirrors.

## Text tools

- `read` streams line by line and returns a window with `N<TAB>` numbering by
  default and one `… truncated at line N` marker when the window is capped.
  It does not preload the whole file, so a small window works on a file larger
  than `max_read_bytes`; that ceiling bounds the returned window and an
  individual line, and a window that exceeds the byte ceiling stops at the last
  line that fits. The reply — truncation marker included — stays inside the
  ceiling: making room for the marker drops whole lines, never a line's tail,
  from the single output buffer that holds the page. Lines before `offset` are
  skipped by scanning for their newline, so reaching a window never buffers or
  decodes what precedes it and a skipped line may exceed the ceiling. A line
  inside the window that alone exceeds the ceiling is an error naming it: the
  page never silently drops content it was asked for.
  With `numbered: false` the window is returned byte for byte, so line endings
  and a trailing newline survive a read → write round trip. An offset past the
  end of the file is an error, never a silently empty answer.
- `edit` takes an ordered `edits[]` array across up to 64 files and 512 total operations, requires each
  `old_text` to be unique (or `replace_all`), and re-hashes every snapshot
  before the first commit: a mismatch detected there writes nothing, while each
  file replacement remains atomic and the commit sequence is not a transaction.
- `patch` applies Codex `*** Begin Patch` / `*** Update File` / `*** Add File`
  sections. Hunks are located by content, an ambiguous match fails, a path
  claimed by two sections (including two spellings through a symlink) is refused
  while planning, added lines inherit the target file's line ending, and the
  Codex `*** End of File` marker is enforced during context candidate selection:
  only matches ending at the last real line are eligible for exact/trimmed search,
  and no hunk may follow it in the same file. An addition-only hunk uses that
  marker as an explicit EOF append anchor; without source context or the marker,
  an insertion is ambiguous and rejected. Strict
  mode preflights every section before any commit and `best_effort` writes the
  clean ones; the shared mutation engine stages all eligible files before any
  commit and bounds the receipt before writes. Each file replacement is atomic,
  but the commit sequence is not a multi-file transaction.
- `ls`, `grep`, `read`, `bread`, `write`, `bwrite`, `edit`, `patch` and `bedit`
  operate only inside canonicalized grants; denies and protected paths are
  dropped before patterns, sorts, or limits apply.
- Directory enumeration is descriptor-rooted on Linux: the walk enumerates the
  pinned FD through procfs and fails closed with an explicit error when procfs is
  unavailable, rather than reopening the directory by pathname.
- Text tools share UTF-8/binary-control classification and keep binary
  payloads in `bread`/`bwrite`/`bedit`. `grep` caps a call at 32 MiB
  of file reads and 4096 candidates, independently of model concurrency.
- `grep` walks with the same descriptor-rooted enumerator as `ls`; regex, glob
  and `.gitignore` matching are pure matchers over paths and buffers already
  held, `.gitignore` files are read through the pinned directory handle, and
  candidate file contents are re-opened one at a time through the
  descriptor-rooted reader beneath the pinned grant root. Symlink entries are
  listed by `ls` and never traversed or searched by `grep`.
- `ls` and `grep` cap their JSON replies at `max_output_bytes` as well as their
  entry/hit limits. The cap is applied by keeping the longest prefix whose
  rendered JSON fits, found with one binary search over the prefix length, so a
  pathologically escaped reply costs `O(log n)` renders instead of one render per
  dropped entry. `grep output_mode=count` reports `page_total` (the sum of the
  returned counts) so a paginated reply cannot be mistaken for a global total.
  `grep content` pages over matching lines; context is clipped at the adjacent
  matching lines outside the page so a later page does not repeat a hit as
  context, the selected batch shrinks to the largest hit prefix that fits the
  remaining byte budget, and the byte budget reserves room for the truncation
  marker before a hit is accepted, so a page always ends with whole hit lines.
- `ls` derives its default depth from the pattern: unbounded for `**`, one level
  per path segment otherwise, because `*` and `?` never cross `/`. A deep pattern
  therefore cannot silently return an empty listing.

## Tool layout

The machine and its policy live in `src/mcp.rs`; the tools live one family per
module under `src/mcp/tools/`, each with its own `#[tool_router]`:

~~~text
src/mcp.rs              policy, pinned descriptors, capability table, ServerHandler
src/staging.rs          staging mode/naming policy + cancellation-safe path backend
src/mutation.rs         exact + stable Unix target locks; cross-process directory locks
src/secure_fs.rs        Linux descriptor-rooted IO and the fd-based staging backend
src/mcp/tools/mod.rs    walk, glob grammar, limits, markers, snapshot commit
src/mcp/tools/{ls,grep,text,edit,patch,binary,shell}.rs
~~~

Two shared rules keep the surface composed rather than copied:

- `TOOL_SPECS` in `src/mcp.rs` is the only record of a tool's capability and
  concurrency class; visibility, the platform operation gate, `--list-tools` and
  the TOOL log all read it. A new tool is one module, one router, one line.
- An existing-file text mutation is plan → verify → stage → commit.
  `Snapshot::verify` re-reads the file and produces the only `Verified` token
  that can stage an update, so an update cannot skip the re-check. Creates
  (`write`, `bwrite`, `*** Add File`) have no prior content and stage directly.
  Staging flushes the bytes to a sibling temporary; committing renames it over
  the target, or uses a no-overwrite hard link / Windows move for creates. Tools that
  write several files stage all of them before committing any.

Writes never truncate in place: `write`, `bwrite`, `edit`, `patch` and `bedit`
all go through `SecureFs::stage`/`stage_update`/`stage_create`/`StagedWrite::commit`,
which writes a sibling temporary beneath the pinned parent directory and
installs it atomically. `src/staging.rs` owns the rules both backends share — the
temporary starts at the target's mode (never broader, so a private file is never
briefly world-readable), and its name is recognized by the walk, which hides
in-flight staging files from `ls`/`grep`. Updates carry the snapshot length and digest and re-check them
immediately before commit; that is a best-effort external-writer check, because
the final check and the rename are not one atomic compare-and-rename. Existing
permission bits are preserved, while Linux ownership, extended attributes and
hard links are not. Non-Linux uses the same staged shape under the legacy
operation gate — the same mode and naming policy, implemented by the path
backend in `src/staging.rs` — preserving the file mode and rejecting symlink or
special-file targets by pathname checks; it does not have Linux's
descriptor-rooted compare-before-rename guarantee.

`ls` and `grep` collect at most `MAX_LIST_ENTRIES` / `MAX_GREP_FILES` matches
before sorting, so on a tree larger than that bound the result is a correct but
arbitrary subset of the collected matches, not a global path-order prefix or
top-N. Those bounds limit collected results, not traversal: a pattern or filter
that matches nothing can still walk the tree the caller named.

## Binary MCP content

bread(format=mcp) maps bytes to MCP typed content:

- image MIME -> image content;
- audio MIME -> audio content;
- other MIME -> embedded blob resource.

base64 and hex modes return encoded text.

bwrite decodes base64 or hex.

bedit applies many byte ranges against original-file offsets, rejects overlaps
and out-of-bounds ranges, applies them in a deterministic order (offset, then
span so an insert precedes a replacement at the same offset, then request
position), supports an optional `sha256` precondition, and caps both the source
and the result at `min(max_write_bytes, 64 MiB)`; unlike multi-file text
plans, binary updates stream source and output in fixed-size buffers, so the
full 64 MiB file ceiling does not require a 128 MiB working set.
