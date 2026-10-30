{native}: let
  inherit (native) pkgs;
in {
  shell = native.craneLib.devShell {
    checks = {
      inherit (native) tests clippy;
    };

    packages =
      [
        pkgs.alejandra
        pkgs.markdownlint-cli2
      ]
      ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
        pkgs.bubblewrap
      ];

    RUST_BACKTRACE = "1";
    DOTLINK_BUILD_BWRAP = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux "${pkgs.bubblewrap}/bin/bwrap";
  };
  formatter = pkgs.writeShellApplication {
    name = "dotlink-fmt";
    runtimeInputs = [pkgs.alejandra];
    text = ''
      exec alejandra --exclude ./.git --exclude ./target --exclude ./.kilo \
        --exclude ./.direnv --exclude ./node_modules --exclude ./dist "$@"
    '';
  };
}
