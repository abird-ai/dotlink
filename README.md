# abird-tunnel

A single Rust binary that connects ChatGPT to the local workspace it is launched from through OpenAI Secure MCP Tunnel.

```text
ChatGPT
   │
   │ Secure MCP Tunnel (outbound HTTPS)
   ▼
api.openai.com
   ▲
   │ long-poll / response
   │
┌──┴────────────────────────────────────────┐
│ abird-tunnel                              │
│                                           │
│ native Rust tunnel transport              │
│           │                               │
│           ▼                               │
│ in-process rmcp server                    │
│   ├── machine_info                        │
│   ├── fs_list                             │
│   ├── fs_stat                             │
│   ├── fs_read_text                        │
│   ├── fs_write_text                       │
│   ├── fs_mkdir                            │
│   ├── fs_remove                           │
│   └── shell_exec → bash                   │
└───────────────────────────────────────────┘
```

There is no Go `tunnel-client`, child MCP process, local HTTP listener, inbound firewall rule, or public endpoint. `abird-tunnel` speaks the OpenAI Secure MCP Tunnel wire protocol directly and dispatches MCP requests into the Rust `rmcp` server in memory.

## One-command UX

Install once:

```bash
cargo install --path .
```

Then enter any project and run:

```bash
cd ~/src/my-project
abird-tunnel
```

That directory becomes the filesystem workspace for this process. All `fs_*` tools are restricted to it.

To expose a different workspace:

```bash
abird-tunnel --cwd=/path/to/project
```

`--cwd` is optional and defaults to the current working directory from which `abird-tunnel` was invoked.

A normal startup stays intentionally small:

```text
abird-tunnel 0.2.0
────────────────────────────────────────────────────────
• Tunnel     tunnel_0123456789abcdef0123456789abcdef
• Workspace  /home/pvl/src/my-project
• Bash       enabled
• Status     connecting…

ChatGPT → Plugins → Tunnel → paste the Tunnel ID above.
Ctrl-C to stop.

✓ Connected — ready
```

Detailed transport logs are hidden by default.

For a concise live view of what ChatGPT asks the bridge to do:

```bash
abird-tunnel --verbose
# or
abird-tunnel -v
```

Example:

```text
→ initialize
← initialize  200  1ms
→ tools/list
← tools/list  200  0ms
→ tools/call fs_read_text  path="README.md"
← fs_read_text  200  2ms
→ tools/call shell_exec  cwd="." command="cargo test"
← shell_exec  200  842ms
```

Verbose mode shows method/tool names and useful small arguments. Bulk
`content` and `stdin` values are shown only as byte counts. For lower-level
transport diagnostics, use `RUST_LOG=abird_tunnel=info`.

## Filesystem boundary

The selected workspace is a hard boundary for every `fs_*` tool.

Filesystem tools:

- accept only paths relative to the workspace;
- reject absolute paths;
- reject `..` traversal;
- canonicalize existing paths and ancestors;
- reject symlink escapes outside the workspace;
- do not recursively traverse symlinked directories;
- refuse to delete the workspace root itself; and
- enforce bounded reads and writes.

For example, if you run:

```bash
cd ~/src/abird
abird-tunnel
```

then `fs_read_text("README.md")` is allowed, while `/etc/passwd`, `../other-project/file`, and a symlink resolving outside `~/src/abird` are rejected.

### Bash is intentionally different

`shell_exec` starts in the selected workspace (or a relative subdirectory supplied to the tool), but Bash is not an OS sandbox. A command can deliberately use absolute paths, `cd ..`, spawn other programs, access the network, and otherwise exercise the permissions of the Unix user running `abird-tunnel`.

If you want a hard boundary for shell commands too, run `abird-tunnel` inside a dedicated user, container, VM, namespace, or another operating-system sandbox.

## First-run OpenAI setup

Setup is designed to be a short one-time checklist:

```text
abird-tunnel setup
────────────────────────────────────────────────────────
1. Create a Runtime API key
   • Permissions: Tunnels Read + Use
   • https://platform.openai.com/settings/organization/api-keys
   Paste key:

2. Choose a tunnel
   • Paste an existing Tunnel ID, or press Enter to create one.
   Tunnel ID [create new]:
```

If you create a new tunnel, setup adds only two more short steps:

- create an **Admin API key** with **Tunnels Manage**;
- choose a ChatGPT workspace ID, or use your OpenAI organization ID.

ID locations:

- **ChatGPT Workspace ID:** https://chatgpt.com/admin — select the workspace,
  open its settings, and copy the Workspace ID/UUID.
- **OpenAI Organization ID:** https://platform.openai.com/settings/organization/general
  — copy the `org-...` identifier.

The Admin key is used exactly once, is **never saved**, and can be deleted as
soon as setup finishes. The restricted Runtime key is saved separately with
user-only permissions on Unix and is reused on future launches.

A fresh tunnel can take a short moment to activate; `abird-tunnel` handles
that automatically.

OpenAI documentation:

- Secure MCP Tunnel: https://developers.openai.com/api/docs/guides/secure-mcp-tunnels
- Connect/test plugins: https://developers.openai.com/plugins/deploy/connect-chatgpt
- Package plugins: https://developers.openai.com/plugins/build/plugins

## Connect it to ChatGPT

Once `abird-tunnel` prints its Tunnel ID:

