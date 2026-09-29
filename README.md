# abird-link

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

The transports are modular: OpenAI, stdio, and HTTP can each be enabled or disabled in persisted setup. Profiles can also persist default rw-cwd and shell permissions.

## Setup and profiles

Run the default setup:

~~~bash
abird-link --setup
~~~

Or create a named profile:

~~~bash
abird-link --setup --profile work
abird-link -s -p work
~~~

Use it later with:

~~~bash
abird-link --profile work
abird-link -p work
~~~

Setup begins with:

~~~text
1. Choose MCP transports
   • openai — OpenAI Secure MCP Tunnel; starts automatically
   • stdio  — local stdio MCP server; start with --stdio
   • http   — local HTTP MCP server; start with --http
   • Enter comma-separated names, 'all', or 'none'.
   Enabled [openai]:

2. Choose default local permissions
   • cwd is always readable unless denied.
   Allow read+write cwd by default? [y/N]:
   Allow shell by default? [y/N]:
~~~

Examples:

~~~text
openai
stdio
http
stdio,http
openai,stdio,http
all
none
~~~

If OpenAI is not selected, every OpenAI credential/tunnel question is skipped and no OpenAI runtime key is required.

Configuration is JSON:

~~~text
~/.config/abird-link/config.json
~/.config/abird-link/config.work.json
~/.config/abird-link/config.personal.json
~~~

The default profile uses config.json. --profile work uses config.work.json.

OpenAI runtime keys are kept separately per profile:

~~~text
~/.config/abird-link/runtime.key
~/.config/abird-link/runtime.work.key
~~~

A config can persist these safe defaults:

~~~json
{
  "permissions": {
    "allow_rw": true,
    "allow_shell": true
  }
}
~~~

allow_rw means rw on the launch cwd. allow_shell enables the normal platform shell; on Linux it remains Bubblewrap-sandboxed and network remains disabled unless separately allowed.

## Transport activation

Configured OpenAI Tunnel starts automatically:

~~~bash
abird-link
~~~

stdio and HTTP start when explicitly requested:

~~~bash
abird-link --stdio
abird-link --http
abird-link --stdio --http
~~~

These runtime flags are authoritative additions: they start the requested transport even if that transport is disabled in persisted setup. Persisted transport selection acts as the default preference; explicit CLI flags override it for the current run.

If OpenAI is also enabled in persisted config, it runs alongside requested local transports.

### stdio

--stdio is the standard subprocess MCP transport for clients such as Claude Desktop, Claude Code, and other clients that launch an MCP server command.

Typical client shape:

