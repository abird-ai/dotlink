# Security model

dotlink is least-privilege by default.

Without write or shell flags, the visible tools are:

~~~text
ls
grep
read
bread
~~~

and the launch directory is the only implicit readable directory. `--no-default-allow` removes even that grant.

## Service and file-backed credentials

The NixOS module runs dotlink as the explicitly selected Unix user (non-root strongly recommended), retains that user's ordinary XDG credential discovery by default, and requires an explicit working directory so the dynamic `.` read grant never silently expands to a surprise directory. Optional systemd `LoadCredential` runtime/ngrok key files are not embedded in generated Nix settings, systemd arguments, or the journal. Their paths are protected from MCP file access, and credential variables are removed from every child process; systemd notification variables never leave dotlink. The systemd-255-or-newer unit restarts all process terminations except statuses 2 and 78 and the non-failure `NOTCONFIGURED` (6), which leaves an unconfigured unit inactive, but systemd stop/restart jobs remain authoritative and suppress automatic restart. The selected Unix user's own filesystem permissions remain a separate boundary; an intentionally unrestricted, unsandboxed shell running as the same user must not be considered isolated from that user's private credentials. Journald tool logging may expose filesystem paths and command excerpts and should be enabled only when desired. See `.agents/docs/nixos-service.md`.

## Profiles and permission precedence

Profiles persist `default_allow`, path grants/denies, shell/network policy, transport/HTTP/ngrok defaults, and local-HTTP OAuth settings. Named profiles use `config.<profile>.jsonc`; the default uses `config.jsonc`. Schema v10 is the current profile format. Known compatible older schemas are migrated forward in memory (v9 → v10 is additive); newer schemas and older schemas without an explicit migration path are rejected rather than guessed. A migrated profile is written as the current schema the next time it is edited or saved.

Runtime allow rules are additive. Denies always take precedence over profile defaults, ordinary allows, and developer-cache grants.

Filesystem controls are symmetric:

~~~text
--allow-read[=DIR]    --deny-read[=DIR]
--allow-write[=DIR]   --deny-write[=DIR]
--allow-rw[=DIR]      --deny-rw[=DIR]
~~~

Bare forms target the launch directory. Bare --allow-write means read+write on that directory. Legacy --deny=PATH is equivalent to denying both read and write.

Capability controls are also symmetric:

~~~text
--allow-shell      --deny-shell
--allow-network    --deny-network
~~~

`--allow-rw=/` is unrestricted filesystem read+write; no separate all-rw flag exists.

Native tools still check the lexical request (including denies) and the
canonical target before access. On Linux, native IO then opens paths relative
to pinned authorized directory descriptors using `openat2` with beneath,
no-symlink and (except for explicit root grants) no-cross-mount constraints.
Direct file reads/writes/edits/patches use those opened handles rather than
reopening a pathname. Search holds paths, not one descriptor per candidate, and
re-opens each candidate through the same descriptor-rooted reader beneath the
pinned grant root, one file at a time. Edit and patch operations that require
both read and write access use the narrower pinned grant root, and identical
read/write grants share one pinned directory identity. Recursive enumeration is
anchored to a pinned directory handle and never follows symlink entries.
Filesystem operations fail closed if Linux does not support `openat2` or if a
path cannot satisfy those constraints.
Directory handles are capabilities, including if an unrelated process later
moves the directory inode; this is not a snapshot or a filesystem transaction.
Enumeration of a pinned directory goes through `/proc/self/fd/<n>`; if procfs is
not mounted, `ls`/`grep` fail with an explicit error instead of reopening the
directory by pathname, because a pathname reopen could be redirected by a
concurrent rename outside the grant.
Linux Bubblewrap mount sources for granted directories and developer caches
are pinned with file descriptors rather than reopened by pathname; CLOEXEC
is cleared for those FDs only in the forked child, avoiding cross-request FD
inheritance.

On Linux and macOS, pinned-descriptor native filesystem operations execute
in parallel by default. The -s/--serialize option explicitly schedules calls
one at a time; it is not an access-control boundary. Windows keeps an
in-process gate and uses named kernel mutexes to coordinate mutations across
processes. Parallel shells retain the configured Bubblewrap restrictions (or the explicitly
unsandboxed host-user authority), and simultaneous writes to the same file
are not protected from application-level lost updates. Existing write/edit
targets must be regular files; special files are rejected.

Tool requirements:

- ls, grep, read, bread require read;
- write, bwrite require write;
- edit, patch, bedit require read+write.

