{
  description = "knot development flake";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        devShells.default = pkgs.mkShell {
          packages =  with pkgs; [
            # General
            curl
            gnumake
            jq
            shellcheck
            yq-go

            # Rust
            rustToolchain
            cargo-nextest
            cargo-watch
            cargo-deny
            sqlx-cli
            mold

            # Frontend
            nodejs_24
            pnpm

            # Browser / e2e
            chromium

            # K8s tooling (for later plans)
            kind
            kubectl
            kubernetes-helm
          ];

          env.PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
          env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH = "${pkgs.chromium}/bin/chromium";
        };
      }
    );
}
