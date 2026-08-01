# SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Nix package for engram (Steelbore Standard §5.5), callPackage-style.
#
# Build now, from the local working tree:
#   nix-build -E 'with import <nixpkgs> {}; callPackage ./packaging/default.nix {}'

{ lib
, rustPlatform
  # , fetchFromGitHub  # uncomment at release (see the src comment below)
}:

rustPlatform.buildRustPackage {
  pname = "engram";
  version = "0.2.0";

  # Local source: 0.2.0-dev is unreleased, so build the repository checkout
  # this file lives in.  The filter keeps build artifacts, the local
  # database, and .git out of the store copy (leaner and deterministic).
  src = builtins.path {
    path = ../.;
    name = "engram-src";
    filter = path: type:
      let base = baseNameOf path;
      in !(builtins.elem base [ ".git" "target" ])
         && builtins.match "engram\\.db.*" base == null;
  };

  # At release, swap `src` for the tagged GitHub fetch:
  #
  # src = fetchFromGitHub {
  #   owner = "Spacecraft-Software";
  #   repo = "Engram";
  #   rev = "v0.2.0";                # release tag
  #   hash = lib.fakeHash;           # TODO at release: replace with the real
  #                                  # sha256-... hash (build once; Nix
  #                                  # reports the got: hash to paste here)
  # };

  cargoLock = {
    lockFile = ../Cargo.lock;
  };

  meta = with lib; {
    description = "Shared verbatim chat memory for multi-model LLM pipelines";
    homepage = "https://Engram.SpacecraftSoftware.org/";
    license = licenses.gpl3Plus;
    maintainers = [ ];  # TODO: nixpkgs maintainer entry (Mohamed Hammad)
    mainProgram = "engram";
  };
}
