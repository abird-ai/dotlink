{
  nixpkgs,
  overlay,
  sourceRoot,
  version,
  mkCraneLib,
}: let
  mkTarget = {
    pkgs,
    target,
    rustFlags ? null,
    nativeBuildInputs ? [],
    extraArgs ? {},
  }: let
    craneLib = mkCraneLib pkgs target;
    src = craneLib.cleanCargoSource sourceRoot;

    commonArgs =
      {
        inherit src version;
        pname = "dotlink";
        strictDeps = true;
        doCheck = false;
        cargoExtraArgs = "--locked --all-features";
        CARGO_BUILD_TARGET = target;
      }
      // nixpkgs.lib.optionalAttrs (rustFlags != null) {
        CARGO_BUILD_RUSTFLAGS = rustFlags;
      }
      // extraArgs;

    cargoArtifacts = craneLib.buildDepsOnly commonArgs;

    package = craneLib.buildPackage (
      commonArgs
      // {
        inherit cargoArtifacts nativeBuildInputs;
        doCheck = false;
        meta = {
          description = "Permission-scoped local MCP bridge";
          homepage = "https://github.com/abird-ai/dotlink";
          license = pkgs.lib.licenses.mit;
        };
      }
    );
  in {
    inherit
      pkgs
      craneLib
      src
      commonArgs
      cargoArtifacts
      package
      ;
  };

  mkCross = {
    localSystem,
    crossSystem,
    target,
    rustFlags ? null,
  }: let
    pkgs = import nixpkgs {
      inherit localSystem crossSystem;
      overlays = [overlay];
    };
  in
    mkTarget {
      inherit pkgs target rustFlags;
    };
in {
  inherit mkTarget mkCross;
}
