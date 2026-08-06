// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
use clap::{Parser, Subcommand, ValueEnum};

/// Retrieval mode for `engram search` — explicit override of the auto rule.
///
/// Present in every build so the CLI schema is stable: without the `vector`
/// feature, `--mode hybrid` parses and then fails with a structured exit-2
/// error naming the missing feature.
#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum SearchMode {
    /// FTS5 full-text only — always available.
    Fts,
    /// FTS5 + vector channels fused with reciprocal rank fusion. Requires
    /// the vector feature, a resolvable local model, and an indexed scope.
    Hybrid,
}

/// Machine output formats — spacecraft-cli-standard §3 `--format`.
/// `yaml` is deferred (serde_yaml is archived); `explore` has no TUI yet.
#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Single JSON document with the metadata envelope (default machine mode).
    Json,
    /// Newline-delimited JSON: one metadata line, then one line per record.
    Jsonl,
    /// RFC 4180 CSV of the data records; metadata goes to stderr as JSON.
    Csv,
}

/// Engram — shared verbatim chat memory for multi-model LLM pipelines.
/// Maintained by Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
/// https://Engram.SpacecraftSoftware.org/
#[derive(Parser)]
#[command(name = "engram", version, about, long_about = None)]
pub struct Cli {
    /// Path to the shared SQLite database. All agents should point at the
    /// same file (or the same `engram serve` instance) to actually share memory.
    #[arg(long, env = "ENGRAM_DB", default_value = "engram.db")]
    pub db: std::path::PathBuf,

    /// Alias for --format json.
    #[arg(long, global = true)]
    pub json: bool,

    /// Machine output format (json, jsonl, csv). Overrides mode auto-detection.
    #[arg(long, global = true, value_enum)]
    pub format: Option<Format>,

    #[arg(long, global = true)]
    pub no_color: bool,

    /// Accessible output (Standard §18): plain linear text, no color, status
    /// tags. Also enabled by SPACECRAFT_A11Y=1; SPACECRAFT_A11Y=0 disables
    /// auto-detection but this flag still wins.
    #[arg(long, global = true)]
    pub accessible: bool,

    /// Read-only auditing: do not update access counts on reads. Without
    /// this flag, recall/search/context/get bump each returned memory's
    /// access_count and last_accessed_at (the decay signal for
    /// `consolidate --report`). CLI-only: the MCP and HTTP surfaces have no
    /// opt-out — agent reads are exactly what the tracking measures.
    #[arg(long, global = true)]
    pub no_track: bool,

