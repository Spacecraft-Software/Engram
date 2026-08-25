// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Importing chat transcripts that were *exported* rather than read live.
//!
//! [`crate::transcript`] reads the session file a harness writes for itself.
//! That covers a harness engram has a reader for, and nothing else — which
//! left two gaps. A harness may store its history somewhere engram cannot
//! parse (protocol buffers, an editor's workspace state) while still offering
//! its own export command. And an archive written months ago is not a live
//! session at all: `save-chat` could write a `.texi` that engram had no way to
//! read back, so engram's own output was the one format it could not ingest.
//!
//! An export file is the common denominator, so this module takes one and
//! turns it into the same rows `transcript::capture` produces. It is
//! deliberately **not** a [`crate::harness::ReaderKind`]: that enum is built
//! around "one path per installed harness", discovered from a `HarnessSpec`
//! and a working directory, and a file somebody hands you fits none of that.
//!
//! Three properties are worth stating because each was learned from the
//! corpus rather than guessed:
//!
//! * **Identity is the content, not the file.** A message's id is a v5 uuid
//!   over `(scope, agent, role, created_at, text)`, so re-importing the same
//!   archive inserts nothing, a file copied to four projects imports once per
//!   scope rather than four times per scope, and the old `save-chat` append
//!   bug — which left one on-disk archive holding two concatenated copies of
//!   the same document — collapses back to one set of messages. No separate
//!   dedupe pass is needed; `INSERT OR IGNORE` does it.
//! * **Roles are not binary.** A real corpus of 7,163 archived messages held
//!   `assistant` 6,395, `user` 632 and **`note` 136**. Coercing that third
//!   role into one of the other two would rewrite history to fit a type.
//! * **A missing timestamp is synthesised in order, never invented from the
//!   clock.** See [`anchor_series`].

use std::path::Path;

/// An export format engram can parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum Format {
    /// A `.texi` written by `engram save-chat`, either dialect.
    EngramTexinfo,
    /// Opencode's (and Kilo's) Markdown export.
    OpencodeMarkdown,
    /// Raw Claude Code terminal scrollback, pasted into a file.
    ClaudeScrollback,
}

impl Format {
    /// The `--format` value and the name reported back.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Format::EngramTexinfo => "engram-texinfo",
            Format::OpencodeMarkdown => "opencode-markdown",
            Format::ClaudeScrollback => "claude-scrollback",
        }
    }
}

/// One message recovered from an export.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Message {
    pub agent: String,
    /// `user`, `assistant` or `note` — whatever the archive recorded.
    pub role: String,
    pub text: String,
    pub created_at: String,
    /// True when [`anchor_series`] supplied the timestamp.
    pub approximate: bool,
}

/// Why an import could not proceed.
#[derive(Debug)]
pub enum ImportError {
    Io(std::io::Error),
    TooLarge {
        bytes: u64,
        max_bytes: u64,
    },
    /// Nothing in the file looked like a transcript engram knows.
    Unrecognised,
    /// A recognised format whose timestamp did not parse. As in
    /// [`crate::transcript`], this is an error rather than a substitution:
    /// recall orders by `created_at`, so a wrong value corrupts reading order
    /// invisibly.
    BadTimestamp {
        value: String,
    },
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Io(e) => write!(f, "{e}"),
            ImportError::TooLarge { bytes, max_bytes } => {
                write!(
                    f,
                    "export is {bytes} bytes, above the {max_bytes}-byte ceiling"
                )
            }
            ImportError::Unrecognised => {
                write!(f, "no known export format matched this file")
            }
            ImportError::BadTimestamp { value } => {
                write!(f, "unparseable timestamp: {value}")
            }
        }
    }
}

