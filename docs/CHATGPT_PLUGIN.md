# Create the ChatGPT plugin for abird-tunnel

This is the companion setup for the local `abird-tunnel` binary.

## 1. Start the bridge

Read-only cwd:

```bash
cd ~/src/my-project
abird-tunnel
```

Read/write cwd:

```bash
abird-tunnel --allow-write
```

Linux sandboxed Bash, project rw, network blocked:

```bash
abird-tunnel --allow-write --allow-shell
```

Add network when needed:

```bash
abird-tunnel --allow-write --allow-shell --allow-network
```

Additional paths can be granted with repeatable `--allow-read=DIR`, `--allow-write=DIR`, and `--allow-rw=DIR` flags. `--deny=DIR` always takes precedence.

Copy the printed `tunnel_...` ID.

Do not give Plugin Creator the Runtime API key or Admin API key.

## 2. Register the tunnel in ChatGPT

In ChatGPT:

1. Open **Settings → Security and login**.
2. Enable **Developer mode**.
3. Open **Plugins** and select **+**.
4. Create a developer connection:
   - Name: `Abird Tunnel`
   - Connection: **Tunnel**
   - Tunnel ID: the printed `tunnel_...` value.
5. Create the connection.
6. Review the discovered tools.

The tool list depends on how the local process was launched.

Default:

```text
ls
read
read_binary
```

With write permission:

```text
edit
ls
patch_binary
read
read_binary
write
write_binary
```

With shell enabled, `bash` on Unix or `powershell` on Windows is added.

You can always inspect the exact local surface first:

```bash
abird-tunnel --list-tools
abird-tunnel --allow-write --allow-shell --list-tools
```

## 3. Copy the registered connection ID

After ChatGPT creates the connection, copy the technical ID from the browser URL:

```text
plugin_asdk_app...
```

This is not the `tunnel_...` ID.

## 4. Create the plugin

Use:

```text
prompts/PLUGIN_CREATOR.md
```

Replace:

```text
<PLUGIN_ASDK_APP_ID>
```

with the `plugin_asdk_app...` ID.

The prompt wires the private plugin to the already registered tunnel connection and teaches the agent that the local tool surface is permission-dependent.

## Information you need

For local/private use:

- printed `tunnel_...` ID;
- resulting `plugin_asdk_app...` connection ID.

Do not provide Plugin Creator with:

- Runtime API key;
- one-time Admin API key;
- local credentials;
- public MCP URL.

Filesystem permissions and sandbox/network policy are selected locally each time `abird-tunnel` starts.

## Distribution

Secure MCP Tunnel is a private/developer connection path. Public plugin distribution may require a publicly reachable HTTPS MCP endpoint under current OpenAI distribution requirements.

References:

- https://developers.openai.com/api/docs/guides/secure-mcp-tunnels
- https://developers.openai.com/plugins/deploy/connect-chatgpt
- https://developers.openai.com/plugins/build/plugins
