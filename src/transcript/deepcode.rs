// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Reader for Deep Code's session logs.
//!
//! Deep Code (`@vegamo/deepcode-cli`) keeps `~/.deepcode/projects/<code>/`, one
//! directory per project, holding `sessions-index.json` and one
//! `<session-id>.jsonl` per conversation. The directory name resembles Claude
//! Code's mangling but is not reversible (long paths are shortened with a
//! hash), so it is not inverted: the index records the project's
//! `originalPath` verbatim, and listing compares that with the working
//! directory exactly.
//!
//! # The record
//!
//! Each line is one `SessionMessage`, as defined in Deep Code's
//! `packages/core/src/session.ts` (read at commit `efb0364`, 2026-10-30, because
//! the only session on the surveyed machine had failed before any reply):
//! `{id, role: system|user|assistant|tool, content, messageParams, visible,
//! compacted, createTime, meta?}`. Every record carries a stable `id`, which is
//! what makes re-ingest idempotent even though Deep Code rewrites the whole
//! file through a temporary file on compaction and undo.
//!
//! - `user` — the typed text is `meta.userPrompt.text` when present, else
//!   `content`. Invisible messages, tool follow-ups and skill loads are harness
//!   injections (`meta`); compaction summaries are `non_message`.
//! - `assistant` — `content` is the reply. `messageParams.reasoning_content`
//!   is thinking, and each `messageParams.tool_calls` entry a tool use.
//! - `tool` — a tool result; only its size is ever stored.
//! - `system` — the system prompt and harness notices: `non_message`.
//!
//! Compacted messages are kept: compaction hides them from the model, not from
//! the history the user saw.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    is_path_safe_id, iso_millis, normalize_text, path_string, redact, sort_newest_first,
    FilterStats, ReadOptions, ReadResult, SessionRef, TranscriptError, Turn, TurnRole,
};
use crate::harness::{self, HarnessSpec};

/// Per-project index naming the project's directory.
const INDEX: &str = "sessions-index.json";

/// Lists the sessions Deep Code recorded for `cwd`, newest first.
///
/// # Errors
///
/// [`TranscriptError::NoHome`] when `$HOME` is unset, or an I/O error when the
/// projects directory exists but cannot be listed.
pub fn sessions(spec: &HarnessSpec, cwd: &Path) -> Result<Vec<SessionRef>, TranscriptError> {
    let base = harness::sessions_dir(spec).ok_or(TranscriptError::NoHome)?;
    let want = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut found = Vec::new();
    for project in subdirs(&base)? {
        let Some(original) = original_path(&project) else {
            continue;
        };
        let candidate = PathBuf::from(&original);
        if candidate.canonicalize().unwrap_or(candidate) != want {
            continue;
        }
        for entry in std::fs::read_dir(&project)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                if let Some(session) = session_ref(spec, &path, Some(original.clone()))? {
                    found.push(session);
                }
            }
        }
    }
    sort_newest_first(&mut found);
    Ok(found.into_iter().map(|(session, _)| session).collect())
}

/// Finds session `id` in any project.
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
    for project in subdirs(&base)? {
        let path = project.join(format!("{id}.jsonl"));
        if let Some((session, _)) = session_ref(spec, &path, original_path(&project))? {
            return Ok(Some(session));
        }
    }
    Ok(None)
}

/// Subdirectories of `dir`; a missing directory has none.
fn subdirs(dir: &Path) -> Result<Vec<PathBuf>, TranscriptError> {
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

/// The project's `originalPath`, when its index is readable and records one.
fn original_path(project: &Path) -> Option<String> {
    let index = std::fs::read_to_string(project.join(INDEX)).ok()?;
    serde_json::from_str::<Value>(&index).ok()?["originalPath"]
        .as_str()
        .map(str::to_owned)
}

/// A session file, or `None` when it does not exist.
fn session_ref(
    spec: &HarnessSpec,
    path: &Path,
    cwd: Option<String>,
) -> Result<Option<(SessionRef, std::time::SystemTime)>, TranscriptError> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(Some((
        SessionRef {
            harness: spec.id,
            session_id: super::session_id_from_path(path),
            path: path_string(path),
            cwd,
            bytes: meta.len(),
        },
        meta.modified().unwrap_or(std::time::UNIX_EPOCH),
    )))
}