/// Identifies the format of an export by looking at its content.
///
/// Content, not extension, on purpose: the corpus contained two `.html` files
/// in a `chat/` directory that were a standalone palette editor rather than a
/// conversation, and a future `.html` genuinely could be a web export. A file
/// that does not announce itself is skipped rather than guessed at.
#[must_use]
pub fn sniff(text: &str) -> Option<Format> {
    if text.contains("\\input texinfo") {
        return Some(Format::EngramTexinfo);
    }
    // Opencode's export leads with a `**Session ID:**` block and marks every
    // speaker with a level-two heading.
    if text.contains("**Session ID:**")
        || text
            .lines()
            .any(|l| l.starts_with("## Assistant (") || l.trim_end() == "## Assistant")
    {
        return Some(Format::OpencodeMarkdown);
    }
    // Scrollback has no headings at all — the speaker is a glyph in column 0.
    if text.lines().any(|l| l.starts_with('❯')) {
        return Some(Format::ClaudeScrollback);
    }
    None
}

/// Parses an export whose format is already known.
///
/// `anchor` is the fallback instant for formats that do not record per-message
/// times; see [`anchor_series`].
///
/// # Errors
///
/// Returns [`ImportError::BadTimestamp`] when a recorded timestamp does not
/// parse.
pub fn parse(
    text: &str,
    format: Format,
    anchor: jiff::Timestamp,
) -> Result<Vec<Message>, ImportError> {
    match format {
        Format::EngramTexinfo => parse_texinfo(text),
        Format::OpencodeMarkdown => Ok(parse_opencode_markdown(text, anchor)),
        Format::ClaudeScrollback => Ok(parse_claude_scrollback(text, anchor)),
    }
}

/// Assigns the `index`-th message of a timestamp-less export its instant.
///
/// One second per message from a file-level anchor. The absolute values are
/// approximate and say so (`Message::approximate`), but the *order* — the only
/// thing `recall` actually depends on — is exact. The alternative considered
/// and rejected was stamping every message with the same instant, which would
/// collapse a whole conversation into one moment and destroy reading order,
/// the precise failure [`crate::transcript`] refuses a wall-clock fallback to
/// avoid.
#[must_use]
pub fn anchor_series(anchor: jiff::Timestamp, index: usize) -> String {
    let shifted = anchor
        .checked_add(jiff::Span::new().seconds(i64::try_from(index).unwrap_or(i64::MAX)))
        .unwrap_or(anchor);
    // `jiff::Timestamp`'s Display is RFC 3339 with a `Z` suffix, which is
    // exactly what `normalize_timestamp` produces for the recorded case.
    shifted.to_string()
}

// ------------------------------------------------------------------ texinfo

