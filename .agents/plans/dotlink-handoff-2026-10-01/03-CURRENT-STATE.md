# Current state

Date: 2026-10-01

## Repository

```text
repo:    https://github.com/abird-ai/dotlink
branch:  main
HEAD:    657ebb3 Harden OAuth and runtime safety after review
parent:  8f94c87 Add OAuth-protected remote MCP access
```

Before adding the handoff-documentation commit, the implementation baseline on local `main` was two commits ahead of `origin/main`, whose visible baseline was:

```text
b5c9d22 Make installers update to latest release
```

Remote tags visible at handoff creation:

```text
v0.5.0
v0.2.0
```

There is no published v0.6.0 tag yet.

Always re-check live Git state before acting.

## Package/config identity

```text
human title:       abird dotlink
crate:             dotlink
binary:            dotlink
MCP server name:   dotlink
crate version:     0.6.0
Rust edition:      2024
toolchain:         Rust 1.98.1
profile schema:    10
OAuth state schema: 1
```

## Source modules

```text
src/main.rs
  CLI
  profile/OAuth command dispatch
  effective permission policy
  transport precedence
  runtime restart loop
  banner/tool listing

src/setup.rs
  schema v10
  JSONC parsing/serialization
  setup wizard
  profile CRUD/rules/toggles
  Runtime key storage
  cache discovery
  config validation
  control-plane protected paths

src/mcp.rs
  LocalMachine
  AccessPolicy
  MCP tool router
  text/binary tools
  shell execution
  Bubblewrap composition
  cache mounts
  filesystem operation gate

src/oauth.rs
  embedded single-owner OAuth
  OAuth metadata
  authorize/token/register/revoke
  owner credential
  CIMD/DCR
  access/refresh grants
  OAuth persistence and cross-process locking
  SSRF/redirect/resource validation

src/controls.rs
  terminal raw-mode/runtime control reader
  Ctrl-C/Ctrl-R/v
  stdio interrupt mode
  RAII terminal restoration

src/logging.rs
  quiet/TOOL/REQ runtime state
  colors
  activity/request metadata logs

src/transports/openai.rs
  OpenAI Secure MCP Tunnel
  poll/response protocol
  transient-failure recovery escalation
  local embedded MCP bridge

src/transports/stdio.rs
  stdio MCP transport
  request-method-only diagnostics
  stdout protocol purity

src/transports/http.rs
  Streamable HTTP
  local/ngrok listener separation
  OAuth middleware/router composition
  stable/ephemeral MCP paths
  ngrok SDK integration

src/transports/mod.rs
  transport supervisor
  cancellation
  peer teardown
  typed runtime-restart marker
```

## Current tool surface

Potential tools:

```text
ls
read
write
edit
read_binary
write_binary
patch_binary
bash
powershell
```

Actual tools are dynamically filtered by policy.

Default profile/runtime authority normally exposes:

```text
ls
read
read_binary
```

against the launch directory.

Tool requirements:

```text
ls/read/read_binary       read
write/write_binary        write
edit/patch_binary         read + write
bash/powershell           allow_shell
```

Only Bash is visible on Unix; only PowerShell on Windows.

## Current permission model

Launch/base directory:

- internal base for relative paths;
- readable by default;
- not a separate persistent permission category.

Persistent/CLI authority:

```text
allow_read[]
allow_write[]
allow_rw[]

deny_read[]
deny_write[]
deny_rw[]

default_allow
allow_shell
allow_network
```

Denies win.

Existing paths are checked against:

1. normalized lexical path for deny rules;
2. canonical resolved target for deny + allow rules.

Create paths:

- reject parent traversal;
- check lexical denies;
- canonicalize nearest existing ancestor;
- verify final canonical candidate against policy.

Writes:

- existing target must be a regular file;
- missing target is allowed;
- special files are rejected.

Edits/patches require regular files.

## Concurrent filesystem-tool safety

`LocalMachine` owns a shared `Arc<RwLock<()>>` operation gate.

Shared side:

```text
ls
read
read_binary
```

Exclusive side:

```text
write
edit
write_binary
patch_binary
bash
powershell
any future/unknown tool by default
```

Purpose:

- prevent dotlink-originated concurrent shell/mutation calls from swapping symlinks/path topology between path authorization and use;
- retain parallelism for pure reads;
- make newly added tools safe-by-default until explicitly reviewed.

This does not attempt to defend against an unrelated hostile local process already running as the same OS user.

## Current sandbox behavior

Linux shell:

