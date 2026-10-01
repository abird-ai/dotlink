# Configuration reference

Current profile schema: **v10**. Compatible older schemas migrate forward automatically in memory through explicit version-to-version migrations; v9 → v10 is supported because the v10 additions are defaulted and preserve existing meaning. The profile is written as the current schema the next time it is edited or saved. Newer schemas, and older schemas without a defined migration path, are rejected rather than guessed.

## Files

```text
~/.config/abird/dotlink/config.jsonc
~/.config/abird/dotlink/config.<profile>.jsonc
~/.config/abird/dotlink/runtime.key
~/.config/abird/dotlink/runtime.<profile>.key

~/.local/state/abird/dotlink/oauth.json
~/.local/state/abird/dotlink/oauth.<profile>.json
```

The config paths follow `$XDG_CONFIG_HOME`; OAuth state follows `$XDG_STATE_HOME` (with the platform-appropriate state-directory fallback).

Profiles are JSONC: comments and trailing commas are allowed. OpenAI Runtime keys are stored separately and never serialized into the profile. OAuth owner-password hashes, approved DCR clients, and hashed refresh grants are stored in the private OAuth state file rather than JSONC. On Unix dotlink-owned config/state directories are mode `0700` and private files are `0600`.

`DOTLINK_CONFIG` overrides only the JSONC location. Runtime keys and OAuth state remain inside dotlink's owned Abird XDG namespaces.

## Setup flow

Transport selection accepts `1`/openai, `2`/stdio, `3`/http, comma-separated combinations, or `all`. `0`, `none`, `cancel`, `q`, and `quit` cancel setup successfully without saving.

When HTTP is selected, setup can persist:

- local ephemeral MCP path;
- public ngrok HTTPS ingress;
- an optional reserved/stable ngrok domain;
- an independent ephemeral ngrok MCP path;
- OAuth protection for local/reverse-proxied HTTP;
- the canonical external OAuth origin when a reverse proxy is used.

**Externally reachable HTTP is OAuth-protected by default.** Public ngrok is protected automatically. A persisted/direct non-loopback HTTP listener must enable local OAuth and provide an HTTPS `oauth.public_url`; otherwise startup/config validation refuses it. `oauth.enabled` controls local/reverse-proxied HTTP only. The one-run `--allow-public-no-auth` flag is the explicit unsafe opt-out for public ngrok or a direct non-loopback listener.

The first time any OAuth-protected route is configured, setup asks for one owner password. The plaintext password is never persisted; dotlink stores only an Argon2id hash. Existing setup shows only `[existing credential]`, and changing it requires a new hidden password + confirmation.

For durable remote OAuth, use both a stable ngrok hostname and the stable `/mcp` path. Changing either the issuer hostname or resource path changes the OAuth identity and requires reconnecting the remote client.

Local access setup asks whether to:

- read the launch directory (default yes);
- write it too;
- expose shell;
- allow shell network access.

On Linux, setup can also discover existing developer caches and grant none/read-only/read+write access per cache. Re-running setup uses current compatible profile values as defaults and preserves rules not directly edited by the wizard.

For OpenAI Tunnel setup, either create/manage the tunnel yourself at https://platform.openai.com/settings/organization/tunnels and paste its existing `tunnel_...` ID (no Admin key is given to dotlink), or type `new` and let dotlink create it with a one-time Admin key that has Tunnels Manage.

## Schema

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
    "ngrok_ephemeral_url": false
  },
  "oauth": {
    // Protect local/reverse-proxied HTTP.
    // Public ngrok is protected independently whenever ngrok=true.
    "enabled": false,
    "public_url": null
  },
  "permissions": {
    "default_allow": true,
    "allow_read": ["/home/me/reference"],
    "allow_write": ["generated"],
    "allow_rw": [".", "/home/me/shared"],
    "deny_read": ["private-inputs"],
    "deny_write": ["locked-output"],
    "deny_rw": [".secrets"],
    "allow_shell": true,
    "allow_network": false
  },
  "caches": [
    {
      "kind": "cargo_registry",
      "path": "/home/me/.cargo/registry",
      "mode": "read_write"
    },
    {
      "kind": "npm",
      "path": "/home/me/.npm",
      "mode": "read_only"
    }
  ],
  "tunnel_id": null,
  "organization_id": null,
  "base_url": "https://api.openai.com",
  "max_shell_timeout_secs": 120,
  "max_output_bytes": 1048576,
  "max_read_bytes": 4194304,
  "max_write_bytes": 4194304
}
```

Relative permission paths resolve from the launch directory. `default_allow=true` adds read access to that directory; `--no-default-allow` suppresses it for one run.

`ngrok_domain` is a hostname only (no scheme/path/port). `oauth.public_url` is a canonical HTTPS origin for a reverse proxy; loopback HTTP is allowed for local testing. dotlink never derives OAuth issuer/resource identity from untrusted Host/forwarded headers.

## Profile manager

`default` is the reserved alias for the unnamed default profile.

```bash
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

