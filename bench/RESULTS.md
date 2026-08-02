<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
SPDX-License-Identifier: GPL-3.0-or-later
-->

# M3 retrieval benchmark — results

**Date:** 2026-08-02 · **Corpus:** `bench/corpus.jsonl` (230 docs, 7 scopes) ·
**Queries:** `bench/queries.jsonl` (65, held out — committed before any hybrid
code existed) · **Model:** `minishlab/potion-base-8M` (Model2Vec, MIT, 256-dim,
loaded from a local directory) · **Runner:** `bench/retrieval_eval.py`
driving the real `engram` binary black-box (debug build, `--features vector`;
metric values are build-profile-independent).

## Gate

> Hybrid must beat FTS5-only recall@5 by ≥ 5 points, else vectors ship
> compiled-off ("measured, declined" is a success outcome). Decided before
> implementation; queries frozen since they were committed.

**Verdict: PASS — hybrid − fts = +6.2 points (0.918 vs 0.856).**
The `vector` feature stays opt-in (`default = []` is unchanged; the gate
governs whether the feature ships at all, not whether it becomes default).

## Headline numbers

| mode | recall@5 | recall@10 | MRR |
|---|---|---|---|
| fts (OR-joined, shipped) | 0.856 | 0.908 | 0.809 |
| hybrid (fts + Model2Vec, RRF k=60) | **0.918** | **0.938** | **0.864** |

Per query kind (recall@5):

| kind | fts | hybrid |
|---|---|---|
| exact-term | 1.000 | 1.000 |
| paraphrase | 0.913 | 0.913 |
| multi-fact | 0.944 | 0.944 |
| conceptual | 0.600 | **0.900** |
| synonym | 0.700 | **0.800** |

Hybrid's entire margin comes from conceptual and synonym queries — precisely
the classes the research predicted embeddings would win (semantic similarity
without shared vocabulary). On everything else the OR-joined FTS5 baseline is
already at or near ceiling for this corpus.

## The baseline finding that mattered more than the gate

The first run measured the then-shipped FTS5 behavior — every query token
joined with FTS5's implicit `AND` — at **recall@5 = 0.108** (exact-term
0.438, everything else ≈ 0). Natural-language queries almost always contain
a filler word the stored text lacks, and one missing token zeroes an AND
match. Comparing hybrid against that baseline would have shown a
misleading **+77.9-point** "win" that was really a defect in the baseline —
the exact "which component is doing the work" trap the MemPalace case study
warns about (`research/research.md` §E).

The fix — join sanitized tokens with `OR` and let BM25 ranking reward
multi-token matches (`sanitize_fts_query`, `src/store.rs`) — took the
default build from 0.108 to 0.856 and ships to every user. The gate was
then re-measured against the fixed baseline and passed on the merits.

| baseline | recall@5 | hybrid margin |
|---|---|---|
| AND-joined (pre-fix) | 0.108 | +77.9 (misleading) |
| OR-joined (shipped) | 0.856 | **+6.2 (honest, PASS)** |

## Reproduction

```sh
cargo build --features vector
# install a Model2Vec model dir (model.safetensors + tokenizer.json +
# config.json) — engram never downloads one; the eval takes the path:
python3 bench/retrieval_eval.py --binary target/debug/engram --model <dir>
```

Frozen-query rule: `bench/queries.jsonl` is append-only history from here on;
editing existing queries after this measurement would invalidate the gate.
