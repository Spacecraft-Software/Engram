# Engram — agent context

Shared verbatim chat memory for multi-model LLM pipelines. SQLite+FTS5, one file, three transports.

## Vitals

```sh
cargo build --release                    # LTO, 1 codegen unit, panic=abort
cargo doc -p rmcp --open                 # verify rmcp 0.16 macro shape if build fails
```

`cargo test` covers the rules subsystem (unit tests in `rules.rs`/`store.rs`) and the memory surfaces (integration tests in `tests/cli.rs`). CI: `.github/workflows/ci.yml` (rustfmt, clippy, tests). Formatter config: `rustfmt.toml`.

## Architecture

`src/main.rs` → `Store` (`src/store.rs`) — single source of truth, synchronous `Arc<Mutex<Store>>`. Three surfaces dispatch to the same `Store` methods:

| Surface | Entrypoint | Notes |
|---|---|---|
| CLI | `engram remember/recall/search/context/save-chat/mcp/serve/schema/describe` | clap derive, stdin fallback for content |
| MCP | `engram mcp` | rmcp 0.16 stdio, `#[tool_router]`/`#[tool_handler]` macros |
| HTTP | `engram serve` | axum, `127.0.0.1:8420`, no auth |

## CLI quirks

- `remember` content: positional **or** stdin pipe. No content → error exit 2. `--dry-run` validates and shows what would be stored without writing.
- `recall`/`search` accept `--budget-tokens N`: results are packed to a token budget (estimator `chars-div-4` — ceil(Unicode chars / 4), min 1; no single tokenizer is correct across models). Recall packs newest-first (oldest drop) but output stays chronological; search packs in rank order. The envelope then carries `metadata.budget` (`requested_tokens`, `estimator`, `estimated_tokens`, `included`, `dropped`, `dropped_ids`, `channels`). Without the flag, output is byte-identical to before. Same `budget_tokens` argument exists on the MCP `recall`/`search` tools (result becomes `{"memories": [...], "budget": {...}}`) and the HTTP recall/search routes.
- `context [--scope S] [--query Q] [--budget-tokens N=3000] [--limit N=50]`: one-shot session-start block — active rules first (**always all included**, even over budget), then memories packed into the remainder. No query → newest-first selection; with `--query` → reciprocal rank fusion (k=60) of the recency and FTS channels. Presentation is chronological either way. Scope resolves via the rule-command cascade. Also `GET /v1/context` over HTTP; no MCP context tool yet (M3).
- Mode cascade: explicit `--format`/`--json` > `AI_AGENT`/`AGENT` set non-empty or `CI` truthy > non-TTY stdout → machine output (single-line JSON to stdout, structured error to stderr).
- `--format <json|jsonl|csv>` (global; `--json` is an alias for `--format json`). `jsonl`: first line `{"metadata":...,"data":null}`, then one line per record. `csv`: RFC 4180 rows on stdout, metadata as one JSON line on stderr. `yaml`/`explore` deferred.
- `--accessible` (global): plain linear output per Standard §18. Also via `SPACECRAFT_A11Y=1`; the flag wins over `SPACECRAFT_A11Y=0`.
- `--no-track` (global): read-only auditing — do not update access counts on reads. CLI-only; MCP/HTTP reads always track (`access_count`/`last_accessed_at` are internal columns feeding `consolidate --report` decay; never serialized on `Memory`).
- `install [--harness H]... [--db-path P] [--list] [--dry-run] [--force] [--hooks]`: write `/engram-save-chat`, `/engram-ingest`, `/engram-context` into each detected harness's command directory. Start with `--list`. Only detected harnesses, only files carrying engram's banner, never a delete; idempotent (a second run reports `unchanged` and touches no mtimes). Of the seven known harnesses only Claude Code, Codex, and Opencode have a writable command surface; the rest are reported as having none. CLI-only — never an HTTP route, because it writes into `$HOME`. `--hooks` additionally merges an opt-in `SessionEnd` hook into the harness's settings (Claude Code only), taking a backup first, leaving other hooks alone, and refusing a settings file it cannot parse. That hook runs `ingest` and never `save-chat`.
- `ingest [--harness H] [--session id|latest|all] [--scope S] [--list] [--dry-run] [--include-thinking|--include-tools|--include-sidechains]`: capture the harness's own session transcript into a scope as `user`/`assistant` memories. Tool payloads and thinking are excluded by default (payloads are never stored even with `--include-tools` — only their size); everything dropped is counted in `filtered`. Readers exist for **Claude Code and Codex**; with both installed, engram refuses to guess and requires `--harness`. Idempotent: turn ids are uuid v5 over (harness, session, record), so a re-run inserts 0 and a live session inserts only its new tail. `--max-bytes` (64 MiB default) refuses an oversized transcript rather than reading it by surprise — 114 MB Codex rollouts exist. A harness with no reader exits 2 with a fallback hint and writes nothing to stdout. CLI-only.
- `save-chat [--scope S] [--file PATH] [--model NAME] [--dry-run]`: archive a scope's history as a complete Texinfo document. Default `chat/<timestamp>.texi`; relative paths resolve against the **project root**, not the cwd; auto-gitignores `chat/` (reported as the `gitignore` object: `path`, `entry`, `action` of `added`/`already-ignored`/`would-add`, and a `detail` sentence naming engram as the actor). `--model` falls back to `MODEL`/`LLM_MODEL`/`AI_AGENT`/`AGENT`. Rewrites whole — never appends — so re-running over an unchanged scope is byte-identical and reports `outcome: "unchanged"`. Rules are excluded and the read is untracked (archiving is not retrieval). `--scope` is optional and resolves via the usual cascade.
- `ENGRAM_DB` env var for db path (default: `engram.db`).
- `--no-color` respects `NO_COLOR` convention.
- `describe` subcommand prints JSON capability manifest (CLI Standard introspection).
- `schema` prints JSON Schema for engram's data types, as `{"Memory": ..., "Rule": ...}`.

