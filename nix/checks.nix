{
  native,
  sourceRoot,
  module,
}: let
  inherit (native) pkgs;
  inherit (pkgs) lib;
  lintSource = extension:
    lib.cleanSourceWith {
      src = sourceRoot;
      filter = path: type:
        if type == "directory"
        then !(lib.elem (baseNameOf path) [".git" ".kilo" "target" "node_modules" ".direnv" "dist"])
        else
          lib.hasSuffix ".${extension}" path
          || (extension == "md" && baseNameOf path == ".markdownlint-cli2.jsonc");
    };

  markdownlint =
    pkgs.runCommand "dotlink-markdownlint" {
      nativeBuildInputs = [pkgs.markdownlint-cli2];
    } ''
      cd ${lintSource "md"}
      markdownlint-cli2
      touch "$out"
    '';

  alejandra =
    pkgs.runCommand "dotlink-alejandra" {
      nativeBuildInputs = [pkgs.alejandra];
    } ''
      alejandra --check ${lintSource "nix"}
      touch "$out"
    '';
in
  {
    inherit (native) package tests clippy fmt;
    inherit markdownlint alejandra;
  }
  // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
    nixos-module = import ./tests/module.nix {inherit pkgs module;};
  }
