# dotlink

Simply **`@dotlink`** anywhere in ChatGPT — chat, space, or your dot to work with files on your machine, use local tools and projects you exposed — securely, from a single binary.

A single lightweight binary with an MCP server, OpenAI tunnel transport, permission engine, profiles, activity logging, and a full Bubblewrap sandbox on Linux — no UI, no third-party relay, guided setup.

- **Let your ChatGPT Web and your dot work directly on your computer through controlled, fine-grained access**: read and edit files, run Git, build, test, execute scripts, and use only the tools you explicitly expose.
- **Give ChatGPT controlled access to several machines without installing the ChatGPT desktop app or provisioning SSH**: run dotlink on each machine and expose only the projects, files, and capabilities you choose.
- **Use local data without the upload/download loop**: analyze files, build reports or slide decks, and work with artifacts directly from your machine.
- **Use ChatGPT as a practical fallback when Codex usage is unavailable or exhausted**: reconnect to the same project state and keep going.

When your machine is offline, keep planning against context already in ChatGPT; reconnect later and let ChatGPT re-read the live project before continuing.

For ChatGPT, dotlink opens an outbound **OpenAI Secure MCP Tunnel directly from your machine to OpenAI**. Your MCP server stays local: no public inbound port, no third-party relay.

**Supports Claude.ai and other remote MCP clients** over HTTP/ngrok, plus local MCP clients over stdio or loopback HTTP. Every transport uses the same local permission policy.

Security is local and opt-in: the launch directory is read-only by default; write, shell, network, extra paths, caches, and public HTTP are separate grants. Linux shell execution is Bubblewrap-sandboxed by default.

## Quick start

### Install

Linux x86_64/ARM64 or macOS Apple Silicon:

~~~bash
curl -fsSL https://raw.githubusercontent.com/abird-ai/dotlink/main/install.sh | sh
~~~

Windows x86_64/ARM64 PowerShell:

~~~powershell
irm https://raw.githubusercontent.com/abird-ai/dotlink/main/install.ps1 | iex
~~~

Both installers download the **latest published GitHub Release** by default, select the correct architecture internally, verify its SHA-256, and require the downloaded executable to identify itself as the requested dotlink version before replacement. The command you run is always simply **`dotlink`**; architecture suffixes exist only on the release assets (`dotlink.exe` is the Windows file on disk). Re-run the same install command at any time to update; if the installed binary already matches the latest release, it is left unchanged.

Or build with Nix/Cargo; see **Build** below.

### Setup

~~~bash
dotlink --setup
cd ~/src/my-project
dotlink
~~~

Setup is transactional: choose `0`, `none`, or `cancel` to exit successfully without writing anything. Re-running `--setup` on an existing profile uses its current values as defaults; secret prompts show only `[existing key]`, and blank input keeps the stored key.

Compatible older profile schemas are migrated forward automatically in memory. In particular, schema v9 loads directly as v10 because the v10 fields are additive/defaulted; the profile is written as the current schema the next time it is edited or saved. Newer schemas are never guessed, and older configs that need an explicit security decision—such as newly protected public ingress—fail with the specific setting that must be updated.

The launch directory is readable by default. Add capabilities only when needed:

~~~bash
dotlink --allow-rw
dotlink --allow-rw --allow-shell
dotlink --allow-rw --allow-shell --allow-network
~~~

### Manage profiles

~~~bash
dotlink profile list
dotlink profile show work
dotlink profile create work
dotlink profile edit work
dotlink profile delete work

dotlink profile allow work rw /shared
dotlink profile deny work read /secret
dotlink profile remove-allow work rw /shared
dotlink profile remove-deny work read /secret

dotlink profile enable work shell
dotlink profile enable work network
dotlink profile disable work network
~~~

Use `default` as the profile name to manage the unnamed default profile. Persisted settings can be toggled with `profile enable/disable`: `openai`, `stdio`, `http`, `http-ephemeral-url`, `ngrok`, `ngrok-ephemeral-url`, `oauth`, `default-allow`, `shell`, and `network`. Enabling a dependent setting automatically enables its prerequisite (`network` → `shell`, `ngrok` → `http`, `oauth` → `http`); disabling a parent safely disables its dependents. Enabling `ngrok` or `oauth` requires an owner OAuth credential; use `dotlink profile edit <name>` when one still needs to be configured.

### What it looks like

Default runs are quiet. Add `-v` to see tool activity:

~~~text
$ dotlink -v -p aw

abird dotlink 0.13.0
────────────────────────────────────────────────────────
• Profile    aw
• Transports openai
• Tunnel     tunnel_…
• Access     read:2 write:1 deny-read:0 deny-write:0 caches:0 + shell
• Sandbox    Bubblewrap
• Network    enabled
• Logging    TOOL

OpenAI Secure MCP Tunnel connecting…
Keys        Ctrl-C: exit · Ctrl-R: restart · v: verbosity

✓ Connected — ready

[10:59:19.184] TOOL read       → p="./.agents/plans/TODO.md"
[10:59:19.194] TOOL read       ← ok  9ms
~~~

`-v` starts with TOOL activity; `-vv` starts with TOOL + REQ diagnostics. During an interactive run, press `v` to cycle `quiet → TOOL → TOOL + REQ → quiet` without restarting. Each change emits a compact `LOG verbosity …` line. `Ctrl+R` fully reloads the profile/policy/transports; `Ctrl+C` exits. Runtime key controls are disabled when stdio MCP is active so protocol stdin is never intercepted. TOOL logging intentionally records full shell commands (including embedded arguments and secrets) as quoted, escaped one-line text. File contents, binary payloads, explicit Runtime keys, and raw request bodies are not intentionally logged.

## Connect dotlink to ChatGPT

### 1. Start dotlink

Run setup once and select **OpenAI Tunnel**:

~~~bash
dotlink --setup
~~~

Then start dotlink from the project you want ChatGPT to access:

~~~bash
cd ~/src/my-project
dotlink
~~~

Copy the printed `tunnel_...` ID and keep dotlink running.

