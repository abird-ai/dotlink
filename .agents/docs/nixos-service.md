# NixOS system service

dotlink exports `nixosModules.default` (alias `nixosModules.dotlink`). It builds
the existing native Linux package and adds systemd supervision; there is no
separate daemon implementation.

## Reuse an existing user's dotlink setup

```nix
{
  inputs.dotlink.url = "github:abird-ai/dotlink";
  # Add inputs.dotlink.nixosModules.default to your NixOS modules.
}
```

```nix
services.dotlink = {
  enable = true;
  user = "alice";
  workingDirectory = "/home/alice/projects";
};
```

This runs `dotlink --no-stdio --color=never` as `alice`, with the specified
`WorkingDirectory`. By default **no new config or secret is generated**.
dotlink loads that user's usual files:

- `$XDG_CONFIG_HOME/abird/dotlink/config.jsonc` or `~/.config/abird/dotlink/config.jsonc`
- `$XDG_CONFIG_HOME/abird/dotlink/runtime.key` or `~/.config/abird/dotlink/runtime.key`
- OAuth state under `$XDG_STATE_HOME/abird/dotlink/` (or `~/.local/state/abird/dotlink/`)

Select a saved named profile with `services.dotlink.profile = "work";`.
The named profile uses `config.work.jsonc`, `runtime.work.key`, and its
corresponding OAuth state. Without a pre-existing profile, startup reports a
noninteractive first-run error. Run `dotlink --setup` interactively as the
selected user or supply declarative `settings`.

The unit is a **system** service. It uses the selected Unix user's HOME, but it
does not source their shell's `.profile` or import their interactive session's
XDG variables. For custom XDG locations, set
`services.dotlink.environment.XDG_CONFIG_HOME` or `XDG_STATE_HOME`
explicitly. The `user` must exist; prefer a non-root account. Explicitly
choosing `user = "root"` grants the process root-level host authority and is
strongly discouraged. The default `user = "dotlink"` creates
a dedicated service user with home `/var/lib/dotlink`.

`workingDirectory` is mandatory and must be absolute. This is deliberate:
the permission `default_allow = true` reads the directory **dotlink starts
from**, and relative allow/deny grants are rooted there. We must not silently
expose a user's entire home or a surprise directory.

## Declarative profile

```nix
services.dotlink = {
  enable = true;
  user = "alice";
  workingDirectory = "/home/alice/projects";

  settings = {
    tunnel_id = "tunnel_...";
    transports = {
      openai = true;
      stdio = false;
    };
    permissions = {
      default_allow = true;
      allow_rw = [ "my-repository" ];
      allow_shell = true;
      allow_network = false;
    };
  };

  extraPackages = with pkgs; [ git ripgrep fd ];
  logging.verbosity = "tools";
};
```

A read-only JSON document with schema version 10 is generated in the Nix store
and passed to dotlink through `DOTLINK_CONFIG`. This *only* overrides the
profile JSON; unless explicitly overridden, dotlink still loads the selected
user's own saved `runtime.key` and OAuth state. The schema is dotlink's
existing JSONC schema, not a second independently maintained config format.
Never include keys, passwords, or tokens inside `settings`, as Nix store
objects are not secret.

Alternatively, set `configFile = "/path/to/config.jsonc";` to select an
external **base** JSONC file without generating a file in the Nix store.
`settings` and `configFile` are mutually exclusive. They also cannot be
combined with an explicit `profile`, because named-profile selection changes
the JSONC filename. For a named instance, dotlink selects the sibling
`config.<instance>.jsonc` file from that base path. Generated `settings`
automatically create the correctly named sibling in the Nix store, so each
instance retains independent saved keys and OAuth state.

## Optional credential overrides

```nix
services.dotlink.credentials = {
  runtimeApiKeyFile = config.sops.secrets.dotlink-runtime.path;
  ngrokAuthTokenFile = config.sops.secrets.dotlink-ngrok.path;
};
```

Each configured file is copied by systemd `LoadCredential` into the service's
private credentials directory. Only the **path** is passed to dotlink:

- `DOTLINK_API_KEY_FILE` overrides the stored Runtime API key, independently
  of the configured `tunnel_id`. Without it, the normal saved
  `runtime[.<profile>].key` is used.
- `NGROK_AUTHTOKEN_FILE` overrides `NGROK_AUTHTOKEN`. Without it, ngrok
  still reads its normal environment variable.

