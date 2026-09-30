# Connect ChatGPT to abird-link

## 1. Start abird-link

```bash
cd ~/src/my-project
abird-link
```

Add only the local authority you want:

```bash
abird-link --allow-rw
abird-link --allow-rw --allow-shell
abird-link --allow-rw --allow-shell --allow-network
```

Copy the printed `tunnel_...` ID. Never paste Runtime or Admin API keys into ChatGPT connection fields.

## 2. Register the tunnel in ChatGPT

1. Open **Settings → Security and login**.
2. Enable **Developer mode**.
3. Open **Plugins** and select **+**.
4. Create a developer connection:
   - Name: `Abird Link`
   - Connection: **Tunnel**
   - Tunnel ID: the printed `tunnel_...` value.
5. Create the connection and review discovered tools.

The tool list is permission-dependent:

```text
default       ls, read, read_binary
write grant   + edit, patch_binary, write, write_binary
shell         + bash (Unix) / powershell (Windows)
```

Inspect the exact local surface at any time:

```bash
abird-link --list-tools
abird-link --allow-rw --allow-shell --list-tools
```

## 3. Use it

In a ChatGPT conversation, open the tools menu and enable **Abird Link**. Ask normally; ChatGPT chooses the MCP tools.

Examples:

```text
Inspect this project and explain its structure.
Run the tests and fix the failures.
Read the local data and build a slide deck from it.
```

After changing tool names, schemas, or permissions, restart abird-link and refresh the developer connection before retesting.

## Troubleshooting

If ChatGPT cannot find the tunnel, verify:

- abird-link is running and shows `✓ Connected — ready`;
- the correct `tunnel_...` ID is registered;
- Developer mode is enabled;
- the tunnel belongs to the current ChatGPT workspace;
- the Runtime API key has **Tunnels Read + Use**.
