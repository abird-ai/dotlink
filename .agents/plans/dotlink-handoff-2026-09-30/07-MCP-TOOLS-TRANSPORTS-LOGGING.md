# MCP tools, transports and logging

## Tool design

The model-facing tool vocabulary intentionally follows the simplicity of Pi.

Text/local coding primitives:

```text
read
write
edit
ls
bash / powershell
```

Binary tools are separate:

```text
read_binary
write_binary
patch_binary
```

Do not restore the old `fs_*` naming unless the whole design is reconsidered.

## Text tools

### read

- UTF-8 text.
- line offset/limit.
- rejects binary and points caller to `read_binary`.

### write

- create/replace UTF-8 text.
- may create parent directories.

### edit

- exact-text replacement.
- focused changes preferred over full-file replacement.

### ls

- list allowed filesystem paths.
- no symlink-directory traversal.

## Binary tools

### read_binary

Formats:

```text
mcp
base64
hex
```

`mcp` default maps media/resources to typed MCP content:

- image MIME → MCP image;
- audio MIME → MCP audio;
- other binary → embedded blob resource.

### write_binary

Input encoding:

```text
base64
hex
```

### patch_binary

Byte-range API:

```text
offset
length
data
encoding
```

Supports replace, insert (`length=0`) and delete (empty replacement + positive length).

## Shell

Unix/Linux:

```text
bash
```

Windows:

```text
powershell
```

PowerShell preference:

```text
pwsh.exe
powershell.exe fallback
```

## Transport architecture

All transports serve the same `LocalMachine` policy core.

### OpenAI Secure MCP Tunnel

File:

```text
src/transports/openai.rs
```

Uses Tunnel ID + Runtime API key.

Intended for private/developer ChatGPT connection.

OpenAI-specific request logging in verbose mode uses the shared logging model.

Automatic recovery:

- transient poll failures retry with backoff;
- retries 1–9 stay within the current runtime;
- the 10th consecutive transient failure requests a full runtime restart;
- the shared supervisor cancels every transport and allows up to 5 seconds for graceful shutdown before force-aborting remaining tasks;
- the top-level runtime reloads the profile/policy, reconstructs `LocalMachine`, embedded MCP state and transports, then starts again;
- repeated unhealthy runtimes back off from 1 second up to a 30-second cap;
- a runtime that had successfully connected resets the restart backoff before a later recovery;
- fatal Tunnel ID/auth/control-plane failures remain fatal;
- normal WARN retry lines omit raw URL/error details; `-v` retains detailed DEBUG diagnostics.

### stdio

File:

```text
src/transports/stdio.rs
```

Use cases:

- Claude Desktop;
- Claude Code;
- local MCP clients that spawn a process.

Critical invariant:

```text
stdout = MCP protocol only
human output = stderr
```

Verbose stdio request observer extracts method name without dumping body.

### Streamable HTTP

File:

```text
src/transports/http.rs
```

Default endpoint:

```text
http://127.0.0.1:3000/mcp
```

Uses rmcp Streamable HTTP service/session management.

Verbose HTTP middleware logs:

```text
method
path
status
latency
```

not bodies.

### ngrok

ngrok is an HTTP publication option, not a fourth MCP transport.

```bash
dotlink --http --ngrok
```

Uses Rust ngrok SDK.

Prints final HTTPS MCP URL.

## Ephemeral paths

Global shorthand:

```text
--ephemeral-url
```

Overrides:

```text
--http-ephemeral-url[=BOOL]
--ngrok-ephemeral-url[=BOOL]
```

Per-transport override wins over global shorthand.

Local HTTP and ngrok can be independently stable/ephemeral.

When policies differ, ngrok uses a separate loopback-only backend so paths do not accidentally cross-expose.

## Transport lifecycle

Multiple transports may run together.

Clean stdio EOF must not stop active HTTP/OpenAI.

A transport error cancels peers.

Interactive non-stdio runs handle `Ctrl+C` as exit, `Ctrl+R` as full runtime restart, and `v` as live verbosity cycling; stdio disables runtime key capture to protect protocol stdin.

## Logging model

### Verbosity

Activity logging is quiet by default.

```text
default    no TOOL / REQ
-v         TOOL
-vv        TOOL + REQ
-s -vv     REQ only
```

TOOL lines are centralized around the MCP tool router, so all transports share the same format. REQ diagnostics stay at transport boundaries and include safe metadata only.

### Color

```text
--color=auto
--color=always
--color=never
```

`auto` follows whether stderr is interactive.

## ChatGPT integration

Documented path:

```text
ChatGPT Settings
→ Security and login
→ Developer mode
→ Plugins
→ +
→ Tunnel
→ select/paste tunnel ID
```

Detailed ChatGPT tunnel setup: `docs/CHATGPT_PLUGIN.md`.

## Claude.ai

Claude.ai remote connector requires public HTTPS MCP, not localhost.

Recommended:

```bash
dotlink --http --ngrok --ngrok-ephemeral-url
```

## Runtime transport override

Persisted transport booleans are defaults/preferences.

Explicit runtime:

```text
--stdio
--http
```

must activate for the current run even when persisted false.
