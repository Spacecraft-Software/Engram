# Engram

Shared verbatim chat memory for multi-model LLM pipelines. One SQLite file
(FTS5-indexed), three ways in: CLI, MCP (stdio), HTTP.

Point every model in your pipeline — Claude Code, Codex, Kimi, Ollama Cloud —
at the same `engram.db` (or the same `engram serve` instance) and they share
memory by construction. No LLM calls needed to write a memory: raw text in,
raw text out, full-text searchable.

## Status

**0.2.0-dev.** Builds and runs against the pinned `rmcp` 0.16. The MCP stdio
handshake tolerates a leading non-`initialize` probe (some hosts, e.g.
Antigravity, send a proprietary discovery request first) — see `src/mcp.rs`.

`cargo test` covers the rules subsystem (upsert semantics, the schema
migration, the markdown sync splice) **and** the memory surfaces (integration
tests in `tests/cli.rs`). CI is active (`.github/workflows/ci.yml`: rustfmt,
clippy, tests). Packaging manifests are present (`packaging/guix.scm`,
`packaging/default.nix`, `packaging/PKGBUILD`), and a Texinfo manual skeleton
lives in `doc/`.

**Breaking change at 0.2.0:** every HTTP route now returns real status codes
(400/404/500). Migration hint: check the response status — errors no longer
arrive as `200` with an `{"error":...}` body.

## Quick start

```sh
cargo build --release
./target/release/engram remember --agent claude-code --scope majestic-adr-023 "Decided: Gnomon stays synchronous, no Tokio in the reader."
./target/release/engram recall --scope majestic-adr-023
./target/release/engram search "Gnomon synchronous"
```

Point another agent's pipeline stage at the same `--db engram.db` and it
reads the same memories back.

Machine output beyond the default JSON envelope: `--format jsonl` streams one
metadata line then one line per record; `--format csv` writes RFC 4180 rows to
stdout with the metadata as a JSON line on stderr. `remember --dry-run`
validates and shows what would be stored without writing.

```sh
./target/release/engram recall --scope majestic-adr-023 --format jsonl
./target/release/engram recall --scope majestic-adr-023 --format csv
./target/release/engram remember --agent claude-code --scope majestic-adr-023 --dry-run "Would store this."
```

### Token budgeting and `context`

`recall` and `search` accept `--budget-tokens N`: results are packed to a token
budget (estimator `chars-div-4` — ceil(characters / 4); no single tokenizer is
correct across a multi-model pipeline). Recall drops the *oldest* first and
keeps the output chronological; search packs in rank order. The response
envelope then carries `metadata.budget` — requested/estimated tokens, included
and dropped counts, the dropped ids, and per-channel candidate counts. The same
`budget_tokens` option exists on the MCP `recall`/`search` tools and the HTTP
recall/search routes.

`engram context` assembles a session-start block in one call: the scope's
active rules first (**always all included**, even if they alone blow the
budget — policy is never silently dropped), then memories packed into what
remains. Without `--query` selection is newest-first; with `--query` the
recency and full-text channels are fused with reciprocal rank fusion (k=60),
so an old-but-relevant memory can beat a new-but-irrelevant one. Included
memories are always presented chronologically.

```sh
./target/release/engram recall --scope majestic-adr-023 --budget-tokens 500
./target/release/engram context --scope majestic-adr-023 --query "Gnomon" --budget-tokens 3000
curl -s "localhost:8420/v1/context?scope=majestic-adr-023&budget_tokens=3000"
```

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

## Rules — policy that outlives the session

A *memory* is something that happened. A *rule* is something that must keep
being true. Rules are stored once, keyed by a stable id, and rendered into the
markdown files agent harnesses load automatically — so they keep applying
without anyone remembering to restate them.

```sh
# Record a rule. Re-running with the same --id revises it in place.
engram rule add --id skill-description-1000 \
  "Do not ship or package a skill whose SKILL.md description field exceeds
   1000 characters. Claude rejects at 1024; 1000 is our margin."

# Read what's in effect.
engram rule list

# Render into AGENTS.md and CLAUDE.md at the project root.
engram rule sync

# Withdraw one when it stops applying, then re-sync.
engram rule retire --id skill-description-1000 && engram rule sync
```

`sync` rewrites **only** the region between its sentinels:

```markdown
<!-- engram:rules:begin scope="engram" count="1" -->
...generated...
<!-- engram:rules:end -->
```

