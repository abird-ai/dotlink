# abird-link

**Connect ChatGPT Web — including Spaces and dots — to the files and tools on your computer, securely from a single binary.**

abird-link turns your local project into a **live working context for ChatGPT**. Keep briefs, design decisions, research, handoffs, and other durable project knowledge in **Spaces**; use ChatGPT to reason and design with that context; let a **dot** carry ongoing work forward where available; then reconnect to the same live repository and toolchain to continue implementation.

Your local project becomes the shared source of truth across **ChatGPT, Codex, Spaces, and dots**: the same Git history, plans, files, generated artifacts, and tools can carry work from one surface to another without pretending their private model state or memory is literally shared.

- **Let ChatGPT Web build, test, and run tools on your machine securely** — read and edit code, run Git, builds, tests, scripts, compilers, package managers, and other tools you explicitly expose.
- **Move between thinking and implementation without losing the project** — plan or document in ChatGPT and Spaces, then reconnect to the live repo and keep building from the current state.
- **Work with local data and files without the upload/download loop** — let ChatGPT read data directly from your machine for analysis, reports, documents, or slide decks instead of repeatedly copy-pasting or moving files in and out of chat.
- **Use ChatGPT as a practical fallback when Codex usage is unavailable or exhausted** — in a pinch, connect ChatGPT Web to the same repository and toolchain and let it continue from the project state already on your machine.

That context also gives you continuity when the machine or tunnel is temporarily offline: keep discussing architecture, planning changes, or designing against project context already present in ChatGPT, then reconnect later and have ChatGPT re-read the live repository, verify what changed, and continue.

For ChatGPT, `abird-link` opens an outbound **OpenAI Secure MCP Tunnel directly from your machine to OpenAI**. Your MCP server stays local: no public inbound port, no ngrok, and no third-party relay in the ChatGPT path. One binary provides the MCP server, tunnel transport, permission engine, profiles, activity logging, and — on Linux — a Bubblewrap shell sandbox.

It also works beyond ChatGPT:

- **Claude.ai and other remote MCP clients** through Streamable HTTP, optionally published over HTTPS/ngrok.
- **Local MCP clients** through stdio or loopback Streamable HTTP.
- **One permission model everywhere** — every transport reaches the same policy-controlled tool surface.

Security is opt-in and local. The working directory starts read-only; write access, shell, network, extra paths, and shared developer caches are separate grants. Deny rules take precedence, unavailable capabilities disappear from the MCP tool list, and Linux shell execution is Bubblewrap-sandboxed by default.

- **Use your real local tools** — Bash, Git, Cargo, npm, test runners, scripts, compilers, and anything else you explicitly expose.
- **Keep private machines private** — ChatGPT can reach a local/private MCP server through OpenAI Secure MCP Tunnel without publishing it to the internet.
- **See what the AI is doing** — tool attempts and completions are logged locally by default.
- **Reuse trust profiles** — keep different cwd, filesystem, shell, network, and cache policies for different projects.

## 60-second quick start

Run the guided setup once:

~~~bash
abird-link --setup
~~~

Then start abird-link from the directory you want the assistant to work with:

~~~bash
cd ~/src/my-project
abird-link
~~~

By default, the project is exposed read-only.

Give the assistant read/write access to the current project:

~~~bash
abird-link --allow-rw
~~~

Add a sandboxed shell for builds, tests, Git, and local tooling:

~~~bash
abird-link --allow-rw --allow-shell
~~~

Allow network access inside that sandbox too:

~~~bash
abird-link --allow-rw --allow-shell --allow-network
~~~

You decide what the assistant can access. abird-link enforces those permissions locally.

### What it looks like

A normal ChatGPT/OpenAI profile starts with a compact security summary, then shows each MCP tool call as it happens:

~~~text
$ abird-link -p aw

abird-link 0.5.0
────────────────────────────────────────────────────────
• Profile    aw
• Transports openai
• Tunnel     tunnel_…
• Cwd        /home/pvl/spaces/abird/src/aw
• Access     read:2 write:1 deny-read:0 deny-write:0 caches:0 + shell
• Sandbox    Bubblewrap
• Network    enabled
• Status     starting…

OpenAI Secure MCP Tunnel active.
Ctrl-C to stop.

✓ Connected — ready

[10:59:19.184] TOOL read       → path=.agents/plans/MASTER-HANDOFF.md
[10:59:19.194] TOOL read       ← ok  9ms
[10:59:20.534] TOOL read       → path=.agents/plans/PHASE19-23-CONTINUATION-STATUS.md
[10:59:20.538] TOOL read       ← ok  4ms
[10:59:21.650] TOOL read       → path=.agents/plans/FULL-HANDOFF.md
[10:59:21.653] TOOL read       ← ok  2ms
[10:59:22.700] TOOL read       → path=.agents/plans/ROADMAP.md
[10:59:22.701] TOOL read       ← ok  1ms
[10:59:49.623] TOOL read       → path=.agents/plans/MASTER-HANDOFF.md offset=1261 limit=400
[10:59:49.624] TOOL read       ← ok  1ms
~~~

