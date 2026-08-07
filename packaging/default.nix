# SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Nix derivation for Engram (Standard section 5.5).
#
# Two ways in:
#
#   nix build                          # via flake.nix, builds this checkout
#   nix-build packaging/default.nix    # release build from the tagged tarball
#
# The flake passes `srcOverride = self`, skipping the fetchFromGitHub path.
# That is what makes a from-checkout build work before a release is tagged --
# and 0.6.0 is not tagged, so today the fetch path cannot resolve at all.
#
# Dependencies are taken from the committed Cargo.lock rather than a vendor
# hash, so there is no `cargoHash` to regenerate on every dependency bump.

{ lib
, rustPlatform
, fetchFromGitHub
, texinfo
  # Override to build a working tree (the flake does). When null, the tagged
  # release tarball is fetched instead.
  #
  # Deliberately not named `src`. callPackage fills any unbound argument from
  # nixpkgs, and nixpkgs *has* a `src` attribute, so a `src ? null` default is
  # silently overridden and `callPackage ./packaging/default.nix { }` fails
  # with a baffling error about that unrelated package. `version` has no such
  # clash.
, srcOverride ? null
, version ? "0.6.0"
}:

rustPlatform.buildRustPackage {
  pname = "engram";
  inherit version;

  # Under `srcOverride = self` this is the git tree, so .gitignore does the
  # source filtering for free: *.db (the local store and its -wal/-shm
  # siblings), /target, chat/ and research/ never reach the store. That is why
  # there is no hand-rolled `builtins.path` filter here any more -- the old one
  # excluded .git/target/engram.db but silently missed chat/ and research/.
  src =
    if srcOverride != null then
      srcOverride
    else
      fetchFromGitHub {
        owner = "Spacecraft-Software";
        repo = "Engram";
        rev = "v${version}";
        # TODO at release: build once and paste the reported sha256-... here.
        # Unreachable until a v-tag exists; the flake never takes this branch.
        hash = lib.fakeHash;
      };

  # The lockfile is committed, so Nix can vendor from it directly. `outputHashes`
  # stays empty because every dependency comes from crates.io -- no git deps.
  cargoLock = {
    lockFile = ../Cargo.lock;
  };

  # rusqlite is built with its `bundled` feature, which compiles vendored
  # SQLite from C. buildRustPackage's stdenv already provides the toolchain --
  # this note exists so nobody "cleans up" by adding or removing inputs for it.

  nativeBuildInputs = [ texinfo ];

  # Standard section 8: the Texinfo manual ships with the package. It is
  # generated, not committed, so it is built here. Engram's manual Makefile
  # lives in doc/, not at the repository root.
  postBuild = ''
    make -C doc info
  '';

  postInstall = ''
    install -Dm644 doc/engram.info "$out/share/info/engram.info"
    install -Dm644 README.md   "$out/share/doc/engram/README.md"
    install -Dm644 NOTICE.md   "$out/share/doc/engram/NOTICE.md"
  '';

  meta = {
    description = "Shared verbatim chat memory for multi-model LLM pipelines";
    homepage = "https://Engram.SpacecraftSoftware.org/";
    license = lib.licenses.gpl3Plus;
    mainProgram = "engram";
    platforms = lib.platforms.unix;
  };
}
