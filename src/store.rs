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
        };
        self.conn.execute(
            "INSERT INTO memories (id, agent, scope, role, content, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![mem.id, mem.agent, mem.scope, mem.role, mem.content, mem.created_at],
        )?;
        Ok(mem)
    }

    /// Most recent memories in a scope, oldest last-N, chronological order.
    ///
    /// Rules are stored in this same table and are returned here alongside
    /// messages — recalling a scope should surface the policy that governs it.
    /// Use [`Store::rules`] when only rules are wanted.
    pub fn recall(&self, scope: &str, limit: u32) -> rusqlite::Result<Vec<Memory>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, agent, scope, role, content, created_at, rule_id, updated_at FROM memories
             WHERE scope = ?1 ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![scope, limit], row_to_memory)?;
        let mut out: Vec<Memory> = rows.collect::<Result<_, _>>()?;
        out.reverse(); // chronological for reading back into a prompt
        Ok(out)
    }

    /// Full-text search across all scopes, or restricted to one scope.
    pub fn search(
        &self,
        query: &str,
        scope: Option<&str>,
        limit: u32,
    ) -> rusqlite::Result<Vec<Memory>> {
        let query = &sanitize_fts_query(query);
        let sql = if scope.is_some() {
            "SELECT m.id, m.agent, m.scope, m.role, m.content, m.created_at, m.rule_id, m.updated_at
             FROM memories_fts f JOIN memories m ON m.rowid = f.rowid
             WHERE memories_fts MATCH ?1 AND m.scope = ?2
             ORDER BY rank LIMIT ?3"
        } else {
            "SELECT m.id, m.agent, m.scope, m.role, m.content, m.created_at, m.rule_id, m.updated_at
             FROM memories_fts f JOIN memories m ON m.rowid = f.rowid
             WHERE memories_fts MATCH ?1
             ORDER BY rank LIMIT ?2"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = if let Some(s) = scope {
            stmt.query_map(params![query, s, limit], row_to_memory)?
        } else {
            stmt.query_map(params![query, limit], row_to_memory)?
        };
        rows.collect()
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "wired into the surfaces at M3 (MCP `get` tool, GET /v1/memory/:id)"
        )
    )]
    pub fn get(&self, id: &str) -> rusqlite::Result<Option<Memory>> {
        self.conn
            .query_row(
                "SELECT id, agent, scope, role, content, created_at, rule_id, updated_at FROM memories WHERE id = ?1",
                params![id],
                row_to_memory,
            )
            .map(Some)
            .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })
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
    // Partial index: enforces one rule per (scope, rule_id) without constraining
    // ordinary messages, which all have a NULL rule_id.
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_memories_rule
         ON memories(scope, rule_id) WHERE rule_id IS NOT NULL;",
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
/// data. Wrap each token as an escaped quoted phrase so implicit AND still
/// works across tokens but no character inside a token can be interpreted
/// as an operator. This is the exact class of bug the TencentDB Agent
/// Memory project had to patch (fts5-query-sanitization) — worth avoiding
/// from the start.
fn sanitize_fts_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|tok| format!("\"{}\"", tok.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
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

        let recalled = store.recall("demo", 50).expect("recall");
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

        let all = store.recall("demo", 50).expect("recall");
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

        let last3 = store.recall("demo", 3).expect("recall limited");
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
        assert!(store.recall("nothing-here", 10).expect("recall").is_empty());
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

        let across = store.search("alpha", None, 10).expect("search all scopes");
        assert_eq!(across.len(), 2, "scope=None must search every scope");
        assert_eq!(
            across[0].scope, "s1",
            "FTS5 rank orders the denser, shorter document first"
        );
        assert_eq!(across[1].scope, "s2");

        let scoped = store
            .search("alpha", Some("s2"), 10)
            .expect("search one scope");
        assert_eq!(scoped.len(), 1, "scope=Some must filter to that scope");
        assert_eq!(scoped[0].scope, "s2");

        assert_eq!(
            store
                .search("alpha", None, 1)
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
            let hits = store.search(content, Some(&scope), 10).unwrap_or_else(|e| {
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
        assert!(store.search("", None, 10).is_err());
    }

    #[test]
    fn get_returns_some_for_known_and_none_for_unknown_ids() {
        let store = Store::open_in_memory().expect("open");
        let mem = store
            .remember("a", "demo", "note", "findable")
            .expect("remember");
        let found = store
            .get(&mem.id)
            .expect("get")
            .expect("known id must be Some");
        assert_eq!(found.id, mem.id);
        assert_eq!(found.content, "findable");
        assert!(store.get("no-such-id").expect("get unknown").is_none());
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
            store.get(&mem.id).expect("get").expect("row").content,
            multibyte
        );

        let big = "0123456789ABCDEF".repeat(640); // exactly 10 KiB
        assert_eq!(big.len(), 10 * 1024);
        let mem = store
            .remember("a", "demo", "note", &big)
            .expect("remember 10 KiB");
        assert_eq!(store.get(&mem.id).expect("get").expect("row").content, big);
    }
}