By default, abird-link logs every MCP tool attempt and completion locally to stderr, including safe metadata such as paths, flags, status, and latency. It does **not** intentionally log file contents, binary payloads, API keys, or raw request bodies. Use `-s/--silent` to hide normal TOOL activity; use `-v/--verbose` to additionally show transport/request-level REQ diagnostics.

## Connect abird-link to ChatGPT

### 1. Configure the OpenAI tunnel

Run:

~~~bash
abird-link --setup
~~~

Choose **openai** as a transport. Setup will guide you through creating or selecting an OpenAI Secure MCP Tunnel and choosing your default local permissions.

Then run abird-link from the project you want ChatGPT to access:

~~~bash
cd ~/src/my-project
abird-link
~~~

Keep this process running while you use ChatGPT.

The startup output includes a tunnel ID such as:

~~~text
• Tunnel     tunnel_...
✓ Connected — ready
~~~

### 2. Enable Developer mode in ChatGPT

In ChatGPT:

1. Open **Settings**.
2. Select **Security and login**.
3. Turn on **Developer mode**.

Developer mode availability can depend on your account or workspace policy.

### 3. Add the connection

In ChatGPT:

1. Open **ChatGPT Plugins**.
2. Select the **+** button.
3. Give the connection a name such as **Abird Link**.
4. Add a short description such as **Secure access to files and tools on my computer**.
5. Under **Connection**, choose **Tunnel**.
6. Select the available tunnel, or paste the `tunnel_...` ID printed by abird-link.
7. Create the connection.
8. Review the tools and metadata ChatGPT discovers.

The discovered tools reflect the permissions of the currently running abird-link process.

A default read-only connection typically exposes:

~~~text
ls
read
read_binary
~~~

With write access, ChatGPT can also see tools such as:

~~~text
edit
patch_binary
write
write_binary
~~~

With shell enabled, it additionally sees:

~~~text
bash         # Linux / Unix
powershell   # Windows
~~~

### 4. Use it in a ChatGPT conversation

Start a new conversation, open the **tools / More** menu from the prompt box, and select **Abird Link**.

Then ask normally — you do not need to name individual MCP tools.

For example:

~~~text
Inspect this project and explain its structure.
~~~

~~~text
Read README.md and Cargo.toml and tell me how this project works.
~~~

With write access:

~~~text
Update the README to document the new profile system.
~~~

With shell access:

~~~text
Run the test suite and fix any failures you find.
~~~

~~~text
Check git status, review the current diff, run the formatter, linter,
and tests, then summarize anything that still needs attention.
~~~

### 5. Optional: call a packaged Abird Link plugin directly

The developer connection above is enough to use abird-link as an MCP tool source.

If you package that connection as a personal **Abird Link** plugin using the included Plugin Creator instructions, you can invoke it directly from a ChatGPT **Work** conversation:

~~~text
@Abird Link inspect this repository and tell me what changed.
~~~

or:

~~~text
@Abird Link run the tests and fix the failing ones.
~~~

Type `@` in the Work prompt box and select **Abird Link** to invoke it explicitly.

### If ChatGPT cannot find the tunnel

Check that:

- abird-link is still running and connected;
- you copied the correct `tunnel_...` ID;
- Developer mode is enabled;
- the tunnel is associated with the ChatGPT workspace you are currently using;
- the Runtime API key has **Tunnels Read + Use** permission.

After changing tool names, schemas, permissions, or metadata, restart abird-link and use **Refresh** on the developer connection in ChatGPT before retesting.

## Connect abird-link to Claude.ai

Claude.ai custom connectors use **remote MCP**: Claude connects from Anthropic's cloud, not from your local machine. That means `http://127.0.0.1:3000/mcp` will not work directly with claude.ai; expose abird-link through a public HTTPS endpoint such as ngrok.

### 1. Start a public Streamable HTTP MCP endpoint

Set your ngrok token, then run:

~~~bash
export NGROK_AUTHTOKEN='...'
abird-link --http --ngrok --ngrok-ephemeral-url
~~~

Add whatever local permissions you actually want Claude to have, for example:

~~~bash
abird-link --http --ngrok --ngrok-ephemeral-url \
  --allow-rw --allow-shell
~~~

abird-link prints a URL similar to:

~~~text
✓ ngrok MCP: https://example.ngrok.app/mcp/<ephemeral-token>
~~~

### 2. Add it to Claude.ai

For individual Claude plans:

1. Open **Customize → Connectors**.
2. Select **+**.
3. Choose **Add custom connector**.
4. Give it a name such as **Abird Link**.
5. Paste the ngrok MCP URL printed by abird-link.
6. Add the connector.

