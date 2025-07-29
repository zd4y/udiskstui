{
  inputs = {
    naersk.url = "github:nix-community/naersk/master";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, utils, naersk }:
    utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        naersk-lib = pkgs.callPackage naersk { };
      in
      {
        packages.default = naersk-lib.buildPackage ./.;
        devShells.default = with pkgs; mkShell {
          buildInputs = [
            cargo
            rustc
            rustfmt
            pre-commit
            rustPackages.clippy

            glib
            polkit
            pkg-config
          ];
          RUST_SRC_PATH = rustPlatform.rustLibSrc;
        };
      });
}
