# Context and history

## Why dotlink exists

The project began as a small local Rust MCP bridge whose main job was to let ChatGPT reach files and tools on the user's computer through OpenAI's Secure MCP Tunnel.

The original concept was intentionally minimal:

- one binary;
- no GUI;
- current directory as the local project boundary;
- small Pi-like MCP tool vocabulary;
- direct OpenAI tunnel support;
- optional local stdio/HTTP MCP;
- explicit local permissions;
- Linux Bubblewrap for shell isolation.

The project evolved quickly from a tunnel client into a reusable **local policy/security boundary with multiple MCP transports**.

## Naming evolution

Historical names:

```text
abird-tunnel
abird-link
```

Current canonical naming:

```text
human-facing title: abird dotlink
crate/binary/MCP identity: dotlink
repo: abird-ai/dotlink
```

The rename to dotlink was chosen to align with ChatGPT's dot concept while keeping Abird branding at the title/product level.

Do not reintroduce the old names in current CLI/config/release paths except when documenting history.

## Product narrative that shaped the README

The user wanted the README to sell a simple practical idea without hype:

> Connect your ChatGPT dot, web and Spaces to the files and tools on your computer, securely with a single command.

Important product messages retained through the redesign:

- let **your ChatGPT Web**, **your dot**, and **your computer** work together under explicit local authority;
- use local files/tools without repeated upload/download/copy/paste loops;
- build, test and run tools on the machine from ChatGPT Web;
- keep project context connected between ChatGPT/Spaces, dot, and the live repository/toolchain;
- use ChatGPT as a practical continuation/fallback when Codex usage is unavailable or exhausted;
- work against cached ChatGPT context while the computer/tunnel is offline, then reconnect and re-read live state later;
- support Claude.ai and other MCP clients without making them the primary marketing story;
- emphasize direct OpenAI Tunnel + local MCP + Bubblewrap security for ChatGPT;
- keep the tone professional, engineering-focused and succinct.

## Initial tool model

The project deliberately copied the simple Pi-style mental model instead of exposing internal implementation names.

Text/filesystem tools:

```text
ls
read
write
edit
```

Shell:

```text
bash        Unix
powershell  Windows
```

Binary operations were kept separate rather than adding encodings/modes to text tools:

```text
read_binary
write_binary
patch_binary
```

This remains a strong project principle.

## Why the permission model changed

Early versions treated a working directory as a first-class permission concept. That proved unnecessarily confusing.

The design was simplified to:

- one internal launch/base directory for relative path resolution;
- default read grant to that launch directory;
- all real authority expressed through the same allow/deny path sets;
- `--no-default-allow` removes the implicit launch-directory read;
- no independent persistent "cwd authority" concept.

This made config and CLI compose naturally.

Filesystem authority became:

```text
allow-read
allow-write
allow-rw

deny-read
deny-write
deny-rw
```

Denies win.

Bare `--allow-write` is intentionally ergonomic and means read+write on the launch directory. Explicit `--allow-write=/path` remains write-only.

`--allow-rw=/` naturally means unrestricted filesystem read/write. There is no separate "all-rw" concept.

Full host authority remains intentionally loud:

```bash
dotlink --allow-all --no-sandbox
```

The two flags require each other.

## Dynamic MCP tool surface

The project deliberately does not always advertise every tool and reject unauthorized calls later.

The runtime policy controls `tools/list`:

- read tools appear only with effective read authority;
- write tools appear only with effective write authority;
- edit/patch require overlapping read+write capability;
- shell appears only when enabled.

This reduces model confusion and makes the exposed MCP surface reflect real authority.

## Bubblewrap evolution

Linux shell sandboxing became a major design area.

Core model:

- shell off by default;
- Bubblewrap required for normal Linux shell access;
- network off by default;
- only explicit filesystem grants become mounts;
- read mounts RO;
- read+write mounts RW;
- write-only paths intentionally are not mounted into the shell;
- denies remask/downgrade mounts.

