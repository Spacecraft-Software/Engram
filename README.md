# Engram

Shared verbatim chat memory for multi-model LLM pipelines. One SQLite file
(FTS5-indexed), three ways in: CLI, MCP (stdio), HTTP.

Point every model in your pipeline — Claude Code, Codex, Kimi, Ollama Cloud —
at the same `engram.db` (or the same `engram serve` instance) and they share
memory by construction. No LLM calls needed to write a memory: raw text in,
raw text out, full-text searchable.

## Status

Builds and runs against the pinned `rmcp` 0.16. The MCP stdio handshake
tolerates a leading non-`initialize` probe (some hosts, e.g. Antigravity,
send a proprietary discovery request first) — see `src/mcp.rs`.

## Quick start

```sh
cargo build --release
./target/release/engram remember --agent claude-code --scope majestic-adr-023 "Decided: Gnomon stays synchronous, no Tokio in the reader."
./target/release/engram recall --scope majestic-adr-023
./target/release/engram search "Gnomon synchronous"
```

Point another agent's pipeline stage at the same `--db engram.db` and it
reads the same memories back.

MCP (stdio), for wiring into Claude Code / Codex / any MCP client:

```sh
engram mcp --db /shared/engram.db
```

HTTP, local-only by default (no auth — do not expose beyond `127.0.0.1`
without adding a Bearer check first):

```sh
engram serve --db /shared/engram.db --addr 127.0.0.1:8420
curl -s -XPOST localhost:8420/v1/memory -d '{"agent":"kimi","scope":"x","content":"..."}'
curl -s "localhost:8420/v1/memory/recall?scope=x&limit=10"
```

## What's deliberately not here yet

- `--format yaml|csv|jsonl|explore` — only `--json` and human text exist.
  Extend `output::mode` and `output::envelope` when you need the rest.
- `--dry-run` on `remember` (it's not really destructive, but the CLI Standard §3 says
  every write command SHOULD accept it — add a no-op path if you want strict
  compliance).
- Packaging manifests (`packaging/guix.scm`, `packaging/default.nix`,
  `packaging/PKGBUILD`), Texinfo manual.
- Semantic (embedding) search — the upgrade path is `sqlite-vec` as a
  loadable extension alongside FTS5, not a replacement for it.
- Auth on the HTTP surface.

## License

GPL-3.0-or-later. Maintained by Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>.
https://Engram.SpacecraftSoftware.org/
