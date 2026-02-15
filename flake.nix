{
  description = "hywoma - Hyprland workspace manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, crane }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        craneLib = crane.mkLib pkgs;

        src = craneLib.cleanCargoSource ./.;

        commonArgs = {
          inherit src;
          pname = "hywoma";
          version = "0.1.0";
          cargoLock = ./Cargo.lock;
        };

        hywoma = craneLib.buildPackage (commonArgs // {
          cargoBuildCommand = "cargo build --release";
          doCheck = false;
        });
      in
      {
        packages.default = hywoma;
        apps.default = flake-utils.lib.mkApp { drv = hywoma; };
      });
}