Search never reaches shell-only grants (cache mounts, Nix runtime paths): `grep`
walks with filesystem read authority only, through the same descriptor-rooted
enumerator as `ls`. Authorization is decided once, for the search root; descent
and `.gitignore` reads travel through pinned directory handles. Candidate file
contents are re-opened through the descriptor-rooted reader beneath the pinned
grant root, one file at a time, so a large tree cannot exhaust descriptors. A
directory renamed inside the grant after the walk can change which inode a
candidate path names; it cannot leave the grant. Symlink entries are listed by
`ls` and never traversed or searched by `grep`. Glob, regex and `.gitignore`
matching are pure matchers over already-authorized paths and bounded buffers.
Search bounds are explicit: 1 MiB per file, 65536 candidate files, 50 matches
per file per `content` page, 200 hits or files, 1024-character patterns, regex
size/DFA limits of 1 MiB, 8 context lines per side, 64 KiB / 1024 lines per
`.gitignore` file with invalid patterns failing closed, and a rendered-output
budget that stops collection at `max_output_bytes` in every mode. `content`
pages select matching lines and clip context at adjacent matching lines outside
the page, so a later page does not repeat a hit as context. A `content` page
reserves room for its truncation marker before accepting a hit, so it ends with
whole hit lines only; a page that stops on the per-file cap says so in the
marker, and an offset past the last hit answers with an explicit marker. The candidate bound
limits collected results; the shared walker visits at most 100,000 directory
entries per request. On a partial walk, `scan_incomplete` distinguishes sorted
results from a globally exhaustive search; narrow path/glob before paging.

Native filesystem mutators (`write`, `bwrite`, `edit`, `patch`, `bedit`) take a
bounded per-path mutation lock before reading a snapshot and hold it through
commit, so two dotlink calls cannot both verify the same bytes and then
overwrite one another. Multi-file edits acquire their shard locks in sorted
order.

Cross-process coordination uses hierarchical advisory `flock`. Each existing
ancestor directory receives a shared intention lock; the deepest existing
parent receives an exclusive lock for snapshot → commit. If a parent is not yet
present, the nearest existing ancestor is exclusive until the directories and
file are created. A concurrent writer must first lock that ancestor, so it
cannot bypass a creator by discovering a newly created directory. A directory
that exists but cannot be opened fails closed, never substituting a different
lock identity. A rename does not change the directory inode. No workspace
lockfile, XDG state, or `/run/lock` capability is required. Locks are ordered
root-to-leaf, wait at most 30 seconds and release automatically on exit. The
256 in-process shards retain concurrency between independent files. Writers in
different directories remain concurrent across processes unless they need to
change a common ancestor. Windows retains its conservative in-process gate.

The locks are advisory, and network filesystems or independent mount
namespaces require additional coordination. A shell command or another program
that ignores the protocol can still race after the final snapshot check.
Shell commands are outside native coordination because their write paths are
arbitrary. Text tools (`edit`, `patch`, `bedit`) plan against an in-memory
snapshot and re-hash every affected file before the first write, so an external
concurrent change detected in that window fails the call with nothing written.
An update can only be staged through the `Verified` token produced by that
re-check, so the check cannot be skipped. The write itself is atomic: the new
bytes go to a sibling temporary file beneath the same pinned parent directory
and are installed at commit, so a reader sees the whole old file or the whole
new one, a failing or interrupted write leaves the original intact, and an
existing file keeps its permission bits. The temporary is created with a mode
no broader than the target's from the first syscall — replacing a `0600` file
never creates a group- or world-readable intermediate — and a new file keeps
ordinary create semantics (0666 minus the umask). Staging temporaries are named
`.dotlink-<pid>-<16 hex>.tmp` and are hidden from `ls`/`grep`, so a listing or a
search never reports a half-installed file. The target and its bytes are re-checked
immediately before the commit, so a name swapped for a symlink or special file,
or a file changed by an external writer after verification, is refused. This is
a best-effort check, not an atomic compare-and-rename: an external writer that
does not participate in Dotlink's lock protocol can still race between the
final check and rename. A multi-file call is not a transaction: a commit-stage failure can leave earlier files replaced, and the
error names that consequence. Ownership,
extended attributes and hard links are not preserved across the Linux rename.
Data is synced before rename. Parent-directory `fsync` is best effort because
the directory can be writable without being readable; Dotlink guarantees
atomic visibility, not power-loss durability. A failed post-commit sync is
logged, not reported as a failed write that a client might replay.
On Windows, staging reads the original target's DACL and applies it to the
still-empty temporary before writing any bytes; an existing target is installed
with `ReplaceFileW` using an explicit sibling backup and no ACL-merge ignore
flags. The original is restored from backup when Windows moved it before
reporting failure. When recovery cannot be proven safe, the backup and staged
files are retained for manual restoration, never blindly deleted. Successful
replacements remove the backup where possible. The non-Linux staging backend
owns the temporary file from creation through asynchronous write/sync: aborts
and cancellations trigger RAII cleanup rather than leaving a half-written file
behind. Windows native writes preserve an existing file's ACL. Separately, dotlink's
own configuration, keys, and OAuth state use a protected DACL granting access
only to the current user, SYSTEM and Administrators, even under a custom
configuration directory. Windows ACL behaviour still requires native tests.
`write`, `bwrite` and `*** Add File` create content without a prior snapshot.
A create-only install refuses any name that already exists: Linux uses a
descriptor-rooted hard link, Windows an atomic `MoveFileW` without replacement,
and macOS a hard link. All leave the target absent until commit. The temporary has a `.dotlink-*.tmp` name
beneath the target directory and is excluded from Dotlink `ls`/`grep`
during the write.

