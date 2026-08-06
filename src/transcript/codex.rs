// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Reader for Codex CLI session transcripts.
//!
//! # Layout
//!
//! `~/.codex/sessions/YYYY/MM/DD/rollout-<ISO8601>-<uuid>.jsonl`. Unlike
//! Claude Code, the directory tree encodes the *date*, not the working
//! directory, so a session cannot be located by mangling a path. Instead each
//! rollout's first record is a `session_meta` carrying `cwd` verbatim, and
//! listing reads exactly that one line from each file — no mangling, and no
//! ambiguity to undo.
//!
//! # Two channels, and why `event_msg` wins
//!
//! A rollout carries the same conversation twice:
//!
//! * `event_msg` — what the UI displayed. `user_message` and `agent_message`
//!   hold **flat strings**.
//! * `response_item` — the raw API traffic, with block arrays and every
//!   function call.
//!
//! `event_msg` is the primary channel, and not merely because it is simpler
//! to parse. Measured on a real rollout: `event_msg` held 2 user messages
//! where `response_item` held 3 — and the extra one was an
//! `<environment_context>` block the harness injects, not something anybody
//! said. The raw channel is *noisier*, not more complete.
//!
//! `response_item` is therefore a **fallback**, used only when a rollout
//! carries no `event_msg` conversation at all, so a format change that
//! retires the display channel degrades rather than silently yielding
//! nothing.
//!
//! # Size
//!
//! One rollout observed on a developer machine was **114 MB**. Everything
//! here streams line by line, and [`super::ReadOptions::max_bytes`] refuses
//! an oversized file with a structured error rather than trying.

use super::{
    normalize_text, normalize_timestamp, path_string, redact, sort_newest_first, FilterStats,
    ReadOptions, ReadResult, SessionRef, TranscriptError, Turn, TurnRole,
};
use crate::harness::{self, HarnessSpec};
use serde_json::Value;
use std::path::Path;

/// `event_msg` payload types that are recognized but carry no conversation.
///
/// Anything outside this list — and outside the conversation types handled in
/// [`event_turn`] — counts as `unknown_record`, which is the early-warning
/// signal for a format change in a file engram does not own. `token_count`
/// dominates a real rollout (2888 of 5178 events across 41 sessions), so
/// leaving it uncategorized would bury a genuine surprise in noise.
const NON_MESSAGE_EVENTS: &[&str] = &[
    "token_count",
    "task_started",
    "task_complete",
    "thread_settings_applied",
    "thread_goal_updated",
    "context_compacted",
    "turn_aborted",
    "item_completed",
    "entered_review_mode",
    "exited_review_mode",
];

/// `event_msg` payload types that report a tool finishing.
const TOOL_EVENTS: &[&str] = &["patch_apply_end", "web_search_end", "mcp_tool_call_end"];

/// Top-level record types that carry no conversation.
const NON_MESSAGE_RECORDS: &[&str] = &["session_meta", "turn_context", "compacted", "world_state"];

/// Lists the rollouts belonging to a working directory, newest first.
///
/// Reads only each rollout's first line: `session_meta` carries `cwd`, so
/// matching is an exact comparison after canonicalization rather than a
/// reversible-name trick.
///
/// # Errors
///
/// Returns [`TranscriptError::NoHome`] when `$HOME` is unset, or an I/O error
/// if the sessions tree exists but cannot be walked. A missing tree is an
/// empty list, not a failure.
pub fn sessions(spec: &HarnessSpec, cwd: &Path) -> Result<Vec<SessionRef>, TranscriptError> {
    let base = harness::sessions_dir(spec).ok_or(TranscriptError::NoHome)?;
    let mut rollouts = Vec::new();
    collect_rollouts(&base, &mut rollouts)?;

    // Canonicalize once; fall back to the literal path when the directory no
    // longer exists, so archived sessions still match by string.
    let want = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

    let mut found = Vec::new();
    for path in rollouts {
        let Some(meta) = read_session_meta(&path) else {
            continue;
        };
        let matches = meta.cwd.as_ref().is_some_and(|c| {
            let candidate = Path::new(c);
            candidate
                .canonicalize()
                .unwrap_or_else(|_| candidate.to_path_buf())
                == want
        });
        if !matches {
            continue;
        }
        let fs_meta = std::fs::metadata(&path).map_err(TranscriptError::Io)?;
        found.push((
            SessionRef {
                harness: spec.id,
                session_id: meta.session_id,
                path: path_string(&path),
                cwd: meta.cwd,
                bytes: fs_meta.len(),
            },
            fs_meta.modified().unwrap_or(std::time::UNIX_EPOCH),
        ));
    }

    sort_newest_first(&mut found);
    Ok(found.into_iter().map(|(s, _)| s).collect())
}