### 2. Add it to ChatGPT

1. Open **<https://chatgpt.com/plugins>**.
2. Select **Add → Create MCP App**.
3. Name it **abird dotlink** and optionally add a short description.
4. Under **Connection**, choose **Tunnel**.
5. Paste the `tunnel_...` ID printed by dotlink.
6. Accept the custom-MCP warning and create the MCP app.
7. In a ChatGPT conversation, open the tools menu, enable **abird dotlink**, and ask normally.

After that, simply mention **`@dotlink`** in ChatGPT Web — in a normal chat, a Space, or your dot — whenever you want to work with the files and projects that machine has exposed.

For several computers, run dotlink on each machine and add each tunnel as its own MCP App. Name the connections by machine or project if useful. ChatGPT can then reach each machine under its own local permission policy, with no ChatGPT desktop app required on those machines.

If **Create MCP App** is unavailable, enable **Developer mode** under **Settings → Security and login** first.

The tools ChatGPT discovers follow dotlink's local permissions:

~~~text
read-only      ls, grep, read, bread
write          + write, bwrite
read + write   + edit, patch, bedit
shell          + bash (Unix) / powershell (Windows)
~~~

Never paste a Runtime or Admin API key into ChatGPT. ChatGPT only needs the `tunnel_...` ID.

If the tunnel is not found, verify that dotlink shows `✓ Connected — ready`, the tunnel belongs to the current ChatGPT workspace, and the Runtime API key has **Tunnels Read + Use**.

Detailed reference: `docs/CHATGPT_PLUGIN.md`.

## Connect dotlink to Claude.ai

Claude.ai custom connectors connect from Anthropic's cloud, so a loopback URL cannot be used directly. dotlink can publish its Streamable HTTP server through ngrok and **automatically protects the public endpoint with OAuth**.

### 1. Configure HTTP + ngrok

Run setup, select **HTTP**, then enable the public ngrok endpoint:

~~~bash
dotlink --setup
~~~

Setup asks for one **owner password** the first time OAuth is needed. The password itself is never stored; dotlink persists only an Argon2id hash in its private state directory. Changing the owner password is effective for new authorizations on a running HTTP server without restarting it; previously issued short-lived access tokens still require restart for immediate invalidation.

Set your ngrok account token and start dotlink:

~~~bash
export NGROK_AUTHTOKEN='...'
dotlink
~~~

You can also add HTTP/ngrok for one run:

~~~bash
dotlink --http --ngrok
~~~

dotlink prints the public MCP resource and OAuth issuer:

~~~text
✓ ngrok MCP: https://my-dotlink.ngrok.app/mcp
✓ OAuth: https://my-dotlink.ngrok.app (resource https://my-dotlink.ngrok.app/mcp)
~~~

For a durable connector, reserve an ngrok domain and persist it during setup or use:

~~~bash
dotlink --http --ngrok --ngrok-domain=my-dotlink.ngrok.app
~~~

Keep the normal stable `/mcp` path for durable OAuth. An automatic ngrok hostname or `--ngrok-ephemeral-url` changes the OAuth resource identity when the URL changes, so the remote client must be re-linked.

### 2. Add it to Claude.ai

1. Open **Customize → Connectors**.
2. Select **+ → Add custom connector**.
3. Name it **abird dotlink**.
4. Paste the printed ngrok MCP URL.
5. Connect it.
6. When OAuth opens, review the client ID, redirect URI, resource, and scope, then enter your dotlink owner password and approve access.

The client receives short-lived bearer tokens plus rotating refresh tokens; reconnects do not require re-entering the owner password while the durable refresh grant remains valid.

For Team/Enterprise organizations, an owner may need to register the custom connector in organization connector settings before members can enable it.

### 3. Enable it in a conversation

In Claude, use the **+** menu in the composer, open **Connectors**, and enable **abird dotlink**. Claude sees only the MCP tools allowed by the running dotlink profile.

## Connect other remote MCP clients

Any remote client that supports MCP Streamable HTTP + OAuth 2.1 can use the same endpoint. dotlink exposes Protected Resource Metadata, OAuth Authorization Server Metadata, authorization-code + PKCE S256, CIMD, DCR fallback, rotating refresh tokens, and revocation.

For a reverse proxy other than ngrok, protect local HTTP with OAuth and tell dotlink its canonical external origin:

~~~bash
dotlink --http --oauth --http-bind=127.0.0.1:3000 \
  --public-url=https://mcp.example.com
~~~

Never derive the OAuth issuer from proxy/Host headers; `--public-url` is the explicit trust boundary. Anonymous OAuth work is also bounded by a trusted network requester identity: direct HTTP uses the TCP peer, while built-in ngrok uses authenticated tunnel connection metadata. Forwarded client-IP headers are ignored. A generic reverse proxy therefore shares one requester allowance unless it applies its own edge admission controls.

Public ngrok ingress is OAuth-protected by default. Direct non-loopback HTTP binds are also refused unless OAuth is enabled with an explicit HTTPS public origin. The only way to intentionally expose either form of public ingress without application-layer authentication is the explicit one-run escape hatch:

~~~bash
dotlink --http --ngrok --allow-public-no-auth
# Or use this machine's concrete LAN interface address (replace as needed):
dotlink --http --http-bind=192.168.1.50:3000 --allow-public-no-auth
~~~

For LAN use, replace the example address with the machine's actual interface address; wildcard binds such as `0.0.0.0`, `::`, and their IPv4-mapped IPv6 forms do not identify that address for Host allowlisting. Conflicting HTTP `Host` and URI/HTTP2 authorities are rejected. This mode exposes every MCP capability granted to the process to anyone who can reach the endpoint and is not recommended.

## Use dotlink as a local sandboxed MCP server

For local MCP clients, no public tunnel is required.

### stdio

Use stdio when the client launches MCP servers as child processes:

~~~bash
dotlink --stdio --allow-rw --allow-shell
~~~

Typical MCP client configuration:

