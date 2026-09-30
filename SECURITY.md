# Security model

dotlink is least-privilege by default.

Without write or shell flags, the visible tools are:

~~~text
ls
read
read_binary
~~~

and the launch directory is the only implicit readable directory. `--no-default-allow` removes even that grant.

## Profiles and permission precedence

Profiles persist `default_allow`, path grants/denies, shell/network policy, and transport/HTTP/ngrok defaults. Named profiles use config.<profile>.jsonc; the default uses config.jsonc. Schema v9 JSONC is the only supported profile format.

Runtime allow rules are additive. Denies always take precedence over profile defaults, ordinary allows, and developer-cache grants.

Filesystem controls are symmetric:

~~~text
--allow-read[=DIR]    --deny-read[=DIR]
--allow-write[=DIR]   --deny-write[=DIR]
--allow-rw[=DIR]      --deny-rw[=DIR]
~~~

Bare forms target the launch directory. Bare --allow-write means read+write on that directory. Legacy --deny=PATH is equivalent to denying both read and write.

Capability controls are also symmetric:

~~~text
--allow-shell      --deny-shell
--allow-network    --deny-network
~~~

`--allow-rw=/` is unrestricted filesystem read+write; no separate all-rw flag exists.

Rust file tools canonicalize existing targets and ancestors before policy checks so symlink traversal cannot escape a grant.

Tool requirements:

- read, read_binary, ls require read;
- write, write_binary require write;
- edit, patch_binary require read+write.

## Developer cache sharing

When shell is enabled, onboarding can autodiscover existing package/build caches and ask whether each cache should be shared with the sandbox as none, read-only, or read+write.

Supported cache families currently include Cargo registry/git, npm, pnpm, Yarn, pip, uv, Go module/build caches, Maven, Gradle, sccache, and ccache.

Cache grants are **shell-only**. They do not add the host cache path to the MCP filesystem allow-list, so `read`, `write`, `ls`, and related MCP tools cannot use cache sharing to inspect the user's home directory.

dotlink maps approved host caches into the sandbox's private home at the package manager's expected location. Only the cache directory itself is mounted; adjacent credential/config files such as `~/.cargo/credentials.toml`, `~/.cargo/config.toml`, or `~/.npmrc` are not included.

Read-only sharing protects host cache integrity but cannot populate cache misses. Read+write sharing provides the normal package-manager experience and lets successful downloads/builds be reused later, but sandboxed code can also modify those shared host cache contents. Choose RW only when that tradeoff is acceptable.

Filesystem deny rules remain authoritative: deny-read removes a matching cache mount and deny-write downgrades a matching RW cache to read-only.

## Transport security

### stdio

stdio is local subprocess IPC over stdin/stdout.

When --stdio is active, stdout is protocol-only. Status and diagnostics are written to stderr. Runtime key capture (`v` / `Ctrl+R`) is disabled whenever stdio is active, so dotlink never competes for protocol stdin. If stdio is launched manually on a TTY, dotlink temporarily ensures `Ctrl+C` is a real interrupt signal and restores the exact original terminal state when it exits.

### local HTTP

HTTP defaults to loopback:

~~~text
127.0.0.1:3000
~~~

Changing --http-bind to a non-loopback address can expose the MCP server to other machines on the network.

The HTTP transport does not add application-layer authentication by itself.

### Ephemeral URL paths

--ephemeral-url gives both local HTTP and ngrok fresh high-entropy MCP paths for the current process.

--http-ephemeral-url and --ngrok-ephemeral-url control the two transports independently and override the shorthand. Explicit =false is supported.

When a transport uses an ephemeral path, its normal /mcp route is not exposed by that transport.

The generated path is approximately 244 bits of randomness and is intended to make accidental discovery difficult. It is not a replacement for authentication or access control.

### ngrok

--ngrok creates a public HTTPS endpoint for the configured HTTP MCP server.

That endpoint exposes the same MCP tools and filesystem/shell permissions as the local server.

Anyone who can reach an unprotected public endpoint may be able to exercise those permissions.

Use ngrok access controls when appropriate and grant only the minimum abird filesystem/shell permissions required.

The ngrok SDK credential is read from NGROK_AUTHTOKEN. It authenticates dotlink to ngrok; it is not, by itself, authentication for MCP callers.

## OpenAI credentials

OpenAI credentials exist only when the OpenAI transport is enabled.

The Runtime key is stored separately for each profile:

~~~text
~/.config/abird/dotlink/runtime.key
~/.config/abird/dotlink/runtime.work.key
~~~

Canonical JSONC config files are config.jsonc or config.<profile>.jsonc. On Unix config/key files are mode 0600 and their directory is mode 0700.

The Admin key used to create a tunnel is never persisted. A profile with OpenAI disabled does not require a runtime key.

Known OpenAI/tunnel credential environment variables and NGROK_AUTHTOKEN are removed from child shell environments.

## Automatic transport recovery

