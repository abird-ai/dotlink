# Architecture

## Core split

abird-link is one Tokio process with a transport-neutral MCP core.

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

The LocalMachine implementation and its dynamic tool router live in src/mcp.rs.

Transport code is isolated under:

~~~text
src/transports/
  mod.rs
  openai.rs
  stdio.rs
  http.rs
~~~

No transport owns filesystem policy.

## Profiles and persisted defaults

The default config is ~/.config/abird-link/config.json. Named profiles use config.<profile>.json and are selected with -p/--profile. Setup can target the same profile with -s/--setup -p <name>.

Each profile persists transport support plus safe local defaults:

~~~text
permissions.allow_rw
permissions.allow_shell
~~~

allow_rw means rw on the launch cwd. allow_shell enables the normal platform shell; Linux still applies Bubblewrap and leaves network disabled by default.

Setup also persists three independent transport booleans:

~~~text
openai
stdio
http
~~~

OpenAI starts automatically when configured.

stdio and HTTP are opt-in at runtime:

~~~text
--stdio
--http
~~~

Persisted transport booleans are defaults/preferences, not hard runtime gates. Explicit --stdio and --http flags add those transports for the current run even when persisted setup has them disabled.

--ngrok modifies the HTTP transport; it is not a fourth MCP transport.

A profile may configure no transport at all; explicit --stdio/--http can still activate local transports for a run.

## Runtime transport orchestration

main.rs builds one LocalMachine and one ActiveTransports plan.

Each active transport gets a clone of the same LocalMachine.

Clean termination of one transport does not terminate the others. For example, stdio EOF from a Claude subprocess session does not stop an active HTTP server or OpenAI Tunnel.

A transport error cancels the remaining active transports.

Ctrl-C cancels the shared root cancellation token.

## stdio

stdio uses rmcp's standard stdin/stdout transport.

stdout is reserved for JSON-RPC protocol frames. All human-readable status is written to stderr.

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

HTTP uses LocalSessionManager for normal Streamable HTTP sessions.

## Independent ephemeral HTTP routes

The HTTP transport resolves two route policies:

~~~text
http_ephemeral_url
ngrok_ephemeral_url
~~~

--ephemeral-url sets both to true by default.

--http-ephemeral-url and --ngrok-ephemeral-url are per-transport overrides and accept explicit =false.

Local HTTP and ngrok are genuinely isolated: ngrok uses a second loopback-only MCP backend listener. This lets one side expose /mcp while the other uses /mcp/<random-token> without accidentally mounting both routes on the same externally reachable listener.

Each ephemeral path is generated independently on process start.

## ngrok

When --http --ngrok is selected:

1. the local HTTP MCP listener is bound first;
2. the ngrok Rust SDK opens a public HTTP endpoint;
3. ngrok forwards that endpoint to the local HTTP listener;
4. abird-link prints the public URL with /mcp appended.

The public URL speaks ordinary MCP Streamable HTTP. Clients connect directly to it.

ngrok uses NGROK_AUTHTOKEN through the SDK's authtoken-from-environment flow.

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

## Permission model

The access policy contains:

~~~text
cwd
read_roots[]
write_roots[]
deny_read_roots[]
deny_write_roots[]
rw_all_dangerous
~~~

Rules:

- read succeeds when a read grant covers the canonical target and no read deny covers it;
- write succeeds when a write grant covers the canonical target and no write deny covers it;
- read+write operations require both capabilities;
- allow-read, allow-write, and allow-rw are additive;
- deny-read, deny-write, and deny-rw are additive and take precedence over grants;
- legacy --deny maps to both read and write deny sets;
- deny-shell and deny-network override profile defaults and runtime allows;
- deny-rw-all-dangerous cancels the unrestricted filesystem grant, including the filesystem portion of allow-all-dangerous.

The effective cwd is readable by default. A profile may also add write-cwd and shell defaults.

Bare --allow-write and --allow-rw add rw permission to cwd. Bare deny-read/deny-write/deny-rw target cwd symmetrically.

Existing paths are canonicalized before checks. Create targets canonicalize their nearest existing ancestor before the final path is checked.

## Dynamic tool router

The implementation defines:

~~~text
read
write
edit
ls
read_binary
write_binary
patch_binary
bash
powershell
~~~

The visible router is policy-specific.

Default:

~~~text
ls
read
read_binary
~~~

Write tools are hidden if no write capability exists.

Shell is hidden unless explicitly enabled.

Only bash is exposed on Unix and only powershell on Windows.

## Linux Bubblewrap mapping

On Linux, enabled shell runs inside Bubblewrap unless dangerous unsandboxed access was explicitly selected.

Effective filesystem grants become mounts:

- readable only -> read-only bind;
- read+write -> writable bind;
- write-only -> not mounted into shell.

More-specific mounts may override broader mounts.

Read-denied paths are masked after allow mounts. Write-denied paths inside otherwise writable trees are rebound read-only, preserving reads while preventing writes.

The saved OpenAI runtime credential is masked when present.

Bubblewrap provides an empty temporary home and temporary directory.

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

## Network isolation

Sandboxed shell starts with an unshared network namespace.

--allow-network restores host network access for the shell child.

This policy applies only to the shell child.

The main abird-link process may still need outbound network access for:

- OpenAI Secure MCP Tunnel;
- ngrok SDK ingress.

## Unsandboxed shell

An unsandboxed shell cannot be constrained by Rust path checks.

Therefore it requires explicit unrestricted filesystem and network acknowledgements:

~~~text
--allow-shell
--no-sandbox
--allow-rw-all-dangerous
--allow-network-dangereous
~~~

--allow-all-dangerous is the full grant shorthand.

Explicit denies still win. On Linux, a filesystem or network deny forces shell execution back into Bubblewrap even if --no-sandbox or --allow-all-dangerous was requested. On platforms without an enforceable shell sandbox, shell + filesystem/network deny combinations are rejected. deny-shell simply removes shell capability.

## Binary MCP content

read_binary(format=mcp) maps bytes to MCP typed content:

- image MIME -> image content;
- audio MIME -> audio content;
- other MIME -> embedded blob resource.

base64 and hex modes return encoded text.

write_binary decodes base64 or hex.

patch_binary edits a bounded byte range in a bounded file.