~~~json
{
  "mcpServers": {
    "dotlink": {
      "command": "dotlink",
      "args": ["--stdio", "--allow-rw", "--allow-shell"]
    }
  }
}
~~~

On Linux the shell is still Bubblewrap-sandboxed.

### Local Streamable HTTP

For local clients that support Streamable HTTP:

~~~bash
dotlink --http --allow-rw --allow-shell
~~~

Connect to:

~~~text
http://127.0.0.1:3000/mcp
~~~

This stays loopback-only unless you explicitly change `--http-bind`. Loopback HTTP is unauthenticated by default; add `--oauth` to protect it. When putting local HTTP behind a reverse proxy, also provide the canonical external origin with `--public-url=https://...`.

## Architecture and security

One `LocalMachine` policy core serves every transport:

~~~text
ChatGPT ── OpenAI Secure MCP Tunnel ─┐
Claude.ai / remote MCP ── HTTP/ngrok ├─ LocalMachine ─ policy-aware tools ─ host
Local MCP clients ── stdio/HTTP ─────┘
                                      ├─ Rust filesystem tools
                                      └─ shell → Bubblewrap on Linux
~~~

Profiles persist transports, path grants/denies, shell/network policy, HTTP/ngrok options, and approved developer caches. Enabled profile transports start automatically; CLI flags add or suppress local transports for one run.

### Default security

~~~text
launch-directory read    ON
launch-directory write   OFF
extra filesystem paths   OFF
shell                    OFF
shell network            OFF
shared developer caches  OFF
KVM / GPU devices        OFF
public HTTP ingress      OFF
unsandboxed host mode    OFF
~~~

Capabilities are independent: `--allow-read`, `--allow-write`, `--allow-rw`, `--allow-shell`, and `--allow-network`. Denies always win. `--allow-rw=/` grants filesystem RW only; full unsandboxed host authority requires `--allow-all --no-sandbox`.

### Filesystem policy

~~~text
ls / grep / read / bread      require read
write / bwrite                require write
edit / patch / bedit          require read + write
~~~

Targets are canonicalized before checks; create targets canonicalize their nearest existing ancestor, preventing symlink escapes. The advertised MCP tool list is permission-aware, so unavailable write/shell tools are omitted rather than merely rejected later.

### Tools

| Tool | Needs | What it does |
| --- | --- | --- |
| `ls` | read | List entries with `pattern` (glob), `depth`, `kind`, `sort`, `limit` |
| `grep` | read | Regex search with `content`/`files`/`count` output, context, glob and `.gitignore` filters |
| `read` | read | Read numbered lines from a UTF-8 file |
| `write` | write | Create or replace a text file |
| `edit` | read+write | Apply an ordered `edits[]` array with preflight and per-file atomic commits |
| `patch` | read+write | Apply a Codex-format patch with strict preflight and per-file atomic commits |
| `bread` | read | Read binary content as MCP typed content, base64, or hex |
| `bwrite` | write | Create or replace a binary file |
| `bedit` | read+write | Apply byte ranges against original-file offsets |

| `bash` / `powershell` | shell | Run a command inside the sandbox |

One glob grammar everywhere (`globset`, `literal_separator`): `*`, `?`, `[...]`,
`{a,b}` stay within one path segment and `**` recurses without a level cap.
Every list/search tool has a documented `limit` and reports `truncated`. Walks
stop after 100,000 visited entries; `scan_incomplete` explicitly marks partial
results, which are sorted only among the visited entries. `grep` also limits
file-content work to 32 MiB across at most 4096 candidates per request. When
`gitignore` is enabled, inaccessible/denied ignore files include
`ignore_rules` in `incomplete_reasons` (or a neutral text warning), not silently
interpreted as absent. The singular `incomplete_reason` remains for compatible
clients; the array preserves simultaneous directory, content, traversal,
ignore-rule, and unreadable-file causes. Ignore processing is separately bounded to 256 non-absent
file reads/4 MiB, including partial failed reads, and rejects any file beyond 64 KiB
or 1024 lines. Narrow `path` or `glob`/`pattern` for large trees. `read` errors
rather than emitting a misleading line-zero marker if the first complete line
plus marker cannot fit. Skipping to a distant line offset is separately limited
to exactly 32 MiB of source I/O per call, including buffered prefetch. Otherwise,
`read` ends a capped result with a single marker. `ls` and `grep`
retain the best sorted candidates within the scanned entries in bounded memory.
Both bound their JSON replies at `max_output_bytes`, keeping the longest prefix
that fits. `ls` derives its default depth from the pattern (one level per segment,
unbounded for `**`) so a deep pattern never silently returns nothing, and
`grep` pages only ever contain whole matching lines. File contents are
UTF-8-validated without copying; each content page keeps only a bounded
selection of matching line indices and streams context from the same buffer,
not a vector of every line or hit. `grep count` reports
`page_total` for the returned page. `read` streams a line window, so
`max_read_bytes` bounds the returned window, its marker and one line rather than
rejecting a large file outright; the page is assembled in one buffer, and lines
skipped to reach `offset` are scanned rather than buffered, so a long first line
never blocks a later window; a single returned line larger than the ceiling is
an error naming it, since cutting it silently would hide content.

Text tools (`grep`, `read`, `write`, `edit`, `patch`) accept UTF-8
without binary control bytes. Use `bread`, `bwrite`, or `bedit` for
byte-oriented files.

Every write is atomic: the new bytes go beside the target and are installed only
at commit, so a failed or interrupted write leaves the old file intact and a
reader never sees a half-written one. The native filesystem mutators (`write`,
`bwrite`, `edit`, `patch`, `bedit`) coordinate from snapshot read through
commit with directory locks on Unix: shared ancestor intent locks and an
exclusive lock on the deepest existing parent. Missing parent creation is also
covered; unreadable directories fail closed. Independent subdirectories remain
concurrent, with no lock service or workspace lockfiles. Shell commands are
outside that coordination because their write paths are arbitrary. Existing-file writes keep their permission
bits; create-only writes are absent until commit. `read` with `numbered: false`
returns the text byte for byte, so a read → write round trip preserves line
endings, and `patch` uses the target file's line ending for added lines.
Multi-file calls preflight every file and replace each atomically, but the
commit sequence is not a transaction.

