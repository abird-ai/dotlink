# Architecture

## Core split

dotlink is one Tokio process with a transport-neutral MCP core.

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

The default config is ~/.config/abird/dotlink/config.jsonc. Named profiles use config.<profile>.jsonc and are selected with -p/--profile. Setup can target the same profile with -S/--setup -p <name>. Schema v9 JSONC supports line/block comments and trailing commas.

Each profile persists transport behavior plus the local permission model:

~~~text
permissions.default_allow
permissions.allow_read[]
permissions.allow_write[]
permissions.allow_rw[]
permissions.deny_read[]
permissions.deny_write[]
permissions.deny_rw[]
permissions.allow_shell
permissions.allow_network

transports.openai
transports.stdio
transports.http
transports.http_bind
transports.http_ephemeral_url
transports.ngrok
transports.ngrok_ephemeral_url
~~~

The process launch directory is the internal relative-path base. It is readable by default when `permissions.default_allow=true`; `--no-default-allow` removes that implicit grant for one run. Relative profile/CLI paths resolve from the launch directory. `allow_rw: ["."]` means rw on the launch directory; `allow_rw: ["/"]` is unrestricted filesystem read+write.

allow_shell enables the normal platform shell. allow_network controls network access for the normal sandboxed shell path. On Linux Bubblewrap still applies unless --allow-all --no-sandbox is explicitly selected.

Every transport enabled in the selected profile starts automatically. Runtime flags add or suppress local transports for one run:

~~~text
--stdio / --no-stdio
--http  / --no-http
--ngrok / --no-ngrok
~~~

`--ngrok` modifies the HTTP transport; it is not a fourth MCP transport. HTTP bind and ephemeral-path flags override persisted HTTP/ngrok settings without requiring `--http` again when HTTP is already enabled in the profile.

A profile may configure no transport at all; explicit --stdio/--http can still activate local transports for a run.

## Runtime transport orchestration

main.rs builds one LocalMachine and one ActiveTransports plan.

Each active transport gets a clone of the same LocalMachine.

Clean termination of one transport does not terminate the others. For example, stdio EOF from a Claude subprocess session does not stop an active HTTP server or OpenAI Tunnel.

A transport error cancels the remaining active transports.

Interactive non-stdio runs also start a terminal-control loop: `v` mutates shared logging verbosity live, `Ctrl+R` cancels/drains the active transports and rebuilds the runtime from the current profile/CLI state, and `Ctrl+C` exits. stdio disables key capture so protocol stdin remains exclusive.

## stdio

stdio uses rmcp's standard stdin/stdout transport.

stdout is reserved for JSON-RPC protocol frames. All human-readable status is written to stderr. For manual stdio runs on a TTY, dotlink temporarily enables the terminal interrupt signal for `Ctrl+C`; piped stdio clients are untouched.

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
4. dotlink prints the public URL with /mcp appended.

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

### Recovery lifecycle

Transient tunnel poll failures are retried with per-poll backoff. At normal log level the retry message stays concise; the underlying request error and URL are DEBUG details available with `-v`.

After 10 consecutive transient poll failures, the OpenAI transport emits a typed runtime-restart request instead of retrying the same client/runtime state indefinitely. The shared transport supervisor cancels every active transport and gives peer tasks up to 5 seconds to shut down cleanly before force-aborting anything still running.

The top-level runtime then reloads the selected profile, reconstructs the permission policy, cache mounts, `LocalMachine`, embedded MCP service, and all configured transports. Repeated unhealthy runtimes restart with exponential backoff capped at 30 seconds. If a runtime had connected successfully before becoming unhealthy, the restart backoff resets to its initial 1-second delay.

Fatal control-plane failures, such as invalid credentials or a missing tunnel outside activation grace, remain fatal and do not enter this recovery loop.

## Permission model

The access policy contains:

~~~text
base_dir                  # internal relative-path base
read_roots[]
write_roots[]
deny_read_roots[]
deny_write_roots[]
unrestricted_fs
~~~

Rules:

- read succeeds when a read grant covers the canonical target and no read deny covers it;
- write succeeds when a write grant covers the canonical target and no write deny covers it;
- read+write operations require both capabilities;
- allow-read, allow-write, and allow-rw are additive;
- deny-read, deny-write, and deny-rw are additive and take precedence over grants;
- legacy --deny maps to both read and write deny sets;
- deny-shell and deny-network override profile defaults and runtime allows;
- unrestricted filesystem access is represented naturally by read+write grant `/`.

The base directory is where dotlink was launched. It is readable by default unless the profile sets `default_allow=false` or the run uses `--no-default-allow`.

Bare --allow-write and --allow-rw add rw permission to the launch/base directory. Bare deny-read/deny-write/deny-rw target that same directory symmetrically.

