# SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Build the Engram manual (Standard §8): one Texinfo source, three outputs.
# Run from this directory: `make info`, `make html`, `make pdf`, or `make all`.
# The toolchain is GNU Texinfo (texi2any/makeinfo, texi2pdf); provision it
# ephemerally if missing, e.g. `guix shell texinfo texlive` or
# `nix shell nixpkgs#texinfo nixpkgs#texliveSmall`.

TEXI     = engram.texi
INFO     = engram.info
HTML     = engram.html
PDF      = engram.pdf

TEXI2ANY ?= texi2any
MAKEINFO ?= makeinfo
TEXI2PDF ?= texi2pdf

.PHONY: all info html pdf clean

all: info html pdf

info: $(INFO)

$(INFO): $(TEXI)
	$(TEXI2ANY) --no-split $(TEXI) -o $(INFO)

html: $(HTML)

$(HTML): $(TEXI)
	$(MAKEINFO) --html --no-split $(TEXI) -o $(HTML)

pdf: $(PDF)

$(PDF): $(TEXI)
	$(TEXI2PDF) --quiet --build=tidy --texinfo='@afourpaper' $(TEXI)

clean:
	rm -f $(INFO) $(HTML) $(PDF)
	rm -rf engram.t2d engram.t2p
	rm -f engram.aux engram.cp engram.cps engram.fn engram.fns engram.ky \
	      engram.kys engram.log engram.pg engram.pgs engram.toc engram.tp \
	      engram.tps engram.vr engram.vrs
