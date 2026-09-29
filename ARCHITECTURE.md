# Architecture

## One process

`abird-tunnel` combines three layers in one Tokio process:

```text
┌───────────────────────────────────────────────────────────┐
│ abird-tunnel                                              │
│                                                           │
│  setup/config                                             │
│      │                                                    │
│      ├──────────────┐                                     │
│      ▼              ▼                                     │
│  tunnel client   LocalMachine                             │
│      │              │                                     │
│      │         rmcp tool router                           │
│      │              │                                     │
│      └──────► EmbeddedMcp                                 │
│              StreamableHttpService                        │
│              (in memory; no listener)                     │
└─────────────┬─────────────────────────────────────────────┘
              │
              │ HTTPS
              ▼
      OpenAI Secure MCP Tunnel
```

`src/tunnel.rs` turns each polled tunnel command into an in-memory HTTP request accepted by `rmcp::StreamableHttpService`. This deliberately reuses the official Rust MCP SDK's JSON-RPC/MCP behavior rather than reimplementing MCP method dispatch ourselves.

## Startup

1. Read environment configuration, or the saved config + runtime credential.
2. If neither exists, run interactive bootstrap.
3. Capture `--cwd`, or the process launch directory when the flag is absent, and canonicalize it as the filesystem workspace boundary.
4. Instantiate `LocalMachine` with that workspace and the filesystem/shell policy.
5. Instantiate a stateless `rmcp` Streamable HTTP service without binding a socket.
6. Begin Secure MCP Tunnel long-polling.
7. Print ready after the first successful control-plane poll.

## First-run bootstrap

If no Tunnel ID exists, setup can create one with `POST /v1/tunnels` using a one-time Admin key. The Admin key lives only in a `Zeroizing<String>` during setup and is not persisted.

The runtime key is saved separately from ordinary configuration.

## Tunnel lifecycle

The client polls:

```text
GET /v1/tunnels/{id}/poll?limit=25&timeout_ms=15000
```

A `204` is an empty successful poll. A `200` contains a command batch. The receipt time is captured immediately after response headers arrive and is used as the origin for `response_timeout` enforcement.

Known command types:

```text
jsonrpc
session_termination
```

Unknown types are logged and ignored rather than guessed from payload shape.

Results are posted to:

```text
POST /v1/tunnels/{id}/response
X-Tunnel-Shard-Token: <opaque token>
```

The shard token is never put in the JSON body.

## MCP dispatch

For each `jsonrpc` command the adapter builds an in-memory POST request with:

```text
Content-Type: application/json
Accept: application/json, text/event-stream
Host: localhost
```

Only MCP protocol headers are copied from the control-plane command.

`rmcp` returns either ordinary JSON or an SSE stream. Ordinary request/response tools use JSON because the embedded server is configured with `json_response=true`; SSE is parsed as a compatibility path for intermediate MCP notifications.

## Statelessness

The main channel uses `rmcp` stateless Streamable HTTP semantics and advertises that it accepts self-contained MCP `2026-07-28` requests. It also advertises process affinity because the local-machine binding itself is process/machine local.

The implementation intentionally does not advertise `wrong-cluster-v1` until that optional correction protocol is actually implemented.

## Backpressure

The tunnel can return up to 25 commands per poll, but that is not the execution concurrency. A Tokio semaphore caps actual work at eight simultaneous commands. Polling naturally backpressures when all permits are occupied.

## Deadlines

A valid tunnel `response_timeout` is parsed using the documented integer + unit grammar (`ns`, `us`, `ms`, `s`, `m`, `h`). Malformed/unknown/overflowing values fail open to legacy no-deadline behavior. Valid zero means immediate expiry.

The deadline spans local MCP work and the response POST. Late commands/results are dropped rather than synthesizing a late reply.

## Retry policy

Polls retry transient network failures, `408`, `429`, and server errors with bounded exponential backoff and jitter. `401`/`403` are fatal configuration/authentication failures.

Terminal response delivery retries transport failures plus the tunnel contract's transient HTTP statuses. Intermediate notifications are best-effort and are not blindly replayed.
