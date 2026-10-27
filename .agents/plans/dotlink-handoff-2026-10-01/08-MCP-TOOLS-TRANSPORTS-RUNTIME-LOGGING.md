# MCP tools, transports, runtime controls and logging

## MCP vocabulary

Current tool names are intentionally short and model-friendly.

Filesystem text:

```text
ls
read
write
edit
```

Binary:

```text
read_binary
write_binary
patch_binary
```

Shell:

```text
bash
powershell
```

Do not reintroduce historical `fs_*` prefixes.

## Tool discovery

`LocalMachine::policy_tool_router()` creates the visible tool surface from effective capability.

This is the single source of truth for advertised tools.

Rules:

- no read capability → hide `ls`, `read`, `read_binary`;
- no write capability → hide `write`, `write_binary`;
- no overlapping read+write capability → hide `edit`, `patch_binary`;
- no shell → hide both shell routes;
- Unix → expose Bash only;
- Windows → expose PowerShell only.

`--list-tools` uses the same policy router. Do not implement a second tool-visibility model.

## Text tools

### ls

- path defaults to `.`;
- read permission;
- `pattern` glob (`*`, `?`) filters entries at every level;
- `depth` bounds descent (default 1);
- max entries bounded; output is `{entries, truncated}`;
- does not recurse through symlinked directories;
- skips denied/protected entries.

### read

- UTF-8 regular file;
- read permission;
- bounded total file size;
- line-based offset/limit;
- line numbers by default (`numbered: false` for exact text).

### write

- write permission;
- missing target allowed;
- existing target must be regular file;
- parent directories may be created;
- write size bounded.

### edit

- read+write;
- existing regular UTF-8 file;
- exact old-text replacement;
- unique match required unless `replace_all=true`;
- result size bounded.

## Binary tools

### read_binary

Formats:

```text
mcp
base64
hex
```

`mcp`:

- entire file must fit read limit;
- image MIME → MCP image content;
- audio MIME → MCP audio content;
- otherwise MCP blob resource.

`base64` / `hex`:

- chunkable via offset/limit;
- returns encoded data + total/truncated metadata.

### write_binary

- base64 or hex input;
- decoded size bounded;
- same regular-or-missing write target rule.

### patch_binary

- read+write;
- existing regular file;
- byte offset;
- optional replaced length;
- supports replacement/insertion/deletion semantics;
- max patchable file size bounded.

## Operation gate

Tool dispatch uses one shared gate.

Concurrent pure reads:

```text
ls
read
read_binary
```

Exclusive:

```text
all mutators
all shell
unknown/future tools
```

Keep this at the dispatch boundary rather than duplicating locks in tool implementations.

## Shell execution

`ShellArgs` includes:

- command;
- directory;
- optional stdin;
- optional timeout;
- optional output limit.

Runtime limits clamp user requests to configured maxima.

Timeout is one absolute deadline across:

- stdin delivery;
- process execution;
- output collection.

Output is continuously drained with bounded retained bytes to avoid deadlock/unbounded memory.

Timeout/error explicitly kills/reaps child.

## Bubblewrap command

Linux shell path:

- resolved executable;
- Bubblewrap executable;
- runtime mounts;
- policy mounts;
- deny masks;
- private HOME/tmp;
- network namespace based on effective policy;
- cache mounts/env.

Keep `bubblewrap_command` as the one composition point.

## Transport supervisor

`src/transports/mod.rs` owns active transport orchestration.

`ActiveTransports` may contain:

- OpenAI;
- stdio;
- HTTP (which may include ngrok).

Cancellation token is shared.

Important behavior:

- clean termination of one peer does not necessarily terminate unrelated active peers incorrectly;
- shutdown cancels all;
- peers get bounded drain time;
- unresponsive tasks are aborted.

Tests cover both graceful drain and forced abort.

## OpenAI Tunnel

`src/transports/openai.rs`.

### Local bridge

The OpenAI transport embeds local Streamable HTTP MCP in-process and translates polled tunnel commands to local MCP requests.

It filters request/response headers.

Do not forward arbitrary headers blindly.

### Poll loop

Status classification:

- OK/no-content → healthy;
- newly created tunnel not ready → activation grace;
- transient network/429/timeout/5xx → retry;
- invalid credentials/missing established tunnel → fatal.

### Recovery

After 10 consecutive transient poll failures:

- emit typed `RuntimeRestartRequested`;
- stop transport runtime;
- top-level main loop reconstructs everything.

This is deliberate.

Do not turn the threshold back into endless HTTP-client retries.

### Concurrency

Tunnel command processing uses bounded semaphore/in-flight tasks.

Each polled request may have a response timeout from the tunnel contract.

Expired work is dropped rather than posting late.

### Logging

Normal warning:

- concise retry status.

Raw request URL/error detail:

- developer/debug logging only.

Avoid dumping sensitive request payloads.

## stdio

`src/transports/stdio.rs`.

Protocol rule:

> stdout belongs exclusively to MCP.

All human output/logs are stderr.

