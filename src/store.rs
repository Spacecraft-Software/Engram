// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Verbatim chat memory store. SQLite + FTS5, single file, single source
//! of truth for every agent in the pipeline. No LLM calls to write a
//! memory — raw text in, raw text out.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

pub struct Store {
    conn: Connection,
    /// Whether reads bump the access-tracking columns (`access_count`,
    /// `last_accessed_at`). Defaults to `true`; the CLI's global
    /// `--no-track` flag turns it off for read-only auditing. The MCP and
    /// HTTP surfaces expose no opt-out of their own — agent reads are
    /// exactly the signal the decay report exists to measure.
    tracking: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Memory {
    pub id: String,
    /// Which agent/model wrote this (e.g. "claude-code", "codex", "kimi").
    pub agent: String,
    /// Free-form grouping key — a project, a task id, a pipeline run id.
    pub scope: String,
    pub role: String, // "user" | "assistant" | "system" | "note" | "rule"
    pub content: String,
    pub created_at: String, // ISO 8601 UTC
    /// Set only on rules: a stable, caller-chosen id, unique within the scope.
    /// Absent on ordinary messages, so plain-memory output is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// Set only on rules, which are mutable in place; messages are verbatim
    /// and never edited, so they carry only `created_at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Bi-temporal validity start. `created_at` is transaction time (when the
    /// row was written); `valid_from`/`valid_to` bound when the statement was
    /// *true*. `None` means "since `created_at`" — queries use
    /// `COALESCE(valid_from, created_at)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    /// Bi-temporal validity end. `None` means currently valid — which is how
    /// every row written before supersession existed stays valid by
    /// construction. Set (to the superseding write's timestamp) when the
    /// memory has been superseded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    /// Id of the memory that superseded this one. Set together with
    /// `valid_to` by [`Store::remember_superseding`], never independently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

/// One transcript turn on its way into the store, with a caller-supplied
/// deterministic id (see [`Store::ingest_turns`]).
#[derive(Debug, Clone)]
pub struct IngestTurn {
    pub id: String,
    pub agent: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
}

/// What one [`Store::ingest_turns`] call did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IngestReport {
    pub inserted: usize,
    /// Turns already present from an earlier ingest of the same session —
    /// the number that proves the operation is idempotent.
    pub skipped_existing: usize,
}

/// Which slice of the bi-temporal history a read should see.
///
/// `status` (the rules lifecycle column) is a separate axis entirely:
/// supersession never touches it, and this filter never reads it.
#[derive(Debug, Clone, Copy, Default)]
pub enum Validity<'a> {
    /// Only currently valid rows (`valid_to IS NULL`) — the default view.
    #[default]
    Current,
    /// Rows that were valid at the given ISO 8601 UTC instant: time travel
    /// over the validity dimension.
    AsOf(&'a str),
    /// Every row, superseded or not — the full verbatim history.
    All,
}

/// Outcome of a [`Store::remember_superseding`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupersedeOutcome {
    /// The target's validity window was closed and the new memory recorded.
    Superseded,
    /// No memory with that id exists in the caller's scope. Supersession is
    /// scope-local, so a target in another scope also reports this.
    NotFound,
    /// The target is a rule row. Rules have their own lifecycle
    /// (`rule add` / `rule retire`) and are never superseded through here.
    TargetIsRule,
    /// The target was already superseded; nothing was written.
    AlreadySuperseded,
}

/// Result of a superseding write, mirroring the [`RuleRetire`] outcome shape.
#[derive(Debug, Clone, Serialize)]
pub struct SupersedeResult {
    pub outcome: SupersedeOutcome,
    /// The newly stored memory — present only on [`SupersedeOutcome::Superseded`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<Memory>,
    /// Id of the memory whose validity window was closed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_id: Option<String>,
    /// On [`SupersedeOutcome::AlreadySuperseded`]: the id that already
    /// superseded the target, so the caller can name the winner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by_existing: Option<String>,
}

/// Outcome of a [`Store::rule_purge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PurgeOutcome {
    /// The retired rule's row (and its FTS entry) was permanently deleted.
    Purged,
    /// The rule exists but is still active; retire it first.
    NotRetired,
    /// No rule with this id exists in this scope.
    NotFound,
}

/// A durable project rule — the read model over rule-shaped `memories` rows.
///
/// Presented as its own type rather than as a `Memory` because every field is
/// non-optional here: agents consuming `rule list` should not have to reason
/// about a `rule_id` that might be null.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Rule {
    /// Stable, caller-chosen identifier, unique within the scope.
    pub rule_id: String,
    pub scope: String,
    /// The rule itself, verbatim.
    pub text: String,
    /// Which agent/model last wrote this rule.
    pub agent: String,
    pub created_at: String, // ISO 8601 UTC
    pub updated_at: String, // ISO 8601 UTC
    /// True when the rule has been withdrawn. Retired rules are excluded from
    /// [`Store::rules`] and never rendered into synced files, but are kept so
    /// the record of a policy that once applied survives.
    pub retired: bool,
}

/// Outcome of a [`Store::rule_retire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetireOutcome {
    /// The rule was active and is now retired.
    Retired,
    /// The rule was already retired; nothing changed. Retiring is idempotent.
    AlreadyRetired,
    /// No rule with this id exists in this scope.
    NotFound,
}

/// Result of retiring a rule, including the rule as it now stands.
#[derive(Debug, Clone, Serialize)]
pub struct RuleRetire {
    pub rule_id: String,
    pub scope: String,
    pub outcome: RetireOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<Rule>,
}

/// Result of a [`Store::rule_add`], distinguishing a new rule from an edit to
/// an existing one so callers can report which happened.
#[derive(Debug, Clone, Serialize)]
pub struct RuleUpsert {
    pub rule: Rule,
    /// False when an existing rule with this id was updated in place.
    pub created: bool,
}

/// A precomputed query embedding for the optional vector channel of
/// [`Store::context`] and the hybrid search path.
///
/// Callers build one only after the auto-hybrid gate passes (vector feature
/// compiled in, model resolved and loaded, at least one vector indexed) —
/// the store itself never touches an embedding model, it only compares
/// vectors it is handed.
#[derive(Debug, Clone, Copy)]
pub struct HybridQuery<'a> {
    /// Model name, matched against `memory_vectors.model`.
    pub model: &'a str,
    /// The embedded query.
    pub query_vec: &'a [f32],
}

/// Result of a [`Store::search_hybrid`]: the fused memories plus how many
/// candidates each channel contributed, so callers can report
/// `channels: {"fts": n, "vector": n, "facts": n}` in a budget envelope.
#[derive(Debug)]
pub struct HybridSearch {
    /// Fused results, best rank first (reciprocal-rank-fusion order).
    pub memories: Vec<Memory>,
    /// Candidates the FTS channel fed into the fusion.
    pub fts_candidates: usize,
    /// Candidates the vector channel fed into the fusion.
    pub vector_candidates: usize,
    /// Parent memories the extracted-fact channel fed into the fusion
    /// (deduped — a parent with several matching facts counts once).
    pub facts_candidates: usize,
}

/// Outcome of a [`Store::facts_extract`] pass, serialized as the
/// `engram consolidate --extract` payload.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ExtractReport {
    /// Current, non-rule memories walked.
    pub scanned: u64,
    /// Fact rows written — or, on a dry run, that would have been written.
    /// Deterministic ids make re-runs report the same number without
    /// growing the table.
    pub facts_written: u64,
    /// Memories that yielded at least one fact.
    pub memories_with_facts: u64,
}

/// One group of near-duplicate memories found by
/// [`Store::consolidate_dedup`].
#[derive(Debug, Clone, Serialize)]
pub struct DedupGroup {
    pub scope: String,
    /// The newest row of the group (max `created_at`, id as tie-break) —
    /// kept as the current truth.
    pub winner: String,
    /// The older rows: superseded when the run applies, merely reported
    /// otherwise.
    pub losers: Vec<String>,
    /// Which detector(s) connected the group: `"exact"` (normalized text)
    /// and/or `"vector"` (stored-embedding cosine).
    pub detectors: Vec<&'static str>,
}

/// Outcome of a [`Store::consolidate_dedup`] pass — the
/// `engram consolidate --dedup` payload.
#[derive(Debug, Clone, Serialize)]
pub struct DedupReport {
    /// Current, non-rule memories walked.
    pub scanned: u64,
    /// Duplicate groups found, each naming its winner and losers.
    pub groups: Vec<DedupGroup>,
    /// True when the losers were actually superseded (`--yes`); false on a
    /// report-only run, which writes nothing.
    pub applied: bool,
    /// Losers whose validity window was closed (0 on report-only).
    pub superseded: u64,
}

/// A suspected contradiction between two current memories — one half of the
/// `engram consolidate --report` payload.
///
/// Flagged by a deliberately crude heuristic (word overlap + a negation
/// marker on exactly one side); the tool never auto-resolves. A human or
/// agent inspects the pair and, if the contradiction is real, records the
/// truth with `engram remember --supersedes`.
#[derive(Debug, Clone, Serialize)]
pub struct ContradictionPair {
    pub scope: String,
    /// The older memory of the pair.
    pub a: String,
    /// The newer memory of the pair.
    pub b: String,
    /// Jaccard overlap of the lowercased word sets (>= 0.5 to be flagged).
    pub jaccard: f64,
    /// Id of the side carrying the negation marker.
    pub negated: String,
}

/// A stale-memory candidate from the decay scoring of
/// `engram consolidate --report`.
#[derive(Debug, Clone, Serialize)]
pub struct DecayCandidate {
    pub id: String,
    pub scope: String,
    /// Days since `created_at`.
    pub age_days: f64,
    /// Times a tracked read returned this memory (NULL column reads as 0).
    pub access_count: u64,
    /// When a tracked read last returned it; absent if never.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_accessed_at: Option<String>,
    /// `age_days * 1.0 + 30.0 / (1 + access_count)` — see
    /// [`Store::consolidate_report`].
    pub staleness: f64,
}

/// Outcome of a [`Store::consolidate_report`] pass — always report-only.
#[derive(Debug, Clone, Serialize)]
pub struct ConsolidateReport {
    /// Current, non-rule memories walked.
    pub scanned: u64,
    /// Suspected contradictions, for a human or agent to resolve.
    pub contradictions: Vec<ContradictionPair>,
    /// The top [`DECAY_TOP`] stalest memories.
    pub decay: Vec<DecayCandidate>,
}

/// Candidates fetched per channel before hybrid fusion. Both the FTS and the
/// vector channel feed their top-50 into [`crate::retrieval::rrf_fuse`].
const HYBRID_CHANNEL_LIMIT: u32 = 50;

/// Cosine threshold above which two stored vectors count as near-duplicates
/// in [`Store::consolidate_dedup`].
const DEDUP_COSINE_THRESHOLD: f32 = 0.92;

/// How many decay candidates [`Store::consolidate_report`] returns.
const DECAY_TOP: usize = 20;

/// Substrings whose presence marks a memory as "negated" for the
/// contradiction heuristic of [`Store::consolidate_report`]. Checked against
/// the lowercased content; the trailing spaces keep e.g. `notable` from
/// matching `not `.
const NEGATION_MARKERS: [&str; 8] = [
    "not ",
    "never ",
    "no longer ",
    "don't ",
    "do not ",
    "isn't ",
    "wasn't ",
    "stopped ",
];

/// A budget-packed context block assembled by [`Store::context`]: the
/// scope's active rules first, then the memories that fit the remaining
/// budget.
#[derive(Debug, Serialize)]
pub struct ContextResult {
    /// Active rules for the scope — always all of them, even over budget.
    pub rules: Vec<Rule>,
    /// Included memories, in chronological order for prompt reading.
    pub memories: Vec<Memory>,
    /// Estimated token cost of the rules section.
    pub rules_tokens: u32,
    /// What was kept, what was cut, and by which yardstick.
    pub budget: crate::retrieval::BudgetReport,
}

