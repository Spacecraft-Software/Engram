// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Reader for Kimi Code's session wire logs.
//!
//! Kimi Code keeps `~/.kimi-code/sessions/wd_<name>_<hash>/<session>/`, and the
//! main agent's log at `agents/main/wire.jsonl`. Sibling `agents/agent-N/`
//! logs are subagents — different conversations, not read, for the same reason
//! the Claude Code reader leaves `subagents/` alone.
//!
//! # Attribution
//!
//! The directory name's hash is not documented, so it is not inverted. The
//! session's own `state.json` records `workDir` for some sessions, and
//! `~/.kimi-code/session_index.jsonl` maps `sessionId` to `workDir` for the
//! rest; between them every session on the surveyed machine was attributed,
//! and where both spoke they agreed. Listing matches that directory exactly.
//!
//! # Which records
//!
//! A wire log carries the conversation twice. `context.append_message` is the
//! model's context — injected reminders, compaction summaries, and, in older
//! logs, no timestamp at all. The display channel is timed: `turn.prompt` and
//! `turn.steer` (what the user typed, at turn start and mid-turn) and the
//! `context.append_loop_event` stream, whose `content.part` events hold the
//! reply text and thinking. That channel is read; the context channel is
//! counted as `non_message`, as Codex's raw `response_item` channel is.
//!
//! Prompts and steers carry an `origin`. Only `origin.kind == "user"` is a
//! person; skill activations and background-task notifications arrive through
//! the same records and are counted as `meta`.
//!
//! A `prompt.accepted` record can carry the prompt's text too. Usually that
//! text reappears as a `turn.prompt`; when it never does — a prompt queued
//! while a turn failed — it is the only record of what the user typed, so it
//! is kept, and dropped as a duplicate otherwise.
//!
//! # Protocol versions
//!
//! Wire protocol 1.3 and later carry the timed display channel. Protocol 1.0
//! logs — on the surveyed machine, sessions migrated from the legacy `~/.kimi`
//! layout — hold only the untimed context channel. Inventing times would
//! destroy reading order, so those sessions are not listed, and asking for one
//! by id is an error that names `kimi export` and `engram import` instead.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    is_path_safe_id, iso_millis, normalize_text, path_string, positional_id, redact,
    sort_newest_first, FilterStats, ReadOptions, ReadResult, SessionRef, TranscriptError, Turn,
    TurnRole,
};
use crate::harness::{self, HarnessSpec};

/// The main agent's log, relative to a session directory.
const WIRE: &str = "agents/main/wire.jsonl";

/// Oldest wire protocol whose display channel carries timestamps.
const FIRST_TIMED_PROTOCOL: (u32, u32) = (1, 3);

/// The refusal for a log with no timed channel.
const UNTIMED: &str = "its wire log records no message times (protocol older than 1.3, \
    typically a session migrated from ~/.kimi), and engram will not invent them";

/// Record types that are harness machinery rather than conversation.
const NON_MESSAGE: &[&str] = &[
    "config.update",
    "context.append_message",
    "context.apply_compaction",
    "file_history.checkpoint",
    "file_history.tracked",
    "full_compaction.begin",
    "full_compaction.cancel",
    "full_compaction.complete",
    "goal.create",
    "goal.update",
    "interaction.request",
    "interaction.resolved",
    "llm.request",
    "llm.tools_snapshot",
    "mcp.tools_discovered",
    "metadata",
    "micro_compaction.apply",
    "permission.record_approval_result",
    "permission.set_mode",
    "plan.revision",
    "plan_mode.cancel",
    "plan_mode.enter",
    "plan_mode.exit",
    "plugin.session_start",
    "profile.bind",
    "prompt.completed",
    "runtime.set_binding",
    "staleGuard.recorded",
    "swarm_mode.enter",
    "swarm_mode.exit",
    "task.started",
    "task.terminated",
    "task.waitDelivered",
    "token_counting.measured",
    "token_counting.rebased",
    "token_counting.turn_recorded",
    "tools.set_active_tools",
    "tools.update_store",
    "turn.cancel",
    "turn.ended",
    "turn.step.interrupted",
    "usage.record",
];

/// Loop events that are step bookkeeping.
const NON_MESSAGE_EVENTS: &[&str] = &["step.begin", "step.end"];

