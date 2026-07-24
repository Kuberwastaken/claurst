{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    cargo rustc rustfmt clippy
    gcc cmake pkg-config gnumake perl
    openssl.dev alsa-lib.dev
    clang libclang.lib libclang.dev
    python3
    libxkbcommon wayland
  ];

  LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
  PKG_CONFIG_PATH = "${pkgs.alsa-lib.dev}/lib/pkgconfig";
}
