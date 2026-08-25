// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Reader for Copilot CLI's session store.
//!
//! The simplest of the store-backed readers, because the harness has already
//! done the work: `turns(session_id, turn_index, user_message,
//! assistant_response, timestamp)` is a flat, *pre-paired* transcript with no
//! JSON to parse at all, and `sessions.cwd` records the directory verbatim.
//!
//! Its entry in the harness table used to say the schema was undocumented and
//! the store therefore unreadable. It is neither — which is the reason that
//! entry, and two others like it, are now required to state only what was
//! actually probed.
//!
//! One structural difference from every other reader here: a row is *two*
//! turns, not one. The prompt and the reply share a row, so each is emitted
//! separately with the role it deserves, and the pair shares the row's single
//! timestamp — the harness records no separate time for the reply.

use std::path::Path;

use super::opencode::{open_readonly, sql_err, store_path};
use super::{
    normalize_text, normalize_timestamp, redact, FilterStats, ReadOptions, ReadResult, SessionRef,
    TranscriptError, Turn, TurnRole,
};
use crate::harness::HarnessSpec;

/// Lists the sessions Copilot CLI recorded for `cwd`.
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
            "SELECT s.id, s.cwd, \
                    COALESCE(SUM(LENGTH(COALESCE(t.user_message, '')) \
                             + LENGTH(COALESCE(t.assistant_response, ''))), 0) \
             FROM sessions s LEFT JOIN turns t ON t.session_id = s.id \
             WHERE s.cwd = ?1 \
             GROUP BY s.id, s.cwd, s.created_at \
             ORDER BY s.created_at DESC, s.id DESC",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([&wanted], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(sql_err)?;

    let mut out = Vec::new();
    for row in rows {
        let (id, dir, bytes) = row.map_err(sql_err)?;
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

/// Reads one Copilot CLI session.
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
            "SELECT turn_index, user_message, assistant_response, timestamp FROM turns \
             WHERE session_id = ?1 ORDER BY turn_index ASC",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([session_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(sql_err)?;

    for row in rows {
        let (index, user, assistant, timestamp) = row.map_err(sql_err)?;
        let record = format!("turn {index}");
        let Some(raw_time) = timestamp else {
            // A turn with no time cannot be ordered, and inventing one would
            // silently reorder the conversation. Counted, not guessed.
            stats.missing_uuid += 1;
            continue;
        };
        let created_at =
            normalize_timestamp(&raw_time).map_err(|value| TranscriptError::BadTimestamp {
                record: record.clone(),
                value,
            })?;

        // The prompt and the reply share one row and one timestamp. `turn_id`
        // folds `source_uuid` in, so the two halves need distinct suffixes or
        // the second would collide with the first and be dropped by
        // `INSERT OR IGNORE`.
        for (role, body, suffix) in [
            (TurnRole::User, user, "user"),
            (TurnRole::Assistant, assistant, "assistant"),
        ] {
            let Some(body) = body else {
                stats.empty += 1;
                continue;
            };
            accumulated = accumulated.saturating_add(body.len() as u64);
            if accumulated > opts.max_bytes {
                return Err(TranscriptError::TooLarge {
                    bytes: accumulated,
                    max_bytes: opts.max_bytes,
                });
            }
            let Some(text) = normalize_text(&body, opts.max_chars_per_turn, &mut stats) else {
                continue;
            };
            let text = redact::scrub(&text, &mut redactions);
            turns.push(Turn {
                source_uuid: format!("{index}:{suffix}"),
                role,
                text,
                created_at: created_at.clone(),
            });
        }
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
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT NOT NULL, \
                                    created_at TEXT NOT NULL);
             CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                                 session_id TEXT NOT NULL, turn_index INTEGER NOT NULL, \
                                 user_message TEXT, assistant_response TEXT, timestamp TEXT);
             INSERT INTO sessions VALUES ('s1', '/work/project', '2026-06-02T21:50:00.351Z');
             INSERT INTO turns (session_id, turn_index, user_message, assistant_response, timestamp)
               VALUES ('s1', 0, 'separate the CLI from the IDE', 'Done, in two commits.',
                       '2026-06-02T21:50:00.351Z');
             INSERT INTO turns (session_id, turn_index, user_message, assistant_response, timestamp)
               VALUES ('s1', 1, 'Make signed commit', NULL, '2026-06-02T21:52:34.150Z');",
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

    /// One row is two turns, and the halves keep distinct identities.
    #[test]
    fn a_row_becomes_a_prompt_and_a_reply() {
        let tmp = std::env::temp_dir().join(format!("engram-cop-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        fixture(&tmp);

        let result = read(&tmp, "s1", &opts()).expect("reads");
        assert_eq!(
            result.turns.len(),
            3,
            "two halves, then a prompt with no reply"
        );
        assert_eq!(result.turns[0].role, TurnRole::User);
        assert_eq!(result.turns[1].role, TurnRole::Assistant);
        assert_eq!(result.turns[0].source_uuid, "0:user");
        assert_eq!(result.turns[1].source_uuid, "0:assistant");
        // Both halves share the row's single timestamp — the harness records
        // no separate time for the reply.
        assert_eq!(result.turns[0].created_at, result.turns[1].created_at);
        // The unanswered prompt counts its missing half rather than inventing
        // an empty reply.
        assert_eq!(result.filtered.empty, 1);
        let _ = std::fs::remove_file(&tmp);
    }
}