/// Reads one Deep Code session into turns.
///
/// # Errors
///
/// [`TranscriptError::TooLarge`] before reading anything, an I/O error, or
/// [`TranscriptError::BadTimestamp`] for a message whose `createTime` is not a
/// valid instant.
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

    for line in std::io::BufReader::new(std::fs::File::open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            stats.torn_line += 1;
            continue;
        };
        let meta = &message["meta"];
        let (role, text) = match message["role"].as_str().unwrap_or("") {
            "system" => {
                stats.non_message += 1;
                continue;
            }
            "user" => {
                if meta["isSummary"] == true {
                    stats.non_message += 1;
                    continue;
                }
                if message["visible"] == false
                    || meta["isToolFollowUp"] == true
                    || meta["skill"].is_object()
                {
                    stats.meta += 1;
                    continue;
                }
                let text = meta["userPrompt"]["text"]
                    .as_str()
                    .or_else(|| message["content"].as_str())
                    .unwrap_or("");
                (TurnRole::User, text.to_string())
            }
            "assistant" => {
                let params = &message["messageParams"];
                let mut parts = Vec::new();
                if let Some(thinking) = params["reasoning_content"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                {
                    stats.thinking += 1;
                    if opts.include_thinking {
                        parts.push(thinking.to_string());
                    }
                }
                if let Some(content) = message["content"].as_str() {
                    parts.push(content.to_string());
                }
                for call in params["tool_calls"].as_array().into_iter().flatten() {
                    stats.tool_use += 1;
                    if opts.include_tools {
                        let name = call["function"]["name"]
                            .as_str()
                            .or_else(|| call["name"].as_str())
                            .unwrap_or("unknown");
                        parts.push(format!("[tool_use: {name}]"));
                    }
                }
                (TurnRole::Assistant, parts.join("\n"))
            }
            "tool" => {
                stats.tool_result += 1;
                if !opts.include_tools {
                    continue;
                }
                let size = message["content"].as_str().map_or(0, str::len);
                (TurnRole::Assistant, format!("[tool_result: {size} bytes]"))
            }
            _ => {
                stats.unknown_record += 1;
                continue;
            }
        };

        let Some(id) = message["id"].as_str().filter(|id| !id.is_empty()) else {
            stats.missing_uuid += 1;
            continue;
        };
        let raw_time = message["createTime"].as_str().unwrap_or("");
        let created_at = raw_time
            .parse::<jiff::Timestamp>()
            .map(iso_millis)
            .map_err(|_| TranscriptError::BadTimestamp {
                record: id.to_string(),
                value: raw_time.to_string(),
            })?;
        let Some(text) = normalize_text(&text, opts.max_chars_per_turn, &mut stats) else {
            stats.empty += 1;
            continue;
        };
        turns.push(Turn {
            source_uuid: id.to_string(),
            role,
            text: redact::scrub(&text, &mut redactions),
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
    use serde_json::json;

    fn message(id: &str, role: &str, content: &str, extra: Value) -> String {
        let mut record = json!({"id": id, "sessionId": "s1", "role": role, "content": content,
            "contentParams": null, "messageParams": null, "compacted": false, "visible": true,
            "createTime": "2026-06-14T23:23:43.001Z", "updateTime": "2026-06-14T23:23:43.001Z"});
        if let (Some(target), Some(source)) = (record.as_object_mut(), extra.as_object()) {
            target.extend(source.clone());
        }
        record.to_string()
    }

    fn read_lines(lines: &[String], opts: ReadOptions) -> ReadResult {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s1.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").expect("write");
        read(&path, &opts).expect("read")
    }

    #[test]
    fn a_conversation_reads_as_user_and_assistant_turns() {
        let result = read_lines(
            &[
                message("m0", "system", "You are Deep Code", json!({})),
                message(
                    "m1",
                    "user",
                    "hello\n<injected/>",
                    json!({"meta": {"userPrompt": {"text": "hello", "imageUrls": []}}}),
                ),
                message(
                    "m2",
                    "assistant",
                    "Hi there.",
                    json!({"messageParams": {"reasoning_content": "private"}}),
                ),
                message(
                    "m3",
                    "user",
                    "summary",
                    json!({"meta": {"isSummary": true}}),
                ),
                message("m4", "user", "hidden", json!({"visible": false})),
                message("m5", "user", "earlier", json!({"compacted": true})),
            ],
            ReadOptions::default(),
        );
        let texts: Vec<_> = result.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["hello", "Hi there.", "earlier"]);
        assert_eq!(result.turns[0].source_uuid, "m1");
        assert_eq!(result.turns[0].created_at, "2026-06-14T23:23:43.001Z");
        assert_eq!(result.filtered.thinking, 1);
        assert_eq!(result.filtered.non_message, 2);
        assert_eq!(result.filtered.meta, 1);
    }

    #[test]
    fn tools_and_thinking_only_on_request_and_never_payloads() {
        let lines = [
            message(
                "a1",
                "assistant",
                "Reading it.",
                json!({"messageParams": {"reasoning_content": "think",
                    "tool_calls": [{"id": "c1", "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\":\"/secret\"}"}}]}}),
            ),
            message(
                "t1",
                "tool",
                "never store this payload",
                json!({"messageParams": {"tool_call_id": "c1"}}),
            ),
        ];
        let default = read_lines(&lines, ReadOptions::default());
        assert_eq!(default.turns.len(), 1);
        assert_eq!(default.turns[0].text, "Reading it.");
        assert_eq!(default.filtered.tool_use, 1);
        assert_eq!(default.filtered.tool_result, 1);

        let included = read_lines(
            &lines,
            ReadOptions {
                include_thinking: true,
                include_tools: true,
                ..ReadOptions::default()
            },
        );
        assert_eq!(
            included.turns[0].text,
            "think\nReading it.\n[tool_use: read]"
        );
        assert_eq!(included.turns[1].text, "[tool_result: 24 bytes]");
        for turn in &included.turns {
            assert!(!turn.text.contains("never store") && !turn.text.contains("/secret"));
        }
    }

    #[test]
    fn missing_ids_unknown_roles_torn_lines_and_bad_times() {
        let mut no_id: Value =
            serde_json::from_str(&message("x", "user", "hi", json!({}))).expect("json");
        no_id["id"] = Value::Null;
        let result = read_lines(
            &[
                no_id.to_string(),
                message("r1", "reviewer", "new role", json!({})),
                "{torn".to_string(),
            ],
            ReadOptions::default(),
        );
        assert_eq!(result.filtered.missing_uuid, 1);
        assert_eq!(result.filtered.unknown_record, 1);
        assert_eq!(result.filtered.torn_line, 1);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s1.jsonl");
        let bad = message("b1", "user", "hi", json!({"createTime": "not a time"}));
        std::fs::write(&path, bad + "\n").expect("write");
        assert!(matches!(
            read(&path, &ReadOptions::default()),
            Err(TranscriptError::BadTimestamp { .. })
        ));
    }
}
