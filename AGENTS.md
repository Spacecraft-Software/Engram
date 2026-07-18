# Engram — agent context

Shared verbatim chat memory for multi-model LLM pipelines. SQLite+FTS5, one file, three transports.

## Vitals

```sh
cargo build --release                    # LTO, 1 codegen unit, panic=abort
cargo doc -p rmcp --open                 # verify rmcp 0.16 macro shape if build fails
```

No tests yet, no CI, no formatter config. Scaffold — compiles-by-inspection.

## Architecture

`src/main.rs` → `Store` (`src/store.rs`) — single source of truth, synchronous `Arc<Mutex<Store>>`. Three surfaces dispatch to the same `Store` methods:

| Surface | Entrypoint | Notes |
|---|---|---|
| CLI | `engram remember/recall/search/mcp/serve/schema/describe` | clap derive, stdin fallback for content |
| MCP | `engram mcp` | rmcp 0.16 stdio, `#[tool_router]`/`#[tool_handler]` macros |
| HTTP | `engram serve` | axum, `127.0.0.1:8420`, no auth |

## CLI quirks

- `remember` content: positional **or** stdin pipe. No content → error exit 2.
- `--json` flag OR any of `AI_AGENT`, `AGENT`, `CI` env vars OR non-TTY stdout → machine output (single-line JSON to stdout, structured error to stderr).
- `ENGRAM_DB` env var for db path (default: `engram.db`).
- `--no-color` respects `NO_COLOR` convention.
- `describe` subcommand prints JSON capability manifest (CLI Standard introspection).
- `schema` prints JSON Schema for the `Memory` type.

## Storage

SQLite + FTS5, WAL journal mode, 5s busy timeout. The `memories_fts` virtual table is kept in sync via `AFTER INSERT/DELETE/UPDATE` triggers on `memories`. FTS5 query sanitization in `sanitize_fts_query` wraps every token as escaped quoted phrase — free-text queries must not hit raw FTS5 syntax.

## Conventions (Spacecraft Software Standard)

- SPDX `SPDX-FileCopyrightText` + `SPDX-License-Identifier: GPL-3.0-or-later` on every `.rs` file.
- ISO 8601 UTC (`Z` suffix) for all timestamps via `jiff` — never local time.
- Structured errors to stderr (single-line JSON in machine mode).
- `Response<T>` envelope on every command/route.
- Role defaults to `"note"` on all surfaces.

## Agent usage

- Call `remember` after any decision, fact, or rationale — scope to project/task/run id.
- Call `recall` at session start for that scope.
- Call `search` before asserting something was already decided.
- All three surfaces (CLI, MCP, HTTP) hit the same `Store`; behavior is identical.