Boolean settings are:

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

Enabling a dependent setting enables its prerequisite (`network` → shell, `ngrok` → HTTP, `ngrok-ephemeral-url` → ngrok + HTTP, `http-ephemeral-url` → HTTP, `oauth` → HTTP). Enabling ngrok requires an owner OAuth credential to already exist; use `profile edit` to configure one.

Disabling HTTP disables local OAuth and ngrok. Disabling ngrok clears its persisted domain/path settings. Disabling local OAuth does **not** weaken a still-enabled public ngrok route; ngrok remains OAuth-protected at runtime. Disabling OpenAI preserves its tunnel/key for later re-enable. Deleting a profile removes its Runtime key and OAuth state.

`profile show` never prints secret material: saved Runtime keys are shown only as `[existing key]`, and an OAuth owner credential only as `[existing credential]`.

## OAuth management

```bash
dotlink oauth status [-p <profile>]
dotlink oauth clients [-p <profile>]
dotlink oauth revoke <client-id> [-p <profile>]
dotlink oauth revoke-all [-p <profile>]
```

DCR registrations are held only in bounded in-memory pending state until the owner approves them. Approved clients and hashed refresh grants are persisted. Authorization codes and access tokens are memory-only. Revoking persisted grants from a separate CLI process takes effect for refresh immediately; restart the running dotlink process (or `Ctrl+R`) to invalidate outstanding short-lived access tokens immediately.

Owner-password changes are performed through `dotlink profile edit <name>` / `dotlink --setup` so the hidden prompt and transactional setup path remain the single secret-entry flow.

## Transport precedence

Every profile-enabled transport starts automatically.

```text
--stdio / --http             add a local transport for one run
--no-stdio                   suppress profile stdio
--no-http                    suppress profile HTTP and ngrok
--ngrok                      enable ngrok for effective HTTP
--no-ngrok                   suppress profile ngrok
--ngrok-domain=DOMAIN        override/pin the ngrok hostname
--no-ngrok-domain            ignore persisted domain for one run
--http-bind=ADDR             override profile HTTP bind

--oauth                      protect local HTTP with OAuth
--no-oauth                   disable local HTTP OAuth for one run
--public-url=HTTPS-ORIGIN    canonical reverse-proxy OAuth origin
--allow-public-no-auth       intentionally allow public HTTP/ngrok without OAuth

--ephemeral-url              enable both local/ngrok ephemeral paths
--http-ephemeral-url         override local HTTP path behavior
--ngrok-ephemeral-url        override ngrok path behavior
```

`--allow-public-no-auth` is valid only when the effective endpoint is externally reachable (ngrok or a non-loopback HTTP bind). It is intentionally one-run-only because it exposes the granted MCP surface to anyone who can reach that endpoint.

Interactive setup never saves a zero-transport selection; that choice cancels setup. Direct `profile disable` commands can intentionally leave a profile with no transports, in which case normal startup requires a one-run `--stdio` or `--http`.

## Path and capability precedence

Allow rules are additive. Deny rules win. Bare path flags target the launch directory.

```text
--allow-read[=DIR]
--allow-write[=DIR]    bare form means read+write launch directory
--allow-rw[=DIR]
--deny-read[=DIR]
--deny-write[=DIR]
--deny-rw[=DIR]
--deny=PATH
--allow-shell / --deny-shell
--allow-network / --deny-network
```

`--allow-rw=/` grants root filesystem read+write but does not disable Bubblewrap or enable shell/network. `--allow-all --no-sandbox` is the explicit full-host escape hatch.

Shell access requires at least one readable directory.

## Runtime terminal controls

Interactive runs without stdio expose immediate key controls:

```text
Ctrl-C    exit
Ctrl-R    reload profile/policy/transports
v         cycle quiet → TOOL → TOOL + REQ → quiet
```

The current verbosity state is shared by all logger clones, so TOOL/REQ behavior changes immediately without restarting. Every `v` press emits a `LOG verbosity ...` line. Runtime controls are intentionally disabled when stdio MCP is active because stdin is reserved for the MCP protocol.

## Developer caches

Autodiscovery covers Cargo registry/git, npm, pnpm, Yarn, pip, uv, Go module/build, Maven, Gradle, sccache, and ccache. It uses environment variables, known existing locations, and non-mutating local queries such as `npm config get cache`, `pip cache dir`, `uv cache dir`, and `go env`.

Cache grants are shell-only. They never expand MCP read/write roots. Deny rules are re-applied to cache sources before mounting; a write deny downgrades a matching RW cache to RO and a read deny removes it.
