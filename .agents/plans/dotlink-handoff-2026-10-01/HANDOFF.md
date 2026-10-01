# abird dotlink — comprehensive agent handoff

Date: 2026-10-01

This is the comprehensive single-file handoff for the current dotlink project. The supporting detailed specs live in this same directory.

Stable entry point:

~~~
.agents/plans/START_HERE.md
~~~

## Preserve the live tree

Do not reset, clean, checkout over, discard, or rewrite unexpected working-tree changes.

Before editing:

~~~bash
git status --short --branch
git log --oneline --decorate -12
git diff --stat
git diff --check
~~~

Current Rust source is always authoritative.

## Authority order

1. Current Rust source.
2. Current Cargo/Nix/scripts/workflow.
3. Current README.md, SECURITY.md, ARCHITECTURE.md, CHANGELOG.md.
4. Current .agents/AGENT.md and .agents/docs/.
5. This 2026-10-01 handoff package.
6. Git history.
7. The 2026-09-30 handoff, historical only.

## Implementation baseline

The implementation baseline described here is:

~~~
657ebb3 Harden OAuth and runtime safety after review
8f94c87 Add OAuth-protected remote MCP access
~~~

At handoff creation:

~~~
branch:       main
crate:        dotlink
version:      0.6.0
schema:       10
upstream:     https://github.com/abird-ai/dotlink
origin/main:  b5c9d22
implementation baseline: 2 commits ahead before handoff-doc commit
latest tag:   v0.5.0
v0.6.0 tag:   not yet published
~~~

A handoff documentation commit may follow these commits. Always inspect current git history.

## Product identity

Human-facing title:

~~~
abird dotlink
~~~

Crate, executable, MCP identity, config namespace and release prefix:

~~~
dotlink
~~~

Historical names:

~~~
abird-tunnel
abird-link
~~~

Do not reintroduce historical names into current product/API paths.

## Product purpose

dotlink is a single lightweight Rust binary that lets ChatGPT Web/Spaces/dots, Claude.ai, local MCP clients and other MCP-capable AI tools work against files and explicitly exposed tools on the user's computer.

Primary product goals:

- connect your ChatGPT Web, your dot and your computer;
- read/edit files and optionally run Git/build/test/scripts;
- use local data without repeated upload/download/copy-paste loops;
- build reports/decks/artifacts from live local data;
- keep ChatGPT/Spaces/dot context connected to the actual project/toolchain;
- provide a practical continuation path when Codex usage is unavailable/exhausted;
- share one local permission boundary across all transports;
- expose remote HTTP safely without a separate auth service;
- remain one binary with no GUI requirement.

## Architecture

~~~
ChatGPT Web/Spaces/dot ─ OpenAI Secure MCP Tunnel ┐
local MCP client ─────── stdio ───────────────────┤
remote web MCP ───────── HTTPS/ngrok + OAuth ─────┤
                                                  ▼
                                             dotlink
                           transport / OAuth / profile / policy
                                                  ▼
                                             LocalMachine
                                 files + optional shell/Bubblewrap
~~~

Core invariant:

> Transport never implies local authority.

OpenAI Tunnel, stdio and HTTP/ngrok all terminate at the same LocalMachine policy core.

OAuth authenticates remote entry.

LocalMachine authorizes local filesystem/shell capability.

## Least-privilege baseline

Default:

~~~
launch-directory read    yes
write                    no
extra paths              no
shell                    no
shell network            no
developer caches         no
public ingress           no
unsandboxed host mode    no
~~~

## MCP tools

Possible tools:

~~~
ls
read
write
edit
read_binary
write_binary
patch_binary
bash
powershell
~~~

Default read-only surface:

~~~
ls
read
read_binary
~~~

Dynamic tool discovery hides unusable tools.

Requirements:

~~~
ls/read/read_binary   read
write/write_binary    write
edit/patch_binary     read + write
shell                 allow_shell
~~~

Only Bash is exposed on Unix and only PowerShell on Windows.

Binary is deliberately separate from text.

read_binary formats:

~~~
mcp
base64
hex
~~~

mcp returns typed image/audio/blob content where possible.

## Filesystem policy

Authority is expressed through:

