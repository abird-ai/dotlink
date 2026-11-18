# NixOS system services for dotlink.
#
# The default mode reuses the selected Unix user's existing dotlink profile,
# runtime key, and OAuth state. Only an explicit settings/configFile override
# changes the profile configuration source.
{self}: {
  config,
  lib,
  pkgs,
  utils,
  ...
}: let
  inherit (lib) mkOption types;
  cfg = config.services.dotlink;
  systemdVersion = lib.getVersion config.systemd.package;

  # Keys the module owns: identity, config file, credential files and the SSH
  # agent socket come from typed options; stdio, colour and supervision are
  # deployment facts the module sets. Accepting any of them in `environment` would
  # either be silently overridden or become a second source of truth, so the
  # module refuses the value instead of picking a winner.
  reservedEnvironment = [
    "DOTLINK_PROFILE"
    "DOTLINK_CONFIG_FILE"
    "DOTLINK_STDIO"
    "DOTLINK_COLOR"
    "DOTLINK_SUPERVISOR"
    "DOTLINK_API_KEY_FILE"
    "NGROK_AUTHTOKEN_FILE"
    "DOTLINK_SSH_AGENT_SOCKET"
  ];

  commonOptions = {
    enableDefault,
    userDefault,
    groupDefault,
    workingDirectoryDefault,
    packageDefault,
    extraPackagesDefault,
    extraGroupsDefault,
    environmentDefault,
    sshAgentSocketDefault,
  }: {
    enable = mkOption {
      type = types.bool;
      default = enableDefault;
      description = "Whether to run this dotlink instance.";
    };
    package = mkOption {
      type = types.package;
      default = packageDefault;
      description = "dotlink executable package; may come from another flake or monorepo.";
    };
    user = mkOption {
      type = types.str;
      default = userDefault;
      description = "Existing Unix user whose dotlink profile and private state are used.";
    };
    group = mkOption {
      type = types.nullOr types.str;
      default = groupDefault;
      description = "Optional primary group override; otherwise use the user's normal group.";
    };
    workingDirectory = mkOption {
      type = types.nullOr types.str;
      default = workingDirectoryDefault;
      example = "/home/alice/projects";
      description = ''
        Required launch directory. Relative dotlink grants and the default "."
        read permission are interpreted against this directory.
      '';
    };
    profile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "work";
      description = ''
        Saved user profile name. Null uses "default" for the main service, or
        the instance name for additional services. "default" explicitly selects
        the user's default profile.
      '';
    };
    settings = mkOption {
      type = types.nullOr (pkgs.formats.json {}).type;
      default = null;
      description = ''
        Optional declarative profile JSON (excluding secrets). Null reuses the
        user's existing dotlink config. Mutually exclusive with configFile.
        Rendered to the Nix store and copied into the unit's private runtime
        directory at each start, so the active config never overlaps the
        sandbox's runtime mounts.
      '';
    };
    configFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/etc/dotlink/config.jsonc";
      description = ''
        Optional explicit JSONC config file, passed as DOTLINK_CONFIG_FILE and
        used verbatim; `profile` then selects only the private state slot. Null
        reuses the selected user's XDG default. Mutually exclusive with
        settings. A file under `/nix/store`, `/etc` or `/run` is copied into the
        unit's runtime directory at start, because the store is an implicit
        shell runtime mount.
      '';
    };
    credentials = {
      runtimeApiKeyFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "/run/secrets/dotlink-api-key";
        description = ''
          Optional OpenAI runtime-key source for systemd LoadCredential.
          If absent, dotlink reads the selected user's saved profile key.
        '';
      };
      ngrokAuthTokenFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "/run/secrets/ngrok-token";
        description = ''
          Optional ngrok token source for systemd LoadCredential.
          If absent, ngrok continues to use NGROK_AUTHTOKEN.
        '';
      };
    };
    sshAgentSocket = mkOption {
      type = types.nullOr types.str;
      default = sshAgentSocketDefault;
      example = "/run/user/1000/gcr/ssh";
      description = ''
        Optional explicit Unix socket path for the SSH signing agent. If
        omitted, dotlink first checks SSH_AUTH_SOCK, then GCR's socket under
        XDG_RUNTIME_DIR or /run/user/<service-uid>/gcr/ssh. A system service
        does not automatically inherit the interactive SSH_AUTH_SOCK. An
        explicitly configured but unavailable socket fails startup rather
        than silently selecting a different agent.
      '';
    };
    extraPackages = mkOption {
      type = types.listOf types.package;
      default = extraPackagesDefault;
      description = "Extra executables made available to dotlink's service and shell tools.";
    };
    extraGroups = mkOption {
      type = types.listOf types.str;
      default = extraGroupsDefault;
      example = ["kvm" "render" "video"];
      description = ''
        Additional supplementary groups for the unit. Groups implied by
        `settings.permissions` are added automatically; declare them here too
        when the grants live in the selected user's saved profile, because the
        module cannot read that file.
      '';
    };
    environment = mkOption {
      type = types.attrsOf types.str;
      default = environmentDefault;
      example = {XDG_CONFIG_HOME = "/home/alice/.config";};
      description = ''
        Additional non-secret environment variables, such as custom XDG paths
        or RUST_LOG. Do not place credentials here or in settings. The keys the
        module owns are refused: identity, config file, credential files and the
        SSH agent socket come from their typed options, and stdio, color and
        supervision are deployment facts the module sets.
      '';
    };
  };

  modulePackage = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  optionsForMain = commonOptions {
    enableDefault = false;
    userDefault = "dotlink";
    groupDefault = null;
    workingDirectoryDefault = null;
    packageDefault = modulePackage;
    extraPackagesDefault = [];
    extraGroupsDefault = [];
    environmentDefault = {};
    sshAgentSocketDefault = null;
  };

  optionsForInstance = commonOptions {
    enableDefault = true;
    userDefault = cfg.user;
    groupDefault = cfg.group;
    workingDirectoryDefault = cfg.workingDirectory;
    packageDefault = cfg.package;
    extraPackagesDefault = cfg.extraPackages;
    extraGroupsDefault = cfg.extraGroups;
    environmentDefault = cfg.environment;
    sshAgentSocketDefault = cfg.sshAgentSocket;
  };

  instances = {default = cfg;} // cfg.instances;
  active = lib.filterAttrs (_: instance: instance.enable) instances;
  named = name:
    if name == "default"
    then "dotlink"
    else "dotlink-${name}";
  validName = name: builtins.match "[A-Za-z0-9_-]+" name != null;
  # Keep in sync with `setup::config_version()`. The module supplies the version
  # so `settings` never restates it and an older dotlink fails closed instead of
  # guessing at a newer file.
  schemaVersion = 13;
  validProfile = profile: profile == null || validName profile;
  # Nix store files are public: reject the common credential-shaped keys,
  # including nested keys. Users must provide secrets with LoadCredential.
  containsSecretKey = value:
    if builtins.isAttrs value
    then
      lib.any (
        key:
          lib.elem (lib.toLower key) [
            "runtime_api_key"
            "runtimeapikey"
            "api_key"
            "apikey"
            "ngrok_authtoken"
            "ngrokauthtoken"
            "password"
            "owner_password"
            "ownerpassword"
            "secret"
            "token"
            "credentials"
          ]
          || containsSecretKey value.${key}
      ) (builtins.attrNames value)
    else if builtins.isList value
    then lib.any containsSecretKey value
    else false;
  effectiveProfile = name: instance:
    if instance.profile != null
    then
      (
        if instance.profile == "default"
        then null
        else instance.profile
      )
    else if name == "default"
    then null
    else name;

  makeService = name: instance: let
    profile = effectiveProfile name instance;
    # The config-file override is a *base* path. With DOTLINK_PROFILE set, the
    # loader selects its sibling config.NAME.jsonc, which keeps credentials and
    # OAuth state independent for named declarative instances too.
    # A rendered config, or one under an immutable system path, cannot be the
    # active config: /nix/store is an implicit shell runtime bind while the
    # active config is a protected path, so the sandbox guard rejects that
    # overlap. Copy it into the unit's private runtime directory at each start
    # rather than relaxing the guard or symlinking it.
    declarative = instance.settings != null;
    immutableConfig =
      instance.configFile
      != null
      && (
        # Every runtime bind root the shell guard would reject, plus /run for
        # another unit's staged config: none of them can be the active file.
        lib.hasPrefix "/nix/" instance.configFile
        || lib.hasPrefix "/etc/" instance.configFile
        || lib.hasPrefix "/run/" instance.configFile
      );
    staged = declarative || immutableConfig;
    runtimeDirectory = named name;
    configSource =
      if declarative
      then
        pkgs.writeText "dotlink-${name}.jsonc" (
          builtins.toJSON (instance.settings // {version = schemaVersion;})
        )
      else instance.configFile;
    configPath =
      if staged
      then "/run/${runtimeDirectory}/config.jsonc"
      else instance.configFile;
    derivedGroups = lib.unique (
      lib.optionals (permissions.allow_kvm or false) ["kvm"]
      ++ lib.optionals (permissions.allow_gpu or [] != []) ["render" "video"]
      ++ instance.extraGroups
    );
    permissions = instance.settings.permissions or {};
    nixEnabled = permissions.allow_nix_daemon or false || permissions.allow_nix_config or false;
    credentials =
      lib.optionals (instance.credentials.runtimeApiKeyFile != null)
      ["dotlink-api-key:${instance.credentials.runtimeApiKeyFile}"]
      ++ lib.optionals (instance.credentials.ngrokAuthTokenFile != null)
      ["ngrok-authtoken:${instance.credentials.ngrokAuthTokenFile}"];
    serviceEnv =
      instance.environment
      // {DOTLINK_SUPERVISOR = "systemd";}
      # A service has no terminal; the module never relies on TTY detection.
      // {DOTLINK_COLOR = "never";}
      # A service has no stdin either, so a selected profile cannot serve the
      # stdio transport. This is a deployment fact, not a user preference.
      // {DOTLINK_STDIO = "0";}
      // lib.optionalAttrs (profile != null) {
        DOTLINK_PROFILE = profile;
      }
      // lib.optionalAttrs (instance.sshAgentSocket != null) {
        DOTLINK_SSH_AGENT_SOCKET = instance.sshAgentSocket;
      }
      // lib.optionalAttrs (configPath != null) {
        DOTLINK_CONFIG_FILE = toString configPath;
      }
      // lib.optionalAttrs (instance.credentials.runtimeApiKeyFile != null) {
        DOTLINK_API_KEY_FILE = "%d/dotlink-api-key";
      }
      // lib.optionalAttrs (instance.credentials.ngrokAuthTokenFile != null) {
        NGROK_AUTHTOKEN_FILE = "%d/ngrok-authtoken";
      };
  in {
    description = "dotlink MCP bridge${lib.optionalString (name != "default") " (${name})"}";
    wantedBy = ["multi-user.target"];
    wants = ["network-online.target"];
    after = ["network-online.target"];
    # Dotlink retries recoverable transport failures in-process; systemd
    # restarts the process with exponential backoff instead of a start fuse.
    startLimitIntervalSec = 0;
    path =
      instance.extraPackages
      ++ lib.optionals nixEnabled [pkgs.nix]
      ++ lib.optionals (permissions.allow_git_config or false) [pkgs.git]
      ++ lib.optionals (permissions.allow_ssh_agent or false) [pkgs.git pkgs.openssh];
    environment = serviceEnv;
    serviceConfig =
      {
        Type = "exec";
        User = instance.user;
        SetLoginEnvironment = true;
        WorkingDirectory = instance.workingDirectory;
        ExecStart = lib.getExe instance.package;
        # Exit 2 (usage) and 78 (CONFIG) need the operator. Exit 6
        # (NOTCONFIGURED) means first-run setup has not run yet: it is a clean
        # stop, so an unprovisioned host never fails its own switch.
        # Restart=always also covers clean exits/signals; systemd stop/restart
        # jobs suppress it. Delays start at 2s and double to 10min.
        # RESTART_RESET=1 needs NotifyAccess.
        Restart = "always";
        RestartPreventExitStatus = "2 CONFIG NOTCONFIGURED";
        SuccessExitStatus = "NOTCONFIGURED";
        RestartSec = "2s";
        RestartSteps = 8;
        RestartMaxDelaySec = "10min";
        NotifyAccess = "main";
        TimeoutStopSec = "15s";
        # Stop and restart reach every descendant, sandboxed or not.
        KillMode = "control-group";
        KillSignal = "SIGTERM";
        StandardInput = "null";
        StandardOutput = "journal";
        StandardError = "journal";
        UMask = "0077";
      }
      // lib.optionalAttrs (instance.group != null) {Group = instance.group;}
      // lib.optionalAttrs (derivedGroups != []) {SupplementaryGroups = derivedGroups;}
      // lib.optionalAttrs staged {
        RuntimeDirectory = runtimeDirectory;
        RuntimeDirectoryMode = "0700";
        # `-`: a failed copy must not become a restart loop. dotlink then starts
        # without a config and classifies the missing file itself.
        ExecStartPre = "-${pkgs.coreutils}/bin/install -m 0400 ${
          utils.escapeSystemdExecArg (toString configSource)
        } /run/${runtimeDirectory}/config.jsonc";
      }
      // lib.optionalAttrs (credentials != []) {LoadCredential = credentials;};
  };

  needsServiceUser =
    lib.any
    (instance: instance.user == "dotlink")
    (lib.attrValues active);