/// Reverses [`crate::archive::escape_texinfo`].
///
/// That function maps exactly three characters — `@`→`@@`, `{`→`@{`, `}`→`@}`
/// — and nothing else, so this is a three-way reverse and not a general
/// Texinfo decoder. Scanning left to right matters: a naive
/// `replace("@@", "@")` run before the brace rules would turn an escaped
/// `@@{` into `@{` and then into a brace that was never there.
#[must_use]
pub fn unescape_texinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '@' {
            match chars.next() {
                Some(n @ ('@' | '{' | '}')) => out.push(n),
                Some(other) => {
                    out.push('@');
                    out.push(other);
                }
                None => out.push('@'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Splits a `Message by <agent> (<role>) at <timestamp>` heading.
///
/// Parsed from the right, because an agent name is the only free-form part:
/// the timestamp is the last token, `(role)` the token before it. The two
/// archive dialects differ only in heading level — the current writer emits
/// `@chapter` (the document already has an `@top`), the older one `@section`
/// under a title chapter — so both are accepted and the level carries no
/// meaning beyond that.
fn parse_message_heading(line: &str) -> Option<(String, String, String)> {
    let rest = line
        .strip_prefix("@chapter Message by ")
        .or_else(|| line.strip_prefix("@section Message by "))?;
    let (left, ts) = rest.rsplit_once(" at ")?;
    let left = left.trim_end();
    let open = left.rfind(" (")?;
    let role = left[open + 2..].strip_suffix(')')?;
    let agent = &left[..open];
    if agent.is_empty() || role.is_empty() || ts.trim().is_empty() {
        return None;
    }
    Some((agent.to_string(), role.to_string(), ts.trim().to_string()))
}

/// True for a Texinfo command line, which is never message content.
///
/// `@@` is an escaped literal `@` and therefore *is* content; a single `@`
/// followed by anything else is markup engram wrote.
fn is_structural(line: &str) -> bool {
    let mut chars = line.chars();
    chars.next() == Some('@') && chars.next() != Some('@')
}

fn parse_texinfo(text: &str) -> Result<Vec<Message>, ImportError> {
    let mut out: Vec<Message> = Vec::new();
    let mut current: Option<(String, String, String)> = None;
    let mut body = String::new();

    let flush = |head: Option<(String, String, String)>,
                 body: &mut String,
                 out: &mut Vec<Message>|
     -> Result<(), ImportError> {
        let Some((agent, role, ts)) = head else {
            body.clear();
            return Ok(());
        };
        let created_at = crate::transcript::normalize_timestamp(&ts)
            .map_err(|value| ImportError::BadTimestamp { value })?;
        let text = unescape_texinfo(body.trim_end_matches('\n'));
        body.clear();
        if text.trim().is_empty() {
            return Ok(());
        }
        out.push(Message {
            agent: unescape_texinfo(&agent),
            role,
            text,
            created_at,
            approximate: false,
        });
        Ok(())
    };

    for line in text.lines() {
        if let Some(head) = parse_message_heading(line) {
            flush(current.take(), &mut body, &mut out)?;
            current = Some(head);
            continue;
        }
        if line.trim_end() == "@bye" {
            flush(current.take(), &mut body, &mut out)?;
            continue;
        }
        // A structural line can never be message content: escaping doubles
        // every literal `@`, so content always arrives as `@@…`. Without this
        // the trailing `@c Signed by:` and `@chapter Chat history…` lines of
        // one document became the tail of the previous message's body — which
        // is exactly how a file holding two concatenated copies of itself
        // failed to deduplicate: the last message of each copy differed by the
        // header of the next one.
        if is_structural(line) {
            continue;
        }
        if current.is_some() {
            body.push_str(line);
            body.push('\n');
        }
    }
    flush(current.take(), &mut body, &mut out)?;
    Ok(out)
}

// ----------------------------------------------------------------- markdown

/// True for a line that opens a message in Opencode's export.
///
/// Anchored on the two literal speakers rather than on `^## `, because an
/// assistant's own reply routinely contains level-two headings — the observed
/// export had `## Root Cause Analysis`, `## Objective`, `## Next Move` inside
/// message bodies. Splitting on every `## ` shattered one 294 KB conversation
/// of 100 messages into several hundred fragments.
fn markdown_speaker(line: &str) -> Option<&'static str> {
    let t = line.trim_end();
    if t == "## User" || t.starts_with("## User (") {
        return Some("user");
    }
    if t == "## Assistant" || t.starts_with("## Assistant (") {
        return Some("assistant");
    }
    None
}

/// Reads `**Created:** 7/7/2026, 9:39:41 PM` out of the export's header.
///
/// US-locale, not RFC 3339 — the exporter formats for a reader, not for a
/// parser. Returns `None` rather than guessing when the shape does not match,
/// leaving the caller's file-level anchor in place.
fn opencode_created(text: &str) -> Option<jiff::Timestamp> {
    let line = text
        .lines()
        .take(40)
        .find_map(|l| l.trim().strip_prefix("**Created:**"))?;
    let (date, time) = line.trim().split_once(", ")?;
    let mut parts = date.split('/');
    let month: i8 = parts.next()?.trim().parse().ok()?;
    let day: i8 = parts.next()?.trim().parse().ok()?;
    let year: i16 = parts.next()?.trim().parse().ok()?;

    let time = time.trim();
    let (clock, meridiem) = time.rsplit_once(' ')?;
    let mut hms = clock.split(':');
    let mut hour: i8 = hms.next()?.parse().ok()?;
    let minute: i8 = hms.next()?.parse().ok()?;
    let second: i8 = hms.next().unwrap_or("0").parse().ok()?;
    match meridiem.to_ascii_uppercase().as_str() {
        "PM" if hour < 12 => hour += 12,
        "AM" if hour == 12 => hour = 0,
        "AM" | "PM" => {}
        _ => return None,
    }
    let dt = jiff::civil::date(year, month, day).at(hour, minute, second, 0);
    dt.to_zoned(jiff::tz::TimeZone::UTC)
        .ok()
        .map(|z| z.timestamp())
}

fn parse_opencode_markdown(text: &str, anchor: jiff::Timestamp) -> Vec<Message> {
    let anchor = opencode_created(text).unwrap_or(anchor);
    let mut out: Vec<Message> = Vec::new();
    let mut role: Option<&'static str> = None;
    let mut body = String::new();

    let flush = |role: Option<&'static str>, body: &mut String, out: &mut Vec<Message>| {
        let Some(role) = role else {
            body.clear();
            return;
        };
        let text = body.trim().to_string();
        body.clear();
        if text.is_empty() {
            return;
        }
        let index = out.len();
        out.push(Message {
            agent: "opencode".to_string(),
            role: role.to_string(),
            text,
            created_at: anchor_series(anchor, index),
            approximate: true,
        });
    };

    for line in text.lines() {
        if let Some(next) = markdown_speaker(line) {
            flush(role.take(), &mut body, &mut out);
            role = Some(next);
            continue;
        }
        if role.is_some() && line.trim_end() != "---" {
            body.push_str(line);
            body.push('\n');
        }
    }
    flush(role.take(), &mut body, &mut out);
    out
}

// --------------------------------------------------------------- scrollback

/// Parses terminal scrollback pasted into a file.
///
/// The lossiest path engram offers, and gated behind an explicit `--format`
/// for that reason. There is no structure here — the speaker is a glyph in
/// column zero (`❯` a prompt, `●` a reply, `⎿` a tool result, `✻` a spinner),
/// the text was hard-wrapped to the terminal's width with a two-space
/// continuation indent, and no timestamp survives anywhere. Wrapping cannot be
/// undone faithfully, so continuation lines are re-joined with a space and the
/// original line breaks are simply gone.
fn parse_claude_scrollback(text: &str, anchor: jiff::Timestamp) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    let mut role: Option<&'static str> = None;
    let mut body = String::new();

    let flush = |role: Option<&'static str>, body: &mut String, out: &mut Vec<Message>| {
        let Some(role) = role else {
            body.clear();
            return;
        };
        let text = body.trim().to_string();
        body.clear();
        if text.is_empty() {
            return;
        }
        let index = out.len();
        out.push(Message {
            agent: "claude-code".to_string(),
            role: role.to_string(),
            text,
            created_at: anchor_series(anchor, index),
            approximate: true,
        });
    };

    for line in text.lines() {
        let mut chars = line.chars();
        match chars.next() {
            Some('❯') => {
                flush(role.take(), &mut body, &mut out);
                role = Some("user");
                body.push_str(chars.as_str().trim());
            }
            Some('●') => {
                flush(role.take(), &mut body, &mut out);
                role = Some("assistant");
                body.push_str(chars.as_str().trim());
            }
            // Tool results, spinners and status lines are chrome, not speech —
            // the same payloads `transcript` excludes by default.
            Some('⎿' | '✻' | '⏺') => {
                flush(role.take(), &mut body, &mut out);
            }
            _ if role.is_some() => {
                let t = line.trim();
                if !t.is_empty() {
                    if !body.is_empty() {
                        body.push(' ');
                    }
                    body.push_str(t);
                }
            }
            _ => {}
        }
    }
    flush(role.take(), &mut body, &mut out);
    out
}