## Opt-in host developer services

Linux Bubblewrap can additionally expose the Nix daemon socket, Nix config,
Git global/XDG config, and the SSH-agent Unix socket through four independent
opt-in permissions. These are **shell-only** and do not expand native MCP
filesystem grants. Config files and sockets are bound read-only by pinned
file descriptors. Private SSH key files are not exposed. An explicitly
configured agent socket must be valid and is never silently replaced by a
different agent. A missing/stale inherited `SSH_AUTH_SOCK` can fall back to
the effective user's GCR socket only when the SSH-agent capability was granted.
Replacing a pinned agent socket requires restarting dotlink. Sockets and
devices grant operations rather than data, so a read-only bind still connects
or issues ioctls: deny-write covering a socket or device refuses the
integration just as deny-read does.

Nix daemon access allows the sandbox to ask the privileged host daemon to
fetch/build outside the sandbox's network/filesystem isolation. This permission
is independent of `--allow-network` and requires explicit consent; ordinary
Nix store paths remain read-only. `NIX_REMOTE=daemon` is set in the sandbox
when the capability is requested. The user's Nix configuration may contain
access tokens, and Git config may contain credentials or executable helpers;
each is shared only when explicitly enabled and should be audited before use.

The SSH agent socket grants **all signing/authentication actions supported by
keys loaded into that agent**, not only Git commit signing. Use an isolated
signing agent or confirmation-constrained keys for sensitive identities.
Dotlink never mounts `~/.ssh` implicitly. When SSH-agent integration is enabled,
OpenSSH also receives minimal read-only Unix account lookup files:
`/etc/passwd`, `/etc/group`, and `/etc/nsswitch.conf` if available. These
contain account metadata, not private keys. Systemd system services generally
lack interactive `SSH_AUTH_SOCK` and `XDG_RUNTIME_DIR`, but dotlink can
discover GCR through `/run/user/<effective-uid>/gcr/ssh` when it is running.
A socket that has not yet appeared at service startup still results in an
actionable error; dotlink does not start GCR itself.

## Opt-in KVM and GPU device access

Linux hardware capabilities are separately granted using `--allow-kvm` or
repeatable `--allow-gpu=amd|nvidia` (or matching profile/NixOS options). Both
are **off by default**; ordinary filesystem or network permissions never
implicitly grant device access. Hardware is shell-only and requires Bubblewrap.
Device access is not an ordinary read-only file permission: it allows ioctls
and DMA operations through host kernel drivers. Only explicitly selected
character devices are passed using Bubblewrap `--dev-bind` with descriptors
pinned by Linux `openat2`, preserving both the device-accessible mount mode
and protection against pathname/symlink swaps. The Linux Unix user/group
permission model remains authoritative.

AMD DRM render/card devices must match the kernel-reported PCI vendor ID
`0x1002`; NVIDIA DRM devices must match `0x10de`. AMD may also receive
`/dev/kfd`, and NVIDIA may receive the narrowly named `/dev/nvidia*` and
`/dev/nvidia-caps/nvidia-cap*` kernel nodes. Intel or unknown DRM devices
are not exposed. A nonexistent requested device causes startup to fail
rather than enabling broader `/dev` access. Driver metadata and runtime
locations are mounted read-only only with the corresponding GPU grant, which
includes a read-only sysfs view required for PCI/ROCm enumeration.

For the NixOS system service, the module adds `SupplementaryGroups` only to
the affected unit (`kvm`, `render`, `video`). This does not mutate the user's
persistent login group membership, and cannot override further restrictions
from systemd's device controller or the host driver. QEMU VM networking or
Wayland/X11 forwarding remain separate permissions.

## Developer cache sharing

When shell is enabled, onboarding can autodiscover existing package/build caches and ask whether each cache should be shared with the sandbox as none, read-only, or read+write.

Supported cache families currently include Cargo registry/git, npm, pnpm, Yarn, pip, uv, Go module/build caches, Maven, Gradle, sccache, and ccache.

Cache grants are **shell-only**. They do not add the host cache path to the MCP filesystem allow-list, so `read`, `write`, `ls`, and related MCP tools cannot use cache sharing to inspect the user's home directory.

dotlink maps approved host caches into the sandbox's private home at the package manager's expected location. Only the cache directory itself is mounted; adjacent credential/config files such as `~/.cargo/credentials.toml`, `~/.cargo/config.toml`, or `~/.npmrc` are not included.

Read-only sharing protects host cache integrity but cannot populate cache misses. Read+write sharing provides the normal package-manager experience and lets successful downloads/builds be reused later, but sandboxed code can also modify those shared host cache contents. Choose RW only when that tradeoff is acceptable.

Filesystem deny rules remain authoritative: deny-read removes a matching cache mount and deny-write downgrades a matching RW cache to read-only. In the shell sandbox a deny-write root is rebound read-only from a pinned descriptor whether it names a directory or a single file; a file deny is never passed to the directory-mount path.

## Transport security

### stdio

stdio is local subprocess IPC over stdin/stdout.