## Storage

SQLite + FTS5, WAL journal mode, 5s busy timeout. The `memories_fts` virtual table is kept in sync via `AFTER INSERT`/`AFTER DELETE` triggers plus a **content-narrowed** `AFTER UPDATE OF content` trigger (M5 — access-tracking bumps and supersession updates must not churn the FTS index; `migrate()` drop+recreates it on every open since SQLite has no `CREATE OR REPLACE TRIGGER`). FTS5 query sanitization in `sanitize_fts_query` wraps every token as escaped quoted phrase — free-text queries must not hit raw FTS5 syntax.

## Project-root resolution

`managed_file::find_git_root` walks up looking for a **working tree**, not for
the mere existence of `.git`. A directory must contain `.git/HEAD`; a `.git`
*file* (worktree or submodule pointer) also counts. Existence alone is not
enough: an empty `/tmp/.git` on the author's machine made `save-chat` resolve
its project root to `/tmp`, create `/tmp/chat/`, and add `chat/` to
`/tmp/.gitignore`. Any test that resolves a project root must plant its own
marker (`pinned_project` in `tests/cli.rs`) rather than depending on whether an
ancestor of the tempdir happens to look like a repository.

## Conventions (Spacecraft Software Standard)

- SPDX `SPDX-FileCopyrightText` + `SPDX-License-Identifier: GPL-3.0-or-later` on every `.rs` file.
- ISO 8601 UTC (`Z` suffix) for all timestamps via `jiff` — never local time.
- Structured errors to stderr (single-line JSON in machine mode).
- `Response<T>` envelope on every command/route.
- Role defaults to `"note"` on all surfaces.

## Rules

Durable policy, as opposed to memories (which record what happened). Stored as
`memories` rows with `role = "rule"` and a stable `rule_id`, unique per scope —
one write path, one FTS index, no parallel table.

```sh
engram rule add --id <kebab-id> [--scope S] [--agent A] [TEXT]   # stdin if TEXT omitted; upserts
engram rule list [--scope S] [--include-retired]
engram rule retire --id <kebab-id> [--scope S]                   # tombstone, not delete
engram rule purge --id <kebab-id> --yes [--dry-run]              # delete a RETIRED rule (CLI-only)
engram remember ... --supersedes <id>                            # close old validity window, record replacement
engram recall --scope S --as-of <ISO8601> | --include-superseded # time travel / full history
engram index [--scope S] [--dry-run]                             # backfill vectors (vector feature; local model only)
engram search Q --mode fts|hybrid                                # explicit retrieval mode (auto-hybrid when ready)
engram consolidate --extract [--scope S] [--dry-run]             # deterministic fact extraction (M4; CLI-only; no scope = ALL scopes)
engram consolidate --dedup [--yes]                               # near-duplicate groups (exact-normalized ∪ vector cosine>=0.92); --yes supersedes losers to the newest — never deletes (M5)
engram consolidate --report                                      # report-only: contradiction pairs (heuristic; resolve via --supersedes) + top-20 decay by age/un-accessedness (M5)
engram rule sync [--scope S] [--file PATH]... [--dry-run]
```

