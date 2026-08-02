<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Retrieval benchmark dataset

Held-out evaluation set for engram's M3 hybrid-retrieval work: FTS5 BM25 +
static-embedding vectors fused via reciprocal rank fusion (RRF), measured
against the FTS5-only baseline.

## The gate

**Hybrid retrieval must beat FTS5-only on recall@5 by >= 5 points on these
queries, or hybrid does not ship.**

This dataset was committed **before any vector code existed** and is
**never edited once M3 starts**. Do not add, remove, reword, re-scope, or
re-label a single corpus document or query to make a number move — that is
tuning on the test set, and it silently converts the gate into a rubber
stamp. Retrieval parameters (RRF `k`, candidate depths, column weights) may
be tuned only on separately generated dev data, never on these files. If
the dataset itself is found to be *defective* (a malformed line, a
relevant_key typo), fix it before M3 begins or document the defect and
score around it — after M3 starts, the files are frozen.

## Files and format

Both data files are JSONL: one JSON object per line, UTF-8, no comments
(license: covered by the repository-level `REUSE.toml` `**` annotation,
GPL-3.0-or-later).

### `corpus.jsonl` — 230 documents

| field | type | values |
|---|---|---|
| `key` | string | stable slug, unique across the corpus |
| `agent` | string | `claude-code`, `codex`, `kimi`, `gemini-cli`, `human` |
| `scope` | string | one of 7 scopes (below) |
| `role` | string | `note`, `assistant`, `user`, `system` |
| `content` | string | 20–800 chars of memory text |

**Keys, not UUIDs.** `key` is a human-stable identifier; the eval runner
maps keys to fresh UUIDs at ingest time and keeps the mapping in memory
(see the `engram-uuid-keys` decision in the corpus itself). Slugs never
enter the store, and this file stays diffable.

Scopes (documents per scope): `engram` (39), `ironway-netcode` (37),
`zamak-bootloader` (36), `bravais-flake` (34), `caliper-tracing` (32),
`anvil-ssh` (30), `craton-kernel` (22).

The corpus is deliberately adversarial for lexical search: shared
vocabulary (`timeout`, `linker`, `cache`, `handshake`, `buffer`, `retry`,
`hash`, `OOM`) recurs across scopes with different meanings, and several
near-topic clusters (stripped symbols, keepalive settings, WAL behavior,
Nagle/TCP_NODELAY, TSC timekeeping) contain multiple plausible documents of
which only one answers a given query.

### `queries.jsonl` — 65 queries

| field | type | values |
|---|---|---|
| `query` | string | engineer-mid-session phrasing |
| `scope` | string or `null` | restricts retrieval to one scope; `null` = all |
| `relevant_keys` | array of strings | corpus keys judged relevant (1–3 here) |
| `kind` | string | taxonomy below |

A retrieved document counts as a hit iff its key is in `relevant_keys`.
recall@5 per query = |relevant retrieved in top 5| / |relevant_keys|;
report the mean over all queries, plus per-kind means.

## Kind taxonomy (actual counts)

| kind | count | share | expectation |
|---|---|---|---|
| `exact-term` | 16 | 24.6% | query shares distinctive tokens with the target — FTS5 should ace these; hybrid must not regress them |
| `paraphrase` | 23 | 35.4% | same fact, different wording (doc: "the linker discarded `.boot_magic`" vs query: "flashed image lost its boot signature") — embeddings should win |
| `synonym` | 10 | 15.4% | jargon swap ("OOM" vs "ran out of memory", "MITM" vs "man-in-the-middle", "TOCTOU" vs "check-then-use race") |
| `conceptual` | 10 | 15.4% | no direct lexical bridge; requires topical understanding |
| `multi-fact` | 6 | 9.2% | 2–3 `relevant_keys`, often spanning scopes |
| total | 65 | 100% | |

## Running

Planned runner (M3):

```sh
cargo run --example retrieval_eval -- \
    --corpus bench/corpus.jsonl \
    --queries bench/queries.jsonl \
    --mode fts,hybrid \
    --k 5
```

The runner ingests the corpus into a throwaway database (mapping keys to
UUIDs), executes every query under each mode, and prints overall and
per-kind recall@5 for both, plus the delta the gate is judged on.

## Validation

```sh
python3 bench/validate.py
```

Checks: every line parses; required fields present and typed; corpus keys
unique; every `relevant_key` resolves; kinds within the taxonomy; content
lengths in range. Exit 0 and per-kind counts when clean. Run it in CI and
before any commit that touches this directory (which, after M3 starts,
should be none).