impl Store {
    /// Opens (creating if needed) the shared database file. Point every
    /// agent at the same path — that's the entire "shared memory" story.
    pub fn open(path: &std::path::Path) -> rusqlite::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// Opens a private in-memory database with exactly the same pragmas,
    /// schema, and migration as [`Store::open`]. Nothing is persisted and
    /// nothing is shared between connections — intended for tests and other
    /// ephemeral consumers that want the real schema without a file.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "test constructor; only in-crate tests call it today"
        )
    )]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    /// Shared body of [`Store::open`] and [`Store::open_in_memory`]: one
    /// place for pragmas + schema + migration, so the two cannot drift.
    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        // Required by the facts index (M4): `facts_extract` upserts with
        // INSERT OR REPLACE, and SQLite fires the conflict-resolution
        // DELETE's triggers only when recursive triggers are enabled.
        // Without this, re-extraction would leave ghost rows in the
        // external-content `facts_fts` index (the parent JOIN would still
        // filter them, but the index would silently drift out of sync).
        // No other engram trigger can recurse — they only write into FTS
        // virtual tables, which have no triggers of their own.
        conn.pragma_update(None, "recursive_triggers", true)?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memories (
                id         TEXT PRIMARY KEY,
                agent      TEXT NOT NULL,
                scope      TEXT NOT NULL,
                role       TEXT NOT NULL,
                content    TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_memories_scope ON memories(scope);
            CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at);

            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                content,
                content='memories',
                content_rowid='rowid'
            );

            CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, content) VALUES('delete', old.rowid, old.content);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, content) VALUES('delete', old.rowid, old.content);
                INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
            END;
            "#,
        )?;
        migrate(&conn)?;
        Ok(Self {
            conn,
            tracking: true,
        })
    }

    /// Turns read-path access tracking on or off for this handle.
    ///
    /// Off means `recall`/`search`/`search_hybrid`/`context`/`get` leave
    /// `access_count` and `last_accessed_at` untouched — the CLI's
    /// `--no-track` (read-only auditing). Tracking defaults to on.
    pub fn set_tracking(&mut self, on: bool) {
        self.tracking = on;
    }

    /// Bumps the access-tracking columns for the given memory ids: one
    /// batched `UPDATE` per 500-id chunk (SQLite's default host-parameter
    /// ceiling is 999; 500 leaves comfortable margin), all sharing one
    /// timestamp. A no-op when tracking is off or `ids` is empty.
    ///
    /// Called at the END of each tracked read, inside the same lock, and
    /// only for the memories actually *returned* — candidates that lost the
    /// fusion or the budget race are not touched. The columns are internal:
    /// [`row_to_memory`] never reads them, so `Memory` serialization is
    /// byte-identical with or without tracking.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if an update fails.
    fn track_access(&self, ids: &[String]) -> rusqlite::Result<()> {
        if !self.tracking || ids.is_empty() {
            return Ok(());
        }
        let now = crate::time::now_iso8601();
        for chunk in ids.chunks(500) {
            let placeholders: Vec<String> =
                (0..chunk.len()).map(|i| format!("?{}", i + 2)).collect();
            let sql = format!(
                "UPDATE memories
                 SET access_count = COALESCE(access_count, 0) + 1, last_accessed_at = ?1
                 WHERE id IN ({})",
                placeholders.join(", ")
            );
            let mut bind: Vec<&dyn rusqlite::ToSql> = vec![&now];
            for id in chunk {
                bind.push(id);
            }
            self.conn.execute(&sql, rusqlite::params_from_iter(bind))?;
        }
        Ok(())
    }

    pub fn remember(
        &self,
        agent: &str,
        scope: &str,
        role: &str,
        content: &str,
    ) -> rusqlite::Result<Memory> {
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            agent: agent.to_string(),
            scope: scope.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            created_at: crate::time::now_iso8601(),
            rule_id: None,
            updated_at: None,
            valid_from: None,
            valid_to: None,
            superseded_by: None,
        };
        self.conn.execute(
            "INSERT INTO memories (id, agent, scope, role, content, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![mem.id, mem.agent, mem.scope, mem.role, mem.content, mem.created_at],
        )?;
        Ok(mem)
    }

    /// Stores a new memory and closes the validity window of the memory it
    /// contradicts, in one transaction.
    ///
    /// Supersession is **scope-local**: the target is looked up by
    /// `(id, scope)`, so a target living in another scope reports
    /// [`SupersedeOutcome::NotFound`] — contradiction resolution never crosses
    /// scope boundaries. Rules are rejected ([`SupersedeOutcome::TargetIsRule`]);
    /// they have their own lifecycle (`rule add` / `rule retire`). A target
    /// whose window is already closed reports
    /// [`SupersedeOutcome::AlreadySuperseded`] and names the existing winner.
    /// On every non-`Superseded` outcome nothing is written.
    ///
    /// One timestamp serves both sides — the new row's `created_at` and the
    /// target's `valid_to` — so the chain has no gap and no overlap.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the transaction fails.
    pub fn remember_superseding(
        &self,
        agent: &str,
        scope: &str,
        role: &str,
        content: &str,
        supersedes: &str,
    ) -> rusqlite::Result<SupersedeResult> {
        // Read-then-write, so wrap both in a transaction (mirroring
        // `rule_add`): two processes sharing the file must not race between
        // the target check and the paired INSERT+UPDATE.
        let tx = self.conn.unchecked_transaction()?;

        let target: Option<(Option<String>, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT rule_id, valid_to, superseded_by FROM memories WHERE id = ?1 AND scope = ?2",
                params![supersedes, scope],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map(Some)
            .or_else(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    Ok(None)
                } else {
                    Err(e)
                }
            })?;

        // Dropping `tx` on the early returns rolls back, but nothing was
        // written on those paths anyway.
        let Some((rule_id, valid_to, superseded_by)) = target else {
            return Ok(SupersedeResult {
                outcome: SupersedeOutcome::NotFound,
                memory: None,
                superseded_id: None,
                superseded_by_existing: None,
            });
        };
        if rule_id.is_some() {
            return Ok(SupersedeResult {
                outcome: SupersedeOutcome::TargetIsRule,
                memory: None,
                superseded_id: None,
                superseded_by_existing: None,
            });
        }
        if valid_to.is_some() {
            return Ok(SupersedeResult {
                outcome: SupersedeOutcome::AlreadySuperseded,
                memory: None,
                superseded_id: None,
                superseded_by_existing: superseded_by,
            });
        }

        // One `now` for both sides: the target's window closes exactly where
        // the replacement's record begins.
        let now = crate::time::now_iso8601();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            agent: agent.to_string(),
            scope: scope.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            created_at: now.clone(),
            rule_id: None,
            updated_at: None,
            valid_from: None,
            valid_to: None,
            superseded_by: None,
        };
        tx.execute(
            "INSERT INTO memories (id, agent, scope, role, content, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![mem.id, mem.agent, mem.scope, mem.role, mem.content, mem.created_at],
        )?;
        tx.execute(
            "UPDATE memories SET valid_to = ?1, superseded_by = ?2 WHERE id = ?3",
            params![now, mem.id, supersedes],
        )?;
        tx.commit()?;

        Ok(SupersedeResult {
            outcome: SupersedeOutcome::Superseded,
            memory: Some(mem),
            superseded_id: Some(supersedes.to_string()),
            superseded_by_existing: None,
        })
    }

    /// Closes `loser_id`'s validity window in favor of an **existing**
    /// memory `winner_id`, at instant `now`.
    ///
    /// This reuses M2's supersession semantics — `valid_to` + `superseded_by`
    /// set together, the row never deleted, `Validity::Current` reads stop
    /// seeing it — but unlike [`Store::remember_superseding`] it inserts no
    /// new row: the winner already exists. It is the dedup primitive
    /// (`consolidate --dedup --yes`), where the newest copy of a duplicate
    /// group absorbs the older ones.
    ///
    /// The `valid_to IS NULL` guard makes it naturally idempotent and
    /// conflict-safe: a loser already superseded (by anything) is left
    /// untouched, and `false` is returned.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the update fails.
    pub fn mark_superseded_by(
        &self,
        loser_id: &str,
        winner_id: &str,
        now: &str,
    ) -> rusqlite::Result<bool> {
        let changed = self.conn.execute(
            "UPDATE memories SET valid_to = ?1, superseded_by = ?2
             WHERE id = ?3 AND valid_to IS NULL",
            params![now, winner_id, loser_id],
        )?;
        Ok(changed == 1)
    }

    /// Most recent memories in a scope, oldest last-N, chronological order.
    ///
    /// Rules are stored in this same table and are returned here alongside
    /// messages — recalling a scope should surface the policy that governs it.
    /// Use [`Store::rules`] when only rules are wanted. `validity` selects
    /// which slice of the bi-temporal history is visible. The returned rows'
    /// access counters are bumped (see [`Store::track_access`]).
    pub fn recall(
        &self,
        scope: &str,
        limit: u32,
        validity: Validity<'_>,
    ) -> rusqlite::Result<Vec<Memory>> {
        let out = self.recall_inner(scope, limit, validity)?;
        self.track_access(&out.iter().map(|m| m.id.clone()).collect::<Vec<_>>())?;
        Ok(out)
    }

    /// [`Store::recall`] without access tracking — the shared body, also
    /// used by composite reads ([`Store::context`]) that must only track
    /// what they finally return, never every candidate.
    fn recall_inner(
        &self,
        scope: &str,
        limit: u32,
        validity: Validity<'_>,
    ) -> rusqlite::Result<Vec<Memory>> {
        let (clause, as_of) = validity_filter(validity, "", 3);
        let sql = format!(
            "SELECT {MEMORY_COLUMNS} FROM memories
             WHERE scope = ?1 {clause} ORDER BY created_at DESC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut bind: Vec<&dyn rusqlite::ToSql> = vec![&scope, &limit];
        if let Some(t) = as_of.as_ref() {
            bind.push(t);
        }
        let rows = stmt.query_map(rusqlite::params_from_iter(bind), row_to_memory)?;
        let mut out: Vec<Memory> = rows.collect::<Result<_, _>>()?;
        out.reverse(); // chronological for reading back into a prompt
        Ok(out)
    }

    /// One transcript turn on its way into the store.
    ///
    /// Unlike [`Store::remember`], the id is supplied by the caller: it is a
    /// deterministic function of the source record (see
    /// [`crate::transcript::turn_id`]), which is what makes re-ingesting a
    /// session a no-op.
    ///
    /// Bulk-inserts transcript turns, skipping any already present.
    ///
    /// `INSERT OR IGNORE` inside one transaction. Two consequences worth
    /// stating: re-ingesting a session inserts nothing and resuming a live
    /// one inserts only the new tail; and because `OR IGNORE` never deletes,
    /// the external-content FTS `AFTER INSERT` trigger fires exactly for rows
    /// that really landed, so the index cannot drift. (Contrast
    /// [`Store::extract_facts`], which uses `INSERT OR REPLACE` and therefore
    /// depends on `recursive_triggers`.)
    ///
    /// `created_at` is the **transcript's** timestamp, not now. That bends
    /// the usual "created_at is transaction time" reading, and it has to:
    /// `recall` orders by `created_at`, so stamping a whole conversation with
    /// one wall-clock instant would destroy its reading order. `valid_from`
    /// is set to the same value so the bi-temporal view agrees.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the transaction fails.
    pub fn ingest_turns(
        &mut self,
        scope: &str,
        turns: &[IngestTurn],
    ) -> rusqlite::Result<IngestReport> {
        let tx = self.conn.transaction()?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO memories
                 (id, agent, scope, role, content, created_at, valid_from)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            )?;
            for turn in turns {
                inserted += stmt.execute(params![
                    turn.id,
                    turn.agent,
                    scope,
                    turn.role,
                    turn.content,
                    turn.created_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok(IngestReport {
            inserted,
            skipped_existing: turns.len() - inserted,
        })
    }

    /// Every message in a scope, chronological, for archival export.
    ///
    /// Three deliberate differences from [`Store::recall`], each of which was
    /// a defect when `save-chat` used `recall` instead:
    ///
    /// * **Untracked.** Archiving is not reading. Bumping `access_count` here
    ///   would make every exported memory look freshly used and corrupt the
    ///   decay signal `consolidate --report` computes — the same distinction
    ///   the `--no-track` flag exists to draw.
    /// * **Rules excluded.** An archive is a transcript. Rules have their own
    ///   delivery mechanism ([`crate::rules`]) and would otherwise interleave
    ///   into the narrative by `created_at`.
    /// * **No validity filter.** Superseded rows stay in the archive: it is a
    ///   record of what was said, not a view of what is currently true.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the query fails.
    pub fn export_history(&self, scope: &str) -> rusqlite::Result<Vec<Memory>> {
        let sql = format!(
            "SELECT {MEMORY_COLUMNS} FROM memories
             WHERE scope = ?1 AND role <> ?2 ORDER BY created_at ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![scope, crate::rules::ROLE], row_to_memory)?;
        rows.collect()
    }

    /// Full-text search across all scopes, or restricted to one scope.
    /// `validity` selects which slice of the bi-temporal history is visible.
    /// The returned rows' access counters are bumped
    /// (see [`Store::track_access`]).
    pub fn search(
        &self,
        query: &str,
        scope: Option<&str>,
        limit: u32,
        validity: Validity<'_>,
    ) -> rusqlite::Result<Vec<Memory>> {
        let out = self.search_inner(query, scope, limit, validity)?;
        self.track_access(&out.iter().map(|m| m.id.clone()).collect::<Vec<_>>())?;
        Ok(out)
    }

    /// [`Store::search`] without access tracking — the shared body, also
    /// the candidate feed for [`Store::search_hybrid`] and
    /// [`Store::context`], which track only their final output.
    fn search_inner(
        &self,
        query: &str,
        scope: Option<&str>,
        limit: u32,
        validity: Validity<'_>,
    ) -> rusqlite::Result<Vec<Memory>> {
        let query = &sanitize_fts_query(query);
        // The validity columns live on the memories side of the FTS join,
        // hence the `m.` qualifier.
        let (clause, as_of) = validity_filter(validity, "m.", if scope.is_some() { 4 } else { 3 });
        let sql = if scope.is_some() {
            format!(
                "SELECT {MEMORY_COLUMNS_M} FROM memories_fts f JOIN memories m ON m.rowid = f.rowid
                 WHERE memories_fts MATCH ?1 AND m.scope = ?2 {clause}
                 ORDER BY rank LIMIT ?3"
            )
        } else {
            format!(
                "SELECT {MEMORY_COLUMNS_M} FROM memories_fts f JOIN memories m ON m.rowid = f.rowid
                 WHERE memories_fts MATCH ?1 {clause}
                 ORDER BY rank LIMIT ?2"
            )
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let mut bind: Vec<&dyn rusqlite::ToSql> = vec![query];
        if let Some(s) = scope.as_ref() {
            bind.push(s);
        }
        bind.push(&limit);
        if let Some(t) = as_of.as_ref() {
            bind.push(t);
        }
        let rows = stmt.query_map(rusqlite::params_from_iter(bind), row_to_memory)?;
        rows.collect()
    }

    /// Stores (or replaces) the embedding for a memory.
    ///
    /// `INSERT OR REPLACE` keyed on `memory_id`: re-indexing after a model
    /// change overwrites in place, so a memory never carries two vectors.
    /// The blob is the raw little-endian f32 array. Not feature-gated —
    /// pure SQL, testable in every build with hand-built vectors.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the write fails.
    #[cfg_attr(
        all(not(feature = "vector"), not(test)),
        expect(
            dead_code,
            reason = "written to only by the vector-feature write paths (engram index, post-remember embedding) and by tests; the method stays compiled so the schema logic is one implementation"
        )
    )]
    pub fn vector_upsert(
        &self,
        memory_id: &str,
        model: &str,
        embedding: &[f32],
    ) -> rusqlite::Result<()> {
        let mut blob = Vec::with_capacity(embedding.len() * 4);
        for value in embedding {
            blob.extend_from_slice(&value.to_le_bytes());
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO memory_vectors (memory_id, model, dim, embedding)
             VALUES (?1, ?2, ?3, ?4)",
            params![memory_id, model, embedding.len() as i64, blob],
        )?;
        Ok(())
    }

    /// How many memories carry an embedding from `model`.
    ///
    /// This is the auto-hybrid gate's probe: zero means hybrid retrieval has
    /// nothing to add and search stays FTS5-only.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the query fails.
    #[cfg_attr(
        all(not(feature = "vector"), not(test)),
        expect(
            dead_code,
            reason = "probed only by the vector-feature auto-hybrid gate and by tests; compiled everywhere so the query logic is one implementation"
        )
    )]
    pub fn vector_count(&self, model: &str) -> rusqlite::Result<u64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM memory_vectors WHERE model = ?1",
            params![model],
            |row| row.get::<_, i64>(0).map(|n| n.max(0) as u64),
        )
    }

    /// Memories that have no embedding from `model` yet — the `engram index`
    /// work queue, oldest first so a resumed backfill stays deterministic.
    ///
    /// Rule rows are skipped: rules are policy, delivered through the rules
    /// section of every context block, not retrieval candidates. Superseded
    /// rows ARE included — hybrid search can read `--as-of`/`--include-superseded`
    /// slices, so history needs vectors too.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the query fails.
    #[cfg_attr(
        all(not(feature = "vector"), not(test)),
        expect(
            dead_code,
            reason = "read only by the vector-feature `engram index` command and by tests; compiled everywhere so the query logic is one implementation"
        )
    )]
    pub fn unindexed_memories(
        &self,
        model: &str,
        scope: Option<&str>,
        limit: u32,
    ) -> rusqlite::Result<Vec<Memory>> {
        let scope_clause = if scope.is_some() {
            "AND m.scope = ?3"
        } else {
            ""
        };
        let sql = format!(
            "SELECT {MEMORY_COLUMNS_M} FROM memories m
             WHERE m.rule_id IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM memory_vectors v
                   WHERE v.memory_id = m.id AND v.model = ?1
               )
               {scope_clause}
             ORDER BY m.created_at ASC, m.id ASC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut bind: Vec<&dyn rusqlite::ToSql> = vec![&model, &limit];
        if let Some(s) = scope.as_ref() {
            bind.push(s);
        }
        let rows = stmt.query_map(rusqlite::params_from_iter(bind), row_to_memory)?;
        rows.collect()
    }

    /// Nearest stored memories to a precomputed query vector, best first.
    ///
    /// Loads every candidate row for `model` (optionally narrowed to one
    /// scope and a validity slice via the shared [`validity_filter`]),
    /// brute-force cosines in Rust, sorts descending, and truncates to
    /// `limit`. Brute force is the deliberate no-index choice — see
    /// [`crate::embed::cosine`]. Rule rows never qualify, mirroring
    /// [`Store::unindexed_memories`]. Not feature-gated: it takes a
    /// precomputed vector, so it is testable without any model.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the query fails.
    pub fn vector_candidates(
        &self,
        query_vec: &[f32],
        model: &str,
        scope: Option<&str>,
        limit: u32,
        validity: Validity<'_>,
    ) -> rusqlite::Result<Vec<(String, f32)>> {
        let (clause, as_of) = validity_filter(validity, "m.", if scope.is_some() { 3 } else { 2 });
        let sql = if scope.is_some() {
            format!(
                "SELECT v.memory_id, v.embedding
                 FROM memory_vectors v JOIN memories m ON m.id = v.memory_id
                 WHERE v.model = ?1 AND m.scope = ?2 AND m.rule_id IS NULL {clause}"
            )
        } else {
            format!(
                "SELECT v.memory_id, v.embedding
                 FROM memory_vectors v JOIN memories m ON m.id = v.memory_id
                 WHERE v.model = ?1 AND m.rule_id IS NULL {clause}"
            )
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let mut bind: Vec<&dyn rusqlite::ToSql> = vec![&model];
        if let Some(s) = scope.as_ref() {
            bind.push(s);
        }
        if let Some(t) = as_of.as_ref() {
            bind.push(t);
        }
        let rows = stmt.query_map(rusqlite::params_from_iter(bind), |row| {
            let id: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((id, blob))
        })?;

        let mut scored: Vec<(String, f32)> = Vec::new();
        for row in rows {
            let (id, blob) = row?;
            let vec: Vec<f32> = blob
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().expect("chunks_exact yields 4 bytes")))
                .collect();
            // A dimension mismatch (different-width model under the same
            // name) cosines to 0.0 rather than erroring — see `cosine`.
            scored.push((id, crate::embed::cosine(query_vec, &vec)));
        }
        // Deterministic total order: score descending, id ascending.
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(scored)
    }

    /// Runs the deterministic fact extractor over every CURRENT, non-rule
    /// memory (optionally narrowed to one scope) and upserts the results
    /// into the `facts` index.
    ///
    /// Extraction is **append-only in v1**: `INSERT OR REPLACE` keyed on the
    /// deterministic [`crate::facts::fact_id`] is the only write — facts
    /// whose parent has since been superseded, or which a changed extractor
    /// would no longer produce, are *not* deleted here. Stale facts are
    /// filtered at **query time** instead, through the parent JOIN in
    /// [`Store::fact_candidates`] — the same never-delete posture as the
    /// rest of the store. Deterministic ids make the pass idempotent:
    /// re-running rewrites the same rows and the table does not grow.
    ///
    /// `scope = None` means **every scope** — deliberately unlike the rule
    /// commands' cascade, because idle-time maintenance naturally spans the
    /// whole database. With `dry_run`, nothing is written and the report
    /// counts what a real run would have written.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if a query or the write
    /// transaction fails.
    pub fn facts_extract(
        &self,
        scope: Option<&str>,
        dry_run: bool,
    ) -> rusqlite::Result<ExtractReport> {
        let scope_clause = if scope.is_some() {
            "AND scope = ?1"
        } else {
            ""
        };
        // Deterministic walk order (it does not change the outcome — ids
        // are content-derived — but it keeps runs comparable in a trace).
        let sql = format!(
            "SELECT id, scope, content FROM memories
             WHERE valid_to IS NULL AND rule_id IS NULL {scope_clause}
             ORDER BY created_at ASC, id ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut bind: Vec<&dyn rusqlite::ToSql> = Vec::new();
        if let Some(s) = scope.as_ref() {
            bind.push(s);
        }
        let rows = stmt.query_map(rusqlite::params_from_iter(bind), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let memories: Vec<(String, String, String)> = rows.collect::<Result<_, _>>()?;

        // One timestamp for the whole pass; one transaction for all writes.
        let now = crate::time::now_iso8601();
        let tx = if dry_run {
            None
        } else {
            Some(self.conn.unchecked_transaction()?)
        };
        let mut report = ExtractReport {
            scanned: 0,
            facts_written: 0,
            memories_with_facts: 0,
        };
        for (memory_id, memory_scope, content) in &memories {
            report.scanned += 1;
            let extracted = crate::facts::extract(content);
            if extracted.is_empty() {
                continue;
            }
            report.memories_with_facts += 1;
            for fact in &extracted {
                if let Some(tx) = tx.as_ref() {
                    tx.execute(
                        "INSERT OR REPLACE INTO facts
                             (id, memory_id, scope, fact, extractor, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            crate::facts::fact_id(memory_id, fact),
                            memory_id,
                            memory_scope,
                            fact,
                            crate::facts::EXTRACTOR,
                            now
                        ],
                    )?;
                }
                report.facts_written += 1;
            }
        }
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok(report)
    }

    /// Finds near-duplicate groups among the CURRENT, non-rule memories of
    /// each scope, and — when `apply` — supersedes every loser in favor of
    /// its group's newest row.
    ///
    /// Two detectors run and their edges are **unioned** into groups
    /// (connected components):
    ///
    /// - **exact** — normalized text equality: trimmed, lowercased, internal
    ///   whitespace collapsed to single spaces. Always on.
    /// - **vector** — cosine >= [`DEDUP_COSINE_THRESHOLD`] between *stored*
    ///   embeddings under `vector_model`, same-scope pairs only. Runs only
    ///   when the caller passes a model (the CLI passes one exactly when the
    ///   vector feature is built in, a model resolves, and that model has
    ///   indexed vectors — the same gate as auto-hybrid). Pairwise over each
    ///   scope: O(n²), the same deliberate no-index posture as brute-force
    ///   cosine search.
    ///
    /// Each group keeps its NEWEST row (max `created_at`, id as tie-break)
    /// as the winner. With `apply`, every loser goes through
    /// [`Store::mark_superseded_by`] — M2 supersession semantics, no new row
    /// inserted, **nothing is ever deleted** — in one transaction sharing
    /// one timestamp. The pass is idempotent: superseded losers are no
    /// longer Current, so a second run finds nothing.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if a query or the write
    /// transaction fails.
    pub fn consolidate_dedup(
        &self,
        scope: Option<&str>,
        vector_model: Option<&str>,
        apply: bool,
    ) -> rusqlite::Result<DedupReport> {
        use std::collections::HashMap;

        let scope_clause = if scope.is_some() {
            "AND scope = ?1"
        } else {
            ""
        };
        // Deterministic walk order; the index into `rows` is the node id for
        // the union-find below.
        let sql = format!(
            "SELECT id, scope, content, created_at FROM memories
             WHERE valid_to IS NULL AND rule_id IS NULL {scope_clause}
             ORDER BY created_at ASC, id ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut bind: Vec<&dyn rusqlite::ToSql> = Vec::new();
        if let Some(s) = scope.as_ref() {
            bind.push(s);
        }
        let fetched = stmt.query_map(rusqlite::params_from_iter(bind), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let rows: Vec<(String, String, String, String)> = fetched.collect::<Result<_, _>>()?;

        // Union-find over row indices; every duplicate edge unions its ends.
        let mut parent: Vec<usize> = (0..rows.len()).collect();
        fn find(parent: &mut [usize], i: usize) -> usize {
            let mut root = i;
            while parent[root] != root {
                root = parent[root];
            }
            let mut walk = i;
            while parent[walk] != root {
                let next = parent[walk];
                parent[walk] = root;
                walk = next;
            }
            root
        }
        fn union(parent: &mut [usize], a: usize, b: usize) {
            let ra = find(parent, a);
            let rb = find(parent, b);
            if ra != rb {
                parent[rb] = ra;
            }
        }

        // Detector 1 (always on): normalized-exact text within a scope.
        let mut exact_edges: Vec<(usize, usize)> = Vec::new();
        let mut by_norm: HashMap<(String, String), usize> = HashMap::new();
        for (index, (_, row_scope, content, _)) in rows.iter().enumerate() {
            let key = (row_scope.clone(), normalize_for_dedup(content));
            match by_norm.get(&key) {
                Some(&first) => {
                    exact_edges.push((first, index));
                    union(&mut parent, first, index);
                }
                None => {
                    by_norm.insert(key, index);
                }
            }
        }

        // Detector 2 (gated by the caller): stored-vector cosine, pairwise
        // within each scope.
        let mut vector_edges: Vec<(usize, usize)> = Vec::new();
        if let Some(model) = vector_model {
            let mut stmt = self
                .conn
                .prepare("SELECT memory_id, embedding FROM memory_vectors WHERE model = ?1")?;
            let fetched = stmt.query_map(params![model], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            let mut vectors: HashMap<String, Vec<f32>> = HashMap::new();
            for row in fetched {
                let (id, blob) = row?;
                vectors.insert(
                    id,
                    blob.chunks_exact(4)
                        .map(|b| {
                            f32::from_le_bytes(b.try_into().expect("chunks_exact yields 4 bytes"))
                        })
                        .collect(),
                );
            }
            for i in 0..rows.len() {
                let Some(vec_i) = vectors.get(&rows[i].0) else {
                    continue;
                };
                for j in (i + 1)..rows.len() {
                    if rows[i].1 != rows[j].1 {
                        continue; // pairs are same-scope only
                    }
                    let Some(vec_j) = vectors.get(&rows[j].0) else {
                        continue;
                    };
                    if crate::embed::cosine(vec_i, vec_j) >= DEDUP_COSINE_THRESHOLD {
                        vector_edges.push((i, j));
                        union(&mut parent, i, j);
                    }
                }
            }
        }

        // Connected components of size >= 2 are the duplicate groups.
        let mut components: HashMap<usize, Vec<usize>> = HashMap::new();
        for index in 0..rows.len() {
            let root = find(&mut parent, index);
            components.entry(root).or_default().push(index);
        }
        let mut exact_roots: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for (a, _) in &exact_edges {
            exact_roots.insert(find(&mut parent, *a));
        }
        let mut vector_roots: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for (a, _) in &vector_edges {
            vector_roots.insert(find(&mut parent, *a));
        }

        let mut groups: Vec<DedupGroup> = Vec::new();
        for (root, mut members) in components {
            if members.len() < 2 {
                continue;
            }
            // Winner: newest by created_at, id as tie-break — the last
            // element once sorted ascending.
            members.sort_by(|&a, &b| {
                rows[a]
                    .3
                    .cmp(&rows[b].3)
                    .then_with(|| rows[a].0.cmp(&rows[b].0))
            });
            let winner = members.pop().expect("component has >= 2 members");
            let mut detectors: Vec<&'static str> = Vec::new();
            if exact_roots.contains(&root) {
                detectors.push("exact");
            }
            if vector_roots.contains(&root) {
                detectors.push("vector");
            }
            groups.push(DedupGroup {
                scope: rows[winner].1.clone(),
                winner: rows[winner].0.clone(),
                losers: members.iter().map(|&i| rows[i].0.clone()).collect(),
                detectors,
            });
        }
        // Deterministic report order regardless of HashMap iteration.
        groups.sort_by(|a, b| a.scope.cmp(&b.scope).then_with(|| a.winner.cmp(&b.winner)));

        let mut superseded: u64 = 0;
        if apply && !groups.is_empty() {
            // One transaction, one timestamp, for the whole apply pass.
            let now = crate::time::now_iso8601();
            let tx = self.conn.unchecked_transaction()?;
            for group in &groups {
                for loser in &group.losers {
                    if self.mark_superseded_by(loser, &group.winner, &now)? {
                        superseded += 1;
                    }
                }
            }
            tx.commit()?;
        }

        Ok(DedupReport {
            scanned: rows.len() as u64,
            groups,
            applied: apply,
            superseded,
        })
    }

    /// Report-only maintenance analysis over the CURRENT, non-rule memories
    /// (optionally one scope): suspected contradictions plus decay scoring.
    ///
    /// **Contradictions** are pairs of same-scope memories where the Jaccard
    /// overlap of the lowercased word sets is >= 0.5 AND exactly one side
    /// contains a [`NEGATION_MARKERS`] substring. This is a deliberately
    /// crude heuristic — it will both miss real contradictions and flag
    /// rephrasings — and the tool therefore **never auto-resolves**: a human
    /// or agent inspects each pair and records the truth with
    /// `remember --supersedes`. Pairwise per scope, O(n²) — the usual
    /// engram-scale posture.
    ///
    /// **Decay** scores every memory
    /// `staleness = age_days * 1.0 + 30.0 / (1 + access_count)` — crude but
    /// monotone in both age and un-accessedness — and returns the top
    /// [`DECAY_TOP`]. Age is computed from `created_at` against
    /// [`crate::time::now_iso8601`] via `jiff`; a `created_at` that fails to
    /// parse (only possible for rows engram did not write) scores age 0
    /// rather than failing the report.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the query fails.
    pub fn consolidate_report(&self, scope: Option<&str>) -> rusqlite::Result<ConsolidateReport> {
        let scope_clause = if scope.is_some() {
            "AND scope = ?1"
        } else {
            ""
        };
        let sql = format!(
            "SELECT id, scope, content, created_at, access_count, last_accessed_at
             FROM memories
             WHERE valid_to IS NULL AND rule_id IS NULL {scope_clause}
             ORDER BY created_at ASC, id ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut bind: Vec<&dyn rusqlite::ToSql> = Vec::new();
        if let Some(s) = scope.as_ref() {
            bind.push(s);
        }
        struct Row {
            id: String,
            scope: String,
            content: String,
            created_at: String,
            access_count: u64,
            last_accessed_at: Option<String>,
        }
        let fetched = stmt.query_map(rusqlite::params_from_iter(bind), |row| {
            Ok(Row {
                id: row.get(0)?,
                scope: row.get(1)?,
                content: row.get(2)?,
                created_at: row.get(3)?,
                access_count: row.get::<_, Option<i64>>(4)?.map_or(0, |n| n.max(0) as u64),
                last_accessed_at: row.get(5)?,
            })
        })?;
        let rows: Vec<Row> = fetched.collect::<Result<_, _>>()?;

        // Contradiction heuristic: word sets and negation flags, once per row.
        let word_sets: Vec<std::collections::HashSet<String>> = rows
            .iter()
            .map(|r| {
                r.content
                    .to_lowercase()
                    .split_whitespace()
                    .map(str::to_string)
                    .collect()
            })
            .collect();
        let negated: Vec<bool> = rows
            .iter()
            .map(|r| {
                let lower = r.content.to_lowercase();
                NEGATION_MARKERS.iter().any(|m| lower.contains(m))
            })
            .collect();
        let mut contradictions: Vec<ContradictionPair> = Vec::new();
        for i in 0..rows.len() {
            for j in (i + 1)..rows.len() {
                if rows[i].scope != rows[j].scope || negated[i] == negated[j] {
                    continue;
                }
                let intersection = word_sets[i].intersection(&word_sets[j]).count();
                let union = word_sets[i].union(&word_sets[j]).count();
                if union == 0 {
                    continue;
                }
                // Set sizes are tiny (bounded by content length); the
                // usize→f64 casts are lossless in practice.
                let jaccard = intersection as f64 / union as f64;
                if jaccard >= 0.5 {
                    contradictions.push(ContradictionPair {
                        scope: rows[i].scope.clone(),
                        a: rows[i].id.clone(),
                        b: rows[j].id.clone(),
                        jaccard,
                        negated: if negated[i] {
                            rows[i].id.clone()
                        } else {
                            rows[j].id.clone()
                        },
                    });
                }
            }
        }

        // Decay scoring.
        let now = crate::time::now_iso8601();
        let now_ts: jiff::Timestamp = now.parse().expect("now_iso8601 emits a valid timestamp");
        let mut decay: Vec<DecayCandidate> = rows
            .iter()
            .map(|r| {
                let age_days = r
                    .created_at
                    .parse::<jiff::Timestamp>()
                    .map(|ts| ((now_ts.as_second() - ts.as_second()).max(0)) as f64 / 86_400.0)
                    .unwrap_or(0.0);
                DecayCandidate {
                    id: r.id.clone(),
                    scope: r.scope.clone(),
                    age_days,
                    access_count: r.access_count,
                    last_accessed_at: r.last_accessed_at.clone(),
                    // staleness = age_days * 1.0 + 30.0 / (1 + access_count):
                    // the age weight is 1.0 per day (written implicitly).
                    staleness: age_days + 30.0 / (1.0 + r.access_count as f64),
                }
            })
            .collect();
        decay.sort_by(|a, b| {
            b.staleness
                .total_cmp(&a.staleness)
                .then_with(|| a.id.cmp(&b.id))
        });
        decay.truncate(DECAY_TOP);

        Ok(ConsolidateReport {
            scanned: rows.len() as u64,
            contradictions,
            decay,
        })
    }

    /// Parent memory ids whose extracted facts match `query`, deduped, in
    /// FTS rank order — the facts retrieval channel.
    ///
    /// The validity clause is applied to the **parent** columns (`m.`):
    /// the fact rows' own `valid_to`/`superseded_by` are reserved and stay
    /// NULL, so a fact is live exactly as long as its parent memory is.
    /// A parent with several matching facts appears once, at its best
    /// rank. No SQL `LIMIT`: duplicates would eat limit slots before the
    /// dedupe, so all matches are fetched and truncated after — fine at
    /// engram's scale, the same no-index reasoning as brute-force cosine.
    /// The `m.rule_id IS NULL` filter is belt-and-braces; rule rows are
    /// never extracted from in the first place.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the query fails —
    /// including the FTS syntax error an empty `query` produces.
    pub fn fact_candidates(
        &self,
        query: &str,
        scope: Option<&str>,
        limit: u32,
        validity: Validity<'_>,
    ) -> rusqlite::Result<Vec<String>> {
        let query = &sanitize_fts_query(query);
        let (clause, as_of) = validity_filter(validity, "m.", if scope.is_some() { 3 } else { 2 });
        let sql = if scope.is_some() {
            format!(
                "SELECT f.memory_id FROM facts_fts ff
                 JOIN facts f ON f.rowid = ff.rowid
                 JOIN memories m ON m.id = f.memory_id
                 WHERE facts_fts MATCH ?1 AND m.scope = ?2 AND m.rule_id IS NULL {clause}
                 ORDER BY rank"
            )
        } else {
            format!(
                "SELECT f.memory_id FROM facts_fts ff
                 JOIN facts f ON f.rowid = ff.rowid
                 JOIN memories m ON m.id = f.memory_id
                 WHERE facts_fts MATCH ?1 AND m.rule_id IS NULL {clause}
                 ORDER BY rank"
            )
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let mut bind: Vec<&dyn rusqlite::ToSql> = vec![query];
        if let Some(s) = scope.as_ref() {
            bind.push(s);
        }
        if let Some(t) = as_of.as_ref() {
            bind.push(t);
        }
        let rows = stmt.query_map(rusqlite::params_from_iter(bind), |row| {
            row.get::<_, String>(0)
        })?;

        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut parents: Vec<String> = Vec::new();
        for row in rows {
            let id = row?;
            if seen.insert(id.clone()) {
                parents.push(id);
                if parents.len() >= limit {
                    break;
                }
            }
        }
        Ok(parents)
    }

    /// Hybrid retrieval: the FTS5, vector, and extracted-fact channels
    /// (top-[`HYBRID_CHANNEL_LIMIT`] each) fused by reciprocal rank fusion
    /// (k = [`crate::retrieval::RRF_K`]), truncated to `limit`.
    ///
    /// The facts channel ([`Store::fact_candidates`]) can never surface a
    /// memory FTS cannot — facts are verbatim substrings of content — but
    /// it *boosts* parents whose marker-prefixed decision/constraint lines
    /// match the query, so the memory that states a decision outranks the
    /// ones that merely mention its words. Rule rows are dropped from every
    /// channel — policy is delivered through the rules section, not through
    /// search. All channels read the same `validity` slice, so a superseded
    /// memory is invisible under [`Validity::Current`] on every path. Not
    /// feature-gated: it takes a precomputed query vector, so hand-built
    /// vectors exercise it in every build.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if any query fails —
    /// including the FTS syntax error an empty `query` produces, exactly as
    /// [`Store::search`] does.
    pub fn search_hybrid(
        &self,
        query: &str,
        query_vec: &[f32],
        model: &str,
        scope: Option<&str>,
        limit: u32,
        validity: Validity<'_>,
    ) -> rusqlite::Result<HybridSearch> {
        use crate::retrieval::{rrf_fuse, RRF_K};
        use std::collections::HashMap;

        let fts: Vec<Memory> = self
            .search_inner(query, scope, HYBRID_CHANNEL_LIMIT, validity)?
            .into_iter()
            .filter(|m| m.rule_id.is_none())
            .collect();
        let vector_hits =
            self.vector_candidates(query_vec, model, scope, HYBRID_CHANNEL_LIMIT, validity)?;
        let facts_channel = self.fact_candidates(query, scope, HYBRID_CHANNEL_LIMIT, validity)?;

        let fts_channel: Vec<String> = fts.iter().map(|m| m.id.clone()).collect();
        let vector_channel: Vec<String> = vector_hits.into_iter().map(|(id, _)| id).collect();
        let fts_candidates = fts_channel.len();
        let vector_candidates = vector_channel.len();
        let facts_candidates = facts_channel.len();
        let fused = rrf_fuse(&[fts_channel, vector_channel, facts_channel], RRF_K);

        // The FTS channel already carries full rows; vector-only ids are
        // fetched under the same validity slice (their rows passed it once
        // already inside vector_candidates, so the get is a plain lookup).
        let mut pool: HashMap<String, Memory> =
            fts.into_iter().map(|m| (m.id.clone(), m)).collect();
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut memories: Vec<Memory> = Vec::new();
        for (id, _score) in fused {
            if memories.len() >= limit {
                break;
            }
            let mem = match pool.remove(&id) {
                Some(m) => Some(m),
                None => self
                    .get_inner(&id, validity)?
                    .filter(|m| m.rule_id.is_none()),
            };
            if let Some(m) = mem {
                memories.push(m);
            }
        }
        // Track only the fused, truncated output — not the (up to 150)
        // per-channel candidates that fed the fusion.
        self.track_access(&memories.iter().map(|m| m.id.clone()).collect::<Vec<_>>())?;
        Ok(HybridSearch {
            memories,
            fts_candidates,
            vector_candidates,
            facts_candidates,
        })
    }

    /// Assembles a budget-packed context block for session start.
    ///
    /// Rules come first and are **always all included**, even when they alone
    /// exceed `budget_tokens` — policy is never silently dropped; the caller
    /// can see the overrun in the returned [`crate::retrieval::BudgetReport`].
    /// Memories are then packed into whatever budget remains.
    ///
    /// Candidate *selection* order is newest-first recency when there is no
    /// query, or the reciprocal-rank fusion of the recency channel
    /// (newest-first), the FTS relevance channel (rank order), and the
    /// extracted-fact channel ([`Store::fact_candidates`]; parents of
    /// matching marker lines) when there is one. When the caller
    /// additionally supplies a [`HybridQuery`] (the auto-hybrid gate passed
    /// and the query was embedded), a vector channel joins the same fusion
    /// — the same auto rule as search.
    /// *Presentation* is different: the included memories are re-sorted
    /// chronologically, because a prompt reads best oldest-first even though
    /// packing priority favors the newest and most relevant.
    ///
    /// A whitespace-only `query` is treated as no query at all — `search`
    /// would reject the degenerate FTS expression with an error — and the
    /// vector channel is skipped with it (nothing was embedded). Rule rows
    /// that `recall`/`search` surface are skipped as candidates: they are
    /// already in the rules section and must not be double-counted.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if any query fails.
    pub fn context(
        &self,
        scope: &str,
        query: Option<&str>,
        limit: u32,
        budget_tokens: u32,
        vector: Option<HybridQuery<'_>>,
    ) -> rusqlite::Result<ContextResult> {
        use crate::retrieval::{estimate_tokens, greedy_pack, BudgetReport, ESTIMATOR, RRF_K};
        use std::collections::{BTreeMap, HashMap, HashSet};

        let rules = self.rules(scope)?;
        let rules_tokens: u32 = rules.iter().map(|r| estimate_tokens(&r.text)).sum();

        let query = query.map(str::trim).filter(|q| !q.is_empty());

        // Rule rows live in the same table and come back from both recall
        // and search; they are already in the rules section, so they are not
        // memory candidates. Both channels read `Validity::Current`: a
        // session-start context block asserts what is true now, and a
        // superseded memory is by definition no longer that.
        let recency: Vec<Memory> = self
            .recall_inner(scope, limit, Validity::Current)?
            .into_iter()
            .filter(|m| m.rule_id.is_none())
            .collect();
        let relevance: Vec<Memory> = match query {
            Some(q) => self
                .search_inner(q, Some(scope), limit, Validity::Current)?
                .into_iter()
                .filter(|m| m.rule_id.is_none())
                .collect(),
            None => Vec::new(),
        };
        let recency_count = recency.len();
        let relevance_count = relevance.len();

        // Facts channel (M4): parents whose extracted marker lines match
        // the query. Always on when a query ran — extraction is idle-time
        // CLI work (`engram consolidate --extract`), so a never-extracted
        // database simply contributes zero candidates here. Same validity
        // slice as the other channels.
        let fact_ids: Vec<String> = match query {
            Some(q) => self.fact_candidates(q, Some(scope), limit, Validity::Current)?,
            None => Vec::new(),
        };
        let facts_count = fact_ids.len();

        // Vector channel: only when a query ran AND the caller embedded it
        // (the auto-hybrid gate lives at the surfaces; a `Some` here means it
        // passed). Same validity slice as the other channels.
        let vector_ids: Vec<String> = match (query, vector) {
            (Some(_), Some(v)) => self
                .vector_candidates(v.query_vec, v.model, Some(scope), limit, Validity::Current)?
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
            _ => Vec::new(),
        };
        let vector_count = vector_ids.len();
        let vector_ran = query.is_some() && vector.is_some();

        // Selection priority: fused when a query ran, plain newest-first
        // otherwise. `rrf_fuse` dedupes ids across channels by construction.
        let priority_ids: Vec<String> = if query.is_some() {
            let recency_channel: Vec<String> = recency.iter().rev().map(|m| m.id.clone()).collect();
            let relevance_channel: Vec<String> = relevance.iter().map(|m| m.id.clone()).collect();
            let mut channels = vec![recency_channel, relevance_channel, fact_ids.clone()];
            if vector_ran {
                channels.push(vector_ids.clone());
            }
            crate::retrieval::rrf_fuse(&channels, RRF_K)
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        } else {
            recency.iter().rev().map(|m| m.id.clone()).collect()
        };

        // Union of the channels, keyed by id so an id appearing in several is
        // included once.
        let mut pool: HashMap<String, Memory> = HashMap::new();
        for m in recency.into_iter().chain(relevance) {
            pool.entry(m.id.clone()).or_insert(m);
        }
        // Facts-only and vector-only hits carry no row yet; fetch them
        // under the same Current view the other channels used (rule rows
        // never qualify — both candidate queries already exclude them,
        // this is belt-and-braces).
        for id in fact_ids.iter().chain(vector_ids.iter()) {
            if !pool.contains_key(id) {
                if let Some(m) = self
                    .get_inner(id, Validity::Current)?
                    .filter(|m| m.rule_id.is_none())
                {
                    pool.insert(m.id.clone(), m);
                }
            }
        }

        let remaining = budget_tokens.saturating_sub(rules_tokens);
        // Every priority id is in the pool by construction (both feed from
        // the same fetches), so the filter_map is belt-and-braces only.
        let costed: Vec<(&str, u32)> = priority_ids
            .iter()
            .filter_map(|id| {
                pool.get(id.as_str())
                    .map(|m| (id.as_str(), estimate_tokens(&m.content)))
            })
            .collect();
        let pack = greedy_pack(&costed, remaining);
        let included_ids: HashSet<String> = pack
            .included
            .iter()
            .map(|&index| costed[index].0.to_string())
            .collect();

        // Presentation order: chronological (with id as a deterministic
        // tie-break), regardless of the selection priority above.
        let mut memories: Vec<Memory> = pool
            .into_values()
            .filter(|m| included_ids.contains(&m.id))
            .collect();
        memories.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut channels: BTreeMap<&'static str, usize> = BTreeMap::new();
        channels.insert("rules", rules.len());
        channels.insert("recency", recency_count);
        if query.is_some() {
            channels.insert("fts", relevance_count);
            channels.insert("facts", facts_count);
        }
        if vector_ran {
            channels.insert("vector", vector_count);
        }

        let budget = BudgetReport {
            requested_tokens: budget_tokens,
            estimator: ESTIMATOR,
            estimated_tokens: rules_tokens + pack.tokens,
            included: rules.len() + memories.len(),
            dropped: pack.dropped_ids.len(),
            dropped_ids: pack.dropped_ids,
            channels,
        };
        // Track only the memories that made it into the assembled block —
        // never the dropped candidates, and never the rules section (rules
        // are policy, not retrieval; `Store::rules` stays untracked).
        self.track_access(&memories.iter().map(|m| m.id.clone()).collect::<Vec<_>>())?;
        Ok(ContextResult {
            rules,
            memories,
            rules_tokens,
            budget,
        })
    }

    /// One memory by id, under a validity slice. Bumps the returned row's
    /// access counters (see [`Store::track_access`]).
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the query fails.
    pub fn get(&self, id: &str, validity: Validity<'_>) -> rusqlite::Result<Option<Memory>> {
        let out = self.get_inner(id, validity)?;
        if let Some(mem) = out.as_ref() {
            self.track_access(std::slice::from_ref(&mem.id))?;
        }
        Ok(out)
    }

    /// [`Store::get`] without access tracking — the shared body, also the
    /// row fetch for composite reads that track only their final output.
    fn get_inner(&self, id: &str, validity: Validity<'_>) -> rusqlite::Result<Option<Memory>> {
        let (clause, as_of) = validity_filter(validity, "", 2);
        let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE id = ?1 {clause}");
        let mut bind: Vec<&dyn rusqlite::ToSql> = vec![&id];
        if let Some(t) = as_of.as_ref() {
            bind.push(t);
        }
        self.conn
            .query_row(&sql, rusqlite::params_from_iter(bind), row_to_memory)
            .map(Some)
            .or_else(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    Ok(None)
                } else {
                    Err(e)
                }
            })
    }

    /// Creates a rule, or updates the existing rule with the same
    /// `(scope, rule_id)` in place.
    ///
    /// Upserting rather than appending is what makes a rule a rule: re-running
    /// `rule add` after editing the wording must not leave two conflicting
    /// versions for an agent to pick between. `created_at` is preserved across
    /// updates; `updated_at` moves.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the transaction fails.
    pub fn rule_add(
        &self,
        agent: &str,
        scope: &str,
        rule_id: &str,
        text: &str,
    ) -> rusqlite::Result<RuleUpsert> {
        let now = crate::time::now_iso8601();
        // Read-then-write, so wrap both in a transaction: two processes sharing
        // the database file could otherwise interleave and lose one edit.
        let tx = self.conn.unchecked_transaction()?;

        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT id, created_at FROM memories WHERE scope = ?1 AND rule_id = ?2",
                params![scope, rule_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map(Some)
            .or_else(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    Ok(None)
                } else {
                    Err(e)
                }
            })?;

        let (created, created_at) = match &existing {
            Some((id, created_at)) => {
                // Reactivates as a side effect: the id is the rule's identity, so
                // re-adding a retired rule reinstates it rather than failing
                // against the unique index or silently writing a hidden row.
                tx.execute(
                    "UPDATE memories SET content = ?1, agent = ?2, updated_at = ?3, status = ?4 WHERE id = ?5",
                    params![text, agent, now, crate::rules::STATUS_ACTIVE, id],
                )?;
                (false, created_at.clone())
            }
            None => {
                tx.execute(
                    "INSERT INTO memories (id, agent, scope, role, content, created_at, rule_id, updated_at, status)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        uuid::Uuid::new_v4().to_string(),
                        agent,
                        scope,
                        crate::rules::ROLE,
                        text,
                        now,
                        rule_id,
                        now,
                        crate::rules::STATUS_ACTIVE
                    ],
                )?;
                (true, now.clone())
            }
        };
        tx.commit()?;

        Ok(RuleUpsert {
            rule: Rule {
                rule_id: rule_id.to_string(),
                scope: scope.to_string(),
                text: text.to_string(),
                agent: agent.to_string(),
                created_at,
                updated_at: now,
                retired: false,
            },
            created,
        })
    }

    /// Withdraws a rule, keeping the row as a tombstone.
    ///
    /// Idempotent: retiring an already-retired rule reports
    /// [`RetireOutcome::AlreadyRetired`] and changes nothing.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the transaction fails.
    pub fn rule_retire(&self, scope: &str, rule_id: &str) -> rusqlite::Result<RuleRetire> {
        let now = crate::time::now_iso8601();
        let tx = self.conn.unchecked_transaction()?;

        let existing: Option<(String, bool)> = tx
            .query_row(
                "SELECT id, status FROM memories WHERE scope = ?1 AND rule_id = ?2",
                params![scope, rule_id],
                |row| {
                    let status: Option<String> = row.get(1)?;
                    Ok((
                        row.get(0)?,
                        status.as_deref() == Some(crate::rules::STATUS_RETIRED),
                    ))
                },
            )
            .map(Some)
            .or_else(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    Ok(None)
                } else {
                    Err(e)
                }
            })?;

        let outcome = match &existing {
            None => RetireOutcome::NotFound,
            Some((_, true)) => RetireOutcome::AlreadyRetired,
            Some((id, false)) => {
                tx.execute(
                    "UPDATE memories SET status = ?1, updated_at = ?2 WHERE id = ?3",
                    params![crate::rules::STATUS_RETIRED, now, id],
                )?;
                RetireOutcome::Retired
            }
        };
        tx.commit()?;

        let rule = if outcome == RetireOutcome::NotFound {
            None
        } else {
            self.rules_including_retired(scope)?
                .into_iter()
                .find(|r| r.rule_id == rule_id)
        };
        Ok(RuleRetire {
            rule_id: rule_id.to_string(),
            scope: scope.to_string(),
            outcome,
            rule,
        })
    }

    /// Permanently deletes a **retired** rule's row.
    ///
    /// The one true delete in the store, and it only reaches tombstones: an
    /// active rule is refused (`NotRetired` — retire it first), and ordinary
    /// messages are unreachable here by construction. The `memories_ad`
    /// trigger scrubs the FTS index entry. Deliberately CLI-only at the
    /// surfaces: destructive operations are not agent-invocable.
    pub fn rule_purge(&self, scope: &str, rule_id: &str) -> rusqlite::Result<PurgeOutcome> {
        let tx = self.conn.unchecked_transaction()?;
        let status: Option<Option<String>> = tx
            .query_row(
                "SELECT status FROM memories WHERE scope = ?1 AND rule_id = ?2",
                params![scope, rule_id],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    Ok(None)
                } else {
                    Err(e)
                }
            })?;
        let outcome = match status {
            None => PurgeOutcome::NotFound,
            Some(s) if s.as_deref() == Some(crate::rules::STATUS_RETIRED) => {
                tx.execute(
                    "DELETE FROM memories WHERE scope = ?1 AND rule_id = ?2 AND status = ?3",
                    params![scope, rule_id, crate::rules::STATUS_RETIRED],
                )?;
                PurgeOutcome::Purged
            }
            Some(_) => PurgeOutcome::NotRetired,
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Active rules for a scope, ordered by `rule_id`.
    ///
    /// The ordering is by id rather than by time so that [`crate::rules::render_block`]
    /// emits the same bytes for the same rule set, which is what keeps `sync`
    /// idempotent and its diffs readable.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the query fails.
    pub fn rules(&self, scope: &str) -> rusqlite::Result<Vec<Rule>> {
        self.query_rules(scope, false)
    }

    /// Every rule for a scope including retired ones, ordered by `rule_id`.
    ///
    /// # Errors
    ///
    /// Returns the underlying `rusqlite` error if the query fails.
    pub fn rules_including_retired(&self, scope: &str) -> rusqlite::Result<Vec<Rule>> {
        self.query_rules(scope, true)
    }

    fn query_rules(&self, scope: &str, include_retired: bool) -> rusqlite::Result<Vec<Rule>> {
        // `status IS NULL` covers rules written before the status column existed;
        // absent a status, a rule is binding.
        let sql = if include_retired {
            "SELECT rule_id, scope, content, agent, created_at, updated_at, status FROM memories
             WHERE scope = ?1 AND rule_id IS NOT NULL ORDER BY rule_id ASC"
        } else {
            "SELECT rule_id, scope, content, agent, created_at, updated_at, status FROM memories
             WHERE scope = ?1 AND rule_id IS NOT NULL AND (status IS NULL OR status <> 'retired')
             ORDER BY rule_id ASC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![scope], |row| {
            let status: Option<String> = row.get(6)?;
            Ok(Rule {
                rule_id: row.get(0)?,
                scope: row.get(1)?,
                text: row.get(2)?,
                agent: row.get(3)?,
                created_at: row.get(4)?,
                // Rows written before the rules migration have no `updated_at`;
                // fall back to `created_at` so the field stays non-optional.
                updated_at: row.get::<_, Option<String>>(5)?.unwrap_or(row.get(4)?),
                retired: status.as_deref() == Some(crate::rules::STATUS_RETIRED),
            })
        })?;
        rows.collect()
    }
}

