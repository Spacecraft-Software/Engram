// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading a harness's own session transcript into engram.
//!
//! Until this module existed, "shared verbatim chat memory" meant *rows are
//! never mutated once written* — not *the conversation was recorded*. The
//! only way anything reached the store was an agent deciding to call
//! `remember`. This is the capture path: it reads the transcript a harness
//! already writes for itself and turns it into ordinary memories.
//!
//! # What is dropped, and why that is the interesting part
//!
//! A real session is mostly not prose. Measured on one engram session:
//! 86 `tool_use` blocks, 86 `tool_result`, 35 `thinking`, and 25 blocks of
//! actual text. Ingesting a transcript raw would fill the store with file
//! contents and command output — which is both useless for retrieval and the
//! single largest privacy exposure engram could create, since tool results
//! carry whatever the session read: `.env` files, diffs, credentials.
//!
//! So the defaults are conservative — tool payloads and thinking are dropped
//! unless explicitly requested — every drop is *counted* and reported, and
//! whatever survives goes through [`redact`] before it is stored. A user who
//! wants the noise can ask for it; nobody gets it by accident.
//!
//! # Never guess
//!
//! Harness transcript formats are private, undocumented, and change without
//! notice. Two rules follow, and both are load-bearing:
//!
//! * Unknown record types are **counted and reported**, never silently
//!   skipped, so a format change surfaces as "N unknown records" rather than
//!   as a quietly shorter archive.
//! * An unparseable timestamp is an **error**, never a substitution of the
//!   current time. `recall` orders by `created_at`; a wall-clock fallback
//!   would collapse reading order invisibly.

pub mod claude_code;
pub mod codex;
pub mod copilot;
pub mod goose;
pub mod opencode;
pub mod qwen;
pub mod redact;

use crate::harness::{Harness, HarnessSpec, ReaderKind, TranscriptSupport};
use serde::Serialize;
use std::path::Path;

/// Which side of the conversation a turn came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnRole {
    User,
    Assistant,
}

impl TurnRole {
    /// The `memories.role` value. These two values have been declared in the
    /// schema since the beginning and, until ingest existed, nothing ever
    /// wrote them.
    pub fn as_role(self) -> &'static str {
        match self {
            TurnRole::User => "user",
            TurnRole::Assistant => "assistant",
        }
    }
}

/// One message extracted from a transcript.
#[derive(Debug, Clone, Serialize)]
pub struct Turn {
    /// Identity of the source record within its session, stable across
    /// re-reads. Claude Code supplies a per-record `uuid`.
    pub source_uuid: String,
    pub role: TurnRole,
    pub text: String,
    /// The transcript's own timestamp, normalized to ISO 8601 UTC.
    pub created_at: String,
}

/// A transcript file engram could read.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRef {
    pub harness: Harness,
    pub session_id: String,
    pub path: String,
    /// Working directory the session ran in, when the transcript records it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub bytes: u64,
}

/// Knobs controlling how much of a transcript survives into the store.
#[derive(Debug, Clone, Copy)]
pub struct ReadOptions {
    pub include_thinking: bool,
    pub include_tools: bool,
    pub include_sidechains: bool,
    pub max_bytes: u64,
    pub max_chars_per_turn: usize,
}

/// Default ceiling on transcript size, in bytes.
///
/// Generous for a conversation, and small enough that a runaway rollout
/// cannot exhaust memory: one Codex rollout observed on a developer machine
/// was 114 MB. Raise it with `--max-bytes` when a session genuinely is that
/// large.
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Default per-turn character cap. Long enough for any human message or model
/// reply; short enough that one pathological turn cannot dominate a scope.
pub const DEFAULT_MAX_CHARS_PER_TURN: usize = 32_768;

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            include_thinking: false,
            include_tools: false,
            include_sidechains: false,
            max_bytes: DEFAULT_MAX_BYTES,
            max_chars_per_turn: DEFAULT_MAX_CHARS_PER_TURN,
        }
    }
}

