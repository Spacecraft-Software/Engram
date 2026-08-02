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
- `save-chat --scope S [--file PATH] [--model NAME]`: export a scope's history to Texinfo. Default `chat/<timestamp>.texi`; appends to an existing file; auto-gitignores `chat/`. `--model` falls back to `MODEL`/`LLM_MODEL`/`AI_AGENT`/`AGENT`.
- `ENGRAM_DB` env var for db path (default: `engram.db`).
- `--no-color` respects `NO_COLOR` convention.
- `describe` subcommand prints JSON capability manifest (CLI Standard introspection).
- `schema` prints JSON Schema for engram's data types, as `{"Memory": ..., "Rule": ...}`.

## Storage

SQLite + FTS5, WAL journal mode, 5s busy timeout. The `memories_fts` virtual table is kept in sync via `AFTER INSERT/DELETE/UPDATE` triggers on `memories`. FTS5 query sanitization in `sanitize_fts_query` wraps every token as escaped quoted phrase — free-text queries must not hit raw FTS5 syntax.

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
