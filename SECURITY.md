# Security model

abird-tunnel is least-privilege by default.

Without write or shell flags, the visible tools are:

~~~text
ls
read
read_binary
~~~

and cwd is the only implicit readable directory.

## Filesystem grants

Launch-time permissions:

~~~text
--allow-read=DIR
--allow-write=DIR
--allow-rw=DIR
--deny=PATH
~~~

Allow rules are additive.

deny always takes precedence.

Bare --allow-write means read+write cwd.

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

### ngrok

--ngrok creates a public HTTPS endpoint for the configured HTTP MCP server.

That endpoint exposes the same MCP tools and filesystem/shell permissions as the local server.

Anyone who can reach an unprotected public endpoint may be able to exercise those permissions.

Use ngrok access controls when appropriate and grant only the minimum abird filesystem/shell permissions required.

The ngrok SDK credential is read from NGROK_AUTHTOKEN. It authenticates abird-tunnel to ngrok; it is not, by itself, authentication for MCP callers.

## OpenAI credentials

OpenAI credentials exist only when the OpenAI transport is enabled.

The Runtime key is stored at:

~~~text
~/.config/abird-tunnel/runtime.key
~~~

On Unix it is mode 0600 and its directory is mode 0700.

The Admin key used to create a tunnel is never persisted.

When OpenAI is disabled in setup, the Runtime key is not required and a previously saved runtime.key is removed.

Known OpenAI/tunnel credential environment variables and NGROK_AUTHTOKEN are removed from child shell environments.

## Linux Bubblewrap shell

--allow-shell enables Bash on Unix.

On Linux it is Bubblewrap-sandboxed by default.

The sandbox:

- mounts readable grants read-only;
- mounts effective read+write grants read-write;
- does not expose purely write-only host grants;
- masks deny paths;
- masks the saved OpenAI runtime key when present;
- uses an empty temporary home;
- mounts required system runtime paths read-only;
- isolates PID, IPC, and UTS namespaces;
- unshares the network namespace by default.

Enable shell network access with:

~~~text
--allow-network
~~~

The shell network namespace is separate from the main abird-tunnel process. OpenAI and ngrok can use outbound networking even while the shell itself has no network.

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

The full shortcut is:

~~~text
--allow-all-dangerous
~~~

deny rules cannot constrain an arbitrary unsandboxed child process, so unsandboxed shell plus deny is rejected.

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
abird-tunnel --stdio
~~~

Writable local project:

~~~bash
abird-tunnel --stdio --allow-write
~~~

Sandboxed build/test shell without shell network:

~~~bash
abird-tunnel --stdio --allow-write --allow-shell
~~~

Local HTTP:

~~~bash
abird-tunnel --http
~~~

Public HTTP through ngrok:

~~~bash
abird-tunnel --http --ngrok
~~~

Use --allow-all-dangerous only when unrestricted host access is explicitly intended.