/// Walks the `YYYY/MM/DD` tree collecting `rollout-*.jsonl` files.
fn collect_rollouts(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<(), TranscriptError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(TranscriptError::Io(e)),
    };
    for entry in entries {
        let entry = entry.map_err(TranscriptError::Io)?;
        let path = entry.path();
        // `file_type` rather than `metadata`: a broken symlink in the tree
        // must not abort the whole listing.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_rollouts(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "jsonl")
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("rollout-"))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// The bits of `session_meta` engram needs.
struct SessionMeta {
    session_id: String,
    cwd: Option<String>,
}

/// Reads a rollout's first record. Returns `None` when the file is empty or
/// its first line is not a usable `session_meta`.
fn read_session_meta(path: &Path) -> Option<SessionMeta> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    let record: Value = serde_json::from_str(first.trim()).ok()?;
    if record.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = record.get("payload")?;
    Some(SessionMeta {
        session_id: session_id_from_filename(path),
        cwd: payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Engram's identifier for a rollout: its file name without the `rollout-`
/// prefix, e.g. `2026-07-17T18-24-31-019f70ad-…`.
///
/// **Deliberately not `session_meta.session_id`.** That field is not unique:
/// resuming a session writes a *new* rollout file carrying the *same*
/// `session_id`, and three files sharing one id were observed on a real
/// machine. Since [`super::turn_id`] is derived from the session id, reusing
/// it would let a turn in one rollout collide with a turn at the same line
/// index in another — and `INSERT OR IGNORE` would then silently drop the
/// second as already-ingested. Losing messages quietly is the one outcome
/// this subsystem must not have.
///
/// The file name carries the start timestamp as well as the uuid, so it is
/// unique per rollout, sortable, and still shows which conversation it
/// belongs to.
pub(crate) fn session_id_from_filename(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    stem.strip_prefix("rollout-")
        .map(str::to_string)
        .unwrap_or(stem)
}

/// Reads one rollout into turns.
///
/// # Errors
///
/// Returns [`TranscriptError::TooLarge`] above `opts.max_bytes` — the default
/// ceiling deliberately refuses the 114 MB rollouts that exist in the wild
/// rather than reading them by surprise — plus [`TranscriptError::Io`] and
/// [`TranscriptError::BadTimestamp`] as for any reader.
pub fn read(path: &Path, opts: &ReadOptions) -> Result<ReadResult, TranscriptError> {
    use std::io::BufRead;

    let meta = std::fs::metadata(path).map_err(TranscriptError::Io)?;
    if meta.len() > opts.max_bytes {
        return Err(TranscriptError::TooLarge {
            bytes: meta.len(),
            max_bytes: opts.max_bytes,
        });
    }

    let file = std::fs::File::open(path).map_err(TranscriptError::Io)?;
    let reader = std::io::BufReader::new(file);

    let mut stats = FilterStats::default();
    let mut redactions = redact::Redactions::default();
    // Both channels are collected in one pass; only the extracted turns are
    // retained, so this costs a conversation's worth of memory, not a
    // rollout's.
    let mut display = Vec::new();
    let mut raw = Vec::new();
    let mut raw_seen = 0usize;

    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(TranscriptError::Io)?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            // A torn line — an interrupted write. Observed mid-file in a real
            // rollout, not only at the end, so this is counted rather than
            // ignored: one unreadable record is worth reporting, and a rising
            // count means something worse.
            stats.unknown_record += 1;
            continue;
        };

        let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "event_msg" => {
                if let Some(turn) = event_turn(&record, index, opts, &mut stats, &mut redactions)? {
                    display.push(turn);
                }
            }
            "response_item" => {
                raw_seen += 1;
                if let Some(turn) =
                    response_turn(&record, index, opts, &mut stats, &mut redactions)?
                {
                    raw.push(turn);
                }
            }
            other if NON_MESSAGE_RECORDS.contains(&other) => stats.non_message += 1,
            _ => stats.unknown_record += 1,
        }
    }

    // The display channel wins when it carried anything: it is what the user
    // actually saw, and it excludes the `<environment_context>` block the
    // harness injects into the raw channel.
    let turns = if display.is_empty() {
        raw
    } else {
        // The raw records duplicate what the display channel already has.
        stats.non_message += raw_seen;
        display
    };

    Ok(ReadResult {
        turns,
        filtered: stats,
        redactions,
    })
}