- **Scope cascade:** `--scope` → `ENGRAM_SCOPE` → git working-tree basename → cwd
  basename. Returned as `scope_origin` on every response. Under MCP this resolves
  against the *server process's* cwd — pass `scope` explicitly for a shared server.
- **`sync` is the delivery step.** `rule add` only writes SQLite; nothing reads
  SQLite. Rendering into `AGENTS.md`/`CLAUDE.md` is what makes a rule take effect.
  Both surfaces return a `next_step` saying so.
- **Sentinels:** `<!-- engram:rules:begin ... -->` / `<!-- engram:rules:end -->`.
  Only that region is rewritten; everything else in the file is preserved. Rule
  text containing `engram:rules:` is rejected at write time — it would break out
  of its own block.
- **Idempotent by construction:** the block carries no generation timestamp, so
  it is a pure function of the rules. Unchanged rules ⇒ `unchanged` ⇒ no write.
  Safe in a hook or commit gate; `--dry-run` makes it a read-only check.
- **Upsert, not append:** re-using a `rule_id` revises that rule. `created_at`
  survives; `updated_at` moves. Two competing copies of a rule would be worse
  than none.
- **Retire tombstones, never deletes.** A retired rule leaves `rule list` and the
  synced files but stays in the database, stays findable via `search`, and comes
  back from `--include-retired` flagged `retired: true`. Erasing the record of a
  policy that once applied would defeat the purpose of a memory store. Re-adding
  the same id reinstates it — the id is the rule's identity. Retiring twice is
  idempotent (`already-retired`); retiring an unknown id is an error (exit 3 /
  HTTP 404), not a silent success. **Retiring does not touch the markdown** —
  follow with `rule sync` or the files keep asserting the retired rule.
- MCP tools: `rule_add`, `rule_list`, `rule_retire`, `rule_sync`.
- HTTP: `POST /v1/rules`, `GET /v1/rules` (`?scope=`, `?include_retired=`),
  `DELETE /v1/rules/:rule_id` (retires; soft), `POST /v1/rules/sync`. As of
  0.2.0 **all** routes — rules and `/v1/memory*` alike — return real status
  codes (400/404/500); errors no longer arrive as `200` with an error body
  (breaking change — check the HTTP status, not just the body).
  `POST /v1/rules/sync` is the only route that writes outside the database;
  paths derive from the server's cwd, never from caller input, and the CLI's
  `--file` override is intentionally not exposed.

## Agent usage

- Call `rule_list` at session start to load the policy governing this project.
- Call `rule_add` + `rule_sync` when the user states a standing requirement —
  something that must hold in *future* sessions. One-off facts go to `remember`.
- Call `rule_retire` + `rule_sync` when the user withdraws one. Do not hand-edit
  the managed block to remove a rule; the next sync would restore it.
- Call `remember` after any decision, fact, or rationale — scope to project/task/run id.
- Call `recall` at session start for that scope — or `context` for rules + budget-packed memories in one call.
- Call `search` before asserting something was already decided.
- CLI, MCP, and HTTP hit the same `Store`; behavior is identical for
  remember/recall/search. Rules are on all three surfaces too.

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

## Command reference

