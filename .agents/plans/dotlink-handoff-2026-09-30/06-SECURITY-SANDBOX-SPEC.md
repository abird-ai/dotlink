# Security and sandbox specification

## Threat model

The AI/MCP client is not trusted with arbitrary host authority.

The local user explicitly selects capabilities; `dotlink` enforces them locally regardless of prompt content.

## Filesystem policy

Core policy sets:

```text
read_roots
write_roots
deny_read_roots
deny_write_roots
```

Existing targets are canonicalized before checks.

Creation targets canonicalize their nearest existing ancestor before the final path is checked.

Intent: prevent symlink/path traversal from escaping allowed roots.

Tool requirements:

```text
read / read_binary / ls      read
write / write_binary         write
edit / patch_binary          read + write
```

Write-only paths are valid destinations and need not be readable.

## Dynamic tool surface

Read-only sessions should advertise:

```text
ls
read
read_binary
```

Mutation tools appear only when effective write authority exists.

Shell tool appears only when effective shell authority exists.

## Linux Bubblewrap model

Normal shell sandbox:

```text
/proc       fresh proc mount
/dev        Bubblewrap-managed minimal /dev
/tmp        private tmpfs
HOME        /tmp/home
network     unshared/off by default
PID         isolated
IPC         isolated
UTS         isolated
```

Policy mounts:

```text
read-only grant       host path → same path RO
read+write grant      host path → same path RW
write-only grant      not exposed to shell
deny-read             hidden/masked
deny-write            readable subtree rebound RO
```

## NixOS runtime support

Read-only runtime graph as present:

```text
/nix/store
/run/current-system
/etc/profiles
/nix/var/nix/profiles
~/.nix-profile
```

Do not mount whole `$HOME` for Nix tooling.

Shell executable and PATH entries are canonicalized so profile symlinks resolve to mounted `/nix/store` targets.

## Nix daemon isolation

Security invariant:

> A shell whose network is denied must not regain external/host authority through the host Nix daemon.

Mask daemon endpoint roots:

```text
/nix/var/nix/daemon-socket
/run/nix-daemon
```

Do not assume a fixed `.../socket` leaf or Unix socket type.

Generic masking abstraction:

```text
directory       inaccessible tmpfs
file/socket     /dev/null bind
```

This design replaced a brittle approach that failed when the expected socket leaf was actually a directory.

## Developer cache sharing

Typed cache sharing is **shell-only**.

It must not expand `read_roots` / `write_roots` used by MCP filesystem tools.

Example mapping:

```text
host ~/.cargo/registry  → sandbox /tmp/home/.cargo/registry
host ~/.cargo/git       → sandbox /tmp/home/.cargo/git
host ~/.npm             → sandbox /tmp/home/.npm
host ~/.cache/uv        → sandbox /tmp/home/.cache/uv
```

Modes:

```text
read_only
read_write
```

RO tradeoff:

- existing packages reusable;
- cache misses cannot populate that cache;
- better host cache integrity.

RW tradeoff:

- package-manager experience is normal;
- successful downloads/builds remain reusable;
- sandboxed code can mutate/poison shared host cache.

Never implicitly share parent credential/config files.

Examples that must remain unshared unless separately granted:

```text
~/.cargo/credentials.toml
~/.cargo/config.toml
~/.npmrc
```

Path denies remain authoritative over cache grants:

```text
deny-read matching cache  → drop cache mount
deny-write matching cache → downgrade RW mount to RO
```

## Cache environment mapping

Current code may set package-manager-specific variables inside sandbox, including:

```text
CARGO_HOME
NPM_CONFIG_CACHE
npm_config_store_dir
YARN_CACHE_FOLDER
PIP_CACHE_DIR
UV_CACHE_DIR
GOMODCACHE
GOCACHE
GRADLE_USER_HOME
SCCACHE_DIR
CCACHE_DIR
```

## Network

Sandbox network is off by default.

`--allow-network` enables normal sandboxed shell network.

`--deny-network` overrides profile/runtime network grants.

Profile `allow_network` requires profile shell authority.

Profile sandbox network permission must not silently authorize the full unsandboxed mode.

## Full unsandboxed mode

Deliberately paired:

```bash
dotlink --allow-all --no-sandbox
```

Do not try to enforce ordinary Rust path denies against an arbitrary unsandboxed subprocess; guard/reject incompatible combinations.

## Secrets

Runtime API keys are stored separately from JSONC.

All `runtime*.key` files in the config directory are protected paths.

Known OpenAI/tunnel key environment variables are removed from child shell environment.

`NGROK_AUTHTOKEN` is removed from child shell environment.

## HTTP/ngrok

Loopback HTTP does not add application auth by itself.

ngrok makes the endpoint remotely reachable.

Ephemeral path is high entropy but **not authentication**.

Treat public URL as sensitive or add external access controls.

## Logging privacy

Normal TOOL logs include safe metadata only:

- tool name;
- path / shell working-directory metadata;
- flags;
- command preview;
- content/data sizes instead of body;
- status;
- latency;
- timestamp.

Verbose REQ logs include protocol/transport metadata:

- stdio JSON-RPC method;
- HTTP method/path/status/latency;
- OpenAI MCP request labels.

Do not intentionally log:

- file contents;
- binary payloads;
- raw request bodies;
- API keys;
- secrets.

`--silent` hides TOOL logs.

`--silent -vv` retains REQ logs while hiding TOOL logs.