~~~bash
dotlink --list-tools
dotlink --allow-rw --allow-shell --list-tools
~~~

Tool inspection resolves the effective filesystem permissions without initializing transports, loading Runtime credentials, requiring a live SSH agent or preparing the sandbox. Execution still enforces all those prerequisites.

### Linux sandbox

With `--allow-shell`, Bash runs inside Bubblewrap by default. Read grants mount RO, read+write grants mount RW, denied paths are masked or downgraded, networking is isolated unless `--allow-network` is granted, credentials stay hidden, and only approved developer caches/Nix runtime paths are exposed. See `docs/SECURITY.md` for the full sandbox model.

### Full unsandboxed access

Unsandboxed shell execution inherently has the OS user's filesystem and network authority, so dotlink exposes one explicit full-host escape hatch rather than several partial "dangerous" flags:

~~~bash
dotlink --allow-all --no-sandbox
~~~

The two flags require each other. This grants:

~~~text
filesystem   read+write /
shell        enabled
network      enabled
sandbox      disabled
~~~

For unrestricted filesystem access **without** removing the Linux sandbox, use the ordinary path model instead:

~~~bash
dotlink --allow-rw=/
~~~

Then add `--allow-shell` and/or `--allow-network` separately if needed. On Linux, those capabilities remain Bubblewrap-sandboxed unless `--allow-all --no-sandbox` is explicitly selected.

## OpenAI Secure MCP Tunnel

This setup is shown only when OpenAI transport is enabled.

Setup asks for:

1. a Runtime API key with Tunnels Read + Use;
2. a tunnel:
   - **No Admin key in dotlink:** create one at <https://platform.openai.com/settings/organization/tunnels>, then paste its existing `tunnel_...` ID;
   - **Create from dotlink:** type `new` and provide a one-time Admin API key with Tunnels Manage;
3. a ChatGPT Workspace ID or OpenAI Organization ID only when dotlink creates the tunnel.

The one-time Admin key is never persisted.

The Runtime key is stored separately from JSONC config and follows the selected profile:

~~~text
~/.config/abird/dotlink/runtime.key
~/.config/abird/dotlink/runtime.<generation>.key
~/.config/abird/dotlink/runtime.work.key
~/.config/abird/dotlink/runtime.work.<generation>.key
~~~

`DOTLINK_CONFIG` may move the JSONC file, but Runtime keys always stay in dotlink's private XDG directory above. The active config and credential directory are control-plane state and are not exposed through dotlink's MCP/filesystem/shell surface.

When OpenAI transport is disabled for a profile, that profile does not require
or even read a saved Runtime key. At startup, explicit environment credentials
and `DOTLINK_API_KEY_FILE` take precedence over a saved key, so a missing
persisted key never blocks an otherwise valid override.
If a selected Runtime key is missing, enabled OpenAI execution fails closed;
`dotlink profile show default` and `dotlink profile edit default` still work
so you can inspect and replace it. Completed credential rotations prune obsolete
generations, preserving the selected and immediately prior versions;
deleting a profile is retryable after an interrupted cleanup.

### Automatic tunnel recovery

Transient OpenAI poll failures retry with backoff. Normal warnings stay concise; full transport diagnostics are available with `-vv`.

After **10 transient poll failures without an accepted poll**, dotlink stops retrying the same runtime state. Intervening 401/403/404 responses do not falsely reset that health counter. Dotlink cancels all active transports, gives them up to 6 seconds to shut down cleanly, force-aborts any remaining transport tasks, reloads the profile and permission policy, reconstructs the local MCP runtime, and starts the configured transports again automatically.

If a freshly restarted runtime keeps failing, restarts back off from 1 second up to a 30-second cap. Once a runtime has connected successfully, that restart backoff resets. A rejected Tunnel ID or Runtime API key retries for 60 seconds before requiring operator action, so a single auth outage or edge rejection cannot stop the service.

Useful locations:

- Tunnels: <https://platform.openai.com/settings/organization/tunnels>
- Runtime API keys: <https://platform.openai.com/settings/organization/api-keys>
- Admin API keys: <https://platform.openai.com/settings/organization/admin-keys>
- ChatGPT Workspace ID: <https://chatgpt.com/admin>
- OpenAI Organization ID: <https://platform.openai.com/settings/organization/general>

For multi-file text edits, pass an ordered `edits` array with a `path` on
every operation (up to 512 operations and 64 distinct files per call). The `edit` tool has **no top-level `path` or `paths` input**;
it derives the distinct target set from the edits. Planning retains at most
64 MiB of original snapshots and editable text combined. Long success receipts
are shortened *before* committing writes; `truncated` and `omitted_files` state
how many per-file details were omitted, while the operation count remains exact.
Legacy requests with a
single top-level default path still work, but are no longer advertised.

`edit`, `patch`, and `bedit` share a 64 MiB retained-buffer budget.
`edit` (512 edits / 64 files), `patch` (32 files / 64 hunks) and `bedit`
(32 ranges) reject excess operations early. `patch` borrows unchanged file
lines, so dense-line patches do not multiply per-line string allocations or
consume the output write limit as scratch memory. `bedit` hashes and stages
binary changes in fixed-size chunks: its source and resulting file can still
be up to 64 MiB (subject to `max_write_bytes`) without holding two copies in
memory, and unchanged results preserve mtime. Large multi-file success receipts
fit the profile output ceiling using `truncated` and omitted counts, checked
before any commits; `write`/`bwrite` likewise avoid overly long path messages.
Patch file names and update-hunk lines are borrowed from the request; add-file
sections retain one contiguous source range rather than even a per-line slice
vector. CRLF target adjustments are represented as borrowed lines plus a flag;
no `String` allocation is needed for each added line. This avoids millions of tiny allocations for large patches. Existing-file
snapshots are rechecked with a fixed scratch buffer; common text replacements
edit in place. Identical binary ranges avoid staging a temporary while still
checking that the source has not changed. Known HTTP/embedded MCP calls also
avoid an unnecessary second JSON tree where practical. The shared staging
engine preserves strict all-stage-before-commit and
`patch` best-effort behavior. Individual file replacements remain atomic;
a multi-file commit is not a cross-file transaction.

