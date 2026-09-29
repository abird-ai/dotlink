# Security, sandbox and permission specification

## Security boundary

dotlink is a local authority broker.

It should assume:

- remote/model input may be untrusted;
- project files may be adversarial;
- shell commands may be adversarial;
- public HTTP clients may be unauthenticated unless OAuth enforces entry;
- the user explicitly controls what local authority is granted.

It does **not** attempt to defend against an unrelated hostile process already executing as the same OS user outside dotlink.

## Security layers

```text
remote caller authentication
        ↓
transport/OAuth boundary
        ↓
dynamic MCP tool surface
        ↓
LocalMachine path/capability policy
        ↓
filesystem operations / optional shell
        ↓
Bubblewrap on Linux
```

Do not merge these layers conceptually.

## Default authority

```text
read launch dir     yes
write               no
shell               no
network             no
extra paths         no
caches              no
public ingress      no
unsandboxed shell   no
```

## Filesystem policy

Core state:

```text
base_dir
read_roots[]
write_roots[]
deny_read_roots[]
deny_write_roots[]
unrestricted_fs
```

`base_dir` is internal relative-path resolution, not independent authority.

## Allow rules

Read grant covers canonical targets below the read root.

Write grant covers canonical targets below the write root.

Unrestricted filesystem is represented naturally by:

```text
read /
write /
```

rather than a special filesystem mode.

## Deny rules

Denies override grants.

Final review invariant:

> deny checks apply to both the normalized lexical request path and the canonical target.

Why both:

- canonical target blocks ordinary symlink escape;
- lexical deny prevents a denied namespace from being replaced later with a symlink to some allowed target.

Example:

```text
deny: project/secrets
later: project/secrets -> project/public
```

A request through `project/secrets/...` remains denied.

## Parent traversal

Client paths containing `..` are rejected.

Callers should use canonical explicit paths instead.

## Existing path resolution

Flow:

1. normalize lexical target;
2. apply lexical denies;
3. canonicalize;
4. apply canonical denies;
5. apply canonical grants;
6. protected-control-plane check.

## Create path resolution

Flow:

1. normalize lexical target;
2. apply lexical denies;
3. if target exists, canonicalize and check normally;
4. otherwise walk to nearest existing ancestor;
5. canonicalize ancestor;
6. reconstruct final candidate;
7. apply policy.

## Operation gate

Filesystem path checks and actual host operations are not intrinsically atomic across arbitrary symlink mutations.

dotlink therefore prevents **its own tools** from racing one another.

One shared `RwLock` per `LocalMachine`.

Shared:

```text
ls
read
read_binary
```

Exclusive:

```text
write
edit
write_binary
patch_binary
bash
powershell
future/unknown tools by default
```

This prevents a concurrent dotlink shell/mutator from changing path topology between another dotlink operation's authorization and use.

Future tool rule:

> treat a new tool as exclusive until it has been explicitly reviewed as a pure read.

## File type safety

Text/binary write:

- target may be missing;
- existing target must be a regular file;
- special files are rejected.

Edit/patch:

- existing regular file required.

Read/read_binary already require regular files where appropriate.

Reason:

- avoid blocking on FIFO/socket/device;
- avoid surprising special-file behavior;
- keep tool semantics simple and predictable.

## Protected control-plane paths

MCP filesystem tools cannot access dotlink-owned control state.

Protected:

- active JSONC config;
- Runtime-key directory/file;
- OAuth state directory.

Standard config layout lets dotlink protect the whole owned Abird config directory.

Custom `DOTLINK_CONFIG` is protected file-by-file while Runtime keys remain in the owned config namespace.

OAuth state remains in the owned XDG state namespace.

## Dynamic MCP router

Tool discovery reflects capability.

Default read-only:

```text
ls
read
read_binary
```

Write-only can expose write destinations without implying read.

Edit/patch require actual overlapping read+write.

Shell hidden unless enabled.

This is security and usability behavior.

## Shell model

### Linux

Bubblewrap sandbox required by default.

Sandbox:

- fresh PID/IPC/UTS namespace;
- fresh proc;
- controlled dev;
- private tmpfs;
- private `HOME=/tmp/home`;
- network namespace isolated unless allowed;
- policy-derived filesystem mounts.

### Mount mapping

Read-only grant:

```text
RO bind
```

Read+write grant:

```text
RW bind
```

Write-only grant:

```text
not mounted into shell
```

This is intentional because a shell cannot practically be given true write-without-read semantics using ordinary bind mounts.

### Denies in Bubblewrap

deny-read:

- hide/mask subtree.

deny-write:

- downgrade writable region to RO when read remains allowed.

Mask type:

- directory → inaccessible tmpfs/mode;
- file/socket → safe masking bind.

## NixOS support

When present, expose the runtime/profile graph read-only rather than home.

Typical mounts:

```text
/nix/store
/run/current-system
/etc/profiles
/nix/var/nix/profiles
~/.nix-profile
```

Canonicalize executable/PATH entries.

Do not mount whole home.

## Nix daemon

When network is denied, hide daemon endpoint namespaces such as:

```text
/nix/var/nix/daemon-socket
/run/nix-daemon
```

Reason:

- daemon may bypass intended network/isolation;
- filesystem shape can vary;
- mask the namespace, not a guessed socket leaf.

Do not expose daemon by default.