NixOS forced several important hardening decisions.

To make developer tooling work without exposing home:

- mount `/nix/store` read-only;
- expose the relevant Nix profile/runtime graph read-only;
- do not mount the entire home directory.

A real environment bug showed that presumed Nix-daemon socket paths can actually be directories. The sandbox therefore masks daemon endpoint **roots** in a type-aware way instead of assuming a socket leaf.

When shell network is denied, Nix daemon endpoint namespaces are hidden so a sandboxed process cannot use the daemon as an indirect privileged/network capability.

## Developer cache sharing

Package/build caches were introduced to make sandboxed build workflows practical without exposing the user's home.

The key decision:

> caches are shell-only mounts, not MCP filesystem grants.

Supported families include Cargo, npm, pnpm, Yarn, pip, uv, Go, Maven, Gradle, sccache and ccache.

Cache modes:

```text
none
read_only
read_write
```

The sandbox maps approved cache directories under its private home (`/tmp/home`) at the tool's expected locations.

Adjacent credentials/config files are not mounted.

Deny rules remain authoritative:

- deny-read removes a matching cache mount;
- deny-write downgrades matching RW cache to RO.

## Profiles and setup evolution

Profiles were added because one permission/transport configuration is not enough for multiple projects/trust contexts.

Default:

```text
config.jsonc
runtime.key
oauth.json
```

Named profile:

```text
config.work.jsonc
runtime.work.key
oauth.work.json
```

All dotlink-owned config/state paths live under the shared Abird namespace:

```text
$XDG_CONFIG_HOME/abird/dotlink
$XDG_STATE_HOME/abird/dotlink
```

Setup was made:

- interactive;
- colored unless disabled;
- profile-aware;
- transactional;
- default-aware on re-run;
- safe for hidden secrets;
- cancellable without saving.

The transport selector became compact and numbered:

```text
1 OpenAI Tunnel
2 stdio
3 HTTP
all
none/cancel
```

A zero/none/cancel selection during setup cancels without saving rather than persisting a broken profile.

## Logging and terminal controls

The logging model changed several times before settling on:

```text
default     quiet
-v          TOOL activity
-vv         TOOL + REQ diagnostics
-q          suppress TOOL activity
-q -vv      REQ only
```

Interactive non-stdio runtime controls:

```text
Ctrl-C  exit
Ctrl-R  restart/reload
v       cycle quiet → TOOL → TOOL+REQ → quiet
```

The terminal reader uses the real terminal device rather than MCP stdin, and it is disabled when stdio is active so protocol bytes are never stolen.

Terminal state restoration became RAII-owned after several edge cases involving raw mode and exceptional shutdown.

## OpenAI Tunnel recovery

A repeated real-world failure mode showed the tunnel poll loop could remain unhealthy indefinitely.

Current recovery model:

- transient poll failures retry;
- after 10 consecutive transient poll failures, the OpenAI transport requests a **full runtime restart**;
- all transports are cancelled;
- peers get up to 5 seconds to drain;
- remaining tasks are force-aborted;
- profile/policy/LocalMachine/transports are reconstructed;
- unhealthy restarts back off 1s → capped 30s;
- successful prior connectivity resets restart backoff;
- fatal auth/missing-tunnel failures remain fatal.

This is intentionally full-runtime recovery, not merely rebuilding the HTTP client.

## Rebrand and marketing cleanup

The repository and upstream moved to:

```text
https://github.com/abird-ai/dotlink
```

The README was repeatedly shortened and refocused around:

- ChatGPT Web / Spaces / dots;
- direct local files/tools;
- local data without upload/download loops;
- project-context continuity;
- security and Bubblewrap;
- Claude.ai and generic MCP as secondary interoperability.

The current README should be treated as the marketing source of truth.

## Build/release evolution

The build architecture moved to Crane with a dependency layer that can be cached independently.

The goal became:

> one x86_64 Linux Nix host can reproducibly build every published binary.

Current published target set:

