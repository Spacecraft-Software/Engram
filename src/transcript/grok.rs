// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Reader for Grok Build's session logs.
//!
//! Grok keeps one directory per working directory under `~/.grok/sessions`,
//! named by percent-encoding the absolute path (`%2Fspacecraft-software`), and
//! one directory per session inside it. Unlike Claude Code's mangling that
//! mapping is reversible, so listing decodes each directory name and compares
//! it with the working directory exactly, as the Codex reader does.
//!
//! # Which file
//!
//! A session holds several logs. `chat_history.jsonl` is the model's context:
//! it carries no timestamps and no record ids, and its user turns arrive wrapped
//! in `<user_query>` alongside injected `<user_info>` and `<system-reminder>`
//! blocks. `updates.jsonl` is what the interface displayed — Agent Client
//! Protocol `session/update` notifications, each stamped with a Unix second —
//! and its user messages are exactly what was typed. It is read for the same
//! reason Codex's `event_msg` channel is: it is the less noisy of the two, and
//! the only one with time.
//!
//! # Message chunks
//!
//! ACP streams a message as `*_message_chunk` updates, so consecutive chunks of
//! one kind are concatenated into one turn. Grok currently writes each message
//! as a single chunk, which makes the join a no-op today and correct if that
//! changes. A run of chunks still open at the end of the file may be a message
//! the harness is in the middle of writing: it is counted as `pending` and
//! captured whole by a later run, rather than stored truncated under an id
//! that would then never be revisited.
//!
//! # Identity and time
//!
//! Records carry no id, so a turn is identified by its first line and a digest
//! of that line's text ([`positional_id`]). Timestamps are whole seconds, and
//! several updates often share one, so each gets an ordering-only millisecond
//! fraction ([`ordered_in_second`]).

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    is_path_safe_id, normalize_text, ordered_in_second, path_string, percent_decode, positional_id,
    redact, sort_newest_first, FilterStats, ReadOptions, ReadResult, SessionRef, TranscriptError,
    Turn, TurnRole,
};
use crate::harness::{self, HarnessSpec};

/// The display-channel log inside each session directory.
const UPDATES: &str = "updates.jsonl";

/// The JSON-RPC methods `updates.jsonl` carries. Conversation arrives as the
/// standard ACP `session/update`; Grok's own extensions (hooks, recaps, turn
/// accounting) use the vendor-prefixed form. Both wrap the same `update`.
const METHODS: &[&str] = &["session/update", "_x.ai/session/update"];

/// Update kinds that are harness machinery rather than conversation.
const NON_MESSAGE: &[&str] = &[
    "hook_execution",
    "turn_completed",
    "session_recap",
    "background_tasks",
    "memory_dream_queued",
    "memory_dream_started",
    "memory_dream_completed",
];

/// Lists the sessions Grok recorded for `cwd`, newest first.
///
/// # Errors
///
/// [`TranscriptError::NoHome`] when `$HOME` is unset, or an I/O error when the
/// sessions directory exists but cannot be listed. No directory is an empty
/// list.
pub fn sessions(spec: &HarnessSpec, cwd: &Path) -> Result<Vec<SessionRef>, TranscriptError> {
    let base = harness::sessions_dir(spec).ok_or(TranscriptError::NoHome)?;
    let want = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut found = Vec::new();
    for project in read_dir_or_empty(&base)? {
        let Some(decoded) = project
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(percent_decode)
        else {
            continue;
        };
        let dir = PathBuf::from(&decoded);
        if !dir.is_absolute() || dir.canonicalize().unwrap_or_else(|_| dir.clone()) != want {
            continue;
        }
        for session in read_dir_or_empty(&project)? {
            if let Some(entry) = session_ref(spec, &session, Some(decoded.clone()))? {
                found.push(entry);
            }
        }
    }
    sort_newest_first(&mut found);
    Ok(found.into_iter().map(|(session, _)| session).collect())
}

/// Finds session `id` under any working directory.
///
/// # Errors
///
/// [`TranscriptError::NoHome`] or an I/O error; an id that is not a single
/// safe path component is simply not found.
pub fn find_by_id(spec: &HarnessSpec, id: &str) -> Result<Option<SessionRef>, TranscriptError> {
    if !is_path_safe_id(id) {
        return Ok(None);
    }
    let base = harness::sessions_dir(spec).ok_or(TranscriptError::NoHome)?;
    for project in read_dir_or_empty(&base)? {
        let cwd = project
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(percent_decode);
        if let Some((session, _)) = session_ref(spec, &project.join(id), cwd)? {
            return Ok(Some(session));
        }
    }
    Ok(None)
}

