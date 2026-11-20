{
  description = "orrery development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      }));
    in
    {
      devShells = forAllSystems (pkgs:
        let
          # Same toolchain as rust-toolchain.toml, plus the release targets for local cross builds.
          rust = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
            targets = [
              "aarch64-unknown-linux-musl"
              "armv7-unknown-linux-musleabihf"
              "arm-unknown-linux-musleabihf"
              "x86_64-unknown-linux-musl"
            ];
          };
        in
        {
          default = pkgs.mkShell {
            packages = [
              rust
              pkgs.git
              pkgs.jujutsu
              pkgs.just
              pkgs.dbus # dbus-daemon, for the D-Bus client tests
              pkgs.qemu # run ARM release binaries locally
              # Only for the conformance reference tiers: upstream Moonraker and real Klipper.
              pkgs.uv
              pkgs.python3
            ];
          };
        });
    };
}
