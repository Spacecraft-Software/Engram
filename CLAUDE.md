# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this project is

Engram is a shared verbatim chat memory store for multi-model LLM pipelines. It's a single SQLite file (with FTS5 full-text search) that multiple models/agents can read from and write to, enabling memory across different stages of a pipeline without requiring LLM calls to encode/retrieve context.

## Building and running

**Build:**
```sh
cargo build --release    # LTO, 1 codegen unit, panic=abort per Standard §3
```

Verify the `rmcp` 0.16 macro shape (if build fails due to macro mismatch):
```sh
cargo doc -p rmcp --open
```

**Run (CLI):**
```sh
./target/release/engram remember --agent claude-code --scope my-task "Decided: X stays synchronous."
./target/release/engram recall --scope my-task
./target/release/engram search "synchronous"
```

**Run as MCP server** (wire into Claude Code / other clients):
```sh
./target/release/engram mcp --db ./engram.db
```

**Run as HTTP server** (local-only, `127.0.0.1:8420`, no auth):
```sh
./target/release/engram serve --db ./engram.db --addr 127.0.0.1:8420
curl -s -XPOST localhost:8420/v1/memory -d '{"agent":"kimi","scope":"x","content":"..."}'
curl -s "localhost:8420/v1/memory/recall?scope=x&limit=10"
```

`cargo test` covers the rules subsystem (unit tests in `rules.rs`/`store.rs`) and the memory surfaces (integration tests in `tests/cli.rs`). CI runs rustfmt, clippy, and the test suite via `.github/workflows/ci.yml`.

## Architecture

**Single source of truth:** The `Store` (`src/store.rs`) wraps a single `rusqlite::Connection` in `Arc<Mutex<Store>>`. All three surfaces (CLI, MCP, HTTP) dispatch to the same `Store` methods.

| Surface | Entrypoint | Caller | Notes |
|---|---|---|---|
| **CLI** | `engram remember/recall/search/context/save-chat/mcp/serve/schema/describe` | Command-line tools, shell scripts | Clap derive; stdin fallback for content |
| **MCP** | `engram mcp` | Claude Code, Codex, other MCP clients | rmcp 0.16 stdio; `#[tool_router]`/`#[tool_handler]` macros |
| **HTTP** | `engram serve` | Any HTTP client (curl, Kimi, Ollama Cloud, etc.) | Axum; `127.0.0.1:8420` only; no auth |

**Module structure:**
- `main.rs` — entry point; parses CLI, instantiates `Store`, dispatches to surface handlers
- `store.rs` — `Store` struct; SQLite schema, migration, CRUD (remember/recall/search, rule_add/rules, context)
- `retrieval.rs` — token budgeting + retrieval assembly, pure functions (no DB handle): `estimate_tokens` (ceil(chars/4), estimator `"chars-div-4"`), `rrf_fuse` (reciprocal rank fusion, `RRF_K = 60.0`), `budget_recall`/`budget_search` (greedy drop-and-continue packing), `BudgetReport`
- `embed.rs` — local Model2Vec embeddings (cfg-gated `vector`): model-path cascade, process-wide embedder cache, cosine
- `rules.rs` — durable project rules: scope resolution, markdown rendering, sentinel-block sync
- `cli.rs` — clap command/argument definitions (`Command` enum, `Cli` struct)
- `mcp.rs` — MCP server (`#[tool_router]` registration, `#[tool_handler]` impls)
- `http.rs` — Axum HTTP server (routes: POST `/v1/memory`, GET `/v1/memory/recall`, GET `/v1/memory/search`, GET `/v1/context`, GET `/v1/health`, the `/v1/rules*` family)
- `error.rs` — `AppError` enum; error codes (InvalidArgument, DbError, etc.), exit codes, structured error emission
- `output/` — Output formatting and envelope
  - `mode.rs` — `OutputMode` and `Format` (json, jsonl, csv); detection logic (`--format`/`--json`, env vars, TTY)
  - `envelope.rs` — `Response<T>` struct for all command outputs
- `time.rs` — ISO 8601 UTC timestamp generation via `jiff` (never local time)

## Storage: SQLite + FTS5

