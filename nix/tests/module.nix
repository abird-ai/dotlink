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

  managed = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      settings = {
        tunnel_id = "tunnel_fixture";
        transports.openai = true;
        permissions.default_allow = true;
      };
      credentials.runtimeApiKeyFile = "/run/secrets/dotlink-key";
      credentials.ngrokAuthTokenFile = "/run/secrets/ngrok";
      logging.verbosity = "tools";
      serialize = true;
      hostAccess = {
        nixDaemon = true;
        nixConfig = true;
        sshAgent = true;
        gitConfig = true;
        kvm = true;
        gpu = ["amd" "nvidia"];
      };
      sshAgentSocket = "/run/user/1000/gcr/ssh";
      cacheAccess = {
        "cargo-registry" = "ro";
        "go-mod" = "rw";
      };
      instances.work = {
        workingDirectory = "/home/alice/work";
        profile = "work";
        logging.verbosity = "requests";
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
      hostAccess.sshAgent = true;
    };
  };
  invalidCache = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
      workingDirectory = "/home/alice/projects";
      cacheAccess."unsupported-kind" = "ro";
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
  invalidWorkingDirectory = eval {
    services.dotlink = {
      enable = true;
      user = "alice";
    };
  };

  managedUnitText = managed.config.systemd.units."dotlink.service".text;

  valid = cfg:
    lib.all
    (a: a.assertion || !lib.hasInfix "dotlink" a.message)
    cfg.config.assertions;

  dotlinkWarnings = cfg: lib.filter (lib.hasInfix "services.dotlink") cfg.config.warnings;

  hasSupervisionPolicy = service:
    (service.startLimitIntervalSec == 0)
    && service.serviceConfig.Restart == "always"
    && service.serviceConfig.RestartPreventExitStatus == "2 CONFIG"
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
    (!(normal.config.systemd.services.dotlink.environment ? DOTLINK_CONFIG))
    (!(normal.config.systemd.services.dotlink.environment ? DOTLINK_API_KEY_FILE))
    (lib.hasInfix "--no-stdio --color=never" normal.config.systemd.services.dotlink.serviceConfig.ExecStart)
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
    (managed.config.systemd.services.dotlink.environment ? DOTLINK_CONFIG)
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
    (lib.hasInfix "--allow-nix-daemon" managed.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (lib.hasInfix "--allow-nix-config" managed.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (lib.hasInfix "--allow-git-config" managed.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (lib.hasInfix "--allow-ssh-agent" managed.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (lib.hasInfix "--allow-kvm" managed.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (lib.hasInfix "--allow-gpu=amd" managed.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (lib.hasInfix "--allow-gpu=nvidia" managed.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (managed.config.systemd.services.dotlink.serviceConfig.SupplementaryGroups == ["kvm" "render" "video"])
    (lib.hasInfix "--allow-cache-cargo-registry=ro" managed.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (lib.hasInfix "--allow-cache-go-mod=rw" managed.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (managed.config.systemd.services.dotlink.environment.DOTLINK_SSH_AGENT_SOCKET == "/run/user/1000/gcr/ssh")
    (lib.elem pkgs.nix managed.config.systemd.services.dotlink.path)
    (lib.elem pkgs.git managed.config.systemd.services.dotlink.path)
    (lib.elem pkgs.openssh managed.config.systemd.services.dotlink.path)
    (lib.hasInfix "--serialize" managed.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (lib.hasSuffix " -v" managed.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (lib.hasInfix "--profile=work" managed.config.systemd.services.dotlink-work.serviceConfig.ExecStart)
    (lib.hasSuffix " -vv" managed.config.systemd.services.dotlink-work.serviceConfig.ExecStart)
    (!(managed.config.systemd.services.dotlink-work.environment ? DOTLINK_CONFIG))
    (hasSupervisionPolicy managed.config.systemd.services.dotlink-work)
    (lib.hasInfix "--profile=test" managed.config.systemd.services.dotlink-test.serviceConfig.ExecStart)
    (managed.config.systemd.services.dotlink-test.environment ? DOTLINK_CONFIG)
    (!(disabled.config.systemd.services ? dotlink))
    (!(disabled.config.systemd.services ? dotlink-work))
    (valid dedicated)
    (dedicated.config.users.users.dotlink.home == "/var/lib/dotlink")
    (dedicated.config.systemd.services.dotlink.serviceConfig.User == "dotlink")
    (!valid invalid)
    (!valid invalidSecret)
    (valid autoDetectedGcrAgent)
    (lib.hasInfix "--allow-ssh-agent" autoDetectedGcrAgent.config.systemd.services.dotlink.serviceConfig.ExecStart)
    (!(autoDetectedGcrAgent.config.systemd.services.dotlink.environment ? DOTLINK_SSH_AGENT_SOCKET))
    (!(autoDetectedGcrAgent.config.systemd.services.dotlink.environment ? SSH_AUTH_SOCK))
    (!valid invalidCache)
    (!valid invalidWorkingDirectory)
  ];
in
  assert lib.all (x: x) checks;
    pkgs.runCommand "dotlink-nixos-module-eval" {} "touch $out"