~~~
read_roots
write_roots
deny_read_roots
deny_write_roots
unrestricted_fs
~~~

The launch directory is only:

- the base for relative paths;
- the default read root when default_allow is enabled.

There is no separate persistent cwd permission concept.

CLI:

~~~
--allow-read[=<DIR>]
--allow-write[=<DIR>]
--allow-rw[=<DIR>]

--deny-read[=<DIR>]
--deny-write[=<DIR>]
--deny-rw[=<DIR>]
--deny <PATH>

--no-default-allow
~~~

Bare --allow-write means read+write launch directory.

Explicit --allow-write=/path remains write-only.

--allow-rw=/ naturally means unrestricted filesystem read/write.

Denies win.

## Path safety

Existing paths:

1. normalize lexical request path;
2. reject parent traversal;
3. check lexical denies;
4. canonicalize;
5. check canonical denies;
6. check canonical grants;
7. reject protected dotlink control paths.

Create paths:

- lexical deny check;
- canonicalize nearest existing ancestor;
- reconstruct/check target.

Final review added lexical deny checks so a denied path namespace cannot later be replaced by a symlink to another allowed subtree.

### File types

write/write_binary:

- missing target allowed;
- existing target must be regular file.

edit/patch:

- existing regular file required.

Special files are rejected.

### Operation gate

LocalMachine owns a shared read/write operation gate.

Concurrent pure reads:

~~~
ls
read
read_binary
~~~

Exclusive:

~~~
all mutators
bash
powershell
unknown/future tools
~~~

Purpose:

- prevent dotlink-originated concurrent shell/mutator operations from changing path topology between authorization and host use.

This does not attempt to defend against a separate malicious process already running as the same OS user.

## Shell and Bubblewrap

Shell is off by default.

Linux normal shell:

- Bash;
- Bubblewrap required;
- network off by default;
- private tmp;
- private home at /tmp/home;
- fresh proc/dev;
- policy-derived mounts;
- Nix runtime/profile mounts as needed;
- developer cache mounts;
- deny masks.

Mount semantics:

~~~
read-only root   → RO
read+write root  → RW
write-only root  → not exposed to shell
deny-read        → masked
deny-write       → RW downgraded to RO where read remains
~~~

### NixOS

Expose runtime/profile graph read-only as present:

~~~
/nix/store
/run/current-system
/etc/profiles
/nix/var/nix/profiles
~/.nix-profile
~~~

Do not mount whole home.

When network is denied, hide Nix daemon endpoint namespaces:

~~~
/nix/var/nix/daemon-socket
/run/nix-daemon
~~~

### Full host mode

Only:

~~~bash
dotlink --allow-all --no-sandbox
~~~

Each flag requires the other.

### macOS/Windows shell

Native shell sandbox equivalent is not implemented.

Rust filesystem MCP tools remain policy constrained.

Unsandboxed native shell requires full-host acknowledgement.

Windows secure-shell recommendation is WSL2 + Linux/Bubblewrap.

## Developer caches

Caches are shell-only and never expand MCP filesystem roots.

Supported families include Cargo, npm, pnpm, Yarn, pip, uv, Go, Maven, Gradle, sccache and ccache.

Modes:

~~~
none
read_only
read_write
~~~

Sandbox maps approved caches under its private home.

Adjacent credential/config files are not mounted.

Deny precedence:

~~~
deny-read  → remove cache mount
deny-write → RW cache becomes RO
~~~

## Config/schema

Strict profile schema:

~~~
10
~~~

Top-level profile shape:

~~~
version
transports
oauth
permissions
caches
tunnel_id
organization_id
base_url
resource limits
~~~

JSONC supports comments/trailing commas.

OpenAI Runtime key is stored separately and never serialized into JSONC.

### Paths

Default:

~~~
$XDG_CONFIG_HOME/abird/dotlink/config.jsonc
$XDG_CONFIG_HOME/abird/dotlink/runtime.key
$XDG_STATE_HOME/abird/dotlink/oauth.json
~~~

Named:

~~~
config.work.jsonc
runtime.work.key
oauth.work.json
~~~

DOTLINK_CONFIG relocates only JSONC.

