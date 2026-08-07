// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Reader for Claude Code session transcripts.
//!
//! # Layout
//!
//! `~/.claude/projects/<mangled-cwd>/<session-uuid>.jsonl`, where the
//! directory name comes from [`super::mangle_cwd`]. Sibling directories named
//! for the same uuid hold subagent transcripts; those are not read here — a
//! subagent's transcript is a different conversation, and folding it into the
//! parent scope would interleave two narratives by timestamp.
//!
//! # Format
//!
//! JSON Lines, one record per line, discriminated by `type`. Only `user` and
//! `assistant` carry conversation. Records form a DAG through `parentUuid`
//! rather than a flat list, but file order is append order, which is the
//! order a reader wants; `created_at` comes from each record's own
//! `timestamp` so ordering survives regardless.
//!
//! # What a real session looks like
//!
//! Measured on one engram session (231 assistant content blocks):
//! 86 `tool_use`, 86 `tool_result`, 35 `thinking`, **25 `text`**. Roughly
//! nine in ten blocks are machinery. `user.message.content` is usually a
//! block array of `tool_result`s and occasionally a bare string. Both shapes
//! are handled; neither is assumed.

use super::{
    normalize_text, normalize_timestamp, path_string, redact, session_id_from_path,
    sort_newest_first, FilterStats, ReadOptions, ReadResult, SessionRef, TranscriptError, Turn,
    TurnRole,
};
use crate::harness::{self, HarnessSpec};
use serde_json::Value;
use std::path::Path;

/// Record `type` values that are recognized but carry no conversation.
///
/// Listed explicitly so that anything *not* here counts as `unknown_record`
/// and shows up in the report. That is the early-warning signal for a format
/// change in a file engram does not own.
const NON_MESSAGE_TYPES: &[&str] = &[
    "mode",
    "permission-mode",
    "file-history-snapshot",
    "file-history-delta",
    "attachment",
    "ai-title",
    "custom-title",
    "agent-name",
    "last-prompt",
    "queue-operation",
    "system",
    "summary",
    // Bookkeeping for the web / remote-control bridge: session ids and a
    // sequence number, no `message` at all.
    "bridge-session",
    // A pull request opened from this session. Re-appended on every update, so
    // one PR yields several records.
    "pr-link",
];

/// Lists this working directory's transcripts, newest first.
///
/// # Errors
///
/// Returns [`TranscriptError::NoHome`] when `$HOME` is unset, or an I/O error
/// if the projects directory exists but cannot be listed. A directory that
/// simply does not exist yields an empty list — that is "no sessions here",
/// not a failure.
pub fn sessions(spec: &HarnessSpec, cwd: &Path) -> Result<Vec<SessionRef>, TranscriptError> {
    let base = harness::sessions_dir(spec).ok_or(TranscriptError::NoHome)?;
    // Mangle forward from a known cwd; the inverse mapping does not exist.
    let dir = base.join(super::mangle_cwd(cwd));

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(TranscriptError::Io(e)),
    };

    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(TranscriptError::Io)?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let meta = entry.metadata().map_err(TranscriptError::Io)?;
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        found.push((
            SessionRef {
                harness: spec.id,
                session_id: session_id_from_path(&path),
                path: path_string(&path),
                cwd: Some(cwd.to_string_lossy().into_owned()),
                bytes: meta.len(),
            },
            modified,
        ));
    }

    sort_newest_first(&mut found);
    Ok(found.into_iter().map(|(s, _)| s).collect())
}