/// Lists the sessions Kimi Code recorded for `cwd`, newest first.
///
/// # Errors
///
/// [`TranscriptError::NoHome`] when `$HOME` is unset, or an I/O error when the
/// sessions directory exists but cannot be listed.
pub fn sessions(spec: &HarnessSpec, cwd: &Path) -> Result<Vec<SessionRef>, TranscriptError> {
    let base = harness::sessions_dir(spec).ok_or(TranscriptError::NoHome)?;
    let want = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let index = session_index(&base);
    let mut found = Vec::new();
    for project in subdirs(&base)? {
        for dir in subdirs(&project)? {
            let Some(id) = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                continue;
            };
            let Some(work_dir) = work_dir(&dir).or_else(|| index.get(&id).cloned()) else {
                continue;
            };
            let candidate = PathBuf::from(&work_dir);
            if candidate.canonicalize().unwrap_or(candidate) != want {
                continue;
            }
            let Some(entry) = session_ref(spec, &dir, id, Some(work_dir))? else {
                continue;
            };
            if is_timed(Path::new(&entry.0.path))? {
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
    let index = session_index(&base);
    for project in subdirs(&base)? {
        let dir = project.join(id);
        let cwd = work_dir(&dir).or_else(|| index.get(id).cloned());
        if let Some((session, _)) = session_ref(spec, &dir, id.to_string(), cwd)? {
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

/// `sessionId` → `workDir` from `session_index.jsonl` beside the sessions
/// directory. Unreadable lines are skipped: the index is a fallback, and a
/// session it cannot attribute is simply not listed.
fn session_index(base: &Path) -> HashMap<String, String> {
    let Some(path) = base.parent().map(|root| root.join("session_index.jsonl")) else {
        return HashMap::new();
    };
    let Ok(file) = std::fs::File::open(path) else {
        return HashMap::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter_map(|entry| {
            Some((
                entry["sessionId"].as_str()?.to_string(),
                entry["workDir"].as_str()?.to_string(),
            ))
        })
        .collect()
}

/// The `workDir` a session's `state.json` records, when it records one.
fn work_dir(dir: &Path) -> Option<String> {
    let state = std::fs::read_to_string(dir.join("state.json")).ok()?;
    serde_json::from_str::<Value>(&state).ok()?["workDir"]
        .as_str()
        .map(str::to_owned)
}

/// A session directory with a main wire log, or `None` when it has none.
fn session_ref(
    spec: &HarnessSpec,
    dir: &Path,
    id: String,
    cwd: Option<String>,
) -> Result<Option<(SessionRef, std::time::SystemTime)>, TranscriptError> {
    let path = dir.join(WIRE);
    let meta = match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(Some((
        SessionRef {
            harness: spec.id,
            session_id: id,
            path: path_string(&path),
            cwd,
            bytes: meta.len(),
        },
        meta.modified().unwrap_or(std::time::UNIX_EPOCH),
    )))
}

/// Whether the log's `metadata` line declares a protocol with a timed
/// display channel. Reads only the first line.
fn is_timed(path: &Path) -> Result<bool, TranscriptError> {
    let mut first = String::new();
    std::io::BufReader::new(std::fs::File::open(path)?).read_line(&mut first)?;
    Ok(serde_json::from_str::<Value>(&first)
        .ok()
        .and_then(|meta| protocol(meta["protocol_version"].as_str()?))
        .is_some_and(|version| version >= FIRST_TIMED_PROTOCOL))
}

/// `"1.4"` → `(1, 4)`.
fn protocol(version: &str) -> Option<(u32, u32)> {
    let (major, minor) = version.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// The text blocks of an `input`/`content` array, joined.
fn text_blocks(blocks: &Value) -> String {
    blocks
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// A turn read from the log, before normalization.
struct Raw {
    role: TurnRole,
    text: String,
    source_uuid: String,
    created_at: String,
}

/// Reads one session's main wire log into turns.
///
/// # Errors
///
/// [`TranscriptError::Unreadable`] for an untimed (pre-1.3) log,
/// [`TranscriptError::TooLarge`] before reading anything, an I/O error, or
/// [`TranscriptError::BadTimestamp`] for a display record whose `time` is not
/// a valid Unix millisecond.
pub fn read(path: &Path, opts: &ReadOptions) -> Result<ReadResult, TranscriptError> {
    let bytes = std::fs::metadata(path)?.len();
    if bytes > opts.max_bytes {
        return Err(TranscriptError::TooLarge {
            bytes,
            max_bytes: opts.max_bytes,
        });
    }
    if !is_timed(path)? {
        return Err(TranscriptError::Unreadable(UNTIMED));
    }

    let mut stats = FilterStats::default();
    let mut raws: Vec<Raw> = Vec::new();
    // Prompt text known from `turn.prompt`, and `prompt.accepted` texts held
    // until the end to see whether a `turn.prompt` repeats them.
    let mut prompted = HashSet::new();
    let mut accepted: Vec<Raw> = Vec::new();

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
        let kind = record["type"].as_str().unwrap_or("");
        match kind {
            "turn.prompt" | "turn.steer" => {
                if record["origin"]["kind"] != "user" {
                    stats.meta += 1;
                    continue;
                }
                let text = text_blocks(&record["input"]);
                prompted.insert(text.trim().to_string());
                raws.push(Raw {
                    role: TurnRole::User,
                    source_uuid: positional_id(line_index, &text),
                    text,
                    created_at: millis(&record, line_index)?,
                });
            }
            "prompt.accepted" => {
                let text = text_blocks(&record["content"]);
                if text.trim().is_empty() {
                    stats.non_message += 1;
                    continue;
                }
                let source_uuid = match record["promptId"].as_str() {
                    Some(id) => format!("prompt:{id}"),
                    None => positional_id(line_index, &text),
                };
                accepted.push(Raw {
                    role: TurnRole::User,
                    source_uuid,
                    text,
                    created_at: millis(&record, line_index)?,
                });
            }
            "context.append_loop_event" => {
                let event = &record["event"];
                match event["type"].as_str().unwrap_or("") {
                    "content.part" => {
                        let part = &event["part"];
                        let (text, thinking) = match part["type"].as_str() {
                            Some("text") => (part["text"].as_str(), false),
                            Some("think") => (part["think"].as_str(), true),
                            _ => {
                                stats.unknown_record += 1;
                                continue;
                            }
                        };
                        if thinking {
                            stats.thinking += 1;
                            if !opts.include_thinking {
                                continue;
                            }
                        }
                        let text = text.unwrap_or("").to_string();
                        let source_uuid = match event["uuid"].as_str() {
                            Some(uuid) => format!("part:{uuid}"),
                            None => {
                                stats.missing_uuid += 1;
                                continue;
                            }
                        };
                        raws.push(Raw {
                            role: TurnRole::Assistant,
                            text,
                            source_uuid,
                            created_at: millis(&record, line_index)?,
                        });
                    }
                    "tool.call" => {
                        stats.tool_use += 1;
                        if opts.include_tools {
                            let name = event["name"].as_str().unwrap_or("unknown");
                            let text = format!("[tool_use: {name}]");
                            raws.push(Raw {
                                role: TurnRole::Assistant,
                                source_uuid: positional_id(line_index, &text),
                                text,
                                created_at: millis(&record, line_index)?,
                            });
                        }
                    }
                    "tool.result" => {
                        stats.tool_result += 1;
                        if opts.include_tools {
                            let size = event["result"].to_string().len();
                            let text = format!("[tool_result: {size} bytes]");
                            raws.push(Raw {
                                role: TurnRole::Assistant,
                                source_uuid: positional_id(line_index, &text),
                                text,
                                created_at: millis(&record, line_index)?,
                            });
                        }
                    }
                    other if NON_MESSAGE_EVENTS.contains(&other) => stats.non_message += 1,
                    _ => stats.unknown_record += 1,
                }
            }
            other if NON_MESSAGE.contains(&other) => stats.non_message += 1,
            _ => stats.unknown_record += 1,
        }
    }

    for raw in accepted {
        if prompted.contains(raw.text.trim()) {
            stats.non_message += 1;
        } else {
            raws.push(raw);
        }
    }
    // Accepted-only prompts were held back; restore file order. The sort is
    // stable, so records sharing a millisecond keep their read order.
    raws.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    let mut redactions = redact::Redactions::default();
    let mut turns = Vec::new();
    for raw in raws {
        let Some(text) = normalize_text(&raw.text, opts.max_chars_per_turn, &mut stats) else {
            stats.empty += 1;
            continue;
        };
        turns.push(Turn {
            source_uuid: raw.source_uuid,
            role: raw.role,
            text: redact::scrub(&text, &mut redactions),
            created_at: raw.created_at,
        });
    }
    Ok(ReadResult {
        turns,
        filtered: stats,
        redactions,
    })
}

/// The record's `time` (Unix milliseconds) as ISO 8601 UTC.
fn millis(record: &Value, line: usize) -> Result<String, TranscriptError> {
    record["time"]
        .as_i64()
        .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map(iso_millis)
        .ok_or_else(|| TranscriptError::BadTimestamp {
            record: format!("line {}", line + 1),
            value: record["time"].to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const META: &str = r#"{"type":"metadata","protocol_version":"1.4","created_at":1}"#;

    fn prompt(ms: i64, kind: &str, origin: &str, text: &str) -> String {
        json!({"type": kind, "input": [{"type": "text", "text": text}],
            "origin": {"kind": origin}, "time": ms})
        .to_string()
    }

    fn part(ms: i64, uuid: &str, kind: &str, text: &str) -> String {
        json!({"type": "context.append_loop_event", "time": ms,
            "event": {"type": "content.part", "uuid": uuid, "part": {"type": kind, kind: text}}})
        .to_string()
    }

    fn read_lines(lines: &[String], opts: ReadOptions) -> Result<ReadResult, TranscriptError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wire.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").expect("write");
        read(&path, &opts)
    }

    #[test]
    fn prompts_steers_and_reply_parts_are_turns_in_order() {
        let result = read_lines(
            &[
                META.to_string(),
                prompt(1_781_305_535_254, "turn.prompt", "user", "Implement the Guile port"),
                json!({"type": "context.append_message", "time": 1_781_305_535_255_i64,
                    "message": {"role": "user", "content": [{"type": "text", "text": "Implement the Guile port"}]}})
                .to_string(),
                part(1_781_305_536_000, "p1", "think", "private"),
                part(1_781_305_537_000, "p2", "text", "Starting with the skeleton."),
                prompt(1_781_305_538_000, "turn.steer", "user", "Fix issues found so far"),
                prompt(1_781_305_539_000, "turn.steer", "background_task", "<notification/>"),
                prompt(1_781_305_540_000, "turn.prompt", "skill_activation", "Skill loaded"),
            ],
            ReadOptions::default(),
        )
        .expect("read");
        let texts: Vec<_> = result.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Implement the Guile port",
                "Starting with the skeleton.",
                "Fix issues found so far"
            ]
        );
        assert_eq!(result.turns[0].created_at, "2026-06-12T23:05:35.254Z");
        assert_eq!(result.turns[1].source_uuid, "part:p2");
        assert_eq!(result.filtered.thinking, 1);
        assert_eq!(result.filtered.meta, 2);
        // The metadata line and the context-channel duplicate.
        assert_eq!(result.filtered.non_message, 2);
        assert_eq!(result.filtered.unknown_record, 0);
    }

    #[test]
    fn an_accepted_prompt_is_kept_only_when_no_turn_prompt_repeats_it() {
        let accepted = |ms: i64, id: &str, text: &str| {
            json!({"type": "prompt.accepted", "promptId": id, "time": ms,
                "content": [{"type": "text", "text": text}]})
            .to_string()
        };
        let result = read_lines(
            &[
                META.to_string(),
                accepted(1_000, "m1", "Continue"),
                prompt(1_001, "turn.prompt", "user", "Continue"),
                accepted(2_000, "m2", "Queued while the turn failed"),
            ],
            ReadOptions::default(),
        )
        .expect("read");
        let texts: Vec<_> = result.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["Continue", "Queued while the turn failed"]);
        assert_eq!(result.turns[1].source_uuid, "prompt:m2");
    }

    #[test]
    fn tools_only_on_request_and_never_payloads() {
        let lines = [
            META.to_string(),
            json!({"type": "context.append_loop_event", "time": 10,
                "event": {"type": "tool.call", "uuid": "t1", "name": "Bash",
                    "args": {"command": "cat ~/.ssh/id_ed25519"}}})
            .to_string(),
            json!({"type": "context.append_loop_event", "time": 11,
                "event": {"type": "tool.result", "toolCallId": "t1",
                    "result": {"output": "never store this payload"}}})
            .to_string(),
        ];
        let default = read_lines(&lines, ReadOptions::default()).expect("read");
        assert!(default.turns.is_empty());
        assert_eq!(default.filtered.tool_use, 1);
        assert_eq!(default.filtered.tool_result, 1);

        let included = read_lines(
            &lines,
            ReadOptions {
                include_tools: true,
                ..ReadOptions::default()
            },
        )
        .expect("read");
        assert_eq!(included.turns[0].text, "[tool_use: Bash]");
        for turn in &included.turns {
            assert!(!turn.text.contains("never store") && !turn.text.contains("ssh"));
        }
    }

    #[test]
    fn untimed_protocol_is_refused_not_guessed() {
        let old = r#"{"type":"metadata","protocol_version":"1.0","created_at":1}"#.to_string();
        assert!(matches!(
            read_lines(&[old], ReadOptions::default()),
            Err(TranscriptError::Unreadable(_))
        ));
        assert_eq!(protocol("1.10"), Some((1, 10)));
        assert!(protocol("1.10").expect("version") >= FIRST_TIMED_PROTOCOL);
    }

    #[test]
    fn bad_time_torn_lines_and_unknown_types() {
        let result = read_lines(
            &[
                META.to_string(),
                "{torn".to_string(),
                r#"{"type":"brand.new","time":1}"#.to_string(),
            ],
            ReadOptions::default(),
        )
        .expect("read");
        assert_eq!(result.filtered.torn_line, 1);
        assert_eq!(result.filtered.unknown_record, 1);

        let bad = json!({"type": "turn.prompt", "origin": {"kind": "user"},
            "input": [{"type": "text", "text": "hi"}], "time": "noon"})
        .to_string();
        assert!(matches!(
            read_lines(&[META.to_string(), bad], ReadOptions::default()),
            Err(TranscriptError::BadTimestamp { .. })
        ));
    }
}
