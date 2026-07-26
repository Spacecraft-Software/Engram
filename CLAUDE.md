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

There are no tests yet — scaffold status. Compile-by-inspection only.

## Architecture

**Single source of truth:** The `Store` (`src/store.rs`) wraps a single `rusqlite::Connection` in `Arc<Mutex<Store>>`. All three surfaces (CLI, MCP, HTTP) dispatch to the same `Store` methods.

| Surface | Entrypoint | Caller | Notes |
|---|---|---|---|
| **CLI** | `engram remember/recall/search/mcp/serve/schema/describe` | Command-line tools, shell scripts | Clap derive; stdin fallback for content |
| **MCP** | `engram mcp` | Claude Code, Codex, other MCP clients | rmcp 0.16 stdio; `#[tool_router]`/`#[tool_handler]` macros |
| **HTTP** | `engram serve` | Any HTTP client (curl, Kimi, Ollama Cloud, etc.) | Axum; `127.0.0.1:8420` only; no auth |

**Module structure:**
- `main.rs` — entry point; parses CLI, instantiates `Store`, dispatches to surface handlers
- `store.rs` — `Store` struct; SQLite schema, migration, CRUD (remember/recall/search, rule_add/rules)
- `rules.rs` — durable project rules: scope resolution, markdown rendering, sentinel-block sync
- `cli.rs` — clap command/argument definitions (`Command` enum, `Cli` struct)
- `mcp.rs` — MCP server (`#[tool_router]` registration, `#[tool_handler]` impls)
- `http.rs` — Axum HTTP server (routes: POST `/v1/memory`, GET `/v1/memory/recall`, `GET /health`, etc.)
- `error.rs` — `AppError` enum; error codes (InvalidArgument, DbError, etc.), exit codes, structured error emission
- `output/` — Output formatting and envelope
  - `mode.rs` — `OutputMode` (Human, Json); detection logic (--json, env vars, TTY)
  - `envelope.rs` — `Response<T>` struct for all command outputs
- `time.rs` — ISO 8601 UTC timestamp generation via `jiff` (never local time)

## Storage: SQLite + FTS5

**Schema:**
- `memories` table: id (PK), agent, scope, role, content, created_at, rule_id (nullable), updated_at (nullable), status (nullable — `active`/`retired`; NULL means active)
- `memories_fts` virtual table (FTS5): full-text index on `content`, kept in sync via `AFTER INSERT/DELETE/UPDATE` triggers
- Indices: `idx_memories_scope`, `idx_memories_created_at` (for recall queries); `idx_memories_rule` — **partial** unique index on `(scope, rule_id) WHERE rule_id IS NOT NULL`, enforcing one rule per id per scope without constraining ordinary messages
- Migration: `migrate()` in `store.rs` probes `pragma_table_info` before each `ALTER TABLE` (SQLite has no `ADD COLUMN IF NOT EXISTS`), so opening a pre-rules database upgrades it in place. Both new columns are nullable — every pre-existing row is a message, which has neither.
- Pragmas: `journal_mode=WAL`, `busy_timeout=5000ms` (allows concurrent readers while one writer is active)

**Query safety:** FTS5 queries are sanitized in `sanitize_fts_query` — every token is wrapped as an escaped quoted phrase to prevent syntax injection.

## Conventions (Spacecraft Software Standard §3, §4, §14)

- **Memory safety:** Rust only. No unsafe blocks without explicit justification.
- **Licensing:** Every `.rs` file carries SPDX headers:
  ```rust
  // SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
  // SPDX-License-Identifier: GPL-3.0-or-later
  ```
- **Timestamps:** ISO 8601 UTC only (via `jiff`, never local time). Suffix with `Z` if needed.
- **CLI shape:** Per the Spacecraft Software Dual-Mode Self-Documenting CLI Standard (v1.0.0) — all commands emit structured output, `--json` flag or non-TTY stdout triggers machine mode (JSON to stdout, structured errors to stderr).
- **Envelopes:** Every command/route returns `Response<T>` (operation name, data, optional error).
- **Role defaults:** `"note"` on all surfaces. Alternatives: `"user"`, `"assistant"`, `"system"`.

## Command reference

