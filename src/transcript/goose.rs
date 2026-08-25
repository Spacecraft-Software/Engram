// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Reader for Goose's session store.
//!
//! The friendliest schema of the harnesses surveyed: `messages` carries `role`
//! as a first-class column rather than buried in a JSON blob, and
//! `sessions.working_dir` records the directory verbatim, so — as with Codex
//! and Opencode — there is no name to mangle and no mapping to invert.
//!
//! `content_json` is a small typed array, `[{"type":"text","text":…}]` or
//! `[{"type":"thinking","thinking":…}]`, which maps directly onto the filtering
//! contract in [`super`]: text is speech, thinking is excluded by default, and
//! anything else is counted rather than dropped in silence.
//!
//! Goose also has the best export of any harness here (`goose session export
//! --format markdown`), so this reader is a convenience rather than the only
//! way in — but a reader needs no manual step, which is the difference that
//! matters when capturing history continuously.

use std::path::Path;

use super::{
    normalize_text, normalize_timestamp, redact, FilterStats, ReadOptions, ReadResult, SessionRef,
    TranscriptError, Turn, TurnRole,
};
use crate::harness::HarnessSpec;

use super::opencode::{open_readonly, store_path};

/// Lists the sessions Goose recorded for `cwd`.
///
/// # Errors
///
/// [`TranscriptError::NoHome`] when `$HOME` is unset, and an I/O error when the
/// store exists but cannot be read. A missing store is an empty list.
pub fn sessions(spec: &HarnessSpec, cwd: &Path) -> Result<Vec<SessionRef>, TranscriptError> {
    let path = store_path(spec)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = open_readonly(&path)?;
    let wanted = cwd.to_string_lossy().into_owned();

    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.working_dir, COALESCE(SUM(LENGTH(m.content_json)), 0) \
             FROM sessions s LEFT JOIN messages m ON m.session_id = s.id \
             WHERE s.working_dir = ?1 \
             GROUP BY s.id, s.working_dir, s.created_at \
             ORDER BY s.created_at DESC, s.id DESC",
        )
        .map_err(super::opencode::sql_err)?;
    let rows = stmt
        .query_map([&wanted], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(super::opencode::sql_err)?;

    let mut out = Vec::new();
    for row in rows {
        let (id, dir, bytes) = row.map_err(super::opencode::sql_err)?;
        out.push(SessionRef {
            harness: spec.id,
            session_id: id,
            path: path.to_string_lossy().into_owned(),
            cwd: Some(dir),
            bytes: u64::try_from(bytes).unwrap_or(0),
        });
    }
    Ok(out)
}

