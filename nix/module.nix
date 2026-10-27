# NixOS system services for dotlink.
#
# The default mode reuses the selected Unix user's existing dotlink profile,
# runtime key, and OAuth state. Only an explicit settings/configFile override
# changes the profile configuration source.
{self}: {
  config,
  lib,
  pkgs,
  ...
}: let
  inherit (lib) mkOption types;
  cfg = config.services.dotlink;
  systemdVersion = lib.getVersion config.systemd.package;

  commonOptions = {
    enableDefault,
    userDefault,
    groupDefault,
    workingDirectoryDefault,
    packageDefault,
    extraPackagesDefault,
    environmentDefault,
    verbosityDefault,
    serializeDefault,
    hostAccessDefault,
    sshAgentSocketDefault,
    cacheAccessDefault,
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
        Declarative settings are placed read-only in the Nix store.
      '';
    };
    configFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/etc/dotlink/config.jsonc";
      description = ''
        Optional explicit JSONC file used as DOTLINK_CONFIG. Null reuses the
        selected user's XDG default. Mutually exclusive with settings.
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
    hostAccess = {
      nixDaemon = mkOption {
        type = types.bool;
        default = hostAccessDefault.nixDaemon;
        description = "Expose the host Nix daemon socket to sandboxed shell tools.";
      };
      nixConfig = mkOption {
        type = types.bool;
        default = hostAccessDefault.nixConfig;
        description = "Share the user's Nix configuration read-only inside the shell sandbox.";
      };
      sshAgent = mkOption {
        type = types.bool;
        default = hostAccessDefault.sshAgent;
        description = "Expose the SSH agent socket, not private SSH keys, inside the shell sandbox.";
      };
      gitConfig = mkOption {
        type = types.bool;
        default = hostAccessDefault.gitConfig;
        description = "Share the user's Git global/XDG config read-only in the shell sandbox.";
      };
      kvm = mkOption {
        type = types.bool;
        default = hostAccessDefault.kvm;
        description = "Grant the sandbox access to /dev/kvm for hardware-accelerated virtualization.";
      };
      gpu = mkOption {
        type = types.listOf (types.enum ["amd" "nvidia"]);
        default = hostAccessDefault.gpu;
        example = ["amd" "nvidia"];
        description = "GPU vendors whose character devices and read-only driver resources are exposed to sandboxed shell commands.";
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
    cacheAccess = mkOption {
      type = types.attrsOf (types.enum ["ro" "rw"]);
      default = cacheAccessDefault;
      example = {
        "cargo-registry" = "rw";
        "go-mod" = "ro";
      };
      description = "Detected shell-only developer caches by CLI flag suffix; ro or rw. A configured cache whose directory is absent on the host is skipped with a warning instead of failing service startup.";
    };
    serialize = mkOption {
      type = types.bool;
      default = serializeDefault;
      description = "Run MCP tool calls sequentially instead of concurrently.";
    };
    logging.verbosity = mkOption {
      type = types.enum ["lifecycle" "tools" "requests"];
      default = verbosityDefault;
      description = ''
        Lifecycle status only, tool activity (-v), or MCP request diagnostics
        (-vv). Tool logging may contain paths and command fragments.
      '';
    };
    extraPackages = mkOption {
      type = types.listOf types.package;
      default = extraPackagesDefault;
      description = "Extra executables made available to dotlink's service and shell tools.";
    };
    environment = mkOption {
      type = types.attrsOf types.str;
      default = environmentDefault;
      example = {XDG_CONFIG_HOME = "/home/alice/.config";};
      description = ''
        Additional non-secret environment variables, such as custom XDG paths
        or RUST_LOG. Do not place credentials here or in settings.
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
    environmentDefault = {};
    verbosityDefault = "lifecycle";
    serializeDefault = false;
    hostAccessDefault = {
      nixDaemon = false;
      nixConfig = false;
      sshAgent = false;
      gitConfig = false;
      kvm = false;
      gpu = [];
    };
    sshAgentSocketDefault = null;
    cacheAccessDefault = {};
  };

  optionsForInstance = commonOptions {
    enableDefault = true;
    userDefault = cfg.user;
    groupDefault = cfg.group;
    workingDirectoryDefault = cfg.workingDirectory;
    packageDefault = cfg.package;
    extraPackagesDefault = cfg.extraPackages;
    environmentDefault = cfg.environment;
    verbosityDefault = cfg.logging.verbosity;
    serializeDefault = cfg.serialize;
    hostAccessDefault = cfg.hostAccess;
    sshAgentSocketDefault = cfg.sshAgentSocket;
    cacheAccessDefault = cfg.cacheAccess;
  };

  instances = {default = cfg;} // cfg.instances;
  active = lib.filterAttrs (_: instance: instance.enable) instances;
  named = name:
    if name == "default"
    then "dotlink"
    else "dotlink-${name}";
  validName = name: builtins.match "[A-Za-z0-9_-]+" name != null;
  validProfile = profile: profile == null || validName profile;
  cacheKeys = [
    "cargo-registry"
    "cargo-git"
    "npm"
    "pnpm"
    "yarn"
    "pip"
    "uv"
    "go-mod"
    "go-build"
    "maven"
    "gradle"
    "sccache"
    "ccache"
  ];

  hasConfig = instance: instance.settings != null || instance.configFile != null;
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
    # DOTLINK_CONFIG is a *base* path. With --profile NAME, the loader
    # selects its sibling config.NAME.jsonc, which keeps credentials and
    # OAuth state independent for named declarative instances too.
    configPath =
      if instance.settings != null
      then "${pkgs.writeTextDir
        (
          if profile == null
          then "config.jsonc"
          else "config.${profile}.jsonc"
        )
        (builtins.toJSON (instance.settings // {version = 10;}))}/config.jsonc"
      else instance.configFile;
    hostEnabled =
      instance.hostAccess.nixDaemon
      || instance.hostAccess.nixConfig
      || instance.hostAccess.sshAgent
      || instance.hostAccess.gitConfig
      || instance.hostAccess.kvm
      || instance.hostAccess.gpu != [];
    args =
      ["--no-stdio" "--color=never"]
      ++ lib.optionals (hostEnabled || instance.cacheAccess != {}) ["--allow-shell"]
      ++ lib.optionals instance.hostAccess.nixDaemon ["--allow-nix-daemon"]
      ++ lib.optionals instance.hostAccess.nixConfig ["--allow-nix-config"]
      ++ lib.optionals instance.hostAccess.sshAgent ["--allow-ssh-agent"]
      ++ lib.optionals instance.hostAccess.gitConfig ["--allow-git-config"]
      ++ lib.optionals instance.hostAccess.kvm ["--allow-kvm"]
      ++ map (vendor: "--allow-gpu=${vendor}") instance.hostAccess.gpu
      ++ lib.mapAttrsToList (name: mode: "--allow-cache-${name}=${mode}") instance.cacheAccess
      ++ lib.optionals instance.serialize ["--serialize"]
      ++ lib.optionals (profile != null) ["--profile=${profile}"]
      ++ lib.optionals (instance.logging.verbosity == "tools") ["-v"]
      ++ lib.optionals (instance.logging.verbosity == "requests") ["-vv"];
    credentials =
      lib.optionals (instance.credentials.runtimeApiKeyFile != null)
      ["dotlink-api-key:${instance.credentials.runtimeApiKeyFile}"]
      ++ lib.optionals (instance.credentials.ngrokAuthTokenFile != null)
      ["ngrok-authtoken:${instance.credentials.ngrokAuthTokenFile}"];
    serviceEnv =
      instance.environment
      // {DOTLINK_SUPERVISOR = "systemd";}
      // lib.optionalAttrs (instance.sshAgentSocket != null) {
        DOTLINK_SSH_AGENT_SOCKET = instance.sshAgentSocket;
      }
      // lib.optionalAttrs (configPath != null) {
        DOTLINK_CONFIG = toString configPath;
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
      ++ lib.optionals (instance.hostAccess.nixDaemon || instance.hostAccess.nixConfig) [pkgs.nix]
      ++ lib.optionals (instance.hostAccess.gitConfig || instance.hostAccess.sshAgent) [pkgs.git]
      ++ lib.optionals instance.hostAccess.sshAgent [pkgs.openssh];
    environment = serviceEnv;
    serviceConfig =
      {
        Type = "exec";
        User = instance.user;
        SetLoginEnvironment = true;
        WorkingDirectory = instance.workingDirectory;
        ExecStart = "${lib.getExe instance.package} ${lib.concatStringsSep " " args}";
        # Exit 2 (usage) and 78 (CONFIG) need the operator. Restart=always
        # also covers clean exits/signals; systemd stop/restart jobs suppress it.
        # Delays start at 2s and double to 10min. RESTART_RESET=1 needs NotifyAccess.
        Restart = "always";
        RestartPreventExitStatus = "2 CONFIG";
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
      // lib.optionalAttrs (instance.hostAccess.kvm || instance.hostAccess.gpu != []) {
        SupplementaryGroups = lib.unique (
          lib.optionals instance.hostAccess.kvm ["kvm"]
          ++ lib.optionals (instance.hostAccess.gpu != []) ["render" "video"]
        );
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
          assertion = validProfile instance.profile;
          message = "dotlink ${name}: profile names may contain only letters, digits, '-' or '_'.";
        })
        active)
      ++ (lib.mapAttrsToList (name: instance: {
          assertion = !hasConfig instance || instance.profile == null;
          message = "dotlink ${name}: profile cannot be combined with managed settings/configFile; use a separate instance instead.";
        })
        active)
      ++ (lib.mapAttrsToList (name: instance: {
          assertion =
            instance.settings
            == null
            || !(instance.settings ? version)
            || instance.settings.version == 10;
          message = "dotlink ${name}: declarative settings must use current schema version 10.";
        })
        active)
      ++ (lib.mapAttrsToList (name: instance: {
          assertion = instance.sshAgentSocket == null || lib.hasPrefix "/" instance.sshAgentSocket;
          message = "dotlink ${name}: sshAgentSocket must be an absolute Unix socket path.";
        })
        active)
      ++ (lib.mapAttrsToList (name: instance: {
          assertion = builtins.length (lib.unique instance.hostAccess.gpu) == builtins.length instance.hostAccess.gpu;
          message = "dotlink ${name}: hostAccess.gpu must not contain duplicate vendors.";
        })
        active)
      ++ (lib.mapAttrsToList (name: instance: {
          assertion = lib.all (key: lib.elem key cacheKeys) (builtins.attrNames instance.cacheAccess);
          message = "dotlink ${name}: cacheAccess contains an unsupported cache name.";
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