Transient OpenAI tunnel failures can trigger an automatic full runtime restart after 10 consecutive failed polls. Recovery does not grant new authority: dotlink reloads the same selected profile and reapplies the same CLI permission/deny flags before reconstructing `LocalMachine`, cache mounts, MCP state, and transports.

All peer transports are cancelled during recovery. They receive up to 5 seconds to stop cleanly before remaining tasks are force-aborted. Repeated unhealthy runtimes use bounded restart backoff, while fatal authentication/tunnel errors remain fatal.

Recovery must never bypass path canonicalization, deny precedence, protected credential paths, Bubblewrap policy, or shell-network policy.

## Linux Bubblewrap shell

--allow-shell enables Bash on Unix.

On Linux it is Bubblewrap-sandboxed by default.

The sandbox:

- mounts readable grants read-only;
- mounts effective read+write grants read-write;
- does not expose purely write-only host grants;
- masks read-denied paths and rebinds write-denied readable subtrees read-only;
- masks the saved OpenAI runtime key when present;
- uses an empty temporary home, with only explicitly approved developer caches mounted back into package-manager-specific subdirectories;
- mounts required system runtime paths read-only;
- on NixOS, mounts the standard Nix store/profile symlink graph read-only (/nix/store, /run/current-system, /etc/profiles, /nix/var/nix/profiles, and ~/.nix-profile when present) without mounting the whole home directory;
- canonicalizes and filters PATH entries so only sandbox-visible tool directories remain;
- isolates PID, IPC, and UTS namespaces;
- unshares the network namespace by default.

The Nix daemon endpoint namespace is deliberately hidden by default when shell network is denied. dotlink masks the daemon endpoint roots (/nix/var/nix/daemon-socket and /run/nix-daemon) rather than assuming a particular socket filename or filesystem type. This prevents layout changes from accidentally exposing daemon-mediated builds or fetches outside the shell's direct filesystem/network isolation.

Enable shell network access with:

~~~text
--allow-network
~~~

The shell network namespace is separate from the main dotlink process. OpenAI and ngrok can use outbound networking even while the shell itself has no network.

## Full unsandboxed host mode

The only unsandboxed shell mode is:

~~~text
--allow-all
--no-sandbox
~~~

The flags require each other and grant root filesystem read+write, shell, and network with no Bubblewrap isolation. Deny rules are not accepted in this mode because an arbitrary unsandboxed child process cannot be constrained by Rust path checks.

For unrestricted filesystem access while keeping Linux sandboxing, use `--allow-rw=/` and add shell/network capabilities separately.

## Windows and macOS

Rust filesystem tools still enforce allow/deny policy on Windows and macOS.

Bubblewrap is Linux-only. Native Windows/macOS shell execution therefore requires `--allow-all --no-sandbox`.

For Windows, WSL2 is the recommended secure shell workflow: run dotlink inside WSL2 and use the normal Linux Bubblewrap sandbox instead of enabling unrestricted native PowerShell.

## Binary data

read_binary supports MCP typed media/blob content plus explicit base64 and hex.

write_binary and patch_binary enforce write limits.

patch_binary also caps total file size processed in memory.

## Terminal state safety

Interactive runtime controls snapshot the terminal state before enabling raw key input. On Unix, dotlink preserves the original output flags while controls are active and restores the complete captured `termios` state on teardown. The guard is RAII-owned by the runtime-control object, so normal exit, `Ctrl+R`, transport failures, `?` error propagation, and unwinding all restore the terminal before returning control to the shell. stdio never enables runtime key capture.

## Logging privacy

At `-v`, TOOL logs show safe metadata such as paths, directories, flags, status, latency, and timestamps. Content/data fields are summarized by size instead of printing file contents or binary payloads.

At `-vv` (or after cycling to TOOL + REQ with `v`), request-level transport/protocol metadata is added. HTTP logs method/path/status, stdio logs JSON-RPC method names, and OpenAI Tunnel logs MCP request labels. Raw request bodies, file contents, binary payloads, runtime keys, and other secrets are not intentionally emitted by the dotlink request logger.

`--quiet` suppresses TOOL activity. With `-vv`, it leaves REQ diagnostics visible while hiding TOOL lines.

## Resource limits

- tunnel concurrency is bounded;
- shell stdout/stderr are drained continuously with bounded retained output;
- text/binary reads and writes are bounded;
- shell calls have a timeout;
- HTTP and transport lifecycles share cancellation;
- recovery teardown waits at most 5 seconds before aborting unresponsive peer transport tasks;
- ngrok forwarding terminates with the HTTP transport/process lifecycle.

## Recommended use

Read-only local MCP:

~~~bash
dotlink --stdio
~~~

Writable local project:

~~~bash
dotlink --stdio --allow-write
~~~

Sandboxed build/test shell without shell network:

~~~bash
dotlink --stdio --allow-write --allow-shell
~~~

Local HTTP:

~~~bash
dotlink --http
~~~

Public HTTP through ngrok:

~~~bash
dotlink --http --ngrok
~~~

Use --allow-all --no-sandbox only when unrestricted host access is explicitly intended.
