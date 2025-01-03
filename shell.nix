{ pkgs ? import <nixpkgs> {} }:
  pkgs.mkShell {
    nativeBuildInputs = with pkgs.buildPackages; [
      cairo
      glib
      libxkbcommon
      pango
      pkg-config
    ];
}
