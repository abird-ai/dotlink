# CLI, config, profiles and setup specification

## CLI philosophy

The CLI is intentionally small and composable:

- profile persists defaults;
- CLI flags are one-run additions/overrides/suppressions;
- setup edits persistent profile state;
- subcommands manage persistent state explicitly;
- dangerous full-host behavior requires paired flags.

Do not add overlapping synonyms unless they materially improve ergonomics.

## Global execution

```bash
dotlink
dotlink -p work
dotlink -S
dotlink -S -p work
```

`-S` is setup. `-q` is quiet.

## Transport flags

```text
--stdio
--no-stdio

--http
--no-http
--http-bind <ADDR>

--ngrok
--no-ngrok
--ngrok-domain <DOMAIN>
--no-ngrok-domain

--oauth
--no-oauth
--public-url <URL>
--allow-public-no-auth

--ephemeral-url
--http-ephemeral-url[=BOOL]
--ngrok-ephemeral-url[=BOOL]
```

Precedence principles:

- persisted enabled transports start automatically;
- positive CLI flags add/enable for this run;
- `--no-*` suppress persisted defaults for this run;
- per-transport ephemeral setting overrides global shorthand;
- ngrok requires effective HTTP;
- public no-auth is allowed only when the endpoint is actually externally reachable.

## Filesystem flags

```text
--allow-read[=<DIR>]
--allow-write[=<DIR>]
--allow-rw[=<DIR>]

--deny-read[=<DIR>]
--deny-write[=<DIR>]
--deny-rw[=<DIR>]
--deny <PATH>

--no-default-allow
```

Bare forms use the launch directory.

Ergonomic special case:

```text
bare --allow-write
→ read+write launch directory
```

Explicit `--allow-write=/path` is write-only.

`--deny` is the legacy deny-rw synonym.

## Shell/network flags

```text
--allow-shell
--deny-shell
--allow-network
--deny-network
```

Full host:

```text
--allow-all --no-sandbox
```

Each requires the other.

Do not add separate `dangerous-*` families.

## Logging/runtime flags

```text
-q / --quiet
-v
-vv
--color auto|always|never
--list-tools
--print-id
```

## Profile schema v10

Representative JSONC:

```jsonc
{
  "version": 10,

  "transports": {
    "openai": false,
    "stdio": false,
    "http": true,
    "http_bind": "127.0.0.1:3000",
    "http_ephemeral_url": false,
    "ngrok": true,
    "ngrok_domain": "my-dotlink.ngrok.app",
    "ngrok_ephemeral_url": false,
  },

  "oauth": {
    // local/reverse-proxied HTTP only;
    // ngrok OAuth is automatic separately.
    "enabled": false,
    "public_url": null,
  },

  "permissions": {
    "default_allow": true,
    "allow_read": [],
    "allow_write": [],
    "allow_rw": [],
    "deny_read": [],
    "deny_write": [],
    "deny_rw": [],
    "allow_shell": false,
    "allow_network": false,
  },

  "caches": [],

  "tunnel_id": null,
  "organization_id": null,
  "base_url": "https://api.openai.com",

  "max_shell_timeout_secs": 120,
  "max_output_bytes": 1048576,
  "max_read_bytes": 4194304,
  "max_write_bytes": 4194304,
}
```

Runtime API key is not in this JSON.

## Config/state paths

Default config:

```text
$XDG_CONFIG_HOME/abird/dotlink/config.jsonc
$XDG_CONFIG_HOME/abird/dotlink/runtime.key
```

Named:

```text
config.<profile>.jsonc
runtime.<profile>.key
```

OAuth state:

```text
$XDG_STATE_HOME/abird/dotlink/oauth.json
$XDG_STATE_HOME/abird/dotlink/oauth.<profile>.json
```

Fallback on Unix:

```text
~/.config/abird/dotlink
~/.local/state/abird/dotlink
```

`DOTLINK_CONFIG` moves only JSONC. Runtime keys and OAuth state stay inside owned Abird namespaces.

## Profile names

Valid named profile:

- 1–64 chars;
- ASCII alphanumeric;
- `-`;
- `_`.

`default` addresses the unnamed profile in management commands.

## Strict schema behavior

Schema v10 is a deliberate hard boundary.

Normal runtime:

```text
v10 → load
anything else → reject
```

Forced setup against an older schema:

- detect older version;
- explain v10 is strict;
- ask whether to replace from scratch;
- do not migrate semantics;
- cancel leaves old file unchanged.

Reason: new security semantics must never be silently ignored by older/newer incompatible binaries.

## JSONC semantics

Supported:

- UTF-8;
- optional BOM;
- line comments;
- block comments;
- trailing commas;
- comment-like strings preserved.

Do not replace this with a permissive parser that changes string semantics.

## Runtime key storage

OpenAI Runtime API key:

- private file;
- never JSONC;
- never printed back;
- setup displays `[existing key]`;
- blank input on re-setup preserves current key;
- deleting profile removes key;
- disabling OpenAI does not destroy it.

Optional Admin key for tunnel creation:

- setup-only;
- not persisted.

## OAuth owner credential

Owner password:

