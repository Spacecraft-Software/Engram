<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Changelog

All notable changes to Engram. Dates are ISO 8601 UTC. The format loosely
follows [Keep a Changelog](https://keepachangelog.com/); versions follow
[semver](https://semver.org/) (pre-1.0: minor bumps may break).

## [Unreleased]

### Fixed

- **Three harness entries claimed a thing was impossible when it was not.**
  Antigravity writes a plain JSONL transcript alongside its protobuf stores;
  Copilot CLI's `turns` table is a flat pre-paired transcript, not an
  undocumented schema; and Kimi's project hash is reversible three ways. All
  three are now `NotImplemented` ("readable, reader not written") rather than
  `Unsupported`, with what was actually probed written down.

- **A project under a dotted directory was unreachable.** `mangle_cwd` replaced
  only `/`, but the harness replaces every character outside `[A-Za-z0-9_-]`, so
  anything beneath `.claude/worktrees/` — every worktree Claude Code creates —
  resolved to a directory that does not exist and reported `NOT_FOUND` as though
  the session were missing.
- **`ingest --cwd` now sets the scope.** It selected which transcripts to read
  while scope still resolved from the process's own directory, so importing many
  projects from one terminal filed them all under one scope and reported
  `scope_origin: "git-root"` while doing it. An explicit `--scope` still wins.

- **A skill description containing a colon silently broke the whole skill.**
  `description: Save this conversation: capture ...` is invalid YAML, so
  Antigravity loaded two of engram's three commands and reported nothing.
  Descriptions are now emitted as quoted scalars.

- **An unwritable target no longer aborts the whole install.** A read-only
  command or skills directory is now reported per file with its reason and the
  run continues; previously the `EROFS` propagated and every harness after the
  failing one was skipped entirely.
- **`is_nix_managed` follows a symlink *chain*.** It used a single `read_link`,
  so `~/.codex/skills` → `~/.agents/skills` → `~/.local/state/construct/current`
  → `/nix/store/…` was reported as writable right up until the write failed. It
  canonicalizes now.

- **Codex commands went to a directory Codex no longer reads.** Codex 0.149
  removed `~/.codex/prompts/` entirely and moved to skills; engram had been
  writing three prompt files there that nothing loaded. Engram now writes
  `~/.codex/skills/engram-<name>/SKILL.md`, the location Codex discovers
  automatically with no marketplace or config registration. Engram never
  deletes, so the stale `~/.codex/prompts/engram-*.md` files are left in place
  and can be removed by hand.

### Added

- **Four native readers, covering five more harnesses**: Opencode (and ZCode,
  whose CLI is an Opencode fork sharing its schema), Goose, Copilot CLI, and
  Qwen. `engram ingest` now reads nine harnesses without any manual export.

- **Ten more harnesses in the table** — grok, zcode, deepcode, poe-code, kilo,
  mimocode, warp, cline, aichat, bailian — recorded, not built for. An absent
  entry is indistinguishable from an unexamined one.
- **`export_command` per harness.** When `ingest` refuses a harness it cannot
  read, the hint now names that harness's own export command and points at
  `engram import`, instead of sending you to re-run the work through
  `remember`.

- **`engram import`** — reads chat transcripts that were exported to files, for
  the harnesses `ingest` has no reader for, and reads back `save-chat`'s own
  `.texi` archives. Three formats, detected by content: engram Texinfo (both
  dialects), Opencode Markdown, and Claude Code scrollback. Each file is filed
  into the scope of the project containing it; identity is a v5 uuid over the
  message's content, so re-importing inserts nothing and duplicate files
  collapse. `--input-format` forces a parser, `--recursive` descends.

- **VS Code** and **Cursor** are supported harnesses (the tenth and eleventh).
  VS Code gets `~/.config/Code/User/prompts/engram-<name>.prompt.md`; Cursor
  gets `~/.cursor/skills/engram-<name>/SKILL.md`. VS Code's `mcp.json` keys its
  servers under `servers`, which the MCP scanner now reads.
- `install` warns when two harnesses share a command or skills directory, so a
  duplicated slash command is explained rather than left to be noticed.

- **Kimi** is a supported harness (the ninth). Skills go to
  `~/.kimi-code/skills/engram-<name>/SKILL.md`; the MCP registration is read
  from `~/.kimi-code/mcp.json`. Its transcripts (`~/.kimi/sessions/<project>/
  <session>/context.jsonl`) are line-oriented and readable, but the project
  directory is a hash with no published mapping back to a working directory, so
  the reader stays `NotImplemented` with that reason recorded.
- **Qwen now gets skills.** It was reported as having no writable surface on the
  grounds that its "command format is unverified"; Qwen Code's own bundled
  `docs/features/skills.md` documents `~/.qwen/skills/<name>/SKILL.md`, so the
  claim was stale rather than true.

- `CommandSurface::Skill { dir }` — a bare skills root scanned directly by the
  harness, distinct from `Plugin { dir }`, which wraps skills in a plugin
  directory with a manifest.

### Added

- **OpenClaude** is a supported harness (the eighth). It is a Claude Code fork
  with its own config root: commands go to `~/.openclaude/commands/`, the MCP
  registration is read from `~/.openclaude.json`, and its transcripts are read
  by the existing Claude Code reader — the record types the fork adds are
  recognised rather than counted as format drift.
- **Antigravity** now gets a plugin at `~/.gemini/config/plugins/engram/`
  (`plugin.json` plus one `skills/engram-<name>/SKILL.md` per command). It has
  no slash-command directory at all; `agy plugin validate` reports a plugin's
  own `commands/` as "converted to skills", so engram writes skills directly.
- `install` reports `db_origin` (`override` / `registered` / `env` / `default`)
  alongside the database it pins, so the relative-`engram.db` fallback is
  visible rather than silent.

### Fixed

- **`install` now detects a stale database pin.** When a generated command
  points at a different database than the harness currently registers, the
  drift is reported on both the file and the harness before being corrected.
  Previously the two could diverge indefinitely: if a harness's registration
  moved after `install` ran, its `/engram-*` commands and its engram MCP tools
  read different stores with nothing to say so.
- **`find_git_root` requires a working tree, not merely a `.git` entry.** A
  directory must contain `.git/HEAD`; a `.git` file (worktree or submodule
  pointer) also counts. An empty `.git` directory in a shared location — e.g.
  `/tmp/.git` — previously captured every path beneath it, so `save-chat` would
  resolve its project root there, create `chat/`, and edit that directory's
  `.gitignore`.
- Harnesses with no writable surface each state their own reason instead of
  sharing one sentence that described none of them precisely.

### Changed

- `HarnessSpec` models its command surface as an enum —
  `CommandSurface::{Markdown, Plugin, None}` — replacing `commands_dir`,
  `command_file`, and the `command_frontmatter` bool. Antigravity's surface
  differs in artifact *shape*, not just in whether a header is read.

### Added

- **`flake.nix`** — Engram is now consumable as a Nix flake input
  (`github:Spacecraft-Software/Engram`), exposing `packages.default`,
  `packages.engram`, `apps.default`, `checks.default`, and `default`/`docs`
  dev shells. The package itself is still defined once in
  `packaging/default.nix` (Standard §5.5); the flake passes `srcOverride =
  self` so it builds the checkout rather than a tagged tarball, which is
  necessary while 0.6.0 is unreleased.

  There is deliberately no `nixosModule`. Engram is entirely `$HOME` state —
  its store, its `~/.claude/commands/` output, its MCP server spawned by a
  user's harness — so a `programs.engram.enable` whose only body is
  `environment.systemPackages` would advertise a system scope the tool does
  not have. Consumers install `packages.default` into `home.packages`.

### Fixed

- `packaging/default.nix` claimed `version = "0.5.0"` while `Cargo.toml` said
  `0.6.0-dev`, and hand-rolled a `builtins.path` source filter that excluded
  `.git`/`target`/`engram.db` but silently missed `chat/` and `research/`.
  It now takes Vacuum's `srcOverride ? null` parameter, so under a flake the
  source is the git tree and `.gitignore` does the filtering. (The argument is
  not named `src` because `callPackage` fills unbound arguments from nixpkgs,
  which has its own `src` attribute — a `src ? null` default is silently
  overridden.)
- The same `0.5.0` skew in `packaging/PKGBUILD`, `packaging/guix.scm` and
  `doc/engram.texi`. `engram --version` reads `CARGO_PKG_VERSION` and was
  always correct; only the packaging metadata and the manual were stale.
  `guix.scm` still does not build — its source hash is a placeholder of zeros
  — and now says so unambiguously instead of implying otherwise.

### Changed

- **Breaking (envelope).** The `filtered` histogram splits `unknown_record`
  into three counters, because one number meant three unrelated things and
  therefore could not be acted on. `unknown_record` now means only an
  unrecognized record `type` — a genuine format change, fixed by extending an
  allowlist. `torn_line` counts lines that are not valid JSON, from a write
  interrupted mid-flight. `missing_uuid` counts conversation records with no
  `uuid`, the only one of the three where a real turn is lost.

  The merge made every torn line read as a format change, which is what the
  counter is documented to signal. A session reading a transcript while its
  harness was still appending reported 56 "unknown records"; every one was a
  partial line that was complete minutes later, and none indicated any change
  in the format. Two false alarms in three teaches a reader to ignore the
  counter, which costs precisely the early warning it exists to give.

  Callers reading `filtered.unknown_record` will see a smaller number for the
  same transcript; the total across the three matches the old value.
- **Breaking (envelope).** `save-chat` replaces the boolean `gitignore_updated`
  with a self-describing `gitignore` object: `path` (which file), `entry`
  (`chat/`), `action` (`added` | `already-ignored` | `would-add`) and `detail`,
  a sentence naming engram as the actor. The boolean was ambiguous about whose
  `.gitignore` was meant and who had acted on it, and `gitignore_updated: false`
  read as a failure report when it actually meant engram correctly left an
  already-correct file alone. Both the CLI and the MCP `save_chat` tool
  serialize the same struct. Callers reading `gitignore_updated` must read
  `gitignore.action` instead.
- `save-chat --dry-run` no longer claims it updated `.gitignore`. The boolean
  returned `true` for a dry run — reporting an update that never happened —
  where the new report distinguishes `would-add` from `added`.

### Fixed

- The Claude Code reader recognizes `bridge-session` and `pr-link` as
  non-message records. Both appeared in transcripts after
  `NON_MESSAGE_TYPES` was written, so both were counted as `unknown_record` —
  the deliberate early-warning signal for a format change in a file engram
  does not own. In one 1372-line session they accounted for **every** such
  record: 55 `bridge-session` and 10 `pr-link`, reported as 65 unknown
  records. Neither carries a `message` at all (`bridge-session` is bridge
  bookkeeping — session ids and a sequence number; `pr-link` records a pull
  request opened from the session, re-appended on each update, so one PR
  yields several records), so **no conversation was ever dropped** and the
  ingested turn count was already complete. The bug was the false alarm
  itself: a warning that fires on every capture trains a reader to ignore the
  one signal that is supposed to mean something. Nothing about which turns get
  ingested changes.
  every other scoped command (`ENGRAM_SCOPE`, git working-tree name, directory
  name). It was the only scoped command requiring the flag, which made the
  generated `/engram-save-chat` slash command fail outright when invoked with
  no argument: the interpolated `$1` collapsed to nothing and clap rejected
  `--scope` as missing its value.
- `context --query ""` is treated as no query rather than a search for the
  empty string, matching how an empty `--scope` is treated.
- Generated command files place the banner *after* the YAML frontmatter. A
  harness parses frontmatter only when `---` opens the file, so the leading
  banner demoted the block to prose and every installed command advertised
  "Generated by `engram install`" as its description instead of what it does.
- The generated `/engram-context` command passes its argument to `--query`.
  It previously appended it as a bare positional, which `context` does not
  accept, so invoking the command with a topic failed.

### Changed

- Slash-command descriptions state what the command does for the user rather
  than naming the underlying subcommand.

Groundwork for the harness integration subsystem: before `save-chat` becomes
the command harnesses invoke by name, it had to stop corrupting its own
output.

### Fixed
- **`save-chat` no longer duplicates the history on every run.** Writing to an
  existing archive used to strip the trailing `@bye` and re-emit the entire
  scope, so each invocation appended a second copy of every message. The
  document is now rendered whole from a pure function of the scope, so a
  re-run over unchanged content is byte-identical and skips the write
  (`outcome: "unchanged"`).
- **`save-chat` no longer archives rules.** It read through `recall`, which
  returns `role="rule"` rows; an archive is a transcript, and rules have their
  own delivery mechanism.
- **`save-chat` no longer corrupts the decay signal.** Reading through
  `recall` bumped `access_count` on every exported row, making a whole scope
  look freshly used to `consolidate --report`. The new `Store::export_history`
  is untracked — archiving is not retrieval.
- **`save-chat` writes at the project root**, resolved the same way
  `rule sync` resolves it, instead of the process's current directory.
  Running it from a subdirectory used to scatter `chat/` directories and
  `.gitignore` edits through the tree.
- The generated Texinfo is now valid: it carries `@documentencoding UTF-8`
  (required for the non-ASCII content real transcripts contain) and a legal
  heading hierarchy. `makeinfo` compiles it without warnings, asserted by a
  test that skips when `makeinfo` is absent.

### Added
- **`engram ingest` — transcript capture.** Reads the session file a harness
  already writes for itself and stores each message as an ordinary memory, so
  `recall`, `search`, `context`, and `consolidate` see the real conversation
  rather than only the notes an agent chose to record. **Readers for Claude
  Code and Codex.** This is what makes "shared verbatim chat memory" literally
  true: until now, `save-chat` could only export what something had
  deliberately `remember`ed.
  - The two harnesses locate a session in incompatible ways, and neither
    approach generalizes: Claude Code names its directory after the working
    directory (mangled, and forward-only — a dash in a path makes the inverse
    ambiguous), while Codex encodes the *date* and records the working
    directory verbatim in each rollout's first line.
  - Codex carries the conversation twice. Engram prefers the display channel
    (`event_msg`) over the raw one (`response_item`) because the raw channel
    is *noisier*, not more complete: it held an extra "user message" that was
    an injected `<environment_context>` block.
  - Codex session ids are keyed on the rollout **file name**, not
    `session_meta.session_id`, which is reused across resumed sessions — three
    files sharing one id exist in the wild. Trusting it would have collided
    turn ids across rollouts and silently discarded messages.
  - `--max-bytes` (64 MiB default) refuses an oversized transcript rather than
    reading it by surprise; a 114 MB rollout exists on a real machine. Both
    readers stream line by line.
  - Roles `user`/`assistant` — values the schema has always declared and that
    nothing ever wrote.
  - **Tool payloads and thinking are excluded by default**, and a tool result
    is summarized to its size even with `--include-tools`. On a real 1.7 MB
    session: 935 records in, 46 turns out.
  - Credential-shaped substrings are redacted before storage and counted per
    kind in the response. Best-effort, not a guarantee.
  - Idempotent by construction: turn ids are UUID v5 over
    `(harness, session, record)`, so re-ingesting inserts nothing and
    resuming a live session inserts only the new tail.
  - `created_at` comes from the transcript, not the wall clock — `recall`
    orders by it, so stamping a conversation with one instant would destroy
    its reading order.
  - A harness with no reader exits 2 with a hint naming the fallback and
    writes **nothing** to stdout; an empty success would be indistinguishable
    from "there is nothing here".
- **`engram install` — the command surface.** Writes `/engram-save-chat`,
  `/engram-ingest`, and `/engram-context` into each detected harness's own
  command directory. Engram was already an MCP server nearly everywhere; this
  is what lets a user invoke it by name from inside a session.
  - Only detected harnesses; only files carrying engram's banner (a
    hand-written one is reported `skipped` with a reason unless `--force`);
    never a delete. Idempotent by byte comparison — a second run reports every
    file `unchanged` and does not touch mtimes.
  - Each command pins `--db` to the path that harness **already registered
    engram against**, discovered from its own MCP config (JSON, JSONC, or
    TOML, each scanned narrowly — no TOML dependency, and comments survive
    because engram only ever reads those files). Without this, a generated
    command would fall back to a relative default and write to a different
    store than the agents read.
  - Of the seven known harnesses, only Claude Code, Codex, and Opencode have a
    writable command surface. The other four are reported as having none
    rather than silently omitted.
  - CLI-only, and deliberately never an HTTP route: it writes into `$HOME`.
- **`save_chat` — the tenth and final MCP tool.** Archives a scope, and with
  `from_transcript: true` captures this harness's session first, so the MCP
  surface can record a conversation rather than only export one.
  - **No `file` argument.** The destination derives from the server's resolved
    project root. A caller-chosen path would be a traversal primitive handed
    to a model whose input includes attacker-influenceable text — the same
    reasoning that keeps `--file` off `rule_sync`'s MCP surface.
  - **The ten-tool ceiling is now reached.** An eleventh must displace an
    existing one, and the displacement must be argued in the manual. The
    ledger now lives in exactly one place; it had drifted across three.
- **`engram install --hooks`** — an opt-in `SessionEnd` hook (Claude Code
  only; no other surveyed harness has a hook system engram can write).
  - The hook runs **`ingest`, never `save-chat`**: capturing into the database
    is invisible and reversible, whereas writing a `.texi` into a repository
    at every session end, unasked, is not.
  - Merging into a settings file engram does not own is done carefully: a
    timestamped backup first, only engram's own entry replaced, **other hooks
    on the same event left alone** (several hooks per event is legitimate),
    and a file that does not parse is refused rather than overwritten.
- **`plugins/engram/`** — the Claude Code plugin, and the single source of
  truth for command bodies (`install.rs` embeds them with `include_str!`, so
  the two cannot drift). Ships `hooks/hooks.json` for the plugin path.
- **A `Harness Integration` chapter** in the manual, collecting the support
  matrix, the format-drift rules, the privacy posture, and the constraints on
  writing into a home directory.
- `src/archive.rs`: the Texinfo renderer and archive writer, lifted out of
  `main.rs` so the CLI and the MCP tool share one implementation.
  `transcript::capture` does the same for the capture path.
- `src/harness.rs`: a registry of the seven known harnesses, where transcript
  support is a typed variant carrying its reason rather than a `bool`.
- `save-chat --dry-run`, matching `remember` and `rule sync`.
- `save-chat` reports `scope_origin`, `root`, `gitignore_updated`, and a
  `file` object with the `created`/`updated`/`unchanged` outcome.
- `src/managed_file.rs`: the idempotent file-write primitive lifted out of
  `rules.rs`, now parameterized by a sentinel and a `WritePolicy` — `Spliced`
  for files engram shares with another author (`AGENTS.md`, `CLAUDE.md`),
  `Owned` for files engram authored outright (archives).
- Tests for `save-chat`, which previously had none: five integration tests and
  five unit tests over the renderer.

### Changed
- `serde_json` is now built with `preserve_order`. Without it, merging one
  hook into a user's `settings.json` would rewrite every key of that file in
  alphabetical order — engram reformatting a config it does not own as a side
  effect. Note this also makes engram's own JSON object output follow
  insertion order rather than alphabetical.
- **Breaking (envelope):** `save-chat`'s `metadata.command` is now
  `engram save-chat`, not `engram memory save-chat` — the only command that
  carried the stray `memory` noun. Its `data` shape changed accordingly
  (`file_path` → `file.path`).
- `--model`'s last-resort fallback is `unknown-model` rather than a specific
  model name, which was attributing archives to a model that had not written
  them.

## [0.5.0] — 2026-08-02

The research-driven retrieval generation (`research/research.md`), spanning
the 0.3/0.4 development cycle in one release.

### Added
- **Hybrid retrieval** (`--features vector`, opt-in; default build unchanged):
  Model2Vec static embeddings loaded from a **local directory only** — the
  network fetch path is compiled out; engram never downloads a model.
  `memory_vectors` BLOB side table + brute-force cosine; `engram index`
  backfill; `search --mode fts|hybrid` with auto-hybrid and structured
  errors on missing prerequisites. Shipping was gated on a held-out
  benchmark: hybrid 0.918 vs fts 0.856 recall@5 (+6.2 ≥ +5 → PASS,
  `bench/RESULTS.md`).
- **Extracted-fact index** (L0↔L1): deterministic marker-based extraction
  (`deterministic-v1`, never an LLM), verbatim facts with drill-down
  pointers, third RRF channel in `context`/hybrid; `consolidate --extract`.
- **Consolidation & decay**: `consolidate --dedup [--yes]` (near-duplicate
  groups superseded — never deleted — newest wins), `consolidate --report`
  (report-only contradiction surfacing + staleness/decay list); access
  tracking (`last_accessed_at`/`access_count`, internal) with CLI
  `--no-track` opt-out.
- MCP tools `get` and `context` (ledger: 9 tools); HTTP `GET /v1/memory/:id`,
  `GET /v1/context`.
- Benchmark corpus/harness: `bench/` (230-doc corpus, 65 frozen held-out
  queries, black-box eval driver).

### Security
- `cargo audit` assessed: RUSTSEC-2026-0189 (rmcp 0.16 Streamable-HTTP DNS
  rebinding) does not apply — engram compiles only rmcp's stdio transport;
  suppressed with reasoning in `.cargo/audit.toml`, to be dropped with the tracked
  rmcp ≥ 1.4 upgrade. Two unmaintained-crate warnings (`paste`,
  `number_prefix`) are transitive and advisory-only.

### Changed
- **FTS query semantics**: sanitized tokens are now joined with `OR` (BM25
  still ranks multi-token matches first). Measured on natural-language
  queries: recall@5 0.108 → 0.856. Result sets are looser than the old
  implicit-`AND` behavior; ranking and `--limit` preserve precision.
- The `memories_au` FTS trigger fires only on `content` updates, so access
  tracking and supersession no longer churn the FTS index.
- `consolidate` output is sectioned: `{extract?, dedup?, report?}`.

## [0.2.0] — 2026-08-01

MVP complete: compliance baseline, token budgeting, bi-temporal supersession.

### Added
- Test baseline across all surfaces (store/CLI/HTTP/MCP), CI (fmt, clippy
  `-D warnings`, feature-matrix tests, REUSE lint), packaging manifests
  (Guix/Nix/PKGBUILD), Texinfo manual, `CREDITS.md`.
- `--format json|jsonl|csv`, `--dry-run` on `remember`, `--accessible` +
  `SPACECRAFT_A11Y` (Standard §18), `[OK]`/`[ERROR]` status tags.
- Token-budgeted retrieval: `--budget-tokens` on `recall`/`search` (envelope
  `metadata.budget`, estimator `chars-div-4`), `engram context` session-start
  assembly (rules always included), RRF fusion core.
- Bi-temporal supersession: `valid_from`/`valid_to`/`superseded_by`,
  `remember --supersedes` (scope-local, conflict-safe), `--as-of` time
  travel, `--include-superseded`, `rule purge` (CLI-only tombstone delete).

### Changed
- **Breaking:** `/v1/memory*` HTTP routes return real status codes
  (400/404/409/500) instead of `200 OK` with an error body. Check the HTTP
  status; errors no longer arrive as 200.
- Corrected output-mode detection: `CI` truthiness, presence-based agent
  vars, `TERM=dumb`.

## [0.1.0] — 2026-07-25

Initial release: verbatim SQLite+FTS5 memory store; CLI/MCP/HTTP surfaces;
durable rules with sentinel-block sync to `AGENTS.md`/`CLAUDE.md`.
