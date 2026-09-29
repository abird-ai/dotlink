# abird-tunnel

A small Rust MCP bridge for permission-scoped local files and optional local command execution.

The same LocalMachine MCP server can be exposed through three independent transports:

~~~text
                         LocalMachine
            read / write / edit / ls / binary / shell
                              |
               +--------------+--------------+
               |              |              |
             OpenAI          stdio      Streamable HTTP
          Secure Tunnel   stdin/stdout       /mcp
                                             |
                                             +-- optional ngrok
                                                 public HTTPS /mcp
~~~

The transports are modular: OpenAI, stdio, and HTTP can each be enabled or disabled in persisted setup.

## Setup

Run once:

~~~bash
abird-tunnel --setup
~~~

Setup begins with:

~~~text
1. Choose MCP transports
   • openai — OpenAI Secure MCP Tunnel; starts automatically
   • stdio  — local stdio MCP server; start with --stdio
   • http   — local HTTP MCP server; start with --http
   • Enter comma-separated names, or 'all'.
   Enabled [openai]:
~~~

Examples:

~~~text
openai
stdio
http
stdio,http
openai,stdio,http
all
~~~

If OpenAI is not selected, every OpenAI credential/tunnel question is skipped and no OpenAI runtime key is required.

The selection is persisted in:

~~~text
~/.config/abird-tunnel/config.toml
~~~

## Transport activation

Configured OpenAI Tunnel starts automatically:

~~~bash
abird-tunnel
~~~

Configured stdio and HTTP start only when explicitly requested:

~~~bash
abird-tunnel --stdio
abird-tunnel --http
abird-tunnel --stdio --http
~~~

If OpenAI is also enabled, it runs alongside requested local transports.

A transport disabled in setup cannot be started until it is enabled with abird-tunnel --setup.

### stdio

--stdio is the standard subprocess MCP transport for clients such as Claude Desktop, Claude Code, and other clients that launch an MCP server command.

Typical client shape:

~~~json
{
  "mcpServers": {
    "abird": {
      "command": "abird-tunnel",
      "args": ["--stdio"]
    }
  }
}
~~~

When stdio is active, stdout is reserved exclusively for MCP JSON-RPC. Human-readable status goes to stderr.

### HTTP

The default local Streamable HTTP endpoint is:

~~~text
http://127.0.0.1:3000/mcp
~~~

Start it with:

~~~bash
abird-tunnel --http
~~~

Override the bind address for one run:

~~~bash
abird-tunnel --http --http-bind=127.0.0.1:8080
~~~

HTTP-capable MCP clients connect directly to /mcp.

### Ephemeral HTTP paths

HTTP and ngrok can each use a fresh hard-to-guess MCP path for one process run.

Use the shorthand for both:

~~~bash
abird-tunnel --http --ngrok --ephemeral-url
~~~

This gives local HTTP and ngrok independent fresh paths such as:

~~~text
http://127.0.0.1:3000/mcp/<64-hex-token>
https://example.ngrok.app/mcp/<different-64-hex-token>
~~~

Control them independently:

~~~bash
# local HTTP ephemeral, ngrok stable
abird-tunnel --http --ngrok --http-ephemeral-url

# local HTTP stable, ngrok ephemeral
abird-tunnel --http --ngrok --ngrok-ephemeral-url
~~~

Per-transport flags override the shorthand, including explicit false:

~~~bash
abird-tunnel --http --ngrok --ephemeral-url --http-ephemeral-url=false
abird-tunnel --http --ngrok --ephemeral-url --ngrok-ephemeral-url=false
~~~

Each ephemeral route is generated fresh at process start using two UUIDv4 values (~244 random bits). The ordinary /mcp path is not mounted for that transport when its ephemeral mode is enabled.

A hard-to-guess path is an additional obscurity layer, not authentication.

### ngrok public MCP endpoint

--ngrok enhances the HTTP transport; it is not another MCP transport.

Set the ngrok SDK token:

~~~bash
export NGROK_AUTHTOKEN='...'
~~~

Then:

~~~bash
abird-tunnel --http --ngrok
~~~

abird-tunnel starts the local Streamable HTTP MCP server, opens a public ngrok endpoint using the ngrok Rust SDK, and prints a URL such as:

~~~text
✓ ngrok MCP: https://example.ngrok.app/mcp
~~~

Any Streamable HTTP MCP client can connect directly to that public /mcp URL.

The public URL exposes whatever MCP permissions were granted to this abird-tunnel process. Treat it as sensitive or add appropriate ngrok access controls.

## Filesystem permissions

Permissions are additive. --deny always wins.

Default:

~~~text
--allow-read=<cwd>
~~~

Additional grants:

~~~bash
abird-tunnel --allow-read=/data/reference
abird-tunnel --allow-write=/data/output
abird-tunnel --allow-rw=/src/project
abird-tunnel --deny=/src/project/secrets
~~~

- --allow-read=DIR adds read permission.
- --allow-write=DIR adds write permission without adding read permission.
- --allow-rw=DIR adds both.
- --deny=PATH overrides matching allow rules.

Bare:

~~~bash
abird-tunnel --allow-write
~~~

is shorthand for read+write on cwd.