/// Directory entries that are themselves directories; a missing directory is
/// empty rather than an error.
fn read_dir_or_empty(dir: &Path) -> Result<Vec<PathBuf>, TranscriptError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            out.push(entry.path());
        }
    }
    Ok(out)
}

/// A session directory with an `updates.jsonl`, or `None` when it has none.
fn session_ref(
    spec: &HarnessSpec,
    dir: &Path,
    cwd: Option<String>,
) -> Result<Option<(SessionRef, std::time::SystemTime)>, TranscriptError> {
    let path = dir.join(UPDATES);
    let meta = match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let Some(id) = dir.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    Ok(Some((
        SessionRef {
            harness: spec.id,
            session_id: id.to_string(),
            path: path_string(&path),
            cwd,
            bytes: meta.len(),
        },
        meta.modified().unwrap_or(std::time::UNIX_EPOCH),
    )))
}

/// Which side of a chunk run, and whether it is speech or thinking.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChunkKind {
    User,
    Agent,
    Thought,
}

/// Consecutive chunks of one kind, joined as they arrive.
struct Run {
    kind: ChunkKind,
    line: usize,
    first_text: String,
    text: String,
    created_at: String,
}

/// Streams one session's `updates.jsonl` into turns.
///
/// # Errors
///
/// [`TranscriptError::TooLarge`] before reading anything, an I/O error, or
/// [`TranscriptError::BadTimestamp`] for a message whose `timestamp` is not a
/// valid Unix second.
pub fn read(path: &Path, opts: &ReadOptions) -> Result<ReadResult, TranscriptError> {
    let bytes = std::fs::metadata(path)?.len();
    if bytes > opts.max_bytes {
        return Err(TranscriptError::TooLarge {
            bytes,
            max_bytes: opts.max_bytes,
        });
    }

    let mut stats = FilterStats::default();
    let mut redactions = redact::Redactions::default();
    let mut turns = Vec::new();
    let mut seconds = HashMap::new();
    let mut run: Option<Run> = None;

    for (line_index, line) in std::io::BufReader::new(std::fs::File::open(path)?)
        .lines()
        .enumerate()
    {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            stats.torn_line += 1;
            continue;
        };
        let update = &record["params"]["update"];
        let kind = update["sessionUpdate"].as_str().unwrap_or("");
        let chunk = match kind {
            "user_message_chunk" => Some(ChunkKind::User),
            "agent_message_chunk" => Some(ChunkKind::Agent),
            "agent_thought_chunk" => Some(ChunkKind::Thought),
            _ => None,
        };

        // Any record other than a continuation of the open run closes it.
        if run.as_ref().is_some_and(|r| Some(r.kind) != chunk) {
            if let Some(done) = run.take() {
                finish(done, opts, &mut stats, &mut redactions, &mut turns);
            }
        }

        if !record["method"]
            .as_str()
            .is_some_and(|method| METHODS.contains(&method))
        {
            stats.unknown_record += 1;
            continue;
        }
        // Every update takes its place in the second, kept or not, so where a
        // turn lands never depends on which --include flags were given.
        let created_at = timestamp(&record, line_index, &mut seconds)?;
        let Some(chunk) = chunk else {
            match kind {
                "tool_call" => {
                    stats.tool_use += 1;
                    if opts.include_tools {
                        let name = update["_meta"]["x.ai/tool"]["name"]
                            .as_str()
                            .or_else(|| update["title"].as_str())
                            .unwrap_or("unknown");
                        push(
                            TurnRole::Assistant,
                            format!("[tool_use: {name}]"),
                            positional_id(line_index, name),
                            created_at,
                            opts,
                            &mut stats,
                            &mut redactions,
                            &mut turns,
                        );
                    }
                }
                // A call is updated twice: once to describe it, once with its
                // result. Only the result counts, so a call is one tool_result.
                "tool_call_update" if update["status"].is_string() => {
                    stats.tool_result += 1;
                    if opts.include_tools {
                        let size = update["rawOutput"].to_string().len()
                            + update["content"].to_string().len();
                        let text = format!("[tool_result: {size} bytes]");
                        let id = positional_id(line_index, &text);
                        push(
                            TurnRole::Assistant,
                            text,
                            id,
                            created_at,
                            opts,
                            &mut stats,
                            &mut redactions,
                            &mut turns,
                        );
                    }
                }
                "tool_call_update" => stats.non_message += 1,
                other if NON_MESSAGE.contains(&other) => stats.non_message += 1,
                _ => stats.unknown_record += 1,
            }
            continue;
        };

        // A non-text block (an image, a resource link) is not speech engram
        // can store; it is counted, and does not open or extend a run.
        let Some(text) = update["content"]["text"]
            .as_str()
            .filter(|_| update["content"]["type"] == "text")
        else {
            stats.non_message += 1;
            continue;
        };
        match run.as_mut() {
            Some(open) => open.text.push_str(text),
            None => {
                run = Some(Run {
                    kind: chunk,
                    line: line_index,
                    first_text: text.to_string(),
                    text: text.to_string(),
                    created_at,
                });
            }
        }
    }

    // A run still open at end of file may be mid-write; a later read captures
    // it whole.
    if run.is_some() {
        stats.pending += 1;
    }

    Ok(ReadResult {
        turns,
        filtered: stats,
        redactions,
    })
}

