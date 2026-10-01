# abird dotlink

Connect **your ChatGPT dot**, **your ChatGPT Web and Spaces** to files and tools on **your computer** — securely with a single command.

A single lightweight binary with an MCP server, OpenAI tunnel transport, permission engine, profiles, activity logging, and a full Bubblewrap sandbox on Linux — no UI, no third-party relay, guided setup.

- **Let your ChatGPT Web and your dot work directly on your computer through controlled, fine-grained access**: read and edit files, run Git, build, test, execute scripts, and use only the tools you explicitly expose.
- **Use local data without the upload/download loop**: analyze files, build reports or slide decks, and work with artifacts directly from your machine.
- **Keep project context connected**: reason and document in ChatGPT/Spaces, then continue against the same live repository and toolchain, then talk to dot about it.
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

Use `default` as the profile name to manage the unnamed default profile. Persisted booleans can be toggled with `profile enable/disable`: `openai`, `stdio`, `http`, `http-ephemeral-url`, `ngrok`, `ngrok-ephemeral-url`, `default-allow`, `shell`, and `network`. Enabling a dependent setting automatically enables its prerequisite (`network` → `shell`, `ngrok` → `http`); disabling a parent safely disables its dependents.

### What it looks like

Default runs are quiet. Add `-v` to see tool activity:

~~~text
$ dotlink -v -p aw

abird dotlink 0.6.0
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

[10:59:19.184] TOOL read       → path=.agents/plans/MASTER-HANDOFF.md
[10:59:19.194] TOOL read       ← ok  9ms
~~~

`-v` starts with TOOL activity; `-vv` starts with TOOL + REQ diagnostics. During an interactive run, press `v` to cycle `quiet → TOOL → TOOL + REQ → quiet` without restarting. Each change emits a compact `LOG verbosity …` line. `Ctrl+R` fully reloads the profile/policy/transports; `Ctrl+C` exits. Runtime key controls are disabled when stdio MCP is active so protocol stdin is never intercepted. File contents, binary payloads, API keys, and raw request bodies are not intentionally logged.

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

1. Open **https://chatgpt.com/plugins**.
2. Select **Add → Create MCP App**.
3. Name it **abird dotlink** and optionally add a short description.
4. Under **Connection**, choose **Tunnel**.
5. Paste the `tunnel_...` ID printed by dotlink.
6. Accept the custom-MCP warning and create the MCP app.
7. In a ChatGPT conversation, open the tools menu, enable **abird dotlink**, and ask normally.

If **Create MCP App** is unavailable, enable **Developer mode** under **Settings → Security and login** first.

The tools ChatGPT discovers follow dotlink's local permissions:

~~~text
read-only     ls, read, read_binary
write        + write, edit, write_binary, patch_binary
shell        + bash (Unix) / powershell (Windows)
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

Setup asks for one **owner password** the first time OAuth is needed. The password itself is never stored; dotlink persists only an Argon2id hash in its private state directory.

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

Never derive the OAuth issuer from proxy/Host headers; `--public-url` is the explicit trust boundary.

Public ngrok ingress is OAuth-protected by default. Direct non-loopback HTTP binds are also refused unless OAuth is enabled with an explicit HTTPS public origin. The only way to intentionally expose either form of public ingress without application-layer authentication is the explicit one-run escape hatch:

~~~bash
dotlink --http --ngrok --allow-public-no-auth
# or, for an intentionally unauthenticated LAN listener:
dotlink --http --http-bind=0.0.0.0:3000 --allow-public-no-auth
~~~

That mode exposes every MCP capability granted to the process to anyone who can reach the endpoint and is not recommended.

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
public HTTP ingress      OFF
unsandboxed host mode    OFF
~~~

Capabilities are independent: `--allow-read`, `--allow-write`, `--allow-rw`, `--allow-shell`, and `--allow-network`. Denies always win. `--allow-rw=/` grants filesystem RW only; full unsandboxed host authority requires `--allow-all --no-sandbox`.

### Filesystem policy

~~~text
read / read_binary / ls       require read
write / write_binary          require write
edit / patch_binary           require read + write
~~~

Targets are canonicalized before checks; create targets canonicalize their nearest existing ancestor, preventing symlink escapes. The advertised MCP tool list is permission-aware, so unavailable write/shell tools are omitted rather than merely rejected later.

### Linux sandbox

With `--allow-shell`, Bash runs inside Bubblewrap by default. Read grants mount RO, read+write grants mount RW, denied paths are masked or downgraded, networking is isolated unless `--allow-network` is granted, credentials stay hidden, and only approved developer caches/Nix runtime paths are exposed. See `SECURITY.md` for the full sandbox model.

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
   - **No Admin key in dotlink:** create one at https://platform.openai.com/settings/organization/tunnels, then paste its existing `tunnel_...` ID;
   - **Create from dotlink:** type `new` and provide a one-time Admin API key with Tunnels Manage;
3. a ChatGPT Workspace ID or OpenAI Organization ID only when dotlink creates the tunnel.

The one-time Admin key is never persisted.

The Runtime key is stored separately from JSONC config and follows the selected profile:

~~~text
~/.config/abird/dotlink/runtime.key
~/.config/abird/dotlink/runtime.work.key
~~~

`DOTLINK_CONFIG` may move the JSONC file, but Runtime keys always stay in dotlink's private XDG directory above. The active config and credential directory are control-plane state and are not exposed through dotlink's MCP/filesystem/shell surface.

When OpenAI transport is disabled for a profile, that profile does not require a runtime key.

### Automatic tunnel recovery