    /// Local Model2Vec model directory for semantic (vector) search. First
    /// step of the model cascade: --model-path, then ENGRAM_MODEL, then
    /// $XDG_DATA_HOME/engram/model (default ~/.local/share/engram/model).
    /// Engram never downloads a model — install one manually. Only present
    /// in builds with the vector feature.
    #[cfg(feature = "vector")]
    #[arg(long, global = true)]
    pub model_path: Option<std::path::PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Store a verbatim message.
    Remember {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        scope: String,
        #[arg(long, default_value = "note")]
        role: String,
        /// Validate and show what would be stored, without writing.
        #[arg(long)]
        dry_run: bool,
        /// Close the validity window of this memory id (same scope) and
        /// record the new memory as its replacement. The old row is never
        /// deleted — recall it with --include-superseded or --as-of.
        #[arg(long)]
        supersedes: Option<String>,
        /// Message content. Reads stdin if omitted.
        content: Option<String>,
    },
    /// Read back the last N messages for a scope, chronological order.
    Recall {
        #[arg(long)]
        scope: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Pack results to a token budget (chars/4 estimate); the envelope
        /// reports included/dropped.
        #[arg(long)]
        budget_tokens: Option<u32>,
        /// Time-travel: show the memories that were valid at this ISO 8601
        /// UTC instant (e.g. 2026-08-01T12:00:00Z).
        #[arg(long)]
        as_of: Option<String>,
        /// Include superseded memories — the full verbatim history.
        #[arg(long, conflicts_with = "as_of")]
        include_superseded: bool,
    },
    /// Full-text search across stored memories.
    Search {
        query: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Pack results to a token budget (chars/4 estimate); the envelope
        /// reports included/dropped.
        #[arg(long)]
        budget_tokens: Option<u32>,
        /// Time-travel: search only memories valid at this ISO 8601 UTC instant.
        #[arg(long)]
        as_of: Option<String>,
        /// Include superseded memories — the full verbatim history.
        #[arg(long, conflicts_with = "as_of")]
        include_superseded: bool,
        /// Retrieval mode. Omitted: hybrid is used automatically when the
        /// vector feature is built in, a local model resolves, and the
        /// model's vectors are indexed — otherwise fts. An explicit
        /// `--mode hybrid` errors (exit 2) when a prerequisite is missing.
        #[arg(long, value_enum)]
        mode: Option<SearchMode>,
    },
    /// Assemble a budget-packed context block for session start: active
    /// rules first (always included), then recency+relevance fused memories.
    Context {
        /// Scope to assemble. Defaults to ENGRAM_SCOPE, else the git
        /// working-tree name, else the current directory name.
        #[arg(long)]
        scope: Option<String>,
        /// Optional full-text query; adds a relevance channel fused with
        /// recency (reciprocal rank fusion) when selecting memories.
        #[arg(long)]
        query: Option<String>,
        /// Token budget (chars/4 estimate) for the whole block.
        #[arg(long, default_value_t = 3000)]
        budget_tokens: u32,
        /// Candidates fetched per channel before packing.
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Idle-time maintenance: fact extraction, dedup, and decay reporting.
    ///
    /// Three combinable phases (at least one required): --extract runs the
    /// deterministic fact extractor (deterministic-v1 — marker-prefixed
    /// lines/sentences, never an LLM) over the current, non-rule memories
    /// and upserts into the facts index that boosts `context` and hybrid
    /// search; --dedup finds near-duplicate groups (normalized-exact text,
    /// plus stored-vector cosine when the hybrid gate passes) and, only
    /// with --yes, supersedes each group's older copies in favor of the
    /// newest (M2 semantics — nothing is ever deleted); --report is always
    /// report-only: suspected contradictions (a documented heuristic; a
    /// human or agent resolves via `remember --supersedes`, never this
    /// tool) plus the top 20 stalest memories by age and un-accessedness.
    /// Deliberately CLI-only: this command exists on neither the MCP nor
    /// the HTTP surface — maintenance is an operator's batch job, and
    /// agents receive its benefits through context/hybrid ranking instead.
    #[command(after_help = "\
EXAMPLES:
  # Extract facts from EVERY scope (deterministic, idempotent).
  engram consolidate --extract

  # Preview one scope first, then run it.
  engram consolidate --extract --scope my-task --dry-run
  engram consolidate --extract --scope my-task

  # See duplicate groups first; supersede the older copies only with --yes.
  engram consolidate --dedup
  engram consolidate --dedup --yes

  # Contradiction + decay report (always read-only), all phases at once.
  engram consolidate --report
  engram consolidate --extract --dedup --report
")]
    Consolidate {
        /// Run the fact-extraction phase (honors --dry-run).
        #[arg(long)]
        extract: bool,
        /// Run the near-duplicate detection phase. Report-only unless
        /// --yes is also passed.
        #[arg(long)]
        dedup: bool,
        /// Run the contradiction + decay report phase (always report-only).
        #[arg(long)]
        report: bool,
        /// Only consolidate this scope. Omitted: EVERY scope — unlike the
        /// rule commands there is no scope cascade here, because idle-time
        /// maintenance naturally spans the whole database.
        #[arg(long)]
        scope: Option<String>,
        /// Report what --extract would write without writing. Meaningful
        /// only for --extract; --dedup without --yes and --report are
        /// already read-only.
        #[arg(long)]
        dry_run: bool,
        /// Actually supersede the losers --dedup found. Consent must be a
        /// flag: this process may not have a TTY to ask on.
        #[arg(long)]
        yes: bool,
    },
    /// Durable project rules — policy that outlives a session.
    ///
    /// A rule is stored once and rendered into the markdown files agent
    /// harnesses load automatically, so it keeps applying without anyone
    /// remembering to restate it.
    Rule {
        #[command(subcommand)]
        action: RuleAction,
    },
    /// Embed stored memories into the vector index (vector feature).
    ///
    /// Backfills embeddings for every memory that has none yet under the
    /// resolved model — including memories written by surfaces that never
    /// embed live (MCP, HTTP) and history from before the model existed.
    /// Rule rows are skipped: policy is delivered through the rules section,
    /// not retrieval. Requires a local Model2Vec model; engram never
    /// downloads one.
    #[command(after_help = "\
EXAMPLES:
  # Index everything the resolved model has not embedded yet.
  engram index

  # Restrict to one scope, and preview first.
  engram index --scope my-task --dry-run
  engram index --scope my-task

  # Point at a model explicitly (cascade: --model-path, ENGRAM_MODEL,
  # $XDG_DATA_HOME/engram/model).
  engram --model-path ~/models/potion-base-8M index
")]
    Index {
        /// Only index memories in this scope. Omitted: every scope.
        #[arg(long)]
        scope: Option<String>,
        /// Memories fetched and embedded per batch.
        #[arg(long, default_value_t = 500)]
        batch: u32,
        /// Report how many memories would be embedded, without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run as an MCP server over stdio.
    Mcp,
    /// Run the HTTP API.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8420")]
        addr: String,
    },
    /// JSON Schema for engram's data types (CLI Standard introspection).
    Schema,
    /// Machine-readable capability manifest (CLI Standard introspection).
    Describe,
    /// Save a complete chat history for a scope to a Texinfo file.
    SaveChat {
        /// The scope of the chat to save.
        #[arg(long)]
        scope: String,

        /// Optional custom destination file path. Defaults to chat/<timestamp>.texi.
        #[arg(long)]
        file: Option<std::path::PathBuf>,

        /// Optional signature model/agent name (e.g. gpt-5.6-pro).
        #[arg(long)]
        model: Option<String>,

        /// Report the archive that would be written without writing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Capture a harness's own session transcript into a scope.
    ///
    /// Reads the transcript the harness already writes for itself and stores
    /// each message as an ordinary memory, so `recall`, `search`, `context`
    /// and `consolidate` see the real conversation rather than only the notes
    /// an agent chose to record.
    ///
    /// Tool payloads and model thinking are excluded by default: they are the
    /// bulk of a transcript, they are useless for retrieval, and they are
    /// where file contents and credentials live. Everything dropped is
    /// counted and reported.
    Ingest {
        /// Harness to read from. Detected from the environment, then the
        /// filesystem, when omitted.
        #[arg(long, value_enum)]
        harness: Option<crate::harness::Harness>,

        /// Session to read: an id, `latest` (default), or `all`.
        #[arg(long, default_value = "latest")]
        session: String,

        /// Scope to store into. Resolves like the `rule` commands when omitted.
        #[arg(long)]
        scope: Option<String>,

        /// Working directory whose sessions to look for. Defaults to the
        /// current directory.
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,

        /// Include the model's thinking blocks.
        #[arg(long)]
        include_thinking: bool,

        /// Include tool calls and results, summarized to one line each.
        /// Payloads are never stored, even with this flag.
        #[arg(long)]
        include_tools: bool,

        /// Include subagent side-chains.
        #[arg(long)]
        include_sidechains: bool,

        /// Refuse a transcript larger than this many bytes.
        #[arg(long, default_value_t = crate::transcript::DEFAULT_MAX_BYTES)]
        max_bytes: u64,

        /// Truncate any single turn longer than this many characters.
        #[arg(long, default_value_t = crate::transcript::DEFAULT_MAX_CHARS_PER_TURN)]
        max_chars_per_turn: usize,

        /// List the sessions available for this directory and write nothing.
        #[arg(long)]
        list: bool,

        /// Parse and filter, reporting what would be stored, without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Write engram's slash commands into the harnesses on this machine.
    ///
    /// Engram is usually already registered as an MCP server everywhere; what
    /// is missing is a command surface. This writes `/engram-save-chat`,
    /// `/engram-ingest`, and `/engram-context` into each detected harness's
    /// own command directory.
    ///
    /// Only files engram authored are ever overwritten, only detected
    /// harnesses are written to, and nothing is ever deleted. Start with
    /// `--list`.
    Install {
        /// Harnesses to install into. Every detected harness with a command
        /// surface when omitted.
        #[arg(long, value_enum)]
        harness: Vec<crate::harness::Harness>,

        /// Database path to bake into the generated commands. Defaults to the
        /// one the harness already registered engram against.
        #[arg(long)]
        db_path: Option<String>,

        /// Report what engram sees and writes nothing.
        #[arg(long)]
        list: bool,

        /// Report every file that would be written, without writing.
        #[arg(long)]
        dry_run: bool,

        /// Overwrite command files engram did not generate.
        #[arg(long)]
        force: bool,

        /// Also install an opt-in SessionEnd hook that captures each finished
        /// session. The hook runs `ingest` only: it never writes an archive
        /// into your repository. Merges into the harness's settings file,
        /// taking a timestamped backup first and leaving other hooks alone.
        #[arg(long)]
        hooks: bool,
    },
}

#[derive(Subcommand)]
pub enum RuleAction {
    /// Record a rule, or revise the existing rule with the same --id.
    ///
    /// Re-running with an id that already exists updates that rule in place
    /// rather than storing a second, competing copy. Storing a rule does not
    /// surface it to anyone — follow with `engram rule sync`.
    #[command(after_help = "\
EXAMPLES:
  # Record a rule; scope defaults to the git working-tree name.
  engram rule add --id skill-description-1000 \\
      \"Do not ship a skill whose SKILL.md description exceeds 1000 characters.\"

  # Revise it — same id, so this edits rather than duplicates.
  engram rule add --id skill-description-1000 \"Revised wording.\"

  # Long rules read better from a file.
  cat rule.txt | engram rule add --id no-unwrap --scope engram
")]
    Add {
        /// Stable kebab-case identifier, unique within the scope. Re-using one
        /// edits that rule.
        #[arg(long)]
        id: String,
        /// Scope to file the rule under. Defaults to ENGRAM_SCOPE, else the
        /// git working-tree name, else the current directory name.
        #[arg(long)]
        scope: Option<String>,
        /// Which agent/model is recording the rule.
        #[arg(long, env = "ENGRAM_AGENT", default_value = "engram-cli")]
        agent: String,
        /// The rule text. Reads stdin if omitted.
        text: Option<String>,
    },
    /// List the rules in effect for a scope, ordered by id.
    #[command(after_help = "\
EXAMPLES:
  engram rule list
  engram rule list --scope spacecraft-software --json
  engram rule list --include-retired
")]
    List {
        /// Scope to read. Defaults to the same cascade as `rule add`.
        #[arg(long)]
        scope: Option<String>,
        /// Also show withdrawn rules, flagged with "retired": true.
        #[arg(long)]
        include_retired: bool,
    },
    /// Withdraw a rule so it stops being binding.
    ///
    /// The rule is tombstoned, not deleted: it disappears from `rule list` and
    /// from synced files, but stays in the database and remains findable via
    /// `engram search`, because "we used to require this" is worth keeping.
    /// Re-running `rule add` with the same id reinstates it.
    #[command(after_help = "\
EXAMPLES:
  engram rule retire --id skill-description-1000
  engram rule retire --id old-policy --scope spacecraft-software

  # Retiring does not update the markdown; follow with a sync.
  engram rule retire --id old-policy && engram rule sync
")]
    Retire {
        /// Id of the rule to withdraw.
        #[arg(long)]
        id: String,
        /// Scope to read. Defaults to the same cascade as `rule add`.
        #[arg(long)]
        scope: Option<String>,
    },
    /// Render the scope's rules into AGENTS.md and CLAUDE.md.
    ///
    /// Rewrites only the region between the engram sentinels, leaving the rest
    /// of each file untouched. Running it twice with unchanged rules is a
    /// no-op, so it is safe to wire into a hook or a commit gate.
    #[command(after_help = "\
EXAMPLES:
  # Render into AGENTS.md and CLAUDE.md at the project root.
  engram rule sync

  # Read-only check: reports 'updated' if the block is stale or hand-edited.
  engram rule sync --dry-run

  # Write somewhere else instead.
  engram rule sync --file docs/RULES.md
")]
    Sync {
        /// Scope to render. Defaults to the same cascade as `rule add`.
        #[arg(long)]
        scope: Option<String>,
        /// Target file, repeatable. Relative paths resolve against the project
        /// root. Defaults to AGENTS.md and CLAUDE.md.
        #[arg(long = "file")]
        files: Vec<std::path::PathBuf>,
        /// Report what would be written without touching any file.
        #[arg(long)]
        dry_run: bool,
    },
    /// Permanently delete a RETIRED rule's row (tombstones only).
    ///
    /// The one destructive command in engram, and deliberately CLI-only:
    /// it exists on neither the MCP nor the HTTP surface, because
    /// destructive operations are not agent-invocable. An active rule is
    /// refused — retire it first.
    #[command(after_help = "\
EXAMPLES:
  # Preview what would be deleted.
  engram rule purge --id old-policy --dry-run

  # Actually delete (a retired rule only; --yes is required).
  engram rule purge --id old-policy --yes
")]
    Purge {
        /// Id of the retired rule to delete permanently.
        #[arg(long)]
        id: String,
        /// Scope to read. Defaults to the same cascade as `rule add`.
        #[arg(long)]
        scope: Option<String>,
        /// Confirm the deletion. Required: this process may not have a TTY
        /// to ask on, so consent must be explicit.
        #[arg(long)]
        yes: bool,
        /// Report what would be deleted without touching the database.
        #[arg(long)]
        dry_run: bool,
    },
}

// Rust guideline compliant 2026-05-18