Runtime keys and OAuth state stay under owned Abird XDG namespaces.

### Strict schema behavior

Normal startup accepts only schema v10.

Anything else is rejected.

--setup against an older schema may replace it from scratch after explicit confirmation.

No security-semantic migration is performed.

Cancel leaves old config unchanged.

## Profiles

Run:

~~~bash
dotlink -p work
dotlink -S -p work
~~~

Management:

~~~bash
dotlink profile list
dotlink profile show work
dotlink profile create work
dotlink profile edit work
dotlink profile delete work

dotlink profile allow work read PATH
dotlink profile allow work write PATH
dotlink profile allow work rw PATH

dotlink profile deny work read PATH
dotlink profile deny work write PATH
dotlink profile deny work rw PATH

dotlink profile remove-allow ...
dotlink profile remove-deny ...

dotlink profile enable work SETTING
dotlink profile disable work SETTING
~~~

default is the reserved alias for the unnamed profile.

Profile names are 1–64 ASCII letters/digits/hyphen/underscore.

A transport enabled in a profile starts automatically.

Positive CLI flags add for one run.

--no-* suppresses profile defaults.

Do not reintroduce a requirement to repeat --http/--stdio for a persisted transport.

## Setup

-S / --setup.

Transport picker:

~~~
1 OpenAI Tunnel   Recommended for ChatGPT
2 stdio           Local MCP clients
3 HTTP            Claude.ai / web MCP clients
~~~

Can select combinations/all.

Cancel tokens include:

~~~
0
none
cancel
q
quit
~~~

Cancel returns success and saves nothing.

Re-setup:

- current values are defaults;
- Runtime key shown only as [existing key];
- OAuth owner shown only as [existing credential];
- blank preserves secrets;
- secret input hidden.

HTTP/ngrok setup can persist:

- bind;
- local ephemeral path;
- ngrok;
- stable ngrok domain;
- ngrok ephemeral path;
- local/reverse-proxy OAuth;
- OAuth public origin.

Public ngrok OAuth is automatic.

## OpenAI Tunnel

Primary ChatGPT transport.

Requires:

- tunnel ID;
- Runtime API key with Tunnels Read + Use.

Setup supports:

1. create/manage tunnel manually in OpenAI Platform and paste tunnel ID;
2. automated one-time Admin-key creation flow.

Admin key is never saved.

Runtime key is private per profile.

### Recovery

Transient poll failures retry.

After 10 consecutive transient poll failures:

- OpenAI transport returns a typed runtime restart request;
- all transports cancel;
- peers get up to five seconds to drain;
- unresponsive peers are aborted;
- profile/policy/LocalMachine/transports are rebuilt.

Restart backoff begins at one second and caps at 30 seconds.

A previously successful runtime resets backoff.

Fatal auth/missing established tunnel errors remain fatal.

## stdio

Local MCP transport.

stdout is protocol-only.

All human/status/logging output goes to stderr.

Request diagnostics log method name, not raw payload.

Runtime key controls are disabled when stdio is active.

## HTTP/public ingress

Default:

~~~
http://127.0.0.1:3000/mcp
~~~

Loopback may be unauthenticated.

Any externally reachable HTTP is safe-by-default.

Direct non-loopback:

~~~
OAuth + explicit HTTPS public origin
OR
--allow-public-no-auth
~~~

Persisted non-loopback profile is accepted only with OAuth enabled and HTTPS oauth.public_url.

Unsafe public no-auth is one-run-only.

## ngrok

ngrok is an HTTP publication enhancer, not a separate MCP protocol.

~~~bash
dotlink --http --ngrok
~~~

Public ngrok OAuth is automatic.

ngrok uses a separate loopback backend listener.

Optional stable domain:

~~~bash
--ngrok-domain=my-dotlink.ngrok.app
~~~

dotlink verifies returned hostname matches requested hostname.

Durable OAuth requires:

- stable hostname;
- stable MCP path.

Automatic hostname or ephemeral MCP path changes identity and requires reconnect/re-authorization.

## Embedded OAuth

Purpose:

- single self-hosted owner;
- remote web MCP without external IdP.

Endpoints:

