# Connect ChatGPT to dotlink

## 1. Start dotlink

```bash
cd ~/src/my-project
dotlink
```

Add only the local authority you want:

```bash
dotlink --allow-rw
dotlink --allow-rw --allow-shell
dotlink --allow-rw --allow-shell --allow-network
```

Copy the printed `tunnel_...` ID. Never paste Runtime or Admin API keys into ChatGPT connection fields.

## 2. Register the tunnel in ChatGPT

1. Open **https://chatgpt.com/plugins**.
2. Select **Add → Create MCP App**.
3. Name it **abird dotlink** and optionally add a short description.
4. Under **Connection**, choose **Tunnel**.
5. Paste the printed `tunnel_...` ID.
6. Accept the custom-MCP warning and create the MCP app.

If **Create MCP App** is unavailable, enable **Developer mode** under **Settings → Security and login** first.

The tool list is permission-dependent:

```text
default       ls, read, read_binary
write grant   + edit, patch_binary, write, write_binary
shell         + bash (Unix) / powershell (Windows)
```

Inspect the exact local surface at any time:

```bash
dotlink --list-tools
dotlink --allow-rw --allow-shell --list-tools
```

## 3. Use it

In a ChatGPT conversation, open the tools menu and enable **abird dotlink**. Ask normally; ChatGPT chooses the MCP tools.

Examples:

```text
Inspect this project and explain its structure.
Run the tests and fix the failures.
Read the local data and build a slide deck from it.
```

After changing tool names, schemas, or permissions, restart dotlink (or press `Ctrl+R` in an interactive run), then open **https://chatgpt.com/settings/plugins-settings**, select **abird dotlink**, and click **Refresh tools** before retesting.

## Troubleshooting

If ChatGPT cannot find the tunnel, verify:

- dotlink is running and shows `✓ Connected — ready`;
- the correct `tunnel_...` ID is registered;
- Developer mode is enabled;
- the tunnel belongs to the current ChatGPT workspace;
- the Runtime API key has **Tunnels Read + Use**.