/// Reads one Goose session.
///
/// # Errors
///
/// An I/O error when the store cannot be read, [`TranscriptError::TooLarge`]
/// when the session's own content exceeds the ceiling, and
/// [`TranscriptError::BadTimestamp`] for a time that is not a valid instant.
pub fn read(
    path: &Path,
    session_id: &str,
    opts: &ReadOptions,
) -> Result<ReadResult, TranscriptError> {
    let conn = open_readonly(path)?;
    let mut stats = FilterStats::default();
    let mut redactions = redact::Redactions::default();
    let mut turns = Vec::new();
    let mut accumulated: u64 = 0;

    let mut stmt = conn
        .prepare(
            "SELECT message_id, role, content_json, created_timestamp FROM messages \
             WHERE session_id = ?1 ORDER BY created_timestamp ASC, id ASC",
        )
        .map_err(super::opencode::sql_err)?;
    let rows = stmt
        .query_map([session_id], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .map_err(super::opencode::sql_err)?;

    for (index, row) in rows.enumerate() {
        let (message_id, role, content, created) = row.map_err(super::opencode::sql_err)?;
        let role = match role.as_str() {
            "user" => TurnRole::User,
            "assistant" => TurnRole::Assistant,
            _ => {
                stats.unknown_record += 1;
                continue;
            }
        };
        let Ok(parts) = serde_json::from_str::<Vec<serde_json::Value>>(&content) else {
            stats.torn_line += 1;
            continue;
        };

        let mut text = String::new();
        for part in &parts {
            match part.get("type").and_then(serde_json::Value::as_str) {
                Some("text") => {
                    if let Some(t) = part.get("text").and_then(serde_json::Value::as_str) {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(t);
                    }
                }
                Some("thinking") => {
                    stats.thinking += 1;
                    if opts.include_thinking {
                        if let Some(t) = part.get("thinking").and_then(serde_json::Value::as_str) {
                            if !text.is_empty() {
                                text.push_str("\n\n");
                            }
                            text.push_str(t);
                        }
                    }
                }
                Some("toolRequest" | "toolResponse" | "toolConfirmationRequest") => {
                    stats.tool_use += 1;
                    if opts.include_tools {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(&format!("[tool: {} bytes]", part.to_string().len()));
                    }
                }
                Some(_) => stats.non_message += 1,
                None => stats.unknown_record += 1,
            }
        }

        accumulated = accumulated.saturating_add(text.len() as u64);
        if accumulated > opts.max_bytes {
            return Err(TranscriptError::TooLarge {
                bytes: accumulated,
                max_bytes: opts.max_bytes,
            });
        }
        let Some(text) = normalize_text(&text, opts.max_chars_per_turn, &mut stats) else {
            continue;
        };
        let text = redact::scrub(&text, &mut redactions);

        // `message_id` is nullable in the schema, so identity falls back to the
        // row's position — enough to be stable for an append-only log, and the
        // same reasoning `codex` uses for records that carry no id of their own.
        let source_uuid = message_id.unwrap_or_else(|| format!("row:{index}"));
        let created_at = super::opencode::epoch_millis(created, &source_uuid)?;
        let created_at =
            normalize_timestamp(&created_at).map_err(|value| TranscriptError::BadTimestamp {
                record: source_uuid.clone(),
                value,
            })?;
        turns.push(Turn {
            source_uuid,
            role,
            text,
            created_at,
        });
    }

    Ok(ReadResult {
        turns,
        filtered: stats,
        redactions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &Path) {
        let conn = rusqlite::Connection::open(path).expect("create store");
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, working_dir TEXT NOT NULL, \
                                    created_at INTEGER NOT NULL);
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, message_id TEXT, \
                                    session_id TEXT NOT NULL, role TEXT NOT NULL, \
                                    content_json TEXT NOT NULL, created_timestamp INTEGER NOT NULL);
             INSERT INTO sessions VALUES ('s1', '/work/project', 1779491760);
             INSERT INTO messages (message_id, session_id, role, content_json, created_timestamp)
               VALUES ('m1','s1','user','[{\"type\":\"text\",\"text\":\"list my repos\"}]',1779491760000);
             INSERT INTO messages (message_id, session_id, role, content_json, created_timestamp)
               VALUES ('m2','s1','assistant','[{\"type\":\"thinking\",\"thinking\":\"ponder\"},{\"type\":\"text\",\"text\":\"here they are\"}]',1779491761000);
             INSERT INTO messages (message_id, session_id, role, content_json, created_timestamp)
               VALUES (NULL,'s1','assistant','[{\"type\":\"toolRequest\",\"id\":\"t1\"}]',1779491762000);",
        )
        .expect("schema");
    }

    fn opts() -> ReadOptions {
        ReadOptions {
            include_thinking: false,
            include_tools: false,
            include_sidechains: false,
            max_bytes: super::super::DEFAULT_MAX_BYTES,
            max_chars_per_turn: super::super::DEFAULT_MAX_CHARS_PER_TURN,
        }
    }

    #[test]
    fn reads_turns_and_excludes_thinking_by_default() {
        let tmp = std::env::temp_dir().join(format!("engram-goose-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        fixture(&tmp);

        let result = read(&tmp, "s1", &opts()).expect("reads");
        assert_eq!(
            result.turns.len(),
            2,
            "the tool-only message yields no turn"
        );
        assert_eq!(result.turns[0].role, TurnRole::User);
        assert_eq!(result.turns[1].text, "here they are", "thinking is dropped");
        assert_eq!(result.filtered.thinking, 1);
        assert_eq!(result.filtered.tool_use, 1);
        let _ = std::fs::remove_file(&tmp);
    }

    /// A row with no `message_id` still gets a stable identity from its
    /// position, so re-ingesting it does not duplicate.
    #[test]
    fn a_null_message_id_falls_back_to_the_row_position() {
        let tmp = std::env::temp_dir().join(format!("engram-goose2-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        fixture(&tmp);

        let mut o = opts();
        o.include_tools = true;
        let result = read(&tmp, "s1", &o).expect("reads");
        let last = result.turns.last().expect("a tool turn");
        assert_eq!(last.source_uuid, "row:2");
        let _ = std::fs::remove_file(&tmp);
    }
}