**CLI:**
- `remember --agent <name> --scope <id> [--role <role>] [--dry-run] [<content>]` — store a message (or read from stdin); `--dry-run` validates and shows what would be stored without writing
- `recall --scope <id> [--limit <n>] [--budget-tokens <n>]` — fetch last N messages for a scope (default 50). With `--budget-tokens`, results are packed to the budget newest-first (the oldest drop), output stays chronological, and the envelope carries `metadata.budget`
- `search <query> [--scope <id>] [--limit <n>] [--budget-tokens <n>]` — full-text search (default limit 20). With `--budget-tokens`, results are packed in rank order and the envelope carries `metadata.budget`
- `context [--scope <id>] [--query <q>] [--budget-tokens <n>] [--limit <n>]` — assemble a budget-packed context block for session start: active rules first (**always all included**, even over budget — policy is never silently dropped), then memories selected newest-first, or by reciprocal-rank fusion of recency+FTS relevance+extracted-fact channels when `--query` is given; included memories are presented chronologically. Defaults: budget 3000, limit 50 per channel; scope resolves via the same cascade as the `rule` commands
- `consolidate [--extract] [--dedup [--yes]] [--report] [--scope <id>] [--dry-run]` — idle-time maintenance, three combinable phases (at least one required, else InvalidArgument exit 2): `--extract` (M4) runs the deterministic fact extractor and upserts into the facts index (`--dry-run` applies here only); `--dedup` (M5) finds near-duplicate groups of current non-rule memories per scope — normalized-exact text always, stored-vector cosine ≥ 0.92 when the hybrid gate passes, edges unioned — reporting winner (newest) + losers, and **only with `--yes`** supersedes each loser via `mark_superseded_by` (M2 semantics, no new row, never a delete; idempotent); `--report` (M5) is always report-only: contradiction pairs (word-set Jaccard ≥ 0.5 + negation marker on exactly one side — a heuristic; resolve via `remember --supersedes`, never auto-resolved) and the top-20 decay candidates (`staleness = age_days + 30/(1+access_count)`). Data shape is one optional section per phase: `{extract?, dedup?, report?}`. `--scope` omitted means **every** scope — no cascade, unlike the rule commands. CLI-only by design
- `save-chat [--scope <id>] [--file <path>] [--model <name>] [--dry-run]` — archive a scope's history as a complete Texinfo document. Paths resolve against the **project root** (`rules::resolve_scope`), not the process cwd, so the command targets the same file from any subdirectory; `--file` defaults to `chat/<timestamp>.texi`, and `chat/` is added to `.gitignore` when absent (reported as the `gitignore` object — `action` is `added`, `already-ignored` or `would-add` — not done silently). `--model` names the archiving model (falls back to `MODEL`/`LLM_MODEL`/`AI_AGENT`/`AGENT`, then `unknown-model`). **The document is a pure function of the scope**, exactly as the rules block is: an existing archive is rewritten *whole* (never appended to), re-running over an unchanged scope is byte-identical and reports `outcome: "unchanged"`, and the provenance header therefore carries the *last message's* timestamp rather than the export's wall clock. Rules are excluded (an archive is a transcript) and the read is untracked via `Store::export_history` (archiving is not retrieval — counting it would corrupt the M5 decay signal); superseded rows *are* included
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

## Environment

