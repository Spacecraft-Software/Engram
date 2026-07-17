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
    pub role: String, // "user" | "assistant" | "system" | "note"
    pub content: String,
    pub created_at: String, // ISO 8601 UTC
}

impl Store {
    /// Opens (creating if needed) the shared database file. Point every
    /// agent at the same path — that's the entire "shared memory" story.
    pub fn open(path: &std::path::Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
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
        Ok(Self { conn })
    }

    pub fn remember(&self, agent: &str, scope: &str, role: &str, content: &str) -> rusqlite::Result<Memory> {
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            agent: agent.to_string(),
            scope: scope.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            created_at: crate::time::now_iso8601(),
        };
        self.conn.execute(
            "INSERT INTO memories (id, agent, scope, role, content, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![mem.id, mem.agent, mem.scope, mem.role, mem.content, mem.created_at],
        )?;
        Ok(mem)
    }

    /// Most recent memories in a scope, oldest last-N, chronological order.
    pub fn recall(&self, scope: &str, limit: u32) -> rusqlite::Result<Vec<Memory>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, agent, scope, role, content, created_at FROM memories
             WHERE scope = ?1 ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![scope, limit], row_to_memory)?;
        let mut out: Vec<Memory> = rows.collect::<Result<_, _>>()?;
        out.reverse(); // chronological for reading back into a prompt
        Ok(out)
    }

    /// Full-text search across all scopes, or restricted to one scope.
    pub fn search(&self, query: &str, scope: Option<&str>, limit: u32) -> rusqlite::Result<Vec<Memory>> {
        let query = &sanitize_fts_query(query);
        let sql = if scope.is_some() {
            "SELECT m.id, m.agent, m.scope, m.role, m.content, m.created_at
             FROM memories_fts f JOIN memories m ON m.rowid = f.rowid
             WHERE memories_fts MATCH ?1 AND m.scope = ?2
             ORDER BY rank LIMIT ?3"
        } else {
            "SELECT m.id, m.agent, m.scope, m.role, m.content, m.created_at
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

    pub fn get(&self, id: &str) -> rusqlite::Result<Option<Memory>> {
        self.conn
            .query_row(
                "SELECT id, agent, scope, role, content, created_at FROM memories WHERE id = ?1",
                params![id],
                row_to_memory,
            )
            .map(Some)
            .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })
    }
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
    })
}