If a future explicit Nix-daemon capability is added, it must be opt-in and independently reviewed.

## Shell network

Default off.

```text
--allow-network
--deny-network
```

Network capability requires shell.

For normal Linux sandbox:

- `--allow-network` removes the isolated-network restriction.

For unsandboxed full-host mode:

- network is naturally available;
- this mode requires the paired full-host flags.

## Full host escape hatch

Only:

```bash
dotlink --allow-all --no-sandbox
```

Each flag requires the other.

Do not add quieter dangerous aliases.

## macOS/Windows

No Bubblewrap-equivalent shell sandbox is implemented.

Current invariant:

- filesystem MCP tools retain Rust allow/deny enforcement;
- shell is not exposed under a pretend sandbox;
- native unsandboxed shell requires full-host acknowledgement;
- Windows recommendation is WSL2 + Linux dotlink for secure shell.

## Developer caches

Caches are separate shell-only capabilities.

They never add MCP read/write roots.

RO cache:

- protects host cache integrity;
- cache misses cannot populate.

RW cache:

- normal package behavior;
- sandboxed code can mutate shared host cache.

Adjacent config/credentials are excluded.

Deny precedence:

```text
deny-read  → remove cache mount
deny-write → RW becomes RO
```

## Runtime key security

OpenAI Runtime API key:

- separate private file;
- not JSONC;
- never printed;
- per profile.

Setup Admin key:

- only optional tunnel-creation flow;
- never saved.

## OAuth state security

Persistent state includes:

- Argon2id owner hash;
- approved DCR client metadata;
- SHA-256 refresh-token hashes.

Does not persist:

- owner plaintext;
- authorization codes;
- access tokens;
- unapproved DCR registrations.

On Unix:

```text
state dir  0700
state file 0600
lock file  0600
```

## Cross-process OAuth locking

Durable state may be touched by:

- running HTTP server;
- setup/profile edit;
- `dotlink oauth` commands.

All durable read/modify/write mutations must go through the private state-lock primitive.

Invariant:

> an atomic file replacement is not enough; the entire read/modify/write transaction must be locked.

This prevents lost approved-client/refresh/password updates across processes.

## Owner password verification

Argon2 is CPU/memory intensive.

Never run it directly on Tokio workers.

Current behavior:

- `spawn_blocking`;
- semaphore cap: two concurrent checks;
- bounded wait for a verifier slot;
- failed attempt delay;
- no global lockout that a remote attacker can force against the owner.

## OAuth public-state bounds

Explicit caps exist for:

- pending DCR registrations;
- pending authorization requests;
- authorization codes;
- access tokens;
- refresh grants;
- approved clients;
- metadata body size;
- request body size.

Pending unauthenticated structures use bounded eviction rather than allowing unbounded memory growth.

## CIMD SSRF protection

Client metadata URL requirements:

- HTTPS;
- path not root;
- no credentials;
- no query;
- no fragment.

Fetch:

- DNS resolve;
- every address must be public;
- validated full address set pinned into reqwest;
- redirects disabled;
- timeout;
- body-size cap;
- client_id in document must match requested URL.

Private/link-local/documentation/etc addresses are rejected.

## Redirect URI policy

OAuth redirect URIs:

- HTTPS allowed;
- loopback HTTP allowed for native/local clients;
- non-loopback HTTP rejected;
- custom URI schemes are not accepted by current policy.

DCR max redirect count is bounded.

## OAuth consent UI

No external JS/assets/analytics.

Security headers:

- restrictive CSP;
- frame denied;
- no-referrer;
- no-store;
- nosniff.

Consent shows:

- client name;
- client ID;
- redirect URI;
- resource;
- scope.

## Public ingress

Loopback HTTP may be unauthenticated.

Any externally reachable HTTP is safe-by-default.

Direct non-loopback:

```text
OAuth + explicit HTTPS public origin
OR
--allow-public-no-auth
```

ngrok:

```text
OAuth automatically
OR
--allow-public-no-auth
```

The unsafe no-auth option is intentionally one-run-only.

## Ephemeral URL paths

Ephemeral paths provide high entropy and reduce accidental discovery.

They are **not authentication**.

Never describe them as a security substitute for OAuth.

Changing the resource path changes OAuth resource identity.

## Logging privacy

TOOL logs may include:

- tool;
- path;
- flags;
- byte counts;
- result;
- latency.

REQ logs may include:

- method;
- status;
- transport metadata.

Do not intentionally log:

- file content;
- binary payloads;
- passwords;
- Runtime keys;
- bearer tokens;
- refresh tokens;
- raw OAuth form bodies.

## Resource limits

Current bounded surfaces include:

- file reads;
- file writes;
- patch size;
- output;
- shell timeout;
- tunnel concurrency;
- OAuth bodies/metadata/state;
- peer-transport shutdown wait.

Shell timeout uses one absolute deadline across:

- stdin delivery;
- process execution;
- output collection.

Children are explicitly killed/reaped on timeout/failure.

## Threat-boundary statement

dotlink protects against authority expansion through its own MCP surface and sandboxed shell.

It does not claim to protect against:

- root;
- kernel compromise;
- a malicious same-user process outside dotlink;
- physical access;
- compromised external AI/provider accounts;
- a user explicitly choosing `--allow-all --no-sandbox`;
- a user explicitly choosing `--allow-public-no-auth`.

Keep this distinction clear in future security claims.