/// Why nothing was extracted, in the caller's terms.
#[derive(Debug)]
pub enum TranscriptError {
    /// The harness has no reader; the string explains what its transcripts
    /// look like instead.
    NoReader(&'static str),
    /// `$HOME` could not be resolved, so no harness path can be built.
    NoHome,
    /// The transcript file could not be read.
    Io(std::io::Error),
    /// The transcript was larger than the configured ceiling.
    TooLarge { bytes: u64, max_bytes: u64 },
    /// A record carried a timestamp that is not a valid instant. Substituting
    /// the current time here would silently destroy reading order.
    BadTimestamp { record: String, value: String },
}

impl std::fmt::Display for TranscriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranscriptError::NoReader(detail) => write!(f, "{detail}"),
            TranscriptError::NoHome => {
                write!(f, "cannot resolve the home directory (HOME is unset)")
            }
            TranscriptError::Io(e) => write!(f, "cannot read the transcript: {e}"),
            TranscriptError::TooLarge { bytes, max_bytes } => write!(
                f,
                "transcript is {bytes} bytes, above the {max_bytes}-byte ceiling"
            ),
            TranscriptError::BadTimestamp { record, value } => write!(
                f,
                "record {record} carries an unparseable timestamp '{value}'"
            ),
        }
    }
}

impl std::error::Error for TranscriptError {}

impl From<std::io::Error> for TranscriptError {
    fn from(e: std::io::Error) -> Self {
        TranscriptError::Io(e)
    }
}

/// Counts of everything a read discarded, reported so a user can see what a
/// transcript is actually made of — and so a format change shows up as a
/// spike in `unknown_record` rather than as silence.
///
/// The three "could not read this" counters are kept apart because they mean
/// different things and call for different responses. An unrecognized `type` is
/// a format change: benign, fixed by extending an allowlist. A torn line is a
/// write interrupted mid-flight: nothing to fix in engram, and transient if the
/// file was being appended while it was read. A record without a `uuid` is a
/// real conversation turn that had to be dropped for want of a stable id. One
/// combined counter made all three read as "the format moved", which is wrong
/// twice out of three times and desensitizes a reader to the case that matters.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FilterStats {
    /// Records whose `type` engram does not recognize. A non-zero count on a
    /// harness that used to read cleanly means the format moved.
    pub unknown_record: usize,
    /// Lines that are not valid JSON — an interrupted write, which can land
    /// mid-file and not only at the end.
    ///
    /// Not a format change, and not necessarily a defect: reading a transcript
    /// while its harness is still appending to it can catch a partial line that
    /// is complete moments later.
    pub torn_line: usize,
    /// Conversation records carrying no `uuid`, which cannot be given a stable
    /// id and so cannot be ingested.
    ///
    /// The only one of the three that means a real turn was lost.
    pub missing_uuid: usize,
    /// Recognized record types that are not conversation (mode changes,
    /// snapshots, titles).
    pub non_message: usize,
    pub thinking: usize,
    pub tool_use: usize,
    pub tool_result: usize,
    pub meta: usize,
    pub sidechain: usize,
    /// Synthetic turns a harness injects for its own slash commands.
    pub command_synthetic: usize,
    /// Records that held no text once filtered.
    pub empty: usize,
    /// Turns whose text exceeded `max_chars_per_turn` and was truncated.
    pub truncated: usize,
}

impl FilterStats {
    /// Accumulates another read's counts, for reporting a total across the
    /// sessions of one `--session all` run.
    pub fn merge(&mut self, other: &FilterStats) {
        self.unknown_record += other.unknown_record;
        self.torn_line += other.torn_line;
        self.missing_uuid += other.missing_uuid;
        self.non_message += other.non_message;
        self.thinking += other.thinking;
        self.tool_use += other.tool_use;
        self.tool_result += other.tool_result;
        self.meta += other.meta;
        self.sidechain += other.sidechain;
        self.command_synthetic += other.command_synthetic;
        self.empty += other.empty;
        self.truncated += other.truncated;
    }
}