On Linux, normal fine-grained shell grants reject readable mounts overlapping
protected/read-denied paths (even read-only binds). For an intentional
unrestricted filesystem grant, **`--allow-rw=/ --allow-shell` works with
Bubblewrap still enabled**; this explicitly opts out of guaranteed private-file
isolation in the sandboxed shell. Dotlink-private paths are accessible to that shell by design; its native MCP
tools still protect them. Explicit deny rules that cannot be enforced are not silently discarded. Shell
network, PID namespace and host integrations retain independent controls.
Native filesystem tools continue enforcing Dotlink's protected paths.

## Logging

Logging is quiet by default:

~~~text
default   no TOOL / REQ activity
-v        TOOL activity
-vv       TOOL + transport/request REQ diagnostics
-q        suppress TOOL activity (useful with -vv for REQ-only)
~~~

Example with `-v`:

~~~text
[01:06:47.410] TOOL read       → p="./README.md" limit=1
[01:06:47.411] TOOL read       ← ok  1ms
~~~

Shell commands are logged in full when TOOL verbosity is enabled, using a compact
single-line, escaped format:

~~~text
[01:06:48.410] TOOL bash       → d="./src" c="echo hello\nls -la" max_b=30000 max_t=60
[01:06:48.418] TOOL bash       ← ok  8ms
~~~

`p` and `d` show `./...` for resolved paths inside Dotlink's launch directory
and absolute `/...` paths outside it. The labels follow existing symlinks on a
best-effort basis, so a symlink pointing outside normally logs its outside
target; actual permission checks remain independent. For writes, `b="N"` reports payload bytes;
`edit` uses a bounded, deduplicated `paths=[...]` preview. These are audit
labels only: permissions use the original paths. Enable TOOL logs deliberately,
since shell commands may include inline credentials.

With `-vv`, transport diagnostics are added:

~~~text
[01:06:47.426] REQ  stdio      → initialize
[01:06:47.427] REQ  stdio      → tools/call
~~~

TOOL logs prioritize `p` (path), `d` (working directory), then complete `c` (shell command). By design, `-v` logs the **complete** shell command as `c="..."`, with newlines/control characters escaped onto one journal line—not `command_bytes` and not a truncated excerpt. **Inline passwords or tokens inside commands will be stored in journald** when TOOL logging is enabled. Keep that setting intentional and restrict journal access/retention accordingly. File contents, patches, search patterns, binary data and stdin remain byte counts; the default is quiet.

Color is automatic on interactive human-facing output, including `--help`. Override with `--color=always`, `--color=never`, or `--color=auto`. Interactive terminal runs also support `v` for live verbosity cycling and `Ctrl+R` for a full runtime restart; stdio keeps stdin protocol-clean and exposes only `Ctrl+C`. In a manual TTY stdio run, dotlink ensures `Ctrl+C` generates an interrupt and restores the terminal state afterward.

## CLI summary

The built-in help is the source of truth: `dotlink --help`, `dotlink profile --help`, and `dotlink oauth --help`.

~~~text
SETUP & PROFILES
-S, --setup                     interactive setup/editor for the selected profile
-p, --profile <NAME>            run/setup with config.<NAME>.jsonc

dotlink profile list
dotlink profile show <NAME>
dotlink profile create <NAME>
dotlink profile edit <NAME>
dotlink profile delete <NAME>
dotlink profile allow <NAME> <read|write|rw> <PATH>
dotlink profile remove-allow <NAME> <read|write|rw> <PATH>
dotlink profile deny <NAME> <read|write|rw> <PATH>
dotlink profile remove-deny <NAME> <read|write|rw> <PATH>
dotlink profile enable <NAME> <SETTING>
dotlink profile disable <NAME> <SETTING>

Persisted SETTING values:
openai, stdio, http, http-ephemeral-url, ngrok, ngrok-ephemeral-url,
oauth, default-allow, shell, network

OAUTH MANAGEMENT
dotlink oauth status [-p <NAME>]
dotlink oauth clients [-p <NAME>]
dotlink oauth revoke [-p <NAME>] <CLIENT_ID>
dotlink oauth revoke-all [-p <NAME>]

TRANSPORTS
--stdio                         add stdio MCP for this run
--no-stdio                      suppress profile stdio for this run
--http                          add HTTP MCP for this run
--no-http                       suppress profile HTTP + ngrok for this run
--http-bind <ADDR>              override HTTP listen address

REMOTE HTTP & OAUTH
--ngrok                         publish effective HTTP through ngrok
--no-ngrok                      suppress profile ngrok for this run
--ngrok-domain <DOMAIN>         use a stable/reserved ngrok hostname
--no-ngrok-domain               ignore persisted ngrok domain for this run
--oauth                         protect local/reverse-proxied HTTP with OAuth
--no-oauth                      disable local HTTP OAuth for this run
--public-url <HTTPS-ORIGIN>     canonical OAuth origin behind a reverse proxy
--allow-public-no-auth          deliberately expose public HTTP/ngrok without OAuth
--ephemeral-url                 use ephemeral local HTTP + ngrok MCP paths
--http-ephemeral-url[=<BOOL>]   override local HTTP path behavior
--ngrok-ephemeral-url[=<BOOL>]  override ngrok path behavior

FILESYSTEM ACCESS
--allow-read[=<DIR>]            add read; bare means launch directory
--allow-write[=<DIR>]           bare: rw launch directory; with DIR: write-only
--allow-rw[=<DIR>]              add rw; bare means launch directory
--no-default-allow              remove implicit launch-directory read for this run
--deny-read[=<DIR>]             deny read; bare means launch directory
--deny-write[=<DIR>]            deny write; bare means launch directory
--deny-rw[=<DIR>]               deny read+write; bare means launch directory
--deny <PATH>                   legacy synonym for --deny-rw=<PATH>