Request diagnostics inspect only the JSON-RPC method prefix/name, not payload content.

The logging reader bounds captured request prefix.

Runtime TTY key controls are disabled while stdio is active.

### Ctrl+C

stdio uses a platform-specific interrupt guard so manual terminal runs can still exit cleanly without corrupting inherited terminal behavior.

Ctrl+D remains normal stdin EOF.

## HTTP transport

`src/transports/http.rs`.

Uses rmcp `StreamableHttpService` + `LocalSessionManager`.

Local listener:

```text
/mcp
or /mcp/<ephemeral>
```

OAuth routing:

```text
OAuth public routes
+
MCP subrouter with bearer middleware
```

Bearer validation is outside/in front of rmcp.

### ngrok isolation

Ngrok gets a separate Axum router served directly on authenticated SDK tunnel connections.

Do not expose the local TCP router through a forwarding backend because:

- local/public path can differ;
- local/public OAuth can differ;
- separate exact resources improve token isolation;
- direct tunnel metadata preserves a trusted requester boundary without forwarded headers.

Network HTTP/ngrok retains Streamable HTTP SSE keepalives for intermediary idle timeouts. SEP-2243 network and embedded calls reject unknown policy tool names before rmcp's negative schema cache; HTTP preflight uses the same request-body ceiling. Only the in-process OpenAI adapter disables wire keepalive/retry events. Shutdown cancels route futures, force-closes accepted IO after the five-second graceful drain bound, and overlaps bounded ngrok tunnel/session close; the parent transport budget is six seconds. A drain timeout is cleanup failure, and cleanup failures take precedence over restart requests.

## Ephemeral paths

High-entropy URL-safe token.

Local and ngrok path policies independent.

Global shorthand:

```text
--ephemeral-url
```

Specific:

```text
--http-ephemeral-url[=BOOL]
--ngrok-ephemeral-url[=BOOL]
```

Specific wins.

Logs redact ephemeral path token to avoid accidental disclosure.

## Runtime controls

`src/controls.rs`.

Interactive non-stdio banner:

```text
Ctrl-C: exit · Ctrl-R: restart · v: verbosity
```

### Ctrl+C

Exit runtime.

Terminal state restored.

### Ctrl+R

Full runtime restart.

Not a partial config reload.

This reconstructs policy/transports and is also a useful way to invalidate in-memory OAuth access tokens.

### v

Cycle runtime verbosity:

```text
quiet
TOOL
TOOL + REQ
quiet
...
```

The state is shared, so cloned logger instances react immediately.

## Terminal-state safety

Runtime controls snapshot terminal state before entering raw input mode.

RAII restoration covers:

- clean exit;
- Ctrl+R;
- transport error;
- `?` early return;
- unwind/drop.

Raw input configuration preserves output flags so banners/logs do not become stair-stepped/garbled.

Do not detach terminal reader ownership from this guard.

## Logging

`src/logging.rs`.

### Levels

Default:

```text
quiet
```

`-v`:

```text
TOOL
```

`-vv`:

```text
TOOL + REQ
```

`-q`:

- suppress TOOL.

`-q -vv`:

- REQ only.

### TOOL log example

```text
[10:59:19.184] TOOL read → path=.agents/plans/HANDOFF.md
[10:59:19.194] TOOL read ← ok 9ms
```

Arguments are summarized.

Bulk content and grep patterns are represented as byte/encoded-character counts, not raw content. Log scopes are ASCII-sanitized and capped.

### REQ logging

Transport/request metadata:

- bounded/control-sanitized HTTP method/path/status, with ephemeral MCP paths redacted;
- stdio JSON-RPC method;
- strict bounded OpenAI MCP protocol labels.

No raw bodies. Control-plane response bodies and malformed local SSE payloads do not enter propagated errors. rmcp tracing is suppressed regardless of `RUST_LOG` because upstream debug events render complete payloads.

## Startup banner

Human output goes stderr when stdio is involved.

Banner summarizes:

- profile;
- transports;
- tunnel when applicable;
- HTTP URL;
- access summary;
- shell sandbox/network;
- ngrok/domain;
- OAuth state;
- status.

Do not write human status to stdout in stdio mode.

## Profile/runtime restart behavior

Manual Ctrl+R:

- reset restart backoff;
- full reconstruction.

OpenAI unhealthy runtime restart:

- typed error marker;
- top-level exponential restart delay;
- max 30 seconds;
- a runtime that had connected successfully resets the next backoff to 1 second.

## Testing expectations for transport/runtime changes

At minimum run:

```bash
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
```

If touching Bubblewrap:

```bash
DOTLINK_TEST_BWRAP=1 cargo test --locked --all-features mcp::tests::bubblewrap -- --nocapture
```

If touching stdio:

- verify stdout purity;
- test Ctrl+C/manual behavior if platform logic changed.

If touching OpenAI recovery:

- retain threshold/restart-marker/peer-drain/backoff tests.

If touching HTTP/OAuth:

- retain router metadata/401 and full DCR/PKCE/refresh/revocation tests.
