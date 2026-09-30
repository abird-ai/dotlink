# Product and architecture specification

## Product promise

`abird-link` is a single Rust binary that securely bridges AI/MCP clients to local files and optional command execution.

Primary headline:

> Connect ChatGPT, Claude.ai, and other MCP-capable AI tools to your computer files and shell securely — from a single binary.

## Target users

- ChatGPT users who want local project access without a public inbound port.
- Claude.ai users who can use remote MCP over HTTPS/ngrok.
- Claude Desktop / Claude Code / other local MCP users.
- Developers who want AI to inspect, edit, build and test local projects under explicit policy.
- Power users who want reusable trust profiles and sandbox controls.

## Core UX

The first-use path should be understandable without prior MCP knowledge.

Simple read-only use:

```bash
cd ~/src/project
abird-link
```

RW project:

```bash
abird-link --allow-rw
```

Sandboxed shell:

```bash
abird-link --allow-rw --allow-shell
```

Sandboxed shell with network:

```bash
abird-link --allow-rw --allow-shell --allow-network
```

## Product principles

### Least privilege

Default posture:

```text
launch-directory read    ON
launch-directory write   OFF
extra filesystem paths   OFF
shell                    OFF
shell network            OFF
shared developer caches  OFF
public HTTP ingress      OFF
unsandboxed host mode    OFF
```

### Deny wins

Deny rules override profile defaults and CLI grants wherever the runtime can technically enforce them.

### Tool surface mirrors authority

Do not advertise mutation tools to read-only sessions.

Do not advertise shell when shell authority is absent.

### Text and binary stay separate

Text primitives stay simple/Pi-like:

```text
read
write
edit
ls
bash / powershell
```

Binary is explicit:

```text
read_binary
write_binary
patch_binary
```

### Transport is not authority

Transport determines how the client reaches `abird-link`; it does not grant local capabilities.

OpenAI Tunnel, stdio and HTTP/ngrok must all terminate at one policy engine.

### Human-first onboarding

Onboarding should explain:

- what AI can read;
- what AI can write;
- whether shell is enabled;
- whether shell has network;
- whether default read access to the launch directory is enabled;
- which existing package caches are shared;
- whether OpenAI credentials/tunnel setup is needed.

### Reusable profiles

Profiles represent different trust contexts:

```text
default
work
personal
readonly
development
```

## High-level architecture

```text
              ChatGPT
                 |
       OpenAI Secure MCP Tunnel
                 |
                 v
Claude.ai / remote MCP       Local MCP clients
        |                    |           |
   HTTPS/ngrok             stdio     loopback HTTP
        |                    |           |
        +---------+----------+-----------+
                  |
             transport layer
                  |
                  v
           +---------------+
           |  LocalMachine |
           +---------------+
                  |
          policy-aware tool router
                  |
        +---------+-----------+
        |                     |
filesystem tools          shell tool
        |                     |
canonical policy        Bubblewrap Linux
        |                     |
        +---------+-----------+
                  |
                host
```

## Security-domain architecture

One process may expose multiple transports simultaneously, but they share:

- effective profile;
- launch-directory/base path;
- allow/deny roots;
- cache grants;
- shell policy;
- network policy;
- tool router.

Do not create separate implicit trust levels by transport.

## ChatGPT workflow

Current documented flow:

```text
abird-link --setup
→ select openai
→ create/select Secure MCP Tunnel
→ run abird-link
→ ChatGPT Settings → Security and login → Developer mode
→ Plugins → + → Tunnel
→ select/paste tunnel ID
→ review discovered tools
→ select Abird Link in chat
```

Optional private packaged plugin can use the developer connection and be invoked from Work via `@Abird Link`.

## Claude.ai workflow

Claude.ai cloud cannot reach localhost directly.

Use:

```bash
abird-link --http --ngrok --ngrok-ephemeral-url
```

Then register the printed HTTPS Streamable HTTP MCP URL as a custom connector.

## Local MCP workflow

stdio:

```bash
abird-link --stdio
```

Loopback HTTP:

```bash
abird-link --http
```

## Public URL principle

Ephemeral path is a capability-like hard-to-guess URL, not authentication.

Do not describe it as equivalent to auth.

## Full-host escape hatch

The deliberately loud full authority path is:

```bash
abird-link --allow-all --no-sandbox
```

This is conceptually distinct from:

```bash
abird-link --allow-rw=/ --allow-shell
```

which still uses the normal Linux sandbox model.

## Non-goals

- multi-user daemon architecture;
- automatic secret discovery;
- automatic shell/network escalation for build convenience;
- mounting the whole home directory for caches;
- exposing Nix daemon by default;
- treating cache sharing as MCP filesystem permission;
- using public ngrok endpoint without acknowledging transport exposure;
- reintroducing old `fs_*` API names;
- overloading text read/write with binary encoding modes.