**No global local admission budget, by design:** direct HTTP and stdio MCP
requests retain model/client-directed concurrency. The tunnel has a separate
per-transport in-flight ceiling. This is an accepted operator trade-off, not
an unimplemented safety feature. Individual tool file/output/timeout bounds
still apply.

When --stdio is active, stdout is protocol-only. Status and diagnostics are written to stderr. Runtime key capture (`v` / `Ctrl+R`) is disabled whenever stdio is active, so dotlink never competes for protocol stdin. If stdio is launched manually on a TTY, dotlink temporarily ensures `Ctrl+C` is a real interrupt signal and restores the exact original terminal state when it exits.

### local HTTP

HTTP defaults to loopback:

~~~text
127.0.0.1:3000
~~~

Loopback HTTP is unauthenticated by default. Any **non-loopback** HTTP listener is treated as public ingress and is refused unless it is OAuth-protected with an explicit HTTPS `--public-url`, or the operator deliberately supplies the one-run `--allow-public-no-auth` escape hatch.

`--oauth` (or persisted `oauth.enabled`) protects local/reverse-proxied HTTP with dotlink's embedded single-owner OAuth server. For a reverse proxy, `--public-url=https://...` is the canonical external origin. dotlink never derives OAuth issuer/resource identity from Host, Forwarded, or X-Forwarded-* headers.

Local OAuth and public-ngrok OAuth are intentionally independent. `--no-oauth` disables only local/reverse-proxied HTTP OAuth; it does not weaken an active public ngrok endpoint.

### Ephemeral URL paths

--ephemeral-url gives both local HTTP and ngrok fresh high-entropy MCP paths for the current process.

--http-ephemeral-url and --ngrok-ephemeral-url control the two transports independently and override the shorthand. Explicit =false is supported.

When a transport uses an ephemeral path, its normal /mcp route is not exposed by that transport.

The generated path is approximately 244 bits of randomness and is intended to make accidental discovery difficult. It is not a replacement for authentication or access control.

### ngrok

`--ngrok` creates a public HTTPS endpoint for the configured HTTP MCP server. Public ngrok ingress is **OAuth-protected by default**, independently of whether local HTTP OAuth is enabled.

The ngrok SDK credential is read from `NGROK_AUTHTOKEN`. It authenticates dotlink to ngrok; caller authentication is provided separately by dotlink OAuth.

For durable OAuth, configure a reserved/stable ngrok hostname with `--ngrok-domain=<DOMAIN>` (or setup) and keep the stable `/mcp` path. OAuth access/refresh grants are bound to the exact issuer and resource URL, so changing the ngrok hostname or using an ephemeral MCP path requires the remote client to reconnect.

`--allow-public-no-auth` is the only runtime escape hatch for externally reachable HTTP without OAuth, whether that exposure is ngrok or a direct non-loopback bind. It is intentionally one-run-only and should be treated as full exposure of every MCP capability granted to that process.

The authenticated ngrok tunnel and local TCP listener use separate routers, so local and public MCP path/auth policies cannot accidentally share a route. Ngrok connections are served directly rather than through a loopback forwarding backend. One outer allowlist protects MCP and every OAuth route after routing is merged. Ngrok trusts only its exact public tunnel authority; direct/reverse-proxied HTTP trusts only exact loopback, bound-IP and configured-public-origin authorities, including port semantics, and ignores forwarded headers. Malformed, missing, duplicate, or conflicting `Host` and HTTP/2/absolute-form URI authorities receive HTTP 400; well-formed non-allowlisted authorities receive HTTP 403. An omitted public port remains valid across an HTTPS edge and plaintext backend. IPv4-mapped IPv6 addresses are normalized before wildcard/loopback classification. Host never determines the OAuth issuer or resource. An unauthenticated direct listener must bind a concrete interface IP (not `0.0.0.0`/`::` or mapped equivalents), so its Host allowlist can accept IP connections without permitting arbitrary DNS-rebinding hostnames.

### Embedded OAuth

dotlink's HTTP OAuth mode is intentionally single-owner rather than a general identity provider.

Anonymous-work admission uses a trusted network requester boundary, never a caller-supplied client ID, redirect URI, hostname, or HTTP forwarding header. Direct HTTP uses the immediate TCP peer; built-in ngrok uses the remote address authenticated by its tunnel protocol. Source ports are discarded, IPv4-mapped addresses normalize to IPv4, IPv6 groups by /48, and loopback is one local identity. This identity is memory-only abuse accounting, not authentication or token binding, and its `Debug` representation is redacted so framework tracing cannot disclose source addresses. NAT users and generic reverse proxies share an allowance; operators of other proxies should enforce distributed abuse controls at that edge. Multiple real source networks can still exhaust finite public capacity, as with any anonymous endpoint, but one requester cannot rotate self-declared OAuth values to do so.

Protocol/security properties:

