<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Changelog

All notable changes to Engram. Dates are ISO 8601 UTC. The format loosely
follows [Keep a Changelog](https://keepachangelog.com/); versions follow
[semver](https://semver.org/) (pre-1.0: minor bumps may break).

## [0.5.0] — 2026-08-02

The research-driven retrieval generation (`research/research.md`), spanning
the 0.3/0.4 development cycle in one release.

### Added
- **Hybrid retrieval** (`--features vector`, opt-in; default build unchanged):
  Model2Vec static embeddings loaded from a **local directory only** — the
  network fetch path is compiled out; engram never downloads a model.
  `memory_vectors` BLOB side table + brute-force cosine; `engram index`
  backfill; `search --mode fts|hybrid` with auto-hybrid and structured
  errors on missing prerequisites. Shipping was gated on a held-out
  benchmark: hybrid 0.918 vs fts 0.856 recall@5 (+6.2 ≥ +5 → PASS,
  `bench/RESULTS.md`).
- **Extracted-fact index** (L0↔L1): deterministic marker-based extraction
  (`deterministic-v1`, never an LLM), verbatim facts with drill-down
  pointers, third RRF channel in `context`/hybrid; `consolidate --extract`.
- **Consolidation & decay**: `consolidate --dedup [--yes]` (near-duplicate
  groups superseded — never deleted — newest wins), `consolidate --report`
  (report-only contradiction surfacing + staleness/decay list); access
  tracking (`last_accessed_at`/`access_count`, internal) with CLI
  `--no-track` opt-out.
- MCP tools `get` and `context` (ledger: 9 tools); HTTP `GET /v1/memory/:id`,
  `GET /v1/context`.
- Benchmark corpus/harness: `bench/` (230-doc corpus, 65 frozen held-out
  queries, black-box eval driver).

### Security
- `cargo audit` assessed: RUSTSEC-2026-0189 (rmcp 0.16 Streamable-HTTP DNS
  rebinding) does not apply — engram compiles only rmcp's stdio transport;
  suppressed with reasoning in `audit.toml`, to be dropped with the tracked
  rmcp ≥ 1.4 upgrade. Two unmaintained-crate warnings (`paste`,
  `number_prefix`) are transitive and advisory-only.

### Changed
- **FTS query semantics**: sanitized tokens are now joined with `OR` (BM25
  still ranks multi-token matches first). Measured on natural-language
  queries: recall@5 0.108 → 0.856. Result sets are looser than the old
  implicit-`AND` behavior; ranking and `--limit` preserve precision.
- The `memories_au` FTS trigger fires only on `content` updates, so access
  tracking and supersession no longer churn the FTS index.
- `consolidate` output is sectioned: `{extract?, dedup?, report?}`.

## [0.2.0] — 2026-08-01

MVP complete: compliance baseline, token budgeting, bi-temporal supersession.

### Added
- Test baseline across all surfaces (store/CLI/HTTP/MCP), CI (fmt, clippy
  `-D warnings`, feature-matrix tests, REUSE lint), packaging manifests
  (Guix/Nix/PKGBUILD), Texinfo manual, `CREDITS.md`.
- `--format json|jsonl|csv`, `--dry-run` on `remember`, `--accessible` +
  `SPACECRAFT_A11Y` (Standard §18), `[OK]`/`[ERROR]` status tags.
- Token-budgeted retrieval: `--budget-tokens` on `recall`/`search` (envelope
  `metadata.budget`, estimator `chars-div-4`), `engram context` session-start
  assembly (rules always included), RRF fusion core.
- Bi-temporal supersession: `valid_from`/`valid_to`/`superseded_by`,
  `remember --supersedes` (scope-local, conflict-safe), `--as-of` time
  travel, `--include-superseded`, `rule purge` (CLI-only tombstone delete).

### Changed
- **Breaking:** `/v1/memory*` HTTP routes return real status codes
  (400/404/409/500) instead of `200 OK` with an error body. Check the HTTP
  status; errors no longer arrive as 200.
- Corrected output-mode detection: `CI` truthiness, presence-based agent
  vars, `TERM=dumb`.

## [0.1.0] — 2026-07-25

Initial release: verbatim SQLite+FTS5 memory store; CLI/MCP/HTTP surfaces;
durable rules with sentinel-block sync to `AGENTS.md`/`CLAUDE.md`.