/// Converts an `event_msg` record into at most one turn.
fn event_turn(
    record: &Value,
    index: usize,
    opts: &ReadOptions,
    stats: &mut FilterStats,
    redactions: &mut redact::Redactions,
) -> Result<Option<Turn>, TranscriptError> {
    let payload = record.get("payload");
    let event = payload
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let (role, raw_text) = match event {
        "user_message" => (TurnRole::User, message_field(payload)),
        // Both `commentary` and `final_answer` phases are real assistant
        // prose; neither is a summary of the other.
        "agent_message" => (TurnRole::Assistant, message_field(payload)),
        "agent_reasoning" => {
            stats.thinking += 1;
            if !opts.include_thinking {
                return Ok(None);
            }
            (TurnRole::Assistant, message_field(payload))
        }
        other if TOOL_EVENTS.contains(&other) => {
            stats.tool_result += 1;
            if !opts.include_tools {
                return Ok(None);
            }
            (TurnRole::Assistant, Some(format!("[{other}]")))
        }
        other if NON_MESSAGE_EVENTS.contains(&other) => {
            stats.non_message += 1;
            return Ok(None);
        }
        _ => {
            stats.unknown_record += 1;
            return Ok(None);
        }
    };

    let Some(raw_text) = raw_text else {
        stats.empty += 1;
        return Ok(None);
    };
    finish_turn(record, index, role, &raw_text, opts, stats, redactions)
}