Transient OpenAI poll failures retry with backoff. Normal warnings stay concise; full transport diagnostics are available with `-vv`.

After **10 consecutive transient poll failures**, dotlink stops retrying the same runtime state. It cancels all active transports, gives them up to 5 seconds to shut down cleanly, force-aborts any remaining transport tasks, reloads the profile and permission policy, reconstructs the local MCP runtime, and starts the configured transports again automatically.

If a freshly restarted runtime keeps failing, restarts back off from 1 second up to a 30-second cap. Once a runtime has connected successfully, that restart backoff resets. Fatal control-plane errors such as an invalid Tunnel ID or invalid Runtime API key still fail immediately rather than entering a restart loop.

Useful locations:

- Tunnels: https://platform.openai.com/settings/organization/tunnels
- Runtime API keys: https://platform.openai.com/settings/organization/api-keys
- Admin API keys: https://platform.openai.com/settings/organization/admin-keys
- ChatGPT Workspace ID: https://chatgpt.com/admin
- OpenAI Organization ID: https://platform.openai.com/settings/organization/general

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
[01:06:47.410] TOOL read       → path=README.md limit=1
[01:06:47.411] TOOL read       ← ok  1ms
~~~

With `-vv`, transport diagnostics are added:

~~~text
[01:06:47.426] REQ  stdio      → initialize
[01:06:47.427] REQ  stdio      → tools/call
~~~

Logs include safe metadata such as paths, methods, status, and latency; file contents, binary payloads, runtime keys, and raw request bodies are not intentionally logged.

Color is automatic on interactive stderr. Override with `--color=always`, `--color=never`, or `--color=auto`. Interactive terminal runs also support `v` for live verbosity cycling and `Ctrl+R` for a full runtime restart; stdio keeps stdin protocol-clean and exposes only `Ctrl+C`. In a manual TTY stdio run, dotlink ensures `Ctrl+C` generates an interrupt and restores the terminal state afterward.

## CLI summary

~~~text
dotlink -S, --setup             interactive setup/editor for selected profile
-p, --profile <NAME>            use config.<NAME>.jsonc
dotlink profile <COMMAND>       list/create/edit/delete/mutate persisted profiles
dotlink oauth <COMMAND>         inspect/revoke OAuth clients and refresh grants

--stdio                         add stdio MCP for this run
--no-stdio                      suppress profile stdio for this run
--http                          add HTTP MCP for this run
--no-http                       suppress profile HTTP + ngrok for this run
--http-bind=<ADDR>              override HTTP listen address
--ngrok                         enable ngrok for effective HTTP
--no-ngrok                      suppress profile ngrok for this run
--ngrok-domain=<DOMAIN>         use a stable/reserved ngrok hostname
--no-ngrok-domain               ignore persisted ngrok domain for this run
--oauth                         protect local HTTP with OAuth
--no-oauth                      disable local HTTP OAuth for this run
--public-url=<HTTPS-ORIGIN>     canonical OAuth origin behind a reverse proxy
--allow-public-no-auth          intentionally allow externally reachable HTTP/ngrok without OAuth
--ephemeral-url                 ephemeral local HTTP + ngrok paths
--http-ephemeral-url[=BOOL]     override local HTTP path behavior
--ngrok-ephemeral-url[=BOOL]    override ngrok path behavior

--no-default-allow              do not implicitly read the launch directory
--allow-read[=<DIR>]            add read; bare means launch directory
--allow-write[=<DIR>]           bare: rw launch directory; with DIR: write-only
--allow-rw[=<DIR>]              add rw; bare means launch directory

--deny-read[=<DIR>]             deny read; bare means launch directory
--deny-write[=<DIR>]            deny write; bare means launch directory
--deny-rw[=<DIR>]               deny rw; bare means launch directory
--deny=<PATH>                   legacy synonym for deny-rw
--deny-shell                    deny shell; always wins
--deny-network                  deny shell network; always wins

--allow-shell                   add platform shell
--allow-network                 network inside Linux shell sandbox
--allow-rw=/                    unrestricted filesystem read+write
--allow-all                     full host filesystem + shell + network; requires --no-sandbox
--no-sandbox                    disable shell sandbox; requires --allow-all

--list-tools                    show exposed tools
-q, --quiet                    suppress TOOL activity
-v, --verbose                   repeatable: -v TOOL, -vv TOOL + REQ
--color=<auto|always|never>     control ANSI colors (default: auto)
--print-id                      print configured OpenAI Tunnel ID
~~~

## FAQ

### I changed dotlink permissions. How do I update ChatGPT?

Restart dotlink (or press `Ctrl+R` in an interactive run) so the new profile/policy is active, then open **https://chatgpt.com/settings/plugins-settings**, select **abird dotlink**, and click **Refresh tools**.

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
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --all-features
~~~

Build the complete reproducible release bundle from an x86_64 Linux host:

~~~bash
nix build .#release-all
# or copy the bundle into ./dist:
./scripts/build-release-artifacts.sh
~~~

The bundle contains static Linux x86_64/ARM64 binaries, Windows x86_64/ARM64, and macOS ARM64. The static Linux builds run across NixOS, Debian, and other compatible distributions. Individual Nix outputs are available as `dist-linux-*`, `dist-windows-*`, and `dist-macos-aarch64`.

To publish a GitHub Release, push a matching version tag such as `v0.6.0`. CI rebuilds the Nix release graph and uploads each binary plus its `.sha256` sidecar as an individual release asset; the Actions ZIP bundle is not published as the release.

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

## License

MIT.