~~~
/.well-known/oauth-protected-resource
/.well-known/oauth-authorization-server
/oauth/authorize
/oauth/token
/oauth/register
/oauth/revoke
~~~

Protocol:

- authorization code;
- PKCE S256 mandatory;
- exact resource binding;
- RFC 9207 iss;
- CIMD;
- DCR fallback;
- public client token auth none;
- scopes mcp:access/offline_access.

OAuth grants access to dotlink; LocalMachine still controls files/shell.

### CIMD

Client ID is stable HTTPS metadata URL.

Requirements:

- non-root path;
- no userinfo;
- no query;
- no fragment;
- redirects disabled;
- all resolved addresses public;
- vetted address set pinned;
- bounded body;
- metadata client_id exact match;
- modern plural token-auth method list authoritative when present.

### DCR

Public client fallback.

Unauthenticated registrations are:

- memory-only;
- bounded;
- short-lived.

Owner consent promotes approved DCR client into persistent state.

### Token lifecycle

~~~
authorization code  memory-only, one use, ~60s
access token        memory-only, ~15m
refresh token       random client value, SHA-256 persisted, ~90d
~~~

Refresh tokens rotate.

Replay rejected.

Refresh token issued only when client advertises refresh_token.

offline_access requires refresh support.

### Owner password

Persisted only as Argon2id hash.

Verification:

- reload current durable hash;
- bounded semaphore;
- spawn_blocking;
- two concurrent checks;
- bounded wait;
- failed attempt delay;
- no global attacker-triggered lockout.

### Durable OAuth state

~~~
$XDG_STATE_HOME/abird/dotlink/oauth[.<profile>].json
~~~

On Unix:

~~~
directory 0700
state file 0600
lock file 0600
~~~

Persistent mutations use a private lock file and OS file lock around the entire read/modify/write transaction.

Do not replace this with atomic rename only.

### OAuth CLI

~~~bash
dotlink oauth status [-p PROFILE]
dotlink oauth clients [-p PROFILE]
dotlink oauth revoke [-p PROFILE] CLIENT_ID
dotlink oauth revoke-all [-p PROFILE]
~~~

Durable revocation is observed by live runtime at relevant boundaries.

Existing access token remains until expiry/restart.

Ctrl+R/restart invalidates in-memory access tokens.

## Runtime controls

Interactive non-stdio:

~~~
Ctrl-C  exit
Ctrl-R  full restart
v       verbosity cycle
~~~

Verbosity:

~~~
quiet → TOOL → TOOL+REQ → quiet
~~~

Terminal state is RAII restored.

stdio never shares runtime key capture.

## Logging

Default quiet.

~~~
-v      TOOL
-vv     TOOL + REQ
-q      suppress TOOL
-q -vv  REQ only
~~~

TOOL logs safe arguments/status/latency.

Bulk content is represented by sizes.

REQ logs transport/protocol metadata, not raw bodies.

Do not intentionally log file contents, passwords, keys, bearer tokens or refresh tokens.

## Build/release

Rust:

~~~
1.98.1
edition 2024
~~~

Crane separates dependencies from source.

From x86_64 Linux:

~~~
dist-linux-x86_64
dist-linux-aarch64
dist-windows-x86_64
dist-windows-aarch64
dist-macos-aarch64
release-all
~~~

Linux is static musl.

No distro-specific NixOS/Debian binaries.

Windows ARM64 uses LLVM-MinGW/UCRT.

macOS ARM64 uses pinned Apple SDK 14.4 + LLVM/ld64.lld.

## GitHub CI/release

Workflow:

~~~
.github/workflows/build-binaries.yml
~~~

CI calls:

~~~bash
nix build .#release-all
~~~

Target/toolchain logic stays in flake.nix.

On v* tag:

- rebuild all;
- validate;
- require tag/version match;
- create/update GitHub Release;
- upload each binary/checksum individually.

Actions ZIP is internal only.

## Installers

Unix:

~~~bash
curl -fsSL https://raw.githubusercontent.com/abird-ai/dotlink/main/install.sh | sh
~~~

Windows:

~~~powershell
irm https://raw.githubusercontent.com/abird-ai/dotlink/main/install.ps1 | iex
~~~

