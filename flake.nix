{
  description = "mdr — A lightweight Markdown viewer with Mermaid diagram support";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        inherit (pkgs) lib;
      in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "mdr";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = with pkgs; [
            pkg-config
          ] ++ lib.optionals stdenv.isLinux [
            wrapGAppsHook3
          ];

          buildInputs = with pkgs; lib.optionals stdenv.isLinux [
            gtk3
            webkitgtk_4_1
            xdotool
            libGL
          ] ++ lib.optionals stdenv.isDarwin [
            darwin.apple_sdk.frameworks.WebKit
            darwin.apple_sdk.frameworks.AppKit
            darwin.apple_sdk.frameworks.CoreServices
          ];

          meta = with lib; {
            description = "A lightweight Markdown viewer with Mermaid diagram support";
            homepage = "https://github.com/CleverCloud/mdr";
            license = licenses.mit;
            maintainers = [];
            mainProgram = "mdr";
          };
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.default ];
          packages = with pkgs; [
            rust-analyzer
            clippy
            rustfmt
          ];
        };
      });
}