Existing paths are canonicalized before checks. Create targets canonicalize their nearest existing ancestor before the final path is checked.

Developer caches are modeled separately from filesystem grants. A cache record stores its tool family, host source path, and RO/RW mode. At runtime the source is canonicalized and mounted only into the shell sandbox, typically below `/tmp/home` at the tool's expected path. Cache mounts never expand the LocalMachine read/write roots. Read/write deny rules are re-applied to canonical cache sources before mounting.

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

On Linux, enabled shell runs inside Bubblewrap unless --allow-all --no-sandbox was explicitly selected.

Effective filesystem grants become mounts:

- readable only -> read-only bind;
- read+write -> writable bind;
- write-only -> not mounted into shell.

More-specific mounts may override broader mounts.

Read-denied paths are masked after allow mounts. Write-denied paths inside otherwise writable trees are rebound read-only, preserving reads while preventing writes.

The saved OpenAI runtime credential is masked when present.

Bubblewrap provides an empty temporary home and temporary directory. Approved developer caches are then mounted selectively into that private home, for example host `~/.cargo/registry` -> sandbox `/tmp/home/.cargo/registry`; tool-specific cache environment variables are set when useful.

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

The main dotlink process may still need outbound network access for:

- OpenAI Secure MCP Tunnel;
- ngrok SDK ingress.

## Unsandboxed shell

An unsandboxed shell cannot be constrained by Rust path checks, so dotlink exposes a single explicit full-host mode:

~~~text
--allow-all
--no-sandbox
~~~

The two flags require each other. This grants filesystem read+write `/`, shell, and network with no shell sandbox. Filesystem/network deny rules are rejected in this mode because they cannot constrain an arbitrary unsandboxed child process.

For unrestricted filesystem access while keeping Linux Bubblewrap, use the normal path model instead:

~~~text
--allow-rw=/
~~~

Then add --allow-shell and/or --allow-network separately.

## Logging model

Logging has two layers:

- normal activity logging is emitted centrally around the MCP tool router, so OpenAI, stdio, and HTTP all produce the same timestamped TOOL start/completion lines;
- REQ logging records incoming request metadata at the transport boundary without dumping request bodies.

Default activity logging is quiet. `-v` enables TOOL logs; `-vv` also enables REQ diagnostics. In interactive non-stdio runs, `v` cycles `quiet → TOOL → TOOL + REQ → quiet` immediately across all logger clones. `-q/--quiet` suppresses TOOL logs. `--color=auto|always|never` controls ANSI rendering; auto follows whether stderr is interactive.

## Build and release architecture

Nix builds use Crane. Each target has a separate dependency artifact derivation created with buildDepsOnly; final package/test/lint derivations import those Cargo artifacts instead of rebuilding dependencies after source-only changes.

Native outputs expose:

~~~text
deps
dotlink
~~~

On x86_64-linux, the release graph cross-builds every published binary:

~~~text
Linux x86_64   x86_64-unknown-linux-musl
Linux ARM64    aarch64-unknown-linux-musl
Windows x86_64 x86_64-pc-windows-gnu
Windows ARM64  aarch64-pc-windows-gnullvm
macOS ARM64    aarch64-apple-darwin
~~~

Each target exposes `cross-*-deps`, `cross-*`, and `dist-*` outputs. Linux is published only as generic static musl `dist-linux-*` artifacts; no distro-specific aliases are needed. `release-all` aggregates every published binary, checksum sidecar, `VERSION`, and `PLATFORMS.txt` into one derivation.

Windows ARM64 uses nixpkgs' pinned LLVM-MinGW/UCRT toolchain. macOS ARM64 uses the hashed Apple SDK 14.4 fetch derivation directly with Linux-hosted clang/ld64.lld and a macOS 11.0 deployment target, avoiding a host-specific Darwin build step.

Dist outputs use stable filenames plus SHA-256 sidecars so CI and local releases consume identical Nix results. `scripts/build-release-artifacts.sh` and the GitHub workflow both delegate target logic to `release-all`. `install.sh` selects Linux x86_64/ARM64 or macOS ARM64 automatically; `install.ps1` selects Windows x86_64/ARM64. The canonical upstream repository is `https://github.com/abird-ai/dotlink`; installers default to that repository while still allowing `DOTLINK_REPO` or a custom release base URL for forks and mirrors.

## Binary MCP content

read_binary(format=mcp) maps bytes to MCP typed content:

- image MIME -> image content;
- audio MIME -> audio content;
- other MIME -> embedded blob resource.

base64 and hex modes return encoded text.

write_binary decodes base64 or hex.

patch_binary edits a bounded byte range in a bounded file.
