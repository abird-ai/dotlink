# Lightweight NixOS module evaluation checks. These exercise generated units
# without needing a running systemd instance or a full VM.
{
  pkgs,
  module,
}: let
  lib = pkgs.lib;
  eval = value:
    import (pkgs.path + "/nixos/lib/eval-config.nix") {
      inherit pkgs;
      system = pkgs.stdenv.hostPlatform.system;
      modules = [
        module
        {system.stateVersion = "25.11";}
        value
      ];
    };

  normal = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
    };
  };

  managedSettings = {
    tunnel_id = "tunnel_fixture";
    transports.openai = true;
    permissions = {
      default_allow = true;
      allow_shell = true;
      allow_nix_daemon = true;
      allow_nix_config = true;
      allow_ssh_agent = true;
      allow_git_config = true;
      allow_kvm = true;
      allow_gpu = ["amd" "nvidia"];
    };
    caches = [
      {
        kind = "cargo-registry";
        mode = "ro";
      }
      {
        kind = "go-mod";
        mode = "rw";
      }
    ];
    mounts = [
      {
        source = "/home/alice/.cache/abird";
        target = "/tmp/home/.cache/abird";
        mode = "rw";
      }
    ];
    logging.verbosity = "tools";
    serialize = true;
  };

  managed = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      settings = managedSettings;
      credentials.runtimeApiKeyFile = "/run/secrets/dotlink-key";
      credentials.ngrokAuthTokenFile = "/run/secrets/ngrok";
      sshAgentSocket = "/run/user/1000/gcr/ssh";
      instances.work = {
        workingDirectory = "/home/alice/work";
      };
      instances.test = {
        workingDirectory = "/home/alice/extra";
        settings = {
          transports.openai = false;
          transports.http = true;
          permissions.default_allow = true;
        };
      };
    };
  };

  disabled = eval {
    services.dotlink = {
      enable = false;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      instances.work.profile = "work";
    };
  };

  dedicated = eval {
    services.dotlink = {
      enable = true;
      workingDirectory = "/var/lib/dotlink/work";
    };
  };

  invalid = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      settings.transports.openai = false;
      configFile = "/etc/dotlink.jsonc";
    };
  };
  invalidSecret = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      settings = {
        transports.openai = true;
        runtime_api_key = "plaintext-test";
      };
    };
  };
  autoDetectedGcrAgent = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      settings.permissions.allow_ssh_agent = true;
    };
  };
  stdioEnabled = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      settings.transports.stdio = true;
    };
  };
  atSystemdVersion = version:
    eval {
      systemd.package = pkgs.systemd // {inherit version;};
      services.dotlink = {
        enable = true;
        user = "alice";
        workingDirectory = "/home/alice/projects";
      };
    };
  systemd254 = atSystemdVersion "254.17";
  systemd255 = atSystemdVersion "255.13";
  systemd257 = atSystemdVersion "257.9";
  systemd258 = atSystemdVersion "258.1";
  profileBacked = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      profile = "work";
      extraGroups = ["kvm" "render" "video"];
      extraPackages = [pkgs.git];
    };
  };
  duplicateSlot = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      instances.work = {
        profile = "default";
        workingDirectory = "/home/alice/work";
      };
    };
  };
  duplicateBind = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      settings.transports = {
        openai = false;
        http = true;
      };
      instances.work.settings.transports = {
        openai = false;
        http = true;
      };
    };
  };
  duplicateTunnel = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      settings = {
        transports.openai = true;
        tunnel_id = "tunnel_shared";
      };
      instances.work.settings = {
        transports.openai = true;
        tunnel_id = "tunnel_shared";
      };
    };
  };
  storeConfig = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      # A store-backed explicit file cannot be the active config either.
      configFile = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dotlink/config.jsonc";
    };
  };
  mutableConfig = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      configFile = "/home/alice/etc/dotlink.jsonc";
    };
  };
  invalidWorkingDirectory = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
    };
  };
  # The module writes these keys from typed options; a second spelling in
  # `environment` is refused rather than silently overridden.
  # Every owned key is refused, so dropping one from the list cannot pass.
  reservedKeysRefused =
    lib.all
    (key:
      !(valid (eval {
        services.dotlink = {
          enable = true;
          user = "alice";
          workingDirectory = "/home/alice/projects";
          environment = {${key} = "x";};
        };
      })))
    [
      "DOTLINK_PROFILE"
      "DOTLINK_CONFIG_FILE"
      "DOTLINK_API_KEY_FILE"
      "NGROK_AUTHTOKEN_FILE"
      "DOTLINK_STDIO"
      "DOTLINK_COLOR"
      "DOTLINK_SUPERVISOR"
      "DOTLINK_SSH_AGENT_SOCKET"
    ];
  reservedEnvironment = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      environment.DOTLINK_PROFILE = "work";
    };
  };
  reservedInstanceEnvironment = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      instances.work = {
        workingDirectory = "/home/alice/work";
        environment.DOTLINK_API_KEY_FILE = "/run/secrets/key";
      };
    };
  };
  # Other environment keys stay available: only what the module owns is closed.
  supportedEnvironment = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      environment = {
        DOTLINK_VERBOSITY = "tools";
        RUST_LOG = "warn";
      };
    };
  };

  managedUnitText = managed.config.systemd.units."dotlink.service".text;
  # The same writeText name and content hash to the same store path, so this is
  # exactly the source the unit stages; reading it needs no internal derivation.
  managedSource = pkgs.writeText "dotlink-default.jsonc" (builtins.toJSON (managedSettings // {version = 13;}));
  managedProfile = builtins.fromJSON (builtins.readFile managedSource);

  valid = cfg:
    lib.all
    (a: a.assertion || !lib.hasInfix "dotlink" a.message)
    cfg.config.assertions;

  dotlinkWarnings = cfg: lib.filter (lib.hasInfix "services.dotlink") cfg.config.warnings;

  hasSupervisionPolicy = service:
    (service.startLimitIntervalSec == 0)
    && service.serviceConfig.Restart == "always"
    && service.serviceConfig.RestartPreventExitStatus == "2 CONFIG NOTCONFIGURED"
    && service.serviceConfig.SuccessExitStatus == "NOTCONFIGURED"
    && service.serviceConfig.RestartSec == "2s"
    && service.serviceConfig.RestartSteps == 8
    && service.serviceConfig.RestartMaxDelaySec == "10min"
    && service.serviceConfig.NotifyAccess == "main"
    && service.serviceConfig.TimeoutStopSec == "15s"
    && service.serviceConfig.KillMode == "control-group"
    && service.serviceConfig.KillSignal == "SIGTERM"
    && !(service.serviceConfig ? OOMPolicy)
    && service.environment.DOTLINK_SUPERVISOR == "systemd";

  checks = [
    (valid normal)
    (normal.config.systemd.services.dotlink.serviceConfig.User == "alice")
    (normal.config.systemd.services.dotlink.serviceConfig.WorkingDirectory == "/home/alice/projects")
    (!(normal.config.systemd.services.dotlink.environment ? DOTLINK_CONFIG_FILE))
    (!(normal.config.systemd.services.dotlink.environment ? DOTLINK_API_KEY_FILE))
    (!(lib.hasInfix "--" normal.config.systemd.services.dotlink.serviceConfig.ExecStart))
    (normal.config.systemd.services.dotlink.environment.DOTLINK_COLOR == "never")
    (!lib.hasInfix "--serialize" normal.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (hasSupervisionPolicy normal.config.systemd.services.dotlink)
    (dotlinkWarnings normal == [])

    (!valid systemd254)
    (lib.any (
        a:
          !a.assertion
          && lib.hasInfix "requires systemd 255 or newer" a.message
      )
      systemd254.config.assertions)
    (dotlinkWarnings systemd254 == [])
    (valid systemd255)
    (lib.any (lib.hasInfix "RESTART_RESET") (dotlinkWarnings systemd255))
    (valid systemd257)
    (lib.any (lib.hasInfix "RESTART_RESET") (dotlinkWarnings systemd257))
    (valid systemd258)
    (dotlinkWarnings systemd258 == [])

    (valid managed)
    (managed.config.systemd.services.dotlink.environment.DOTLINK_CONFIG_FILE == "/run/dotlink/config.jsonc")
    (managed.config.systemd.services.dotlink.serviceConfig.RuntimeDirectory == "dotlink")
    (managed.config.systemd.services.dotlink.serviceConfig.RuntimeDirectoryMode == "0700")
    (lib.hasInfix "-/nix/store/" managed.config.systemd.services.dotlink.serviceConfig.ExecStartPre)
    (lib.hasInfix "install -m 0400" managed.config.systemd.services.dotlink.serviceConfig.ExecStartPre)
    (lib.hasInfix
      (builtins.unsafeDiscardStringContext (toString managedSource))
      (builtins.unsafeDiscardStringContext managed.config.systemd.services.dotlink.serviceConfig.ExecStartPre))
    (!(managed.config.environment.etc ? "dotlink/config.jsonc"))
    (normal.config.systemd.services.dotlink.environment.DOTLINK_STDIO == "0")
    (managed.config.systemd.services.dotlink-test.serviceConfig.RuntimeDirectory == "dotlink-test")
    (managed.config.systemd.services.dotlink-test.environment.DOTLINK_CONFIG_FILE == "/run/dotlink-test/config.jsonc")
    (managed.config.systemd.services.dotlink-test.environment.DOTLINK_PROFILE == "test")
    (!(managed.config.systemd.services.dotlink-work.serviceConfig ? RuntimeDirectory))
    (managed.config.systemd.services.dotlink.environment.DOTLINK_API_KEY_FILE == "%d/dotlink-api-key")
    (managed.config.systemd.services.dotlink.environment.NGROK_AUTHTOKEN_FILE == "%d/ngrok-authtoken")
    (managed.config.systemd.services.dotlink.serviceConfig.LoadCredential
      == [
        "dotlink-api-key:/run/secrets/dotlink-key"
        "ngrok-authtoken:/run/secrets/ngrok"
      ])
    (lib.hasInfix "DOTLINK_API_KEY_FILE=%d/dotlink-api-key" managedUnitText)
    (lib.hasInfix "LoadCredential=dotlink-api-key:/run/secrets/dotlink-key" managedUnitText)
    (lib.hasInfix "StartLimitIntervalSec=0" managedUnitText)
    (lib.hasInfix "Restart=always" managedUnitText)
    (lib.hasInfix "RestartPreventExitStatus=2 CONFIG" managedUnitText)
    (lib.hasInfix "RestartSec=2s" managedUnitText)
    (lib.hasInfix "RestartSteps=8" managedUnitText)
    (lib.hasInfix "RestartMaxDelaySec=10min" managedUnitText)
    (lib.hasInfix "SetLoginEnvironment=true" managedUnitText)
    (managed.config.systemd.services.dotlink.serviceConfig.SupplementaryGroups == ["kvm" "render" "video"])
    # Policy lives in the config file the module renders; only deployment facts
    # stay in the environment.
    (lib.any (cache: cache.kind == "cargo-registry" && cache.mode == "ro") managedProfile.caches)
    (lib.any (cache: cache.kind == "go-mod" && cache.mode == "rw") managedProfile.caches)
    (lib.any (mount: mount.target == "/tmp/home/.cache/abird" && mount.mode == "rw") managedProfile.mounts)
    (!(managed.config.systemd.services.dotlink.environment ? DOTLINK_ALLOW_KVM))
    (!(managed.config.systemd.services.dotlink.environment ? DOTLINK_SERIALIZE))
    (managed.config.systemd.services.dotlink.environment.DOTLINK_STDIO == "0")
    (!(managed.config.systemd.services.dotlink.environment ? DOTLINK_VERBOSITY))
    (managed.config.systemd.services.dotlink.environment.DOTLINK_COLOR == "never")
    managedProfile.permissions.allow_kvm
    managedProfile.permissions.allow_nix_daemon
    managedProfile.permissions.allow_ssh_agent
    managedProfile.permissions.allow_shell
    (managedProfile.permissions.allow_gpu == ["amd" "nvidia"])
    managedProfile.serialize
    (!(managedProfile.transports.stdio or false))
    (managedProfile.logging.verbosity == "tools")
    (!(managed.config.systemd.services.dotlink.environment ? DOTLINK_PROFILE))
    (managed.config.systemd.services.dotlink-work.environment.DOTLINK_PROFILE == "work")
    (managed.config.systemd.services.dotlink.environment.DOTLINK_SSH_AGENT_SOCKET == "/run/user/1000/gcr/ssh")
    (lib.elem pkgs.nix managed.config.systemd.services.dotlink.path)
    (lib.elem pkgs.git managed.config.systemd.services.dotlink.path)
    (lib.elem pkgs.openssh managed.config.systemd.services.dotlink.path)
    (!(lib.hasInfix " -v" managed.config.systemd.services.dotlink.serviceConfig.ExecStart))
    (!(managed.config.systemd.services.dotlink-work.environment ? DOTLINK_CONFIG_FILE))
    (hasSupervisionPolicy managed.config.systemd.services.dotlink-work)
    (managed.config.systemd.services.dotlink-test.environment ? DOTLINK_CONFIG_FILE)
    (!(disabled.config.systemd.services ? dotlink))
    (!(disabled.config.systemd.services ? dotlink-work))
    (valid dedicated)
    (dedicated.config.users.users.dotlink.home == "/var/lib/dotlink")
    (dedicated.config.systemd.services.dotlink.serviceConfig.User == "dotlink")
    (!valid invalid)
    (!valid invalidSecret)
    (valid autoDetectedGcrAgent)
    (autoDetectedGcrAgent.config.systemd.services.dotlink.serviceConfig.RuntimeDirectory == "dotlink")
    # Mode B: policy and provisioning come from the selected user's profile plus
    # explicit deployment options, because the module cannot read that profile.
    (valid profileBacked)
    (profileBacked.config.systemd.services.dotlink.environment.DOTLINK_PROFILE == "work")
    (!(profileBacked.config.systemd.services.dotlink.environment ? DOTLINK_CONFIG_FILE))
    (!(profileBacked.config.systemd.services.dotlink.serviceConfig ? RuntimeDirectory))
    (profileBacked.config.systemd.services.dotlink.serviceConfig.SupplementaryGroups == ["kvm" "render" "video"])
    (lib.elem pkgs.git profileBacked.config.systemd.services.dotlink.path)
    (!(lib.elem pkgs.nix profileBacked.config.systemd.services.dotlink.path))
    # A store-backed explicit file is staged too; an operator-owned path is used
    # as written, so a hand-edited config needs no restart-time copy.
    (storeConfig.config.systemd.services.dotlink.serviceConfig.RuntimeDirectory == "dotlink")
    (storeConfig.config.systemd.services.dotlink.environment.DOTLINK_CONFIG_FILE == "/run/dotlink/config.jsonc")
    (!(mutableConfig.config.systemd.services.dotlink.serviceConfig ? RuntimeDirectory))
    (mutableConfig.config.systemd.services.dotlink.environment.DOTLINK_CONFIG_FILE == "/home/alice/etc/dotlink.jsonc")
    (!valid duplicateSlot)
    (!valid duplicateBind)
    (!valid duplicateTunnel)
    (!(autoDetectedGcrAgent.config.systemd.services.dotlink.environment ? DOTLINK_SSH_AGENT_SOCKET))
    (!(autoDetectedGcrAgent.config.systemd.services.dotlink.environment ? SSH_AUTH_SOCK))
    (!valid stdioEnabled)
    (!valid invalidWorkingDirectory)
    (!valid reservedEnvironment)
    (!valid reservedInstanceEnvironment)
    reservedKeysRefused
    (valid supportedEnvironment)
    (supportedEnvironment.config.systemd.services.dotlink.environment.DOTLINK_VERBOSITY == "tools")
    (supportedEnvironment.config.systemd.services.dotlink.environment.RUST_LOG == "warn")
  ];
in
  assert lib.all (x: x) checks;
    pkgs.runCommand "dotlink-nixos-module-eval" {} "touch $out"