/// The record's Unix second as an ordered ISO 8601 instant.
fn timestamp(
    record: &Value,
    line: usize,
    seconds: &mut HashMap<i64, i64>,
) -> Result<String, TranscriptError> {
    record["timestamp"]
        .as_i64()
        .and_then(|second| ordered_in_second(seconds, second))
        .ok_or_else(|| TranscriptError::BadTimestamp {
            record: format!("line {}", line + 1),
            value: record["timestamp"].to_string(),
        })
}

/// Turns a closed chunk run into a turn, or counts why it is not one.
fn finish(
    run: Run,
    opts: &ReadOptions,
    stats: &mut FilterStats,
    redactions: &mut redact::Redactions,
    turns: &mut Vec<Turn>,
) {
    let role = match run.kind {
        ChunkKind::User => TurnRole::User,
        ChunkKind::Agent => TurnRole::Assistant,
        ChunkKind::Thought => {
            stats.thinking += 1;
            if !opts.include_thinking {
                return;
            }
            TurnRole::Assistant
        }
    };
    let id = positional_id(run.line, &run.first_text);
    push(
        role,
        run.text,
        id,
        run.created_at,
        opts,
        stats,
        redactions,
        turns,
    );
}

/// Normalizes and redacts `text` and appends it as a turn, or counts it empty.
#[expect(
    clippy::too_many_arguments,
    reason = "one call site per record kind; a struct would only rename the arguments"
)]
fn push(
    role: TurnRole,
    text: String,
    source_uuid: String,
    created_at: String,
    opts: &ReadOptions,
    stats: &mut FilterStats,
    redactions: &mut redact::Redactions,
    turns: &mut Vec<Turn>,
) {
    let Some(text) = normalize_text(&text, opts.max_chars_per_turn, stats) else {
        stats.empty += 1;
        return;
    };
    turns.push(Turn {
        source_uuid,
        role,
        text: redact::scrub(&text, redactions),
        created_at,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Mirrors the real split: conversation on the standard ACP method,
    /// Grok's machinery on its vendor-prefixed one.
    fn update(second: i64, update: Value) -> String {
        let method = if NON_MESSAGE.contains(&update["sessionUpdate"].as_str().unwrap_or("")) {
            "_x.ai/session/update"
        } else {
            "session/update"
        };
        json!({"timestamp": second, "method": method,
            "params": {"sessionId": "s1", "update": update}})
        .to_string()
    }

    fn chunk(second: i64, kind: &str, text: &str) -> String {
        update(
            second,
            json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}),
        )
    }

    fn read_lines(lines: &[String], opts: ReadOptions) -> ReadResult {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(UPDATES);
        std::fs::write(&path, lines.join("\n") + "\n").expect("write");
        read(&path, &opts).expect("read")
    }

    fn done(second: i64) -> String {
        update(
            second,
            json!({"sessionUpdate": "turn_completed", "stop_reason": "end_turn"}),
        )
    }

    #[test]
    fn user_and_agent_chunks_become_ordered_turns() {
        let result = read_lines(
            &[
                chunk(1_793_114_986, "user_message_chunk", "What versions exist?"),
                chunk(1_793_114_986, "agent_thought_chunk", "private"),
                chunk(1_793_114_986, "agent_message_chunk", "Three."),
                done(1_793_114_990),
            ],
            ReadOptions::default(),
        );
        let texts: Vec<_> = result.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["What versions exist?", "Three."]);
        assert_eq!(result.turns[0].role, TurnRole::User);
        assert_eq!(result.turns[0].created_at, "2026-10-27T15:29:46.000Z");
        // The filtered thought still took .001, so the reply's place in the
        // second does not depend on --include-thinking.
        assert_eq!(result.turns[1].created_at, "2026-10-27T15:29:46.002Z");
        assert_eq!(result.filtered.thinking, 1);
        assert_eq!(result.filtered.non_message, 1);
        assert_eq!(result.filtered.pending, 0);
    }

    /// ACP streams a message as chunks; consecutive ones are one turn.
    #[test]
    fn consecutive_chunks_join_and_a_trailing_run_is_pending() {
        let result = read_lines(
            &[
                chunk(10, "agent_message_chunk", "Hel"),
                chunk(10, "agent_message_chunk", "lo"),
                done(11),
                chunk(12, "agent_message_chunk", "still writ"),
            ],
            ReadOptions::default(),
        );
        assert_eq!(result.turns.len(), 1);
        assert_eq!(result.turns[0].text, "Hello");
        assert_eq!(result.filtered.pending, 1);
    }

    #[test]
    fn identity_survives_the_run_growing() {
        let short = read_lines(
            &[chunk(10, "agent_message_chunk", "Hel"), done(11)],
            ReadOptions::default(),
        );
        let long = read_lines(
            &[
                chunk(10, "agent_message_chunk", "Hel"),
                chunk(10, "agent_message_chunk", "lo"),
                done(11),
            ],
            ReadOptions::default(),
        );
        assert_eq!(short.turns[0].source_uuid, long.turns[0].source_uuid);
    }

    #[test]
    fn tools_and_thinking_only_on_request_and_never_payloads() {
        let lines = [
            update(
                20,
                json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "read_file",
                    "rawInput": {"target_file": "/secret/path"},
                    "_meta": {"x.ai/tool": {"name": "read_file"}}}),
            ),
            update(
                20,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "kind": "read",
                    "title": "Read `/secret/path`"}),
            ),
            update(
                21,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1",
                    "status": "completed", "rawOutput": "never store this payload"}),
            ),
            chunk(22, "agent_thought_chunk", "reasoning"),
            done(23),
        ];
        let default = read_lines(&lines, ReadOptions::default());
        assert!(default.turns.is_empty());
        assert_eq!(default.filtered.tool_use, 1);
        assert_eq!(default.filtered.tool_result, 1);
        assert_eq!(default.filtered.non_message, 2);

        let included = read_lines(
            &lines,
            ReadOptions {
                include_thinking: true,
                include_tools: true,
                ..ReadOptions::default()
            },
        );
        let texts: Vec<_> = included.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts[0], "[tool_use: read_file]");
        assert!(texts[1].starts_with("[tool_result: ") && texts[1].ends_with(" bytes]"));
        assert_eq!(texts[2], "reasoning");
        for text in texts {
            assert!(!text.contains("never store") && !text.contains("/secret"));
        }
    }

    #[test]
    fn unknown_kinds_torn_lines_and_bad_timestamps() {
        let result = read_lines(
            &[
                update(1, json!({"sessionUpdate": "something_new"})),
                "{torn".to_string(),
                json!({"timestamp": 1, "method": "other/method", "params": {}}).to_string(),
            ],
            ReadOptions::default(),
        );
        assert_eq!(result.filtered.unknown_record, 2);
        assert_eq!(result.filtered.torn_line, 1);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(UPDATES);
        let bad = json!({"timestamp": "yesterday", "method": "session/update",
            "params": {"update": {"sessionUpdate": "user_message_chunk",
                "content": {"type": "text", "text": "hi"}}}});
        std::fs::write(&path, format!("{bad}\n")).expect("write");
        assert!(matches!(
            read(&path, &ReadOptions::default()),
            Err(TranscriptError::BadTimestamp { .. })
        ));
    }

    #[test]
    fn credentials_are_redacted() {
        let result = read_lines(
            &[
                chunk(
                    5,
                    "user_message_chunk",
                    "token ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA here",
                ),
                done(6),
            ],
            ReadOptions::default(),
        );
        assert!(!result.turns[0].text.contains("ghp_AAAA"));
    }
}
