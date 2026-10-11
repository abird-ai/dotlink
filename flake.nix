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

  outputs = inputs:
    import ./nix {
      inherit inputs;
      sourceRoot = ./.;
    };
}
