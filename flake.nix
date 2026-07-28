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

        # rustqual - structural code-quality analyzer scoring seven dimensions
        # (IOSP, Complexity, DRY, SRP, Coupling, Test Quality, Architecture);
        # not in nixpkgs, so built from crates.io. Reads the LCOV that
        # cargo-tarpaulin writes to target/tarpaulin (see rustqual.toml and the
        # regression + floor gate in .github/workflows/ci.yml).
        rustqual = rustPlatform.buildRustPackage rec {
          pname = "rustqual";
          version = "1.8.2";
          src = pkgs.fetchCrate {
            inherit pname version;
            hash = "sha256-5IbBtRLUJr+PyYAKXh7HKIIa/LwyMW3S8IRusRdjYcs=";
          };
          cargoHash = "sha256-azFtI+tk89R61fEFXRlirchRsoqbqBoT3sNnkROU+Vs=";
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
            rustqual
          ];

          env.RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
        };
      }
    );
}