/// Everything one read produced.
#[derive(Debug)]
pub struct ReadResult {
    pub turns: Vec<Turn>,
    pub filtered: FilterStats,
    pub redactions: redact::Redactions,
}

/// Deterministic id for a transcript turn.
///
/// The same discipline as [`crate::facts::fact_id`], and for the same reason:
/// with a UUID v5 over stable inputs, re-ingesting a session is an
/// `INSERT OR IGNORE` that inserts nothing, and resuming a live session
/// inserts only the tail. Idempotence falls out of the id rather than out of
/// a "have I seen this file" bookkeeping table.
pub fn turn_id(harness: &str, session_id: &str, source_uuid: &str) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("engram-turn:{harness}:{session_id}:{source_uuid}").as_bytes(),
    )
    .to_string()
}

/// Maps a working directory to Claude Code's project-directory name.
///
/// `/spacecraft-software/engram` becomes `-spacecraft-software-engram`: every
/// Every character outside `[A-Za-z0-9_-]` becomes `-`, which turns the
/// leading slash into a leading dash, and **case is preserved**
/// (`-spacecraft-software-Majestic` and `-spacecraft-software-majestic` are
/// different directories on disk). An empty result becomes `unknown`.
///
/// This mirrors the harness's own function, which is the authority — engram
/// does not get to choose the spelling of a directory somebody else creates:
///
/// ```js
/// function tl(e){ let t=e.replace(/[^a-zA-Z0-9\-_]/g,"-"); return t===""?"unknown":t }
/// ```
///
/// Replacing only `/` was wrong for any path containing a dot, and silently
/// so: `/spacecraft-software/construct/.claude/worktrees/x` mangles to
/// `…construct--claude-worktrees-x`, engram looked for
/// `…construct-.claude-worktrees-x`, and the session came back `NOT_FOUND` as
/// though it did not exist. Claude Code puts every worktree it creates under
/// `.claude/worktrees/`, so the blind spot grows with use.
///
/// Forward-only by construction. A literal `-` in a directory name is
/// indistinguishable from a replaced character in the result, so the inverse
/// mapping does not exist and is deliberately not offered — callers always
/// start from a working directory they already know.
pub fn mangle_cwd(cwd: &Path) -> String {
    let mangled: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if mangled.is_empty() {
        "unknown".to_owned()
    } else {
        mangled
    }
}

/// Normalizes a transcript timestamp to ISO 8601 UTC with a `Z` suffix.
///
/// # Errors
///
/// Returns the offending value when it is not a valid instant. The caller
/// turns that into an error rather than substituting the current time.
pub fn normalize_timestamp(value: &str) -> Result<String, String> {
    value
        .trim()
        .parse::<jiff::Timestamp>()
        .map(|t| t.to_string())
        .map_err(|_| value.to_string())
}

/// Strips ANSI CSI escape sequences, which appear inline in captured output.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // ESC [ ... <final byte in @..~>. Anything else after ESC is a short
        // two-character sequence; drop the one following character.
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

/// Wrappers a harness injects around its own slash-command bookkeeping. A
/// turn consisting only of one of these is the harness talking to itself, not
/// a message.
const SYNTHETIC_TAGS: &[&str] = &[
    "<local-command-caveat>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<command-name>",
    "<command-message>",
    "<command-args>",
];

/// True when the text is entirely one of the harness's synthetic wrappers.
pub fn is_command_synthetic(text: &str) -> bool {
    let trimmed = text.trim();
    SYNTHETIC_TAGS.iter().any(|tag| trimmed.starts_with(tag))
}

