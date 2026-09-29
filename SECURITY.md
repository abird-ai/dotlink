# Security model

`abird-tunnel` is least-privilege by default.

With no permission flags it exposes only:

```text
ls
read
read_binary
```

and only the effective cwd is readable.

## Filesystem grants

Filesystem permissions are selected on every launch.

```text
--allow-read=DIR
--allow-write=DIR
--allow-rw=DIR
--deny=DIR
```

Rules are additive. `--deny` always takes precedence.

The effective cwd is read-enabled by default. Bare `--allow-write` is shorthand for rw on cwd.

Rust filesystem tools canonicalize existing targets and ancestors before access checks. Symlink escapes therefore do not bypass the grant policy.

Text and binary mutations use the same checks:

- `write` / `write_binary` require write permission;
- `edit` / `patch_binary` require read + write permission;
- `read` / `read_binary` / `ls` require read permission.

The saved tunnel runtime credential is separately protected even if a broad read grant would otherwise contain it.

## Linux Bubblewrap shell

`--allow-shell` enables Bash on Unix. On Linux it runs inside Bubblewrap unless unsandboxed dangerous access was explicitly requested.

The sandbox:

- mounts readable grants read-only;
- mounts paths with effective read + write permission read-write;
- does not mount a purely write-only grant;
- masks denied directories;
- masks the saved tunnel runtime key;
- uses an empty temporary home;
- mounts system runtime paths read-only;
- isolates PID, IPC, and UTS namespaces; and
- unshares the network namespace by default.

Network is restored only with:

```text
--allow-network
```

Write-only host directories are intentionally not presented to Bash because Bubblewrap cannot safely make an existing directory writable while preventing reads. Use `--allow-rw=DIR` if shell access to that directory is required.

## Dangerous unsandboxed shell

`--no-sandbox` alone does not enable shell access.

An unsandboxed shell inherits the OS user's real host authority. Because the program cannot reliably restrict that process after removing the OS sandbox, both acknowledgements are required:

```text
--allow-rw-all-dangerous
--allow-network-dangereous
```

plus `--allow-shell --no-sandbox`.

The shortcut:

```text
--allow-all-dangerous
```

enables unrestricted filesystem tools, network, and unsandboxed shell access.

A Rust-tool `--deny` can still override `--allow-rw-all-dangerous`. However, deny rules cannot constrain an unsandboxed shell. Therefore abird-tunnel refuses to start an unsandboxed shell when any `--deny` rule is present.

## Windows

The Windows shell tool is `powershell`, preferring `pwsh.exe` and falling back to `powershell.exe`.

Bubblewrap is Linux-only, so Windows shell execution currently follows the unsandboxed-dangerous requirements above.

Filesystem tools still enforce allow/deny policy in Rust.

## Network

The tunnel itself always needs outbound HTTPS to the OpenAI control plane.

That tunnel process is separate from shell network permissions.

Inside the Linux Bubblewrap child:

- default: no network namespace access;
- `--allow-network`: host network namespace retained.

Outside the sandbox, network cannot be meaningfully blocked by abird-tunnel; that is why unsandboxed shell requires `--allow-network-dangereous`. The corrected spelling `--allow-network-dangerous` is accepted as an alias.

## Credentials

First-run setup uses two credentials:

1. **Runtime key** — narrowly scoped to Tunnels Read + Use and needed while running.
2. **Admin key** — used only to create a tunnel and never persisted.

The Runtime key is stored at:

```text
~/.config/abird-tunnel/runtime.key
```

On Unix the file is mode `0600` and its directory is `0700`.

Known OpenAI/tunnel credential environment variables are removed from child shell environments.

## Binary data

`read_binary` can return MCP image/audio/blob content or explicit base64/hex. Read limits still apply.

`write_binary` and `patch_binary` enforce write limits. `patch_binary` additionally caps the total file size it will patch in memory.

## Tunnel boundary

The local process does not listen on a TCP port. It uses outbound HTTPS to OpenAI's Secure MCP Tunnel.

Control-plane request headers are filtered before MCP dispatch. Only MCP protocol headers are forwarded to the local service.

## Resource limits

- tunnel execution concurrency is bounded;
- shell stdout/stderr are continuously drained with bounded retained output;
- text and binary reads/writes are bounded;
- shell commands have a timeout and are terminated on timeout/drop.

These limits reduce accidental resource exhaustion but are not a substitute for the Bubblewrap or OS security boundary.

## Recommended modes

Safest ordinary mode:

```bash
abird-tunnel
```

Writable project:

```bash
abird-tunnel --allow-write
```

Sandboxed build/test shell with project writes but no network:

```bash
abird-tunnel --allow-write --allow-shell
```

Sandboxed shell with network:

```bash
abird-tunnel --allow-write --allow-shell --allow-network
```

Avoid `--allow-all-dangerous` unless unrestricted host access is explicitly intended. Never run abird-tunnel as root.