- OAuth authorization-code flow with mandatory PKCE S256;
- Protected Resource Metadata and OAuth Authorization Server Metadata discovery;
- exact `resource` binding on authorization, token exchange, refresh, and bearer validation;
- RFC 9207 `iss` on authorization responses;
- CIMD client metadata with HTTPS-only fetches, redirects disabled, public-IP/DNS checks, response-size limits, and DNS pinning to reduce SSRF/rebinding risk. Nonpublic literal IPs are rejected before admission. Each trusted requester gets 12 starts per rolling minute and at most two in flight, with four concurrent fetches process-wide. The 256-row requester table never evicts unexpired history: unseen requesters receive 429 while it is full, while known rows keep their own progress. DNS uses a cancellable async system resolver; timeout/request cancellation drops the lookup and releases the complete admission lease immediately;
- DCR fallback for clients that still require dynamic registration;
- HTTPS redirect URIs, with loopback HTTP allowed for native/local callbacks;
- short-lived opaque access tokens kept only in memory;
- rotating opaque refresh tokens persisted only as SHA-256 hashes; token issuance uses bounded concurrent admission and performs durable grant changes without holding the in-memory access-token validation mutex;
- one-time authorization codes held only in memory;
- owner password persisted only as an Argon2id hash, with each live authorization resolving the current committed profile revision and rechecking it after successful Argon2 verification; a deleted saved profile cannot reactivate an older legacy hash. Verification allows six rolling-minute starts per trusted requester under a two-check process-wide concurrency bound; the consent creator and password POST source are both charged, deduplicated when equal, and the 256-row requester history does not evict active limits;
- bounded pending registrations and owner authorizations reject excess requests with HTTP 429 rather than evicting another live request; every redirect URI/client share, a four-entry per-requester share within each redirect/client group, and an eight-entry overall trusted-requester share are enforced, so caller-chosen identifiers cannot let one requester consume either 256-entry store or target all capacity for another requester. In-flight owner consent retains a claim until completion or cancellation, so retry and one-time redemption are both safe. Authorization codes, access tokens, and refresh grants prune expired entries but never evict live entries for new issuance; access tokens enforce a 64-entry per-client share within the 512-entry process bound, and overload leaves the original authorization code or refresh grant retryable. Approved DCR clients retain their own limit;
- Argon2 verification offloaded to blocking workers that **own** their two-check concurrency permits through completion, including after the HTTP request is cancelled; failed attempts are delayed without a remotely triggerable global lockout;
- OAuth refresh grants and authorization codes are checked for correct client/resource/PKCE bindings before consumption; code redemption uses a short exclusive claim rather than retaining a mutex across durable I/O, so duplicates shed without consuming global admission and temporary storage failure releases the claim for retry instead of burning the code. An authoritative post-issuance check finding a revoked DCR client consumes the now-terminal code;
- cross-process locked OAuth state mutations so a live server, setup, and `dotlink oauth` management command cannot lose each other's approved-client/refresh/password changes;
- refresh tokens issued only when the approved client advertises the `refresh_token` grant; `offline_access` requires that capability;
- no JWT signing key or external auth service.

Unauthenticated DCR registrations are bounded and memory-only. Client display names are length-bounded and neutralize terminal controls, line separators, and Unicode bidi controls before owner UI or management output. A DCR client is persisted only after the owner successfully approves it. Approved DCR clients and hashed refresh grants live in the profile-specific OAuth state file.

OAuth state lives under the Abird XDG state namespace:

~~~text
$XDG_STATE_HOME/abird/dotlink/oauth.json
$XDG_STATE_HOME/abird/dotlink/oauth.<profile>.json
~~~

with the usual `~/.local/state/abird/dotlink` fallback on Unix; an empty or relative `XDG_STATE_HOME` is ignored. On Unix the directory is mode 0700 and state files are mode 0600. The state directory is part of dotlink's protected control-plane paths, so MCP filesystem tools and the normal sandboxed shell cannot read or modify it.

`dotlink oauth status/clients/revoke/revoke-all` provides the management surface. Revoking a refresh grant prevents future refresh immediately. Access tokens are short-lived/in-memory; restart dotlink (or `Ctrl+R`) when immediate invalidation of all outstanding access tokens is required.

The embedded authorization page uses no external scripts/assets and sends restrictive CSP, frame, referrer, cache, and content-type headers. It shows the client ID, redirect URI, resource, and requested scope before owner approval.

## OpenAI credentials

OpenAI credentials exist only when the OpenAI transport is enabled.

The Runtime key is stored separately for each profile:

~~~text
~/.config/abird/dotlink/runtime.key
~/.config/abird/dotlink/runtime.work.key
~~~

Canonical JSONC config files are `config.jsonc` or `config.<profile>.jsonc`. On Unix dotlink-owned config/key files are mode 0600 and the owned `~/.config/abird/dotlink` directory is mode 0700. `DOTLINK_CONFIG` may place JSONC elsewhere without chmod'ing its parent; Runtime keys still remain in the owned XDG directory.

The Admin key used to create a tunnel is never persisted. A profile with OpenAI disabled does not require a runtime key.