For Team/Enterprise organizations, an owner may need to register the custom connector under the organization's connector settings first; members can then connect and enable it.

### 3. Enable it in a conversation

In Claude, use the **+** menu in the chat composer, open **Connectors**, and enable **Abird Link** for that conversation. Claude can then call the tools exposed by the running abird-link process.

> **Security:** abird-link's HTTP/ngrok transport does not currently add application-layer authentication. Treat the public URL as sensitive. An ephemeral path makes accidental discovery much harder, but it is not authentication. Use ngrok access controls where appropriate and grant only the minimum local permissions needed.

## Connect other remote MCP clients through ngrok

Any MCP client that supports remote **Streamable HTTP** can use the same public endpoint:

~~~bash
abird-link --http --ngrok --ngrok-ephemeral-url
~~~

Then give the client the printed HTTPS MCP URL. The remote client receives exactly the tool surface and permissions exposed by that abird-link process.

## Use abird-link as a local sandboxed MCP server

For local MCP clients, no public tunnel is required.

### stdio

Use stdio when the client launches MCP servers as child processes:

~~~bash
abird-link --stdio --allow-rw --allow-shell
~~~

Typical MCP client configuration:

~~~json
{
  "mcpServers": {
    "abird-link": {
      "command": "abird-link",
      "args": ["--stdio", "--allow-rw", "--allow-shell"]
    }
  }
}
~~~

On Linux the shell is still Bubblewrap-sandboxed.

### Local Streamable HTTP

For local clients that support Streamable HTTP:

~~~bash
abird-link --http --allow-rw --allow-shell
~~~

Connect to:

~~~text
http://127.0.0.1:3000/mcp
~~~

This stays loopback-only unless you explicitly change `--http-bind`.

## How it connects

The same LocalMachine MCP server can be exposed through three independent transports:

~~~text
                         LocalMachine
            read / write / edit / ls / binary / shell
                              |
               +--------------+--------------+
               |              |              |
             OpenAI          stdio      Streamable HTTP
          Secure Tunnel   stdin/stdout       /mcp
                                             |
                                             +-- optional ngrok
                                                 public HTTPS /mcp
~~~

The transports are modular: OpenAI, stdio, and HTTP can each be enabled or disabled in persisted setup. Profiles can also persist cwd, allow/deny path rules, shell, network, and approved developer-cache access.

## Technical architecture

abird-link is one binary with one local policy core and multiple connection adapters. The transport you use changes **how an AI reaches abird-link**, not **what it is allowed to do**.

~~~text
                  ChatGPT
                     |
          OpenAI Secure MCP Tunnel
                     |
                     v
Claude.ai / remote AI tools        Local MCP clients
          |                         |            |
     HTTPS / ngrok                stdio     loopback HTTP
          |                         |            |
          +------------+------------+------------+
                       |
                 transport adapters
                       |
                       v
               +-------------------+
               |   LocalMachine    |
               | shared MCP server |
               +-------------------+
                       |
              policy-aware tool router
                       |
          +------------+-------------+
          |                          |
   Rust filesystem tools         shell tool
 read/write/edit/ls/binary      Bash / PowerShell
          |                          |
 canonical path policy          Linux: Bubblewrap
          |                          |
 allow + deny roots             mount + namespace
          |                          |
          +------------+-------------+
                       |
                  host machine
~~~

Every transport uses the same `LocalMachine` instance and therefore the same effective profile, cwd, allow/deny paths, cache grants, shell policy, and network policy. Running OpenAI Tunnel and HTTP at the same time does not create two different security domains.

### Opt-in security model

The default posture is intentionally small:

~~~text
cwd read access          ON
cwd write access         OFF
extra filesystem paths   OFF
shell                    OFF
shell network            OFF
shared developer caches  OFF
public HTTP ingress      OFF
unsandboxed host mode    OFF
~~~

Capabilities are added independently:

~~~text
--allow-read=DIR
--allow-write=DIR
--allow-rw=DIR
--allow-shell
--allow-network
~~~

Profiles persist the same model in JSONC. CLI grants merge on top of profile grants; deny rules take precedence in the normal sandboxed model.

`--allow-rw=/` is simply a root filesystem RW grant. It does **not** automatically enable shell, network, or disable Bubblewrap.

The only full unsandboxed escape hatch is:

~~~bash
abird-link --allow-all --no-sandbox
~~~

That pair intentionally grants root filesystem RW, shell, and network with no shell sandbox. Deny rules are rejected in that mode because an arbitrary unsandboxed child process cannot be constrained by abird-link's Rust path checks.

### Filesystem policy layer

The Rust filesystem tools always go through the same canonical-path policy before touching the host filesystem.

~~~text
read / read_binary / ls       require read
write / write_binary          require write
edit / patch_binary           require read + write
~~~