/// The stable id of an imported message.
///
/// Over the message's own content rather than its file, which is what makes
/// every dedupe requirement fall out of `INSERT OR IGNORE` instead of a
/// separate pass: re-importing an archive inserts nothing, thirteen
/// byte-identical copies of one file import once, and an archive holding two
/// concatenated copies of itself (the old `save-chat` append bug, still on
/// disk) yields one set of messages. Scope is included so the same archive
/// filed under two scopes really does land in both.
#[must_use]
pub fn import_id(scope: &str, m: &Message) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!(
            "engram-import:{scope}:{}:{}:{}:{}",
            m.agent, m.role, m.created_at, m.text
        )
        .as_bytes(),
    )
    .to_string()
}

/// Reads and parses one export file.
///
/// # Errors
///
/// Propagates I/O failures, refuses a file above `max_bytes`, and returns
/// [`ImportError::Unrecognised`] when no format matches and none was forced.
pub fn read_file(
    path: &Path,
    forced: Option<Format>,
    max_bytes: u64,
) -> Result<(Format, Vec<Message>), ImportError> {
    let meta = std::fs::metadata(path).map_err(ImportError::Io)?;
    if meta.len() > max_bytes {
        return Err(ImportError::TooLarge {
            bytes: meta.len(),
            max_bytes,
        });
    }
    let text = std::fs::read_to_string(path).map_err(ImportError::Io)?;
    let format = forced
        .or_else(|| sniff(&text))
        .ok_or(ImportError::Unrecognised)?;
    let anchor = file_anchor(&meta);
    let messages = parse(&text, format, anchor)?;
    Ok((format, messages))
}

