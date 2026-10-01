{
  description = "dotlink — permission-scoped local MCP bridge";

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
      version = "0.6.0";

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
            pname = "dotlink";
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

      mkTarget =
        {
          pkgs,
          target,
          rustFlags ? null,
          nativeBuildInputs ? [ ],
          extraArgs ? { },
        }:
        let
          craneLib = mkCraneLib pkgs target;
          src = craneLib.cleanCargoSource ./.;

          commonArgs = {
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
        in
        mkTarget {
          inherit pkgs target rustFlags;
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
          dotlink = native.package;

          # Exposed intentionally so CI/cache infrastructure can build/cache
          # the dependency layer independently from the application source.
          deps = native.cargoArtifacts;
        }
        // nixpkgs.lib.optionalAttrs (system == "x86_64-linux") (
          let
            # Portable static Linux binary. It runs on Debian without requiring
            # Nix or a particular host glibc.
            linuxX86_64 = mkCross {
              localSystem = system;
              crossSystem = {
                config = "x86_64-unknown-linux-musl";
                libc = "musl";
              };
              target = "x86_64-unknown-linux-musl";
              rustFlags = "-C target-feature=+crt-static";
            };

            linuxAarch64 = mkCross {
              localSystem = system;
              crossSystem = {
                config = "aarch64-unknown-linux-musl";
                libc = "musl";
              };
              target = "aarch64-unknown-linux-musl";
              rustFlags = "-C target-feature=+crt-static";
            };

            # Native Windows x86_64 binary using the GNU/MSVCRT toolchain.
            windowsX86_64 = mkCross {
              localSystem = system;
              crossSystem = {
                config = "x86_64-w64-mingw32";
                libc = "msvcrt";
              };
              target = "x86_64-pc-windows-gnu";
            };

            # Windows ARM64 has no Rust GNU target. nixpkgs ships a pinned
            # LLVM-MinGW/UCRT toolchain, so use Rust's gnullvm target.
            llvmMingwBase = pkgs.callPackage (nixpkgs + "/pkgs/applications/emulators/wine/llvm-mingw.nix") { };
            llvmMingw = llvmMingwBase.overrideAttrs (old: {
              buildInputs = (old.buildInputs or [ ]) ++ [
                pkgs.zstd
                pkgs.libxml2_13
                pkgs.xz
                pkgs.ncurses
              ];
            });
            windowsAarch64 = mkTarget {
              inherit pkgs;
              target = "aarch64-pc-windows-gnullvm";
              nativeBuildInputs = [ llvmMingw ];
              extraArgs = {
                CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER = "${llvmMingw}/bin/aarch64-w64-mingw32-clang";
                CC_aarch64_pc_windows_gnullvm = "${llvmMingw}/bin/aarch64-w64-mingw32-clang";
                CXX_aarch64_pc_windows_gnullvm = "${llvmMingw}/bin/aarch64-w64-mingw32-clang++";
                AR_aarch64_pc_windows_gnullvm = "${llvmMingw}/bin/aarch64-w64-mingw32-ar";
                RANLIB_aarch64_pc_windows_gnullvm = "${llvmMingw}/bin/aarch64-w64-mingw32-ranlib";
              };
            };

            # Build macOS ARM64 directly from Linux with the pinned Apple SDK
            # and LLVM's Mach-O linker. This avoids Nixpkgs' Darwin xcbuild
            # bootstrap while keeping the full toolchain reproducible in Nix.
            fetchMacosSdk = pkgs.callPackage (nixpkgs + "/pkgs/by-name/ap/apple-sdk/common/fetch-sdk.nix") { };
            macosSdk = fetchMacosSdk {
              urls = [
                "https://swcdn.apple.com/content/downloads/14/48/052-59890-A_I0F5YGAY0Y/p9n40hio7892gou31o1v031ng6fnm9sb3c/CLTools_macOSNMOS_SDK.pkg"
                "https://web.archive.org/web/20250211001355/https://swcdn.apple.com/content/downloads/14/48/052-59890-A_I0F5YGAY0Y/p9n40hio7892gou31o1v031ng6fnm9sb3c/CLTools_macOSNMOS_SDK.pkg"
              ];
              version = "14.4";
              hash = "sha256-QozDiwY0Czc0g45vPD7G4v4Ra+3DujCJbSads3fJjjM=";
            };

            macosCc = pkgs.writeShellScript "dotlink-aarch64-apple-darwin-cc" ''
              export PATH="${pkgs.lib.makeBinPath [ pkgs.llvmPackages.lld ]}:$PATH"
              exec ${pkgs.llvmPackages.clang-unwrapped}/bin/clang                 --target=aarch64-apple-darwin                 -isysroot ${macosSdk}                 -mmacosx-version-min=11.0                 -fuse-ld=lld                 "$@"
            '';

            macosCxx = pkgs.writeShellScript "dotlink-aarch64-apple-darwin-cxx" ''
              export PATH="${pkgs.lib.makeBinPath [ pkgs.llvmPackages.lld ]}:$PATH"
              exec ${pkgs.llvmPackages.clang-unwrapped}/bin/clang++                 --target=aarch64-apple-darwin                 -isysroot ${macosSdk}                 -mmacosx-version-min=11.0                 -fuse-ld=lld                 "$@"
            '';

            macosAarch64 = mkTarget {
              inherit pkgs;
              target = "aarch64-apple-darwin";
              nativeBuildInputs = [
                pkgs.llvmPackages.clang-unwrapped
                pkgs.llvmPackages.lld
                pkgs.llvmPackages.llvm
              ];
              extraArgs = {
                CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER = macosCc;
                CC_aarch64_apple_darwin = macosCc;
                CXX_aarch64_apple_darwin = macosCxx;
                AR_aarch64_apple_darwin = "${pkgs.llvmPackages.llvm}/bin/llvm-ar";
                RANLIB_aarch64_apple_darwin = "${pkgs.llvmPackages.llvm}/bin/llvm-ranlib";
                MACOSX_DEPLOYMENT_TARGET = "11.0";
                SDKROOT = macosSdk;
              };
            };

            linuxX86_64Dist = mkDist pkgs {
              package = linuxX86_64.package;
              sourceName = "dotlink";
              assetName = "dotlink-linux-x86_64";
            };

            linuxAarch64Dist = mkDist pkgs {
              package = linuxAarch64.package;
              sourceName = "dotlink";
              assetName = "dotlink-linux-aarch64";
            };

            windowsX86_64Dist = mkDist pkgs {
              package = windowsX86_64.package;
              sourceName = "dotlink.exe";
              assetName = "dotlink-windows-x86_64.exe";
            };

            windowsAarch64Dist = mkDist pkgs {
              package = windowsAarch64.package;
              sourceName = "dotlink.exe";
              assetName = "dotlink-windows-aarch64.exe";
            };

            macosAarch64Dist = mkDist pkgs {
              package = macosAarch64.package;
              sourceName = "dotlink";
              assetName = "dotlink-macos-aarch64";
            };

            releaseAll =
              pkgs.runCommand "dotlink-release-all"
                {
                  nativeBuildInputs = [ pkgs.coreutils ];
                }
                ''
                  mkdir -p "$out"
                  cp "${linuxX86_64Dist}"/dotlink-linux-x86_64* "$out/"
                  cp "${linuxAarch64Dist}"/dotlink-linux-aarch64* "$out/"
                  cp "${windowsX86_64Dist}"/dotlink-windows-x86_64.exe* "$out/"
                  cp "${windowsAarch64Dist}"/dotlink-windows-aarch64.exe* "$out/"
                  cp "${macosAarch64Dist}"/dotlink-macos-aarch64* "$out/"
                  printf '%s\n' '${version}' > "$out/VERSION"
                  cat > "$out/PLATFORMS.txt" <<'EOF'
                  dotlink-linux-x86_64: Linux x86_64 (static musl)
                  dotlink-linux-aarch64: Linux ARM64 (static musl)
                  dotlink-windows-x86_64.exe: Windows x86_64
                  dotlink-windows-aarch64.exe: Windows ARM64
                  dotlink-macos-aarch64: macOS ARM64 (Apple Silicon)
                  EOF
                '';
          in
          {
            cross-linux-x86_64-deps = linuxX86_64.cargoArtifacts;
            cross-linux-x86_64 = linuxX86_64.package;
            dist-linux-x86_64 = linuxX86_64Dist;

            cross-linux-aarch64-deps = linuxAarch64.cargoArtifacts;
            cross-linux-aarch64 = linuxAarch64.package;
            dist-linux-aarch64 = linuxAarch64Dist;

            cross-windows-x86_64-deps = windowsX86_64.cargoArtifacts;
            cross-windows-x86_64 = windowsX86_64.package;
            dist-windows-x86_64 = windowsX86_64Dist;

            cross-windows-aarch64-deps = windowsAarch64.cargoArtifacts;
            cross-windows-aarch64 = windowsAarch64.package;
            dist-windows-aarch64 = windowsAarch64Dist;

            cross-macos-aarch64-deps = macosAarch64.cargoArtifacts;
            cross-macos-aarch64 = macosAarch64.package;
            dist-macos-aarch64 = macosAarch64Dist;

            release-all = releaseAll;
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
            program = "${native.package}/bin/dotlink";
            meta = {
              description = "Permission-scoped local MCP bridge";
              homepage = "https://github.com/abird-ai/dotlink";
              license = native.pkgs.lib.licenses.mit;
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