/// Adds the rule columns to an existing database.
///
/// `ALTER TABLE ... ADD COLUMN` has no `IF NOT EXISTS` in SQLite, so the live
/// schema is probed first. Both columns are nullable because every row written
/// before this migration is an ordinary message, which has neither.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let columns = table_columns(conn, "memories")?;
    if !columns.iter().any(|c| c == "rule_id") {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN rule_id TEXT;")?;
    }
    if !columns.iter().any(|c| c == "updated_at") {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN updated_at TEXT;")?;
    }
    if !columns.iter().any(|c| c == "status") {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN status TEXT;")?;
    }
    // Bi-temporal validity (M2). All nullable: NULL valid_to == currently
    // valid, so every pre-existing row is valid by construction; NULL
    // valid_from reads as created_at via COALESCE. `status` above remains
    // exclusively the rules lifecycle column — supersession never touches it.
    if !columns.iter().any(|c| c == "valid_from") {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN valid_from TEXT;")?;
    }
    if !columns.iter().any(|c| c == "valid_to") {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN valid_to TEXT;")?;
    }
    if !columns.iter().any(|c| c == "superseded_by") {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN superseded_by TEXT;")?;
    }
    // Access tracking (M5). Both nullable: a NULL access_count reads as 0,
    // and every pre-existing row has simply never been read. The columns are
    // internal — `row_to_memory` does not read them, so `Memory` output is
    // unchanged — and they feed only the `consolidate --report` decay
    // scoring.
    if !columns.iter().any(|c| c == "last_accessed_at") {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN last_accessed_at TEXT;")?;
    }
    if !columns.iter().any(|c| c == "access_count") {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN access_count INTEGER;")?;
    }
    // Narrow the FTS update trigger to content changes (M5). The original
    // trigger fired on EVERY update, so each access-tracking bump — i.e.
    // every read — and each supersession (`valid_to`/`superseded_by`) would
    // churn the FTS index with a pointless delete+reinsert of unchanged
    // content. `AFTER UPDATE OF content` fires only when a statement assigns
    // the content column (rule revisions), which is the only case the index
    // must follow. SQLite has no CREATE OR REPLACE TRIGGER, so the narrowing
    // is an unconditional drop+create on every open — idempotent, and it
    // also converts the broad trigger in any pre-M5 database in place.
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS memories_au;
         CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE OF content ON memories BEGIN
             INSERT INTO memories_fts(memories_fts, rowid, content) VALUES('delete', old.rowid, old.content);
             INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
         END;",
    )?;
    // Partial index: enforces one rule per (scope, rule_id) without constraining
    // ordinary messages, which all have a NULL rule_id.
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_memories_rule
         ON memories(scope, rule_id) WHERE rule_id IS NOT NULL;",
    )?;
    // Vector sidecar (M3). Deliberately NOT feature-gated: the table simply
    // stays empty in a build without the `vector` feature, and a database
    // touched by a vector-enabled binary keeps working everywhere else.
    // `embedding` is a little-endian f32 array, `dim` entries long; `model`
    // names the embedding model so vectors from different models are never
    // compared against each other.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_vectors (
            memory_id TEXT PRIMARY KEY REFERENCES memories(id),
            model     TEXT NOT NULL,
            dim       INTEGER NOT NULL,
            embedding BLOB NOT NULL
        );",
    )?;
    // Extracted-fact index (M4, the TencentDB L0↔L1 pattern). Facts are an
    // INDEX over memories, never a replacement: each row is a verbatim
    // marker-prefixed line/sentence of its parent's content, with
    // `memory_id` as the drill-down pointer back to the full record
    // (`engram get` / the MCP `get` tool). `id` is a deterministic UUID v5
    // over (memory_id, fact) — see `crate::facts::fact_id` — so
    // re-extraction upserts in place. `valid_to`/`superseded_by` are
    // RESERVED and stay NULL for now: a fact's liveness derives from its
    // PARENT's validity — every fact-channel query joins `memories` and
    // applies the validity clause to the parent columns, so superseding a
    // memory retires its facts at query time without touching fact rows.
    // Rule rows are never extracted from.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS facts (
            id            TEXT PRIMARY KEY,
            memory_id     TEXT NOT NULL REFERENCES memories(id),
            scope         TEXT NOT NULL,
            fact          TEXT NOT NULL,
            extractor     TEXT NOT NULL,
            created_at    TEXT NOT NULL,
            valid_to      TEXT,
            superseded_by TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_facts_scope ON facts(scope);
        CREATE INDEX IF NOT EXISTS idx_facts_memory ON facts(memory_id);

        CREATE VIRTUAL TABLE IF NOT EXISTS facts_fts USING fts5(
            fact,
            content='facts',
            content_rowid='rowid'
        );

        CREATE TRIGGER IF NOT EXISTS facts_ai AFTER INSERT ON facts BEGIN
            INSERT INTO facts_fts(rowid, fact) VALUES (new.rowid, new.fact);
        END;
        CREATE TRIGGER IF NOT EXISTS facts_ad AFTER DELETE ON facts BEGIN
            INSERT INTO facts_fts(facts_fts, rowid, fact) VALUES('delete', old.rowid, old.fact);
        END;
        CREATE TRIGGER IF NOT EXISTS facts_au AFTER UPDATE ON facts BEGIN
            INSERT INTO facts_fts(facts_fts, rowid, fact) VALUES('delete', old.rowid, old.fact);
            INSERT INTO facts_fts(rowid, fact) VALUES (new.rowid, new.fact);
        END;",
    )?;
    Ok(())
}