Relative MCP paths resolve from --cwd. Absolute paths work when granted. Existing paths and ancestors are canonicalized before Rust policy checks to prevent symlink escapes.

edit and patch_binary require read+write. write and write_binary require write only.

## Tool surface

Default:

~~~text
ls
read
read_binary
~~~

Any write grant adds:

~~~text
write
edit
write_binary
patch_binary
~~~

Shell adds one platform tool:

~~~text
bash         # Unix
powershell   # Windows
~~~

Inspect the visible tool surface without starting a transport:

~~~bash
abird-tunnel --list-tools
abird-tunnel --allow-write --list-tools
abird-tunnel --allow-shell --list-tools
~~~

## Text and binary tools

The Pi-like text tools stay simple:

~~~text
read
write
edit
ls
bash / powershell
~~~

Binary work is separate:

~~~text
read_binary
write_binary
patch_binary
~~~

read_binary supports:

~~~text
format=mcp      typed MCP image/audio/blob content
format=base64   base64 text
format=hex      hexadecimal text
~~~

format=mcp is the default. Images become MCP image content, audio becomes MCP audio content, and other binary files become MCP blob resources.

write_binary accepts encoding=base64|hex.

patch_binary works by byte offset and can replace, insert with length=0, or delete with an empty payload plus positive length.

## Linux shell sandbox

Enable shell explicitly:

~~~bash
abird-tunnel --allow-shell
~~~

On Linux, Bash runs inside Bubblewrap by default.

The sandbox:

- mounts readable grants read-only;
- mounts effective read+write grants read-write;
- does not expose purely write-only host paths to Bash;
- masks denied paths;
- masks the saved OpenAI tunnel runtime key when present;
- uses an empty temporary home;
- isolates PID, IPC, and UTS namespaces;
- blocks shell network access by default.

Enable network inside the sandbox with:

~~~bash
abird-tunnel --allow-shell --allow-network
~~~

This shell-network policy does not affect the main abird-tunnel process. OpenAI Tunnel and ngrok use outbound networking from that main process.

## Dangerous unsandboxed access

Unsandboxed shell execution inherently has the OS user's filesystem and network authority.

It therefore requires:

~~~bash
abird-tunnel \
  --allow-shell \
  --no-sandbox \
  --allow-rw-all-dangerous \
  --allow-network-dangereous
~~~

The corrected alias --allow-network-dangerous is also accepted.

The full escape hatch is:

~~~bash
abird-tunnel --allow-all-dangerous
~~~

That means unrestricted filesystem + unsandboxed shell + network.

--deny cannot constrain an unsandboxed shell, so that combination is rejected.

## OpenAI Secure MCP Tunnel

This setup is shown only when OpenAI transport is enabled.

Setup asks for:

1. a Runtime API key with Tunnels Read + Use;
2. an existing Tunnel ID, or a one-time Admin API key with Tunnels Manage;
3. a ChatGPT Workspace ID or OpenAI Organization ID when creating a tunnel.

The Admin key is never persisted.

The Runtime key is stored separately at:

~~~text
~/.config/abird-tunnel/runtime.key
~~~

When OpenAI transport is disabled, that key is not required and setup removes a previously saved runtime key.

Useful locations:

- Runtime API keys: https://platform.openai.com/settings/organization/api-keys
- Admin API keys: https://platform.openai.com/settings/organization/admin-keys
- ChatGPT Workspace ID: https://chatgpt.com/admin
- OpenAI Organization ID: https://platform.openai.com/settings/organization/general

## CLI summary

~~~text
abird-tunnel --setup           configure supported transports

--stdio                        start configured stdio MCP
--http                         start configured HTTP MCP
--http-bind=<ADDR>             override HTTP listen address
--ngrok                        publish --http through ngrok
--ephemeral-url                ephemeral local HTTP + ngrok paths
--http-ephemeral-url[=BOOL]    override local HTTP path behavior
--ngrok-ephemeral-url[=BOOL]   override ngrok path behavior

--cwd=<DIR>                    default cwd

--allow-read=<DIR>             add read
--allow-write=<DIR>            add write-only
--allow-rw=<DIR>               add read+write
--allow-write                  shorthand: rw cwd
--deny=<PATH>                  deny; always wins

--allow-shell                  add platform shell
--allow-network                network inside Linux shell sandbox
--no-sandbox                   disable Bubblewrap

--allow-rw-all-dangerous       unrestricted Rust filesystem tools
--allow-network-dangereous     acknowledge unsandboxed network
--allow-all-dangerous          unrestricted rw + network + unsandboxed shell

--list-tools                   show exposed tools
-v, --verbose                  concise request/tool logs
--print-id                     print configured OpenAI Tunnel ID
~~~

## Build

~~~bash
nix build
nix run .
nix flake check
nix develop
~~~

Inside nix develop:

~~~bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --all-features
~~~

The Linux Nix package includes Bubblewrap and Bash.

## Project layout

~~~text
src/
  main.rs
  mcp.rs
  setup.rs
  transports/
    mod.rs
    openai.rs
    stdio.rs
    http.rs
~~~

mcp.rs owns the permission-scoped tool implementation. Each transport is isolated in its own module.

## License

MIT.
