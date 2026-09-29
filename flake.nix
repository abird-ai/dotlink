{
  description = "abird-tunnel — native Rust Secure MCP Tunnel bridge";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
            }
          )
        );
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              let
                name = baseNameOf path;
              in
              name != "target" && name != ".git";
          };

          abird-tunnel = pkgs.rustPlatform.buildRustPackage {
            pname = "abird-tunnel";
            version = "0.3.0";
            inherit src;

            cargoLock.lockFile = ./Cargo.lock;
            strictDeps = true;
            nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.makeWrapper ];

            # Package builds also run the Rust unit test suite.
            doCheck = true;
            checkPhase = ''
              runHook preCheck
              cargo test --all-features
              runHook postCheck
            '';

            postInstall = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              wrapProgram $out/bin/abird-tunnel                 --prefix PATH : ${
                pkgs.lib.makeBinPath [
                  pkgs.bash
                  pkgs.bubblewrap
                ]
              }
            '';

            meta = {
              description = "Native Rust Secure MCP Tunnel bridge for local workspaces";
              license = pkgs.lib.licenses.mit;
              mainProgram = "abird-tunnel";
              platforms = pkgs.lib.platforms.unix;
            };
          };
        in
        {
          default = abird-tunnel;
          inherit abird-tunnel;
        }
      );

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/abird-tunnel";
        };
      });

      checks = forAllSystems (
        pkgs:
        let
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          src = package.src;
        in
        {
          # Builds the release package and runs cargo test.
          package-and-tests = package;

          rustfmt =
            pkgs.runCommand "abird-tunnel-rustfmt"
              {
                nativeBuildInputs = [
                  pkgs.cargo
                  pkgs.rustfmt
                ];
              }
              ''
                cd ${src}
                cargo fmt --check
                touch $out
              '';

          clippy = pkgs.rustPlatform.buildRustPackage {
            pname = "abird-tunnel-clippy";
            version = "0.3.0";
            inherit src;

            cargoLock.lockFile = ./Cargo.lock;
            strictDeps = true;
            nativeBuildInputs = [ pkgs.clippy ];
            doCheck = false;

            buildPhase = ''
              runHook preBuild
              cargo clippy --all-targets --all-features -- -D warnings
              runHook postBuild
            '';

            installPhase = ''
              mkdir -p $out
              touch $out/passed
            '';
          };
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.nixfmt
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            pkgs.bubblewrap
          ];
          RUST_BACKTRACE = "1";
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