1. In ChatGPT, enable **Settings → Security and login → Developer mode**.
2. Open **Plugins**, select **+** and create a developer-mode connection.
3. Use a name such as **Abird Tunnel**.
4. Under **Connection**, choose **Tunnel**.
5. Select the tunnel or paste the printed `tunnel_...` ID.
6. Create the connection and verify the discovered MCP tools.
7. Copy the connection's technical ID from its ChatGPT browser URL. It begins with `plugin_asdk_app...`.
8. Give that technical ID to Plugin Creator using the ready prompt in [`prompts/PLUGIN_CREATOR.md`](prompts/PLUGIN_CREATOR.md).

See [`docs/CHATGPT_PLUGIN.md`](docs/CHATGPT_PLUGIN.md) for the complete flow and the exact information you need to provide.

## Building

### Nix — recommended

The flake is a complete build/run/test interface:

```bash
nix build                 # build ./result/bin/abird-tunnel
nix run .                 # run abird-tunnel
nix run . -- --setup      # pass CLI arguments
nix flake check           # build + tests + rustfmt + clippy -D warnings
nix develop               # Rust development shell
```

The checked-in `flake.lock` pins Nixpkgs so CI and local builds use the same
toolchain. The package build itself runs `cargo test --all-features`.

Inside `nix develop`, the equivalent direct Rust checks are:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --all-features
```

### Cargo

Rust 1.98.1 is pinned by `rust-toolchain.toml`:

```bash
cargo build --release
cargo install --path .
abird-tunnel
```

## Persistent state

By default on Linux/macOS:

```text
~/.config/abird-tunnel/config.toml
~/.config/abird-tunnel/runtime.key
```

`config.toml` contains non-secret tunnel/client settings. It does **not** store a workspace path: the filesystem workspace is selected afresh from the process working directory on every run.

`runtime.key` contains the restricted tunnel runtime key and is mode `0600` on Unix; its directory is mode `0700`. The normal `fs_*` tools explicitly protect that credential path if the selected workspace happens to contain it. Bash remains full-user-power.

Set `ABIRD_TUNNEL_CONFIG` to move the config file; `runtime.key` is stored alongside it.

## Environment-only mode

For unattended use, persisted config is optional:

```bash
export CONTROL_PLANE_TUNNEL_ID='tunnel_0123456789abcdef0123456789abcdef'
export CONTROL_PLANE_API_KEY='...'
cd /path/to/workspace
abird-tunnel
```

Recognized aliases include:

```text
ABIRD_TUNNEL_ID
ABIRD_TUNNEL_API_KEY
ABIRD_TUNNEL_ALLOW_SHELL
ABIRD_TUNNEL_SHELL
ABIRD_TUNNEL_ORGANIZATION_ID
ABIRD_TUNNEL_BASE_URL
CONTROL_PLANE_TUNNEL_ID
CONTROL_PLANE_API_KEY
CONTROL_PLANE_ORGANIZATION_ID
CONTROL_PLANE_BASE_URL
OPENAI_ORGANIZATION
```

Known tunnel/OpenAI credential variables are removed from the environment inherited by `shell_exec`.

## CLI

```text
abird-tunnel                     start everything; workspace = current directory
abird-tunnel --cwd=<DIR>         use DIR as the filesystem workspace
abird-tunnel --setup             redo first-run tunnel setup
abird-tunnel --print-id          print the configured Tunnel ID and exit
abird-tunnel --list-tools        list every exposed MCP tool and exit
abird-tunnel -v                  show concise incoming requests/tool calls
abird-tunnel --verbose           same as -v
abird-tunnel --no-shell          disable shell_exec for this run
```

Flags can be combined, for example:

```bash
abird-tunnel --cwd=~/src/abird --no-shell
```

## MCP tools

List the live tool set directly from the MCP router:

```bash
abird-tunnel --list-tools
```

| Tool | Purpose | Mutates? |
|---|---|---:|
| `machine_info` | Show the workspace boundary, platform, and execution policy | No |
| `fs_list` | List files/directories inside the workspace | No |
| `fs_stat` | Inspect a path inside the workspace | No |
| `fs_read_text` | Read a bounded UTF-8 text file inside the workspace | No |
| `fs_write_text` | Create/overwrite/append a bounded text file inside the workspace | Yes |
| `fs_mkdir` | Create a directory inside the workspace | Yes |
| `fs_remove` | Remove a file/directory inside the workspace | Yes |
| `shell_exec` | Run `bash --noprofile --norc -lc ...` starting inside the workspace | Potentially anything |

Default limits:

```text
shell timeout       120 seconds maximum (30 seconds default per call)
stdout              1 MiB retained
stderr              1 MiB retained
text read            4 MiB
text write           4 MiB
concurrent commands  8
```

## Native tunnel implementation

`src/tunnel.rs` implements the Secure MCP Tunnel client contract, including:

- `GET /v1/tunnels/{tunnel_id}/poll`;
- `POST /v1/tunnels/{tunnel_id}/response`;
- Bearer runtime authentication;
- opaque request/shard identifiers;
- JSON-RPC and notification handling;
- bounded concurrent command execution;
- response deadlines;
- bounded retry/backoff behavior;
- protocol-relevant MCP header filtering; and
- MCP JSON/SSE response handling.

Use one active `abird-tunnel` process per Tunnel ID.

## Development checks

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

## License

MIT.