Existing targets are canonicalized before policy checks, and create targets canonicalize their nearest existing ancestor. This prevents symlink traversal from escaping an allowed root.

The visible MCP tool router is also permission-aware. A read-only process does not merely reject `write`; the write/edit/patch tools are omitted from the advertised MCP tool list. Shell is similarly absent until explicitly enabled.

### Linux Bubblewrap shell sandbox

On Linux, `--allow-shell` runs Bash inside Bubblewrap by default. The Rust filesystem policy determines what Bubblewrap mounts into the child:

~~~text
read-only grant       host path -> same path, RO
read+write grant      host path -> same path, RW
write-only grant      not exposed to shell
deny-read             hidden/masked
deny-write            readable subtree rebound RO
~~~

The sandbox also creates a constrained runtime environment:

~~~text
/proc                  fresh proc mount
/dev                   Bubblewrap-managed minimal /dev
/tmp                   private tmpfs
HOME                   /tmp/home
developer caches       only explicitly approved cache dirs
/nix/store             RO on NixOS
Nix profile/runtime    RO where required
Nix daemon endpoint    hidden when shell network is denied
~~~

Bubblewrap also isolates PID, IPC, and UTS namespaces. The shell network namespace is unshared by default; `--allow-network` restores host network access for the shell child.

Approved Cargo/npm/uv/etc. caches are mounted only into expected locations under the private sandbox home. They are **not** added to the MCP filesystem allow-list, so sharing `~/.cargo/registry` with a build does not let the AI call `read ~/.cargo/registry/...` through the normal MCP filesystem tools.

### Transport security is separate from local authority

A transport does not grant filesystem or shell permissions.

- **OpenAI Secure MCP Tunnel** gives ChatGPT a private transport to the local server.
- **stdio** is local child-process IPC.
- **loopback HTTP** stays on the local machine by default.
- **ngrok** makes the HTTP MCP endpoint remotely reachable and therefore increases exposure, but it still exposes only the tools permitted by the local policy.

For public HTTP/ngrok, the URL should be treated as sensitive. Ephemeral MCP paths make accidental discovery harder but are not authentication; use external access controls when appropriate.

## Setup and profiles

Run the default setup:

~~~bash
abird-link --setup
~~~

Or create a named profile:

~~~bash
abird-link --setup --profile work
abird-link -S -p work
~~~

Use it later with:

~~~bash
abird-link --profile work
abird-link -p work
~~~

Setup begins with:

~~~text
1. Choose MCP transports
   • openai — OpenAI Secure MCP Tunnel; starts automatically
   • stdio  — local stdio MCP server; start with --stdio
   • http   — local HTTP MCP server; start with --http
   • Enter comma-separated names, 'all', or 'none'.
   Enabled [openai]:

2. Choose default local permissions
   • cwd is always readable unless denied.
   Pin this profile to /current/project? [y/N]:
   Allow read+write cwd by default? [y/N]:
   Allow shell by default? [y/N]:
   Allow shell network access by default? [y/N]:   # asked only when shell=yes

3. Discover developer caches
   Scanning known package/build cache locations…

   Found:
   • Cargo registry     ~/.cargo/registry
   • Cargo git          ~/.cargo/git
   • npm                ~/.npm
   • uv                 ~/.cache/uv

   • Only cache directories are shared; adjacent credentials/config files are excluded.
   • read+write is fastest, but allows sandboxed builds to modify the shared host cache.

   Configure access to discovered caches? [y/N]:
   Cargo registry ...
      Access [n]one / [r]ead-only / read+[w]rite [n]:
~~~

Examples:

~~~text
openai
stdio
http
stdio,http
openai,stdio,http
all
none
~~~

If OpenAI is not selected, every OpenAI credential/tunnel question is skipped and no OpenAI runtime key is required.

Configuration is JSONC (JSON with comments and trailing commas):

~~~text
~/.config/abird-link/config.jsonc
~/.config/abird-link/config.work.jsonc
~/.config/abird-link/config.personal.jsonc
~~~

The default profile uses config.jsonc. --profile work uses config.work.jsonc. Existing .json profile files remain readable as a legacy fallback; setup/save writes the canonical JSONC format.

OpenAI runtime keys are kept separately per profile:

~~~text
~/.config/abird-link/runtime.key
~/.config/abird-link/runtime.work.key
~~~

A profile can persist the default cwd and the same allow/deny filesystem rules available on the CLI:

~~~jsonc
// Comments and trailing commas are allowed.
{
  "permissions": {
    // Optional. If omitted, cwd is where abird-link is launched.
    "cwd": "/home/me/src/project",

    // Paths may be absolute or relative to the effective cwd.
    "allow_read": [
      "/home/me/reference",
    ],
    "allow_write": [
      "generated",
    ],
    "allow_rw": [
      ".",                  // rw on cwd
      "/home/me/shared",
    ],

    "deny_read": [
      "private-inputs",
    ],
    "deny_write": [
      "locked-output",
    ],
    "deny_rw": [
      ".secrets",
    ],

    "allow_shell": true,
    "allow_network": false,
  },

  "caches": [
    {
      "kind": "cargo_registry",
      "path": "/home/me/.cargo/registry",
      "mode": "read_write"
    },
    {
      "kind": "npm",
      "path": "/home/me/.npm",
      "mode": "read_only"
    }
  ]
}
~~~