/// Removes `<system-reminder>` spans, which are harness-injected context
/// rather than anything a participant said.
pub fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        match after.find(CLOSE) {
            Some(end) => rest = &after[end + CLOSE.len()..],
            // Unterminated: drop the remainder rather than emitting a
            // half-open reminder as if a participant had written it.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Applies every text normalization in order, returning `None` when nothing
/// survives.
pub fn normalize_text(raw: &str, max_chars: usize, stats: &mut FilterStats) -> Option<String> {
    let text = strip_system_reminders(&strip_ansi(raw));
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if is_command_synthetic(trimmed) {
        stats.command_synthetic += 1;
        return None;
    }
    if trimmed.chars().count() > max_chars {
        stats.truncated += 1;
        let kept: String = trimmed.chars().take(max_chars).collect();
        let dropped = trimmed.chars().count() - max_chars;
        return Some(format!("{kept}\n[truncated {dropped} characters]"));
    }
    Some(trimmed.to_string())
}

/// Lists the transcripts a harness holds for a working directory.
///
/// # Errors
///
/// Returns [`TranscriptError::NoReader`] when the harness has no reader, so
/// an unreadable harness can never yield an empty-but-successful listing.
pub fn sessions_for(spec: &HarnessSpec, cwd: &Path) -> Result<Vec<SessionRef>, TranscriptError> {
    match spec.transcript {
        TranscriptSupport::Reader(ReaderKind::ClaudeCode) => claude_code::sessions(spec, cwd),
        TranscriptSupport::Reader(ReaderKind::Codex) => codex::sessions(spec, cwd),
        TranscriptSupport::Reader(ReaderKind::Opencode) => opencode::sessions(spec, cwd),
        TranscriptSupport::Reader(ReaderKind::Goose) => goose::sessions(spec, cwd),
        TranscriptSupport::Reader(ReaderKind::CopilotCli) => copilot::sessions(spec, cwd),
        TranscriptSupport::Reader(ReaderKind::Qwen) => qwen::sessions(spec, cwd),
        TranscriptSupport::NotImplemented { detail }
        | TranscriptSupport::Unsupported { detail } => Err(TranscriptError::NoReader(detail)),
    }
}

/// Reads one transcript into turns.
///
/// # Errors
///
/// Returns [`TranscriptError`] on an unreadable harness, an I/O failure, an
/// oversized transcript, or an unparseable timestamp.
pub fn read_session(
    spec: &HarnessSpec,
    session: &SessionRef,
    opts: &ReadOptions,
) -> Result<ReadResult, TranscriptError> {
    match spec.transcript {
        TranscriptSupport::Reader(ReaderKind::ClaudeCode) => {
            claude_code::read(Path::new(&session.path), opts)
        }
        TranscriptSupport::Reader(ReaderKind::Codex) => codex::read(Path::new(&session.path), opts),
        // A store-backed harness keeps every session in one file, so the
        // reader needs the id as well as the path.
        TranscriptSupport::Reader(ReaderKind::Opencode) => {
            opencode::read(Path::new(&session.path), &session.session_id, opts)
        }
        TranscriptSupport::Reader(ReaderKind::Goose) => {
            goose::read(Path::new(&session.path), &session.session_id, opts)
        }
        TranscriptSupport::Reader(ReaderKind::CopilotCli) => {
            copilot::read(Path::new(&session.path), &session.session_id, opts)
        }
        // Qwen writes Claude Code's records, so it shares that reader; only
        // session discovery differs.
        TranscriptSupport::Reader(ReaderKind::Qwen) => {
            claude_code::read(Path::new(&session.path), opts)
        }
        TranscriptSupport::NotImplemented { detail }
        | TranscriptSupport::Unsupported { detail } => Err(TranscriptError::NoReader(detail)),
    }
}

/// What one capture run did, across however many sessions it read.
///
/// Shared by the CLI's `ingest` and the MCP `save_chat` tool, so the two
/// cannot drift in what they count or how they count it.
#[derive(Debug, Default, Serialize)]
pub struct CaptureSummary {
    pub sessions: Vec<CapturedSession>,
    pub inserted: usize,
    pub skipped_existing: usize,
    pub filtered: FilterStats,
    /// Credential-shaped substrings replaced before storage, by kind.
    #[serde(skip_serializing_if = "redact::Redactions::is_empty")]
    pub redactions: redact::Redactions,
}

#[derive(Debug, Serialize)]
pub struct CapturedSession {
    pub session_id: String,
    pub path: String,
    pub turns: usize,
    pub inserted: usize,
    pub skipped_existing: usize,
}

/// Reads the given sessions and stores their turns in `scope`.
///
/// The single write path for transcript capture. A `dry_run` still performs
/// the whole read, filter, redaction, and id derivation — only the
/// transaction is skipped — so a preview exercises the same code as the real
/// thing rather than a parallel approximation of it.
///
/// # Errors
///
/// Returns a [`TranscriptError`] if any session cannot be read, or the
/// storage error if the insert fails.
pub fn capture(
    store: &std::sync::Mutex<crate::store::Store>,
    spec: &HarnessSpec,
    sessions: &[SessionRef],
    scope: &str,
    opts: &ReadOptions,
    dry_run: bool,
) -> Result<CaptureSummary, CaptureError> {
    let mut summary = CaptureSummary::default();

    for session in sessions {
        let result = read_session(spec, session, opts).map_err(CaptureError::Transcript)?;
        summary.filtered.merge(&result.filtered);
        summary.redactions.merge(&result.redactions);

        let rows: Vec<crate::store::IngestTurn> = result
            .turns
            .iter()
            .map(|t| crate::store::IngestTurn {
                id: turn_id(spec.name, &session.session_id, &t.source_uuid),
                agent: spec.name.to_string(),
                role: t.role.as_role().to_string(),
                content: t.text.clone(),
                created_at: t.created_at.clone(),
            })
            .collect();

        let report = if dry_run {
            crate::store::IngestReport {
                inserted: rows.len(),
                skipped_existing: 0,
            }
        } else {
            let mut guard = store.lock().expect("store lock poisoned");
            guard
                .ingest_turns(scope, &rows)
                .map_err(CaptureError::Storage)?
        };

        summary.inserted += report.inserted;
        summary.skipped_existing += report.skipped_existing;
        summary.sessions.push(CapturedSession {
            session_id: session.session_id.clone(),
            path: session.path.clone(),
            turns: rows.len(),
            inserted: report.inserted,
            skipped_existing: report.skipped_existing,
        });
    }

    Ok(summary)
}

/// Failure of a [`capture`] run.
#[derive(Debug)]
pub enum CaptureError {
    Transcript(TranscriptError),
    Storage(rusqlite::Error),
}

/// Session id derived from a transcript's file name.
pub(crate) fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Sorts newest-first by modification time so `--session latest` is stable.
pub(crate) fn sort_newest_first(sessions: &mut [(SessionRef, std::time::SystemTime)]) {
    sessions.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.session_id.cmp(&b.0.session_id))
    });
}