Both overrides are optional and independent of `settings`. Neither plaintext
key is embedded in the unit, command line, generated JSON or Nix store.
Since `LoadCredential` snapshots its source at service start, restart the unit
when a secret rotates (for example, via your secret manager's `restartUnits`).
File-backed paths are explicitly protected by dotlink's MCP filesystem policy
and removed from the shell environment. Do not run an unrestricted unsandboxed
shell as the same Unix user when credentials are accessible to that user.

Owner OAuth passwords cannot be configured as plaintext Nix settings. Complete
OAuth owner setup using dotlink's interactive setup under the **same**
user/XDG paths before enabling public OAuth, then reuse its persistent
OAuth state. For a fully unattended first installation, owner bootstrap should
be a separate explicit secure initialization step, not a plaintext Nix option.

## Developer service access and SSH signing

Host integrations and discovered caches can be enabled declaratively without
giving the systemd service access to the user's entire home or SSH private key
files:

```nix
services.dotlink = {
  enable = true;
  user = "alice";
  workingDirectory = "/home/alice/projects";
  hostAccess = {
    nixDaemon = true;
    nixConfig = true;
    gitConfig = true;
    sshAgent = true;
  };
  # GCR is discovered automatically from the service user's UID.
  # sshAgentSocket = "/run/user/1000/gcr/ssh"; # optional override
  cacheAccess."cargo-registry" = "rw";
  cacheAccess."cargo-git" = "ro";
};
```

The module supplies `--allow-shell` whenever a host integration or cache is
requested, and automatically adds the matching `pkgs.nix`, `pkgs.git`, and
`pkgs.openssh` executables to the service path. Other tools can still be
added via `extraPackages`; the security boundary remains Linux Bubblewrap.
The service uses the selected Unix user's effective UID to discover a running
GCR SSH agent even without `SSH_AUTH_SOCK` or `XDG_RUNTIME_DIR`. Discovery
checks, in order:

1. Explicit `sshAgentSocket` (passed as `DOTLINK_SSH_AGENT_SOCKET`).
2. Service `environment.SSH_AUTH_SOCK`, if it names a valid socket.
3. `$XDG_RUNTIME_DIR/gcr/ssh`, if available.
4. `/run/user/<service-uid>/gcr/ssh`.

The selected socket is mounted into Bubblewrap as `/tmp/ssh-agent.sock`, and
that path is supplied to Git/OpenSSH as `SSH_AUTH_SOCK`. The explicit override
must name a valid socket; it never silently selects a different agent. Stale
inherited `SSH_AUTH_SOCK` values can fall back to GCR.

A system service does not start GCR automatically: the agent must already be
running and accessible as the service user. GCR commonly starts with the
interactive user session, not at boot. Start/restart dotlink after GCR becomes
available if needed. Dotlink reports an actionable error if no permitted agent
socket exists at startup; restart it after the agent socket changes to refresh
its pinned descriptor.

An SSH-agent socket does *not* expose `~/.ssh` or private key files. Minimal
read-only `/etc/passwd`, `/etc/group`, and `/etc/nsswitch.conf` files are
shared so OpenSSH can identify the service user when signing. The socket does
authorize any signing/authentication supported by the loaded agent keys, not
just Git signing; consider `ssh-add -c` or a dedicated signing agent. Git
signing works best when `user.signingkey` contains a `key::ssh-ed25519 ...`
**public** key string rather than a private/public file path under `~/.ssh`.

The Nix daemon grants controlled Unix-socket access, independent of shell
network permission. `NIX_REMOTE=daemon` is configured internally, and the
user's XDG nix.conf is shared read-only only when `hostAccess.nixConfig` is
true. Git similarly maps only the user's existing `~/.gitconfig` and
`$XDG_CONFIG_HOME/git/config`; `includeIf` targets and `.ssh` are not
implicitly mounted. These config files may contain credentials—review before
enabling. Each enabled `cacheAccess` key maps to a matching discovered
`--allow-cache-<kind>=ro|rw` flag; all 13 known families are accepted.

## KVM and GPU acceleration

Opt into KVM or AMD/NVIDIA GPU access per service, while continuing to use the
user's saved default profile and keeping Bubblewrap enabled:

```nix
services.dotlink = {
  enable = true;
  user = "alice";
  workingDirectory = "/home/alice/projects";
  hostAccess = {
    kvm = true;
    gpu = [ "amd" "nvidia" ];
  };
  # Optional: install selected tools into this service's PATH.
  # extraPackages = with pkgs; [ vulkan-tools qemu ];
};
```

The module grants `SupplementaryGroups = [ "kvm" "render" "video" ];`
when all three capabilities are enabled. A KVM-only service receives just
`kvm`; GPU access adds `render` and `video`. Only this systemd unit receives
these additional groups; dotlink does not change the user's global groups.
The selected user and active NixOS graphics stack must support the device
nodes. Additional `DevicePolicy` or `DeviceAllow` restrictions configured
elsewhere in systemd can still prohibit access. Dotlink does not install
CUDA/ROCm SDKs, NVIDIA drivers, VM network devices, or display-server sockets;
manage drivers through the host NixOS configuration, toolchains through
`extraPackages` or `nix develop`, and other capabilities separately.

## Multiple services

```nix
services.dotlink = {
  enable = true;
  user = "alice";
  workingDirectory = "/home/alice/projects";

  instances.work = {
    workingDirectory = "/home/alice/work";
    # Defaults to saved profile "work".
  };

  instances.personal = {
    workingDirectory = "/home/alice/personal";
    settings = {
      transports.openai = false;
      transports.http = true;
      permissions.default_allow = true;
    };
    # Generated config.personal.jsonc; Runtime/OAuth use named profile "personal".
  };
};
```

This creates `dotlink.service`, `dotlink-work.service`, and
`dotlink-personal.service`. Named instances inherit the root package,
user, group, environment, working directory, extra packages and log verbosity.
If an instance has no managed config, it defaults to a saved profile named
after the instance. Each instance may override its own credential files.
Disable individual instances with `instances.work.enable = false;` or all
dotlink services with `services.dotlink.enable = false;`. Disabling does
not remove their saved credentials or OAuth state.

Do not point simultaneous instances at the same OpenAI tunnel or HTTP port.
Named profiles keep their persistent OAuth state independent.

## systemd lifecycle and logging

The service waits for `network-online.target`, restarts on failure, and
handles SIGTERM gracefully, cancelling transports before exit. It disables
stdio MCP to keep systemd's stdin and journal separate from the protocol.
Service stdout/stderr flow to journald with color disabled.

```bash
systemctl status dotlink
journalctl -u dotlink -f
systemctl restart dotlink

systemctl status dotlink-work
journalctl -u dotlink-work -f
```

`services.dotlink.serialize = true;` optionally passes `--serialize` to execute
one MCP tool call at a time. The default is `false` on Linux, allowing
independent shell and file operations concurrently. Named instances can
override the setting. See the README for the non-Linux fallback.

`logging.verbosity` controls activity detail:

| Setting | Contents |
| --- | --- |
| `"lifecycle"` (default) | Startup, connections, warnings, recovery and errors |
| `"tools"` | Above + tool names, paths / command summaries, outcomes and timings |
| `"requests"` | Above + MCP request diagnostics |

These map to normal CLI output, `-v`, and `-vv`. `RUST_LOG` is separately
configurable using `environment.RUST_LOG`. Logs may include filesystem paths,
tunnel IDs, public MCP URLs and bounded request metadata; commands, grep
patterns and bulk payloads are represented only by size. rmcp tracing remains
suppressed regardless of `RUST_LOG` because upstream debug events render full
MCP payloads. Journald retention/access remains under the host's policy. Avoid
request logging on shared systems unless needed.

systemd hardening must preserve dotlink's Bubblewrap namespaces, user-owned
project directories, and writable developer caches. The module intentionally
does not impose `ProtectHome`, `ProtectSystem=strict`, or a system-wide
read-only filesystem policy; those can contradict a user-profile permission
policy. dotlink's own access/deny rules, the selected Unix user's normal
filesystem permissions, and Bubblewrap are still enforced.

The module does not automatically open firewall ports. Inbound public HTTP
continues to require dotlink's current OAuth/HTTPS-origin safety checks. ngrok
uses outbound access.

## Testing

```bash
nix flake check
nix eval .#nixosModules.default --apply builtins.typeOf
```

The flake exports a `nixos-module` check for Linux which evaluates real
NixOS configurations for user-profile fallback, declarative config,
credentials, named services, enable/disable, and option validation.
