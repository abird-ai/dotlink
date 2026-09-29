# abird-tunnel

A single Rust binary that connects ChatGPT to local files and tools through OpenAI Secure MCP Tunnel.

```text
ChatGPT
   │ Secure MCP Tunnel (outbound HTTPS)
   ▼
api.openai.com
   ▲
   │
abird-tunnel
   ├── read / write / edit / ls
   ├── read_binary / write_binary / patch_binary
   └── bash (Unix) / powershell (Windows)
```

There is no Go `tunnel-client`, local MCP listener, inbound firewall rule, or public endpoint. The Rust process speaks the OpenAI tunnel protocol directly and dispatches MCP in memory through `rmcp`.

## Quick start

```bash
cd ~/src/my-project
abird-tunnel
```

With no permission flags, only the effective cwd is readable and the exposed tools are:

```text
ls
read
read_binary
```

Choose another default cwd with:

```bash
abird-tunnel --cwd=/path/to/project
```

## Filesystem permissions

Permissions are additive. `--deny` always takes precedence.

The default is equivalent to:

```text
--allow-read=<cwd>
```

Add repeatable grants:

```bash
abird-tunnel --allow-read=/data/reference
abird-tunnel --allow-write=/data/output
abird-tunnel --allow-rw=/src/project
abird-tunnel --deny=/src/project/secrets
```

- `--allow-read=DIR`: read DIR.
- `--allow-write=DIR`: write DIR without granting read.
- `--allow-rw=DIR`: read + write DIR.
- `--deny=DIR`: deny DIR even if a broader allow contains it.

Bare:

```bash
abird-tunnel --allow-write
```

is shorthand for:

```text
--allow-rw=<cwd>
```

while `--allow-write=/some/dir` is genuinely write-only.

Relative tool paths resolve from `--cwd`. Absolute paths are accepted when allowed. Existing targets and ancestors are canonicalized, so symlink escapes cannot bypass the policy.

`edit` and `patch_binary` require both read and write permission. `write` and `write_binary` require write permission only.

## Linux shell sandbox

Enable the shell explicitly:

```bash
abird-tunnel --allow-shell
```

On Linux, Bash runs inside **Bubblewrap** by default. The sandbox:

- mounts readable grants read-only;
- mounts paths with both read + write permission read-write;
- leaves write-only grants out of the shell unless a read grant overlaps them;
- masks `--deny` directories;
- masks the saved tunnel runtime key;
- uses an empty temporary home;
- isolates PID, IPC, and UTS namespaces; and
- **blocks network access by default**.

Allow network inside the sandbox with:

```bash
abird-tunnel --allow-shell --allow-network
```

`--allow-network` is only valid for the Linux sandbox.

Bubblewrap cannot safely expose an existing host directory as truly write-only. Use `--allow-rw=DIR` when the shell itself needs writable access to that directory.

## Dangerous unsandboxed access

`--no-sandbox` does not grant shell access by itself.

An unsandboxed shell inherently has host filesystem and network capability, so both acknowledgements are required:

```bash
abird-tunnel \
  --allow-shell \
  --no-sandbox \
  --allow-rw-all-dangerous \
  --allow-network-dangereous
```

`--allow-rw-all-dangerous` also makes the Rust filesystem tools unrestricted. Explicit `--deny` still wins for Rust tools.

An unsandboxed shell cannot enforce `--deny`, so that combination is rejected.

The full shortcut is:

```bash
abird-tunnel --allow-all-dangerous
```

which means unrestricted filesystem + network + unsandboxed shell.

On Windows the shell tool is `powershell`, preferring `pwsh.exe` and then `powershell.exe`. Bubblewrap is Linux-only, so Windows shell execution requires the dangerous unsandboxed acknowledgements.

## Binary tools

The text tools stay Pi-like and text-only:

```text
read
write
edit
ls
bash / powershell
```

Binary operations are separate:

