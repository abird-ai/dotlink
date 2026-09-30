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

Linux x86_64:

~~~bash
curl -fsSL https://raw.githubusercontent.com/abird-ai/dotlink/main/install.sh | sh
~~~

Windows PowerShell:

~~~powershell
irm https://raw.githubusercontent.com/abird-ai/dotlink/main/install.ps1 | iex
~~~

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

abird dotlink 0.5.0
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

Claude.ai custom connectors use **remote MCP**: Claude connects from Anthropic's cloud, not from your local machine. That means `http://127.0.0.1:3000/mcp` will not work directly with claude.ai; expose dotlink through a public HTTPS endpoint such as ngrok.

### 1. Start a public Streamable HTTP MCP endpoint

Set your ngrok token, then run:

~~~bash
export NGROK_AUTHTOKEN='...'
dotlink --http --ngrok --ngrok-ephemeral-url
~~~

Add whatever local permissions you actually want Claude to have, for example:

~~~bash
dotlink --http --ngrok --ngrok-ephemeral-url \
  --allow-rw --allow-shell
~~~

dotlink prints a URL similar to:

~~~text
✓ ngrok MCP: https://example.ngrok.app/mcp/<ephemeral-token>
~~~

### 2. Add it to Claude.ai

For individual Claude plans:

1. Open **Customize → Connectors**.
2. Select **+**.
3. Choose **Add custom connector**.
4. Give it a name such as **abird dotlink**.
5. Paste the ngrok MCP URL printed by dotlink.
6. Add the connector.

For Team/Enterprise organizations, an owner may need to register the custom connector under the organization's connector settings first; members can then connect and enable it.

### 3. Enable it in a conversation

In Claude, use the **+** menu in the chat composer, open **Connectors**, and enable **abird dotlink** for that conversation. Claude can then call the tools exposed by the running dotlink process.

> **Security:** dotlink's HTTP/ngrok transport does not currently add application-layer authentication. Treat the public URL as sensitive. An ephemeral path makes accidental discovery much harder, but it is not authentication. Use ngrok access controls where appropriate and grant only the minimum local permissions needed.

## Connect other remote MCP clients through ngrok

Any MCP client that supports remote **Streamable HTTP** can use the same public endpoint:

~~~bash
dotlink --http --ngrok --ngrok-ephemeral-url
~~~

Then give the client the printed HTTPS MCP URL. The remote client receives exactly the tool surface and permissions exposed by that dotlink process.

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

This stays loopback-only unless you explicitly change `--http-bind`.

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

--stdio                         add stdio MCP for this run
--no-stdio                      suppress profile stdio for this run
--http                          add HTTP MCP for this run
--no-http                       suppress profile HTTP + ngrok for this run
--http-bind=<ADDR>              override HTTP listen address
--ngrok                         enable ngrok for effective HTTP
--no-ngrok                      suppress profile ngrok for this run
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

Profiles live under `~/.config/abird/dotlink/` (or the equivalent XDG config directory). Runtime API keys are stored separately from JSONC profiles and are never shown back in plaintext during setup.

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

Build stable Linux + Windows release artifacts:

~~~bash
./scripts/build-release-artifacts.sh
~~~

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

The current nixpkgs unstable used by the flake supports Apple Silicon macOS but has dropped x86_64-darwin, so Intel macOS should build from source with Cargo for now rather than relying on `nix build`.

## License

MIT.
