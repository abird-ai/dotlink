# Configuration reference

Current profile schema: **v10**. Compatible older schemas migrate forward automatically in memory through explicit version-to-version migrations; v9 → v10 is supported because the v10 additions are defaulted and preserve existing meaning. The profile is written as the current schema the next time it is edited or saved. Newer schemas, and older schemas without a defined migration path, are rejected rather than guessed.

## Files

```text
~/.config/abird/dotlink/config.jsonc
~/.config/abird/dotlink/config.<profile>.jsonc
~/.config/abird/dotlink/runtime.key
~/.config/abird/dotlink/runtime.<profile>.key
~/.config/abird/dotlink/runtime.<generation>.key
~/.config/abird/dotlink/runtime.<profile>.<generation>.key

~/.local/state/abird/dotlink/oauth.json
~/.local/state/abird/dotlink/oauth.<profile>.json
```

`dotlink --print-id` reads only the configured tunnel ID (or an explicit
default-profile `DOTLINK_ID` override); it does not need saved Runtime
credentials, transports or interactive setup.

The config paths follow `$XDG_CONFIG_HOME`; OAuth state follows `$XDG_STATE_HOME` (with the platform-appropriate state-directory fallback). An XDG override must be non-empty and absolute; a relative or empty value is ignored and the home-directory fallback is used.

On runtime startup, explicit `DOTLINK_ID` + `DOTLINK_API_KEY` (or corresponding `CONTROL_PLANE_*` aliases) and `DOTLINK_API_KEY_FILE` are resolved before any saved Runtime key. The file override takes precedence over a key in the environment; a file override alone may reuse the profile's saved tunnel ID. When OpenAI is disabled and not explicitly enabled through environment credentials, dotlink does not open its saved Runtime key at all. A missing/unreadable saved key matters only when an enabled OpenAI transport actually needs it. Explicit overrides do not rewrite the saved profile.

Profiles are JSONC: comments and trailing commas are allowed. OpenAI Runtime keys are stored separately and never serialized into the profile. The optional `runtime_key_revision` and `owner_password_revision` fields are opaque selectors: setup stages immutable credential generations before atomically selecting them with the JSONC rename. A crash before selection keeps the previous key and owner password live; legacy profiles without selectors keep using `runtime[.<profile>].key` and the original OAuth owner hash. After a durable credential rotation, obsolete generations are pruned, retaining the active and immediately preceding secrets; unrelated profile edits do not retire credentials potentially in use by running processes. Failed cleanup warns but never turns an already-committed edit into an error. Legacy profiles continue to load unchanged. A missing selected Runtime key fails closed when OpenAI executes, but `profile show` and interactive `profile edit` remain usable to repair it. Interrupted profile deletion is retryable even when only orphaned generated keys remain. OAuth owner-password hashes, approved DCR clients, and hashed refresh grants are stored in the private OAuth state file rather than JSONC. On Unix dotlink-owned config/state directories are mode `0700` and private files are `0600`.

`DOTLINK_CONFIG` overrides only the JSONC location. Runtime keys and OAuth state remain inside dotlink's owned Abird XDG namespaces.

Local HTTP and stdio **intentionally have no shared admission budget**: the model/client controls concurrency. The OpenAI tunnel has a separate in-flight bound, and tool-specific file/output/timeout bounds still apply. This is a deliberate operator choice and should not be re-raised as an architectural defect without a concrete separate bug.

Running `dotlink --serialize` (or `-s`) executes all MCP calls sequentially within that process; it does not change the persisted schema, require a profile migration, or coordinate separate dotlink instances. Linux defaults to concurrent tool execution with descriptor-rooted filesystem IO; other platforms preserve the conservative gate until their secure file operations are implemented.

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

The first time any OAuth-protected route is configured, setup asks for one owner password. The plaintext password is never persisted; dotlink stores only an Argon2id hash. Existing setup shows only `[existing credential]`, and changing it requires a new hidden password + confirmation. A running OAuth server resolves the currently committed profile selector for every owner-password check and verifies the selection again after a successful hash, so changing the owner password takes effect without restarting the HTTP transport; a removed previously saved profile fails closed. Already issued access tokens remain valid until expiry or runtime restart as documented under OAuth management.

For durable remote OAuth, use both a stable ngrok hostname and the stable `/mcp` path. Changing either the issuer hostname or resource path changes the OAuth identity and requires reconnecting the remote client.

Local access setup asks whether to:

- read the launch directory (default yes);
- write it too;
- expose shell;
- allow shell network access.

On Linux, setup can also discover existing developer caches and grant none/read-only/read+write access per cache. Re-running setup uses current compatible profile values as defaults and preserves rules not directly edited by the wizard.