/// `payload.message`, or `payload.text` for the reasoning event.
fn message_field(payload: Option<&Value>) -> Option<String> {
    let payload = payload?;
    payload
        .get("message")
        .or_else(|| payload.get("text"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Converts a `response_item` record into at most one turn — the fallback
/// channel, and the place tool traffic is counted.
fn response_turn(
    record: &Value,
    index: usize,
    opts: &ReadOptions,
    stats: &mut FilterStats,
    redactions: &mut redact::Redactions,
) -> Result<Option<Turn>, TranscriptError> {
    let payload = record.get("payload");
    let item = payload
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");

    match item {
        "message" => {}
        "reasoning" => {
            stats.thinking += 1;
            return Ok(None);
        }
        // Every shape of tool invocation. The `web_search_call` /
        // `tool_search_*` trio was found by the `unknown_record` counter on
        // real rollouts after the first three were implemented — which is
        // exactly what that counter is for.
        "function_call" | "custom_tool_call" | "web_search_call" | "tool_search_call"
        | "local_shell_call" => {
            stats.tool_use += 1;
            return Ok(None);
        }
        "function_call_output"
        | "custom_tool_call_output"
        | "tool_search_output"
        | "local_shell_call_output" => {
            stats.tool_result += 1;
            return Ok(None);
        }
        _ => {
            stats.unknown_record += 1;
            return Ok(None);
        }
    }

    let role = match payload.and_then(|p| p.get("role")).and_then(Value::as_str) {
        Some("user") => TurnRole::User,
        Some("assistant") => TurnRole::Assistant,
        // `developer` and `system` are instructions to the model, not
        // conversation.
        _ => {
            stats.non_message += 1;
            return Ok(None);
        }
    };

    let text = payload
        .and_then(|p| p.get("content"))
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default();

    finish_turn(record, index, role, &text, opts, stats, redactions)
}

/// Shared tail: normalize, redact, stamp, and identify.
fn finish_turn(
    record: &Value,
    index: usize,
    role: TurnRole,
    raw_text: &str,
    opts: &ReadOptions,
    stats: &mut FilterStats,
    redactions: &mut redact::Redactions,
) -> Result<Option<Turn>, TranscriptError> {
    let Some(text) = normalize_text(raw_text, opts.max_chars_per_turn, stats) else {
        stats.empty += 1;
        return Ok(None);
    };
    let text = redact::scrub(&text, redactions);
    if text.trim().is_empty() {
        stats.empty += 1;
        return Ok(None);
    }

    let source_uuid = source_uuid(index, &text);
    let raw_ts = record
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("");
    let created_at =
        normalize_timestamp(raw_ts).map_err(|value| TranscriptError::BadTimestamp {
            record: source_uuid.clone(),
            value,
        })?;

    Ok(Some(Turn {
        source_uuid,
        role,
        text,
        created_at,
    }))
}

/// Stable per-record identity for a rollout.
///
/// Codex records carry no id of their own, so identity is the record's line
/// index **plus a digest of its text**. The index alone would be stable for
/// an append-only log — which rollouts are today — but folding in the content
/// means that if a rollout is ever rewritten with a line inserted, the
/// unchanged turns keep their ids instead of every subsequent turn appearing
/// to be new. The digest is a v5 UUID purely because that hash is already a
/// dependency; nothing here is a security boundary.
pub(crate) fn source_uuid(index: usize, text: &str) -> String {
    let digest = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, text.as_bytes());
    format!("{index}:{}", digest.simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_event(line: &str, opts: &ReadOptions) -> (Option<Turn>, FilterStats) {
        let mut stats = FilterStats::default();
        let mut red = redact::Redactions::default();
        let record: Value = serde_json::from_str(line).expect("valid JSON");
        let turn = event_turn(&record, 0, opts, &mut stats, &mut red).expect("no timestamp error");
        (turn, stats)
    }

    fn parse_response(line: &str, opts: &ReadOptions) -> (Option<Turn>, FilterStats) {
        let mut stats = FilterStats::default();
        let mut red = redact::Redactions::default();
        let record: Value = serde_json::from_str(line).expect("valid JSON");
        let turn =
            response_turn(&record, 0, opts, &mut stats, &mut red).expect("no timestamp error");
        (turn, stats)
    }

    #[test]
    fn event_user_and_agent_messages_are_flat_strings() {
        let (turn, _) = parse_event(
            r#"{"timestamp":"2026-06-25T15:26:28.060Z","type":"event_msg",
                "payload":{"type":"user_message","message":"how do I wire this up?"}}"#,
            &ReadOptions::default(),
        );
        let turn = turn.expect("a turn");
        assert_eq!(turn.role, TurnRole::User);
        assert_eq!(turn.text, "how do I wire this up?");
        assert_eq!(turn.created_at, "2026-06-25T15:26:28.06Z");

        let (turn, _) = parse_event(
            r#"{"timestamp":"2026-06-25T15:26:35.068Z","type":"event_msg",
                "payload":{"type":"agent_message","message":"Like this.","phase":"final_answer"}}"#,
            &ReadOptions::default(),
        );
        assert_eq!(turn.expect("a turn").role, TurnRole::Assistant);
    }

    /// Both phases are real prose; neither summarizes the other.
    #[test]
    fn commentary_and_final_answer_are_both_kept() {
        for phase in ["commentary", "final_answer"] {
            let (turn, _) = parse_event(
                &format!(
                    r#"{{"timestamp":"2026-06-25T15:26:35Z","type":"event_msg",
                        "payload":{{"type":"agent_message","message":"text","phase":"{phase}"}}}}"#
                ),
                &ReadOptions::default(),
            );
            assert!(turn.is_some(), "{phase} was dropped");
        }
    }

    #[test]
    fn agent_reasoning_is_thinking_and_gated() {
        let line = r#"{"timestamp":"2026-06-25T15:26:35Z","type":"event_msg",
            "payload":{"type":"agent_reasoning","text":"internal deliberation"}}"#;

        let (turn, stats) = parse_event(line, &ReadOptions::default());
        assert!(turn.is_none());
        assert_eq!(stats.thinking, 1);

        let opts = ReadOptions {
            include_thinking: true,
            ..ReadOptions::default()
        };
        let (turn, _) = parse_event(line, &opts);
        assert_eq!(turn.expect("a turn").text, "internal deliberation");
    }

    /// `token_count` alone is 2888 of 5178 events across 41 real sessions;
    /// leaving it uncategorized would bury a genuine format change.
    #[test]
    fn known_non_message_events_do_not_look_like_a_format_change() {
        for event in NON_MESSAGE_EVENTS {
            let (turn, stats) = parse_event(
                &format!(
                    r#"{{"timestamp":"2026-06-25T15:26:35Z","type":"event_msg",
                        "payload":{{"type":"{event}"}}}}"#
                ),
                &ReadOptions::default(),
            );
            assert!(turn.is_none(), "{event} produced a turn");
            assert_eq!(stats.non_message, 1, "{event} miscounted");
            assert_eq!(stats.unknown_record, 0, "{event} looked unknown");
        }
    }

    #[test]
    fn an_unrecognized_event_is_counted_as_unknown() {
        let (turn, stats) = parse_event(
            r#"{"timestamp":"2026-06-25T15:26:35Z","type":"event_msg",
                "payload":{"type":"something_new_in_a_future_release"}}"#,
            &ReadOptions::default(),
        );
        assert!(turn.is_none());
        assert_eq!(stats.unknown_record, 1);
        assert_eq!(stats.non_message, 0);
    }

    #[test]
    fn response_items_count_tools_and_read_block_arrays() {
        // Every tool shape seen in real rollouts must be recognized as tool
        // traffic, not reported as a format change.
        for call in [
            "function_call",
            "custom_tool_call",
            "web_search_call",
            "tool_search_call",
        ] {
            let (turn, stats) = parse_response(
                &format!(
                    r#"{{"timestamp":"2026-06-25T15:26:28Z","type":"response_item",
                        "payload":{{"type":"{call}","name":"shell"}}}}"#
                ),
                &ReadOptions::default(),
            );
            assert!(turn.is_none(), "{call} produced a turn");
            assert_eq!(stats.tool_use, 1, "{call} not counted as a tool call");
            assert_eq!(
                stats.unknown_record, 0,
                "{call} looked like a format change"
            );
        }
        for output in [
            "function_call_output",
            "custom_tool_call_output",
            "tool_search_output",
        ] {
            let (_, stats) = parse_response(
                &format!(
                    r#"{{"timestamp":"2026-06-25T15:26:28Z","type":"response_item",
                        "payload":{{"type":"{output}","output":"x"}}}}"#
                ),
                &ReadOptions::default(),
            );
            assert_eq!(stats.tool_result, 1, "{output} not counted");
            assert_eq!(
                stats.unknown_record, 0,
                "{output} looked like a format change"
            );
        }

        let (turn, _) = parse_response(
            r#"{"timestamp":"2026-06-25T15:26:28Z","type":"response_item",
                "payload":{"type":"message","role":"assistant",
                "content":[{"type":"output_text","text":"first"},
                           {"type":"output_text","text":"second"}]}}"#,
            &ReadOptions::default(),
        );
        assert_eq!(turn.expect("a turn").text, "first\n\nsecond");
    }

    /// `developer` messages are instructions to the model, not conversation.
    #[test]
    fn developer_and_system_response_messages_are_not_conversation() {
        for role in ["developer", "system"] {
            let (turn, stats) = parse_response(
                &format!(
                    r#"{{"timestamp":"2026-06-25T15:26:28Z","type":"response_item",
                        "payload":{{"type":"message","role":"{role}",
                        "content":[{{"type":"input_text","text":"instructions"}}]}}}}"#
                ),
                &ReadOptions::default(),
            );
            assert!(turn.is_none(), "{role} produced a turn");
            assert_eq!(stats.non_message, 1);
        }
    }

    #[test]
    fn source_uuid_is_stable_and_content_sensitive() {
        let a = source_uuid(3, "hello");
        assert_eq!(a, source_uuid(3, "hello"));
        // Position matters, so two identical messages stay distinct...
        assert_ne!(a, source_uuid(4, "hello"));
        // ...and content matters, so an inserted line does not renumber every
        // turn into a new identity.
        assert_ne!(a, source_uuid(3, "goodbye"));
        assert!(a.starts_with("3:"));
    }

    /// Resuming a Codex session writes a new rollout carrying the *same*
    /// `session_meta.session_id` — three files sharing one id were observed
    /// on a real machine. Engram's session id must therefore come from the
    /// file name, or turns at the same line index in two rollouts would
    /// derive the same [`super::turn_id`] and `INSERT OR IGNORE` would
    /// silently drop the second.
    #[test]
    fn session_id_is_unique_per_rollout_not_per_conversation() {
        let a = session_id_from_filename(Path::new(
            "/x/rollout-2026-07-17T18-24-31-019f70ad-e1ad-79f2-8fe6-ecab598ed547.jsonl",
        ));
        let b = session_id_from_filename(Path::new(
            "/x/rollout-2026-07-17T19-02-08-019f70ad-e1ad-79f2-8fe6-ecab598ed547.jsonl",
        ));
        assert_eq!(
            a,
            "2026-07-17T18-24-31-019f70ad-e1ad-79f2-8fe6-ecab598ed547"
        );
        assert_ne!(
            a, b,
            "two rollouts of one conversation must not share an id"
        );

        // And therefore the derived turn ids cannot collide either.
        let same_record = source_uuid(7, "identical text");
        assert_ne!(
            super::super::turn_id("codex", &a, &same_record),
            super::super::turn_id("codex", &b, &same_record),
        );

        // A name without the prefix is kept whole rather than mangled.
        assert_eq!(
            session_id_from_filename(Path::new("/x/something-else.jsonl")),
            "something-else"
        );
    }

    #[test]
    fn credentials_are_redacted_before_storage() {
        let (turn, _) = parse_event(
            r#"{"timestamp":"2026-06-25T15:26:28Z","type":"event_msg",
                "payload":{"type":"user_message","message":"token ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA here"}}"#,
            &ReadOptions::default(),
        );
        let turn = turn.expect("a turn");
        assert!(!turn.text.contains("ghp_AAAA"), "leaked: {}", turn.text);
        assert!(turn.text.contains("[redacted:github-token]"));
    }

    #[test]
    fn an_unparseable_timestamp_is_an_error_not_a_substitution() {
        let mut stats = FilterStats::default();
        let mut red = redact::Redactions::default();
        let record: Value = serde_json::from_str(
            r#"{"timestamp":"whenever","type":"event_msg",
                "payload":{"type":"user_message","message":"hello"}}"#,
        )
        .expect("valid JSON");
        let err = event_turn(&record, 0, &ReadOptions::default(), &mut stats, &mut red)
            .expect_err("must not succeed");
        assert!(matches!(err, TranscriptError::BadTimestamp { .. }));
    }
}

// Rust guideline compliant 2026-05-18