in {
  options.services.dotlink =
    optionsForMain
    // {
      instances = mkOption {
        type = types.attrsOf (types.submodule ({...}: {
          options = optionsForInstance;
        }));
        default = {};
        description = ''
          Optional additional independent services. Each defaults to its own
          saved named profile and has a separate systemd unit.
        '';
      };
    };

  config = lib.mkIf cfg.enable {
    warnings =
      lib.optional (
        !lib.versionOlder systemdVersion "255"
        && lib.versionOlder systemdVersion "258"
      )
      "services.dotlink: systemd ${systemdVersion} ignores RESTART_RESET=1, so healthy runs never reset restart backoff; delays stay at up to 10 minutes until an explicit restart. Use systemd 258 or newer.";

    assertions =
      [
        {
          assertion = !lib.versionOlder systemdVersion "255";
          message = "services.dotlink requires systemd 255 or newer (RestartSteps/RestartMaxDelaySec require 254; SetLoginEnvironment requires 255), but systemd ${systemdVersion} is configured.";
        }
      ]
      ++ (lib.mapAttrsToList (name: instance: {
          assertion =
            instance.workingDirectory
            != null
            && lib.hasPrefix "/" instance.workingDirectory;
          message = "services.dotlink${
            if name == "default"
            then ""
            else ".instances.${name}"
          }.workingDirectory must be an absolute path to the intended access root.";
        })
        active)
      ++ (lib.mapAttrsToList (name: instance: {
          assertion = instance.settings == null || instance.configFile == null;
          message = "dotlink ${name}: settings and configFile are mutually exclusive.";
        })
        active)
      ++ (lib.mapAttrsToList (name: instance: {
          assertion = !(lib.any (key: builtins.hasAttr key instance.environment) reservedEnvironment);
          message = "services.dotlink${
            if name == "default"
            then ""
            else ".instances.${name}"
          }.environment must not set ${lib.concatStringsSep ", " reservedEnvironment}: the module owns those values — identity, config file, credential files and the SSH agent socket come from their typed options, and stdio, color and supervision are deployment facts.";
        })
        active)
      ++ (lib.mapAttrsToList (name: instance: {
          assertion = validProfile instance.profile;
          message = "dotlink ${name}: profile names may contain only letters, digits, '-' or '_'.";
        })
        active)
      ++ (lib.mapAttrsToList (name: instance: {
          assertion =
            instance.settings
            == null
            || !(instance.settings.transports.stdio or false);
          message = "dotlink ${name}: a systemd service has no stdin, so settings.transports.stdio must stay disabled.";
        })
        active)
      ++ (
        let
          transportOf = instance: instance.settings.transports or {};
          # `false` = no HTTP transport. Null = the built-in default port, so two
          # instances that both rely on it still collide without naming it here.
          binds = lib.filter (bind: bind != false) (
            lib.mapAttrsToList (
              _: instance:
                if (transportOf instance).http or false
                then (transportOf instance).http_bind or null
                else false
            )
            active
          );
          tunnels = lib.filter (tunnel: tunnel != null) (
            lib.mapAttrsToList (
              _: instance:
                if (transportOf instance).openai or false
                then instance.settings.tunnel_id or null
                else null
            )
            active
          );
        in [
          {
            assertion = lib.length binds == lib.length (lib.unique binds);
            message = "dotlink instances must not share one HTTP ingress: one settings.transports.http_bind (or the built-in default) is used twice.";
          }
          {
            assertion = lib.length tunnels == lib.length (lib.unique tunnels);
            message = "dotlink instances must not share one settings.tunnel_id: one tunnel is used twice.";
          }
        ]
      )
      ++ (lib.mapAttrsToList (name: instance: {
          assertion =
            instance.settings
            == null
            || !(instance.settings ? version)
            || instance.settings.version == schemaVersion;
          message = "dotlink ${name}: declarative settings must use the current schema version ${toString schemaVersion} (omit version and the module supplies it).";
        })
        active)
      ++ (lib.mapAttrsToList (name: instance: {
          assertion = instance.sshAgentSocket == null || lib.hasPrefix "/" instance.sshAgentSocket;
          message = "dotlink ${name}: sshAgentSocket must be an absolute Unix socket path.";
        })
        active)
      ++ (lib.mapAttrsToList (name: instance: {
          assertion = instance.settings == null || !containsSecretKey instance.settings;
          message = "dotlink ${name}: plaintext credentials must not be placed in public Nix-store settings; use credentials.*File instead.";
        })
        active)
      ++ [
        {
          assertion =
            !(cfg.instances ? default)
            && lib.all validName (builtins.attrNames cfg.instances);
          message = "dotlink instance names must be unique, non-'default', and contain only letters, digits, '-' or '_'.";
        }
        {
          # One user plus one profile is one private state slot: credentials,
          # OAuth grants and the default config path are shared, so two
          # "independent" services would silently own the same secrets.
          assertion = let
            slots =
              lib.mapAttrsToList (
                name: instance: "${instance.user}:${
                  let
                    slot = effectiveProfile name instance;
                  in
                    if slot == null
                    then "default"
                    else slot
                }"
              )
              active;
          in
            lib.length slots == lib.length (lib.unique slots);
          message = "dotlink instances must not share one user and profile: they would share credentials and OAuth state; give each instance a distinct profile.";
        }
      ];

    users.groups = lib.mkIf needsServiceUser {
      dotlink = {};
    };
    users.users = lib.mkIf needsServiceUser {
      dotlink = {
        isSystemUser = lib.mkDefault true;
        group = lib.mkDefault "dotlink";
        home = lib.mkDefault "/var/lib/dotlink";
        createHome = lib.mkDefault true;
      };
    };

    systemd.services =
      lib.mapAttrs' (
        name: instance:
          lib.nameValuePair (named name) (makeService name instance)
      )
      active;
  };
}