SHELL & NETWORK
--allow-shell                   enable Bash (Unix) / PowerShell (Windows)
--deny-shell                    disable shell; deny always wins
--allow-network                 network inside the Linux shell sandbox
--deny-network                  deny shell network; deny always wins
--allow-rw=/                    filesystem rw / while keeping Linux sandboxing
--allow-all --no-sandbox        explicit full-host filesystem + shell + network

SHELL INTEGRATIONS (Linux Bubblewrap only; separately opted in)
--allow-nix-daemon              share only the Nix daemon Unix socket
--allow-nix-config              mount user's nix.conf read-only
--allow-ssh-agent               share SSH agent socket; never private key files
--allow-git-config              mount ~/.gitconfig and XDG Git config read-only

HARDWARE (Linux Bubblewrap; separate opt-in device access)
--allow-kvm                     pass /dev/kvm for accelerated VMs
--allow-gpu=amd                 pass AMD GPU render/KFD nodes and driver runtime
--allow-gpu=nvidia              pass NVIDIA CUDA/render nodes and driver runtime
                                --allow-gpu may be repeated for both vendors

DEVELOPER CACHES (discovered paths; bare flag = ro, =rw enables writes)
--allow-cache-cargo-registry[=ro|rw]   --allow-cache-cargo-git[=ro|rw]
--allow-cache-npm[=ro|rw]              --allow-cache-pnpm[=ro|rw]
--allow-cache-yarn[=ro|rw]             --allow-cache-pip[=ro|rw]
--allow-cache-uv[=ro|rw]               --allow-cache-go-mod[=ro|rw]
--allow-cache-go-build[=ro|rw]         --allow-cache-maven[=ro|rw]
--allow-cache-gradle[=ro|rw]           --allow-cache-sccache[=ro|rw]
--allow-cache-ccache[=ro|rw]

OUTPUT & DIAGNOSTICS
--list-tools                    show tools exposed by the effective policy
--print-id                      print configured tunnel ID (no Runtime key needed)
-s, --serialize                 execute MCP tools one at a time (opt-in)
-q, --quiet                     suppress TOOL activity
-v, --verbose                   -v TOOL; -vv TOOL + REQ
--color <auto|always|never>     control colors in help and human output
~~~

## Host Nix, Git, SSH agent, and developer caches

Linux Bubblewrap starts with a private home and no host SSH keys, Git config,
Nix config, or daemon socket. These optional *shell-only* grants expose just
the selected resource, not the whole user home or additional native MCP file
permissions. Interactive `dotlink --setup` discovers the available services
and caches, then asks before enabling each one; discovery never grants access.

~~~bash
dotlink --allow-rw --allow-shell \
  --allow-nix-daemon --allow-nix-config \
  --allow-git-config --allow-ssh-agent \
  --allow-cache-cargo-registry=rw --allow-cache-cargo-git=rw
~~~

- **Nix:** The daemon socket is available without `--allow-network`, and
  dotlink selects `NIX_REMOTE=daemon` internally. Your XDG
  `~/.config/nix/nix.conf` is mounted read-only into the private sandbox
  home if it exists. Ordinary `nix build`, `nix run`, and `nix develop` can
  then use your host Nix daemon, whose own fetch/build environment is *outside*
  Bubblewrap. It is a meaningful additional authority, not ordinary network
  access.
- **Git:** Only `~/.gitconfig` and `$XDG_CONFIG_HOME/git/config` are shared
  read-only when present. Their `include`/`includeIf` files are **not**
  implicitly shared. Beware plaintext tokens or credential helper settings
  inside Git configuration; only opt in for trusted shell workloads.
- **SSH agent:** The detected agent (including GCR when no session
  `SSH_AUTH_SOCK` exists) is mounted into the private shell, without mounting
  `.ssh` or private
  key files. **Agent access can request signatures and authentication using
  *any* key that agent offers**; it is not limited to Git commits. Consider a
  dedicated signing-only agent or `ssh-add -c` confirmations. The sandbox also
  receives minimal read-only Unix account lookup files needed by OpenSSH.

The startup banner lists the host integrations actually mounted, without exposing socket locations or secret values.

For agent-backed Git commit signing, configure `gpg.format=ssh`,
`commit.gpgsign=true`, and a `user.signingkey` in the form
`key::ssh-ed25519 AAAA...` (the public key text). A signing-key path
under `~/.ssh` is not automatically mounted.

All 13 cache families already supported by setup also have matching
`--allow-cache-<kind>[=ro|rw]` flags. Each finds the existing host cache
using the same discovery rules as onboarding. Bare flags share **read-only**;
`=rw` explicitly permits modifications to the host cache. An explicitly
requested cache whose directory does not exist is skipped with a warning
instead of failing startup, so declarative services do not break when a
cache has not been created yet. Cache grants
never expand filesystem MCP tool permissions and honor deny rules.

Systemd services have no reliable interactive `SSH_AUTH_SOCK` environment.
For `services.dotlink.hostAccess.sshAgent = true`, no socket path is needed
when GCR runs as the service user: dotlink discovers
`/run/user/<uid>/gcr/ssh` automatically. Detection prefers an explicit
`services.dotlink.sshAgentSocket` (a strict override), then a valid
`SSH_AUTH_SOCK`, `$XDG_RUNTIME_DIR/gcr/ssh`, and the effective user's
`/run/user/<uid>/gcr/ssh`. The module also includes Nix, Git, and OpenSSH
executables for enabled integrations. A system service does not start the GCR
agent: it must already be available at service startup. Restart dotlink after
the host agent/socket changes. See
[.agents/docs/nixos-service.md](.agents/docs/nixos-service.md).

## Hardware acceleration inside Bubblewrap

