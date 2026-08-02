// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Verbatim chat memory store. SQLite + FTS5, single file, single source
//! of truth for every agent in the pipeline. No LLM calls to write a
//! memory — raw text in, raw text out.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

pub struct Store {
    conn: Connection,
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
/// `channels: {"fts": n, "vector": n}` in a budget envelope.
#[derive(Debug)]
pub struct HybridSearch {
    /// Fused results, best rank first (reciprocal-rank-fusion order).
    pub memories: Vec<Memory>,
    /// Candidates the FTS channel fed into the fusion.
    pub fts_candidates: usize,
    /// Candidates the vector channel fed into the fusion.
    pub vector_candidates: usize,
}

/// Candidates fetched per channel before hybrid fusion. Both the FTS and the
/// vector channel feed their top-50 into [`crate::retrieval::rrf_fuse`].
const HYBRID_CHANNEL_LIMIT: u32 = 50;

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
        Ok(Self { conn })
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

    /// Most recent memories in a scope, oldest last-N, chronological order.
    ///
    /// Rules are stored in this same table and are returned here alongside
    /// messages — recalling a scope should surface the policy that governs it.
    /// Use [`Store::rules`] when only rules are wanted. `validity` selects
    /// which slice of the bi-temporal history is visible.
    pub fn recall(
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

    /// Full-text search across all scopes, or restricted to one scope.
    /// `validity` selects which slice of the bi-temporal history is visible.
    pub fn search(
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

    /// Hybrid retrieval: FTS5 top-[`HYBRID_CHANNEL_LIMIT`] fused with the
    /// vector top-[`HYBRID_CHANNEL_LIMIT`] by reciprocal rank fusion
    /// (k = [`crate::retrieval::RRF_K`]), truncated to `limit`.
    ///
    /// Rule rows are dropped from both channels — policy is delivered
    /// through the rules section, not through search. Both channels read the
    /// same `validity` slice, so a superseded memory is invisible under
    /// [`Validity::Current`] on either path. Not feature-gated: it takes a
    /// precomputed query vector, so hand-built vectors exercise it in every
    /// build.
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
            .search(query, scope, HYBRID_CHANNEL_LIMIT, validity)?
            .into_iter()
            .filter(|m| m.rule_id.is_none())
            .collect();
        let vector_hits =
            self.vector_candidates(query_vec, model, scope, HYBRID_CHANNEL_LIMIT, validity)?;

        let fts_channel: Vec<String> = fts.iter().map(|m| m.id.clone()).collect();
        let vector_channel: Vec<String> = vector_hits.into_iter().map(|(id, _)| id).collect();
        let fts_candidates = fts_channel.len();
        let vector_candidates = vector_channel.len();
        let fused = rrf_fuse(&[fts_channel, vector_channel], RRF_K);

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
                None => self.get(&id, validity)?.filter(|m| m.rule_id.is_none()),
            };
            if let Some(m) = mem {
                memories.push(m);
            }
        }
        Ok(HybridSearch {
            memories,
            fts_candidates,
            vector_candidates,
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
    /// (newest-first) with the FTS relevance channel (rank order) when there
    /// is one. When the caller additionally supplies a [`HybridQuery`] (the
    /// auto-hybrid gate passed and the query was embedded), a third vector
    /// channel joins the same fusion — the same auto rule as search.
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
            .recall(scope, limit, Validity::Current)?
            .into_iter()
            .filter(|m| m.rule_id.is_none())
            .collect();
        let relevance: Vec<Memory> = match query {
            Some(q) => self
                .search(q, Some(scope), limit, Validity::Current)?
                .into_iter()
                .filter(|m| m.rule_id.is_none())
                .collect(),
            None => Vec::new(),
        };
        let recency_count = recency.len();
        let relevance_count = relevance.len();

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
            let mut channels = vec![recency_channel, relevance_channel];
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
        // Vector-only hits carry no row yet; fetch them under the same
        // Current view the other channels used (rule rows never qualify —
        // vector_candidates already excludes them, this is belt-and-braces).
        for id in &vector_ids {
            if !pool.contains_key(id) {
                if let Some(m) = self
                    .get(id, Validity::Current)?
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
        Ok(ContextResult {
            rules,
            memories,
            rules_tokens,
            budget,
        })
    }

    pub fn get(&self, id: &str, validity: Validity<'_>) -> rusqlite::Result<Option<Memory>> {
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
        let store = Store { conn };
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
}
