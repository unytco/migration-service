{
  description = "Flake for the unyt migration notary daemon — static-musl cross-build toolchain for deploy";

  inputs = {
    holonix.url = "github:holochain/holonix?ref=main-0.6";

    nixpkgs.follows = "holonix/nixpkgs";
    flake-parts.follows = "holonix/flake-parts";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs@{ nixpkgs, flake-parts, rust-overlay, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = builtins.attrNames inputs.holonix.devShells;
      perSystem = { inputs', pkgs, system, ... }: {
        _module.args.pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        formatter = pkgs.nixpkgs-fmt;

        # Build-only dev shell: the Rust toolchain plus the musl cross-compiler
        # and the C build deps the daemon's native crates need (ring, the
        # holochain client / lair stack). No Holochain runtime binaries — the
        # daemon's own `cargo test` mocks the conductor, so this shell exists to
        # produce the static-musl deploy binary, not to run a conductor.
        devShells.default = pkgs.mkShell {
          packages = (with pkgs; [
            (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
            perl
            pkg-config
            openssl
            llvmPackages_18.libunwind
            pkgsCross.musl64.stdenv.cc
          ]);

          shellHook = ''
            export PS1='\[\033[1;35m\][migration-notary:\w]\$\[\033[0m\] '
            export LIBCLANG_PATH="${pkgs.llvmPackages_18.libclang.lib}/lib"

            # Cross-compile to x86_64-unknown-linux-musl so the daemon is a
            # fully-static ELF that can be scp'd to the non-Nix notary droplets
            # without a /nix/store dependency for the dynamic loader (the same
            # status=203/EXEC trap the watchtower observer build avoids).
            export CC_x86_64_unknown_linux_musl="${pkgs.pkgsCross.musl64.stdenv.cc}/bin/x86_64-unknown-linux-musl-cc"
            export AR_x86_64_unknown_linux_musl="${pkgs.pkgsCross.musl64.stdenv.cc.bintools.bintools}/bin/x86_64-unknown-linux-musl-ar"
            export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$CC_x86_64_unknown_linux_musl"
            export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=+crt-static -C link-self-contained=yes"
          '';
        };
      };
    };
}