```text
Linux x86_64   static musl
Linux ARM64    static musl
Windows x86_64
Windows ARM64
macOS ARM64
```

NixOS and Debian aliases were deliberately removed because the static musl Linux artifact is distro-independent.

Windows ARM64 uses LLVM-MinGW/UCRT.

macOS ARM64 is cross-built from Linux using a pinned Apple SDK and LLVM/ld64.lld, avoiding a macOS-specific CI build host.

## GitHub releases and installers

The GitHub workflow is intentionally thin:

```bash
nix build .#release-all
```

Target/toolchain logic remains in `flake.nix`.

For `v*` tags:

- CI rebuilds every binary;
- validates checksums/version;
- tag must equal `v$(cat VERSION)`;
- GitHub Release is created/updated;
- each binary and checksum sidecar is uploaded as an individual release asset;
- the Actions ZIP is only an internal CI artifact.

Installers were changed into idempotent updaters.

Default behavior:

- resolve latest GitHub Release;
- detect platform/architecture internally;
- download the architecture-specific asset;
- verify SHA-256;
- execute `--version` to validate identity;
- install under the simple command name `dotlink` / `dotlink.exe`;
- no-op if already identical;
- replace atomically/same-directory when updating.

## Why OAuth was added

Public HTTP/ngrok originally relied on obscurity or external controls. The user explicitly requested full OAuth for a **single self-hosted owner** so Claude.ai, ChatGPT remote MCP, and other web clients could connect safely without a second auth service.

The chosen architecture is an embedded OAuth authorization server + resource server in the same binary.

Why embedded is acceptable here:

- exactly one owner;
- no signup;
- no tenants;
- no organizations;
- no email reset flow;
- no social/federated identity;
- no hosted multi-user product.

If dotlink becomes multi-user/hosted, this decision should be revisited and a mature external IdP used instead.

## OAuth implementation evolution

The 0.6.0 implementation added:

- strict profile schema v10;
- Protected Resource Metadata;
- OAuth Authorization Server Metadata;
- authorization code;
- mandatory PKCE S256;
- exact resource binding;
- RFC 9207 `iss`;
- CIMD;
- DCR fallback;
- owner consent/password page;
- opaque access/refresh tokens;
- rotating refresh;
- revocation;
- private persistent OAuth state;
- stable ngrok domain support;
- public ingress OAuth-by-default.

The final safety review strengthened it further:

- any externally reachable HTTP is safe-by-default, not only ngrok;
- direct non-loopback HTTP requires OAuth + explicit HTTPS public origin unless the one-run unsafe flag is supplied;
- Argon2 verification runs in bounded blocking workers rather than Tokio workers;
- no attacker-triggerable global password lockout;
- CIMD plural token-auth-method negotiation is authoritative;
- CIMD DNS validation preserves multiple vetted public addresses;
- refresh tokens are issued only when the client supports `refresh_token`;
- public OAuth state is bounded;
- unauthenticated DCR registrations are memory-only until owner consent;
- durable OAuth state read/modify/write is cross-process locked;
- password rollback modifies only the password field and preserves concurrent grants;
- revocation is observed by a running server at durable-state boundaries.

## Final filesystem safety review

The last review also tightened local filesystem semantics:

- denies apply to both normalized lexical paths and canonical targets;
- a denied namespace cannot be bypassed by replacing it with a symlink;
- filesystem reads may run concurrently;
- mutators and shell execution take an exclusive operation gate;
- this prevents **dotlink-originated** concurrent tools from changing path topology between authorization and use;
- unrelated hostile processes running as the same OS user remain outside dotlink's threat boundary;
- writes accept regular files or missing paths only;
- edit/patch require regular files;
- special files are rejected.

## Historical handoff

The Sep 30 handoff remains valuable because it records the reasoning that produced profiles, Bubblewrap, caches, logging and Crane.

However, it is stale for current-state facts.

Never copy current version/schema/release/auth claims from the Sep 30 bundle without verifying them against current source.
