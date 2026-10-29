// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Reader for Antigravity's brain JSONL transcripts.
//!
//! CLI, legacy and IDE brains each hold `<id>/.system_generated/logs/transcript.jsonl`.
//! Only those three roots are scanned; backup siblings and protobuf stores are
//! deliberately outside this format. Workspace attribution comes from each
//! root's sibling `conversation_summaries.db`, opened read-only with a short
//! busy timeout because the harness may still be running. Missing attribution
//! never guesses a project: exact canonical cwd matches list, while by-id
//! lookup can reach unattributed IDE conversations without a summaries DB.
//!
//! Listing has no read options, so nested sessions are listed and filtered at
//! read time. Only USER_REQUEST bodies and planner prose are conversation;
//! metadata is discarded, thinking is opt-in, and tools expose names or byte
//! counts only, never arguments or payloads. Every retained turn uses the shared
//! normalization and credential scrubber, and every dropped record is counted.
//!
//! IDE logs can replay records and restart step indices. Identity therefore
//! hashes canonical source fields, excluding status and position. Replays count
//! as `non_message`; RUNNING records count as `pending` without preventing a
//! later DONE version from being retained. Streaming and a file-size ceiling
//! bound the input, and invalid timestamps are errors rather than invented time.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use rusqlite::OpenFlags;
use serde_json::Value;

use super::{
    normalize_text, path_string, redact, sort_newest_first, FilterStats, ReadOptions, ReadResult,
    SessionRef, TranscriptError, Turn, TurnRole,
};
use crate::harness::{self, HarnessSpec};

const ROOTS: &[&str] = &[
    ".gemini/antigravity-cli/brain",
    ".gemini/antigravity/brain",
    ".gemini/antigravity-ide/brain",
];
const TRANSCRIPT: &str = ".system_generated/logs/transcript.jsonl";
const TOOLS: &[&str] = &[
    "VIEW_FILE",
    "RUN_COMMAND",
    "GREP_SEARCH",
    "LIST_DIRECTORY",
    "CODE_ACTION",
    "GENERIC",
    "READ_URL_CONTENT",
    "SEARCH_WEB",
    "MCP_TOOL",
    "INVOKE_SUBAGENT",
    "ASK_QUESTION",
    "ERROR_MESSAGE",
];
const NON_MESSAGES: &[&str] = &[
    "SYSTEM_MESSAGE",
    "EPHEMERAL_MESSAGE",
    "CONVERSATION_HISTORY",
    "CHECKPOINT",
    "KNOWLEDGE_ARTIFACTS",
];

struct Summary {
    workspaces: Vec<PathBuf>,
    nested: bool,
}

fn roots() -> Result<Vec<PathBuf>, TranscriptError> {
    let home = harness::home_dir().ok_or(TranscriptError::NoHome)?;
    Ok(ROOTS.iter().map(|root| home.join(root)).collect())
}

/// Decodes local file URIs; malformed escapes and nonlocal paths give no attribution.
fn workspace(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let mut bytes = Vec::new();
    let mut iter = encoded.bytes();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let high = char::from(iter.next()?).to_digit(16)?;
            let low = char::from(iter.next()?).to_digit(16)?;
            bytes.push((high * 16 + low) as u8);
        } else {
            bytes.push(byte);
        }
    }
    let path = PathBuf::from(String::from_utf8(bytes).ok()?);
    if !path.is_absolute() {
        return None;
    }
    if path.extension().is_some_and(|ext| ext == "code-workspace") {
        Some(path.parent()?.to_path_buf())
    } else {
        Some(path)
    }
}

/// Missing databases mean unattributed sessions; present but broken ones are errors.
fn summaries(root: &Path) -> Result<HashMap<String, Summary>, TranscriptError> {
    let path = root.with_file_name("conversation_summaries.db");
    let fail =
        |e: String| TranscriptError::Io(std::io::Error::other(format!("{}: {e}", path.display())));
    if !path.try_exists().map_err(|e| fail(e.to_string()))? {
        return Ok(HashMap::new());
    }
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| fail(e.to_string()))?;
    // Two seconds tolerates brief WAL contention without hanging session discovery.
    conn.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|e| fail(e.to_string()))?;
    let mut stmt = conn.prepare(
        "SELECT conversation_id, workspace_uris, parent_conversation_id, nesting_depth FROM conversation_summaries",
    ).map_err(|e| fail(e.to_string()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|e| fail(e.to_string()))?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, uris, parent, depth) = row.map_err(|e| fail(e.to_string()))?;
        let workspaces = serde_json::from_str::<Vec<String>>(&uris)
            .unwrap_or_default()
            .iter()
            .filter_map(|uri| workspace(uri))
            .collect();
        out.insert(
            id,
            Summary {
                workspaces,
                nested: !parent.is_empty() || depth > 0,
            },
        );
    }
    Ok(out)
}