Known OpenAI/tunnel credential environment variables and NGROK_AUTHTOKEN are removed from child shell environments. Active file-supplied Runtime/ngrok credentials are opened once; the descriptor identity must match the resolved protected target, and the captured value is carried into transport startup without a second pathname read. Protected control-plane paths and deny rules retain both their normalized lexical name and resolved target, including missing targets behind dangling symlinks; resolution follows each symlink before a later `..`, as the kernel does. Unlinking or retargeting a configured alias therefore never frees its name, and every sandboxed command re-resolves aliases and masks what they name at that moment as well. Sandboxed shells reject any protected or denied path inside a writable bind unless it lies below an existing, non-symlink deny that is the bind's direct child: only such a mount point cannot be renamed or unlinked, so an ancestor cannot be moved around a path mask and the denied name recreated. Cache and host remapping cannot bypass protected or denied roots. Runtime and network paths are bound read-only by pinned descriptor at their canonical location, where masks apply; an alias such as `/bin -> usr/bin` becomes a sandbox symlink rather than a second, unmasked bind. Runtime mounts are masked even without a matching filesystem grant. A policy the sandbox cannot enforce exits `CONFIG`/78. Dormant credential-file variables are not opened.

## Automatic transport recovery

Transient OpenAI tunnel failures trigger a full runtime restart after 10 failures without an accepted poll; intervening 401/403/404 responses do not falsely reset that health counter. Those rejection statuses retry for 60 seconds before requiring operator action, so one auth outage or edge rejection cannot stop a supervised service. The window spans in-process runtime restarts and resets only on an accepted poll or a changed endpoint, tunnel, key or organization, so interleaved 5xx restarts cannot postpone it indefinitely. Recovery does not grant new authority: dotlink reloads the same selected profile and reapplies the same CLI permission/deny flags before reconstructing `LocalMachine`, cache mounts, MCP state, and transports.

All peer transports are cancelled during recovery. They receive up to 6 seconds to stop cleanly before remaining tasks are force-aborted. Repeated unhealthy runtimes use bounded restart backoff, while a persistent authentication/tunnel rejection exits with `CONFIG`/78.

Recovery must never bypass path canonicalization, deny precedence, protected credential paths, Bubblewrap policy, or shell-network policy.

## Linux Bubblewrap shell

--allow-shell enables Bash on Unix.

On Linux it is Bubblewrap-sandboxed by default.

The sandbox:

- mounts readable grants read-only;
- mounts effective read+write grants read-write;
- does not expose purely write-only host grants;
- masks read-denied paths and rebinds write-denied readable subtrees read-only;
- masks dotlink control-plane state: the active JSONC config, owned Runtime-key directory, and OAuth state directory;
- uses an empty temporary home, with only explicitly approved developer caches mounted back into package-manager-specific subdirectories;
- mounts required system runtime paths read-only;
- on NixOS, mounts the standard Nix store/profile symlink graph read-only (/nix/store, /run/current-system, /etc/profiles, /nix/var/nix/profiles, and ~/.nix-profile when present) without mounting the whole home directory;
- canonicalizes and filters PATH entries so only sandbox-visible tool directories remain;
- isolates PID, IPC, and UTS namespaces;
- unshares the network namespace by default.

The Nix daemon endpoint namespace is deliberately hidden **unless** `--allow-nix-daemon` is explicitly enabled, independently of `--allow-network` and even if broad filesystem grants include the socket directory. Dotlink masks the daemon endpoint roots (`/nix/var/nix/daemon-socket` and `/run/nix-daemon`) after all other sandbox mounts so later grants cannot reopen them. This avoids unexpectedly delegating work to the host daemon outside the shell's direct filesystem/network isolation.

Enable shell network access with:

~~~text
--allow-network
~~~

The shell network namespace is separate from the main dotlink process. OpenAI and ngrok can use outbound networking even while the shell itself has no network.

## Full unsandboxed host mode

The only unsandboxed shell mode is:

~~~text
--allow-all
--no-sandbox
~~~

The flags require each other and grant root filesystem read+write, shell, and network with no Bubblewrap isolation. Deny rules are not accepted in this mode because an arbitrary unsandboxed child process cannot be constrained by Rust path checks.

For unrestricted filesystem access while keeping Linux sandboxing, use `--allow-rw=/` and add shell/network capabilities separately.

## Windows and macOS

Rust filesystem tools enforce allow/deny policy on Windows and macOS.
On macOS the pinned-root Unix backend uses a component-wise openat/O_NOFOLLOW
walk where Linux openat2 is unavailable. Directory enumeration stays
rooted in the descriptor too: macOS `/dev/fd` nodes are character devices, so
entries come from `fdopendir` on a duplicate of the pinned directory and are
buffered within the walk's own entry ceiling. Windows pins ancestor directories
against rename, refuses reparse points during file opens, and coordinates
same-path writers across processes with a named Global kernel mutex. The
acquiring thread releases the mutex when its guard drops; a process crash
abandons it automatically. Failure to pin a directory or open the mutex fails
closed. Native Windows/macOS behavior still requires target-host validation.

Tool replies name paths with `/` on every platform, so one request produces one
reply on Linux, macOS and Windows. The host spelling is still what the runtime
resolves internally.