/// Reads one transcript file into turns.
///
/// # Errors
///
/// Returns [`TranscriptError::TooLarge`] above `opts.max_bytes`,
/// [`TranscriptError::Io`] on a read failure, and
/// [`TranscriptError::BadTimestamp`] when a conversation record's timestamp
/// cannot be parsed — never a silent substitution of the current time.
pub fn read(path: &Path, opts: &ReadOptions) -> Result<ReadResult, TranscriptError> {
    use std::io::BufRead;

    let meta = std::fs::metadata(path).map_err(TranscriptError::Io)?;
    if meta.len() > opts.max_bytes {
        return Err(TranscriptError::TooLarge {
            bytes: meta.len(),
            max_bytes: opts.max_bytes,
        });
    }

    // Streamed rather than slurped: transcripts are append-only logs whose
    // size is bounded only by session length.
    let file = std::fs::File::open(path).map_err(TranscriptError::Io)?;
    let reader = std::io::BufReader::new(file);

    let mut stats = FilterStats::default();
    let mut redactions = redact::Redactions::default();
    let mut turns = Vec::new();

    for line in reader.lines() {
        let line = line.map_err(TranscriptError::Io)?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            // A torn line — an interrupted write, which can land mid-file and
            // not only at the end. Counted rather than ignored.
            stats.unknown_record += 1;
            continue;
        };
        if let Some(turn) = parse_record(&record, opts, &mut stats, &mut redactions)? {
            turns.push(turn);
        }
    }

    Ok(ReadResult {
        turns,
        filtered: stats,
        redactions,
    })
}

/// Converts one record into at most one [`Turn`].
///
/// # Errors
///
/// Returns [`TranscriptError::BadTimestamp`] for a conversation record whose
/// `timestamp` is not a valid instant.
pub(crate) fn parse_record(
    record: &Value,
    opts: &ReadOptions,
    stats: &mut FilterStats,
    redactions: &mut redact::Redactions,
) -> Result<Option<Turn>, TranscriptError> {
    let kind = record.get("type").and_then(Value::as_str).unwrap_or("");

    let role = match kind {
        "user" => TurnRole::User,
        "assistant" => TurnRole::Assistant,
        other if NON_MESSAGE_TYPES.contains(&other) => {
            stats.non_message += 1;
            return Ok(None);
        }
        _ => {
            stats.unknown_record += 1;
            return Ok(None);
        }
    };

    // Harness bookkeeping injected into the conversation, not a participant.
    if record.get("isMeta").and_then(Value::as_bool) == Some(true) {
        stats.meta += 1;
        return Ok(None);
    }
    if record.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
        stats.non_message += 1;
        return Ok(None);
    }
    if !opts.include_sidechains && record.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        stats.sidechain += 1;
        return Ok(None);
    }

    let Some(content) = record.pointer("/message/content") else {
        stats.empty += 1;
        return Ok(None);
    };
    let raw = collect_text(content, opts, stats);

    let Some(text) = normalize_text(&raw, opts.max_chars_per_turn, stats) else {
        stats.empty += 1;
        return Ok(None);
    };
    let text = redact::scrub(&text, redactions);
    if text.trim().is_empty() {
        stats.empty += 1;
        return Ok(None);
    }

    // Identity and time come from the record; neither is synthesized. A
    // record without a uuid cannot be given a stable id, so it is reported as
    // unknown rather than ingested under a fabricated one.
    let Some(source_uuid) = record.get("uuid").and_then(Value::as_str) else {
        stats.unknown_record += 1;
        return Ok(None);
    };
    let raw_ts = record
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("");
    let created_at =
        normalize_timestamp(raw_ts).map_err(|value| TranscriptError::BadTimestamp {
            record: source_uuid.to_string(),
            value,
        })?;

    Ok(Some(Turn {
        source_uuid: source_uuid.to_string(),
        role,
        text,
        created_at,
    }))
}