- Bubblewrap by default;
- shell disabled by default;
- network disabled by default;
- private `/tmp`;
- private home at `/tmp/home`;
- fresh proc/dev;
- policy-derived mounts;
- Nix runtime/profile graph RO where present;
- Nix daemon endpoints hidden when network is disabled;
- developer caches mounted explicitly according to RO/RW grant;
- denies remask/downgrade mounts.

Non-Linux shell:

- native sandbox equivalent is not implemented;
- filesystem MCP tools remain Rust-policy constrained;
- shell requires the explicit full-host pair `--allow-all --no-sandbox`;
- Windows secure-shell recommendation is WSL2 + Linux/Bubblewrap.

## Current transports

### OpenAI Tunnel

Profile transport:

```jsonc
"openai": true
```

Requires:

- tunnel ID;
- Runtime API key;
- optional organization/workspace ID only as needed for tunnel creation.

The Admin key is used only for optional setup-time tunnel creation and is never saved.

Runtime recovery:

- transient polls retry;
- 10 consecutive transient poll failures request full runtime restart;
- all transports tear down;
- peers get up to 5 seconds to drain;
- runtime rebuilt;
- unhealthy restarts back off up to 30s;
- prior successful connection resets backoff;
- fatal auth/missing tunnel errors remain fatal.

### stdio

- local MCP subprocess transport;
- stdout is protocol-only;
- human/log output uses stderr;
- runtime key controls disabled while stdio is active.

### HTTP

Default:

```text
127.0.0.1:3000/mcp
```

Loopback may be unauthenticated.

Direct non-loopback HTTP is treated as public ingress:

- persisted config requires OAuth + HTTPS public origin;
- one-run override requires OAuth + `--public-url=https://...`, or explicit `--allow-public-no-auth`.

### ngrok

ngrok is an HTTP publication enhancer, not a separate MCP protocol.

- public ngrok is OAuth-protected automatically;
- ngrok backend is a separate loopback listener;
- optional stable `ngrok_domain`;
- local and public paths can have independent ephemeral settings;
- exact public hostname is verified when a domain was requested;
- automatic hostname / ephemeral MCP path is allowed but requires reconnecting OAuth clients when identity changes.

## OAuth current state

Embedded single-owner authorization/resource server.

Discovery/endpoints:

```text
/.well-known/oauth-protected-resource
/.well-known/oauth-authorization-server
/oauth/authorize
/oauth/token
/oauth/register
/oauth/revoke
/mcp[/<ephemeral>]
```

Core protocol:

- authorization code;
- mandatory PKCE S256;
- exact resource binding;
- RFC 9207 `iss`;
- CIMD;
- DCR fallback;
- public token endpoint auth method `none`;
- access + optional refresh tokens;
- rotation/replay rejection;
- revocation.

Scopes:

```text
mcp:access
offline_access
```

OAuth scopes authenticate entry to dotlink. They do not duplicate filesystem/shell policy.

Refresh behavior:

- refresh token issued only when approved client supports `refresh_token`;
- `offline_access` requires that grant.

Token/state model:

```text
authorization code   random, one-use, memory-only, ~60s
access token         random, memory-only, 15m
refresh token        random to client, SHA-256 hash persisted, 90d, rotate-on-use
owner password       plaintext never persisted; Argon2id hash only
pending DCR          memory-only, 15m, bounded
approved DCR client  persisted
```

Durable state:

```text
$XDG_STATE_HOME/abird/dotlink/oauth.json
$XDG_STATE_HOME/abird/dotlink/oauth.<profile>.json
```

On Unix:

```text
directory 0700
files     0600
```

Durable state mutations use a private lock file and cross-process file locking so setup, management commands and a live server do not overwrite one another's updates.

Argon2 verification:

- offloaded with `spawn_blocking`;
- max two concurrent checks;
- bounded wait;
- failed attempts delayed;
- no attacker-triggerable global owner lockout.

CIMD:

- HTTPS document URL;
- non-root path;
- no credentials/query/fragment;
- redirects disabled;
- all resolved addresses must be public;
- validated full address set pinned into reqwest;
- response body bounded;
- client_id must match document;
- plural token auth method list authoritative when present.

DCR:

- public clients only;
- authorization_code required;
- response type code;
- refresh optional;
- pending registration memory-only until owner approves;
- approved client persisted.

Management:

```bash
dotlink oauth status [-p PROFILE]
dotlink oauth clients [-p PROFILE]
dotlink oauth revoke [-p PROFILE] CLIENT_ID
dotlink oauth revoke-all [-p PROFILE]
```

## Config schema v10

Current top-level shape:

```text
version
transports
oauth
permissions
caches
tunnel_id
organization_id
base_url
resource limits
```

