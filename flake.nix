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
        nativeBuildInputs = with pkgs; [
          glib
          polkit
          pkg-config
        ];
      in
      {
        packages.default = naersk-lib.buildPackage {
          inherit nativeBuildInputs;
          src = ./.;
        };
        devShells.default = with pkgs; mkShell {
          inherit nativeBuildInputs;
          buildInputs = [
            cargo
            rustc
            rustfmt
            pre-commit
            rustPackages.clippy
          ];
          RUST_SRC_PATH = rustPlatform.rustLibSrc;
        };
      });
}
