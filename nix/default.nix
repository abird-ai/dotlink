{
  inputs,
  sourceRoot,
}: let
  inherit (inputs) self nixpkgs crane rust-overlay;
  version = (builtins.fromTOML (builtins.readFile (sourceRoot + "/Cargo.toml"))).package.version;
  systems = ["x86_64-linux" "aarch64-linux" "aarch64-darwin"];
  forAllSystems = f: nixpkgs.lib.genAttrs systems f;
  overlay = import rust-overlay;

  mkCraneLib = pkgs: target:
    (crane.mkLib pkgs).overrideToolchain (p: let
      base = p.rust-bin.fromRustupToolchainFile (sourceRoot + "/rust-toolchain.toml");
    in
      if target == null
      then base
      else base.override {targets = [target];});

  native = forAllSystems (system:
    import ./native.nix {
      pkgs = import nixpkgs {
        inherit system;
        overlays = [overlay];
      };
      inherit sourceRoot version mkCraneLib;
    });
  cross = import ./cross.nix {inherit nixpkgs overlay sourceRoot version mkCraneLib;};
  development = forAllSystems (system: import ./dev.nix {native = native.${system};});
in {
  nixosModules = {
    default = import ./module.nix {inherit self;};
    dotlink = self.nixosModules.default;
  };
  packages = forAllSystems (system:
    {
      default = native.${system}.package;
      dotlink = native.${system}.package;
      deps = native.${system}.cargoArtifacts;
    }
    // nixpkgs.lib.optionalAttrs (system == "x86_64-linux") (import ./release.nix {
      inherit system nixpkgs version;
      inherit (cross) mkTarget mkCross;
      inherit (native.${system}) pkgs;
    }));
  apps = forAllSystems (system: {
    default = {
      type = "app";
      program = "${native.${system}.package}/bin/dotlink";
      meta = {
        description = "Permission-scoped local MCP bridge";
        homepage = "https://github.com/abird-ai/dotlink";
        license = native.${system}.pkgs.lib.licenses.mit;
      };
    };
  });
  checks = forAllSystems (system:
    import ./checks.nix {
      inherit sourceRoot;
      native = native.${system};
      module = self.nixosModules.default;
    });
  devShells = forAllSystems (system: {default = development.${system}.shell;});
  formatter = forAllSystems (system: development.${system}.formatter);
}