Hardware devices are **off by default**, independently of network and file
permissions. To run hardware-accelerated QEMU, GPU inference, CUDA/ROCm builds
or Vulkan tests without turning off Bubblewrap:

~~~bash
dotlink --allow-shell --allow-rw --allow-kvm \
  --allow-gpu=amd --allow-gpu=nvidia
~~~

The vendor flags are repeatable. `--setup` discovers available KVM/AMD/NVIDIA
hardware and asks before enabling each capability. Only selected character
devices become accessible inside the sandbox. AMD GPU DRM nodes are verified
using the kernel PCI vendor ID (`0x1002`), NVIDIA DRM nodes using `0x10de`;
unknown/Intel devices stay hidden. AMD also uses `/dev/kfd` when present;
NVIDIA uses the relevant `/dev/nvidia*` and capability nodes. Driver search
locations (`/run/opengl-driver`, Vulkan/OpenCL ICD paths, sysfs, and relevant
NVIDIA proc metadata) are shared **read-only**. NixOS graphics drivers must
already be configured on the host; dotlink does not bundle GPU toolchains.

`--allow-kvm` requires `/dev/kvm` on the host; VMs requiring TAP/TUN or GUI
displays may need separately authorized networking/display integration. Linux
user and group permissions still apply. GPU and KVM ioctls execute against
host kernel drivers, so treat these as significant additional permissions.
Requested hardware that is unavailable fails clearly rather than silently
falling back to software or allowing other devices. Changes to host driver or
device-node identities require restarting dotlink.

For NixOS services, configure `hostAccess.kvm = true;` and/or
`hostAccess.gpu = [ "amd" "nvidia" ];`. The module adds only the appropriate
service-level `kvm`, `render` and `video` supplementary groups, without
changing the Unix user's regular group membership. Provide optional GPU
utilities through `extraPackages` or your project's `nix develop` shell.
See [.agents/docs/nixos-service.md](.agents/docs/nixos-service.md).

## Parallel tool execution

A single tunnel can dispatch up to sixteen in-flight MCP requests. On Linux,
file tools use descriptor-rooted `openat2` access and Bubblewrap shells run
independently; **tools execute concurrently by default**. Local HTTP and stdio
intentionally have **no global admission cap**: the client/model directs
parallelism. The tunnel maintains its own in-flight limit. Choose
`dotlink -s` or `dotlink --serialize` if you want one MCP call at a time
across all transports handled by this process. This is an *ordering preference*,
not an additional security boundary. It does not serialize separate dotlink
processes or subprocesses that outlive their MCP call.

The Linux path enforces access beneath pinned directory handles, refuses
symlink traversal during the final operation, and rejects nested mountpoints
unless the user has explicitly authorized filesystem root `/`. It requires
Linux `openat2` (kernel 5.6+); if unavailable, native tool startup fails
closed rather than falling back to unsafe pathname I/O. Existing safe symlink
aliases are canonicalized before the final descriptor-relative access.
On macOS, the pinned Unix directory model uses a component-wise
openat/O_NOFOLLOW walk. On Windows, native tools pin directory handles and
reject reparse points, while named cross-process mutexes coordinate writers
to the same path across services and CLI instances. Windows also retains
its in-process gate; -s opts into global serial execution. External applications
do not participate in these advisory locks.

## FAQ

### I changed dotlink permissions. How do I update ChatGPT?

Restart dotlink (or press `Ctrl+R` in an interactive run) so the new profile/policy is active, then open **<https://chatgpt.com/settings/plugins-settings>**, select **abird dotlink**, and click **Refresh tools**.

Refreshing is especially important when the exposed tool set changes — for example, adding/removing write or shell access. It is also a good habit after changing path permissions so ChatGPT's connection metadata is definitely current.

### Does ChatGPT get access to my whole computer?

No. dotlink starts read-only on the launch directory and exposes only the capabilities you grant. Write, shell, network, extra paths, caches, and unsandboxed access are separate opt-ins; deny rules always win.

### Do I need to upload files to ChatGPT first?

No. Once connected, ChatGPT can read permitted local files directly through dotlink. That is useful for code, data analysis, reports, slide decks, and other local artifacts without repeatedly uploading/downloading files.

### Do I need to expose a public port for ChatGPT?

No. The ChatGPT path uses the outbound OpenAI Secure MCP Tunnel. Your MCP server stays local; no public inbound port or third-party relay is required.

### Can I use more than one project or permission set?

Yes. Use named profiles such as `dotlink -p work` and manage them with `dotlink profile ...`. Each profile can keep its own transports, permissions, shell/network settings, caches, and OpenAI tunnel state.

### Does changing a profile automatically change a running dotlink process?

Not until the runtime reloads it. Restart dotlink or press `Ctrl+R` during an interactive run. If the ChatGPT-visible tool surface changed, also use **Refresh tools** in ChatGPT afterward.

### Can I use dotlink with Claude.ai or local MCP clients too?

Yes. Claude.ai and other remote MCP clients can use the HTTP/ngrok transport; local clients can use stdio or loopback Streamable HTTP. All transports share the same local permission policy.

### Where are dotlink's config and secrets stored?

Profiles live under `~/.config/abird/dotlink/` (or the equivalent XDG config directory). OpenAI Runtime API keys are stored separately from JSONC profiles and are never shown back in plaintext during setup.

OAuth state lives separately under `$XDG_STATE_HOME/abird/dotlink/` (normally `~/.local/state/abird/dotlink/`). dotlink stores only the Argon2id owner-password hash, approved DCR client metadata, and SHA-256 hashes of refresh tokens there; plaintext owner passwords, access tokens, and authorization codes are not persisted.

### How do I revoke a remote OAuth client?

Use `dotlink oauth clients -p <profile>` to inspect approved DCR clients, then `dotlink oauth revoke -p <profile> <client-id>`. `dotlink oauth revoke-all -p <profile>` revokes all persisted refresh grants. Restart dotlink or press `Ctrl+R` if you also want all currently issued short-lived access tokens invalidated immediately.

