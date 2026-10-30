;; SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
;; SPDX-License-Identifier: GPL-3.0-or-later
;;
;; GNU Guix package definition for engram (Steelbore Standard §5.5).
;;
;; Build:  guix build -f packaging/guix.scm
;;
;; Builds the tagged v0.6.0 release tarball.  Bump `version' and the
;; sha256 together at each release; `guix download <tarball-url>' prints
;; the new hash.

(use-modules (guix packages)
             (guix download)
             (guix build-system cargo)
             ((guix licenses) #:prefix license:))

(define-public engram
  (package
    (name "engram")
    (version "0.6.0")
    (source
     (origin
       (method url-fetch)
       (uri (string-append
             "https://github.com/Spacecraft-Software/Engram"
             "/archive/refs/tags/v" version ".tar.gz"))
       (file-name (string-append name "-" version ".tar.gz"))
       (sha256
        (base32
         "05a9l39ln1qpdmzfq4p3m71bbcwp4bybr730ahfmm626v1v5bizx"))))
    (build-system cargo-build-system)
    (arguments
     (list #:install-source? #f))
    (synopsis "Shared verbatim chat memory for multi-model LLM pipelines")
    (description
     "Engram is a shared verbatim chat memory store for multi-model LLM
pipelines: a single SQLite file (with FTS5 full-text search) that multiple
models and agents read from and write to, so context survives across
pipeline stages without any LLM calls to encode or retrieve it.  It exposes
the same store over three surfaces: a dual-mode self-documenting CLI, an
MCP stdio server, and a local-only HTTP API.")
    (home-page "https://Engram.SpacecraftSoftware.org/")
    (license license:gpl3+)))

;; Return the package so `guix build -f packaging/guix.scm` works.
engram
