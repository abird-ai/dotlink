{
  pkgs,
  sourceRoot,
  version,
  mkCraneLib,
}: let
  inherit (pkgs) lib;
  craneLib = mkCraneLib pkgs null;
  src = craneLib.cleanCargoSource sourceRoot;

  commonArgs = {
    inherit src version;
    pname = "dotlink";
    strictDeps = true;
    cargoExtraArgs = "--locked --all-features";
  };

  # Production packages compile the exact Bubblewrap dependency into launcher
  # trust. Keep that runtime authority out of hermetic test/clippy derivations:
  # a Nix builder is itself sandboxed and cannot promise nested Bubblewrap
  # execution merely because the package's trusted launcher exists in the store.
  packageArgs =
    commonArgs
    // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
      DOTLINK_BUILD_BWRAP = "${pkgs.bubblewrap}/bin/bwrap";
    };

  # Build dependency artifacts separately so source-only changes do not
  # invalidate the dependency graph.
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;

  package = craneLib.buildPackage (
    packageArgs
    // {
      inherit cargoArtifacts;
      doCheck = false;

      nativeBuildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
        pkgs.makeWrapper
      ];

      postInstall = lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
        wrapProgram $out/bin/dotlink \
          --prefix PATH : ${
          lib.makeBinPath [
            pkgs.bash
            pkgs.bubblewrap
          ]
        }
      '';

      meta = {
        description = "Permission-scoped local MCP bridge";
        homepage = "https://github.com/abird-ai/dotlink";
        license = lib.licenses.mit;
        mainProgram = "dotlink";
      };
    }
  );

  tests = craneLib.cargoTest (
    commonArgs
    // {
      inherit cargoArtifacts;
      # Nix builders intentionally do not provide a normal writable HOME.
      # Runtime/job tests exercise XDG-backed private state, so give the test
      # process an explicit hermetic home rather than weakening production
      # ownership/permission checks for /homeless-shelter.
      preCheck = ''
        export HOME="$TMPDIR/home"
        export XDG_CONFIG_HOME="$HOME/.config"
        export XDG_STATE_HOME="$HOME/.local/state"
        export XDG_CACHE_HOME="$HOME/.cache"
        mkdir -p "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"
      '';
    }
  );

  clippy = craneLib.cargoClippy (
    commonArgs
    // {
      inherit cargoArtifacts;
      cargoClippyExtraArgs = "--all-targets -- --deny warnings";
    }
  );

  fmt = craneLib.cargoFmt {
    src = craneLib.path sourceRoot;
  };
in {
  inherit pkgs craneLib src commonArgs cargoArtifacts package tests clippy fmt;
}