Bubblewrap is Linux-only. Native Windows/macOS shell execution therefore requires `--allow-all --no-sandbox`.

For Windows, WSL2 is the recommended secure shell workflow: run dotlink inside WSL2 and use the normal Linux Bubblewrap sandbox instead of enabling unrestricted native PowerShell.

## Binary data

bread supports MCP typed media/blob content plus explicit base64 and hex.

bwrite and bedit enforce `max_write_bytes`; bedit caps binary planning at `min(max_write_bytes, 64 MiB)`.

bedit applies at most 32 ranges at original-file offsets, rejects overlaps and
out-of-bounds ranges, caps total file size processed in memory, and can require a
`sha256` precondition. The precondition and the snapshot are re-checked against
fresh bytes immediately before the atomic write.

## Terminal state safety

Interactive runtime controls snapshot the terminal state before enabling raw key input. On Unix, dotlink preserves the original output flags while controls are active and restores the complete captured `termios` state on teardown. The guard is RAII-owned by the runtime-control object, so normal exit, `Ctrl+R`, transport failures, `?` error propagation, and unwinding all restore the terminal before returning control to the shell. stdio never enables runtime key capture.

## External OAuth metadata and tunnel limits

Anonymous OAuth metadata lookup is limited to four concurrent fetches and 12
starts per rolling minute for each trusted requester; there is no anonymously
exhaustible global rolling-minute quota. Cancellable async system DNS has a
3-second timeout; timeout/request cancellation drops the lookup and releases
its complete admission lease immediately. Missing system DNS configuration
disables CIMD fetches without preventing registered-client OAuth from starting.
The HTTPS request has a 5-second
timeout and a 64 KiB response ceiling.
Proxy environment settings are disabled for the metadata HTTP client, so DNS
pinning cannot silently be bypassed by a proxy resolving the target itself.
Only public resolved addresses are allowed, and redirects are disabled.

OpenAI tunnel polls use a streaming ceiling of at least 64 MiB, raised to
fit the configured maximum single JSON request plus command-wrapper headroom.
The explicit safety maximum is 256 MiB; oversized write configurations fail at
tunnel startup and can use local HTTP/stdio instead. Poll command count remains
bounded independently. The shared byte-ceiling primitive applies before
deserialization rather than after buffering a response.

## Logging privacy

At `-v`, TOOL logs show metadata such as paths, directories, flags, status, latency, and timestamps. Arguments that carry a payload are reduced to a byte count — `content`, `stdin`, `data`, `old_text`, `new_text` and `patch` — so file contents, patches and piped input never reach the journal.

`bash` and `powershell` commands also count as sensitive payloads: even a short
command may embed a credential, so TOOL and REQ logs record `command_bytes`
instead of an excerpt. Time, tool, working directory, outcome and latency
remain available for operational auditing; the MCP client retains the command
transcript for replay. Journald does not become a second, uncontrolled secret
store.

At `-vv` (or after cycling to TOOL + REQ with `v`), request-level transport/protocol metadata is added. HTTP logs bounded, control-character-sanitized method/path labels (with ephemeral MCP paths redacted) and status; stdio logs JSON-RPC method names; OpenAI Tunnel logs strict bounded protocol labels. Generic log scopes are ASCII-sanitized and capped, and grep patterns join commands/content/stdin as byte counts rather than literal text. OpenAI control-plane response bodies and malformed local SSE payloads are not copied into errors. rmcp tracing targets are always suppressed—even when `RUST_LOG` requests them—because upstream debug events render complete MCP request/response payloads. Raw request bodies, file contents, binary payloads, runtime keys, and other secrets are not intentionally emitted by the dotlink request logger.

`--quiet` suppresses TOOL activity. With `-vv`, it leaves REQ diagnostics visible while hiding TOOL lines.

## Resource limits