Default:

- latest published GitHub Release;
- architecture selected internally;
- SHA verified;
- downloaded --version must identify dotlink;
- explicit requested version must match;
- user command remains dotlink / dotlink.exe;
- rerun updates;
- same hash no-op;
- same-directory temporary replacement.

## Final validation snapshot

2026-10-01:

~~~
cargo fmt                         PASS
cargo test --all-features        137 / 137 PASS
cargo clippy -D warnings         PASS
Bubblewrap runtime tests         4 / 4 PASS
ShellCheck                       PASS
Unix installer smokes            PASS
PowerShell installer smokes      PASS
actionlint                        PASS
RustSec vulnerabilities          0
Nix all-system evaluation        PASS
Nix native package/tests/clippy/fmt PASS
release-all                       PASS
5 binary SHA sidecars            PASS
Linux static ELF checks          PASS
Windows PE checks                PASS
macOS ARM64 Mach-O check         PASS
interactive OAuth setup          PASS
live OAuth discovery/401         PASS
~~~

RustSec warnings:

~~~
generational-arena  unmaintained via ngrok
rustls-pemfile      unmaintained via ngrok
yoke-derive         yanked via URL/IDNA graph
~~~

No known RustSec vulnerabilities.

## Known limitations

- embedded OAuth is single-owner;
- no persistent owner browser session;
- token endpoint currently uses public-client auth none + PKCE;
- in-memory access tokens live until expiry/restart after durable revocation;
- native macOS/Windows shell has no Bubblewrap equivalent;
- public macOS release is ARM64 only;
- no release signing/notarization/SBOM/provenance yet;
- malicious unrelated same-user local process is outside threat boundary;
- upstream ngrok transitive maintenance warnings remain;
- external provider/API availability remains external;
- v0.6.0 is not yet confirmed released at handoff creation.

## Current docs and handoff

Authoritative current docs:

~~~
README.md
SECURITY.md
ARCHITECTURE.md
CHANGELOG.md
.agents/AGENT.md
.agents/docs/configuration.md
.agents/docs/release-install.md
~~~

Historical handoff:

~~~
.agents/plans/dotlink-handoff-2026-09-30/
~~~

Current handoff:

~~~
.agents/plans/dotlink-handoff-2026-10-01/
~~~

Stable handoff pointer:

~~~
.agents/plans/START_HERE.md
~~~

## Do not casually change

- do not mount whole home for cache convenience;
- do not expose Nix daemon by default;
- do not let cache grants become MCP filesystem grants;
- do not make public ingress unauthenticated by default;
- do not persist public-no-auth;
- do not infer OAuth issuer from Host/forwarded headers;
- do not store owner password or refresh plaintext;
- do not remove cross-process OAuth locking;
- do not weaken lexical + canonical denies;
- do not let future tools bypass the operation gate;
- do not silently enable network;
- do not remove Bubblewrap default on Linux;
- do not call ephemeral URLs authentication;
- do not restore fs_* tool names;
- do not overload text read/write with binary modes;
- do not move release target logic into CI;
- do not add duplicate distro-specific Linux assets.

## Remaining work

The implementation phase is complete.

Immediate operational work only:

1. push local commits if the user asks;
2. tag/publish v0.6.0 if the user asks;
3. refresh the live ChatGPT MCP App/tool schema after deployment;
4. optionally perform a final Claude.ai remote OAuth smoke against stable ngrok.

Optional future work is documented in:

~~~
13-NEXT-WORK-ROADMAP.md
~~~

Do not invent implementation gaps from the stale Sep 30 roadmap.

## Next-agent first actions

1. Read .agents/plans/START_HERE.md.
2. Read 00-START-HERE.md and this HANDOFF.md.
3. Follow the complete supporting read order.
4. Inspect live git status/history.
5. Inspect live Rust source before architectural changes.
6. Treat 657ebb3 and 8f94c87 as completed implementation baselines unless history shows newer code.
7. If publishing, re-check GitHub state before push/tag.
8. Add regression tests before security-sensitive changes.
9. Run targeted validation plus full Nix/release matrix when platform/release behavior changes.
10. Do not push unless explicitly requested.