~~~json
{
  "mcpServers": {
    "abird": {
      "command": "abird-link",
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
abird-link --http
~~~

Override the bind address for one run:

~~~bash
abird-link --http --http-bind=127.0.0.1:8080
~~~

HTTP-capable MCP clients connect directly to /mcp.

### Ephemeral HTTP paths

HTTP and ngrok can each use a fresh hard-to-guess MCP path for one process run.

Use the shorthand for both:

~~~bash
abird-link --http --ngrok --ephemeral-url
~~~

This gives local HTTP and ngrok independent fresh paths such as:

~~~text
http://127.0.0.1:3000/mcp/<64-hex-token>
https://example.ngrok.app/mcp/<different-64-hex-token>
~~~

Control them independently:

~~~bash
# local HTTP ephemeral, ngrok stable
abird-link --http --ngrok --http-ephemeral-url

# local HTTP stable, ngrok ephemeral
abird-link --http --ngrok --ngrok-ephemeral-url
~~~

Per-transport flags override the shorthand, including explicit false:

~~~bash
abird-link --http --ngrok --ephemeral-url --http-ephemeral-url=false
abird-link --http --ngrok --ephemeral-url --ngrok-ephemeral-url=false
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
abird-link --http --ngrok
~~~

abird-link starts the local Streamable HTTP MCP server, opens a public ngrok endpoint using the ngrok Rust SDK, and prints a URL such as:

~~~text
✓ ngrok MCP: https://example.ngrok.app/mcp
~~~

Any Streamable HTTP MCP client can connect directly to that public /mcp URL.

The public URL exposes whatever MCP permissions were granted to this abird-link process. Treat it as sensitive or add appropriate ngrok access controls.

## Filesystem and capability permissions

Allow rules are additive. Deny rules always take precedence over profile defaults and runtime allow flags.

The cwd is readable by default.

### Filesystem grants

~~~bash
abird-link --allow-read=/data/reference
abird-link --allow-write=/data/output
abird-link --allow-rw=/src/project
~~~

Bare forms use cwd:

~~~bash
abird-link --allow-read
abird-link --allow-write
abird-link --allow-rw
~~~

Bare --allow-write keeps its historical ergonomic behavior and means rw-cwd. With an explicit DIR, --allow-write=DIR is write-only. --allow-rw always grants both.

### Symmetric denies

~~~bash
abird-link --deny-read=/data/private
abird-link --deny-write=/src/generated
abird-link --deny-rw=/src/secret
abird-link --deny-shell
abird-link --deny-network
~~~

Bare filesystem deny forms use cwd:

~~~bash
abird-link --deny-read
abird-link --deny-write
abird-link --deny-rw
~~~

The older --deny=PATH remains a synonym for denying both read and write.

- --deny-read=DIR blocks reads while writes may still be allowed.
- --deny-write=DIR blocks writes while reads may still be allowed.
- --deny-rw=DIR blocks both.
- --deny-shell hides the shell even if the profile or --allow-all-dangerous enabled it.
- --deny-network keeps shell networking off.

On Linux, filesystem or network denies force shell execution into Bubblewrap even if --no-sandbox or --allow-all-dangerous was also requested, because the sandbox is required to enforce those denies. On systems without an enforceable shell sandbox, shell + deny combinations are rejected.

Filesystem denies also constrain --allow-rw-all-dangerous and --allow-all-dangerous.

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
abird-link --list-tools
abird-link --allow-write --list-tools
abird-link --allow-shell --list-tools
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
abird-link --allow-shell
~~~

On Linux, Bash runs inside Bubblewrap by default.

The sandbox:

- mounts readable grants read-only;
- mounts effective read+write grants read-write;
- does not expose purely write-only host paths to Bash;
- masks denied paths;
- masks the saved OpenAI tunnel runtime key when present;
- uses an empty temporary home;
- on NixOS, mounts the Nix store/profile graph read-only (`/nix/store`, `/run/current-system`, `/etc/profiles`, `/nix/var/nix/profiles`, and `~/.nix-profile` when present) so Bash, Cargo, Git, Rust and other profile-provided tools remain executable without exposing the whole home directory;
- canonicalizes and filters PATH to directories that are actually visible in the sandbox;
- isolates PID, IPC, and UTS namespaces;
- blocks shell network access by default.

The host Nix daemon socket is not mounted by default, because daemon-mediated builds/fetches could bypass the shell sandbox's direct filesystem/network restrictions.

Enable network inside the sandbox with:

~~~bash
abird-link --allow-shell --allow-network
~~~

This shell-network policy does not affect the main abird-link process. OpenAI Tunnel and ngrok use outbound networking from that main process.

## Dangerous unsandboxed access

Unsandboxed shell execution inherently has the OS user's filesystem and network authority.

It therefore requires:

~~~bash
abird-link \
  --allow-shell \
  --no-sandbox \
  --allow-rw-all-dangerous \
  --allow-network-dangereous
~~~

The corrected alias --allow-network-dangerous is also accepted.

The full escape hatch is:

~~~bash
abird-link --allow-all-dangerous
~~~

That is a grant shortcut for unrestricted filesystem + unsandboxed shell + network. Explicit deny flags still win. On Linux, a filesystem/network deny automatically restores Bubblewrap so the deny can be enforced; --deny-shell disables shell entirely.

## OpenAI Secure MCP Tunnel

This setup is shown only when OpenAI transport is enabled.

Setup asks for:

1. a Runtime API key with Tunnels Read + Use;
2. an existing Tunnel ID, or a one-time Admin API key with Tunnels Manage;
3. a ChatGPT Workspace ID or OpenAI Organization ID when creating a tunnel.

The Admin key is never persisted.

The Runtime key is stored separately from JSON config and follows the selected profile:

~~~text
~/.config/abird-link/runtime.key
~/.config/abird-link/runtime.work.key
~~~

When OpenAI transport is disabled for a profile, that profile does not require a runtime key.

Useful locations:

- Runtime API keys: https://platform.openai.com/settings/organization/api-keys
- Admin API keys: https://platform.openai.com/settings/organization/admin-keys
- ChatGPT Workspace ID: https://chatgpt.com/admin
- OpenAI Organization ID: https://platform.openai.com/settings/organization/general

## CLI summary

~~~text
abird-link -s, --setup          interactive setup for selected profile
-p, --profile <NAME>            use config.<NAME>.json

--stdio                         start stdio MCP for this run
--http                          start HTTP MCP for this run
--http-bind=<ADDR>              override HTTP listen address
--ngrok                         publish --http through ngrok
--ephemeral-url                 ephemeral local HTTP + ngrok paths
--http-ephemeral-url[=BOOL]     override local HTTP path behavior
--ngrok-ephemeral-url[=BOOL]    override ngrok path behavior

--cwd=<DIR>                     default cwd

--allow-read[=<DIR>]            add read; bare means cwd
--allow-write[=<DIR>]           bare: rw cwd; with DIR: write-only
--allow-rw[=<DIR>]              add rw; bare means cwd

--deny-read[=<DIR>]             deny read; bare means cwd
--deny-write[=<DIR>]            deny write; bare means cwd
--deny-rw[=<DIR>]               deny rw; bare means cwd
--deny=<PATH>                   legacy synonym for deny-rw
--deny-shell                    deny shell; always wins
--deny-network                  deny shell network; always wins
--deny-rw-all-dangerous         cancel unrestricted filesystem grant

--allow-shell                   add platform shell
--allow-network                 network inside Linux shell sandbox
--no-sandbox                    disable Bubblewrap when no deny requires it

--allow-rw-all-dangerous        unrestricted Rust filesystem grant
--allow-network-dangereous      acknowledge unsandboxed network
--allow-all-dangerous           unrestricted rw + shell + network grant shortcut

--list-tools                    show exposed tools
-v, --verbose                   concise request/tool logs
--print-id                      print configured OpenAI Tunnel ID
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