The CLI merges additional grants/denies on top of the profile. `--cwd` overrides the profile's persistent `cwd` for that run. Relative permission paths resolve from the effective cwd.

`allow_rw: ["."]` is the persistent equivalent of bare `--allow-rw`. `allow_rw: ["/"]` is unrestricted filesystem read+write, exactly like `--allow-rw=/`.

Older v7 profiles that stored `"allow_rw": true` are still accepted and migrate logically to `"allow_rw": ["."]`.

`allow_shell` enables the normal platform shell. `allow_network` controls network access for the normal sandboxed shell path; on Linux Bubblewrap remains enabled. A profile cannot set `allow_network=true` while `allow_shell=false`.

Cache grants are shell-only. They do **not** expand the MCP read/write filesystem policy. On Linux, approved host caches are mounted into the private sandbox home at the package manager's expected location (for example host `~/.cargo/registry` → sandbox `/tmp/home/.cargo/registry`). Read-only grants can reuse existing packages without modification; read+write grants also let builds populate/update the shared host cache.

Autodiscovery currently understands Cargo registry/git, npm, pnpm, Yarn, pip, uv, Go module/build caches, Maven, Gradle, sccache, and ccache. It uses environment variables and known existing cache locations, plus non-mutating local queries such as `npm config get cache`, `pip cache dir`, `uv cache dir`, and `go env`. Probes that may initialize a cache directory are deliberately avoided. It never grants the parent config/credential directory just because a cache lives nearby.

## Transport activation

Configured OpenAI Tunnel starts automatically:

~~~bash
abird-link
~~~

stdio and HTTP start when explicitly requested:

~~~bash
abird-link --stdio
abird-link --http
abird-link --stdio --http
~~~

These runtime flags are authoritative additions: they start the requested transport even if that transport is disabled in persisted setup. Persisted transport selection acts as the default preference; explicit CLI flags override it for the current run.

If OpenAI is also enabled in persisted config, it runs alongside requested local transports.

### stdio

--stdio is the standard subprocess MCP transport for clients such as Claude Desktop, Claude Code, and other clients that launch an MCP server command.

Typical client shape:

~~~json
{
  "mcpServers": {
    "abird": {
      "command": "abird-link",
      "args": ["--stdio"]
    }
  }
}
~~~

When stdio is active, stdout is reserved exclusively for MCP JSON-RPC. Human-readable status goes to stderr.

### HTTP

The default local Streamable HTTP endpoint is:

~~~text
http://127.0.0.1:3000/mcp
~~~

Start it with:

~~~bash
abird-link --http
~~~

Override the bind address for one run:

~~~bash
abird-link --http --http-bind=127.0.0.1:8080
~~~

HTTP-capable MCP clients connect directly to /mcp.

### Ephemeral HTTP paths

HTTP and ngrok can each use a fresh hard-to-guess MCP path for one process run.

Use the shorthand for both:

~~~bash
abird-link --http --ngrok --ephemeral-url
~~~

This gives local HTTP and ngrok independent fresh paths such as:

~~~text
http://127.0.0.1:3000/mcp/<64-hex-token>
https://example.ngrok.app/mcp/<different-64-hex-token>
~~~

Control them independently:

~~~bash
# local HTTP ephemeral, ngrok stable
abird-link --http --ngrok --http-ephemeral-url

# local HTTP stable, ngrok ephemeral
abird-link --http --ngrok --ngrok-ephemeral-url
~~~

Per-transport flags override the shorthand, including explicit false:

~~~bash
abird-link --http --ngrok --ephemeral-url --http-ephemeral-url=false
abird-link --http --ngrok --ephemeral-url --ngrok-ephemeral-url=false
~~~

Each ephemeral route is generated fresh at process start using two UUIDv4 values (~244 random bits). The ordinary /mcp path is not mounted for that transport when its ephemeral mode is enabled.

A hard-to-guess path is an additional obscurity layer, not authentication.

### ngrok public MCP endpoint

--ngrok enhances the HTTP transport; it is not another MCP transport.

Set the ngrok SDK token:

~~~bash
export NGROK_AUTHTOKEN='...'
~~~

Then:

~~~bash
abird-link --http --ngrok
~~~

abird-link starts the local Streamable HTTP MCP server, opens a public ngrok endpoint using the ngrok Rust SDK, and prints a URL such as:

~~~text
✓ ngrok MCP: https://example.ngrok.app/mcp
~~~

