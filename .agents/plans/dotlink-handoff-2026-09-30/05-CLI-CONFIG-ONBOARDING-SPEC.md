# CLI, configuration and onboarding specification

## Policy precedence

Effective authority is assembled conceptually from:

1. safe implicit defaults;
2. selected JSONC profile;
3. runtime CLI additions/overrides;
4. runtime deny rules.

Deny rules win wherever the active sandbox can technically enforce them.

## Core path flags

Allows:

```text
--allow-read[=<DIR>]
--allow-write[=<DIR>]
--allow-rw[=<DIR>]
```

Bare forms target the directory where dotlink was launched.

Ergonomic behavior:

```text
bare --allow-write == read+write launch directory
```

The launch directory is also the internal relative-path base. It is read-allowed by default unless the profile sets `default_allow=false` or the run uses `--no-default-allow`.

Explicit write-only remains:

```text
--allow-write=/destination
```

Read+write:

```text
--allow-rw=/path
```

## Denies

```text
--deny-read[=<DIR>]
--deny-write[=<DIR>]
--deny-rw[=<DIR>]
--deny <PATH>    legacy deny-rw synonym
```

Denied paths are separate read/write capabilities:

- deny-read may still permit writes;
- deny-write may still permit reads;
- deny-rw blocks both.

## Shell/network

```text
--allow-shell
--deny-shell
--allow-network
--deny-network
```

Linux shell normally remains Bubblewrap-sandboxed.

Profile `allow_network` applies to the normal sandboxed shell path.

`--allow-network` without shell authority should error rather than silently do nothing.

`--deny-network` overrides profile/runtime network grants.

## Full host mode

Current intentional full-host path:

```text
--allow-all --no-sandbox
```

Do not proliferate old `*-dangerous` variants unless a new design explicitly replaces this decision.

## Profiles

Default config:

```text
~/.config/abird/dotlink/config.jsonc
```

Named:

```text
~/.config/abird/dotlink/config.<profile>.jsonc
```

Use:

```bash
dotlink -p work
dotlink --profile work
```

Setup:

```bash
dotlink -S -p work
dotlink --setup --profile work
```

`-q` is reserved for quiet logging.

## JSONC requirements

Parser supports:

- `//` comments;
- `/* ... */` comments;
- trailing commas;
- UTF-8 BOM.

Must not strip comment markers inside strings such as URLs.

Canonical save header currently indicates JSONC/comment support.

## Schema

Current source schema version:

```text
9
```

Conceptual current config:

```jsonc
{
  "version": 9,

  "transports": {
    "openai": true,
    "stdio": false,
    "http": false,
    "http_bind": "127.0.0.1:3000",
    "http_ephemeral_url": false,
    "ngrok": false,
    "ngrok_ephemeral_url": false,
  },

  "permissions": {
    "default_allow": true,

    "allow_read": [],
    "allow_write": [],
    "allow_rw": ["."],

    "deny_read": [],
    "deny_write": [],
    "deny_rw": [],

    "allow_shell": true,
    "allow_network": false,
  },

  "caches": [
    {
      "kind": "cargo_registry",
      "path": "/home/me/.cargo/registry",
      "mode": "read_write",
    }
  ],

  "tunnel_id": null,
  "organization_id": null,
  "base_url": "https://api.openai.com",

  "max_shell_timeout_secs": 120,
  "max_output_bytes": 1048576,
  "max_read_bytes": 4194304,
  "max_write_bytes": 4194304,
}
```

Runtime API key is never serialized into config.

## Schema policy

Schema v9 is the only supported profile schema. Older configs must be recreated with `dotlink --setup`.

## Onboarding flow

### Step 1 — transports

The compact numbered selector accepts numbers or names:

```text
1  openai
2  stdio
3  http
all
0 / none / cancel   cancel setup without saving
```

OpenAI is presented as the recommended ChatGPT path; stdio is described for local MCP clients; HTTP is described for Claude.ai and other web MCP clients.

When HTTP is selected, setup conditionally asks:

```text
Use an ephemeral local MCP URL?
Publish a public ngrok HTTPS endpoint?
Use an ephemeral ngrok MCP URL?     only when ngrok=yes
```

If OpenAI is not selected, skip all OpenAI credential/tunnel questions.

### Step 2 — local defaults

Current questions:

```text
Allow read access to: <launch directory>?
Allow write access too?             only when read=yes
Allow shell access?
Allow shell network access?         only when shell=yes
```

Read defaults to yes; write, shell, and network default to no for new profiles. Re-running setup uses existing values as defaults, preserves path rules not directly edited by the wizard, and masks stored Runtime API keys as `[existing key]`; blank keeps the saved key.

### Step 3 — developer caches

Linux sandbox path only.

Discover existing supported caches.

Show exact path.

Explain that adjacent credentials/config are excluded.

Ask whether to configure sharing.

For each detected cache:

```text
none
read-only
read+write
```

Do not let discovery create a cache directory merely to report it.

### OpenAI tunnel step

If OpenAI selected:

- Runtime API key with Tunnels Read + Use;
- existing Tunnel ID or one-time Admin key;
- Workspace ID or Organization ID when creating tunnel;
- Admin key used once and never persisted;
- Runtime key stored separately per profile.

## Profile manager

Persistent profile administration is available without hand-editing JSONC:

```text
dotlink profile list
dotlink profile show <name>
dotlink profile create <name>
dotlink profile edit <name>
dotlink profile delete <name>
dotlink profile allow <name> read|write|rw <path>
dotlink profile remove-allow <name> read|write|rw <path>
dotlink profile deny <name> read|write|rw <path>
dotlink profile remove-deny <name> read|write|rw <path>
dotlink profile enable <name> <setting>
dotlink profile disable <name> <setting>
```

`default` is the manager alias for the unnamed default profile. Boolean settings are `openai`, `stdio`, `http`, `http-ephemeral-url`, `ngrok`, `ngrok-ephemeral-url`, `default-allow`, `shell`, and `network`. Enabling a dependent setting automatically enables its prerequisites; disabling a parent cascades dependent booleans off. Disabling OpenAI preserves tunnel/key data; deleting the profile removes the saved key.

## Cache kinds

Current typed kinds:

```text
cargo_registry
cargo_git
npm
pnpm
yarn
pip
uv
go_mod
go_build
maven
gradle
sccache
ccache
```

Modes:

```text
read_only
read_write
```

## Environment prefix

Canonical:

```text
DOTLINK_*
```

Examples:

```text
DOTLINK_CONFIG
DOTLINK_ID
DOTLINK_API_KEY
DOTLINK_ORGANIZATION_ID
DOTLINK_BASE_URL
```

`DOTLINK_CONFIG` overrides only the JSONC path. Runtime keys always remain under dotlink's owned Abird XDG directory (`~/.config/abird/dotlink` or `$XDG_CONFIG_HOME/abird/dotlink`) so secrets never follow a project-local custom config.

## Transport runtime overrides

Configured profile transports start automatically. Local runtime overrides are symmetric:

```text
--stdio      add stdio for this run
--no-stdio   suppress profile stdio
--http       add HTTP for this run
--no-http    suppress profile HTTP and ngrok
--ngrok      add ngrok to effective HTTP
--no-ngrok   suppress profile ngrok
```

HTTP bind and ephemeral-path flags override persisted HTTP/ngrok behavior without requiring `--http` again when HTTP is already active from the profile.