- `ENGRAM_DB` — override database path
- `ENGRAM_SCOPE` — default scope for `rule` commands
- `ENGRAM_AGENT` — default `--agent` for `rule add`
- `AI_AGENT`, `AGENT` (set non-empty), `CI` (truthy) — trigger machine output mode (detected for structured logging in CI/agent contexts)
- `SPACECRAFT_A11Y` — `1` enables accessible output, `0` disables auto-detection (`--accessible` still wins)
- `NO_COLOR` — disable colors

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
- **Never guess, two rules.** (a) Anything a read cannot turn into a turn is counted rather than skipped silently, in **three separate counters**, because the three mean different things and call for different responses. `unknown_record` is an unrecognized record `type` — a format change in a file engram does not own, fixed by extending an allowlist; it earned its keep by surfacing three Codex tool types (`web_search_call`, `tool_search_call`, `tool_search_output`) that the first implementation miscategorized. `torn_line` is an interrupted write, which lands mid-file and not only at EOF; nothing in engram is wrong, and it is *transient* when a transcript is read while its harness is still appending. `missing_uuid` is a conversation record with no `uuid` — the only one of the three where a real turn was lost. These shared one counter until 2026-08-08, which made every torn line read as a format change and sent a reader chasing a harness that had not moved: one session reported 56 "unknown records" that were all complete lines minutes later. A signal that cries wolf two times in three stops being read, which costs exactly the early warning the counter exists to give. (b) An unparseable timestamp is an **error**, never a substitution of now: `recall_inner` orders by `created_at`, so a wall-clock fallback would collapse a whole conversation into one instant and destroy reading order invisibly.
- **`created_at` is the transcript's timestamp**, and `valid_from` is set to match. This bends the documented "`created_at` is transaction time" reading, and has to, for the ordering reason above.
- **Idempotence comes from the id, not from bookkeeping.** `turn_id = uuid_v5(NAMESPACE_OID, "engram-turn:{harness}:{session}:{record}")` — the same discipline as `facts::fact_id` — plus `Store::ingest_turns`'s `INSERT OR IGNORE` in one transaction. Re-ingesting inserts 0; resuming a live session inserts only the new tail. `OR IGNORE` never deletes, so the external-content FTS trigger fires only for rows that really landed and the index cannot drift (contrast `extract_facts`, which uses `INSERT OR REPLACE` and therefore depends on `recursive_triggers`).
- **No reader is a typed variant, not a `bool`.** `TranscriptSupport::{Reader, NotImplemented{detail}, Unsupported{detail}}` makes "0 turns captured" structurally unreachable for a harness engram cannot read: the caller must match, and the reason is already written down. Antigravity (protobuf + SQLite summaries) and Copilot CLI (`session-store.db`) are `Unsupported`; Codex/Opencode/Goose/Qwen are `NotImplemented`. All of them exit 2 with a hint naming the `remember`-then-`save-chat` fallback, and **stdout stays empty** — an empty success is exactly the failure mode this design prevents.
- **Redaction** (`redact.rs`) replaces credential-shaped substrings before storage and counts them per kind in the envelope. Best-effort, not a guarantee — it catches machine-issued token shapes, not a password typed in prose. The real defense is the default filtering above. `harness::home_dir()` reads `$HOME` directly rather than via the `dirs` crate: a **testability decision**, since every harness path derives from it and a test that sets `HOME` to a tempdir is then hermetic by construction. Do not turn it into a dependency.
- **Fixtures are synthetic**, never copied sessions (`tests/fixtures/transcripts/README.md` explains why): a real transcript holds whatever the user pasted.

## Harness command delivery (`engram install`)

`src/install.rs` + `plugins/engram/`. Engram was already an MCP server in every harness on a typical machine; what was missing was a *command surface*.