Any Streamable HTTP MCP client can connect directly to that public /mcp URL.

The public URL exposes whatever MCP permissions were granted to this abird-link process. Treat it as sensitive or add appropriate ngrok access controls.

## Filesystem and capability permissions

Allow rules are additive. Deny rules always take precedence over profile defaults and runtime allow flags.

The cwd is readable by default.

### Filesystem grants

~~~bash
abird-link --allow-read=/data/reference
abird-link --allow-write=/data/output
abird-link --allow-rw=/src/project
~~~

Bare forms use cwd:

~~~bash
abird-link --allow-read
abird-link --allow-write
abird-link --allow-rw
~~~

Bare --allow-write keeps its historical ergonomic behavior and means rw-cwd. With an explicit DIR, --allow-write=DIR is write-only. --allow-rw always grants both.

### Symmetric denies

~~~bash
abird-link --deny-read=/data/private
abird-link --deny-write=/src/generated
abird-link --deny-rw=/src/secret
abird-link --deny-shell
abird-link --deny-network
~~~

Bare filesystem deny forms use cwd:

~~~bash
abird-link --deny-read
abird-link --deny-write
abird-link --deny-rw
~~~

The older --deny=PATH remains a synonym for denying both read and write.

- --deny-read=DIR blocks reads while writes may still be allowed.
- --deny-write=DIR blocks writes while reads may still be allowed.
- --deny-rw=DIR blocks both.
- --deny-shell hides the shell even if the profile enabled it.
- --deny-network keeps shell networking off.

`--allow-rw=/` is simply a root read+write grant, so it provides unrestricted filesystem access without needing a separate "allow all rw" flag. It does **not** automatically enable shell, network, or disable the sandbox.

`--allow-all --no-sandbox` is the explicit full-host escape hatch. It grants root filesystem RW, shell, and network with no Bubblewrap isolation. Because deny rules cannot constrain an arbitrary unsandboxed child process, `--allow-all` cannot be combined with deny rules.

Relative MCP paths resolve from --cwd. Absolute paths work when granted. Existing paths and ancestors are canonicalized before Rust policy checks to prevent symlink escapes.

edit and patch_binary require read+write. write and write_binary require write only.

## Tool surface

Default:

~~~text
ls
read
read_binary
~~~

Any write grant adds:

~~~text
write
edit
write_binary
patch_binary
~~~

Shell adds one platform tool:

~~~text
bash         # Unix
powershell   # Windows
~~~

Inspect the visible tool surface without starting a transport:

~~~bash
abird-link --list-tools
abird-link --allow-write --list-tools
abird-link --allow-shell --list-tools
~~~

## Text and binary tools

The Pi-like text tools stay simple:

~~~text
read
write
edit
ls
bash / powershell
~~~

Binary work is separate:

~~~text
read_binary
write_binary
patch_binary
~~~

read_binary supports:

~~~text
format=mcp      typed MCP image/audio/blob content
format=base64   base64 text
format=hex      hexadecimal text
~~~

format=mcp is the default. Images become MCP image content, audio becomes MCP audio content, and other binary files become MCP blob resources.

write_binary accepts encoding=base64|hex.

patch_binary works by byte offset and can replace, insert with length=0, or delete with an empty payload plus positive length.

## Linux shell sandbox

Enable shell explicitly:

~~~bash
abird-link --allow-shell
~~~

On Linux, Bash runs inside Bubblewrap by default.

The sandbox:

- mounts readable grants read-only;
- mounts effective read+write grants read-write;
- does not expose purely write-only host paths to Bash;
- masks denied paths;
- masks the saved OpenAI tunnel runtime key when present;
- uses an empty temporary home;
- on NixOS, mounts the Nix store/profile graph read-only (`/nix/store`, `/run/current-system`, `/etc/profiles`, `/nix/var/nix/profiles`, and `~/.nix-profile` when present) so Bash, Cargo, Git, Rust and other profile-provided tools remain executable without exposing the whole home directory;
- canonicalizes and filters PATH to directories that are actually visible in the sandbox;
- isolates PID, IPC, and UTS namespaces;
- blocks shell network access by default.

The host Nix daemon socket is not mounted by default, because daemon-mediated builds/fetches could bypass the shell sandbox's direct filesystem/network restrictions.

Enable network inside the sandbox with:

~~~bash
abird-link --allow-shell --allow-network
~~~

This shell-network policy does not affect the main abird-link process. OpenAI Tunnel and ngrok use outbound networking from that main process.

## Full unsandboxed access

Unsandboxed shell execution inherently has the OS user's filesystem and network authority, so abird-link exposes one explicit full-host escape hatch rather than several partial "dangerous" flags:

~~~bash
abird-link --allow-all --no-sandbox
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
abird-link --allow-rw=/
~~~

