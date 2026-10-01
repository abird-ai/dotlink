# Product and architecture specification

## Product definition

**abird dotlink** is a single lightweight Rust binary that connects AI clients to files and tools on a user's computer under an explicit local permission boundary.

Primary surfaces:

- ChatGPT Web;
- ChatGPT Spaces;
- ChatGPT dots;
- Claude.ai;
- Claude Desktop / local MCP clients;
- Codex and other local MCP consumers;
- other remote Streamable HTTP MCP clients.

Primary product promise:

> Connect your ChatGPT dot, web and Spaces to the files and tools on your computer securely with a single command.

The product is intentionally not a desktop UI. It is a small local runtime/security layer.

## Product outcomes

dotlink should let a user:

- read/edit project files from ChatGPT;
- run Git/build/test/scripts when shell is explicitly allowed;
- use local project data without manual uploads/downloads;
- generate reports/decks/artifacts using live local inputs;
- keep ChatGPT/Spaces/dot context connected to the actual repository/toolchain;
- continue implementation from ChatGPT when Codex usage is unavailable/exhausted;
- use the same local policy through multiple MCP transports;
- expose remote HTTP safely without deploying a separate auth service.

## Non-goals

Current dotlink is not:

- a hosted multi-user service;
- a general identity provider;
- a remote desktop;
- a GUI;
- a whole-machine agent by default;
- a replacement for OS account isolation;
- a container/orchestrator;
- a package manager;
- a source-control system;
- a persistent cloud storage layer.

## Architectural principle

Transport and authority are independent.

```text
OpenAI Tunnel ─┐
stdio          ├──> same LocalMachine policy core
HTTP           │
ngrok/HTTP     ┘
```

No transport implicitly expands local authority.

## High-level architecture

```text
                         ┌─────────────────────┐
ChatGPT Web/Spaces/dot ──┤ OpenAI Secure MCP  │
                         │ Tunnel              │
                         └──────────┬──────────┘
                                    │
Claude Desktop/local MCP ─ stdio ───┤
                                    │
Remote MCP client ─ HTTPS/ngrok ────┤
                                    ▼
                           ┌───────────────────┐
                           │      dotlink      │
                           │                   │
                           │ transport adapters│
                           │ embedded OAuth    │
                           │ dynamic tool list │
                           │ profile/config    │
                           │ permission policy │
                           │ logging/controls  │
                           └─────────┬─────────┘
                                     │
                                     ▼
                           ┌───────────────────┐
                           │   LocalMachine    │
                           │                   │
                           │ files             │
                           │ optional shell    │
                           │ Bubblewrap/Linux  │
                           │ developer caches  │
                           └───────────────────┘
```

## Core layers

### 1. Configuration/profile layer

Owns:

- schema v10;
- setup;
- named profiles;
- secret separation;
- transport defaults;
- local permissions;
- developer cache grants;
- resource limits.

### 2. Effective-policy layer

Combines:

- profile defaults;
- CLI additions;
- CLI denies/suppressions;
- launch directory;
- platform restrictions.

Produces the effective `Policy` / `AccessSpec`.

### 3. LocalMachine

Owns:

- canonical/lexical path policy;
- dynamic MCP tool surface;
- filesystem operations;
- binary content;
- shell execution;
- Bubblewrap composition;
- operation serialization.

### 4. Transport adapters

OpenAI Tunnel, stdio and HTTP only translate transport/lifecycle concerns.

They do not contain local permission logic.

### 5. Embedded OAuth

Used only for HTTP-style remote ingress.

OAuth answers:

> Is this remote client allowed to enter this dotlink resource?

LocalMachine answers:

> What can the authenticated client actually do?

Do not collapse these two policy layers.

## Least-privilege default

Default authority:

```text
launch directory read    yes
write                    no
shell                    no
shell network            no
extra paths              no
developer caches         no
public ingress           no
unsandboxed mode         no
```

## Public ingress invariant

Externally reachable HTTP is safe-by-default.

