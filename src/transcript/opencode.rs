// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Reader for Opencode's session store, and for Z.ai's Z Code.
//!
//! One reader serves both because Z Code's CLI is an Opencode fork carrying the
//! same `session`/`message`/`part` schema — the same relationship
//! [`super::claude_code`] already has with OpenClaude. Only the database's
//! location differs, and that comes from the harness spec.
//!
//! The store is SQLite rather than a file per session, which changes two things
//! about the reader's shape. Every session shares one path, so [`read`] takes
//! the session id as well as the path; and the database belongs to a program
//! that may be running right now, so it is opened **read-only**. Engram has no
//! business writing to another tool's store, and a read-only handle makes that
//! structural rather than a promise.
//!
//! Reconstructing a turn is a two-table join. `message` holds the role and the
//! time; the text lives in `part` rows keyed by message, ordered by their own
//! creation time. A part is typed, and the types are what the filtering
//! contract in [`super`] is expressed over: on this machine a real store held
//! `tool` 9854, `step-start` 6439, `step-finish` 6335, `reasoning` 3945, `text`
//! 3322 and `patch` 461 — so the overwhelming majority of parts are *not*
//! conversation, and counting them rather than dropping them silently is the
//! whole point.

use std::path::Path;

use rusqlite::OpenFlags;

use super::{
    normalize_text, normalize_timestamp, redact, FilterStats, ReadOptions, ReadResult, SessionRef,
    TranscriptError, Turn, TurnRole,
};
use crate::harness::HarnessSpec;

/// Opens a harness's store without any possibility of writing to it.
///
/// `SQLITE_OPEN_READ_ONLY` is the point: this is somebody else's database,
/// very possibly open in another process while engram reads it.
/// Maps a `rusqlite` failure into the transcript error type.
///
/// Shared by the store-backed readers so they cannot disagree about how a
/// query failure surfaces.
pub(super) fn sql_err(e: rusqlite::Error) -> TranscriptError {
    TranscriptError::Io(std::io::Error::other(e.to_string()))
}

pub(super) fn open_readonly(path: &Path) -> Result<rusqlite::Connection, TranscriptError> {
    rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(sql_err)
}

/// Resolves the harness's database file.
pub(super) fn store_path(spec: &HarnessSpec) -> Result<std::path::PathBuf, TranscriptError> {
    let rel = spec.sessions_dir.ok_or(TranscriptError::NoReader(
        "this harness declares no session store",
    ))?;
    crate::harness::in_home(rel).ok_or(TranscriptError::NoHome)
}

/// Lists the sessions this harness recorded for `cwd`.
///
/// Matched on `session.directory`, which the harness records verbatim, so
/// there is no name to mangle and no mapping to invert — the same situation as
/// Codex, and the opposite of Claude Code.
///
/// # Errors
///
/// Returns [`TranscriptError::NoHome`] when `$HOME` is unset, and an I/O error
/// when the store exists but cannot be opened. A store that does not exist is
/// an empty list, not an error: a harness that is installed but has never been
/// run is not a failure.
pub fn sessions(spec: &HarnessSpec, cwd: &Path) -> Result<Vec<SessionRef>, TranscriptError> {
    let path = store_path(spec)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = open_readonly(&path)?;
    let wanted = cwd.to_string_lossy().into_owned();

    // `bytes` is the size of this *session*, summed from its parts, not the
    // size of the store. Reporting the file would say 550 MB for every session
    // in it, which is both useless to a reader and — as `--max-bytes` found out
    // — actively wrong to compare a ceiling against.
    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.directory, COALESCE(SUM(LENGTH(p.data)), 0) \
             FROM session s LEFT JOIN part p ON p.session_id = s.id \
             WHERE s.directory = ?1 \
             GROUP BY s.id, s.directory, s.time_created \
             ORDER BY s.time_created DESC, s.id DESC",
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
        let (id, directory, bytes) = row.map_err(sql_err)?;
        out.push(SessionRef {
            harness: spec.id,
            session_id: id,
            path: path.to_string_lossy().into_owned(),
            cwd: Some(directory),
            bytes: u64::try_from(bytes).unwrap_or(0),
        });
    }
    Ok(out)
}

/// Converts the harness's epoch-milliseconds to ISO 8601 UTC.
///
/// An out-of-range value is an error rather than a substitution, for the reason
/// stated in [`super`]: `recall` orders by `created_at`, so a wall-clock
/// fallback would destroy reading order without saying so.
pub(super) fn epoch_millis(ms: i64, record: &str) -> Result<String, TranscriptError> {
    jiff::Timestamp::from_millisecond(ms)
        .map_err(|_| TranscriptError::BadTimestamp {
            record: record.to_string(),
            value: ms.to_string(),
        })
        .map(|t| t.to_string())
}