**Schema:**
- `memories` table: id (PK), agent, scope, role, content, created_at, rule_id (nullable), updated_at (nullable), status (nullable — `active`/`retired`; NULL means active), the bi-temporal trio `valid_from`/`valid_to`/`superseded_by` (all nullable; NULL `valid_to` means currently valid, so every pre-supersession row stays valid by construction; `created_at` is transaction time, `valid_*` is validity time), plus the M5 access-tracking pair `access_count`/`last_accessed_at` (nullable; NULL `access_count` reads as 0; **internal** — `row_to_memory` never reads them, so `Memory` output is unchanged). `status` is exclusively the rules axis; supersession never touches it.
- `memories_fts` virtual table (FTS5): full-text index on `content`, kept in sync via `AFTER INSERT`/`AFTER DELETE` triggers plus a **content-narrowed** update trigger — `memories_au` is `AFTER UPDATE OF content` (M5): the original full-row trigger fired the FTS delete+reinsert on EVERY update, so each access-tracking bump (i.e. every read) and each supersession would churn the index. `migrate()` unconditionally `DROP TRIGGER IF EXISTS` + recreates it narrowed on every open (SQLite has no `CREATE OR REPLACE TRIGGER`; the drop+create is idempotent and converts pre-M5 databases in place)
- `facts` table (M4): id (PK — deterministic UUID v5 over `(memory_id, fact)`, see `facts::fact_id`), memory_id (FK → memories), scope, fact (verbatim extracted line/sentence), extractor (`"deterministic-v1"`), created_at, plus **reserved** `valid_to`/`superseded_by` (always NULL today — a fact's liveness derives from its PARENT's validity: every fact-channel query JOINs `memories` and applies the validity clause to the parent columns). Indices `idx_facts_scope`, `idx_facts_memory`; `facts_fts` (FTS5, content='facts') kept in sync by `facts_ai/ad/au` triggers. `PRAGMA recursive_triggers=ON` is set at open so `INSERT OR REPLACE` fires the delete trigger (otherwise the external-content FTS index drifts)
- Indices: `idx_memories_scope`, `idx_memories_created_at` (for recall queries); `idx_memories_rule` — **partial** unique index on `(scope, rule_id) WHERE rule_id IS NOT NULL`, enforcing one rule per id per scope without constraining ordinary messages
- Migration: `migrate()` in `store.rs` probes `pragma_table_info` before each `ALTER TABLE` (SQLite has no `ADD COLUMN IF NOT EXISTS`), so opening a pre-rules database upgrades it in place. Both new columns are nullable — every pre-existing row is a message, which has neither.
- Pragmas: `journal_mode=WAL`, `busy_timeout=5000ms` (allows concurrent readers while one writer is active)