For OpenAI Tunnel setup, either create/manage the tunnel yourself at <https://platform.openai.com/settings/organization/tunnels> and paste its existing `tunnel_...` ID (no Admin key is given to dotlink), or type `new` and let dotlink create it with a one-time Admin key that has Tunnels Manage.

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
  "max_shell_timeout_secs": 3600,
  "max_output_bytes": 1048576,
  "max_read_bytes": 4194304,
  "max_write_bytes": 4194304
}
```

Relative permission paths resolve from the launch directory. `default_allow=true` adds read access to that directory; `--no-default-allow` suppresses it for one run.

grep has a per-request work limit: up to 100,000 directory entries, 32 MiB
of file-content I/O, and 4096 candidate files. Incomplete results include
`scan_incomplete` and `incomplete_reason` (directory or content budget).
Text tools reject binary controls and invalid UTF-8; use bread/bwrite/bedit
for bytes. Local HTTP/stdio tool calls remain intentionally unrestricted in
aggregate concurrency.

`max_shell_timeout_secs` and `max_output_bytes` are policy ceilings, not the values themselves, and a configured ceiling is honoured exactly: a byte limit below **1024** or a timeout below **1 s** is rejected when the profile is loaded, never raised at startup. Per-call arguments are caller-controlled, clamped to `1..=ceiling`, and fall back to the ceiling when omitted. On the OpenAI tunnel the client owns the delivery window, so an omitted `timeout_secs` defaults to that window minus a 5 s delivery reserve instead of the ceiling; an explicit value larger than the window is shortened, and every tool dispatch is bounded by the same reserve so the terminal response can still be posted. The default shell ceiling is 3600 s (60 min) and the default output ceiling is 1 MiB; lower them in config to bound runaway commands or oversized results. `max_output_bytes` also bounds the JSON replies of `grep` and `ls`, not only a shell stream. `max_read_bytes` bounds the `read` reply — the returned window, the truncation marker and each returned line — not the total size of a file that is scanned to reach an offset; making room for the marker drops whole lines, never part of one, and a line that alone exceeds the ceiling is an error naming it (read it with `bread` or raise the ceiling). Lines skipped to reach `offset` are only scanned for their newline, so they may exceed the ceiling and are never decoded. `max_write_bytes` is also the per-call payload ceiling for every mutating tool (`write` content, `edit` old+new text, `patch` text, `bwrite` decoded data, `bedit` decoded ranges), and the streamable-HTTP request-body ceiling follows it: a body may be six times `max_write_bytes` plus 64 KiB, so raising the ceiling raises the transport ceiling instead of hitting a hidden 4 MiB limit. The embedded tunnel's MCP response limit is similarly derived from the configured read/output ceilings, with JSON escaping and framing headroom, instead of a fixed 16 MiB limit. The advertised `bash`/`powershell` schema reports those ceilings as concrete numbers, and a shell call emits bare `notifications/progress` heartbeats (monotonic counter, no total, no message) while it runs when the client supplies a progress token.

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

## Host integrations

Profile `permissions.allow_nix_daemon`, `allow_nix_config`, `allow_git_config`
and `allow_ssh_agent` default to `false` (additive schema-v10 fields). They can
be toggled with `dotlink profile enable/disable <name> nix-daemon|nix-config|git-config|ssh-agent`,
selected in interactive setup after discovery, or enabled for a run via
`--allow-nix-daemon`, `--allow-nix-config`, `--allow-git-config`, or
`--allow-ssh-agent`. All require a Linux Bubblewrap shell; discovery does not
implicitly grant access. Nix daemon and SSH agent use Unix socket mounts, not
full directory grants; Nix/Git configs are mounted read-only into the private
sandbox HOME. Agent discovery prioritizes the explicit
`DOTLINK_SSH_AGENT_SOCKET` override (used by NixOS `sshAgentSocket`), a valid
`SSH_AUTH_SOCK`, `$XDG_RUNTIME_DIR/gcr/ssh`, and finally
`/run/user/<effective-uid>/gcr/ssh`. This finds GCR even when a systemd system
service inherits neither session variable. An explicit missing/invalid socket
fails startup; a stale inherited `SSH_AUTH_SOCK` may fall back to GCR. See
`nixos-service.md` for service startup and agent-lifecycle considerations.

## Hardware acceleration

Linux Bubblewrap supports `--allow-kvm` and repeatable
`--allow-gpu=amd`/`--allow-gpu=nvidia`. Profile schema v10 has additive,
default-disabled `permissions.allow_kvm: bool` and
`permissions.allow_gpu: ["amd", "nvidia"]` fields. Persist with
`dotlink profile enable/disable <name> kvm|gpu-amd|gpu-nvidia` or opt in
during guided setup. Runtime discovery requires real matching character
devices; DRM render/card nodes are matched by kernel PCI vendor ID before
mounting. The corresponding NixOS options are `hostAccess.kvm` and
`hostAccess.gpu`. Hardware device grants do not alter ordinary filesystem
permissions and do not implicitly enable shell networking. A request for
missing hardware fails clearly.

## Developer caches

Autodiscovery covers Cargo registry/git, npm, pnpm, Yarn, pip, uv, Go module/build, Maven, Gradle, sccache, and ccache. It uses environment variables, known existing locations, and non-mutating local queries such as `npm config get cache`, `pip cache dir`, `uv cache dir`, and `go env`.

Use `--allow-cache-cargo-registry[=ro|rw]`,
`--allow-cache-cargo-git[=ro|rw]`, `--allow-cache-npm[=ro|rw]`, and analogous
flags for `pnpm`, `yarn`, `pip`, `uv`, `go-mod`, `go-build`, `maven`, `gradle`,
`sccache`, and `ccache`. Bare flags default to `ro`; `=rw` opts into writes.
Every explicit CLI flag uses the same detected source as onboarding; a
requested cache whose directory does not exist is skipped with a warning
rather than failing startup, so a service configured for a cache that has not
been created yet still starts. A CLI-selected mode overrides
the persisted mode for that cache for one run.

Cache grants are shell-only. They never expand MCP read/write roots. Deny rules are re-applied to cache sources before mounting; a write deny downgrades a matching RW cache to RO and a read deny removes it.