/// Lists attributed sessions for an exact canonical cwd, newest file first.
///
/// # Errors
/// Returns home, filesystem or summaries database errors, naming a failed DB.
pub fn sessions(spec: &HarnessSpec, cwd: &Path) -> Result<Vec<SessionRef>, TranscriptError> {
    let want = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut found = Vec::new();
    for root in roots()? {
        let summaries = summaries(&root)?;
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let Some(summary) = summaries.get(&id) else {
                continue;
            };
            let Some(matched) = summary
                .workspaces
                .iter()
                .find(|dir| dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()) == want)
            else {
                continue;
            };
            let path = entry.path().join(TRANSCRIPT);
            let meta = match std::fs::metadata(&path) {
                Ok(meta) if meta.is_file() => meta,
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            found.push((
                SessionRef {
                    harness: spec.id,
                    session_id: id,
                    path: path_string(&path),
                    cwd: Some(path_string(matched)),
                    bytes: meta.len(),
                },
                meta.modified().unwrap_or(std::time::UNIX_EPOCH),
            ));
        }
    }
    sort_newest_first(&mut found);
    Ok(found.into_iter().map(|(session, _)| session).collect())
}

/// Finds an explicit id even when its workspace attribution is unavailable.
///
/// # Errors
/// Returns home or filesystem errors; rejects path syntax by returning no match.
pub fn find_by_id(spec: &HarnessSpec, id: &str) -> Result<Option<SessionRef>, TranscriptError> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Ok(None);
    }
    for root in roots()? {
        let path = root.join(id).join(TRANSCRIPT);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {
                return Ok(Some(SessionRef {
                    harness: spec.id,
                    session_id: id.to_string(),
                    path: path_string(&path),
                    cwd: None,
                    bytes: meta.len(),
                }))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

/// Serializes recursively sorted object keys, preserving array order and values.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!(
                        "{}:{}",
                        Value::String(key.clone()),
                        canonical_json(&map[key])
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => value.to_string(),
    }
}

fn fingerprint(record: &Value) -> String {
    let fields = [
        "type",
        "source",
        "step_index",
        "created_at",
        "content",
        "thinking",
        "tool_calls",
    ];
    let value = Value::Array(fields.iter().map(|key| record[*key].clone()).collect());
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        canonical_json(&value).as_bytes(),
    )
    .simple()
    .to_string()
}

/// Adds ordering-only milliseconds, not a claim of harness clock precision.
///
/// Recall orders by (created_at, id), and id is a UUID: source seconds alone
/// would reorder same-second turns randomly. All distinct parsed fingerprints
/// consume a position, including filtered records; exact replays consume none.
fn ordered_timestamp(
    record: &Value,
    id: &str,
    seconds: &mut HashMap<i64, usize>,
) -> Result<String, TranscriptError> {
    let raw = record["created_at"].as_str().unwrap_or("");
    let ts = raw
        .parse::<jiff::Timestamp>()
        .map_err(|_| TranscriptError::BadTimestamp {
            record: id.to_string(),
            value: raw.to_string(),
        })?;
    let index = seconds.entry(ts.as_second()).or_default();
    let millis = (*index).min(999);
    *index = index.saturating_add(1);
    let second = jiff::Timestamp::from_second(ts.as_second())
        .map_err(|_| TranscriptError::BadTimestamp {
            record: id.to_string(),
            value: raw.to_string(),
        })?
        .to_string();
    Ok(format!("{}.{millis:03}Z", second.trim_end_matches('Z')))
}