```text
read_binary
write_binary
patch_binary
```

`read_binary` supports:

```text
format=mcp      typed MCP image/audio/blob content
format=base64   base64 text
format=hex      hexadecimal text
```

`format=mcp` is the default: images become MCP image content, audio becomes MCP audio content, and other binary types become MCP blob resources.

`write_binary` accepts `encoding=base64|hex`.

`patch_binary` replaces a byte range by offset and supports replacement, insertion (`length=0`), and deletion (empty payload + positive `length`).

## Tool surface

Default:

```bash
abird-tunnel --list-tools
```

```text
ls
read
read_binary
```

With any write grant:

```text
edit
ls
patch_binary
read
read_binary
write
write_binary
```

With shell enabled, `bash` on Unix or `powershell` on Windows is added.

Examples:

```bash
abird-tunnel --allow-write --list-tools
abird-tunnel --allow-shell --list-tools
abird-tunnel --allow-rw=/src/project --allow-shell --list-tools
```

## Verbose request logging

```bash
abird-tunnel -v
# or
abird-tunnel --verbose
```

Example:

```text
→ tools/call read  path="README.md"
← read  200  2ms
→ tools/call bash  cwd="." command="cargo test"
← bash  200  842ms
```

Bulk `content` and `stdin` are logged only as byte counts.

## First-run OpenAI setup

Setup asks for:

1. a restricted Runtime API key with **Tunnels Read + Use**;
2. an existing tunnel ID, or a one-time Admin key with **Tunnels Manage** to create one;
3. a ChatGPT Workspace ID or OpenAI Organization ID when creating a tunnel.

Useful links:

- Runtime API keys: https://platform.openai.com/settings/organization/api-keys
- Admin keys: https://platform.openai.com/settings/organization/admin-keys
- ChatGPT Workspace ID: https://chatgpt.com/admin
- OpenAI Organization ID: https://platform.openai.com/settings/organization/general

The Admin key is used once, never saved, and can be deleted after setup.

## Build

Nix is the recommended Linux path:

```bash
nix build
nix run .
nix flake check
nix develop
```

The Linux Nix package includes Bubblewrap and Bash in the runtime wrapper.

Inside `nix develop`:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --all-features
```

Cargo install also works; install `bubblewrap` separately on Linux if you want `--allow-shell`.

## CLI summary

```text
abird-tunnel
    read-only cwd

--allow-read=<DIR>          add read
--allow-write=<DIR>         add write-only
--allow-rw=<DIR>            add read+write
--allow-write               shorthand: rw cwd
--deny=<DIR>                deny; always wins

--allow-shell               add platform shell
--allow-network             network inside Linux sandbox
--no-sandbox                disable Bubblewrap only

--allow-rw-all-dangerous    unrestricted Rust filesystem tools
--allow-network-dangereous   acknowledge unsandboxed network
                             corrected alias: --allow-network-dangerous
--allow-all-dangerous       unrestricted rw + network + unsandboxed shell

--cwd=<DIR>                 default cwd
--list-tools                show exposed tools
-v, --verbose               concise tool logs
--setup                     redo tunnel setup
--print-id                  print Tunnel ID
```

## Persistent state

```text
~/.config/abird-tunnel/config.toml
~/.config/abird-tunnel/runtime.key
```

Filesystem grants are selected fresh on every launch. The runtime key is mode `0600` on Unix and its parent directory is `0700`. Rust filesystem tools reject that credential path, and the Linux sandbox masks it.

## Connect to ChatGPT

After `abird-tunnel` prints the `tunnel_...` ID:

1. enable ChatGPT Developer mode;
2. create a Plugin developer connection using **Tunnel**;
3. paste the tunnel ID;
4. copy the resulting `plugin_asdk_app...` connection ID;
5. use `prompts/PLUGIN_CREATOR.md` to create the private plugin package.

See `docs/CHATGPT_PLUGIN.md` for the companion flow.

## License

MIT.
