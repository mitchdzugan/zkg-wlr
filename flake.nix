{
  description = "(wayland roots) keyboard grabber and key press reporter";
  inputs = {
    nixpkgs.url = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };
  outputs = { self, nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in rec {
        packages.default = packages.zkg-wlr;
        packages.zkg-wlr = pkgs.rustPlatform.buildRustPackage rec {
          pname = "zkg-wlr";
          version = "0.0.1";
          src = ./.;
          cargoHash = "sha256-OusLfY7MxcPM19khHngpJKqENNdvFwP60VCs7eCtfkc=";
          buildInputs = with pkgs; [
            cairo
            glib
            libxkbcommon
            pango
          ];
          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
        };
      }
    );
}
