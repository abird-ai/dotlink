# Configuration reference

Current profile schema: **v9**. Older profile schemas are rejected; rerun setup to recreate them.

## Files

```text
~/.config/abird/dotlink/config.jsonc
~/.config/abird/dotlink/config.<profile>.jsonc
~/.config/abird/dotlink/runtime.key
~/.config/abird/dotlink/runtime.<profile>.key
```

Profiles are JSONC only: comments and trailing commas are allowed. OpenAI runtime keys are stored separately and never serialized into the profile.

## Setup flow

Transport selection accepts `1`/`openai`, `2`/`stdio`, `3`/`http`, comma-separated combinations, `all`, or `none`.

When HTTP is selected, setup can persist:

- local ephemeral MCP path;
- public ngrok HTTPS endpoint;
- independent ephemeral ngrok MCP path.

Local access setup asks whether to:

- read the launch directory (default yes);
- write it too;
- expose shell;
- allow shell network access.

On Linux, setup can also discover existing developer caches and grant none/read-only/read+write access per cache.

## Schema

```jsonc
{
  "version": 9,
  "transports": {
    "openai": false,
    "stdio": false,
    "http": true,
    "http_bind": "127.0.0.1:3000",
    "http_ephemeral_url": true,
    "ngrok": false,
    "ngrok_ephemeral_url": false
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

Relative paths resolve from the launch directory. `default_allow=true` adds read access to that directory; `--no-default-allow` suppresses it for one run.

## Transport precedence

Every profile-enabled transport starts automatically.

```text
--stdio / --http       add a local transport for one run
--no-stdio             suppress profile stdio
--no-http              suppress profile HTTP and ngrok
--ngrok                enable ngrok for effective HTTP
--no-ngrok             suppress profile ngrok
--http-bind            override profile HTTP bind
--ephemeral-url        enable both local/ngrok ephemeral paths
--http-ephemeral-url   override local HTTP path behavior
--ngrok-ephemeral-url  override ngrok path behavior
```

A profile may persist no transport. Normal startup then errors until `--stdio` or `--http` is supplied.

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

## Developer caches

Autodiscovery covers Cargo registry/git, npm, pnpm, Yarn, pip, uv, Go module/build, Maven, Gradle, sccache, and ccache. It uses environment variables, known existing locations, and non-mutating local queries such as `npm config get cache`, `pip cache dir`, `uv cache dir`, and `go env`.

Cache grants are shell-only. They never expand MCP read/write roots. Deny rules are re-applied to cache sources before mounting; a write deny downgrades a matching RW cache to RO and a read deny removes it.
