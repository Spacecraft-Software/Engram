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
- `store.rs` — `Store` struct; SQLite schema, schema setup, CRUD operations (remember/recall/search)
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
- `memories` table: id (PK), agent, scope, role, content, created_at
- `memories_fts` virtual table (FTS5): full-text index on `content`, kept in sync via `AFTER INSERT/DELETE/UPDATE` triggers
- Indices: `idx_memories_scope`, `idx_memories_created_at` (for recall queries)
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
- **CLI shape:** Per SFRS (Spacecraft Free Reference Spec) — all commands emit structured output, `--json` flag or non-TTY stdout triggers machine mode (JSON to stdout, structured errors to stderr).
- **Envelopes:** Every command/route returns `Response<T>` (operation name, data, optional error).
- **Role defaults:** `"note"` on all surfaces. Alternatives: `"user"`, `"assistant"`, `"system"`.

## Command reference

**CLI:**
- `remember --agent <name> --scope <id> [--role <role>] [<content>]` — store a message (or read from stdin)
- `recall --scope <id> [--limit <n>]` — fetch last N messages for a scope (default 50)
- `search <query> [--scope <id>] [--limit <n>]` — full-text search (default limit 20)
- `mcp` — run as MCP server (stdio)
- `serve [--addr <ip:port>]` — run HTTP server (default `127.0.0.1:8420`)
- `schema` — print JSON Schema for `Memory` type
- `describe` — print SFRS capability manifest (JSON)

**Global flags:**
- `--db <path>` — database file (env: `ENGRAM_DB`, default: `engram.db`)
- `--json` — machine output (single-line JSON)
- `--no-color` — disable colors (respects `NO_COLOR` env var)

## Environment

- `ENGRAM_DB` — override database path
- `AI_AGENT`, `AGENT`, `CI` — trigger machine output mode (detected for structured logging in CI/agent contexts)
- `NO_COLOR` — disable colors

## Agent usage

In Claude Code or other multi-model pipelines:

1. **Call `remember`** after any decision, fact, or design rationale worth persisting — scope it to your project/task/run ID so related sessions can recall it.
2. **Call `recall`** at the start of a session for that scope to load prior context (or search for specific topics).
3. **Call `search`** before asserting something was already decided — verify rather than guess.

All three surfaces (CLI, MCP, HTTP) hit the same `Store`, so memories are shared across deployment modes.

## What's not yet implemented

- Format options (`--format yaml|csv|jsonl`); only `--json` and human text exist.
- `--dry-run` on `remember` (Standard §3 calls for it on every write command).
- Packaging manifests (Guix, Nix, PKGBUILD), Texinfo manual, `CREDITS.md`.
- Semantic (embedding) search — upgrade path is `sqlite-vec` as a loadable extension.
- Authentication on the HTTP surface (currently `127.0.0.1`-only, no bearer check).
- CI, test suite, formatter config.

## See also

- `README.md` — project description, status, quick-start examples
- `AGENTS.md` — agent-oriented guidance (covers same content as this file but in a different form)
- `CONTRIBUTING.md` — licensing and contribution guidelines
- [The Steelbore Standard](https://Construct.SpacecraftSoftware.org/) — umbrella conventions on memory safety, CLI shape, SPDX licensing, timestamps, etc.
- [rmcp 0.16 documentation](https://docs.rs/rmcp/0.16/rmcp/) — if macros need debugging
