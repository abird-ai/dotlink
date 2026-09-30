# Create the ChatGPT plugin for abird-link

This is the companion setup for the local `abird-link` binary.

## 1. Start the bridge

Read-only cwd:

```bash
cd ~/src/my-project
abird-link
```

Read/write cwd:

```bash
abird-link --allow-write
```

Linux sandboxed Bash, project rw, network blocked:

```bash
abird-link --allow-write --allow-shell
```

Add network when needed:

```bash
abird-link --allow-write --allow-shell --allow-network
```

Additional paths can be granted with repeatable allow-read/allow-write/allow-rw flags. Deny-read, deny-write, deny-rw, deny-shell, and deny-network take precedence. Legacy --deny=DIR denies both read and write.

Copy the printed `tunnel_...` ID.

Do not give Plugin Creator the Runtime API key or Admin API key.

## 2. Register the tunnel in ChatGPT

In ChatGPT:

1. Open **Settings → Security and login**.
2. Enable **Developer mode**.
3. Open **Plugins** and select **+**.
4. Create a developer connection:
   - Name: `Abird Link`
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
abird-link --list-tools
abird-link --allow-write --allow-shell --list-tools
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

Filesystem/shell/network defaults may be persisted per profile, then refined or denied at launch. Use -p/--profile to select config.<profile>.jsonc; explicit deny flags always win.

## Distribution

Secure MCP Tunnel is a private/developer connection path. Public plugin distribution may require a publicly reachable HTTPS MCP endpoint under current OpenAI distribution requirements.

References:

- https://developers.openai.com/api/docs/guides/secure-mcp-tunnels
- https://developers.openai.com/plugins/deploy/connect-chatgpt
- https://developers.openai.com/plugins/build/plugins


## Other MCP clients

The OpenAI plugin flow above uses the OpenAI transport. The same LocalMachine MCP server can also run without OpenAI.

For subprocess MCP clients such as Claude Desktop or Claude Code, launch (optionally with -p/--profile):

~~~text
abird-link --stdio
~~~

For Streamable HTTP clients, run:

~~~text
abird-link --http
~~~

Explicit --stdio and --http flags are runtime overrides: they activate those transports for the current run even if persisted setup has them disabled.

The default endpoint is:

~~~text
http://127.0.0.1:3000/mcp
~~~

For a public HTTPS endpoint through the ngrok Rust SDK:

~~~text
NGROK_AUTHTOKEN=... abird-link --http --ngrok
~~~

abird-link prints the final public /mcp URL. A Streamable HTTP MCP client can connect directly to that URL.


For hard-to-guess per-run HTTP paths:

~~~text
abird-link --http --ephemeral-url
abird-link --http --ngrok --ngrok-ephemeral-url
~~~

The local HTTP and ngrok ephemeral settings are independent.