/// Streams a session through filtering, normalization, truncation and redaction.
///
/// # Errors
/// Returns file/DB errors, TooLarge before reading, or BadTimestamp for any
/// parsed record without a valid source time, even when that record is filtered.
pub fn read(
    path: &Path,
    session_id: &str,
    opts: &ReadOptions,
) -> Result<ReadResult, TranscriptError> {
    let bytes = std::fs::metadata(path)?.len();
    if bytes > opts.max_bytes {
        return Err(TranscriptError::TooLarge {
            bytes,
            max_bytes: opts.max_bytes,
        });
    }
    let nested = if let Some(root) = path.ancestors().nth(4) {
        summaries(root)?
            .get(session_id)
            .is_some_and(|summary| summary.nested)
    } else {
        false
    };
    let mut stats = FilterStats::default();
    let mut redactions = redact::Redactions::default();
    let mut turns = Vec::new();
    let mut timestamps = HashMap::new();
    let mut seconds = HashMap::new();
    let mut finished = HashSet::new();
    for line in std::io::BufReader::new(std::fs::File::open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            stats.torn_line += 1;
            continue;
        };
        let id = fingerprint(&record);
        let created_at = match timestamps.get(&id) {
            Some(time) => time,
            None => {
                let time = ordered_timestamp(&record, &id, &mut seconds)?;
                timestamps.entry(id.clone()).or_insert(time)
            }
        }
        .clone();
        if record["status"] == "RUNNING" {
            stats.pending += 1;
            continue;
        }
        if !finished.insert(id.clone()) {
            stats.non_message += 1;
            continue;
        }
        let kind = record["type"].as_str().unwrap_or("");
        let conversation =
            kind == "USER_INPUT" || kind == "PLANNER_RESPONSE" || TOOLS.contains(&kind);
        if conversation && nested && !opts.include_sidechains {
            stats.sidechain += 1;
            continue;
        }
        let content = record["content"].as_str().unwrap_or("");
        let mut fields = Vec::new();
        let (role, raw) = match (kind, record["status"].as_str().unwrap_or("")) {
            ("USER_INPUT", "DONE") => {
                let Some((_, rest)) = content.split_once("<USER_REQUEST>") else {
                    stats.unknown_record += 1;
                    continue;
                };
                let Some((body, _)) = rest.split_once("</USER_REQUEST>") else {
                    stats.unknown_record += 1;
                    continue;
                };
                fields.push("content");
                (TurnRole::User, body.trim().to_string())
            }
            ("PLANNER_RESPONSE", "DONE") => {
                let mut parts = Vec::new();
                if let Some(thinking) = record["thinking"].as_str().filter(|s| !s.is_empty()) {
                    stats.thinking += 1;
                    if opts.include_thinking {
                        parts.push(thinking.to_string());
                        fields.push("thinking");
                    }
                }
                parts.push(content.to_string());
                fields.push("content");
                if let Some(calls) = record["tool_calls"].as_array() {
                    for call in calls {
                        stats.tool_use += 1;
                        if opts.include_tools {
                            let name = call["name"]
                                .as_str()
                                .or_else(|| call["function"]["name"].as_str())
                                .unwrap_or("unknown");
                            parts.push(format!("[tool_use: {name}]"));
                        }
                    }
                }
                (TurnRole::Assistant, parts.join("\n"))
            }
            (kind, "DONE" | "ERROR") if TOOLS.contains(&kind) => {
                stats.tool_result += 1;
                if !opts.include_tools {
                    continue;
                }
                (
                    TurnRole::Assistant,
                    format!("[{kind}: {} bytes]", content.len()),
                )
            }
            (kind, _) if NON_MESSAGES.contains(&kind) => {
                stats.non_message += 1;
                continue;
            }
            _ => {
                stats.unknown_record += 1;
                continue;
            }
        };
        let Some(text) = normalize_text(&raw, opts.max_chars_per_turn, &mut stats) else {
            stats.empty += 1;
            continue;
        };
        let text = redact::scrub(&text, &mut redactions);
        if text.trim().is_empty() {
            stats.empty += 1;
            continue;
        }
        // Source truncation counts only fields retained as prose, never tool payloads.
        if record["truncated_fields"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item.as_str().is_some_and(|field| fields.contains(&field)))
        }) {
            stats.truncated += 1;
        }
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
    use serde_json::json;

    fn record(kind: &str, step: i64, content: &str) -> Value {
        json!({"type": kind, "source": "synthetic", "step_index": step,
            "status": "DONE", "created_at": "2026-10-01T12:00:00Z", "content": content})
    }

    fn read_records(records: &[Value], opts: ReadOptions) -> ReadResult {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("transcript.jsonl");
        let text = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, text).expect("write");
        read(&path, "synthetic", &opts).expect("read")
    }

    #[test]
    fn replay_dedupe_step_restart_and_same_second_order() {
        let a = record("USER_INPUT", 8, "<USER_REQUEST>first</USER_REQUEST>");
        let b = record("PLANNER_RESPONSE", 9, "second");
        let c = record("USER_INPUT", 0, "<USER_REQUEST>third</USER_REQUEST>");
        let result = read_records(&[a.clone(), b.clone(), a, b, c], ReadOptions::default());
        assert_eq!(result.filtered.non_message, 2);
        assert_eq!(
            result
                .turns
                .iter()
                .map(|t| t.text.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        for (index, turn) in result.turns.iter().enumerate() {
            assert_eq!(turn.created_at, format!("2026-10-01T12:00:00.{index:03}Z"));
        }
        assert_ne!(result.turns[0].source_uuid, result.turns[2].source_uuid);
    }

    #[test]
    fn running_does_not_consume_the_done_identity_or_an_extra_timestamp() {
        let done = record("PLANNER_RESPONSE", 0, "complete");
        let mut running = done.clone();
        running["status"] = json!("RUNNING");
        assert_eq!(fingerprint(&done), fingerprint(&running));
        let result = read_records(&[running, done.clone(), done], ReadOptions::default());
        assert_eq!(result.filtered.pending, 1);
        assert_eq!(result.filtered.non_message, 1);
        assert_eq!(result.turns.len(), 1);
        assert_eq!(result.turns[0].created_at, "2026-10-01T12:00:00.000Z");
    }

    #[test]
    fn user_wrapper_is_the_only_user_text() {
        let result = read_records(&[
            record("USER_INPUT", 0, "settings<USER_REQUEST>  hello \n </USER_REQUEST><ADDITIONAL_METADATA>secret</ADDITIONAL_METADATA>"),
            record("USER_INPUT", 1, "<USER_REQUEST> \n </USER_REQUEST>"),
            record("USER_INPUT", 2, "missing wrapper"),
            record("USER_INPUT", 3, "<USER_REQUEST>unterminated"),
        ], ReadOptions::default());
        assert_eq!(result.turns.len(), 1);
        assert_eq!(result.turns[0].text, "hello");
        assert_eq!(result.filtered.empty, 1);
        assert_eq!(result.filtered.unknown_record, 2);
    }

    #[test]
    fn thinking_tools_redaction_and_retained_source_truncation() {
        let mut planner = record(
            "PLANNER_RESPONSE",
            0,
            "reply ghp_abcdefghijklmnopqrstuvwxyz1234567890",
        );
        planner["thinking"] = json!("private reasoning");
        planner["tool_calls"] =
            json!([{"name": "read", "arguments": {"secret": "never store args"}}]);
        planner["truncated_fields"] = json!(["thinking"]);
        let mut tool = record("VIEW_FILE", 1, "never store payload");
        tool["truncated_fields"] = json!(["content"]);
        let default = read_records(&[planner.clone(), tool.clone()], ReadOptions::default());
        assert_eq!(default.turns.len(), 1);
        assert!(!default.turns[0].text.contains("private reasoning"));
        assert!(!default.turns[0].text.contains("ghp_"));
        assert_eq!(default.filtered.thinking, 1);
        assert_eq!(default.filtered.tool_use, 1);
        assert_eq!(default.filtered.tool_result, 1);
        assert_eq!(default.filtered.truncated, 0);
        let included = read_records(
            &[planner, tool],
            ReadOptions {
                include_thinking: true,
                include_tools: true,
                ..ReadOptions::default()
            },
        );
        assert_eq!(included.turns.len(), 2);
        assert!(included.turns[0].text.starts_with("private reasoning\n"));
        assert!(included.turns[0].text.contains("[tool_use: read]"));
        assert_eq!(included.turns[1].text, "[VIEW_FILE: 19 bytes]");
        assert_eq!(included.filtered.truncated, 1);
        for turn in included.turns {
            assert!(!turn.text.contains("never store"));
        }
    }

    #[test]
    fn canonical_fingerprint_sorts_nested_tool_keys_and_ignores_status() {
        let a: Value = serde_json::from_str(r#"{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"x","args":{"z":1,"a":2}}],"status":"RUNNING"}"#).expect("JSON");
        let b: Value = serde_json::from_str(r#"{"status":"DONE","tool_calls":[{"args":{"a":2,"z":1},"name":"x"}],"type":"PLANNER_RESPONSE"}"#).expect("JSON");
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn timestamps_are_required_even_for_filtered_records_and_torn_lines_are_counted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("transcript.jsonl");
        for value in [json!("invalid"), Value::Null] {
            let mut bad = record("SYSTEM_MESSAGE", 0, "ignored");
            bad["created_at"] = value;
            std::fs::write(&path, bad.to_string()).expect("write");
            assert!(matches!(
                read(&path, "id", &ReadOptions::default()),
                Err(TranscriptError::BadTimestamp { .. })
            ));
        }
        std::fs::write(
            &path,
            format!("{{torn\n{}\n", record("PLANNER_RESPONSE", 1, "hello")),
        )
        .expect("write");
        let result = read(&path, "id", &ReadOptions::default()).expect("read");
        assert_eq!(result.filtered.torn_line, 1);
        assert_eq!(result.turns.len(), 1);
        assert!(matches!(
            read(
                &path,
                "id",
                &ReadOptions {
                    max_bytes: 1,
                    ..ReadOptions::default()
                }
            ),
            Err(TranscriptError::TooLarge { .. })
        ));
    }

    #[test]
    fn all_machine_types_count_and_timestamps_saturate() {
        let mut records = Vec::new();
        for (index, kind) in TOOLS.iter().chain(NON_MESSAGES).enumerate() {
            let mut r = record(kind, index as i64, "payload");
            if TOOLS.contains(kind) {
                r["status"] = json!("ERROR");
            }
            records.push(r);
        }
        records.push(record("FUTURE_TYPE", 100, "unknown"));
        let result = read_records(&records, ReadOptions::default());
        assert!(result.turns.is_empty());
        assert_eq!(result.filtered.tool_result, TOOLS.len());
        assert_eq!(result.filtered.non_message, NON_MESSAGES.len());
        assert_eq!(result.filtered.unknown_record, 1);
        let r = record("PLANNER_RESPONSE", 0, "reply");
        let mut seconds = HashMap::from([(1_759_320_000, 1001)]);
        let ts = r["created_at"]
            .as_str()
            .expect("timestamp")
            .parse::<jiff::Timestamp>()
            .expect("timestamp");
        seconds.insert(ts.as_second(), 1001);
        assert_eq!(
            ordered_timestamp(&r, "id", &mut seconds).expect("time"),
            "2026-10-01T12:00:00.999Z"
        );
    }

    /// A root the reader scans but detection does not probe is a supported
    /// layout that `ingest` without `--harness` reports as not installed.
    #[test]
    fn every_scanned_root_is_a_detection_probe() {
        let spec = harness::ALL
            .iter()
            .find(|spec| spec.id == harness::Harness::Antigravity)
            .expect("antigravity spec");
        for root in ROOTS {
            let parent = root.strip_suffix("/brain").expect("brain root");
            assert!(spec.probe.contains(&parent), "{parent} is not probed");
        }
    }

    #[test]
    fn workspace_uri_decoding_and_workspace_file_parent() {
        assert_eq!(
            workspace("file:///tmp/project%20name/test.code-workspace"),
            Some(PathBuf::from("/tmp/project name"))
        );
        assert_eq!(
            workspace("file:///tmp/%E2%98%83"),
            Some(PathBuf::from("/tmp/☃"))
        );
        for uri in [
            "",
            "https://example.org",
            "file://host/path",
            "file:///bad%",
            "file:///bad%zz",
            "file:///bad%ff",
        ] {
            assert!(workspace(uri).is_none(), "{uri}");
        }
    }
}
