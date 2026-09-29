# Create the ChatGPT plugin for abird-tunnel

This is the companion setup for the local `abird-tunnel` binary.

## 1. Start the local workspace bridge

Run it from the project you want ChatGPT to see:

```bash
cd ~/src/my-project
abird-tunnel
```

or explicitly choose a directory:

```bash
abird-tunnel --cwd=~/src/my-project
```

Copy the printed `tunnel_...` ID.

Do not give ChatGPT Plugin Creator the runtime API key or Admin API key.

## 2. Register the tunnel connection in ChatGPT

In ChatGPT:

1. Open **Settings → Security and login**.
2. Enable **Developer mode**.
3. Open **Plugins** and select **+**.
4. Create a new developer-mode connection with:
   - Name: `Abird Tunnel`
   - Description: `Secure access to the local workspace exposed by abird-tunnel, with bounded filesystem tools and an explicit Bash shell.`
   - Connection: **Tunnel**
   - Tunnel ID: the `tunnel_...` value printed by the binary.
5. Create the connection.
6. Review that these tools are discovered:
   - `machine_info`
   - `fs_list`
   - `fs_stat`
   - `fs_read_text`
   - `fs_write_text`
   - `fs_mkdir`
   - `fs_remove`
   - `shell_exec`

The connection is the bridge between the installed plugin and whichever local `abird-tunnel` process is currently running for that tunnel.

## 3. Copy the registered connection ID

After ChatGPT creates the connection, copy its technical ID from the browser URL. It starts with:

```text
plugin_asdk_app...
```

This is the only generated identifier Plugin Creator needs.

It is **not** the same thing as the `tunnel_...` ID.

## 4. Create the plugin with Plugin Creator

Open Plugin Creator and paste the prompt in:

```text
prompts/PLUGIN_CREATOR.md
```

Replace:

```text
<PLUGIN_ASDK_APP_ID>
```

with the `plugin_asdk_app...` ID from step 3.

The prompt asks Plugin Creator to wire the plugin through the registered MCP connection in `.app.json`. It intentionally does not ask for a public MCP URL or a portable `mcp.json`, because this development/private connection is tunnel-backed and already registered in ChatGPT.

## Information you need to provide

Required for the private/local version:

- the `tunnel_...` ID printed by `abird-tunnel` when registering the connection;
- the resulting `plugin_asdk_app...` technical connection ID when running Plugin Creator.

Optional metadata you may want to customize later:

- developer/publisher display name;
- plugin icon/logo;
- website URL;
- privacy-policy URL;
- terms-of-service URL;
- different default prompts or skill behavior.

You do **not** need to provide Plugin Creator with:

- the restricted runtime API key;
- the one-time Admin API key;
- your local filesystem path;
- an externally reachable MCP URL.

The workspace path comes dynamically from each invocation of `abird-tunnel`.

## Private testing vs public distribution

Secure MCP Tunnel is intended for private/developer access to MCP servers that are not publicly reachable. Current OpenAI plugin documentation distinguishes that from public plugin submission: a publicly distributed plugin with MCP still requires a public HTTPS MCP endpoint. Therefore this tunnel-backed package should be treated as a private/development plugin unless OpenAI's distribution requirements change.

References:

- https://developers.openai.com/api/docs/guides/secure-mcp-tunnels
- https://developers.openai.com/plugins/deploy/connect-chatgpt
- https://developers.openai.com/plugins/build/plugins
