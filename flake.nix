# SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Nix flake for Engram --- shared verbatim chat memory for multi-model LLM
# pipelines.
#
# Usage:
#   nix run .                          # run engram without installing it
#   nix run . -- --db ~/store.db mcp   # the MCP server, over stdio
#   nix build                          # build; result/bin/engram
#   nix profile install .              # install for the current user
#   nix develop                        # development shell
#   nix develop -c cargo test
#   nix flake check                    # build + test suite
#
# The package is defined once, in packaging/default.nix (Standard section 5.5);
# this flake passes `srcOverride = self` so it builds the checkout rather than a
# tagged tarball. That matters today: 0.6.0 is unreleased, so the
# fetchFromGitHub path in packaging/default.nix cannot resolve.
#
# There is deliberately NO nixosModule here. Engram is entirely $HOME state ---
# its SQLite store, its ~/.claude/commands/ output, its MCP server spawned by a
# user's harness. A `programs.engram.enable` whose only body is
# environment.systemPackages would advertise a system scope the tool does not
# have. Consumers install packages.default into home.packages instead.
{
  description = "Engram --- shared verbatim chat memory for multi-model LLM pipelines (MCP + HTTP, SQLite FTS5)";

  inputs = {
    nixpkgs.url     = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        engram = pkgs.callPackage ./packaging/default.nix {
          srcOverride = self;
          # Not a release: mark it so `engram --version` never claims to be a
          # tagged 0.6.0 when it is really whatever is checked out.
          version = "0.6.0-dev";
        };

        common = with pkgs; [
          # The Rust toolchain from nixpkgs rather than through rustup, which
          # would download a second complete toolchain (~1.5 GB) beside the one
          # nixpkgs already has. This crate pins no toolchain (no
          # rust-toolchain.toml, no rust-version), so nothing is lost.
          rustc
          cargo
          clippy
          rustfmt
          rust-analyzer

          gcc             # rusqlite's `bundled` feature compiles vendored SQLite.
          pkg-config
          mold            # Standard section 3.2.1: the linker LTO wants on NixOS.

          sqlite          # `sqlite3 store.db` when inspecting the store by hand.

          reuse           # section 4.3 --- `reuse lint` must pass.
          texinfo         # section 8 --- make -C doc info, make -C doc html.

          git
          gnumake
        ];
      in
      {
        packages.default = engram;
        packages.engram = engram;

        apps.default = {
          type = "app";
          program = "${engram}/bin/engram";
        };

        # `nix flake check` builds the package, which runs `cargo test` as part
        # of buildRustPackage's check phase.
        checks.default = engram;

        devShells.default = pkgs.mkShell {
          name = "engram-dev";
          nativeBuildInputs = common;
          shellHook = ''
            echo "engram dev shell."
            echo "  cargo test                                   tests"
            echo "  cargo clippy --all-targets -- -D warnings"
            echo "  cargo fmt --all -- --check                   formatting"
            echo "  reuse lint                                   section 4.3 gate"
            echo "  make -C doc info                             Texinfo manual"
            echo "  nix develop .#docs                           + texi2pdf"
          '';
        };

        # `make -C doc pdf` only. Standard section 8 wants all three formats,
        # but only at release time --- TeX Live is hundreds of megabytes that
        # someone editing the store layer never touches.
        devShells.docs = pkgs.mkShell {
          name = "engram-docs";
          nativeBuildInputs = common ++ [ pkgs.texliveSmall ];
        };
      }
    );
}
