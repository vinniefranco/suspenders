{
  description = "suspenders - a Rust project";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        # CRAP metric (cyclomatic complexity x uncovered code) per function;
        # not in nixpkgs yet, so built from crates.io. Reads the LCOV that
        # cargo-tarpaulin writes to target/tarpaulin (see .cargo-crap.toml).
        cargo-crap = rustPlatform.buildRustPackage rec {
          pname = "cargo-crap";
          version = "0.3.1";
          src = pkgs.fetchCrate {
            inherit pname version;
            hash = "sha256-3qvyS5+7kQgmfk8Sl+29VJq+u+aECoh6n9A/9i0fRyY=";
          };
          cargoHash = "sha256-wajI7ex7t8nOvMMBVL16LOzZJiwc0IGd6D+fYmTXXGo=";
          # upstream's own test suite is not our gate; keep the build lean
          doCheck = false;
        };
      in
      {
        packages.default = rustPlatform.buildRustPackage {
          pname = "suspenders";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
        };

        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.rust-analyzer
            pkgs.cargo-nextest
            pkgs.cargo-tarpaulin
            cargo-crap
          ];

          env.RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
        };
      }
    );
}