Loopback:

```text
127.0.0.1 / ::1
→ may be unauthenticated
```

Direct non-loopback bind:

```text
OAuth + explicit HTTPS public origin
OR
--allow-public-no-auth (one run, explicit unsafe opt-out)
```

ngrok:

```text
OAuth automatically on
OR
--allow-public-no-auth
```

The unsafe flag is deliberately not persisted.

## Stable identity for remote OAuth

OAuth identity is bound to:

```text
issuer origin
resource URL
```

For ngrok, a durable remote connection therefore needs:

- stable/reserved ngrok hostname;
- stable MCP path (`/mcp`, not an ephemeral path).

If either changes, reconnect/re-authorize the client.

## Local filesystem design

All filesystem authority is expressed by allow/deny roots.

There is no persistent "cwd permission" concept.

The launch directory is:

- the base for relative paths;
- default readable root.

This reduces conceptual duplication and makes CLI/config merge cleanly.

## Shell design

Shell is distinct from filesystem MCP tools.

Linux:

- Bubblewrap by default;
- read/read-write mounts derived from policy;
- private temp/home;
- network off unless enabled;
- Nix runtime mounts as needed;
- cache mounts separate.

macOS/Windows:

- Rust filesystem tools remain permission constrained;
- native shell sandbox equivalent is not implemented;
- full host shell requires explicit `--allow-all --no-sandbox`;
- Windows recommendation: WSL2 + Linux/Bubblewrap for secure shell workflows.

## Developer cache model

Caches are a separate capability family from MCP filesystem roots.

Why:

- package managers need performance/reuse;
- exposing cache parents often exposes credentials/config;
- cache write integrity is a different risk from project filesystem authority.

Therefore:

```text
MCP read/write policy ≠ cache sharing policy
```

## Dynamic tool surface

The tool router is generated from effective authority.

This is intentional product behavior, not just optimization.

Benefits:

- less model ambiguity;
- tools/list communicates real capability;
- no misleading unusable tools;
- makes policy easier to audit.

## Runtime lifecycle

Top-level runtime is reconstructable.

A full restart rebuilds:

- setup/profile state;
- effective permissions;
- cache mounts;
- LocalMachine;
- OAuth runtime;
- transports;
- runtime controls.

This is why OpenAI health recovery uses full-runtime restart rather than attempting increasingly complex partial state repair.

## Failure model

Fatal:

- bad OpenAI credentials;
- missing configured tunnel outside activation grace;
- invalid config/schema;
- unsafe/noncomposable transport configuration;
- missing required sandbox executable.

Recoverable:

- repeated transient OpenAI poll failures;
- user Ctrl+R;
- transport peer teardown/restart.

## Maintenance principles

When extending dotlink:

1. Keep policy centralized.
2. Prefer additive capability models over special-case modes.
3. Keep transport adapters dumb about local authority.
4. Keep human setup simpler than the config model.
5. Make unsafe public behavior explicit and non-persistent.
6. Add new tools as exclusive/mutating by default until reviewed as pure reads.
7. Never make caches silently expand filesystem authority.
8. Keep secret storage out of JSONC.
9. Keep release target logic in Nix, not CI YAML.
10. Prefer invariants enforceable in one place over duplicated checks.

## Future modularization guidance

Current large modules (`oauth.rs`, `mcp.rs`, `setup.rs`) are behaviorally well-tested but sizable.

A future purely-structural refactor may split them, for example:

```text
src/oauth/
  mod.rs
  metadata.rs
  authorize.rs
  tokens.rs
  clients.rs
  state.rs
  security.rs

src/mcp/
  mod.rs
  policy.rs
  files.rs
  binary.rs
  shell.rs
  sandbox.rs

src/setup/
  mod.rs
  schema.rs
  profiles.rs
  wizard.rs
  caches.rs
  secrets.rs
```

Do not do this casually while changing behavior. If undertaken, preserve APIs/invariants and keep tests green after each extraction step.