**Query safety:** FTS5 queries are sanitized in `sanitize_fts_query` — every token is wrapped as an escaped quoted phrase to prevent syntax injection, and tokens are joined with `OR` (not FTS5's implicit `AND`): natural-language queries almost always contain a filler word the stored text lacks, and one missing token zeroes an AND match. Measured on the M3 bench: AND-joined recall@5 0.108 → OR-joined 0.856 (`bench/RESULTS.md`); BM25 still ranks multi-token matches first.

## Conventions (Spacecraft Software Standard §3, §4, §14)

- **Memory safety:** Rust only. No unsafe blocks without explicit justification.
- **Licensing:** Every `.rs` file carries SPDX headers:
  ```rust
  // SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
  // SPDX-License-Identifier: GPL-3.0-or-later
  ```
- **Timestamps:** ISO 8601 UTC only (via `jiff`, never local time). Suffix with `Z` if needed.
- **CLI shape:** Per the Spacecraft Software Dual-Mode Self-Documenting CLI Standard (v1.0.0) — all commands emit structured output. Mode cascade: explicit `--format`/`--json` > agent env vars (`AI_AGENT`/`AGENT` set non-empty, `CI` truthy) > non-TTY stdout ⇒ machine mode (JSON to stdout, structured errors to stderr).
- **Envelopes:** Every command/route returns `Response<T>` (operation name, data, optional error). When token budgeting is requested, `metadata.budget` carries a `BudgetReport`: `requested_tokens`, `estimator` (`"chars-div-4"` — ceil(Unicode chars / 4), min 1; multi-model pipelines have no single correct tokenizer), `estimated_tokens` (included items only), `included`, `dropped`, `dropped_ids`, and `channels` (candidate count per channel: `recency`/`fts`/`rules`).
- **Role defaults:** `"note"` on all surfaces. Alternatives: `"user"`, `"assistant"`, `"system"`.

## Command reference

**CLI:**
- `remember --agent <name> --scope <id> [--role <role>] [--dry-run] [<content>]` — store a message (or read from stdin); `--dry-run` validates and shows what would be stored without writing
- `recall --scope <id> [--limit <n>] [--budget-tokens <n>]` — fetch last N messages for a scope (default 50). With `--budget-tokens`, results are packed to the budget newest-first (the oldest drop), output stays chronological, and the envelope carries `metadata.budget`
- `search <query> [--scope <id>] [--limit <n>] [--budget-tokens <n>]` — full-text search (default limit 20). With `--budget-tokens`, results are packed in rank order and the envelope carries `metadata.budget`
- `context [--scope <id>] [--query <q>] [--budget-tokens <n>] [--limit <n>]` — assemble a budget-packed context block for session start: active rules first (**always all included**, even over budget — policy is never silently dropped), then memories selected newest-first, or by reciprocal-rank fusion of recency+FTS relevance+extracted-fact channels when `--query` is given; included memories are presented chronologically. Defaults: budget 3000, limit 50 per channel; scope resolves via the same cascade as the `rule` commands
- `consolidate [--extract] [--dedup [--yes]] [--report] [--scope <id>] [--dry-run]` — idle-time maintenance, three combinable phases (at least one required, else InvalidArgument exit 2): `--extract` (M4) runs the deterministic fact extractor and upserts into the facts index (`--dry-run` applies here only); `--dedup` (M5) finds near-duplicate groups of current non-rule memories per scope — normalized-exact text always, stored-vector cosine ≥ 0.92 when the hybrid gate passes, edges unioned — reporting winner (newest) + losers, and **only with `--yes`** supersedes each loser via `mark_superseded_by` (M2 semantics, no new row, never a delete; idempotent); `--report` (M5) is always report-only: contradiction pairs (word-set Jaccard ≥ 0.5 + negation marker on exactly one side — a heuristic; resolve via `remember --supersedes`, never auto-resolved) and the top-20 decay candidates (`staleness = age_days + 30/(1+access_count)`). Data shape is one optional section per phase: `{extract?, dedup?, report?}`. `--scope` omitted means **every** scope — no cascade, unlike the rule commands. CLI-only by design
- `save-chat --scope <id> [--file <path>] [--model <name>]` — export a scope's full history to a Texinfo file. `--file` defaults to `chat/<timestamp>.texi`; an existing file is appended to (a new signed chapter), and `chat/` is added to `.gitignore` automatically. `--model` names the signing model (falls back to `MODEL`/`LLM_MODEL`/`AI_AGENT`/`AGENT` env vars)
- `rule add --id <kebab-id> [--scope <id>] [--agent <name>] [<text>]` — record or revise a rule (stdin if text omitted)
- `rule list [--scope <id>] [--include-retired]` — rules in effect, ordered by id
- `rule retire --id <kebab-id> [--scope <id>]` — withdraw a rule (tombstone; re-adding reinstates)
- `rule purge --id <kebab-id> [--scope <id>] --yes [--dry-run]` — permanently delete a **retired** rule's row (the one true delete; CLI-only — destructive ops are not agent-invocable)
- `rule sync [--scope <id>] [--file <path>]... [--dry-run]` — render rules into `AGENTS.md`/`CLAUDE.md`
- `mcp` — run as MCP server (stdio)
- `serve [--addr <ip:port>]` — run HTTP server (default `127.0.0.1:8420`)
- `schema` — print JSON Schema, as `{"Memory": ..., "Rule": ...}`
- `describe` — print CLI Standard capability manifest (JSON)

**Global flags:**
- `--db <path>` — database file (env: `ENGRAM_DB`, default: `engram.db`)
- `--json` — machine output; alias for `--format json`
- `--format <json|jsonl|csv>` — machine output format, overrides mode auto-detection. `jsonl`: first line is `{"metadata":...,"data":null}`, then one line per record (arrays) or one line with the object. `csv`: RFC 4180 rows on stdout (header from the first record's keys), metadata as one JSON line on stderr. `yaml`/`explore` are deferred
- `--no-color` — disable colors (respects `NO_COLOR` env var)
- `--accessible` — accessible output per Standard §18: plain linear text, no color, status tags. Also enabled by `SPACECRAFT_A11Y=1`; the flag wins over `SPACECRAFT_A11Y=0`
- `--no-track` — read-only auditing: do not update access counts on reads. CLI-only; MCP/HTTP have no opt-out (agent reads are exactly what the tracking measures)

## Rules (`src/rules.rs`)

Durable policy, distinct from memories: a memory records what happened, a rule states what must keep being true. Implemented as `memories` rows with `role = "rule"` plus a stable `rule_id` — reusing the table keeps one write path, one FTS index, and identical behavior across surfaces.

Three invariants worth not breaking:

1. **`sync` is the delivery mechanism, not an export.** A row in SQLite never reaches a model's context. Rendering into `AGENTS.md`/`CLAUDE.md` — files harnesses auto-load — is what makes a rule take effect. Both surfaces return `next_step` reminding the caller.
2. **The rendered block is a pure function of the rules.** No generation timestamp, rules ordered by `rule_id`. That is what makes `sync` idempotent (`unchanged` ⇒ no write) and therefore safe in a hook or commit gate. Do not add a timestamp to the block.
3. **`add` upserts.** Re-using a `rule_id` revises in place; `created_at` survives, `updated_at` moves. Two competing copies of a rule are worse than none.
4. **`retire` tombstones, never deletes.** `status='retired'` hides a rule from `rules()` and from synced files, but the row survives and stays searchable — erasing the record of a policy that once applied would defeat the point of a memory store. `rule_add` on a retired id reinstates it (sets `status='active'`), which also avoids colliding with the unique index. Retiring is idempotent; an unknown id is an error (exit 3 / HTTP 404), not a silent success.

`status IS NULL` means active — that is how rules written before the status column keep working. Any new query filtering on status must preserve that.

Sentinels are `<!-- engram:rules:begin ... -->` / `<!-- engram:rules:end -->`; only that region is rewritten. Rule text containing `engram:rules:` is rejected at write time (it would terminate its own block). An opening sentinel with no closing one is treated as a mangled block and replaced wholesale rather than appended after.

Scope cascade: `--scope` → `ENGRAM_SCOPE` → git working-tree basename → cwd basename, reported as `scope_origin`. Under MCP this resolves against the **server process's** cwd, so a shared server needs an explicit `scope`.

Rules are on all three surfaces. HTTP routes: `POST /v1/rules`, `GET /v1/rules` (`?scope=`, `?include_retired=`), `DELETE /v1/rules/:rule_id` (retires — soft), `POST /v1/rules/sync`.

Two HTTP notes worth carrying forward:

- **Status codes.** As of 0.2.0, **all** routes return real status codes via the `ApiResult`/`ok`/`err` helpers in `http.rs` — 400 on a malformed request (e.g. empty/whitespace `content` on `POST /v1/memory`), 404 on an unknown rule, 500 on storage failure. This was a deliberate breaking change: the 0.1.x `/v1/memory*` handlers answered `200 OK` with an `{"error":...}` body, and callers that only parsed the body must now check the HTTP status. Keep following this pattern when adding endpoints.
- **Path-param syntax.** Routes use `:rule_id`, not `{rule_id}`. axum is pinned at 0.7 (matchit 0.7), where the brace form compiles but matches only the literal string, so the route silently never fires. Change to braces when upgrading to axum 0.8+.

`POST /v1/rules/sync` is the only route that writes outside the database. Targets derive from the server process's cwd, never from caller input (no traversal surface), and the CLI's `--file` override is deliberately not exposed. With the no-auth posture this means any local process can rewrite that project's `AGENTS.md`/`CLAUDE.md`.

## Environment

- `ENGRAM_DB` — override database path
- `ENGRAM_SCOPE` — default scope for `rule` commands
- `ENGRAM_AGENT` — default `--agent` for `rule add`
- `AI_AGENT`, `AGENT` (set non-empty), `CI` (truthy) — trigger machine output mode (detected for structured logging in CI/agent contexts)
- `SPACECRAFT_A11Y` — `1` enables accessible output, `0` disables auto-detection (`--accessible` still wins)
- `NO_COLOR` — disable colors

## Agent usage

In Claude Code or other multi-model pipelines:

1. **Call `remember`** after any decision, fact, or design rationale worth persisting — scope it to your project/task/run ID so related sessions can recall it.
2. **Call `recall`** at the start of a session for that scope to load prior context (or search for specific topics) — or call `context` to get rules + budget-packed memories in one shot.
3. **Call `search`** before asserting something was already decided — verify rather than guess.

4. **Call `rule_list`** at session start to load standing policy, and `rule_add` + `rule_sync` when the user states a requirement that must hold in future sessions (as opposed to a fact about this one).

All three surfaces (CLI, MCP, HTTP) hit the same `Store`, so memories are shared across deployment modes. Rules are on all three surfaces too.

## Semantic search (the `vector` feature, M3)

Opt-in at build time: `cargo build --release --features vector`. The default build stays FTS5-only with zero ML dependencies. Facts:

- **Engine:** Model2Vec static embeddings via `model2vec-rs` compiled with `default-features = false` + `local-only` — the hf-hub network fetch path is compiled OUT; engram never downloads a model (§9 PFA). Install one by hand (e.g. `minishlab/potion-base-8M`: `model.safetensors` + `tokenizer.json` + `config.json`).
- **Model cascade:** `--model-path` → `ENGRAM_MODEL` → `$XDG_DATA_HOME/engram/model` (default `~/.local/share/engram/model`). The directory basename is the model name in `memory_vectors.model`; vectors from different models are never compared.
- **Storage:** `memory_vectors` side table (memory_id PK/FK, model, dim, embedding BLOB f32-LE) — deliberately NOT sqlite-vec (alpha C extension vs §5.5 packaging) and NOT columns on `memories` (FTS triggers untouched). Similarity is brute-force cosine: sub-millisecond under 100k rows.
- **Indexing:** `engram remember` embeds live on the CLI when a model resolves; `engram index [--scope] [--batch] [--dry-run]` backfills everything else (MCP/HTTP writes, pre-model history). Rule rows are never embedded.
- **Retrieval:** `search --mode fts|hybrid`; omitted, hybrid engages automatically when (feature ∧ model resolves ∧ vectors indexed), else fts. Explicit `--mode hybrid` with a missing prerequisite is a structured exit-2/HTTP-400 error, never a silent fallback. Hybrid = FTS top-50 + cosine top-50 → `rrf_fuse(k=60)`; `context` gains the vector as a third channel the same way.
- **The gate:** measured 2026-08-02 on the held-out `bench/queries.jsonl` (frozen before implementation): hybrid 0.918 vs fts 0.856 recall@5 = +6.2 points ≥ +5 → PASS; the margin is entirely conceptual/synonym queries. See `bench/RESULTS.md` — including why the first (+77.9) measurement was rejected as a baseline defect.

## Extracted-fact index (M4)

The TencentDB L0↔L1 pattern: L0 is the verbatim memory, L1 is a *derived index* of the decision/constraint sentences inside it. Facts never replace verbatim — each `facts` row is a verbatim substring of its parent's content plus a drill-down pointer (`memory_id` → `engram get` / the MCP `get` tool).

- **Extractor: `deterministic-v1` only — no LLM on the write path, ever.** `facts::extract` splits content into lines (plus sentence-splits of multi-sentence lines), trims bullet markers, and keeps units that start (case-insensitively) with one of 19 markers (`Decided:`, `Decision:`, `TODO`, `FIXME`, `NOTE:`, `Rule:`, `Fix:`, `Fixed:`, `Chose:`, `Chosen:`, `Rejected:`, `Constraint:`, `Gotcha:`, `Warning:`, `Never `, `Always `, `Must `, `Do not `, `Don't `). Floor 12 chars, cap 8 facts per memory (first eight distinct in document order), exact-dedupe. Facts are stored verbatim — rewriting would be the lossy-extraction trap.
- **Liveness derives from the parent.** Extraction is append-only (`INSERT OR REPLACE` on deterministic v5 ids — idempotent, re-runs don't grow the table); nothing deletes facts when a memory is superseded. Instead `fact_candidates` JOINs `memories` and applies the validity filter to the parent, so stale facts stop surfacing the moment their parent does. The fact columns `valid_to`/`superseded_by` are reserved and stay NULL.
- **Channel wiring.** With a `--query`, `context` fuses recency + FTS + **facts** (parents of matching facts, deduped, rank order) — plus vector when the hybrid gate passes — and reports `channels.facts`. Hybrid search is now fts + vector + facts. Plain FTS `search` is unchanged (memories only; facts are substrings of content, so the channel can only *boost* the memory that states a decision above ones that merely mention its words — it can never be a sole finder).
- **CLI-only, on purpose.** `engram consolidate --extract` exists on neither MCP nor HTTP: extraction is an operator's idle-time batch job; agents get facts through `context`/hybrid ranking automatically. The MCP tool ledger is unchanged (still 9 tools of the 10-tool ceiling).
- Rule rows are never extracted from — policy travels through the rules section, not retrieval.

## Idle consolidation + decay (M5)

`engram consolidate` grew two phases beyond `--extract` (all combinable; at least one required; still CLI-only — the MCP ledger stays at 9 tools):

- **`--dedup [--yes]`** — near-duplicate detection over the CURRENT, non-rule memories of each scope. Two detectors run and their edges are **unioned** into connected components: *exact* (normalized text: trim, lowercase, collapse internal whitespace — always on) and *vector* (cosine ≥ 0.92 between **stored** embeddings, same-scope pairs only — runs exactly when the auto-hybrid gate would pass: feature ∧ model resolves ∧ vectors indexed). Each group's NEWEST row (max `created_at`, id tie-break) wins. Without `--yes` it is report-only; with `--yes` every loser goes through `Store::mark_superseded_by(loser, winner, now)` — **M2 supersession semantics reused** (`valid_to` + `superseded_by` set, `WHERE valid_to IS NULL`), *not* `remember_superseding`: no new row is inserted because the winner already exists. Dedup NEVER deletes, and it is idempotent — superseded losers are no longer Current, so a second run finds nothing.
- **`--report`** — always report-only, two sections. (a) *Contradictions*: pairs of CURRENT same-scope non-rule memories with word-set Jaccard ≥ 0.5 AND a negation marker (`not `, `never `, `no longer `, `don't `, `do not `, `isn't `, `wasn't `, `stopped `) on exactly one side. A documented heuristic — a human or agent resolves via `remember --supersedes`; the tool never auto-resolves. (b) *Decay*: every CURRENT non-rule memory scored `staleness = age_days * 1.0 + 30.0/(1+access_count)` (crude, but monotone in age and un-accessedness), top 20 returned with age, access_count, last_accessed_at.

**Access tracking** feeds the decay signal: `recall`/`search`/`search_hybrid`/`context`/`get` bump `access_count`/`last_accessed_at` at the end of the read, inside the same lock, for the memories actually **returned** (never dropped candidates, never the rules section — `rules()` is untracked, and dry-runs write nothing). The columns are internal: `Memory` serialization is byte-identical with or without them. Opt-out is the global `--no-track` CLI flag (read-only auditing), wired right after `Store::open`; MCP and HTTP have no opt-out — agent reads are exactly what the tracking measures.

## What's not yet implemented

Landed in 0.2.0 (no longer gaps): `--format jsonl|csv`, `remember --dry-run`, real status codes on **all** HTTP routes (a breaking change — see the HTTP notes above), packaging manifests (`packaging/`), the Texinfo manual skeleton (`doc/engram.texi`), `CREDITS.md`, CI, and tests over the memory surfaces.

Still missing:

- `--format yaml` (deferred — `serde_yaml` is archived) and `--format explore` (no TUI yet).
- Authentication on the HTTP surface (currently `127.0.0.1`-only, no bearer check).

## See also

- `README.md` — project description, status, quick-start examples
- `AGENTS.md` — agent-oriented guidance (covers same content as this file but in a different form)
- `CONTRIBUTING.md` — licensing and contribution guidelines
- [The Steelbore Standard](https://Construct.SpacecraftSoftware.org/) — umbrella conventions on memory safety, CLI shape, SPDX licensing, timestamps, etc.
- [rmcp 0.16 documentation](https://docs.rs/rmcp/0.16/rmcp/) — if macros need debugging
