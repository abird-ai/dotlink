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

No transport owns filesystem policy.

## NixOS service

The flake exports `nixosModules.default` (`nixosModules.dotlink`). The thin module in `nix/module.nix` runs the unchanged binary through systemd and delegates authority/OAuth to the normal runtime. With only `user` and `workingDirectory` configured, it intentionally sets no `DOTLINK_CONFIG` or credential overrides: startup reads that Unix user's saved XDG profile/key/OAuth state. Optional declarative `settings` generate a read-only schema-v10 JSON file in the Nix store, while optional `LoadCredential` files override only the relevant secret. Named instances create independent systemd units with named-profile defaults. Service mode always suppresses stdio MCP, uses stderr/journald logging, and handles SIGTERM with normal bounded transport cancellation. See `.agents/docs/nixos-service.md`.

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

OAuth secrets/grants are deliberately not stored in JSONC. Profile-specific OAuth state lives under the Abird XDG state namespace (`$XDG_STATE_HOME/abird/dotlink/oauth[.<profile>].json`), while OpenAI Runtime keys remain under the Abird XDG config namespace. Both are protected control-plane paths.

The process launch directory is the internal relative-path base. It is readable by default when `permissions.default_allow=true`; `--no-default-allow` removes that implicit grant for one run. Relative profile/CLI paths resolve from the launch directory. `allow_rw: ["."]` means rw on the launch directory; `allow_rw: ["/"]` is unrestricted filesystem read+write.

allow_shell enables the normal platform shell. allow_network controls network access for the normal sandboxed shell path. On Linux Bubblewrap still applies unless --allow-all --no-sandbox is explicitly selected.

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

Interactive non-stdio runs also start a terminal-control loop: `v` mutates shared logging verbosity live, `Ctrl+R` cancels/drains the active transports and rebuilds the runtime from the current profile/CLI state, and `Ctrl+C` exits. stdio disables key capture so protocol stdin remains exclusive.

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
request-body ceiling: it is derived from `max_write_bytes` (six times that
payload, for worst-case JSON escaping, plus 64 KiB of framing). rmcp's 4 MiB
default would otherwise refuse a write the tools accept.

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
- an Argon2id owner-password hash with bounded `spawn_blocking` verification;
- cross-process file locking around OAuth state read/modify/write operations so setup, a live server, and management commands compose without lost updates.

Unauthenticated DCR registrations stay bounded/in-memory until successful owner consent; only approved clients are persisted.

## Independent ephemeral HTTP routes

The HTTP transport resolves two route policies:

~~~text
http_ephemeral_url
ngrok_ephemeral_url
~~~

--ephemeral-url sets both to true by default.

--http-ephemeral-url and --ngrok-ephemeral-url are per-transport overrides and accept explicit =false.

Local HTTP and ngrok are genuinely isolated: ngrok uses a second loopback-only MCP backend listener. This lets one side expose `/mcp` while the other uses `/mcp/<random-token>` without accidentally mounting both routes on the same externally reachable listener.

Each ephemeral path is generated independently on process start.

## ngrok

When `--http --ngrok` is selected:

1. the normal local HTTP listener is bound;
2. a second loopback-only backend listener is created for ngrok;
3. the ngrok Rust SDK opens a public HTTPS endpoint, optionally requesting `transports.ngrok_domain`;
4. dotlink verifies that a requested stable domain matches the hostname ngrok actually returned;
5. a separate OAuth server instance is bound to the exact public issuer/resource URL;
6. ngrok forwards to its private backend and dotlink prints the public MCP URL.

Public ngrok is OAuth-protected automatically, even when local HTTP OAuth is disabled. Only the explicit one-run `--allow-public-no-auth` flag suppresses that protection.

The OAuth runtime is shared across the local/public HTTP servers for the same dotlink process, but each `Server` instance binds tokens/grants to its own exact issuer/resource. This prevents a token issued for local HTTP from being replayed against the public ngrok resource or vice versa.

For durable remote connectors, both the ngrok hostname and MCP path must stay stable. An automatic ngrok hostname or `ngrok_ephemeral_url` changes OAuth identity and requires re-linking.

ngrok uses `NGROK_AUTHTOKEN` through the SDK's authtoken-from-environment flow.

## OpenAI Secure MCP Tunnel

src/transports/openai.rs owns the OpenAI-specific protocol implementation.

It converts tunnel-polled JSON-RPC requests into in-process requests against an rmcp StreamableHttpService and returns the MCP response to the OpenAI control plane.

The OpenAI transport is the only transport that requires:

~~~text
tunnel_id
runtime API key
optional organization ID
~~~

When OpenAI is disabled, setup skips these fields and no runtime key is loaded.

### Recovery lifecycle

Transient tunnel poll failures are retried with per-poll backoff. At normal log level the retry message stays concise; the underlying request error and URL are DEBUG details available with `-v`.

After 10 consecutive transient poll failures, the OpenAI transport emits a typed runtime-restart request instead of retrying the same client/runtime state indefinitely. The shared transport supervisor cancels every active transport and gives peer tasks up to 5 seconds to shut down cleanly before force-aborting anything still running.

The top-level runtime then reloads the selected profile, reconstructs the permission policy, cache mounts, `LocalMachine`, embedded MCP service, and all configured transports. Repeated unhealthy runtimes restart with exponential backoff capped at 30 seconds. If a runtime had connected successfully before becoming unhealthy, the restart backoff resets to its initial 1-second delay.