/// The instant a timestamp-less export is anchored to: its own mtime.
///
/// Not the wall clock. An archive's mtime is at least *about* when the
/// conversation happened, so ordering between files stays roughly right, while
/// `now()` would stack every historical import into the present and sort them
/// by the order they happened to be read.
fn file_anchor(meta: &std::fs::Metadata) -> jiff::Timestamp {
    meta.modified()
        .ok()
        .and_then(|t| jiff::Timestamp::try_from(t).ok())
        .unwrap_or_else(jiff::Timestamp::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor() -> jiff::Timestamp {
        "2026-01-01T00:00:00Z".parse().expect("anchor")
    }

    #[test]
    fn unescape_reverses_exactly_three_characters() {
        assert_eq!(unescape_texinfo("@@code @{a@} b"), "@code {a} b");
        // A left-to-right scan: the escaped `@@` is consumed before the brace
        // rule can see the `{` behind it.
        assert_eq!(unescape_texinfo("@@{"), "@{");
        // An `@` that is not an escape survives untouched.
        assert_eq!(unescape_texinfo("user@host"), "user@host");
    }

    #[test]
    fn heading_parses_both_dialects() {
        let v2 = "@chapter Message by claude-code (user) at 2026-08-09T22:18:52.917Z";
        let v1 = "@section Message by codex (assistant) at 2026-07-26T00:04:16.394725123Z";
        assert_eq!(
            parse_message_heading(v2),
            Some((
                "claude-code".to_string(),
                "user".to_string(),
                "2026-08-09T22:18:52.917Z".to_string()
            ))
        );
        assert_eq!(
            parse_message_heading(v1).map(|(a, r, _)| (a, r)),
            Some(("codex".to_string(), "assistant".to_string()))
        );
        // A document title is not a message.
        assert_eq!(
            parse_message_heading("@chapter Chat history for scope: engram"),
            None
        );
    }

    /// The third role survives. A corpus of 7,163 archived messages held 136
    /// `note` rows; folding them into `user` or `assistant` would rewrite what
    /// the archive said to fit a two-variant type.
    #[test]
    fn texinfo_keeps_the_note_role() {
        let doc = "\\input texinfo\n\
                   @chapter Message by test-model (note) at 2026-07-11T08:08:35Z\n\
                   Hello from model\n\
                   @bye\n";
        let msgs = parse_texinfo(doc).expect("parses");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "note");
        assert_eq!(msgs[0].text, "Hello from model");
        assert!(!msgs[0].approximate);
    }

    /// The old `save-chat` appended instead of rewriting, so one archive on
    /// disk holds two copies of the same document. Identity is the content, so
    /// the duplicate collapses at insert time.
    #[test]
    fn a_double_appended_archive_yields_duplicate_ids() {
        // The real file interleaves a provenance comment and a title chapter
        // between the two copies — that is what used to leak into the body.
        let one = "@c Signed by: a-model on 2026-07-11T08:08:39Z\n\
                   @chapter Chat history for scope: plan-test\n\
                   @section Message by test-model (note) at 2026-07-11T08:08:35Z\n\
                   Hello\n";
        let doc = format!("\\input texinfo\n{one}{one}@bye\n");
        let msgs = parse_texinfo(&doc).expect("parses");
        assert_eq!(msgs.len(), 2, "both copies are parsed");
        assert_eq!(msgs[0].text, "Hello", "no structural line leaked in");
        assert_eq!(
            import_id("s", &msgs[0]),
            import_id("s", &msgs[1]),
            "and collapse to one row on INSERT OR IGNORE"
        );
    }

    #[test]
    fn markdown_ignores_headings_inside_a_reply() {
        let doc = "# Project\n\n**Session ID:** ses_x\n\n---\n\n\
                   ## User\n\nfix the build\n\n---\n\n\
                   ## Assistant (Build · Model · 4.7s)\n\n\
                   ## Root Cause Analysis\n\nthe flag moved\n\n\
                   ## Next Move\n\nrename it\n\n---\n";
        let msgs = parse_opencode_markdown(doc, anchor());
        assert_eq!(msgs.len(), 2, "one user turn and one assistant turn");
        assert_eq!(msgs[0].role, "user");
        assert!(msgs[1].text.contains("Root Cause Analysis"));
        assert!(msgs[1].text.contains("rename it"));
    }

    /// Ordering is exact even though the absolute times are not.
    #[test]
    fn synthesised_timestamps_are_ordered_and_flagged() {
        let doc = "**Session ID:** x\n## User\na\n## Assistant\nb\n## User\nc\n";
        let msgs = parse_opencode_markdown(doc, anchor());
        assert_eq!(msgs.len(), 3);
        assert!(msgs.iter().all(|m| m.approximate));
        assert!(msgs[0].created_at < msgs[1].created_at);
        assert!(msgs[1].created_at < msgs[2].created_at);
    }

    #[test]
    fn opencode_header_time_beats_the_file_anchor() {
        let doc = "**Session ID:** x\n**Created:** 7/7/2026, 9:39:41 PM\n## User\na\n";
        let msgs = parse_opencode_markdown(doc, anchor());
        assert!(
            msgs[0].created_at.starts_with("2026-07-07T21:39:41"),
            "got {}",
            msgs[0].created_at
        );
    }

    #[test]
    fn scrollback_uses_glyphs_and_drops_chrome() {
        let doc = "❯ finish the feature\n  because it is missing\n\
                   ⎿  Listed directory\n\
                   ● I'll start by loading\n  the six skills\n\
                   ✻ Baked for 5m 21s\n";
        let msgs = parse_claude_scrollback(doc, anchor());
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        // Hard wrapping is re-joined; the original breaks are unrecoverable.
        assert_eq!(msgs[0].text, "finish the feature because it is missing");
        assert_eq!(msgs[1].role, "assistant");
        assert!(!msgs[1].text.contains("Baked for"));
    }

    #[test]
    fn sniff_recognises_each_family_and_declines_the_rest() {
        assert_eq!(
            sniff("\\input texinfo\n@bye\n"),
            Some(Format::EngramTexinfo)
        );
        assert_eq!(
            sniff("**Session ID:** x\n## User\nhi\n"),
            Some(Format::OpencodeMarkdown)
        );
        assert_eq!(sniff("❯ hello\n"), Some(Format::ClaudeScrollback));
        // The palette-editor page that shared a `chat/` directory with real
        // archives: an extension check would have taken it, a content check
        // does not.
        assert_eq!(sniff("<!doctype html>\n<title>Palette Lab</title>"), None);
    }

    #[test]
    fn a_bad_timestamp_is_an_error_not_a_substitution() {
        let doc = "\\input texinfo\n@chapter Message by a (user) at not-a-time\nhi\n@bye\n";
        assert!(matches!(
            parse_texinfo(doc),
            Err(ImportError::BadTimestamp { .. })
        ));
    }
}