/// Absolute path form used when reporting a session.
pub(crate) fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mangle_cwd_replaces_separators_and_preserves_case() {
        assert_eq!(
            mangle_cwd(Path::new("/spacecraft-software/engram")),
            "-spacecraft-software-engram"
        );
        // Case is significant: two directories differing only in case are
        // distinct on disk, and lowercasing would merge them.
        assert_eq!(
            mangle_cwd(Path::new("/spacecraft-software/Majestic")),
            "-spacecraft-software-Majestic"
        );
        assert_eq!(mangle_cwd(Path::new("/")), "-");
        // A literal dash survives, which is exactly why the inverse mapping
        // is not offered.
        assert_eq!(mangle_cwd(Path::new("/a-b/c")), "-a-b-c");
    }

    /// A dot is replaced too, and an underscore is not.
    ///
    /// Replacing only `/` made every worktree under `.claude/worktrees/`
    /// unreadable — engram looked for a directory the harness never wrote and
    /// reported `NOT_FOUND`, which reads as "no such session" rather than "I
    /// mangled the name". The underscore case pins the other half of the
    /// harness's character class: `_` is *kept*, so mapping it to `-` would
    /// break paths that a naive "replace punctuation" rule would mangle.
    #[test]
    fn mangle_cwd_replaces_dots_and_keeps_underscores() {
        assert_eq!(
            mangle_cwd(Path::new(
                "/spacecraft-software/construct/.claude/worktrees/peaceful-jones"
            )),
            "-spacecraft-software-construct--claude-worktrees-peaceful-jones"
        );
        assert_eq!(
            mangle_cwd(Path::new("/home/mj/my_project")),
            "-home-mj-my_project"
        );
        // Anything else outside the class collapses to a dash as well.
        assert_eq!(mangle_cwd(Path::new("/a b/c.d")), "-a-b-c-d");
        // One `char`, one dash — the mapping is per-`char`, not per-byte, so a
        // multi-byte character does not become a run of dashes.
        assert_eq!(mangle_cwd(Path::new("/v1.2/café")), "-v1-2-caf-");
    }

    /// An empty path is `unknown`, not the empty string.
    ///
    /// The harness names that directory `unknown`; a bare `""` would make
    /// engram join the sessions root itself and list every project at once.
    #[test]
    fn mangle_cwd_of_an_empty_path_is_unknown() {
        assert_eq!(mangle_cwd(Path::new("")), "unknown");
    }

    #[test]
    fn turn_id_is_stable_and_distinct() {
        let a = turn_id("claude-code", "sess", "rec");
        assert_eq!(a, turn_id("claude-code", "sess", "rec"));
        assert_ne!(a, turn_id("claude-code", "sess", "other"));
        assert_ne!(a, turn_id("claude-code", "other", "rec"));
        assert_ne!(a, turn_id("codex", "sess", "rec"));
        assert!(uuid::Uuid::parse_str(&a).is_ok());
    }

    #[test]
    fn timestamps_normalize_or_fail_loudly() {
        assert_eq!(
            normalize_timestamp("2026-08-01T12:00:00.000Z").expect("valid"),
            "2026-08-01T12:00:00Z"
        );
        assert!(normalize_timestamp("not a timestamp").is_err());
        assert!(normalize_timestamp("").is_err());
    }

    #[test]
    fn ansi_escapes_are_stripped() {
        assert_eq!(
            strip_ansi("Set model to \u{1b}[1mHaiku 4.5\u{1b}[22m now"),
            "Set model to Haiku 4.5 now"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn system_reminders_are_removed() {
        assert_eq!(
            strip_system_reminders("before <system-reminder>noise</system-reminder> after"),
            "before  after"
        );
        // Unterminated span: drop the tail rather than emit half a reminder.
        assert_eq!(
            strip_system_reminders("kept <system-reminder>dangling"),
            "kept "
        );
        assert_eq!(strip_system_reminders("untouched"), "untouched");
    }

    #[test]
    fn synthetic_command_turns_are_recognized() {
        assert!(is_command_synthetic(
            "<local-command-stdout>Set model</local-command-stdout>"
        ));
        assert!(is_command_synthetic("<command-name>/model</command-name>"));
        assert!(!is_command_synthetic("a normal message"));
    }

    #[test]
    fn normalize_text_drops_noise_and_truncates() {
        let mut stats = FilterStats::default();
        assert_eq!(normalize_text("   ", 100, &mut stats), None);
        assert_eq!(
            normalize_text("<command-name>/x</command-name>", 100, &mut stats),
            None
        );
        assert_eq!(stats.command_synthetic, 1);

        let long = "x".repeat(50);
        let out = normalize_text(&long, 10, &mut stats).expect("truncated but kept");
        assert!(out.starts_with(&"x".repeat(10)));
        assert!(out.contains("[truncated 40 characters]"));
        assert_eq!(stats.truncated, 1);
    }
}

// Rust guideline compliant 2026-05-18
