{
  description = "abird-link — permission-scoped local MCP bridge";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    crane.url = "github:ipetkov/crane/v0.24.0";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
      ...
    }:
    let
      version = "0.5.0";

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      forAllSystems = f: nixpkgs.lib.genAttrs systems f;

      overlay = import rust-overlay;

      mkCraneLib =
        pkgs: target:
        (crane.mkLib pkgs).overrideToolchain (
          p:
          let
            base = p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          in
          if target == null then base else base.override { targets = [ target ]; }
        );

      mkNative =
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ overlay ];
          };
          inherit (pkgs) lib;

          craneLib = mkCraneLib pkgs null;
          src = craneLib.cleanCargoSource ./.;

          commonArgs = {
            inherit src version;
            pname = "abird-link";
            strictDeps = true;
            cargoExtraArgs = "--locked --all-features";
          };

          # Build dependency artifacts separately so source-only changes do not
          # invalidate the dependency graph.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          package = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              doCheck = false;

              nativeBuildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
                pkgs.makeWrapper
              ];

              postInstall = lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
                wrapProgram $out/bin/abird-link \
                  --prefix PATH : ${
                    lib.makeBinPath [
                      pkgs.bash
                      pkgs.bubblewrap
                    ]
                  }
              '';
            }
          );

          tests = craneLib.cargoTest (
            commonArgs
            // {
              inherit cargoArtifacts;
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
            src = craneLib.path ./.;
          };
        in
        {
          inherit
            pkgs
            craneLib
            src
            commonArgs
            cargoArtifacts
            package
            tests
            clippy
            fmt
            ;
        };

      mkCross =
        {
          localSystem,
          crossSystem,
          target,
          rustFlags ? null,
        }:
        let
          pkgs = import nixpkgs {
            inherit localSystem crossSystem;
            overlays = [ overlay ];
          };

          craneLib = mkCraneLib pkgs target;
          src = craneLib.cleanCargoSource ./.;

          commonArgs =
            {
              inherit src version;
              pname = "abird-link";
              strictDeps = true;
              doCheck = false;
              cargoExtraArgs = "--locked --all-features";
              CARGO_BUILD_TARGET = target;
            }
            // nixpkgs.lib.optionalAttrs (rustFlags != null) {
              CARGO_BUILD_RUSTFLAGS = rustFlags;
            };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          package = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              doCheck = false;
            }
          );
        in
        {
          inherit
            pkgs
            craneLib
            src
            commonArgs
            cargoArtifacts
            package
            ;
        };

      mkDist =
        pkgs:
        {
          package,
          sourceName,
          assetName,
        }:
        pkgs.runCommand "${assetName}-dist"
          {
            nativeBuildInputs = [ pkgs.coreutils ];
          }
          ''
            mkdir -p "$out"
            cp "${package}/bin/${sourceName}" "$out/${assetName}"
            chmod 0755 "$out/${assetName}"
            (
              cd "$out"
              sha256sum "${assetName}" > "${assetName}.sha256"
            )
            printf '%s\n' '${version}' > "$out/VERSION"
          '';
    in
    {
      packages = forAllSystems (
        system:
        let
          native = mkNative system;
          inherit (native) pkgs;
        in
        {
          default = native.package;
          abird-link = native.package;

          # Exposed intentionally so CI/cache infrastructure can build/cache
          # the dependency layer independently from the application source.
          deps = native.cargoArtifacts;
        }
        // nixpkgs.lib.optionalAttrs (system == "x86_64-linux") (
          let
            # Portable static Linux binary. It runs on Debian without requiring
            # Nix or a particular host glibc.
            linux = mkCross {
              localSystem = system;
              crossSystem = {
                config = "x86_64-unknown-linux-musl";
                libc = "musl";
              };
              target = "x86_64-unknown-linux-musl";
              rustFlags = "-C target-feature=+crt-static";
            };

            # Native Windows x86_64 binary using the GNU/MSVCRT toolchain.
            windows = mkCross {
              localSystem = system;
              crossSystem = {
                config = "x86_64-w64-mingw32";
                libc = "msvcrt";
              };
              target = "x86_64-pc-windows-gnu";
            };

            linuxDist = mkDist pkgs {
              package = linux.package;
              sourceName = "abird-link";
              assetName = "abird-link-linux-x86_64";
            };

            windowsDist = mkDist pkgs {
              package = windows.package;
              sourceName = "abird-link.exe";
              assetName = "abird-link-windows-x86_64.exe";
            };
          in
          {
            cross-linux-x86_64-deps = linux.cargoArtifacts;
            cross-linux-x86_64 = linux.package;
            dist-linux-x86_64 = linuxDist;

            cross-windows-x86_64-deps = windows.cargoArtifacts;
            cross-windows-x86_64 = windows.package;
            dist-windows-x86_64 = windowsDist;
          }
        )
      );

      apps = forAllSystems (
        system:
        let
          native = mkNative system;
        in
        {
          default = {
            type = "app";
            program = "${native.package}/bin/abird-link";
            meta = {
              description = "Permission-scoped local MCP bridge";
            };
          };
        }
      );

      checks = forAllSystems (
        system:
        let
          native = mkNative system;
        in
        {
          package = native.package;
          tests = native.tests;
          clippy = native.clippy;
          fmt = native.fmt;
        }
      );

      devShells = forAllSystems (
        system:
        let
          native = mkNative system;
          inherit (native) pkgs;
        in
        {
          default = native.craneLib.devShell {
            checks = {
              inherit (native) tests clippy;
            };

            packages = [
              pkgs.nixfmt
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              pkgs.bubblewrap
            ];

            RUST_BACKTRACE = "1";
          };
        }
      );

      formatter = forAllSystems (system: (mkNative system).pkgs.nixfmt);
    };
}