/// Reads one session out of the store.
///
/// # Errors
///
/// Returns an I/O error when the store cannot be read, and
/// [`TranscriptError::BadTimestamp`] for a time that is not a valid instant.
pub fn read(
    path: &Path,
    session_id: &str,
    opts: &ReadOptions,
) -> Result<ReadResult, TranscriptError> {
    // The ceiling applies to this session's own content, not to the file.
    //
    // For a harness that writes one file per session the two are the same
    // thing, which is why the check reads as a file-size check in the other
    // readers. Here they are not: the store held 550 MB of *all* sessions, so
    // measuring the file refused every session in it with "transcript is
    // 550273024 bytes" — a transcript nobody asked for. The guard's real job is
    // to stop one runaway conversation exhausting memory, so it is enforced
    // below against the text actually accumulated.
    let conn = open_readonly(path)?;
    let mut stats = FilterStats::default();
    let mut redactions = redact::Redactions::default();
    let mut turns = Vec::new();
    let mut accumulated: u64 = 0;

    let mut messages = conn
        .prepare(
            "SELECT id, data, time_created FROM message \
             WHERE session_id = ?1 ORDER BY time_created ASC, id ASC",
        )
        .map_err(sql_err)?;
    let rows = messages
        .query_map([session_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(sql_err)?;

    let mut parts = conn
        .prepare(
            "SELECT data FROM part WHERE message_id = ?1 \
             ORDER BY time_created ASC, id ASC",
        )
        .map_err(sql_err)?;

    for row in rows {
        let (id, data, created) = row.map_err(sql_err)?;
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&data) else {
            stats.torn_line += 1;
            continue;
        };
        let role = match msg.get("role").and_then(serde_json::Value::as_str) {
            Some("user") => TurnRole::User,
            Some("assistant") => TurnRole::Assistant,
            // A role engram does not know is a format change in a file it does
            // not own — the signal `unknown_record` exists to raise.
            _ => {
                stats.unknown_record += 1;
                continue;
            }
        };

        let part_rows = parts
            .query_map([&id], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;

        let mut text = String::new();
        for part in part_rows {
            let part = part.map_err(sql_err)?;
            let Ok(p) = serde_json::from_str::<serde_json::Value>(&part) else {
                stats.torn_line += 1;
                continue;
            };
            match p.get("type").and_then(serde_json::Value::as_str) {
                Some("text") => {
                    if let Some(t) = p.get("text").and_then(serde_json::Value::as_str) {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(t);
                    }
                }
                Some("reasoning") => {
                    stats.thinking += 1;
                    if opts.include_thinking {
                        if let Some(t) = p.get("text").and_then(serde_json::Value::as_str) {
                            if !text.is_empty() {
                                text.push_str("\n\n");
                            }
                            text.push_str(t);
                        }
                    }
                }
                Some("tool") => {
                    stats.tool_use += 1;
                    // Summarized at most, never stored: a tool payload is where
                    // file contents, command output and credentials live.
                    if opts.include_tools {
                        let name = p
                            .get("tool")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("tool");
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(&format!("[{name}: {} bytes]", part.len()));
                    }
                }
                // Step markers, file references and patches are structure, not
                // speech. Counted so a format change is visible.
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
        let created_at = epoch_millis(created, &id)?;
        // The harness's own value is normalized through the same path a
        // string-valued timestamp takes, so both readers agree on the shape.
        let created_at =
            normalize_timestamp(&created_at).map_err(|value| TranscriptError::BadTimestamp {
                record: id.clone(),
                value,
            })?;
        turns.push(Turn {
            source_uuid: id,
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

    /// Builds a store with Opencode's schema and one session.
    fn fixture(path: &Path) {
        let conn = rusqlite::Connection::open(path).expect("create store");
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL, \
                                   time_created INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, \
                                   time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, \
                                time_created INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .expect("schema");
        conn.execute(
            "INSERT INTO session VALUES ('ses_1', '/work/project', 1779491760000)",
            [],
        )
        .expect("session");
        conn.execute(
            "INSERT INTO message VALUES ('msg_1', 'ses_1', 1779491760492, '{\"role\":\"user\"}')",
            [],
        )
        .expect("m1");
        conn.execute(
            "INSERT INTO message VALUES ('msg_2', 'ses_1', 1779491785525, '{\"role\":\"assistant\"}')",
            [],
        )
        .expect("m2");
        conn.execute_batch(
            "INSERT INTO part VALUES ('p1','msg_1',1,'{\"type\":\"text\",\"text\":\"fix the build\"}');
             INSERT INTO part VALUES ('p2','msg_2',1,'{\"type\":\"reasoning\",\"text\":\"secret plan\"}');
             INSERT INTO part VALUES ('p3','msg_2',2,'{\"type\":\"tool\",\"tool\":\"edit\"}');
             INSERT INTO part VALUES ('p4','msg_2',3,'{\"type\":\"step-start\"}');
             INSERT INTO part VALUES ('p5','msg_2',4,'{\"type\":\"text\",\"text\":\"renamed it\"}');",
        )
        .expect("parts");
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
    fn reads_two_turns_and_counts_what_it_drops() {
        let tmp = std::env::temp_dir().join(format!("engram-oc-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        fixture(&tmp);

        let result = read(&tmp, "ses_1", &opts()).expect("reads");
        assert_eq!(result.turns.len(), 2);
        assert_eq!(result.turns[0].role, TurnRole::User);
        assert_eq!(result.turns[0].text, "fix the build");
        // Two text parts of one message join into one turn.
        assert_eq!(result.turns[1].text, "renamed it");
        // Everything else is counted, never silently skipped.
        assert_eq!(result.filtered.thinking, 1);
        assert_eq!(result.filtered.tool_use, 1);
        assert_eq!(result.filtered.non_message, 1);
        // Epoch milliseconds become ISO 8601 UTC.
        assert!(result.turns[0].created_at.ends_with('Z'));
        let _ = std::fs::remove_file(&tmp);
    }

    /// Thinking is excluded by default and included on request — and a tool
    /// payload is summarized either way, never stored.
    #[test]
    fn include_flags_widen_without_storing_payloads() {
        let tmp = std::env::temp_dir().join(format!("engram-oc2-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        fixture(&tmp);

        let mut o = opts();
        o.include_thinking = true;
        o.include_tools = true;
        let result = read(&tmp, "ses_1", &o).expect("reads");
        let assistant = &result.turns[1].text;
        assert!(assistant.contains("secret plan"), "{assistant}");
        assert!(assistant.contains("[edit:"), "{assistant}");
        let _ = std::fs::remove_file(&tmp);
    }
}
