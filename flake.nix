{
  description = "A basic flake with a shell";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.systems.url = "github:nix-systems/default";
  inputs.flake-utils = {
    url = "github:numtide/flake-utils";
    inputs.systems.follows = "systems";
  };
  inputs.intent-system-flake.url = "github:turtton/intent-system-flake";

  outputs =
    { nixpkgs, flake-utils, intent-system-flake, ... }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        intent-system = intent-system-flake.packages."${system}".intent-cli;
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.bashInteractive
            pkgs.rustc
            pkgs.cargo
            pkgs.rustfmt
            pkgs.clippy
            pkgs.rust-analyzer
            intent-system
            # GUI (evorch-gui / winit+wgpu) が dev shell から起動できるようにする動的ライブラリ群
            pkgs.pkg-config
            pkgs.wayland
            pkgs.wayland-protocols
            pkgs.libxkbcommon
            pkgs.vulkan-loader
            pkgs.mesa # lavapipe (ソフトウェアレンダリング fallback 用)
          ];
          shellHook = ''
            export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath [
              pkgs.wayland
              pkgs.libxkbcommon
              pkgs.vulkan-loader
              pkgs.mesa
            ]}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
          '';
        };
      }
    );
}