Fatal control-plane failures, such as invalid credentials or a missing tunnel outside activation grace, remain fatal and do not enter this recovery loop.

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

On Linux, MCP tools dispatch concurrently by default. Native file I/O
uses `src/secure_fs.rs`: canonical policy authorization first, followed by
kernel-enforced `openat2` traversal from pinned allowed-directory handles
(`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, plus `RESOLVE_NO_XDEV` inside
normal grants). Reads, edits, binary patching, recursive listing, writes and
parent-directory creation then use opened handles rather than reopening
validated host paths. The unrestricted `/` grant intentionally permits
cross-mount traversal. Bubblewrap shell sandbox mounts use pinned source FDs
(`--bind-fd` / `--ro-bind-fd`) and pass them only to the forked child,
preventing a concurrent symlink swap from redirecting an authorized mount.
Read+write tools select the narrower pinned grant root, rather than relying
only on the writable ancestor, and duplicate grants reuse the same inode.
Native mutations share `src/mutation.rs`: 256 in-process shards followed by
sorted hierarchical directory locks. Existing ancestors take shared intent
locks; the deepest existing parent takes an exclusive lock across snapshot →
commit. Missing parent creation is covered by that ancestor's exclusive lock,
so another instance cannot bypass it by opening a newly created subdirectory.
An unreadable directory fails closed rather than falling back to a different
inode. Disjoint subdirectories remain concurrent across independent processes,
while changes within a single directory serialize. Lock descriptors survive
file replacement and release on task cancellation or process exit. Lock waits
are bounded to 30 seconds; no workspace files, XDG coordination state or
`/run/lock` service is needed. Windows retains a conservative in-process gate.
External writers and shells remain outside advisory coordination.

The tunnel polls, OAuth metadata fetches, and local MCP responses all use the
same streaming byte-ceiling primitive (`src/bounded.rs`) before deserialization;
OAuth metadata requests additionally use per-process admission and no proxies.

Only the explicitly requested `-s/--serialize` runs all tool handlers through
one exclusive gate. The non-Linux implementations retain the previous
shared-read/exclusive-mutator gate by default while platform-specific
descriptor-relative operations remain unimplemented. Serialization is not a
security requirement on Linux and the per-path locks are not an OS-level
transaction or cross-process coordinator; external writers are outside their
scope.

Developer caches are modeled separately from filesystem grants. A cache record stores its tool family, host source path, and RO/RW mode. At runtime the source is canonicalized and mounted only into the shell sandbox, typically below `/tmp/home` at the tool's expected path. Cache mounts never expand the LocalMachine read/write roots. Read/write deny rules are re-applied to canonical cache sources before mounting.

## Dynamic tool router

The implementation defines:

~~~text
discovery   ls     grep
text        read   write   edit   patch
binary      bread  bwrite  bedit
shell       bash   powershell
~~~

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

Only bash is exposed on Unix and only powershell on Windows.

## Linux Bubblewrap mapping

On Linux, enabled shell runs inside Bubblewrap unless --allow-all --no-sandbox was explicitly selected.

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

Only the ~/.nix-profile entry is exposed; the user's home directory itself is not mounted for this purpose. PATH entries are canonicalized and filtered to sandbox-visible locations, so profile symlinks resolve to their mounted /nix/store targets.

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

An unsandboxed shell cannot be constrained by Rust path checks, so dotlink exposes a single explicit full-host mode:

~~~text
--allow-all
--no-sandbox
~~~

The two flags require each other. This grants filesystem read+write `/`, shell, and network with no shell sandbox. Filesystem/network deny rules are rejected in this mode because they cannot constrain an arbitrary unsandboxed child process.

For unrestricted filesystem access while keeping Linux Bubblewrap, use the normal path model instead:

~~~text
--allow-rw=/
~~~

Then add --allow-shell and/or --allow-network separately.

## Logging model

Logging has two layers:

- normal activity logging is emitted centrally around the MCP tool router, so OpenAI, stdio, and HTTP all produce the same timestamped TOOL start/completion lines;
- REQ logging records incoming request metadata at the transport boundary without dumping request bodies.

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
- `edit` takes an ordered `edits[]` array across up to 16 files, requires each
  `old_text` to be unique (or `replace_all`), and re-hashes every snapshot
  before the first commit: a mismatch detected there writes nothing, while each
  file replacement remains atomic and the commit sequence is not a transaction.
- `patch` applies Codex `*** Begin Patch` / `*** Update File` / `*** Add File`
  sections. Hunks are located by content, an ambiguous match fails, a path
  claimed by two sections (including two spellings through a symlink) is refused
  while planning, added lines inherit the target file's line ending, and the
  Codex `*** End of File` marker is enforced: the hunk must end at the last line,
  and no hunk may follow it in the same file. Strict
  mode preflights every section before any commit and `best_effort` writes the
  clean ones; each file replacement is atomic, but the commit sequence is not a
  multi-file transaction.
- `ls`, `grep`, `read`, `bread`, `write`, `bwrite`, `edit`, `patch` and `bedit`
  operate only inside canonicalized grants; denies and protected paths are
  dropped before patterns, sorts, or limits apply.
- Directory enumeration is descriptor-rooted on Linux: the walk enumerates the
  pinned FD through procfs and fails closed with an explicit error when procfs is
  unavailable, rather than reopening the directory by pathname.
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
src/mutation.rs         process shards + stable cross-process mutation locks
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
and the result at `min(max_write_bytes, 64 MiB)`.