/// Flattens a record's content into prose, counting what it drops.
///
/// `content` is either a bare string or an array of typed blocks. Tool blocks
/// are summarized to a single line when requested and never inlined whole:
/// the payload is where file contents, command output, and credentials live,
/// and it is the reason ingest is a privacy question at all.
fn collect_text(content: &Value, opts: &ReadOptions, stats: &mut FilterStats) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };

    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text" => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            "thinking" => {
                stats.thinking += 1;
                if opts.include_thinking {
                    if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                        parts.push(t.to_string());
                    }
                }
            }
            "tool_use" => {
                stats.tool_use += 1;
                if opts.include_tools {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                    parts.push(format!("[tool_use: {name}]"));
                }
            }
            "tool_result" => {
                stats.tool_result += 1;
                if opts.include_tools {
                    // Size only. Never the payload.
                    let bytes = block
                        .get("content")
                        .map(|c| c.to_string().len())
                        .unwrap_or(0);
                    parts.push(format!("[tool_result: {bytes} bytes]"));
                }
            }
            _ => stats.non_message += 1,
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str, opts: &ReadOptions) -> (Option<Turn>, FilterStats) {
        let mut stats = FilterStats::default();
        let mut red = redact::Redactions::default();
        let record: Value = serde_json::from_str(line).expect("test fixture is valid JSON");
        let turn = parse_record(&record, opts, &mut stats, &mut red).expect("no timestamp error");
        (turn, stats)
    }

    #[test]
    fn user_content_as_a_bare_string_is_read() {
        let (turn, _) = parse(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-08-01T00:00:00.000Z",
                "message":{"role":"user","content":"just a string"}}"#,
            &ReadOptions::default(),
        );
        let turn = turn.expect("a turn");
        assert_eq!(turn.role, TurnRole::User);
        assert_eq!(turn.text, "just a string");
        assert_eq!(turn.created_at, "2026-08-01T00:00:00Z");
        assert_eq!(turn.source_uuid, "u1");
    }

    #[test]
    fn assistant_text_blocks_are_joined_and_thinking_dropped() {
        let opts = ReadOptions::default();
        let (turn, stats) = parse(
            r#"{"type":"assistant","uuid":"a1","timestamp":"2026-08-01T00:00:00Z",
                "message":{"role":"assistant","content":[
                  {"type":"thinking","thinking":"secret reasoning"},
                  {"type":"text","text":"first"},
                  {"type":"text","text":"second"}]}}"#,
            &opts,
        );
        let turn = turn.expect("a turn");
        assert_eq!(turn.text, "first\n\nsecond");
        assert!(!turn.text.contains("secret reasoning"));
        assert_eq!(stats.thinking, 1);
    }

    #[test]
    fn thinking_is_included_when_asked_for() {
        let opts = ReadOptions {
            include_thinking: true,
            ..ReadOptions::default()
        };
        let (turn, _) = parse(
            r#"{"type":"assistant","uuid":"a1","timestamp":"2026-08-01T00:00:00Z",
                "message":{"role":"assistant","content":[
                  {"type":"thinking","thinking":"the reasoning"}]}}"#,
            &opts,
        );
        assert_eq!(turn.expect("a turn").text, "the reasoning");
    }

    /// The privacy-critical case: even opted in, the payload never appears.
    #[test]
    fn tool_blocks_are_summarized_never_inlined() {
        let opts = ReadOptions {
            include_tools: true,
            ..ReadOptions::default()
        };
        let (turn, stats) = parse(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-08-01T00:00:00Z",
                "message":{"role":"user","content":[
                  {"type":"tool_result","content":"AWS_SECRET_ACCESS_KEY=hunter2"}]}}"#,
            &opts,
        );
        let turn = turn.expect("a turn");
        assert!(
            !turn.text.contains("hunter2"),
            "tool payload leaked: {}",
            turn.text
        );
        assert!(turn.text.starts_with("[tool_result:"));
        assert_eq!(stats.tool_result, 1);
    }

    #[test]
    fn a_tool_only_turn_is_dropped_by_default() {
        let (turn, stats) = parse(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-08-01T00:00:00Z",
                "message":{"role":"user","content":[
                  {"type":"tool_result","content":"file contents"}]}}"#,
            &ReadOptions::default(),
        );
        assert!(turn.is_none());
        assert_eq!(stats.tool_result, 1);
        assert_eq!(stats.empty, 1);
    }

    #[test]
    fn meta_and_sidechain_records_are_dropped_and_counted() {
        let (turn, stats) = parse(
            r#"{"type":"user","uuid":"u1","isMeta":true,"timestamp":"2026-08-01T00:00:00Z",
                "message":{"role":"user","content":"caveat text"}}"#,
            &ReadOptions::default(),
        );
        assert!(turn.is_none());
        assert_eq!(stats.meta, 1);

        let (turn, stats) = parse(
            r#"{"type":"user","uuid":"u2","isSidechain":true,"timestamp":"2026-08-01T00:00:00Z",
                "message":{"role":"user","content":"subagent chatter"}}"#,
            &ReadOptions::default(),
        );
        assert!(turn.is_none());
        assert_eq!(stats.sidechain, 1);
    }

    #[test]
    fn synthetic_slash_command_turns_are_dropped() {
        let (turn, stats) = parse(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-08-01T00:00:00Z",
                "message":{"role":"user","content":"<local-command-stdout>Set model to Haiku</local-command-stdout>"}}"#,
            &ReadOptions::default(),
        );
        assert!(turn.is_none());
        assert_eq!(stats.command_synthetic, 1);
    }

    #[test]
    fn non_message_and_unknown_records_are_counted_separately() {
        let (_, stats) = parse(
            r#"{"type":"mode","mode":"normal"}"#,
            &ReadOptions::default(),
        );
        assert_eq!(stats.non_message, 1);
        assert_eq!(stats.unknown_record, 0);

        // The early-warning signal for a format change in a file we do not own.
        let (_, stats) = parse(
            r#"{"type":"something-new-in-a-future-release"}"#,
            &ReadOptions::default(),
        );
        assert_eq!(stats.unknown_record, 1);
        assert_eq!(stats.non_message, 0);
    }

    /// Both types appeared in Claude Code transcripts after the allowlist was
    /// written, and between them accounted for every `unknown_record` in a
    /// 1372-line session — 55 `bridge-session` and 10 `pr-link`. Neither
    /// carries a `message`, so nothing was ever dropped; the count was pure
    /// false alarm, which is worse than useless because it desensitizes a
    /// reader to the one signal that is supposed to mean something.
    #[test]
    fn bridge_session_and_pr_link_are_non_messages() {
        for record in [
            r#"{"type":"bridge-session","sessionId":"s1","bridgeSessionId":"cse_1","lastSequenceNum":0}"#,
            r#"{"type":"pr-link","sessionId":"s1","prNumber":25,"prUrl":"https://example.invalid/pull/25","prRepository":"owner/repo","timestamp":"2026-08-06T21:12:33.853Z"}"#,
        ] {
            let (turn, stats) = parse(record, &ReadOptions::default());
            assert!(turn.is_none(), "{record} must not become a turn");
            assert_eq!(stats.non_message, 1, "{record} must count as non_message");
            assert_eq!(stats.unknown_record, 0, "{record} must not be unknown");
        }
    }

    #[test]
    fn ansi_and_system_reminders_are_stripped_from_turns() {
        let (turn, _) = parse(
            "{\"type\":\"user\",\"uuid\":\"u1\",\"timestamp\":\"2026-08-01T00:00:00Z\",\
              \"message\":{\"role\":\"user\",\"content\":\"kept \\u001b[1mbold\\u001b[22m <system-reminder>noise</system-reminder>end\"}}",
            &ReadOptions::default(),
        );
        let turn = turn.expect("a turn");
        assert_eq!(turn.text, "kept bold end");
    }

    #[test]
    fn credentials_are_redacted_before_storage() {
        let (turn, _) = parse(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-08-01T00:00:00Z",
                "message":{"role":"user","content":"my key is ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA ok"}}"#,
            &ReadOptions::default(),
        );
        let turn = turn.expect("a turn");
        assert!(!turn.text.contains("ghp_AAAA"), "leaked: {}", turn.text);
        assert!(turn.text.contains("[redacted:github-token]"));
    }

    /// Substituting the current time would collapse reading order invisibly,
    /// because `recall` orders by `created_at`.
    #[test]
    fn an_unparseable_timestamp_is_an_error_not_a_substitution() {
        let mut stats = FilterStats::default();
        let mut red = redact::Redactions::default();
        let record: Value = serde_json::from_str(
            r#"{"type":"user","uuid":"u1","timestamp":"whenever",
                "message":{"role":"user","content":"hello"}}"#,
        )
        .expect("valid JSON");
        let err = parse_record(&record, &ReadOptions::default(), &mut stats, &mut red)
            .expect_err("must not succeed");
        assert!(matches!(err, TranscriptError::BadTimestamp { .. }));
    }

    #[test]
    fn a_record_without_a_uuid_cannot_be_given_a_stable_id() {
        let (turn, stats) = parse(
            r#"{"type":"user","timestamp":"2026-08-01T00:00:00Z",
                "message":{"role":"user","content":"hello"}}"#,
            &ReadOptions::default(),
        );
        assert!(turn.is_none());
        assert_eq!(stats.unknown_record, 1);
    }
}

// Rust guideline compliant 2026-05-18