Runtime API key is stored separately and is never serialized into JSONC.

Schema is strict:

- normal startup rejects other versions;
- `--setup` may replace an older profile from scratch after explicit confirmation;
- no semantic migration is performed;
- cancellation leaves the older profile untouched.

## Profile manager

```bash
dotlink profile list
dotlink profile show NAME
dotlink profile create NAME
dotlink profile edit NAME
dotlink profile delete NAME

dotlink profile allow NAME read|write|rw PATH
dotlink profile remove-allow NAME read|write|rw PATH
dotlink profile deny NAME read|write|rw PATH
dotlink profile remove-deny NAME read|write|rw PATH

dotlink profile enable NAME SETTING
dotlink profile disable NAME SETTING
```

`default` is the reserved alias for the unnamed profile.

Profile names:

- 1–64 chars;
- ASCII letters/digits/`-`/`_`.

## Setup state

Setup is complete and profile-aware.

It:

- uses current values as defaults;
- colors human UI unless disabled;
- links current GitHub README for help;
- masks Runtime key as `[existing key]`;
- masks OAuth owner as `[existing credential]`;
- uses hidden secret input;
- preserves terminal state;
- saves transactionally;
- can cancel without saving;
- supports Admin-key-free OpenAI tunnel onboarding.

When HTTP/ngrok is chosen:

- local HTTP auth is independently configurable;
- public ngrok auth is automatic;
- stable ngrok domain may be saved;
- reverse-proxy canonical public origin may be saved;
- owner OAuth credential is created when needed.

## Logging/runtime controls

Default logging:

```text
quiet
```

CLI:

```text
-v      TOOL
-vv     TOOL + REQ
-q      suppress TOOL
-q -vv  REQ only
```

Interactive non-stdio controls:

```text
Ctrl-C  exit
Ctrl-R  full runtime restart
v       cycle quiet → TOOL → TOOL+REQ → quiet
```

REQ/TOOL logging intentionally avoids raw file contents, binary payloads and secret values.

## Build/release state

`flake.nix`/Crane supports:

Native:

```text
package
deps
checks: package/tests/clippy/fmt
```

From x86_64 Linux:

```text
dist-linux-x86_64
dist-linux-aarch64
dist-windows-x86_64
dist-windows-aarch64
dist-macos-aarch64
release-all
```

Linux builds are static musl.

No NixOS/Debian-specific release aliases exist.

Windows ARM64 uses LLVM-MinGW/UCRT.

macOS ARM64 uses pinned Apple SDK 14.4 + Linux-host clang/ld64.lld, deployment target macOS 11.0.

## CI/release state

Workflow:

```text
.github/workflows/build-binaries.yml
```

Normal branch/PR/manual run:

- build `release-all`;
- validate checksums/version;
- upload internal CI artifact.

`v*` tag:

- build same Nix graph;
- tag must match `v$(VERSION)`;
- create/update GitHub Release;
- upload binary/checksum files individually.

Public release assets:

```text
dotlink-linux-x86_64
dotlink-linux-x86_64.sha256
dotlink-linux-aarch64
dotlink-linux-aarch64.sha256
dotlink-windows-x86_64.exe
dotlink-windows-x86_64.exe.sha256
dotlink-windows-aarch64.exe
dotlink-windows-aarch64.exe.sha256
dotlink-macos-aarch64
dotlink-macos-aarch64.sha256
```

The GitHub Actions bundle ZIP is not a public Release asset.

## Installer state

`install.sh`:

- Linux x86_64/ARM64;
- macOS ARM64;
- latest Release by default;
- version override;
- SHA-256 verification;
- downloaded binary identity/version validation;
- simple installed filename `dotlink`;
- no-op if identical;
- same-directory temp + rename for update.

`install.ps1`:

- Windows x86_64/ARM64;
- same version/checksum semantics;
- same-directory temporary replacement;
- installed filename `dotlink.exe`.

Both act as updaters when re-run.

## Documentation state

Current authoritative docs:

```text
README.md
SECURITY.md
ARCHITECTURE.md
CHANGELOG.md
.agents/AGENT.md
.agents/docs/configuration.md
.agents/docs/release-install.md
```

Current historical plans:

```text
.agents/plans/dotlink-handoff-2026-09-30/
```

Current handoff:

```text
.agents/plans/dotlink-handoff-2026-10-01/
```

## Current publication status

At handoff creation:

- implementation is complete;
- validation is green;
- local main is two commits ahead of origin/main;
- v0.6.0 has not yet been pushed/tagged;
- v0.5.0 exists remotely.

Do not claim v0.6.0 is released until live Git state confirms it.
