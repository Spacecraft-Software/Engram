<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
SPDX-License-Identifier: GPL-3.0-or-later
-->

# CREDITS

Attribution per The Steelbore Standard §15.3: external work whose ideas or
artifacts engram substantially builds upon. This is distinct from the
mechanical SPDX/REUSE metadata (`REUSE.toml`, per-file headers), which covers
licensing; this file credits the people and projects behind the design.

Unless a row says otherwise, these are **research-derived design inputs**:
engram borrows the concept or pattern, not the code. No source code from the
projects below has been incorporated.

| Name | Author | License | Source | Scope of use |
|---|---|---|---|---|
| TencentDB Agent Memory | Tencent Cloud | MIT | <https://github.com/TencentCloud/TencentDB-Agent-Memory> | L0–L3 drill-down hierarchy concept (raw conversation → atomic facts → scenario → persona, with deterministic pointers back to ground truth) — design input for the planned extracted-fact index over verbatim storage. |
| Hindsight | Vectorize (vectorize-io) | MIT | <https://github.com/vectorize-io/hindsight> | Token-budgeted retrieval assembly pattern (multi-channel retrieval fused, then packed to an explicit token budget rather than a raw result count) — design input for planned retrieval assembly. |
| Perseus Vault | Perseus Computing LLC | MIT | <https://github.com/Perseus-Computing-LLC/perseus-vault> | Bi-temporal SQLite columns pattern (valid time vs. transaction time as plain columns; supersession closes a validity window instead of deleting) — design input for planned bi-temporal supersession. |
| Reciprocal Rank Fusion | Gordon V. Cormack, Charles L. A. Clarke & Stefan Büttcher (SIGIR 2009) | N/A (academic publication) | <https://doi.org/10.1145/1571941.1572114> | Rank-fusion method (RRF, k=60) — design input for planned hybrid FTS5 + vector retrieval. |
| MemGPT / Letta | Charles Packer et al.; Letta (formerly MemGPT) | Apache-2.0 | <https://arxiv.org/abs/2310.08560>, <https://github.com/letta-ai/letta> | Tiered memory concept (context window as RAM, external stores as disk, paged in on demand) — design input for engram's memory hierarchy thinking. |
| Model2Vec | MinishLab | MIT | <https://github.com/MinishLab/model2vec> | Static (non-contextual) embeddings distilled from sentence transformers — **planned M3 dependency** for local, LLM-free semantic search. Not yet shipped. |
| SQLite FTS5 | D. Richard Hipp & the SQLite developers | Public Domain | <https://sqlite.org/fts5.html> | Full-text search engine engram is built on today — a shipped dependency (via `rusqlite`'s bundled SQLite), not merely a design input. |