Then add `--allow-shell` and/or `--allow-network` separately if needed. On Linux, those capabilities remain Bubblewrap-sandboxed unless `--allow-all --no-sandbox` is explicitly selected.

## OpenAI Secure MCP Tunnel

This setup is shown only when OpenAI transport is enabled.

Setup asks for:

1. a Runtime API key with Tunnels Read + Use;
2. an existing Tunnel ID, or a one-time Admin API key with Tunnels Manage;
3. a ChatGPT Workspace ID or OpenAI Organization ID when creating a tunnel.

The Admin key is never persisted.

The Runtime key is stored separately from JSONC config and follows the selected profile:

~~~text
~/.config/abird-link/runtime.key
~/.config/abird-link/runtime.work.key
~~~

When OpenAI transport is disabled for a profile, that profile does not require a runtime key.

### Automatic tunnel recovery

Transient OpenAI poll failures retry with backoff. Normal logging keeps attempts concise; the full transport error is available with `-v/--verbose`.

After **10 consecutive transient poll failures**, abird-link stops retrying the same runtime state. It cancels all active transports, gives them up to 5 seconds to shut down cleanly, force-aborts any remaining transport tasks, reloads the profile and permission policy, reconstructs the local MCP runtime, and starts the configured transports again automatically.

If a freshly restarted runtime keeps failing, restarts back off from 1 second up to a 30-second cap. Once a runtime has connected successfully, that restart backoff resets. Fatal control-plane errors such as an invalid Tunnel ID or invalid Runtime API key still fail immediately rather than entering a restart loop.

Useful locations:

- Runtime API keys: https://platform.openai.com/settings/organization/api-keys
- Admin API keys: https://platform.openai.com/settings/organization/admin-keys
- ChatGPT Workspace ID: https://chatgpt.com/admin
- OpenAI Organization ID: https://platform.openai.com/settings/organization/general

## Logging

Normal runs show concise tool activity on stderr with local timestamps:

~~~text
[01:06:47.410] TOOL read       → path=README.md limit=1
[01:06:47.411] TOOL read       ← ok  1ms
~~~

This activity stream is transport-independent: OpenAI Tunnel, stdio, and HTTP all produce the same tool lines.

Suppress normal tool activity with:

~~~bash
abird-link --silent
abird-link -s
~~~

Enable developer request logging with:

~~~bash
abird-link --verbose
abird-link -v
~~~

Verbose mode keeps normal tool activity and additionally logs incoming transport/protocol requests. For example:

~~~text
[01:06:47.426] REQ  stdio      → initialize
[01:06:47.427] REQ  stdio      → tools/call
[01:06:47.427] TOOL read       → path=README.md limit=1
[01:06:47.429] TOOL read       ← ok  1ms
~~~

HTTP verbose logs include method/path/status/latency. OpenAI Tunnel verbose logs include MCP request methods. Request bodies, file contents, binary payloads, and secrets are not dumped into developer logs.

`--silent --verbose` is valid: it hides the user-focused TOOL activity lines while retaining developer REQ logs.

Color is automatic by default: stderr is colored when attached to an interactive terminal and plain when redirected or piped. Override it with:

~~~bash
abird-link --color=always
abird-link --color=never
abird-link --color=auto
~~~

## CLI summary

~~~text
abird-link -S, --setup          interactive setup for selected profile
-p, --profile <NAME>            use config.<NAME>.jsonc

--stdio                         start stdio MCP for this run
--http                          start HTTP MCP for this run
--http-bind=<ADDR>              override HTTP listen address
--ngrok                         publish --http through ngrok
--ephemeral-url                 ephemeral local HTTP + ngrok paths
--http-ephemeral-url[=BOOL]     override local HTTP path behavior
--ngrok-ephemeral-url[=BOOL]    override ngrok path behavior

--cwd=<DIR>                     default cwd

--allow-read[=<DIR>]            add read; bare means cwd
--allow-write[=<DIR>]           bare: rw cwd; with DIR: write-only
--allow-rw[=<DIR>]              add rw; bare means cwd

--deny-read[=<DIR>]             deny read; bare means cwd
--deny-write[=<DIR>]            deny write; bare means cwd
--deny-rw[=<DIR>]               deny rw; bare means cwd
--deny=<PATH>                   legacy synonym for deny-rw
--deny-shell                    deny shell; always wins
--deny-network                  deny shell network; always wins

--allow-shell                   add platform shell
--allow-network                 network inside Linux shell sandbox
--allow-rw=/                    unrestricted filesystem read+write
--allow-all                     full host filesystem + shell + network; requires --no-sandbox
--no-sandbox                    disable shell sandbox; requires --allow-all

--list-tools                    show exposed tools
-s, --silent                    hide normal tool activity logs
-v, --verbose                   add developer request logging
--color=<auto|always|never>     control ANSI colors (default: auto)
--print-id                      print configured OpenAI Tunnel ID
~~~

