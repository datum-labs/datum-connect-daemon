{
  description = "Datum Connect plugin — Rust binary (datum-connect) + Go plugin (datumctl-connect)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        # Pinned to match the rust-toolchain.toml in connect-lib/ (if any) or
        # the latest stable on nixpkgs unstable. The plugin Rust binary only
        # builds for native host targets — no WASM, no cross-compile from this
        # shell (release builds happen via scripts/release.sh which sets its
        # own targets).
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
        };

        # Native build inputs (tools needed at build time).
        nativeBuildInputs = with pkgs; [
          pkg-config
        ];

        # Build inputs (libraries linked against).
        # openssl is needed by openssl-sys (transitive via reqwest in
        # connect-lib). libiconv is required on darwin.
        buildInputs = with pkgs; [
          openssl
        ] ++ lib.optionals stdenv.isDarwin [
          libiconv
        ];

      in
      {
        # ── Packaged Rust binary ──────────────────────────────────────────
        # `nix build` produces the datum-connect Rust binary used by the
        # Go plugin as a subprocess in plugin mode.
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "datum-connect";
          version = "0.1.0";
          src = ./connect-lib;

          cargoLock = {
            lockFile = ./connect-lib/Cargo.lock;
            # iroh-proxy-utils is a git dependency; its hash is required for
            # reproducible builds. Update via `nix build` failure → copy the
            # expected hash into this map.
            outputHashes = {
              "iroh-proxy-utils-0.1.0" = "sha256-ZV71q22zCWBqFdrc0jzkwyQdVc/H0r0BBB6dKrNARr8=";
            };
          };

          inherit nativeBuildInputs buildInputs;

          cargoBuildFlags = [ "-p" "datum-connect" ];
          # Workspace tests require network (iroh STUN/relay); run locally
          # via `task test:rust` in the dev shell.
          doCheck = false;

          meta = with pkgs.lib; {
            description = "Datum Connect tunnel agent (plugin-mode Rust binary)";
            homepage = "https://github.com/datum-cloud/datumctl-plugins";
            license = licenses.agpl3Only;
            mainProgram = "datum-connect";
          };
        };

        # ── Development shell ─────────────────────────────────────────────
        # Use via `nix develop` from this directory (or `nix develop
        # path:./connect` from the workspace root). Provides Rust + Go +
        # task + pkg-config + openssl, with PKG_CONFIG_PATH set so
        # openssl-sys finds its lib via pkg-config out of the box.
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            # Rust toolchain (stable, with clippy + rustfmt + rust-analyzer)
            rustToolchain

            # Go toolchain for the datumctl-connect plugin shell.
            # Pinned to match the directive in go.mod (currently 1.25.x);
            # nixpkgs unstable's `go` is the active stable Go release.
            go

            # Task runner — Taskfile.yaml at connect root is the canonical
            # entry point for build / test / install workflows.
            go-task

            # Common build tools
            pkg-config
            openssl

            # Useful dev utilities
            git
          ] ++ lib.optionals stdenv.isDarwin [ libiconv ];

          # openssl-sys reads OpenSSL paths from pkg-config. nixpkgs splits
          # openssl into `out` (libs) and `dev` (headers + .pc files); the
          # latter is what pkg-config needs to be pointed at.
          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";

          # rust-analyzer wants this to navigate to std/core sources.
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";

          shellHook = "";
        };

        # nix fmt
        formatter = pkgs.nixpkgs-fmt;
      });
}