## NixOS service

NixOS users can enable dotlink as a systemd service directly from this flake. By default it **reuses the selected user's existing profile, saved Runtime key, and OAuth state**—no duplicated setup or generated configuration required:

~~~nix
# Add inputs.dotlink.nixosModules.default to your NixOS modules.
services.dotlink = {
  enable = true;
  user = "alice";
  workingDirectory = "/home/alice/projects";
};
~~~

The explicit working directory determines what dotlink's default `"."` read grant means. Use `profile = "work";` to select an existing named profile. The default user is a dedicated `dotlink` system account; set `user` to your normal account to reuse its saved profile. Since this is a system service, custom shell-session XDG environment variables must be provided through `services.dotlink.environment`.

For declarative configuration, set `services.dotlink.settings` using the existing dotlink JSONC schema. It is generated as public Nix-store JSON (never put secrets in it). Leave `settings` unset to use the user's existing config. Optionally override only a credential via systemd `LoadCredential`:

~~~nix
services.dotlink.credentials.runtimeApiKeyFile =
  config.sops.secrets.dotlink-runtime.path;
~~~

Omitting this option retains the user's saved Runtime key. `credentials.ngrokAuthTokenFile` works similarly. There are also `instances.<name>` for independent named services, `extraPackages` for shell tools, and `logging.verbosity = "lifecycle" | "tools" | "requests"` (default: lifecycle). The service disables stdio MCP, gracefully handles SIGTERM, and logs to journald. Exits 2 and 78 stay stopped until fixed and explicitly restarted; exit 6 (`NOTCONFIGURED`, setup still pending) also stays stopped but counts as a clean stop, so an unprovisioned host never fails its own activation; any other termination, including exit 0 or a clean signal outside a systemd stop/restart job, restarts with exponential backoff up to 10 minutes. Explicit `systemctl stop` remains stopped. The module requires systemd 255 or newer and warns on 255–257 because backoff reset notifications require 258:

~~~bash
systemctl status dotlink
journalctl -u dotlink -f
~~~

See [NixOS service setup, profiles, multi-instance operation, logging and security](.agents/docs/nixos-service.md) for the full option and lifecycle details.

## Build

Nix/Crane native workflow:

~~~bash
nix build
nix run .
nix flake check
nix develop
~~~

Inside `nix develop`:

~~~bash
cargo fmt --check
alejandra --check flake.nix nix
markdownlint-cli2
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --all-features
~~~

`nix flake check` runs the package build, Rust tests, rustfmt, Clippy (warnings
denied), Markdown linting, Alejandra formatting checks, and Linux NixOS module
checks. Each check is independently selectable, for example
`nix build .#checks.x86_64-linux.markdownlint`. `nix fmt` formats Nix files with
Alejandra. Markdown linting includes `.agents/` documentation; its configuration
allows soft-wrapped prose and repeated headings in separate changelog sections.
CI runs these checks before building release artifacts.

Build the complete reproducible release bundle from an x86_64 Linux host:

~~~bash
nix build .#release-all
# or copy the bundle into ./dist:
./scripts/build-release-artifacts.sh
~~~

The bundle contains static Linux x86_64/ARM64 binaries, Windows x86_64/ARM64, and macOS ARM64. The static Linux builds run across NixOS, Debian, and other compatible distributions. Individual Nix outputs are available as `dist-linux-*`, `dist-windows-*`, and `dist-macos-aarch64`.

To publish a GitHub Release, push a matching version tag such as `v0.8.0`. CI rebuilds the Nix release graph and uploads each binary plus its `.sha256` sidecar as an individual release asset; the Actions ZIP bundle is not published as the release.

Cross-build outputs, stable filenames, installer overrides, and platform release details: `.agents/docs/release-install.md`.

## macOS and Windows

The filesystem MCP tools (`read`, `write`, `edit`, `ls`, binary tools) still enforce dotlink's allow/deny policy on macOS and Windows.

The shell is different: Bubblewrap is Linux-specific, so native macOS and Windows do not currently have an equivalent dotlink shell sandbox. To enable shell execution natively on those platforms, use the explicit full-host mode:

~~~bash
dotlink --allow-all --no-sandbox
~~~

That intentionally removes dotlink's shell isolation and gives the child shell the OS user's filesystem/network authority. Use it only when that is what you want.

### Windows recommendation: WSL2

For Windows development, the recommended secure shell workflow is to run dotlink **inside WSL2** and use the normal Linux Bubblewrap sandbox there:

~~~bash
# inside WSL2
dotlink --allow-rw --allow-shell
~~~

That preserves the same Linux permission/mount/network model described above instead of exposing an unrestricted native PowerShell shell.

Native Windows can still use the Rust filesystem tools with allow/deny enforcement without enabling PowerShell.

### macOS

macOS can use the Rust filesystem tools with the same allow/deny policy, plus stdio/HTTP/OpenAI transports. Because Bubblewrap is unavailable, shell execution requires `--allow-all --no-sandbox` until a macOS-native sandbox backend is added.

Apple Silicon release binaries are cross-built reproducibly from the same x86_64 Linux Nix release graph using a pinned Apple SDK and LLVM's Mach-O linker. Intel macOS is not currently published; build it from source with Cargo if needed.

## Contributing

We build dotlink in the open and would love your ideas, architectural challenges,
research, and focused improvements. See [CONTRIBUTING.md](CONTRIBUTING.md) for
the best ways to take part.

## AI disclosure

**Human-centric design. AI-centric implementation.** dotlink is built with
extensive AI assistance for research, code, tests, and documentation. We believe
the best software comes from human-led design and AI-assisted implementation:
humans own the architecture, priorities, and acceptance criteria, while AI helps
implement, test, and iterate. We don't hand direction to autonomous models or
claim every generated line is manually reviewed — correctness rests on explicit
designs, reproducible tests, and continuous validation.

## License

[MIT](LICENSE) © 2026 Prasanna Loganathar and dotlink contributors. Third-party
libraries and assets retain their respective licenses.
