# Architecture

## One process

`abird-tunnel` combines setup, policy enforcement, the MCP server, and the Secure MCP Tunnel client in one Tokio process.

```text
┌──────────────────────────────────────────────────────────────┐
│ abird-tunnel                                                 │
│                                                              │
│ setup/config                                                 │
│      │                                                       │
│      ├──────────────┐                                        │
│      ▼              ▼                                        │
│ tunnel client   LocalMachine                                 │
│      │          ├── access policy                            │
│      │          ├── rmcp tool router                         │
│      │          └── Bubblewrap shell launcher (Linux)        │
│      │                                                       │
│      └──────────► EmbeddedMcp StreamableHttpService          │
│                  (in memory; no listening socket)            │
└──────────────┬───────────────────────────────────────────────┘
               │ outbound HTTPS
               ▼
       OpenAI Secure MCP Tunnel
```

`src/tunnel.rs` converts each polled tunnel command into an in-memory request accepted by `rmcp::StreamableHttpService`.

## Startup

1. Parse permission and capability flags.
2. Canonicalize `--cwd` and all allow/deny directories.
3. Add the implicit default read grant for cwd.
4. Resolve additive read/write/rw grants and deny rules.
5. Validate shell/sandbox/network policy.
6. Load tunnel configuration and the restricted Runtime key.
7. Construct the policy-aware `LocalMachine` MCP server.
8. Start Secure MCP Tunnel polling.
9. Print ready after the first successful poll.

## Permission model

The access policy contains:

```text
cwd
read_roots[]
write_roots[]
deny_roots[]
rw_all_dangerous
```

Access checks are capability-based rather than tied to one root.

For a path:

- deny match → reject;
- read succeeds if any read root contains the canonical path;
- write succeeds if any write root contains the canonical path;
- read+write operations require both.

`--allow-rw=DIR` simply inserts DIR into both root sets.

The effective cwd is always inserted into `read_roots` unless a deny overrides it.

Bare `--allow-write` inserts cwd into `write_roots`, producing rw cwd because cwd was already readable.

Existing paths are canonicalized before policy checks. Create targets canonicalize their nearest existing ancestor and check the resulting path before mutation.

## Dynamic MCP router

The static Rust implementation defines:

```text
read
write
edit
ls
read_binary
write_binary
patch_binary
bash
powershell
```

The server builds a policy-specific router at runtime.

Default visible surface:

```text
ls
read
read_binary
```

Mutation tools are hidden unless at least one write capability exists.

Shell tools are hidden unless shell execution is enabled. Only `bash` is exposed on Unix and only `powershell` on Windows.

Hidden tools are absent from `tools/list` and rejected as unknown if called directly.

## Bubblewrap translation

On Linux, when shell is enabled without `--no-sandbox`, filesystem policy is translated to Bubblewrap mounts.

For each distinct grant path, effective read/write capability is computed from all overlapping grants:

- readable + not writable → `--ro-bind`;
- readable + writable → `--bind`;
- write-only → not mounted into the shell.

Mounts are applied from shallow paths to deeper paths, allowing a more specific nested grant to override a broader parent mount.

Denied directories are masked after allow mounts with an empty mode-000 tmpfs.

The tunnel runtime key is masked separately.

Bubblewrap uses a temporary home and temp directory plus read-only system runtime mounts.

## Network isolation

Sandboxed shell starts with `--unshare-net`.

`--allow-network` removes that isolation and mounts minimal network-related system files such as resolver/certificate paths when present.

Network permission affects only the shell child. The main abird-tunnel process always needs outbound HTTPS to the OpenAI tunnel service.

## Unsandboxed shell

An unsandboxed shell cannot be constrained by Rust path checks after execution begins.

Therefore unsandboxed shell is enabled only when the user explicitly combines:

```text
--allow-shell
--no-sandbox
--allow-rw-all-dangerous
--allow-network-dangereous
```

`--allow-all-dangerous` is the shortcut for that full capability set.

Because deny rules cannot be enforced against an unsandboxed process, startup rejects unsandboxed shell + `--deny`.

## Binary transport

`read_binary(format=mcp)` maps bytes into typed MCP content:

- image MIME → `ContentBlock::image`;
- audio MIME → `ContentBlock::audio`;
- other MIME → embedded `BlobResourceContents`.

`base64` and `hex` modes return encoded text for byte-level work.

`write_binary` decodes base64/hex to bytes.

`patch_binary` loads a bounded file, replaces the requested byte range, then writes it back.

## Tunnel lifecycle

The client polls:

```text
GET /v1/tunnels/{id}/poll?limit=25&timeout_ms=15000
```

Results are posted to:

```text
POST /v1/tunnels/{id}/response
X-Tunnel-Shard-Token: <opaque token>
```

A Tokio semaphore limits concurrent commands. Tunnel response deadlines cover local MCP execution and response delivery.

Polling and terminal response delivery use bounded retry/backoff for transient failures. Authentication/configuration failures remain fatal.
