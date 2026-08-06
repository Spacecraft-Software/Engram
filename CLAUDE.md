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
- `save-chat --scope <id> [--file <path>] [--model <name>] [--dry-run]` — archive a scope's history as a complete Texinfo document. Paths resolve against the **project root** (`rules::resolve_scope`), not the process cwd, so the command targets the same file from any subdirectory; `--file` defaults to `chat/<timestamp>.texi`, and `chat/` is added to `.gitignore` when absent (reported as `gitignore_updated`, not done silently). `--model` names the archiving model (falls back to `MODEL`/`LLM_MODEL`/`AI_AGENT`/`AGENT`, then `unknown-model`). **The document is a pure function of the scope**, exactly as the rules block is: an existing archive is rewritten *whole* (never appended to), re-running over an unchanged scope is byte-identical and reports `outcome: "unchanged"`, and the provenance header therefore carries the *last message's* timestamp rather than the export's wall clock. Rules are excluded (an archive is a transcript) and the read is untracked via `Store::export_history` (archiving is not retrieval — counting it would corrupt the M5 decay signal); superseded rows *are* included
- `ingest [--harness <name>] [--session <id|latest|all>] [--scope <id>] [--cwd <path>] [--include-thinking] [--include-tools] [--include-sidechains] [--max-bytes <n>] [--max-chars-per-turn <n>] [--list] [--dry-run]` — **capture a harness's own session transcript** into a scope as ordinary memories, with roles `user`/`assistant` (values the schema always declared and nothing ever wrote until now). This is what makes "verbatim chat memory" literally true; before it, `save-chat` could only export what an agent chose to `remember`. Harness resolution: `--harness` → environment marker (`CLAUDECODE` etc.) → the single installed harness with a reader — **two candidates is an error, never a guess**. Scope maps by cwd through the same `rules::resolve_scope` cascade. `--list` reports sessions *plus* the whole harness table, so "no sessions here" is distinguishable from "cannot read this harness". CLI-only
- `rule add --id <kebab-id> [--scope <id>] [--agent <name>] [<text>]` — record or revise a rule (stdin if text omitted)
- `rule list [--scope <id>] [--include-retired]` — rules in effect, ordered by id
- `rule retire --id <kebab-id> [--scope <id>]` — withdraw a rule (tombstone; re-adding reinstates)
- `rule purge --id <kebab-id> [--scope <id>] --yes [--dry-run]` — permanently delete a **retired** rule's row (the one true delete; CLI-only — destructive ops are not agent-invocable)
- `rule sync [--scope <id>] [--file <path>]... [--dry-run]` — render rules into `AGENTS.md`/`CLAUDE.md`
- `install [--harness <name>]... [--db-path <path>] [--list] [--dry-run] [--force]` — write engram's slash commands (`/engram-save-chat`, `/engram-ingest`, `/engram-context`) into each detected harness's own command directory. **Start with `--list`.** Only *detected* harnesses are written to (engram never `mkdir`s a home for software you don't have), only files carrying engram's banner are overwritten (`--force` overrides, and a hand-written file is reported `skipped` with a reason), and nothing is deleted. Idempotent by byte comparison — a second run reports every file `unchanged` and does not touch mtimes. **CLI-only, never an HTTP route** (see below)
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
- **CLI-only, on purpose.** `engram consolidate --extract` exists on neither MCP nor HTTP: extraction is an operator's idle-time batch job; agents get facts through `context`/hybrid ranking automatically. CLI-only (see the MCP tool ledger below for the one canonical count).
- Rule rows are never extracted from — policy travels through the rules section, not retrieval.

## Idle consolidation + decay (M5)

`engram consolidate` grew two phases beyond `--extract` (all combinable; at least one required; still CLI-only):

- **`--dedup [--yes]`** — near-duplicate detection over the CURRENT, non-rule memories of each scope. Two detectors run and their edges are **unioned** into connected components: *exact* (normalized text: trim, lowercase, collapse internal whitespace — always on) and *vector* (cosine ≥ 0.92 between **stored** embeddings, same-scope pairs only — runs exactly when the auto-hybrid gate would pass: feature ∧ model resolves ∧ vectors indexed). Each group's NEWEST row (max `created_at`, id tie-break) wins. Without `--yes` it is report-only; with `--yes` every loser goes through `Store::mark_superseded_by(loser, winner, now)` — **M2 supersession semantics reused** (`valid_to` + `superseded_by` set, `WHERE valid_to IS NULL`), *not* `remember_superseding`: no new row is inserted because the winner already exists. Dedup NEVER deletes, and it is idempotent — superseded losers are no longer Current, so a second run finds nothing.
- **`--report`** — always report-only, two sections. (a) *Contradictions*: pairs of CURRENT same-scope non-rule memories with word-set Jaccard ≥ 0.5 AND a negation marker (`not `, `never `, `no longer `, `don't `, `do not `, `isn't `, `wasn't `, `stopped `) on exactly one side. A documented heuristic — a human or agent resolves via `remember --supersedes`; the tool never auto-resolves. (b) *Decay*: every CURRENT non-rule memory scored `staleness = age_days * 1.0 + 30.0/(1+access_count)` (crude, but monotone in age and un-accessedness), top 20 returned with age, access_count, last_accessed_at.

**Access tracking** feeds the decay signal: `recall`/`search`/`search_hybrid`/`context`/`get` bump `access_count`/`last_accessed_at` at the end of the read, inside the same lock, for the memories actually **returned** (never dropped candidates, never the rules section — `rules()` is untracked, and dry-runs write nothing). The columns are internal: `Memory` serialization is byte-identical with or without them. Opt-out is the global `--no-track` CLI flag (read-only auditing), wired right after `Store::open`; MCP and HTTP have no opt-out — agent reads are exactly what the tracking measures.

## MCP tool ledger — the canonical count

**Ten tools, and the ceiling is now reached.** This is the single place the count lives; it used to be restated in three sections and drifted. `src/mcp.rs`'s module doc carries the same statement for readers who are in the code.

`remember`, `recall`, `search`, `get`, `context`, `rule_add`, `rule_list`, `rule_retire`, `rule_sync`, `save_chat`.

Every tool's schema costs context on every turn of every conversation, which is why the cap exists (`doc/engram.texi`). **An eleventh tool must displace an existing one, and the displacement must be argued in the manual.**

- `save_chat` (M4) earned the last slot only because it carries *both halves* of the capture story: `from_transcript: true` captures the session, then archives. Spending the slot on archiving alone would have left MCP able to export a conversation but never record one, with no slot left to fix it.
- **No `file` argument, ever.** The destination derives from the server's resolved project root. A caller-chosen path is a traversal primitive handed to a model whose input includes attacker-influenceable text — the same reasoning that keeps `--file` off `rule_sync`'s MCP surface.
- Deliberately CLI-only and **not** candidates for the slot: `install` (writes into `$HOME`), `ingest` (agents reach it through `save_chat --from-transcript`), `consolidate`/`index` (operator batch jobs whose results arrive through ranking anyway), `rule purge` (destructive ops are not agent-invocable).
- Both surfaces share one implementation: `archive::save_chat` and `transcript::capture` are called by the CLI *and* the MCP tool, so they cannot drift in what they write, filter, redact, or count.

## Transcript capture (`engram ingest`)

`src/harness.rs` + `src/transcript/{mod,claude_code,redact}.rs`. Reads the session file a harness already writes for itself and stores each message as an ordinary memory, so `recall`/`search`/`context`/`consolidate` see the real conversation.

Two readers exist: `claude_code` and `codex`. Adding a third means adding a `ReaderKind` variant, which the two `match`es in `transcript/mod.rs` then force you to handle.

- **Codex layout:** `~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<uuid>.jsonl`. The tree encodes the **date, not the cwd**, so there is nothing to mangle — each rollout's first record is a `session_meta` carrying `cwd` verbatim, and listing reads exactly that one line per file.
- **Codex has two channels, and `event_msg` wins.** `event_msg` is what the UI displayed (flat strings); `response_item` is the raw API traffic. `event_msg` is primary not merely because it parses more easily but because it is *less* noisy: on a real rollout it held 2 user messages where `response_item` held 3, and the extra one was an `<environment_context>` block the harness injects. `response_item` is a fallback used only when a rollout has no `event_msg` conversation at all, so retiring the display channel would degrade rather than silently yield nothing. When the display channel wins, the raw duplicates are counted as `non_message`.
- **Codex session ids are per-rollout, NOT `session_meta.session_id`.** That field is *not unique* — resuming a session writes a new file reusing the same id, and three files sharing one id exist on this machine. Since `turn_id` derives from the session id, reusing it would collide turns at the same line index across rollouts and `INSERT OR IGNORE` would silently drop them. Engram therefore keys on the file name minus `rollout-` (unique, sortable, still contains the uuid). There is a test for exactly this.
- **Codex records carry no per-record id**, so `source_uuid` is `{line_index}:{v5 digest of the text}`. The index alone would suffice for an append-only log; folding in the content means an inserted line does not renumber every later turn into a new identity.
- **`--max-bytes` is not theoretical.** A 114 MB rollout exists on this machine; the 64 MiB default refuses it with a structured error naming the override. Both readers stream line by line.
- **Claude Code layout:** `~/.claude/projects/<mangled-cwd>/<session-uuid>.jsonl`. `mangle_cwd` replaces every `/` with `-` (so the leading slash becomes a leading dash) and **preserves case** — `-spacecraft-software-Majestic` and `…-majestic` are different directories. **Forward-only by construction**: a literal `-` in a path is indistinguishable from a separator in the result, so no inverse is exported. Sibling `<uuid>/subagents/` transcripts are deliberately not read — a subagent is a different conversation and folding it in would interleave two narratives by timestamp.
- **Filtering is the feature, not a detail.** Measured on a real 1.7 MB session: 935 records in, **46 turns out** — 140 `tool_use`, 139 `tool_result`, 52 `thinking`, 226 non-message, 331 empty. Tool payloads and thinking are excluded **by default**; even with `--include-tools` a tool result is summarized to its byte size and the payload is *never* stored, because payloads are where file contents, command output, and credentials live. Every drop is counted in `filtered` and reported.
- **Never guess, two rules.** (a) Unrecognized record `type`s increment `unknown_record` rather than being skipped silently — that counter is the early-warning signal for a format change in a file engram does not own, and it has already earned its keep twice: it surfaced three Codex tool types (`web_search_call`, `tool_search_call`, `tool_search_output`) that the first implementation miscategorized, and it correctly flags genuinely torn lines from interrupted writes (one exists mid-file in a real rollout, not at EOF). (b) An unparseable timestamp is an **error**, never a substitution of now: `recall_inner` orders by `created_at`, so a wall-clock fallback would collapse a whole conversation into one instant and destroy reading order invisibly.
- **`created_at` is the transcript's timestamp**, and `valid_from` is set to match. This bends the documented "`created_at` is transaction time" reading, and has to, for the ordering reason above.
- **Idempotence comes from the id, not from bookkeeping.** `turn_id = uuid_v5(NAMESPACE_OID, "engram-turn:{harness}:{session}:{record}")` — the same discipline as `facts::fact_id` — plus `Store::ingest_turns`'s `INSERT OR IGNORE` in one transaction. Re-ingesting inserts 0; resuming a live session inserts only the new tail. `OR IGNORE` never deletes, so the external-content FTS trigger fires only for rows that really landed and the index cannot drift (contrast `extract_facts`, which uses `INSERT OR REPLACE` and therefore depends on `recursive_triggers`).
- **No reader is a typed variant, not a `bool`.** `TranscriptSupport::{Reader, NotImplemented{detail}, Unsupported{detail}}` makes "0 turns captured" structurally unreachable for a harness engram cannot read: the caller must match, and the reason is already written down. Antigravity (protobuf + SQLite summaries) and Copilot CLI (`session-store.db`) are `Unsupported`; Codex/Opencode/Goose/Qwen are `NotImplemented`. All of them exit 2 with a hint naming the `remember`-then-`save-chat` fallback, and **stdout stays empty** — an empty success is exactly the failure mode this design prevents.
- **Redaction** (`redact.rs`) replaces credential-shaped substrings before storage and counts them per kind in the envelope. Best-effort, not a guarantee — it catches machine-issued token shapes, not a password typed in prose. The real defense is the default filtering above. `harness::home_dir()` reads `$HOME` directly rather than via the `dirs` crate: a **testability decision**, since every harness path derives from it and a test that sets `HOME` to a tempdir is then hermetic by construction. Do not turn it into a dependency.
- **Fixtures are synthetic**, never copied sessions (`tests/fixtures/transcripts/README.md` explains why): a real transcript holds whatever the user pasted.

## Harness command delivery (`engram install`)

`src/install.rs` + `plugins/engram/`. Engram was already an MCP server in every harness on a typical machine; what was missing was a *command surface*.

- **`plugins/engram/` is the single source of truth.** `install.rs` embeds the command bodies with `include_str!`, so the plugin directory and the installed files cannot drift and the compiler enforces the files exist. Exactly two substitutions, via `str::replace`, no template engine: `{{DB}}` and `{{HARNESS}}`.
- **`{{DB}}` is load-bearing.** The path is discovered from the harness's *own* MCP registration (`harness::registered_db`) — all three writable harnesses on this machine point at `/home/mj/.gemini/engram.db`. A generated command that omitted `--db` would fall back to clap's relative `engram.db` default and quietly write to a different store than the agents read. Config formats are scanned narrowly rather than deserialized: JSON (`mcpServers`, or Opencode's `mcp`), **JSONC** (comment-stripped by a string-aware pass — a `//` inside `"https://…"` must survive), and TOML (line-scanned, so engram needs no TOML dependency). Engram **reads** JSONC and never rewrites it; a serde round-trip would delete the user's comments.
- **Only 3 of 7 harnesses can host a command.** Claude Code, Codex, and Opencode have writable command dirs; Antigravity, Goose, Copilot CLI, and Qwen are reported `note: "no command surface engram can write"` rather than silently omitted. Saying "works in all seven" would make the feature read as broken on four of them.
- **Frontmatter is per-harness.** `command_frontmatter: false` for Codex, whose prompts are plain markdown and would otherwise render the YAML block as literal text at the top of every prompt.
- **The banner carries no version** (`<!-- Generated by \`engram install\`. Edits are overwritten. -->`). Putting one there would make every release rewrite every installed file, turning `install` from idempotent into perpetually-updating.
- **Nix:** `~/.claude/skills` is a read-only symlink into the Nix store, so runtime *skill* installation is impossible. Engram therefore never ships a skill — it ships commands, and command dirs are writable. `is_nix_managed` warns when a target resolves into the store, since the next `home-manager switch` would clobber the write; those users reference `plugins/engram/` declaratively instead.
- **`install` copies, never symlinks.** A symlink breaks when the repo moves and would hand `${CLAUDE_PLUGIN_ROOT}` semantics to a non-plugin context where it is undefined.
- **`--hooks` is opt-in twice over.** It merges a `SessionEnd` entry into `~/.claude/settings.json` (Claude Code is the only harness here with a hook system engram can write). The hook runs **`ingest`, never `save-chat`** — capturing into the database is invisible and reversible; writing a `.texi` into someone's repo at every session end, unasked, is not. Three properties matter: a **timestamped backup** is written before any change; **other people's `SessionEnd` hooks are left alone** (the field is an array, and several hooks on one event is legitimate, not a conflict); and a settings file that does not parse is **refused, never overwritten**. `serde_json` is compiled with `preserve_order` specifically so the merge does not alphabetize a config engram does not own — there is a test asserting key order survives.
- **CLI-only, and there must never be an HTTP route.** `POST /v1/rules/sync` already lets any local process rewrite a project's `AGENTS.md`; an HTTP `install` would extend that to `$HOME` — and, once hooks land, to code executed at every session end, on an unauthenticated port.

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