- entered via hidden terminal prompt;
- minimum 12 chars;
- confirmation required;
- plaintext never persisted;
- Argon2id hash in OAuth state;
- setup/profile show says `[existing credential]`;
- blank/default re-setup preserves it;
- password changes are transactional and preserve concurrent approved clients/grants.

## Profile manager

```bash
dotlink profile list
dotlink profile show <name>
dotlink profile create <name>
dotlink profile edit <name>
dotlink profile delete <name>
```

Rules:

```bash
dotlink profile allow <name> read <path>
dotlink profile allow <name> write <path>
dotlink profile allow <name> rw <path>

dotlink profile remove-allow ...

dotlink profile deny <name> read <path>
dotlink profile deny <name> write <path>
dotlink profile deny <name> rw <path>

dotlink profile remove-deny ...
```

Boolean settings:

```text
openai
stdio
http
http-ephemeral-url
ngrok
ngrok-ephemeral-url
oauth
default-allow
shell
network
```

Dependency cascades:

- network enable → shell;
- ngrok enable → HTTP;
- ngrok ephemeral → ngrok + HTTP;
- HTTP ephemeral → HTTP;
- OAuth → HTTP.

Disable cascades:

- disabling HTTP disables ngrok and persisted local OAuth;
- disabling ngrok clears domain/path ngrok settings;
- disabling shell disables network;
- disabling OpenAI does not delete tunnel/key;
- disabling local OAuth does not disable ngrok OAuth.

## Persistent public HTTP rule

Schema validation forbids a persisted non-loopback HTTP listener unless:

```text
oauth.enabled = true
oauth.public_url = HTTPS origin
```

There is no persisted unauthenticated public mode.

Unsafe direct public no-auth is one-run-only:

```bash
dotlink --http   --http-bind=0.0.0.0:3000   --allow-public-no-auth
```

This is intentional.

## Setup UX

Start:

```text
abird dotlink setup
────────────────────────────────────────
help/documentation URL
```

Transport selection:

```text
1 OpenAI Tunnel   Recommended for ChatGPT
2 stdio           Local MCP clients (Claude Desktop, Codex, etc.)
3 HTTP            Claude.ai and other web MCP clients
```

Accept:

- number;
- name;
- comma-separated combinations;
- `all`;
- cancel tokens.

Cancel tokens:

```text
0
none
cancel
q
quit
```

Cancel must return success and save nothing.

### HTTP setup

When HTTP selected:

- local ephemeral path?
- ngrok?
- stable ngrok domain?
- ngrok ephemeral path?
- local/reverse-proxy OAuth?
- canonical public origin if relevant?
- owner credential if any OAuth-protected path needs it?

Public ngrok OAuth is automatic.

### Local permission setup

Default-oriented prompts:

- allow read to launch directory? default yes;
- allow write too?
- shell?
- network?
- developer caches when applicable.

Do not expose internal path-policy complexity in the wizard unless necessary.

## OpenAI setup paths

Two supported approaches.

### Manual tunnel

Preferred when user wants no Admin key in dotlink:

1. create/manage tunnel in OpenAI Platform;
2. paste existing `tunnel_...` ID;
3. supply Runtime key with Tunnels Read + Use.

### Automated tunnel creation

Optional:

- one-time Admin key with Tunnels Manage;
- optional org/workspace ID;
- create tunnel;
- save returned tunnel ID;
- Admin key discarded;
- Runtime key saved separately.

## Developer cache setup

Discovery should remain conservative:

- known locations;
- known environment variables;
- safe read-only CLI queries;
- existing directories only;
- no whole-home scan;
- no mutation/initialization probes.

Per cache:

```text
none
read_only
read_write
```

Config stores typed entries:

```jsonc
{
  "kind": "cargo_registry",
  "path": "/home/me/.cargo/registry",
  "mode": "read_write"
}
```

## Runtime vs profile authority

A profile must be runnable without repeating flags.

CLI flags should be a one-run adjustment layer, not required activation flags for persisted settings.

Example:

If profile contains:

```jsonc
"http": true
```

then:

```bash
dotlink -p work
```

must start HTTP.

`--http` adds it when profile does not.

`--no-http` suppresses it.

This was a previously fixed regression; do not reintroduce "profile configured but CLI flag required" behavior.

## Setup transaction requirements

A profile update may involve:

- JSONC;
- Runtime key;
- OAuth password hash.

Failure must not leave an inconsistent half-save.

Current implementation:

- atomic private file writes;
- owner password change apply + rollback around config save;
- rollback modifies only the owner-password field and preserves concurrent grants;
- newly empty OAuth state is removed after rollback.

When changing setup persistence, preserve this transactional model.

## UI principles

- succinct;
- obvious defaults;
- no jargon when plain language works;
- hide secrets;
- do not print architecture-specific asset names during normal installation UX;
- safe defaults should require Enter, not expert knowledge;
- dangerous behavior should be visibly deliberate;
- profile and CLI semantics should be predictable from names.

## Help/documentation

Setup includes a GitHub README help hint.

README is the public quick-start/manual.

`.agents/docs/configuration.md` is the concise maintainer configuration reference.

This handoff is deeper implementation context, not the user-facing manual.