- tunnel concurrency is bounded; SSE events are decoded incrementally and forwarded through a bounded notification queue while the tool is still running;
- shell stdout/stderr are drained continuously with bounded retained output;
- shell stdin is written concurrently with process execution after the output drains start, so a child that writes before reading its input cannot deadlock both pipes;
- `max_output_bytes`, `max_read_bytes` and `max_write_bytes` are hard ceilings: a value below the supported minimum is rejected at configuration time, never silently widened at startup;
- the shared traversal boundary records unreadable descendant directories and metadata errors (including races with deletions), exposes `traversal_error` to `ls` and `grep`, and distinguishes intentionally filtered entries; grep also marks individually unreadable discovered files. JSON keeps compatibility `incomplete_reason` and adds `incomplete_reasons` so simultaneous causes remain visible;
- UTF-8 text tools share binary-control rejection, and grep skips non-text candidates; byte-oriented files use bread/bwrite/bedit; grep caps a request at 32 MiB content I/O and 4096 candidate files, reporting incomplete search coverage; optional gitignore work is separately capped at 4 MiB and 256 non-absent file reads, charging bytes returned before failed reads and rejecting any per-file overflow of 64 KiB or 1024 lines instead of silently discarding rules; unreadable or policy-denied ignore files mark discovery incomplete (`ignore_rules`) without reading denied bytes;
- text/binary reads and writes are bounded; `read` streams a line window instead of preloading the whole file, builds the page in one output buffer, and its reply — truncation marker included — fits `max_read_bytes`; a source-level limiter ensures buffered prefetch cannot exceed the separate 32 MiB skipped-line I/O budget, while preserving already-prefetched selected-window bytes; a preceding line larger than the output ceiling still works within that budget;
- the streamable-HTTP request-body ceiling is derived from the profile, not from the transport's default: a request body may be six times `max_write_bytes` (the worst-case JSON escaping of a payload the tools accept) plus 64 KiB of framing, for the local HTTP server and the OpenAI tunnel alike;
- local MCP response bodies are bounded during streaming according to the configured `max_read_bytes` and `max_output_bytes`: ordinary text gets sixfold JSON-escape headroom, while shell reserves sevenfold nested-JSON headroom for each independently capped stdout/stderr stream, plus framing; unlike a fixed 16 MiB tunnel cap, larger configured limits can be returned consistently via HTTP, stdio, and the embedded tunnel;
- shell progress heartbeats are best-effort with a 100 ms delivery budget clipped to the command's absolute deadline, never an unbounded wait or delayed timeout inside the child-process supervision loop;
- shell calls use one absolute timeout covering stdin delivery, process execution, and stdout/stderr collection (including inherited pipes from descendants), chosen per call by the client, bounded by the tunnel client's own `response_timeout` window when present, and clamped to the profile `max_shell_timeout_secs` ceiling; a timeout still returns a bounded, non-secret error result with any partial output;
- protected and denied paths beneath writable shell mounts need an immutable deny boundary (see above), so a missing one cannot be created by the command and needs no host-side placeholder. Existing paths are masked at their canonical object by inaccessible tmpfs or /dev/null;
- shell cancellation and timeout abort request-owned pipe tasks and terminate the associated Unix process group or Windows Job Object; Windows creates the process suspended until it joins the Job Object. Unsandboxed Unix descendants that call `setsid` can escape process-group cleanup by design; Bubblewrap's PID namespace provides the stronger default boundary. Successful commands can deliberately start background work;
- HTTP and transport lifecycles share cancellation; outer middleware drops every accepted MCP/OAuth route future (including body extraction) during shutdown, force-close-aware accepted IO wakes detached Axum connection tasks at the drain bound, and the ngrok HTTP supervisor owns the local listener and direct tunnel server tasks, explicitly closes the public tunnel and parent session with bounded RPCs concurrent with connection draining, treats a drain timeout as cleanup failure because synchronous handler work cannot be preempted, gives cleanup failures precedence over restart requests, and aborts tasks on unexpected termination;
- OAuth request bodies/client metadata, pending DCR registrations, authorization requests/codes, access tokens, refresh grants, and approved DCR clients are all explicitly bounded; SEP-2243 HTTP/OpenAI tool calls preflight unknown names against the fixed policy router before rmcp's schema cache, with the network preflight enforcing the same exact request-body ceiling; public revocation rejects malformed tokens before disk I/O and admits at most two concurrent calls without an anonymous minute quota that could deny legitimate revocation; public token exchange admits at most four concurrent calls; the shared read-only state index also caches approved client metadata, using the separate namespaces of DCR IDs and CIMD URLs to avoid touching private state for arbitrary public URL requests. Definite misses are rejected, contention/version races fall through to authoritative work, and a stable malformed snapshot returns retryable 503 without repeated parsing. OAuth state (16 MiB), saved profile JSONC (4 MiB), and Runtime/explicit key files (16 KiB) share symmetric read/write ceilings, with serialization checked before replacement and Unix FIFOs/devices rejected through nonblocking descriptor validation; on Windows the OAuth state index uses native volume/file identity to recognize atomic replacements; public responses never echo private filesystem error details; pending owner authorizations use one bounded store that restores claimed requests after transient internal errors, while expired/full DCR promotions return actionable terminal errors; access-token poison recovery is consistent across publication, validation and revocation;
- recovery teardown waits at most 6 seconds before aborting unresponsive peer transport tasks;
- the directly served ngrok tunnel and all accepted HTTP connections terminate with the HTTP transport/process lifecycle.

## Recommended use

Read-only local MCP:

~~~bash
dotlink --stdio
~~~

Writable local project:

~~~bash
dotlink --stdio --allow-write
~~~

Sandboxed build/test shell without shell network:

~~~bash
dotlink --stdio --allow-write --allow-shell
~~~

Local HTTP:

~~~bash
dotlink --http
~~~

Public HTTP through ngrok (OAuth-protected automatically):

~~~bash
dotlink --http --ngrok --ngrok-domain=my-dotlink.ngrok.app
~~~

Use a stable hostname and stable `/mcp` path for a durable remote OAuth connection. Use `--allow-public-no-auth` only when intentionally exposing the granted MCP surface without caller authentication.

Use --allow-all --no-sandbox only when unrestricted host access is explicitly intended.
