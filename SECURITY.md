# Security model

abird-link is least-privilege by default.

Without write or shell flags, the visible tools are:

~~~text
ls
read
read_binary
~~~

and cwd is the only implicit readable directory.

## Profiles and permission precedence

Profiles may persist allow_rw (rw-cwd) and allow_shell defaults. Named profiles use config.<profile>.json; the default uses config.json.

Runtime allow rules are additive. Denies always take precedence over profile defaults, ordinary allows, and dangerous grant shortcuts.

Filesystem controls are symmetric:

~~~text
--allow-read[=DIR]    --deny-read[=DIR]
--allow-write[=DIR]   --deny-write[=DIR]
--allow-rw[=DIR]      --deny-rw[=DIR]
~~~

Bare forms target cwd. Bare --allow-write retains its historical behavior and means rw-cwd. Legacy --deny=PATH is equivalent to denying both read and write.

Capability controls are also symmetric:

~~~text
--allow-shell             --deny-shell
--allow-network           --deny-network
--allow-rw-all-dangerous  --deny-rw-all-dangerous
~~~

Rust file tools canonicalize existing targets and ancestors before policy checks so symlink traversal cannot escape a grant.

Tool requirements:

- read, read_binary, ls require read;
- write, write_binary require write;
- edit, patch_binary require read+write.

## Transport security

### stdio

stdio is local subprocess IPC over stdin/stdout.

When --stdio is active, stdout is protocol-only. Status and diagnostics are written to stderr.

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

The ngrok SDK credential is read from NGROK_AUTHTOKEN. It authenticates abird-link to ngrok; it is not, by itself, authentication for MCP callers.

## OpenAI credentials

OpenAI credentials exist only when the OpenAI transport is enabled.

The Runtime key is stored separately for each profile:

~~~text
~/.config/abird-link/runtime.key
~/.config/abird-link/runtime.work.key
~~~

JSON config files are config.json or config.<profile>.json. On Unix config/key files are mode 0600 and their directory is mode 0700.

The Admin key used to create a tunnel is never persisted. A profile with OpenAI disabled does not require a runtime key.

Known OpenAI/tunnel credential environment variables and NGROK_AUTHTOKEN are removed from child shell environments.

## Linux Bubblewrap shell

--allow-shell enables Bash on Unix.

On Linux it is Bubblewrap-sandboxed by default.

The sandbox:

- mounts readable grants read-only;
- mounts effective read+write grants read-write;
- does not expose purely write-only host grants;
- masks read-denied paths and rebinds write-denied readable subtrees read-only;
- masks the saved OpenAI runtime key when present;
- uses an empty temporary home;
- mounts required system runtime paths read-only;
- on NixOS, mounts the standard Nix store/profile symlink graph read-only (/nix/store, /run/current-system, /etc/profiles, /nix/var/nix/profiles, and ~/.nix-profile when present) without mounting the whole home directory;
- canonicalizes and filters PATH entries so only sandbox-visible tool directories remain;
- isolates PID, IPC, and UTS namespaces;
- unshares the network namespace by default.

The Nix daemon socket is deliberately not exposed by default. Giving a sandboxed shell access to the host Nix daemon could bypass the intended filesystem/network isolation through daemon-mediated builds or fetches.

Enable shell network access with:

~~~text
--allow-network
~~~

The shell network namespace is separate from the main abird-link process. OpenAI and ngrok can use outbound networking even while the shell itself has no network.

## Dangerous unsandboxed shell

--no-sandbox does not grant shell access by itself.

Unsandboxed execution requires:

~~~text
--allow-shell
--no-sandbox
--allow-rw-all-dangerous
--allow-network-dangereous
~~~

The corrected alias --allow-network-dangerous is accepted.

The full grant shortcut is:

~~~text
--allow-all-dangerous
~~~

Explicit denies still win. On Linux, filesystem or network denies force the shell back into Bubblewrap so those denies remain enforceable even if --no-sandbox or --allow-all-dangerous was requested. deny-shell removes shell capability entirely. On platforms without an enforceable shell sandbox, shell + filesystem/network deny combinations are rejected.

## Windows

The Windows shell tool is powershell, preferring pwsh.exe and falling back to powershell.exe.

Bubblewrap is Linux-only, so Windows shell execution follows the explicit unsandboxed-dangerous requirements.

Rust filesystem tools still enforce allow/deny policy on Windows.

## Binary data

read_binary supports MCP typed media/blob content plus explicit base64 and hex.

write_binary and patch_binary enforce write limits.

patch_binary also caps total file size processed in memory.

## Resource limits

- tunnel concurrency is bounded;
- shell stdout/stderr are drained continuously with bounded retained output;
- text/binary reads and writes are bounded;
- shell calls have a timeout;
- HTTP and transport lifecycles share cancellation;
- ngrok forwarding terminates with the HTTP transport/process lifecycle.

## Recommended use

Read-only local MCP:

~~~bash
abird-link --stdio
~~~

Writable local project:

~~~bash
abird-link --stdio --allow-write
~~~

Sandboxed build/test shell without shell network:

~~~bash
abird-link --stdio --allow-write --allow-shell
~~~

Local HTTP:

~~~bash
abird-link --http
~~~

Public HTTP through ngrok:

~~~bash
abird-link --http --ngrok
~~~

Use --allow-all-dangerous only when unrestricted host access is explicitly intended.