**CLI:**
- `remember --agent <name> --scope <id> [--role <role>] [<content>]` — store a message (or read from stdin)
- `recall --scope <id> [--limit <n>]` — fetch last N messages for a scope (default 50)
- `search <query> [--scope <id>] [--limit <n>]` — full-text search (default limit 20)
- `rule add --id <kebab-id> [--scope <id>] [--agent <name>] [<text>]` — record or revise a rule (stdin if text omitted)
- `rule list [--scope <id>] [--include-retired]` — rules in effect, ordered by id
- `rule retire --id <kebab-id> [--scope <id>]` — withdraw a rule (tombstone; re-adding reinstates)
- `rule sync [--scope <id>] [--file <path>]... [--dry-run]` — render rules into `AGENTS.md`/`CLAUDE.md`
- `mcp` — run as MCP server (stdio)
- `serve [--addr <ip:port>]` — run HTTP server (default `127.0.0.1:8420`)
- `schema` — print JSON Schema, as `{"Memory": ..., "Rule": ...}`
- `describe` — print CLI Standard capability manifest (JSON)

**Global flags:**
- `--db <path>` — database file (env: `ENGRAM_DB`, default: `engram.db`)
- `--json` — machine output (single-line JSON)
- `--no-color` — disable colors (respects `NO_COLOR` env var)

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

- **Status codes.** The rule routes return real ones (400 malformed, 404 unknown rule) via the `ApiResult`/`ok`/`err` helpers in `http.rs`. The older `/v1/memory*` handlers answer `200 OK` with an `{"error":...}` body — a defect kept only because fixing it breaks callers. Copy the rule routes when adding endpoints.
- **Path-param syntax.** Routes use `:rule_id`, not `{rule_id}`. axum is pinned at 0.7 (matchit 0.7), where the brace form compiles but matches only the literal string, so the route silently never fires. Change to braces when upgrading to axum 0.8+.

`POST /v1/rules/sync` is the only route that writes outside the database. Targets derive from the server process's cwd, never from caller input (no traversal surface), and the CLI's `--file` override is deliberately not exposed. With the no-auth posture this means any local process can rewrite that project's `AGENTS.md`/`CLAUDE.md`.

## Environment

- `ENGRAM_DB` — override database path
- `ENGRAM_SCOPE` — default scope for `rule` commands
- `ENGRAM_AGENT` — default `--agent` for `rule add`
- `AI_AGENT`, `AGENT`, `CI` — trigger machine output mode (detected for structured logging in CI/agent contexts)
- `NO_COLOR` — disable colors

## Agent usage

In Claude Code or other multi-model pipelines:

1. **Call `remember`** after any decision, fact, or design rationale worth persisting — scope it to your project/task/run ID so related sessions can recall it.
2. **Call `recall`** at the start of a session for that scope to load prior context (or search for specific topics).
3. **Call `search`** before asserting something was already decided — verify rather than guess.

4. **Call `rule_list`** at session start to load standing policy, and `rule_add` + `rule_sync` when the user states a requirement that must hold in future sessions (as opposed to a fact about this one).

All three surfaces (CLI, MCP, HTTP) hit the same `Store`, so memories are shared across deployment modes. Rules are CLI + MCP only.

## What's not yet implemented

- Format options (`--format yaml|csv|jsonl`); only `--json` and human text exist.
- `--dry-run` on `remember` (Standard §3 calls for it on every write command). `rule sync` has one.
- Purging retired rules — tombstones accumulate, and there is no `rule purge`.
- Correct status codes on the older `/v1/memory*` routes (breaking change, awaits a version bump).
- Packaging manifests (Guix, Nix, PKGBUILD), Texinfo manual, `CREDITS.md`.
- Semantic (embedding) search — upgrade path is `sqlite-vec` as a loadable extension.
- Authentication on the HTTP surface (currently `127.0.0.1`-only, no bearer check).
- CI and formatter config. Tests exist for the rules subsystem only (`cargo test`, 11 unit tests in `rules.rs` and `store.rs`); the memory surfaces have none.

## See also

- `README.md` — project description, status, quick-start examples
- `AGENTS.md` — agent-oriented guidance (covers same content as this file but in a different form)
- `CONTRIBUTING.md` — licensing and contribution guidelines
- [The Steelbore Standard](https://Construct.SpacecraftSoftware.org/) — umbrella conventions on memory safety, CLI shape, SPDX licensing, timestamps, etc.
- [rmcp 0.16 documentation](https://docs.rs/rmcp/0.16/rmcp/) — if macros need debugging