fn table_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let rows = stmt.query_map(params![table], |row| row.get::<_, String>(0))?;
    rows.collect()
}

/// FTS5 treats AND/OR/NOT/NEAR, quotes, parens, `*`, `:`, `-` as query
/// syntax. A user's or agent's free-text query ("what did we decide about
/// GraphQL - and why not REST?") is not a query language expression; it's
/// data. Wrap each token as an escaped quoted phrase so no character inside
/// a token can be interpreted as an operator. This is the exact class of
/// bug the TencentDB Agent Memory project had to patch
/// (fts5-query-sanitization) — worth avoiding from the start.
///
/// Tokens are joined with `OR`, not FTS5's implicit `AND`: memory queries
/// are natural language ("why did the binary lose its symbols"), and
/// requiring *every* token to appear zeroes out any query with a filler
/// word the stored text lacks. The M3 benchmark measured the difference on
/// engineer-typed queries: AND-joined recall@5 was 0.108, OR-joined 0.856
/// (bench/RESULTS.md). BM25 ranking still rewards documents matching more
/// of the tokens, so precision survives the looser match — and `--limit`
/// caps the tail.
fn sanitize_fts_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|tok| format!("\"{}\"", tok.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Normalization for the exact-text dedup detector: trim, lowercase, and
/// collapse every internal whitespace run to a single space. Two memories
/// whose normalized forms are equal restate the same text; formatting-only
/// variance (case, indentation, line wrapping) is not a distinct memory.
fn normalize_for_dedup(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The full `memories` column list every read shares, in [`row_to_memory`]
/// order. One definition so a future column lands everywhere at once.
const MEMORY_COLUMNS: &str =
    "id, agent, scope, role, content, created_at, rule_id, updated_at, valid_from, valid_to, superseded_by";

/// [`MEMORY_COLUMNS`] with the `m.` qualifier for the FTS join.
const MEMORY_COLUMNS_M: &str =
    "m.id, m.agent, m.scope, m.role, m.content, m.created_at, m.rule_id, m.updated_at, m.valid_from, m.valid_to, m.superseded_by";

/// SQL fragment + optional bind value for a [`Validity`] filter.
///
/// `prefix` qualifies the columns (`"m."` on the FTS join); `param` is the
/// 1-based numbered placeholder the `AsOf` instant binds to. The placeholder
/// appears twice in the clause — SQLite binds both occurrences to the same
/// value, which is why numbered (not anonymous) parameters are used here.
fn validity_filter(validity: Validity<'_>, prefix: &str, param: usize) -> (String, Option<String>) {
    match validity {
        Validity::Current => (format!("AND {prefix}valid_to IS NULL"), None),
        Validity::AsOf(t) => (
            format!(
                "AND COALESCE({prefix}valid_from, {prefix}created_at) <= ?{param} \
                 AND ({prefix}valid_to IS NULL OR {prefix}valid_to > ?{param})"
            ),
            Some(t.to_string()),
        ),
        Validity::All => (String::new(), None),
    }
}

fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        agent: row.get(1)?,
        scope: row.get(2)?,
        role: row.get(3)?,
        content: row.get(4)?,
        created_at: row.get(5)?,
        rule_id: row.get(6)?,
        updated_at: row.get(7)?,
        valid_from: row.get(8)?,
        valid_to: row.get(9)?,
        superseded_by: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        // In-memory database: exercises the real schema and migration path
        // without touching the filesystem.
        let conn = Connection::open_in_memory().expect("open in-memory database");
        let store = Store {
            conn,
            tracking: true,
        };
        store
            .conn
            .execute_batch(
                r#"
                CREATE TABLE memories (
                    id TEXT PRIMARY KEY, agent TEXT NOT NULL, scope TEXT NOT NULL,
                    role TEXT NOT NULL, content TEXT NOT NULL, created_at TEXT NOT NULL
                );
                CREATE VIRTUAL TABLE memories_fts USING fts5(
                    content, content='memories', content_rowid='rowid'
                );
                CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
                    INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
                END;
                "#,
            )
            .expect("legacy schema");
        migrate(&store.conn).expect("migrate");
        store
    }

    #[test]
    fn migration_is_idempotent_over_a_pre_rules_schema() {
        let store = store();
        // Second run must be a no-op rather than an "duplicate column" error.
        migrate(&store.conn).expect("re-migrate");
        let columns = table_columns(&store.conn, "memories").expect("columns");
        assert!(columns.iter().any(|c| c == "rule_id"));
        assert!(columns.iter().any(|c| c == "updated_at"));
        // M2 bi-temporal columns arrive through the same migration path.
        assert!(columns.iter().any(|c| c == "valid_from"));
        assert!(columns.iter().any(|c| c == "valid_to"));
        assert!(columns.iter().any(|c| c == "superseded_by"));
    }

    #[test]
    fn legacy_rows_recall_as_currently_valid_after_migration() {
        // A row written before the bi-temporal columns existed has NULL in
        // all three — which must read as "currently valid".
        let store = store();
        store
            .conn
            .execute(
                "INSERT INTO memories (id, agent, scope, role, content, created_at)
                 VALUES ('legacy-1', 'old-agent', 'demo', 'note', 'Pre-M2 row.', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert legacy row");
        let current = store.recall("demo", 10, Validity::Current).expect("recall");
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, "legacy-1");
        assert!(current[0].valid_to.is_none());
    }

    #[test]
    fn rule_add_upserts_instead_of_appending() {
        let store = store();
        let first = store
            .rule_add("claude-code", "demo", "no-panics", "Original.")
            .expect("add");
        assert!(first.created);

        let second = store
            .rule_add("codex", "demo", "no-panics", "Revised.")
            .expect("update");
        assert!(!second.created);
        assert_eq!(
            second.rule.created_at, first.rule.created_at,
            "created_at must survive an edit"
        );

        let rules = store.rules("demo").expect("list");
        assert_eq!(
            rules.len(),
            1,
            "re-adding an id must not produce a second rule"
        );
        assert_eq!(rules[0].text, "Revised.");
        assert_eq!(rules[0].agent, "codex");
    }

    #[test]
    fn rules_are_scoped_and_ordered_by_id() {
        let store = store();
        store.rule_add("a", "demo", "zeta", "Z.").expect("add");
        store.rule_add("a", "demo", "alpha", "A.").expect("add");
        store.rule_add("a", "other", "beta", "B.").expect("add");

        let ids: Vec<_> = store
            .rules("demo")
            .expect("list")
            .into_iter()
            .map(|r| r.rule_id)
            .collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
        assert_eq!(store.rules("other").expect("list").len(), 1);
    }

    #[test]
    fn rules_do_not_leak_into_plain_memory_output_as_nulls() {
        let store = store();
        let mem = store
            .remember("claude-code", "demo", "note", "A message.")
            .expect("remember");
        assert!(mem.rule_id.is_none());
        let json = serde_json::to_string(&mem).expect("serialize");
        assert!(
            !json.contains("rule_id"),
            "plain memories must serialize exactly as before"
        );
        // The bi-temporal keys are equally absent on a plain row.
        assert!(!json.contains("valid_from"));
        assert!(!json.contains("valid_to"));
        assert!(!json.contains("superseded_by"));
    }

    #[test]
    fn retiring_hides_a_rule_without_erasing_it() {
        let store = store();
        store
            .rule_add("claude-code", "demo", "old-policy", "Superseded.")
            .expect("add");

        let retired = store.rule_retire("demo", "old-policy").expect("retire");
        assert_eq!(retired.outcome, RetireOutcome::Retired);
        assert!(
            store.rules("demo").expect("list").is_empty(),
            "retired rules must not be binding"
        );

        let all = store.rules_including_retired("demo").expect("list all");
        assert_eq!(all.len(), 1, "the tombstone must survive");
        assert!(all[0].retired);
        assert_eq!(
            all[0].text, "Superseded.",
            "text must be preserved for the record"
        );
    }

    #[test]
    fn retiring_is_idempotent_and_reports_a_missing_rule() {
        let store = store();
        store
            .rule_add("claude-code", "demo", "x", "Text.")
            .expect("add");

        assert_eq!(
            store.rule_retire("demo", "x").expect("retire").outcome,
            RetireOutcome::Retired
        );
        assert_eq!(
            store
                .rule_retire("demo", "x")
                .expect("retire again")
                .outcome,
            RetireOutcome::AlreadyRetired
        );
        assert_eq!(
            store
                .rule_retire("demo", "ghost")
                .expect("retire ghost")
                .outcome,
            RetireOutcome::NotFound
        );
    }

    #[test]
    fn re_adding_a_retired_rule_reinstates_it() {
        let store = store();
        store
            .rule_add("claude-code", "demo", "x", "Original.")
            .expect("add");
        store.rule_retire("demo", "x").expect("retire");

        // The id is the rule's identity, so re-adding must reinstate rather than
        // collide with the unique index or leave a hidden retired duplicate.
        let re_added = store
            .rule_add("claude-code", "demo", "x", "Back, revised.")
            .expect("re-add");
        assert!(!re_added.created, "the original row is reused");
        assert!(!re_added.rule.retired);

        let active = store.rules("demo").expect("list");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].text, "Back, revised.");
        assert_eq!(
            store.rules_including_retired("demo").expect("all").len(),
            1,
            "no duplicate row"
        );
    }

    #[test]
    fn recall_surfaces_rules_alongside_messages() {
        let store = store();
        store
            .remember("claude-code", "demo", "note", "A message.")
            .expect("remember");
        store
            .rule_add("claude-code", "demo", "no-panics", "No panics.")
            .expect("add");

        let recalled = store.recall("demo", 50, Validity::Current).expect("recall");
        assert_eq!(recalled.len(), 2);
        assert!(recalled.iter().any(|m| m.role == crate::rules::ROLE));
    }

    // ---- Memory-surface tests -------------------------------------------
    //
    // These use `Store::open_in_memory()` — the production constructor path
    // (pragmas + schema + migrate), not the legacy-schema fixture above.

    #[test]
    fn remember_returns_a_well_formed_memory() {
        let store = Store::open_in_memory().expect("open");
        let mem = store
            .remember("claude-code", "demo", "note", "Hello.")
            .expect("remember");
        uuid::Uuid::parse_str(&mem.id).expect("id must be a UUID");
        assert_eq!(mem.agent, "claude-code");
        assert_eq!(mem.scope, "demo");
        assert_eq!(mem.role, "note");
        assert_eq!(mem.content, "Hello.");
        assert!(
            mem.created_at.ends_with('Z'),
            "ISO 8601 UTC, got {}",
            mem.created_at
        );
        assert!(mem.rule_id.is_none());
        assert!(mem.updated_at.is_none());
    }

    #[test]
    fn recall_is_chronological_and_limit_keeps_the_most_recent() {
        let store = Store::open_in_memory().expect("open");
        for i in 0..5 {
            store
                .remember("a", "demo", "note", &format!("message {i}"))
                .expect("remember");
            // Distinct created_at per row: recall orders by timestamp, and two
            // inserts within the same clock tick would make the order ambiguous.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let all = store.recall("demo", 50, Validity::Current).expect("recall");
        let contents: Vec<_> = all.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            [
                "message 0",
                "message 1",
                "message 2",
                "message 3",
                "message 4"
            ]
        );

        let last3 = store
            .recall("demo", 3, Validity::Current)
            .expect("recall limited");
        let contents: Vec<_> = last3.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            ["message 2", "message 3", "message 4"],
            "limit must keep the LAST N, still chronological"
        );
    }

    #[test]
    fn recall_of_an_empty_scope_is_empty_not_an_error() {
        let store = Store::open_in_memory().expect("open");
        assert!(store
            .recall("nothing-here", 10, Validity::Current)
            .expect("recall")
            .is_empty());
    }

    #[test]
    fn search_spans_scopes_filters_by_scope_ranks_and_limits() {
        let store = Store::open_in_memory().expect("open");
        // Short document, three hits: best BM25 rank for "alpha".
        store
            .remember("a", "s1", "note", "alpha alpha alpha")
            .expect("remember");
        // Long document, one hit: worse rank.
        store
            .remember(
                "a",
                "s2",
                "note",
                "alpha buried in a much longer run of entirely unrelated filler words",
            )
            .expect("remember");
        store
            .remember("a", "s3", "note", "no match here at all")
            .expect("remember");

        let across = store
            .search("alpha", None, 10, Validity::Current)
            .expect("search all scopes");
        assert_eq!(across.len(), 2, "scope=None must search every scope");
        assert_eq!(
            across[0].scope, "s1",
            "FTS5 rank orders the denser, shorter document first"
        );
        assert_eq!(across[1].scope, "s2");

        let scoped = store
            .search("alpha", Some("s2"), 10, Validity::Current)
            .expect("search one scope");
        assert_eq!(scoped.len(), 1, "scope=Some must filter to that scope");
        assert_eq!(scoped[0].scope, "s2");

        assert_eq!(
            store
                .search("alpha", None, 1, Validity::Current)
                .expect("search limited")
                .len(),
            1
        );
    }

    #[test]
    fn adversarial_fts_syntax_is_stored_and_searched_without_error() {
        let store = Store::open_in_memory().expect("open");
        let nasty = [
            "a AND b",
            "-negated",
            "col:value",
            "phrase \"quoted\" here",
            "star*",
            "(parens)",
        ];
        for (i, content) in nasty.iter().enumerate() {
            let scope = format!("adv-{i}");
            store
                .remember("a", &scope, "note", content)
                .expect("remember");
            // Query with the raw adversarial text itself: sanitize_fts_query must
            // keep FTS5 from reading any of it as query syntax.
            let hits = store
                .search(content, Some(&scope), 10, Validity::Current)
                .unwrap_or_else(|e| {
                    panic!("query {content:?} must not be an FTS5 syntax error: {e}")
                });
            assert_eq!(hits.len(), 1, "query {content:?} should find its own row");
            assert_eq!(hits[0].content, *content);
        }
    }

    #[test]
    fn search_with_an_empty_query_is_a_storage_error_today() {
        // Documents (not blesses) current behavior: sanitize_fts_query("")
        // yields an empty MATCH expression, which FTS5 rejects as a syntax
        // error, so `search` surfaces Err rather than Ok(vec![]).
        let store = Store::open_in_memory().expect("open");
        store
            .remember("a", "demo", "note", "content")
            .expect("remember");
        assert!(store.search("", None, 10, Validity::Current).is_err());
    }

    #[test]
    fn get_returns_some_for_known_and_none_for_unknown_ids() {
        let store = Store::open_in_memory().expect("open");
        let mem = store
            .remember("a", "demo", "note", "findable")
            .expect("remember");
        let found = store
            .get(&mem.id, Validity::Current)
            .expect("get")
            .expect("known id must be Some");
        assert_eq!(found.id, mem.id);
        assert_eq!(found.content, "findable");
        assert!(store
            .get("no-such-id", Validity::Current)
            .expect("get unknown")
            .is_none());
    }

    #[test]
    fn remember_preserves_content_verbatim() {
        let store = Store::open_in_memory().expect("open");

        let multibyte = "unicode: 日本語 🚀 émoji Ω\nsecond line\r\nthird\tline";
        let mem = store
            .remember("a", "demo", "note", multibyte)
            .expect("remember multibyte");
        assert_eq!(mem.content, multibyte);
        assert_eq!(
            store
                .get(&mem.id, Validity::Current)
                .expect("get")
                .expect("row")
                .content,
            multibyte
        );

        let big = "0123456789ABCDEF".repeat(640); // exactly 10 KiB
        assert_eq!(big.len(), 10 * 1024);
        let mem = store
            .remember("a", "demo", "note", &big)
            .expect("remember 10 KiB");
        assert_eq!(
            store
                .get(&mem.id, Validity::Current)
                .expect("get")
                .expect("row")
                .content,
            big
        );
    }

    // ---- Context-assembly tests -----------------------------------------

    /// Stores `content` and pauses so the next row gets a strictly later
    /// `created_at` — recall orders by timestamp.
    fn remember_spaced(store: &Store, scope: &str, content: &str) {
        store
            .remember("test-agent", scope, "note", content)
            .expect("remember");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    #[test]
    fn context_always_includes_rules_even_when_they_alone_exceed_the_budget() {
        let store = Store::open_in_memory().expect("open");
        // 100 chars => 25 estimated tokens, well past the budget of 10.
        let rule_text = "R".repeat(100);
        store
            .rule_add("test-agent", "ctx", "big-rule", &rule_text)
            .expect("add rule");
        remember_spaced(&store, "ctx", "a message");
        remember_spaced(&store, "ctx", "another message");

        let ctx = store.context("ctx", None, 50, 10, None).expect("context");
        assert_eq!(ctx.rules.len(), 1, "policy is never silently dropped");
        assert!(
            ctx.rules_tokens > 10,
            "rules alone exceed the requested budget"
        );
        assert!(
            ctx.memories.is_empty(),
            "no budget remains for any memory: {:?}",
            ctx.memories
        );
        assert_eq!(ctx.budget.included, 1, "the rule is the only included item");
        assert_eq!(ctx.budget.dropped, 2, "both memories were dropped");
        assert_eq!(ctx.budget.estimated_tokens, ctx.rules_tokens);
    }

    #[test]
    fn context_without_query_selects_newest_first_and_presents_chronologically() {
        let store = Store::open_in_memory().expect("open");
        // Four 6-token messages (21–24 chars each) against a 12-token budget:
        // only the newest two fit.
        for content in [
            "irrelevant chatter one",
            "irrelevant chatter two",
            "irrelevant chatter three",
            "irrelevant chatter four",
        ] {
            remember_spaced(&store, "ctx-recency", content);
        }

        let ctx = store
            .context("ctx-recency", None, 50, 12, None)
            .expect("context");
        let contents: Vec<&str> = ctx.memories.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            ["irrelevant chatter three", "irrelevant chatter four"],
            "selection is newest-first, presentation is chronological"
        );
        assert_eq!(ctx.budget.dropped, 2);
        assert_eq!(ctx.budget.channels[&"recency"], 4);
        assert!(
            !ctx.budget.channels.contains_key("fts"),
            "no query ran, so no fts channel is reported"
        );
    }

    #[test]
    fn context_with_query_rescues_an_old_relevant_memory_recency_would_drop() {
        let store = Store::open_in_memory().expect("open");
        let scope = "ctx-fused";
        // Oldest message is the only one matching the query; every message
        // estimates to 6 tokens, and the budget fits exactly two.
        remember_spaced(&store, scope, "launch window at dawn");
        for content in [
            "irrelevant chatter one",
            "irrelevant chatter two",
            "irrelevant chatter three",
            "irrelevant chatter four",
        ] {
            remember_spaced(&store, scope, content);
        }

        let without_query = store.context(scope, None, 50, 12, None).expect("context");
        assert!(
            !without_query
                .memories
                .iter()
                .any(|m| m.content.contains("launch")),
            "pure recency at this budget drops the oldest message"
        );

        let with_query = store
            .context(scope, Some("launch"), 50, 12, None)
            .expect("context with query");
        assert!(
            with_query
                .memories
                .iter()
                .any(|m| m.content == "launch window at dawn"),
            "the fts channel pulls the old-but-relevant memory back in: {:?}",
            with_query.memories
        );
        assert_eq!(
            with_query.memories[0].content, "launch window at dawn",
            "presentation stays chronological, so the oldest comes first"
        );
        assert_eq!(with_query.budget.channels[&"fts"], 1);
        assert_eq!(with_query.budget.channels[&"recency"], 5);

        // A whitespace-only query is treated as no query, not a search error.
        let blank = store
            .context(scope, Some("   "), 50, 12, None)
            .expect("context");
        assert!(!blank.budget.channels.contains_key("fts"));
    }

    #[test]
    fn context_never_lists_rule_rows_among_memories() {
        let store = Store::open_in_memory().expect("open");
        let scope = "ctx-rules";
        store
            .rule_add(
                "test-agent",
                scope,
                "no-port-eight",
                "never expose port eight thousand",
            )
            .expect("add rule");
        remember_spaced(&store, scope, "an ordinary message");

        // Generous budget; the query matches the rule text, so search
        // surfaces the rule row — it must still be filtered from memories.
        let ctx = store
            .context(scope, Some("expose port"), 50, 1000, None)
            .expect("context");
        assert_eq!(ctx.rules.len(), 1);
        assert!(
            ctx.memories.iter().all(|m| m.rule_id.is_none()),
            "rule rows are already in the rules section: {:?}",
            ctx.memories
        );
        assert_eq!(ctx.memories.len(), 1);
        assert_eq!(ctx.memories[0].content, "an ordinary message");
        assert_eq!(ctx.budget.channels[&"rules"], 1);
    }

    #[test]
    fn supersession_chain_supports_time_travel() {
        let store = Store::open_in_memory().expect("open");
        let a = store
            .remember("claude-code", "demo", "note", "shared-term version one")
            .expect("remember A");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t1 = crate::time::now_iso8601();
        std::thread::sleep(std::time::Duration::from_millis(2));

        let b = store
            .remember_superseding("codex", "demo", "note", "shared-term version two", &a.id)
            .expect("supersede A");
        let b_mem = b.memory.expect("B stored");
        assert!(matches!(b.outcome, SupersedeOutcome::Superseded));
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t2 = crate::time::now_iso8601();
        std::thread::sleep(std::time::Duration::from_millis(2));

        let c = store
            .remember_superseding(
                "kimi",
                "demo",
                "note",
                "shared-term version three",
                &b_mem.id,
            )
            .expect("supersede B");
        let c_mem = c.memory.expect("C stored");

        // Current view: only C.
        let current = store
            .recall("demo", 10, Validity::Current)
            .expect("recall current");
        assert_eq!(
            current.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec![c_mem.id.as_str()]
        );

        // Time travel: A at t1, B at t2.
        let at_t1 = store
            .recall("demo", 10, Validity::AsOf(&t1))
            .expect("as-of t1");
        assert_eq!(
            at_t1.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec![a.id.as_str()]
        );
        let at_t2 = store
            .recall("demo", 10, Validity::AsOf(&t2))
            .expect("as-of t2");
        assert_eq!(
            at_t2.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec![b_mem.id.as_str()]
        );

        // Full history: all three, chronological.
        let all = store.recall("demo", 10, Validity::All).expect("recall all");
        assert_eq!(all.len(), 3);

        // The chain has no gap: A's window closes exactly where B begins.
        let a_row = store
            .get(&a.id, Validity::All)
            .expect("get")
            .expect("A exists");
        assert_eq!(a_row.superseded_by.as_deref(), Some(b_mem.id.as_str()));
        assert_eq!(a_row.valid_to.as_deref(), Some(b_mem.created_at.as_str()));

        // Search respects the same filter.
        let hits = store
            .search("shared-term", Some("demo"), 10, Validity::Current)
            .expect("search current");
        assert_eq!(
            hits.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec![c_mem.id.as_str()]
        );
    }

    #[test]
    fn supersede_guards_reject_bad_targets_without_writing() {
        let store = Store::open_in_memory().expect("open");
        let a = store
            .remember("claude-code", "demo", "note", "the fact")
            .expect("remember");
        store
            .rule_add("claude-code", "demo", "some-rule", "Policy.")
            .expect("rule");
        let count = |store: &Store| -> i64 {
            store
                .conn
                .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
                .expect("count")
        };
        let before = count(&store);

        // Unknown id, and a real id in the wrong scope, both report NotFound.
        let missing = store
            .remember_superseding("x", "demo", "note", "new", "no-such-id")
            .expect("call");
        assert!(matches!(missing.outcome, SupersedeOutcome::NotFound));
        let wrong_scope = store
            .remember_superseding("x", "other-scope", "note", "new", &a.id)
            .expect("call");
        assert!(matches!(wrong_scope.outcome, SupersedeOutcome::NotFound));

        // A rule row is never superseded through here.
        let rule_row_id: String = store
            .conn
            .query_row(
                "SELECT id FROM memories WHERE rule_id = 'some-rule'",
                [],
                |r| r.get(0),
            )
            .expect("rule row id");
        let rule_target = store
            .remember_superseding("x", "demo", "note", "new", &rule_row_id)
            .expect("call");
        assert!(matches!(
            rule_target.outcome,
            SupersedeOutcome::TargetIsRule
        ));

        // Double supersede: second attempt names the winner and writes nothing.
        let b = store
            .remember_superseding("x", "demo", "note", "replacement", &a.id)
            .expect("supersede");
        let b_id = b.memory.expect("stored").id;
        let after_valid = count(&store);
        let dup = store
            .remember_superseding("x", "demo", "note", "competing replacement", &a.id)
            .expect("call");
        assert!(matches!(dup.outcome, SupersedeOutcome::AlreadySuperseded));
        assert_eq!(dup.superseded_by_existing.as_deref(), Some(b_id.as_str()));

        // Failure outcomes inserted nothing.
        assert_eq!(count(&store), after_valid);
        assert_eq!(after_valid, before + 1); // only B was ever added
    }

    // ---- M3: vector storage + hybrid fusion ------------------------------
    //
    // All hand-built vectors — no embedding model anywhere. These run in
    // every feature set: the storage and fusion layer is deliberately not
    // feature-gated, only the model-loading glue is.

    #[test]
    fn vector_upsert_round_trips_and_replaces_in_place() {
        let store = Store::open_in_memory().expect("open");
        let mem = store
            .remember("a", "vec", "note", "the fact")
            .expect("remember");

        store
            .vector_upsert(&mem.id, "test-model", &[1.0, 2.5, -3.0])
            .expect("upsert");
        assert_eq!(store.vector_count("test-model").expect("count"), 1);
        assert_eq!(
            store.vector_count("other-model").expect("count"),
            0,
            "counts are per model"
        );

        // Byte-exact round trip through the little-endian blob: an identical
        // query vector must cosine to exactly 1.0.
        let hits = store
            .vector_candidates(&[1.0, 2.5, -3.0], "test-model", None, 10, Validity::Current)
            .expect("candidates");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, mem.id);
        assert!((hits[0].1 - 1.0).abs() < 1e-6, "score: {}", hits[0].1);

        // Upsert replaces: same memory, new vector, still one row.
        store
            .vector_upsert(&mem.id, "test-model", &[0.0, 1.0, 0.0])
            .expect("re-upsert");
        assert_eq!(store.vector_count("test-model").expect("count"), 1);
        let hits = store
            .vector_candidates(&[0.0, 1.0, 0.0], "test-model", None, 10, Validity::Current)
            .expect("candidates");
        assert!((hits[0].1 - 1.0).abs() < 1e-6, "the new vector is in force");
    }

    #[test]
    fn vector_candidates_orders_by_cosine_and_respects_scope_and_validity() {
        let store = Store::open_in_memory().expect("open");
        let near = store.remember("a", "s1", "note", "near").expect("remember");
        let far = store.remember("a", "s1", "note", "far").expect("remember");
        let other = store
            .remember("a", "s2", "note", "other scope")
            .expect("remember");
        store
            .vector_upsert(&near.id, "m", &[1.0, 0.0])
            .expect("upsert");
        store
            .vector_upsert(&far.id, "m", &[0.0, 1.0])
            .expect("upsert");
        store
            .vector_upsert(&other.id, "m", &[1.0, 0.0])
            .expect("upsert");

        // Cosine ordering, across scopes.
        let hits = store
            .vector_candidates(&[1.0, 0.1], "m", None, 10, Validity::Current)
            .expect("candidates");
        let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[2], far.id, "the orthogonal vector ranks last");
        assert!(hits[0].1 > hits[2].1, "scores are descending");

        // Scope filter.
        let hits = store
            .vector_candidates(&[1.0, 0.0], "m", Some("s2"), 10, Validity::Current)
            .expect("candidates");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, other.id);

        // Limit truncates after sorting.
        let hits = store
            .vector_candidates(&[1.0, 0.1], "m", Some("s1"), 1, Validity::Current)
            .expect("candidates");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, near.id, "truncation keeps the best-scoring hit");

        // Validity: superseding `near` hides it from Current but not All.
        let replacement = store
            .remember_superseding("a", "s1", "note", "corrected", &near.id)
            .expect("supersede")
            .memory
            .expect("stored");
        store
            .vector_upsert(&replacement.id, "m", &[1.0, 0.0])
            .expect("upsert");
        let current = store
            .vector_candidates(&[1.0, 0.0], "m", Some("s1"), 10, Validity::Current)
            .expect("candidates");
        assert!(
            current.iter().all(|(id, _)| *id != near.id),
            "superseded rows are invisible under Current"
        );
        let all = store
            .vector_candidates(&[1.0, 0.0], "m", Some("s1"), 10, Validity::All)
            .expect("candidates");
        assert!(
            all.iter().any(|(id, _)| *id == near.id),
            "the full history still reaches them"
        );
    }

    #[test]
    fn vector_candidates_scores_zero_on_dimension_mismatch() {
        let store = Store::open_in_memory().expect("open");
        let mem = store
            .remember("a", "s", "note", "3d row")
            .expect("remember");
        store
            .vector_upsert(&mem.id, "m", &[1.0, 2.0, 3.0])
            .expect("upsert");
        let hits = store
            .vector_candidates(&[1.0, 2.0], "m", None, 10, Validity::Current)
            .expect("candidates");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, 0.0, "a width mismatch scores 0.0, never errors");
    }

    #[test]
    fn unindexed_memories_skips_rules_and_already_indexed_rows() {
        let store = Store::open_in_memory().expect("open");
        let first = store.remember("a", "s", "note", "one").expect("remember");
        let second = store.remember("a", "s", "note", "two").expect("remember");
        store
            .rule_add("a", "s", "some-policy", "Policy.")
            .expect("rule");

        let pending = store.unindexed_memories("m", None, 100).expect("unindexed");
        let ids: Vec<&str> = pending.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "the rule row is not a retrieval candidate");
        assert!(ids.contains(&first.id.as_str()));
        assert!(ids.contains(&second.id.as_str()));

        store.vector_upsert(&first.id, "m", &[1.0]).expect("upsert");
        let pending = store.unindexed_memories("m", None, 100).expect("unindexed");
        assert_eq!(pending.len(), 1, "an indexed row leaves the work queue");
        assert_eq!(pending[0].id, second.id);

        // Per-model: the other model still sees both.
        assert_eq!(
            store
                .unindexed_memories("other", None, 100)
                .expect("unindexed")
                .len(),
            2
        );

        // Scope filter narrows the queue.
        store
            .remember("a", "elsewhere", "note", "three")
            .expect("remember");
        assert_eq!(
            store
                .unindexed_memories("m", Some("s"), 100)
                .expect("unindexed")
                .len(),
            1
        );
    }

    #[test]
    fn search_hybrid_pulls_a_vector_only_hit_into_the_results() {
        let store = Store::open_in_memory().expect("open");
        let scope = "hybrid";
        let textual = store
            .remember("a", scope, "note", "the launch window opens at dawn")
            .expect("remember");
        // No shared vocabulary with the query — FTS5 alone can never find it.
        let semantic = store
            .remember("a", scope, "note", "liftoff begins when the sun rises")
            .expect("remember");
        store
            .vector_upsert(&textual.id, "m", &[1.0, 0.0])
            .expect("upsert");
        store
            .vector_upsert(&semantic.id, "m", &[0.9, 0.1])
            .expect("upsert");

        // FTS finds only `textual`; the query vector is near both.
        let fts_only = store
            .search("launch window", Some(scope), 10, Validity::Current)
            .expect("search");
        assert_eq!(
            fts_only.len(),
            1,
            "precondition: FTS alone misses the paraphrase"
        );

        let hybrid = store
            .search_hybrid(
                "launch window",
                &[1.0, 0.05],
                "m",
                Some(scope),
                10,
                Validity::Current,
            )
            .expect("hybrid");
        let ids: Vec<&str> = hybrid.memories.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&textual.id.as_str()));
        assert!(
            ids.contains(&semantic.id.as_str()),
            "the vector channel rescues the paraphrase: {ids:?}"
        );
        assert_eq!(
            ids[0], textual.id,
            "the id in both channels outranks the vector-only one"
        );
        assert_eq!(hybrid.fts_candidates, 1);
        assert_eq!(hybrid.vector_candidates, 2);

        // Limit still applies to the fused list.
        let one = store
            .search_hybrid(
                "launch window",
                &[1.0, 0.05],
                "m",
                Some(scope),
                1,
                Validity::Current,
            )
            .expect("hybrid");
        assert_eq!(one.memories.len(), 1);
    }

    #[test]
    fn search_hybrid_excludes_superseded_rows_under_current_and_skips_rules() {
        let store = Store::open_in_memory().expect("open");
        let scope = "hybrid-validity";
        let stale = store
            .remember("a", scope, "note", "telemetry cadence is five seconds")
            .expect("remember");
        store
            .vector_upsert(&stale.id, "m", &[1.0, 0.0])
            .expect("upsert");
        let fresh = store
            .remember_superseding(
                "a",
                scope,
                "note",
                "telemetry cadence is one second",
                &stale.id,
            )
            .expect("supersede")
            .memory
            .expect("stored");
        store
            .vector_upsert(&fresh.id, "m", &[1.0, 0.0])
            .expect("upsert");
        // A rule row that matches the query textually AND carries a vector
        // (hand-inserted; the index command would never do this).
        store
            .rule_add(
                "a",
                scope,
                "telemetry-policy",
                "telemetry cadence is policy",
            )
            .expect("rule");
        let rule_row_id: String = store
            .conn
            .query_row(
                "SELECT id FROM memories WHERE rule_id = 'telemetry-policy'",
                [],
                |r| r.get(0),
            )
            .expect("rule row id");
        store
            .vector_upsert(&rule_row_id, "m", &[1.0, 0.0])
            .expect("upsert");

        let current = store
            .search_hybrid(
                "telemetry cadence",
                &[1.0, 0.0],
                "m",
                Some(scope),
                10,
                Validity::Current,
            )
            .expect("hybrid");
        let ids: Vec<&str> = current.memories.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![fresh.id.as_str()],
            "only the current truth: {ids:?}"
        );

        let all = store
            .search_hybrid(
                "telemetry cadence",
                &[1.0, 0.0],
                "m",
                Some(scope),
                10,
                Validity::All,
            )
            .expect("hybrid");
        assert_eq!(
            all.memories.len(),
            2,
            "the full history keeps both versions but never the rule row"
        );
    }

    #[test]
    fn context_vector_channel_rescues_a_semantic_match_and_reports_it() {
        let store = Store::open_in_memory().expect("open");
        let scope = "ctx-vector";
        // The paraphrase is oldest and shares no vocabulary with the query;
        // then enough chatter that recency alone (at this budget) drops it.
        let semantic = store
            .remember("a", scope, "note", "liftoff begins when the sun rises")
            .expect("remember");
        std::thread::sleep(std::time::Duration::from_millis(2));
        for content in [
            "irrelevant chatter one",
            "irrelevant chatter two",
            "irrelevant chatter three",
            "irrelevant chatter four",
        ] {
            remember_spaced(&store, scope, content);
        }
        store
            .vector_upsert(&semantic.id, "m", &[1.0, 0.0])
            .expect("upsert");

        let vector = HybridQuery {
            model: "m",
            query_vec: &[1.0, 0.05],
        };
        let ctx = store
            .context(scope, Some("launch"), 50, 12, Some(vector))
            .expect("context");
        assert!(
            ctx.memories.iter().any(|m| m.id == semantic.id),
            "the vector channel pulls the paraphrase in: {:?}",
            ctx.memories
        );
        assert_eq!(ctx.budget.channels[&"vector"], 1);

        // Without the vector, the same call must not report the channel.
        let ctx = store
            .context(scope, Some("launch"), 50, 12, None)
            .expect("context");
        assert!(!ctx.budget.channels.contains_key("vector"));

        // No query means no vector channel even when a vector is supplied.
        let ctx = store
            .context(scope, None, 50, 12, Some(vector))
            .expect("context");
        assert!(!ctx.budget.channels.contains_key("vector"));
    }

    #[test]
    fn rule_purge_deletes_tombstones_only() {
        let store = Store::open_in_memory().expect("open");
        store
            .rule_add(
                "a",
                "demo",
                "doomed",
                "Temporary policy with searchable-marker.",
            )
            .expect("add");

        // Active rule: refused.
        assert_eq!(
            store.rule_purge("demo", "doomed").expect("purge"),
            PurgeOutcome::NotRetired
        );
        // Unknown rule: reported as such.
        assert_eq!(
            store.rule_purge("demo", "ghost").expect("purge"),
            PurgeOutcome::NotFound
        );

        store.rule_retire("demo", "doomed").expect("retire");
        assert_eq!(
            store.rule_purge("demo", "doomed").expect("purge"),
            PurgeOutcome::Purged
        );

        // Gone from the tombstone list AND from the FTS index.
        assert!(store
            .rules_including_retired("demo")
            .expect("list")
            .is_empty());
        assert!(store
            .search("searchable-marker", Some("demo"), 10, Validity::All)
            .expect("search")
            .is_empty());
    }

    // ---- M4: extracted-fact index ----------------------------------------
    //
    // Extraction is deterministic (`facts::extract`, no LLM), append-only
    // (INSERT OR REPLACE on deterministic ids), and stale facts are
    // filtered at query time through the parent JOIN — these tests pin all
    // three properties.

    #[test]
    fn facts_extract_is_idempotent_with_deterministic_ids() {
        let store = Store::open_in_memory().expect("open");
        let marked = store
            .remember(
                "a",
                "m4",
                "note",
                "Decided: the flange torque stays at spec.\njust chatter on a second line",
            )
            .expect("remember");
        store
            .remember("a", "m4", "note", "plain narrative with no markers at all")
            .expect("remember");
        let count = |store: &Store| -> i64 {
            store
                .conn
                .query_row("SELECT COUNT(*) FROM facts", [], |r| r.get(0))
                .expect("count")
        };

        // Dry run: full report, no rows.
        let dry = store.facts_extract(Some("m4"), true).expect("dry run");
        assert_eq!(dry.scanned, 2);
        assert_eq!(dry.memories_with_facts, 1);
        assert_eq!(dry.facts_written, 1);
        assert_eq!(count(&store), 0, "a dry run must write nothing");

        // Real run: one row, with the deterministic v5 id.
        let first = store.facts_extract(Some("m4"), false).expect("extract");
        assert_eq!(first.facts_written, 1);
        assert_eq!(count(&store), 1);
        let (id, extractor): (String, String) = store
            .conn
            .query_row("SELECT id, extractor FROM facts", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .expect("fact row");
        assert_eq!(
            id,
            crate::facts::fact_id(&marked.id, "Decided: the flange torque stays at spec."),
            "the row id is the deterministic UUID v5 over (memory, fact)"
        );
        assert_eq!(extractor, crate::facts::EXTRACTOR);

        // Idempotent: same report, no growth.
        let second = store.facts_extract(Some("m4"), false).expect("re-extract");
        assert_eq!(second.facts_written, first.facts_written);
        assert_eq!(count(&store), 1, "re-extraction must not grow the table");
    }

    #[test]
    fn facts_extract_skips_rule_rows_and_superseded_memories() {
        let store = Store::open_in_memory().expect("open");
        let scope = "m4-skip";
        // A rule whose text would extract if rules were eligible.
        store
            .rule_add(
                "a",
                scope,
                "flange-rule",
                "Never loosen the flange without a spotter.",
            )
            .expect("rule");
        // A superseded memory whose text would extract if history were.
        let old = store
            .remember(
                "a",
                scope,
                "note",
                "Decided: torque limit is five newton meters.",
            )
            .expect("remember");
        let new = store
            .remember_superseding(
                "a",
                scope,
                "note",
                "Decided: torque limit is six newton meters.",
                &old.id,
            )
            .expect("supersede")
            .memory
            .expect("stored");

        let report = store.facts_extract(Some(scope), false).expect("extract");
        assert_eq!(
            report.scanned, 1,
            "only the current, non-rule memory is walked"
        );
        assert_eq!(report.facts_written, 1);

        let parents: Vec<String> = store
            .conn
            .prepare("SELECT memory_id FROM facts")
            .expect("prepare")
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(
            parents,
            vec![new.id.clone()],
            "only the replacement was extracted from"
        );
    }

    #[test]
    fn fact_candidates_dedupes_parents_and_follows_parent_validity() {
        let store = Store::open_in_memory().expect("open");
        let scope = "m4-cand";
        // Two matching facts on one parent — must appear once.
        let multi = store
            .remember(
                "a",
                scope,
                "note",
                "Decided: use the zeta flange coupling.\n\
                 Gotcha: the zeta flange coupling seizes when cold.",
            )
            .expect("remember");
        let single = store
            .remember(
                "a",
                scope,
                "note",
                "TODO inspect the zeta flange coupling next week",
            )
            .expect("remember");
        // Matching content, but no marker — never extracted, never a candidate.
        store
            .remember(
                "a",
                scope,
                "note",
                "the zeta flange coupling came up in chatter",
            )
            .expect("remember");
        store.facts_extract(Some(scope), false).expect("extract");

        let hits = store
            .fact_candidates("zeta flange coupling", Some(scope), 10, Validity::Current)
            .expect("candidates");
        assert_eq!(
            hits.len(),
            2,
            "three matching facts, two distinct parents: {hits:?}"
        );
        assert!(hits.contains(&multi.id));
        assert!(hits.contains(&single.id));

        // Superseding the parent retires its facts at QUERY time: the fact
        // rows are untouched (append-only v1), but the parent JOIN's
        // validity clause stops surfacing them under Current.
        store
            .remember_superseding("a", scope, "note", "replacement without markers", &multi.id)
            .expect("supersede");
        let current = store
            .fact_candidates("zeta flange coupling", Some(scope), 10, Validity::Current)
            .expect("candidates");
        assert_eq!(
            current,
            vec![single.id.clone()],
            "the superseded parent is gone"
        );
        let all = store
            .fact_candidates("zeta flange coupling", Some(scope), 10, Validity::All)
            .expect("candidates");
        assert!(
            all.contains(&multi.id),
            "the full-history view still reaches the stale facts' parent"
        );
    }

    #[test]
    fn context_facts_channel_boosts_the_marker_parent_and_reports_it() {
        let store = Store::open_in_memory().expect("open");
        let scope = "ctx-facts";
        // The decision memory is OLDEST and LONG: last in the recency
        // channel, and its BM25 rank trails the short chatter (one "flange"
        // in a long document vs one in three words). Only the facts channel
        // puts it at rank 0 — without that boost it loses the budget race.
        let decided = store
            .remember(
                "a",
                scope,
                "note",
                "Decided: adopt the flange protocol for docking. The rest of this \
                 memory is deliberately long filler so its rank in the full-text \
                 channel trails the short mentions that follow it.",
            )
            .expect("remember");
        std::thread::sleep(std::time::Duration::from_millis(2));
        for content in [
            "flange chatter one",
            "flange chatter two",
            "flange chatter three",
        ] {
            remember_spaced(&store, scope, content);
        }
        store.facts_extract(Some(scope), false).expect("extract");

        // A budget of exactly the decision memory's cost: whichever
        // candidate is selected first consumes it all.
        let budget = crate::retrieval::estimate_tokens(&decided.content);
        let ctx = store
            .context(scope, Some("flange"), 50, budget, None)
            .expect("context");
        let ids: Vec<&str> = ctx.memories.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![decided.id.as_str()],
            "the facts channel ranks the decision memory first: {:?}",
            ctx.memories
        );
        assert_eq!(ctx.budget.channels[&"facts"], 1);
        assert_eq!(ctx.budget.channels[&"fts"], 4);
        assert_eq!(ctx.budget.dropped, 3);

        // No query: the facts channel does not run and is not reported.
        let ctx = store
            .context(scope, None, 50, budget, None)
            .expect("context");
        assert!(!ctx.budget.channels.contains_key("facts"));
    }

    #[test]
    fn search_hybrid_fuses_the_facts_channel_and_reports_it() {
        let store = Store::open_in_memory().expect("open");
        let scope = "hybrid-facts";
        let marker = store
            .remember(
                "a",
                scope,
                "note",
                "Decided: the flange protocol is frozen for the mission duration.",
            )
            .expect("remember");
        let plain = store
            .remember(
                "a",
                scope,
                "note",
                "the flange came up in passing conversation",
            )
            .expect("remember");
        // Only the plain memory carries a vector; only the marker memory
        // yields a fact — every channel contributes something distinct.
        store
            .vector_upsert(&plain.id, "m", &[1.0, 0.0])
            .expect("upsert");
        store.facts_extract(Some(scope), false).expect("extract");

        let hybrid = store
            .search_hybrid(
                "flange",
                &[1.0, 0.0],
                "m",
                Some(scope),
                10,
                Validity::Current,
            )
            .expect("hybrid");
        assert_eq!(hybrid.fts_candidates, 2);
        assert_eq!(hybrid.vector_candidates, 1);
        assert_eq!(
            hybrid.facts_candidates, 1,
            "one parent with a matching fact"
        );
        let ids: Vec<&str> = hybrid.memories.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&marker.id.as_str()));
        assert!(ids.contains(&plain.id.as_str()));
    }

    // ---- M5: access tracking + consolidation -----------------------------
    //
    // Access columns are internal (never serialized), reads bump only what
    // they return, dedup supersedes but never deletes, and the report phase
    // is pure analysis. The narrowed FTS trigger keeps all of it from
    // churning the full-text index.

    /// Reads the raw access columns for one memory id.
    fn access(store: &Store, id: &str) -> (Option<i64>, Option<String>) {
        store
            .conn
            .query_row(
                "SELECT access_count, last_accessed_at FROM memories WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("access columns")
    }

    #[test]
    fn migration_adds_access_columns_and_narrows_the_fts_update_trigger() {
        // The legacy-schema fixture: migrate() must add both columns and
        // install the content-narrowed trigger even over a pre-M5 database
        // whose memories_au fired on every update.
        let store = store();
        let columns = table_columns(&store.conn, "memories").expect("columns");
        assert!(columns.iter().any(|c| c == "last_accessed_at"));
        assert!(columns.iter().any(|c| c == "access_count"));

        let trigger_sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'memories_au'",
                [],
                |r| r.get(0),
            )
            .expect("memories_au exists");
        assert!(
            trigger_sql.contains("UPDATE OF content"),
            "the update trigger must fire on content changes only: {trigger_sql}"
        );
    }

    #[test]
    fn tracked_reads_and_supersession_leave_the_fts_index_consistent() {
        let store = Store::open_in_memory().expect("open");
        let scope = "fts-consistency";
        let a = store
            .remember("a", scope, "note", "unique-marker-alpha content")
            .expect("remember");
        store
            .remember("a", scope, "note", "unique-marker-beta content")
            .expect("remember");

        // Tracked reads (recall bumps access columns via UPDATE): with the
        // old full-row trigger every one of these churned the FTS index.
        for _ in 0..3 {
            store.recall(scope, 10, Validity::Current).expect("recall");
        }
        let hits = store
            .search("unique-marker-alpha", Some(scope), 10, Validity::All)
            .expect("search");
        assert_eq!(
            hits.len(),
            1,
            "after tracked reads the row is indexed exactly once: {hits:?}"
        );

        // Supersession (valid_to/superseded_by UPDATE) no longer touches
        // FTS either: the old row stays findable exactly once under All.
        store
            .remember_superseding("a", scope, "note", "replacement text", &a.id)
            .expect("supersede");
        let hits = store
            .search("unique-marker-alpha", Some(scope), 10, Validity::All)
            .expect("search");
        assert_eq!(
            hits.len(),
            1,
            "the superseded row's FTS entry survives, once: {hits:?}"
        );

        // Content updates still reindex: a rule revision must be findable
        // under its new wording, not its old one.
        store
            .rule_add("a", scope, "some-rule", "original-wording here")
            .expect("rule");
        store
            .rule_add("a", scope, "some-rule", "revised-wording here")
            .expect("revise");
        assert_eq!(
            store
                .search("revised-wording", Some(scope), 10, Validity::All)
                .expect("search")
                .len(),
            1,
            "content updates must still reach the index"
        );
        assert!(
            store
                .search("original-wording", Some(scope), 10, Validity::All)
                .expect("search")
                .is_empty(),
            "the old wording must have left the index"
        );
    }

    #[test]
    fn recall_bumps_access_count_once_per_returned_row() {
        let store = Store::open_in_memory().expect("open");
        let scope = "track-recall";
        let first = store.remember("a", scope, "note", "one").expect("remember");
        let second = store.remember("a", scope, "note", "two").expect("remember");

        assert_eq!(
            access(&store, &first.id),
            (None, None),
            "unread rows stay NULL"
        );

        store.recall(scope, 10, Validity::Current).expect("recall");
        let (count, at) = access(&store, &first.id);
        assert_eq!(count, Some(1), "NULL reads as 0 and bumps to 1");
        assert!(at.expect("last_accessed_at set").ends_with('Z'));
        assert_eq!(access(&store, &second.id).0, Some(1));

        store.recall(scope, 10, Validity::Current).expect("recall");
        assert_eq!(access(&store, &first.id).0, Some(2), "1 bumps to 2");
        assert_eq!(access(&store, &second.id).0, Some(2));
    }

    #[test]
    fn only_returned_rows_bump_and_rule_reads_stay_untracked() {
        let store = Store::open_in_memory().expect("open");
        let scope = "track-limit";
        let old = store
            .remember("a", scope, "note", "the oldest")
            .expect("remember");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let newest = store
            .remember("a", scope, "note", "the newest")
            .expect("remember");
        store
            .rule_add("a", scope, "some-policy", "Policy text.")
            .expect("rule");

        // limit 1 returns only the scope's most recent row — the rule row,
        // added last. Every row outside the limit must stay NULL.
        store.recall(scope, 1, Validity::Current).expect("recall");
        assert_eq!(
            access(&store, &old.id),
            (None, None),
            "a row outside the limit was not returned, so it must not bump"
        );
        assert_eq!(access(&store, &newest.id), (None, None));
        let rule_row_id: String = store
            .conn
            .query_row(
                "SELECT id FROM memories WHERE rule_id = 'some-policy'",
                [],
                |r| r.get(0),
            )
            .expect("rule row id");
        assert_eq!(
            access(&store, &rule_row_id).0,
            Some(1),
            "the one returned row (the rule row, via recall) bumps"
        );

        // Rules are policy, not retrieval: rules() never tracks.
        store.rules(scope).expect("rules");
        store.rules(scope).expect("rules again");
        assert_eq!(
            access(&store, &rule_row_id).0,
            Some(1),
            "rule listing must never bump access counters"
        );

        // get() tracks exactly its returned row.
        store.get(&newest.id, Validity::Current).expect("get");
        assert_eq!(access(&store, &newest.id).0, Some(1), "get bumps once");
        assert_eq!(access(&store, &old.id), (None, None));
    }

    #[test]
    fn set_tracking_off_leaves_the_columns_null() {
        let mut store = Store::open_in_memory().expect("open");
        let scope = "track-off";
        let mem = store
            .remember("a", scope, "note", "auditable content")
            .expect("remember");

        store.set_tracking(false);
        store.recall(scope, 10, Validity::Current).expect("recall");
        store
            .search("auditable", Some(scope), 10, Validity::Current)
            .expect("search");
        store.get(&mem.id, Validity::Current).expect("get");
        store
            .context(scope, Some("auditable"), 50, 1000, None)
            .expect("context");
        assert_eq!(
            access(&store, &mem.id),
            (None, None),
            "no read path may bump while tracking is off"
        );

        store.set_tracking(true);
        store.recall(scope, 10, Validity::Current).expect("recall");
        assert_eq!(access(&store, &mem.id).0, Some(1), "tracking resumes");
    }

    #[test]
    fn memory_output_never_carries_the_access_columns() {
        let store = Store::open_in_memory().expect("open");
        let scope = "track-shape";
        store
            .remember("a", scope, "note", "shape check")
            .expect("remember");
        // Read it back TRACKED, so the columns are populated in SQLite —
        // and still absent from the serialized Memory.
        let recalled = store.recall(scope, 10, Validity::Current).expect("recall");
        let json = serde_json::to_string(&recalled[0]).expect("serialize");
        assert!(
            !json.contains("access_count") && !json.contains("last_accessed_at"),
            "access columns are internal; row_to_memory must not read them: {json}"
        );
        // The plain-memory JSON key set is exactly the pre-M5 shape.
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["agent", "content", "created_at", "id", "role", "scope"],
            "plain JSON unchanged"
        );
    }

    #[test]
    fn mark_superseded_by_reuses_m2_semantics_without_a_new_row() {
        let store = Store::open_in_memory().expect("open");
        let scope = "mark-superseded";
        let loser = store
            .remember("a", scope, "note", "the old duplicate")
            .expect("remember");
        let winner = store
            .remember("a", scope, "note", "the newer copy")
            .expect("remember");
        let count = |store: &Store| -> i64 {
            store
                .conn
                .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
                .expect("count")
        };
        let before = count(&store);

        let now = crate::time::now_iso8601();
        assert!(store
            .mark_superseded_by(&loser.id, &winner.id, &now)
            .expect("mark"));
        assert_eq!(count(&store), before, "no new row is inserted");

        // The chain is visible exactly as after remember --supersedes.
        let all = store.recall(scope, 10, Validity::All).expect("recall all");
        assert_eq!(all.len(), 2);
        let closed = all.iter().find(|m| m.id == loser.id).expect("loser row");
        assert_eq!(closed.valid_to.as_deref(), Some(now.as_str()));
        assert_eq!(closed.superseded_by.as_deref(), Some(winner.id.as_str()));
        let current = store
            .recall(scope, 10, Validity::Current)
            .expect("recall current");
        assert_eq!(
            current.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec![winner.id.as_str()]
        );

        // Idempotent: an already-closed window is left untouched.
        assert!(!store
            .mark_superseded_by(&loser.id, &winner.id, &now)
            .expect("re-mark"));
    }

    #[test]
    fn dedup_groups_normalized_exact_text_newest_wins_and_reruns_find_nothing() {
        let store = Store::open_in_memory().expect("open");
        let scope = "dedup-exact";
        // Three copies distinct only in case/whitespace, plus one bystander.
        let oldest = store
            .remember(
                "a",
                scope,
                "note",
                "Decided: keep the flange torque at spec.",
            )
            .expect("remember");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let middle = store
            .remember(
                "a",
                scope,
                "note",
                "  decided:   keep the FLANGE torque at spec.  ",
            )
            .expect("remember");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let newest = store
            .remember(
                "a",
                scope,
                "note",
                "DECIDED: KEEP THE FLANGE TORQUE AT SPEC.",
            )
            .expect("remember");
        let bystander = store
            .remember("a", scope, "note", "unrelated chatter entirely")
            .expect("remember");

        // Report-only: the group is named, nothing changes.
        let report = store
            .consolidate_dedup(Some(scope), None, false)
            .expect("dedup");
        assert_eq!(report.scanned, 4);
        assert!(!report.applied);
        assert_eq!(report.superseded, 0);
        assert_eq!(report.groups.len(), 1);
        let group = &report.groups[0];
        assert_eq!(group.winner, newest.id, "max created_at wins");
        assert_eq!(group.losers.len(), 2);
        assert!(group.losers.contains(&oldest.id));
        assert!(group.losers.contains(&middle.id));
        assert_eq!(group.detectors, vec!["exact"]);
        assert_eq!(
            store
                .recall(scope, 10, Validity::Current)
                .expect("recall")
                .len(),
            4,
            "report-only leaves every row Current"
        );

        // Apply: the two losers are superseded by the winner; the bystander
        // and the winner stay Current. Nothing is deleted.
        let applied = store
            .consolidate_dedup(Some(scope), None, true)
            .expect("dedup --yes");
        assert!(applied.applied);
        assert_eq!(applied.superseded, 2);
        let current = store.recall(scope, 10, Validity::Current).expect("recall");
        let ids: Vec<&str> = current.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&newest.id.as_str()));
        assert!(ids.contains(&bystander.id.as_str()));
        assert_eq!(ids.len(), 2);
        let closed = store
            .get(&oldest.id, Validity::All)
            .expect("get")
            .expect("row survives");
        assert_eq!(closed.superseded_by.as_deref(), Some(newest.id.as_str()));

        // Idempotent: the losers are no longer Current, so a second run
        // finds nothing at all.
        let rerun = store
            .consolidate_dedup(Some(scope), None, true)
            .expect("re-dedup");
        assert!(rerun.groups.is_empty(), "{:?}", rerun.groups);
        assert_eq!(rerun.superseded, 0);
    }

    #[test]
    fn dedup_vector_detector_pairs_near_identical_vectors_within_a_scope() {
        // Hand-built vectors, no model anywhere — the detector compares
        // stored embeddings, so it is exercised in every feature set (the
        // same posture as the search_hybrid tests).
        let store = Store::open_in_memory().expect("open");
        let scope = "dedup-vector";
        let stale = store
            .remember("a", scope, "note", "telemetry cadence is five seconds")
            .expect("remember");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let fresh = store
            .remember("a", scope, "note", "cadence of telemetry: five seconds")
            .expect("remember");
        let other = store
            .remember("a", scope, "note", "reactor coolant flow is nominal")
            .expect("remember");
        // A same-vector twin in ANOTHER scope: must never pair.
        let elsewhere = store
            .remember(
                "a",
                "other-scope",
                "note",
                "telemetry cadence is five seconds!!",
            )
            .expect("remember");
        store
            .vector_upsert(&stale.id, "m", &[1.0, 0.0])
            .expect("upsert");
        store
            .vector_upsert(&fresh.id, "m", &[0.999, 0.01])
            .expect("upsert");
        store
            .vector_upsert(&other.id, "m", &[0.0, 1.0])
            .expect("upsert");
        store
            .vector_upsert(&elsewhere.id, "m", &[1.0, 0.0])
            .expect("upsert");

        // Without the model, the texts normalize differently: no group.
        let text_only = store
            .consolidate_dedup(Some(scope), None, false)
            .expect("dedup");
        assert!(text_only.groups.is_empty());

        // With the model, the near-identical vectors pair — and only they.
        let report = store
            .consolidate_dedup(Some(scope), Some("m"), false)
            .expect("dedup vector");
        assert_eq!(report.groups.len(), 1);
        let group = &report.groups[0];
        assert_eq!(group.winner, fresh.id, "the newer of the pair wins");
        assert_eq!(group.losers, vec![stale.id.clone()]);
        assert_eq!(group.detectors, vec!["vector"]);

        // Across ALL scopes the same-vector twin still pairs with nothing:
        // vector pairs are same-scope only.
        let all_scopes = store
            .consolidate_dedup(None, Some("m"), false)
            .expect("dedup");
        assert!(
            all_scopes
                .groups
                .iter()
                .all(|g| !g.losers.contains(&elsewhere.id) && g.winner != elsewhere.id),
            "cross-scope vectors must never form a group: {:?}",
            all_scopes.groups
        );
    }

    #[test]
    fn contradiction_heuristic_needs_overlap_and_exactly_one_negation() {
        let store = Store::open_in_memory().expect("open");
        let scope = "contra";
        let plain = store
            .remember(
                "a",
                scope,
                "note",
                "the deploy pipeline uses docker for builds",
            )
            .expect("remember");
        let negated = store
            .remember(
                "a",
                scope,
                "note",
                "the deploy pipeline does not use docker for builds",
            )
            .expect("remember");
        // High overlap with the negated row but itself negated too: a
        // both-negated pair is never flagged, whatever the overlap. (Its
        // overlap with the plain row is diluted below the 0.5 line.)
        store
            .remember(
                "a",
                scope,
                "note",
                "remember we never said the deploy pipeline does not use docker \
                 for builds anymore honestly speaking friends",
            )
            .expect("remember");
        // One negated but almost no overlap: not flagged.
        store
            .remember("a", scope, "note", "the reactor is not overheating today")
            .expect("remember");

        let report = store.consolidate_report(Some(scope)).expect("report");
        assert_eq!(report.scanned, 4);
        assert_eq!(
            report.contradictions.len(),
            1,
            "exactly the high-overlap, single-negation pair: {:?}",
            report.contradictions
        );
        let pair = &report.contradictions[0];
        assert_eq!(pair.a, plain.id);
        assert_eq!(pair.b, negated.id);
        assert_eq!(pair.negated, negated.id, "the negated side is named");
        assert!(pair.jaccard >= 0.5, "jaccard: {}", pair.jaccard);
    }

    #[test]
    fn decay_ranks_the_old_unaccessed_above_the_new_hot() {
        let store = Store::open_in_memory().expect("open");
        let scope = "decay";
        // An old, never-read row (hand-inserted so its age is real).
        store
            .conn
            .execute(
                "INSERT INTO memories (id, agent, scope, role, content, created_at)
                 VALUES ('old-cold', 'a', ?1, 'note', 'forgotten lore', '2026-01-01T00:00:00Z')",
                params![scope],
            )
            .expect("insert old row");
        // A brand-new, heavily-read row.
        let hot = store
            .remember("a", scope, "note", "everyone reads this")
            .expect("remember");
        store
            .conn
            .execute(
                "UPDATE memories SET access_count = 50, last_accessed_at = ?1 WHERE id = ?2",
                params![crate::time::now_iso8601(), hot.id],
            )
            .expect("mark hot");

        let report = store.consolidate_report(Some(scope)).expect("report");
        assert_eq!(report.decay.len(), 2);
        let top = &report.decay[0];
        assert_eq!(
            top.id, "old-cold",
            "age + un-accessedness outranks recency + heat: {:?}",
            report.decay
        );
        assert_eq!(top.access_count, 0, "NULL access_count reads as 0");
        assert!(top.last_accessed_at.is_none());
        assert!(top.age_days > 100.0, "age_days: {}", top.age_days);
        let runner_up = &report.decay[1];
        assert_eq!(runner_up.access_count, 50);
        assert!(runner_up.last_accessed_at.is_some());
        assert!(
            top.staleness > runner_up.staleness,
            "staleness is monotone in age and un-accessedness"
        );
    }
}