## Build

The Nix build uses Crane. Dependencies are built in a separate derivation with buildDepsOnly, then reused by the application package, tests, and Clippy. This keeps source-only changes from rebuilding the dependency graph.

Native build:

~~~bash
nix build
nix run .
nix flake check
nix develop
~~~

The dependency layer is also exposed directly for cache-oriented CI jobs:

~~~bash
nix build .#deps
~~~

Inside nix develop:

~~~bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --all-features
~~~

The native Linux Nix package includes Bubblewrap and Bash.

### Cross builds

The x86_64-linux flake exposes cross-build outputs intended for release CI.

Portable Linux x86_64:

~~~bash
nix build .#cross-linux-x86_64-deps
nix build .#cross-linux-x86_64
nix build .#dist-linux-x86_64
~~~

This target is x86_64-unknown-linux-musl with static CRT linking, so the resulting binary is suitable for Debian and other x86_64 Linux systems without requiring Nix or the host's glibc version.

Windows x86_64:

~~~bash
nix build .#cross-windows-x86_64-deps
nix build .#cross-windows-x86_64
nix build .#dist-windows-x86_64
~~~

This target is x86_64-pc-windows-gnu using the MinGW/MSVCRT cross toolchain.

The dist outputs use stable release filenames:

~~~text
abird-link-linux-x86_64
abird-link-linux-x86_64.sha256
abird-link-windows-x86_64.exe
abird-link-windows-x86_64.exe.sha256
~~~

Build both release artifacts into ./dist:

~~~bash
./scripts/build-release-artifacts.sh
~~~

These outputs are deliberately separate so CI can build/cache the dependency derivations first, then build and upload the stable-named release assets.

### Install script

The repository includes install.sh for curl/sh installs and install.ps1 for native PowerShell installs.

The upstream repository is **https://github.com/abird-ai/abird-link**. Install the latest Linux x86_64 release with:

~~~bash
curl -fsSL https://raw.githubusercontent.com/abird-ai/abird-link/main/install.sh | sh
~~~

By default it installs the latest GitHub release into ~/.local/bin. Pin a release with:

~~~bash
curl -fsSL https://raw.githubusercontent.com/abird-ai/abird-link/main/install.sh | ABIRD_LINK_VERSION=0.5.0 sh
~~~

`ABIRD_LINK_REPO` remains available as an override for forks or mirrors.

A non-GitHub release/CDN can be used instead:

~~~bash
curl -fsSL https://example.com/install.sh   | ABIRD_LINK_RELEASE_BASE_URL=https://example.com/releases/v0.5.0 sh
~~~

Windows PowerShell uses the same release assets and checksum verification:

~~~powershell
irm https://raw.githubusercontent.com/abird-ai/abird-link/main/install.ps1 | iex
~~~

The installers verify the matching SHA-256 sidecar before replacing the executable.


## macOS and Windows

The filesystem MCP tools (`read`, `write`, `edit`, `ls`, binary tools) still enforce abird-link's allow/deny policy on macOS and Windows.

The shell is different: Bubblewrap is Linux-specific, so native macOS and Windows do not currently have an equivalent abird-link shell sandbox. To enable shell execution natively on those platforms, use the explicit full-host mode:

~~~bash
abird-link --allow-all --no-sandbox
~~~

That intentionally removes abird-link's shell isolation and gives the child shell the OS user's filesystem/network authority. Use it only when that is what you want.

### Windows recommendation: WSL2

For Windows development, the recommended secure shell workflow is to run abird-link **inside WSL2** and use the normal Linux Bubblewrap sandbox there:

~~~bash
# inside WSL2
abird-link --allow-rw --allow-shell
~~~

That preserves the same Linux permission/mount/network model described above instead of exposing an unrestricted native PowerShell shell.

Native Windows can still use the Rust filesystem tools with allow/deny enforcement without enabling PowerShell.

### macOS

macOS can use the Rust filesystem tools with the same allow/deny policy, plus stdio/HTTP/OpenAI transports. Because Bubblewrap is unavailable, shell execution requires `--allow-all --no-sandbox` until a macOS-native sandbox backend is added.

The current nixpkgs unstable used by the flake supports Apple Silicon macOS but has dropped x86_64-darwin, so Intel macOS should build from source with Cargo for now rather than relying on `nix build`.

## Project layout

~~~text
src/
  main.rs
  logging.rs
  mcp.rs
  setup.rs
  transports/
    mod.rs
    openai.rs
    stdio.rs
    http.rs

scripts/
  build-release-artifacts.sh

install.sh
install.ps1
~~~

mcp.rs owns the permission-scoped tool implementation and Bubblewrap mapping. logging.rs owns the shared timestamp/color activity logger. setup.rs owns JSONC profiles, onboarding, and developer-cache discovery. Each transport is isolated in its own module around the same LocalMachine policy core.

## License

MIT.
