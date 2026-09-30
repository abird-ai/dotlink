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

Bare forms target the directory where abird-link was launched.

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
~/.config/abird-link/config.jsonc
```

Named:

```text
~/.config/abird-link/config.<profile>.jsonc
```

Use:

```bash
abird-link -p work
abird-link --profile work
```

Setup:

```bash
abird-link -S -p work
abird-link --setup --profile work
```

`-s` is reserved for silent logging.

## JSONC requirements

Parser supports:

- `//` comments;
- `/* ... */` comments;
- trailing commas;
- UTF-8 BOM.

Must not strip comment markers inside strings such as URLs.

Canonical save header currently indicates JSONC/comment support.

Legacy `.json` profile files remain readable.

If `ABIRD_LINK_CONFIG` explicitly points to `.json`, preserve valid plain JSON behavior for that path.

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

Schema v9 is the only supported profile schema. Older configs must be recreated with `abird-link --setup`.

## Onboarding flow

### Step 1 — transports

The compact numbered selector accepts numbers or names:

```text
1  openai
2  stdio
3  http
all
none
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

Read defaults to yes; write, shell, and network default to no.

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
ABIRD_LINK_*
```

Examples:

```text
ABIRD_LINK_CONFIG
ABIRD_LINK_ID
ABIRD_LINK_API_KEY
ABIRD_LINK_ORGANIZATION_ID
ABIRD_LINK_BASE_URL
```

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