Everything outside them is preserved verbatim, so the block can sit inside a
hand-written `CLAUDE.md` indefinitely. The rendered block is a pure function of
the rules — no generation timestamp — so re-running `sync` with unchanged rules
writes nothing at all. That makes it safe in a `SessionStart` hook, a pre-commit
gate, or a CI check (`engram rule sync --dry-run` reports `updated` if someone
hand-edited the block).

### Scope

Rules are grouped by scope, resolved first-match-wins:

| Order | Source |
|---|---|
| 1 | `--scope` (CLI) / `scope` (MCP) |
| 2 | `ENGRAM_SCOPE` |
| 3 | basename of the enclosing git working tree |
| 4 | basename of the current directory |

`sync` writes to the git working-tree root when there is one, not to wherever
the process was started. The resolved origin comes back in every response as
`scope_origin`, so a caller can tell an explicit choice from a directory-name
guess.

Under MCP, resolution uses the *server process's* working directory. Pass
`scope` explicitly whenever one engram server is shared across projects.

### Storing vs. surfacing

`rule add` writes to SQLite. SQLite is not in anyone's context window — the
`sync` step is what actually puts a rule in front of a model. Both surfaces say
so in their responses (`next_step`), because a stored-but-unsynced rule is read
by nobody.

### Retiring

`rule retire` withdraws a rule: it leaves `rule list`, and the next `sync` drops
it from the markdown. It is a **tombstone, not a delete** — engram is a memory
store, and erasing the record of a policy that once applied would defeat the
point. A retired rule stays in the database, still turns up in `engram search`,
and comes back from `rule list --include-retired` flagged `"retired": true`.
Re-running `rule add` with the same id reinstates it, because the id *is* the
rule's identity.

Retiring is idempotent (`already-retired`), and retiring an id that was never
recorded is an error (exit 3 / HTTP 404) rather than a silent success — a caller
that thinks it withdrew something should hear otherwise.

### MCP

Four tools, same semantics as the CLI: `rule_add`, `rule_list`, `rule_retire`,
`rule_sync`. Wire the server in and an agent can record a standing requirement
mid-session and have it apply to every session afterwards:

```jsonc
{"name": "rule_add", "arguments": {
  "rule_id": "signed-commits",
  "text": "Every commit must be signed and show Verified on GitHub.",
  "scope": "engram", "agent": "claude-code"}}
{"name": "rule_sync", "arguments": {"scope": "engram"}}
```

### HTTP

```sh
curl -s -XPOST localhost:8420/v1/rules \
  -d '{"rule_id":"signed-commits","text":"Every commit must be signed.","scope":"engram"}'
curl -s "localhost:8420/v1/rules?scope=engram"
curl -s "localhost:8420/v1/rules?scope=engram&include_retired=true"
curl -s -XDELETE "localhost:8420/v1/rules/signed-commits?scope=engram"
curl -s -XPOST localhost:8420/v1/rules/sync -d '{"scope":"engram"}'
```

`DELETE` retires (soft-deletes) rather than erases, per above. As of 0.2.0
**all** routes — rules and memory alike — return real status codes: 400 on a
malformed id, empty text, or empty `content`; 404 on an unknown rule; 500 on
storage failure. New routes should keep following that pattern.

> **`POST /v1/rules/sync` writes to your filesystem.** It is the only route that
> touches anything outside the database. Target paths come from the server
> process's own working directory and never from caller input, so there is no
> path-traversal surface — but combined with the no-auth posture it means any
> local process can rewrite that project's `AGENTS.md` and `CLAUDE.md`. The
> CLI's `--file` override is deliberately not exposed over HTTP. Weigh this
> before binding the server anywhere but `127.0.0.1`.

## What's deliberately not here yet

- `--format yaml` (deferred — `serde_yaml` is archived) and `--format explore`
  (no TUI yet). `json`, `jsonl`, and `csv` exist.
- An MCP `context` tool — `context` is CLI + HTTP for now; the MCP tool lands
  at M3 (the MCP `recall`/`search` tools do accept `budget_tokens` already).
- Purging a retired rule. Tombstones accumulate; there is no `rule purge`.
- Auth on the HTTP surface — now more consequential, since `POST /v1/rules/sync`
  writes files.
- Semantic (embedding) search — the upgrade path is `sqlite-vec` as a
  loadable extension alongside FTS5, not a replacement for it.

## License

GPL-3.0-or-later. Maintained by Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>.
https://Engram.SpacecraftSoftware.org/