- **`plugins/engram/` is the single source of truth.** `install.rs` embeds the command bodies with `include_str!`, so the plugin directory and the installed files cannot drift and the compiler enforces the files exist. Exactly two substitutions, via `str::replace`, no template engine: `{{DB}}` and `{{HARNESS}}`.
- **`{{DB}}` is load-bearing.** The path is discovered from the harness's *own* MCP registration (`harness::registered_db`) — on a typical host all writable harnesses point at one shared store (here `~/.local/share/engram/engram.db`) — but see the drift note below: what they registered *yesterday* is not necessarily what a previously-generated command still pins. A generated command that omitted `--db` would fall back to clap's relative `engram.db` default and quietly write to a different store than the agents read. Config formats are scanned narrowly rather than deserialized: JSON (`mcpServers`, or Opencode's `mcp`), **JSONC** (comment-stripped by a string-aware pass — a `//` inside `"https://…"` must survive), and TOML (line-scanned, so engram needs no TOML dependency). Engram **reads** JSONC and never rewrites it; a serde round-trip would delete the user's comments.
- **5 of 8 harnesses can host something; only 3 host a *command*.** Claude Code, **OpenClaude**, and Opencode have writable command dirs. **Codex 0.149 removed `~/.codex/prompts/`** — the binary contains no such string — and moved to skills at `~/.codex/skills/<name>/SKILL.md`, discovered automatically with nothing to register; engram writes there now. It wrote prompt files nobody read for a release, which is exactly what a harness table drifting from reality looks like. **Antigravity has no slash-command directory at all** — its extension surface is skills, packaged in plugins, and `agy plugin validate` reports a plugin's `commands/` as "2 processed (converted to skills)", so a command there is a skill either way. Engram writes it a plugin (`~/.gemini/config/plugins/engram/`: `plugin.json` + one `skills/engram-<name>/SKILL.md` per command). Goose, Copilot CLI, and Qwen have nothing engram can write and each says so **in its own words** — one shared sentence described none of them accurately.
- **OpenClaude is a Claude Code fork** (`@gitlawb/openclaude`) with its own config root. Its MCP registration lives in `~/.openclaude.json` — the `~/.claude.json` analogue — **not** `~/.openclaude/settings.json`, which holds env/model/hooks and no servers block. Its transcripts are Claude Code's format down to the record keys, so `ReaderKind::ClaudeCode` serves both; the fork-only record types (`mode`, `file-history-snapshot`, `last-prompt`) are already in the non-message allowlist and must stay there, since a fork tripping `unknown_record` every run would train the reader to ignore its own drift alarm.
- **`CommandSurface` is an enum, not a bool.** `Markdown { dir, file, frontmatter }`, `Skill { dir }` (a bare skills root, scanned directly — Codex), `Plugin { dir }` (a plugin wrapping skills — Antigravity), `None { detail }`. Antigravity broke the old `command_frontmatter: bool` because the *shape* of the artifact differs, not just its header — and `None` carries a per-harness reason.
- **Frontmatter is per-surface.** `Markdown { frontmatter: false }` exists for a harness whose command files are plain markdown and would otherwise render the YAML block as literal text; no shipped harness uses it since Codex moved to skills, and it stays because the next one may. A `Skill` or `Plugin` skill has a *different* contract again — `name` + `description`, no `argument-hint`, no `allowed-tools` — and lifts its description from the shared template so the two surfaces cannot describe the same command differently.
- **The banner carries no version** (`<!-- Generated by \`engram install\`. Edits are overwritten. -->`). Putting one there would make every release rewrite every installed file, turning `install` from idempotent into perpetually-updating.
- **Nix, and the corrected skill rule.** Engram installs into whatever surface a harness makes *writable*, and never into the Nix store. The older rule — "engram never ships a skill" — was written for `~/.claude/skills`, a read-only symlink into the store; it does not generalise. Antigravity's `~/.gemini/config/skills` is store-managed too, but its sibling `~/.gemini/config/plugins` is writable, and a plugin may contain skills — so that is where engram writes. `is_nix_managed` warns when a target resolves into the store, since the next `home-manager switch` would clobber the write; those users reference `plugins/` declaratively instead.
- **The pinned database is checked against the registered one.** `install` reads the `--db` already baked into a generated command and, when it differs from what the harness now registers, reports the drift on the file *and* the harness before correcting it. This is not hypothetical: on the author's machine every harness moved to `~/.local/share/engram/engram.db` after `install` had pinned `~/.gemini/engram.db`, so the slash commands and the MCP tools read different stores for weeks with nothing to say so. Every response also carries `db_origin` (`override` / `registered` / `env` / `default`), because `default` is a *relative* `engram.db` that resolves against whatever directory the command runs in.
- **`install` copies, never symlinks.** A symlink breaks when the repo moves and would hand `${CLAUDE_PLUGIN_ROOT}` semantics to a non-plugin context where it is undefined.
- **`--hooks` is opt-in twice over.** It merges a `SessionEnd` entry into `~/.claude/settings.json` (Claude Code is the only harness here with a hook system engram can write). The hook runs **`ingest`, never `save-chat`** — capturing into the database is invisible and reversible; writing a `.texi` into someone's repo at every session end, unasked, is not. Three properties matter: a **timestamped backup** is written before any change; **other people's `SessionEnd` hooks are left alone** (the field is an array, and several hooks on one event is legitimate, not a conflict); and a settings file that does not parse is **refused, never overwritten**. `serde_json` is compiled with `preserve_order` specifically so the merge does not alphabetize a config engram does not own — there is a test asserting key order survives.
- **CLI-only, and there must never be an HTTP route.** `POST /v1/rules/sync` already lets any local process rewrite a project's `AGENTS.md`; an HTTP `install` would extend that to `$HOME` — and, once hooks land, to code executed at every session end, on an unauthenticated port.

## What's not yet implemented

Landed in 0.2.0 (no longer gaps): `--format jsonl|csv`, `remember --dry-run`, real status codes on **all** HTTP routes (a breaking change — see the HTTP notes above), packaging manifests (`packaging/`), the Texinfo manual skeleton (`doc/engram.texi`), `CREDITS.md`, CI, and tests over the memory surfaces.

Still missing:

- `--format yaml` (deferred — `serde_yaml` is archived) and `--format explore` (no TUI yet).
- Authentication on the HTTP surface (currently `127.0.0.1`-only, no bearer check).
