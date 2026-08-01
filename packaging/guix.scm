;; SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
;; SPDX-License-Identifier: GPL-3.0-or-later
;;
;; GNU Guix package definition for engram (Steelbore Standard §5.5).
;;
;; Build:  guix build -f packaging/guix.scm
;;
;; 0.2.0-dev is unreleased, so the buildable path below fetches the git
;; tree.  The hashes are placeholders and MUST be replaced before this
;; definition can actually build — Guix refuses a wrong hash, it never
;; ignores one.

(use-modules (guix packages)
             (guix download)
             (guix git-download)
             (guix build-system cargo)
             ((guix licenses) #:prefix license:))

(define-public engram
  (package
    (name "engram")
    (version "0.2.0")
    ;; Release stanza — swap in at the first tagged release:
    ;;
    ;; (source
    ;;  (origin
    ;;    (method url-fetch)
    ;;    (uri (string-append
    ;;          "https://github.com/Spacecraft-Software/Engram"
    ;;          "/archive/refs/tags/v" version ".tar.gz"))
    ;;    (sha256
    ;;     (base32
    ;;      ;; TODO at release tag: set real hash
    ;;      ;; (guix download <tarball-url> prints it)
    ;;      "0000000000000000000000000000000000000000000000000000"))))
    ;;
    ;; Buildable path while 0.2.0-dev is unreleased: git checkout of main.
    (source
     (origin
       (method git-fetch)
       (uri (git-reference
             (url "https://github.com/Spacecraft-Software/Engram")
             ;; TODO: pin a commit hash; "main" is a moving target and is
             ;; only acceptable while the project is pre-release.
             (commit "main")))
       (file-name (git-file-name name version))
       (sha256
        (base32
         ;; PLACEHOLDER — not a real hash.  Compute with:
         ;;   guix hash --serializer=nar -x <checkout>
         "0000000000000000000000000000000000000000000000000000"))))
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
